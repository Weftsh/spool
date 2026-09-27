//! Fault-injection end-to-end: the server behind a scriptable store
//! proxy. Injected 412s prove the CAS retry loops converge; injected
//! errors and outages prove every surface fails loudly and recovers —
//! the arms a healthy store never runs.

use std::time::{Duration, Instant};
use stratum_store::{LatencyModel, ObjectStore};
use stratum_testkit::closure::{assert_closed, check_bucket};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::{FaultProxy, Minio, Server};

fn spawn_server_with(store_url: &str, scratch: &Scratch, extra: &[(&str, String)]) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint("faults")
        .data_dir(scratch.path().join("data"))
        .envs(extra)
        .env("STRATUM_COMPACT_POLL_SECS", "86400")
        .start()
}

fn spawn_server(store_url: &str, scratch: &Scratch) -> Server {
    spawn_server_with(store_url, scratch, &[])
}

fn commit(
    server: &Server,
    token: &str,
    rp: &str,
    path: &str,
    content: &str,
) -> (u16, serde_json::Value) {
    server.req(
        "POST",
        &format!("{rp}/commits"),
        token,
        Some(serde_json::json!({
            "message": format!("add {path}"),
            "operations": [ { "op": "put", "path": path, "content": content } ],
        })),
    )
}

/// Returns the bucket's *direct* URL alongside the proxied one. Closure
/// is checked against MinIO itself: reading it through the fault proxy
/// would let an injected 503 read as a missing object, which is exactly
/// the failure the oracle is supposed to be able to name.
fn setup(bucket_hint: &str) -> (String, FaultProxy, Scratch) {
    let minio = Minio::shared();
    let bucket = minio.bucket(bucket_hint);
    let upstream = bucket
        .base_url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let proxy = FaultProxy::start(&upstream);
    let bucket_name = bucket.base_url.rsplit('/').next().unwrap().to_string();
    let scratch = Scratch::new(bucket_hint);
    // The proxy URL carries the bucket path just like the direct URL.
    let proxy = FaultProxy {
        url: format!("{}/{bucket_name}", proxy.url),
        handle: proxy.handle,
    };
    (bucket.base_url, proxy, scratch)
}

/// Injected 412s on the manifest swap: wire pushes and REST commits both
/// retry the CAS and land; the retry budget is finite (a storm of
/// conflicts becomes a clean error, not a hang).
#[test]
fn cas_conflicts_are_retried_then_bounded() {
    let (direct, proxy, scratch) = setup("faults-cas");
    let server = spawn_server(&proxy.url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    let (st, _) = commit(&server, &admin, rp, "seed.txt", "seed\n");
    assert_eq!(st, 201);

    // Wire push through two injected conflicts → retries → success.
    let url = server.authed_url(&admin, "acme", "app");
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&url, &clone);
    std::fs::write(clone.join("pushed.txt"), "x\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "conflicted push"]);
    proxy.handle.inject("PUT manifest.json", 2, 412);
    gitcli::git(&clone, &["push", "-q", "origin", "main"]);
    proxy.handle.clear();
    let (st, log) = server.req("GET", &format!("{rp}/log?limit=1"), &admin, None);
    assert_eq!(st, 200);
    let pushed = gitcli::git(&clone, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    assert_eq!(log["entries"][0]["commit"].as_str().unwrap(), pushed);

    // REST commit through injected conflicts → retries → 201.
    proxy.handle.inject("PUT manifest.json", 2, 412);
    let (st, out) = commit(&server, &admin, rp, "retry.txt", "r\n");
    proxy.handle.clear();
    assert_eq!(st, 201, "{out}");

    // A conflict storm exhausts the budget into a clean error.
    proxy.handle.inject("PUT manifest.json", 1000, 412);
    let (st, out) = commit(&server, &admin, rp, "storm.txt", "s\n");
    proxy.handle.clear();
    assert!(
        st == 409 || st == 500,
        "storm must end in a clean error, got {st}: {out}"
    );
    // And the repo still works afterwards.
    let (st, _) = commit(&server, &admin, rp, "after.txt", "ok\n");
    assert_eq!(st, 201);

    // A retry loop that republished the manifest it read before the
    // conflict would leave the winner's WAL segment unreferenced and the
    // loser's pointer naming an object the winner never wrote.
    assert_closed(&direct);
}

/// Store errors surface as clean 5xx API answers — reads, commits, and
/// refops fail loudly and recover the moment the store does.
#[test]
fn store_errors_surface_and_recover() {
    let (direct, proxy, scratch) = setup("faults-errors");
    let server = spawn_server(&proxy.url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    let (st, _) = commit(&server, &admin, rp, "f.txt", "v\n");
    assert_eq!(st, 201);

    // Reads: manifest GET errors (non-404, past the 5xx retry budget).
    proxy.handle.inject("GET manifest.json", 10, 503);
    let (st, _) = server.req("GET", &format!("{rp}/files/f.txt"), &admin, None);
    proxy.handle.clear();
    assert_eq!(st, 500, "store failure is a server error, not a 404");
    let (st, body) = server.req("GET", &format!("{rp}/files/f.txt"), &admin, None);
    assert_eq!(st, 200, "recovers with the store: {body}");

    // Commits: manifest load failure.
    proxy.handle.inject("GET manifest.json", 10, 503);
    let (st, _) = commit(&server, &admin, rp, "g.txt", "x\n");
    proxy.handle.clear();
    assert_eq!(st, 500);

    // Refops: reset against a broken store.
    proxy.handle.inject("GET manifest.json", 10, 503);
    let (st, _) = server.req(
        "POST",
        &format!("{rp}/reset"),
        &admin,
        Some(serde_json::json!({ "branch": "main", "to": "HEAD" })),
    );
    proxy.handle.clear();
    assert!(st >= 400, "reset against broken store: {st}");

    // Repo create: the empty-manifest PUT fails → clean error, and the
    // name is not burned (create works after recovery).
    proxy.handle.inject("PUT manifest.json", 10, 503);
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "unlucky" })),
    );
    proxy.handle.clear();
    assert_eq!(st, 500);

    // readyz goes not-ready while the store is down, and back.
    proxy.handle.set_down(true);
    let err = ureq::get(&format!("{}/readyz", server.base))
        .timeout(Duration::from_secs(20))
        .call()
        .unwrap_err();
    assert!(matches!(err, ureq::Error::Status(503, _)));
    proxy.handle.set_down(false);
    let ok = ureq::get(&format!("{}/readyz", server.base))
        .call()
        .unwrap();
    assert_eq!(ok.status(), 200);

    // A *stalled* store — one that accepts the connection and then never
    // answers — is the harder outage, and the one readiness exists for. A
    // slammed connection is an instant transport error; a stall is
    // indistinguishable from slowness until a read timeout fires, and the
    // store client's default is 300 s. Probing through that default made
    // /readyz hang for five minutes rather than answer 503, so no load
    // balancer ever evicted the task and the fleet degraded silently.
    // The probe now uses a short timeout of its own; this asserts the
    // answer arrives well inside the default it used to inherit.
    proxy.handle.set_stalled(true);
    let started = Instant::now();
    let err = ureq::get(&format!("{}/readyz", server.base))
        .timeout(Duration::from_secs(30))
        .call()
        .unwrap_err();
    let took = started.elapsed();
    proxy.handle.set_stalled(false);
    assert!(
        matches!(err, ureq::Error::Status(503, _)),
        "a stalled store must be reported, not waited out: {err}"
    );
    assert!(
        took < Duration::from_secs(20),
        "readyz answered in {took:?}; a probe that cannot fail fast is not a probe"
    );

    // And it recovers once the store answers again.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !server.healthy() {
        std::thread::sleep(Duration::from_millis(100));
    }
    let ok = ureq::get(&format!("{}/readyz", server.base))
        .call()
        .unwrap();
    assert_eq!(ok.status(), 200, "ready again once the store answers");

    // Several writes were interrupted partway through. I7 says the
    // manifest is published last, so a half-finished write must leave
    // orphaned data rather than a pointer to nothing.
    assert_closed(&direct);
}

