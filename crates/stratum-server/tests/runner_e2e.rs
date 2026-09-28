//! The workflow loop, end to end, through a real `weft-runner` agent the
//! organisation registered — with nothing of ours on the client side.
//!
//! Every job in this edition runs on a machine somebody registered, so
//! every case here goes the whole way round the way an operator's box
//! does: a registration token minted over the API, `weft-runner
//! register` exchanging it for the machine's own credential, `weft-runner
//! run` long-polling `POST /v1/runners/claim` as a separate process. A
//! commit lands by the real `git` CLI or the commit API, the trigger
//! writes a run, the agent claims the job, fetches the repository over
//! HTTP with the job token it was handed, runs the steps with the shell,
//! streams its log back, reports a verdict — and the verdict shows up
//! where a person looks for it: the run page, the commit's checks, and
//! the land gate.
//!
//! There is no fake anywhere on the runner's side of the wire. What the
//! suite knows about the agent is what the agent printed and what the
//! server recorded; the agent keeps its job tokens in memory and puts
//! them nowhere a test could read, and that is the design being tested
//! rather than an obstacle to it. A cancellation is proved the way an
//! operator would see it — the step's process is gone and the agent has
//! moved on — not by a row changing state.
//!
//! The situations are the ones that happen. A push to trunk; a step that
//! fails and the change it blocks; a second push to a branch while the
//! first is still building; a machine that takes a job and goes quiet; a
//! machine that is restarted mid-build; a workflow that does not parse;
//! a change from a fork; a repository no machine can serve.

use std::path::PathBuf;
use std::time::{Duration, Instant};
use stratum_control::{registry, ControlDb};
use stratum_testkit::browser::Browser;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::runner_bin::{registration_token, runner_bin_next_to, Agent, Registered};
use stratum_testkit::{wait_until, Minio, Server};

const PASSWORD: &str = "a long enough password";

/// A workflow whose steps prove the runner's environment rather than
/// merely that a shell ran: the job key, the commit, the ref and the
/// event all have to reach the step, and the checkout has to be the
/// commit that was pushed. No `runs-on:`, which asks for any machine the
/// organisation registered — the same thing `[self-hosted]` says.
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

/// A step that never ends on its own, and says who it is.
///
/// The pid is how a test proves a cancellation reached the machine: the
/// process that printed it is gone. The ticking is not decoration. A
/// runner learns that its job is over from the answer to its next call,
/// and a step that prints nothing gets a heartbeat only every thirty
/// seconds; one that prints is flushed every second, so a cancel reaches
/// it inside that — the same as any real build that is producing output.
const SLOW: &str = "\
name: ci
on: push
jobs:
  test:
    steps:
      - name: Wait
        run: |
          echo \"waiting pid=$$\"
          while true; do echo tick; sleep 0.2; done
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
        run: |
          echo \"waiting pid=$$\"
          while true; do echo tick; sleep 0.2; done
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

/// The server's own claim poll: a claim that finds nothing looks again
/// this often (`CLAIM_POLL` in the runners API) until its long poll runs
/// out, and an agent asks again at once. A test proving a *negative* —
/// nothing more was taken — waits a few of these, because that is how
/// long a mistaken job would take to reach an agent that is listening.
const CLAIM_POLL: Duration = Duration::from_millis(500);

/// How long a cancellation may take to reach an agent whose step is
/// printing: the log is flushed every second and the answer to that
/// flush is the 410. Generous for a loaded machine, and far short of
/// anything a step in this file would reach by finishing on its own —
/// they never do.
const REACH: Duration = Duration::from_secs(30);

/// The environment every server in this file starts with.
fn fast() -> Vec<(&'static str, String)> {
    vec![
        // The sweeper's tick. Every wait in this file is on an
        // observable, so this is pure latency.
        ("STRATUM_RUNNER_POLL_SECS", "0.1".into()),
        ("STRATUM_LAND_POLL_SECS", "0.1".into()),
        // The long poll's length. An idle agent holds a claim open for
        // this long, and so does the server's shutdown when the test
        // ends; the claim itself looks for work every half second
        // whatever this is.
        ("STRATUM_RUNNER_CLAIM_WAIT_MS", "700".into()),
    ]
}

fn runner_bin() -> PathBuf {
    runner_bin_next_to(env!("CARGO_BIN_EXE_stratum-server"))
}

struct World {
    /// First, so they are stopped while the server they call is still
    /// there: struct fields drop in declaration order.
    agents: Vec<Agent>,
    server: Server,
    admin: String,
    scratch: Scratch,
    bin: PathBuf,
}

/// A server, an `acme` organisation with an `app` repository, and one
/// machine — `box-1` — registered to it and listening.
fn world(hint: &str) -> World {
    world_with(hint, &[], &["box-1"])
}

/// `world`, with more environment and the named agents (none, for a case
/// that registers its own or needs there to be none).
fn world_with(hint: &str, extra: &[(&str, String)], agents: &[&str]) -> World {
    let minio = Minio::shared();
    let bucket = minio.bucket(hint);
    let scratch = Scratch::new(hint);
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .envs(&fast())
        .envs(extra)
        .start();
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    assert_eq!(st, 201, "{out}");
    let mut w = World {
        agents: Vec::new(),
        server,
        admin,
        scratch,
        bin: runner_bin(),
    };
    for name in agents {
        w.attach(name, &[]);
    }
    w
}

impl World {
    /// Register one more machine and start it, as its operator would.
    fn attach(&mut self, name: &str, labels: &[&str]) -> &Agent {
        let token = registration_token(&self.server, &self.admin, "acme");
        let dir = self.scratch.path().join(format!("agent-{name}"));
        let agent = Agent::attach(&self.bin, &self.server.base, &token, name, labels, &dir);
        self.agents.push(agent);
        self.agents.last().expect("just pushed")
    }

    fn agent(&self, name: &str) -> &Agent {
        self.agents
            .iter()
            .find(|a| a.name() == name)
            .unwrap_or_else(|| panic!("no agent {name}"))
    }

    /// What every agent said — the half of a failure the run list cannot
    /// show.
    fn said(&self) -> String {
        self.agents.iter().map(Agent::said).collect()
    }

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
    /// Giving up prints the runs *and* what the agents said, which
    /// between them are the whole story.
    fn wait_runs(
        &self,
        what: &str,
        mut pred: impl FnMut(&[serde_json::Value]) -> bool,
    ) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let runs = self.runs();
            if pred(&runs) {
                return runs;
            }
            assert!(
                Instant::now() < deadline,
                "waited 60s for {what}; runs were:\n{}\n{}",
                serde_json::to_string_pretty(&runs).unwrap(),
                self.said()
            );
            std::thread::sleep(Duration::from_millis(100));
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

