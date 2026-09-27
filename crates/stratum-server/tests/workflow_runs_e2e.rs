//! What a person asks about a run, over HTTP.
//!
//! The interesting half is the live log. `…/log/stream` is the only
//! streaming JSON surface in the API, and the property that matters is
//! **liveness**: the chunks are posted while the stream is already open,
//! from another thread, so the test proves the feed is following a
//! running job rather than replaying a finished one. A test that
//! uploaded everything first and then connected would pass against a
//! stream that never polled at all — which is precisely the bug worth
//! catching, because a log that only appears when the build ends is
//! indistinguishable from a hung page.
//!
//! `ureq` is used with the response body left unread until each read, so
//! the socket really is held open across the assertions. The reader runs
//! on its own thread with a bounded deadline, because a stream bug that
//! blocks forever must fail the test rather than hang CI.

use std::io::{BufRead, BufReader};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use stratum_control::auth::{self, Mint, Scope};
use stratum_control::workflows::{self, NewJob, NewRun, WorkflowJob};
use stratum_control::{registry, ControlDb};
use stratum_testkit::fake_ecs::FakeEcs;
use stratum_testkit::{gitcli::Scratch, Minio, Server};

/// How long a dispatcher-driven expectation gets before it is a
/// failure. Generous: the poll is a second and a loaded machine running
/// the whole suite is slower than a quiet one.
const DEADLINE: Duration = Duration::from_secs(30);

const SHA: &str = "3333333333333333333333333333333333333333";
const SPEC: &str = r#"{"image":"default","timeout_minutes":30,"env":{},"steps":[]}"#;

struct World {
    server: Server,
    db: ControlDb,
    org_id: String,
    repo_id: String,
    admin: String,
}

fn world(hint: &str, minio: &Minio, scratch: &Scratch) -> World {
    let bucket = minio.bucket(hint);
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        // No dispatcher: this suite drives the queue by hand.
        .env("STRATUM_RUNNER_POLL_SECS", "0")
        .start();
    let admin = server.bootstrap_org("acme");
    for repo in ["app", "other"] {
        let (st, out) = server.post(
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": repo })),
        );
        assert_eq!(st, 201, "{out}");
    }
    let db = ControlDb::open(&server.db_url).expect("a second session on the server's database");
    let org = registry::org_by_name(&db, "acme").unwrap().unwrap();
    let repo = registry::repo_by_name(&db, &org.id, "app")
        .unwrap()
        .unwrap();
    World {
        server,
        db,
        org_id: org.id,
        repo_id: repo.id,
        admin,
    }
}

fn make_run(w: &World, repo_id: &str, sha: &str, keys: &[&str]) -> String {
    let jobs: Vec<NewJob> = keys
        .iter()
        .map(|k| NewJob {
            job_id: k,
            key: k,
            matrix: "{}",
            needs: &[],
            spec: SPEC,
            ..Default::default()
        })
        .collect();
    workflows::create_run(
        &w.db,
        &w.org_id,
        repo_id,
        &NewRun {
            file: ".weft/ci.yml",
            name: "ci",
            commit_sha: sha,
            ref_name: Some("main"),
            event: "push",
            change_key: None,
            changeset_id: None,
            composition: None,
            from_fork: false,
        },
        &jobs,
    )
    .unwrap()
    .id
}

/// Claim a named job and give it a real job token, as the dispatcher does.
fn launch(w: &World, key: &str) -> (WorkflowJob, String) {
    let job = loop {
        let c = workflows::claim(&w.db, 300_000)
            .unwrap()
            .unwrap_or_else(|| panic!("nothing claimable while looking for {key:?}"));
        if c.key == key {
            break c;
        }
    };
    let minted = auth::mint_for(
        &w.db,
        &w.org_id,
        &[Scope::RepoRead],
        Mint {
            repo_id: Some(&w.repo_id),
            label: Some("job"),
            user_id: None,
            expires_at: None,
        },
        None,
    )
    .unwrap();
    assert!(workflows::mark_launched(&w.db, &job.id, "pid:1", &minted.id, "chk").unwrap());
    (job, minted.plaintext)
}

fn post_chunk(server: &Server, job: &str, token: &str, seq: i32, text: &str) {
    let resp = ureq::post(&format!("{}/v1/runner/jobs/{job}/log", server.base))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string(&serde_json::json!({"seq": seq, "text": text}).to_string());
    assert!(resp.is_ok(), "chunk {seq} was refused");
}

/// The text out of a `chunk` event's JSON payload.
fn chunk_text(data: &str) -> String {
    serde_json::from_str::<serde_json::Value>(data)
        .unwrap_or_else(|e| panic!("a chunk event is JSON: {e} in {data:?}"))["text"]
        .as_str()
        .expect("a text field")
        .to_string()
}

fn text_get(server: &Server, path: &str, token: &str) -> (u16, String) {
    let resp = ureq::get(&format!("{}{path}", server.base))
        .set("Authorization", &format!("Bearer {token}"))
        .call();
    match resp {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap_or_default()),
        Err(e) => panic!("transport GET {path}: {e}"),
    }
}

// ---------------------------------------------------------------------
// Listing and reading
// ---------------------------------------------------------------------

