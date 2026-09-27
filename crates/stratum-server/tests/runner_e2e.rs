//! The hosted runner, end to end, with nothing of ours standing in for
//! the parts that matter.
//!
//! Every case here goes the whole way round: a commit lands by the real
//! `git` CLI or the commit API, the trigger writes a run, the dispatcher
//! signs a `RunTask` to a fake ECS with the dispatch credential, the
//! fake starts the **real `weft-runner` binary** as a separate
//! process, that process fetches the repository over HTTP with the job
//! token it was handed, runs the steps with `sh`, streams its log back
//! through the runner API, reports a verdict, and the verdict shows up
//! where a person looks for it — the run page, the commit's checks, and
//! the land gate.
//!
//! The fake ECS is the only fake. It is a fake of AWS, not of us: it
//! verifies the signature, records the request, and starts the process.
//! Everything between the dispatcher and the check row is the product.
//!
//! The situations are the ones that happen. A push to trunk; a step that
//! fails and the change it blocks; a second push to a branch while the
//! first is still building; a runner that starts and never reports; a
//! cluster with no room; a workflow that does not parse; a change from
//! a fork; a deployment with no runner at all.

use std::time::{Duration, Instant};
use stratum_control::{registry, workflows, ControlDb};
use stratum_testkit::browser::Browser;
use stratum_testkit::fake_ecs::{runner_bin_next_to, Answer, FakeEcs};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::mailbox::Mailbox;
use stratum_testkit::{wait_until, Minio, Server};

const PASSWORD: &str = "a long enough password";

/// A workflow whose steps prove the runner's environment rather than
/// merely that a shell ran: the job key, the commit, the ref and the
/// event all have to reach the step, and the checkout has to be the
/// commit that was pushed.
const CI: &str = "\
name: ci
on: [push, change]
jobs:
  test:
    steps:
      - name: Env
        run: echo \"job=$WEFT_JOB sha=$WEFT_SHA ref=$WEFT_REF event=$WEFT_EVENT ci=$CI\"
      - name: Files
        run: cat README.md
";

/// One step that fails, one after it that must not run.
const RED: &str = "\
name: ci
on: [push, change]
jobs:
  test:
    steps:
      - name: Unit
        run: echo starting; exit 3
      - name: Ship
        run: echo never
";

/// A step that sleeps long enough for something to happen to it.
const SLOW: &str = "\
name: ci
on: push
jobs:
  test:
    steps:
      - name: Wait
        run: echo waiting; sleep 60
";

/// The same, for a change only — so a push to the branch the change
/// comes from starts nothing of its own and the run list holds only
/// the patchsets' builds.
const SLOW_CHANGE: &str = "\
name: ci
on: change
jobs:
  test:
    steps:
      - name: Wait
        run: echo waiting; sleep 60
";

/// A stable review identity, so a second push from the fork is a second
/// **patchset of the same change** rather than a second change. Without
/// the trailer a change is keyed from the commit oid, and "approve this
/// tip, then push another" would silently become two unrelated changes —
/// which is not the thing the per-tip rule is about.
const CHANGE_ID: &str = "Change-Id: I9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f";

const QUICK_CHANGE: &str = "\
name: ci
on: change
jobs:
  test:
    steps:
      - name: Done
        run: echo done
";

/// The poll interval every server in this file is started with, as a
/// `Duration`, so that a test proving a *negative* — no run appeared,
/// no second task was launched — can say "three ticks" rather than
/// picking a number that quietly stops meaning three ticks the moment
/// the knob moves.
const POLL: Duration = Duration::from_millis(100);

struct World {
    server: Server,
    ecs: FakeEcs,
    admin: String,
    scratch: Scratch,
}

/// A server dispatching every second through a fake ECS that starts the
/// real runner, and an `acme/app` repository to push at.
fn world(hint: &str) -> World {
    world_with(hint, &[])
}

fn world_with(hint: &str, extra: &[(&str, String)]) -> World {
    world_where(hint, extra, |_| {})
}

/// `world_with`, with a hook between the org existing and its repository
/// being created — for a deployment that sells things, where a private
/// repository needs the org to be paying first.
fn world_where(hint: &str, extra: &[(&str, String)], before_repo: impl FnOnce(&Server)) -> World {
    let minio = Minio::shared();
    let bucket = minio.bucket(hint);
    let scratch = Scratch::new(hint);
    let runner = runner_bin_next_to(env!("CARGO_BIN_EXE_stratum-server"));
    let ecs = FakeEcs::start(Some(runner), scratch.path().join("runners"));
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .envs(&ecs.env())
        // Fractions, not seconds. Every wait in this file is on an
        // observable, so the poll interval is pure latency: it is how
        // long a queued job sits before the dispatcher notices it, once
        // per job, in thirty-odd tests. It is also what the negative
        // assertions have to sleep past — three ticks of a one-second
        // poller is three seconds of nothing happening, and three ticks
        // of this is three hundred milliseconds.
        .env("STRATUM_RUNNER_POLL_SECS", "0.1")
        .env("STRATUM_LAND_POLL_SECS", "0.1")
        .envs(extra)
        .start();
    let admin = server.bootstrap_org("acme");
    before_repo(&server);
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    assert_eq!(st, 201, "{out}");
    World {
        server,
        ecs,
        admin,
        scratch,
    }
}

impl World {
    fn commit(&self, branch: &str, message: &str, files: &[(&str, &str)]) -> String {
        let ops: Vec<serde_json::Value> = files
            .iter()
            .map(|(p, c)| serde_json::json!({"op": "put", "path": p, "content": c}))
            .collect();
        let (st, out) = self.server.post(
            "/v1/orgs/acme/repos/app/commits",
            &self.admin,
            Some(serde_json::json!({
                "branch": branch,
                "message": message,
                "operations": ops,
            })),
        );
        assert_eq!(st, 201, "commit to {branch}: {out}");
        out["commit"].as_str().unwrap().to_string()
    }

    fn branch(&self, name: &str, from: &str) {
        let (st, out) = self.server.post(
            "/v1/orgs/acme/repos/app/branches",
            &self.admin,
            Some(serde_json::json!({"name": name, "from": from})),
        );
        assert_eq!(st, 201, "branch {name} from {from}: {out}");
    }

    /// A reviewer approves the change, so the only thing left between
    /// it and trunk is the build.
    fn approve(&self, change_key: &str) {
        self.server
            .admin(&[
                "admin",
                "user-create",
                "--org",
                "acme",
                "--email",
                "reviewer@acme.test",
                "--name",
                "Reviewer",
                "--password",
                PASSWORD,
                "--role",
                "member",
            ])
            .expect("user-create");
        let mut reviewer = Browser::signed_in(&self.server, "reviewer@acme.test", PASSWORD);
        let (st, out) = reviewer.req(
            "POST",
            &format!("/v1/orgs/acme/repos/app/changes/{change_key}/approve"),
            None,
        );
        assert_eq!(st, 204, "approve {change_key}: {out}");
    }

    fn runs(&self) -> Vec<serde_json::Value> {
        let (st, out) = self
            .server
            .get("/v1/orgs/acme/repos/app/workflow-runs", &self.admin);
        assert_eq!(st, 200, "{out}");
        out["runs"].as_array().cloned().unwrap_or_default()
    }

