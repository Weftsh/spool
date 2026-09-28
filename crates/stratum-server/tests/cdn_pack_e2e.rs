//! CDN-offloaded clones end to end: a REAL `git clone` that fetches the
//! bulk of the repo from a CDN URL (git's `packfile-uri`) and tops up the
//! remainder inline from the server.
//!
//! The properties here were each established by driving stock git against
//! a real server before the feature was designed, and this suite pins
//! them:
//!   * an opted-in client (`fetch.uriprotocols`) really does fetch the
//!     advertised URL — proven by the CDN's own access log, not by a
//!     green clone alone;
//!   * a client that has NOT opted in clones normally with zero CDN hits
//!     (the whole existing fleet must be unaffected);
//!   * a stale pack is safe: the client tops up and still lands fsck-clean
//!     at the true tip;
//!   * a pack that is missing is never advertised, because an opted-in
//!     clone would abort rather than fall back.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};
use stratum_testkit::closure::assert_closed;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::minio::{ROOT_PASSWORD, ROOT_USER};
use stratum_testkit::Minio;

struct Server {
    child: Child,
    base: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A stand-in for CloudFront: serves the advertised pack over plain HTTP
/// and records every path it was asked for. That log is the evidence the
/// client really offloaded, rather than a green clone alone.
///
/// It fronts whichever origin the deployment shape uses — the bucket
/// (CloudFront + S3 origin, the production shape) or this server (the
/// origin-route shape, for edges without CloudFront key pairs).
struct FakeCdn {
    base: String,
    hits: mpsc::Receiver<String>,
    /// Set after the server is up, for the origin-route shape.
    server: Arc<OnceLock<String>>,
}

fn spawn_fake_cdn(bucket_base_url: &str) -> FakeCdn {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, hits) = mpsc::channel();
    // MinIO speaks S3, so every read is SigV4-signed — the fake edge
    // reads through the same store client the server uses.
    let upstream =
        stratum_store::ObjectStore::new(bucket_base_url, stratum_store::LatencyModel::None);
    let server: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
    let origin = server.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { continue };
            let mut buf = [0u8; 8192];
            let n = sock.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            let _ = tx.send(path.clone());
            let fetched = match origin.get() {
                // Origin-route shape: forward verbatim, query and all —
                // the token in it is what authorizes the read.
                Some(base) => match ureq::get(&format!("{base}{path}")).call() {
                    Ok(r) => {
                        let mut v = Vec::new();
                        r.into_reader().read_to_end(&mut v).ok().map(|_| v)
                    }
                    Err(_) => None,
                },
                // Store-origin shape: the path is the object key.
                None => {
                    let key = path.split('?').next().unwrap_or("").trim_start_matches('/');
                    upstream.get(key).ok()
                }
            };
            let resp = match fetched {
                Some(b) => {
                    let mut h = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        b.len()
                    )
                    .into_bytes();
                    h.extend_from_slice(&b);
                    h
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            let _ = sock.write_all(&resp);
        }
    });
    FakeCdn {
        base: format!("http://127.0.0.1:{port}"),
        hits,
        server,
    }
}

impl FakeCdn {
    fn pack_hits(&self) -> Vec<String> {
        self.hits
            .try_iter()
            .filter(|p| p.contains(".pack"))
            .collect()
    }
    fn drain(&self) {
        while self.hits.try_recv().is_ok() {}
    }
    /// Point the edge at the server (the origin-route deployment shape).
    fn front(&self, server_base: &str) {
        self.server.set(server_base.to_string()).unwrap();
    }
}

/// Spawn through `stratum_testkit::server::spawn_on_free_port`, and not
/// through a port picked here.
///
/// This used to bind `127.0.0.1:0`, read the port back, drop the
/// listener and spawn on it — and then wait for `/healthz` to answer
/// *anything*. Every part of that is the hazard `spawn_on_free_port`
/// exists to close, and this suite had opted out of all of it:
///
///   * the probe port is free when it is read and not when it is used,
///     which is the same bind-and-release race `Pg::start` was already
///     caught making (see its comment — losing that race is worse than a
///     clash, because `connect` *succeeds* against the stranger);
///   * `Ok(_) => break` accepts whoever is on the port. No
///     `STRATUM_INSTANCE_ID` was set and no `x-weft-instance` compared,
///     so a server belonging to another test satisfied the readiness
///     check perfectly;
///   * a child that exited during startup was never noticed — the loop
///     waited out its twenty seconds against somebody else's server
///     instead.
///
/// The visible cost was a `git clone` in `seed_repo` failing with
/// `remote: authentication required`, twice in three days, on two
/// different tests and two different CI jobs — a token minted in *this*
/// test's database presented to a server holding *another* one. Three
/// investigations could not name it, and could not have: nothing in the
/// harness recorded which server answered. `spawn_on_free_port` verifies
/// the instance before it trusts the port, retries on a fresh one when a
/// stranger replies, and says so.
fn spawn_server(store_url: &str, scratch: &Scratch, extra: &[(&str, String)]) -> (Server, String) {
    let db_url = stratum_testkit::pg::test_db_url("cdnpack");
    let extra = extra.to_vec();
    let store_url = store_url.to_string();
    let data_dir = scratch.path().join("data");
    let db_for_build = db_url.clone();
    let (child, bind) = stratum_testkit::server::spawn_on_free_port(move |bind| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        cmd.env_clear()
            .envs(std::env::var("LLVM_PROFILE_FILE").map(|p| ("LLVM_PROFILE_FILE", p)))
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("STRATUM_STORE_URL", &store_url)
            .env("STRATUM_DB_URL", &db_for_build)
            .env("STRATUM_DATA_DIR", &data_dir)
            .env("STRATUM_BIND", bind)
            .env("STRATUM_MIRROR_POLL_SECS", "0")
            // The tests drive packing explicitly; no background churn.
            .env("STRATUM_CDNPACK_POLL_SECS", "0")
            .env("AWS_ACCESS_KEY_ID", ROOT_USER)
            .env("AWS_SECRET_ACCESS_KEY", ROOT_PASSWORD)
            .env("AWS_REGION", "us-east-1");
        for (k, v) in &extra {
            cmd.env(k, v);
        }
        cmd
    });
    let s = Server {
        child,
        base: format!("http://{bind}"),
    };
    (s, db_url)
}