/// An export job whose store breaks mid-run lands in state=failed with
/// the error recorded — and a later export succeeds.
#[test]
fn export_job_failure_is_recorded() {
    let (_direct, proxy, scratch) = setup("faults-export");
    let server = spawn_server(&proxy.url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    let (st, _) = commit(&server, &admin, rp, "f.txt", "v\n");
    assert_eq!(st, 201);

    proxy.handle.inject("GET", 500, 503);
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/export"),
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 202, "{out}");
    let job = out["job"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (st, s) = server.req("GET", &format!("{rp}/export/{job}"), &admin, None);
        assert_eq!(st, 200);
        match s["state"].as_str().unwrap() {
            "failed" => {
                assert!(s["error"].is_string(), "failure recorded: {s}");
                break;
            }
            "done" => panic!("export should have failed: {s}"),
            _ => {
                assert!(Instant::now() < deadline, "job stuck: {s}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    proxy.handle.clear();

    // Recovery: a fresh export completes.
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/export"),
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 202);
    let job = out["job"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, s) = server.req("GET", &format!("{rp}/export/{job}"), &admin, None);
        match s["state"].as_str().unwrap() {
            "done" => break,
            "failed" => panic!("recovered export failed: {s}"),
            _ => {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// Compaction against a broken store fails the job cleanly and the API
/// reports the error; serving is untouched.
#[test]
fn compaction_failure_fails_the_job_not_the_repo() {
    let (direct, proxy, scratch) = setup("faults-compact");
    let server = spawn_server(&proxy.url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    for i in 0..9 {
        let (st, _) = commit(
            &server,
            &admin,
            rp,
            &format!("f{}.txt", i % 3),
            &format!("v{i}\n"),
        );
        assert_eq!(st, 201);
    }
    // Compaction must read WAL segments; refuse those GETs.
    proxy.handle.inject("GET wal", 500, 503);
    let (st, out) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    proxy.handle.clear();
    assert_eq!(st, 500, "compaction failure surfaces: {out}");
    // Serving still works, and a retried compaction succeeds.
    let url = server.authed_url(&admin, "acme", "app");
    gitcli::clone_and_fsck(&url, &scratch.path().join("clone"));
    let (st, out) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    assert_eq!(st, 200, "{out}");

    // One compaction died partway and a second one finished. The abandoned
    // epoch may linger, but nothing live may point into it.
    assert_closed(&direct);
}

/// A clone whose pack stream breaks mid-flight gets an in-band sideband
/// error naming stratum — never a silently truncated pack.
#[test]
fn broken_stream_is_an_inband_error_not_truncation() {
    let (_direct, proxy, scratch) = setup("faults-stream");
    let server = spawn_server(&proxy.url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    for i in 0..3 {
        let (st, _) = commit(&server, &admin, rp, &format!("f{i}.txt"), "data\n");
        assert_eq!(st, 201);
    }
    // Break the WAL payload reads the clone plan streams from.
    proxy.handle.inject("GET wal", 500, 503);
    let url = server.authed_url(&admin, "acme", "app");
    let dest = scratch.path().join("broken-clone");
    let err = gitcli::git_expect_err(scratch.path(), &["clone", &url, dest.to_str().unwrap()])
        .expect("clone against broken store must fail");
    proxy.handle.clear();
    assert!(
        err.contains("weft") || err.contains("early EOF") || err.contains("remote"),
        "failure is loud: {err}"
    );
    // And afterwards the same clone succeeds and fscks clean.
    gitcli::clone_and_fsck(&url, &scratch.path().join("good-clone"));
}

/// A store outage during a GC sweep: the worker logs the failure per
/// repo, survives, and completes the sweep once the store returns.
#[test]
fn gc_sweep_survives_store_outage() {
    let (_direct, proxy, scratch) = setup("faults-gc");
    let server = spawn_server_with(
        &proxy.url,
        &scratch,
        &[
            ("STRATUM_GC_SECS", "1".into()),
            ("STRATUM_GC_GRACE_SECS", "0".into()),
        ],
    );
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "doomed" })),
    );
    let (st, _) = commit(
        &server,
        &admin,
        "/v1/orgs/acme/repos/doomed",
        "f.txt",
        "v\n",
    );
    assert_eq!(st, 201);
    let (_, repo) = server.req("GET", "/v1/orgs/acme/repos/doomed", &admin, None);
    let prefix = format!(
        "o/{}/r/{}/",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );
    let (st, _) = server.req("DELETE", "/v1/orgs/acme/repos/doomed", &admin, None);
    assert_eq!(st, 204);

    // Break the store while sweeps run; the worker must keep ticking.
    proxy.handle.set_down(true);
    std::thread::sleep(Duration::from_secs(3));
    proxy.handle.set_down(false);

    let store = stratum_store::ObjectStore::new(&proxy.url, stratum_store::LatencyModel::None);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if store.list(&prefix).unwrap_or_default().is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sweep never completed after recovery"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Control-plane writes while another session holds table locks: the
/// server's lock_timeout expires and every mutating handler answers its
/// database-error arm cleanly; reads and recovery are unaffected
/// (EXCLUSIVE table locks block writers, not readers).
#[test]
fn locked_database_surfaces_and_recovers() {
    let (_direct, proxy, scratch) = setup("faults-db");
    let server = spawn_server_with(
        &proxy.url,
        &scratch,
        &[("STRATUM_DB_LOCK_TIMEOUT_MS", "400".to_string())],
    );
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let (st, _) = commit(&server, &admin, "/v1/orgs/acme/repos/app", "f.txt", "v\n");
    assert_eq!(st, 201);

    // Take write-blocking locks from outside and hold them.
    let mut lock = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    let mut tx = lock.transaction().unwrap();
    tx.batch_execute("LOCK TABLE orgs, repos, tokens, jobs, audit_log IN EXCLUSIVE MODE")
        .unwrap();

    // Mutations fail loudly (after the server's lock timeout); metadata
    // reads still serve.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "unlucky" })),
    );
    assert!(st >= 500, "create against locked db: {st} {out}");
    let (st, _) = server.req("GET", "/v1/orgs/acme/repos", &admin, None);
    assert_eq!(st, 200, "reads keep working");

    // Release and verify recovery.
    drop(tx);
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "recovered" })),
    );
    assert_eq!(st, 201, "{out}");
}

