//! Mirror SKU end-to-end (M1–M5): registration, initial ingest, webhook
//! sync, the freshness contract's every branch, read-only enforcement, and
//! the GitHub App provider path — all hermetic (file:// origins, fake
//! GitHub API, MinIO).

use hmac::Mac;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use stratum_store::{LatencyModel, ObjectStore};
use stratum_testkit::fake_github;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::minio::{ROOT_PASSWORD, ROOT_USER};
use stratum_testkit::Minio;

const WEBHOOK_SECRET: &str = "test-webhook-secret";

struct Server {
    child: Child,
    base: String,
    db_url: String,
    store_url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        // SIGINT takes the server's graceful-shutdown path, which also
        // lets an instrumented (coverage) child flush its profile;
        // SIGKILL only as a bounded fallback.
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

fn spawn_server(store_url: &str, scratch: &Scratch, extra_env: &[(&str, String)]) -> Server {
    let db_url = stratum_testkit::pg::test_db_url("mirror");
    let (child, bind) = stratum_testkit::server::spawn_on_free_port(|bind| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        cmd.env_clear()
            // Coverage runs need the instrumented child to write its profile.
            .envs(std::env::var("LLVM_PROFILE_FILE").map(|p| ("LLVM_PROFILE_FILE", p)))
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("STRATUM_STORE_URL", store_url)
            .env("STRATUM_DB_URL", &db_url)
            .env("STRATUM_DATA_DIR", scratch.path().join("data"))
            .env("STRATUM_BIND", bind)
            .env("STRATUM_WEBHOOK_SECRET", WEBHOOK_SECRET)
            .env("STRATUM_MIRROR_POLL_SECS", "0") // deterministic tests
            .env("STRATUM_FRESHNESS_TIMEOUT_SECS", "15")
            .env("AWS_ACCESS_KEY_ID", ROOT_USER)
            .env("AWS_SECRET_ACCESS_KEY", ROOT_PASSWORD)
            .env("AWS_REGION", "us-east-1");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd
    });
    Server {
        child,
        base: format!("http://{bind}"),
        db_url,
        store_url: store_url.to_string(),
    }
}

impl Server {
    fn bootstrap_org(&self, org: &str) -> String {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        cmd.env_clear()
            // Coverage runs need the instrumented child to write its profile.
            .envs(std::env::var("LLVM_PROFILE_FILE").map(|p| ("LLVM_PROFILE_FILE", p)))
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("STRATUM_STORE_URL", &self.store_url)
            .env("STRATUM_DB_URL", &self.db_url)
            .env("AWS_ACCESS_KEY_ID", ROOT_USER)
            .env("AWS_SECRET_ACCESS_KEY", ROOT_PASSWORD)
            .env("AWS_REGION", "us-east-1");
        let out = cmd
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

    fn post(&self, path: &str, token: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let resp = ureq::post(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {token}"))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        match resp {
            Ok(r) => {
                let st = r.status();
                let b = r.into_string().unwrap_or_default();
                (
                    st,
                    serde_json::from_str(&b).unwrap_or(serde_json::Value::Null),
                )
            }
            Err(ureq::Error::Status(code, r)) => {
                let b = r.into_string().unwrap_or_default();
                (
                    code,
                    serde_json::from_str(&b).unwrap_or(serde_json::Value::Null),
                )
            }
            Err(e) => panic!("transport: {e}"),
        }
    }

    fn patch(&self, path: &str, token: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let resp = ureq::request("PATCH", &format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {token}"))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        match resp {
            Ok(r) => {
                let st = r.status();
                let b = r.into_string().unwrap_or_default();
                (
                    st,
                    serde_json::from_str(&b).unwrap_or(serde_json::Value::Null),
                )
            }
            Err(ureq::Error::Status(code, r)) => {
                let b = r.into_string().unwrap_or_default();
                (
                    code,
                    serde_json::from_str(&b).unwrap_or(serde_json::Value::Null),
                )
            }
            Err(e) => panic!("transport: {e}"),
        }
    }

    fn delete(&self, path: &str, token: &str) -> (u16, serde_json::Value) {
        let resp = ureq::delete(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {token}"))
            .call();
        match resp {
            Ok(r) => (r.status(), serde_json::Value::Null),
            Err(ureq::Error::Status(code, r)) => {
                let b = r.into_string().unwrap_or_default();
                (
                    code,
                    serde_json::from_str(&b).unwrap_or(serde_json::Value::Null),
                )
            }
            Err(e) => panic!("transport: {e}"),
        }
    }

    fn get(&self, path: &str, token: &str) -> (u16, serde_json::Value) {
        let resp = ureq::get(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {token}"))
            .call();
        match resp {
            Ok(r) => {
                let st = r.status();
                let b = r.into_string().unwrap_or_default();
                (
                    st,
                    serde_json::from_str(&b).unwrap_or(serde_json::Value::Null),
                )
            }
            Err(ureq::Error::Status(code, r)) => {
                let b = r.into_string().unwrap_or_default();
                (
                    code,
                    serde_json::from_str(&b).unwrap_or(serde_json::Value::Null),
                )
            }
            Err(e) => panic!("transport: {e}"),
        }
    }

    fn sync_now(&self, token: &str, org: &str, repo: &str) -> (u16, serde_json::Value) {
        self.post(
            &format!("/v1/orgs/{org}/mirrors/{repo}/sync"),
            token,
            serde_json::json!({}),
        )
    }

    /// Start the install flow and arrive at the callback, the way a
    /// browser does; the same thing `stratum_testkit::Server` offers,
    /// for this file's own client.
    fn connect_installation(&self, org: &str, admin: &str, installation_id: &str) {
        let (st, out) = self.post(
            &format!("/v1/orgs/{org}/github/install"),
            admin,
            serde_json::json!({}),
        );
        assert_eq!(st, 200, "start install: {out}");
        let state = out["url"]
            .as_str()
            .and_then(|u| u.split("state=").nth(1))
            .expect("install url carries a state")
            .split('&')
            .next()
            .unwrap()
            .to_string();
        let url = format!(
            "{}/v1/github/setup?installation_id={installation_id}&state={state}",
            self.base
        );
        let resp = match ureq::builder().redirects(0).build().get(&url).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("transport GET {url}: {e}"),
        };
        let loc = resp.header("location").unwrap_or_default().to_string();
        assert_eq!(resp.status(), 303, "callback answered {}", resp.status());
        assert!(loc.contains("connect=ok"), "connecting: {loc}");
    }

    fn healthy(&self) -> bool {
        ureq::get(&format!("{}/healthz", self.base))
            .call()
            .map(|r| r.status() == 200)
            .unwrap_or(false)
    }

    fn authed_url(&self, token: &str, org: &str, repo: &str) -> String {
        let base = self.base.strip_prefix("http://").unwrap();
        format!("http://x:{token}@{base}/{org}/{repo}.git")
    }
}

/// A bare origin repo under a file:// root, addressable as owner/name.
fn make_origin(root: &Path, full_name: &str, commits: usize) -> (PathBuf, String) {
    let work = root.join("work").join(full_name.replace('/', "-"));
    let tip = gitcli::fixture_repo(&work, commits);
    let bare = root.join(format!("{full_name}.git"));
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    gitcli::git(
        work.parent().unwrap(),
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    gitcli::git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    (bare, tip)
}

/// Commit into the origin's working repo and push to the bare origin.
fn advance_origin(root: &Path, full_name: &str, filename: &str) -> String {
    let work = root.join("work").join(full_name.replace('/', "-"));
    std::fs::write(work.join(filename), format!("content of {filename}\n")).unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", &format!("add {filename}")]);
    let bare = root.join(format!("{full_name}.git"));
    gitcli::git(&work, &["push", "-q", bare.to_str().unwrap(), "main:main"]);
    gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string()
}

fn webhook_sig(body: &str) -> String {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    format!(
        "sha256={}",
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

#[test]
fn mirror_lifecycle_webhook_freshness_readonly() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-e2e");
    let scratch = Scratch::new("mirror");
    let origins = scratch.path().join("origins");
    let (_bare, tip) = make_origin(&origins, "acme/widget", 15);

    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}/acme/widget.git", origins.display());

    // M1: register → initial ingest (async; drive it with sync_now).
    let (status, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "widget", "provider": "generic", "origin": origin_url }),
    );
    assert_eq!(status, 202, "{out}");
    let (status, out) = server.sync_now(&admin, "acme", "widget");
    assert_eq!(status, 200, "{out}");
    assert!(out["sync_error"].is_null(), "{out}");
    // A sync is a write, and it queues the fold a push queues. Nobody
    // pushes to a mirror, so before this the compactor never heard of
    // one: the first real mirror reached 102 WAL entries (threshold: 8)
    // and every API read downloaded all of them before answering.
    let mut ctl = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    let compacts: i64 = ctl
        .query_one("SELECT COUNT(*) FROM jobs WHERE kind = 'compact'", &[])
        .unwrap()
        .get(0);
    assert!(compacts >= 1, "a mirror sync must enqueue compaction");

    // Clone through the mirror: fsck-clean, right tip (I11/M3).
    let url = server.authed_url(&admin, "acme", "widget");
    let c1 = scratch.path().join("clone1");
    let head = gitcli::clone_and_fsck(&url, &c1);
    assert_eq!(head, tip);

    // M1 webhook: origin advances; a signed webhook makes it clonable.
    let tip2 = advance_origin(&origins, "acme/widget", "feature.txt");
    // Generic-provider webhooks identify the origin by its URL.
    let body = serde_json::json!({ "full_name": origin_url }).to_string();
    let resp = ureq::post(&format!("{}/webhooks/generic", server.base))
        .set("X-Hub-Signature-256", &webhook_sig(&body))
        .set("Content-Type", "application/json")
        .send_string(&body)
        .unwrap();
    assert_eq!(resp.status(), 202);
    // Sync runs in the background; poll the wire until the new tip serves.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        gitcli::git(&c1, &["fetch", "-q", "origin"]);
        let seen = gitcli::git(&c1, &["rev-parse", "origin/main"])
            .trim()
            .to_string();
        if seen == tip2 {
            break;
        }
        assert!(Instant::now() < deadline, "webhook sync never landed");
        std::thread::sleep(Duration::from_millis(200));
    }
    gitcli::fsck(&c1);

    // Unsigned/garbage webhooks are refused.
    let resp = ureq::post(&format!("{}/webhooks/generic", server.base))
        .set("X-Hub-Signature-256", "sha256=deadbeef")
        .send_string(&body);
    match resp {
        Err(ureq::Error::Status(401, _)) => {}
        other => panic!("expected 401 for bad signature, got {other:?}"),
    }