fn admin_bootstrap(store_url: &str, db_url: &str, org: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_stratum-server"))
        .env_clear()
        .envs(std::env::var("LLVM_PROFILE_FILE").map(|p| ("LLVM_PROFILE_FILE", p)))
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("STRATUM_STORE_URL", store_url)
        .env("STRATUM_DB_URL", db_url)
        .env("AWS_ACCESS_KEY_ID", ROOT_USER)
        .env("AWS_SECRET_ACCESS_KEY", ROOT_PASSWORD)
        .env("AWS_REGION", "us-east-1")
        .args(["admin", "bootstrap", "--org", org])
        .output()
        .expect("bootstrap");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["admin_token"].as_str().unwrap().to_string()
}

impl Server {
    fn post(&self, path: &str, token: &str, body: Option<serde_json::Value>) -> (u16, String) {
        let mut r = ureq::post(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {token}"));
        if body.is_some() {
            r = r.set("Content-Type", "application/json");
        }
        let resp = match body {
            Some(b) => r.send_string(&b.to_string()),
            None => r.call(),
        };
        match resp {
            Ok(x) => (x.status(), x.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(c, x)) => (c, x.into_string().unwrap_or_default()),
            Err(e) => panic!("transport: {e}"),
        }
    }
    /// Status of an unauthenticated GET — how git itself fetches a pack.
    fn get_status(&self, path: &str) -> u16 {
        match ureq::get(&format!("{}{path}", self.base)).call() {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(c, _)) => c,
            Err(e) => panic!("transport: {e}"),
        }
    }
    fn authed_url(&self, token: &str, org: &str, repo: &str) -> String {
        let base = self.base.strip_prefix("http://").unwrap();
        format!("http://x:{token}@{base}/{org}/{repo}.git")
    }
}

/// Clone with the CDN opt-in the real feature requires. `http` is in
/// `fetch.uriprotocols` because the fake CDN speaks plain HTTP.
fn clone_opted_in(cwd: &std::path::Path, url: &str, dest: &std::path::Path) -> String {
    gitcli::git(
        cwd,
        &[
            "-c",
            "protocol.version=2",
            "-c",
            "fetch.uriprotocols=http,https",
            "clone",
            "-q",
            url,
            dest.to_str().unwrap(),
        ],
    )
}

fn seed_repo(server: &Server, admin: &str, scratch: &Scratch, org: &str, repo: &str) -> String {
    let url = server.authed_url(admin, org, repo);
    let work = scratch.path().join(format!("seed-{org}-{repo}"));
    gitcli::git(
        scratch.path(),
        &["clone", "-q", &url, work.to_str().unwrap()],
    );
    gitcli::git(&work, &["checkout", "-q", "-b", "main"]);
    // Something big enough that offloading it is meaningful.
    let blob: String = (0..4000)
        .map(|i| format!("line {i} of bulk content\n"))
        .collect();
    std::fs::write(work.join("bulk.txt"), &blob).unwrap();
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "bulk"]);
    gitcli::git(&work, &["push", "-q", "origin", "main"]);
    gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string()
}

#[test]
fn opted_in_clone_pulls_bulk_from_the_cdn_and_is_fsck_clean() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-e2e");
    let scratch = Scratch::new("cdn-e2e");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_CDN_BASE", cdn.base.clone())],
    );
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos",
                &admin,
                Some(serde_json::json!({"name":"app"}))
            )
            .0,
        201
    );
    let tip = seed_repo(&server, &admin, &scratch, "acme", "app");

    // Build the CDN pack (the worker's job, driven deterministically).
    let (st, body) = server.post("/v1/orgs/acme/repos/app/cdn-pack", &admin, None);
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("Built"), "{body}");
    cdn.drain();

    // An opted-in clone must actually hit the CDN…
    let url = server.authed_url(&admin, "acme", "app");
    let dest = scratch.path().join("via-cdn");
    clone_opted_in(scratch.path(), &url, &dest);
    let hits = cdn.pack_hits();
    assert!(
        !hits.is_empty(),
        "client did not fetch the advertised pack from the CDN"
    );
    assert!(hits[0].contains("/cdn/"), "unexpected CDN path: {hits:?}");

    // …and the resulting repo must be correct, not merely present.
    gitcli::fsck(&dest);
    assert_eq!(gitcli::git(&dest, &["rev-parse", "HEAD"]).trim(), tip);
    assert_eq!(
        std::fs::read_to_string(dest.join("README.md")).unwrap(),
        "hello\n"
    );
    assert!(dest.join("bulk.txt").exists(), "offloaded content missing");

    // A client that never opted in behaves exactly as before: no CDN
    // traffic at all, and a complete clone. This is the property that
    // protects the existing fleet.
    cdn.drain();
    let plain = scratch.path().join("no-optin");
    gitcli::git(
        scratch.path(),
        &["clone", "-q", &url, plain.to_str().unwrap()],
    );
    gitcli::fsck(&plain);
    assert_eq!(gitcli::git(&plain, &["rev-parse", "HEAD"]).trim(), tip);
    assert!(
        cdn.pack_hits().is_empty(),
        "a non-opted-in client must never be sent to the CDN"
    );

    // The pack build reads the layout and writes beside it; a builder that
    // reused an epoch directory would show up as a length disagreement.
    assert_closed(&bucket.base_url);
}

