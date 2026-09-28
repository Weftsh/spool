//! End-to-end over the real wire: the compiled stratum-server binary
//! (multi-tenant, authenticated), MinIO, the REST API, and stock git.
//! Every produced clone runs `fsck --full --strict` (I11).

use std::process::{Child, Command};
use std::time::{Duration, Instant};
use stratum_engine::ingest::{publish, PublishMode};
use stratum_engine::{build_locator, ingest, IngestConfig};
use stratum_store::{LatencyModel, ObjectStore};
use stratum_testkit::closure::assert_closed;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::minio::{ROOT_PASSWORD, ROOT_USER};
use stratum_testkit::Minio;

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

fn server_env(cmd: &mut Command, store_url: &str, db: &str) {
    cmd.env_clear()
        // Coverage runs need the instrumented child to write its profile.
        .envs(std::env::var("LLVM_PROFILE_FILE").map(|p| ("LLVM_PROFILE_FILE", p)))
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("STRATUM_STORE_URL", store_url)
        .env("STRATUM_DB_URL", db)
        .env("AWS_ACCESS_KEY_ID", ROOT_USER)
        .env("AWS_SECRET_ACCESS_KEY", ROOT_PASSWORD)
        .env("AWS_REGION", "us-east-1");
}

fn spawn_server(store_url: &str, _scratch: &Scratch) -> Server {
    let db_url = stratum_testkit::pg::test_db_url("git");
    let (child, bind) = stratum_testkit::server::spawn_on_free_port(|bind| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        server_env(&mut cmd, store_url, &db_url);
        cmd.env("STRATUM_BIND", bind);
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
    /// `stratum-server admin bootstrap` against the same control DB.
    fn bootstrap_org(&self, org: &str) -> String {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        server_env(&mut cmd, &self.store_url, &self.db_url);
        let out = cmd
            .args(["admin", "bootstrap", "--org", org])
            .output()
            .expect("run admin bootstrap");
        assert!(
            out.status.success(),
            "bootstrap failed: {}",
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
        flatten(resp)
    }

    fn get_json(&self, path: &str, token: &str) -> (u16, serde_json::Value) {
        let resp = ureq::get(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {token}"))
            .call();
        flatten(resp)
    }

    fn delete(&self, path: &str, token: &str) -> u16 {
        match ureq::delete(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {token}"))
            .call()
        {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(code, _)) => code,
            Err(e) => panic!("transport: {e}"),
        }
    }

    fn authed_url(&self, token: &str, org: &str, repo: &str) -> String {
        let base = self.base.strip_prefix("http://").unwrap();
        format!("http://x:{token}@{base}/{org}/{repo}.git")
    }
}

fn flatten(resp: Result<ureq::Response, ureq::Error>) -> (u16, serde_json::Value) {
    match resp {
        Ok(r) => {
            let status = r.status();
            let body = r.into_string().unwrap_or_default();
            (
                status,
                serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
            )
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            (
                code,
                serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
            )
        }
        Err(e) => panic!("transport: {e}"),
    }
}

#[test]
fn full_stack_create_push_clone_audit() {
    let minio = Minio::shared();
    let bucket = minio.bucket("e2e-stack");
    let scratch = Scratch::new("e2e-stack");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");

    // R1: create a repo through the API.
    let (status, repo) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "app" }),
    );
    assert_eq!(status, 201, "{repo}");
    assert_eq!(repo["name"], "app");
    assert!(repo["clone_url"]
        .as_str()
        .unwrap()
        .ends_with("/acme/app.git"));

    // Clone the empty repo, commit, push over the wire with a token.
    let url = server.authed_url(&admin, "acme", "app");
    let work = scratch.path().join("work");
    gitcli::git(
        scratch.path(),
        &["clone", "-q", &url, work.to_str().unwrap()],
    );
    gitcli::git(&work, &["checkout", "-q", "-b", "main"]);
    std::fs::write(work.join("hello.txt"), "hello stratum\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "first commit"]);
    let tip = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    gitcli::git(&work, &["push", "-q", "origin", "main"]);

    // A fresh clone sees the pushed commit, fsck-clean.
    let verify = scratch.path().join("verify");
    let head = gitcli::clone_and_fsck(&url, &verify);
    assert_eq!(head, tip);
    assert_eq!(
        std::fs::read_to_string(verify.join("hello.txt")).unwrap(),
        "hello stratum\n"
    );

    // Batch create + list + delete.
    let (status, out) = server.post(
        "/v1/orgs/acme/repos/batch/create",
        &admin,
        serde_json::json!({ "repos": (0..20).map(|i| serde_json::json!({"name": format!("fleet-{i}")})).collect::<Vec<_>>() }),
    );
    assert_eq!(status, 200, "{out}");
    assert!(out["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["ok"] == true));

    let (status, list) = server.get_json("/v1/orgs/acme/repos?limit=10", &admin);
    assert_eq!(status, 200);
    assert_eq!(list["repos"].as_array().unwrap().len(), 10);
    assert!(list["next_after"].is_string());

    assert_eq!(server.delete("/v1/orgs/acme/repos/fleet-0", &admin), 204);
    let (status, _) = server.get_json("/v1/orgs/acme/repos/fleet-0", &admin);
    assert_eq!(status, 404);

    // Audit: create + push are on the record with the acting principal.
    let (status, audit) = server.get_json("/v1/orgs/acme/audit", &admin);
    assert_eq!(status, 200);
    let actions: Vec<&str> = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    assert!(actions.contains(&"repo.create"));
    assert!(actions.contains(&"repo.push"));
    assert!(actions.contains(&"repo.delete"));
    assert!(audit["entries"][0]["principal"]
        .as_str()
        .unwrap()
        .starts_with("token:"));

    // A push, ten repo creations and a delete all moved objects; the
    // clones passing proves the read path, not that the store is closed.
    assert_closed(&bucket.base_url);
}

#[test]
fn ingested_layout_serves_under_a_tenant_prefix() {
    let minio = Minio::shared();
    let bucket = minio.bucket("e2e-tenant-ingest");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let scratch = Scratch::new("e2e-tenant");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");

    let (status, repo) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "seeded" }),
    );
    assert_eq!(status, 201);
    let prefix = format!(
        "o/{}/r/{}/prod",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );

    // Ingest a fixture into the tenant prefix (replacing the empty
    // manifest) — the P3 mirror sync will do exactly this.
    let upstream = scratch.path().join("upstream");
    let tip = gitcli::fixture_repo(&upstream, 12);
    let cfg = IngestConfig {
        budget_bytes: 8 * 1024,
        hot_commits: 6,
        hot_budget_bytes: 16 * 1024,
        hot_anchor: 3,
        ..IngestConfig::default()
    };
    let mut out = ingest(
        &upstream,
        &prefix,
        "main",
        &cfg,
        &scratch.path().join("staging"),
    )
    .unwrap();
    let hdr = build_locator(&upstream, &mut out, &prefix, 0).unwrap();
    publish(&store, &prefix, &out, &hdr, PublishMode::Replace).unwrap();

    let url = server.authed_url(&admin, "acme", "seeded");
    let clone = scratch.path().join("clone");
    let head = gitcli::clone_and_fsck(&url, &clone);
    assert_eq!(head, tip);

    // `publish` wrote the tiered layout directly, bypassing every server
    // path — this is the one place the ingest writer is the only author.
    assert_closed(&bucket.base_url);
}