    // M2 freshness: origin advances silently (no webhook); an explicit
    // fetch of the new commit triggers the synchronous sync and succeeds.
    let tip3 = advance_origin(&origins, "acme/widget", "hotfix.txt");
    gitcli::git(&c1, &["fetch", "-q", "origin", &tip3]);
    let got = gitcli::git(&c1, &["rev-parse", "FETCH_HEAD"])
        .trim()
        .to_string();
    assert_eq!(got, tip3, "want-miss fetch must sync synchronously");

    // M2: a commit that exists nowhere → 404 with explanation, not a hang.
    let bogus = "1234567890123456789012345678901234567890";
    let err = gitcli::git_expect_err(&c1, &["fetch", "-q", "origin", bogus]).unwrap();
    assert!(!err.is_empty());

    // M2 origin-down: break the origin, force a sync failure, then verify
    // (a) known content still serves, (b) the staleness header is set.
    let bare = origins.join("acme/widget.git");
    let hidden = origins.join("acme/widget.gone");
    std::fs::rename(&bare, &hidden).unwrap();
    let (status, out) = server.sync_now(&admin, "acme", "widget");
    assert_eq!(
        status, 502,
        "sync against dead origin must fail loudly: {out}"
    );

    let c2 = scratch.path().join("clone2");
    let head = gitcli::clone_and_fsck(&url, &c2); // last-known state serves
    assert_eq!(head, tip3);
    let token = admin.clone();
    let resp = ureq::get(&format!(
        "{}/acme/widget.git/info/refs?service=git-upload-pack",
        server.base
    ))
    .set("Git-Protocol", "version=2")
    .set("Authorization", &format!("Bearer {token}"))
    .call()
    .unwrap();
    assert!(
        resp.header("x-weft-staleness").is_some(),
        "stale mirror responses must carry the staleness header"
    );
    assert!(resp.header("x-weft-origin-error").is_some());

    // Origin restored: sync heals, staleness clears.
    std::fs::rename(&hidden, &bare).unwrap();
    let (status, out) = server.sync_now(&admin, "acme", "widget");
    assert_eq!(status, 200, "{out}");
    let resp = ureq::get(&format!(
        "{}/acme/widget.git/info/refs?service=git-upload-pack",
        server.base
    ))
    .set("Git-Protocol", "version=2")
    .set("Authorization", &format!("Bearer {token}"))
    .call()
    .unwrap();
    assert!(resp.header("x-weft-staleness").is_none());

    // M4, inverted: a push to the mirror is forwarded to the origin
    // first, and the mirror reflects it before the client hears `ok`.
    // The origin's main is the pushed tip; a fresh clone of the mirror
    // has it with no sync asked for; and the audit row says where the
    // push went. Before write-through this push was refused with
    // "this mirror is read-only".
    gitcli::git(&c1, &["fetch", "-q", "origin"]);
    gitcli::git(&c1, &["reset", "-q", "--hard", "origin/main"]);
    std::fs::write(c1.join("through.txt"), "pushed through the mirror\n").unwrap();
    gitcli::git(&c1, &["add", "-A"]);
    gitcli::git(&c1, &["commit", "-q", "-m", "through the mirror"]);
    gitcli::git(&c1, &["tag", "-a", "-m", "release", "v1.0"]);
    let pushed = gitcli::git(&c1, &["rev-parse", "HEAD"]).trim().to_string();
    gitcli::git(&c1, &["push", "-q", "origin", "main", "v1.0"]);
    assert_eq!(
        gitcli::git(&bare, &["rev-parse", "refs/heads/main"]).trim(),
        pushed,
        "the origin took the push first"
    );
    assert_eq!(
        gitcli::git(&bare, &["rev-parse", "refs/tags/v1.0^{commit}"]).trim(),
        pushed,
        "the tag went with it"
    );
    let c3 = scratch.path().join("clone3");
    assert_eq!(
        gitcli::clone_and_fsck(&url, &c3),
        pushed,
        "the mirror serves what it forwarded, with no sync asked for"
    );
    assert!(gitcli::git(&c3, &["tag"]).contains("v1.0"));
    let (st, audit) = server.get("/v1/orgs/acme/audit?limit=50", &admin);
    assert_eq!(st, 200, "{audit}");
    let push_rows: Vec<&serde_json::Value> = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "repo.push")
        .collect();
    assert!(
        push_rows
            .iter()
            .any(|e| e.to_string().contains("forwarded_to")),
        "a forwarded push says where it went: {push_rows:?}"
    );
}

/// One node syncs a mirror at a time, fleet-wide.
///
/// The per-repo mutex inside `SyncManager` serializes a process; the
/// fleet has two, and their poll ticks line up. Prod's log showed the
/// second node fetching the origin again, rebuilding the same layout
/// and losing the manifest swap — "manifest swap lost a race
/// (concurrent writer)" — for work the first node had already done.
/// The other node is stood in for here by a second database session
/// holding the same advisory lock: while it does, a sync answers
/// `Elsewhere` at once and touches nothing; when it lets go, the same
/// request syncs.
#[test]
fn a_mirror_is_synced_by_one_node_at_a_time() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-onenode");
    let scratch = Scratch::new("mirror-onenode");
    let origins = scratch.path().join("origins");
    let (_bare, tip) = make_origin(&origins, "acme/widget", 5);
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}/acme/widget.git", origins.display());
    let (status, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "widget", "provider": "generic", "origin": origin_url }),
    );
    assert_eq!(status, 202, "{out}");
    let repo_id = out["repo"]["id"].as_str().unwrap().to_string();
    // Creation's own initial sync runs in the background and holds the
    // lock while it does; drive it to the end before standing in for
    // the other node, so what is measured is contention and not the
    // tail of that ingest.
    let (status, out) = server.sync_now(&admin, "acme", "widget");
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["last_synced_commit"], tip, "{out}");
    let synced_at = server.get("/v1/orgs/acme/repos/widget", &admin).1["last_sync_at"]
        .as_i64()
        .expect("a synced mirror has a sync time");

    // The other node, mid-sync: its session holds the repository's lock.
    let other = stratum_control::ControlDb::open(&server.db_url).expect("second session");
    let held = stratum_control::jobs::try_lock_scoped(&other, "mirror-sync", &repo_id)
        .expect("lock")
        .expect("nobody else holds it yet");
    let (status, out) = server.sync_now(&admin, "acme", "widget");
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["outcome"], "Elsewhere", "{out}");
    assert!(
        out["sync_error"].is_null(),
        "a deferred sync is not a failure: {out}"
    );
    let after = server.get("/v1/orgs/acme/repos/widget", &admin).1;
    assert_eq!(
        after["last_sync_at"].as_i64(),
        Some(synced_at),
        "a deferred sync recorded itself as one: {after}"
    );
    // Another repository is not held by this one's lock.
    let unrelated =
        stratum_control::jobs::try_lock_scoped(&other, "mirror-sync", "01someotherrepo")
            .expect("lock");
    assert!(
        unrelated.is_some(),
        "one repository's sync locked another's"
    );
    drop(unrelated);

    // The other node is done: this one syncs, and the clone is whole.
    drop(held);
    let (status, out) = server.sync_now(&admin, "acme", "widget");
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["outcome"], "NoChange", "{out}");
    let url = server.authed_url(&admin, "acme", "widget");
    let clone = scratch.path().join("onenode-clone");
    assert_eq!(gitcli::clone_and_fsck(&url, &clone), tip);
}

/// A pasted `github.com/owner/name` is a GitHub origin like any other.
///
/// Creation stored the origin as typed and the provider's `fetch_url`
/// read it as `owner/name`, so the first sync of a mirror registered from
/// the dashboard's paste box fetched `<git base>/github.com/owner/name.git`
/// and reported the origin unreachable. The probe had normalised the same
/// string a moment earlier and said "found it". Both the stored identity
/// and the fetch now agree on `owner/name`.
#[test]
fn a_pasted_github_url_is_mirrored_as_owner_name() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-pasted");
    let scratch = Scratch::new("mirror-pasted");
    let origins = scratch.path().join("origins");
    let (_bare, tip) = make_origin(&origins, "acme/pasted", 3);

    let gh = fake_github::spawn();
    let key_path = scratch.path().join("app-key.pem");
    std::fs::write(&key_path, fake_github::TEST_APP_KEY_PEM).unwrap();
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_GITHUB_APP_ID", "12345".into()),
            ("STRATUM_GITHUB_APP_KEY_PEM", key_path.display().to_string()),
            ("STRATUM_GITHUB_API_BASE", gh.base_url.clone()),
            (
                "STRATUM_GITHUB_GIT_BASE",
                format!("file://{}", origins.display()),
            ),
            (
                "STRATUM_GITHUB_INSTALL_URL",
                "https://github.com/apps/stratum/installations/new".into(),
            ),
        ],
    );
    let admin = server.bootstrap_org("acme");

    let (status, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({
            "name": "pasted",
            "provider": "github",
            "origin": "github.com/acme/pasted",
            "public": true,
        }),
    );
    assert_eq!(status, 202, "{out}");
    assert_eq!(out["repo"]["origin_url"], "acme/pasted", "{out}");
    let (status, out) = server.sync_now(&admin, "acme", "pasted");
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["error"], serde_json::Value::Null, "{out}");
    let clone = scratch.path().join("pasted-clone");
    assert_eq!(
        gitcli::clone_and_fsck(&server.authed_url(&admin, "acme", "pasted"), &clone),
        tip
    );

    // A host that is not GitHub is refused for this provider, up front,
    // rather than fetched from GitHub under the wrong name.
    let (status, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({
            "name": "elsewhere",
            "provider": "github",
            "origin": "https://gitlab.com/acme/widget",
        }),
    );
    assert_eq!(status, 422, "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("not github.com"),
        "{out}"
    );
}