/// A pack that lags the tip must not be offered. The inline remainder
/// would be the pushers' own **thin** packs, whose delta bases live back
/// in the layout — inside the CDN pack — and git indexes the inline pack
/// BEFORE downloading the advertised URIs, so those bases are absent and
/// the clone dies with "unresolved deltas".
///
/// It only appears to work when a push happens to carry no deltas against
/// older objects, which is why this test pushes *similar* content: that
/// is what makes git delta against the base commits and what turns the
/// bug from invisible to fatal.
#[test]
fn a_lagging_pack_is_not_offloaded_and_the_clone_is_still_correct() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-stale");
    let scratch = Scratch::new("cdn-stale");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_CDN_BASE", cdn.base.clone())],
    );
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name":"app"})),
    );
    seed_repo(&server, &admin, &scratch, "acme", "app");
    assert!(server
        .post("/v1/orgs/acme/repos/app/cdn-pack", &admin, None)
        .1
        .contains("Built"));

    // Push AFTER the pack was built, with content close enough to the
    // seed that git deltas against it.
    let url = server.authed_url(&admin, "acme", "app");
    let work = scratch.path().join("more");
    gitcli::git(
        scratch.path(),
        &["clone", "-q", &url, work.to_str().unwrap()],
    );
    let seeded = std::fs::read_to_string(work.join("bulk.txt")).unwrap();
    for i in 0..3 {
        std::fs::write(
            work.join(format!("near{i}.txt")),
            format!("{seeded}near-duplicate {i}\n"),
        )
        .unwrap();
        gitcli::git(&work, &["add", "-A"]);
        gitcli::git(&work, &["commit", "-q", "-m", &format!("after-{i}")]);
    }
    gitcli::git(&work, &["push", "-q", "origin", "HEAD:main"]);
    let true_tip = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // The clone must succeed and be correct — served inline, with the
    // lagging pack left out of the advertisement entirely.
    cdn.drain();
    let dest = scratch.path().join("stale-clone");
    clone_opted_in(scratch.path(), &url, &dest);
    gitcli::fsck(&dest);
    assert_eq!(
        gitcli::git(&dest, &["rev-parse", "HEAD"]).trim(),
        true_tip,
        "the clone must land at the true tip"
    );
    for i in 0..3 {
        assert!(
            dest.join(format!("near{i}.txt")).exists(),
            "post-pack commit {i} missing"
        );
    }
    assert!(
        cdn.pack_hits().is_empty(),
        "a pack that no longer covers the layout must not be advertised"
    );

    // Once the packer catches up, offload engages again.
    assert!(server
        .post("/v1/orgs/acme/repos/app/cdn-pack", &admin, None)
        .1
        .contains("Built"));
    cdn.drain();
    let after = scratch.path().join("caught-up");
    clone_opted_in(scratch.path(), &url, &after);
    assert!(
        !cdn.pack_hits().is_empty(),
        "a current pack must be offloaded again"
    );
    gitcli::fsck(&after);
    assert_eq!(gitcli::git(&after, &["rev-parse", "HEAD"]).trim(), true_tip);

    // Two pushes straddling two pack builds — the state where the pack
    // and the layout are most likely to have drifted apart.
    assert_closed(&bucket.base_url);
}

#[test]
fn a_missing_pack_is_never_advertised_and_clones_keep_working() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-missing");
    let scratch = Scratch::new("cdn-missing");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_CDN_BASE", cdn.base.clone())],
    );
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name":"app"})),
    );
    let tip = seed_repo(&server, &admin, &scratch, "acme", "app");
    server.post("/v1/orgs/acme/repos/app/cdn-pack", &admin, None);

    // Delete the pack out from under the descriptor — the exact state
    // that would abort an opted-in clone if we advertised it blindly.
    let store =
        stratum_store::ObjectStore::new(&bucket.base_url, stratum_store::LatencyModel::None);
    let (_, repo_json) = {
        let r = ureq::get(&format!("{}/v1/orgs/acme/repos/app", server.base))
            .set("Authorization", &format!("Bearer {admin}"))
            .call()
            .unwrap();
        (r.status(), r.into_string().unwrap())
    };
    let repo: serde_json::Value = serde_json::from_str(&repo_json).unwrap();
    let prefix = format!(
        "o/{}/r/{}/prod",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );
    let desc: serde_json::Value =
        serde_json::from_slice(&store.get(&format!("{prefix}/cdn/current.json")).unwrap()).unwrap();
    store.delete(desc["pack_key"].as_str().unwrap()).unwrap();

    // The clone must still succeed — by not being offered the CDN at all.
    cdn.drain();
    let url = server.authed_url(&admin, "acme", "app");
    let dest = scratch.path().join("degraded");
    clone_opted_in(scratch.path(), &url, &dest);
    gitcli::fsck(&dest);
    assert_eq!(gitcli::git(&dest, &["rev-parse", "HEAD"]).trim(), tip);
    assert!(
        cdn.pack_hits().is_empty(),
        "a pack we cannot confirm must never be advertised"
    );

    // The pack was deleted out from under the descriptor. That object is
    // not manifest-referenced, so the layout must still be closed — this
    // is the assertion that says the deletion hit only what it aimed at.
    assert_closed(&bucket.base_url);
}

#[test]
fn the_kill_switch_disables_offload_without_breaking_clones() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-kill");
    let scratch = Scratch::new("cdn-kill");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_CDN_BASE", cdn.base.clone()),
            ("STRATUM_CDN_ENABLED", "0".into()),
        ],
    );
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name":"app"})),
    );
    let tip = seed_repo(&server, &admin, &scratch, "acme", "app");
    server.post("/v1/orgs/acme/repos/app/cdn-pack", &admin, None);

    cdn.drain();
    let url = server.authed_url(&admin, "acme", "app");
    let dest = scratch.path().join("killed");
    clone_opted_in(scratch.path(), &url, &dest);
    gitcli::fsck(&dest);
    assert_eq!(gitcli::git(&dest, &["rev-parse", "HEAD"]).trim(), tip);
    assert!(
        cdn.pack_hits().is_empty(),
        "kill switch must stop advertisement fleet-wide"
    );
}