#[test]
fn isolation_and_auth_matrix() {
    let minio = Minio::shared();
    let bucket = minio.bucket("e2e-isolation");
    let scratch = Scratch::new("e2e-iso");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin_a = server.bootstrap_org("org-a");
    let admin_b = server.bootstrap_org("org-b");

    let (status, _) = server.post(
        "/v1/orgs/org-a/repos",
        &admin_a,
        serde_json::json!({ "name": "secret" }),
    );
    assert_eq!(status, 201);

    // Org B's valid credentials can never see org A's repo — 404, not 403.
    let (status, _) = server.get_json("/v1/orgs/org-a/repos/secret", &admin_b);
    assert_eq!(status, 404);
    let (status, _) = server.get_json("/v1/orgs/org-a/repos", &admin_b);
    assert_eq!(status, 404);

    // Anonymous REST read of a private repo: 401 (no existence leak
    // before credentials).
    let resp = ureq::get(&format!("{}/v1/orgs/org-a/repos/secret", server.base)).call();
    match resp {
        Err(ureq::Error::Status(code, _)) => assert_eq!(code, 401),
        other => panic!("expected 401, got {other:?}"),
    }

    // Git wire: org B token on org A's repo → 404 during advert.
    let err = gitcli::git_expect_err(
        scratch.path(),
        &[
            "clone",
            "-q",
            &server.authed_url(&admin_b, "org-a", "secret"),
            scratch.path().join("nope").to_str().unwrap(),
        ],
    )
    .unwrap();
    assert!(err.contains("not found") || err.contains("404"), "{err}");

    // Repo-scoped token: works on its repo, invisible elsewhere.
    let (status, minted) = server.post(
        "/v1/orgs/org-a/tokens",
        &admin_a,
        serde_json::json!({ "scopes": ["repo:read"], "repo": "secret" }),
    );
    assert_eq!(status, 201, "{minted}");
    let scoped = minted["token"].as_str().unwrap().to_string();
    let clone_dir = scratch.path().join("scoped-clone");
    gitcli::git(
        scratch.path(),
        &[
            "clone",
            "-q",
            &server.authed_url(&scoped, "org-a", "secret"),
            clone_dir.to_str().unwrap(),
        ],
    );
    // …but cannot create repos or read the org listing.
    let (status, _) = server.post(
        "/v1/orgs/org-a/repos",
        &scoped,
        serde_json::json!({ "name": "sneaky" }),
    );
    assert_eq!(status, 404);

    // Revocation is instant.
    let token_id = minted["id"].as_str().unwrap();
    assert_eq!(
        server.delete(&format!("/v1/orgs/org-a/tokens/{token_id}"), &admin_a),
        204
    );
    let err = gitcli::git_expect_err(
        scratch.path(),
        &[
            "clone",
            "-q",
            &server.authed_url(&scoped, "org-a", "secret"),
            scratch.path().join("revoked").to_str().unwrap(),
        ],
    )
    .unwrap();
    assert!(
        err.contains("401") || err.contains("Authentication") || err.contains("authentication"),
        "{err}"
    );
}

