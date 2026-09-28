//! Composed CI for a changeset, end to end, through a real `weft-runner`
//! agent the organisation registered.
//!
//! A changeset is landed as one thing, so it has to be *tested* as one
//! thing: a job declared `on: changeset` runs once per member repository
//! with every member's patchset materialised under `$WEFT_WORKSPACE`,
//! and its verdict lands on the changeset — not on any one commit — where
//! the changeset's land gate reads it. Everything between the changeset
//! API and that gate is the product: the trigger, the composition hash,
//! the claim a registered machine makes, the per-member read tokens the
//! spec hands it, the checkout of every member, the step env, the mirror
//! into `changeset_checks`, the fold into the verdict. The machine is the
//! real agent — `weft-runner register`, then `weft-runner run` as its own
//! process — and what the suite knows of it is what it printed.
//!
//! The situations are the ones that happen. A composed run that passes
//! and lets the changeset land; a composed check that fails and blocks
//! the changeset while every member's own gate is green; a new patchset
//! on one member that supersedes the composition; membership changing
//! under a live build; a member token that can read its own repository
//! and nothing else, and is dead the moment the job is.

use std::path::PathBuf;
use std::time::{Duration, Instant};
use stratum_testkit::browser::Browser;
use stratum_testkit::gitcli::Scratch;
use stratum_testkit::runner_bin::{registration_token, runner_bin_next_to, Agent, Registered};
use stratum_testkit::{wait_for, wait_until, Minio, Server};

const CS: &str = "/v1/orgs/acme/changesets";

/// The poll interval every worker in these tests is configured with,
/// spelled the way the env knob wants it. An absence is the one thing
/// that cannot be waited on directly, so the negative assertions below
/// wait a stated number of *these* rather than a round number of
/// wall-clock seconds — and if somebody changes the knob, this is where
/// the arithmetic that justifies the wait lives.
const POLL_SECS: &str = "0.1";

/// How long to give the server to do the thing we are asserting it does
/// *not* do. The slowest thing that could do it is a listening agent's
/// claim, which looks for work every half second (`CLAIM_POLL` in the
/// runners API); this is three of those, and fifteen ticks of
/// `POLL_SECS`. Enough that a missed poll is not the reason the
/// assertion held, short enough that it is not the test's runtime. This
/// is the only shape of wait in this file that is a duration rather than
/// an observable, because there is no observable for "and then nothing
/// happened".
const NEGATIVE_WINDOW: Duration = Duration::from_millis(1500);

/// How often a bespoke poll loop below re-reads the server. The same
/// interval `stratum_testkit::wait` polls at; these loops exist only
/// because they dump the state they were waiting on when they give up,
/// which is what has actually made a red run here readable.
const POLL: Duration = Duration::from_millis(25);