/// Mirror sync through injected manifest-CAS conflicts: bounded retries
/// land the sync; an unbounded storm ends in the named error.
#[test]
fn sync_cas_conflicts_retry_then_bound() {
    let (direct, proxy, scratch) = setup("faults-synccas");
    let server = spawn_server(&proxy.url, &scratch);
    let admin = server.bootstrap_org("acme");

    // Local origin for a generic mirror.
    let origins = scratch.path().join("origins");
    let work = origins.join("work");
    let tip = gitcli::fixture_repo(&work, 5);
    let bare = origins.join("origin.git");
    gitcli::git(
        origins.as_path(),
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    gitcli::git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let origin_url = format!("file://{}", bare.display());
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(serde_json::json!({ "name": "m", "provider": "generic", "origin": origin_url })),
    );
    assert_eq!(st, 202, "{out}");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors/m/sync",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");

    // Advance origin; incremental sync retries through two conflicts.
    std::fs::write(work.join("adv.txt"), "x\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "advance"]);
    gitcli::git(&work, &["push", "-q", bare.to_str().unwrap(), "main:main"]);
    proxy.handle.inject("PUT manifest.json", 2, 412);
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors/m/sync",
        &admin,
        Some(serde_json::json!({})),
    );
    proxy.handle.clear();
    assert_eq!(st, 200, "retried sync lands: {out}");
    let _ = tip;

    // A storm exhausts the CAS budget into the named error.
    std::fs::write(work.join("adv2.txt"), "y\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "advance 2"]);
    gitcli::git(&work, &["push", "-q", bare.to_str().unwrap(), "main:main"]);
    proxy.handle.inject("PUT manifest.json", 1000, 412);
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors/m/sync",
        &admin,
        Some(serde_json::json!({})),
    );
    proxy.handle.clear();
    assert_eq!(st, 502, "{out}");
    assert!(out["error"].as_str().unwrap().contains("CAS race"), "{out}");
    // Recovery.
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors/m/sync",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200);

    // Three syncs, two of them fighting injected CAS conflicts, each
    // rewriting a whole mirrored layout.
    assert_closed(&direct);
}