/// The origin-route deployment shape: the CDN fronts *this server*, and
/// the advertised URL carries a short-lived HMAC token because git sends
/// no credentials. Unlike CloudFront's edge validation, this whole path —
/// mint, advertise, fetch, authorize, stream — runs in CI.
#[test]
fn the_origin_route_serves_an_offloaded_clone_and_is_fsck_clean() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-origin");
    let scratch = Scratch::new("cdn-origin");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_CDN_BASE", cdn.base.clone()),
            ("STRATUM_CDN_ORIGIN_SECRET", "origin-secret".into()),
        ],
    );
    cdn.front(&server.base);
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos",
                &admin,
                Some(serde_json::json!({"name":"app"}))
            )
            .0,
        201
    );
    let tip = seed_repo(&server, &admin, &scratch, "acme", "app");
    assert!(server
        .post("/v1/orgs/acme/repos/app/cdn-pack", &admin, None)
        .1
        .contains("Built"));
    cdn.drain();

    let url = server.authed_url(&admin, "acme", "app");
    let dest = scratch.path().join("via-origin-route");
    clone_opted_in(scratch.path(), &url, &dest);
    let hits = cdn.pack_hits();
    assert!(!hits.is_empty(), "client did not offload to the CDN");
    assert!(
        hits[0].starts_with("/v1/orgs/acme/repos/app/cdn/") && hits[0].contains("sig="),
        "the advertised URL must be the token-bearing pack route: {hits:?}"
    );
    gitcli::fsck(&dest);
    assert_eq!(gitcli::git(&dest, &["rev-parse", "HEAD"]).trim(), tip);
    assert!(dest.join("bulk.txt").exists(), "offloaded content missing");
}

/// Everything a bad actor would try against the unauthenticated pack
/// route. Each must be a masked 404 that leaks nothing — and the server
/// must still be serving afterwards.
#[test]
fn the_pack_route_refuses_forged_expired_and_cross_tenant_requests() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-abuse");
    let scratch = Scratch::new("cdn-abuse");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_CDN_BASE", cdn.base.clone()),
            ("STRATUM_CDN_ORIGIN_SECRET", "origin-secret".into()),
        ],
    );
    cdn.front(&server.base);
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    let victim_admin = admin_bootstrap(&bucket.base_url, &db, "victim");
    for (tok, org) in [(&admin, "acme"), (&victim_admin, "victim")] {
        server.post(
            &format!("/v1/orgs/{org}/repos"),
            tok,
            Some(serde_json::json!({"name":"app"})),
        );
        seed_repo(&server, tok, &scratch, org, "app");
        assert!(server
            .post(&format!("/v1/orgs/{org}/repos/app/cdn-pack"), tok, None)
            .1
            .contains("Built"));
    }

    // Capture a legitimate URL by watching what an opted-in clone is told.
    cdn.drain();
    let url = server.authed_url(&admin, "acme", "app");
    clone_opted_in(scratch.path(), &url, &scratch.path().join("legit"));
    let signed = cdn
        .pack_hits()
        .into_iter()
        .next()
        .expect("no CDN URL was advertised");
    let (route, query) = signed.split_once('?').expect("URL carries a token");
    let pack_file = route.rsplit('/').next().unwrap().to_string();
    let exp: u64 = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("exp="))
        .unwrap()
        .parse()
        .unwrap();
    let sig = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("sig="))
        .unwrap()
        .to_string();

    // The real thing works — the baseline every refusal is measured against.
    assert_eq!(server.get_status(&signed), 200, "legit token must serve");

    let flipped: String = {
        let mut c: Vec<char> = sig.chars().collect();
        c[0] = if c[0] == 'a' { 'b' } else { 'a' };
        c.into_iter().collect()
    };
    let cases: Vec<(&str, String)> = vec![
        ("no token at all", route.to_string()),
        ("empty signature", format!("{route}?exp={exp}&sig=")),
        (
            "tampered signature",
            format!("{route}?exp={exp}&sig={flipped}"),
        ),
        (
            "extended expiry (replay past the TTL)",
            format!("{route}?exp={}&sig={sig}", exp + 86_400),
        ),
        (
            "already expired",
            format!("{route}?exp=1&sig={}", "0".repeat(64)),
        ),
        (
            "non-numeric expiry",
            format!("{route}?exp=notanumber&sig={sig}"),
        ),
        (
            // Cross-tenant: the same file name under another org. The token
            // is HMAC'd over the full key, so it cannot travel.
            "another tenant's repo",
            format!("/v1/orgs/victim/repos/app/cdn/{pack_file}?exp={exp}&sig={sig}"),
        ),
        (
            "a repo that does not exist",
            format!("/v1/orgs/acme/repos/ghost/cdn/{pack_file}?exp={exp}&sig={sig}"),
        ),
        (
            "an org that does not exist",
            format!("/v1/orgs/ghost/repos/app/cdn/{pack_file}?exp={exp}&sig={sig}"),
        ),
        (
            // The descriptor sits next to the packs; the route must not be
            // a way to read anything but a pack.
            "the descriptor instead of a pack",
            format!("/v1/orgs/acme/repos/app/cdn/current.json?exp={exp}&sig={sig}"),
        ),
        (
            "path traversal in the pack segment",
            format!("/v1/orgs/acme/repos/app/cdn/..%2f..%2fcurrent.json?exp={exp}&sig={sig}"),
        ),
        (
            "a pack name that is not pack-shaped",
            format!("/v1/orgs/acme/repos/app/cdn/not%20a%20pack.pack?exp={exp}&sig={sig}"),
        ),
    ];
    for (what, path) in cases {
        let st = server.get_status(&path);
        assert_eq!(
            st, 404,
            "{what}: expected a masked 404, got {st} for {path}"
        );
    }

    // Still healthy, still serving: a refusal must not be a denial of
    // service against everyone else.
    assert_eq!(server.get_status("/healthz"), 200);
    assert_eq!(server.get_status(&signed), 200);
    let after = scratch.path().join("after-abuse");
    clone_opted_in(scratch.path(), &url, &after);
    gitcli::fsck(&after);
}