/// A workflow that runs for a change on its own and again for every
/// composition the change is part of. The composed steps prove what the
/// runner promised rather than that a shell ran: the event, the
/// changeset, the member list, that the working directory is this
/// repository's own checkout inside the workspace, and that the *other*
/// member's patchset is there to be read.
const COMPOSED: &str = "\
name: ci
on: [change, changeset]
jobs:
  test:
    steps:
      - name: Env
        run: echo \"event=$WEFT_EVENT change=$WEFT_CHANGE changeset=$WEFT_CHANGESET\"
      - name: Members
        run: echo \"members=$WEFT_CHANGESET_MEMBERS\"
      - name: Workspace
        run: |
          if [ \"$WEFT_EVENT\" = changeset ]; then
            test -d \"$WEFT_WORKSPACE\" || exit 41
            here=$(pwd -P); want=$(cd \"$WEFT_WORKSPACE\" && pwd -P)
            case \"$here\" in \"$want\"/*) ;; *) echo \"cwd $here is not under $want\"; exit 42;; esac
            test -f feature.txt || exit 43
            cat \"$WEFT_WORKSPACE\"/*/feature.txt
          else
            test -z \"$WEFT_WORKSPACE\" || exit 44
            test -z \"$WEFT_CHANGESET\" || exit 45
          fi
";

/// Composed only, and red.
const COMPOSED_RED: &str = "\
name: ci
on: changeset
jobs:
  test:
    steps:
      - name: Integration
        run: echo \"event=$WEFT_EVENT\"; exit 3
";

/// Composed only; never ends while the api member's patchset says
/// `slow`, so the composition it belongs to can be superseded under it —
/// and finishes at once when a later patchset says otherwise. The wait
/// is keyed on another member's file so that the very thing that ends
/// it is a new composition.
///
/// It prints its pid, which is how a test proves a cancellation reached
/// the machine — that process is gone — and it keeps printing, because a
/// runner learns its job is over from the answer to its next call: a
/// step that prints is flushed every second, one that is silent gets a
/// heartbeat every thirty.
const SLOW_WHILE_API_SLOW: &str = "\
name: ci
on: changeset
jobs:
  test:
    steps:
      - name: Wait
        run: |
          if grep -q slow \"$WEFT_WORKSPACE/api/feature.txt\"; then
            echo \"waiting pid=$$\"
            while true; do echo tick; sleep 0.2; done
          fi
          echo quick
";

/// Composed only; never ends while web is a member.
const SLOW_WHILE_WEB_IN: &str = "\
name: ci
on: changeset
jobs:
  test:
    steps:
      - name: Wait
        run: |
          case \"$WEFT_CHANGESET_MEMBERS\" in *web*)
            echo \"waiting pid=$$\"
            while true; do echo tick; sleep 0.2; done;;
          esac
          echo quick
";

/// How long a cancellation may take to reach an agent whose step is
/// printing: the log is flushed every second and the answer to that
/// flush is the 410. Generous for a loaded machine, and far short of
/// anything the steps above would reach by finishing on their own —
/// while they wait, they never do.
const REACH: Duration = Duration::from_secs(30);

struct World {
    /// First, so they are stopped while the server they call is still
    /// there: struct fields drop in declaration order.
    agents: Vec<Agent>,
    server: Server,
    admin: String,
    scratch: Scratch,
}

/// The environment every server in this file starts with: every worker
/// at `POLL_SECS`, and a claim's long poll short enough that an idle
/// agent — and the server's shutdown behind it — is not held for twenty
/// seconds. The claim looks for work every half second either way.
fn fast() -> Vec<(&'static str, String)> {
    vec![
        ("STRATUM_RUNNER_POLL_SECS", POLL_SECS.into()),
        ("STRATUM_LAND_POLL_SECS", POLL_SECS.into()),
        ("STRATUM_RUNNER_CLAIM_WAIT_MS", "700".into()),
        // Only the fork case forks, but the fork worker's default poll is
        // five seconds — long enough that its wait was mostly this knob
        // rather than anything about forking.
        ("STRATUM_FORK_POLL_SECS", POLL_SECS.into()),
    ]
}

fn runner_bin() -> PathBuf {
    runner_bin_next_to(env!("CARGO_BIN_EXE_stratum-server"))
}

/// A server with a bootstrapped `acme`, and one machine — `box-1` —
/// registered to it and listening.
fn world(hint: &str) -> World {
    world_with(hint, &["box-1"])
}

fn world_with(hint: &str, agents: &[&str]) -> World {
    let minio = Minio::shared();
    let bucket = minio.bucket(hint);
    let scratch = Scratch::new(hint);
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .envs(&fast())
        .start();
    let admin = server.bootstrap_org("acme");
    let bin = runner_bin();
    let agents = agents
        .iter()
        .map(|name| {
            let token = registration_token(&server, &admin, "acme");
            let dir = scratch.path().join(format!("agent-{name}"));
            Agent::attach(&bin, &server.base, &token, name, &[], &dir)
        })
        .collect();
    World {
        agents,
        server,
        admin,
        scratch,
    }
}

fn put(path: &str, content: &str) -> serde_json::Value {
    serde_json::json!({"op": "put", "path": path, "content": content})
}

impl World {
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

    fn commit(
        &self,
        repo: &str,
        branch: &str,
        message: &str,
        ops: Vec<serde_json::Value>,
    ) -> String {
        let (st, out) = self.server.post(
            &format!("/v1/orgs/acme/repos/{repo}/commits"),
            &self.admin,
            Some(serde_json::json!({"branch": branch, "message": message, "operations": ops})),
        );
        assert_eq!(st, 201, "commit to {repo}/{branch}: {out}");
        out["commit"].as_str().unwrap().to_string()
    }

    /// A private repository whose trunk has `OWNERS` naming `owner`, a
    /// `feature` branch adding `feature.txt` (content `text`) and the
    /// workflow file when there is one, and an open change `key` from it.
    /// Returns the change's tip.
    fn repo_with_change(
        &self,
        repo: &str,
        owner: &str,
        key: &str,
        text: &str,
        workflow: Option<&str>,
    ) -> String {
        let (st, out) = self.server.post(
            "/v1/orgs/acme/repos",
            &self.admin,
            Some(serde_json::json!({"name": repo})),
        );
        assert_eq!(st, 201, "create repo {repo}: {out}");
        self.commit(
            repo,
            "main",
            "trunk",
            vec![put("OWNERS", &format!("{owner}\n")), put("readme", "v1\n")],
        );
        let (st, out) = self.server.post(
            &format!("/v1/orgs/acme/repos/{repo}/branches"),
            &self.admin,
            Some(serde_json::json!({"name": "feature", "from": "main"})),
        );
        assert_eq!(st, 201, "branch feature in {repo}: {out}");
        self.patchset(repo, key, text, workflow)
    }

    /// Another patchset of `key` in `repo`: a commit on `feature` and the
    /// change re-posted. Returns the new tip.
    fn patchset(&self, repo: &str, key: &str, text: &str, workflow: Option<&str>) -> String {
        let mut ops = vec![put("feature.txt", &format!("{text}\n"))];
        if let Some(w) = workflow {
            ops.push(put(".weft/ci.yml", w));
        }
        let sha = self.commit(
            repo,
            "feature",
            &format!("change {repo}: {text}\n\nChange-Id: {key}\n"),
            ops,
        );
        let (st, out) = self.server.post(
            &format!("/v1/orgs/acme/repos/{repo}/changes"),
            &self.admin,
            Some(serde_json::json!({"from": "feature"})),
        );
        assert_eq!(st, 201, "open change in {repo}: {out}");
        assert_eq!(out["patchset"]["commit"], sha, "{out}");
        sha
    }

    fn approve(&self, email: &str, repo: &str, key: &str) {
        let name = email.split('@').next().unwrap();
        // Idempotent: the same owner may approve in two repositories.
        let _ = self.server.admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            email,
            "--name",
            name,
            "--password",
            PASSWORD,
            "--role",
            "member",
        ]);
        let mut who = Browser::signed_in(&self.server, email, PASSWORD);
        let (st, out) = who.req(
            "POST",
            &format!("/v1/orgs/acme/repos/{repo}/changes/{key}/approve"),
            None,
        );
        assert_eq!(st, 204, "{email} approving {repo}/{key}: {out}");
    }

    fn compose(&self, key: &str, members: &[(&str, &str)]) -> serde_json::Value {
        let members: Vec<_> = members
            .iter()
            .map(|(r, c)| serde_json::json!({"repo": r, "change": c}))
            .collect();
        let (st, out) = self.server.post(
            CS,
            &self.admin,
            Some(serde_json::json!({"key": key, "title": "compose", "members": members})),
        );
        assert_eq!(st, 201, "{out}");
        out
    }

    fn changeset(&self, key: &str) -> serde_json::Value {
        let (st, out) = self.server.get(&format!("{CS}/{key}"), &self.admin);
        assert_eq!(st, 200, "{out}");
        out
    }

    fn verdict(&self, key: &str) -> serde_json::Value {
        let (st, out) = self.server.get(&format!("{CS}/{key}/verdict"), &self.admin);
        assert_eq!(st, 200, "{out}");
        out
    }

    fn runs(&self, repo: &str) -> Vec<serde_json::Value> {
        let (st, out) = self.server.get(
            &format!("/v1/orgs/acme/repos/{repo}/workflow-runs"),
            &self.admin,
        );
        assert_eq!(st, 200, "{out}");
        out["runs"].as_array().cloned().unwrap_or_default()
    }

    /// The repository's composed runs, as the Checks tab asks for them:
    /// `?event=changeset`, the server's filter. Checked against the
    /// client-side filtering of the full list every time, so the two
    /// can never quietly disagree about which runs are composed.
    fn composed_runs(&self, repo: &str) -> Vec<serde_json::Value> {
        let (st, out) = self.server.get(
            &format!("/v1/orgs/acme/repos/{repo}/workflow-runs?event=changeset"),
            &self.admin,
        );
        assert_eq!(st, 200, "{out}");
        let filtered = out["runs"].as_array().cloned().unwrap_or_default();
        assert!(
            filtered.iter().all(|r| r["event"] == "changeset"),
            "event=changeset answered a run of another kind: {filtered:?}"
        );
        let by_hand: Vec<serde_json::Value> = self
            .runs(repo)
            .into_iter()
            .filter(|r| r["event"] == "changeset")
            .collect();
        let ids = |rs: &[serde_json::Value]| -> Vec<String> {
            rs.iter().map(|r| r["id"].to_string()).collect()
        };
        // A run can settle between the two reads, so compare identity,
        // not state.
        assert_eq!(ids(&filtered), ids(&by_hand));
        filtered
    }

    /// Poll `repo`'s run list until `pred` holds of it.
    ///
    /// Not `wait_for`: giving up here prints the run list it was looking
    /// at, and that dump is most of what makes a red composed-CI run
    /// readable — which run was still `running`, which composition it
    /// was for. The poll interval is `wait`'s.
    fn wait_runs(
        &self,
        repo: &str,
        what: &str,
        pred: impl Fn(&[serde_json::Value]) -> bool,
    ) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let runs = self.runs(repo);
            if pred(&runs) {
                return runs;
            }
            assert!(
                Instant::now() < deadline,
                "waited 60s for {what} in {repo}; runs were:\n{}\n{}",
                serde_json::to_string_pretty(&runs).unwrap(),
                self.said()
            );
            std::thread::sleep(POLL);
        }
    }

    fn wait_all_settled(&self, repo: &str, n: usize) -> Vec<serde_json::Value> {
        self.wait_runs(repo, &format!("{n} runs to settle"), |runs| {
            runs.len() == n && runs.iter().all(|r| r["state"] != "running")
        })
    }

    fn log(&self, repo: &str, job_id: &str) -> String {
        let (st, out) = self.server.get(
            &format!("/v1/orgs/acme/repos/{repo}/workflow-jobs/{job_id}/log"),
            &self.admin,
        );
        assert_eq!(st, 200, "{out}");
        out.as_str().unwrap_or_default().to_string()
    }

    /// Same reason as `wait_runs` for not being `wait_for`: the log it
    /// was reading is the whole diagnostic when the step never got where
    /// the test expected.
    fn wait_log(&self, repo: &str, job_id: &str, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let log = self.log(repo, job_id);
            if log.contains(needle) {
                return log;
            }
            assert!(
                Instant::now() < deadline,
                "{repo} job {job_id} never logged {needle:?}:\n{log}"
            );
            std::thread::sleep(POLL);
        }
    }

    /// The pid a waiting step printed, once it has: the moment there is
    /// a build in progress on a machine to do something to.
    fn wait_step(&self, repo: &str, job_id: &str) -> u32 {
        let log = self.wait_log(repo, job_id, "waiting pid=");
        let rest = &log[log.find("waiting pid=").unwrap() + "waiting pid=".len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits
            .parse()
            .unwrap_or_else(|e| panic!("no pid after `waiting pid=` ({e}):\n{log}"))
    }

    fn checks(&self, repo: &str, sha: &str) -> Vec<serde_json::Value> {
        let (st, out) = self.server.get(
            &format!("/v1/orgs/acme/repos/{repo}/commits/{sha}/checks"),
            &self.admin,
        );
        assert_eq!(st, 200, "{out}");
        out["runs"].as_array().cloned().unwrap_or_default()
    }

    fn tokens(&self) -> Vec<serde_json::Value> {
        let (st, out) = self.server.get("/v1/orgs/acme/tokens", &self.admin);
        assert_eq!(st, 200, "{out}");
        out["tokens"].as_array().cloned().unwrap_or_default()
    }
}