/// Compaction that loses the manifest CAS to an injected conflict is a
/// LostRace outcome, not a failure.
#[test]
fn compaction_lost_race_via_injection() {
    let (direct, proxy, scratch) = setup("faults-lostrace");
    let server = spawn_server(&proxy.url, &scratch);
    let admin = server.bootstrap_org("acme");
    let (_, repo) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    for i in 0..9 {
        let (st, _) = commit(
            &server,
            &admin,
            rp,
            &format!("f{}.txt", i % 3),
            &format!("v{i}\n"),
        );
        assert_eq!(st, 201);
    }
    let store = ObjectStore::new(&proxy.url, LatencyModel::None);
    let store_direct = ObjectStore::new(&direct, LatencyModel::None);
    let prefix_no_slash = format!(
        "o/{}/r/{}/prod",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );

    // The pre-compaction truth, to compare the straddled state against.
    let url = server.authed_url(&admin, "acme", "app");
    let before = gitcli::clone_and_fsck(&url, &scratch.path().join("clone-before"));
    let (st, refs_before) = server.req("GET", &format!("{rp}/refs"), &admin, None);
    assert_eq!(st, 200);

    // The needle requires BOTH terms in the request line, so the
    // `locator.hdr` PUT sails through and only the manifest CAS is
    // rejected. `publish` swaps the hdr before the manifest, so this
    // leaves the two pointers straddled: hdr at the new epoch, manifest
    // still at the old one. That skew is legal under I15 — each pointer
    // must be internally consistent on its own — but "legal" and "tested"
    // are different things, and this is the one place where the single
    // commit point is really a two-step.
    proxy.handle.inject("PUT manifest.json", 1000, 412);
    let (st, out) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    proxy.handle.clear();
    assert_eq!(st, 200, "{out}");
    assert!(
        out["outcome"].as_str().unwrap().contains("LostRace"),
        "{out}"
    );

    // The rollback: losing the manifest CAS must put `locator.hdr` back
    // where it was, rather than leaving the read plane pointed at an
    // epoch the manifest never adopted. I15 tolerates the skew, but a
    // lost race is the *ordinary* outcome in a fleet, and the stale plane
    // is what receive-pack asks whether an object exists — so a later
    // push whose thin base the winner folded can be wrongly refused.
    //
    // The invariant is not "the header still exists" — this compaction
    // created it, so rolling back correctly means deleting it, and an
    // absent header is a state the reader already handles (`LayoutReader`
    // treats it as "no plane yet" and falls back). What must hold is that
    // the header never names an epoch the manifest has not adopted.
    let manifest: serde_json::Value = serde_json::from_slice(
        &store
            .get(&format!("{prefix_no_slash}/manifest.json"))
            .expect("manifest readable"),
    )
    .expect("manifest parses");
    match store.get(&format!("{prefix_no_slash}/locator.hdr")) {
        Err(e) => assert!(
            e.contains("HTTP 404"),
            "locator.hdr unreadable for a reason other than absence: {e}"
        ),
        Ok(hdr) => {
            let elen = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
            let hdr_epoch = String::from_utf8(hdr[6..6 + elen].to_vec()).expect("epoch is utf-8");
            assert_eq!(
                hdr_epoch,
                manifest["epoch"].as_str().unwrap(),
                "a lost manifest CAS left locator.hdr pointing at an epoch \
                 the manifest never adopted"
            );
        }
    }

    // What a reader sees while the pointers are straddled. Serving must
    // still be driven entirely by the manifest, so the clone is the
    // pre-compaction tip and nothing points into the orphaned epoch.
    let after = gitcli::clone_and_fsck(&url, &scratch.path().join("clone-straddled"));
    assert_eq!(after, before, "a lost compaction must not move the tip");
    let (st, refs_after) = server.req("GET", &format!("{rp}/refs"), &admin, None);
    assert_eq!(st, 200);
    assert_eq!(refs_after, refs_before, "refs must match the old manifest");

    // A point read resolves through the locator plane — the pointer that
    // *did* advance. This is the assertion that catches a reader mixing
    // the locator's epoch with the manifest's (I15).
    let (st, body) = server.req("GET", &format!("{rp}/files/f0.txt"), &admin, None);
    assert_eq!(st, 200, "point read across the pointer skew: {body}");

    // The store right after a lost race is the state the oracle exists
    // for: an orphaned epoch on disk, both pointers rolled back onto the
    // old one, and every key either of them names still durable. Checked
    // here rather than only at the end of the test, because the GC below
    // erases the evidence.
    let report =
        check_bucket(&store_direct).expect("a lost compaction race must leave the store closed");
    assert_eq!(report.repos, 1, "{report}");

    // And the skew converges: with the fault cleared, compaction re-runs
    // and lands, leaving a clone that still fscks at the same tip.
    let (st, out) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    assert_eq!(st, 200, "{out}");
    assert!(
        out["outcome"].as_str().unwrap().contains("Compacted"),
        "compaction must converge once the fault clears: {out}"
    );
    let healed = gitcli::clone_and_fsck(&url, &scratch.path().join("clone-healed"));
    assert_eq!(healed, before, "compaction must preserve the tip");
    let (st, refs_healed) = server.req("GET", &format!("{rp}/refs"), &admin, None);
    assert_eq!(st, 200);
    assert_eq!(refs_healed, refs_before, "compaction must preserve refs");

    // And the epoch the lost race orphaned is reclaimable, not a leak.
    //
    // Worth pinning because the obvious reading of the code says
    // otherwise: `gc::live_epochs` deliberately treats the epoch named by
    // `locator.hdr` as live in its own right (I15), so while the pointers
    // are straddled the orphan is protected — and it is easy to conclude
    // from that alone that every lost race leaks an epoch forever. It
    // does not. The next *successful* compaction moves both pointers to a
    // third epoch, at which point nothing references the orphan and the
    // ordinary sweep takes it. The leak is bounded by one epoch per repo,
    // and only until the next compaction lands.
    let prefix = format!(
        "o/{}/r/{}/prod/",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );
    let epochs = |store: &ObjectStore| -> std::collections::BTreeSet<String> {
        store
            .list(&prefix)
            .unwrap()
            .into_iter()
            .filter_map(|(k, _)| {
                k.strip_prefix(&prefix)
                    .and_then(|rest| rest.split_once('/'))
                    .map(|(epoch, _)| epoch.to_string())
            })
            .collect()
    };
    assert!(
        epochs(&store).len() >= 2,
        "the lost race should have left an orphaned epoch behind"
    );
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/gc"),
        &admin,
        Some(serde_json::json!({ "grace_secs": 0 })),
    );
    assert_eq!(st, 200, "{out}");
    let after_gc = epochs(&store);
    assert_eq!(
        after_gc.len(),
        1,
        "the orphaned epoch outlived a sweep it was no longer referenced by: {after_gc:?}"
    );

    // ...and the sweep that reclaimed it took nothing the survivor needs.
    assert_closed(&direct);
}