/// A store-origin deployment must not also expose the route as a second,
/// token-free way into the bucket.
#[test]
fn the_pack_route_is_closed_when_the_deployment_does_not_use_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-noroute");
    let scratch = Scratch::new("cdn-noroute");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_CDN_BASE", cdn.base.clone())],
    );
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name":"app"})),
    );
    seed_repo(&server, &admin, &scratch, "acme", "app");
    server.post("/v1/orgs/acme/repos/app/cdn-pack", &admin, None);
    let pack = format!("{}-{}.pack", "a".repeat(40), "b".repeat(40));
    assert_eq!(
        server.get_status(&format!(
            "/v1/orgs/acme/repos/app/cdn/{pack}?exp=99999999999&sig={}",
            "0".repeat(64)
        )),
        404
    );

    // And a deployment with no CDN configured at all: same answer.
    let (plain, db2) = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin2 = admin_bootstrap(&bucket.base_url, &db2, "acme2");
    plain.post(
        "/v1/orgs/acme2/repos",
        &admin2,
        Some(serde_json::json!({"name":"app"})),
    );
    assert_eq!(
        plain.get_status(&format!(
            "/v1/orgs/acme2/repos/app/cdn/{pack}?exp=99999999999&sig={}",
            "0".repeat(64)
        )),
        404
    );
}

/// What the edge is allowed to keep: nothing. Every repository is
/// private to its organisation, so every pack is authorized by an
/// expiring token and must never sit in a shared cache after it expires
/// — there is no public repository whose pack could be immutable and
/// shareable any more, and a `public` cache header here would be one a
/// shared edge was entitled to serve to anybody.
#[test]
fn a_pack_is_never_publicly_cacheable() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-cache");
    let scratch = Scratch::new("cdn-cache");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_CDN_BASE", cdn.base.clone()),
            ("STRATUM_CDN_ORIGIN_SECRET", "origin-secret".into()),
        ],
    );
    cdn.front(&server.base);
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    let name = "app";
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": name})),
    );
    assert_eq!(st, 201, "{out}");
    seed_repo(&server, &admin, &scratch, "acme", name);
    assert!(server
        .post(
            &format!("/v1/orgs/acme/repos/{name}/cdn-pack"),
            &admin,
            None
        )
        .1
        .contains("Built"));
    cdn.drain();
    clone_opted_in(
        scratch.path(),
        &server.authed_url(&admin, "acme", name),
        &scratch.path().join(format!("clone-{name}")),
    );
    let signed = cdn
        .pack_hits()
        .into_iter()
        .next()
        .expect("no CDN URL advertised");
    let resp = ureq::get(&format!("{}{signed}", server.base))
        .call()
        .unwrap();
    let cache = resp.header("cache-control").unwrap_or_default().to_string();
    assert_eq!(cache, "private, no-store", "{cache}");

    // A token that outlives its object: the pack is gone, so the route
    // answers 404 rather than 500 — the descriptor may legitimately be
    // superseded between advertisement and fetch.
    let store =
        stratum_store::ObjectStore::new(&bucket.base_url, stratum_store::LatencyModel::None);
    let prefix = repo_prefix(&server, &admin, "acme", name);
    let desc: serde_json::Value =
        serde_json::from_slice(&store.get(&format!("{prefix}/cdn/current.json")).unwrap()).unwrap();
    store.delete(desc["pack_key"].as_str().unwrap()).unwrap();
    assert_eq!(server.get_status(&signed), 404, "a vanished pack must 404");
    assert_eq!(server.get_status("/healthz"), 200);
}

/// A signing key that is valid-looking at boot but unusable at signing
/// time must degrade to serving inline. The one thing it must never do
/// is fall back to an UNSIGNED URL — for a private repo that would be an
/// open door.
#[test]
fn a_broken_signing_key_degrades_to_inline_serving_not_to_an_unsigned_url() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-badkey");
    let scratch = Scratch::new("cdn-badkey");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_CDN_BASE", cdn.base.clone()),
            ("STRATUM_CDN_KEY_PAIR_ID", "KBROKEN1".into()),
            (
                "STRATUM_CDN_PRIVATE_KEY",
                "-----BEGIN PRIVATE KEY-----\nnot a key\n-----END PRIVATE KEY-----\n".into(),
            ),
        ],
    );
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name":"app"})),
    );
    let tip = seed_repo(&server, &admin, &scratch, "acme", "app");
    assert!(server
        .post("/v1/orgs/acme/repos/app/cdn-pack", &admin, None)
        .1
        .contains("Built"));

    cdn.drain();
    let dest = scratch.path().join("degraded-signing");
    clone_opted_in(
        scratch.path(),
        &server.authed_url(&admin, "acme", "app"),
        &dest,
    );
    gitcli::fsck(&dest);
    assert_eq!(gitcli::git(&dest, &["rev-parse", "HEAD"]).trim(), tip);
    assert!(
        cdn.pack_hits().is_empty(),
        "a URL we could not sign must not be advertised at all"
    );
}

// ---------------------------------------------------------------------
// SSH transport
// ---------------------------------------------------------------------

/// `ssh-keygen` a real keypair; returns (private path, public line).
fn keygen(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, String) {
    let priv_path = dir.join(name);
    let out = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&priv_path)
        .stdin(Stdio::null())
        .output()
        .expect("run ssh-keygen (openssh-client must be installed)");
    assert!(
        out.status.success(),
        "ssh-keygen: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pub_line = std::fs::read_to_string(priv_path.with_extension("pub")).unwrap();
    (priv_path, pub_line.trim().to_string())
}

/// The ssh invocation git runs, hermetic: no user config, no agent.
fn ssh_command(key: &std::path::Path, known_hosts: &std::path::Path) -> String {
    format!(
        "ssh -F none -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none \
         -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={} -i {}",
        known_hosts.display(),
        key.display()
    )
}