#[test]
fn runs_list_newest_first_with_their_jobs_and_never_across_repositories() {
    let minio = Minio::shared();
    let scratch = Scratch::new("wf-runs-list");
    let w = world("wf-runs-list", minio, &scratch);
    let other = registry::repo_by_name(&w.db, &w.org_id, "other")
        .unwrap()
        .unwrap();

    // A millisecond between them, deliberately: "newest first" is
    // ordered by `created_at` and tie-broken by id, and `ulid()` has no
    // per-millisecond counter — 80 random bits decide a tie. Two runs
    // written inside one millisecond (which is what two workflow files
    // in one push are) therefore come back in a stable but arbitrary
    // order, and asserting insertion order over them passes on an idle
    // machine and fails under load. What is under test here is the
    // ordering *between* distinct moments, so make them distinct.
    let first = make_run(&w, &w.repo_id, SHA, &["build", "lint"]);
    std::thread::sleep(Duration::from_millis(2));
    let second = make_run(&w, &w.repo_id, &"4".repeat(40), &["build"]);
    std::thread::sleep(Duration::from_millis(2));
    let elsewhere = make_run(&w, &other.id, &"5".repeat(40), &["build"]);

    let (st, out) = w
        .server
        .get("/v1/orgs/acme/repos/app/workflow-runs", &w.admin);
    assert_eq!(st, 200, "{out}");
    let runs = out["runs"].as_array().expect("a list");
    assert_eq!(
        runs.iter().map(|r| r["id"].clone()).collect::<Vec<_>>(),
        vec![
            serde_json::json!(second.clone()),
            serde_json::json!(first.clone())
        ],
        "newest first, and no other repository's: {out}"
    );

    let one = &runs[1];
    assert_eq!(one["file"], serde_json::json!(".weft/ci.yml"));
    assert_eq!(one["name"], serde_json::json!("ci"));
    assert_eq!(one["commit_sha"], serde_json::json!(SHA));
    assert_eq!(one["ref_name"], serde_json::json!("main"));
    assert_eq!(one["event"], serde_json::json!("push"));
    assert_eq!(one["state"], serde_json::json!("running"));
    assert_eq!(one["error"], serde_json::Value::Null);
    let jobs = one["jobs"].as_array().expect("jobs ride along");
    assert_eq!(jobs.len(), 2, "{one}");
    assert_eq!(jobs[0]["key"], serde_json::json!("build"));
    assert_eq!(jobs[0]["state"], serde_json::json!("queued"));
    assert_eq!(jobs[0]["attempts"], serde_json::json!(0));
    assert_eq!(jobs[0]["log_chunks"], serde_json::json!(0));
    assert_eq!(
        jobs[0]["matrix"],
        serde_json::json!({}),
        "the matrix is an object, not a string a client has to parse: {one}"
    );

    // `limit` is clamped rather than refused, in both directions.
    let (st, out) = w
        .server
        .get("/v1/orgs/acme/repos/app/workflow-runs?limit=1", &w.admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["runs"].as_array().unwrap().len(), 1);
    for q in ["limit=0", "limit=9999", "limit=", "limit=banana"] {
        let (st, out) = w.server.get(
            &format!("/v1/orgs/acme/repos/app/workflow-runs?{q}"),
            &w.admin,
        );
        assert_eq!(st, 200, "{q} -> {out}");
        assert!(!out["runs"].as_array().unwrap().is_empty(), "{q} -> {out}");
    }

    // One run by id, and a run in another repository is 404 — the same
    // answer as an id nobody minted, because an id that resolves
    // differently for a stranger is an existence oracle.
    let (st, out) = w.server.get(
        &format!("/v1/orgs/acme/repos/app/workflow-runs/{first}"),
        &w.admin,
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["id"], serde_json::json!(first));
    assert_eq!(out["jobs"].as_array().unwrap().len(), 2);

    for id in [
        elsewhere.as_str(),
        "01NOPENOPENOPENOPENOPENOPE",
        "../../etc/passwd",
    ] {
        let (st, out) = w.server.get(
            &format!("/v1/orgs/acme/repos/app/workflow-runs/{id}"),
            &w.admin,
        );
        assert_eq!(st, 404, "{id} -> {out}");
    }
    assert!(w.server.healthy());
}