#[test]
fn v2_gate_still_enforced() {
    let minio = Minio::shared();
    let bucket = minio.bucket("e2e-v2gate");
    let scratch = Scratch::new("e2e-v2");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    let (status, _) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "app" }),
    );
    assert_eq!(status, 201);

    let advert = |repo: &str, token: &str, v2: bool| -> u16 {
        let mut r = ureq::get(&format!(
            "{}/acme/{repo}/info/refs?service=git-upload-pack",
            server.base
        ));
        if !token.is_empty() {
            r = r.set("Authorization", &format!("Bearer {token}"));
        }
        if v2 {
            r = r.set("Git-Protocol", "version=2");
        }
        match r.call() {
            Ok(x) => x.status(),
            Err(ureq::Error::Status(c, _)) => c,
            Err(e) => panic!("transport: {e}"),
        }
    };

    // The advert works for a reader with the v2 header…
    assert_eq!(advert("app", &admin, true), 200);

    // …and a v0 client is refused loudly.
    assert_eq!(
        advert("app", &admin, false),
        400,
        "expected 400 for a v0 client"
    );

    // The protocol gate is not an existence oracle: anonymous is told to
    // authenticate, in either protocol, for a repository that exists and
    // one that does not — never "wrong protocol" for the real one only.
    for v2 in [true, false] {
        assert_eq!(advert("app", "", v2), 401, "anonymous, v2={v2}");
        assert_eq!(advert("ghost", "", v2), 401, "anonymous absent, v2={v2}");
    }
    assert!(server.healthy());
}

