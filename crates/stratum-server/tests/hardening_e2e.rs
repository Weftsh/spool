//! Hardening end-to-end: the R8 isolation matrix (cross-org and
//! cross-repo tokens never see content, never learn existence),
//! immediate revocation, the 64MB push cap (413), readiness, RTT-count
//! regression budgets through the counting store proxy, and a concurrent
//! clone smoke with fsck on every product (I11).

use std::time::{Duration, Instant};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::{CountingProxy, Minio, Server};

fn spawn_server(store_url: &str, scratch: &Scratch, extra: &[(&str, String)]) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint("harden")
        .data_dir(scratch.path().join("data"))
        .envs(extra)
        .start()
}

fn create_repo(server: &Server, token: &str, org: &str, name: &str) {
    let (st, out) = server.req(
        "POST",
        &format!("/v1/orgs/{org}/repos"),
        token,
        Some(serde_json::json!({ "name": name })),
    );
    assert_eq!(st, 201, "{out}");
}

fn commit_file(server: &Server, token: &str, org: &str, repo: &str, path: &str, content: &str) {
    let (st, out) = server.req(
        "POST",
        &format!("/v1/orgs/{org}/repos/{repo}/commits"),
        token,
        Some(serde_json::json!({
            "message": format!("add {path}"),
            "operations": [ { "op": "put", "path": path, "content": content } ],
        })),
    );
    assert_eq!(st, 201, "{out}");
}

const SECRET: &str = "TOP-SECRET-bravo-payload-42";

/// Every cross-tenant probe answers 404 with no body detail — never the
/// resource, never a 403 that confirms existence.
#[test]
fn isolation_matrix_cross_org_and_cross_repo() {
    let minio = Minio::shared();
    let bucket = minio.bucket("hard-iso");
    let scratch = Scratch::new("iso");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);

    let alpha = server.bootstrap_org("alpha");
    let bravo = server.bootstrap_org("bravo");
    create_repo(&server, &alpha, "alpha", "app");
    create_repo(&server, &bravo, "bravo", "secret");
    commit_file(&server, &bravo, "bravo", "secret", "creds.txt", SECRET);

    // Org-A admin against every org-B surface.
    let gets = [
        "/v1/orgs/bravo/repos",
        "/v1/orgs/bravo/repos/secret",
        "/v1/orgs/bravo/repos/secret/files/creds.txt",
        "/v1/orgs/bravo/repos/secret/tree",
        "/v1/orgs/bravo/repos/secret/log",
        "/v1/orgs/bravo/repos/secret/refs",
        "/v1/orgs/bravo/repos/secret/owners?path=creds.txt",
        "/v1/orgs/bravo/repos/secret/owners/check?from=HEAD&to=HEAD",
        "/v1/orgs/bravo/repos/secret/changes",
        "/v1/orgs/bravo/repos/secret/land-queue",
        "/v1/orgs/bravo/repos/secret/protections",
        "/v1/orgs/bravo/repos/secret/changes/gdeadbeef/checks",
        "/v1/orgs/bravo/repos/secret/metrics",
        "/v1/orgs/bravo/usage",
        "/v1/orgs/bravo/audit",
    ];
    for path in gets {
        let (st, body) = server.req("GET", path, &alpha, None);
        assert_eq!(st, 404, "GET {path} leaked: {body}");
        assert!(
            !body.to_string().contains(SECRET),
            "GET {path} leaked content"
        );
    }
    let posts = [
        (
            "/v1/orgs/bravo/repos",
            serde_json::json!({ "name": "intruder" }),
        ),
        (
            "/v1/orgs/bravo/repos/secret/commits",
            serde_json::json!({ "message": "x", "operations": [] }),
        ),
        (
            "/v1/orgs/bravo/repos/secret/branches",
            serde_json::json!({ "name": "evil", "from": "main" }),
        ),
        (
            "/v1/orgs/bravo/repos/secret/reset",
            serde_json::json!({ "branch": "main", "to": "HEAD~1" }),
        ),
        ("/v1/orgs/bravo/repos/secret/export", serde_json::json!({})),
        (
            "/v1/orgs/bravo/tokens",
            serde_json::json!({ "scopes": ["org:admin"] }),
        ),
        (
            "/v1/orgs/bravo/repos/secret/webhooks",
            serde_json::json!({ "url": "http://evil", "secret": "s" }),
        ),
        (
            "/v1/orgs/bravo/repos/secret/changes",
            serde_json::json!({ "from": "main" }),
        ),
        (
            "/v1/orgs/bravo/repos/secret/protections",
            serde_json::json!({ "branch": "main" }),
        ),
        (
            "/v1/orgs/bravo/repos/secret/changes/gdeadbeef/checks",
            serde_json::json!({ "name": "ci/x", "state": "failing" }),
        ),
    ];
    for (path, body) in posts {
        let (st, out) = server.req("POST", path, &alpha, Some(body));
        assert_eq!(st, 404, "POST {path} allowed: {out}");
    }
    let (st, _) = server.req("DELETE", "/v1/orgs/bravo/repos/secret", &alpha, None);
    assert_eq!(st, 404);
    let (st, _) = server.req(
        "PATCH",
        "/v1/orgs/bravo/repos/secret",
        &alpha,
        Some(serde_json::json!({ "default_branch": "main" })),
    );
    assert_eq!(st, 404);
    let (st, _) = server.req(
        "DELETE",
        "/v1/orgs/bravo/repos/secret/protections/main",
        &alpha,
        None,
    );
    assert_eq!(st, 404);
    // The repo is untouched for its owner.
    let (st, body) = server.req(
        "GET",
        "/v1/orgs/bravo/repos/secret/files/creds.txt",
        &bravo,
        None,
    );
    assert_eq!(st, 200);
    assert!(body.to_string().contains(SECRET));

    // Anonymous: private resources answer 401 (ask for creds), not 404,
    // and never content — including repos that do not exist at all (the
    // credentials-first rule masks existence uniformly).
    let (st, _) = server.req("GET", "/v1/orgs/bravo/repos/secret", "", None);
    assert_eq!(st, 401);
    let anon_wire = ureq::get(&format!(
        "{}/bravo/never-existed/info/refs?service=git-upload-pack",
        server.base
    ))
    .call();
    assert!(
        matches!(anon_wire, Err(ureq::Error::Status(401, _))),
        "{anon_wire:?}"
    );

    // Git wire: alpha creds on bravo's repo fail without serving refs;
    // no creds prompts for auth. Either way no data.
    let dest = scratch.path().join("steal");
    let err = gitcli::git_expect_err(
        scratch.path(),
        &[
            "clone",
            &server.authed_url(&alpha, "bravo", "secret"),
            dest.to_str().unwrap(),
        ],
    )
    .expect("cross-org clone must fail");
    assert!(!err.contains(SECRET));
    assert!(!dest.join(".git").exists());

    // Repo-scoped token: reads its own repo, nothing else in the org.
    create_repo(&server, &alpha, "alpha", "other");
    commit_file(&server, &alpha, "alpha", "app", "a.txt", "app-data");
    commit_file(&server, &alpha, "alpha", "other", "o.txt", "other-data");
    let scoped = server.admin_json(&[
        "admin",
        "mint",
        "--org",
        "alpha",
        "--scopes",
        "repo:read",
        "--repo",
        "app",
    ])["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, body) = server.req("GET", "/v1/orgs/alpha/repos/app/files/a.txt", &scoped, None);
    assert_eq!(st, 200, "{body}");
    let (st, body) = server.req(
        "GET",
        "/v1/orgs/alpha/repos/other/files/o.txt",
        &scoped,
        None,
    );
    assert_eq!(st, 404, "repo-scoped token crossed repos: {body}");
    // And it cannot mint tokens or write.
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/alpha/tokens",
        &scoped,
        Some(serde_json::json!({ "scopes": ["org:admin"] })),
    );
    assert!(st == 404 || st == 401, "scoped token minted a token: {st}");
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/alpha/repos/app/commits",
        &scoped,
        Some(serde_json::json!({ "message": "x", "operations": [] })),
    );
    assert_eq!(st, 404, "read scope wrote");

    // A garbage token is 401 everywhere, even for real resources.
    let (st, _) = server.req("GET", "/v1/orgs/alpha/repos/app", "weft_bogus_bogus", None);
    assert_eq!(st, 401);
}