const PASSWORD: &str = "a long enough password";

fn job<'a>(run: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    run["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["key"] == key)
        .unwrap_or_else(|| panic!("no job {key} in {run}"))
}

/// Whether the step that printed `pid` is still running: the process
/// exists, is not a zombie, and is still the shell running one of the
/// waiting loops above — the last so that a pid the kernel has since
/// handed to something else cannot pass for it. Asked of `ps` rather
/// than `/proc` so the answer means the same on a Mac as on Linux.
fn step_alive(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    !line.is_empty() && !line.starts_with('Z') && line.contains("tick")
}

/// That a cancelled composed build really stopped on the machine: the
/// step's process is gone, and the agent is done with the job in its own
/// words — `cancelled, exiting quietly`, the ending a 410 gets and not
/// the one a verdict gets.
fn assert_stopped_on_the_machine(agent: &Agent, job_id: &str, pid: u32) {
    wait_until(
        &format!("the step of job {job_id} (pid {pid}) to be killed"),
        REACH,
        || !step_alive(pid),
    );
    agent.wait_until(
        &format!("{} to let go of job {job_id}", agent.name()),
        REACH,
        |a| a.finished().iter().any(|j| j == job_id),
    );
    assert!(
        agent
            .stderr()
            .contains(&format!("job {job_id}: cancelled, exiting quietly")),
        "the agent did not end the job as a cancellation:\n{}",
        agent.said()
    );
}

fn the_one(runs: Vec<serde_json::Value>) -> serde_json::Value {
    assert_eq!(runs.len(), 1, "{runs:?}");
    runs.into_iter().next().unwrap()
}

// ---------------------------------------------------------------------
// The happy path, with every seam checked on the way round
// ---------------------------------------------------------------------