    /// Poll the run list until `pred` holds of it; the list at that
    /// moment. Sixty seconds covers a cold runner binary and a slow
    /// machine, and a case that needs longer is a case that is wrong.
    fn wait_runs(
        &self,
        what: &str,
        pred: impl Fn(&[serde_json::Value]) -> bool,
    ) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let runs = self.runs();
            if pred(&runs) {
                return runs;
            }
            assert!(
                Instant::now() < deadline,
                "waited 60s for {what}; runs were:\n{}",
                serde_json::to_string_pretty(&runs).unwrap()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The single run for `sha`, once it has settled.
    fn wait_settled(&self, sha: &str) -> serde_json::Value {
        let runs = self.wait_runs(&format!("the run for {sha} to settle"), |runs| {
            runs.iter()
                .any(|r| r["commit_sha"] == sha && r["state"] != "running")
        });
        runs.into_iter()
            .find(|r| r["commit_sha"] == sha)
            .expect("the run we waited for")
    }

    /// The run for `sha` once its first job has been launched.
    ///
    /// Launched means the fake ECS has the `RunTask` for one of its
    /// jobs, not merely that a job reads `running`: the claim marks the
    /// job running *before* the dispatcher asks for the task, and the
    /// gap between the two — the org's refusal checks, minting the
    /// token — is wide enough under a full test run for a poll to land
    /// in it and a caller to count zero tasks a moment later.
    fn wait_launched(&self, sha: &str) -> serde_json::Value {
        let running_ids = |r: &serde_json::Value| -> Vec<String> {
            r["jobs"]
                .as_array()
                .map(|js| {
                    js.iter()
                        .filter(|j| j["state"] == "running")
                        .filter_map(|j| j["id"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let runs = self.wait_runs(&format!("a job of {sha} to launch"), |runs| {
            runs.iter().any(|r| {
                r["commit_sha"] == sha && {
                    let ids = running_ids(r);
                    !ids.is_empty()
                        && self.ecs.run_tasks().iter().any(|t| {
                            t.env()
                                .get("STRATUM_JOB_ID")
                                .is_some_and(|id| ids.contains(id))
                        })
                }
            })
        });
        runs.into_iter()
            .find(|r| r["commit_sha"] == sha)
            .expect("the run we waited for")
    }

    /// The `RunTask` calls the fake has seen, once there are at least
    /// `n` of them.
    ///
    /// A job is marked running when the dispatcher claims it and only
    /// then does its `RunTask` go out, so a test that saw "running" and
    /// read the fake's call list in the same breath found it empty on a
    /// loaded CI runner — `index out of bounds: the len is 0`. Wait on
    /// the observable the next line needs, not on a proxy for it.
    fn wait_run_tasks(&self, n: usize) -> Vec<stratum_testkit::fake_ecs::EcsCall> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let calls = self.ecs.run_tasks();
            if calls.len() >= n {
                return calls;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "waited 30s for {n} RunTask call(s); the fake saw {}",
                calls.len()
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn checks(&self, sha: &str) -> Vec<serde_json::Value> {
        let (st, out) = self.server.get(
            &format!("/v1/orgs/acme/repos/app/commits/{sha}/checks"),
            &self.admin,
        );
        assert_eq!(st, 200, "{out}");
        out["runs"].as_array().cloned().unwrap_or_default()
    }

    fn log(&self, job_id: &str) -> String {
        let (st, out) = self.server.get(
            &format!("/v1/orgs/acme/repos/app/workflow-jobs/{job_id}/log"),
            &self.admin,
        );
        assert_eq!(st, 200, "{out}");
        out.as_str().unwrap_or_default().to_string()
    }
}

fn job<'a>(run: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    run["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["key"] == key)
        .unwrap_or_else(|| panic!("no job {key} in {run}"))
}

/// A working clone pushed with the real `git` CLI: what a developer's
/// machine does, rather than what our commit API does.
fn push_with_git(w: &World, branch: &str, files: &[(&str, &str)]) -> String {
    let dir = w.scratch.path().join(format!("work-{branch}"));
    std::fs::create_dir_all(&dir).unwrap();
    gitcli::git(&dir, &["init", "-q", "-b", branch]);
    for (path, content) in files {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    gitcli::git(&dir, &["add", "-A"]);
    gitcli::git(&dir, &["commit", "-q", "-m", "add ci"]);
    let url = w.server.authed_url(&w.admin, "acme", "app");
    gitcli::git(
        &dir,
        &["push", "-q", &url, &format!("HEAD:refs/heads/{branch}")],
    );
    gitcli::git(&dir, &["rev-parse", "HEAD"]).trim().to_string()
}

// ---------------------------------------------------------------------
// The happy path, with every seam checked on the way round
// ---------------------------------------------------------------------

#[test]
fn a_push_with_the_real_git_cli_runs_the_workflow_and_the_check_passes() {
    let w = world("runner-push");
    let sha = push_with_git(
        &w,
        "main",
        &[(".weft/ci.yml", CI), ("README.md", "hello from the repo\n")],
    );

    let run = w.wait_settled(&sha);
    assert_eq!(run["state"], "passed", "{run}");
    assert_eq!(run["event"], "push", "{run}");
    assert_eq!(run["ref_name"], "main", "{run}");
    assert_eq!(run["name"], "ci", "{run}");
    assert_eq!(run["file"], ".weft/ci.yml", "{run}");
    let test = job(&run, "test");
    assert_eq!(test["state"], "passed", "{test}");
    assert_eq!(test["attempts"], 1, "{test}");
    assert!(test["error"].is_null(), "{test}");

    // The log is what the steps printed, in the runner's format, and
    // the environment the steps saw is the one the contract promises.
    let log = w.log(test["id"].as_str().unwrap());
    assert!(log.contains("▶ Checkout"), "{log}");
    assert!(log.contains("▶ Env\n"), "{log}");
    assert!(
        log.contains(&format!("job=test sha={sha} ref=main event=push ci=true")),
        "the steps did not see the job's environment:\n{log}"
    );
    assert!(log.contains("hello from the repo"), "{log}");
    assert!(log.contains("✓ Files"), "{log}");

    // The commit's checks carry the verdict under the run's name, which
    // is what the Checks tab and the land gate read.
    let checks = w.checks(&sha);
    let check = checks
        .iter()
        .find(|c| c["name"] == "ci / test")
        .unwrap_or_else(|| panic!("no check for the job: {checks:?}"));
    assert_eq!(check["state"], "passing", "{check}");
    assert_eq!(check["provider"], "weft", "{check}");

    // One task was asked for, signed with the dispatch credential, on
    // the runner's own network, with exactly the three variables the
    // runner needs and nothing that could reach anything else.
    let launches = w.ecs.run_tasks();
    assert_eq!(launches.len(), 1, "{launches:?}");
    let launch = &launches[0];
    let auth = launch.authorization.as_deref().unwrap_or_default();
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDFAKEDISPATCH/"),
        "{auth}"
    );
    assert_eq!(launch.body["launchType"], "FARGATE", "{}", launch.body);
    assert_eq!(launch.body["cluster"], "fake-cluster", "{}", launch.body);
    let net = &launch.body["networkConfiguration"]["awsvpcConfiguration"];
    assert_eq!(net["assignPublicIp"], "DISABLED", "{net}");
    assert_eq!(
        net["subnets"],
        serde_json::json!(["subnet-fake-a", "subnet-fake-b"]),
        "{net}"
    );
    assert_eq!(
        net["securityGroups"],
        serde_json::json!(["sg-fake"]),
        "{net}"
    );
    let env = launch.env();
    assert_eq!(
        env.keys().collect::<Vec<_>>(),
        vec!["STRATUM_JOB_ID", "STRATUM_JOB_TOKEN", "STRATUM_RUNNER_URL"],
        "{env:?}"
    );
    assert_eq!(env["STRATUM_JOB_ID"], test["id"], "{env:?}");
    assert_eq!(env["STRATUM_RUNNER_URL"], w.server.base, "{env:?}");
    assert_eq!(
        launch.body["startedBy"],
        format!("stratum:{}", test["id"].as_str().unwrap()),
        "{}",
        launch.body
    );

    // The runner exited cleanly — a verdict it could not report is a
    // non-zero exit, and that would be a runner-side failure this test
    // would otherwise not see.
    let exits = w.ecs.wait_all(Duration::from_secs(10));
    assert_eq!(exits.values().collect::<Vec<_>>(), vec![&0], "{exits:?}");

    // The job token died with the job. The steps ran untrusted code as
    // the same user the runner is, so anything in the runner's
    // environment has to be assumed read; a credential that outlives
    // the job is a credential somebody may still be holding.
    //
    // The runner routes cannot show that: they answer this token
    // 410-with-state, because it is the job's own token and the job has
    // stopped, which is the one thing a late runner still needs to be
    // told. The clone below is the proof — `repo:read` is what the
    // token was minted for, and it is gone.
    let token = &env["STRATUM_JOB_TOKEN"];
    let (st, out) = w.server.get(
        &format!("/v1/runner/jobs/{}", test["id"].as_str().unwrap()),
        token,
    );
    assert_eq!(st, 410, "a finished job's token is told it is over: {out}");
    assert_eq!(out["state"], "passed", "{out}");
    let url = w.server.authed_url(token, "acme", "app");
    let dest = w.scratch.path().join("after");
    assert!(
        gitcli::git_expect_err(
            w.scratch.path(),
            &["clone", "-q", &url, dest.to_str().unwrap()]
        )
        .is_ok(),
        "a finished job's token can still clone the repository"
    );

    assert!(w.server.healthy());
}

/// A runner clones from the address it calls the control plane on, not
/// the public one. They coincide behind CloudFront and nowhere else — a
/// fleet on a private network is given `STRATUM_RUNNER_URL` precisely
/// because it cannot reach the public host, and it was being handed a
/// clone URL built from that host anyway.
#[test]
fn a_runner_clones_from_the_address_it_calls_back_on_not_the_public_one() {
    let w = world_with(
        "runner-clone-url",
        &[
            ("STRATUM_PUBLIC_URL", "http://stratum.public.invalid".into()),
            ("STRATUM_RUNNER_URL", "http://{bind}".into()),
        ],
    );
    let sha = w.commit(
        "main",
        "ci",
        &[("README.md", "# app\n"), (".weft/ci.yml", CI)],
    );
    let run = w.wait_settled(&sha);
    assert_eq!(run["state"], "passed", "{run}");
    let log = w.log(job(&run, "test")["id"].as_str().unwrap());
    assert!(log.contains("✓ Files"), "{log}");
    // The public host is what the browser sees; the runner never saw it.
    assert!(!log.contains("stratum.public.invalid"), "{log}");
    let launched = w.ecs.run_tasks();
    assert_eq!(launched.len(), 1);
    let env = launched[0].env();
    assert_eq!(env["STRATUM_RUNNER_URL"], w.server.base, "{env:?}");
}

/// A runner can be up and calling before the platform has answered
/// `RunTask` — a local docker daemon starts the container in the time it
/// takes to return its id. The job token has to be bound to the job
/// before that, or the runner's first call is a 403 for a credential
/// minted for exactly that job and the job sits `running` until its
/// start lease lapses. The fake takes this to its limit: the runner has
/// exited before the dispatcher hears the task exists.
#[test]
fn a_runner_that_reports_before_ecs_has_answered_is_still_its_job() {
    let w = world("runner-early-report");
    w.ecs.script([Answer::LaunchAndWait]);
    let sha = w.commit(
        "main",
        "ci",
        &[("README.md", "# app\n"), (".weft/ci.yml", CI)],
    );
    let run = w.wait_settled(&sha);
    assert_eq!(run["state"], "passed", "{run}");
    let test = job(&run, "test");
    assert_eq!(test["attempts"], 1, "{test}");
    assert_eq!(w.ecs.run_tasks().len(), 1);
    // Nothing was stopped: the job did not get cancelled, it finished.
    assert!(w.ecs.stop_tasks().is_empty(), "{:?}", w.ecs.stop_tasks());
    let exits = w.ecs.wait_all(Duration::from_secs(5));
    assert_eq!(
        exits.values().copied().collect::<Vec<_>>(),
        vec![0],
        "runner exit codes: {exits:?}"
    );
    assert!(w.server.healthy());
}

#[test]
fn pushing_the_same_commit_again_does_not_run_it_again() {
    let w = world("runner-idem");
    let sha = w.commit(
        "main",
        "add ci",
        &[(".weft/ci.yml", CI), ("README.md", "x\n")],
    );
    let run = w.wait_settled(&sha);
    assert_eq!(run["state"], "passed", "{run}");

    // The same tip, pushed again from a clone — a `git push` that
    // moves nothing still walks through receive-pack.
    let dir = w.scratch.path().join("again");
    let url = w.server.authed_url(&w.admin, "acme", "app");
    gitcli::git(
        w.scratch.path(),
        &["clone", "-q", &url, dir.to_str().unwrap()],
    );
    gitcli::git(&dir, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    // And a branch created at the same commit: a new ref, same tree,
    // same file — that is a run the author would not thank us for.
    w.branch("release", "main");
    // The push above moved nothing, so git never sent receive-pack a
    // command and the trigger never ran; the API branch does not go
    // through the push door at all. Neither reaches the one-run-per-
    // commit guard, so a test with only those two would pass with the
    // guard deleted. A new ref pushed by git at the same commit is a
    // real receive-pack command, and the trigger has to find the run
    // that exists.
    gitcli::git(&dir, &["push", "-q", "origin", "HEAD:refs/heads/from-git"]);
    let refs = gitcli::git(&dir, &["ls-remote", "--heads", "origin", "from-git"]);
    assert!(
        refs.contains(&sha),
        "the new ref did not land, so the push door was not walked: {refs}"
    );

    // A negative: nothing more appears. The trigger runs inside the
    // push, so the run list is already final when `git push` returns —
    // what this window is for is the dispatcher, which could still
    // claim and launch a second time. Three ticks of the world's poller
    // and nothing has.
    std::thread::sleep(3 * POLL);
    let runs = w.runs();
    let for_sha: Vec<_> = runs.iter().filter(|r| r["commit_sha"] == sha).collect();
    assert_eq!(
        for_sha.len(),
        1,
        "one commit, one run per workflow file: {runs:?}"
    );
    assert_eq!(w.ecs.run_tasks().len(), 1);
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// A red build, and the change it holds up
// ---------------------------------------------------------------------

#[test]
fn a_failing_step_fails_the_job_names_the_step_and_blocks_the_change() {
    let w = world("runner-red");
    w.commit("main", "seed", &[("README.md", "x\n")]);
    w.branch("feature", "main");
    let sha = w.commit(
        "feature",
        "break it\n\nChange-Id: I1a2d0001\n",
        &[(".weft/ci.yml", RED)],
    );
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos/app/changes",
        &w.admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["patchset"]["commit"], sha, "{out}");

    // A branch push *and* a change at the same commit: the push run
    // has to exist as well, so wait for the change's own run.
    let runs = w.wait_runs("the change run to settle", |runs| {
        runs.iter()
            .any(|r| r["event"] == "change" && r["state"] != "running")
    });
    let run = runs
        .iter()
        .find(|r| r["event"] == "change")
        .cloned()
        .unwrap();
    assert_eq!(run["state"], "failed", "{run}");
    assert_eq!(run["change_key"], "I1a2d0001", "{run}");
    assert_eq!(
        run["ref_name"], "main",
        "a change run is recorded against its target: {run}"
    );
    let test = job(&run, "test");
    assert_eq!(test["state"], "failed", "{test}");
    assert_eq!(test["error"], "step \"Unit\" exited 3", "{test}");

    let log = w.log(test["id"].as_str().unwrap());
    assert!(log.contains("starting"), "{log}");
    assert!(log.contains("✗ Unit exited 3"), "{log}");
    assert!(log.contains("– Ship (not run)"), "{log}");
    assert!(
        !log.contains("never"),
        "a step after the failure ran:\n{log}"
    );

    // Reviewed and approved: the gate reads the check, and refuses in
    // words that name it.
    w.approve("I1a2d0001");
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos/app/changes/I1a2d0001/land",
        &w.admin,
        None,
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["gate"], "blocked", "{out}");
    assert!(
        out["reason"]
            .as_str()
            .unwrap()
            .contains("check 'ci / test' is failing"),
        "{out}"
    );

    // Both runs — the branch push and the change — got their own task.
    w.wait_runs("the push run to settle", |runs| {
        runs.iter().all(|r| r["state"] != "running")
    });
    assert_eq!(w.ecs.run_tasks().len(), 2, "{:?}", w.ecs.run_tasks());
    assert!(w.server.healthy());
}

#[test]
fn a_green_change_run_lets_the_change_land() {
    let w = world("runner-green-land");
    w.commit("main", "seed", &[("README.md", "x\n")]);
    w.branch("feature", "main");
    let sha = w.commit(
        "feature",
        "add ci\n\nChange-Id: I1a2d0002\n",
        &[(".weft/ci.yml", CI)],
    );
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos/app/changes",
        &w.admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    w.wait_runs("every run to settle", |runs| {
        runs.len() == 2 && runs.iter().all(|r| r["state"] == "passed")
    });
    let checks = w.checks(&sha);
    assert!(
        checks
            .iter()
            .all(|c| c["name"] == "ci / test" && c["state"] == "passing"),
        "{checks:?}"
    );

    w.approve("I1a2d0002");
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos/app/changes/I1a2d0002/land",
        &w.admin,
        None,
    );
    assert_eq!(st, 202, "{out}");
    assert_eq!(out["waiting_on"], serde_json::json!([]), "{out}");
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// Superseding
// ---------------------------------------------------------------------

#[test]
fn a_second_push_to_a_branch_cancels_the_build_of_the_first() {
    let w = world("runner-supersede");
    w.commit("main", "seed", &[("README.md", "x\n")]);
    w.branch("feature", "main");
    let first = w.commit("feature", "slow", &[(".weft/ci.yml", SLOW)]);
    let run1 = w.wait_launched(&first);
    let job1 = job(&run1, "test").clone();
    let launched = w.wait_run_tasks(1);
    assert_eq!(launched.len(), 1);
    let token1 = launched[0].env()["STRATUM_JOB_TOKEN"].clone();
    // Let the runner get as far as its step, so what is cancelled is a
    // build in progress and not a task that never started.
    wait_until(
        "the first build to start its step",
        Duration::from_secs(30),
        || w.log(job1["id"].as_str().unwrap()).contains("waiting"),
    );

    // The fix also makes the build quick, so the second run can be
    // watched to the end.
    let second = w.commit(
        "feature",
        "fix",
        &[(".weft/ci.yml", CI), ("README.md", "y\n")],
    );

    let run1 = w.wait_settled(&first);
    assert_eq!(run1["state"], "cancelled", "{run1}");
    assert_eq!(
        run1["error"],
        format!("superseded by {}", &second[..12]),
        "{run1}"
    );
    assert_eq!(job(&run1, "test")["state"], "cancelled", "{run1}");
    let check = w.checks(&first);
    assert_eq!(check[0]["state"], "cancelled", "{check:?}");

    // The task was told to stop, with the reason on it, and its token
    // stopped working — a runner that ignored the StopTask learns from
    // the next call it makes.
    let stops = w.ecs.stop_tasks();
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0].body["cluster"], "fake-cluster");
    assert!(
        stops[0].body["reason"]
            .as_str()
            .unwrap()
            .starts_with("superseded by "),
        "{}",
        stops[0].body
    );
    // This is the supersede case exactly: the first build's container
    // may still be running, holding a token the cancellation revoked,
    // and 410-with-state is the only answer that makes it stop. 401 is
    // what it used to get, and a runner retries a 401 — so the build
    // superseding was supposed to save ran every step it had left.
    let (st, out) = w.server.get(
        &format!("/v1/runner/jobs/{}", job1["id"].as_str().unwrap()),
        &token1,
    );
    assert_eq!(st, 410, "{out}");
    assert_eq!(out["state"], "cancelled", "{out}");
    // The 410 is scoped to the job the token was minted for. The same
    // dead token against a job that does not exist is refused like any
    // other bad credential: a revoked token must not be a way to learn
    // the state of jobs it never belonged to, or which ids are real.
    let (st, out) = w
        .server
        .get("/v1/runner/jobs/01m1nosuchjob0000000000000", &token1);
    assert_eq!(st, 401, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown, revoked or expired"),
        "{out}"
    );

    // The second build runs to the end on its own.
    let run2 = w.wait_settled(&second);
    assert_eq!(run2["state"], "passed", "{run2}");
    assert_eq!(w.ecs.run_tasks().len(), 2);
    let exits = w.ecs.wait_all(Duration::from_secs(20));
    assert_eq!(exits.len(), 2, "{exits:?}");
    assert!(w.server.healthy());
}

#[test]
fn a_new_patchset_cancels_the_build_of_the_one_before_it() {
    let w = world("runner-patchset");
    w.commit("main", "seed", &[("README.md", "x\n")]);
    w.branch("feature", "main");
    let first = w.commit(
        "feature",
        "slow\n\nChange-Id: I1a2d0003\n",
        &[(".weft/ci.yml", SLOW_CHANGE)],
    );
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos/app/changes",
        &w.admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let run1 = w.wait_launched(&first);
    assert_eq!(run1["event"], "change", "{run1}");
    let job1 = job(&run1, "test").clone();
    wait_until(
        "the first patchset's build to start its step",
        Duration::from_secs(30),
        || w.log(job1["id"].as_str().unwrap()).contains("waiting"),
    );

    // The author pushes a fix and re-posts the change: patchset 2. The
    // push itself starts nothing (the file is change-only), so what
    // cancels the first build is the new patchset and nothing else.
    let second = w.commit(
        "feature",
        "fix\n\nChange-Id: I1a2d0003\n",
        &[(".weft/ci.yml", QUICK_CHANGE), ("README.md", "y\n")],
    );
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos/app/changes",
        &w.admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["patchset"]["number"], 2, "{out}");
    assert_eq!(out["patchset"]["commit"], second, "{out}");

    let run1 = w.wait_settled(&first);
    assert_eq!(run1["state"], "cancelled", "{run1}");
    assert_eq!(
        run1["error"],
        format!("superseded by {}", &second[..12]),
        "{run1}"
    );
    assert_eq!(run1["change_key"], "I1a2d0003", "{run1}");
    assert_eq!(w.checks(&first)[0]["state"], "cancelled");
    let stops = w.ecs.stop_tasks();
    assert_eq!(stops.len(), 1, "{stops:?}");

    let run2 = w.wait_settled(&second);
    assert_eq!(run2["state"], "passed", "{run2}");
    assert_eq!(run2["event"], "change", "{run2}");
    assert_eq!(w.runs().len(), 2, "one run per patchset: {:?}", w.runs());
    let exits = w.ecs.wait_all(Duration::from_secs(20));
    assert_eq!(exits.len(), 2, "{exits:?}");
    assert!(w.server.healthy());
}

#[test]
fn a_second_push_to_trunk_does_not_cancel_the_first() {
    let w = world("runner-trunk");
    let first = w.commit("main", "slow", &[(".weft/ci.yml", SLOW)]);
    w.wait_launched(&first);
    let second = w.commit("main", "more", &[("README.md", "y\n")]);
    w.wait_launched(&second);

    // Both are building. Trunk's history is what gets deployed, and
    // every commit on it deserves its own verdict.
    let runs = w.runs();
    assert!(
        runs.iter().all(|r| r["state"] == "running"),
        "a trunk build was cancelled: {runs:?}"
    );
    assert_eq!(w.ecs.stop_tasks().len(), 0);
    assert_eq!(w.ecs.running(), 2);

    // Cancelling one by hand stops exactly that one and not the other.
    let (st, out) = w.server.post(
        &format!(
            "/v1/orgs/acme/repos/app/workflow-runs/{}/cancel",
            runs[1]["id"].as_str().unwrap()
        ),
        &w.admin,
        None,
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["state"], "cancelled", "{out}");
    let (st, out) = w.server.post(
        &format!(
            "/v1/orgs/acme/repos/app/workflow-runs/{}/cancel",
            runs[1]["id"].as_str().unwrap()
        ),
        &w.admin,
        None,
    );
    assert_eq!(st, 409, "cancelling twice: {out}");
    assert_eq!(w.ecs.stop_tasks().len(), 1);
    wait_until(
        "the stopped runner to be gone, leaving one",
        Duration::from_secs(10),
        || w.ecs.running() == 1,
    );
    let runs = w.runs();
    assert_eq!(
        runs.iter().filter(|r| r["state"] == "running").count(),
        1,
        "{runs:?}"
    );
    assert!(w.server.healthy());
}

#[test]
fn deleting_a_branch_cancels_its_build() {
    let w = world("runner-delete");
    w.commit("main", "seed", &[("README.md", "x\n")]);
    w.branch("feature", "main");
    let sha = w.commit("feature", "slow", &[(".weft/ci.yml", SLOW)]);
    w.wait_launched(&sha);

    let dir = w.scratch.path().join("deleter");
    let url = w.server.authed_url(&w.admin, "acme", "app");
    gitcli::git(
        w.scratch.path(),
        &["clone", "-q", &url, dir.to_str().unwrap()],
    );
    gitcli::git(&dir, &["push", "-q", "origin", ":refs/heads/feature"]);

    let run = w.wait_settled(&sha);
    assert_eq!(run["state"], "cancelled", "{run}");
    assert_eq!(run["error"], "branch deleted", "{run}");
    assert_eq!(w.ecs.stop_tasks().len(), 1);
    assert!(w.server.healthy());
}

#[test]
fn a_job_cancelled_while_ecs_is_still_answering_is_stopped_when_the_answer_comes() {
    // RunTask is not instant: the dispatcher sits in the call for a
    // second or more while the world moves on. If the branch is deleted
    // in that window the job is already `cancelled` when the ARN
    // arrives, and a task nobody asked for is up and holding a live
    // credential. The dispatcher has to notice, stop it and retire the
    // token — not record the launch over the cancellation.
    let w = world("runner-cancel-inflight");
    w.ecs.script([Answer::LaunchAfter(Duration::from_secs(3))]);
    w.commit("main", "seed", &[("README.md", "x\n")]);
    w.branch("feature", "main");
    let sha = w.commit("feature", "slow", &[(".weft/ci.yml", SLOW)]);
    wait_until("RunTask to be called", Duration::from_secs(30), || {
        !w.ecs.run_tasks().is_empty()
    });
    let token = w.wait_run_tasks(1)[0].env()["STRATUM_JOB_TOKEN"].clone();

    let dir = w.scratch.path().join("deleter");
    let url = w.server.authed_url(&w.admin, "acme", "app");
    gitcli::git(
        w.scratch.path(),
        &["clone", "-q", &url, dir.to_str().unwrap()],
    );
    gitcli::git(&dir, &["push", "-q", "origin", ":refs/heads/feature"]);
    let run = w.wait_settled(&sha);
    assert_eq!(run["state"], "cancelled", "{run}");
    assert_eq!(run["error"], "branch deleted", "{run}");
    assert_eq!(w.ecs.stop_tasks().len(), 0, "nothing to stop yet");

    // ECS answers. The task it just started is stopped on the spot.
    let deadline = Instant::now() + Duration::from_secs(20);
    while w.ecs.stop_tasks().is_empty() {
        assert!(
            Instant::now() < deadline,
            "the task ECS reported after the cancellation was never stopped: {:?}",
            w.runs()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let stops = w.ecs.stop_tasks();
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(
        stops[0].body["reason"], "cancelled before it started",
        "{stops:?}"
    );
    // The token was revoked with the cancellation, and the call it is
    // used on still answers 410 rather than 401 — that is what makes a
    // runner that outlived its StopTask stop instead of retrying.
    let (st, out) = w.server.get(
        &format!(
            "/v1/runner/jobs/{}",
            job(&run, "test")["id"].as_str().unwrap()
        ),
        &token,
    );
    assert_eq!(st, 410, "the cancelled job's runner was not told to stop");
    assert_eq!(out["state"], "cancelled", "{out}");
    let run = w.wait_settled(&sha);
    assert_eq!(job(&run, "test")["state"], "cancelled", "{run}");
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// The fleet misbehaving
// ---------------------------------------------------------------------

/// A limit of one minute — the shortest the file may ask for.
/// How long the sweep may take once its input is already overdue: the
/// dispatcher's poll plus room for a loaded runner. Not the timeout —
/// that is the job's own, and the row is moved past it above.
const SWEEP: Duration = Duration::from_secs(30);

const ONE_MINUTE: &str = "\
name: ci
on: push
jobs:
  test:
    timeout-minutes: 1
    steps:
      - name: Wait
        run: echo waiting; sleep 600
";

#[test]
fn a_job_whose_runner_went_quiet_past_its_timeout_is_failed_by_the_sweep() {
    // The runner enforces the timeout itself, so the sweep only fires
    // for a runner that cannot: the task was killed under it, or it
    // is still heartbeating (the lease is long here) but will never
    // finish. `Vanish` is a task ECS accepted and never started, and
    // the start lease is longer than the test, so nothing but the
    // sweep can settle this job.
    let w = world_with(
        "runner-sweep",
        &[
            ("STRATUM_RUNNER_START_LEASE_SECS", "600".into()),
            ("STRATUM_RUNNER_LEASE_SECS", "600".into()),
            ("STRATUM_RUNNER_OVERDUE_SLACK_SECS", "1".into()),
        ],
    );
    w.ecs.script([Answer::Vanish]);
    let sha = w.commit(
        "main",
        "add ci",
        &[(".weft/ci.yml", ONE_MINUTE), ("README.md", "x\n")],
    );
    let run = w.wait_launched(&sha);
    let test = job(&run, "test").clone();
    let token = w.wait_run_tasks(1)[0].env()["STRATUM_JOB_TOKEN"].clone();

    // The minute this used to spend was the clock, not the product.
    //
    // `workflows::overdue_jobs` decides with `started + timeout_minutes
    // * 60_000 + slack_ms < now`, read off the job's own row — so moving
    // the row back past that deadline exercises exactly that arithmetic,
    // and the spec parsing behind `timeout_minutes`, without waiting for
    // it. The file may not ask for less than a minute of timeout and the
    // slack can only add, so there was no other way down: the only
    // alternative found was to teach the product to accept a *negative*
    // slack, which would let an operator have builds killed before the
    // `timeout-minutes` they wrote and make the failure message this test
    // asserts a lie by the length of the slack. A test is not worth that.
    //
    // `jobs::lifecycle_and_lease_claims` already makes the same move for
    // the same reason — "a negative lease is *already expired* without
    // sleeping".
    let mut db = postgres::Client::connect(&w.server.db_url, postgres::NoTls)
        .expect("connect to the server's database");
    let moved = db
        .execute(
            "UPDATE workflow_jobs SET started_at = started_at - 120000 \
             WHERE state = 'running' AND started_at IS NOT NULL",
            &[],
        )
        .expect("move the running job past its deadline");
    assert_eq!(
        moved, 1,
        "expected exactly the one running job to move; the sweep's input \
         is not what this test thinks it is"
    );

    let mut seen = Vec::new();
    let run = stratum_testkit::wait_for("the sweep to fail the overdue job", SWEEP, || {
        seen = w.runs();
        seen.iter().find(|r| r["state"] != "running").cloned()
    });
    assert_eq!(run["state"], "failed", "{run}");
    // The verdict is the job's, as a failed step's is; the run carries
    // an error only for what happened to the run as a whole.
    let reason = "timed out after 1 minutes and the runner did not report back";
    assert_eq!(job(&run, "test")["error"], reason, "{run}");
    assert_eq!(run["error"], serde_json::Value::Null, "{run}");
    // The task is stopped with the reason on it, the credential dies
    // with it, and the check says what happened.
    let stops = w.ecs.stop_tasks();
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0].body["reason"], reason, "{stops:?}");
    // A runner that is still alive out there — the sweep gave up on it,
    // it did not die — learns from its next call that its job is over
    // and which way, rather than being refused and retrying.
    let (st, out) = w.server.get(
        &format!("/v1/runner/jobs/{}", test["id"].as_str().unwrap()),
        &token,
    );
    assert_eq!(st, 410, "{out}");
    assert_eq!(out["state"], "failed", "{out}");
    // The check is mirrored *after* the job is failed and the task
    // stopped, and the run state this test waited on flips before the
    // mirror is written. Wait on the check itself — the observable this
    // assertion is about — rather than on the run as a proxy for it.
    // Seen failing once in a full-workspace run under load and not
    // reproduced alone; the ordering above is the mechanism that fits.
    let checks = stratum_testkit::wait_for("the check to mirror the failure", SWEEP, || {
        let checks = w.checks(&sha);
        (checks.first().is_some_and(|c| c["state"] == "failing")).then_some(checks)
    });
    assert_eq!(checks[0]["state"], "failing", "{checks:?}");
    assert!(w.server.healthy());
}

#[test]
fn a_poll_interval_of_zero_turns_the_dispatcher_off() {
    // Documented as the way to run a control plane that must not
    // dispatch — a read replica, a migration window. The runs are still
    // recorded and stay queued for whichever node does.
    let w = world_with(
        "runner-poll-off",
        &[("STRATUM_RUNNER_POLL_SECS", "0".into())],
    );
    let sha = w.commit(
        "main",
        "add ci",
        &[(".weft/ci.yml", CI), ("README.md", "x\n")],
    );
    w.wait_runs("the run to be recorded", |runs| {
        runs.iter().any(|r| r["commit_sha"] == sha)
    });
    // The one negative here with nothing to wait on: the claim is that
    // a loop does not exist, and an absent loop has no tick to watch
    // for. So this is a real wall-clock window — and a short one is the
    // whole of the proof that is available. `0` parses to a zero
    // `Duration`, so a dispatcher that read the knob and lost the
    // `is_zero` guard would spin with no delay at all and claim this
    // job within milliseconds; ten ticks is generous cover for that.
    // The other way to break it — misreading `0` and falling back to
    // the built-in five seconds — is not caught by any window shorter
    // than five seconds, and was not caught by the three this used to
    // sleep either.
    std::thread::sleep(10 * POLL);
    let runs = w.runs();
    let run = runs
        .iter()
        .find(|r| r["commit_sha"] == sha)
        .expect("the run");
    assert_eq!(run["state"], "running", "{run}");
    assert_eq!(job(run, "test")["state"], "queued", "{run}");
    assert!(w.ecs.run_tasks().is_empty(), "the dispatcher is off");
    assert!(w.server.healthy());
}

#[test]
fn a_cluster_with_no_room_keeps_the_job_queued_until_there_is() {
    let w = world("runner-capacity");
    w.ecs
        .script([Answer::NoCapacity, Answer::Throttle, Answer::Launch]);
    let sha = w.commit(
        "main",
        "add ci",
        &[(".weft/ci.yml", CI), ("README.md", "x\n")],
    );

    let run = w.wait_settled(&sha);
    assert_eq!(run["state"], "passed", "{run}");
    let test = job(&run, "test");
    // Three claims, three launches asked for, one accepted. A refusal
    // for capacity is not the job's fault: the claim is handed back and
    // does not count against it as a lost runner.
    assert_eq!(test["attempts"], 1, "{test}");
    assert_eq!(w.ecs.run_tasks().len(), 3);
    // Every refused launch's token was revoked before the next was
    // minted: a credential nobody will ever use is a credential to
    // kill, and the two the fake never saw a runner for must be dead.
    //
    // 401 and not 410 here, deliberately. A cancelled job's *own* token
    // is told 410 so the container it is in stops; these two are not
    // the job's token any more — the job was handed back to the queue
    // and the attempt that ran bound a third one — and there is no
    // container holding them, because the launch was refused. A token
    // that is nobody's job gets nobody's state.
    for call in &w.wait_run_tasks(2)[..2] {
        let (st, _) = w.server.get(
            &format!("/v1/runner/jobs/{}", test["id"].as_str().unwrap()),
            &call.env()["STRATUM_JOB_TOKEN"],
        );
        assert_eq!(st, 401, "a token from a refused launch still works");
    }
    assert!(w.server.healthy());
}

#[test]
fn a_runner_that_starts_and_never_reports_is_retried_then_given_up_on() {
    // A one-second start lease, so a task that never calls home lapses
    // in seconds rather than the ten minutes a real cold start needs.
    let w = world_with(
        "runner-vanish",
        &[("STRATUM_RUNNER_START_LEASE_SECS", "1".into())],
    );
    w.ecs
        .script([Answer::Vanish, Answer::Vanish, Answer::Vanish]);
    let sha = w.commit(
        "main",
        "add ci",
        &[(".weft/ci.yml", CI), ("README.md", "x\n")],
    );

    let run = w.wait_settled(&sha);
    assert_eq!(run["state"], "failed", "{run}");
    let test = job(&run, "test");
    assert_eq!(
        test["error"], "the runner was lost 2 times (it started but stopped reporting back)",
        "{test}"
    );
    // Two launches were tried (the default `STRATUM_RUNNER_MAX_ATTEMPTS`);
    // the third claim is where the dispatcher gave up, without asking
    // for a task it would not wait for.
    assert_eq!(test["attempts"], 3, "{test}");
    assert_eq!(w.ecs.run_tasks().len(), 2, "{:?}", w.ecs.run_tasks());
    // Each vanished task was stopped when its job was handed out again,
    // in case it was merely slow rather than gone — two runners working
    // the same job would report twice.
    assert_eq!(w.ecs.stop_tasks().len(), 2, "{:?}", w.ecs.stop_tasks());
    let checks = w.checks(&sha);
    assert_eq!(checks[0]["state"], "failing", "{checks:?}");
    assert!(w.server.healthy());
}

#[test]
fn ecs_refusing_the_task_definition_fails_the_job_in_ecs_own_words() {
    let w = world("runner-refused");
    w.ecs
        .script([Answer::Refuse("TaskDefinition not found.".into())]);
    let sha = w.commit(
        "main",
        "add ci",
        &[(".weft/ci.yml", CI), ("README.md", "x\n")],
    );
    let run = w.wait_settled(&sha);
    assert_eq!(run["state"], "failed", "{run}");
    let test = job(&run, "test");
    assert!(
        test["error"]
            .as_str()
            .unwrap()
            .contains("TaskDefinition not found."),
        "{test}"
    );
    assert_eq!(test["attempts"], 1, "a refusal is not retried: {test}");
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// Runs that never get a task
// ---------------------------------------------------------------------

#[test]
fn an_image_the_hosted_runner_does_not_have_is_refused_before_launch() {
    let w = world("runner-image");
    let sha = w.commit(
        "main",
        "add ci",
        &[(
            ".weft/ci.yml",
            "name: ci\non: push\njobs:\n  test:\n    image: rust:1.83\n    steps:\n      - run: cargo test\n",
        )],
    );
    let run = w.wait_settled(&sha);
    assert_eq!(run["state"], "failed", "{run}");
    let test = job(&run, "test");
    assert_eq!(
        test["error"], "image \"rust:1.83\" is not available on hosted runners (only default)",
        "{test}"
    );
    assert_eq!(w.ecs.run_tasks().len(), 0, "a task was asked for anyway");
    // The check row is mirrored from the job by a different writer than
    // the one that settles the run, and `queued` is `map_state`'s
    // deliberate fallback — so reading it the instant the run settles
    // catches the window in between and reports the fallback as the
    // verdict. Wait for the mirror, then assert what it says.
    wait_until(
        "the refusal to reach the commit's checks",
        Duration::from_secs(30),
        || {
            w.checks(&sha)
                .first()
                .is_some_and(|c| c["state"] != "queued")
        },
    );
    assert_eq!(w.checks(&sha)[0]["state"], "failing");
    assert!(w.server.healthy());
}

#[test]
fn a_workflow_that_does_not_parse_is_a_failing_check_that_says_why() {
    let w = world("runner-refusal");
    let sha = w.commit(
        "main",
        "add ci",
        &[(
            ".weft/ci.yml",
            "name: ci\non: push\njobs:\n  test:\n    needs: buidl\n    steps:\n      - run: true\n",
        )],
    );
    let run = w.wait_settled(&sha);
    assert_eq!(run["state"], "failed", "{run}");
    assert_eq!(run["jobs"], serde_json::json!([]), "{run}");
    let error = run["error"].as_str().unwrap();
    assert!(error.contains("buidl"), "{error}");
    assert!(error.contains(".weft/ci.yml"), "{error}");

    // One check, named for the file, so the commit page shows the
    // refusal where the verdict would have been.
    let checks = w.checks(&sha);
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert_eq!(checks[0]["name"], ".weft/ci.yml", "{checks:?}");
    assert_eq!(checks[0]["state"], "failing", "{checks:?}");
    assert_eq!(w.ecs.run_tasks().len(), 0);
    assert!(w.server.healthy());
}

#[test]
fn a_workflow_for_another_event_does_not_run() {
    let w = world("runner-other-event");
    let sha = w.commit(
        "main",
        "add ci",
        &[(
            ".weft/ci.yml",
            "name: ci\non: change\njobs:\n  test:\n    steps:\n      - run: true\n",
        )],
    );
    // The trigger runs inside the commit request, so by the time the
    // 201 came back the decision not to run this file had already been
    // made; the window is only cover for a deferred path that might yet
    // write a run behind it. Three ticks of the world's poller.
    std::thread::sleep(3 * POLL);
    assert_eq!(w.runs(), Vec::<serde_json::Value>::new());
    assert_eq!(w.checks(&sha), Vec::<serde_json::Value>::new());
    assert!(w.server.healthy());
}

#[test]
fn a_deployment_with_no_runner_says_so_instead_of_queueing_forever() {
    let minio = Minio::shared();
    let bucket = minio.bucket("runner-none");
    let scratch = Scratch::new("runner-none");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("runner-none")
        .data_dir(scratch.path().join("data"))
        .start();
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        &admin,
        Some(serde_json::json!({
            "branch": "main",
            "message": "add ci",
            "operations": [{"op": "put", "path": ".weft/ci.yml", "content": CI}],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let sha = out["commit"].as_str().unwrap();

    let (st, out) = server.get("/v1/orgs/acme/repos/app/workflow-runs", &admin);
    assert_eq!(st, 200, "{out}");
    let run = &out["runs"][0];
    assert_eq!(run["commit_sha"], sha, "{out}");
    assert_eq!(run["state"], "failed", "{run}");
    assert_eq!(
        run["error"],
        "no hosted runner is configured for this deployment (set STRATUM_RUNNER_ECS_* or STRATUM_RUNNER_EXEC)",
        "{run}"
    );
    let (st, checks) = server.get(
        &format!("/v1/orgs/acme/repos/app/commits/{sha}/checks"),
        &admin,
    );
    assert_eq!(st, 200, "{checks}");
    assert_eq!(checks["runs"][0]["state"], "failing", "{checks}");
    assert!(server.healthy());
}

// ---------------------------------------------------------------------
// The trigger's read of `.weft/`, when the store makes it hard
// ---------------------------------------------------------------------

/// A push whose workflow directory the trigger cannot read is a failed
/// check on the commit, not a line in the server log.
///
/// This happened. A fold between two pushes left the second one
/// unreadable through the API (see `wal_reads` in the engine), the
/// trigger printed one line and returned, and the commit sat with no
/// checks at all — which is what a commit whose CI has not started
/// looks like, so nobody knew to look. The store fault stands in for
/// any reason the read can fail; the assertion is about what the person
/// who pushed gets to see.
#[test]
fn a_workflow_directory_the_trigger_cannot_read_is_a_failing_check_that_says_so() {
    let minio = Minio::shared();
    let bucket = minio.bucket("runner-unreadable");
    let scratch = Scratch::new("runner-unreadable");
    let upstream = bucket
        .base_url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let proxy = stratum_testkit::FaultProxy::start(&upstream);
    let bucket_name = bucket.base_url.rsplit('/').next().unwrap().to_string();
    let store_url = format!("{}/{bucket_name}", proxy.url);
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &store_url)
        .db_hint("runner-unreadable")
        .data_dir(scratch.path().join("data"))
        .start();
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    assert_eq!(st, 201, "{out}");

    // The push lands — only reads of the pushed pack are refused, and
    // the trigger's is the first of those. The budget covers the other
    // readers a push wakes (the contribution walk), so the trigger's
    // own read cannot be the one that happens to get through.
    proxy.handle.inject("GET .seg", 3, 503);
    let dir = scratch.path().join("work");
    std::fs::create_dir_all(dir.join(".weft")).unwrap();
    gitcli::git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join(".weft/ci.yml"), CI).unwrap();
    gitcli::git(&dir, &["add", "-A"]);
    gitcli::git(&dir, &["commit", "-q", "-m", "add ci"]);
    let url = server.authed_url(&admin, "acme", "app");
    gitcli::git(&dir, &["push", "-q", &url, "HEAD:refs/heads/main"]);
    let sha = gitcli::git(&dir, &["rev-parse", "HEAD"]).trim().to_string();

    let deadline = Instant::now() + Duration::from_secs(30);
    let run = loop {
        let (st, out) = server.get("/v1/orgs/acme/repos/app/workflow-runs", &admin);
        assert_eq!(st, 200, "{out}");
        if let Some(run) = out["runs"]
            .as_array()
            .and_then(|rs| rs.iter().find(|r| r["commit_sha"] == sha))
        {
            break run.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no run appeared for {sha} after an unreadable trigger: {out}"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    proxy.handle.clear();
    assert_eq!(run["state"], "failed", "{run}");
    assert_eq!(run["file"], ".weft", "{run}");
    let error = run["error"].as_str().unwrap();
    assert!(error.contains("could not be read"), "{error}");
    assert!(
        error.contains("503"),
        "the store's answer is in the reason: {error}"
    );
    assert!(error.contains("Nothing ran"), "{error}");

    let (st, checks) = server.get(
        &format!("/v1/orgs/acme/repos/app/commits/{sha}/checks"),
        &admin,
    );
    assert_eq!(st, 200, "{checks}");
    assert_eq!(checks["runs"][0]["name"], ".weft", "{checks}");
    assert_eq!(checks["runs"][0]["state"], "failing", "{checks}");
    assert!(server.healthy());
}

/// Pushes on either side of a fold. Every push after the first deltas
/// against what the client knows the server has; once a fold has moved
/// that into the plane, the next push's pack reaches the plane for its
/// bases and the push after that reaches into *it*. The trigger has to
/// read `.weft/` through both.
///
/// Found by hand: a push to a branch after the stack's first compaction
/// created no run, the server said `not in locator` for a tree the
/// previous push had carried, and the runner — which clones through a
/// different reader — had built that very tree minutes before.
#[test]
fn a_push_after_a_fold_still_runs_the_workflow() {
    let w = world_with("runner-fold", &[("STRATUM_COMPACT_POLL_SECS", "0".into())]);
    let dir = w.scratch.path().join("work");
    std::fs::create_dir_all(&dir).unwrap();
    gitcli::git(&dir, &["init", "-q", "-b", "main"]);
    let url = w.server.authed_url(&w.admin, "acme", "app");
    // A file big enough, and similar enough version to version, that
    // git deltas it against the one before rather than storing it whole.
    let mut notes: String = (0..40)
        .map(|i| format!("line {i}: the quick brown fox jumps over the lazy dog\n"))
        .collect();
    let mut push = |dir: &std::path::Path, msg: &str| -> String {
        std::fs::write(dir.join("README.md"), &notes).unwrap();
        gitcli::git(dir, &["add", "-A"]);
        gitcli::git(dir, &["commit", "-q", "-m", msg]);
        gitcli::git(dir, &["push", "-q", &url, "HEAD:refs/heads/main"]);
        notes.push_str(&format!("appended after {msg}\n"));
        gitcli::git(dir, &["rev-parse", "HEAD"]).trim().to_string()
    };
    // Enough pushes to cross the WAL threshold, with no workflow yet so
    // nothing runs; then the fold.
    for i in 0..8 {
        push(&dir, &format!("notes {i}"));
    }
    let (st, out) = w
        .server
        .post("/v1/orgs/acme/repos/app/compact", &w.admin, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["outcome"], "Compacted", "{out}");

    // Now the workflow, in a push that deltas against the plane; and
    // one more, that deltas against the push before it.
    std::fs::create_dir_all(dir.join(".weft")).unwrap();
    std::fs::write(dir.join(".weft/ci.yml"), CI).unwrap();
    let first = push(&dir, "add ci");
    let second = push(&dir, "notes after ci");

    // The API reads the second push's tree — the read the trigger makes.
    let (st, out) = w.server.get(
        &format!("/v1/orgs/acme/repos/app/files/README.md?at={second}"),
        &w.admin,
    );
    assert_eq!(st, 200, "the push after the fold is unreadable: {out}");

    // And each push got its run; the second one built.
    let run = w.wait_settled(&second);
    assert_eq!(run["state"], "passed", "{run}");
    let runs = w.runs();
    assert!(
        runs.iter()
            .any(|r| r["commit_sha"] == first && r["file"] == ".weft/ci.yml"),
        "no run for the first push after the fold: {runs:?}"
    );
    assert!(w.server.healthy());
}

/// A `git revert` re-sends the tree from before the reverted commit,
/// blob for blob, and the ingest drops those held objects from the
/// segment rather than storing them twice. What lands is a thin pack: a
/// commit alone, or a commit and a tree whose entries all point outside
/// it. `git_e2e` proves a clone of that is sound — but a clone streams
/// whole segments and never resolves a base server-side, so it cannot
/// tell whether the segment and its `.oids` sidecar agree. The trigger,
/// the tree API and the contribution walk go through `LayoutReader`,
/// which trusts the sidecar to say which pack holds an object; a
/// sidecar written from the pre-dedup list would name a pack that does
/// not carry the object, and every read of that tree on the repository
/// would fail from then on, with `fsck` clean and a clone fine.
///
/// Written after a manual stack wedged on `not in locator` for a
/// reverting commit's root tree. That turned out to be the cross-pack
/// delta chain `a_push_after_a_fold_still_runs_the_workflow` pins —
/// this shape read fine under the old reader too — so what this test
/// holds is the ingest side: with the sidecar built from `pushed` instead
/// of `fresh`, it fails on the revert against the plane with
/// "listed by … but not among its entries".
#[test]
fn a_push_that_reintroduces_held_objects_stays_readable_and_runs() {
    let w = world_with(
        "runner-revert",
        &[("STRATUM_COMPACT_POLL_SECS", "0".into())],
    );
    let dir = w.scratch.path().join("work");
    std::fs::create_dir_all(&dir).unwrap();
    gitcli::git(&dir, &["init", "-q", "-b", "main"]);
    let url = w.server.authed_url(&w.admin, "acme", "app");
    let push_head = || -> String {
        gitcli::git(&dir, &["push", "-q", &url, "HEAD:refs/heads/main"]);
        gitcli::git(&dir, &["rev-parse", "HEAD"]).trim().to_string()
    };
    let push = |msg: &str| -> String {
        gitcli::git(&dir, &["add", "-A"]);
        gitcli::git(&dir, &["commit", "-q", "-m", msg]);
        push_head()
    };
    let read = |sha: &str, path: &str| -> (u16, serde_json::Value) {
        w.server.get(
            &format!("/v1/orgs/acme/repos/app/files/{path}?at={sha}"),
            &w.admin,
        )
    };

    std::fs::create_dir_all(dir.join(".weft")).unwrap();
    std::fs::write(dir.join(".weft/ci.yml"), CI).unwrap();
    std::fs::write(dir.join("README.md"), "the readme\n").unwrap();
    let base = push("ci and readme");
    std::fs::write(dir.join("g.txt"), "gone soon\n").unwrap();
    let added = push("add g");
    gitcli::git(&dir, &["rm", "-q", "g.txt"]);
    let removed = push("remove g");
    // The revert's root tree is `added`'s root tree, and its blobs are
    // all held: the pack that lands carries the commit alone.
    gitcli::git(&dir, &["revert", "--no-edit", "HEAD"]);
    let reverted = push_head();

    // Every tip reads through the API, the reverting one included.
    for (sha, path, body) in [
        (&base, "README.md", "the readme\n"),
        (&added, "g.txt", "gone soon\n"),
        (&reverted, "g.txt", "gone soon\n"),
        (&reverted, ".weft/ci.yml", CI),
    ] {
        let (st, out) = read(sha, path);
        assert_eq!(st, 200, "{path} at {sha}: {out}");
        assert_eq!(out, body, "{path} at {sha}: {out}");
    }
    let (st, out) = read(&removed, "g.txt");
    assert_eq!(st, 404, "g.txt was removed at {removed}: {out}");

    // And each push got its run, the revert's built.
    let run = w.wait_settled(&reverted);
    assert_eq!(run["state"], "passed", "{run}");
    let runs = w.runs();
    for sha in [&base, &added, &removed] {
        assert!(
            runs.iter().any(|r| r["commit_sha"] == *sha),
            "no run for {sha}: {runs:?}"
        );
    }

    // Fold everything into the plane and read the same trees through it.
    for i in 0..8 {
        std::fs::write(dir.join("README.md"), format!("the readme {i}\n")).unwrap();
        push(&format!("readme {i}"));
    }
    let (st, out) = w
        .server
        .post("/v1/orgs/acme/repos/app/compact", &w.admin, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["outcome"], "Compacted", "{out}");
    let (st, out) = read(&reverted, "g.txt");
    assert_eq!(st, 200, "the reverting tip after the fold: {out}");
    assert_eq!(out, "gone soon\n", "{out}");
    let (st, out) = read(&added, "g.txt");
    assert_eq!(st, 200, "the first carrier of g.txt after the fold: {out}");

    // One more revert-shaped push against the folded plane.
    gitcli::git(&dir, &["rm", "-q", "g.txt"]);
    push("remove g again");
    gitcli::git(&dir, &["revert", "--no-edit", "HEAD"]);
    let again = push_head();
    let (st, out) = read(&again, "g.txt");
    assert_eq!(st, 200, "a revert against the plane: {out}");
    let run = w.wait_settled(&again);
    assert_eq!(run["state"], "passed", "{run}");
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// A change from a fork
// ---------------------------------------------------------------------

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

fn signup<'a>(server: &'a Server, mail: &Mailbox, handle: &str, email: &str) -> Browser<'a> {
    let (st, body) = server.req(
        "POST",
        "/v1/auth/signup",
        "",
        Some(serde_json::json!({
            "handle": handle, "email": email, "name": handle, "password": PASSWORD,
        })),
    );
    assert_eq!(st, 202, "signup {handle}: {body}");
    let msg = mail.wait_for(email, Duration::from_secs(10));
    let link = msg.link().unwrap_or_else(|| panic!("no link in {msg:?}"));
    let token = urldecode(link.split_once("#verify=").expect("a verify link").1);
    let mut b = Browser::new(server);
    let (st, body) = b.req(
        "POST",
        "/v1/auth/verify",
        Some(serde_json::json!({ "token": token })),
    );
    assert_eq!(st, 200, "verify {handle}: {body}");
    b
}

/// ada's public `widget` with a workflow in it, bob's fork of it, and a
/// change from bob's branch — the state every fork case starts in.
///
/// `runner` decides whether the fake ECS starts the real runner binary:
/// the case that asserts nothing runs does not need one, and the case
/// that approves the workflows needs the build to actually happen.
struct Fork {
    server: Server,
    ecs: FakeEcs,
    /// Held so the temporary mail directory and scratch tree outlive the
    /// server that is writing into them.
    #[allow(dead_code)]
    mail: Mailbox,
    #[allow(dead_code)]
    scratch: Scratch,
    /// bob's contribution, and the change ada is looking at.
    sha: String,
    change_key: String,
}

fn fork_stack(hint: &str, runner: bool) -> Fork {
    let minio = Minio::shared();
    let bucket = minio.bucket(hint);
    let scratch = Scratch::new(hint);
    let mail = Mailbox::temp(hint);
    let bin = runner.then(|| runner_bin_next_to(env!("CARGO_BIN_EXE_stratum-server")));
    let ecs = FakeEcs::start(bin, scratch.path().join("runners"));
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .envs(&ecs.env())
        .envs(&mail.env())
        .env("STRATUM_RUNNER_POLL_SECS", "0.1")
        // The fork itself is a worker's job, and its knob defaults to
        // five seconds — every stack here paid that once, waiting for
        // `fork_state: ready` before it could do anything at all.
        .env("STRATUM_FORK_POLL_SECS", "0.1")
        .start();

    let (sha, change_key) = {
        let mut ada = signup(&server, &mail, "ada", "ada@example.com");
        let mut bob = signup(&server, &mail, "bob", "bob@example.com");
        let (st, body) = ada.req(
            "POST",
            "/v1/orgs/ada/repos",
            Some(serde_json::json!({ "name": "widget", "public": true })),
        );
        assert_eq!(st, 201, "{body}");
        let (st, body) = ada.req(
            "POST",
            "/v1/orgs/ada/repos/widget/commits",
            Some(serde_json::json!({
                "message": "add ci",
                "operations": [
                    { "op": "put", "path": "README.md", "content": "# seed\n" },
                    { "op": "put", "path": ".weft/ci.yml", "content": CI },
                ],
            })),
        );
        assert_eq!(st, 201, "{body}");

        let (st, body) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
        assert_eq!(st, 202, "{body}");
        wait_until(
            "bob's fork to become ready",
            Duration::from_secs(10),
            || {
                let (_, body) = bob.req("GET", "/v1/orgs/bob/repos/widget", None);
                body["fork_state"].as_str() == Some("ready")
            },
        );
        // On a branch off trunk, so the contribution carries the workflow
        // file with it — a commit to a branch that does not exist yet is a
        // root commit, and a root commit has no `.weft/` to run.
        let (st, body) = bob.req(
            "POST",
            "/v1/orgs/bob/repos/widget/branches",
            Some(serde_json::json!({ "name": "contrib", "from": "main" })),
        );
        assert_eq!(st, 201, "{body}");
        let (st, body) = bob.req(
            "POST",
            "/v1/orgs/bob/repos/widget/commits",
            Some(serde_json::json!({
                "message": format!("a contribution\n\n{CHANGE_ID}"),
                "branch": "contrib",
                "operations": [{ "op": "put", "path": "src/lib.rs", "content": "// hi\n" }],
            })),
        );
        assert_eq!(st, 201, "{body}");
        let sha = body["commit"].as_str().unwrap().to_string();
        let (st, opened) = bob.req(
            "POST",
            "/v1/orgs/ada/repos/widget/changes",
            Some(serde_json::json!({ "from": "contrib", "source": "bob/widget" })),
        );
        assert_eq!(st, 201, "{opened}");
        let key = opened["change"]["key"]
            .as_str()
            .or_else(|| opened["key"].as_str())
            .unwrap_or_else(|| panic!("no change key in {opened}"))
            .to_string();
        (sha, key)
    };
    Fork {
        server,
        ecs,
        mail,
        scratch,
        sha,
        change_key,
    }
}

/// A fork change under a **suspended** organisation says so, and the
/// approve button cannot help it.
///
/// The ordering matters more than it looks. All three refusals arrive as
/// `blocked`, so if a fork change under a suspension were coded `fork`,
/// the dashboard would offer "approve and run these workflows" — and
/// pressing it would delete the placeholder, re-trigger, and block again
/// with the same sentence. A button that does nothing but rewrite a row
/// is worse than no button: the reader concludes the product is broken
/// rather than that their organisation is suspended.
///
/// So the organisation's refusal is decided *before* the fork gate, the
/// code says `suspended`, and approving is still allowed to be pressed
/// (a maintainer may hold both facts) but tells the truth about what it
/// produced: 202, and a run that is still blocked.
#[test]
fn a_fork_change_under_a_suspension_is_coded_for_the_suspension_not_the_fork() {
    let f = fork_stack("runner-fork-susp", false);
    let server = &f.server;
    let key = f.change_key.clone();
    let db = ControlDb::open(&server.db_url).unwrap();
    let ada_org = registry::org_by_name(&db, "ada").unwrap().unwrap();
    let mut ada = Browser::signed_in(server, "ada@example.com", PASSWORD);
    let mut bob = Browser::signed_in(server, "bob@example.com", PASSWORD);

    // The first patchset is held for the fork, as it should be.
    let runs_at = |b: &mut Browser, sha: &str| -> Option<serde_json::Value> {
        let (st, runs) = b.req(
            "GET",
            &format!("/v1/orgs/ada/repos/widget/workflow-runs?change_key={key}&commit_sha={sha}"),
            None,
        );
        assert_eq!(st, 200, "{runs}");
        runs["runs"].as_array().unwrap().first().cloned()
    };
    let wait_for = |b: &mut Browser, sha: &str| -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(r) = runs_at(b, sha) {
                return r;
            }
            assert!(Instant::now() < deadline, "waited 30s for a run at {sha}");
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    let first = wait_for(&mut ada, &f.sha.clone());
    assert_eq!(first["blocked_reason"], "fork", "{first}");

    // Now the organisation is suspended, and the contributor pushes
    // again. Same fork, same change, different refusal.
    assert!(workflows::suspend_ci(
        &db,
        &ada_org.id,
        "mining software detected: xmrig",
        stratum_control::ids::now_ms(),
    )
    .unwrap());
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/bob/repos/widget/commits",
        Some(serde_json::json!({
            "message": format!("more\n\n{CHANGE_ID}"),
            "branch": "contrib",
            "operations": [{ "op": "put", "path": "src/more.rs", "content": "// more\n" }],
        })),
    );
    assert_eq!(st, 201, "{body}");
    let next = body["commit"].as_str().unwrap().to_string();
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/changes",
        Some(serde_json::json!({ "from": "contrib", "source": "bob/widget" })),
    );
    assert_eq!(st, 201, "{body}");

    let held = wait_for(&mut ada, &next);
    assert_eq!(held["state"], "blocked", "{held}");
    assert_eq!(
        held["blocked_reason"], "suspended",
        "a suspended organisation is not waiting for a maintainer: {held}"
    );
    assert_eq!(
        held["error"],
        "hosted workflows are suspended for this organisation: mining software detected: xmrig",
        "{held}"
    );
    let placeholder = held["id"].as_str().unwrap().to_string();

    // Approving is honest about what it produced: it really did retrigger
    // — the run is a new one — and the answer is still blocked, still for
    // the suspension. Nothing was queued and nothing ran.
    let (st, out) = ada.req(
        "POST",
        &format!("/v1/orgs/ada/repos/widget/changes/{key}/workflows/approve"),
        None,
    );
    assert_eq!(st, 202, "{out}");
    let started = out["runs"].as_array().unwrap();
    assert_eq!(started.len(), 1, "{out}");
    assert_ne!(started[0]["id"], placeholder.as_str(), "{out}");
    assert_eq!(started[0]["state"], "blocked", "{out}");
    assert_eq!(started[0]["blocked_reason"], "suspended", "{out}");
    assert!(started[0]["jobs"].as_array().unwrap().is_empty(), "{out}");
    // And it stays that way. A blocked run has no jobs, so no dispatcher
    // tick can find one to launch — the two tasks this stack did produce
    // are ada's own seed push and bob's push to his own fork, both from
    // before the suspension and neither this change's. Three ticks of
    // the dispatcher is the window; there is no observable for a launch
    // that must not happen.
    std::thread::sleep(3 * POLL);
    let after = runs_at(&mut ada, &next).expect("the approved run is still there");
    assert_eq!(after["state"], "blocked", "{after}");
    assert_eq!(after["blocked_reason"], "suspended", "{after}");
    assert!(after["jobs"].as_array().unwrap().is_empty(), "{after}");
    assert!(
        f.ecs
            .run_tasks()
            .iter()
            .all(|c| c.env()["STRATUM_JOB_ID"] != after["id"]),
        "a suspended organisation got a machine: {:?}",
        f.ecs.run_tasks()
    );
    assert!(server.healthy());
}

#[test]
fn a_change_from_a_fork_is_held_for_approval_and_runs_nothing() {
    let f = fork_stack("runner-fork", false);
    let (server, ecs) = (&f.server, &f.ecs);
    let sha = f.sha.clone();
    let mut ada = Browser::signed_in(server, "ada@example.com", PASSWORD);
    let mut bob = Browser::signed_in(server, "bob@example.com", PASSWORD);

    // The workflow was read, and deliberately not run: a stranger's
    // `run:` lines do not get a machine until a maintainer says so.
    let (st, runs) = ada.req("GET", "/v1/orgs/ada/repos/widget/workflow-runs", None);
    assert_eq!(st, 200, "{runs}");
    let run = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["event"] == "change")
        .unwrap_or_else(|| panic!("no change run: {runs}"));
    assert_eq!(run["state"], "blocked", "{run}");
    assert_eq!(run["commit_sha"], sha, "{run}");
    assert_eq!(
        run["error"],
        "this change comes from a fork; a maintainer has to approve its workflows before they run",
        "{run}"
    );
    // The word the dashboard decides on. It offers "approve these
    // workflows" for this refusal and for neither of the other two, and
    // it must not reach that decision by matching the sentence above —
    // which is written for a person and will be rewritten.
    assert_eq!(run["blocked_reason"], "fork", "{run}");
    // Two builds *did* run, and both are somebody's own push to their
    // own repository: ada's commit to trunk, and bob's to his fork.
    // Every task the fake was asked for has to be one of those, so the
    // stranger's change got a machine from nobody. Three dispatcher
    // ticks: the negative has no observable of its own.
    std::thread::sleep(3 * POLL);
    let (st, bobs) = bob.req("GET", "/v1/orgs/bob/repos/widget/workflow-runs", None);
    assert_eq!(st, 200, "{bobs}");
    let own_push_jobs: Vec<String> = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .chain(bobs["runs"].as_array().unwrap().iter())
        .filter(|r| r["event"] == "push")
        .flat_map(|r| r["jobs"].as_array().unwrap().iter())
        .map(|j| j["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(own_push_jobs.len(), 2, "{runs} {bobs}");
    let launched = ecs.run_tasks();
    assert_eq!(launched.len(), 2, "{launched:?}");
    for call in &launched {
        assert!(
            own_push_jobs.contains(&call.env()["STRATUM_JOB_ID"]),
            "a task was launched for something other than an owner's push: {call:?}"
        );
    }

    // Held, not failed: the check is queued, so the change waits at the
    // gate rather than being refused by it.
    let (st, checks) = ada.req(
        "GET",
        &format!("/v1/orgs/ada/repos/widget/commits/{sha}/checks"),
        None,
    );
    assert_eq!(st, 200, "{checks}");
    assert_eq!(checks["runs"][0]["state"], "queued", "{checks}");
    assert!(server.healthy());
}

// ---------------------------------------------------------------------
// Where a check row sends the reader
// ---------------------------------------------------------------------

/// Every row we write into `check_runs` links to the run behind it.
///
/// A check row is a verdict and an address; every other provider fills
/// the address with a link into their own build, and a hosted row that
/// leaves it null is a dead end — a red `ci / test` beside a Buildkite
/// row that goes somewhere is worse than useless, and a *refused file*
/// with no link hides the only copy of the reason it was refused.
///
/// All three kinds of row are checked here, because they are written by
/// three different callers: a job that reported (the runner API), a
/// file we would not run (the trigger's settle), and a build somebody
/// stopped (the cancel route). The public URL carries a trailing slash,
/// which is how half of the deployments spell it — a naive `format!`
/// emits `https://forge.example//acme/...`, which some readers
/// normalise and others 404.
#[test]
fn every_hosted_check_row_links_to_the_run_that_produced_it() {
    let w = world_with(
        "runner-detail-url",
        &[
            ("STRATUM_PUBLIC_URL", "https://forge.example/".into()),
            // The public host is not reachable from here; the runner
            // clones from the address it calls back on.
            ("STRATUM_RUNNER_URL", "http://{bind}".into()),
        ],
    );
    let link = |run: &serde_json::Value| {
        format!(
            "https://forge.example/acme/app/checks/runs/{}",
            run["id"].as_str().unwrap()
        )
    };

    // A job that ran and reported.
    let green = w.commit(
        "main",
        "add ci",
        &[("README.md", "# app\n"), (".weft/ci.yml", CI)],
    );
    let run = w.wait_settled(&green);
    assert_eq!(run["state"], "passed", "{run}");
    let checks = w.checks(&green);
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert_eq!(checks[0]["detail_url"], link(&run), "{checks:?}");

    // A file we refused: no job ever existed, and the row still points
    // at the run page carrying the reason.
    w.branch("bad", "main");
    let bad = w.commit(
        "bad",
        "typo",
        &[(
            ".weft/ci.yml",
            "name: ci\non: push\njobs:\n  test:\n    needs: buidl\n    steps:\n      - run: true\n",
        )],
    );
    let refused = w.wait_settled(&bad);
    assert_eq!(refused["jobs"], serde_json::json!([]), "{refused}");
    let checks = w.checks(&bad);
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert_eq!(checks[0]["name"], ".weft/ci.yml", "{checks:?}");
    assert_eq!(checks[0]["detail_url"], link(&refused), "{checks:?}");

    // A build somebody stopped.
    w.branch("slow", "main");
    let slow = w.commit("slow", "slow", &[(".weft/ci.yml", SLOW)]);
    let running = w.wait_launched(&slow);
    let (st, out) = w.server.post(
        &format!(
            "/v1/orgs/acme/repos/app/workflow-runs/{}/cancel",
            running["id"].as_str().unwrap()
        ),
        &w.admin,
        None,
    );
    assert_eq!(st, 200, "{out}");
    let cancelled = w.wait_settled(&slow);
    assert_eq!(cancelled["state"], "cancelled", "{cancelled}");
    let checks = w.checks(&slow);
    assert_eq!(checks[0]["state"], "cancelled", "{checks:?}");
    assert_eq!(checks[0]["detail_url"], link(&cancelled), "{checks:?}");

    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// What stops a hosted fleet becoming somebody else's free compute
// ---------------------------------------------------------------------

/// A workflow asking for longer than the fleet allows.
const LONG: &str = "\
name: ci
on: push
jobs:
  test:
    timeout-minutes: 720
    steps:
      - name: Forever
        run: echo hello
";

/// The same file, exactly at the cap. The boundary is `>`, not `>=`: a
/// job asking for precisely the limit is asking for something the fleet
/// offers.
const AT_CAP: &str = "\
name: lint
on: push
jobs:
  check:
    timeout-minutes: 60
    steps:
      - name: Lint
        run: echo linting
";

/// A `timeout-minutes:` over this fleet's ceiling is refused where the
/// person who wrote it will see it, and nothing is launched.
///
/// Refused rather than clamped down to the cap, which is the decision
/// worth pinning: a build told it may run for twelve hours and stopped
/// at six fails in a way its author cannot explain from anything they
/// wrote, and they will spend the afternoon looking for the bug that
/// made it slow. The refusal names both numbers instead.
#[test]
fn a_timeout_over_the_fleets_cap_is_refused_when_the_run_is_triggered() {
    let w = world_with(
        "runner-timeout-cap",
        &[
            ("STRATUM_RUNNER_MAX_TIMEOUT_MINUTES", "60".into()),
            // A typo in a budget knob is not a boot failure and not a
            // budget of zero. These are ceilings on somebody else's
            // builds: refusing to start over a bad character would take
            // a deployment down to protect it from running builds for
            // slightly too long, and reading it as zero would refuse
            // every build on the instance. Unreadable means "no limit
            // configured", which is what the file below relies on to
            // run at all.
            ("STRATUM_RUNNER_MINUTES_PER_MONTH", "lots".into()),
        ],
    );
    let sha = push_with_git(
        &w,
        "main",
        &[
            (".weft/ci.yml", LONG),
            (".weft/lint.yml", AT_CAP),
            ("README.md", "hello\n"),
        ],
    );

    // The file at the cap runs and passes; the one over it never does.
    let runs = w.wait_runs("both files to settle", |runs| {
        runs.iter().filter(|r| r["commit_sha"] == sha).count() == 2
            && runs
                .iter()
                .all(|r| r["commit_sha"] != sha || r["state"] != "running")
    });
    let over = runs
        .iter()
        .find(|r| r["file"] == ".weft/ci.yml")
        .unwrap_or_else(|| panic!("no run for ci.yml: {runs:?}"));
    assert_eq!(over["state"], "failed", "{over}");
    assert_eq!(
        over["error"], "timeout-minutes: 720 exceeds this fleet's limit of 60",
        "{over}"
    );
    assert!(
        over["jobs"].as_array().unwrap().is_empty(),
        "a refused file has no jobs to run: {over}"
    );
    let at_cap = runs
        .iter()
        .find(|r| r["file"] == ".weft/lint.yml")
        .unwrap_or_else(|| panic!("no run for lint.yml: {runs:?}"));
    assert_eq!(
        at_cap["state"], "passed",
        "a job asking for exactly the limit is inside it: {at_cap}"
    );

    // Where a person looks: a red check naming the file, with the
    // reason on it, beside the green one from the file that was fine.
    let checks = w.checks(&sha);
    let refused = checks
        .iter()
        .find(|c| c["name"] == ".weft/ci.yml")
        .unwrap_or_else(|| panic!("no check for the refused file: {checks:?}"));
    assert_eq!(refused["state"], "failing", "{refused}");

    // One task, for the file that was allowed to run.
    let launched = w.ecs.run_tasks();
    assert_eq!(launched.len(), 1, "{launched:?}");
    assert!(w.server.healthy());
}

/// An organisation that has spent its month is told so, on the commit,
/// and gets no machine.
///
/// The budget is a minute, so the first build spends it: a job is billed
/// per job and rounded up, because a thousand eleven-second jobs is the
/// shape of an abusive workload and summing milliseconds would price it
/// at nothing. The second push is the case that matters — it is refused
/// at trigger time, where there is still somebody to tell. A job that
/// quietly never claimed would look exactly like a build that has not
/// started yet, and its author would wait for it.
#[test]
fn an_organisation_out_of_minutes_is_told_so_rather_than_left_waiting() {
    let w = world_with(
        "runner-minutes",
        &[("STRATUM_RUNNER_MINUTES_PER_MONTH", "1".into())],
    );
    let first = push_with_git(
        &w,
        "main",
        &[(".weft/ci.yml", CI), ("README.md", "hello from the repo\n")],
    );
    // The first build is inside the budget and runs the whole way: the
    // job being admitted must not be counted against its own admission,
    // or a one-minute fleet would cancel every first build it ever ran.
    let run = w.wait_settled(&first);
    assert_eq!(run["state"], "passed", "{run}");

    // That minute is now spent, and the next push is refused with the
    // number in it.
    w.branch("feature", "main");
    let second = w.commit("feature", "more", &[("src/lib.rs", "// hi\n")]);
    let blocked = w.wait_settled(&second);
    assert_eq!(blocked["state"], "blocked", "{blocked}");
    assert_eq!(
        blocked["error"], "this organisation has used its 1 hosted-runner minutes for the month",
        "{blocked}"
    );
    assert_eq!(
        blocked["blocked_reason"], "budget",
        "an out-of-minutes refusal is not a fork approval, and a button \
         offered here could only ever answer 409: {blocked}"
    );
    assert!(
        blocked["jobs"].as_array().unwrap().is_empty(),
        "nothing was queued: {blocked}"
    );

    // Queued rather than failing: nothing is wrong with the change, and
    // a red check would send its author to fix code that is fine.
    let checks = w.checks(&second);
    assert_eq!(checks[0]["state"], "queued", "{checks:?}");

    // One task for the whole test: the first build's.
    assert_eq!(w.ecs.run_tasks().len(), 1, "{:?}", w.ecs.run_tasks());

    // And the number is on the billing view, where somebody whose
    // builds have just stopped will go looking for it. `null` would be
    // unlimited, which is the opposite fact from "none left".
    let (st, bill) = w.server.get("/v1/orgs/acme/billing", &w.admin);
    assert_eq!(st, 200, "{bill}");
    assert_eq!(bill["ci_minutes_limit"], 1, "{bill}");
    assert_eq!(bill["ci_minutes_used"], 1, "{bill}");
    assert_eq!(bill["ci_minutes_remaining"], 0, "{bill}");
    assert!(bill["ci_suspended_reason"].is_null(), "{bill}");

    assert!(w.server.healthy());
}

/// A budget crossed **after** a job was queued stops it at the claim,
/// and does not kill what is already running.
///
/// This is the gap the trigger cannot close. A job can sit in the queue
/// behind other tenants' work for as long as the fleet is busy, and the
/// organisation's month can end while it waits — so the dispatcher asks
/// again at the moment it would start a task, which is the last moment
/// anything can be stopped for free.
///
/// The queue is held shut with the concurrency limit rather than by
/// timing, so the sequence is exact: the second run is triggered while
/// there is budget, the budget is then spent, and only then does the
/// job become claimable.
#[test]
fn a_budget_crossed_while_a_job_waits_in_the_queue_stops_it_at_the_claim() {
    let w = world("runner-budget-queued");
    let db = ControlDb::open(&w.server.db_url).expect("a second session on the server's database");
    let org = registry::org_by_name(&db, "acme").unwrap().unwrap();
    // One at a time, so the second run's job stays queued while the
    // first holds the only slot.
    workflows::set_concurrency(&db, &org.id, Some(1)).unwrap();

    let first = push_with_git(&w, "main", &[(".weft/ci.yml", SLOW)]);
    let running = w.wait_launched(&first);

    // Triggered while the organisation still has an unlimited budget:
    // this run is queued, not blocked.
    w.branch("feature", "main");
    let second = w.commit("feature", "more", &[("src/lib.rs", "// hi\n")]);
    let queued = w.wait_runs("the second run to be queued", |runs| {
        runs.iter()
            .any(|r| r["commit_sha"] == second && r["state"] == "running")
    });
    let queued = queued
        .iter()
        .find(|r| r["commit_sha"] == second)
        .unwrap()
        .clone();
    assert_eq!(job(&queued, "test")["state"], "queued", "{queued}");

    // The month ends, and the slot opens. In that order.
    workflows::set_minutes_budget(&db, &org.id, Some(1)).unwrap();
    workflows::set_concurrency(&db, &org.id, None).unwrap();

    let stopped = w.wait_settled(&second);
    assert_eq!(stopped["state"], "cancelled", "{stopped}");
    assert_eq!(
        stopped["error"], "this organisation has used its 1 hosted-runner minutes for the month",
        "{stopped}"
    );

    // The build that was already running is untouched. The limit is a
    // ceiling on what may be *started*: killing a build halfway spends
    // the minutes anyway and destroys the only thing they bought.
    let (st, still) = w.server.get(
        &format!(
            "/v1/orgs/acme/repos/app/workflow-runs/{}",
            running["id"].as_str().unwrap()
        ),
        &w.admin,
    );
    assert_eq!(st, 200, "{still}");
    assert_eq!(still["state"], "running", "{still}");

    // One task for the whole test: the one that was already up.
    assert_eq!(w.ecs.run_tasks().len(), 1, "{:?}", w.ecs.run_tasks());
    assert!(w.ecs.stop_tasks().is_empty(), "nothing running was stopped");
    assert!(w.server.healthy());
}

/// The billing gate has the same gap as the budget, and closes it the
/// same way: a job queued while the org was paying, whose payment then
/// fails — `past_due`, which pauses hosted work on private repositories
/// — is stopped at the claim, in the words that say what to do, and the
/// build already running is left alone.
///
/// The trigger cannot meet this state on its own for a job that is
/// already queued: the dispatcher is the one place a lapsed org still
/// has hosted work to decline.
#[test]
fn a_payment_failing_while_a_job_waits_in_the_queue_stops_it_at_the_claim() {
    let stripe = stratum_testkit::fake_stripe::FakeStripe::start("whsec_test");
    let w = world_where("runner-card-queued", &stripe.env(), |server| {
        server
            .admin(&["admin", "set-plan", "--org", "acme", "--plan", "paid"])
            .unwrap();
    });
    let db = ControlDb::open(&w.server.db_url).expect("a second session on the server's database");
    let org = registry::org_by_name(&db, "acme").unwrap().unwrap();
    workflows::set_concurrency(&db, &org.id, Some(1)).unwrap();

    let first = push_with_git(&w, "main", &[(".weft/ci.yml", SLOW)]);
    let running = w.wait_launched(&first);

    w.branch("feature", "main");
    let second = w.commit("feature", "more", &[("src/lib.rs", "// hi\n")]);
    let queued = w.wait_runs("the second run to be queued", |runs| {
        runs.iter()
            .any(|r| r["commit_sha"] == second && r["state"] == "running")
    });
    let queued = queued
        .iter()
        .find(|r| r["commit_sha"] == second)
        .unwrap()
        .clone();
    assert_eq!(job(&queued, "test")["state"], "queued", "{queued}");

    // The payment fails, and the slot opens. In that order.
    w.server
        .admin(&["admin", "set-plan", "--org", "acme", "--plan", "past_due"])
        .unwrap();
    workflows::set_concurrency(&db, &org.id, None).unwrap();

    let stopped = w.wait_settled(&second);
    assert_eq!(stopped["state"], "cancelled", "{stopped}");
    assert_eq!(
        stopped["error"],
        "hosted workflows for private repositories are paused while this organisation's \
         last payment is unsettled — public repositories and self-hosted runners are \
         unaffected",
        "{stopped}"
    );
    let (st, still) = w.server.get(
        &format!(
            "/v1/orgs/acme/repos/app/workflow-runs/{}",
            running["id"].as_str().unwrap()
        ),
        &w.admin,
    );
    assert_eq!(st, 200, "{still}");
    assert_eq!(still["state"], "running", "{still}");
    assert_eq!(w.ecs.run_tasks().len(), 1, "{:?}", w.ecs.run_tasks());
    assert!(w.ecs.stop_tasks().is_empty(), "nothing running was stopped");
    assert!(w.server.healthy());
}

/// A maintainer approves a fork's workflows, they run at that tip, and
/// the next patchset is held again.
///
/// Four separate properties, and each is a way this could be wrong
/// while looking right:
///
/// * **Who may.** The contributor cannot approve their own change's
///   workflows — that would make the gate ornamental — and the door is
///   the one that lands, not the one that reviews. A reviewer with read
///   access may hold an opinion; starting a machine is a different
///   grant.
/// * **It actually runs.** Not "the row changed state": the build has
///   to clone the *target* repository with a job token and check out the
///   fork's commit, which only works because opening the change
///   transplanted those objects here and pinned `refs/patchsets/<sha>`.
///   A test that stopped at the API response would pass with an
///   unreachable commit.
/// * **The placeholder is gone**, in both tables. A `blocked` run left
///   behind means the real run is never created; a placeholder *check
///   row* left behind is a permanently queued check holding the land
///   gate for a run that was replaced, which is the failure nobody can
///   see from the page.
/// * **Per tip.** A new patchset is blocked again. An approval that
///   survived new commits would be a standing offer to run whatever the
///   contributor pushes next, which is the whole thing being defended
///   against.
#[test]
fn approving_a_forks_workflows_runs_them_at_that_tip_and_holds_the_next_one() {
    let f = fork_stack("runner-approve", true);
    let server = &f.server;
    let key = f.change_key.clone();
    let sha = f.sha.clone();
    let approve = format!("/v1/orgs/ada/repos/widget/changes/{key}/workflows/approve");
    let mut ada = Browser::signed_in(server, "ada@example.com", PASSWORD);
    let mut bob = Browser::signed_in(server, "bob@example.com", PASSWORD);

    // Through the narrowing the dashboard actually sends: the approval
    // panel asks for this change at this tip, and asking for a window
    // and filtering in the client is what loses the button on a busy
    // repository.
    let change_runs = |b: &mut Browser| -> Vec<serde_json::Value> {
        let (st, runs) = b.req(
            "GET",
            &format!("/v1/orgs/ada/repos/widget/workflow-runs?change_key={key}"),
            None,
        );
        assert_eq!(st, 200, "{runs}");
        runs["runs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["event"] == "change")
            .cloned()
            .collect()
    };

    let held = change_runs(&mut ada);
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["state"], "blocked", "{held:?}");
    let placeholder = held[0]["id"].as_str().unwrap().to_string();

    // The contributor cannot let their own contribution run. **404,
    // not 403**, and deliberately: `rest_repo_auth` masks a repository
    // from anyone who cannot do the thing being asked (R8), so a
    // stranger cannot use a refusal to learn what exists. This is the
    // same answer the land route gives bob, which is the point — one
    // door, one behaviour.
    let (st, out) = bob.req("POST", &approve, None);
    assert_eq!(
        st, 404,
        "bob may not approve his own change's workflows: {out}"
    );
    let (land_st, _) = bob.req(
        "POST",
        &format!("/v1/orgs/ada/repos/widget/changes/{key}/land"),
        None,
    );
    assert_eq!(land_st, st, "approval and landing must refuse alike");
    assert_eq!(
        change_runs(&mut ada)[0]["state"],
        "blocked",
        "a refused approval changed something"
    );

    let (st, out) = ada.req("POST", &approve, None);
    assert_eq!(st, 202, "{out}");
    let started = out["runs"].as_array().unwrap();
    assert_eq!(started.len(), 1, "{out}");
    assert_ne!(started[0]["id"], placeholder.as_str(), "{out}");
    assert_eq!(started[0]["commit_sha"], sha, "{out}");
    assert_eq!(started[0]["state"], "running", "{out}");

    // It really runs: the runner clones ada's repository with a job
    // token and checks out bob's commit, which is only possible because
    // opening the change put those objects here.
    let deadline = Instant::now() + Duration::from_secs(60);
    let done = loop {
        let runs = change_runs(&mut ada);
        if let Some(r) = runs.iter().find(|r| r["state"] != "running") {
            break r.clone();
        }
        assert!(
            Instant::now() < deadline,
            "waited 60s for the approved run: {runs:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(done["state"], "passed", "{done}");
    assert_eq!(done["commit_sha"], sha, "{done}");
    let approved_job = job(&done, "test")["id"].as_str().unwrap().to_string();
    assert!(
        f.ecs
            .run_tasks()
            .iter()
            .any(|c| c.env()["STRATUM_JOB_ID"] == approved_job),
        "no task was launched for the approved job: {:?}",
        f.ecs.run_tasks()
    );

    // "Blocked, then approved by ada" has to survive the placeholder
    // row's deletion, so the approval is in the audit log — who, which
    // change, which tip, and which files were let go.
    let db = ControlDb::open(&server.db_url).unwrap();
    let ada_org = registry::org_by_name(&db, "ada").unwrap().unwrap();
    let approvals = stratum_control::audit::query(
        &db,
        &ada_org.id,
        &stratum_control::audit::AuditQuery {
            action: Some("workflow.approved"),
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    assert_eq!(
        approvals[0].user_email.as_deref(),
        Some("ada@example.com"),
        "the trail has to name who let a stranger's code onto a machine: {:?}",
        approvals[0]
    );
    let context = approvals[0].context.clone().unwrap_or_default();
    assert_eq!(context["change_key"], key.as_str(), "{context}");
    assert_eq!(context["commit"], sha.as_str(), "{context}");
    assert_eq!(
        context["files"],
        serde_json::json!([".weft/ci.yml"]),
        "an approval that does not say which files is not a trail: {context}"
    );

    // The placeholder is gone from both tables: one run at this tip, and
    // no queued check row for the file left holding the land gate.
    assert_eq!(change_runs(&mut ada).len(), 1, "the placeholder survived");
    let (st, checks) = ada.req(
        "GET",
        &format!("/v1/orgs/ada/repos/widget/commits/{sha}/checks"),
        None,
    );
    assert_eq!(st, 200, "{checks}");
    let rows = checks["runs"].as_array().unwrap();
    assert!(
        rows.iter().all(|c| c["name"] != ".weft/ci.yml"),
        "the blocked file's check row is still there: {checks}"
    );
    assert!(
        rows.iter()
            .any(|c| c["name"] == "ci / test" && c["state"] == "passing"),
        "{checks}"
    );

    // Nothing is left to approve at this tip, and saying so is not an
    // error the dashboard has to explain twice.
    let (st, out) = ada.req("POST", &approve, None);
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"], "no workflows are waiting for approval at this change's current tip",
        "{out}"
    );

    // A new patchset is a new file nobody has read. Held again.
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/bob/repos/widget/commits",
        Some(serde_json::json!({
            "message": format!("more\n\n{CHANGE_ID}"),
            "branch": "contrib",
            "operations": [{ "op": "put", "path": "src/more.rs", "content": "// more\n" }],
        })),
    );
    assert_eq!(st, 201, "{body}");
    let next = body["commit"].as_str().unwrap().to_string();
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/changes",
        Some(serde_json::json!({ "from": "contrib", "source": "bob/widget" })),
    );
    // 201: a *second patchset* of the same change — the trailer keeps
    // the review identity, which is what makes this a per-tip test
    // rather than two unrelated changes.
    assert_eq!(st, 201, "{body}");
    assert_eq!(body["change"]["key"], key.as_str(), "{body}");
    assert_eq!(body["patchset"]["number"], 2, "{body}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let held = loop {
        let runs = change_runs(&mut ada);
        if let Some(r) = runs.iter().find(|r| r["commit_sha"] == next.as_str()) {
            break r.clone();
        }
        assert!(
            Instant::now() < deadline,
            "waited 30s for the new patchset's run: {runs:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(held["state"], "blocked", "{held}");
    assert_eq!(
        held["error"],
        "this change comes from a fork; a maintainer has to approve its workflows before they run",
        "{held}"
    );
    assert_eq!(held["blocked_reason"], "fork", "{held}");

    // A change key nobody minted is a 404 naming the key, the same
    // answer every other id in this API gives — the caller already
    // reached the repository, so there is nothing to mask beyond the
    // change itself, and the docs promise this shape.
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/ada/repos/widget/changes/I0000000000000000000000000000000000000000/workflows/approve",
        None,
    );
    assert_eq!(st, 404, "{out}");
    assert_eq!(
        out["error"], "no change \"I0000000000000000000000000000000000000000\"",
        "{out}"
    );

    // And a change that is no longer open cannot have its workflows
    // started: there is nothing to approve *for*, and starting compute
    // for a change nobody can land is the button-that-cannot-help case
    // in its purest form. 409 with the state in it, so the page can say
    // which state without inventing one.
    let (st, out) = ada.req(
        "POST",
        &format!("/v1/orgs/ada/repos/widget/changes/{key}/abandon"),
        None,
    );
    assert_eq!(st, 204, "{out}");
    let (st, out) = ada.req("POST", &approve, None);
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["error"], "change is abandoned", "{out}");

    assert!(server.healthy());
}

// ---------------------------------------------------------------------
// The real binary, as a self-hosted agent
// ---------------------------------------------------------------------

/// A workflow for a machine the organisation registered, whose steps
/// prove the checkout happened rather than only that a shell ran.
const SELF_HOSTED_CI: &str = "\
name: ci
on: push
jobs:
  test:
    runs-on: [self-hosted, gpu]
    steps:
      - name: Env
        run: echo \"job=$WEFT_JOB sha=$WEFT_SHA event=$WEFT_EVENT\"
      - name: Files
        run: cat README.md
";

/// `weft-runner register` then `weft-runner run`, as a separate
/// process on somebody else's machine, all the way to a green check.
///
/// The one case in this file where **nothing** of ours is on the client
/// side of the wire: no fake ECS starting the process with an
/// environment we wrote, no test helper making the HTTP calls. The
/// binary reads a `.runner` file it wrote itself, long-polls for work,
/// clones over HTTP with the job token it was handed, runs the steps,
/// and reports — and the four seams that only exist when somebody else
/// is on the other end (the registration exchange, the credential, the
/// claim's long poll, the clone from outside) are exercised by the thing
/// that will actually be exercising them.
#[test]
fn the_real_binary_registers_takes_a_job_and_reports_a_verdict() {
    // `STRATUM_RUNNER_URL` is where *our* fleet reaches this server — a
    // private listener, or `host.docker.internal` on a laptop — and the
    // job's clone URL is built from it. A machine somebody else owns is
    // outside that network by definition; the one address it has proved
    // it can reach is the one it registered with. The manual pass found
    // every self-hosted job failing its checkout with "could not resolve
    // host: host.docker.internal" for exactly this reason.
    let w = world_with(
        "runner-selfhosted",
        &[
            ("STRATUM_RUNNER_CLAIM_WAIT_MS", "700".into()),
            ("STRATUM_RUNNER_URL", "http://fleet.private.invalid".into()),
        ],
    );
    let bin = runner_bin_next_to(env!("CARGO_BIN_EXE_stratum-server"));
    let dir = w.scratch.path().join("agent");
    std::fs::create_dir_all(&dir).unwrap();

    // The operator mints a token in the dashboard and pastes the command
    // the page shows them.
    let (st, minted) = w
        .server
        .post("/v1/orgs/acme/runners/registration-token", &w.admin, None);
    assert_eq!(st, 201, "{minted}");
    let token = minted["token"].as_str().unwrap();

    let out = std::process::Command::new(&bin)
        .args([
            "register",
            "--url",
            &w.server.base,
            "--token",
            token,
            "--name",
            "e2e-box",
            "--labels",
            "gpu",
            "--dir",
            dir.to_str().unwrap(),
        ])
        .output()
        .expect("run weft-runner register");
    assert!(
        out.status.success(),
        "register exited {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("registered e2e-box as") && said.contains("in group default"),
        "register printed nothing an operator could act on: {said:?}"
    );

    // The credential it wrote is a file only its owner can read: it is a
    // long-lived bearer secret sitting on a shared build box.
    let state = dir.join(".runner");
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).expect("register wrote .runner")).unwrap();
    assert_eq!(written["name"], "e2e-box", "{written}");
    assert!(
        written["credential"]
            .as_str()
            .unwrap_or_default()
            .starts_with("weftr_"),
        "{written}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&state).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the credential file is readable by others");
    }

    // The server agrees it exists, and shows it as something an operator
    // can see rather than only as a row.
    let (st, listed) = w.server.get("/v1/orgs/acme/runners", &w.admin);
    assert_eq!(st, 200, "{listed}");
    let row = &listed["runners"][0];
    assert_eq!(row["name"], "e2e-box", "{listed}");
    // `self-hosted` first, the custom label last, and this machine's own
    // OS and architecture in between — asserted as membership rather than
    // as a literal, because the binary reports the host it is really on
    // and this suite runs on more than one.
    let labels: Vec<&str> = row["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert_eq!(labels.len(), 4, "{listed}");
    assert_eq!(labels[0], "self-hosted", "{listed}");
    assert!(
        ["linux", "macos", "windows"].contains(&labels[1]),
        "{listed}"
    );
    assert!(["x64", "arm64"].contains(&labels[2]), "{listed}");
    assert_eq!(labels[3], "gpu", "{listed}");
    assert_eq!(row["os"], labels[1], "{listed}");
    assert_eq!(row["arch"], labels[2], "{listed}");

    // Now start the agent and give it something to do.
    let log = dir.join("agent.log");
    let mut agent = std::process::Command::new(&bin)
        .args(["run", "--dir", dir.to_str().unwrap()])
        .stdout(std::fs::File::create(&log).unwrap())
        .stderr(std::fs::File::create(dir.join("agent.err")).unwrap())
        .spawn()
        .expect("run weft-runner run");

    let sha = push_with_git(
        &w,
        "main",
        &[
            (".weft/ci.yml", SELF_HOSTED_CI),
            ("README.md", "hello from the repo\n"),
        ],
    );
    let run = w.wait_settled(&sha);
    let stop = |agent: &mut std::process::Child| {
        // SIGTERM rather than a kill: an idle agent's contract is to
        // exit 0 on it, and a killed child never writes its coverage
        // profile.
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &agent.id().to_string()])
            .status();
        let _ = agent.wait();
    };
    if run["state"] != "passed" {
        stop(&mut agent);
        panic!(
            "the run did not pass: {run}\n--- agent stdout ---\n{}\n--- agent stderr ---\n{}",
            std::fs::read_to_string(&log).unwrap_or_default(),
            std::fs::read_to_string(dir.join("agent.err")).unwrap_or_default(),
        );
    }

    let test = job(&run, "test");
    assert_eq!(test["pool"], "self_hosted", "{test}");
    assert_eq!(test["runner"]["name"], "e2e-box", "{test}");
    assert_eq!(
        test["labels"],
        serde_json::json!(["self-hosted", "gpu"]),
        "{test}"
    );

    // The steps really ran, against a real checkout of the pushed
    // commit — the whole reason this test spawns a process rather than
    // asserting on rows.
    let text = w.log(test["id"].as_str().unwrap());
    assert!(text.contains(&format!("sha={sha}")), "{text}");
    assert!(text.contains("event=push"), "{text}");
    assert!(text.contains("hello from the repo"), "{text}");
    // It cloned from the address it registered with, not the fleet's.
    assert!(!text.contains("fleet.private.invalid"), "{text}");

    // And the agent narrated what it did, which is all an operator
    // watching a terminal has.
    let said = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        said.contains("listening as e2e-box"),
        "the agent said nothing at startup: {said:?}"
    );

    // The ending an operator actually performs: the machine is
    // decommissioned from the page, and the process finds out on its
    // next call and stops itself. Nothing is signalled from here — a
    // removed runner is one whose credential has just died, which is
    // exactly what a stolen credential looks like from the server's
    // side, and the agent has to be able to tell the difference and
    // give up rather than hammer.
    let id = row["id"].as_str().unwrap().to_string();
    let (st, out) = w
        .server
        .delete(&format!("/v1/orgs/acme/runners/{id}"), &w.admin);
    assert_eq!(st, 204, "{out}");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let code = loop {
        match agent.try_wait().expect("wait on the agent") {
            Some(status) => break status.code(),
            None if std::time::Instant::now() >= deadline => {
                stop(&mut agent);
                panic!(
                    "a removed runner kept running\n--- agent stdout ---\n{}\n--- agent stderr ---\n{}",
                    std::fs::read_to_string(&log).unwrap_or_default(),
                    std::fs::read_to_string(dir.join("agent.err")).unwrap_or_default(),
                );
            }
            None => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    };
    let said = format!(
        "{}{}",
        std::fs::read_to_string(&log).unwrap_or_default(),
        std::fs::read_to_string(dir.join("agent.err")).unwrap_or_default(),
    );
    // 2 is the contract's exit code for "this machine is no longer
    // registered", and it is distinct from 0 so that whatever supervises
    // the agent does not restart it into a loop it cannot win.
    assert_eq!(code, Some(2), "a removed runner exited {code:?}: {said}");
    assert!(
        said.contains("this runner has been removed"),
        "a removed runner exited without saying why: {said}"
    );

    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// The registry proxy, in a real job
// ---------------------------------------------------------------------

/// A workflow that reads the registry configuration the runner wrote and
/// then installs through it.
///
/// `cat`ting the file is not decoration: the whole point of the design is
/// that what lands on disk names loopback and carries no credential, so
/// the file's contents are the claim. The fetch is the other half —
/// a config nobody can use proves nothing.
const REGISTRY_CI: &str = "\
name: ci
on: push
jobs:
  test:
    steps:
      - name: Config
        run: cat ../.npmrc
      - name: Resolve
        run: curl -sS \"$(sed -n 's/^@acme:registry=//p' ../.npmrc)@acme%2fwidget\"
      - name: Named
        run: echo \"registry host $WEFT_REGISTRY\" && curl -fsS \"http://$WEFT_REGISTRY/npm/acme/@acme%2fwidget\" >/dev/null && echo named-proxy-answers
";

/// Standard base64, to build an `_attachments` body the way npm does.
fn npm_b64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            A[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The job's registry, all the way through: the organization has npm
/// switched on, so the assignment carries a registry, so the runner binds
/// a loopback proxy for the job's lifetime and writes the client
/// configuration beside the checkout — and a step resolves a private
/// package through it.
///
/// The credential is the part being proved. It is minted for the job and
/// lives in the runner process's memory; what a step can read is a file
/// naming `127.0.0.1` and a placeholder, and the log — which the step
/// itself wrote — contains no token at all.
#[test]
fn a_job_whose_org_has_a_registry_resolves_through_the_runners_own_proxy() {
    let w = world("runner-registry");

    let (st, out) = w.server.req(
        "PUT",
        "/v1/orgs/acme/packages/ecosystems",
        &w.admin,
        Some(serde_json::json!({ "ecosystem": "npm", "mode": "private" })),
    );
    assert_eq!(st, 200, "enabling npm: {out}");

    let tarball = b"a tarball's worth of bytes, near enough for a test";
    let (st, out) = w.server.req(
        "PUT",
        "/v1/registry/npm/acme/@acme%2fwidget",
        &w.admin,
        Some(serde_json::json!({
            "_id": "@acme/widget",
            "name": "@acme/widget",
            "dist-tags": { "latest": "1.2.3" },
            "versions": {
                "1.2.3": { "name": "@acme/widget", "version": "1.2.3", "license": "MIT" }
            },
            "_attachments": {
                "widget-1.2.3.tgz": {
                    "content_type": "application/octet-stream",
                    "data": npm_b64(tarball),
                    "length": tarball.len(),
                }
            }
        })),
    );
    assert_eq!(st, 201, "publishing: {out}");

    let sha = push_with_git(&w, "main", &[(".weft/ci.yml", REGISTRY_CI)]);
    let run = w.wait_settled(&sha);
    assert_eq!(run["state"], "passed", "{run}");
    let test = job(&run, "test");
    assert_eq!(test["state"], "passed", "{test}");

    let log = w.log(test["id"].as_str().unwrap());
    assert!(
        log.contains("@acme:registry=http://127.0.0.1:"),
        "the runner wrote no npm configuration, or not one pointing at itself:\n{log}"
    );
    assert!(
        log.contains("_authToken=weft-local-proxy"),
        "the configuration did not carry the placeholder credential:\n{log}"
    );
    // The real credential never reaches the job's side of the wire. Every
    // token this product mints is `weft_<id>_<secret>`, so the absence of
    // that prefix in a log a *step* wrote is the property.
    assert!(
        !log.contains("weft_"),
        "a credential reached a step's output:\n{log}"
    );
    // `$WEFT_REGISTRY` names the same proxy — what `docker push` needs,
    // since an image's name starts with its registry's host.
    assert!(
        log.contains("registry host 127.0.0.1:") && log.contains("named-proxy-answers"),
        "the step was not told where the registry proxy listens:\n{log}"
    );
    // And the proxy is really serving: the packument came back through
    // loopback, with the credential the proxy attached on its way out.
    assert!(
        log.contains("\"1.2.3\""),
        "the private package did not resolve through the proxy:\n{log}"
    );

    assert!(w.server.healthy());
}
