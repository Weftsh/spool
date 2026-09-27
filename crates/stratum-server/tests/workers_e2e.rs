//! Workers end-to-end: compaction (WAL fold restoring the depth-1 fast
//! path), epoch GC, metrics + usage rollup, repo webhooks, the free-tier
//! quota, and audit shipping to object storage.

use hmac::Mac;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;
use stratum_store::{LatencyModel, ObjectStore};
use stratum_testkit::closure::assert_closed;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::{wait_for, wait_until, Minio, Server};

fn spawn_server(store_url: &str, scratch: &Scratch, extra: &[(&str, String)]) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint("workers")
        .data_dir(scratch.path().join("data"))
        .envs(extra)
        .start()
}

fn commit(server: &Server, token: &str, rp: &str, i: usize) -> String {
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/commits"),
        token,
        Some(serde_json::json!({
            "message": format!("step {i}"),
            "operations": [
                { "op": "put", "path": format!("file-{}.txt", i % 3), "content": format!("v{i}\n") },
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");
    out["commit"].as_str().unwrap().to_string()
}

#[test]
fn compaction_folds_wal_and_gc_reclaims_epochs() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-compact");
    let scratch = Scratch::new("compact");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_COMPACT_POLL_SECS", "0".into())], // manual only
    );
    let admin = server.bootstrap_org("acme");
    let (st, repo) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201);
    let rp = "/v1/orgs/acme/repos/app";
    let prefix = format!(
        "o/{}/r/{}/prod",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );

    // Under threshold: NotNeeded.
    for i in 0..3 {
        commit(&server, &admin, rp, i);
    }
    let (st, out) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["outcome"], "NotNeeded");

    // Depth-1 leaves the precomputed path while the WAL is non-empty —
    // and still **serves a clone**.
    //
    // This asserted that the clone *failed*, which is what the engine
    // did: off the precomputed path it returned an error and the fronts
    // turned that into a 500. So the spec this cited was about which
    // path answers, and the test had quietly promoted "no path answers"
    // into expected behaviour — pinning a repository that could not be
    // shallow-cloned for the whole of its early life, which is exactly
    // when CI is pointed at it.
    //
    // Off the precomputed path the answer is now the full clone plan: a
    // correct superset, unshallow, at the tip. What is asserted is what
    // the user gets.
    let url = server.authed_url(&admin, "acme", "app");
    let shallow_pre = scratch.path().join("shallow-pre");
    gitcli::git(
        scratch.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &url,
            shallow_pre.to_str().unwrap(),
        ],
    );
    gitcli::fsck(&shallow_pre);
    assert_eq!(
        std::fs::read_to_string(shallow_pre.join("file-2.txt")).unwrap(),
        "v2\n",
        "a depth-1 clone off the precomputed path is not at the tip"
    );

    for i in 3..9 {
        commit(&server, &admin, rp, i);
    }
    let (st, out) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["outcome"], "Compacted", "{out}");

    // Post-compaction: full clone fsck-clean, content current, depth-1
    // served from the fresh snapshot again.
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&url, &clone);
    assert_eq!(
        std::fs::read_to_string(clone.join("file-2.txt")).unwrap(),
        "v8\n"
    );
    let shallow = scratch.path().join("shallow");
    gitcli::git(
        scratch.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &url,
            shallow.to_str().unwrap(),
        ],
    );
    assert_eq!(
        gitcli::git(&shallow, &["rev-list", "--count", "HEAD"]).trim(),
        "1"
    );

    // The old epoch is now unreferenced: GC with a zero grace reclaims it.
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let epochs_before: std::collections::HashSet<String> = store
        .list(&format!("{prefix}/"))
        .unwrap()
        .into_iter()
        .filter_map(|(k, _)| {
            k.strip_prefix(&format!("{prefix}/"))
                .and_then(|r| r.split_once('/'))
                .map(|(e, _)| e.to_string())
        })
        .collect();
    assert!(epochs_before.len() >= 2, "{epochs_before:?}");
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/gc"),
        &admin,
        Some(serde_json::json!({ "grace_secs": 0 })),
    );
    assert_eq!(st, 200, "{out}");
    assert!(out["epochs_deleted"].as_u64().unwrap() >= 1, "{out}");
    // Serving still works after the sweep.
    let clone2 = scratch.path().join("clone2");
    gitcli::clone_and_fsck(&url, &clone2);

    // A sweep that reclaimed an epoch the surviving manifest still points
    // into would clone fine from the client's cache and be broken here.
    assert_closed(&bucket.base_url);
}

#[test]
fn metrics_usage_and_prometheus() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-metrics");
    let scratch = Scratch::new("metrics");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_BILLING_ROLLUP_SECS", "1".into())],
    );
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    commit(&server, &admin, rp, 0);

    let url = server.authed_url(&admin, "acme", "app");
    for i in 0..2 {
        let d = scratch.path().join(format!("c{i}"));
        gitcli::clone_and_fsck(&url, &d);
    }
    // Incremental fetch (with haves).
    let c0 = scratch.path().join("c0");
    commit(&server, &admin, rp, 1);
    gitcli::git(&c0, &["fetch", "-q", "origin"]);

    // Metrics land asynchronously. Wait for the two counts the
    // assertions below actually read.
    let m = wait_for(
        "the two clones and the fetch to be counted",
        Duration::from_secs(10),
        || {
            let (st, m) = server.req("GET", &format!("{rp}/metrics"), &admin, None);
            assert_eq!(st, 200);
            let clones = m["kinds"]["clone"]["count"].as_u64().unwrap_or(0);
            let fetches = m["kinds"]["fetch"]["count"].as_u64().unwrap_or(0);
            (clones >= 2 && fetches >= 1).then_some(m)
        },
    );
    assert!(m["kinds"]["clone"]["bytes"].as_u64().unwrap() > 0);
    assert!(m["kinds"]["clone"]["p50_ms"].is_number());

    // CSV export (the renewal artifact).
    let resp = ureq::get(&format!("{}{rp}/metrics?format=csv", server.base))
        .set("Authorization", &format!("Bearer {admin}"))
        .call()
        .unwrap();
    let csv = resp.into_string().unwrap();
    assert!(csv.starts_with("kind,count,bytes"));
    assert!(csv.contains("clone,"));

    // Usage rollup (billing worker on a 1s interval —
    // `STRATUM_BILLING_ROLLUP_SECS` is whole seconds, so a tick is the
    // floor on how soon this can be true).
    let u = wait_for(
        "the rollup to fold today's requests",
        Duration::from_secs(10),
        || {
            let (st, u) = server.req("GET", "/v1/orgs/acme/usage", &admin, None);
            assert_eq!(st, 200);
            (u["days"][0]["requests"].as_i64().unwrap_or(0) > 0).then_some(u)
        },
    );
    assert_eq!(u["plan"], "free");
    assert!(u["days"][0]["active_repos"].as_i64().unwrap() >= 1);

    // Prometheus surface.
    let text = ureq::get(&format!("{}/metrics", server.base))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(text.contains("stratum_requests_total"));
}