/// A corrupted WAL segment: exports fail with the byte-count mismatch
/// (materialize is fsck-gated, never silently wrong).
#[test]
fn corrupted_segment_fails_export_loudly() {
    let (direct, proxy, scratch) = setup("faults-corrupt");
    let server = spawn_server(&proxy.url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    let (st, _) = commit(&server, &admin, rp, "f.txt", "content\n");
    assert_eq!(st, 201);

    // Truncate the WAL segment behind the manifest's back.
    let store = stratum_store::ObjectStore::new(&proxy.url, stratum_store::LatencyModel::None);
    let store_direct = ObjectStore::new(&direct, LatencyModel::None);
    let (_, repo) = server.req("GET", rp, &admin, None);
    let prefix = format!(
        "o/{}/r/{}/prod/",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );
    let seg = store
        .list(&prefix)
        .unwrap()
        .into_iter()
        .find(|(k, _)| k.contains("/wal/") && k.ends_with(".seg"))
        .expect("wal segment")
        .0;
    store
        .put(&seg, b"short", stratum_store::PutCond::None)
        .unwrap();

    let (st, out) = server.req(
        "POST",
        &format!("{rp}/export"),
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 202, "{out}");
    let job = out["job"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, s) = server.req("GET", &format!("{rp}/export/{job}"), &admin, None);
        match s["state"].as_str().unwrap() {
            "failed" => {
                let err = s["error"].as_str().unwrap();
                assert!(err.contains("expected"), "byte mismatch named: {err}");
                break;
            }
            "done" => panic!("corrupted export must fail: {s}"),
            _ => {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }

    // The store really is broken here, deliberately, so this is the one
    // place the oracle is asserted to *fire*. A WAL key is its payload's
    // sha1 (I10), so a segment rewritten under its own name is caught by
    // the name alone — and the declared length no longer matches either.
    let v = check_bucket(&store_direct).expect_err("a truncated WAL segment is not closed");
    assert!(
        v.iter().any(|v| v.invariant == "I10") && v.iter().any(|v| v.invariant == "I6"),
        "{v:#?}"
    );
    assert!(
        v.iter()
            .all(|v| v.prefix.starts_with(&prefix[..prefix.len() - 1])),
        "{v:#?}"
    );
}

/// Wire pushes under manifest-CAS storms and PUT failures: the retry
/// budget exhausts into the documented ng report; a hard PUT error maps
/// through cleanly.
#[test]
fn wire_push_cas_storm_and_put_failure() {
    let (direct, proxy, scratch) = setup("faults-wirecas");
    let server = spawn_server(&proxy.url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let (st, _) = commit(
        &server,
        &admin,
        "/v1/orgs/acme/repos/app",
        "seed.txt",
        "s\n",
    );
    assert_eq!(st, 201);
    let url = server.authed_url(&admin, "acme", "app");
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&url, &clone);

    // CAS storm: the push retries CAS_RETRIES times then reports ng.
    std::fs::write(clone.join("a.txt"), "a\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "stormy"]);
    proxy.handle.inject("PUT manifest.json", 1000, 412);
    let err = gitcli::git_expect_err(&clone, &["push", "-q", "origin", "main"]).unwrap();
    proxy.handle.clear();
    assert!(err.contains("concurrent pushes"), "{err}");

    // Hard PUT failure (503) on the manifest maps to a clean rejection.
    proxy.handle.inject("PUT manifest.json", 10, 503);
    let err = gitcli::git_expect_err(&clone, &["push", "-q", "origin", "main"]).unwrap();
    proxy.handle.clear();
    assert!(
        err.contains("PUT") || err.contains("rejected") || err.contains("failed"),
        "{err}"
    );

    // Recovery: the same push lands.
    gitcli::git(&clone, &["push", "-q", "origin", "main"]);

    // Two pushes uploaded their pack and then failed to publish it. The
    // third landed. Whatever the first two left behind must be orphaned
    // data, never a manifest entry.
    assert_closed(&direct);
}

/// Faults on the remaining ops surfaces: receive-pack advertisement with
/// a broken store, and gc_now against an outage — both loud, both
/// recoverable.
#[test]
fn advert_and_gc_fault_answers() {
    let (_direct, proxy, scratch) = setup("faults-misc");
    let server = spawn_server(&proxy.url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let (st, _) = commit(&server, &admin, "/v1/orgs/acme/repos/app", "f.txt", "v\n");
    assert_eq!(st, 201);

    // Receive advert needs the manifest; break it → 5xx, then recover.
    proxy.handle.inject("GET manifest.json", 10, 503);
    let resp = ureq::get(&format!(
        "{}/acme/app/info/refs?service=git-receive-pack",
        server.base
    ))
    .set("Authorization", &format!("Bearer {admin}"))
    .call();
    proxy.handle.clear();
    assert!(
        matches!(resp, Err(ureq::Error::Status(c, _)) if c >= 500),
        "{resp:?}"
    );
    let ok = ureq::get(&format!(
        "{}/acme/app/info/refs?service=git-receive-pack",
        server.base
    ))
    .set("Authorization", &format!("Bearer {admin}"))
    .call()
    .unwrap();
    assert_eq!(ok.status(), 200);

    // gc_now with the store down answers 502.
    proxy.handle.set_down(true);
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/repos/app/gc",
        &admin,
        Some(serde_json::json!({ "grace_secs": 0 })),
    );
    proxy.handle.set_down(false);
    assert_eq!(st, 502);
}

/// The control database connection dies (backend terminated — the shape
/// of a Postgres restart or failover): the server re-establishes it on
/// the next call, transparently. The database disappearing entirely
/// (dropped with force) makes every surface fail loudly — and readiness
/// name the database — instead of hanging.
#[test]
fn db_connection_loss_reconnects_and_dropped_db_fails_loudly() {
    let (_direct, proxy, scratch) = setup("faults-dbconn");
    let server = spawn_server(&proxy.url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );

    // Terminate the server's backend from a second session.
    let mut ctl = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    let dbname: String = ctl
        .query_one("SELECT current_database()", &[])
        .unwrap()
        .get(0);
    ctl.query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND pid <> pg_backend_pid()",
        &[&dbname],
    )
    .unwrap();

    // The very next authenticated request reconnects and serves.
    let (st, out) = server.req("GET", "/v1/orgs/acme/repos", &admin, None);
    assert_eq!(st, 200, "reconnect after backend termination: {out}");

    // Now the database itself is dropped (force-disconnects the server).
    let admin_url = {
        let (head, _) = server.db_url.rsplit_once('/').unwrap();
        format!("{head}/postgres")
    };
    drop(ctl);
    let mut maint = postgres::Client::connect(&admin_url, postgres::NoTls).unwrap();
    maint
        .batch_execute(&format!("DROP DATABASE \"{dbname}\" WITH (FORCE)"))
        .unwrap();

    // Mutations and reads answer their database-error arms; nothing hangs.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "orphaned" })),
    );
    assert!(st >= 500, "create with dropped db: {st} {out}");
    let (st, _) = server.req("GET", "/v1/orgs/acme/repos", &admin, None);
    assert!(st >= 401, "read with dropped db must fail: {st}");

    // Readiness reports the database as the failing dependency.
    let err = ureq::get(&format!("{}/readyz", server.base))
        .timeout(Duration::from_secs(15))
        .call()
        .unwrap_err();
    match err {
        ureq::Error::Status(503, r) => {
            let body = r.into_string().unwrap();
            assert!(body.contains("db"), "readyz names the db: {body}");
        }
        other => panic!("expected 503, got {other:?}"),
    }
}

/// Poll `f` until it answers true, or fail with `what`. Every wait in
/// this file is on the observable the next assertion depends on — a
/// counter the proxy really incremented, a row the server really wrote —
/// never on a bare sleep, which passes or fails with the load on the
/// machine.
fn wait_until(what: &str, within: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// A background job that cannot be enqueued must not fail the write that
/// asked for it.
///
/// The compactor and the CDN packer are enqueued *after* a commit has
/// landed in the object store and been audited, and neither is part of
/// the caller's answer: a fold that never runs makes reads slower, a
/// pack that never rebuilds makes clones fall back to the inline path,
/// and the next write re-enqueues both. Failing the commit over a queue
/// insert would throw away durable work the caller cannot see was kept —
/// so they retry, and the history has the commit twice.
#[test]
fn an_unenqueueable_job_does_not_fail_the_write_that_asked_for_it() {
    let (_direct, proxy, scratch) = setup("faults-enqueue");
    let server = spawn_server_with(
        &proxy.url,
        &scratch,
        &[
            ("STRATUM_DB_LOCK_TIMEOUT_MS", "400".to_string()),
            // Nothing may claim a job out from under the assertions.
            ("STRATUM_CDNPACK_POLL_SECS", "86400".to_string()),
        ],
    );
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    let (st, _) = commit(&server, &admin, rp, "seed.txt", "seed\n");
    assert_eq!(st, 201);

    // Clear the queue first: `enqueue_unique` is a no-op while a job for
    // the repo is already active, so a leftover row from the seed commit
    // would make the next assertion pass without the failure arm running
    // at all.
    let mut ctl = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    ctl.execute("DELETE FROM jobs", &[]).unwrap();
    let jobs = |c: &mut postgres::Client| -> i64 {
        c.query_one("SELECT count(*) FROM jobs", &[])
            .unwrap()
            .get(0)
    };
    assert_eq!(jobs(&mut ctl), 0);

    // Hold the queue table shut. `ACCESS EXCLUSIVE` and not `EXCLUSIVE`:
    // the insert has to block, and `EXCLUSIVE` lets it through.
    let mut lock = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    let mut tx = lock.transaction().unwrap();
    tx.batch_execute("LOCK TABLE jobs IN ACCESS EXCLUSIVE MODE")
        .unwrap();

    let (st, out) = commit(&server, &admin, rp, "queued.txt", "q\n");
    assert_eq!(
        st, 201,
        "an unqueueable fold must not fail the commit: {out}"
    );
    let landed = out["commit"].as_str().unwrap().to_string();

    drop(tx);
    drop(lock);

    // The write is real and readable — the answer was not optimism.
    let (st, log) = server.req("GET", &format!("{rp}/log?limit=1"), &admin, None);
    assert_eq!(st, 200);
    assert_eq!(log["entries"][0]["commit"].as_str().unwrap(), landed);
    // And nothing was queued for it: both enqueues really did fail, so
    // the 201 above was the failure arm being tolerated, not avoided.
    assert_eq!(
        jobs(&mut ctl),
        0,
        "the enqueues were supposed to fail against a locked table"
    );

    // With the table free the very same call queues every kind a write
    // fans out to, which is what makes the count above a measurement
    // rather than a coincidence.
    //
    // The list is spelled out rather than counted on purpose: a new
    // fan-out kind has to be added here deliberately, and the reason is
    // the property this test is about. `contrib` joined `compact` and
    // `cdnpack` with the contribution walker, and it is enqueued with
    // exactly the same discipline — a queue that will not take the row
    // warns and the write still succeeds, because a push that happened
    // must not be un-pushed by a bookkeeping failure.
    //
    // `sitepublish` joined them with static site hosting, and holds to
    // the same rule: `workers::sitepublish::enqueue` warns and returns
    // on a queue that refuses the row, so a push whose site failed to
    // be scheduled is still a push. The zero-count assertion above is
    // what proves that, and it covers this kind too.
    let (st, _) = commit(&server, &admin, rp, "after.txt", "ok\n");
    assert_eq!(st, 201);
    let mut kinds: Vec<String> = ctl
        .query("SELECT kind FROM jobs", &[])
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        vec![
            "cdnpack".to_string(),
            "compact".to_string(),
            "contrib".to_string(),
            "sitepublish".to_string()
        ]
    );
}

/// The audit shipper against a store that applies the write and then
/// says it failed.
///
/// This is the fault an outage cannot simulate and the reason the shard
/// PUT is create-only: the batch lands, the shipper is told 503, the
/// cursor stays put, and the next pass re-ships *the same key*. If that
/// retry could overwrite, the immutable record would have been rewritten
/// by a fault nobody could observe. It must instead lose the condition
/// and be treated as done.
#[test]
fn an_audit_shard_that_lands_under_a_lying_store_is_never_rewritten() {
    let (direct, proxy, scratch) = setup("faults-shipper");
    // The shipper starts off, so the rows below accumulate unshipped and
    // the batch the fault meets is a known, closed set.
    let mut server = spawn_server_with(
        &proxy.url,
        &scratch,
        &[("STRATUM_AUDIT_SHIP_SECS", "0".to_string())],
    );
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    for i in 0..3 {
        let (st, _) = commit(&server, &admin, rp, &format!("f{i}.txt"), "v\n");
        assert_eq!(st, 201);
    }

    let mut ctl = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    let org_id: String = ctl
        .query_one("SELECT id FROM orgs WHERE name = 'acme'", &[])
        .unwrap()
        .get(0);
    let cursor_key = format!("audit_cursor:{org_id}");
    let cursor = move |c: &mut postgres::Client| -> Option<String> {
        c.query_opt("SELECT value FROM meta WHERE key = $1", &[&cursor_key])
            .unwrap()
            .map(|r| r.get(0))
    };
    assert_eq!(cursor(&mut ctl), None, "the shipper was supposed to be off");

    // Every shard PUT now lands upstream and answers 503 anyway.
    let store = ObjectStore::new(&direct, LatencyModel::None);
    let shard_prefix = format!("o/{org_id}/audit/");
    proxy.handle.reset_stats();
    proxy.handle.set_plan(
        stratum_testkit::faultproxy::FaultPlan::new(1).with(
            stratum_testkit::faultproxy::FaultRule::new(
                stratum_testkit::faultproxy::Fault::ErrorAfter,
                1.0,
            )
            .only_keys([shard_prefix.clone()])
            .only_methods(["PUT"]),
        ),
    );
    server.restart_with(&[("STRATUM_AUDIT_SHIP_SECS", "1".to_string())]);

    // Wait for the **shard**, not for the fault counter. The counter is
    // the proxy's record that it decided to fail a request, and the
    // upstream write it forwards is not necessarily finished when the
    // decision is recorded — so on a loaded runner the list below ran
    // between the two and found nothing, which read as "the write never
    // landed" and is the one thing this test is here to disprove. CI
    // was red on `left: 0, right: 1` where a development machine was
    // green. Wait on the observable the next line depends on.
    let mut shards = Vec::new();
    wait_until(
        "the shard PUT to land and be denied",
        Duration::from_secs(30),
        || {
            if proxy
                .handle
                .stats()
                .count(stratum_testkit::faultproxy::Fault::ErrorAfter)
                < 1
            {
                return false;
            }
            shards = store.list(&shard_prefix).unwrap();
            !shards.is_empty()
        },
    );

    // The write landed; the shipper does not believe it did.
    assert_eq!(shards.len(), 1, "{shards:?}");
    let key = shards[0].0.clone();
    let landed = store.get(&key).unwrap();
    assert!(!landed.is_empty());
    assert_eq!(
        cursor(&mut ctl),
        None,
        "a shipper told 503 must not advance the cursor"
    );

    // Heal: the retry meets its own object and the create-only condition
    // refuses it, which is the whole point — the pass completes without
    // rewriting a byte.
    proxy.handle.heal();
    wait_until(
        "the cursor to advance once the store is honest",
        Duration::from_secs(30),
        || cursor(&mut ctl).is_some(),
    );
    let after = store.list(&shard_prefix).unwrap();
    assert_eq!(after.len(), 1, "the retry wrote a second shard: {after:?}");
    assert_eq!(
        store.get(&key).unwrap(),
        landed,
        "the retry rewrote an immutable shard"
    );

    // The shard is the record: every audit seq up to the cursor, once,
    // in order.
    let shipped: Vec<i64> = String::from_utf8(landed)
        .unwrap()
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["seq"]
                .as_i64()
                .unwrap()
        })
        .collect();
    assert!(shipped.windows(2).all(|w| w[1] > w[0]), "{shipped:?}");
    let cursor_now: i64 = cursor(&mut ctl).unwrap().parse().unwrap();
    assert_eq!(*shipped.last().unwrap(), cursor_now);
    let recorded: Vec<i64> = ctl
        .query(
            "SELECT seq FROM audit_log WHERE org_id = $1 AND seq <= $2 ORDER BY seq",
            &[&org_id, &cursor_now],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(shipped, recorded);

    assert!(server.healthy(), "the server must survive its own shipper");
}

