//! Somebody else's machines, driven the way somebody else's machine
//! drives them.
//!
//! Every case here goes over HTTP with nothing of ours on the client
//! side: a registration token minted through the dashboard's route, a
//! `POST /v1/runners/register` with a bearer that is not a session and
//! not an API token, a `POST /v1/runners/claim` that blocks, and then
//! exactly the five per-job calls a runner makes. That is the point of
//! the suite — a runner is a separate process on somebody's network,
//! and a test that reached in through a Rust function would prove our
//! code agrees with itself and nothing about the wire.
//!
//! The two properties everything else hangs off:
//!
//! * **A repository has to be admitted to a machine.** The group's
//!   repository access and the organisation's policy are two different
//!   people's decisions, and each is checked at trigger time (so a
//!   person reads a sentence) and again at claim time (so a policy
//!   change catches a queued job).
//! * **Every machine belongs to the organisation that registered it**,
//!   so the fork gate applies to every job: the thing being protected
//!   is the machine's owner.

use std::time::{Duration, Instant};
use stratum_testkit::browser::Browser;
use stratum_testkit::gitcli::Scratch;
use stratum_testkit::mailbox::Mailbox;
use stratum_testkit::{Minio, Server};

const PASSWORD: &str = "a long enough password";

/// A workflow for a machine the organisation registered.
const MINE: &str = "\
name: ci
on: [push, change]
jobs:
  test:
    runs-on: [self-hosted]
    steps:
      - name: Work
        run: echo mine
";

/// The same, but asking for a machine with a GPU.
const GPU: &str = "\
name: ci
on: push
jobs:
  test:
    runs-on: [self-hosted, gpu]
    steps:
      - name: Work
        run: echo gpu
";

/// A self-hosted file that also asks for a container image — which is
/// the one thing a self-hosted job cannot have.
const IMAGED: &str = "\
name: ci
on: push
jobs:
  test:
    runs-on: [self-hosted]
    image: rust:1.83
    steps:
      - name: Work
        run: cargo test
";

// ---------------------------------------------------------------------
// The world
// ---------------------------------------------------------------------

struct World {
    server: Server,
    admin: String,
    #[allow(dead_code)]
    scratch: Scratch,
}

fn world(hint: &str) -> World {
    world_with(hint, &[])
}

fn world_with(hint: &str, extra: &[(&str, String)]) -> World {
    let minio = Minio::shared();
    let bucket = minio.bucket(hint);
    let scratch = Scratch::new(hint);
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_RUNNER_POLL_SECS", "1")
        // The claim's long poll, shortened so a "nothing to do" answer
        // is a fast 204 rather than twenty seconds of test.
        .env("STRATUM_RUNNER_CLAIM_WAIT_MS", "700")
        .envs(extra)
        .start();
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    assert_eq!(st, 201, "{out}");
    World {
        server,
        admin,
        scratch,
    }
}

impl World {
    fn commit(&self, repo: &str, branch: &str, files: &[(&str, &str)]) -> String {
        let ops: Vec<serde_json::Value> = files
            .iter()
            .map(|(p, c)| serde_json::json!({"op": "put", "path": p, "content": c}))
            .collect();
        let (st, out) = self.server.post(
            &format!("/v1/orgs/acme/repos/{repo}/commits"),
            &self.admin,
            Some(serde_json::json!({
                "branch": branch, "message": "add ci", "operations": ops,
            })),
        );
        assert_eq!(st, 201, "commit to {repo}/{branch}: {out}");
        out["commit"].as_str().unwrap().to_string()
    }

    fn runs(&self, repo: &str) -> Vec<serde_json::Value> {
        let (st, out) = self.server.get(
            &format!("/v1/orgs/acme/repos/{repo}/workflow-runs"),
            &self.admin,
        );
        assert_eq!(st, 200, "{out}");
        out["runs"].as_array().cloned().unwrap_or_default()
    }

    fn run_for(&self, repo: &str, sha: &str) -> serde_json::Value {
        self.wait_run(repo, sha, "the run to exist", |_| true)
    }

