//! The five calls a runner makes, driven as a runner makes them.
//!
//! The setup deliberately stops short of the claim route: the run, the
//! claim and the job token are created straight through the control
//! plane, exactly as `POST /v1/runners/claim` does, and everything after
//! that is plain HTTP with a bearer token — no helper of ours on the client
//! side. That is the whole point of the suite. The runner is a separate
//! process that will talk to this API over a network, so a test that
//! reached in through a Rust function would prove that our own code
//! agrees with itself and nothing about the wire.
//!
//! Half of this file is the negative suite, because this is a new
//! attack surface reached from a container that is executing untrusted
//! `run:` lines. The refusal *order* is part of the contract — 401, then
//! 404, then 403, then 410 — so a job's state is only ever disclosed to
//! the credential that owns it, and each case ends by proving the server
//! is still healthy and still serving.

use stratum_control::auth::{self, Mint, Scope};
use stratum_control::workflows::{self, NewJob, NewRun, WorkflowJob};
use stratum_control::{registry, ControlDb};
use stratum_store::{LatencyModel, ObjectStore};
use stratum_testkit::{gitcli::Scratch, Minio, Server};

const SHA: &str = "1111111111111111111111111111111111111111";

const SPEC: &str = r#"{"image":"default","timeout_minutes":30,"env":{"K":"V"},
  "steps":[{"name":"Test","run":"cargo test","env":{}}]}"#;

struct World {
    server: Server,
    db: ControlDb,
    store_url: String,
    org_id: String,
    repo_id: String,
    admin: String,
}

/// A repository with one two-job run, the first job claimed and launched
/// with a real token — the state a runner starts its life in.
fn world(hint: &str, minio: &Minio, scratch: &Scratch) -> (World, WorkflowJob, String) {
    let bucket = minio.bucket(hint);
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        // No sweeper: this suite drives the queue by hand, and a
        // background sweep would race every assertion in it.
        .env("STRATUM_RUNNER_POLL_SECS", "0")
        .start();
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    assert_eq!(st, 201, "{out}");

    let db = ControlDb::open(&server.db_url).expect("a second session on the server's database");
    let org = registry::org_by_name(&db, "acme").unwrap().unwrap();
    let repo = registry::repo_by_name(&db, &org.id, "app")
        .unwrap()
        .unwrap();

    workflows::create_run(
        &db,
        &org.id,
        &repo.id,
        &NewRun {
            file: ".weft/ci.yml",
            name: "ci",
            commit_sha: SHA,
            ref_name: Some("main"),
            event: "push",
            change_key: None,
            changeset_id: None,
            composition: None,
            from_fork: false,
        },
        &[
            NewJob {
                job_id: "build",
                key: "build (linux)",
                matrix: r#"{"os":"linux"}"#,
                needs: &[],
                spec: SPEC,
                ..Default::default()
            },
            NewJob {
                job_id: "ship",
                key: "ship",
                matrix: "{}",
                needs: &[0],
                spec: SPEC,
                ..Default::default()
            },
        ],
    )
    .unwrap();

    let job = claim(&db, &org.id).expect("build is ready");
    let token = launch(&db, &org.id, &repo.id, &job, None);
    let store_url = bucket.base_url.clone();
    (
        World {
            server,
            db,
            store_url,
            org_id: org.id,
            repo_id: repo.id,
            admin,
        },
        job,
        token,
    )
}

/// Claim the next ready job as a runner in the default group would —
/// the query `POST /v1/runners/claim` makes, minus the long poll.
fn claim(db: &ControlDb, org_id: &str) -> Option<WorkflowJob> {
    let labels = vec!["self-hosted".to_string()];
    workflows::claim_self_hosted(
        db,
        &workflows::RunnerRoute {
            runner_id: "e2e-runner",
            org_id,
            group_id: "",
            labels: &labels,
            all_repos: true,
        },
        60_000,
    )
    .unwrap()
}

/// Mint a job token and attach it, the way the claim route does.
fn launch(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    job: &WorkflowJob,
    expires_at: Option<i64>,
) -> String {
    let minted = auth::mint_for(
        db,
        org_id,
        &[Scope::RepoRead],
        Mint {
            repo_id: Some(repo_id),
            label: Some("job"),
            user_id: None,
            expires_at,
        },
        None,
    )
    .unwrap();
    assert!(workflows::mark_launched(db, &job.id, "pid:1", &minted.id, "chk-placeholder").unwrap());
    minted.plaintext
}

// --- the wire, as a runner speaks it ---------------------------------