#[test]
fn revocation_is_immediate() {
    let minio = Minio::shared();
    let bucket = minio.bucket("hard-revoke");
    let scratch = Scratch::new("revoke");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    create_repo(&server, &admin, "acme", "app");

    let minted = server.admin_json(&["admin", "mint", "--org", "acme", "--scopes", "org:read"]);
    let (tid, tok) = (
        minted["id"].as_str().unwrap().to_string(),
        minted["token"].as_str().unwrap().to_string(),
    );
    let (st, _) = server.req("GET", "/v1/orgs/acme/repos", &tok, None);
    assert_eq!(st, 200);
    let (st, _) = server.req(
        "DELETE",
        &format!("/v1/orgs/acme/tokens/{tid}"),
        &admin,
        None,
    );
    assert_eq!(st, 204);
    // No cache to expire: the very next request is refused.
    let (st, _) = server.req("GET", "/v1/orgs/acme/repos", &tok, None);
    assert_eq!(st, 401);
}

/// The 64MB request cap answers 413 before the engine sees a byte.
#[test]
fn oversized_push_is_rejected_413() {
    let minio = Minio::shared();
    let bucket = minio.bucket("hard-413");
    let scratch = Scratch::new("cap");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    create_repo(&server, &admin, "acme", "app");

    // **Write and read at the same time.**
    //
    // The cap is enforced on the streamed body, not on `Content-Length`:
    // the limit layer counts bytes as they arrive and answers 413 once
    // they pass 64MB, so a request that merely *declares* oversize gets
    // no answer at all — the server is still waiting for it. The body has
    // to be sent.
    //
    // That is what made this test unstable. It asserted the code `curl`
    // reported, and curl is still uploading when the answer arrives: it
    // saw `413` alone, `100` under load when an expect-continue interim
    // was the last thing it got, and `000` when the reset beat the
    // response. All three are the same race between the upload finishing
    // and the response being read, and none of them is about the cap.
    //
    // Reading on the main thread while a writer thread pushes the body
    // takes the race out: the status line is read the moment it is sent,
    // whatever the socket does afterwards. Write errors are expected and
    // ignored — the peer stops reading once it has decided.
    let over: usize = 64 * 1024 * 1024 + 1;
    let probe = |path: &str, content_type: &str| -> String {
        use std::io::{Read, Write};
        let addr = server.base.trim_start_matches("http://").to_string();
        let mut sock = std::net::TcpStream::connect(&addr).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {admin}\r\n\
             Content-Type: {content_type}\r\nContent-Length: {over}\r\n\r\n"
        );
        sock.write_all(head.as_bytes()).expect("write head");
        let mut writer = sock.try_clone().expect("clone socket");
        let pump = std::thread::spawn(move || {
            let chunk = vec![b'0'; 1024 * 1024];
            let mut sent = 0usize;
            while sent < over {
                let n = chunk.len().min(over - sent);
                if writer.write_all(&chunk[..n]).is_err() {
                    break; // the server has answered and stopped reading
                }
                sent += n;
            }
            let _ = writer.flush();
        });
        let mut buf = [0u8; 256];
        let n = sock.read(&mut buf).unwrap_or(0);
        let _ = pump.join();
        String::from_utf8_lossy(&buf[..n])
            .split_whitespace()
            .nth(1)
            .unwrap_or("none")
            .to_string()
    };

    assert_eq!(
        probe(
            "/acme/app/git-receive-pack",
            "application/x-git-receive-pack-request"
        ),
        "413",
        "wire push over the cap"
    );
    assert_eq!(
        probe("/v1/orgs/acme/repos/app/commits", "application/json"),
        "413",
        "REST commit over the cap"
    );
}