    /// The build of `sha` once a machine has it and its step is running:
    /// the job is `running` on a named runner and the step has printed
    /// its pid into the log. That is the moment something can be done
    /// *to* a build, as opposed to a job that has merely been claimed.
    fn wait_step(&self, sha: &str) -> Step {
        let mut found: Option<Step> = None;
        self.wait_runs(&format!("the step of {sha} to be running"), |runs| {
            let Some(run) = runs.iter().find(|r| r["commit_sha"] == sha) else {
                return false;
            };
            let Some(job) = run["jobs"]
                .as_array()
                .and_then(|js| js.iter().find(|j| j["state"] == "running"))
            else {
                return false;
            };
            let Some(runner) = job["runner"]["name"].as_str() else {
                return false;
            };
            let id = job["id"].as_str().unwrap().to_string();
            let Some(pid) = step_pid(&self.log(&id)) else {
                return false;
            };
            found = Some(Step {
                run: run.clone(),
                job: id,
                runner: runner.to_string(),
                pid,
            });
            true
        });
        found.expect("the step we waited for")
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

    /// The job tokens minted for `job_id`, one per attempt — as the
    /// organisation's token list shows them, which is where an admin
    /// looking for a live credential would look.
    fn job_tokens(&self, job_id: &str) -> Vec<serde_json::Value> {
        let (st, out) = self.server.get("/v1/orgs/acme/tokens", &self.admin);
        assert_eq!(st, 200, "{out}");
        let label = format!("ci:{job_id}");
        out["tokens"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t["label"] == label.as_str())
            .collect()
    }

    fn db(&self) -> postgres::Client {
        postgres::Client::connect(&self.server.db_url, postgres::NoTls)
            .expect("connect to the server's database")
    }

    /// Let attempt `attempt` of a job's lease run out now rather than in
    /// two minutes; whether there was a live lease to lapse.
    ///
    /// The lease a runner renews with every log flush is a fixed 120
    /// seconds (`LEASE_MS` in the runner API), and there is no knob for
    /// it — nor should there be one only a test wants. What decides a
    /// reclaim is `lease_until < now` on the job's own row, so moving
    /// that value back exercises exactly the comparison the claim makes,
    /// the same move `a_job_whose_runner_went_quiet_past_its_timeout…`
    /// makes for the timeout. Only ever done to an attempt whose machine
    /// has been paused or stopped first, so nothing it does from then on
    /// renews it — and scoped to that attempt, so it can never touch the
    /// attempt that replaced it.
    fn lapse(&self, db: &mut postgres::Client, job_id: &str, attempt: i64) -> bool {
        db.execute(
            "UPDATE workflow_jobs SET lease_until = 0 \
             WHERE id = $1 AND state = 'running' AND attempts = $2 AND lease_until > 0",
            &[&job_id, &attempt],
        )
        .expect("lapse the job's lease")
            > 0
    }

    /// Hold attempt `attempt` of `job_id`'s lease lapsed until `done`
    /// holds of the run list; the list then.
    ///
    /// Held, not set once. The machine is paused before this is called,
    /// but a log flush it sent just before the pause can still be in the
    /// server's hands, and if that lands after a single `UPDATE` the
    /// lease is back to two minutes and the reclaim being waited for
    /// never comes. Re-lapsing on every poll closes that window whatever
    /// its width.
    fn lapse_until(
        &self,
        job_id: &str,
        attempt: i64,
        what: &str,
        mut done: impl FnMut(&[serde_json::Value]) -> bool,
    ) -> Vec<serde_json::Value> {
        let mut db = self.db();
        let mut lapsed = false;
        let runs = self.wait_runs(what, |runs| {
            if done(runs) {
                return true;
            }
            lapsed |= self.lapse(&mut db, job_id, attempt);
            false
        });
        assert!(
            lapsed,
            "attempt {attempt} of job {job_id} never had a lease to lapse; what \
             happened next is not what this test thinks it is"
        );
        runs
    }
}

/// A build in progress: its run, its job, the machine it is on, and the
/// pid of the step's shell on that machine.
struct Step {
    run: serde_json::Value,
    job: String,
    runner: String,
    pid: u32,
}

/// The pid a `SLOW` step printed, if it has.
fn step_pid(log: &str) -> Option<u32> {
    let rest = &log[log.find("waiting pid=")? + "waiting pid=".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Whether the step that printed `pid` is still running: the process
/// exists, is not a zombie, and is still the shell running the `SLOW`
/// loop — the last so that a pid the kernel has since handed to
/// something else cannot pass for it. Asked of `ps` rather than `/proc`
/// so the answer means the same on a Mac as on Linux.
fn step_alive(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    !line.is_empty() && !line.starts_with('Z') && line.contains("tick")
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
    assert_eq!(run["state"], "passed", "{run}\n{}", w.said());
    assert_eq!(run["event"], "push", "{run}");
    assert_eq!(run["ref_name"], "main", "{run}");
    assert_eq!(run["name"], "ci", "{run}");
    assert_eq!(run["file"], ".weft/ci.yml", "{run}");
    let test = job(&run, "test");
    let id = test["id"].as_str().unwrap().to_string();
    assert_eq!(test["state"], "passed", "{test}");
    assert_eq!(test["attempts"], 1, "{test}");
    assert!(test["error"].is_null(), "{test}");
    // It ran on the machine the organisation registered, routed by the
    // label set a file with no `runs-on:` asks for.
    assert_eq!(test["pool"], "self_hosted", "{test}");
    assert_eq!(test["runner"]["name"], "box-1", "{test}");
    assert_eq!(test["labels"], serde_json::json!(["self-hosted"]), "{test}");

    // The log is what the steps printed, in the runner's format, and
    // the environment the steps saw is the one the contract promises.
    let log = w.log(&id);
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

    // The agent took exactly this job and was done with it cleanly: a
    // verdict it could not report is a non-zero exit of the job, and
    // the agent prints the code after the id when there is one.
    let agent = w.agent("box-1");
    agent.wait_until("the agent to be done with the job", REACH, |a| {
        a.finished().contains(&id)
    });
    assert_eq!(agent.took(), vec![id.clone()], "{}", agent.said());
    assert!(
        agent.stdout().contains(&format!("finished job {id}\n")),
        "the job did not end cleanly on the machine:\n{}",
        agent.said()
    );

    // The job token died with the job. The steps ran untrusted code as
    // the same user the runner is, so anything the runner held has to
    // be assumed read; a credential that outlives the job is a
    // credential somebody may still be holding. It was minted for this
    // job alone — read-only, bound to this repository — and the token
    // list, where an admin looks for live credentials, says it is gone.
    // (What the runner routes answer that dead token is `runner_api_e2e`'s
    // to pin; the agent never lets a test hold it.)
    let tokens = w.job_tokens(&id);
    assert_eq!(tokens.len(), 1, "one attempt, one token: {tokens:?}");
    assert_eq!(
        tokens[0]["scopes"],
        serde_json::json!(["repo:read"]),
        "{tokens:?}"
    );
    assert!(!tokens[0]["repo_id"].is_null(), "unbound: {tokens:?}");
    assert!(
        !tokens[0]["revoked_at"].is_null(),
        "a finished job's token is still live: {tokens:?}"
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
    // what this window is for is the listening agent, which would take
    // a second job within a claim poll of one existing. Three of them,
    // and it has not.
    std::thread::sleep(3 * CLAIM_POLL);
    let runs = w.runs();
    let for_sha: Vec<_> = runs.iter().filter(|r| r["commit_sha"] == sha).collect();
    assert_eq!(
        for_sha.len(),
        1,
        "one commit, one run per workflow file: {runs:?}"
    );
    let agent = w.agent("box-1");
    assert_eq!(agent.took().len(), 1, "{}", agent.said());
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

    // Both runs — the branch push and the change — were each a job the
    // machine took, one after the other.
    w.wait_runs("the push run to settle", |runs| {
        runs.iter().all(|r| r["state"] != "running")
    });
    let agent = w.agent("box-1");
    assert_eq!(agent.took().len(), 2, "{}", agent.said());
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

/// That a cancelled build really stopped on the machine: the step's
/// process is gone, and the agent is done with the job — in its own
/// words, `cancelled, exiting quietly`, which is the ending a 410 gets
/// and not the one a verdict gets.
fn assert_stopped_on_the_machine(agent: &Agent, step: &Step) {
    wait_until(
        &format!(
            "the step of job {} (pid {}) to be killed",
            step.job, step.pid
        ),
        REACH,
        || !step_alive(step.pid),
    );
    agent.wait_until(
        &format!("{} to let go of job {}", agent.name(), step.job),
        REACH,
        |a| a.finished().contains(&step.job),
    );
    assert!(
        agent
            .stderr()
            .contains(&format!("job {}: cancelled, exiting quietly", step.job)),
        "the agent did not end the job as a cancellation:\n{}",
        agent.said()
    );
}

#[test]
fn a_second_push_to_a_branch_cancels_the_build_of_the_first() {
    let w = world("runner-supersede");
    w.commit("main", "seed", &[("README.md", "x\n")]);
    w.branch("feature", "main");
    let first = w.commit("feature", "slow", &[(".weft/ci.yml", SLOW)]);
    // As far as the step: what is cancelled is a build in progress, not
    // a job that was merely claimed.
    let step = w.wait_step(&first);
    assert_eq!(step.runner, "box-1");

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

    // The cancellation reached the machine. Nothing can be sent *to* a
    // self-hosted runner, so it learns from the answer to its next call —
    // and if that answer were anything but 410 (401 is what it used to
    // be, and a runner retries a 401) the build superseding was meant to
    // save would still be running every step it had left.
    let agent = w.agent("box-1");
    assert_stopped_on_the_machine(agent, &step);
    // Its credential went with it.
    let tokens = w.job_tokens(&step.job);
    assert!(
        tokens.iter().all(|t| !t["revoked_at"].is_null()),
        "a superseded job's token is still live: {tokens:?}"
    );

    // And the machine it freed takes the build that replaced it, which
    // runs to the end. One machine: the second build could not have
    // started unless the first had really let go.
    let run2 = w.wait_settled(&second);
    assert_eq!(run2["state"], "passed", "{run2}");
    let job2 = job(&run2, "test")["id"].as_str().unwrap().to_string();
    assert_eq!(
        agent.took(),
        vec![step.job.clone(), job2],
        "{}",
        agent.said()
    );
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
    let step = w.wait_step(&first);
    assert_eq!(step.run["event"], "change", "{}", step.run);

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
    let agent = w.agent("box-1");
    assert_stopped_on_the_machine(agent, &step);

    let run2 = w.wait_settled(&second);
    assert_eq!(run2["state"], "passed", "{run2}");
    assert_eq!(run2["event"], "change", "{run2}");
    assert_eq!(w.runs().len(), 2, "one run per patchset: {:?}", w.runs());
    assert_eq!(agent.took().len(), 2, "{}", agent.said());
    assert!(w.server.healthy());
}

/// Trunk's history is what gets deployed, and every commit on it
/// deserves its own verdict — so two builds of trunk run side by side,
/// which takes two machines: a runner runs one job at a time.
#[test]
fn a_second_push_to_trunk_does_not_cancel_the_first() {
    let w = world_with("runner-trunk", &[], &["box-1", "box-2"]);
    let first = w.commit("main", "slow", &[(".weft/ci.yml", SLOW)]);
    let one = w.wait_step(&first);
    let second = w.commit("main", "more", &[("README.md", "y\n")]);
    let two = w.wait_step(&second);

    // Both are building, each on its own machine, and neither was
    // touched by the other's arrival.
    assert_ne!(one.runner, two.runner, "one machine ran two jobs at once");
    let runs = w.runs();
    assert!(
        runs.iter().all(|r| r["state"] == "running"),
        "a trunk build was cancelled: {runs:?}"
    );
    assert!(step_alive(one.pid) && step_alive(two.pid));

    // Cancelling one by hand stops exactly that one and not the other.
    let cancel = format!(
        "/v1/orgs/acme/repos/app/workflow-runs/{}/cancel",
        one.run["id"].as_str().unwrap()
    );
    let (st, out) = w.server.post(&cancel, &w.admin, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["state"], "cancelled", "{out}");
    let (st, out) = w.server.post(&cancel, &w.admin, None);
    assert_eq!(st, 409, "cancelling twice: {out}");

    assert_stopped_on_the_machine(w.agent(&one.runner), &one);
    assert!(
        step_alive(two.pid),
        "cancelling one trunk build killed the other's step"
    );
    let runs = w.runs();
    let still: Vec<_> = runs.iter().filter(|r| r["state"] == "running").collect();
    assert_eq!(still.len(), 1, "{runs:?}");
    assert_eq!(still[0]["commit_sha"], second, "{runs:?}");
    assert!(w.server.healthy());
}

#[test]
fn deleting_a_branch_cancels_its_build() {
    let w = world("runner-delete");
    w.commit("main", "seed", &[("README.md", "x\n")]);
    w.branch("feature", "main");
    let sha = w.commit("feature", "slow", &[(".weft/ci.yml", SLOW)]);
    let step = w.wait_step(&sha);

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
    assert_stopped_on_the_machine(w.agent("box-1"), &step);
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// A machine that goes quiet
// ---------------------------------------------------------------------

/// How long the sweep may take once its input is already overdue: the
/// sweeper's poll plus room for a loaded machine. Not the timeout —
/// that is the job's own, and the row is moved past it below.
const SWEEP: Duration = Duration::from_secs(30);

/// A limit of one minute — the shortest a file may ask for.
const ONE_MINUTE: &str = "\
name: ci
on: push
jobs:
  test:
    timeout-minutes: 1
    steps:
      - name: Wait
        run: |
          echo \"waiting pid=$$\"
          while true; do echo tick; sleep 0.2; done
";

#[test]
fn a_job_whose_runner_went_quiet_past_its_timeout_is_failed_by_the_sweep() {
    // The runner enforces the timeout itself, so the sweep only fires
    // for a runner that cannot: the machine is there but saying nothing
    // — frozen, partitioned, swapped to death. SIGSTOP is exactly that:
    // no claim, no log, no heartbeat, no verdict, and a step still
    // running in its own process group.
    let w = world_with(
        "runner-sweep",
        &[("STRATUM_RUNNER_OVERDUE_SLACK_SECS", "1".into())],
        &["box-1"],
    );
    let sha = w.commit(
        "main",
        "add ci",
        &[(".weft/ci.yml", ONE_MINUTE), ("README.md", "x\n")],
    );
    let step = w.wait_step(&sha);
    let agent = w.agent("box-1");
    agent.pause();

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
    let moved = w
        .db()
        .execute(
            "UPDATE workflow_jobs SET started_at = started_at - 120000 \
             WHERE id = $1 AND state = 'running' AND started_at IS NOT NULL",
            &[&step.job],
        )
        .expect("move the running job past its deadline");
    assert_eq!(
        moved, 1,
        "expected exactly the one running job to move; the sweep's input \
         is not what this test thinks it is"
    );

    let run = stratum_testkit::wait_for("the sweep to fail the overdue job", SWEEP, || {
        w.runs().into_iter().find(|r| r["state"] != "running")
    });
    assert_eq!(run["state"], "failed", "{run}");
    // The verdict is the job's, as a failed step's is; the run carries
    // an error only for what happened to the run as a whole.
    let reason = "timed out after 1 minutes and the runner did not report back";
    assert_eq!(job(&run, "test")["error"], reason, "{run}");
    assert_eq!(run["error"], serde_json::Value::Null, "{run}");
    // The check is mirrored *after* the job is failed, and the run state
    // this test waited on flips before the mirror is written. Wait on
    // the check itself — the observable this assertion is about.
    let checks = stratum_testkit::wait_for("the check to mirror the failure", SWEEP, || {
        let checks = w.checks(&sha);
        (checks.first().is_some_and(|c| c["state"] == "failing")).then_some(checks)
    });
    assert_eq!(checks[0]["state"], "failing", "{checks:?}");

    // The machine comes back. The sweep gave up on it, it did not die,
    // and its next call is answered 410 — the job is over, and which way
    // — so it stops the step rather than carrying on for a verdict
    // nobody will take.
    agent.resume();
    assert_stopped_on_the_machine(agent, &step);
    // And it reports nothing over the sweep's verdict: the job is still
    // failed, for the sweep's reason.
    let run = w.wait_settled(&sha);
    assert_eq!(job(&run, "test")["error"], reason, "{run}");
    assert!(w.server.healthy());
}

/// A machine that takes a job and goes quiet loses it when its lease
/// lapses, the next machine gets it — and the third time, nobody does:
/// a job whose runner has vanished twice is not going to work on the
/// next machine either, and passing it round forever would be a build
/// nobody ever hears the end of.
#[test]
fn a_runner_that_takes_a_job_and_goes_quiet_is_replaced_then_given_up_on() {
    // `STRATUM_RUNNER_MAX_ATTEMPTS` is left at its default, two.
    let mut w = world_with("runner-vanish", &[], &["first"]);
    let sha = w.commit(
        "main",
        "add ci",
        &[(".weft/ci.yml", SLOW), ("README.md", "x\n")],
    );
    let step = w.wait_step(&sha);
    assert_eq!(step.runner, "first");
    w.agent("first").pause();

    // A second machine comes up and, once the first one's lease has
    // lapsed, is handed the same job.
    w.attach("second", &[]);
    let handed_on = w.lapse_until(
        &step.job,
        1,
        "the job to be handed to the second machine",
        |runs| {
            runs.iter().any(|r| {
                r["commit_sha"] == sha && {
                    let j = job(r, "test");
                    j["runner"]["name"] == "second" && j["attempts"] == 2
                }
            })
        },
    );
    assert_eq!(handed_on[0]["state"], "running", "{handed_on:?}");
    let second = w.agent("second");
    second.wait_until("the second machine to take the job", REACH, |a| {
        a.took().contains(&step.job)
    });
    // …and goes quiet too.
    second.pause();

    // A third machine asks, and is where the job is given up on rather
    // than handed out again.
    w.attach("third", &[]);
    w.lapse_until(&step.job, 2, "the job to be given up on", |runs| {
        runs.iter()
            .any(|r| r["commit_sha"] == sha && r["state"] != "running")
    });
    let run = w.wait_settled(&sha);
    assert_eq!(run["state"], "failed", "{run}");
    let test = job(&run, "test");
    assert_eq!(
        test["error"], "the runner was lost 2 times (it took the job but stopped reporting back)",
        "{test}"
    );
    assert_eq!(test["attempts"], 3, "{test}");
    let checks = stratum_testkit::wait_for("the check to mirror the failure", SWEEP, || {
        let checks = w.checks(&sha);
        (checks.first().is_some_and(|c| c["state"] == "failing")).then_some(checks)
    });
    assert_eq!(checks[0]["state"], "failing", "{checks:?}");
    // The third machine was never handed it: the claim that found the
    // job over its cap failed it and went on looking.
    let third = w.agent("third");
    assert!(third.took().is_empty(), "{}", third.said());
    // Every attempt's credential is dead.
    let tokens = w.job_tokens(&step.job);
    assert_eq!(tokens.len(), 2, "two attempts, two tokens: {tokens:?}");
    assert!(
        tokens.iter().all(|t| !t["revoked_at"].is_null()),
        "{tokens:?}"
    );
    assert!(w.server.healthy());
}

/// A machine whose job was handed on while it was quiet learns so from
/// its next call, and stops.
///
/// `take_one` retires the quiet machine's token when it hands the job to
/// the next one, on the promise that "the old runner learns it is over
/// from the 410 its next call gets". It did not: `dead_job_token`
/// answered 410 only to *the job's current* token once the job had
/// stopped, and the job now carried the new attempt's token and was
/// still running, so the old machine was told 401 — which the runner
/// does not treat as "stop" — and carried on running the steps of a
/// build another machine was also running, until its own timeout. A
/// deploy step would have run twice. An earlier attempt's token is now
/// answered 410 `reassigned`.
#[test]
fn a_runner_whose_job_was_handed_on_is_told_to_stop_when_it_calls_again() {
    let mut w = world_with("runner-handed-on", &[], &["first"]);
    let sha = w.commit(
        "main",
        "add ci",
        &[(".weft/ci.yml", SLOW), ("README.md", "x\n")],
    );
    let step = w.wait_step(&sha);
    w.agent("first").pause();
    w.attach("second", &[]);
    w.lapse_until(
        &step.job,
        1,
        "the job to be handed to the second machine",
        |runs| {
            runs.iter()
                .any(|r| r["commit_sha"] == sha && job(r, "test")["runner"]["name"] == "second")
        },
    );

    // The first machine comes back, mid-step, and flushes its log.
    let first = w.agent("first");
    first.resume();
    let deadline = Instant::now() + REACH;
    while step_alive(step.pid) {
        assert!(
            Instant::now() < deadline,
            "the machine whose job was handed on is still running \
             its step (pid {}) {REACH:?} after it came back. Its revoked token is answered \
             401 rather than 410 by `runner_api::dead_job_token`, because the job has a \
             newer attempt's token and is still running — so the runner never learns \
             the job is no longer its own.\n{}",
            step.pid,
            w.said()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    first.wait_until("the first machine to let go of the job", REACH, |a| {
        a.finished().contains(&step.job)
    });
    // The machine that holds the job now is untouched by it.
    let run = w
        .runs()
        .into_iter()
        .find(|r| r["commit_sha"] == sha)
        .unwrap();
    assert_eq!(run["state"], "running", "{run}");
    assert_eq!(job(&run, "test")["runner"]["name"], "second", "{run}");
    assert!(w.server.healthy());
}

/// A machine restarted in the middle of a build takes work again once
/// the build's lease has lapsed.
///
/// `systemctl restart`, a reboot, a crash: the job the machine was in
/// ends without a verdict, and the machine comes back with the same
/// `.runner`. The claim used to answer it **409 busy** for as long as a
/// `running` row named it — `workflows::running_job_for_runner` did not
/// look at the lease — and the only thing that cleared that row was
/// another machine reclaiming the job or the overdue sweep. In an
/// organisation with one machine that was the job's timeout plus the
/// slack: six hours and five minutes of every build queued behind a
/// machine that was idle and asking. Busy now means a live lease.
#[test]
fn a_restarted_runner_takes_work_again_once_its_old_lease_lapses() {
    let mut w = world_with("runner-restart", &[], &["box-1"]);
    let sha = w.commit(
        "main",
        "add ci",
        &[(".weft/ci.yml", SLOW), ("README.md", "x\n")],
    );
    let step = w.wait_step(&sha);

    // SIGTERM, and the same `.runner` comes back: the first life ends
    // the job as a stop — the step killed, no verdict sent — and exits 0.
    let first_life = w.agents[0].restart();
    let agent = w.agent("box-1");
    // 0: a stop is not a failure, so a supervisor with
    // `Restart=on-failure` does not count it as one.
    assert_eq!(
        first_life.and_then(|s| s.code()),
        Some(0),
        "the first life exited {first_life:?}:\n{}",
        agent.said()
    );
    wait_until("the first life's step to be killed", REACH, || {
        !step_alive(step.pid)
    });
    assert!(
        agent.finished().contains(&step.job),
        "the first life did not let go of the job:\n{}",
        agent.said()
    );
    let run = w
        .runs()
        .into_iter()
        .find(|r| r["commit_sha"] == sha)
        .unwrap();
    assert_eq!(run["state"], "running", "a stop is not a verdict: {run}");

    // Nobody is renewing the lease now. When it lapses, the job is
    // anybody's to take — this machine's as much as any. Held lapsed on
    // every pass, for the reason `lapse_until` gives.
    let mut db = w.db();
    let deadline = Instant::now() + REACH;
    loop {
        w.lapse(&mut db, &step.job, 1);
        let run = w
            .runs()
            .into_iter()
            .find(|r| r["commit_sha"] == sha)
            .unwrap();
        let j = job(&run, "test");
        if j["attempts"] == 2 && j["state"] == "running" {
            assert_eq!(j["runner"]["name"], "box-1", "{run}");
            break;
        }
        if Instant::now() >= deadline {
            // Ask the claim ourselves, as this machine, for the answer the
            // agent is getting and — rightly — not printing: it reads 409
            // as "nothing for you yet". Only here, on the way out: while
            // the test can still pass, a claim from the test would be a
            // second machine competing for the job.
            let credential = agent.registration().state()["credential"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let answer = w.server.post("/v1/runners/claim", &credential, None);
            panic!(
                "the restarted machine never took the lapsed job back \
                 ({REACH:?} after the lease lapsed). `POST /v1/runners/claim` answers it \
                 {answer:?} for as long as a running row names it — \
                 `workflows::running_job_for_runner` ignores the lease — so a one-machine \
                 organisation waits for the overdue sweep.\nrun: {run}\n{}",
                w.said()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        agent.took(),
        vec![step.job.clone(), step.job.clone()],
        "{}",
        agent.said()
    );
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// Runs no machine takes
// ---------------------------------------------------------------------

#[test]
fn an_image_is_refused_before_any_runner_takes_it() {
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
    // Refused for the file, at trigger time: a self-hosted job runs its
    // steps directly on the machine, so there is no container to put an
    // image in, and the run never had a job for anything to take.
    assert_eq!(
        run["error"],
        "image \"rust:1.83\" is not available on self-hosted runners; steps run directly on the machine",
        "{run}"
    );
    assert_eq!(run["jobs"], serde_json::json!([]), "{run}");
    // The check row is mirrored from the run by the trigger's settle;
    // wait for it rather than reading the instant the run settles.
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
    // A listening machine was offered nothing. Three claim polls.
    std::thread::sleep(3 * CLAIM_POLL);
    let agent = w.agent("box-1");
    assert!(agent.took().is_empty(), "{}", agent.said());
    assert!(w.server.healthy());
}

#[test]
fn a_workflow_that_does_not_parse_is_a_failing_check_that_says_why() {
    let w = world("runner-refusal");
    let sha = w.commit(
        "main",
        "add ci",
        &[
            // Not YAML this parser can read at all: a mapping entry with
            // no colon, under a key that expects a mapping.
            (
                ".weft/broken.yml",
                "name: broken\non: push\njobs:\n  test\n    steps:\n      - run: true\n",
            ),
            // YAML, but a job that needs one that does not exist.
            (
                ".weft/typo.yml",
                "name: typo\non: push\njobs:\n  test:\n    needs: buidl\n    steps:\n      - run: true\n",
            ),
            // A hosted label, which this server has none of.
            (
                ".weft/hosted.yml",
                "name: hosted\non: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
            ),
        ],
    );
    let runs = w.wait_runs("all three files to settle", |runs| {
        runs.iter().filter(|r| r["commit_sha"] == sha).count() == 3
            && runs.iter().all(|r| r["state"] != "running")
    });
    let run_for = |file: &str| -> serde_json::Value {
        runs.iter()
            .find(|r| r["file"] == file)
            .cloned()
            .unwrap_or_else(|| panic!("no run for {file}: {runs:?}"))
    };
    for file in [".weft/broken.yml", ".weft/typo.yml", ".weft/hosted.yml"] {
        let run = run_for(file);
        assert_eq!(run["state"], "failed", "{run}");
        assert_eq!(run["jobs"], serde_json::json!([]), "{run}");
        let error = run["error"].as_str().unwrap();
        assert!(error.contains(file), "the refusal names its file: {error}");
    }
    let typo = run_for(".weft/typo.yml");
    assert!(typo["error"].as_str().unwrap().contains("buidl"), "{typo}");
    // The parser's own sentence, with the way out beside it.
    let hosted = run_for(".weft/hosted.yml");
    let error = hosted["error"].as_str().unwrap();
    assert!(
        error.contains("`runs-on: ubuntu-latest` names a hosted runner, and this server has none"),
        "{error}"
    );
    assert!(error.contains("runs-on: [self-hosted]"), "{error}");

    // One check per file, named for the file, so the commit page shows
    // each refusal where its verdict would have been.
    let checks = w.checks(&sha);
    assert_eq!(checks.len(), 3, "{checks:?}");
    for c in &checks {
        assert!(
            c["name"].as_str().unwrap().starts_with(".weft/"),
            "{checks:?}"
        );
        assert_eq!(c["state"], "failing", "{checks:?}");
    }
    let agent = w.agent("box-1");
    assert!(agent.took().is_empty(), "{}", agent.said());
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
    // write a run behind it and a listening machine that would take it.
    std::thread::sleep(3 * CLAIM_POLL);
    assert_eq!(w.runs(), Vec::<serde_json::Value>::new());
    assert_eq!(w.checks(&sha), Vec::<serde_json::Value>::new());
    assert!(w.agent("box-1").took().is_empty());
    assert!(w.server.healthy());
}

/// A repository no machine can serve is told so at trigger time, where
/// there is somebody to tell — not left queued for a machine that does
/// not exist, which from the commit page looks exactly like a build that
/// has not started yet.
#[test]
fn a_repository_no_runner_can_serve_says_so_instead_of_queueing_forever() {
    let mut w = world_with("runner-none", &[], &[]);
    let sha = w.commit(
        "main",
        "add ci",
        &[(".weft/ci.yml", CI), ("README.md", "x\n")],
    );
    // Settled inside the commit request: nothing to wait for.
    let run = w
        .runs()
        .into_iter()
        .find(|r| r["commit_sha"] == sha)
        .expect("a run for the commit");
    assert_eq!(run["state"], "failed", "{run}");
    assert_eq!(
        run["error"], "no runner with labels [self-hosted] is registered for this repository",
        "{run}"
    );
    assert_eq!(run["jobs"], serde_json::json!([]), "{run}");
    assert_eq!(w.checks(&sha)[0]["state"], "failing");

    // A machine arrives, but not the one the next file asks for. The
    // sentence names the labels in the order the file wrote them, so it
    // reads back as the `runs-on:` line its author typed.
    w.attach("plain", &[]);
    let gpu = w.commit(
        "main",
        "gpu",
        &[(
            ".weft/ci.yml",
            "name: ci\non: push\njobs:\n  test:\n    runs-on: [self-hosted, gpu]\n    steps:\n      - run: true\n",
        )],
    );
    let run = w
        .runs()
        .into_iter()
        .find(|r| r["commit_sha"] == gpu)
        .expect("a run for the commit");
    assert_eq!(run["state"], "failed", "{run}");
    assert_eq!(
        run["error"], "no runner with labels [self-hosted, gpu] is registered for this repository",
        "{run}"
    );
    std::thread::sleep(3 * CLAIM_POLL);
    let plain = w.agent("plain");
    assert!(plain.took().is_empty(), "{}", plain.said());
    assert!(w.server.healthy());
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
    let w = world_with(
        "runner-fold",
        &[("STRATUM_COMPACT_POLL_SECS", "0".into())],
        &["box-1"],
    );
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
        &["box-1"],
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

/// `acme/widget` with a workflow in it, a machine acme registered, and a
/// change into it from bob's fork — the state every fork case starts in.
///
/// bob is a **viewer** of acme: he may read `widget`, and so fork it into
/// his own namespace and propose a change from there, and he may not
/// push to it. That is the contributor the fork gate is about, and in an
/// edition with no public repositories it is the only one there is. The
/// machine is acme's and it is listening throughout, so "nothing ran" is
/// a decision the server made and not a machine that was absent.
struct Fork {
    /// First, so it is stopped before the server goes.
    agent: Agent,
    server: Server,
    /// Held so the scratch tree outlives the server writing into it.
    #[allow(dead_code)]
    scratch: Scratch,
    /// ada's push to trunk, which the machine did run.
    seed: String,
    /// bob's contribution, and the change ada is looking at.
    sha: String,
    change_key: String,
}

fn fork_stack(hint: &str) -> Fork {
    let minio = Minio::shared();
    let bucket = minio.bucket(hint);
    let scratch = Scratch::new(hint);
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .envs(&fast())
        // The fork itself is a worker's job, and its knob defaults to
        // five seconds — every stack here paid that once, waiting for
        // `fork_state: ready` before it could do anything at all.
        .env("STRATUM_FORK_POLL_SECS", "0.1")
        .start();
    let admin = server.bootstrap_org("acme");
    for (email, role) in [("ada@acme.test", "owner"), ("bob@acme.test", "viewer")] {
        server
            .admin(&[
                "admin",
                "user-create",
                "--org",
                "acme",
                "--email",
                email,
                "--password",
                PASSWORD,
                "--role",
                role,
            ])
            .unwrap_or_else(|e| panic!("user-create {email}: {e}"));
    }
    let token = registration_token(&server, &admin, "acme");
    let agent = Agent::attach(
        &runner_bin(),
        &server.base,
        &token,
        "box-1",
        &[],
        &scratch.path().join("agent-box-1"),
    );

    let (seed, sha, change_key) = {
        let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
        let mut bob = Browser::signed_in(&server, "bob@acme.test", PASSWORD);
        let (st, body) = ada.req(
            "POST",
            "/v1/orgs/acme/repos",
            Some(serde_json::json!({ "name": "widget" })),
        );
        assert_eq!(st, 201, "{body}");
        let (st, body) = ada.req(
            "POST",
            "/v1/orgs/acme/repos/widget/commits",
            Some(serde_json::json!({
                "message": "add ci",
                "operations": [
                    { "op": "put", "path": "README.md", "content": "# seed\n" },
                    { "op": "put", "path": ".weft/ci.yml", "content": CI },
                ],
            })),
        );
        assert_eq!(st, 201, "{body}");
        let seed = body["commit"].as_str().unwrap().to_string();

        let (st, body) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
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
            "/v1/orgs/acme/repos/widget/changes",
            Some(serde_json::json!({ "from": "contrib", "source": "bob/widget" })),
        );
        assert_eq!(st, 201, "{opened}");
        let key = opened["change"]["key"]
            .as_str()
            .or_else(|| opened["key"].as_str())
            .unwrap_or_else(|| panic!("no change key in {opened}"))
            .to_string();
        (seed, sha, key)
    };
    Fork {
        agent,
        server,
        scratch,
        seed,
        sha,
        change_key,
    }
}

#[test]
fn a_change_from_a_fork_is_held_for_approval_and_runs_nothing() {
    let f = fork_stack("runner-fork");
    let server = &f.server;
    let sha = f.sha.clone();
    let mut ada = Browser::signed_in(server, "ada@acme.test", PASSWORD);
    let mut bob = Browser::signed_in(server, "bob@acme.test", PASSWORD);

    // The workflow was read, and deliberately not run: a stranger's
    // `run:` lines do not get a machine until a maintainer says so.
    let (st, runs) = ada.req("GET", "/v1/orgs/acme/repos/widget/workflow-runs", None);
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
    // workflows" for this refusal and for no other, and it must not
    // reach that decision by matching the sentence above — which is
    // written for a person and will be rewritten.
    assert_eq!(run["blocked_reason"], "fork", "{run}");
    assert!(run["jobs"].as_array().unwrap().is_empty(), "{run}");

    // The one build that did run is ada's own push to trunk, on acme's
    // machine.
    let seed_job = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["event"] == "push" && r["commit_sha"] == f.seed)
        .map(|r| job(r, "test")["id"].as_str().unwrap().to_string())
        .unwrap_or_else(|| panic!("no run for ada's push: {runs}"));
    // bob's push to his own fork ran nowhere: acme's machines are
    // acme's, and his namespace has none — said on his commit, in the
    // same words any repository with no machine gets.
    let (st, bobs) = bob.req("GET", "/v1/orgs/bob/repos/widget/workflow-runs", None);
    assert_eq!(st, 200, "{bobs}");
    let own = bobs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["commit_sha"] == sha)
        .unwrap_or_else(|| panic!("no run for bob's push to his fork: {bobs}"));
    assert_eq!(own["state"], "failed", "{own}");
    assert_eq!(
        own["error"], "no runner with labels [self-hosted] is registered for this repository",
        "{own}"
    );
    // The machine was listening the whole time and was handed exactly
    // ada's build. Three claim polls: the negative has no observable of
    // its own.
    f.agent
        .wait_until("the machine to finish ada's build", REACH, |a| {
            a.finished().contains(&seed_job)
        });
    std::thread::sleep(3 * CLAIM_POLL);
    assert_eq!(f.agent.took(), vec![seed_job], "{}", f.agent.said());

    // Held, not failed: the check is queued, so the change waits at the
    // gate rather than being refused by it.
    let (st, checks) = ada.req(
        "GET",
        &format!("/v1/orgs/acme/repos/widget/commits/{sha}/checks"),
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
/// the address with a link into their own build, and a row that leaves
/// it null is a dead end — a red `ci / test` beside a Buildkite row that
/// goes somewhere is worse than useless, and a *refused file* with no
/// link hides the only copy of the reason it was refused.
///
/// All three kinds of row are checked here, because they are written by
/// three different callers: a job that reported (the runner API), a
/// file we would not run (the trigger's settle), and a build somebody
/// stopped (the cancel route). The public URL carries a trailing slash,
/// which is how half of the deployments spell it — a naive `format!`
/// emits `https://forge.example//acme/...`, which some readers
/// normalise and others 404. It is also a host nothing here can reach,
/// and the builds still run: a machine clones from the address it
/// registered with, not from the one a browser is sent to.
#[test]
fn every_check_row_links_to_the_run_that_produced_it() {
    let w = world_with(
        "runner-detail-url",
        &[("STRATUM_PUBLIC_URL", "https://forge.example/".into())],
        &["box-1"],
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
    assert_eq!(run["state"], "passed", "{run}\n{}", w.said());
    let log = w.log(job(&run, "test")["id"].as_str().unwrap());
    assert!(log.contains("✓ Files"), "{log}");
    assert!(!log.contains("forge.example"), "{log}");
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
    let step = w.wait_step(&slow);
    let (st, out) = w.server.post(
        &format!(
            "/v1/orgs/acme/repos/app/workflow-runs/{}/cancel",
            step.run["id"].as_str().unwrap()
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
    assert_stopped_on_the_machine(w.agent("box-1"), &step);

    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// The deployment's ceiling on a build
// ---------------------------------------------------------------------

/// A workflow asking for longer than the deployment allows.
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
/// job asking for precisely the limit is asking for something the
/// deployment offers.
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

/// A `timeout-minutes:` over the deployment's ceiling
/// (`STRATUM_RUNNER_MAX_TIMEOUT_MINUTES`) is refused where the person who
/// wrote it will see it, and no machine is handed it.
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
        &[("STRATUM_RUNNER_MAX_TIMEOUT_MINUTES", "60".into())],
        &["box-1"],
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
        over["error"], "timeout-minutes: 720 exceeds this server's limit of 60",
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

    // One job taken, for the file that was allowed to run.
    let agent = w.agent("box-1");
    assert_eq!(
        agent.took(),
        vec![job(at_cap, "check")["id"].as_str().unwrap().to_string()],
        "{}",
        agent.said()
    );
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
///   the one that lands, not the one that reviews. A reader may hold an
///   opinion; starting a machine is a different grant.
/// * **It actually runs.** Not "the row changed state": the machine has
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
    let f = fork_stack("runner-approve");
    let server = &f.server;
    let key = f.change_key.clone();
    let sha = f.sha.clone();
    let approve = format!("/v1/orgs/acme/repos/widget/changes/{key}/workflows/approve");
    let mut ada = Browser::signed_in(server, "ada@acme.test", PASSWORD);
    let mut bob = Browser::signed_in(server, "bob@acme.test", PASSWORD);

    // Through the narrowing the dashboard actually sends: the approval
    // panel asks for this change at this tip, and asking for a window
    // and filtering in the client is what loses the button on a busy
    // repository.
    let change_runs = |b: &mut Browser| -> Vec<serde_json::Value> {
        let (st, runs) = b.req(
            "GET",
            &format!("/v1/orgs/acme/repos/widget/workflow-runs?change_key={key}"),
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
    // from anyone who cannot do the thing being asked (R8), so a reader
    // cannot use a refusal to learn what a write would touch. This is
    // the same answer the land route gives bob, which is the point — one
    // door, one behaviour.
    let (st, out) = bob.req("POST", &approve, None);
    assert_eq!(
        st, 404,
        "bob may not approve his own change's workflows: {out}"
    );
    let (land_st, _) = bob.req(
        "POST",
        &format!("/v1/orgs/acme/repos/widget/changes/{key}/land"),
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

    // It really runs: the machine clones acme's repository with a job
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
            "waited 60s for the approved run: {runs:?}\n{}",
            f.agent.said()
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(done["state"], "passed", "{done}\n{}", f.agent.said());
    assert_eq!(done["commit_sha"], sha, "{done}");
    let approved_job = job(&done, "test")["id"].as_str().unwrap().to_string();
    assert_eq!(job(&done, "test")["runner"]["name"], "box-1", "{done}");
    assert!(
        f.agent.took().contains(&approved_job),
        "the machine never took the approved job:\n{}",
        f.agent.said()
    );

    // "Blocked, then approved by ada" has to survive the placeholder
    // row's deletion, so the approval is in the audit log — who, which
    // change, which tip, and which files were let go.
    let db = ControlDb::open(&server.db_url).unwrap();
    let acme = registry::org_by_name(&db, "acme").unwrap().unwrap();
    let approvals = stratum_control::audit::query(
        &db,
        &acme.id,
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
        Some("ada@acme.test"),
        "the trail has to name who let a contributor's code onto a machine: {:?}",
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
        &format!("/v1/orgs/acme/repos/widget/commits/{sha}/checks"),
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
        "/v1/orgs/acme/repos/widget/changes",
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
        "/v1/orgs/acme/repos/widget/changes/I0000000000000000000000000000000000000000/workflows/approve",
        None,
    );
    assert_eq!(st, 404, "{out}");
    assert_eq!(
        out["error"], "no change \"I0000000000000000000000000000000000000000\"",
        "{out}"
    );

    // And a change that is no longer open cannot have its workflows
    // started: there is nothing to approve *for*, and starting a machine
    // for a change nobody can land is the button-that-cannot-help case
    // in its purest form. 409 with the state in it, so the page can say
    // which state without inventing one.
    let (st, out) = ada.req(
        "POST",
        &format!("/v1/orgs/acme/repos/widget/changes/{key}/abandon"),
        None,
    );
    assert_eq!(st, 204, "{out}");
    let (st, out) = ada.req("POST", &approve, None);
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["error"], "change is abandoned", "{out}");

    assert!(server.healthy());
}

// ---------------------------------------------------------------------
// The agent's own contract: registering, and being removed
// ---------------------------------------------------------------------

/// A workflow for a machine with a particular label, whose steps prove
/// the checkout happened rather than only that a shell ran.
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
/// process on somebody else's machine, all the way to a green check —
/// and then the ending an operator actually performs.
///
/// The four seams that only exist when somebody else is on the other end
/// (the registration exchange, the credential, the claim's long poll,
/// the clone from outside) are exercised by the thing that will actually
/// be exercising them, and what the binary itself promises its operator
/// — what it prints, the file it writes and the mode it writes it with,
/// the exit code that tells a supervisor not to restart it — is checked
/// here and nowhere else.
#[test]
fn the_real_binary_registers_takes_a_job_and_reports_a_verdict() {
    // `STRATUM_RUNNER_URL` names an address *our* network reaches this
    // server at — a private listener, `host.docker.internal` on a
    // laptop — and the job's clone URL is built from it. A machine
    // somebody else owns is outside that network by definition; the one
    // address it has proved it can reach is the one it registered with.
    // The manual pass found every self-hosted job failing its checkout
    // with "could not resolve host: host.docker.internal" for exactly
    // this reason.
    let w = world_with(
        "runner-selfhosted",
        &[("STRATUM_RUNNER_URL", "http://fleet.private.invalid".into())],
        &[],
    );
    let dir = w.scratch.path().join("agent");

    // The operator mints a token in the dashboard and pastes the command
    // the page shows them.
    let token = registration_token(&w.server, &w.admin, "acme");
    let registered =
        Registered::register(&w.bin, &w.server.base, &token, "e2e-box", &["gpu"], &dir);
    assert!(
        registered.printed.contains("registered e2e-box as")
            && registered.printed.contains("in group default"),
        "register printed nothing an operator could act on: {:?}",
        registered.printed
    );

    // The credential it wrote is a file only its owner can read: it is a
    // long-lived bearer secret sitting on a shared build box.
    let written = registered.state();
    assert_eq!(written["name"], "e2e-box", "{}", written["name"]);
    assert!(
        written["credential"]
            .as_str()
            .unwrap_or_default()
            .starts_with("weftr_"),
        "the credential is not a runner credential"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join(".runner"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
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
    let mut agent = registered.run();
    let sha = push_with_git(
        &w,
        "main",
        &[
            (".weft/ci.yml", SELF_HOSTED_CI),
            ("README.md", "hello from the repo\n"),
        ],
    );
    let run = w.wait_settled(&sha);
    assert_eq!(
        run["state"],
        "passed",
        "the run did not pass: {run}\n{}",
        agent.said()
    );

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
    assert!(
        agent.stdout().contains("listening as e2e-box"),
        "the agent said nothing at startup:\n{}",
        agent.said()
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

    let code = agent
        .wait_exit(Duration::from_secs(60))
        .unwrap_or_else(|| panic!("a removed runner kept running\n{}", agent.said()))
        .code();
    // 2 is the contract's exit code for "this machine is no longer
    // registered", and it is distinct from 0 so that whatever supervises
    // the agent does not restart it into a loop it cannot win.
    assert_eq!(
        code,
        Some(2),
        "a removed runner exited {code:?}:\n{}",
        agent.said()
    );
    assert!(
        agent.stderr().contains("this runner has been removed"),
        "a removed runner exited without saying why:\n{}",
        agent.said()
    );

    assert!(w.server.healthy());
}