/// The audit shipper against a database it cannot read.
///
/// A pass that cannot query the log is a failed pass, not an empty one.
/// The difference is the cursor: an empty pass advances nothing because
/// there was nothing, and a failed pass must advance nothing because it
/// does not know what there was. Advancing on the error would skip
/// exactly the rows it could not read, and the durable record — the
/// product — would have a hole in it that nothing ever fills.
#[test]
fn a_shipper_that_cannot_read_the_log_ships_nothing_and_loses_nothing() {
    let (direct, proxy, scratch) = setup("faults-shipperdb");
    let server = spawn_server_with(
        &proxy.url,
        &scratch,
        &[
            ("STRATUM_AUDIT_SHIP_SECS", "1".to_string()),
            ("STRATUM_DB_LOCK_TIMEOUT_MS", "300".to_string()),
        ],
    );
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );

    let mut ctl = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    let org_id: String = ctl
        .query_one("SELECT id FROM orgs WHERE name = 'acme'", &[])
        .unwrap()
        .get(0);
    let store = ObjectStore::new(&direct, LatencyModel::None);
    let shard_prefix = format!("o/{org_id}/audit/");
    // Let the shipper get going normally first, so what follows is the
    // lock stopping a working shipper rather than one that never ran.
    wait_until(
        "the first ordinary shipping pass",
        Duration::from_secs(30),
        || !store.list(&shard_prefix).unwrap().is_empty(),
    );
    let before = store.list(&shard_prefix).unwrap().len();
    let cursor_before: String = ctl
        .query_one(
            "SELECT value FROM meta WHERE key = $1",
            &[&format!("audit_cursor:{org_id}")],
        )
        .unwrap()
        .get(0);

    // Shut the log to readers. `ACCESS EXCLUSIVE` is the one mode that
    // blocks a plain SELECT, so the shipper's query hits the server's
    // lock timeout and the pass fails where it reads.
    let mut lock = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    let mut tx = lock.transaction().unwrap();
    tx.batch_execute("LOCK TABLE audit_log IN ACCESS EXCLUSIVE MODE")
        .unwrap();
    // Several passes' worth: each one claims the worker lock, reads the
    // org list, and then fails on the log itself.
    std::thread::sleep(Duration::from_secs(4));
    // Nothing shipped, and nothing skipped.
    assert_eq!(store.list(&shard_prefix).unwrap().len(), before);
    // Metadata reads that do not touch the log keep serving throughout.
    let (st, _) = server.req("GET", "/v1/orgs/acme/repos", &admin, None);
    assert_eq!(
        st, 200,
        "an unreadable audit log must not take the API down"
    );
    drop(tx);
    drop(lock);

    // The rows written while the log was shut are shipped once it opens:
    // a failed pass costs a delay, never a gap.
    let (st, _) = commit(
        &server,
        &admin,
        "/v1/orgs/acme/repos/app",
        "after.txt",
        "ok\n",
    );
    assert_eq!(st, 201);
    wait_until(
        "the cursor to move past the stall",
        Duration::from_secs(30),
        || {
            let now: String = ctl
                .query_one(
                    "SELECT value FROM meta WHERE key = $1",
                    &[&format!("audit_cursor:{org_id}")],
                )
                .unwrap()
                .get(0);
            now.parse::<i64>().unwrap() > cursor_before.parse::<i64>().unwrap()
        },
    );
    assert!(server.healthy());
}