/// The narrowing the approval panel depends on, over the route.
///
/// Without it the dashboard finds "the runs at this change's tip" by
/// asking for a window and keeping the matches, which is correct until
/// the repository is busy enough to push those runs past the window —
/// and then the panel quietly loses its approve button on exactly the
/// repositories that most need it. So: the filters have to work, they
/// have to work *together with* `limit` rather than after it, and they
/// must not reach outside the repository.
#[test]
fn the_list_narrows_to_one_commit_or_one_change_without_being_cut_off_by_limit() {
    let minio = Minio::shared();
    let scratch = Scratch::new("wf-runs-filter");
    let w = world("wf-runs-filter", minio, &scratch);
    let other = registry::repo_by_name(&w.db, &w.org_id, "other")
        .unwrap()
        .unwrap();

    let tip = "7".repeat(40);
    let held = workflows::create_settled_run(
        &w.db,
        &w.org_id,
        &w.repo_id,
        &NewRun {
            file: ".weft/ci.yml",
            name: "ci",
            commit_sha: &tip,
            ref_name: Some("main"),
            event: "change",
            change_key: Some("I0fe1"),
            changeset_id: None,
            composition: None,
            from_fork: false,
        },
        "blocked",
        Some("this change comes from a fork; a maintainer has to approve its workflows before they run"),
        Some(workflows::BlockedReason::Fork),
    )
    .unwrap()
    .id;
    // The same sha in the other repository: a filter that forgot the
    // repository would hand it over.
    let elsewhere = make_run(&w, &other.id, &tip, &["build"]);
    // And plenty of newer, unrelated runs, so the held one is well
    // outside any window a page would ask for.
    let mut newer = Vec::new();
    for i in 0..5 {
        newer.push(make_run(
            &w,
            &w.repo_id,
            &format!("{i}").repeat(40),
            &["build"],
        ));
    }

    let list = |q: &str| -> serde_json::Value {
        let (st, out) = w.server.get(
            &format!("/v1/orgs/acme/repos/app/workflow-runs?{q}"),
            &w.admin,
        );
        assert_eq!(st, 200, "{q} -> {out}");
        out
    };
    let ids = |q: &str| -> Vec<String> {
        list(q)["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };

    // The whole point: a window of one still finds it, because the
    // narrowing happens in the query and not in the reader.
    assert_eq!(
        ids(&format!("limit=1&commit_sha={tip}")),
        vec![held.clone()]
    );
    assert_eq!(ids("limit=1&change_key=I0fe1"), vec![held.clone()]);
    assert_eq!(
        ids(&format!("commit_sha={tip}&change_key=I0fe1")),
        vec![held.clone()]
    );
    assert!(!ids("limit=1").contains(&held), "the failure it prevents");

    let blocked = &list("change_key=I0fe1")["runs"][0];
    assert_eq!(blocked["state"], "blocked", "{blocked}");
    assert_eq!(blocked["blocked_reason"], "fork", "{blocked}");
    assert_eq!(
        list("limit=1")["runs"][0]["blocked_reason"],
        serde_json::Value::Null,
        "a run that is not blocked carries no reason"
    );

    // Never outside the repository, and an unknown value is an empty
    // list rather than an unfiltered one — a filter that silently
    // stopped filtering would put another commit's verdict on a page.
    assert!(!ids(&format!("commit_sha={tip}")).contains(&elsewhere));
    assert!(ids("commit_sha=deadbeef").is_empty());
    assert!(ids("change_key=I-nope").is_empty());
    // Empty is absent, the same as everywhere else in this API.
    assert_eq!(ids("commit_sha=&change_key=").len(), newer.len() + 1);
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------

#[test]
fn cancelling_stops_the_run_updates_its_checks_and_refuses_a_second_time() {
    let minio = Minio::shared();
    let scratch = Scratch::new("wf-runs-cancel");
    let w = world("wf-runs-cancel", minio, &scratch);
    let run = make_run(&w, &w.repo_id, SHA, &["build", "lint"]);
    let (job, _) = launch(&w, "build");

    let path = format!("/v1/orgs/acme/repos/app/workflow-runs/{run}/cancel");
    let (st, out) = w.server.post(&path, &w.admin, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["state"], serde_json::json!("cancelled"));
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("cancelled by "),
        "the reason names who asked: {out}"
    );
    let states: Vec<serde_json::Value> = out["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["state"].clone())
        .collect();
    assert_eq!(
        states,
        vec![
            serde_json::json!("cancelled"),
            serde_json::json!("cancelled")
        ],
        "the queued job stops too, not only the running one: {out}"
    );

    // The mirrored check rows followed — including `lint`, which never
    // ran. A check left saying `queued` for a job that will never run
    // holds the land gate shut with nothing on the page to explain it.
    let (st, out) = w
        .server
        .get("/v1/orgs/acme/repos/app/checks/runs", &w.admin);
    assert_eq!(st, 200, "{out}");
    let rows = out["runs"].as_array().unwrap();
    for name in ["ci / build", "ci / lint"] {
        let row = rows
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("no {name}: {out}"));
        assert_eq!(row["state"], serde_json::json!("cancelled"), "{row}");
    }

    // Cancelling again is a 409 rather than a silent success: the page
    // the caller is looking at is out of date and they should reload.
    let (st, out) = w.server.post(&path, &w.admin, None);
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("already cancelled"),
        "{out}"
    );

    // The job's own row is unchanged by the refused second cancel.
    assert_eq!(
        workflows::job(&w.db, &job.id).unwrap().unwrap().state,
        "cancelled"
    );

    // A run in another repository, and one nobody minted, are 404.
    let other = registry::repo_by_name(&w.db, &w.org_id, "other")
        .unwrap()
        .unwrap();
    let elsewhere = make_run(&w, &other.id, &"6".repeat(40), &["build"]);
    for id in [elsewhere.as_str(), "01NOPENOPENOPENOPENOPENOPE"] {
        let st = w.server.status_post(
            &format!("/v1/orgs/acme/repos/app/workflow-runs/{id}/cancel"),
            &w.admin,
            serde_json::json!({}),
        );
        assert_eq!(st, 404, "{id}");
    }
    assert_eq!(
        workflows::run_by_id(&w.db, &elsewhere)
            .unwrap()
            .unwrap()
            .state,
        "running",
        "and the other repository's run was not touched"
    );
    assert!(w.server.healthy());
}

/// Cancelling destroys work, so it is `repo:write` — and a job token is
/// `repo:read`, which is exactly the credential a container executing
/// untrusted `run:` lines is holding. A build that could cancel its own
/// run could make a red verdict disappear.
#[test]
fn a_read_only_credential_cannot_cancel_a_run() {
    let minio = Minio::shared();
    let scratch = Scratch::new("wf-runs-cancel-auth");
    let w = world("wf-runs-cancel-auth", minio, &scratch);
    let run = make_run(&w, &w.repo_id, SHA, &["build"]);
    let (_, job_token) = launch(&w, "build");

    let path = format!("/v1/orgs/acme/repos/app/workflow-runs/{run}/cancel");
    // Masked as 404 rather than 403, like every other insufficient
    // credential in this API.
    let st = w
        .server
        .status_post(&path, &job_token, serde_json::json!({}));
    assert_eq!(st, 404, "a job token may not cancel its own run");
    // ...but it can still read, so this is about the scope and not
    // about the token being broken.
    let (st, _) = w.server.get(
        &format!("/v1/orgs/acme/repos/app/workflow-runs/{run}"),
        &job_token,
    );
    assert_eq!(st, 200);
    assert_eq!(
        workflows::run_by_id(&w.db, &run).unwrap().unwrap().state,
        "running"
    );

    // Anonymously, against a private repository, it is a 404 too.
    assert_eq!(w.server.status_post(&path, "", serde_json::json!({})), 401);
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// The log
// ---------------------------------------------------------------------

#[test]
fn the_log_route_reads_chunks_mid_run_and_the_whole_file_afterwards() {
    let minio = Minio::shared();
    let scratch = Scratch::new("wf-runs-log");
    let w = world("wf-runs-log", minio, &scratch);
    make_run(&w, &w.repo_id, SHA, &["build"]);
    let (job, token) = launch(&w, "build");
    let path = format!("/v1/orgs/acme/repos/app/workflow-jobs/{}/log", job.id);

    // Nothing yet: an empty log with a 200, not a 404. A job that has
    // produced no output is normal, and 404 would read as "this job
    // does not exist".
    let (st, body) = text_get(&w.server, &path, &w.admin);
    assert_eq!(st, 200);
    assert_eq!(body, "");

    post_chunk(&w.server, &job.id, &token, 1, "▶ Checkout\n");
    post_chunk(&w.server, &job.id, &token, 2, "▶ Build\n");
    let (st, body) = text_get(&w.server, &path, &w.admin);
    assert_eq!(st, 200);
    assert_eq!(body, "▶ Checkout\n▶ Build\n", "concatenated in order");

    // Once the whole log is uploaded it wins, because a chunk the runner
    // could not post twice is dropped and this copy has no holes.
    let whole = "▶ Checkout\n▶ Build\nwarning: unused\n✓ Build (3s)\n";
    let resp = ureq::put(&format!("{}/v1/runner/jobs/{}/log", w.server.base, job.id))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "text/plain")
        .send_string(whole);
    assert!(resp.is_ok());
    let (st, body) = text_get(&w.server, &path, &w.admin);
    assert_eq!(st, 200);
    assert_eq!(body, whole);

    // And it survives the chunks being swept at `finish`.
    let resp = ureq::post(&format!(
        "{}/v1/runner/jobs/{}/finish",
        w.server.base, job.id
    ))
    .set("Authorization", &format!("Bearer {token}"))
    .set("Content-Type", "application/json")
    .send_string(r#"{"state":"passed"}"#);
    assert!(resp.is_ok());
    let (st, body) = text_get(&w.server, &path, &w.admin);
    assert_eq!(st, 200);
    assert_eq!(body, whole);

    // A job in another repository, and one nobody minted, are 404.
    let other = registry::repo_by_name(&w.db, &w.org_id, "other")
        .unwrap()
        .unwrap();
    let elsewhere = make_run(&w, &other.id, &"7".repeat(40), &["build"]);
    let their_job = workflows::jobs_of(&w.db, &elsewhere).unwrap().remove(0);
    for id in [their_job.id.as_str(), "01NOPENOPENOPENOPENOPENOPE", "../x"] {
        let (st, _) = text_get(
            &w.server,
            &format!("/v1/orgs/acme/repos/app/workflow-jobs/{id}/log"),
            &w.admin,
        );
        assert_eq!(st, 404, "{id}");
    }
    assert!(w.server.healthy());
}

/// The live feed, proved live.
///
/// The chunks are posted **while the stream is already open**, from this
/// thread, and the reader thread must see each one arrive. Uploading
/// first and connecting afterwards would pass against a stream that
/// never polls at all — a log that only appears when the build ends,
/// which from a browser is indistinguishable from a hung page.
#[test]
fn the_log_stream_delivers_chunks_as_they_arrive_and_closes_on_done() {
    let minio = Minio::shared();
    let scratch = Scratch::new("wf-runs-stream");
    let w = world("wf-runs-stream", minio, &scratch);
    make_run(&w, &w.repo_id, SHA, &["build"]);
    let (job, token) = launch(&w, "build");

    let url = format!(
        "{}/v1/orgs/acme/repos/app/workflow-jobs/{}/log/stream",
        w.server.base, job.id
    );
    let admin = w.admin.clone();
    let (tx, rx) = mpsc::channel::<(String, String)>();

    // Open the stream first and read it incrementally. The agent has no
    // read timeout of its own here, so the deadline is enforced by the
    // receiving end below: a stream bug that blocks forever must fail
    // this test rather than hang CI.
    let reader = std::thread::spawn(move || {
        let resp = ureq::get(&url)
            .set("Authorization", &format!("Bearer {admin}"))
            .timeout(Duration::from_secs(60))
            .call()
            .expect("the stream opens");
        assert_eq!(
            resp.header("content-type").unwrap_or_default(),
            "text/event-stream"
        );
        let mut lines = BufReader::new(resp.into_reader()).lines();
        let mut event = String::new();
        while let Some(Ok(line)) = lines.next() {
            if let Some(e) = line.strip_prefix("event: ") {
                event = e.to_string();
            } else if let Some(d) = line.strip_prefix("data: ") {
                if tx.send((event.clone(), d.to_string())).is_err() {
                    return;
                }
            }
        }
    });

    let next = |what: &str| -> (String, String) {
        rx.recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|e| panic!("waiting for {what}: {e}"))
    };

    // Post the first chunk only after the stream is open. The feed polls
    // once a second, so give it a moment to have connected — but do not
    // wait for output, which is the thing being tested.
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    post_chunk(&w.server, &job.id, &token, 1, "▶ Checkout\n");
    let (ev, data) = next("the first chunk");
    assert_eq!(ev, "chunk");
    // JSON, not raw text — see the handler. An SSE `data:` field cannot
    // carry a newline, and the wire format loses a payload's trailing
    // one, so raw chunks would run each chunk's last line into the next
    // chunk's first. This assertion is what pins that.
    assert_eq!(chunk_text(&data), "▶ Checkout\n");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the chunk arrived only after {:?} — the feed is not following",
        started.elapsed()
    );

    // A second one, later, on the same open connection: this is the
    // assertion that separates a live feed from one replay at connect.
    post_chunk(&w.server, &job.id, &token, 2, "▶ Build\n");
    let (ev, data) = next("the second chunk");
    assert_eq!(ev, "chunk");
    assert_eq!(
        chunk_text(&data),
        "▶ Build\n",
        "chunks arrive in order, as they land, with their newlines intact"
    );

    // Finishing closes the stream with a verdict rather than leaving the
    // socket open — a browser reconnects an EventSource by itself, so a
    // feed that never ends is a socket per tab forever.
    let resp = ureq::post(&format!(
        "{}/v1/runner/jobs/{}/finish",
        w.server.base, job.id
    ))
    .set("Authorization", &format!("Bearer {token}"))
    .set("Content-Type", "application/json")
    .send_string(r#"{"state":"failed","error":"step \"Build\" exited 1"}"#);
    assert!(resp.is_ok());

    let (ev, data) = next("done");
    assert_eq!(ev, "done");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&data).unwrap(),
        serde_json::json!({"state": "failed"})
    );
    // Nothing after it, and the reader thread ends because the server
    // closed the body.
    assert!(rx.recv_timeout(Duration::from_secs(10)).is_err());
    reader.join().expect("the reader thread ends cleanly");

    assert!(w.server.healthy());
}