#[test]
fn github_app_provider_token_exchange_path() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-gh");
    let scratch = Scratch::new("mirror-gh");
    let origins = scratch.path().join("origins");
    let (_bare, tip) = make_origin(&origins, "acme/private", 8);

    let gh = fake_github::spawn();
    let key_path = scratch.path().join("app-key.pem");
    std::fs::write(&key_path, fake_github::TEST_APP_KEY_PEM).unwrap();
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_GITHUB_APP_ID", "12345".into()),
            ("STRATUM_GITHUB_APP_KEY_PEM", key_path.display().to_string()),
            ("STRATUM_GITHUB_API_BASE", gh.base_url.clone()),
            (
                "STRATUM_GITHUB_GIT_BASE",
                format!("file://{}", origins.display()),
            ),
            (
                "STRATUM_GITHUB_INSTALL_URL",
                "https://github.com/apps/stratum/installations/new".into(),
            ),
            // The CDN packer, running: a mirror's pack is built by the
            // sync that moved its layout, not by a push nobody makes.
            ("STRATUM_CDNPACK_POLL_SECS", "1".into()),
        ],
    );
    let admin = server.bootstrap_org("acme");
    // Creation refuses an installation the org has not connected, so
    // connect 777 the way a person does before mirroring through it.
    server.connect_installation("acme", &admin, "777");

    let (status, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({
            "name": "private",
            "provider": "github",
            "origin": "acme/private",
            "installation_id": "777",
        }),
    );
    assert_eq!(status, 202, "{out}");
    let (status, out) = server.sync_now(&admin, "acme", "private");
    assert_eq!(status, 200, "{out}");

    let url = server.authed_url(&admin, "acme", "private");
    let clone = scratch.path().join("gh-clone");
    let head = gitcli::clone_and_fsck(&url, &clone);
    assert_eq!(head, tip);

    // The sync that ingested the origin armed the two things a push
    // arms and a mirror never had: a checks poll through the same
    // installation, and a CDN pack at the new tip. The first real
    // mirror on weft.sh had neither — its Checks tab said "first poll
    // has not finished" for hours, and every clone took the inline path
    // until somebody built the pack by hand.
    let (st, repo) = server.get("/v1/orgs/acme/repos/private", &admin);
    assert_eq!(st, 200, "{repo}");
    let org_id = repo["org_id"].as_str().unwrap().to_string();
    let repo_id = repo["id"].as_str().unwrap().to_string();
    let db = stratum_control::ControlDb::open(&server.db_url).expect("control plane");
    let poll = stratum_control::jobs::latest_for_repo(&db, &org_id, &repo_id, "checkspoll")
        .expect("read jobs")
        .expect("the sync armed a checks poll");
    assert!(
        matches!(poll.state.as_str(), "queued" | "running" | "done"),
        "{poll:?}"
    );
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let descriptor = format!("o/{org_id}/r/{repo_id}/prod/cdn/current.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let desc: serde_json::Value = loop {
        if let Ok(bytes) = store.get(&descriptor) {
            break serde_json::from_slice(&bytes).expect("descriptor json");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no CDN pack was built for the mirror after its sync"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    assert_eq!(desc["tip"], tip, "the pack is at the synced tip: {desc}");
    let pack = store
        .get(desc["pack_key"].as_str().unwrap())
        .expect("the advertised pack exists");
    assert!(pack.starts_with(b"PACK"));

    // The file:// git base short-circuits credentials, so the token
    // exchange isn't exercised by the fetch itself — drive the provider's
    // JWT + token path explicitly through a non-file base registration.
    // (The fake counts tokens; an https-based fetch URL is not fetchable
    // here, so we assert the API fake is wired and reachable instead.)
    let resp = ureq::post(&format!(
        "{}/app/installations/777/access_tokens",
        gh.base_url
    ))
    .set("Authorization", "Bearer a.b.c")
    .send_string("{}")
    .unwrap();
    assert_eq!(resp.status(), 201);
}

/// Raw protocol-v2 fetch POST for one want — unlike the git CLI, this
/// surfaces the 404 body and response headers for assertions.
fn raw_fetch_want(
    base: &str,
    token: &str,
    org: &str,
    repo: &str,
    want: &str,
) -> (u16, String, Vec<(String, String)>) {
    let mut body = Vec::new();
    for line in [
        "command=fetch\n".to_string(),
        "object-format=sha1\n".to_string(),
    ] {
        body.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
        body.extend_from_slice(line.as_bytes());
    }
    body.extend_from_slice(b"0001");
    for line in [
        "ofs-delta\n".to_string(),
        format!("want {want}\n"),
        "done\n".to_string(),
    ] {
        body.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
        body.extend_from_slice(line.as_bytes());
    }
    body.extend_from_slice(b"0000");
    let resp = ureq::post(&format!("{base}/{org}/{repo}/git-upload-pack"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Git-Protocol", "version=2")
        .set("Content-Type", "application/x-git-upload-pack-request")
        .send_bytes(&body);
    match resp {
        Ok(r) | Err(ureq::Error::Status(_, r)) => {
            let st = r.status();
            let headers: Vec<(String, String)> = r
                .headers_names()
                .iter()
                .map(|n| (n.clone(), r.header(n).unwrap_or("").to_string()))
                .collect();
            (st, r.into_string().unwrap_or_default(), headers)
        }
        Err(e) => panic!("raw fetch transport error: {e}"),
    }
}

/// The 60s-class poller is the webhook-loss floor: with a 1s interval, a
/// silent origin advance lands on the mirror with no webhook at all.
#[test]
fn poller_recovers_from_missed_webhooks() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-poller");
    let scratch = Scratch::new("poller");
    let origins = scratch.path().join("origins");
    let (_bare, _tip) = make_origin(&origins, "acme/polled", 5);

    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_MIRROR_POLL_SECS", "1".into())],
    );
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}/acme/polled.git", origins.display());
    let (status, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "polled", "provider": "generic", "origin": origin_url }),
    );
    assert_eq!(status, 202, "{out}");
    let (status, out) = server.sync_now(&admin, "acme", "polled");
    assert_eq!(status, 200, "{out}");

    let url = server.authed_url(&admin, "acme", "polled");
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&url, &clone);

    // Advance the origin and send NO webhook — only the poller can notice.
    let tip2 = advance_origin(&origins, "acme/polled", "silent.txt");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        gitcli::git(&clone, &["fetch", "-q", "origin"]);
        let seen = gitcli::git(&clone, &["rev-parse", "origin/main"])
            .trim()
            .to_string();
        if seen == tip2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "poller never picked up the drift"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    gitcli::fsck(&clone);
}

/// Freshness budget: with a zero budget every want-miss answers the
/// documented 404 naming the budget, instead of hanging on the sync.
#[test]
fn freshness_budget_exceeded_is_a_clean_404() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-budget");
    let scratch = Scratch::new("budget");
    let origins = scratch.path().join("origins");
    make_origin(&origins, "acme/slow", 5);

    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_FRESHNESS_TIMEOUT_SECS", "0".into())],
    );
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}/acme/slow.git", origins.display());
    server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "slow", "provider": "generic", "origin": origin_url }),
    );
    server.sync_now(&admin, "acme", "slow");
    let url = server.authed_url(&admin, "acme", "slow");
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&url, &clone);

    let tip2 = advance_origin(&origins, "acme/slow", "late.txt");
    // The git CLI hides 404 bodies; hit upload-pack raw for the message.
    let (st, body, _) = raw_fetch_want(&server.base, &admin, "acme", "slow", &tip2);
    assert_eq!(st, 404);
    assert!(
        body.contains("freshness budget"),
        "want-miss past the budget must name it: {body}"
    );
    // Through the CLI: either a clean failure, or success — the timed-out
    // sync keeps running detached, so a retry may already find the commit
    // (that is the documented "retry shortly" contract). Never a hang.
    let _ = gitcli::git_expect_err(&clone, &["fetch", "-q", "origin", &tip2]);
}

/// Origin down + want-miss: the 404 names the unreachable origin and the
/// response carries the staleness headers — the only path to old data.
#[test]
fn origin_down_want_miss_explains_and_marks_staleness() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-downmiss");
    let scratch = Scratch::new("downmiss");
    let origins = scratch.path().join("origins");
    make_origin(&origins, "acme/gone", 5);

    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}/acme/gone.git", origins.display());
    server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "gone", "provider": "generic", "origin": origin_url }),
    );
    server.sync_now(&admin, "acme", "gone");
    let url = server.authed_url(&admin, "acme", "gone");
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&url, &clone);

    // Take the origin away, then ask for a commit the mirror lacks.
    std::fs::rename(
        origins.join("acme/gone.git"),
        origins.join("acme/hidden.git"),
    )
    .unwrap();
    let bogus = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (st, body, headers) = raw_fetch_want(&server.base, &admin, "acme", "gone", bogus);
    assert_eq!(st, 404);
    assert!(
        body.contains("unreachable") && body.contains("last-known"),
        "unreachable-origin want-miss must explain itself: {body}"
    );
    assert!(
        headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("x-weft-staleness")),
        "the miss answer itself is stale-marked: {headers:?}"
    );
    // Known content still serves, stale-marked (checked via the raw
    // advert since headers don't surface through the git CLI).
    let resp = ureq::get(&format!(
        "{}/acme/gone/info/refs?service=git-upload-pack",
        server.base
    ))
    .set("Authorization", &format!("Bearer {admin}"))
    .set("Git-Protocol", "version=2")
    .call()
    .unwrap();
    assert!(resp.header("X-Weft-Staleness").is_some());
    assert!(resp.header("X-Weft-Origin-Error").is_some());
}

/// A fetch against a registered-but-never-synced mirror triggers the
/// first sync from the want-miss itself: nothing is known (no manifest
/// yet), the freshness path syncs, and the fetch serves.
#[test]
fn unsynced_mirror_first_want_triggers_initial_sync() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-firstwant");
    let scratch = Scratch::new("firstwant");
    let origins = scratch.path().join("origins");
    let (_bare, tip) = make_origin(&origins, "acme/fresh", 5);

    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}/acme/fresh.git", origins.display());
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "fresh", "provider": "generic", "origin": origin_url }),
    );
    assert_eq!(st, 202, "{out}");
    // No sync_now: the raw want-fetch is the first thing that happens.
    // (The registration schedules a background initial sync; even if it
    // races us, both paths land the same state.)
    let (st, body, _) = raw_fetch_want(&server.base, &admin, "acme", "fresh", &tip);
    assert!(
        st == 200 && String::from_utf8_lossy(body.as_bytes()).contains("packfile"),
        "first want must sync-and-serve: {st} {body}"
    );
}

/// A want that arrives while another node is syncing the same mirror
/// waits for that sync rather than being refused.
///
/// The freshness path coalesces concurrent syncs through a fleet-wide
/// lock, and the loser used to be told `Elsewhere` and go straight on to
/// check whether the commit was known — before the winner had finished
/// writing it — and answer 404 "did not surface it". Two jobs of one
/// workflow, fetching the same just-pushed commit within the same second
/// on weft.sh: one served, one refused by name for a commit the mirror
/// was in the middle of ingesting. The lock is held here by the test,
/// the way another node would hold it, and released after a while; the
/// want must be served, and must have waited.
#[test]
fn a_want_during_another_nodes_sync_waits_for_it_rather_than_being_refused() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-elsewhere");
    let scratch = Scratch::new("mirror-elsewhere");
    let origins = scratch.path().join("origins");
    let (bare, _tip) = make_origin(&origins, "acme/held", 3);
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}", bare.display());
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "held", "provider": "generic", "origin": origin_url }),
    );
    assert_eq!(st, 202, "{out}");
    let (st, out) = server.sync_now(&admin, "acme", "held");
    assert_eq!(st, 200, "{out}");
    let (st, repo) = server.get("/v1/orgs/acme/repos/held", &admin);
    assert_eq!(st, 200, "{repo}");
    let repo_id = repo["id"].as_str().unwrap().to_string();

    // A commit the mirror does not have yet.
    let work = origins.join("work").join("acme-held");
    std::fs::write(work.join("later.txt"), "later\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "later"]);
    gitcli::git(&work, &["push", "-q", bare.to_str().unwrap(), "main:main"]);
    let fresh = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // Another node is syncing this mirror: hold its lock for a while.
    let db = stratum_control::ControlDb::open(&server.db_url).expect("control plane");
    let guard = stratum_control::jobs::try_lock_scoped(&db, "mirror-sync", &repo_id)
        .expect("lock")
        .expect("nobody else holds it");
    let hold = Duration::from_millis(1500);
    let base = server.base.clone();
    let admin2 = admin.clone();
    let started = Instant::now();
    let fetch = std::thread::spawn(move || raw_fetch_want(&base, &admin2, "acme", "held", &fresh));
    std::thread::sleep(hold);
    drop(guard);
    let (st, body, _) = fetch.join().unwrap();
    assert_eq!(st, 200, "the want was refused instead of waiting: {body}");
    assert!(body.contains("packfile"), "{body}");
    assert!(
        started.elapsed() >= hold,
        "served in {:?}, before the lock was released",
        started.elapsed()
    );
}