#[test]
fn a_composed_run_materialises_every_member_and_lets_the_changeset_land() {
    let w = world("cs-ci-green");
    let api = w.repo_with_change(
        "api",
        "oa@acme.test",
        "Iaa000001",
        "change api",
        Some(COMPOSED),
    );
    let web = w.repo_with_change(
        "web",
        "ow@acme.test",
        "Ibb000002",
        "change web",
        Some(COMPOSED),
    );
    // Opening the changes started their own `change` runs; nothing composed
    // exists until there is a changeset.
    w.wait_all_settled("api", 1);
    w.wait_all_settled("web", 1);
    assert!(w.composed_runs("api").is_empty());
    // The "what would run" listing spells the new event the way the file
    // does — the one place the trigger is printed rather than parsed. The
    // file is on the change's branch, not on main, so ask for that rev.
    let (st, listed) = w
        .server
        .get("/v1/orgs/acme/repos/api/workflows?at=feature", &w.admin);
    assert_eq!(st, 200, "{listed}");
    assert_eq!(listed["workflows"][0]["ok"], true, "{listed}");
    assert_eq!(
        listed["workflows"][0]["on"],
        serde_json::json!(["change", "changeset"]),
        "{listed}"
    );

    let cs = w.compose("Ic5000001", &[("api", "Iaa000001"), ("web", "Ibb000002")]);
    let composition = cs["composition"]
        .as_str()
        .expect("a composition")
        .to_string();
    assert_eq!(composition.len(), 64, "sha256 hex: {composition}");
    // Nothing has run yet, but the checks the gate will wait on are
    // already in the body as queued — the person sees what is coming.
    let names: Vec<_> = cs["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["repo"].as_str().unwrap().to_string(),
                c["name"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        names,
        vec![
            ("api".into(), "ci / test".into()),
            ("web".into(), "ci / test".into())
        ],
        "{cs}"
    );

    let api_runs = w.wait_all_settled("api", 2);
    let web_runs = w.wait_all_settled("web", 2);
    for r in api_runs.iter().chain(web_runs.iter()) {
        assert_eq!(r["state"], "passed", "{r}");
    }
    let api_cs = the_one(w.composed_runs("api"));
    let web_cs = the_one(w.composed_runs("web"));
    for (r, sha, key) in [(&api_cs, &api, "Iaa000001"), (&web_cs, &web, "Ibb000002")] {
        assert_eq!(r["event"], "changeset", "{r}");
        assert_eq!(r["commit_sha"], *sha, "{r}");
        assert_eq!(r["change_key"], key, "{r}");
        assert_eq!(r["changeset"]["key"], "Ic5000001", "{r}");
        assert_eq!(r["composition"], composition, "{r}");
    }
    // The change runs carry none of it.
    for r in w.runs("api").iter().filter(|r| r["event"] == "change") {
        assert!(
            r["changeset"].is_null() && r["composition"].is_null(),
            "{r}"
        );
    }

    // What the steps saw. api's job read web's patchset and its own; the
    // member list names both with their changes and commits; the event
    // and the changeset reached the env.
    let log = w.log("api", job(&api_cs, "test")["id"].as_str().unwrap());
    assert!(
        log.contains("event=changeset change=Iaa000001 changeset=Ic5000001"),
        "{log}"
    );
    assert!(
        log.contains("change api\nchange web")
            || log.contains("change api") && log.contains("change web"),
        "{log}"
    );
    let members_line = log
        .lines()
        .find(|l| l.starts_with("members="))
        .unwrap_or_else(|| panic!("{log}"));
    let members: serde_json::Value =
        serde_json::from_str(&members_line["members=".len()..]).unwrap();
    let members = members.as_array().unwrap();
    assert_eq!(members.len(), 2, "{members:?}");
    assert_eq!(members[0]["repo"], "api");
    assert_eq!(members[0]["change"], "Iaa000001");
    assert_eq!(members[0]["commit"], api);
    assert!(
        members[0]["path"].as_str().unwrap().ends_with("/api"),
        "{members:?}"
    );
    assert_eq!(members[1]["repo"], "web");
    assert_eq!(members[1]["commit"], web);
    // And the change run saw none of it (exit 44/45 would have failed it).
    let change_log = w.log(
        "api",
        job(
            w.runs("api")
                .iter()
                .find(|r| r["event"] == "change")
                .unwrap(),
            "test",
        )["id"]
            .as_str()
            .unwrap(),
    );
    assert!(
        change_log.contains("event=change change=Iaa000001 changeset="),
        "{change_log}"
    );

    // The verdict lands on the changeset, not the commit: one `ci / test`
    // per commit, from the change run; two on the changeset.
    for (repo, sha) in [("api", &api), ("web", &web)] {
        let checks = w.checks(repo, sha);
        assert_eq!(checks.len(), 1, "{repo}: {checks:?}");
        assert_eq!(checks[0]["name"], "ci / test");
        assert_eq!(checks[0]["state"], "passing");
    }
    let cs = w.changeset("Ic5000001");
    let checks = cs["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 2, "{cs}");
    for (c, r) in checks.iter().zip([&api_cs, &web_cs]) {
        assert_eq!(c["state"], "passing", "{c}");
        assert_eq!(c["name"], "ci / test", "{c}");
        assert_eq!(c["run"], r["id"], "{c}");
        assert!(
            c["detail_url"]
                .as_str()
                .unwrap()
                .contains(r["id"].as_str().unwrap()),
            "{c}"
        );
    }

    // Re-posting the same tip is not a new patchset and composes nothing new.
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos/api/changes",
        &w.admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 200, "the same tip is the same patchset: {out}");
    assert_eq!(out["patchset"]["number"], 1, "{out}");
    // Nothing to wait *for*: the claim is that no third run appears. Give
    // the server `NEGATIVE_WINDOW` to have made the mistake, then read the
    // list.
    std::thread::sleep(NEGATIVE_WINDOW);
    assert_eq!(w.runs("api").len(), 2, "{:?}", w.runs("api"));
    assert_eq!(w.runs("web").len(), 2);

    // The member tokens: one per job for the *other* member, bound to
    // that repository alone, and revoked with the job.
    let member_tokens: Vec<_> = w
        .tokens()
        .into_iter()
        .filter(|t| {
            t["label"]
                .as_str()
                .is_some_and(|l| l.starts_with("ci:") && l.matches(':').count() == 2)
        })
        .collect();
    assert_eq!(member_tokens.len(), 2, "{member_tokens:?}");
    for t in &member_tokens {
        assert!(!t["revoked_at"].is_null(), "still live after the job: {t}");
        assert_eq!(t["scopes"], serde_json::json!(["repo:read"]), "{t}");
        assert!(!t["repo_id"].is_null(), "unbound: {t}");
    }
    let api_job = job(&api_cs, "test")["id"].as_str().unwrap();
    assert!(
        member_tokens
            .iter()
            .any(|t| t["label"] == format!("ci:{api_job}:web")),
        "{member_tokens:?}"
    );

    w.approve("oa@acme.test", "api", "Iaa000001");
    w.approve("ow@acme.test", "web", "Ibb000002");
    let v = w.verdict("Ic5000001");
    assert_eq!(v["landable"], true, "{v}");
    assert_eq!(v["gate"], "ready", "{v}");
    let (st, out) = w
        .server
        .post(&format!("{CS}/Ic5000001/land"), &w.admin, None);
    assert_eq!(st, 202, "{out}");
    // Wait on the body that stops saying `landing`, and assert against
    // *that* read rather than a fresh one — the old loop re-fetched, so a
    // state moving on between the two would have been read as a failure
    // to land.
    let landed = wait_for(
        "the changeset to stop landing",
        Duration::from_secs(60),
        || {
            let cs = w.changeset("Ic5000001");
            (cs["state"] != "landing").then_some(cs)
        },
    );
    assert_eq!(landed["state"], "landed", "{landed}");
    // Four jobs — two change runs, two composed — each taken by the one
    // machine and each ended cleanly: a job that could not report its
    // verdict is announced with the runner's exit code after its id.
    let agent = w.agent("box-1");
    agent.wait_until("the machine to be done with all four jobs", REACH, |a| {
        a.finished().len() == 4
    });
    assert_eq!(agent.took().len(), 4, "{}", agent.said());
    assert!(
        agent
            .stdout()
            .lines()
            .filter(|l| l.contains("(the runner exited"))
            .count()
            == 0,
        "a job ended badly on the machine:\n{}",
        agent.said()
    );
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// A red composed check
// ---------------------------------------------------------------------

#[test]
fn a_failing_composed_check_blocks_the_changeset_and_no_member() {
    let w = world("cs-ci-red");
    let api = w.repo_with_change("api", "oa@acme.test", "Iaa000001", "change api", None);
    let web = w.repo_with_change(
        "web",
        "ow@acme.test",
        "Ibb000002",
        "change web",
        Some(COMPOSED_RED),
    );
    w.approve("oa@acme.test", "api", "Iaa000001");
    w.approve("ow@acme.test", "web", "Ibb000002");
    w.compose("Ic5000001", &[("api", "Iaa000001"), ("web", "Ibb000002")]);

    // api declares nothing composed, so it has no composed run; web's
    // fails.
    let run = the_one(w.wait_all_settled("web", 1));
    assert_eq!(run["event"], "changeset", "{run}");
    assert_eq!(run["state"], "failed", "{run}");
    // api declares nothing composed. An absence again: `NEGATIVE_WINDOW`
    // in which it could have started something, and did not.
    std::thread::sleep(NEGATIVE_WINDOW);
    assert!(w.runs("api").is_empty(), "{:?}", w.runs("api"));
    let log = w.log("web", job(&run, "test")["id"].as_str().unwrap());
    assert!(log.contains("event=changeset"), "{log}");

    let v = w.verdict("Ic5000001");
    assert_eq!(v["landable"], false, "{v}");
    assert_eq!(v["gate"], "blocked", "{v}");
    assert!(
        v["explanation"].as_str().unwrap().contains("ci / test"),
        "{v}"
    );
    assert!(v["explanation"].as_str().unwrap().contains("web"), "{v}");
    // Every member's own gate is untouched: the failure is the
    // changeset's, and lands on no commit.
    for m in v["members"].as_array().unwrap() {
        assert_eq!(m["gate"], "ready", "{m}");
        assert_eq!(m["landable"], true, "{m}");
    }
    assert!(
        w.checks("web", &web).is_empty(),
        "{:?}",
        w.checks("web", &web)
    );
    assert!(w.checks("api", &api).is_empty());
    let cs = w.changeset("Ic5000001");
    assert_eq!(cs["checks"][0]["state"], "failing", "{cs}");
    assert_eq!(cs["checks"][0]["repo"], "web", "{cs}");

    let (st, out) = w
        .server
        .post(&format!("{CS}/Ic5000001/land"), &w.admin, None);
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("ci / test"),
        "{out}"
    );
    assert_eq!(w.changeset("Ic5000001")["state"], "open");
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// Superseding
// ---------------------------------------------------------------------

#[test]
fn a_new_patchset_on_one_member_supersedes_the_whole_composition() {
    let w = world("cs-ci-patchset");
    let api1 = w.repo_with_change("api", "oa@acme.test", "Iaa000001", "slow", None);
    let web = w.repo_with_change(
        "web",
        "ow@acme.test",
        "Ibb000002",
        "change web",
        Some(SLOW_WHILE_API_SLOW),
    );
    let cs = w.compose("Ic5000001", &[("api", "Iaa000001"), ("web", "Ibb000002")]);
    let first = cs["composition"].as_str().unwrap().to_string();

    let run1 = w
        .wait_runs("web", "the composed job to launch", |runs| {
            runs.iter().any(|r| {
                r["event"] == "changeset"
                    && r["jobs"]
                        .as_array()
                        .is_some_and(|js| js.iter().any(|j| j["state"] == "running"))
            })
        })
        .into_iter()
        .find(|r| r["event"] == "changeset")
        .unwrap();
    let job1 = job(&run1, "test")["id"].as_str().unwrap().to_string();
    let pid1 = w.wait_step("web", &job1);

    // api's author pushes patchset 2. Nothing changed in web, but the
    // thing under test is the *composition*, and that is a different one.
    let api2 = w.patchset("api", "Iaa000001", "quick", None);
    assert_ne!(api1, api2);
    let run1 = w
        .wait_runs("web", "the first composition to be cancelled", |runs| {
            runs.iter()
                .any(|r| r["id"] == run1["id"] && r["state"] != "running")
        })
        .into_iter()
        .find(|r| r["id"] == run1["id"])
        .unwrap();
    assert_eq!(run1["state"], "cancelled", "{run1}");
    assert_eq!(
        run1["error"], "superseded by a new composition of changeset Ic5000001",
        "{run1}"
    );
    assert_eq!(run1["composition"], first, "{run1}");
    // The cancellation reached the machine, which is the only thing that
    // makes superseding save anything; and the machine it freed is the
    // one that builds the new composition.
    let agent = w.agent("box-1");
    assert_stopped_on_the_machine(agent, &job1, pid1);

    let runs = w.wait_all_settled("web", 2);
    let run2 = runs.iter().find(|r| r["id"] != run1["id"]).unwrap();
    assert_eq!(run2["state"], "passed", "{run2}");
    assert_eq!(
        run2["commit_sha"], web,
        "web's own tip did not move: {run2}"
    );
    let second = run2["composition"].as_str().unwrap();
    assert_ne!(second, first);
    let log = w.log("web", job(run2, "test")["id"].as_str().unwrap());
    assert!(
        log.contains("quick") && !log.contains("waiting"),
        "read api's second patchset: {log}"
    );
    assert_eq!(
        agent.took(),
        vec![
            job1.clone(),
            job(run2, "test")["id"].as_str().unwrap().to_string()
        ],
        "{}",
        agent.said()
    );

    // The body shows the current composition only: one check, the green
    // one. The cancelled one belongs to a composition that no longer
    // exists and would otherwise block a gate for a tree nobody is landing.
    let cs = w.changeset("Ic5000001");
    assert_eq!(cs["composition"], second, "{cs}");
    let checks = cs["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 1, "{cs}");
    assert_eq!(checks[0]["state"], "passing");
    assert_eq!(checks[0]["run"], run2["id"]);
    let v = w.verdict("Ic5000001");
    assert_eq!(v["gate"], "ready", "{v}");
    assert!(w.server.healthy());
}

#[test]
fn membership_changes_recompose_and_abandoning_cancels() {
    let w = world("cs-ci-members");
    w.repo_with_change(
        "api",
        "oa@acme.test",
        "Iaa000001",
        "change api",
        Some(SLOW_WHILE_WEB_IN),
    );
    w.repo_with_change("web", "ow@acme.test", "Ibb000002", "change web", None);
    let cs = w.compose("Ic5000001", &[("api", "Iaa000001"), ("web", "Ibb000002")]);
    let with_web = cs["composition"].as_str().unwrap().to_string();
    let run1 = w
        .wait_runs("api", "the composed job to launch", |runs| {
            runs.iter().any(|r| {
                r["jobs"]
                    .as_array()
                    .is_some_and(|js| js.iter().any(|j| j["state"] == "running"))
            })
        })
        .into_iter()
        .next()
        .unwrap();
    let job1 = job(&run1, "test")["id"].as_str().unwrap().to_string();
    let pid1 = w.wait_step("api", &job1);

    // web leaves. api's own tip is unchanged; the composition is not.
    let (st, out) = w
        .server
        .delete(&format!("{CS}/Ic5000001/members/web/Ibb000002"), &w.admin);
    assert_eq!(st, 204, "{out}");
    let alone = w.changeset("Ic5000001")["composition"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(alone, with_web);
    let runs = w.wait_all_settled("api", 2);
    let run1 = runs.iter().find(|r| r["id"] == run1["id"]).unwrap();
    assert_eq!(run1["state"], "cancelled", "{run1}");
    assert_eq!(
        run1["error"], "superseded by a new composition of changeset Ic5000001",
        "{run1}"
    );
    let run2 = runs.iter().find(|r| r["id"] != run1["id"]).unwrap();
    assert_eq!(run2["state"], "passed", "{run2}");
    assert_eq!(run2["composition"], alone, "{run2}");
    assert_stopped_on_the_machine(w.agent("box-1"), &job1, pid1);
    assert_eq!(
        w.changeset("Ic5000001")["checks"].as_array().unwrap().len(),
        1
    );

    // web comes back: the earlier composition again, and the passed run
    // for it is history — a fresh one starts, and waits.
    let (st, out) = w.server.post(
        &format!("{CS}/Ic5000001/members"),
        &w.admin,
        Some(serde_json::json!({"repo": "web", "change": "Ibb000002"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["composition"], with_web, "{out}");
    let run3 = w
        .wait_runs("api", "a third composed run to launch", |runs| {
            runs.len() == 3
                && runs.iter().any(|r| {
                    r["jobs"]
                        .as_array()
                        .is_some_and(|js| js.iter().any(|j| j["state"] == "running"))
                })
        })
        .into_iter()
        .find(|r| r["state"] == "running")
        .unwrap();
    assert_eq!(run3["composition"], with_web, "{run3}");
    let job3 = job(&run3, "test")["id"].as_str().unwrap().to_string();
    let pid3 = w.wait_step("api", &job3);
    let v = w.verdict("Ic5000001");
    assert_eq!(v["gate"], "waiting", "{v}");
    assert_eq!(
        v["waiting_on"],
        serde_json::json!(["api: ci / test"]),
        "{v}"
    );

    let (st, out) = w
        .server
        .post(&format!("{CS}/Ic5000001/abandon"), &w.admin, None);
    assert_eq!(st, 204, "{out}");
    let runs = w.wait_all_settled("api", 3);
    let run3 = runs.iter().find(|r| r["id"] == run3["id"]).unwrap();
    assert_eq!(run3["state"], "cancelled", "{run3}");
    assert_eq!(run3["error"], "changeset abandoned", "{run3}");
    assert_stopped_on_the_machine(w.agent("box-1"), &job3, pid3);
    assert_eq!(w.agent("box-1").took().len(), 3, "{}", w.said());
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// The member token
// ---------------------------------------------------------------------

/// The one case in this file where the test is the machine.
///
/// What is under test is what a composed job's credentials can do, and
/// the real agent — rightly — keeps them in memory, hands the member
/// tokens to `git` through its environment and nowhere else, and writes
/// none of them down. So the machine is registered by the real binary,
/// and the test then asks for work with the credential that registration
/// wrote, exactly as `weft-runner run` does: the claim hands over the
/// job token, and the spec the job token fetches hands over the member
/// tokens. That the agent really *uses* them — fetching the other
/// member's patchset from outside — is what the green case above proves.
#[test]
fn a_member_token_reads_its_own_repository_only_and_dies_with_the_job() {
    let w = world_with("cs-ci-token", &[]);
    let token = registration_token(&w.server, &w.admin, "acme");
    let machine = Registered::register(
        &runner_bin(),
        &w.server.base,
        &token,
        "inspector",
        &[],
        &w.scratch.path().join("inspector"),
    );
    let credential = machine.state()["credential"]
        .as_str()
        .expect("register wrote a credential")
        .to_string();

    w.repo_with_change(
        "api",
        "oa@acme.test",
        "Iaa000001",
        "change api",
        Some(SLOW_WHILE_WEB_IN),
    );
    w.repo_with_change("web", "ow@acme.test", "Ibb000002", "change web", None);
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos",
        &w.admin,
        Some(serde_json::json!({"name": "vault"})),
    );
    assert_eq!(st, 201, "{out}");
    w.compose("Ic5000001", &[("api", "Iaa000001"), ("web", "Ibb000002")]);

    // The claim long-polls and answers 204 when its wait runs out, so
    // ask until it hands something over — as the agent does.
    let claimed = wait_for(
        "the composed job to be handed to the machine",
        Duration::from_secs(30),
        || {
            let (st, body) = w.server.post("/v1/runners/claim", &credential, None);
            (st == 200).then_some(body)
        },
    );
    let job_id = claimed["job_id"].as_str().unwrap().to_string();
    let job_token = claimed["token"].as_str().unwrap().to_string();
    let run = the_one(w.composed_runs("api"));
    assert_eq!(job(&run, "test")["id"], job_id.as_str(), "{run}");
    assert_eq!(job(&run, "test")["state"], "running", "{run}");

    // What a runner sees when it fetches the spec.
    let (st, spec) = w
        .server
        .get(&format!("/v1/runner/jobs/{job_id}"), &job_token);
    assert_eq!(st, 200, "{spec}");
    assert_eq!(spec["event"], "changeset", "{spec}");
    assert_eq!(spec["changeset"]["key"], "Ic5000001", "{spec}");
    let members = spec["changeset"]["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{spec}");
    assert_eq!(members[0]["repo"], "api");
    assert!(
        members[0]["token"].is_null(),
        "the job's own repository is read with the job token: {spec}"
    );
    assert_eq!(members[1]["repo"], "web");
    assert!(
        members[1]["fetch_ref"]
            .as_str()
            .unwrap()
            .starts_with("refs/patchsets/"),
        "{spec}"
    );
    assert!(
        members[1]["clone_url"]
            .as_str()
            .unwrap()
            .ends_with("/acme/web.git"),
        "{spec}"
    );
    let first_web_token = members[1]["token"]
        .as_str()
        .expect("a token for the other member")
        .to_string();

    // Fetching it again — a runner retrying after a dropped answer —
    // mints a fresh set of member tokens and retires the first: a set
    // nobody will use again is a set to kill.
    let (st, again) = w
        .server
        .get(&format!("/v1/runner/jobs/{job_id}"), &job_token);
    assert_eq!(st, 200, "{again}");
    let web_token = again["changeset"]["members"][1]["token"]
        .as_str()
        .expect("a token for the other member")
        .to_string();
    assert_ne!(
        web_token, first_web_token,
        "the second fetch reissued the first set"
    );
    let (st, _) = w.server.get("/v1/orgs/acme/repos/web", &first_web_token);
    assert_eq!(st, 401, "the retired set still reads web");

    // It reads web, and nothing else in the org — not another member,
    // not a repository the changeset never mentioned.
    let (st, _) = w.server.get("/v1/orgs/acme/repos/web", &web_token);
    assert_eq!(st, 200);
    let (st, _) = w.server.get("/v1/orgs/acme/repos/api", &web_token);
    assert_ne!(st, 200, "a member token read another member");
    let (st, _) = w.server.get("/v1/orgs/acme/repos/vault", &web_token);
    assert_ne!(
        st, 200,
        "a member token read a repository outside the changeset"
    );
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos/web/commits",
        &web_token,
        Some(serde_json::json!({"branch": "main", "message": "x", "operations": [put("x", "x")]})),
    );
    // Masked, like every insufficient scope: a read token is not an
    // oracle for what it may not do.
    assert_eq!(st, 404, "a member token wrote: {out}");
    // Two sets were minted, and only the latest is live.
    let live: Vec<_> = w
        .tokens()
        .into_iter()
        .filter(|t| {
            t["label"]
                .as_str()
                .is_some_and(|l| l.starts_with(&format!("ci:{job_id}:")))
        })
        .collect();
    assert_eq!(live.len(), 2, "{live:?}");
    assert_eq!(
        live.iter().filter(|t| t["revoked_at"].is_null()).count(),
        1,
        "{live:?}"
    );

    // Stopping the job takes the credential with it.
    let (st, out) = w.server.post(
        &format!(
            "/v1/orgs/acme/repos/api/workflow-runs/{}/cancel",
            run["id"].as_str().unwrap()
        ),
        &w.admin,
        None,
    );
    assert_eq!(st, 200, "{out}");
    wait_until(
        "the member token to stop reading web once its job is cancelled",
        Duration::from_secs(30),
        || w.server.get("/v1/orgs/acme/repos/web", &web_token).0 == 401,
    );
    assert!(w
        .tokens()
        .iter()
        .filter(|t| t["label"].as_str().is_some_and(|l| l.starts_with("ci:")))
        .all(|t| !t["revoked_at"].is_null()));
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// A member from a fork
// ---------------------------------------------------------------------

const FORK_CHANGE_ID: &str = "Change-Id: I9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f";

/// A composed job in the maintainer's own repository materialises the
/// contributor's tree and may well execute it — `make -C
/// $WEFT_WORKSPACE/widget` is the natural thing to write. So one
/// unapproved fork member holds *every* member's composed run, and the
/// approval that a maintainer already gives the fork change's own
/// workflows releases the whole composition.
///
/// The contributor is bob, a **viewer** of acme: he may read `widget`,
/// fork it into his own namespace and propose a change from there, and
/// may not push to it. In an edition with no public repositories that is
/// the only contributor a fork gate can be about. acme's machine is
/// listening throughout, so "nothing composed ran" is a decision the
/// server made, not a machine that was absent.
#[test]
fn one_member_from_a_fork_holds_every_composed_run_until_it_is_approved() {
    let w = world("cs-ci-fork");
    let server = &w.server;
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
    let mut ada = Browser::signed_in(server, "ada@acme.test", PASSWORD);
    let mut bob = Browser::signed_in(server, "bob@acme.test", PASSWORD);

    // acme's two repositories, each with the composed workflow on trunk;
    // `docs` also has ada's own open change.
    for repo in ["widget", "docs"] {
        let (st, body) = ada.req(
            "POST",
            "/v1/orgs/acme/repos",
            Some(serde_json::json!({"name": repo})),
        );
        assert_eq!(st, 201, "{body}");
        let (st, body) = ada.req(
            "POST",
            &format!("/v1/orgs/acme/repos/{repo}/commits"),
            Some(serde_json::json!({
                "message": "seed",
                "operations": [put("feature.txt", &format!("trunk {repo}\n")), put(".weft/ci.yml", COMPOSED)],
            })),
        );
        assert_eq!(st, 201, "{body}");
    }
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/docs/branches",
        Some(serde_json::json!({"name": "feature", "from": "main"})),
    );
    assert_eq!(st, 201, "{body}");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/docs/commits",
        Some(serde_json::json!({
            "branch": "feature",
            "message": "docs change\n\nChange-Id: Idd000001\n",
            "operations": [put("feature.txt", "change docs\n")],
        })),
    );
    assert_eq!(st, 201, "{body}");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/docs/changes",
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{body}");

    // bob forks widget and contributes.
    let (st, body) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    wait_until(
        "bob's fork of widget to become ready",
        Duration::from_secs(10),
        || bob.req("GET", "/v1/orgs/bob/repos/widget", None).1["fork_state"] == "ready",
    );
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/bob/repos/widget/branches",
        Some(serde_json::json!({"name": "contrib", "from": "main"})),
    );
    assert_eq!(st, 201, "{body}");
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/bob/repos/widget/commits",
        Some(serde_json::json!({
            "message": format!("a contribution\n\n{FORK_CHANGE_ID}"),
            "branch": "contrib",
            "operations": [put("feature.txt", "change widget from bob\n")],
        })),
    );
    assert_eq!(st, 201, "{body}");
    let (st, opened) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/changes",
        Some(serde_json::json!({"from": "contrib", "source": "bob/widget"})),
    );
    assert_eq!(st, 201, "{opened}");
    let fork_key = opened["change"]["key"]
        .as_str()
        .or_else(|| opened["key"].as_str())
        .unwrap()
        .to_string();

    let runs = |who: &mut Browser, repo: &str| -> Vec<serde_json::Value> {
        let (st, out) = who.req(
            "GET",
            &format!("/v1/orgs/acme/repos/{repo}/workflow-runs"),
            None,
        );
        assert_eq!(st, 200, "{out}");
        out["runs"].as_array().cloned().unwrap_or_default()
    };
    let wait = |who: &mut Browser,
                repo: &str,
                what: &str,
                pred: &dyn Fn(&[serde_json::Value]) -> bool|
     -> Vec<serde_json::Value> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let rs = runs(who, repo);
            if pred(&rs) {
                return rs;
            }
            assert!(
                Instant::now() < deadline,
                "waited 60s for {what} in {repo}: {rs:?}\n{}",
                w.said()
            );
            std::thread::sleep(POLL);
        }
    };
    // docs' own change run passes; widget's fork change run is held.
    wait(&mut ada, "docs", "the change run to pass", &|rs| {
        rs.iter()
            .any(|r| r["event"] == "change" && r["state"] == "passed")
    });
    let held = runs(&mut ada, "widget")
        .into_iter()
        .find(|r| r["event"] == "change")
        .unwrap();
    assert_eq!(held["state"], "blocked", "{held}");

    let (st, cs) = ada.req(
        "POST",
        "/v1/orgs/acme/changesets",
        Some(
            serde_json::json!({"key": "Ic5000001", "title": "compose", "members": [
            {"repo": "widget", "change": fork_key}, {"repo": "docs", "change": "Idd000001"}]}),
        ),
    );
    assert_eq!(st, 201, "{cs}");
    // The composed runs are what everything below reads, so wait for
    // them: one per member, and the assertions that follow are about the
    // state they were created in.
    for repo in ["widget", "docs"] {
        wait(&mut ada, repo, "the composed run to be created", &|rs| {
            rs.iter().any(|r| r["event"] == "changeset")
        });
    }
    // The claim after that is an absence — that neither composed run was
    // handed to the machine that is listening — so give it
    // `NEGATIVE_WINDOW`, a claim poll's worth of chances, to have taken
    // one anyway.
    std::thread::sleep(NEGATIVE_WINDOW);
    // Both composed runs exist, and both are held — docs' too, though
    // docs' change is ada's own.
    let widget_cs = runs(&mut ada, "widget")
        .into_iter()
        .find(|r| r["event"] == "changeset")
        .unwrap_or_else(|| panic!("no composed run in widget"));
    let docs_cs = runs(&mut ada, "docs")
        .into_iter()
        .find(|r| r["event"] == "changeset")
        .unwrap_or_else(|| panic!("no composed run in docs"));
    assert_eq!(widget_cs["state"], "blocked", "{widget_cs}");
    assert_eq!(widget_cs["blocked_reason"], "fork", "{widget_cs}");
    assert_eq!(
        widget_cs["error"],
        "this change comes from a fork; a maintainer has to approve its workflows before they run",
        "{widget_cs}"
    );
    assert_eq!(docs_cs["state"], "blocked", "{docs_cs}");
    assert_eq!(docs_cs["blocked_reason"], "fork", "{docs_cs}");
    assert_eq!(docs_cs["error"], format!("member widget/{fork_key} comes from a fork; a maintainer has to approve its workflows before the changeset's runs"), "{docs_cs}");
    // Nothing composed got a machine: a held run has no jobs to hand
    // out, and the one job the machine did take is ada's own change in
    // docs.
    assert!(
        widget_cs["jobs"].as_array().unwrap().is_empty(),
        "{widget_cs}"
    );
    assert!(docs_cs["jobs"].as_array().unwrap().is_empty(), "{docs_cs}");
    let docs_change_job = runs(&mut ada, "docs")
        .into_iter()
        .find(|r| r["event"] == "change")
        .map(|r| job(&r, "test")["id"].as_str().unwrap().to_string())
        .expect("docs' own change run");
    let agent = w.agent("box-1");
    assert_eq!(agent.took(), vec![docs_change_job], "{}", agent.said());
    let (st, v) = ada.req("GET", "/v1/orgs/acme/changesets/Ic5000001/verdict", None);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["gate"], "waiting", "{v}");
    let (st, body) = ada.req("GET", "/v1/orgs/acme/changesets/Ic5000001", None);
    assert_eq!(st, 200, "{body}");
    assert!(
        body["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["state"] == "queued"),
        "{body}"
    );

    // ada approves the contributor's workflows — once, on the fork change,
    // as she would have to anyway — and the whole composition runs.
    let (st, body) = ada.req(
        "POST",
        &format!("/v1/orgs/acme/repos/widget/changes/{fork_key}/workflows/approve"),
        None,
    );
    assert_eq!(st, 202, "{body}");
    // Two runs each — `COMPOSED` has no push trigger, so the seed commit
    // started nothing; the held change run and the held composed run are
    // both released.
    let widget_runs = wait(&mut ada, "widget", "every run to settle", &|rs| {
        rs.len() == 2 && rs.iter().all(|r| r["state"] == "passed")
    });
    let docs_runs = wait(&mut ada, "docs", "every run to settle", &|rs| {
        rs.len() == 2 && rs.iter().all(|r| r["state"] == "passed")
    });
    assert_eq!(
        widget_runs
            .iter()
            .filter(|r| r["event"] == "changeset")
            .count(),
        1,
        "{widget_runs:?}"
    );
    assert_eq!(
        docs_runs
            .iter()
            .filter(|r| r["event"] == "changeset")
            .count(),
        1,
        "the placeholder was replaced, not doubled: {docs_runs:?}"
    );
    // Every one of them on acme's machine: docs' change run before the
    // hold, and after it widget's change run and both composed runs.
    assert_eq!(w.agent("box-1").took().len(), 4, "{}", w.said());
    let (st, body) = ada.req("GET", "/v1/orgs/acme/changesets/Ic5000001", None);
    assert_eq!(st, 200, "{body}");
    let checks = body["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 2, "{body}");
    assert!(checks.iter().all(|c| c["state"] == "passing"), "{body}");
    let (st, v) = ada.req("GET", "/v1/orgs/acme/changesets/Ic5000001/verdict", None);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["gate"], "ready", "{v}");
    assert!(server.healthy());
}