/// Minimal webhook receiver: captures (signature, body) pairs.
fn webhook_receiver() -> (String, mpsc::Receiver<(String, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let mut header_end = 0;
                while header_end == 0 {
                    let Ok(n) = s.read(&mut tmp) else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = p + 4;
                    }
                }
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let clen: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap_or(0))
                    })
                    .unwrap_or(0);
                while buf.len() < header_end + clen {
                    let Ok(n) = s.read(&mut tmp) else { return };
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body = String::from_utf8_lossy(&buf[header_end..]).to_string();
                let sig = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .starts_with("x-weft-signature-256:")
                            .then(|| l.split_once(':').unwrap().1.trim().to_string())
                    })
                    .unwrap_or_default();
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = tx.send((sig, body));
            });
        }
    });
    (format!("http://{addr}/hook"), rx)
}

#[test]
fn repo_webhooks_deliver_signed_events() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-hooks");
    let scratch = Scratch::new("hooks");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";

    let (hook_url, rx) = webhook_receiver();
    let (st, sub) = server.req(
        "POST",
        &format!("{rp}/webhooks"),
        &admin,
        Some(serde_json::json!({ "url": hook_url })),
    );
    assert_eq!(st, 201, "{sub}");
    let secret = sub["secret"].as_str().unwrap().to_string();

    // REST commit → signed delivery.
    let c = commit(&server, &admin, rp, 0);
    let (sig, body) = rx.recv_timeout(Duration::from_secs(10)).expect("delivery");
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let want = format!(
        "sha256={}",
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    assert_eq!(sig, want, "delivery must be HMAC-signed");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["event"], "push");
    assert_eq!(v["payload"]["commit"].as_str().unwrap(), c);

    // Wire push → another delivery.
    let url = server.authed_url(&admin, "acme", "app");
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&url, &clone);
    std::fs::write(clone.join("pushed.txt"), "x\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "push"]);
    gitcli::git(&clone, &["push", "-q", "origin", "main"]);
    let (_, body) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("push delivery");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["payload"]["via"], "git");

    // Subscription management: list shows it, delete removes it, and a
    // non-http URL is refused at create.
    let (st, listed) = server.req("GET", &format!("{rp}/webhooks"), &admin, None);
    assert_eq!(st, 200);
    let subs = listed["subscriptions"].as_array().unwrap();
    assert_eq!(subs.len(), 1);
    let id = subs[0]["id"].as_str().unwrap().to_string();
    let (st, _) = server.req(
        "POST",
        &format!("{rp}/webhooks"),
        &admin,
        Some(serde_json::json!({ "url": "ftp://nope" })),
    );
    assert_eq!(st, 400);
    let (st, _) = server.req("DELETE", &format!("{rp}/webhooks/{id}"), &admin, None);
    assert_eq!(st, 204);
    let (st, listed) = server.req("GET", &format!("{rp}/webhooks"), &admin, None);
    assert_eq!(st, 200);
    assert!(listed["subscriptions"].as_array().unwrap().is_empty());
    // Deleting an unknown id is a 404, not an error.
    let (st, _) = server.req("DELETE", &format!("{rp}/webhooks/{id}"), &admin, None);
    assert_eq!(st, 404);
}

/// The GC worker loop: with the interval on and a zero grace window, a
/// deleted repo's storage prefix is swept and active repos survive the
/// same sweep untouched.
#[test]
fn gc_worker_sweeps_deleted_repo_storage() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-gcworker");
    let scratch = Scratch::new("gcworker");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_GC_SECS", "1".into()),
            ("STRATUM_GC_GRACE_SECS", "0".into()),
        ],
    );
    let admin = server.bootstrap_org("acme");
    let (st, doomed) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "doomed" })),
    );
    assert_eq!(st, 201);
    let (st, keeper) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "keeper" })),
    );
    assert_eq!(st, 201);
    commit(&server, &admin, "/v1/orgs/acme/repos/doomed", 0);
    commit(&server, &admin, "/v1/orgs/acme/repos/keeper", 0);

    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let org_id = doomed["org_id"].as_str().unwrap();
    let doomed_prefix = format!("o/{org_id}/r/{}/", doomed["id"].as_str().unwrap());
    let keeper_prefix = format!("o/{org_id}/r/{}/", keeper["id"].as_str().unwrap());
    assert!(!store.list(&doomed_prefix).unwrap().is_empty());

    let (st, _) = server.req("DELETE", "/v1/orgs/acme/repos/doomed", &admin, None);
    assert_eq!(st, 204);
    wait_until(
        "the gc worker to sweep the deleted repo's prefix",
        Duration::from_secs(20),
        || store.list(&doomed_prefix).unwrap().is_empty(),
    );
    // The surviving repo's data is intact and still serves.
    assert!(!store.list(&keeper_prefix).unwrap().is_empty());
    let (st, body) = server.req("GET", "/v1/orgs/acme/repos/keeper/log", &admin, None);
    assert_eq!(st, 200, "{body}");

    // The keeper is the point: a prefix sweep that reached one key too
    // far leaves a manifest pointing at objects that are no longer there.
    assert_closed(&bucket.base_url);
}