/// On a paged mirror, a want that is neither a tip nor on the spine is
/// resolved through the locator plane — no origin round-trip for commits
/// the mirror already holds.
#[test]
fn paged_mirror_resolves_wants_through_the_locator() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-planewant");
    let scratch = Scratch::new("planewant");
    let origins = scratch.path().join("origins");
    let (_bare, _tip) = make_origin(&origins, "acme/planed", 6);
    // A two-commit side branch: its parent commit is in the layout but is
    // no ref tip and no spine commit.
    let work = origins.join("work").join("acme-planed");
    gitcli::git(&work, &["checkout", "-q", "-b", "side2"]);
    std::fs::write(work.join("s1.txt"), "s1\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "side 1"]);
    let side_parent = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    std::fs::write(work.join("s2.txt"), "s2\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "side 2"]);
    let bare = origins.join("acme/planed.git");
    gitcli::git(
        &work,
        &[
            "push",
            "-q",
            bare.to_str().unwrap(),
            "main:main",
            "side2:side2",
        ],
    );
    gitcli::git(&work, &["checkout", "-q", "main"]);

    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_REF_PAGE_SIZE", "2".into())],
    );
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}/acme/planed.git", origins.display());
    server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "planed", "provider": "generic", "origin": origin_url }),
    );
    let (st, out) = server.sync_now(&admin, "acme", "planed");
    assert_eq!(st, 200, "{out}");

    let (st, body, _) = raw_fetch_want(&server.base, &admin, "acme", "planed", &side_parent);
    assert_eq!(st, 200, "locator-known want serves: {body}");
    assert!(body.contains("packfile"), "{body}");

    // Two non-tip wants in one request: the locator plane loads once and
    // answers both membership checks.
    let grand = gitcli::git(&work, &["rev-parse", &format!("{side_parent}~1")])
        .trim()
        .to_string();
    let mut body = Vec::new();
    for line in [
        "command=fetch\n".to_string(),
        "object-format=sha1\n".to_string(),
    ] {
        body.extend_from_slice(format!("{:04x}{line}", line.len() + 4).as_bytes());
    }
    body.extend_from_slice(b"0001");
    for line in [
        "ofs-delta\n".to_string(),
        format!("want {side_parent}\n"),
        format!("want {grand}\n"),
        "done\n".to_string(),
    ] {
        body.extend_from_slice(format!("{:04x}{line}", line.len() + 4).as_bytes());
    }
    body.extend_from_slice(b"0000");
    let resp = ureq::post(&format!("{}/acme/planed/git-upload-pack", server.base))
        .set("Authorization", &format!("Bearer {admin}"))
        .set("Git-Protocol", "version=2")
        .set("Content-Type", "application/x-git-upload-pack-request")
        .send_bytes(&body)
        .unwrap();
    assert_eq!(resp.status(), 200, "both locator-known wants serve");
}

/// GitHub-provider webhooks: signature-checked, repository.full_name
/// extracted; garbage bodies are refused. (The app key is provided
/// inline, covering the non-PEM-file configuration path.)
#[test]
fn github_webhook_verification_and_inline_key() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-ghhook");
    let scratch = Scratch::new("ghhook");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_GITHUB_APP_ID", "4242".into()),
            (
                "STRATUM_GITHUB_APP_KEY",
                stratum_testkit::fake_github::TEST_APP_KEY_PEM.into(),
            ),
            ("STRATUM_GITHUB_WEBHOOK_SECRET", WEBHOOK_SECRET.into()),
        ],
    );
    server.bootstrap_org("acme");

    // Valid signature + github-shaped body → accepted (202) even when no
    // mirror matches: webhook receipt is decoupled from registration.
    let body = serde_json::json!({ "repository": { "full_name": "acme/widget" } }).to_string();
    let resp = ureq::post(&format!("{}/webhooks/github", server.base))
        .set("X-Hub-Signature-256", &webhook_sig(&body))
        .set("Content-Type", "application/json")
        .send_string(&body)
        .unwrap();
    assert_eq!(resp.status(), 202);

    // Signed but malformed body (no repository.full_name) → 4xx.
    let junk = serde_json::json!({ "not": "a webhook" }).to_string();
    let resp = ureq::post(&format!("{}/webhooks/github", server.base))
        .set("X-Hub-Signature-256", &webhook_sig(&junk))
        .set("Content-Type", "application/json")
        .send_string(&junk);
    assert!(matches!(resp, Err(ureq::Error::Status(c, _)) if c >= 400));
}

/// A webhook for a mirror whose origin has vanished: the background sync
/// fails and records the error on the repo (incident-mode input).
#[test]
fn webhook_sync_failure_records_the_error() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-hookfail");
    let scratch = Scratch::new("hookfail");
    let origins = scratch.path().join("origins");
    make_origin(&origins, "acme/vanish", 4);
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}/acme/vanish.git", origins.display());
    server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "vanish", "provider": "generic", "origin": origin_url }),
    );
    let (st, _) = server.sync_now(&admin, "acme", "vanish");
    assert_eq!(st, 200);

    std::fs::rename(
        origins.join("acme/vanish.git"),
        origins.join("acme/hidden.git"),
    )
    .unwrap();
    let body = serde_json::json!({ "full_name": origin_url }).to_string();
    let resp = ureq::post(&format!("{}/webhooks/generic", server.base))
        .set("X-Hub-Signature-256", &webhook_sig(&body))
        .set("Content-Type", "application/json")
        .send_string(&body)
        .unwrap();
    assert_eq!(resp.status(), 202);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let resp = ureq::get(&format!("{}/v1/orgs/acme/repos/vanish", server.base))
            .set("Authorization", &format!("Bearer {admin}"))
            .call()
            .unwrap();
        let repo: serde_json::Value = serde_json::from_str(&resp.into_string().unwrap()).unwrap();
        if repo["sync_error"].is_string() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sync error never recorded: {repo}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Origin quirks: a non-main HEAD branch is adopted as the default
/// branch; an empty origin and an origin whose HEAD names a missing
/// branch fail with precise errors.
#[test]
fn origin_head_detection_and_error_cases() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-heads");
    let scratch = Scratch::new("heads");
    let origins = scratch.path().join("origins");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");

    // Origin with HEAD on "trunk".
    let work = origins.join("work").join("trunky");
    gitcli::fixture_repo(&work, 4);
    gitcli::git(&work, &["branch", "-m", "main", "trunk"]);
    let bare = origins.join("acme/trunky.git");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    gitcli::git(
        work.parent().unwrap(),
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    gitcli::git(&bare, &["symbolic-ref", "HEAD", "refs/heads/trunk"]);
    let origin_url = format!("file://{}", bare.display());
    server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "trunky", "provider": "generic", "origin": origin_url }),
    );
    let (st, out) = server.sync_now(&admin, "acme", "trunky");
    assert_eq!(st, 200, "{out}");
    let resp = ureq::get(&format!("{}/v1/orgs/acme/repos/trunky", server.base))
        .set("Authorization", &format!("Bearer {admin}"))
        .call()
        .unwrap();
    let repo: serde_json::Value = serde_json::from_str(&resp.into_string().unwrap()).unwrap();
    assert_eq!(repo["default_branch"], "trunk", "{repo}");
    // And clones land on trunk.
    let url = server.authed_url(&admin, "acme", "trunky");
    let clone = scratch.path().join("trunk-clone");
    gitcli::clone_and_fsck(&url, &clone);
    let head = gitcli::git(&clone, &["symbolic-ref", "--short", "HEAD"]);
    assert_eq!(head.trim(), "trunk");

    // Empty origin: nothing to mirror, said plainly.
    let empty = origins.join("acme/empty.git");
    std::fs::create_dir_all(&empty).unwrap();
    gitcli::git(
        origins.as_path(),
        &["init", "-q", "--bare", empty.to_str().unwrap()],
    );
    server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "empty", "provider": "generic",
                            "origin": format!("file://{}", empty.display()) }),
    );
    let (st, out) = server.sync_now(&admin, "acme", "empty");
    assert_eq!(st, 502, "{out}");
    assert!(out["error"].as_str().unwrap().contains("no refs"), "{out}");

    // A dangling HEAD symref on the origin (no symref advertised) falls
    // back to the repo's configured default branch and still mirrors.
    let lost = origins.join("acme/lost.git");
    let work2 = origins.join("work").join("lost");
    gitcli::fixture_repo(&work2, 3);
    gitcli::git(
        work2.parent().unwrap(),
        &[
            "clone",
            "-q",
            "--bare",
            work2.to_str().unwrap(),
            lost.to_str().unwrap(),
        ],
    );
    gitcli::git(&lost, &["symbolic-ref", "HEAD", "refs/heads/never-created"]);
    server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "lost", "provider": "generic",
                            "origin": format!("file://{}", lost.display()) }),
    );
    let (st, out) = server.sync_now(&admin, "acme", "lost");
    assert_eq!(st, 200, "dangling HEAD falls back to main: {out}");
    let url = server.authed_url(&admin, "acme", "lost");
    gitcli::clone_and_fsck(&url, &scratch.path().join("lost-clone"));
}