/// Offload is a property of the protocol, not of HTTP: the same
/// advertisement and the same CDN fetch must happen over SSH, where
/// there is no HTTP request for the pack URL to ride on at all.
#[test]
fn ssh_clones_offload_to_the_cdn_and_are_fsck_clean() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-ssh");
    let scratch = Scratch::new("cdn-ssh");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (_, host_pem) = {
        let (p, _) = keygen(scratch.path(), "host-key");
        let pem = std::fs::read_to_string(&p).unwrap();
        (p, pem)
    };
    let ssh_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_CDN_BASE", cdn.base.clone()),
            ("STRATUM_SSH_BIND", format!("127.0.0.1:{ssh_port}")),
            ("STRATUM_SSH_HOST_KEY", host_pem),
        ],
    );
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    let token_id = admin.split('_').nth(1).unwrap().to_string();
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name":"app"})),
    );
    let (st, added) = server.post(
        "/v1/orgs/acme/ssh-keys",
        &admin,
        Some(serde_json::json!({
            "public_key": keygen(scratch.path(), "dev-key").1,
            "token_id": token_id,
            "label": "laptop",
        })),
    );
    assert_eq!(st, 201, "{added}");
    let tip = seed_repo(&server, &admin, &scratch, "acme", "app");
    assert!(server
        .post("/v1/orgs/acme/repos/app/cdn-pack", &admin, None)
        .1
        .contains("Built"));

    let ssh = ssh_command(
        &scratch.path().join("dev-key"),
        &scratch.path().join("known_hosts"),
    );
    let url = format!("ssh://git@127.0.0.1:{ssh_port}/acme/app.git");
    cdn.drain();
    let dest = scratch.path().join("ssh-cdn-clone");
    gitcli::git(
        scratch.path(),
        &[
            "-c",
            &format!("core.sshCommand={ssh}"),
            "-c",
            "protocol.version=2",
            "-c",
            "fetch.uriprotocols=http,https",
            "clone",
            "-q",
            &url,
            dest.to_str().unwrap(),
        ],
    );
    assert!(
        !cdn.pack_hits().is_empty(),
        "an ssh clone must offload exactly like an http one"
    );
    gitcli::fsck(&dest);
    assert_eq!(gitcli::git(&dest, &["rev-parse", "HEAD"]).trim(), tip);
    assert!(dest.join("bulk.txt").exists(), "offloaded content missing");

    // And an ssh client that never opted in still gets a complete repo
    // without touching the CDN.
    cdn.drain();
    let plain = scratch.path().join("ssh-plain-clone");
    gitcli::git(
        scratch.path(),
        &[
            "-c",
            &format!("core.sshCommand={ssh}"),
            "clone",
            "-q",
            &url,
            plain.to_str().unwrap(),
        ],
    );
    gitcli::fsck(&plain);
    assert!(cdn.pack_hits().is_empty());
}

/// A pack corrupted or truncated in transit must fail loudly. git checks
/// the pack it downloads, so the failure mode we must never have — a
/// silently incomplete repo that passes as good — cannot happen; this
/// pins that, since a "successful" clone missing objects would be the
/// worst possible outcome of the whole feature.
#[test]
fn a_corrupt_pack_at_the_cdn_fails_loudly_rather_than_silently() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-corrupt");
    let scratch = Scratch::new("cdn-corrupt");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_CDN_BASE", cdn.base.clone())],
    );
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name":"app"})),
    );
    let tip = seed_repo(&server, &admin, &scratch, "acme", "app");
    assert!(server
        .post("/v1/orgs/acme/repos/app/cdn-pack", &admin, None)
        .1
        .contains("Built"));

    // Truncate the pack in the store, leaving the descriptor pointing at
    // it: the pack still *exists*, so verify-before-advertise passes and
    // the client really does download the damaged bytes.
    let store =
        stratum_store::ObjectStore::new(&bucket.base_url, stratum_store::LatencyModel::None);
    let prefix = repo_prefix(&server, &admin, "acme", "app");
    let desc: serde_json::Value =
        serde_json::from_slice(&store.get(&format!("{prefix}/cdn/current.json")).unwrap()).unwrap();
    let key = desc["pack_key"].as_str().unwrap();
    let good = store.get(key).unwrap();
    store
        .put(key, &good[..good.len() / 2], stratum_store::PutCond::None)
        .unwrap();

    let url = server.authed_url(&admin, "acme", "app");
    let dest = scratch.path().join("corrupt");
    let err = gitcli::git_expect_err(
        scratch.path(),
        &[
            "-c",
            "protocol.version=2",
            "-c",
            "fetch.uriprotocols=http,https",
            "clone",
            "-q",
            &url,
            dest.to_str().unwrap(),
        ],
    )
    .expect("a corrupt pack must not produce a successful clone");
    assert!(
        !dest.join(".git").exists() && !dest.join("HEAD").exists(),
        "a failed clone must leave no repo behind: {err}"
    );

    // Restoring the pack restores service — the damage was the pack's,
    // not the server's.
    store.put(key, &good, stratum_store::PutCond::None).unwrap();
    let ok = scratch.path().join("recovered");
    clone_opted_in(scratch.path(), &url, &ok);
    gitcli::fsck(&ok);
    assert_eq!(gitcli::git(&ok, &["rev-parse", "HEAD"]).trim(), tip);
}