#[test]
fn free_tier_quota_and_plan_upgrade() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-quota");
    let scratch = Scratch::new("quota");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_FREE_TIER_REPOS", "2".into())],
    );
    let admin = server.bootstrap_org("acme");
    for name in ["a", "b"] {
        let (st, _) = server.req(
            "POST",
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name })),
        );
        assert_eq!(st, 201);
    }
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "c" })),
    );
    assert_eq!(st, 402, "{out}");
    // An organization's way past the cap is a subscription, and the
    // refusal names it. It used to say "upgrade", the same word a
    // personal namespace got — and a personal namespace has nothing to
    // upgrade to (forks_e2e pins that sentence).
    assert_eq!(
        out["error"],
        "quota: the free plan is limited to 2 repositories — subscribe from Billing to create more"
    );

    // `plan` is a closed vocabulary now, and a word outside it is
    // refused with the words that exist rather than a database error.
    let e = server.admin_expect_err(&["admin", "set-plan", "--org", "acme", "--plan", "pro"]);
    assert!(e.contains("unknown plan"), "{e}");
    assert!(e.contains("free, paid, past_due"), "{e}");
    // …and the org is untouched by the refusal.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "c" })),
    );
    assert_eq!(st, 402, "{out}");

    server.admin_json(&["admin", "set-plan", "--org", "acme", "--plan", "paid"]);
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "c" })),
    );
    assert_eq!(st, 201);
}

#[test]
fn audit_ships_to_object_storage() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-audit");
    let scratch = Scratch::new("audit-ship");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_AUDIT_SHIP_SECS", "1".into())],
    );
    let admin = server.bootstrap_org("acme");
    let (_, repo) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let org_id = repo["org_id"].as_str().unwrap().to_string();
    commit(&server, &admin, "/v1/orgs/acme/repos/app", 0);

    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);

    // Wait for the *trail*, not for whichever object landed first.
    //
    // The shipper runs on a timer, so it may ship a batch after
    // `repo.create` is recorded and before `repo.commit` is — and then
    // the first object holds one line and this asserted two. That is a
    // real flake and it fired: `workers_e2e.rs` at this assertion is the
    // failure in CI runs 34058632747, 34061064538 and 34067763963, on
    // three different branches, and it reads as the audit trail having
    // lost an event rather than as the test having looked too early.
    //
    // The trail is the union of what has shipped, which is also what an
    // auditor reading the bucket would see, so nothing is weakened by
    // reading it that way.
    let trail = wait_for(
        "an audit trail carrying both the create and the commit",
        Duration::from_secs(30),
        || {
            let files = store.list(&format!("o/{org_id}/audit/")).ok()?;
            let mut all = String::new();
            for (k, _) in &files {
                all.push_str(&String::from_utf8(store.get(k).ok()?).ok()?);
            }
            (all.contains("repo.create") && all.contains("repo.commit")).then_some(all)
        },
    );
    assert!(trail.lines().count() >= 2, "{trail}");
}

#[test]
fn web_assets_and_openapi_served() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-web");
    let scratch = Scratch::new("web");

    // Stand-in build outputs: an Astro-shaped site (directory-per-page +
    // llms.txt) and a Vite-shaped dashboard (hashed bundle + SPA index).
    let site = scratch.path().join("site");
    std::fs::create_dir_all(site.join("docs")).unwrap();
    std::fs::write(site.join("index.html"), "<h1>Weft</h1>").unwrap();
    std::fs::write(site.join("docs/index.html"), "<h1>Docs</h1>").unwrap();
    std::fs::write(site.join("llms.txt"), "# Weft\n").unwrap();
    let dash = scratch.path().join("dash");
    std::fs::create_dir_all(dash.join("assets")).unwrap();
    std::fs::write(dash.join("index.html"), "<div id=root></div>").unwrap();
    std::fs::write(dash.join("assets/app-abc123.js"), "console.log(1)").unwrap();

    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_SITE_DIR", site.display().to_string()),
            ("STRATUM_DASHBOARD_DIR", dash.display().to_string()),
        ],
    );

    let get = |path: &str| {
        let resp = ureq::get(&format!("{}{path}", server.base)).call().unwrap();
        let ct = resp.header("Content-Type").unwrap_or("").to_string();
        let cache = resp.header("Cache-Control").unwrap_or("").to_string();
        (resp.into_string().unwrap(), ct, cache)
    };

    // Site at `/`, directory pages, agent docs.
    let (body, ct, _) = get("/");
    assert!(body.contains("Weft") && ct.starts_with("text/html"));
    let (body, _, _) = get("/docs/");
    assert!(body.contains("Docs"));
    let (body, ct, _) = get("/llms.txt");
    assert!(body.starts_with("# Weft") && ct.starts_with("text/plain"));

    // OpenAPI is compiled into the binary — served even with no site dir.
    let (body, ct, _) = get("/openapi.json");
    assert!(ct.starts_with("application/json"));
    let spec: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(spec["openapi"], "3.1.0");
    assert!(spec["paths"]["/v1/orgs/{org}/repos"].is_object());

    // Dashboard SPA: index, deep-link fallback, immutable hashed bundle.
    let (body, _, _) = get("/dashboard/");
    assert!(body.contains("id=root"));
    // A deep client route, which the server knows nothing about: it
    // strips `/dashboard`, finds no such file, and hands back the shell.
    // Any path proves that, so it names a route the SPA actually has —
    // it used to say `/dashboard/repos/widget`, which stopped being one
    // when a repository moved to its single public address.
    let (body, _, _) = get("/dashboard/settings/tokens");
    assert!(body.contains("id=root"), "SPA fallback for client routes");
    let (_, ct, cache) = get("/dashboard/assets/app-abc123.js");
    assert!(ct.starts_with("text/javascript"));
    assert!(cache.contains("immutable"));

    // Unknown paths still 404; API routes are not shadowed by the fallback.
    let err = ureq::get(&format!("{}/no-such-page", server.base))
        .call()
        .unwrap_err();
    assert!(matches!(err, ureq::Error::Status(404, _)));
    let (health, _, _) = get("/healthz");
    assert_eq!(health, "ok\n");
}