/// Paged mirrors: an incremental origin advance rewrites only the
/// covering page, and a branch deletion falls back to a full re-ingest
/// (page-wise deletion is not supported yet).
#[test]
fn paged_mirror_incremental_update_and_deletion_reingest() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-pagedsync");
    let scratch = Scratch::new("pagedsync");
    let origins = scratch.path().join("origins");
    let (_bare, _tip) = make_origin(&origins, "acme/psync", 6);
    let work = origins.join("work").join("acme-psync");
    let bare = origins.join("acme/psync.git");
    for i in 0..4 {
        gitcli::git(&work, &["branch", &format!("feat-{i}")]);
    }
    gitcli::git(&work, &["push", "-q", bare.to_str().unwrap(), "--all"]);

    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_REF_PAGE_SIZE", "2".into())],
    );
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}/acme/psync.git", origins.display());
    server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "psync", "provider": "generic", "origin": origin_url }),
    );
    let (st, out) = server.sync_now(&admin, "acme", "psync");
    assert_eq!(st, 200, "{out}");

    // Incremental: advance main → the paged ref store updates page-wise.
    let tip2 = advance_origin(&origins, "acme/psync", "advance.txt");
    let (st, out) = server.sync_now(&admin, "acme", "psync");
    assert_eq!(st, 200, "{out}");
    let url = server.authed_url(&admin, "acme", "psync");
    let c1 = scratch.path().join("c1");
    let head = gitcli::clone_and_fsck(&url, &c1);
    assert_eq!(head, tip2);

    // Deletion: drop a branch on the origin → full re-ingest, branch gone.
    gitcli::git(&work, &["push", "-q", bare.to_str().unwrap(), ":feat-0"]);
    let (st, out) = server.sync_now(&admin, "acme", "psync");
    assert_eq!(st, 200, "{out}");
    let c2 = scratch.path().join("c2");
    gitcli::clone_and_fsck(&url, &c2);
    let branches = gitcli::git(&c2, &["branch", "-r"]);
    assert!(!branches.contains("feat-0"), "{branches}");
    assert!(branches.contains("feat-1"), "{branches}");
}

/// Origin force-pushed back to an already-mirrored commit: the sync is a
/// ref-only manifest swap (no new objects), and the mirror serves the
/// rewound tip.
#[test]
fn force_push_back_mirrors_as_ref_only_change() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-forceback");
    let scratch = Scratch::new("forceback");
    let origins = scratch.path().join("origins");
    let (_bare, tip1) = make_origin(&origins, "acme/rewind", 5);
    let work = origins.join("work").join("acme-rewind");
    let bare = origins.join("acme/rewind.git");

    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}", bare.display());
    server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "rewind", "provider": "generic", "origin": origin_url }),
    );
    let (st, out) = server.sync_now(&admin, "acme", "rewind");
    assert_eq!(st, 200, "{out}");

    // Advance and sync (tip2 mirrored)…
    let tip2 = advance_origin(&origins, "acme/rewind", "extra.txt");
    let (st, _) = server.sync_now(&admin, "acme", "rewind");
    assert_eq!(st, 200);
    // …then force the origin back to tip1.
    gitcli::git(&work, &["reset", "-q", "--hard", &tip1]);
    gitcli::git(
        &work,
        &["push", "-q", "-f", bare.to_str().unwrap(), "main:main"],
    );
    let (st, out) = server.sync_now(&admin, "acme", "rewind");
    assert_eq!(st, 200, "force-back sync is ref-only: {out}");

    let url = server.authed_url(&admin, "acme", "rewind");
    let clone = scratch.path().join("clone");
    let head = gitcli::clone_and_fsck(&url, &clone);
    assert_eq!(head, tip1, "mirror follows the rewound origin");
    let _ = tip2;
}

// ---------------------------------------------------------------------
// Mirrors against an unwell store. Both paths below load the point-read
// plane, and both have to tell "there is no plane" from "I could not
// read the plane" — the first is the ordinary state of a young layout
// and means fall back, the second is an infrastructure failure and means
// say so. Conflating them makes a broken store look like a repository
// with no history, which is the worst possible answer to give a mirror
// user: it is indistinguishable from the origin having deleted the
// commit they asked for.
// ---------------------------------------------------------------------

/// A fault proxy in front of the shared MinIO, with the bucket's direct
/// URL alongside it for assertions about what really landed.
fn proxied(hint: &str) -> (String, stratum_testkit::FaultProxy) {
    let minio = Minio::shared();
    let bucket = minio.bucket(hint);
    let host = bucket
        .base_url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let name = bucket.base_url.rsplit('/').next().unwrap().to_string();
    let proxy = stratum_testkit::FaultProxy::start(&host);
    let proxy = stratum_testkit::FaultProxy {
        url: format!("{}/{name}", proxy.url),
        handle: proxy.handle,
    };
    (bucket.base_url, proxy)
}

/// The background workers, silenced. Every assertion here counts store
/// requests by injecting against a named key, and a compactor or packer
/// waking up mid-test would spend the injection on its own read.
fn quiet_workers() -> Vec<(&'static str, String)> {
    vec![
        ("STRATUM_COMPACT_POLL_SECS", "86400".to_string()),
        // The compaction *sweep* runs on its own interval, not the poll,
        // and reads the layout when it fires — a fault-injection test
        // that silences only the poll still has the sweep racing its
        // injected `locator.hdr` fault.
        ("STRATUM_COMPACT_SWEEP_SECS", "86400".to_string()),
        ("STRATUM_CDNPACK_POLL_SECS", "86400".to_string()),
        ("STRATUM_AUDIT_SHIP_SECS", "0".to_string()),
        // `STRATUM_GC_SECS`, and `0` — not `STRATUM_GC_POLL_SECS`, which
        // no worker reads, and not a long interval, which would be worse
        // than nothing. The GC worker takes `env_secs` where zero means
        // "do not spawn"; the two above take `env_period`, where zero
        // would panic, so a long interval is the only way to quieten
        // them and their first tick still fires at startup.
        //
        // This line said `STRATUM_GC_POLL_SECS` and silenced nothing.
        // It was harmless only because GC defaults to off — and there is
        // an open finding that GC never runs in production *because* of
        // that default, so the day somebody fixes it this test would
        // start failing for a reason nobody would connect to it.
        ("STRATUM_GC_SECS", "0".to_string()),
        // Every sync enqueues a contributions walk, and the contribs
        // worker polls every **5 seconds** by default — it reads
        // `locator.hdr` for the job the sync just queued and, under a
        // slow full-suite coverage run, consumes the fault meant for
        // the sync's own read, so the sync succeeds where the test
        // demanded it fail. Silence it, and the site publisher and the
        // forker for the same reason: in these tests only the sync may
        // read the layout.
        ("STRATUM_CONTRIB_POLL_SECS", "86400".to_string()),
        ("STRATUM_SITEPUBLISH_POLL_SECS", "86400".to_string()),
        ("STRATUM_FORK_POLL_SECS", "86400".to_string()),
    ]
}

/// An incremental sync whose locator plane is unreadable.
///
/// The plane is the sync's dedup oracle: it says which of the origin's
/// objects the layout already holds. Absent, the honest answer is "none
/// of them" — the sync re-sends objects it may already have, which costs
/// bytes and is always correct. Unreadable is a different thing, and
/// answering "none of them" there would fold the store's failure into a
/// silent, permanent inflation of every mirror sync. It has to fail.
#[test]
fn an_incremental_sync_tolerates_a_missing_plane_and_refuses_an_unreadable_one() {
    let (_direct, proxy, scratch, origins) = {
        let (direct, proxy) = proxied("mirror-planefault");
        let scratch = Scratch::new("planefault");
        let origins = scratch.path().join("origins");
        (direct, proxy, scratch, origins)
    };
    make_origin(&origins, "acme/mir", 6);
    let server = spawn_server(&proxy.url, &scratch, &quiet_workers());
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}/acme/mir.git", origins.display());
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "mir", "provider": "generic", "origin": origin_url }),
    );
    assert_eq!(st, 202, "{out}");
    let (st, out) = server.sync_now(&admin, "acme", "mir");
    assert_eq!(st, 200, "{out}");

    // A plane the sync cannot see is not an error: it syncs anyway, and
    // the result is a clonable, fsck-clean mirror at the new tip.
    let tip2 = advance_origin(&origins, "acme/mir", "one.txt");
    proxy.handle.inject("GET locator.hdr", 1, 404);
    let (st, out) = server.sync_now(&admin, "acme", "mir");
    proxy.handle.clear();
    assert_eq!(st, 200, "an absent plane must not fail a sync: {out}");
    let url = server.authed_url(&admin, "acme", "mir");
    let c1 = scratch.path().join("clone-absent");
    assert_eq!(gitcli::clone_and_fsck(&url, &c1), tip2);

    // A plane the store refuses to serve *is* an error, and it is the
    // store's error that comes back — not a quiet, slower sync.
    let tip3 = advance_origin(&origins, "acme/mir", "two.txt");
    // Three: `ObjectStore::get` retries a 5xx twice before giving up.
    proxy.handle.inject("GET locator.hdr", 3, 503);
    let (st, out) = server.sync_now(&admin, "acme", "mir");
    proxy.handle.clear();
    assert!(
        st >= 500,
        "an unreadable plane must fail the sync loudly, got {st}: {out}"
    );

    // The mirror kept serving the state it had while the store was sick…
    let c2 = scratch.path().join("clone-stale");
    assert_eq!(gitcli::clone_and_fsck(&url, &c2), tip2);
    // …and the very same sync lands once the store is well, so the
    // refusal above was the injected fault and nothing else.
    let (st, out) = server.sync_now(&admin, "acme", "mir");
    assert_eq!(st, 200, "{out}");
    let c3 = scratch.path().join("clone-healed");
    assert_eq!(gitcli::clone_and_fsck(&url, &c3), tip3);
}