/// Offloaded clones must be *visible* as offloaded: capacity planning and
/// billing both depend on telling "we served this" from "the edge did".
#[test]
fn offloaded_clones_meter_separately_from_inline_ones() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-meter");
    let scratch = Scratch::new("cdn-meter");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_CDN_BASE", cdn.base.clone())],
    );
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name":"app"})),
    );
    seed_repo(&server, &admin, &scratch, "acme", "app");
    server.post("/v1/orgs/acme/repos/app/cdn-pack", &admin, None);

    let url = server.authed_url(&admin, "acme", "app");
    clone_opted_in(scratch.path(), &url, &scratch.path().join("m-cdn"));
    gitcli::git(
        scratch.path(),
        &[
            "clone",
            "-q",
            &url,
            scratch.path().join("m-inline").to_str().unwrap(),
        ],
    );

    // Meter rows are written asynchronously; poll rather than guess.
    let deadline = Instant::now() + Duration::from_secs(10);
    let kinds = loop {
        let body = ureq::get(&format!("{}/v1/orgs/acme/repos/app/metrics", server.base))
            .set("Authorization", &format!("Bearer {admin}"))
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let kinds = v["kinds"].clone();
        if kinds.get("cdn_clone").is_some() && kinds.get("clone").is_some() {
            break kinds;
        }
        assert!(Instant::now() < deadline, "metrics never showed both: {v}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(kinds["cdn_clone"]["count"].as_u64(), Some(1));
    assert_eq!(kinds["clone"]["count"].as_u64(), Some(1));
    // The offloaded clone streams far fewer bytes through the app — that
    // saving is the entire point of the feature.
    let cdn_bytes = kinds["cdn_clone"]["bytes"].as_u64().unwrap();
    let inline_bytes = kinds["clone"]["bytes"].as_u64().unwrap();
    assert!(
        cdn_bytes * 4 < inline_bytes,
        "offload saved little or nothing: {cdn_bytes} vs {inline_bytes} inline bytes"
    );
}

/// The store prefix for a repo, read back through the API.
fn repo_prefix(server: &Server, token: &str, org: &str, repo: &str) -> String {
    let body = ureq::get(&format!("{}/v1/orgs/{org}/repos/{repo}", server.base))
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    format!(
        "o/{}/r/{}/prod",
        v["org_id"].as_str().unwrap(),
        v["id"].as_str().unwrap()
    )
}

/// Boot with these env vars and return (success, stderr).
fn try_boot(store_url: &str, db_url: &str, extra: &[(&str, String)]) -> (bool, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
    cmd.env_clear()
        .envs(std::env::var("LLVM_PROFILE_FILE").map(|p| ("LLVM_PROFILE_FILE", p)))
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("STRATUM_STORE_URL", store_url)
        .env("STRATUM_DB_URL", db_url)
        .env("STRATUM_BIND", "127.0.0.1:0")
        .env("AWS_ACCESS_KEY_ID", ROOT_USER)
        .env("AWS_SECRET_ACCESS_KEY", ROOT_PASSWORD)
        .env("AWS_REGION", "us-east-1");
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into(),
    )
}

/// A misconfigured signer must fail at deploy time, not at clone time.
/// Both of these mistakes would otherwise be discovered by a user whose
/// private repo was served through an unsigned URL, or not at all.
#[test]
fn a_misconfigured_cdn_signer_is_a_boot_error() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-boot");
    let db = stratum_testkit::pg::test_db_url("cdnboot");

    // Half a signer: a key-pair id with no key would hand out UNSIGNED
    // URLs for what may be private objects.
    let (ok, err) = try_boot(
        &bucket.base_url,
        &db,
        &[
            ("STRATUM_CDN_BASE", "https://d1.cloudfront.net".into()),
            ("STRATUM_CDN_KEY_PAIR_ID", "K123".into()),
        ],
    );
    assert!(!ok, "a half-configured signer must not boot");
    assert!(err.contains("STRATUM_CDN_KEY_PAIR_ID"), "{err}");

    // Two origins at once: the URL can only address one of them, and
    // guessing which would be worse than refusing.
    let (ok, err) = try_boot(
        &bucket.base_url,
        &db,
        &[
            ("STRATUM_CDN_BASE", "https://d1.cloudfront.net".into()),
            ("STRATUM_CDN_KEY_PAIR_ID", "K123".into()),
            ("STRATUM_CDN_PRIVATE_KEY", test_rsa_pem()),
            ("STRATUM_CDN_ORIGIN_SECRET", "s".into()),
        ],
    );
    assert!(!ok, "two conflicting origins must not boot");
    assert!(err.contains("STRATUM_CDN_ORIGIN_SECRET"), "{err}");
}

/// An RSA-2048 PEM, generated once per process (key generation is slow).
fn test_rsa_pem() -> String {
    use std::sync::OnceLock;
    static PEM: OnceLock<String> = OnceLock::new();
    PEM.get_or_init(|| {
        let out = Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
            ])
            .output()
            .expect("openssl genpkey");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    })
    .clone()
}

/// The CloudFront-signing deployment shape, end to end with real git:
/// the advertised URL carries `Expires`/`Signature`/`Key-Pair-Id`, and a
/// client that follows it gets a correct repo. (CloudFront's own
/// validation of that signature is the edge's, and is proved instead by
/// `deploy/smoke.sh` against a real distribution.)
#[test]
fn signed_urls_are_advertised_and_followed_with_the_key_from_a_file() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-signed");
    let scratch = Scratch::new("cdn-signed");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    // The key travels as a PATH here — the shape a container uses when
    // the secret is mounted as a file rather than an env value.
    let pem_path = scratch.path().join("cdn-key.pem");
    std::fs::write(&pem_path, test_rsa_pem()).unwrap();
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_CDN_BASE", cdn.base.clone()),
            ("STRATUM_CDN_KEY_PAIR_ID", "KTESTPAIR1".into()),
            (
                "STRATUM_CDN_PRIVATE_KEY_PEM",
                pem_path.to_str().unwrap().to_string(),
            ),
            ("STRATUM_CDN_URL_TTL_SECS", "900".into()),
        ],
    );
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name":"app"})),
    );
    let tip = seed_repo(&server, &admin, &scratch, "acme", "app");
    assert!(server
        .post("/v1/orgs/acme/repos/app/cdn-pack", &admin, None)
        .1
        .contains("Built"));

    cdn.drain();
    let url = server.authed_url(&admin, "acme", "app");
    let dest = scratch.path().join("signed-clone");
    clone_opted_in(scratch.path(), &url, &dest);
    let hits = cdn.pack_hits();
    assert!(!hits.is_empty(), "no CDN fetch happened");
    for want in ["Expires=", "Signature=", "Key-Pair-Id=KTESTPAIR1"] {
        assert!(hits[0].contains(want), "{want} missing from {hits:?}");
    }
    gitcli::fsck(&dest);
    assert_eq!(gitcli::git(&dest, &["rev-parse", "HEAD"]).trim(), tip);
}