/// Paged ref store over the wire: compaction under STRATUM_REF_PAGE_SIZE
/// shards refs into pages; advertise, non-tip wants (served through the
/// spine/locator), wire pushes, and REST refops all keep working.
#[test]
fn paged_refs_serve_fetch_and_push() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-paged");
    let scratch = Scratch::new("paged");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_REF_PAGE_SIZE", "2".into()),
            ("STRATUM_COMPACT_POLL_SECS", "86400".into()), // manual compact
        ],
    );
    let admin = server.bootstrap_org("acme");
    let (st, repo) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "paged" })),
    );
    assert_eq!(st, 201);
    let rp = "/v1/orgs/acme/repos/paged";
    let mut commits = Vec::new();
    for i in 0..9 {
        commits.push(commit(&server, &admin, rp, i));
    }
    // A handful of branches so pagination has something to shard.
    for (i, c) in commits.iter().take(5).enumerate() {
        let (st, out) = server.req(
            "POST",
            &format!("{rp}/branches"),
            &admin,
            Some(serde_json::json!({ "name": format!("topic-{i}"), "from": c })),
        );
        assert_eq!(st, 201, "{out}");
    }
    let (st, out) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    assert_eq!(st, 200, "{out}");

    // The compacted manifest is paged.
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let prefix = format!(
        "o/{}/r/{}/prod",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&store.get(&format!("{prefix}/manifest.json")).unwrap()).unwrap();
    assert!(
        manifest["ref_pages"]
            .as_array()
            .is_some_and(|p| p.len() >= 2),
        "expected sharded ref pages, got {}",
        manifest["ref_pages"]
    );

    // Paged advertise: a stock clone sees every branch and fscks clean.
    let url = server.authed_url(&admin, "acme", "paged");
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&url, &clone);
    let branches = gitcli::git(&clone, &["branch", "-r"]);
    for i in 0..5 {
        assert!(branches.contains(&format!("topic-{i}")), "{branches}");
    }

    // Non-tip want: a fresh clone fetching an old spine commit by SHA is
    // served through the locator-plane membership check.
    let fresh = scratch.path().join("fresh");
    gitcli::git(scratch.path(), &["init", "-q", "fresh"]);
    gitcli::git(&fresh, &["remote", "add", "origin", &url]);
    gitcli::git(&fresh, &["fetch", "-q", "origin", &commits[1]]);
    let got = gitcli::git(&fresh, &["rev-parse", "FETCH_HEAD"])
        .trim()
        .to_string();
    assert_eq!(got, commits[1]);

    // Wire push onto a paged repo: the receive path updates the covering
    // page (manifest CAS still the only commit point).
    std::fs::write(clone.join("pushed.txt"), "paged push\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "push onto paged"]);
    gitcli::git(&clone, &["push", "-q", "origin", "main"]);
    let (st, log) = server.req("GET", &format!("{rp}/log?limit=1"), &admin, None);
    assert_eq!(st, 200);
    let pushed = gitcli::git(&clone, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    assert_eq!(log["entries"][0]["commit"].as_str().unwrap(), pushed);

    // REST refops on a paged repo route through the page store too.
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/branches"),
        &admin,
        Some(serde_json::json!({ "name": "after-paging", "from": pushed })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, refs) = server.req("GET", &format!("{rp}/refs"), &admin, None);
    assert_eq!(st, 200);
    assert!(refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "refs/heads/after-paging"));
    // And the whole thing still clones clean.
    let check = scratch.path().join("check");
    gitcli::clone_and_fsck(&url, &check);

    // Paging is where the ref rules bite: pages must tile the name space
    // without overlapping, and each body must match the range and the
    // count the manifest advertises for it.
    assert_closed(&bucket.base_url);
}

/// **The sweep enqueues; it does not fold.**
///
/// With the claiming worker switched off, nothing can fold — so a
/// repository driven past the thresholds stays past them, and the sweep
/// ticking over it must leave it that way. That is the design: a fold
/// rewrites a whole layout and belongs to the leased worker, where one
/// node does it at a time and a crash mid-fold is recoverable. A sweep
/// that folded inline would do that work unleased, on every node, with
/// no claim.
///
/// It is also the only arrangement that reaches the sweep's "this one is
/// behind" arm deterministically. Running the sweep beside a live worker
/// reaches it only if a tick lands before the fold does, which is a race
/// — it held on a laptop and lost on a two-core runner, where the arm
/// went uncovered and the coverage gate said so.
#[test]
fn the_sweep_enqueues_rather_than_folding_it_itself() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-sweep-enqueue");
    let scratch = Scratch::new("sweep-enqueue");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            // No claiming worker: nothing in this process folds.
            ("STRATUM_COMPACT_POLL_SECS", "0".into()),
            ("STRATUM_COMPACT_SWEEP_SECS", "1".into()),
        ],
    );
    let admin = server.bootstrap_org("acme");
    let (st, repo) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "unfolded" })),
    );
    assert_eq!(st, 201);
    // Past the entry threshold, so every sweep tick sees it as behind.
    for i in 0..9 {
        commit(&server, &admin, "/v1/orgs/acme/repos/unfolded", i);
    }

    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let prefix = format!(
        "o/{}/r/{}/prod",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );
    let wal_len = || -> usize {
        let m: serde_json::Value =
            serde_json::from_slice(&store.get(&format!("{prefix}/manifest.json")).unwrap())
                .unwrap();
        m["wal"].as_array().map(|w| w.len()).unwrap_or(0)
    };
    let before = wal_len();
    assert!(
        before >= 8,
        "the fixture must be past the threshold: {before}"
    );

    // Several ticks pass over a repository the sweep can see is behind.
    std::thread::sleep(Duration::from_secs(4));

    assert_eq!(
        wal_len(),
        before,
        "the sweep folded a WAL itself; folding belongs to the leased \
         worker, one node at a time"
    );
    assert!(server.healthy());
}