/// The freshness contract with an unreadable locator plane.
///
/// `ensure_wants` has two failure answers and they mean opposite things.
/// A 404 says the contract was applied: the commit is not here, the
/// origin was asked, and it is not there either — a client is entitled
/// to give up. A 5xx says the contract could not be applied at all. A
/// store that cannot answer must produce the second: telling a client
/// that a commit does not exist upstream, on the strength of a locator
/// read that failed, is a wrong answer that reads like a right one.
#[test]
fn an_unreadable_plane_denies_a_want_as_infrastructure_not_as_absence() {
    let (_direct, proxy) = proxied("mirror-freshfault");
    let scratch = Scratch::new("freshfault");
    let origins = scratch.path().join("origins");
    make_origin(&origins, "acme/fresh", 6);
    // A side branch whose parent is in the layout but is neither a ref
    // tip nor a spine commit: the only want that reaches the plane.
    let work = origins.join("work").join("acme-fresh");
    gitcli::git(&work, &["checkout", "-q", "-b", "sidefault"]);
    std::fs::write(work.join("s1.txt"), "s1\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "side 1"]);
    let side_parent = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    std::fs::write(work.join("s2.txt"), "s2\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "side 2"]);
    let bare = origins.join("acme/fresh.git");
    gitcli::git(
        &work,
        &[
            "push",
            "-q",
            bare.to_str().unwrap(),
            "main:main",
            "sidefault:sidefault",
        ],
    );
    gitcli::git(&work, &["checkout", "-q", "main"]);

    let mut env = quiet_workers();
    env.push(("STRATUM_REF_PAGE_SIZE", "2".to_string()));
    let server = spawn_server(&proxy.url, &scratch, &env);
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}/acme/fresh.git", origins.display());
    server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "fresh", "provider": "generic", "origin": origin_url }),
    );
    let (st, out) = server.sync_now(&admin, "acme", "fresh");
    assert_eq!(st, 200, "{out}");

    // Baseline: the plane knows this want, so it serves with no origin
    // round-trip at all. Everything below is a departure from this.
    let (st, body, _) = raw_fetch_want(&server.base, &admin, "acme", "fresh", &side_parent);
    assert_eq!(st, 200, "locator-known want serves: {body}");

    // The store refuses the locator read: an infrastructure answer, not
    // the contract's "it is not upstream either".
    proxy.handle.inject("GET locator.hdr", 3, 503);
    let (st, body, _) = raw_fetch_want(&server.base, &admin, "acme", "fresh", &side_parent);
    proxy.handle.clear();
    assert!(
        st >= 500,
        "an unreadable plane is not evidence of absence, got {st}: {body}"
    );
    assert!(
        !body.contains("may not exist upstream"),
        "a store failure must not be reported as the origin's answer: {body}"
    );

    // A plane that is merely *absent* is the opposite: the mirror cannot
    // vouch for the want itself, so it asks the origin. With the origin
    // gone that is the contract's 404 — which names the origin and
    // carries the staleness signal, and is exactly the answer a client
    // may act on.
    let hidden = origins.join("acme/fresh.gone");
    std::fs::rename(&bare, &hidden).unwrap();
    proxy.handle.inject("GET locator.hdr", 1, 404);
    let (st, body, headers) = raw_fetch_want(&server.base, &admin, "acme", "fresh", &side_parent);
    proxy.handle.clear();
    assert_eq!(st, 404, "{body}");
    assert!(body.contains("unreachable"), "{body}");
    assert!(
        headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("x-weft-staleness")),
        "the honest-staleness signal is missing: {headers:?}"
    );

    // Origin back, store well: the same want serves from the plane
    // again, so both refusals above were the injected faults.
    std::fs::rename(&hidden, &bare).unwrap();
    let (st, body, _) = raw_fetch_want(&server.base, &admin, "acme", "fresh", &side_parent);
    assert_eq!(st, 200, "{body}");
}

/// **An upstream that changes shape underneath the mirror.**
///
/// Everything here advances an origin by adding commits, which is the
/// one thing a mirror never has to think about: refs only move forward
/// and nothing is ever removed. Real upstreams are not like that. They
/// force-push, they delete branches, and they rename their default
/// branch — and each of those exercises a different part of sync that
/// nothing was covering. `reset --hard` does not appear anywhere in this
/// file.
///
/// Each is a distinct failure if it goes wrong, and none of them is
/// loud: a rewrite the mirror does not follow leaves clones serving
/// history the origin has disowned; a branch the mirror does not prune
/// leaves a ref pointing at objects nobody upstream has any more; a
/// default-branch rename the mirror does not notice leaves every fresh
/// clone checking out the wrong branch, or none.
#[test]
fn a_mirror_follows_an_upstream_that_rewrites_prunes_and_renames() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-drift");
    let scratch = Scratch::new("mirror-drift");
    let origins = scratch.path().join("origins");
    let (bare, _tip) = make_origin(&origins, "acme/widget", 6);
    let work = origins.join("work").join("acme-widget");

    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let origin_url = format!("file://{}/acme/widget.git", origins.display());
    let (status, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "widget", "provider": "generic", "origin": origin_url }),
    );
    assert_eq!(status, 202, "{out}");
    let (status, out) = server.sync_now(&admin, "acme", "widget");
    assert_eq!(status, 200, "{out}");

    // A side branch to delete later, and a tag, so the prune below is
    // about a ref going away rather than about an empty origin.
    gitcli::git(&work, &["checkout", "-q", "-b", "doomed"]);
    std::fs::write(work.join("doomed.txt"), "temporary\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "doomed work"]);
    gitcli::git(
        &work,
        &["push", "-q", bare.to_str().unwrap(), "doomed:doomed"],
    );
    gitcli::git(&work, &["checkout", "-q", "main"]);
    let (status, out) = server.sync_now(&admin, "acme", "widget");
    assert_eq!(status, 200, "{out}");

    let url = server.authed_url(&admin, "acme", "widget");
    let c1 = scratch.path().join("c1");
    gitcli::clone_and_fsck(&url, &c1);
    assert!(
        gitcli::git(&c1, &["ls-remote", &url]).contains("refs/heads/doomed"),
        "the side branch never reached the mirror"
    );

    // --- 1. the upstream rewrites history -------------------------
    //
    // `main` moves to a commit that is not a descendant of the one the
    // mirror holds. A mirror that only ever fast-forwards would keep
    // serving the disowned history and never say so.
    gitcli::git(&work, &["reset", "-q", "--hard", "HEAD~2"]);
    std::fs::write(work.join("rewritten.txt"), "after the rewrite\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "rewritten history"]);
    let rewritten = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    gitcli::git(
        &work,
        &["push", "-q", "-f", bare.to_str().unwrap(), "main:main"],
    );

    let (status, out) = server.sync_now(&admin, "acme", "widget");
    assert_eq!(status, 200, "{out}");
    assert!(
        out["sync_error"].is_null(),
        "a rewritten upstream failed the sync: {out}"
    );

    let c2 = scratch.path().join("c2");
    let head = gitcli::clone_and_fsck(&url, &c2);
    assert_eq!(
        head, rewritten,
        "the mirror is still serving history the origin disowned"
    );
    assert!(
        c2.join("rewritten.txt").exists(),
        "the rewrite's content did not arrive"
    );

    // --- 2. the upstream deletes a branch --------------------------
    gitcli::git(
        &work,
        &["push", "-q", bare.to_str().unwrap(), ":refs/heads/doomed"],
    );
    let (status, out) = server.sync_now(&admin, "acme", "widget");
    assert_eq!(status, 200, "{out}");
    assert!(out["sync_error"].is_null(), "{out}");
    let refs = gitcli::git(&c2, &["ls-remote", &url]);
    assert!(
        !refs.contains("refs/heads/doomed"),
        "a branch the origin deleted is still advertised by the mirror:\n{refs}"
    );
    assert!(refs.contains("refs/heads/main"), "{refs}");

    // --- 3. the upstream renames its default branch ----------------
    gitcli::git(&work, &["branch", "-m", "main", "trunk"]);
    gitcli::git(
        &work,
        &["push", "-q", bare.to_str().unwrap(), "trunk:trunk"],
    );
    gitcli::git(&bare, &["symbolic-ref", "HEAD", "refs/heads/trunk"]);
    gitcli::git(
        &work,
        &["push", "-q", bare.to_str().unwrap(), ":refs/heads/main"],
    );

    let (status, out) = server.sync_now(&admin, "acme", "widget");
    assert_eq!(status, 200, "{out}");
    assert!(
        out["sync_error"].is_null(),
        "a renamed default failed the sync: {out}"
    );

    // The mirror's own default follows, so a fresh clone checks out the
    // branch the origin actually uses rather than landing on nothing.
    let (status, repo) = server.get("/v1/orgs/acme/repos/widget", &admin);
    assert_eq!(status, 200, "{repo}");
    assert_eq!(
        repo["default_branch"],
        serde_json::json!("trunk"),
        "the mirror did not follow the origin's renamed default: {repo}"
    );
    let c3 = scratch.path().join("c3");
    gitcli::clone_and_fsck(&url, &c3);
    assert_eq!(
        gitcli::git(&c3, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "trunk",
        "a fresh clone of the mirror did not check out the origin's default"
    );
    let refs = gitcli::git(&c3, &["ls-remote", &url]);
    assert!(!refs.contains("refs/heads/main"), "{refs}");

    assert_eq!(server.get("/healthz", &admin).0, 200);
}

// ---------------------------------------------------------------------
// Write-through: a push to a mirror is forwarded to its origin. The
// lifecycle test above holds the happy path; these hold every way the
// forward can be refused, and the promise that a refusal changes
// nothing on either side.
// ---------------------------------------------------------------------

/// A registered, synced mirror of `acme/widget` and a clone of it, ready
/// to push. What every forwarding test starts from.
fn mirrored_widget(
    server: &Server,
    scratch: &Scratch,
    origins: &Path,
    admin: &str,
    commits: usize,
) -> (PathBuf, PathBuf, String) {
    let (bare, tip) = make_origin(origins, "acme/widget", commits);
    let origin_url = format!("file://{}/acme/widget.git", origins.display());
    let (status, out) = server.post(
        "/v1/orgs/acme/mirrors",
        admin,
        serde_json::json!({ "name": "widget", "provider": "generic", "origin": origin_url }),
    );
    assert_eq!(status, 202, "{out}");
    let (status, out) = server.sync_now(admin, "acme", "widget");
    assert_eq!(status, 200, "{out}");
    let clone = scratch.path().join("push-clone");
    assert_eq!(
        gitcli::clone_and_fsck(&server.authed_url(admin, "acme", "widget"), &clone),
        tip
    );
    (bare, clone, tip)
}

fn commit_file(clone: &Path, name: &str, msg: &str) -> String {
    std::fs::write(clone.join(name), format!("{name}\n")).unwrap();
    gitcli::git(clone, &["add", "-A"]);
    gitcli::git(clone, &["commit", "-q", "-m", msg]);
    gitcli::git(clone, &["rev-parse", "HEAD"])
        .trim()
        .to_string()
}

fn origin_ref(bare: &Path, name: &str) -> Option<String> {
    let out = gitcli::git(bare, &["for-each-ref", "--format=%(objectname)", name]);
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_string())
}

/// The origin refuses one ref in a two-ref push. The whole push is
/// refused, each command says why in the origin's words, and neither
/// the origin nor the mirror moved — a refused push that landed half
/// of itself somewhere would be worse than the read-only mirror it
/// replaced.
#[test]
fn a_forwarded_push_is_atomic_when_the_origin_refuses_one_ref() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-atomic");
    let scratch = Scratch::new("mirror-atomic");
    let origins = scratch.path().join("origins");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let (bare, clone, tip) = mirrored_widget(&server, &scratch, &origins, &admin, 3);

    // The origin's own policy, in the shape GitHub's takes: one named
    // branch is declined and the others fall with it, each saying which
    // it was.
    //
    // An `update` hook, not `pre-receive`, and that distinction is the
    // whole point. `pre-receive` runs once for the whole push, so a
    // non-zero exit declines *every* ref with the same message — which
    // is what this test used to do, and it meant the innocent sibling
    // was reported as "pre-receive hook declined" citing the blocked
    // branch's own GH006 text. Real GitHub declines only the offending
    // ref and git then marks the siblings `atomic transaction failed`.
    // The fake could not produce that shape, so the suite could not see
    // that `classify` did not know the phrase; only
    // `scripts/manual-mirror-push.sh protected` caught it. `update` runs
    // per ref, which reproduces it.
    let hook = bare.join("hooks/update");
    std::fs::write(
        &hook,
        "#!/bin/sh\nif [ \"$1\" = \"refs/heads/blocked\" ]; then\n  echo \"error: GH006: Protected branch update failed for refs/heads/blocked.\" >&2\n  exit 1\nfi\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let main2 = commit_file(&clone, "fine.txt", "fine on main");
    gitcli::git(&clone, &["branch", "blocked"]);
    let err = gitcli::git_expect_err(&clone, &["push", "-q", "origin", "main", "blocked"]).unwrap();
    // `hook declined`, not `pre-receive hook declined`: an `update` hook
    // is per ref, and that is what git calls it. Real GitHub's phrase is
    // `protected branch hook declined`, which no local hook can produce
    // — that one is pinned against recorded wire by
    // `mirror_push_fixtures_classify_like_the_fake`.
    assert!(
        err.contains("hook declined") && err.contains("GH006"),
        "the origin's refusal, in its own words: {err}"
    );
    // Each ref learns which it was. The offending one carries the
    // origin's own reason; the sibling is told it fell *with* it, and is
    // not handed the blocked branch's GH006 text as though it were its
    // own problem. Before the hook above became an `update` hook, both
    // refs were refused with the same "pre-receive hook declined"
    // message quoting `blocked`, so a pusher was sent looking for a
    // fault in a branch that was fine.
    assert!(
        err.contains("[remote rejected] main -> main (atomic push failed)"),
        "the sibling is named, and named as a sibling: {err}"
    );
    assert!(
        err.contains("[remote rejected] blocked") && err.contains("GH006"),
        "the blocked ref carries the origin's own reason: {err}"
    );
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(tip.as_str())
    );
    assert_eq!(origin_ref(&bare, "refs/heads/blocked"), None);
    let again = scratch.path().join("after-refusal");
    assert_eq!(
        gitcli::clone_and_fsck(&server.authed_url(&admin, "acme", "widget"), &again),
        tip,
        "the mirror did not move either"
    );
    assert_ne!(main2, tip);

    // The same push without the refused ref lands; the server is fine.
    gitcli::git(&clone, &["push", "-q", "origin", "main"]);
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(main2.as_str())
    );
    assert!(server.healthy());
}