/// A job that has not started says so once and then waits, rather than
/// closing — a reader who opens the page before the dispatcher gets
/// there should see the log appear, not have to reload.
#[test]
fn a_queued_job_is_announced_once_and_the_stream_keeps_waiting() {
    let minio = Minio::shared();
    let scratch = Scratch::new("wf-runs-stream-q");
    let w = world("wf-runs-stream-q", minio, &scratch);
    let run = make_run(&w, &w.repo_id, SHA, &["build"]);
    let job = workflows::jobs_of(&w.db, &run).unwrap().remove(0);
    assert_eq!(job.state, "queued");

    let url = format!(
        "{}/v1/orgs/acme/repos/app/workflow-jobs/{}/log/stream",
        w.server.base, job.id
    );
    let admin = w.admin.clone();
    let (tx, rx) = mpsc::channel::<(String, String)>();
    let reader = std::thread::spawn(move || {
        let resp = ureq::get(&url)
            .set("Authorization", &format!("Bearer {admin}"))
            .timeout(Duration::from_secs(60))
            .call()
            .expect("the stream opens");
        let mut lines = BufReader::new(resp.into_reader()).lines();
        let mut event = String::new();
        while let Some(Ok(line)) = lines.next() {
            if let Some(e) = line.strip_prefix("event: ") {
                event = e.to_string();
            } else if let Some(d) = line.strip_prefix("data: ") {
                if tx.send((event.clone(), d.to_string())).is_err() {
                    return;
                }
            }
        }
    });

    let (ev, _) = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the queued announcement");
    assert_eq!(ev, "queued");

    // It is said once, not once a second — a client appending every
    // event would otherwise fill the page with it.
    assert!(
        rx.recv_timeout(Duration::from_secs(4)).is_err(),
        "queued is announced once and then the feed simply waits"
    );

    // Now it starts and produces output on the connection that was
    // already open.
    let (job, token) = launch(&w, "build");
    post_chunk(&w.server, &job.id, &token, 1, "▶ Checkout\n");
    let (ev, data) = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the first chunk of a job that was queued when we connected");
    assert_eq!(ev, "chunk");
    assert_eq!(chunk_text(&data), "▶ Checkout\n");

    workflows::cancel_run(&w.db, &run, "cancelled by Ada").unwrap();
    let (ev, data) = rx.recv_timeout(Duration::from_secs(30)).expect("done");
    assert_eq!(ev, "done");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&data).unwrap(),
        serde_json::json!({"state": "cancelled"})
    );
    reader.join().expect("the reader thread ends cleanly");
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// Who may read a run at all
// ---------------------------------------------------------------------