/// **The sweep runs, repeatedly, without getting in the way.**
///
/// Compaction is otherwise enqueued only by a write, which is nothing at
/// all for a repository that got ahead and then went quiet — and two
/// have, in production. The sweep is the net underneath, so it runs on
/// every node for the life of the process, holding a fleet-wide lock and
/// reading a manifest per repository each pass.
///
/// That is a lot of chances to wedge something. This drives it at one
/// second against a repository that is actively being folded, which is
/// the interleaving with the most to go wrong: the sweep takes the lock
/// while the worker holds a job, reads a manifest mid-fold, and enqueues
/// against a repository that already has work pending. The fold must
/// still land, and the server must still be serving afterwards.
#[test]
fn the_compaction_sweep_runs_beside_the_worker_without_disturbing_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-sweep");
    let scratch = Scratch::new("sweep");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_COMPACT_POLL_SECS", "0.1".into()),
            // Far faster than production's hour: the point is to make it
            // tick many times inside one test.
            ("STRATUM_COMPACT_SWEEP_SECS", "1".into()),
        ],
    );
    let admin = server.bootstrap_org("acme");
    let (st, repo) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "swept" })),
    );
    assert_eq!(st, 201);
    for i in 0..9 {
        commit(&server, &admin, "/v1/orgs/acme/repos/swept", i);
    }

    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let prefix = format!(
        "o/{}/r/{}/prod",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );
    wait_until(
        "the WAL to fold with the sweep ticking alongside",
        Duration::from_secs(30),
        || {
            let m: serde_json::Value =
                serde_json::from_slice(&store.get(&format!("{prefix}/manifest.json")).unwrap())
                    .unwrap();
            m["wal"].as_array().is_some_and(|w| w.is_empty())
        },
    );

    // Several sweep ticks have now passed over a repository that is
    // below the thresholds — the case it must skip cheaply rather than
    // re-fold forever.
    std::thread::sleep(Duration::from_secs(3));
    let m: serde_json::Value =
        serde_json::from_slice(&store.get(&format!("{prefix}/manifest.json")).unwrap()).unwrap();
    assert!(
        m["wal"].as_array().is_some_and(|w| w.is_empty()),
        "a swept repository must stay folded, not be re-folded into a new WAL"
    );

    let url = server.authed_url(&admin, "acme", "swept");
    gitcli::clone_and_fsck(&url, &scratch.path().join("clone"));
    assert!(server.healthy());
}

/// The compactor worker end-to-end: writes past the WAL thresholds
/// enqueue a job; the polling worker claims it and folds the WAL without
/// any operator involvement.
#[test]
fn compactor_worker_folds_wal_automatically() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-autocompact");
    let scratch = Scratch::new("autocompact");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        // Fractional: what is under test is that the worker claims the
        // enqueued job at all, not how long a tick is. 100ms means the
        // wait below is bounded by the fold, not by the poll.
        &[
            ("STRATUM_COMPACT_POLL_SECS", "0.1".into()),
            // The sweep off, so what folds here is the write-triggered
            // job and nothing else — and `0` is its documented off
            // switch, which ought to be exercised by something.
            ("STRATUM_COMPACT_SWEEP_SECS", "0".into()),
        ],
    );
    let admin = server.bootstrap_org("acme");
    let (st, repo) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "busy" })),
    );
    assert_eq!(st, 201);
    for i in 0..9 {
        commit(&server, &admin, "/v1/orgs/acme/repos/busy", i);
    }

    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let prefix = format!(
        "o/{}/r/{}/prod",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );
    // **Folded, not necessarily empty.** This used to wait for an empty
    // WAL, and that is a proxy which only holds when every one of the
    // nine writes lands before the fold starts — true on a development
    // machine, and not on a loaded two-core runner, where it timed out
    // intermittently for days with no mechanism named.
    //
    // The mechanism: the threshold is eight entries, so the eighth write
    // enqueues the fold and the worker claims it within a 100ms poll. If
    // the ninth lands in the window after that fold's CAS swap but before
    // its job row is completed, its own enqueue is deduplicated away by
    // `jobs_active_per_repo` — and the entry it left is *one*, which is
    // below the threshold, so every later fold correctly answers
    // `NotNeeded`. The WAL is then never empty and never will be, and the
    // wait could only expire.
    //
    // What the test is actually about is that a write past the threshold
    // gets folded by the polling worker with nobody asking. Nine entries
    // becoming at most one is that, exactly, and it does not depend on
    // how the writes interleave with the fold.
    let wal_len = |m: &serde_json::Value| m["wal"].as_array().map_or(0, |w| w.len());
    let manifest = || -> serde_json::Value {
        serde_json::from_slice(&store.get(&format!("{prefix}/manifest.json")).unwrap()).unwrap()
    };
    // On a timeout, say what the WAL and the job row look like: this
    // wait has expired on CI before with nothing to tell a fold that was
    // never queued from one that was still running, and the mechanism
    // named for the last such failure (the ninth write racing the fold)
    // cannot produce a WAL of eight or more — so the next one has to
    // arrive with its evidence.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while wal_len(&manifest()) >= 8 {
        if std::time::Instant::now() > deadline {
            let db = stratum_control::ControlDb::open(&server.db_url).unwrap();
            let job = stratum_control::jobs::latest_for_repo(
                &db,
                repo["org_id"].as_str().unwrap(),
                repo["id"].as_str().unwrap(),
                "compact",
            );
            panic!(
                "waited 30s for the compactor worker to fold the WAL down past the \
                 threshold and it never happened: the WAL holds {} entries and the \
                 repository's latest compact job is {job:?}",
                wal_len(&manifest())
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let left = wal_len(&manifest());
    assert!(
        left <= 1,
        "a fold consumes every entry it saw, so at most the one write that \
         raced it can be left; {left} means something other than that race"
    );
    // Still serves correctly after the background fold.
    let url = server.authed_url(&admin, "acme", "busy");
    gitcli::clone_and_fsck(&url, &scratch.path().join("clone"));

    // The fold rewrote the whole layout with nobody watching; the new
    // manifest owes every key it now names.
    assert_closed(&bucket.base_url);
}

/// Stripe-shaped billing: the rollup worker reports each org's usage to
/// the configured (fake) endpoint with the secret key, and marks the day
/// The rollup folds each org's day into `usage_daily` and stamps it.
#[test]
fn the_usage_rollup_folds_each_day_and_marks_it_done() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-rollup");
    let scratch = Scratch::new("rollup");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_BILLING_ROLLUP_SECS", "1".into())],
    );
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    commit(&server, &admin, "/v1/orgs/acme/repos/app", 0);

    // The day lands in usage_daily and is stamped as folded. There is no
    // external meter to report to — what this produces is read by
    // people, on the dashboard, not invoiced.
    //
    // Wait for a stamped day that also *shows the activity generated
    // above*, not merely for reported_at: the worker's first tick can
    // legitimately fold and stamp a zero-usage day before the repo and
    // commit land (it re-folds on the next tick). Breaking on the first
    // stamp raced exactly that and went red on slow runners.
    let day = wait_for(
        "a rolled-up day carrying the repo and the commit",
        Duration::from_secs(15),
        || {
            let (st, usage) = server.req("GET", "/v1/orgs/acme/usage", &admin, None);
            assert_eq!(st, 200);
            usage["days"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| {
                    !d["reported_at"].is_null()
                        && d["active_repos"].as_u64().unwrap_or(0) >= 1
                        && d["requests"].as_u64().unwrap_or(0) >= 1
                })
                .cloned()
        },
    );
    assert!(day["day"].as_str().unwrap().starts_with("20"), "{day}");
    assert!(server.healthy());
}