fn call(
    server: &Server,
    method: &str,
    path: &str,
    token: &str,
    content_type: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    let mut r = ureq::request(method, &format!("{}{path}", server.base));
    if !token.is_empty() {
        r = r.set("Authorization", &format!("Bearer {token}"));
    }
    if let Some(ct) = content_type {
        r = r.set("Content-Type", ct);
    }
    let resp = match body {
        Some(b) => r.send_string(b),
        None => r.call(),
    };
    let resp = match resp {
        Ok(x) => x,
        Err(ureq::Error::Status(_, x)) => x,
        Err(e) => panic!("transport {method} {path}: {e}"),
    };
    (resp.status(), resp.into_string().unwrap_or_default())
}

fn json(
    server: &Server,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&str>,
) -> (u16, serde_json::Value) {
    let (st, text) = call(
        server,
        method,
        path,
        token,
        body.map(|_| "application/json"),
        body,
    );
    (
        st,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

fn post_chunk(
    server: &Server,
    job: &str,
    token: &str,
    seq: i32,
    text: &str,
) -> (u16, serde_json::Value) {
    json(
        server,
        "POST",
        &format!("/v1/runner/jobs/{job}/log"),
        token,
        Some(&serde_json::json!({"seq": seq, "text": text}).to_string()),
    )
}

// ---------------------------------------------------------------------
// The happy path, all the way to a green check
// ---------------------------------------------------------------------

/// One job's whole life over HTTP: read the spec, stream output, keep
/// the lease, upload the log, report a verdict — and then the parts a
/// runner never sees, which are the ones a reader depends on: the run
/// settles, the check row goes green on the commit, and the chunks are
/// gone.
#[test]
fn a_runner_reads_its_spec_streams_its_log_and_reports_a_verdict() {
    let minio = Minio::shared();
    let scratch = Scratch::new("runner-api-e2e");
    let (w, job, token) = world("runner-api-e2e", minio, &scratch);

    // --- GET the spec ------------------------------------------------
    let (st, spec) = json(
        &w.server,
        "GET",
        &format!("/v1/runner/jobs/{}", job.id),
        &token,
        None,
    );
    assert_eq!(st, 200, "{spec}");
    assert_eq!(spec["id"], serde_json::json!(job.id));
    assert_eq!(spec["run_id"], serde_json::json!(job.run_id));
    assert_eq!(spec["attempt"], serde_json::json!(1));
    assert_eq!(spec["key"], serde_json::json!("build (linux)"));
    assert_eq!(spec["job"], serde_json::json!("build"));
    assert_eq!(spec["commit_sha"], serde_json::json!(SHA));
    assert_eq!(spec["ref_name"], serde_json::json!("main"));
    assert_eq!(spec["event"], serde_json::json!("push"));
    assert_eq!(spec["change_key"], serde_json::Value::Null);
    assert_eq!(
        spec["fetch_ref"],
        serde_json::json!("refs/heads/main"),
        "a push is fetched by branch"
    );
    // The JobSpec is merged in, not nested.
    assert_eq!(spec["image"], serde_json::json!("default"));
    assert_eq!(spec["timeout_minutes"], serde_json::json!(30));
    assert_eq!(spec["env"], serde_json::json!({"K": "V"}));
    assert_eq!(spec["steps"][0]["run"], serde_json::json!("cargo test"));
    // The matrix arrives parsed, so no caller has to JSON.parse a field
    // out of a JSON document.
    assert_eq!(spec["matrix"], serde_json::json!({"os": "linux"}));

    // The clone URL is reachable and carries **no** credential — a token
    // in a remote URL ends up in a config file, a reflog and a `ps`
    // listing, which is why the runner puts it in a header instead.
    let clone_url = spec["clone_url"].as_str().expect("a clone url");
    assert_eq!(clone_url, format!("{}/acme/app.git", w.server.base));
    assert!(
        !clone_url.contains('@') && !clone_url.contains("weft_"),
        "{clone_url}"
    );

    // --- POST log chunks ---------------------------------------------
    let (st, out) = post_chunk(&w.server, &job.id, &token, 1, "▶ Checkout\n");
    assert_eq!(st, 200, "{out}");
    let first_lease = out["lease_until"].as_i64().expect("a lease");
    let (st, out) = post_chunk(&w.server, &job.id, &token, 2, "▶ Test\nok\n");
    assert_eq!(st, 200, "{out}");
    assert!(out["lease_until"].as_i64().unwrap() >= first_lease);

    // The chunk really is an object in the bucket, at the contract's
    // key. If this only lived in Postgres the log would not survive the
    // node that received it.
    let store = ObjectStore::new(&w.store_url, LatencyModel::None);
    let key = format!("ci/logs/{}/{}/1/000001.txt", job.run_id, job.id);
    assert_eq!(
        String::from_utf8(store.get(&key).expect("the chunk object")).unwrap(),
        "▶ Checkout\n"
    );
    assert_eq!(
        workflows::job(&w.db, &job.id).unwrap().unwrap().log_chunks,
        2
    );

    // A retry of an earlier sequence is idempotent: 200, the object is
    // replaced, and the counter does not walk backwards and hide chunk
    // 2 from a reader who has already seen it.
    let (st, out) = post_chunk(&w.server, &job.id, &token, 1, "▶ Checkout (retry)\n");
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        String::from_utf8(store.get(&key).unwrap()).unwrap(),
        "▶ Checkout (retry)\n"
    );
    assert_eq!(
        workflows::job(&w.db, &job.id).unwrap().unwrap().log_chunks,
        2
    );

    // --- POST lease ---------------------------------------------------
    let (st, out) = json(
        &w.server,
        "POST",
        &format!("/v1/runner/jobs/{}/lease", job.id),
        &token,
        None,
    );
    assert_eq!(st, 200, "{out}");
    assert!(out["lease_until"].as_i64().is_some(), "{out}");

    // --- PUT the whole log --------------------------------------------
    let whole = "▶ Checkout\n▶ Test\nok\n✓ Test (1s)\n";
    let (st, body) = call(
        &w.server,
        "PUT",
        &format!("/v1/runner/jobs/{}/log", job.id),
        &token,
        Some("text/plain"),
        Some(whole),
    );
    assert_eq!(st, 200, "{body}");

    // --- POST the verdict ---------------------------------------------
    let (st, out) = json(
        &w.server,
        "POST",
        &format!("/v1/runner/jobs/{}/finish", job.id),
        &token,
        Some(r#"{"state":"passed","error":null}"#),
    );
    assert_eq!(st, 200, "{out}");

    // The job passed, `ship` became ready, and the run is still going —
    // one job passing does not settle a two-job run.
    let done = workflows::job(&w.db, &job.id).unwrap().unwrap();
    assert_eq!(done.state, "passed");
    assert_eq!(
        workflows::run_by_id(&w.db, &job.run_id)
            .unwrap()
            .unwrap()
            .state,
        "running"
    );

    // The mirrored check row is on the commit, green, under the name a
    // reader recognises — with no request to a checks route to make it
    // so. This is the property the whole mirror exists for.
    let (st, out) = w
        .server
        .get("/v1/orgs/acme/repos/app/checks/runs", &w.admin);
    assert_eq!(st, 200, "{out}");
    // `checks/runs` has no `?sha=` filter — it is a repository history —
    // so the commit is asserted on each row instead.
    let rows: Vec<serde_json::Value> = out["runs"]
        .as_array()
        .expect("check runs")
        .iter()
        .filter(|r| r["commit_sha"] == serde_json::json!(SHA))
        .cloned()
        .collect();
    let build = rows
        .iter()
        .find(|r| r["name"] == "ci / build (linux)")
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(build["state"], serde_json::json!("passing"), "{build}");
    assert_eq!(build["provider"], serde_json::json!("weft"));
    assert_eq!(build["commit_sha"], serde_json::json!(SHA));
    // The still-queued `ship` job is mirrored too, so the tab shows the
    // whole run rather than only the parts that have reported.
    assert!(
        rows.iter().any(|r| r["name"] == "ci / ship"),
        "every job has a row: {out}"
    );

    // The chunks are gone — the whole log replaced them.
    assert!(
        store
            .list(&format!("ci/logs/{}/{}/1/", job.run_id, job.id))
            .unwrap()
            .is_empty(),
        "the chunks are cleaned up once the log is stored"
    );
    assert_eq!(
        String::from_utf8(
            store
                .get(&format!("ci/logs/{}/{}/log.txt", job.run_id, job.id))
                .unwrap()
        )
        .unwrap(),
        whole
    );
    assert!(w.server.healthy());
}

/// A change's commits are not on a branch, so its runner is told to
/// fetch the patchset ref instead. Getting this wrong is a clone that
/// fails for every fork contribution and works for every push, which is
/// the shape of bug that reaches production.
#[test]
fn a_change_run_is_fetched_by_its_patchset_ref() {
    let minio = Minio::shared();
    let scratch = Scratch::new("runner-api-change");
    let (w, _, _) = world("runner-api-change", minio, &scratch);

    let sha = "2".repeat(40);
    workflows::create_run(
        &w.db,
        &w.org_id,
        &w.repo_id,
        &NewRun {
            file: ".weft/ci.yml",
            name: "ci",
            commit_sha: &sha,
            ref_name: Some("main"),
            event: "change",
            change_key: Some("Ic0ffee"),
            changeset_id: None,
            composition: None,
            from_fork: false,
        },
        &[NewJob {
            job_id: "test",
            key: "test",
            matrix: "{}",
            needs: &[],
            spec: SPEC,
            ..Default::default()
        }],
    )
    .unwrap();
    // The first run's `build` is still running and holds a slot, so
    // claim until this run's job comes out.
    let job = loop {
        let c = claim(&w.db, &w.org_id).expect("a claimable job");
        if c.key == "test" {
            break c;
        }
    };
    let token = launch(&w.db, &w.org_id, &w.repo_id, &job, None);

    let (st, spec) = json(
        &w.server,
        "GET",
        &format!("/v1/runner/jobs/{}", job.id),
        &token,
        None,
    );
    assert_eq!(st, 200, "{spec}");
    assert_eq!(spec["event"], serde_json::json!("change"));
    assert_eq!(spec["change_key"], serde_json::json!("Ic0ffee"));
    assert_eq!(
        spec["fetch_ref"],
        serde_json::json!(format!("refs/patchsets/{sha}"))
    );
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// The negative suite
// ---------------------------------------------------------------------

/// Every way a call can be refused, in the order the contract fixes.
///
/// The order matters as much as the codes: 410 last means a job's state
/// is never disclosed to a credential that does not own the job, and
/// 404-before-403 means a token cannot enumerate job ids by watching
/// which ones answer differently.
#[test]
fn a_runner_call_is_refused_by_credential_before_it_is_told_anything() {
    let minio = Minio::shared();
    let scratch = Scratch::new("runner-api-neg");
    let (w, job, token) = world("runner-api-neg", minio, &scratch);
    let path = format!("/v1/runner/jobs/{}", job.id);

    // No credential at all, and a credential that is not one of ours.
    for bad in ["", "not-a-token", "weft_nope_nope"] {
        let (st, out) = json(&w.server, "GET", &path, bad, None);
        assert_eq!(st, 401, "{bad:?} -> {out}");
    }
    assert!(w.server.healthy());

    // An expired token is 401, not 403: it is not a wrong credential,
    // it is no longer a credential.
    let dead = auth::mint_for(
        &w.db,
        &w.org_id,
        &[Scope::RepoRead],
        Mint {
            repo_id: Some(&w.repo_id),
            label: Some("expired"),
            user_id: None,
            expires_at: Some(stratum_control::ids::now_ms() - 1_000),
        },
        None,
    )
    .unwrap();
    let (st, out) = json(&w.server, "GET", &path, &dead.plaintext, None);
    assert_eq!(st, 401, "{out}");

    // A job nobody minted: 404, and the same 404 for a shape that is not
    // an id at all — an invalid id is definitionally absent, and must
    // never reach a query.
    for id in ["01NOPENOPENOPENOPENOPENOPE", "../../etc/passwd", "%00"] {
        let (st, out) = json(
            &w.server,
            "GET",
            &format!("/v1/runner/jobs/{id}"),
            &token,
            None,
        );
        assert_eq!(st, 404, "{id:?} -> {out}");
    }

    // A perfectly good token for the right repository, minted for a
    // *different* job. This is the one that matters: without it, one
    // build could report another's verdict, which is how a green check
    // gets forged for code that never compiled.
    let sibling = workflows::jobs_of(&w.db, &job.run_id)
        .unwrap()
        .into_iter()
        .find(|j| j.key == "ship")
        .unwrap();
    let others = auth::mint_for(
        &w.db,
        &w.org_id,
        &[Scope::RepoRead],
        Mint {
            repo_id: Some(&w.repo_id),
            label: Some("another job"),
            user_id: None,
            expires_at: None,
        },
        None,
    )
    .unwrap();
    let (st, out) = json(&w.server, "GET", &path, &others.plaintext, None);
    assert_eq!(st, 403, "{out}");
    // ...and the sibling's own id with this job's token is 403 as well,
    // rather than leaking that the sibling is queued.
    let (st, out) = json(
        &w.server,
        "GET",
        &format!("/v1/runner/jobs/{}", sibling.id),
        &token,
        None,
    );
    assert_eq!(st, 403, "{out}");

    // A token belonging to another organisation entirely.
    let (st, out) = json(&w.server, "GET", &path, &w.admin, None);
    assert_eq!(st, 403, "an org admin token is not this job's token: {out}");

    // Nothing above wrote anything.
    assert_eq!(
        workflows::job(&w.db, &job.id).unwrap().unwrap().state,
        "running"
    );
    assert!(w.server.healthy());
}

/// Bodies are bounded before they are believed, and a refusal is a
/// refusal rather than a silent truncation — a truncated log is worse
/// than a rejected one, because it looks complete.
#[test]
fn an_oversized_chunk_or_log_is_refused_rather_than_truncated() {
    let minio = Minio::shared();
    let scratch = Scratch::new("runner-api-big");
    let (w, job, token) = world("runner-api-big", minio, &scratch);

    let big = "x".repeat(256 * 1024 + 1);
    let (st, out) = post_chunk(&w.server, &job.id, &token, 1, &big);
    assert_eq!(st, 413, "{out}");
    // Exactly at the cap is fine — the boundary is a cap, not a hint.
    let (st, out) = post_chunk(&w.server, &job.id, &token, 1, &"y".repeat(256 * 1024));
    assert_eq!(st, 200, "{out}");

    // seq below 1 is a 400 rather than a 500 from a key formatter.
    let (st, out) = post_chunk(&w.server, &job.id, &token, 0, "hi");
    assert_eq!(st, 400, "{out}");
    let (st, out) = json(
        &w.server,
        "POST",
        &format!("/v1/runner/jobs/{}/log", job.id),
        &token,
        Some("not json at all"),
    );
    assert_eq!(st, 400, "{out}");

    // The whole log, one byte over 16 MiB.
    let (st, body) = call(
        &w.server,
        "PUT",
        &format!("/v1/runner/jobs/{}/log", job.id),
        &token,
        Some("text/plain"),
        Some(&"z".repeat(16 * 1024 * 1024 + 1)),
    );
    assert_eq!(st, 413, "{body}");
    // ...and a large but legal one is accepted. Without this the case
    // above would pass just as well against the router's own 64 MiB
    // body limit, or against a handler that refused every big upload —
    // it is the pair that proves *this* cap is the one that fired.
    let (st, body) = call(
        &w.server,
        "PUT",
        &format!("/v1/runner/jobs/{}/log", job.id),
        &token,
        Some("text/plain"),
        Some(&"z".repeat(4 * 1024 * 1024)),
    );
    assert_eq!(st, 200, "a four-megabyte log is ordinary: {body}");

    // A verdict that is not one.
    for bad in [
        r#"{"state":"cancelled"}"#,
        r#"{"state":"queued"}"#,
        r#"{"state":"banana"}"#,
        r#"{}"#,
    ] {
        let (st, out) = json(
            &w.server,
            "POST",
            &format!("/v1/runner/jobs/{}/finish", job.id),
            &token,
            Some(bad),
        );
        assert_eq!(st, 400, "{bad} -> {out}");
    }
    assert_eq!(
        workflows::job(&w.db, &job.id).unwrap().unwrap().state,
        "running",
        "nothing above reported a verdict"
    );
    assert!(w.server.healthy());
}

/// Cancellation is the only way to stop a container, and it works by
/// telling the runner on its next call. So *every* call has to answer
/// 410 with the state in it — a single one that does not is a runner
/// that keeps building after somebody hit stop.
#[test]
fn every_runner_call_answers_410_once_the_job_is_cancelled() {
    let minio = Minio::shared();
    let scratch = Scratch::new("runner-api-410");
    let (w, job, token) = world("runner-api-410", minio, &scratch);

    // It works first...
    let (st, _) = json(
        &w.server,
        "GET",
        &format!("/v1/runner/jobs/{}", job.id),
        &token,
        None,
    );
    assert_eq!(st, 200);

    workflows::cancel_run(&w.db, &job.run_id, "cancelled by Ada").unwrap();

    let calls: Vec<(&str, String, Option<String>)> = vec![
        ("GET", format!("/v1/runner/jobs/{}", job.id), None),
        (
            "POST",
            format!("/v1/runner/jobs/{}/log", job.id),
            Some(r#"{"seq":1,"text":"more output"}"#.to_string()),
        ),
        ("POST", format!("/v1/runner/jobs/{}/lease", job.id), None),
        (
            "PUT",
            format!("/v1/runner/jobs/{}/log", job.id),
            Some("the whole log".to_string()),
        ),
        (
            "POST",
            format!("/v1/runner/jobs/{}/finish", job.id),
            Some(r#"{"state":"passed"}"#.to_string()),
        ),
    ];
    for (method, path, body) in &calls {
        let (st, out) = json(&w.server, method, path, &token, body.as_deref());
        assert_eq!(st, 410, "{method} {path} -> {out}");
        assert_eq!(out["error"], serde_json::json!("job is no longer running"));
        assert_eq!(
            out["state"],
            serde_json::json!("cancelled"),
            "the runner's log should say why it exited: {out}"
        );
    }

    // And the verdict the runner tried to report did not land.
    let after = workflows::job(&w.db, &job.id).unwrap().unwrap();
    assert_eq!(after.state, "cancelled");
    assert_eq!(after.error.as_deref(), Some("cancelled by Ada"));

    // A job that finished normally is equally gone, with its own state.
    let sibling = workflows::jobs_of(&w.db, &job.run_id)
        .unwrap()
        .into_iter()
        .find(|j| j.key == "ship")
        .unwrap();
    assert_eq!(sibling.state, "cancelled");

    assert!(w.server.healthy());
    // Still serving the ordinary API, not just /healthz.
    let (st, out) = w.server.get("/v1/orgs/acme/repos/app", &w.admin);
    assert_eq!(st, 200, "{out}");
}

/// The cancelled job's **own, revoked** token is told 410, not 401.
///
/// This is the case production is actually in and the one that was
/// broken. Cancelling revokes the job token and *then* asks the
/// platform to stop the container, so the container we are trying to
/// stop is holding a dead credential. `verify` answers `None` for a
/// revoked token, so every runner call was refused 401 at the door and
/// the 410 that means "stop" was never reached — and a runner reads 401
/// as something to retry. A superseded build therefore ran every
/// remaining step to completion and learned it had been cancelled only
/// when it tried to report the verdict, which is precisely the compute
/// superseding exists to save.
///
/// So the cancellation goes through the route a person's Cancel button
/// goes through, revoking included, rather than through `cancel_run`
/// alone — a test that leaves the token live cannot see this at all.
#[test]
fn a_cancelled_jobs_own_revoked_token_is_told_410_not_401() {
    let minio = Minio::shared();
    let scratch = Scratch::new("runner-api-410-revoked");
    let (w, job, token) = world("runner-api-410-revoked", minio, &scratch);

    // The product's own door: cancel revokes the job's token and asks
    // for the task to be stopped.
    let (st, out) = w.server.post(
        &format!(
            "/v1/orgs/acme/repos/app/workflow-runs/{}/cancel",
            job.run_id
        ),
        &w.admin,
        None,
    );
    assert_eq!(st, 200, "{out}");
    assert!(
        auth::verify(&w.db, &token).unwrap().is_none(),
        "the cancel did not revoke the job token, so this test proves nothing"
    );

    for (method, path, body) in [
        ("GET", format!("/v1/runner/jobs/{}", job.id), None),
        (
            "POST",
            format!("/v1/runner/jobs/{}/log", job.id),
            Some(r#"{"seq":1,"text":"still going"}"#.to_string()),
        ),
        ("POST", format!("/v1/runner/jobs/{}/lease", job.id), None),
        (
            "POST",
            format!("/v1/runner/jobs/{}/finish", job.id),
            Some(r#"{"state":"passed"}"#.to_string()),
        ),
    ] {
        let (st, out) = json(&w.server, method, &path, &token, body.as_deref());
        assert_eq!(st, 410, "{method} {path} -> {out}");
        assert_eq!(
            out["state"],
            serde_json::json!("cancelled"),
            "the runner has to be told what happened, not merely refused: {out}"
        );
    }

    // The refusal ladder is unchanged for everybody else. A revoked
    // token that is not this job's, and a token that never existed, are
    // both 401 — the new answer is only ever given to the credential
    // that owns the job.
    let sibling = workflows::jobs_of(&w.db, &job.run_id)
        .unwrap()
        .into_iter()
        .find(|j| j.key == "ship")
        .unwrap();
    let (st, out) = json(
        &w.server,
        "GET",
        &format!("/v1/runner/jobs/{}", sibling.id),
        &token,
        None,
    );
    assert_eq!(
        st, 401,
        "another job's state, to a token that is not its: {out}"
    );
    let (st, out) = json(
        &w.server,
        "GET",
        &format!("/v1/runner/jobs/{}", job.id),
        "weft_nosuchtoken_nosuchsecret",
        None,
    );
    assert_eq!(st, 401, "{out}");
    // And the revocation is real where it matters: the repository is
    // closed to that credential however the runner routes answer.
    let (st, out) = json(&w.server, "GET", "/v1/orgs/acme/repos/app", &token, None);
    assert_eq!(st, 401, "{out}");

    // The verdict it tried to report did not land.
    let after = workflows::job(&w.db, &job.id).unwrap().unwrap();
    assert_eq!(after.state, "cancelled");
    assert!(w.server.healthy());
}

/// A verdict reported twice. The second is refused rather than applied
/// — a runner that retries a `finish` it never saw the answer to must
/// not be able to turn a failure into a pass.
///
/// It is refused at the job (410, with the state it settled in) even
/// though reporting a verdict revokes the token that reported it: a
/// runner retrying a call it never saw the answer to is the one caller
/// entitled to know its job is over, and 401 would send it round the
/// retry loop again. That the credential itself is dead is asserted
/// where it is actually load-bearing — against the repository, below.
#[test]
fn a_second_verdict_is_refused_rather_than_applied() {
    let minio = Minio::shared();
    let scratch = Scratch::new("runner-api-twice");
    let (w, job, token) = world("runner-api-twice", minio, &scratch);

    let path = format!("/v1/runner/jobs/{}/finish", job.id);
    let (st, out) = json(
        &w.server,
        "POST",
        &path,
        &token,
        Some(r#"{"state":"failed","error":"step \"Test\" exited 1"}"#),
    );
    assert_eq!(st, 200, "{out}");

    let (st, out) = json(
        &w.server,
        "POST",
        &path,
        &token,
        Some(r#"{"state":"passed"}"#),
    );
    assert_eq!(st, 410, "{out}");
    assert_eq!(
        out["state"],
        serde_json::json!("failed"),
        "the retry has to learn the verdict already landed, and which one: {out}"
    );
    // The credential really is gone; the repository is the thing it was
    // minted to read, and it is closed.
    let (st, out) = json(&w.server, "GET", "/v1/orgs/acme/repos/app", &token, None);
    assert_eq!(
        st, 401,
        "the reporting credential outlived its verdict: {out}"
    );
    let after = workflows::job(&w.db, &job.id).unwrap().unwrap();
    assert_eq!(after.state, "failed");
    assert_eq!(after.error.as_deref(), Some("step \"Test\" exited 1"));

    // The failure cascaded, the run is failed, and — the part that is
    // easy to leave out — the skipped job's mirrored check row followed
    // it. A check left saying `queued` for a job that will never run
    // holds the land gate shut with nothing on the page to explain it.
    assert_eq!(
        workflows::run_by_id(&w.db, &job.run_id)
            .unwrap()
            .unwrap()
            .state,
        "failed"
    );
    let (st, out) = w
        .server
        .get("/v1/orgs/acme/repos/app/checks/runs", &w.admin);
    assert_eq!(st, 200, "{out}");
    let rows: Vec<serde_json::Value> = out["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["commit_sha"] == serde_json::json!(SHA))
        .cloned()
        .collect();
    let by = |n: &str| {
        rows.iter()
            .find(|r| r["name"] == n)
            .unwrap_or_else(|| panic!("no {n}: {out}"))
            .clone()
    };
    assert_eq!(
        by("ci / build (linux)")["state"],
        serde_json::json!("failing")
    );
    assert_eq!(
        by("ci / ship")["state"],
        serde_json::json!("skipped"),
        "the cascade reached the check row, not only the job row"
    );
    assert!(w.server.healthy());
}

/// The verdict is the last thing the job's credential is entitled to do.
///
/// The token is minted to outlive the job's whole timeout plus slack, so
/// a job that finishes in a minute would otherwise leave a live
/// `repo:read` on the repository for hours — held by a container that
/// has just finished executing somebody's untrusted `run:` lines as the
/// same uid as the runner, which means the token in its environment has
/// to be assumed read. `finish` revokes it.
///
/// The assertion that matters is the **repository** one. The runner
/// routes answer this token 410-with-state — it is the job's own token
/// and the job has stopped, which is the one thing a runner still needs
/// to be told — so they say nothing about whether the credential
/// survived. Only a repository route can: `repo:read` is what the token
/// was actually minted for, and a 401 there is the revocation.
#[test]
fn a_verdict_revokes_the_credential_that_reported_it() {
    let minio = Minio::shared();
    let scratch = Scratch::new("runner-api-revoke");
    let (w, job, token) = world("runner-api-revoke", minio, &scratch);

    // While the job runs, the token reads its own spec.
    let (st, out) = json(
        &w.server,
        "GET",
        &format!("/v1/runner/jobs/{}", job.id),
        &token,
        None,
    );
    assert_eq!(st, 200, "{out}");

    let (st, out) = json(
        &w.server,
        "POST",
        &format!("/v1/runner/jobs/{}/finish", job.id),
        &token,
        Some(r#"{"state":"passed"}"#),
    );
    assert_eq!(st, 200, "{out}");

    // Every call, not only the one the runner would retry.
    for (method, path, body) in [
        ("GET", format!("/v1/runner/jobs/{}", job.id), None),
        (
            "POST",
            format!("/v1/runner/jobs/{}/lease", job.id),
            Some("{}".to_string()),
        ),
        (
            "POST",
            format!("/v1/runner/jobs/{}/finish", job.id),
            Some(r#"{"state":"failed"}"#.to_string()),
        ),
    ] {
        let (st, out) = json(&w.server, method, &path, &token, body.as_deref());
        assert_eq!(st, 410, "{method} {path} after the verdict: {out}");
        assert_eq!(out["state"], serde_json::json!("passed"), "{out}");
    }
    let (st, out) = post_chunk(&w.server, &job.id, &token, 1, "late");
    assert_eq!(st, 410, "{out}");

    // And the repository itself is closed to it, which is what the
    // credential was actually for.
    let (st, out) = json(&w.server, "GET", "/v1/orgs/acme/repos/app", &token, None);
    assert_eq!(st, 401, "{out}");

    // The verdict still landed, and the next job's own credential is
    // untouched — the revoke is one token, not the org's.
    assert_eq!(
        workflows::job(&w.db, &job.id).unwrap().unwrap().state,
        "passed"
    );
    let next = claim(&w.db, &w.org_id).expect("ship is ready");
    let token2 = launch(&w.db, &w.org_id, &w.repo_id, &next, None);
    let (st, out) = json(
        &w.server,
        "GET",
        &format!("/v1/runner/jobs/{}", next.id),
        &token2,
        None,
    );
    assert_eq!(st, 200, "{out}");
    assert!(w.server.healthy());
}

/// A spec that is not a JSON object still produces a usable job.
///
/// The planner writes that column, so a spec that is not an object is
/// our bug rather than the runner's — and the runner is the worst place
/// to discover it. A 500 here is retried, and a runner that cannot read
/// its own spec has nothing to report and no way to stop asking, so the
/// job burns its attempts and the person waiting sees a build that never
/// starts and never fails. Handing back the identity fields with no
/// steps gives a job that runs, does nothing, and reports `passed` in
/// seconds — visible, finite, and diagnosable from the log.
#[test]
fn a_spec_that_is_not_an_object_is_answered_rather_than_500ed() {
    let minio = Minio::shared();
    let scratch = Scratch::new("runner-api-badspec");
    let (w, _first, _token) = world("runner-api-badspec", minio, &scratch);

    workflows::create_run(
        &w.db,
        &w.org_id,
        &w.repo_id,
        &NewRun {
            file: ".weft/broken.yml",
            name: "broken",
            commit_sha: &"2".repeat(40),
            ref_name: Some("main"),
            event: "push",
            change_key: None,
            changeset_id: None,
            composition: None,
            from_fork: false,
        },
        &[NewJob {
            job_id: "solo",
            key: "solo",
            matrix: "{}",
            needs: &[],
            // Valid JSON, wrong shape — the case a `_ =>` arm exists
            // for. (A spec that is not JSON at all takes the same arm.)
            spec: "[\"steps\"]",
            ..Default::default()
        }],
    )
    .unwrap();

    let job = claim(&w.db, &w.org_id).expect("the new run's only job is ready");
    let token = launch(&w.db, &w.org_id, &w.repo_id, &job, None);

    let (st, spec) = json(
        &w.server,
        "GET",
        &format!("/v1/runner/jobs/{}", job.id),
        &token,
        None,
    );
    assert_eq!(st, 200, "{spec}");
    assert_eq!(spec["id"], serde_json::json!(job.id));
    assert_eq!(spec["key"], serde_json::json!("solo"));
    assert_eq!(
        spec["clone_url"],
        serde_json::json!(format!("{}/acme/app.git", w.server.base)),
        "the identity a runner needs to do anything at all is still there"
    );
    assert!(
        spec.get("steps").is_none(),
        "and nothing was invented to fill the gap: {spec}"
    );

    // It is a job like any other from here: it reports, and the run
    // settles rather than hanging on a spec nobody could read.
    let (st, out) = json(
        &w.server,
        "POST",
        &format!("/v1/runner/jobs/{}/finish", job.id),
        &token,
        Some(r#"{"state":"passed"}"#),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        workflows::run_by_id(&w.db, &job.run_id)
            .unwrap()
            .unwrap()
            .state,
        "passed"
    );
    assert!(w.server.healthy());
}