/// Push a branch, delete it, push it again. Ordinary, and it was refused.
///
/// Deleting a ref does not delete its objects, so the second push carried
/// objects the layout still held and the server answered
/// "object … already present (concurrent push?) — fetch and retry". The
/// advice could not work: the object is unreferenced, so no fetch brings
/// it and the client already has it. Anyone who tidied a branch and then
/// wanted it back was stuck on that commit until GC, with amending it into
/// a different sha as the only escape — and nothing said so.
///
/// The check was defending I5, which is real: `clone_plan` appends every
/// WAL segment whole after the locator's segments, so an object in both
/// would reach the client twice. Dropping the duplicates upholds I5 without
/// refusing the push, which is what `api::commits` and `mirror::sync`
/// already do.
///
/// This one re-pushes the *whole* branch, so every object in it is already
/// present and there is nothing new to store at all — the case where the
/// push writes no WAL segment and is only a ref update.
#[test]
fn a_branch_pushed_deleted_and_pushed_again_is_accepted() {
    let minio = Minio::shared();
    let bucket = minio.bucket("e2e-repush");
    let scratch = Scratch::new("e2e-repush");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    let (status, _) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "app" }),
    );
    assert_eq!(status, 201);

    let url = server.authed_url(&admin, "acme", "app");
    let work = scratch.path().join("work");
    gitcli::git(
        scratch.path(),
        &["clone", "-q", &url, work.to_str().unwrap()],
    );
    gitcli::git(&work, &["checkout", "-q", "-b", "main"]);
    std::fs::write(work.join("a.txt"), "a\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "base"]);
    gitcli::git(&work, &["push", "-q", "origin", "main"]);

    gitcli::git(&work, &["checkout", "-q", "-b", "feature-x"]);
    std::fs::write(work.join("b.txt"), "b\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "work"]);
    let tip = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    gitcli::git(&work, &["push", "-q", "origin", "feature-x"]);
    gitcli::git(&work, &["push", "-q", "origin", "--delete", "feature-x"]);
    // The push this test exists for.
    gitcli::git(&work, &["push", "-q", "origin", "feature-x"]);

    // It is really there, and the clone is still sound (I11). fsck is the
    // assertion that matters: if the segment had carried an object the
    // layout already held, the client would receive it twice.
    let verify = scratch.path().join("verify");
    gitcli::clone_and_fsck(&url, &verify);
    let back = gitcli::git(&verify, &["rev-parse", "origin/feature-x"])
        .trim()
        .to_string();
    assert_eq!(
        back, tip,
        "the re-pushed branch does not point at its commit"
    );
    assert_closed(&bucket.base_url);
}

/// The other half: a push carrying some objects the layout holds and some
/// it does not. The duplicates are dropped and the new ones still land, so
/// the segment written here holds exactly what was missing.
#[test]
fn a_push_mixing_held_and_new_objects_keeps_the_new_ones() {
    let minio = Minio::shared();
    let bucket = minio.bucket("e2e-mixed");
    let scratch = Scratch::new("e2e-mixed");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    let (status, _) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "app" }),
    );
    assert_eq!(status, 201);

    let url = server.authed_url(&admin, "acme", "app");
    let work = scratch.path().join("work");
    gitcli::git(
        scratch.path(),
        &["clone", "-q", &url, work.to_str().unwrap()],
    );
    gitcli::git(&work, &["checkout", "-q", "-b", "main"]);
    std::fs::write(work.join("a.txt"), "a\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "base"]);
    gitcli::git(&work, &["push", "-q", "origin", "main"]);

    // One commit pushed and then orphaned by deleting its branch…
    gitcli::git(&work, &["checkout", "-q", "-b", "feature-x"]);
    std::fs::write(work.join("b.txt"), "b\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "held"]);
    gitcli::git(&work, &["push", "-q", "origin", "feature-x"]);
    gitcli::git(&work, &["push", "-q", "origin", "--delete", "feature-x"]);

    // …and a second commit on top of it, so the next push carries both the
    // orphaned objects and genuinely new ones.
    std::fs::write(work.join("c.txt"), "c\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "new"]);
    let tip = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    gitcli::git(&work, &["push", "-q", "origin", "feature-x"]);

    let verify = scratch.path().join("verify");
    gitcli::clone_and_fsck(&url, &verify);
    let back = gitcli::git(&verify, &["rev-parse", "origin/feature-x"])
        .trim()
        .to_string();
    assert_eq!(back, tip);
    // Both files, read out of the clone's own object store rather than its
    // worktree — the clone has `main` checked out, and these live on the
    // branch. `b.txt` is the one that matters: its blob was among the
    // objects dropped as already-present, so reading it back proves the
    // drop removed a redundant *copy* and not the object.
    assert_eq!(
        gitcli::git(&verify, &["show", "origin/feature-x:b.txt"]),
        "b\n"
    );
    assert_eq!(
        gitcli::git(&verify, &["show", "origin/feature-x:c.txt"]),
        "c\n"
    );
    assert_closed(&bucket.base_url);
}

/// **A fast-forward that brings back a blob the server already holds.**
///
/// `git revert`, restoring a deleted file, changing a line back — every
/// one of these makes a commit whose tree points at a blob that is
/// already in the layout under an earlier commit, and git sends that blob
/// again because, from the client's side, it is part of the new commit.
/// The server refused the whole push with "object … already present
/// (concurrent push?) — fetch and retry": no force, no second client, no
/// concurrency, and advice that cannot work because the object is on both
/// sides already. That commit could never be pushed.
///
/// The refusal was kept on updates on purpose, because on a *stale force
/// push* the duplicates are what give it away — the client does not have
/// the tip the server advertised, so it re-sends live history. But a
/// fast-forward has the advertised tip in its own ancestry by definition;
/// it takes nobody's work, exactly like a create, and the duplicates are
/// dropped the same way. The refusal now stands only where the old tip is
/// not reachable from the new one, which is the case
/// `a_force_push_rewrites_an_unprotected_branch_and_never_somebody_elses_work`
/// pins.
#[test]
fn a_fast_forward_that_reintroduces_a_held_blob_is_accepted() {
    let minio = Minio::shared();
    let bucket = minio.bucket("e2e-revert");
    let scratch = Scratch::new("e2e-revert");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    let (status, _) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "app" }),
    );
    assert_eq!(status, 201);

    let url = server.authed_url(&admin, "acme", "app");
    let work = scratch.path().join("work");
    gitcli::git(
        scratch.path(),
        &["clone", "-q", &url, work.to_str().unwrap()],
    );
    gitcli::git(&work, &["checkout", "-q", "-b", "main"]);
    std::fs::write(work.join("f.txt"), "AAA\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "c1"]);
    gitcli::git(&work, &["push", "-q", "origin", "main"]);

    // Change it, push, change it back, push: three ordinary fast-forwards
    // on a branch nobody else touches. The third re-sends the AAA blob.
    gitcli::git(&work, &["checkout", "-q", "-b", "t"]);
    std::fs::write(work.join("f.txt"), "BBB\n").unwrap();
    gitcli::git(&work, &["commit", "-q", "-am", "c2"]);
    gitcli::git(&work, &["push", "-q", "origin", "t"]);
    std::fs::write(work.join("f.txt"), "AAA\n").unwrap();
    gitcli::git(&work, &["commit", "-q", "-am", "c3"]);
    gitcli::git(&work, &["push", "-q", "origin", "t"]);

    // And the way most people hit it: `git revert`, whose tree is the
    // one from before the reverted commit, blob for blob.
    std::fs::write(work.join("g.txt"), "gone soon\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "add g"]);
    gitcli::git(&work, &["push", "-q", "origin", "t"]);
    gitcli::git(&work, &["rm", "-q", "g.txt"]);
    gitcli::git(&work, &["commit", "-q", "-m", "remove g"]);
    gitcli::git(&work, &["push", "-q", "origin", "t"]);
    gitcli::git(&work, &["revert", "--no-edit", "HEAD"]);
    gitcli::git(&work, &["push", "-q", "origin", "t"]);
    let tip = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // Every push landed, and a clone is still sound (I11): the duplicates
    // were dropped from the segment, not stored twice.
    let verify = scratch.path().join("verify");
    gitcli::clone_and_fsck(&url, &verify);
    assert_eq!(
        gitcli::git(&verify, &["rev-parse", "origin/t"]).trim(),
        tip,
        "the branch does not point at the reverting commit"
    );
    assert_eq!(gitcli::git(&verify, &["show", "origin/t:f.txt"]), "AAA\n");
    assert_eq!(
        gitcli::git(&verify, &["show", "origin/t:g.txt"]),
        "gone soon\n"
    );
    assert_closed(&bucket.base_url);
}

/// **`git commit --amend`, then `git push --force-with-lease`.**
///
/// The most ordinary answer to review there is: the contributor adds a
/// directory and a note in one commit, is asked to reword the note, amends,
/// and force-pushes their own branch. The new commit's tree still points at
/// the untouched directory, and git sends that subtree and its blob again
/// — it excludes the objects of the commit it replaces at the *commit*
/// level, but only walks the trees of boundary commits, and the replaced
/// tip is a sibling of the new one, not its parent. The server saw a
/// rewrite carrying objects it already held and refused it as a stale
/// client: "object … already present (concurrent push?) — fetch and
/// retry", against a branch nobody else had touched, with advice that
/// cannot work because the client has the tip and just fetched it.
///
/// What gives a stale client away is not a re-sent tree — a client that
/// holds the advertised tip re-sends trees and blobs from under it every
/// day, for the reason above — but a re-sent **commit**: the history under
/// the advertised tip that only a client without that tip would send. The
/// refusal now stands on that alone, which is what
/// `a_force_push_rewrites_an_unprotected_branch_and_never_somebody_elses_work`
/// exercises; the shape here is accepted and the duplicates are dropped
/// from the segment as on every fast-forward.
#[test]
fn an_amend_that_keeps_a_subtree_can_be_force_pushed() {
    let minio = Minio::shared();
    let bucket = minio.bucket("e2e-amend");
    let scratch = Scratch::new("e2e-amend");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    let (status, _) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "app" }),
    );
    assert_eq!(status, 201);

    let url = server.authed_url(&admin, "acme", "app");
    let work = scratch.path().join("work");
    gitcli::git(
        scratch.path(),
        &["clone", "-q", &url, work.to_str().unwrap()],
    );
    gitcli::git(&work, &["checkout", "-q", "-b", "main"]);
    std::fs::write(work.join("README.md"), "app\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "seed"]);
    gitcli::git(&work, &["push", "-q", "origin", "main"]);

    // One commit that adds a directory and a note beside it.
    gitcli::git(&work, &["checkout", "-q", "-b", "contrib"]);
    std::fs::create_dir_all(work.join(".weft")).unwrap();
    std::fs::write(work.join(".weft/ci.yml"), "jobs: {}\n").unwrap();
    std::fs::write(work.join("NOTE.md"), "first wording\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "a workflow and a note"]);
    gitcli::git(&work, &["push", "-q", "origin", "contrib"]);
    let first = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // Review asks for a different note. The directory is untouched, so the
    // amended tree shares `.weft` with the commit it replaces.
    std::fs::write(work.join("NOTE.md"), "second wording\n").unwrap();
    gitcli::git(&work, &["commit", "-q", "-a", "--amend", "--no-edit"]);
    let second = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    assert_ne!(first, second);
    assert_eq!(
        gitcli::git(&work, &["rev-parse", &format!("{first}^{{tree}}:.weft")]),
        gitcli::git(&work, &["rev-parse", &format!("{second}^{{tree}}:.weft")]),
        "the shape under test needs a subtree the amend kept"
    );
    gitcli::git(
        &work,
        &["push", "-q", "--force-with-lease", "origin", "contrib"],
    );

    // And once more with plain --force, which fills `old` from the advert
    // — the same shape, and the same client that holds the tip.
    std::fs::write(work.join("NOTE.md"), "third wording\n").unwrap();
    gitcli::git(&work, &["commit", "-q", "-a", "--amend", "--no-edit"]);
    let third = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    gitcli::git(&work, &["push", "-q", "-f", "origin", "contrib"]);

    // The branch is the amended history, and a clone of it is sound (I11):
    // the re-sent subtree was dropped from the segment, not stored twice.
    let verify = scratch.path().join("verify");
    gitcli::clone_and_fsck(&url, &verify);
    assert_eq!(
        gitcli::git(&verify, &["rev-parse", "origin/contrib"]).trim(),
        third,
        "the force push did not move the branch to the amended commit"
    );
    assert_eq!(
        gitcli::git(&verify, &["show", "origin/contrib:NOTE.md"]),
        "third wording\n"
    );
    assert_eq!(
        gitcli::git(&verify, &["show", "origin/contrib:.weft/ci.yml"]),
        "jobs: {}\n"
    );
    assert_closed(&bucket.base_url);
}