/// The background worker, not the synchronous test hook: with polling on,
/// a push must produce a CDN pack on its own.
#[test]
fn the_worker_builds_packs_on_its_own_and_skips_repos_with_nothing_to_pack() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-worker");
    let scratch = Scratch::new("cdn-worker");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_CDN_BASE", cdn.base.clone()),
            ("STRATUM_CDNPACK_POLL_SECS", "1".into()),
        ],
    );
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");

    // A repo with nothing pushed has no manifest at all: the worker must
    // answer "not needed", never fail the job.
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name":"empty"})),
    );
    let (st, body) = server.post("/v1/orgs/acme/repos/empty/cdn-pack", &admin, None);
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("NotNeeded"), "{body}");

    // A pushed repo: nothing drives the worker here but its own poll.
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name":"app"})),
    );
    seed_repo(&server, &admin, &scratch, "acme", "app");
    let store =
        stratum_store::ObjectStore::new(&bucket.base_url, stratum_store::LatencyModel::None);
    let key = format!(
        "{}/cdn/current.json",
        repo_prefix(&server, &admin, "acme", "app")
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while store.get(&key).is_err() {
        assert!(
            Instant::now() < deadline,
            "the poller never built a pack for a pushed repo"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // And the pack it built really is usable by a client.
    let url = server.authed_url(&admin, "acme", "app");
    cdn.drain();
    let dest = scratch.path().join("worker-built");
    clone_opted_in(scratch.path(), &url, &dest);
    assert!(!cdn.pack_hits().is_empty());
    gitcli::fsck(&dest);

    // Two repos, one of them never pushed to: the empty one still owes a
    // parseable manifest, and the worker must not have written into it.
    assert_closed(&bucket.base_url);
}

/// The build hook is a write operation, and is authorized like one.
#[test]
fn building_a_cdn_pack_requires_write_scope() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-auth");
    let scratch = Scratch::new("cdn-auth");
    let (server, db) = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name":"app"})),
    );
    // Anonymous.
    assert_eq!(
        match ureq::post(&format!("{}/v1/orgs/acme/repos/app/cdn-pack", server.base)).call() {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(c, _)) => c,
            Err(e) => panic!("transport: {e}"),
        },
        401
    );
    // A read-only token: masked 404, like every other unauthorized repo op.
    let (_, minted) = server.post(
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({"scopes":["repo:read"]})),
    );
    let ro: serde_json::Value = serde_json::from_str(&minted).unwrap();
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos/app/cdn-pack",
                ro["token"].as_str().unwrap(),
                None
            )
            .0,
        404
    );
}

/// Commit `files` new files in one commit, without pushing. The content
/// deliberately repeats, so git deltas these objects against each other
/// and against earlier commits — the shape that exposes a thin remainder.
fn commit_files(work: &std::path::Path, tag: &str, files: usize) {
    for i in 0..files {
        std::fs::write(
            work.join(format!("{tag}-{i}.txt")),
            format!("{tag} file {i}\n{}", "payload line\n".repeat(40)),
        )
        .unwrap();
    }
    gitcli::git(work, &["add", "-A"]);
    gitcli::git(work, &["commit", "-q", "-m", tag]);
}

/// The steady state on a repo that is both pushed and cloned: a push
/// disengages offload until the background packer catches up, then it
/// engages again — with every clone in between correct.
#[test]
fn a_push_disengages_offload_until_the_packer_catches_up() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdn-churn");
    let scratch = Scratch::new("cdn-churn");
    let cdn = spawn_fake_cdn(&bucket.base_url);
    let (server, db) = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_CDN_BASE", cdn.base.clone()),
            ("STRATUM_CDNPACK_POLL_SECS", "1".into()),
        ],
    );
    let admin = admin_bootstrap(&bucket.base_url, &db, "acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name":"app"})),
    );

    let url = server.authed_url(&admin, "acme", "app");
    let work = scratch.path().join("seed");
    gitcli::git(
        scratch.path(),
        &["clone", "-q", &url, work.to_str().unwrap()],
    );
    gitcli::git(&work, &["checkout", "-q", "-b", "main"]);
    for c in 0..4 {
        commit_files(&work, &format!("base{c}"), 8);
    }
    gitcli::git(&work, &["push", "-q", "origin", "main"]);

    let store =
        stratum_store::ObjectStore::new(&bucket.base_url, stratum_store::LatencyModel::None);
    let prefix = repo_prefix(&server, &admin, "acme", "app");
    let key = format!("{prefix}/cdn/current.json");
    let tip_of = || -> String {
        let d: serde_json::Value = serde_json::from_slice(&store.get(&key).unwrap()).unwrap();
        d["tip"].as_str().unwrap().to_string()
    };
    let wait_for = |want: &str| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while store.get(&key).is_err() || tip_of() != want {
            assert!(
                Instant::now() < deadline,
                "the packer never caught up to {want}"
            );
            std::thread::sleep(Duration::from_millis(250));
        }
    };
    let head = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    wait_for(&head);

    // Offloaded while current.
    cdn.drain();
    let a = scratch.path().join("current");
    clone_opted_in(scratch.path(), &url, &a);
    assert!(!cdn.pack_hits().is_empty(), "a current pack must offload");
    gitcli::fsck(&a);

    // Push again. The very next clone races the packer: whichever side
    // wins, the clone must be correct — offloaded only if the pack is
    // current, inline otherwise. Never a broken repo.
    commit_files(&work, "next", 6);
    gitcli::git(&work, &["push", "-q", "origin", "HEAD:main"]);
    let next = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let racing = scratch.path().join("racing");
    clone_opted_in(scratch.path(), &url, &racing);
    gitcli::fsck(&racing);
    assert_eq!(gitcli::git(&racing, &["rev-parse", "HEAD"]).trim(), next);

    // And once the packer catches up, offload is back on.
    wait_for(&next);
    cdn.drain();
    let b = scratch.path().join("recaught");
    clone_opted_in(scratch.path(), &url, &b);
    assert!(
        !cdn.pack_hits().is_empty(),
        "offload must resume once the packer catches up"
    );
    gitcli::fsck(&b);
    assert_eq!(gitcli::git(&b, &["rev-parse", "HEAD"]).trim(), next);

    // The churniest path in this suite: two pushes and a background packer
    // rebuilding underneath them, deleting the superseded pack each time.
    assert_closed(&bucket.base_url);
}