/// The GC sweep is fleet-wide exclusive.
///
/// Every node used to run the whole sweep on every tick: N times the
/// LIST/DELETE traffic against one bucket, and N nodes deciding what is
/// past the grace window from N unsynchronised clocks. This test *is* the
/// second node — it holds the sweep lock from its own session and watches
/// the server stand down, then hands it over and watches the sweep
/// happen. Without the lock the server sweeps regardless and the prefix
/// is gone long before the release.
///
/// **The lock is taken before the server process exists**, and that
/// ordering is the whole rendezvous. The first version of this test
/// started the server and *then* asked for the lock, assuming it would be
/// free. Nothing guarantees that: `tokio::time::interval` fires its first
/// tick immediately, so the gc worker is already inside `sweep_all`
/// holding the lock while the server is still answering `/healthz`. On an
/// idle machine that sweep finishes in microseconds and the test wins the
/// next attempt; under a full `--workspace` run it does not, and the test
/// failed in its own setup with "the fleet lock was free" — a false red
/// on the harness, not on the product.
///
/// Retrying the acquisition would also work, but it would be waiting on a
/// proxy (the server happening to be between ticks). Creating the
/// database first and passing it in with `db_url` means the lock is held
/// before there is a worker to race: the server's very first tick finds
/// it taken, which is exactly the state under test.
#[test]
fn a_gc_sweep_waits_for_the_lock_another_node_is_holding() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-gclock");
    let scratch = Scratch::new("gclock");

    // Our own session, on a database that exists before any server does —
    // a second node, as far as the lock is concerned.
    let db_url = stratum_testkit::pg::test_db_url("gclock");
    let other_node = stratum_control::ControlDb::open(&db_url).unwrap();
    let held = stratum_control::jobs::try_lock(&other_node, "gc-sweep")
        .unwrap()
        .expect("nothing else can hold it: no server has started yet");

    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_url(&db_url)
        .data_dir(scratch.path().join("data"))
        .envs(&[
            ("STRATUM_GC_SECS", "1".into()),
            ("STRATUM_GC_GRACE_SECS", "0".into()),
        ])
        .start();

    let admin = server.bootstrap_org("acme");
    let (st, doomed) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "doomed" })),
    );
    assert_eq!(st, 201);
    commit(&server, &admin, "/v1/orgs/acme/repos/doomed", 0);
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let prefix = format!(
        "o/{}/r/{}/",
        doomed["org_id"].as_str().unwrap(),
        doomed["id"].as_str().unwrap()
    );
    assert!(!store.list(&prefix).unwrap().is_empty());

    let (st, _) = server.req("DELETE", "/v1/orgs/acme/repos/doomed", &admin, None);
    assert_eq!(st, 204);
    // Several ticks pass with the lock held elsewhere. The server must
    // leave the storage alone — and stay healthy while doing so.
    //
    // This one is a real clock, not a proxy for one. There is no
    // observable for "the gc worker woke up and stood down", so the only
    // way to prove the negative is to let ticks go by; and `STRATUM_GC_SECS`
    // reads through `env_secs`, so a tick is a whole second and cannot be
    // shortened the way a fractional poll knob can. Eight half-second
    // checks are four ticks.
    for _ in 0..8 {
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !store.list(&prefix).unwrap().is_empty(),
            "the server swept while another node held the gc lock"
        );
    }
    assert!(server.healthy());

    // Handing the lock over is all it takes. The deadline is generous
    // because it only bounds how long a genuine failure takes to report;
    // a loaded machine must not turn a slow sweep into a red test.
    drop(held);
    wait_until(
        "the gc sweep to run once the lock was released",
        Duration::from_secs(60),
        || store.list(&prefix).unwrap().is_empty(),
    );
}