#[test]
fn readiness_reflects_dependencies() {
    let minio = Minio::shared();
    let bucket = minio.bucket("hard-ready");
    let scratch = Scratch::new("ready");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let resp = ureq::get(&format!("{}/readyz", server.base))
        .call()
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.into_string().unwrap(), "ready\n");

    // A server whose store is unreachable is alive but not ready.
    let dead_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
        // listener dropped: nothing answers this port
    };
    let scratch2 = Scratch::new("ready-dead");
    let dead = spawn_server(
        &format!("http://127.0.0.1:{dead_port}/none"),
        &scratch2,
        &[],
    );
    let err = ureq::get(&format!("{}/readyz", dead.base))
        .timeout(Duration::from_secs(15))
        .call()
        .unwrap_err();
    match err {
        ureq::Error::Status(503, r) => {
            assert!(r.into_string().unwrap().contains("store"));
        }
        other => panic!("expected 503, got {other:?}"),
    }
}

/// Latency regression budgets by store round-trip count — deterministic
/// where wall-clock is noisy. Bounds are ceilings with headroom, not
/// targets; the point is that a refactor that doubles the op count fails.
#[test]
fn store_rtt_budgets() {
    let minio = Minio::shared();
    let bucket = minio.bucket("hard-rtt");
    let scratch = Scratch::new("rtt");

    // Interpose the counting proxy between server and MinIO.
    let upstream = bucket
        .base_url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let proxy = CountingProxy::start(&upstream);
    let bucket_name = bucket.base_url.rsplit('/').next().unwrap();
    // Background workers are quiesced so bracketed counts only see the
    // request under test.
    let server = spawn_server(
        &format!("{}/{bucket_name}", proxy.url),
        &scratch,
        &[
            ("STRATUM_COMPACT_POLL_SECS", "86400".into()),
            ("STRATUM_AUDIT_SHIP_SECS", "86400".into()),
            ("STRATUM_BILLING_ROLLUP_SECS", "86400".into()),
            // The storage sweep's first tick fires at boot and reads
            // every repository's manifest; landing inside the create
            // window, it read as a third store op against a budget of
            // two — on CI, not on a laptop, because the tick and the
            // create raced. Off, like every other background worker
            // here: this test counts the request under test.
            ("STRATUM_STORAGE_SWEEP_SECS", "0".into()),
        ],
    );
    let admin = server.bootstrap_org("acme");

    // R1: repo create is one DB insert + one conditional PUT.
    let before = proxy.count();
    let t0 = Instant::now();
    create_repo(&server, &admin, "acme", "app");
    let create_ops = proxy.since(before);
    let create_wall = t0.elapsed();
    assert!(
        create_ops <= 2,
        "repo create used {create_ops} store ops (budget 2)"
    );
    // Generous wall-clock canary for the <100ms design target: loopback
    // MinIO through a proxy plus process scheduling — 2s means "not
    // pathological", the RTT count above is the real regression gate.
    assert!(
        create_wall < Duration::from_secs(2),
        "create took {create_wall:?}"
    );

    // R2: a REST commit stays in single-digit store ops.
    let before = proxy.count();
    commit_file(
        &server,
        &admin,
        "acme",
        "app",
        "src/main.rs",
        "fn main() {}\n",
    );
    let commit_ops = proxy.since(before);
    assert!(
        commit_ops <= 12,
        "commit used {commit_ops} store ops (budget 12)"
    );

    // R3: a point read of a small file.
    let before = proxy.count();
    let (st, body) = server.req(
        "GET",
        "/v1/orgs/acme/repos/app/files/src/main.rs",
        &admin,
        None,
    );
    assert_eq!(st, 200, "{body}");
    let read_ops = proxy.since(before);
    assert!(
        read_ops <= 8,
        "file read used {read_ops} store ops (budget 8)"
    );
}