/// The origin moved and nobody told the mirror. A push built on what
/// the mirror advertised is refused — the lease it carries is stale at
/// the origin — and the refusal is the mirror catching up and saying
/// so, not the origin's "non-fast-forward". Fetch, rebase, push: lands.
#[test]
fn a_push_against_a_mirror_that_is_behind_its_origin_says_so_and_catches_up() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-behind");
    let scratch = Scratch::new("mirror-behind");
    let origins = scratch.path().join("origins");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let (bare, clone, _tip) = mirrored_widget(&server, &scratch, &origins, &admin, 3);

    let upstream = advance_origin(&origins, "acme/widget", "upstream.txt");
    let mine = commit_file(&clone, "mine.txt", "mine");
    let err = gitcli::git_expect_err(&clone, &["push", "-q", "origin", "main"]).unwrap();
    assert!(
        err.contains("behind its origin") && err.contains("fetch and push again"),
        "{err}"
    );
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(upstream.as_str())
    );

    // The refusal already brought the mirror up to date: a fetch sees
    // the upstream commit without any sync being asked for.
    gitcli::git(&clone, &["fetch", "-q", "origin"]);
    assert_eq!(
        gitcli::git(&clone, &["rev-parse", "origin/main"]).trim(),
        upstream
    );
    gitcli::git(&clone, &["rebase", "-q", "origin/main"]);
    let rebased = gitcli::git(&clone, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    assert_ne!(rebased, mine);
    gitcli::git(&clone, &["push", "-q", "origin", "main"]);
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(rebased.as_str())
    );
    assert!(server.healthy());
}

/// A push that only deletes carries no pack. It is forwarded all the
/// same — a branch and a tag — and the default branch is the one
/// deletion refused here, before the origin is asked.
#[test]
fn a_delete_only_push_forwards_and_the_default_branch_is_refused_locally() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-delete");
    let scratch = Scratch::new("mirror-delete");
    let origins = scratch.path().join("origins");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let (bare, clone, tip) = mirrored_widget(&server, &scratch, &origins, &admin, 3);

    gitcli::git(&clone, &["branch", "topic"]);
    gitcli::git(&clone, &["tag", "v0"]);
    gitcli::git(&clone, &["push", "-q", "origin", "topic", "v0"]);
    assert_eq!(
        origin_ref(&bare, "refs/heads/topic").as_deref(),
        Some(tip.as_str())
    );
    assert_eq!(
        origin_ref(&bare, "refs/tags/v0").as_deref(),
        Some(tip.as_str())
    );

    gitcli::git(&clone, &["push", "-q", "origin", ":topic", ":refs/tags/v0"]);
    assert_eq!(origin_ref(&bare, "refs/heads/topic"), None);
    assert_eq!(origin_ref(&bare, "refs/tags/v0"), None);
    let listed = gitcli::git(&clone, &["ls-remote", "origin"]);
    assert!(
        !listed.contains("refs/heads/topic") && !listed.contains("refs/tags/v0"),
        "the mirror still lists what the origin deleted: {listed}"
    );

    let err = gitcli::git_expect_err(&clone, &["push", "-q", "origin", ":main"]).unwrap();
    assert!(err.contains("default branch"), "{err}");
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(tip.as_str())
    );
    assert!(server.healthy());
}

/// Another node is syncing the mirror when a push arrives. The push
/// waits — the promise is that the mirror shows what the origin took,
/// and that needs the lock — and lands once the other node is done.
/// Held past the deadline, it is refused with the origin untouched:
/// nothing was forwarded before the lock was ours.
#[test]
fn a_forwarded_push_waits_for_a_sync_held_by_another_node() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-pushwait");
    let scratch = Scratch::new("mirror-pushwait");
    let origins = scratch.path().join("origins");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_FRESHNESS_TIMEOUT_SECS", "3".into())],
    );
    let admin = server.bootstrap_org("acme");
    let (bare, clone, tip) = mirrored_widget(&server, &scratch, &origins, &admin, 3);
    let (_, view) = server.get("/v1/orgs/acme/repos/widget", &admin);
    let repo_id = view["id"].as_str().unwrap().to_string();

    let other = stratum_control::ControlDb::open(&server.db_url).expect("second session");
    let held = stratum_control::jobs::try_lock_scoped(&other, "mirror-sync", &repo_id)
        .expect("lock")
        .expect("nobody else holds it yet");
    let c2 = commit_file(&clone, "waited.txt", "waited for the lock");
    let clone_for_push = clone.clone();
    let pusher = std::thread::spawn(move || {
        gitcli::git_expect_err(&clone_for_push, &["push", "-q", "origin", "main"])
    });
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(tip.as_str()),
        "the push went to the origin while another node held the mirror"
    );
    drop(held);
    // `git_expect_err` answers `Err` when the command succeeds.
    let out = pusher.join().unwrap();
    assert!(
        out.is_err(),
        "the push should land once the lock is free: {out:?}"
    );
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(c2.as_str())
    );

    // Past the deadline: refused, and the origin never heard of it.
    let held = stratum_control::jobs::try_lock_scoped(&other, "mirror-sync", &repo_id)
        .expect("lock")
        .expect("free again");
    let c3 = commit_file(&clone, "toolong.txt", "held too long");
    let err = gitcli::git_expect_err(&clone, &["push", "-q", "origin", "main"]).unwrap();
    assert!(err.contains("another node is syncing"), "{err}");
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(c2.as_str())
    );
    assert_ne!(c3, c2);
    drop(held);
    assert!(server.healthy());
}

/// A GitHub mirror registered from a pasted URL has no installation and
/// fetches its origin as a stranger. It cannot push as one either, and
/// says so at the advert — naming the origin and the way to fix it —
/// before the client builds a pack and before any socket is opened.
#[test]
fn a_github_mirror_without_an_installation_cannot_forward() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-nocred");
    let scratch = Scratch::new("mirror-nocred");
    let gh = fake_github::spawn();
    let key_path = scratch.path().join("app-key.pem");
    std::fs::write(&key_path, fake_github::TEST_APP_KEY_PEM).unwrap();
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_GITHUB_APP_ID", "12345".into()),
            ("STRATUM_GITHUB_APP_KEY_PEM", key_path.display().to_string()),
            ("STRATUM_GITHUB_API_BASE", gh.base_url.clone()),
            ("STRATUM_GITHUB_GIT_BASE", "https://github.com".into()),
            (
                "STRATUM_GITHUB_INSTALL_URL",
                "https://github.com/apps/stratum/installations/new".into(),
            ),
        ],
    );
    let admin = server.bootstrap_org("acme");
    // Registered the way a pasted public URL is, minus the probe a real
    // host would need: the row itself, with no installation.
    let db = stratum_control::ControlDb::open(&server.db_url).expect("control plane");
    let mut ctl = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    let org_id: String = ctl
        .query_one("SELECT id FROM orgs WHERE name = 'acme'", &[])
        .unwrap()
        .get(0);
    stratum_control::registry::create_repo(
        &db,
        &org_id,
        &stratum_control::registry::NewRepo {
            name: "public",
            description: None,
            kind: stratum_control::registry::RepoKind::Mirror,
            public: true,
            default_branch: "main",
            origin_url: Some("acme/public"),
            origin_provider: Some("github"),
            origin_installation: None,
        },
    )
    .expect("a mirror row");

    let work = scratch.path().join("work");
    gitcli::fixture_repo(&work, 2);
    let url = server.authed_url(&admin, "acme", "public");
    let err = gitcli::git_expect_err(&work, &["push", "-q", &url, "HEAD:refs/heads/main"]).unwrap();
    assert!(
        err.contains("no credential that can push") && err.contains("acme/public"),
        "{err}"
    );
    assert!(err.contains("connect the GitHub App"), "{err}");

    // The repository page says so before anybody pushes.
    let (st, view) = server.get("/v1/orgs/acme/repos/public", &admin);
    assert_eq!(st, 200, "{view}");
    assert_eq!(view["push"]["forwarding"], false, "{view}");
    assert!(
        view["push"]["blocked"]
            .as_str()
            .is_some_and(|s| s.contains("no credential that can push")),
        "{view}"
    );
    assert_eq!(view["push"]["needs_permission"], false, "{view}");

    // Attaching an installation makes it push-capable — but only one
    // the org has connected. An id it never earned is masked, exactly
    // as creation masks it: the id is a bearer of somebody's source.
    // An id that is not even a plausible installation number is the
    // same 404 — masked before the org membership is ever consulted.
    let (st, out) = server.patch(
        "/v1/orgs/acme/repos/public",
        &admin,
        serde_json::json!({ "installation_id": "not-a-number" }),
    );
    assert_eq!(st, 404, "{out}");
    let (st, out) = server.patch(
        "/v1/orgs/acme/repos/public",
        &admin,
        serde_json::json!({ "installation_id": "4001" }),
    );
    assert_eq!(st, 404, "{out}");
    server.connect_installation("acme", &admin, "4001");
    let (st, out) = server.patch(
        "/v1/orgs/acme/repos/public",
        &admin,
        serde_json::json!({ "installation_id": "4001" }),
    );
    assert_eq!(st, 200, "{out}");
    let (_, view) = server.get("/v1/orgs/acme/repos/public", &admin);
    assert_eq!(view["push"]["forwarding"], true, "{view}");
    assert_eq!(view["origin_installation"], "4001", "{view}");
    // A native repository has no installation to attach.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "native" }),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.patch(
        "/v1/orgs/acme/repos/native",
        &admin,
        serde_json::json!({ "installation_id": "4001" }),
    );
    assert_eq!(st, 400, "{out}");
    assert!(server.healthy());
}