/// The enqueue race, through the real write path.
///
/// `compactor::enqueue` used to ask "is one already active?" and then
/// insert — two statements, and on a fleet two nodes accepting two pushes
/// in the same instant both read `false` and both insert. The claim side
/// then behaves perfectly and makes it worse: `FOR UPDATE SKIP LOCKED`
/// hands the two *different* rows to two *different* workers, which fold
/// the same prefix concurrently. Concurrent writes here stand in for
/// concurrent nodes; they hit the same table through the same code.
///
/// The dedup is a partial unique index now, so what is asserted is the
/// database's own state: one active sweep per (kind, repo), and repos do
/// not deduplicate each other.
#[test]
fn concurrent_writes_leave_exactly_one_active_sweep_per_repo() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-enqueue-race");
    let scratch = Scratch::new("enqueue-race");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        // Nothing claims: the queue is the subject of this test.
        &[
            ("STRATUM_COMPACT_POLL_SECS", "0".into()),
            ("STRATUM_CDNPACK_POLL_SECS", "0".into()),
        ],
    );
    let admin = server.bootstrap_org("acme");
    for name in ["app", "other"] {
        let (st, _) = server.req(
            "POST",
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name })),
        );
        assert_eq!(st, 201);
    }

    // Eight writes to one repo, as concurrently as the client can make
    // them, plus one to a second repo.
    std::thread::scope(|s| {
        for i in 0..8 {
            let server = &server;
            let admin = &admin;
            s.spawn(move || {
                commit(server, admin, "/v1/orgs/acme/repos/app", i);
            });
        }
        s.spawn(|| {
            commit(&server, &admin, "/v1/orgs/acme/repos/other", 0);
        });
    });

    let mut db = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    let rows = db
        .query(
            "SELECT kind, repo_id, COUNT(*) FROM jobs \
             WHERE state IN ('queued','running') GROUP BY kind, repo_id",
            &[],
        )
        .unwrap();
    let mut seen = 0;
    for r in &rows {
        let kind: String = r.get(0);
        let repo: Option<String> = r.get(1);
        let n: i64 = r.get(2);
        if kind == "compact" || kind == "cdnpack" {
            assert_eq!(n, 1, "{kind} for {repo:?} has {n} active rows");
            seen += 1;
        }
    }
    // Two repos × two sweep kinds: the index dedups per repo, not across
    // repos, and not across kinds.
    assert_eq!(seen, 4, "{rows:?}");
    assert!(server.healthy());

    // Eight racing writers took turns at one manifest CAS. A retry loop
    // that republished a stale pointer would show up as a dangling key.
    assert_closed(&bucket.base_url);
}

#[test]
fn disabled_workers_and_notify_retry_exhaustion() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-off");
    let scratch = Scratch::new("workers-off");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_COMPACT_POLL_SECS", "0".into()),
            ("STRATUM_BILLING_ROLLUP_SECS", "0".into()),
            ("STRATUM_AUDIT_SHIP_SECS", "0".into()),
            ("STRATUM_GC_SECS", "0".into()),
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

    // A webhook pointing at a dead port: delivery retries then fails.
    let dead_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (st, _) = server.req(
        "POST",
        &format!("{rp}/webhooks"),
        &admin,
        Some(serde_json::json!({ "url": format!("http://127.0.0.1:{dead_port}/hook") })),
    );
    assert_eq!(st, 201);
    commit(&server, &admin, rp, 0);
    // Three attempts with 200/400/600ms backoff, and the notifier writes
    // a `failed` delivery row when the last one gives up. That row is the
    // observable this test needs: the point of what follows is that a
    // *burnt-out* retry loop did not wedge the notifier, and a fixed 2s
    // sleep only happened to outlast the backoff — a loaded machine would
    // have added the live subscription while the dead one was still
    // retrying, which proves something else entirely and would have read
    // as a product failure.
    let mut db = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    wait_until(
        "the dead webhook's delivery to exhaust its retries",
        Duration::from_secs(20),
        || {
            db.query_one(
                "SELECT COUNT(*) FROM webhook_deliveries WHERE state = 'failed'",
                &[],
            )
            .unwrap()
            .get::<_, i64>(0)
                > 0
        },
    );

    // A live webhook added afterwards still delivers: the retry loop
    // didn't wedge the notifier.
    let (hook_url, rx) = webhook_receiver();
    let (st, _) = server.req(
        "POST",
        &format!("{rp}/webhooks"),
        &admin,
        Some(serde_json::json!({ "url": hook_url })),
    );
    assert_eq!(st, 201);
    commit(&server, &admin, rp, 1);
    rx.recv_timeout(Duration::from_secs(10))
        .expect("live webhook still delivers after a dead one");
}

/// The CDN packer publishes a **self-contained** packfile plus a pointer,
/// skips when already current, and is a no-op (not a failure) on repos
/// with nothing to pack. Self-containment is the load-bearing property:
/// a client fetching this pack in isolation must be able to index it, so
/// the test verifies it with the real `git index-pack`, not just bytes.
#[test]
fn cdn_pack_publishes_a_self_contained_pack_and_skips_when_current() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cdnpack");
    let scratch = Scratch::new("cdnpack");
    // Poll 0 disables the background loop so the test drives it directly.
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_CDNPACK_POLL_SECS", "0".into())],
    );
    let admin = server.bootstrap_org("acme");
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201);

    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let (st, repo) = server.req("GET", "/v1/orgs/acme/repos/app", &admin, None);
    assert_eq!(st, 200);
    let prefix = format!(
        "o/{}/r/{}/prod",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );
    let pack_now = |token: &str| -> (u16, serde_json::Value) {
        server.req("POST", "/v1/orgs/acme/repos/app/cdn-pack", token, None)
    };

    // An empty repo has nothing to pack — a no-op, never a failed job.
    let (st, out) = pack_now(&admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["outcome"], "NotNeeded", "empty repo must be a no-op");

    // Push real content over the wire, then pack it.
    let url = server.authed_url(&admin, "acme", "app");
    let work_repo = scratch.path().join("w");
    gitcli::git(
        scratch.path(),
        &["clone", "-q", &url, work_repo.to_str().unwrap()],
    );
    gitcli::git(&work_repo, &["checkout", "-q", "-b", "main"]);
    std::fs::write(work_repo.join("a.txt"), "hello cdn\n").unwrap();
    gitcli::git(&work_repo, &["add", "-A"]);
    gitcli::git(&work_repo, &["commit", "-q", "-m", "one"]);
    gitcli::git(&work_repo, &["push", "-q", "origin", "main"]);

    let (st, out) = pack_now(&admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["outcome"], "Built");

    // The descriptor points at a pack that actually exists…
    let desc: serde_json::Value =
        serde_json::from_slice(&store.get(&format!("{prefix}/cdn/current.json")).unwrap()).unwrap();
    let pack_key = desc["pack_key"].as_str().unwrap();
    let pack = store.get(pack_key).expect("advertised pack must exist");
    assert!(pack.starts_with(b"PACK"), "not a packfile");
    assert_eq!(desc["size"].as_u64().unwrap(), pack.len() as u64);
    assert!(
        pack_key.starts_with(&prefix),
        "pack escaped the repo prefix"
    );

    // …and real git can index it standalone — the property that makes it
    // safe to hand to a client that has nothing else.
    let idx_dir = scratch.path().join("idx");
    std::fs::create_dir_all(&idx_dir).unwrap();
    let pack_path = idx_dir.join("cdn.pack");
    std::fs::write(&pack_path, &pack).unwrap();
    gitcli::git(&idx_dir, &["init", "-q", "--bare", "."]);
    gitcli::git(&idx_dir, &["index-pack", "-v", pack_path.to_str().unwrap()]);

    // Already current → skip (no rebuild churn on every poll).
    assert_eq!(pack_now(&admin).1["outcome"], "NotNeeded");

    // A new push makes it stale, and the next run republishes.
    std::fs::write(work_repo.join("b.txt"), "second\n").unwrap();
    gitcli::git(&work_repo, &["add", "-A"]);
    gitcli::git(&work_repo, &["commit", "-q", "-m", "two"]);
    gitcli::git(&work_repo, &["push", "-q", "origin", "main"]);
    assert_eq!(pack_now(&admin).1["outcome"], "Built");
    let desc2: serde_json::Value =
        serde_json::from_slice(&store.get(&format!("{prefix}/cdn/current.json")).unwrap()).unwrap();
    assert_ne!(desc2["tip"], desc["tip"], "tip must advance");
    assert!(store.get(desc2["pack_key"].as_str().unwrap()).is_ok());
    // The superseded pack is cleaned up.
    assert!(
        store.get(pack_key).is_err(),
        "old pack should be deleted after the pointer swap"
    );

    // Two pushes and two pack builds, one of which deleted an object.
    assert_closed(&bucket.base_url);
}