/// Load smoke: one repo, 8 concurrent clones, every product fsck-clean.
#[test]
fn concurrent_clones_all_fsck_clean() {
    let minio = Minio::shared();
    let bucket = minio.bucket("hard-load");
    let scratch = Scratch::new("load");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    create_repo(&server, &admin, "acme", "app");
    for i in 0..5 {
        commit_file(
            &server,
            &admin,
            "acme",
            "app",
            &format!("f{i}.txt"),
            &format!("content {i}\n"),
        );
    }

    let url = server.authed_url(&admin, "acme", "app");
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let url = url.clone();
                let dest = scratch.path().join(format!("clone-{i}"));
                s.spawn(move || {
                    gitcli::clone_and_fsck(&url, &dest);
                })
            })
            .collect();
        for h in handles {
            h.join().expect("concurrent clone failed");
        }
    });
}

// ---------------------------------------------------------------------
// The routes this branch added: contributors, the check reads, the
// poller's door, and the commit-scoped intake.
//
// Their happy paths are asserted elsewhere. What is here is the half
// that only matters when somebody is trying: that none of them is a new
// way to learn a private repository exists, that none of them accepts a
// credential which may not reach it, and that a run id is a fact about
// one repository rather than a global name.
// ---------------------------------------------------------------------

fn create_public_repo(server: &Server, token: &str, org: &str, name: &str) {
    let (st, out) = server.req(
        "POST",
        &format!("/v1/orgs/{org}/repos"),
        token,
        Some(serde_json::json!({ "name": name, "public": true })),
    );
    assert_eq!(st, 201, "{out}");
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// `sha256=<hex>` the way the intake computes it. Computed here rather
/// than borrowed from the server so a test signing correctly proves the
/// wire contract, not that one copy of the code agrees with itself.
fn hmac_sig(secret: &str, body: &str) -> String {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    format!(
        "sha256={}",
        stratum_store::pack::hex(&mac.finalize().into_bytes())
    )
}

/// A POST with a raw body and a signature and **no Authorization
/// header** — the posture a CI runner is actually in.
fn post_signed(
    server: &Server,
    path: &str,
    signature: Option<&str>,
    body: &str,
) -> (u16, serde_json::Value) {
    let mut r =
        ureq::post(&format!("{}{path}", server.base)).set("Content-Type", "application/json");
    if let Some(s) = signature {
        r = r.set("X-Weft-Signature-256", s);
    }
    let resp = match r.send_string(body) {
        Ok(x) => x,
        Err(ureq::Error::Status(_, x)) => x,
        Err(e) => panic!("transport POST {path}: {e}"),
    };
    let status = resp.status();
    let text = resp.into_string().unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

fn mint_intake_secret(server: &Server, token: &str, org: &str, repo: &str) -> String {
    let (st, out) = server.req(
        "POST",
        &format!("/v1/orgs/{org}/repos/{repo}/ci/secret"),
        token,
        None,
    );
    assert_eq!(st, 201, "{out}");
    out["secret"].as_str().expect("a secret").to_string()
}

/// Record one commit-scoped run through the signed intake, and return
/// its id.
fn record_run(server: &Server, org: &str, repo: &str, secret: &str, commit: &str) -> String {
    let body = serde_json::json!({
        "commit": commit,
        "name": "ci/tests",
        "state": "passing",
        "sent_at": now_ms(),
    })
    .to_string();
    let (st, out) = post_signed(
        server,
        &format!("/v1/orgs/{org}/repos/{repo}/ci/checks"),
        Some(&hmac_sig(secret, &body)),
        &body,
    );
    assert_eq!(st, 200, "{out}");
    out["id"].as_str().expect("a run id").to_string()
}

/// Every new read answers a stranger about a private repository with the
/// **same bytes** it answers them about a repository that never existed.
///
/// Asserted as an equality between the two responses rather than against
/// a literal status code, because the literal is the part that is easy
/// to keep green while the property rots: a route that grew a 400 about
/// a malformed `limit` before its auth check would still be "404 on a
/// private repo" for the plain URL and an existence oracle for every
/// other one. Comparing the pair catches that; comparing to `404` does
/// not.
#[test]
fn the_new_reads_mask_a_private_repository_exactly_as_a_missing_one() {
    let minio = Minio::shared();
    let bucket = minio.bucket("hard-newmask");
    let scratch = Scratch::new("newmask");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let alpha = server.bootstrap_org("alpha");
    let bravo = server.bootstrap_org("bravo");
    create_repo(&server, &alpha, "alpha", "secret");
    commit_file(&server, &alpha, "alpha", "secret", "creds.txt", SECRET);
    // A real run in it, so "empty" is never what makes the two answers
    // match: the private repository genuinely has something to leak.
    let secret_key = mint_intake_secret(&server, &alpha, "alpha", "secret");
    let run = record_run(
        &server,
        "alpha",
        "secret",
        &secret_key,
        "3f2a1b4c5d6e7f8091a2b3c4d5e6f708192a3b4c",
    );

    // Every route this branch added under a repository, plus the shapes
    // of them that carry arguments — an argument is a second place a
    // handler can decide to answer before it decides who is asking.
    let suffixes = [
        "/contributors".to_string(),
        "/contributors?limit=5".to_string(),
        "/contributors?limit=not-a-number".to_string(),
        "/checks/runs".to_string(),
        "/checks/runs?branch=main&state=passing&limit=5".to_string(),
        // A `state` the vocabulary does not have, and a `limit` that is
        // not a number: both are 400s on a repository the caller may
        // read, and a 400 here would be one only a real repository could
        // produce.
        "/checks/runs?state=success".to_string(),
        "/checks/runs?limit=NaN".to_string(),
        "/checks/runs?before=NaN".to_string(),
        format!("/checks/runs/{run}"),
        "/checks/runs/01hxrunthatneverwas".to_string(),
        "/ci/poll".to_string(),
        "/meta".to_string(),
    ];
    // The two postures a stranger can be in. They answer differently
    // from each other — 401 for no credential, 404 for a credential that
    // does not reach — and that is fine; what may not differ is the
    // answer *within* a posture.
    //
    // A third posture, an unparseable token, is deliberately not here:
    // it does not obey this rule anywhere in the API, and
    // `an_unparseable_token_tells_a_stranger_whether_a_private_repository_exists`
    // below is the test that says so.
    let strangers = [("anonymous", ""), ("another org's admin", bravo.as_str())];
    for (who, token) in strangers {
        for suffix in &suffixes {
            let private = server.req(
                "GET",
                &format!("/v1/orgs/alpha/repos/secret{suffix}"),
                token,
                None,
            );
            let missing = server.req(
                "GET",
                &format!("/v1/orgs/alpha/repos/never-existed{suffix}"),
                token,
                None,
            );
            assert_eq!(
                private, missing,
                "{who} can tell alpha/secret{suffix} apart from a repository \
                 that does not exist"
            );
            assert!(
                !private.1.to_string().contains(SECRET),
                "GET {suffix} leaked content to {who}"
            );
            assert!(
                private.0 < 500,
                "GET {suffix} answered {} to {who}",
                private.0
            );
        }
        // The write side of the poller's door asks the same question.
        let private = server.req("POST", "/v1/orgs/alpha/repos/secret/ci/poll", token, None);
        let missing = server.req(
            "POST",
            "/v1/orgs/alpha/repos/never-existed/ci/poll",
            token,
            None,
        );
        assert_eq!(
            private, missing,
            "{who} can tell alpha/secret from a missing repo through POST /ci/poll"
        );
    }

    // The masking is not a route that refuses everybody: its owner still
    // reads every one of them.
    for suffix in ["/contributors", "/checks/runs", "/ci/poll", "/meta"] {
        let (st, body) = server.req(
            "GET",
            &format!("/v1/orgs/alpha/repos/secret{suffix}"),
            &alpha,
            None,
        );
        assert_eq!(st, 200, "the owner cannot read {suffix}: {body}");
    }
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// Who may reach each new route, credential by credential — and, for the
/// poller, that being refused for want of a scope tells the caller
/// nothing about whether the repository is there.
#[test]
fn the_new_routes_refuse_every_credential_that_may_not_reach_them() {
    let minio = Minio::shared();
    let bucket = minio.bucket("hard-newauthz");
    let scratch = Scratch::new("newauthz");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let alpha = server.bootstrap_org("alpha");
    let bravo = server.bootstrap_org("bravo");
    create_repo(&server, &alpha, "alpha", "app");
    create_repo(&server, &alpha, "alpha", "other");
    commit_file(&server, &alpha, "alpha", "app", "a.txt", "app-data");

    let mint = |scopes: &str, repo: Option<&str>| -> String {
        let mut args = vec!["admin", "mint", "--org", "alpha", "--scopes", scopes];
        if let Some(r) = repo {
            args.push("--repo");
            args.push(r);
        }
        server.admin_json(&args)["token"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // A viewer: org-wide read and nothing more. `org:read` grants
    // `repo:read`, so this is the credential that must be able to *see*
    // the checks and must not be able to ask for a poll.
    let viewer = mint("org:read", None);
    let scoped_write = mint("repo:write", Some("app"));
    let scoped_read_elsewhere = mint("repo:read", Some("other"));

    // --- the reads -----------------------------------------------------
    for suffix in ["/contributors", "/checks/runs", "/ci/poll", "/meta"] {
        let path = format!("/v1/orgs/alpha/repos/app{suffix}");
        for (who, token, want) in [
            ("the org admin", alpha.as_str(), 200u16),
            ("a viewer", viewer.as_str(), 200),
            (
                "a repo:write token for this repo",
                scoped_write.as_str(),
                200,
            ),
            (
                "a repo:read token for another repo",
                scoped_read_elsewhere.as_str(),
                404,
            ),
            ("another org's admin", bravo.as_str(), 404),
            ("anonymous", "", 401),
        ] {
            let (st, body) = server.req("GET", &path, token, None);
            assert_eq!(st, want, "GET {suffix} as {who}: {body}");
        }
    }

    // --- POST /ci/poll, which needs repo:write -------------------------
    //
    // 400 is the *pass* here: it is the answer from past the
    // authorization gate, saying this repository has no GitHub origin to
    // poll. Anything that reaches it has been allowed through.
    let path = "/v1/orgs/alpha/repos/app/ci/poll";
    for (who, token, want) in [
        ("the org admin", alpha.as_str(), 400u16),
        (
            "a repo:write token for this repo",
            scoped_write.as_str(),
            400,
        ),
        ("a viewer", viewer.as_str(), 404),
        (
            "a repo:read token for another repo",
            scoped_read_elsewhere.as_str(),
            404,
        ),
        ("another org's admin", bravo.as_str(), 404),
        ("anonymous", "", 401),
    ] {
        let (st, body) = server.req("POST", path, token, None);
        assert_eq!(st, want, "POST /ci/poll as {who}: {body}");
    }

    // And the read-only refusal is not an existence oracle: "you may not
    // poll this" and "there is nothing here to poll" are the same bytes.
    let refused = server.req("POST", path, &viewer, None);
    let absent = server.req(
        "POST",
        "/v1/orgs/alpha/repos/never-existed/ci/poll",
        &viewer,
        None,
    );
    assert_eq!(
        refused, absent,
        "a viewer refused the poll learns that alpha/app exists"
    );

    // Nothing above minted a job or a row: the repository is exactly as
    // it was, and the server is still serving.
    let (st, body) = server.req("GET", "/v1/orgs/alpha/repos/app/ci/poll", &alpha, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["polled"], serde_json::json!(false), "{body}");
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A run id is a fact about one repository, and reading it through any
/// other path is a 404 that cannot be told from an id nobody ever issued.
///
/// `checks_api::get` claims this is true because `checks::get` scopes on
/// `repo_id` inside the query rather than fetching the row and comparing
/// afterwards. The observable difference between the two designs is
/// exactly this equality, so this is the test that holds the claim.
#[test]
fn a_check_run_id_is_scoped_to_its_repository_and_its_org() {
    let minio = Minio::shared();
    let bucket = minio.bucket("hard-runscope");
    let scratch = Scratch::new("runscope");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let alpha = server.bootstrap_org("alpha");
    let bravo = server.bootstrap_org("bravo");
    // Public on both sides, so that any 404 below is about the *run*
    // rather than about the repository being unreadable — a private repo
    // would answer 404 to everything and the test would pass vacuously.
    create_public_repo(&server, &alpha, "alpha", "app");
    create_public_repo(&server, &alpha, "alpha", "other");
    create_public_repo(&server, &bravo, "bravo", "app");

    let commit = "3f2a1b4c5d6e7f8091a2b3c4d5e6f708192a3b4c";
    let alpha_run = record_run(
        &server,
        "alpha",
        "app",
        &mint_intake_secret(&server, &alpha, "alpha", "app"),
        commit,
    );
    let bravo_run = record_run(
        &server,
        "bravo",
        "app",
        &mint_intake_secret(&server, &bravo, "bravo", "app"),
        commit,
    );
    assert_ne!(alpha_run, bravo_run, "two runs, two ids");

    // It reads where it belongs, to anybody, because the repo is public.
    let (st, body) = server.req(
        "GET",
        &format!("/v1/orgs/alpha/repos/app/checks/runs/{alpha_run}"),
        "",
        None,
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["id"], serde_json::json!(alpha_run));

    // The id nobody ever issued, which is the answer every other path
    // has to match byte for byte.
    let never = server.req(
        "GET",
        "/v1/orgs/alpha/repos/app/checks/runs/01hxrunthatneverwas",
        "",
        None,
    );
    assert_eq!(never.0, 404, "{:?}", never.1);

    // Each case is asked by callers who may read *that* repository —
    // otherwise the 404 would be the repository's rather than the run's,
    // and the equality would be asserting the wrong thing.
    for (why, path, tokens) in [
        (
            "another repository in the same org",
            format!("/v1/orgs/alpha/repos/other/checks/runs/{alpha_run}"),
            ["", alpha.as_str()],
        ),
        (
            "another org's repository of the same name",
            format!("/v1/orgs/bravo/repos/app/checks/runs/{alpha_run}"),
            ["", bravo.as_str()],
        ),
        (
            "bravo's run read through alpha's path",
            format!("/v1/orgs/alpha/repos/app/checks/runs/{bravo_run}"),
            ["", alpha.as_str()],
        ),
    ] {
        for token in tokens {
            let got = server.req("GET", &path, token, None);
            assert_eq!(
                got, never,
                "a run read through {why} is distinguishable from one that \
                 was never issued"
            );
        }
    }

    // Both runs are still exactly where they were: nothing above moved a
    // row between repositories.
    for (org, id) in [("alpha", &alpha_run), ("bravo", &bravo_run)] {
        let (st, body) = server.req(
            "GET",
            &format!("/v1/orgs/{org}/repos/app/checks/runs/{id}"),
            "",
            None,
        );
        assert_eq!(st, 200, "{body}");
    }
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// FINDING, and this test is red on purpose.
///
/// An **unparseable** bearer token is an existence oracle for private
/// repositories, on every repo-scoped REST route in the API — the ones
/// this branch added and the ones that predate it alike.
///
/// The mechanism is the order of two checks in `app::rest_repo_auth`. It
/// resolves the repository first, through `repo_or_masked`, which for a
/// repository that is not there answers `authx::masked(headers)` — and
/// masked, seeing *some* credential on the request, answers **404**. Only
/// if the repository does resolve does it go on to
/// `authx::principal_opt`, which rejects a token it cannot parse with a
/// **401**. So:
///
/// * `GET /v1/orgs/alpha/repos/secret` with `Bearer weft_bogus_bogus`
///   → 401 "authentication required", and the repository exists;
/// * `GET /v1/orgs/alpha/repos/never-existed` with the same header
///   → 404 "not found", and it does not.
///
/// Anonymously the two are both 401, and with a valid foreign token both
/// 404, which is the rule `authx::masked` documents. A *malformed* token
/// is the third posture, and it is the one nothing has to prove to be
/// in: an attacker enumerating a namespace's private repositories needs
/// no credential at all, and a made-up one is strictly better than none.
///
/// Not fixed here: the fix is in `authx`/`app`, which this track does not
/// own. The shape of it is that a credential which does not authenticate
/// should count as no credential *for the purpose of masking* — the 401
/// has to be decided after the repository has been resolved and found
/// unreadable, not before, so that "your token is nonsense" cannot also
/// be a statement about which repositories exist.
#[test]
fn an_unparseable_token_tells_a_stranger_whether_a_private_repository_exists() {
    let minio = Minio::shared();
    let bucket = minio.bucket("hard-badtok");
    let scratch = Scratch::new("badtok");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let alpha = server.bootstrap_org("alpha");
    create_repo(&server, &alpha, "alpha", "secret");

    // The routes this branch added, and one that predates all of them —
    // the finding is about `rest_repo_auth`, not about any of them.
    for suffix in [
        "",
        "/contributors",
        "/checks/runs",
        "/checks/runs/01hxrunthatneverwas",
        "/ci/poll",
        "/meta",
    ] {
        let exists = server.req(
            "GET",
            &format!("/v1/orgs/alpha/repos/secret{suffix}"),
            "weft_bogus_bogus",
            None,
        );
        let absent = server.req(
            "GET",
            &format!("/v1/orgs/alpha/repos/never-existed{suffix}"),
            "weft_bogus_bogus",
            None,
        );
        assert_eq!(
            exists, absent,
            "a made-up token distinguishes alpha/secret{suffix} from a \
             repository that does not exist"
        );
    }
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// **A credential that dies between the round trips of one operation.**
///
/// `revocation_is_immediate` proves the next REST call is refused. A git
/// operation is not one call: a clone is an advert, then `ls-refs`, then
/// `fetch`, and a push is an advert then the RPC — each a separate HTTP
/// request that has to be authorised on its own. The interesting moment
/// is the middle one, because it is where "authorised at the door" and
/// "authorised at the till" come apart. A front that treated the advert
/// as the check and the RPC as a continuation of it would let a
/// just-revoked token walk out with the whole repository, and no
/// existing test looks there.
///
/// Both directions, because they fail differently: a **revoked** token
/// is refused outright, and a **downgraded** one still authenticates but
/// must lose the authority it had — which is the subtler of the two,
/// since the caller is still a real principal the server is happy to
/// talk to.
#[test]
fn a_credential_revoked_between_round_trips_does_not_finish_the_operation() {
    let minio = Minio::shared();
    let bucket = minio.bucket("hard-midflight");
    let scratch = Scratch::new("midflight");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    create_repo(&server, &admin, "acme", "app");
    commit_file(&server, &admin, "acme", "app", "seed.txt", "seed\n");

    // A read token, used for the advert exactly as git would.
    let minted = server.admin_json(&["admin", "mint", "--org", "acme", "--scopes", "repo:read"]);
    let (tid, tok) = (
        minted["id"].as_str().unwrap().to_string(),
        minted["token"].as_str().unwrap().to_string(),
    );
    let advert = |t: &str| -> u16 {
        match ureq::get(&format!(
            "{}/acme/app/info/refs?service=git-upload-pack",
            server.base
        ))
        .set("Authorization", &format!("Bearer {t}"))
        .set("Git-Protocol", "version=2")
        .call()
        {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(c, _)) => c,
            Err(e) => panic!("transport: {e}"),
        }
    };
    let rpc = |t: &str, path: &str, ct: &str, body: Vec<u8>| -> u16 {
        match ureq::post(&format!("{}/acme/app/{path}", server.base))
            .set("Authorization", &format!("Bearer {t}"))
            .set("Git-Protocol", "version=2")
            .set("Content-Type", ct)
            .send_bytes(&body)
        {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(c, _)) => c,
            Err(e) => panic!("transport: {e}"),
        }
    };
    fn pkt(line: &str) -> Vec<u8> {
        let mut v = format!("{:04x}", line.len() + 4).into_bytes();
        v.extend_from_slice(line.as_bytes());
        v
    }
    let ls_refs = || -> Vec<u8> {
        let mut b = pkt("command=ls-refs\n");
        b.extend_from_slice(&pkt("object-format=sha1\n"));
        b.extend_from_slice(b"0001");
        b.extend_from_slice(&pkt("ref-prefix refs/heads/\n"));
        b.extend_from_slice(b"0000");
        b
    };

    // The advert succeeds — this is the door.
    assert_eq!(advert(&tok), 200, "the read token could not start a clone");

    // Revoked in the window between the advert and the RPC.
    let (st, _) = server.req(
        "DELETE",
        &format!("/v1/orgs/acme/tokens/{tid}"),
        &admin,
        None,
    );
    assert_eq!(st, 204);

    // The RPC is a new request and must be judged on its own.
    let st = rpc(
        &tok,
        "git-upload-pack",
        "application/x-git-upload-pack-request",
        ls_refs(),
    );
    assert_ne!(
        st, 200,
        "a revoked token finished an operation it had only started — the \
         advert was treated as the authorisation and the RPC as a \
         continuation of it"
    );

    // --- and the second half: a write credential revoked mid-push ----
    let minted = server.admin_json(&[
        "admin",
        "mint",
        "--org",
        "acme",
        "--scopes",
        "repo:read,repo:write",
    ]);
    let (wid, wtok) = (
        minted["id"].as_str().unwrap().to_string(),
        minted["token"].as_str().unwrap().to_string(),
    );
    // The push advert is the door for a write.
    let st = match ureq::get(&format!(
        "{}/acme/app/info/refs?service=git-receive-pack",
        server.base
    ))
    .set("Authorization", &format!("Bearer {wtok}"))
    .set("Git-Protocol", "version=2")
    .call()
    {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(c, _)) => c,
        Err(e) => panic!("transport: {e}"),
    };
    assert_eq!(st, 200, "the write token could not start a push");

    let (st, _) = server.req(
        "DELETE",
        &format!("/v1/orgs/acme/tokens/{wid}"),
        &admin,
        None,
    );
    assert_eq!(st, 204);

    // An empty command set is enough: what is under test is whether the
    // request is authorised at all, not what it would have written.
    let st = rpc(
        &wtok,
        "git-receive-pack",
        "application/x-git-receive-pack-request",
        b"0000".to_vec(),
    );
    assert_ne!(
        st, 200,
        "a revoked write credential still reached the push front"
    );

    // --- the subtler case: authority lowered, not removed ------------
    //
    // A person whose per-repo grant is downgraded between the advert and
    // the RPC. They still authenticate — a real principal the server is
    // happy to talk to — and that is exactly why the RPC has to re-ask
    // what they may *do* rather than trusting that the advert already
    // said yes.
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "vic@acme.test",
            "--name",
            "Vic",
            "--password",
            "a long enough password",
            "--role",
            "viewer",
        ])
        .expect("create vic");
    let (st, members) = server.req("GET", "/v1/orgs/acme/members", &admin, None);
    assert_eq!(st, 200, "{members}");
    let vic_id = members["members"]
        .as_array()
        .expect("members")
        .iter()
        .find(|m| m["email"] == serde_json::json!("vic@acme.test"))
        .and_then(|m| m["user_id"].as_str().or_else(|| m["id"].as_str()))
        .expect("vic's id")
        .to_string();
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/app/grants",
        &admin,
        Some(serde_json::json!({"user_id": vic_id, "role": "member"})),
    );
    assert!(st == 200 || st == 204, "{st} {out}");

    let resp = ureq::post(&format!("{}/v1/auth/login", server.base))
        .set("Content-Type", "application/json")
        .send_string(
            &serde_json::json!({"email":"vic@acme.test","password":"a long enough password"})
                .to_string(),
        )
        .expect("sign in");
    let cookie = resp
        .header("set-cookie")
        .and_then(|c| c.split(';').next())
        .expect("a session cookie")
        .to_string();
    // A token *she* minted, so it carries her authority rather than the
    // org's. The git wire takes tokens, not browser sessions — git does
    // not send cookies — so this is also the only shape of credential
    // the case can be tested with.
    let minted = match ureq::post(&format!("{}/v1/orgs/acme/tokens", server.base))
        .set("Cookie", &cookie)
        .set("Content-Type", "application/json")
        .send_string(
            &serde_json::json!({"label": "vic-push", "scopes": ["repo:read", "repo:write"]})
                .to_string(),
        ) {
        Ok(r) => serde_json::from_str::<serde_json::Value>(&r.into_string().unwrap_or_default())
            .expect("token json"),
        Err(ureq::Error::Status(c, r)) => {
            panic!("mint as vic: {c} {}", r.into_string().unwrap_or_default())
        }
        Err(e) => panic!("transport: {e}"),
    };
    let vtok = minted["token"].as_str().expect("a token").to_string();
    let advert_as = |t: &str| -> u16 {
        match ureq::get(&format!(
            "{}/acme/app/info/refs?service=git-receive-pack",
            server.base
        ))
        .set("Authorization", &format!("Bearer {t}"))
        .set("Git-Protocol", "version=2")
        .call()
        {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(c, _)) => c,
            Err(e) => panic!("transport: {e}"),
        }
    };
    assert_eq!(advert_as(&vtok), 200, "a writer could not start a push");

    // Lowered to read while the push is in flight.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/app/grants",
        &admin,
        Some(serde_json::json!({"user_id": vic_id, "role": "viewer"})),
    );
    assert!(st == 200 || st == 204, "{st} {out}");

    let st = match ureq::post(&format!("{}/acme/app/git-receive-pack", server.base))
        .set("Authorization", &format!("Bearer {vtok}"))
        .set("Git-Protocol", "version=2")
        .set("Content-Type", "application/x-git-receive-pack-request")
        .send_bytes(b"0000")
    {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(c, _)) => c,
        Err(e) => panic!("transport: {e}"),
    };
    assert_ne!(
        st, 200,
        "a downgraded writer still reached the push front: the advert was          taken as the authorisation for the RPC behind it"
    );
    // …and they can still read, so what was lost is the authority and
    // not the account.
    assert_eq!(
        match ureq::get(&format!(
            "{}/acme/app/info/refs?service=git-upload-pack",
            server.base
        ))
        .set("Authorization", &format!("Bearer {vtok}"))
        .set("Git-Protocol", "version=2")
        .call()
        {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(c, _)) => c,
            Err(e) => panic!("transport: {e}"),
        },
        200,
        "the downgrade took reading away too"
    );

    // The repository is unharmed and still serves a live credential.
    assert_eq!(advert(&admin), 200);
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&server.authed_url(&admin, "acme", "app"), &clone);
}