/// The installation behind a mirror was approved before the App asked
/// for `Contents: write`. The push is refused with the permission named
/// and where to approve it, the origin is untouched, and nothing was
/// packed for nothing: the permission is read from the API before the
/// seed is touched.
#[test]
fn a_push_under_an_installation_lacking_contents_write_is_refused_by_name() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-noperm");
    let scratch = Scratch::new("mirror-noperm");
    let origins = scratch.path().join("origins");
    let (bare, tip) = make_origin(&origins, "acme/private", 3);
    let gh = fake_github::spawn();
    let key_path = scratch.path().join("app-key.pem");
    std::fs::write(&key_path, fake_github::TEST_APP_KEY_PEM).unwrap();
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_GITHUB_APP_ID", "12345".into()),
            ("STRATUM_GITHUB_APP_KEY_PEM", key_path.display().to_string()),
            ("STRATUM_GITHUB_API_BASE", gh.base_url.clone()),
            (
                "STRATUM_GITHUB_GIT_BASE",
                format!("file://{}", origins.display()),
            ),
            (
                "STRATUM_GITHUB_INSTALL_URL",
                "https://github.com/apps/stratum/installations/new".into(),
            ),
        ],
    );
    let admin = server.bootstrap_org("acme");
    let mirror_via = |installation: &str, name: &str| {
        server.connect_installation("acme", &admin, installation);
        let (status, out) = server.post(
            "/v1/orgs/acme/mirrors",
            &admin,
            serde_json::json!({
                "name": name,
                "provider": "github",
                "origin": "acme/private",
                "installation_id": installation,
            }),
        );
        assert_eq!(status, 202, "{out}");
        let (status, out) = server.sync_now(&admin, "acme", name);
        assert_eq!(status, 200, "{out}");
    };

    // 4007: runners approved, pushes not.
    mirror_via("4007", "prepush");
    let clone = scratch.path().join("prepush-clone");
    assert_eq!(
        gitcli::clone_and_fsck(&server.authed_url(&admin, "acme", "prepush"), &clone),
        tip
    );
    commit_file(&clone, "nope.txt", "under the old approval");
    let err = gitcli::git_expect_err(&clone, &["push", "-q", "origin", "main"]).unwrap();
    assert!(
        err.contains("Contents: write") && err.contains("installation settings"),
        "{err}"
    );
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(tip.as_str())
    );
    // The page knew before the push did: the repository view, the sync
    // status and the installation list all say which permission is
    // missing and where it is approved.
    let (st, view) = server.get("/v1/orgs/acme/repos/prepush", &admin);
    assert_eq!(st, 200, "{view}");
    assert_eq!(view["push"]["forwarding"], false, "{view}");
    assert_eq!(view["push"]["needs_permission"], true, "{view}");
    assert!(
        view["push"]["approve_url"]
            .as_str()
            .is_some_and(|u| u.contains("installations/4007")),
        "{view}"
    );
    let (st, status) = server.get("/v1/orgs/acme/repos/prepush/sync-status", &admin);
    assert_eq!(st, 200, "{status}");
    assert_eq!(status["push"]["needs_permission"], true, "{status}");
    let (st, list) = server.get("/v1/orgs/acme/github/installations", &admin);
    assert_eq!(st, 200, "{list}");
    let inst = list["installations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["installation_id"] == "4007")
        .expect("4007 is connected");
    assert_eq!(inst["detail"]["push_ready"], false, "{inst}");
    assert_eq!(inst["detail"]["runners_ready"], true, "{inst}");
    assert_eq!(inst["detail"]["contents_write"], false, "{inst}");

    // The same refusal over REST says which permission and where to
    // approve it, so a dashboard can offer the link.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/prepush/commits",
        &admin,
        serde_json::json!({
            "message": "under the old approval",
            "operations": [{"op": "put", "path": "nope.txt", "content": "no"}],
        }),
    );
    assert_eq!(st, 403, "{out}");
    assert_eq!(out["needs_permission"], true, "{out}");
    assert!(
        out["approve_url"]
            .as_str()
            .is_some_and(|u| u.contains("installations/4007")),
        "{out}"
    );
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(tip.as_str())
    );

    // 4001: approved for everything. The same push lands, and the page
    // says it will.
    mirror_via("4001", "current");
    let (_, view) = server.get("/v1/orgs/acme/repos/current", &admin);
    assert_eq!(view["push"]["forwarding"], true, "{view}");
    let (_, list) = server.get("/v1/orgs/acme/github/installations", &admin);
    let inst = list["installations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["installation_id"] == "4001")
        .expect("4001 is connected");
    assert_eq!(inst["detail"]["push_ready"], true, "{inst}");
    let clone2 = scratch.path().join("current-clone");
    gitcli::clone_and_fsck(&server.authed_url(&admin, "acme", "current"), &clone2);
    let c = commit_file(&clone2, "yes.txt", "under the current approval");
    gitcli::git(&clone2, &["push", "-q", "origin", "main"]);
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(c.as_str())
    );
    assert!(server.healthy());
}

/// The REST write routes forward the way the wire does: a commit from
/// tree operations, a branch and a tag made and deleted, a reset and a
/// revert all land on the origin first and are read back from the
/// mirror; a pinned parent that the origin has moved past is a 409
/// with the tip the mirror holds now; and every answer has the shape
/// a native repository's has, so a client need not know which it is
/// talking to.
#[test]
fn rest_writes_on_a_mirror_land_on_the_origin_first() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mirror-rest");
    let scratch = Scratch::new("mirror-rest");
    let origins = scratch.path().join("origins");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let (bare, _clone, tip) = mirrored_widget(&server, &scratch, &origins, &admin, 3);
    let rp = "/v1/orgs/acme/repos/widget";

    // A commit, on top of whatever main is: the origin has it, the
    // mirror serves it, and the answer names the parent it built on.
    let (st, out) = server.post(
        &format!("{rp}/commits"),
        &admin,
        serde_json::json!({
            "message": "over the api",
            "operations": [{"op": "put", "path": "api.txt", "content": "through the mirror\n"}],
        }),
    );
    assert_eq!(st, 201, "{out}");
    let c1 = out["commit"].as_str().unwrap().to_string();
    assert_eq!(out["parent"], tip, "{out}");
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(c1.as_str())
    );
    let (st, log) = server.get(&format!("{rp}/log?limit=1"), &admin);
    assert_eq!(st, 200, "{log}");
    assert_eq!(
        log["entries"][0]["commit"], c1,
        "the mirror serves it: {log}"
    );

    // A pinned parent the origin has moved past: 409 with the tip the
    // mirror holds now, which is the origin's. (The origin's own work
    // tree first catches up with what the mirror forwarded.)
    let work = origins.join("work").join("acme-widget");
    gitcli::git(&work, &["fetch", "-q", bare.to_str().unwrap(), "main"]);
    gitcli::git(&work, &["reset", "-q", "--hard", "FETCH_HEAD"]);
    let upstream = advance_origin(&origins, "acme/widget", "upstream.txt");
    let (st, out) = server.post(
        &format!("{rp}/commits"),
        &admin,
        serde_json::json!({
            "message": "stale",
            "expected_parent": c1,
            "operations": [{"op": "put", "path": "stale.txt", "content": "x"}],
        }),
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(upstream.as_str())
    );
    // And the refusal caught the mirror up, so the same commit unpinned
    // builds on the origin's tip.
    let (st, out) = server.post(
        &format!("{rp}/commits"),
        &admin,
        serde_json::json!({
            "message": "rebuilt",
            "operations": [{"op": "put", "path": "fresh.txt", "content": "y"}],
        }),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["parent"], upstream, "{out}");
    let c2 = out["commit"].as_str().unwrap().to_string();

    // Branch and tag, made and deleted.
    let (st, out) = server.post(
        &format!("{rp}/branches"),
        &admin,
        serde_json::json!({ "name": "topic", "from": "main" }),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(
        origin_ref(&bare, "refs/heads/topic").as_deref(),
        Some(c2.as_str())
    );
    let (st, out) = server.post(
        &format!("{rp}/tags"),
        &admin,
        serde_json::json!({ "name": "v1", "target": "main" }),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(
        origin_ref(&bare, "refs/tags/v1").as_deref(),
        Some(c2.as_str())
    );
    let (st, out) = server.delete(&format!("{rp}/branches/topic"), &admin);
    assert_eq!(st, 204, "{out}");
    assert_eq!(origin_ref(&bare, "refs/heads/topic"), None);
    let (st, out) = server.delete(&format!("{rp}/tags/v1"), &admin);
    assert_eq!(st, 204, "{out}");
    assert_eq!(origin_ref(&bare, "refs/tags/v1"), None);
    let (st, out) = server.delete(&format!("{rp}/tags/v1"), &admin);
    assert_eq!(st, 404, "deleting what is not there: {out}");

    // Reset moves the origin's main back; revert appends the undo.
    let (st, out) = server.post(
        &format!("{rp}/reset"),
        &admin,
        serde_json::json!({ "branch": "main", "to": upstream }),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(upstream.as_str())
    );
    let (st, out) = server.post(
        &format!("{rp}/revert"),
        &admin,
        serde_json::json!({ "branch": "main" }),
    );
    assert_eq!(st, 201, "{out}");
    let reverted = out["commit"].as_str().unwrap().to_string();
    assert_eq!(
        origin_ref(&bare, "refs/heads/main").as_deref(),
        Some(reverted.as_str())
    );
    let check = scratch.path().join("rest-check");
    assert_eq!(
        gitcli::clone_and_fsck(&server.authed_url(&admin, "acme", "widget"), &check),
        reverted
    );
    assert!(server.healthy());
}