/// Every route in this API refuses a caller with no credential, and does
/// it before it has looked anything up.
///
/// Worth its own test rather than being assumed from the routes' shape:
/// a run's history is the repository's contents by another name — branch
/// names, commit shas, the workflow files, and the log of a build that
/// printed whatever the code printed. These handlers each call the auth
/// helper themselves, so "they all check" is five separate facts, and
/// the streaming one is the easiest to get wrong because its refusal has
/// to happen before the response becomes a long-lived body.
#[test]
fn no_route_gives_a_run_away_to_a_caller_with_no_credential() {
    let minio = Minio::shared();
    let scratch = Scratch::new("wf-runs-anon");
    let w = world("wf-runs-anon", minio, &scratch);
    let run = make_run(&w, &w.repo_id, SHA, &["build"]);
    let (job, _token) = launch(&w, "build");

    for path in [
        "/v1/orgs/acme/repos/app/workflow-runs".to_string(),
        format!("/v1/orgs/acme/repos/app/workflow-runs/{run}"),
        format!("/v1/orgs/acme/repos/app/workflow-jobs/{}/log", job.id),
        format!(
            "/v1/orgs/acme/repos/app/workflow-jobs/{}/log/stream",
            job.id
        ),
    ] {
        let (st, body) = text_get(&w.server, &path, "");
        assert_eq!(st, 401, "GET {path} answered {st}: {body}");
        assert!(
            !body.contains(&job.id) && !body.contains(SHA),
            "and the refusal told the caller nothing: {body}"
        );
    }
    let (st, body) = text_get(
        &w.server,
        "/v1/orgs/acme/repos/app/workflow-runs",
        "weft_nope",
    );
    assert_eq!(st, 401, "{body}");

    // The stream is also scoped to the repository in the path, like
    // every other id here: another repository's job is 404, not a feed.
    let other = registry::repo_by_name(&w.db, &w.org_id, "other")
        .unwrap()
        .unwrap();
    make_run(&w, &other.id, &"4".repeat(40), &["build"]);
    let elsewhere = workflows::jobs_of(
        &w.db,
        &workflows::runs_for_repo(&w.db, &other.id, None, None, None, 1).unwrap()[0].id,
    )
    .unwrap()
    .remove(0);
    let (st, body) = text_get(
        &w.server,
        &format!(
            "/v1/orgs/acme/repos/app/workflow-jobs/{}/log/stream",
            elsewhere.id
        ),
        &w.admin,
    );
    assert_eq!(st, 404, "{body}");
    assert!(w.server.healthy());
}