    /// Poll until a run for `sha` exists and satisfies `pred`.
    fn wait_run(
        &self,
        repo: &str,
        sha: &str,
        what: &str,
        pred: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let runs = self.runs(repo);
            if let Some(r) = runs
                .iter()
                .find(|r| r["commit_sha"] == sha)
                .filter(|r| pred(r))
            {
                return r.clone();
            }
            assert!(
                Instant::now() < deadline,
                "waited 60s for {what}; runs were:\n{}",
                serde_json::to_string_pretty(&runs).unwrap()
            );
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    fn wait_settled(&self, repo: &str, sha: &str) -> serde_json::Value {
        self.wait_run(repo, sha, "the run to settle", |r| r["state"] != "running")
    }

    /// A registration token, as the dashboard's Add-a-runner button
    /// mints one.
    fn registration_token(&self, group: Option<&str>) -> serde_json::Value {
        let body = group.map(|g| serde_json::json!({ "group": g }));
        let (st, out) = self.server.post(
            "/v1/orgs/acme/runners/registration-token",
            &self.admin,
            body,
        );
        assert_eq!(st, 201, "mint registration token: {out}");
        out
    }

    fn register(&self, name: &str, labels: &[&str], group: Option<&str>) -> Sh {
        let token = self.registration_token(group);
        Sh::register(
            &self.server,
            token["token"].as_str().unwrap(),
            name,
            labels,
            false,
        )
        .expect("registration should have been accepted")
    }

    fn runner_list(&self) -> Vec<serde_json::Value> {
        let (st, out) = self.server.get("/v1/orgs/acme/runners", &self.admin);
        assert_eq!(st, 200, "{out}");
        out["runners"].as_array().cloned().unwrap_or_default()
    }

    fn policy(&self) -> serde_json::Value {
        let (st, out) = self.server.get("/v1/orgs/acme/runner-policy", &self.admin);
        assert_eq!(st, 200, "{out}");
        out
    }

    fn set_policy(&self, body: serde_json::Value) -> (u16, serde_json::Value) {
        self.server.req(
            "PATCH",
            "/v1/orgs/acme/runner-policy",
            &self.admin,
            Some(body),
        )
    }

    fn groups(&self) -> Vec<serde_json::Value> {
        let (st, out) = self.server.get("/v1/orgs/acme/runner-groups", &self.admin);
        assert_eq!(st, 200, "{out}");
        out["groups"].as_array().cloned().unwrap_or_default()
    }

    fn default_group(&self) -> serde_json::Value {
        self.groups()
            .into_iter()
            .find(|g| g["is_default"] == true)
            .expect("every organisation has a default group")
    }

    fn audit(&self) -> Vec<serde_json::Value> {
        let (st, out) = self
            .server
            .get("/v1/orgs/acme/audit?limit=100", &self.admin);
        assert_eq!(st, 200, "{out}");
        out["entries"]
            .as_array()
            .or_else(|| out["audit"].as_array())
            .cloned()
            .unwrap_or_default()
    }

    fn audited(&self, action: &str) -> bool {
        self.audit().iter().any(|e| e["action"] == action)
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

// ---------------------------------------------------------------------
// A self-hosted runner, as a client
// ---------------------------------------------------------------------

/// What `weft-runner register` writes to `.runner`, and what `run`
/// then does with it — over plain HTTP, with no helper of ours.
#[derive(Debug)]
struct Sh {
    id: String,
    credential: String,
    labels: Vec<String>,
    group: String,
}

/// One claim's answer.
enum Claimed {
    Job {
        job_id: String,
        token: String,
        runner_url: String,
    },
    Nothing,
    Dead,
    Busy,
}

impl Sh {
    fn register(
        server: &Server,
        token: &str,
        name: &str,
        labels: &[&str],
        ephemeral: bool,
    ) -> Result<Sh, (u16, serde_json::Value)> {
        let (st, out) = server.post(
            "/v1/runners/register",
            token,
            Some(serde_json::json!({
                "name": name,
                "labels": labels,
                "os": "linux",
                "arch": "x64",
                "version": "0.1.0-test",
                "ephemeral": ephemeral,
            })),
        );
        if st != 201 {
            return Err((st, out));
        }
        Ok(Sh {
            id: out["runner_id"].as_str().unwrap().to_string(),
            credential: out["credential"].as_str().unwrap().to_string(),
            labels: out["labels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l.as_str().unwrap().to_string())
                .collect(),
            group: out["group"].as_str().unwrap().to_string(),
        })
    }

    fn claim(&self, server: &Server) -> Claimed {
        let (st, out) = server.post("/v1/runners/claim", &self.credential, None);
        match st {
            200 => Claimed::Job {
                job_id: out["job_id"].as_str().unwrap().to_string(),
                token: out["token"].as_str().unwrap().to_string(),
                runner_url: out["runner_url"].as_str().unwrap_or_default().to_string(),
            },
            204 => Claimed::Nothing,
            401 => Claimed::Dead,
            409 => {
                assert_eq!(out["error"], "busy", "{out}");
                Claimed::Busy
            }
            other => panic!("claim answered {other}: {out}"),
        }
    }

    /// Keep asking until a job comes, the way `weft-runner run` does.
    fn wait_for_job(&self, server: &Server, what: &str) -> (String, String) {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            match self.claim(server) {
                Claimed::Job {
                    job_id,
                    token,
                    runner_url,
                } => {
                    // The address to report on rides in the claim rather
                    // than being assumed from the URL the runner
                    // registered with: the two are the same behind a CDN
                    // and differ on a private network, and a runner that
                    // guessed would fail its first call with a network
                    // error saying nothing about which half was wrong.
                    assert!(
                        runner_url.starts_with("http://"),
                        "the claim handed over no address to report on: {runner_url:?}"
                    );
                    return (job_id, token);
                }
                Claimed::Nothing => {}
                Claimed::Dead => panic!("the runner was removed while waiting for {what}"),
                Claimed::Busy => panic!("the runner was busy while waiting for {what}"),
            }
            assert!(Instant::now() < deadline, "waited 45s for {what}");
        }
    }

    /// Ask once and expect nothing — the 204 the long poll ends with.
    fn expect_nothing(&self, server: &Server, why: &str) {
        match self.claim(server) {
            Claimed::Nothing => {}
            Claimed::Job { job_id, .. } => panic!("{why}: was handed job {job_id}"),
            Claimed::Dead => panic!("{why}: the credential was refused"),
            Claimed::Busy => panic!("{why}: reported busy"),
        }
    }
}

/// The five per-job calls, exactly as `runner_api_e2e` drives them.
fn spec(server: &Server, job_id: &str, token: &str) -> (u16, serde_json::Value) {
    server.get(&format!("/v1/runner/jobs/{job_id}"), token)
}

fn lease(server: &Server, job_id: &str, token: &str) -> (u16, serde_json::Value) {
    server.post(&format!("/v1/runner/jobs/{job_id}/lease"), token, None)
}

fn log_chunk(server: &Server, job_id: &str, token: &str, seq: i32, text: &str) -> u16 {
    server
        .post(
            &format!("/v1/runner/jobs/{job_id}/log"),
            token,
            Some(serde_json::json!({ "seq": seq, "text": text })),
        )
        .0
}

fn put_log(server: &Server, job_id: &str, token: &str, text: &str) -> u16 {
    let mut r = ureq::request(
        "PUT",
        &format!("{}/v1/runner/jobs/{job_id}/log", server.base),
    );
    r = r.set("Authorization", &format!("Bearer {token}"));
    match r.send_string(text) {
        Ok(x) => x.status(),
        Err(ureq::Error::Status(s, _)) => s,
        Err(e) => panic!("transport: {e}"),
    }
}

fn finish(
    server: &Server,
    job_id: &str,
    token: &str,
    state: &str,
    error: Option<&str>,
    abuse: Option<&str>,
) -> (u16, serde_json::Value) {
    let mut body = serde_json::json!({ "state": state });
    if let Some(e) = error {
        body["error"] = e.into();
    }
    if let Some(a) = abuse {
        body["abuse"] = a.into();
    }
    server.post(
        &format!("/v1/runner/jobs/{job_id}/finish"),
        token,
        Some(body),
    )
}

// ---------------------------------------------------------------------
// The whole way round
// ---------------------------------------------------------------------

/// Register, claim, read the spec, heartbeat, stream a log, report a
/// verdict — and see it where a person looks for it.
///
/// The one test that proves the feature exists. Everything after it is a
/// rule about which machine gets which job, and none of those rules
/// matter if this does not work.
#[test]
fn a_registered_machine_takes_a_job_and_its_verdict_reaches_the_run() {
    let w = world("sh-happy");
    let sh = w.register("build-box", &["gpu"], None);
    assert_eq!(
        sh.labels,
        vec!["self-hosted", "linux", "x64", "gpu"],
        "the server adds self-hosted, the OS and the architecture"
    );
    assert_eq!(sh.group, "default");

    let sha = w.commit("app", "main", &[(".weft/ci.yml", MINE)]);
    let (job_id, token) = sh.wait_for_job(&w.server, "the pushed job");

    // The spec carries what the job asked for — its labels, in the
    // order the file wrote them — and the commit to check out.
    let (st, spec) = spec(&w.server, &job_id, &token);
    assert_eq!(st, 200, "{spec}");
    assert_eq!(spec["job"], "test", "{spec}");
    assert_eq!(spec["commit_sha"], sha, "{spec}");
    assert_eq!(spec["fetch_ref"], "refs/heads/main", "{spec}");
    assert_eq!(spec["labels"][0], "self-hosted", "{spec}");
    assert!(
        spec["clone_url"]
            .as_str()
            .unwrap()
            .ends_with("/acme/app.git"),
        "{spec}"
    );

    // While it is working, the list says so and names the job — which is
    // the only thing on the runners page that is not in the row.
    let listed = w.runner_list();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["state"], "busy", "{:?}", listed[0]);
    assert_eq!(listed[0]["job"]["job_id"], job_id, "{:?}", listed[0]);
    assert_eq!(listed[0]["job"]["key"], "test", "{:?}", listed[0]);
    // The name, not the id: the page links to `/acme/app/…` and cannot
    // build that from a repository id.
    assert_eq!(listed[0]["job"]["repo"], "app", "{:?}", listed[0]);
    // …and a second claim from the same machine is refused rather than
    // handed work nobody would ever report on.
    assert!(matches!(sh.claim(&w.server), Claimed::Busy));

    assert_eq!(lease(&w.server, &job_id, &token).0, 200);
    assert_eq!(log_chunk(&w.server, &job_id, &token, 1, "mine\n"), 200);
    assert_eq!(put_log(&w.server, &job_id, &token, "mine\n"), 200);
    let (st, out) = finish(&w.server, &job_id, &token, "passed", None, None);
    assert_eq!(st, 200, "{out}");

    let run = w.wait_settled("app", &sha);
    assert_eq!(run["state"], "passed", "{run}");
    let test = job(&run, "test");
    assert_eq!(test["pool"], "self_hosted", "{test}");
    assert_eq!(test["labels"], serde_json::json!(["self-hosted"]), "{test}");
    assert_eq!(test["runner"]["id"], sh.id, "{test}");
    assert_eq!(test["runner"]["name"], "build-box", "{test}");

    // The log a person reads is the one the machine uploaded.
    let (st, log) = w.server.get(
        &format!(
            "/v1/orgs/acme/repos/app/workflow-jobs/{}/log",
            test["id"].as_str().unwrap()
        ),
        &w.admin,
    );
    assert_eq!(st, 200);
    assert!(log.as_str().unwrap().contains("mine"), "{log}");

    // The verdict reached the commit's checks, so the land gate sees it.
    let (st, checks) = w.server.get(
        &format!("/v1/orgs/acme/repos/app/commits/{sha}/checks"),
        &w.admin,
    );
    assert_eq!(st, 200, "{checks}");
    let row = checks["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "ci / test")
        .unwrap_or_else(|| panic!("no mirrored check row: {checks}"));
    assert_eq!(row["state"], "passing", "{row}");
    assert_eq!(row["provider"], "weft", "{row}");

    // And the machine goes back to idle, with the job gone from its row.
    let listed = w.runner_list();
    assert_ne!(listed[0]["state"], "busy", "{:?}", listed[0]);
    assert!(listed[0]["job"].is_null(), "{:?}", listed[0]);
    sh.expect_nothing(&w.server, "the queue is empty");

    // Every mutation left a trail entry.
    assert!(w.audited("runner.registration_token.created"));
    assert!(w.audited("runner.registered"));
}

/// A file that asks for a hosted runner is refused on its line, and no
/// machine is offered anything for it.
///
/// There is no fleet behind this server, and `runs-on: ubuntu-latest`
/// is what a pasted Actions file says. Queueing it would leave a check
/// waiting for a runner nobody will ever register; handing it to one of
/// the organisation's own machines would run it somewhere its author
/// did not ask for.
#[test]
fn a_file_that_asks_for_a_hosted_runner_is_refused_on_its_line() {
    let w = world("sh-hosted");
    let sh = w.register("build-box", &[], None);
    let sha = w.commit(
        "app",
        "main",
        &[(
            ".weft/ci.yml",
            "name: ci\non: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hosted\n",
        )],
    );
    let refused = w.wait_settled("app", &sha);
    assert_eq!(refused["state"], "failed", "{refused}");
    let error = refused["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("`runs-on: ubuntu-latest` names a hosted runner, and this server has none"),
        "{refused}"
    );
    assert!(refused["blocked_reason"].is_null(), "{refused}");
    sh.expect_nothing(&w.server, "a machine was offered a hosted job");
}

/// Labels are a subset test, and a job asking for one nobody has is
/// refused at trigger time rather than queued forever — GitHub's
/// most common support question, answered with a sentence.
#[test]
fn a_job_routes_only_to_a_machine_with_every_label_it_asked_for() {
    let w = world("sh-labels");
    let plain = w.register("plain", &[], None);

    // Nothing has `gpu` yet, so the file is refused where its author can
    // read the refusal.
    let sha = w.commit("app", "main", &[(".weft/ci.yml", GPU)]);
    let refused = w.wait_settled("app", &sha);
    assert_eq!(refused["state"], "failed", "{refused}");
    assert_eq!(
        refused["error"],
        "no runner with labels [self-hosted, gpu] is registered for this repository",
        "{refused}"
    );
    assert!(
        refused["blocked_reason"].is_null(),
        "nothing lifts this by itself, so it must not read as waiting: {refused}"
    );

    // Register one that has it, push again, and only that machine is
    // offered the work.
    let gpu = w.register("gpu-box", &["gpu"], None);
    let sha = w.commit("app", "main", &[("README.md", "# gpu\n")]);
    let (job_id, token) = gpu.wait_for_job(&w.server, "the gpu job");
    plain.expect_nothing(&w.server, "a machine without gpu was offered a gpu job");
    assert_eq!(
        finish(&w.server, &job_id, &token, "passed", None, None).0,
        200
    );

    let run = w.wait_settled("app", &sha);
    assert_eq!(run["state"], "passed", "{run}");
    assert_eq!(job(&run, "test")["runner"]["name"], "gpu-box", "{run}");
}

/// The organisation's policy, each setting, and the sentence it leaves
/// behind.
#[test]
fn the_policy_refuses_in_its_own_words() {
    let w = world("sh-policy");
    w.register("build-box", &[], None);

    // Allowed by default: a file for the organisation's machines runs.
    let sh = w.register("box-2", &[], None);
    let sha = w.commit("app", "mine-branch", &[(".weft/ci.yml", MINE)]);
    let (job_id, token) = sh.wait_for_job(&w.server, "the self-hosted job");
    assert_eq!(
        finish(&w.server, &job_id, &token, "passed", None, None).0,
        200
    );
    assert_eq!(w.wait_settled("app", &sha)["state"], "passed");

    // Off: now it is refused, in the policy's own words.
    let (st, out) = w.set_policy(serde_json::json!({ "self_hosted": "disabled" }));
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["self_hosted"], "disabled", "{out}");
    assert!(w.audited("runner_policy.updated"));
    let sha = w.commit("app", "mine-branch", &[("README.md", "# again\n")]);
    let refused = w.wait_settled("app", &sha);
    assert_eq!(
        refused["error"],
        "self-hosted runners are not allowed for this repository (organisation policy)",
        "{refused}"
    );

    // `selected`, naming a different repository, refuses the same way —
    // the refusal is about *this* repository, not about the switch.
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos",
        &w.admin,
        Some(serde_json::json!({"name": "other"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = w.set_policy(serde_json::json!({
        "self_hosted": "selected", "self_hosted_repos": ["other"],
    }));
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        out["self_hosted_repos"],
        serde_json::json!(["other"]),
        "{out}"
    );
    let sha = w.commit("app", "mine-branch", &[("README.md", "# third\n")]);
    assert_eq!(
        w.wait_settled("app", &sha)["error"],
        "self-hosted runners are not allowed for this repository (organisation policy)"
    );

    // Naming this one lets it through.
    let (st, out) = w.set_policy(serde_json::json!({ "self_hosted_repos": ["app", "other"] }));
    assert_eq!(st, 200, "{out}");
    let sha = w.commit("app", "mine-branch", &[("README.md", "# fourth\n")]);
    let (job_id, token) = sh.wait_for_job(&w.server, "the job in a selected repository");
    assert_eq!(
        finish(&w.server, &job_id, &token, "passed", None, None).0,
        200
    );
    assert_eq!(w.wait_settled("app", &sha)["state"], "passed");

    // A value outside the enumeration is refused, and nothing moves.
    assert_eq!(
        w.set_policy(serde_json::json!({ "self_hosted": "some" })).0,
        422
    );
    assert_eq!(w.policy()["self_hosted"], "selected");
}

/// The last of the five refusals: a repository whose organisation has
/// never registered anything.
#[test]
fn a_file_for_a_pool_with_no_machines_says_so_at_trigger_time() {
    let w = world("sh-no-runners");
    let sha = w.commit("app", "main", &[(".weft/ci.yml", MINE)]);
    let refused = w.wait_settled("app", &sha);
    assert_eq!(refused["state"], "failed", "{refused}");
    assert_eq!(
        refused["error"], "no runner with labels [self-hosted] is registered for this repository",
        "{refused}"
    );
    assert!(refused["blocked_reason"].is_null(), "{refused}");
}

/// A miner caught on the organisation's own machine fails the job and
/// stops there.
///
/// The watch is protecting the machine's owner from a stranger's change.
/// There is no compute of ours being spent, so there is nothing to
/// suspend: the job fails in the runner's words and the audit log says
/// so.
#[test]
fn a_miner_on_a_self_hosted_machine_fails_the_job_and_nothing_else() {
    let w = world("sh-abuse");
    let sh = w.register("build-box", &[], None);
    let sha = w.commit("app", "main", &[(".weft/ci.yml", MINE)]);
    let (job_id, token) = sh.wait_for_job(&w.server, "the job the miner is in");

    let (st, out) = finish(
        &w.server,
        &job_id,
        &token,
        "failed",
        Some("mining software detected: xmrig"),
        Some("mining"),
    );
    assert_eq!(st, 200, "{out}");

    let run = w.wait_settled("app", &sha);
    assert_eq!(run["state"], "failed", "{run}");
    assert_eq!(
        job(&run, "test")["error"],
        "mining software detected: xmrig",
        "{run}"
    );

    // And the organisation's next push still runs on its machine: the
    // watch stopped one job, it did not stop the organisation.
    let sha = w.commit("app", "main", &[("README.md", "# after\n")]);
    let (job_id, token) = sh.wait_for_job(&w.server, "the next job after the miner");
    assert_eq!(
        finish(&w.server, &job_id, &token, "passed", None, None).0,
        200
    );
    assert_eq!(w.wait_settled("app", &sha)["state"], "passed");

    // It is recorded, where an owner deciding what to do about the
    // repository will look.
    let entry = w
        .audit()
        .into_iter()
        .find(|e| e["action"] == "workflow.abuse")
        .expect("the abuse was recorded");
    assert_eq!(entry["context"]["pool"], "self_hosted", "{entry}");
    assert_eq!(entry["context"]["abuse"], "mining", "{entry}");
    assert!(
        !w.audited("workflow.suspended"),
        "a suspension was recorded for a self-hosted job"
    );
}

/// Removing a machine kills its credential and fails what it was in the
/// middle of — failed, not handed to the next machine.
#[test]
fn removing_a_machine_stops_its_credential_and_fails_its_job() {
    let w = world("sh-remove");
    let sh = w.register("build-box", &[], None);
    let spare = w.register("spare", &[], None);
    let sha = w.commit("app", "main", &[(".weft/ci.yml", MINE)]);
    let (job_id, token) = sh.wait_for_job(&w.server, "the job to remove out from under");

    let (st, out) = w
        .server
        .delete(&format!("/v1/orgs/acme/runners/{}", sh.id), &w.admin);
    assert_eq!(st, 204, "{out}");
    assert!(w.audited("runner.removed"));

    let run = w.wait_settled("app", &sha);
    assert_eq!(run["state"], "failed", "{run}");
    assert_eq!(
        job(&run, "test")["error"],
        "runner removed while the job was running",
        "{run}"
    );
    // Not re-queued. The operator asked for the machine to stop, and a
    // silent re-run somewhere else is the opposite of that.
    spare.expect_nothing(&w.server, "a removed runner's job was handed on");

    // The credential is dead from now, on both surfaces it reaches.
    assert!(matches!(sh.claim(&w.server), Claimed::Dead));
    let (st, _) = spec(&w.server, &job_id, &token);
    assert!(
        st == 401 || st == 410,
        "the job token outlived the runner: {st}"
    );

    // And it is gone from the list rather than sitting there offline.
    let names: Vec<String> = w
        .runner_list()
        .iter()
        .map(|r| r["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(names, vec!["spare".to_string()], "{names:?}");
}

/// An ephemeral machine exists for one job and is gone the moment that
/// job reaches a verdict — GitHub's ephemeral+autoscale shape.
#[test]
fn an_ephemeral_machine_is_retired_after_its_one_job() {
    let w = world("sh-ephemeral");
    let token = w.registration_token(None);
    let sh = Sh::register(
        &w.server,
        token["token"].as_str().unwrap(),
        "once",
        &[],
        true,
    )
    .expect("registration");
    assert!(w.runner_list()[0]["ephemeral"].as_bool().unwrap());

    let sha = w.commit("app", "main", &[(".weft/ci.yml", MINE)]);
    let (job_id, job_token) = sh.wait_for_job(&w.server, "the one job");
    assert_eq!(
        finish(&w.server, &job_id, &job_token, "passed", None, None).0,
        200
    );
    assert_eq!(w.wait_settled("app", &sha)["state"], "passed");

    // Gone from the list, and its credential with it.
    assert!(w.runner_list().is_empty(), "{:?}", w.runner_list());
    assert!(matches!(sh.claim(&w.server), Claimed::Dead));
    // The run still names it, because the row is a tombstone rather than
    // a delete: "which machine ran this" has to stay answerable.
    let run = w.run_for("app", &sha);
    assert_eq!(job(&run, "test")["runner"]["name"], "once", "{run}");
}

/// A registration token works once, expires after an hour, and every
/// wrong shape answers identically.
#[test]
fn a_registration_token_is_single_use_and_short_lived() {
    let w = world("sh-regtoken");
    let minted = w.registration_token(None);
    let token = minted["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("weftg_"), "{minted}");
    assert_eq!(minted["group"], "default", "{minted}");
    // The command is spelled out, so nobody has to assemble the URL.
    let command = minted["command"].as_str().unwrap();
    assert!(
        command.starts_with("weft-runner register --url "),
        "{command}"
    );
    assert!(command.ends_with(&format!("--token {token}")), "{command}");
    // An hour, near enough that a clock skew of seconds cannot fail it.
    let ttl = minted["expires_at"].as_i64().unwrap() - stratum_control::ids::now_ms();
    assert!((3_000_000..=3_600_000).contains(&ttl), "ttl was {ttl}ms");

    assert!(Sh::register(&w.server, &token, "one", &[], false).is_ok());
    // Once. A second machine must not register on one operator's token.
    let (st, out) = Sh::register(&w.server, &token, "two", &[], false).unwrap_err();
    assert_eq!(st, 401, "{out}");

    // Every other way of being wrong answers the same, so the route
    // cannot be used to ask which organisations exist.
    for bad in [
        "",
        "nonsense",
        "weftg_",
        "weftg_onlyid",
        "weftr_01zzzzzzzzzzzzzzzzzzzzzzzz_x",
    ] {
        let (st, out) = Sh::register(&w.server, bad, "three", &[], false).unwrap_err();
        assert_eq!(st, 401, "{bad:?} answered {st}: {out}");
    }

    // Expiry itself is pinned where a token can be aged without a
    // second process reaching past this API:
    // `runners::tests::an_expired_registration_token_is_refused`. What
    // this route owes is the *hour* — asserted above — and that an
    // unspendable token is refused identically to every other kind of
    // wrong, which the loop just did.
}

/// Re-registering under a live name rotates the credential: the old one
/// dies the instant the new one exists, and the list shows one machine.
#[test]
fn re_registering_a_name_rotates_the_credential() {
    let w = world("sh-rotate");
    let first = w.register("build-box", &["gpu"], None);
    let second = w.register("build-box", &["gpu"], None);
    assert_ne!(first.credential, second.credential);

    assert!(matches!(first.claim(&w.server), Claimed::Dead));
    second.expect_nothing(&w.server, "the new credential should work");

    let listed = w.runner_list();
    assert_eq!(listed.len(), 1, "rotation left two machines: {listed:?}");
    assert_eq!(listed[0]["id"], second.id, "{listed:?}");

    // A machine whose registration is refused leaves nothing behind.
    let token = w.registration_token(None);
    let (st, out) = Sh::register(
        &w.server,
        token["token"].as_str().unwrap(),
        "has space",
        &[],
        false,
    )
    .unwrap_err();
    assert_eq!(st, 422, "{out}");
    assert_eq!(w.runner_list().len(), 1);
}

/// A lease that lapses hands the job to the next machine, and the third
/// try gives up rather than passing a doomed job around forever.
#[test]
fn a_lapsed_lease_moves_the_job_and_the_attempt_cap_ends_it() {
    let w = world_with(
        "sh-lease",
        &[
            // A one-second lease, so a machine that takes a job and says
            // nothing loses it inside the test.
            ("STRATUM_RUNNER_START_LEASE_SECS", "1".into()),
            ("STRATUM_RUNNER_MAX_ATTEMPTS", "2".into()),
        ],
    );
    let first = w.register("first", &[], None);
    let second = w.register("second", &[], None);
    let sha = w.commit("app", "main", &[(".weft/ci.yml", MINE)]);

    let (job_id, _) = first.wait_for_job(&w.server, "the first attempt");
    // The first machine goes quiet. Its lease lapses and the second one
    // is offered the same job.
    let (again, _) = second.wait_for_job(&w.server, "the job to be handed on");
    assert_eq!(again, job_id, "a different job was handed out");

    // …and the third time nobody gets it: a job whose runner has
    // vanished twice is not going to work on the next machine either.
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let run = w.run_for("app", &sha);
        if run["state"] == "failed" {
            assert!(
                job(&run, "test")["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("stopped reporting back"),
                "{run}"
            );
            break;
        }
        // Keep asking; the cap is applied by the claim, so somebody has
        // to ask for the job to be given up on.
        let _ = first.claim(&w.server);
        let _ = second.claim(&w.server);
        assert!(
            Instant::now() < deadline,
            "the attempt cap never fired: {run}"
        );
    }
}

/// Reading is a member's; writing is an admin's; and a stranger cannot
/// tell any of it from a namespace that does not exist.
#[test]
fn membership_decides_who_may_read_and_who_may_write() {
    let w = world("sh-authz");
    w.register("build-box", &[], None);
    let group = w.default_group();
    let gid = group["id"].as_str().unwrap().to_string();

    w.server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "member@acme.test",
            "--name",
            "Member",
            "--password",
            PASSWORD,
            "--role",
            "member",
        ])
        .expect("user-create");
    let mut member = Browser::signed_in(&w.server, "member@acme.test", PASSWORD);

    // A member sees the machines and the settings that govern them.
    for path in [
        "/v1/orgs/acme/runners",
        "/v1/orgs/acme/runner-groups",
        "/v1/orgs/acme/runner-policy",
    ] {
        let (st, out) = member.req("GET", path, None);
        assert_eq!(st, 200, "member reading {path}: {out}");
    }
    // …and may change none of it. 404 rather than 403, the masking rule:
    // a member with no admin role is told no more about the shape of
    // this organisation's settings than a stranger is.
    let writes: Vec<(&str, String, serde_json::Value)> = vec![
        (
            "PATCH",
            "/v1/orgs/acme/runner-policy".into(),
            serde_json::json!({ "self_hosted": "disabled" }),
        ),
        (
            "POST",
            "/v1/orgs/acme/runner-groups".into(),
            serde_json::json!({ "name": "sneaky" }),
        ),
        (
            "PATCH",
            format!("/v1/orgs/acme/runner-groups/{gid}"),
            serde_json::json!({ "name": "renamed" }),
        ),
        (
            "DELETE",
            format!("/v1/orgs/acme/runner-groups/{gid}"),
            serde_json::Value::Null,
        ),
        (
            "POST",
            "/v1/orgs/acme/runners/registration-token".into(),
            serde_json::Value::Null,
        ),
        (
            "DELETE",
            format!(
                "/v1/orgs/acme/runners/{}",
                w.runner_list()[0]["id"].as_str().unwrap()
            ),
            serde_json::Value::Null,
        ),
    ];
    for (method, path, body) in writes {
        let body = (!body.is_null()).then_some(body);
        let (st, out) = member.req(method, &path, body);
        assert_eq!(st, 404, "a member wrote {method} {path}: {out}");
    }
    // Nothing moved.
    assert_eq!(w.policy()["self_hosted"], "all");
    assert_eq!(w.groups().len(), 1);

    // A person with no role in this organisation cannot tell it from one
    // that does not exist — the masking rule, on a new surface.
    w.server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "nobody@acme.test",
            "--name",
            "Nobody",
            "--password",
            PASSWORD,
            "--role",
            "member",
        ])
        .expect("user-create");
    let mut stranger = Browser::signed_in(&w.server, "nobody@acme.test", PASSWORD);
    let (st, _) = stranger.req("GET", "/v1/orgs/globex/runners", None);
    assert_eq!(st, 404, "a namespace that does not exist must answer 404");

    // And no credential at all is a 401 rather than a 404, so a client
    // knows to present one.
    assert_eq!(w.server.status_get("/v1/orgs/acme/runners", None), 401);
}

/// Groups: created, renamed, given repositories, deleted — and the
/// default one is immovable, because its name is in every registration
/// command ever handed out.
#[test]
fn groups_can_be_managed_and_the_default_cannot_be_taken_away() {
    let w = world("sh-groups");
    let (st, out) = w.server.post(
        "/v1/orgs/acme/runner-groups",
        &w.admin,
        Some(serde_json::json!({
            "name": "builders", "repo_access": "selected", "repos": ["app"],
        })),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["repos"], serde_json::json!(["app"]), "{out}");
    assert_eq!(out["runners"], 0, "{out}");
    let gid = out["id"].as_str().unwrap().to_string();
    assert!(w.audited("runner_group.created"));

    // A duplicate name is a conflict, not a second group.
    let (st, out) = w.server.post(
        "/v1/orgs/acme/runner-groups",
        &w.admin,
        Some(serde_json::json!({ "name": "builders" })),
    );
    assert_eq!(st, 409, "{out}");
    // A repository that is not there is refused rather than silently
    // dropped: a group that admits four of the five somebody listed is a
    // build that does not run and a page that says it should.
    let (st, out) = w.server.post(
        "/v1/orgs/acme/runner-groups",
        &w.admin,
        Some(serde_json::json!({ "name": "typo", "repo_access": "selected", "repos": ["ap"] })),
    );
    assert_eq!(st, 422, "{out}");

    // A machine registered into it counts against it.
    let sh = w.register("build-box", &[], Some("builders"));
    assert_eq!(sh.group, "builders");
    let listed = w
        .groups()
        .into_iter()
        .find(|g| g["id"] == gid.as_str())
        .unwrap();
    assert_eq!(listed["runners"], 1, "{listed}");

    // The default group's name is fixed, and it cannot be deleted.
    let default = w.default_group();
    let did = default["id"].as_str().unwrap();
    let (st, out) = w.server.req(
        "PATCH",
        &format!("/v1/orgs/acme/runner-groups/{did}"),
        &w.admin,
        Some(serde_json::json!({ "name": "renamed" })),
    );
    assert_eq!(st, 422, "{out}");
    let (st, out) = w
        .server
        .delete(&format!("/v1/orgs/acme/runner-groups/{did}"), &w.admin);
    assert_eq!(st, 422, "{out}");

    // Deleting a real one moves its machines to the default rather than
    // stranding them where nothing can route to them.
    let (st, out) = w
        .server
        .delete(&format!("/v1/orgs/acme/runner-groups/{gid}"), &w.admin);
    assert_eq!(st, 204, "{out}");
    assert!(w.audited("runner_group.deleted"));
    assert_eq!(w.runner_list()[0]["group"]["name"], "default");
    // …and the machine still works where it landed.
    let sha = w.commit("app", "main", &[(".weft/ci.yml", MINE)]);
    let (job_id, token) = sh.wait_for_job(&w.server, "a job after the group moved");
    assert_eq!(
        finish(&w.server, &job_id, &token, "passed", None, None).0,
        200
    );
    assert_eq!(w.wait_settled("app", &sha)["state"], "passed");

    // An id nobody minted is absent rather than an error.
    let (st, _) = w
        .server
        .delete("/v1/orgs/acme/runner-groups/not-an-id", &w.admin);
    assert_eq!(st, 404);
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

/// A change from a fork is held for approval **even though** the job
/// would run on the maintainer's own machine — and especially then.
///
/// This is the case the fork gate matters most for: a fork's change
/// carries its own `run:` lines, and on a self-hosted machine they run
/// on hardware in somebody's office, with whatever is on that network
/// reachable from it. bob can read ada's organisation's repository — he
/// was invited in as a viewer, which is the only way anybody reads it —
/// and that is exactly not the same thing as being trusted to run code
/// on her box.
#[test]
fn a_change_from_a_fork_is_held_even_for_a_machine_the_maintainer_owns() {
    let minio = Minio::shared();
    let bucket = minio.bucket("sh-fork");
    let scratch = Scratch::new("sh-fork");
    let mail = Mailbox::temp("sh-fork");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("sh-fork")
        .data_dir(scratch.path().join("data"))
        .envs(&mail.env())
        .env("STRATUM_RUNNER_POLL_SECS", "1")
        .env("STRATUM_RUNNER_CLAIM_WAIT_MS", "700")
        .start();

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    // A personal namespace has no members, so a repository somebody
    // else can read lives in an organisation.
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs",
        Some(serde_json::json!({ "name": "acme" })),
    );
    assert_eq!(st, 201, "{body}");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/acme/repos",
        Some(serde_json::json!({ "name": "widget" })),
    );
    assert_eq!(st, 201, "{body}");
    ada.invite_and_accept("acme", "bob@example.com", "viewer");

    // ada's own machine, in the default group, which admits every
    // repository — so the only thing that can hold bob's change is the
    // fork gate.
    let (st, minted) = ada.req("POST", "/v1/orgs/acme/runners/registration-token", None);
    assert_eq!(st, 201, "{minted}");
    let sh = Sh::register(
        &server,
        minted["token"].as_str().unwrap(),
        "ada-box",
        &[],
        false,
    )
    .expect("registration");

    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/widget/commits",
        Some(serde_json::json!({
            "message": "add ci",
            "operations": [
                { "op": "put", "path": "README.md", "content": "# seed\n" },
                { "op": "put", "path": ".weft/ci.yml", "content": MINE },
            ],
        })),
    );
    assert_eq!(st, 201, "{body}");
    // ada's own push runs on her machine — so the stack is proven good
    // before the fork case, and a held fork change cannot be a
    // misconfiguration passing as a gate.
    let seed = body["commit"].as_str().unwrap().to_string();
    let (job_id, token) = sh.wait_for_job(&server, "ada's own push");
    assert_eq!(
        finish(&server, &job_id, &token, "passed", None, None).0,
        200
    );
    let _ = seed;

    let (st, body) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, body) = bob.req("GET", "/v1/orgs/bob/repos/widget", None);
        match body["fork_state"].as_str() {
            Some("ready") => break,
            _ => assert!(Instant::now() < deadline, "fork never became ready: {body}"),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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
            "message": "a contribution\n\nChange-Id: I9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f",
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

    // Held, with the fork's own code and sentence — and, crucially, not
    // offered to ada's machine.
    let deadline = Instant::now() + Duration::from_secs(30);
    let held = loop {
        let (_, out) = ada.req(
            "GET",
            &format!("/v1/orgs/acme/repos/widget/workflow-runs?commit_sha={sha}"),
            None,
        );
        let runs = out["runs"].as_array().cloned().unwrap_or_default();
        if let Some(r) = runs.into_iter().next() {
            break r;
        }
        assert!(
            Instant::now() < deadline,
            "no run was recorded for the fork change"
        );
        std::thread::sleep(Duration::from_millis(150));
    };
    assert_eq!(held["state"], "blocked", "{held}");
    assert_eq!(held["blocked_reason"], "fork", "{held}");
    assert_eq!(
        held["error"],
        "this change comes from a fork; a maintainer has to approve its workflows before they run",
        "{held}"
    );
    sh.expect_nothing(
        &server,
        "a fork's change reached the maintainer's own machine",
    );
}

/// A self-hosted job runs its steps directly on the machine, as the
/// person who registered it, in whatever that machine has installed —
/// there is no container, so there is nothing an `image:` could select.
/// Refused at trigger time, naming the image back, rather than run
/// against a toolchain nobody asked for.
#[test]
fn a_self_hosted_job_that_names_an_image_is_refused_by_name() {
    let w = world("sh-image");
    let sh = w.register("build-box", &[], None);

    let sha = w.commit("app", "main", &[(".weft/ci.yml", IMAGED)]);
    let refused = w.wait_settled("app", &sha);
    assert_eq!(refused["state"], "failed", "{refused}");
    assert_eq!(
        refused["error"],
        "image \"rust:1.83\" is not available on self-hosted runners; \
         steps run directly on the machine"
    );
    // And nothing was queued for the machine: the refusal is the whole
    // outcome, not a job left waiting behind a failed run.
    sh.expect_nothing(&w.server, "an imaged self-hosted file queued a job anyway");

    // The same file without the image runs. `default` — what every job
    // that says nothing gets — is not an image anybody is naming.
    let plain = IMAGED.replace("    image: rust:1.83\n", "    image: default\n");
    let sha = w.commit("app", "main", &[(".weft/ci.yml", &plain)]);
    let (job_id, token) = sh.wait_for_job(&w.server, "the same file with image: default");
    assert_eq!(
        finish(&w.server, &job_id, &token, "passed", None, None).0,
        200
    );
    assert_eq!(w.wait_settled("app", &sha)["state"], "passed");
}

// ---------------------------------------------------------------------
// The refusals
// ---------------------------------------------------------------------

/// Every runners route, walked by somebody who should not be there.
///
/// A namespace that does not exist and a namespace that is not yours
/// answer identically — 404 on all nine routes, reads and writes alike —
/// so the fleet a competitor runs cannot be enumerated by reading status
/// codes off this surface. `membership_decides_who_may_read_and_who_may_write`
/// covers the member-who-is-not-an-admin half; this one is the two
/// callers who have no business here at all, and it ends by proving the
/// server is still answering the organisation that does own the fleet.
#[test]
fn no_runners_route_admits_an_unknown_namespace_or_a_foreign_token() {
    let w = world("sh-404s");
    let sh = w.register("build-box", &[], None);
    let gid = w.default_group()["id"].as_str().unwrap().to_string();

    // Nine routes, and a namespace nobody ever created. `org_or_404`
    // runs before the permission check on every one of them, which is
    // what makes "no such organisation" and "not yours" the same answer.
    let routes: Vec<(&str, String, serde_json::Value)> = vec![
        (
            "GET",
            "/v1/orgs/globex/runner-policy".into(),
            serde_json::Value::Null,
        ),
        (
            "PATCH",
            "/v1/orgs/globex/runner-policy".into(),
            serde_json::json!({ "hosted": "disabled" }),
        ),
        (
            "GET",
            "/v1/orgs/globex/runner-groups".into(),
            serde_json::Value::Null,
        ),
        (
            "POST",
            "/v1/orgs/globex/runner-groups".into(),
            serde_json::json!({ "name": "theirs" }),
        ),
        (
            "PATCH",
            format!("/v1/orgs/globex/runner-groups/{gid}"),
            serde_json::json!({ "allow_public": true }),
        ),
        (
            "DELETE",
            format!("/v1/orgs/globex/runner-groups/{gid}"),
            serde_json::Value::Null,
        ),
        (
            "GET",
            "/v1/orgs/globex/runners".into(),
            serde_json::Value::Null,
        ),
        (
            "DELETE",
            format!("/v1/orgs/globex/runners/{}", sh.id),
            serde_json::Value::Null,
        ),
        (
            "POST",
            "/v1/orgs/globex/runners/registration-token".into(),
            serde_json::Value::Null,
        ),
    ];
    for (method, path, body) in &routes {
        let body = (!body.is_null()).then(|| body.clone());
        let (st, out) = w.server.req(method, path, &w.admin, body);
        assert_eq!(st, 404, "{method} {path} answered {st}: {out}");
    }

    // The same nine against the organisation that *does* exist, holding
    // another organisation's admin token. A foreign credential gets the
    // absence answer rather than a refusal, so the two are one answer.
    let rival = w.server.bootstrap_org("rival");
    for (method, path, body) in &routes {
        let path = path.replace("/orgs/globex/", "/orgs/acme/");
        let body = (!body.is_null()).then(|| body.clone());
        let (st, out) = w.server.req(method, &path, &rival, body);
        assert_eq!(st, 404, "rival did {method} {path}: {st}: {out}");
    }

    // Nothing moved, and the organisation that owns the fleet is still
    // being served — a surface that survives an attack by refusing
    // everybody has failed differently, not passed.
    assert!(w.server.healthy());
    assert_eq!(w.policy()["self_hosted"], "all");
    assert_eq!(w.groups().len(), 1);
    assert_eq!(w.runner_list().len(), 1);
    let sha = w.commit("app", "main", &[(".weft/ci.yml", MINE)]);
    let (job_id, token) = sh.wait_for_job(&w.server, "a job after the refusals");
    assert_eq!(
        finish(&w.server, &job_id, &token, "passed", None, None).0,
        200
    );
    assert_eq!(w.wait_settled("app", &sha)["state"], "passed");
}

/// The shapes these routes cannot act on, each answered by what it is
/// rather than by a 500: a missing name, an id nobody minted, a group
/// name that is not there, and a machine asking for work with nothing to
/// say who it is.
#[test]
fn a_runner_route_names_the_shape_it_cannot_act_on() {
    let w = world("sh-shapes");

    // A group needs a name. `{}` is a form somebody submitted empty, and
    // a group with no name is not something the routing could ever pick.
    let (st, out) = w.server.post(
        "/v1/orgs/acme/runner-groups",
        &w.admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 422, "{out}");
    assert_eq!(w.groups().len(), 1, "an unnamed group was created");

    // An id nobody minted is absent, whether or not it is even shaped
    // like one of ours — on the group routes and the runner route alike.
    const NO_SUCH_ID: &str = "01HZZZZZZZZZZZZZZZZZZZZZZZ";
    for id in [NO_SUCH_ID, "not-an-id"] {
        let (st, out) = w.server.req(
            "PATCH",
            &format!("/v1/orgs/acme/runner-groups/{id}"),
            &w.admin,
            Some(serde_json::json!({ "allow_public": true })),
        );
        assert_eq!(st, 404, "PATCH group {id}: {out}");
        let (st, out) = w
            .server
            .delete(&format!("/v1/orgs/acme/runners/{id}"), &w.admin);
        assert_eq!(st, 404, "DELETE runner {id}: {out}");
    }

    // A registration token for a group that is not there is refused at
    // the mint rather than handed out: a token naming a group nobody has
    // would register a machine into nothing, and the operator would find
    // out on a machine rather than in the dashboard.
    let (st, out) = w.server.post(
        "/v1/orgs/acme/runners/registration-token",
        &w.admin,
        Some(serde_json::json!({ "group": "nope" })),
    );
    assert_eq!(st, 422, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no runner group named"),
        "{out}"
    );

    // A machine that does not say what version it is says "unknown"
    // rather than an empty cell: the runners page reads that column, and
    // a blank is indistinguishable from a rendering bug.
    let minted = w.registration_token(None);
    let (st, out) = w.server.post(
        "/v1/runners/register",
        minted["token"].as_str().unwrap(),
        Some(serde_json::json!({
            "name": "unversioned", "labels": [], "os": "linux", "arch": "x64",
        })),
    );
    assert_eq!(st, 201, "{out}");
    let listed = w.runner_list();
    let row = listed
        .iter()
        .find(|r| r["name"] == "unversioned")
        .unwrap_or_else(|| panic!("{listed:?}"));
    assert_eq!(row["version"], "unknown", "{row}");

    // And a claim with no credential at all is a 401 rather than a 204:
    // a runner whose `.runner` file is missing has to be told to
    // register, not left polling an empty queue forever.
    let (st, out) = w.server.post("/v1/runners/claim", "", None);
    assert_eq!(st, 401, "{out}");
    assert!(w.server.healthy());
}

/// A machine nobody has heard from for a fortnight is tombstoned by the
/// sweep that rides on the dispatcher's tick — and its credential dies
/// with it, which is the whole point: there is nothing listening on the
/// operator's machine to tell, so it finds out by being refused.
#[test]
fn a_machine_that_stops_calling_is_swept_and_its_credential_dies() {
    let w = world("sh-sweep");
    let gone = w.register("abandoned", &[], None);
    let live = w.register("still-here", &[], None);

    // Age it past the deadline where a second session can reach — the
    // alternative is a test that waits fourteen days.
    let mut db = postgres::Client::connect(&w.server.db_url, postgres::NoTls)
        .expect("a second session on the server's database");
    let stale = stratum_control::ids::now_ms() - stratum_control::runners::STALE_MS - 60_000;
    let aged = db
        .execute(
            "UPDATE runners SET last_seen_at = $2 WHERE id = $1",
            &[&gone.id, &stale],
        )
        .expect("age the runner");
    assert_eq!(aged, 1, "the runner to age was not there");

    // The dispatcher ticks every second here and sweeps on each tick.
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let names: Vec<String> = w
            .runner_list()
            .iter()
            .map(|r| r["name"].as_str().unwrap_or_default().to_string())
            .collect();
        if names == vec!["still-here".to_string()] {
            break;
        }
        assert!(Instant::now() < deadline, "the sweep never ran: {names:?}");
        std::thread::sleep(Duration::from_millis(200));
    }

    // Tombstoned, not deleted: the credential is refused rather than
    // silently answering "nothing for you", so the machine exits instead
    // of polling a queue it can never be given work from.
    assert!(matches!(gone.claim(&w.server), Claimed::Dead));
    // The machine that kept calling is untouched, and still gets work.
    let sha = w.commit("app", "main", &[(".weft/ci.yml", MINE)]);
    let (job_id, token) = live.wait_for_job(&w.server, "a job after the sweep");
    assert_eq!(
        finish(&w.server, &job_id, &token, "passed", None, None).0,
        200
    );
    assert_eq!(w.wait_settled("app", &sha)["state"], "passed");
}