/// The published OpenAPI must describe the repo object the server really
/// returns. This is the field most prone to silent drift — `ssh_clone_url`
/// shipped and went undocumented — and a spec that lies is worse than no
/// spec, because clients generate against it.
#[test]
fn the_openapi_repo_schema_matches_what_the_server_returns() {
    let minio = Minio::shared();
    let bucket = minio.bucket("openapi-drift");
    let scratch = Scratch::new("openapi-drift");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        // With an SSH endpoint configured, so the nullable field is
        // populated rather than absent.
        &[(
            "STRATUM_SSH_PUBLIC_URL",
            "ssh://git@ssh.example:2222".to_string(),
        )],
    );
    let admin = server.bootstrap_org("acme");
    let (st, repo) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{repo}");

    let spec: serde_json::Value = serde_json::from_str(
        &ureq::get(&format!("{}/openapi.json", server.base))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    let documented: std::collections::BTreeSet<String> = spec["components"]["schemas"]["Repo"]
        ["properties"]
        .as_object()
        .expect("Repo schema")
        .keys()
        .cloned()
        .collect();
    let returned: std::collections::BTreeSet<String> = repo
        .as_object()
        .expect("repo object")
        .keys()
        .cloned()
        .collect();

    let undocumented: Vec<_> = returned.difference(&documented).collect();
    assert!(
        undocumented.is_empty(),
        "the server returns fields the OpenAPI does not document: {undocumented:?}"
    );
    let phantom: Vec<_> = documented.difference(&returned).collect();
    assert!(
        phantom.is_empty(),
        "the OpenAPI documents fields the server does not return: {phantom:?}"
    );
    // And the field that started this: really present, really an ssh URL.
    assert_eq!(
        repo["ssh_clone_url"].as_str().unwrap(),
        "ssh://git@ssh.example:2222/acme/app.git"
    );
}

/// Two compactions of one repository at the same time finish, and
/// neither one deletes the other's working tree.
///
/// The `compact` route is synchronous and the compactor also polls, so
/// "somebody pressed it while the queued job was running" is the
/// ordinary case. Both ran in `data/compact/<repo>` and both began by
/// clearing it, so the loser of the race had its materialised seed
/// removed from under `git`, mid-run:
///
/// ```text
/// materialized stream rejected: fatal: cannot change to
///   '…/data/compact/01m1pzhk…/compact-seed.git': No such file or directory
/// ```
///
/// Found on a CI runner slow enough for two of them to overlap inside a
/// test about rebased patchsets — a store-shaped error message with no
/// hint that anything was sharing a directory. The scratch is per **job**
/// now (`workers::job_work_dir`), which is pinned by a unit test; this
/// one is the behaviour: whatever the two runs decide between them, a
/// second compaction may not turn the first into a 500, and the
/// repository is intact afterwards.
#[test]
fn two_compactions_at_once_do_not_delete_each_others_working_tree() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workers-compact-race");
    let scratch = Scratch::new("compact-race");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        // The poller is off: this test wants two runs it can *place*,
        // not two it has to wait for.
        &[("STRATUM_COMPACT_POLL_SECS", "0".into())],
    );
    let admin = server.bootstrap_org("acme");
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201);
    let rp = "/v1/orgs/acme/repos/app";
    // Well past the WAL threshold, so both runs have real work to do
    // rather than answering NotNeeded before they touch the disk.
    for i in 0..14 {
        commit(&server, &admin, rp, i);
    }

    let (a, b) = std::thread::scope(|s| {
        let one = s.spawn(|| server.req("POST", &format!("{rp}/compact"), &admin, None));
        let two = s.spawn(|| server.req("POST", &format!("{rp}/compact"), &admin, None));
        (one.join().unwrap(), two.join().unwrap())
    });
    for (st, out) in [&a, &b] {
        assert_eq!(*st, 200, "a concurrent compaction failed: {out}");
        let outcome = out["outcome"].as_str().unwrap_or_default();
        assert!(
            ["Compacted", "LostRace", "NotNeeded"].contains(&outcome),
            "{out}"
        );
    }
    // One of them may legitimately lose the manifest CAS; neither may
    // lose its files.
    for (_, out) in [&a, &b] {
        assert!(
            !out.to_string().contains("No such file or directory"),
            "one run deleted the other's working tree: {out}"
        );
    }

    // And the repository is still whole: a clone reads, and fsck agrees
    // (I11), which is what says neither run published a broken layout.
    let url = server.authed_url(&admin, "acme", "app");
    gitcli::clone_and_fsck(&url, &scratch.path().join("clone-after-race"));
}