/// Deleting a repository takes its changes with it, and a change's
/// membership goes with the change. Nothing announces that to the
/// changeset, so the combination it is at changes under it — and with the
/// last member gone there is no combination at all. Every read of the
/// changeset has to survive both, and the composed run that was started
/// for the old combination is left where it is rather than cancelled
/// into a state nothing would replace — including when a membership
/// change arrives at a changeset that can no longer be composed at all.
#[test]
fn deleting_member_repositories_leaves_the_changeset_readable_and_its_runs_alone() {
    let w = world("cs-ci-deleted");
    w.repo_with_change(
        "api",
        "oa@acme.test",
        "Iaa000001",
        "change api",
        Some(COMPOSED),
    );
    w.repo_with_change(
        "web",
        "ow@acme.test",
        "Ibb000002",
        "change web",
        Some(COMPOSED),
    );
    let cs = w.compose("Ic5000001", &[("api", "Iaa000001"), ("web", "Ibb000002")]);
    let both = cs["composition"].as_str().unwrap().to_string();
    w.wait_all_settled("api", 2);
    w.wait_all_settled("web", 2);
    assert_eq!(
        w.changeset("Ic5000001")["checks"].as_array().unwrap().len(),
        2
    );

    // web goes. The changeset is api alone now: a combination nothing has
    // built, so it has no checks — and api's run for the old one is
    // still there, passed, at the composition it was for.
    let (st, out) = w.server.delete("/v1/orgs/acme/repos/web", &w.admin);
    assert_eq!(st, 204, "{out}");
    let cs = w.changeset("Ic5000001");
    let alone = cs["composition"].as_str().expect("still composable");
    assert_ne!(alone, both, "{cs}");
    assert_eq!(cs["members"].as_array().unwrap().len(), 1, "{cs}");
    assert_eq!(cs["checks"], serde_json::json!([]), "{cs}");
    let runs = w.composed_runs("api");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["state"], "passed", "{runs:?}");
    assert_eq!(runs[0]["composition"], both, "{runs:?}");
    let (st, v) = w.server.get(&format!("{CS}/Ic5000001/verdict"), &w.admin);
    assert_eq!(st, 200, "{v}");
    // No composed check exists at the combination the changeset is now
    // at, so CI has nothing to say; approval is what is still missing.
    assert_eq!(v["gate"], "ready", "{v}");
    assert_eq!(v["landable"], false, "{v}");

    // Composed CI goes on for what is left: api's next patchset is built
    // as the combination the body shows — api alone — and the workspace
    // holds exactly that. It used to be that a deleted member made the
    // changeset uncomposable for good, so nothing composed ever ran for
    // it again while the body went on listing api as a member.
    let api2 = w.patchset("api", "Iaa000001", "api after web", None);
    let runs = w.wait_all_settled("api", 4);
    let after = runs
        .iter()
        .find(|r| r["event"] == "changeset" && r["commit_sha"] == api2)
        .unwrap_or_else(|| panic!("a composed run at api's new tip: {runs:?}"));
    assert_eq!(after["state"], "passed", "{after}");
    let alone_now = after["composition"].as_str().unwrap();
    assert_ne!(alone_now, both, "{after}");
    let log = w.log("api", job(after, "test")["id"].as_str().unwrap());
    let members: Vec<serde_json::Value> = serde_json::from_str(
        log.lines()
            .find_map(|l| l.strip_prefix("members="))
            .unwrap_or_else(|| panic!("{log}")),
    )
    .unwrap();
    assert_eq!(members.len(), 1, "{log}");
    assert_eq!(members[0]["repo"], "api", "{log}");
    let cs = w.changeset("Ic5000001");
    assert_eq!(cs["composition"], alone_now, "{cs}");
    assert_eq!(cs["checks"].as_array().unwrap().len(), 1, "{cs}");
    assert_eq!(cs["checks"][0]["state"], "passing", "{cs}");

    // api leaves. The only member row left is web's, and web's
    // repository is gone, so there is no combination at all: recomposing
    // has nothing to build and nothing to replace api's runs with, and it
    // leaves them where they are — a membership change that can be built
    // supersedes the old combination's runs; one that cannot does not
    // cancel them into a state nothing would restart.
    let (st, out) = w
        .server
        .delete(&format!("{CS}/Ic5000001/members/api/Iaa000001"), &w.admin);
    assert_eq!(st, 204, "{out}");
    let cs = w.changeset("Ic5000001");
    assert_eq!(cs["members"], serde_json::json!([]), "{cs}");
    assert_eq!(cs["composition"], serde_json::Value::Null, "{cs}");
    assert_eq!(cs["checks"], serde_json::json!([]), "{cs}");
    let runs = w.composed_runs("api");
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(
        runs.iter().all(|r| r["state"] == "passed"),
        "api's composed runs are left alone: {runs:?}"
    );
    let (st, v) = w.server.get(&format!("{CS}/Ic5000001/verdict"), &w.admin);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["explanation"], "changeset has no members", "{v}");

    // And the repository going after that changes nothing the body says.
    let (st, out) = w.server.delete("/v1/orgs/acme/repos/api", &w.admin);
    assert_eq!(st, 204, "{out}");
    let cs = w.changeset("Ic5000001");
    assert_eq!(cs["members"], serde_json::json!([]), "{cs}");
    assert_eq!(cs["composition"], serde_json::Value::Null, "{cs}");
    assert!(w.server.healthy());
}