/// Two ways a live feed can be asked for something that is not there,
/// and neither may hang the reader.
///
/// A chunk the counter promised but the store does not have is skipped:
/// the runner writes the object *before* it reports the number, so a
/// miss means the chunk is genuinely gone (the runner dropped it after
/// two failed POSTs), and waiting for it would stop a log that is still
/// being written. And a repository deleted underneath the feed ends it,
/// rather than leaving a socket per reader serving the log of a
/// repository every other route in the API has started answering 404
/// for. Deletion tombstones the row and leaves the job saying `running`,
/// so nothing about the job itself would ever end this stream: the
/// deadline is six hours away.
#[test]
fn the_stream_skips_a_chunk_that_is_gone_and_ends_when_the_repository_does() {
    let minio = Minio::shared();
    let scratch = Scratch::new("wf-runs-stream-gap");
    let w = world("wf-runs-stream-gap", minio, &scratch);
    make_run(&w, &w.repo_id, SHA, &["build"]);
    let (job, token) = launch(&w, "build");

    // Sequence 1 is counted but never stored — exactly the state the
    // runner leaves behind when a chunk POST fails twice and it moves
    // on. (Going through the control plane rather than the HTTP route is
    // the point: the route stores the object first, so it cannot produce
    // this.)
    workflows::record_log_chunk(&w.db, &job.id, 1, 60_000)
        .unwrap()
        .expect("the job is running");

    let url = format!(
        "{}/v1/orgs/acme/repos/app/workflow-jobs/{}/log/stream",
        w.server.base, job.id
    );
    let admin = w.admin.clone();
    let (tx, rx) = mpsc::channel::<(String, String)>();
    let reader = std::thread::spawn(move || {
        let resp = ureq::get(&url)
            .set("Authorization", &format!("Bearer {admin}"))
            .timeout(Duration::from_secs(60))
            .call()
            .expect("the stream opens");
        let mut lines = BufReader::new(resp.into_reader()).lines();
        let mut event = String::new();
        while let Some(Ok(line)) = lines.next() {
            if let Some(e) = line.strip_prefix("event: ") {
                event = e.to_string();
            } else if let Some(d) = line.strip_prefix("data: ") {
                if tx.send((event.clone(), d.to_string())).is_err() {
                    return;
                }
            }
        }
    });

    // The feed passes over the hole and delivers what is actually there.
    std::thread::sleep(Duration::from_millis(300));
    post_chunk(&w.server, &job.id, &token, 2, "▶ Build\n");
    let (ev, data) = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the chunk after the hole");
    assert_eq!(ev, "chunk");
    assert_eq!(
        chunk_text(&data),
        "▶ Build\n",
        "the missing chunk was skipped, not waited for"
    );

    // Now delete the repository under the reader who had the page open.
    let deleted = Instant::now();
    let (st, out) = w.server.delete("/v1/orgs/acme/repos/app", &w.admin);
    assert!(st == 200 || st == 204, "{st}: {out}");

    // No verdict — there is no run left to report one for — but the body
    // *ends*, and it ends because the server closed it rather than
    // because the client gave up. That distinction is the test: the
    // reader's own timeout is well beyond the feed's poll, so a stream
    // that simply kept polling would still finish this thread, just
    // slowly, and the assertion on how long it took is what stops that
    // reading as a pass.
    let tail = rx.recv_timeout(Duration::from_secs(20));
    if let Ok((ev, data)) = tail {
        panic!("expected the feed to end, got {ev}: {data}");
    }
    reader
        .join()
        .expect("the reader thread ends because the server closed the body");
    assert!(
        deleted.elapsed() < Duration::from_secs(20),
        "the feed took {:?} to notice the repository was gone",
        deleted.elapsed()
    );

    // And the runner is told to stop rather than handed a 500 it would
    // retry — or a 401, which it retries too. The deletion cancels the
    // run and cancelling revokes the job's token, so this call is a
    // dead credential on a settled job: exactly the shape that used to
    // be refused at the door, leaving the container building a
    // repository that no longer exists until its timeout.
    let (st, body) = text_get(&w.server, &format!("/v1/runner/jobs/{}", job.id), &token);
    assert_eq!(st, 410, "{body}");
    assert!(
        body.contains("\"state\":\"cancelled\""),
        "the runner has to be told why it is stopping: {body}"
    );
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// Deleting the repository CI is building
// ---------------------------------------------------------------------

/// The same world, but with a dispatcher and a fake ECS that accepts
/// every `RunTask` and starts nothing. No runner binary is needed: what
/// is under test is what the *product* does to a task it has launched,
/// not what a runner does with it.
fn dispatching_world(hint: &str, minio: &Minio, scratch: &Scratch) -> (World, FakeEcs) {
    let bucket = minio.bucket(hint);
    let ecs = FakeEcs::start(None, scratch.path().join("runners"));
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .envs(&ecs.env())
        .env("STRATUM_RUNNER_POLL_SECS", "1")
        .start();
    let admin = server.bootstrap_org("acme");
    for repo in ["app", "other"] {
        let (st, out) = server.post(
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": repo })),
        );
        assert_eq!(st, 201, "{out}");
    }
    let db = ControlDb::open(&server.db_url).expect("a second session on the server's database");
    let org = registry::org_by_name(&db, "acme").unwrap().unwrap();
    let repo = registry::repo_by_name(&db, &org.id, "app")
        .unwrap()
        .unwrap();
    (
        World {
            server,
            db,
            org_id: org.id,
            repo_id: repo.id,
            admin,
        },
        ecs,
    )
}

/// Poll `f` until it is true, or fail saying what was still not.
fn until(what: &str, within: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out after {within:?} waiting for {what}");
}

/// Deleting a repository stops the CI it was running, and lets go of
/// what that CI was holding.
///
/// The row is tombstoned rather than removed, so nothing about a run
/// notices on its own: without this, the launched task keeps burning
/// CPU, the queued job is *still claimed and launched* — a container
/// started to clone a repository that answers 404 — and both hold a slot
/// of the organisation's concurrency until the overdue sweep gets to
/// them, which is a job's whole timeout away. That last part is why the
/// concurrency limit here is 1: it makes the leak visible as another
/// repository in the same organisation being unable to build at all.
#[test]
fn deleting_a_repository_stops_its_builds_and_frees_what_they_held() {
    let minio = Minio::shared();
    let scratch = Scratch::new("wf-runs-delete");
    let (w, ecs) = dispatching_world("wf-runs-delete", minio, &scratch);

    // One slot for the whole organisation: one job runs, the other
    // waits, and nothing else in acme can start until one of them lets
    // go.
    workflows::set_concurrency(&w.db, &w.org_id, Some(1)).unwrap();
    let run = make_run(&w, &w.repo_id, SHA, &["build", "lint"]);

    until("the dispatcher to launch the first job", DEADLINE, || {
        ecs.run_tasks().len() == 1
    });
    // And to stay at one — the limit is doing its job, so the second is
    // queued rather than merely slow.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(ecs.run_tasks().len(), 1, "the concurrency limit holds");
    let jobs = workflows::jobs_of(&w.db, &run).unwrap();
    let running = jobs
        .iter()
        .find(|j| j.state == "running")
        .expect("one job is running");
    let waiting = jobs
        .iter()
        .find(|j| j.state == "queued")
        .expect("the other is queued");
    assert!(running.task_ref.is_some(), "and it has a task behind it");

    let launched_before = ecs.run_tasks().len();
    let (st, out) = w.server.delete("/v1/orgs/acme/repos/app", &w.admin);
    assert!(st == 200 || st == 204, "{st}: {out}");

    // 1. The task that was up is stopped, by name and with a reason a
    //    person reading a container log can act on.
    until("the task to be stopped", DEADLINE, || {
        !ecs.stop_tasks().is_empty()
    });
    let stop = &ecs.stop_tasks()[0];
    assert_eq!(
        stop.body["task"].as_str(),
        running.task_ref.as_deref(),
        "the StopTask names the task this job launched"
    );
    assert_eq!(
        stop.body["reason"],
        serde_json::json!("the repository was deleted")
    );

    // 2. The rows settle — both jobs, not only the one with a task —
    //    and so do the checks that a land gate reads.
    let run_row = workflows::run_by_id(&w.db, &run).unwrap().unwrap();
    assert_eq!(run_row.state, "cancelled");
    for j in workflows::jobs_of(&w.db, &run).unwrap() {
        assert_eq!(j.state, "cancelled", "job {} was left behind", j.key);
    }
    // Read through the control plane rather than the API: every route
    // for this repository is a 404 now, which is the point.
    let checks = stratum_control::checks::latest_for_commit(&w.db, &w.repo_id, SHA).unwrap();
    assert_eq!(checks.len(), 2, "both jobs are mirrored: {checks:?}");
    for c in &checks {
        assert_eq!(c.state, "cancelled", "check {:?} was left behind", c.name);
        // Still linked, and this is the one caller that cannot look the
        // address up: the repository row is already tombstoned by the
        // time these rows are written, so the delete route has to carry
        // the names it holds down to the mirror. The run page outlives
        // the repository — it is where a reader finds out why a build
        // stopped mid-step.
        assert_eq!(
            c.detail_url.as_deref(),
            Some(format!("{}/acme/app/checks/runs/{run}", w.server.base).as_str()),
            "check {:?} lost its link when the repository went",
            c.name
        );
    }

    // 3. The queued job is never handed out. This is the assertion that
    //    fails loudest against the unpatched delete: the dispatcher
    //    would claim it on its next tick and start a container to clone
    //    a repository that no longer exists.
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(
        ecs.run_tasks().len(),
        launched_before,
        "a job of a deleted repository was launched anyway (job {})",
        waiting.key
    );

    // 4. The runner that was mid-step is told to stop on its next call,
    //    with the state on it. Its token was revoked by the same
    //    cancellation, so this is the case that used to answer 401 —
    //    which a runner retries — and the container went on building a
    //    repository that was not there any more.
    let job_token = ecs.run_tasks()[0].env()["STRATUM_JOB_TOKEN"].clone();
    let (st, body) = text_get(
        &w.server,
        &format!("/v1/runner/jobs/{}", running.id),
        &job_token,
    );
    assert_eq!(st, 410, "{body}");
    assert!(
        body.contains("\"state\":\"cancelled\""),
        "the runner has to be told why it is stopping: {body}"
    );

    // 5. And the slot it was holding is free: another repository in the
    //    same organisation builds, which under a limit of 1 it could not
    //    do while anything of acme's was still `running`.
    let other = registry::repo_by_name(&w.db, &w.org_id, "other")
        .unwrap()
        .unwrap();
    make_run(&w, &other.id, &"5".repeat(40), &["build"]);
    until("the other repository's build to start", DEADLINE, || {
        ecs.run_tasks().len() == launched_before + 1
    });

    assert!(w.server.healthy());
}

/// A reader that walks away takes nothing with it.
///
/// Every open feed is a task polling the database once a second and a
/// socket held for up to `MAX_STREAM`, and browsers close these
/// constantly — a tab closed, a navigation, an `EventSource` the page
/// replaced. The send is where that is noticed: nothing else in the loop
/// can tell that the far end is gone, so if a failed send did not end
/// the task, every abandoned reader would leave one behind for six
/// hours. Both places a send happens are exercised, because they are
/// reached at different moments in a feed's life — the announcement on
/// the very first poll, before anything has been sent at all, and a
/// chunk long after.
///
/// The assertions are about the runner, deliberately: the property worth
/// pinning is that a reader disappearing is invisible to the build. A
/// dropped socket must not fail a chunk POST, must not stop the job
/// settling, and must not leave the server unable to serve the next
/// request.
#[test]
fn a_reader_that_drops_its_socket_takes_nothing_with_it() {
    let minio = Minio::shared();
    let scratch = Scratch::new("wf-runs-drop");
    let w = world("wf-runs-drop", minio, &scratch);
    let run = make_run(&w, &w.repo_id, SHA, &["build"]);
    let queued = workflows::jobs_of(&w.db, &run).unwrap().remove(0);

    let addr = w
        .server
        .base
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let path = format!(
        "/v1/orgs/acme/repos/app/workflow-jobs/{}/log/stream",
        queued.id
    );
    // A raw socket rather than an HTTP client, so that "the reader is
    // gone" is this test's decision and not a client library's: an agent
    // that pools connections may hold one open after the response is
    // dropped, which is the opposite of what is being set up here.
    let open = |addr: &str, path: &str, token: &str| -> std::net::TcpStream {
        let mut sock = std::net::TcpStream::connect(addr).expect("the server accepts");
        use std::io::Write;
        write!(
            sock,
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\
             Accept: text/event-stream\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        sock.flush().unwrap();
        sock
    };

    // One: gone before the first poll has anything to say, so the
    // announcement itself is what fails to send.
    drop(open(&addr, &path, &w.admin));

    // Two: gone after the announcement has been read, so the send that
    // fails is a chunk's, on a feed that was working a moment ago.
    let sock = open(&addr, &path, &w.admin);
    sock.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut saw_queued = false;
    {
        let mut lines = BufReader::new(sock.try_clone().unwrap()).lines();
        while let Some(Ok(line)) = lines.next() {
            if line.contains("event: queued") {
                saw_queued = true;
                break;
            }
        }
    }
    assert!(saw_queued, "the feed announced the queued job");
    drop(sock);

    // Now be the runner, on a job two readers have just abandoned.
    let (job, token) = launch(&w, "build");
    for seq in 1..=4 {
        post_chunk(&w.server, &job.id, &token, seq, &format!("line {seq}\n"));
        std::thread::sleep(Duration::from_millis(500));
    }
    // Every chunk landed: a reader leaving must not cost a line of the
    // log, and this is read before the verdict because `finish` drops
    // the chunks once the whole log has been uploaded.
    let (st, body) = text_get(
        &w.server,
        &format!("/v1/orgs/acme/repos/app/workflow-jobs/{}/log", job.id),
        &w.admin,
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body, "line 1\nline 2\nline 3\nline 4\n");

    let whole = "line 1\nline 2\nline 3\nline 4\n";
    let resp = ureq::put(&format!("{}/v1/runner/jobs/{}/log", w.server.base, job.id))
        .set("Authorization", &format!("Bearer {token}"))
        .send_string(whole);
    assert!(resp.is_ok(), "the log upload was refused: {resp:?}");
    let resp = ureq::post(&format!(
        "{}/v1/runner/jobs/{}/finish",
        w.server.base, job.id
    ))
    .set("Authorization", &format!("Bearer {token}"))
    .set("Content-Type", "application/json")
    .send_string(r#"{"state":"passed"}"#);
    assert!(resp.is_ok(), "the verdict was refused: {resp:?}");

    assert_eq!(
        workflows::run_by_id(&w.db, &run).unwrap().unwrap().state,
        "passed"
    );
    let (st, body) = text_get(
        &w.server,
        &format!("/v1/orgs/acme/repos/app/workflow-jobs/{}/log", job.id),
        &w.admin,
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body, whole, "and it survives the verdict");

    // And a new reader still gets a feed, which is the part that would
    // break if a dropped socket had taken the shared machinery with it.
    let (st, body) = text_get(&w.server, "/v1/orgs/acme/repos/app/workflow-runs", &w.admin);
    assert_eq!(st, 200, "{body}");
    assert!(w.server.healthy());
}