/// A store that refuses every read fails the queued background jobs —
/// the fold, the CDN pack, the authorship walk — and not the server, and
/// a notification whose OWNERS file cannot be read still reaches the
/// people the change already involves.
///
/// These are the arms a healthy store never runs, and until now they
/// were ledgered as unreachable ("needs a store fault landing in that
/// window"). The window is easy to hit on purpose: stop every worker,
/// queue the work with real writes, refuse the reads, start the workers.
/// Each job then meets the fault on its own clock and is asserted on the
/// row it leaves behind — `failed`, with the store's refusal as the
/// reason — rather than on a log line. The writes that queued them are
/// untouched: once the store answers again, a fresh commit's fold
/// completes and the pack rebuilds.
///
/// The notifier is the odd one out on purpose. An unreadable OWNERS file
/// is treated as "nobody is required", so the author is still told about
/// a comment while the owner the file would have named is not; when the
/// store is back, the next comment reaches both. Failing the whole
/// notification over the reviewers half would have left the author
/// unnotified for as long as the store was down.
#[test]
fn a_store_that_refuses_every_read_fails_the_queued_jobs_and_not_the_server() {
    use stratum_control::{jobs, registry, ControlDb};
    use stratum_testkit::browser::Browser;
    use stratum_testkit::mailbox::Mailbox;
    const PASSWORD: &str = "a long enough password";

    let (_direct, proxy, scratch) = setup("faults-workers");
    let mailbox = Mailbox::temp("faults-workers");
    let stopped: Vec<(&str, String)> = [
        "STRATUM_COMPACT_POLL_SECS",
        "STRATUM_CDNPACK_POLL_SECS",
        "STRATUM_CONTRIB_POLL_SECS",
        "STRATUM_NOTIFY_POLL_SECS",
    ]
    .iter()
    .map(|k| (*k, "0".to_string()))
    .collect();
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &proxy.url)
        .db_hint("faults")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_PUBLIC_URL", "http://stratum.test:9999")
        .envs(&stopped);
    for (k, v) in mailbox.env() {
        b = b.env(k, v);
    }
    let mut server = b.start();
    server.bootstrap_org("acme");
    for (email, name, role) in [
        ("ada@acme.test", "Ada", "owner"),
        ("bo@acme.test", "Bo", "member"),
        ("cy@acme.test", "Cy", "member"),
    ] {
        server
            .admin(&[
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
                role,
            ])
            .unwrap_or_else(|e| panic!("user-create {email}: {e}"));
    }
    // The browsers borrow the server, and the restart below needs it
    // back; sessions live in the database, so signing in again after
    // the restart is the same person.
    let key = {
        let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
        let mut bo = Browser::signed_in(&server, "bo@acme.test", PASSWORD);
        let (st, _) = ada.req(
            "POST",
            "/v1/orgs/acme/repos",
            Some(serde_json::json!({ "name": "app" })),
        );
        assert_eq!(st, 201);
        // Cy owns everything; a readable OWNERS file would name Cy on every
        // event about this change.
        let (st, out) = ada.req(
            "POST",
            "/v1/orgs/acme/repos/app/commits",
            Some(serde_json::json!({
                "message": "seed",
                "operations": [
                    {"op": "put", "path": "OWNERS", "content": "cy@acme.test\n"},
                    {"op": "put", "path": "readme", "content": "hello\n"},
                ],
            })),
        );
        assert_eq!(st, 201, "{out}");
        // Branched from main so the change has a trunk to land on. The
        // patchset then *deletes* OWNERS: who is told about a change is
        // decided by the rules on the branch it lands on, not by the
        // files the patchset would like to be judged against, so Cy is
        // still the one who hears.
        let (st, out) = ada.req(
            "POST",
            "/v1/orgs/acme/repos/app/branches",
            Some(serde_json::json!({ "name": "review/one", "from": "main" })),
        );
        assert_eq!(st, 201, "{out}");
        let (st, out) = ada.req(
            "POST",
            "/v1/orgs/acme/repos/app/commits",
            Some(serde_json::json!({
                "message": "a change\n\nChange-Id: I00000000000000000000000000000000000000fa\n",
                "branch": "review/one",
                "operations": [
                    {"op": "delete", "path": "OWNERS"},
                    {"op": "put", "path": "readme", "content": "hello again\n"},
                ],
            })),
        );
        assert_eq!(st, 201, "{out}");
        let (st, out) = ada.req(
            "POST",
            "/v1/orgs/acme/repos/app/changes",
            Some(serde_json::json!({ "from": "review/one" })),
        );
        assert_eq!(st, 201, "{out}");
        let key = out["change"]["key"].as_str().unwrap().to_string();
        let (st, out) = bo.req(
            "POST",
            &format!("/v1/orgs/acme/repos/app/changes/{key}/comments"),
            Some(serde_json::json!({ "body": "one question" })),
        );
        assert_eq!(st, 201, "{out}");
        key
    };

    // Everything above queued work nobody has run. Now the store refuses
    // every read of a manifest — the first thing each of those jobs
    // does — and the workers are let at the queue.
    let db = ControlDb::open(&server.db_url).expect("the control plane");
    let org_id = registry::org_by_name(&db, "acme").unwrap().unwrap().id;
    let repo_id = registry::repo_by_name(&db, &org_id, "app")
        .unwrap()
        .unwrap()
        .id;
    for kind in ["compact", "cdnpack", "contrib"] {
        let job = jobs::latest_for_repo(&db, &org_id, &repo_id, kind)
            .unwrap()
            .unwrap_or_else(|| panic!("a {kind} job was queued by the writes"));
        assert_eq!(job.state, "queued", "{kind}: {job:?}");
    }
    proxy.handle.inject("GET manifest.json", 1_000_000, 503);
    let running: Vec<(&str, String)> = stopped.iter().map(|(k, _)| (*k, "1".to_string())).collect();
    server.restart_with(&running);

    for kind in ["compact", "cdnpack", "contrib"] {
        let mut job = None;
        wait_until(
            &format!("the {kind} job to fail"),
            Duration::from_secs(30),
            || {
                job = jobs::latest_for_repo(&db, &org_id, &repo_id, kind)
                    .unwrap()
                    .filter(|j| j.state == "failed");
                job.is_some()
            },
        );
        let error = job.unwrap().error.unwrap_or_default();
        assert!(
            error.contains("503"),
            "{kind}: the job records the store's refusal, not a rewording of it: {error}"
        );
    }
    // Ada hears about Bo's comment; Cy, whom the unreadable OWNERS file
    // would have named, does not.
    let got = mailbox.wait_for("ada@acme.test", Duration::from_secs(30));
    assert!(got.text.contains(&key), "{got:?}");
    assert!(
        mailbox.to("cy@acme.test").is_empty(),
        "an owner the store could not name was told anyway: {:?}",
        mailbox.to("cy@acme.test")
    );

    // The store answers again: the next write's fold completes, and the
    // next comment reaches the owner too.
    proxy.handle.clear();
    assert!(server.healthy());
    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    let mut bo = Browser::signed_in(&server, "bo@acme.test", PASSWORD);
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/commits",
        Some(serde_json::json!({
            "message": "after",
            "operations": [{"op": "put", "path": "readme", "content": "hello, again\n"}],
        })),
    );
    assert_eq!(st, 201, "{out}");
    for kind in ["compact", "cdnpack", "contrib"] {
        wait_until(
            &format!("the next {kind} job to complete"),
            Duration::from_secs(30),
            || {
                jobs::latest_for_repo(&db, &org_id, &repo_id, kind)
                    .unwrap()
                    .is_some_and(|j| j.state == "done")
            },
        );
    }
    let (st, out) = bo.req(
        "POST",
        &format!("/v1/orgs/acme/repos/app/changes/{key}/comments"),
        Some(serde_json::json!({ "body": "and another" })),
    );
    assert_eq!(st, 201, "{out}");
    let got = mailbox.wait_for("cy@acme.test", Duration::from_secs(30));
    assert!(got.text.contains(&key), "{got:?}");
    assert_eq!(mailbox.to("ada@acme.test").len(), 2);
    assert!(server.healthy());
}
