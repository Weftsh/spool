//! Real-world, multi-step development workflows end-to-end: the compiled
//! binary driven the way a team actually uses it day to day — onboarding,
//! feature-branch work reviewed over REST while CI pulls over the wire,
//! an origin incident in the middle of a working session, a bad commit
//! recovered live, and a credential rotated without stopping work. Every
//! clone the server produces is fsck'd (I11).

use std::time::{Duration, Instant};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::{Minio, Server};

const WEBHOOK_SECRET: &str = "workflow-hook-secret";

fn spawn_server_with(store_url: &str, scratch: &Scratch, extra: &[(&str, String)]) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint("workflow")
        .data_dir(scratch.path().join("data"))
        .envs(extra)
        .env("STRATUM_WEBHOOK_SECRET", WEBHOOK_SECRET)
        .start()
}

fn spawn_server(store_url: &str, scratch: &Scratch) -> Server {
    spawn_server_with(store_url, scratch, &[])
}

fn commit_ops(
    server: &Server,
    token: &str,
    rp: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    server.req("POST", &format!("{rp}/commits"), token, Some(body))
}

fn head_of(server: &Server, token: &str, rp: &str, branch: &str) -> String {
    let (st, refs) = server.req("GET", &format!("{rp}/refs"), token, None);
    assert_eq!(st, 200, "{refs}");
    refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == format!("refs/heads/{branch}"))
        .unwrap_or_else(|| panic!("no branch {branch} in {refs}"))["oid"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A team onboards: org, repo, scoped tokens for a developer and for CI.
/// The developer works a feature branch over REST while CI repeatedly
/// pulls main over the wire; the branch is reviewed (log + diff),
/// fast-forwarded into main, tagged, and exported — and the export
/// bundle is a standalone, fsck-clean repository.
#[test]
fn feature_branch_review_release_and_export() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wf-feature");
    let scratch = Scratch::new("wf-feature");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");

    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "webapp" })),
    );
    assert_eq!(st, 201);
    let rp = "/v1/orgs/acme/repos/webapp";

    // Scoped credentials: the developer writes, CI only reads.
    let (st, dev_tok) = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({ "scopes": ["repo:write"], "repo": "webapp", "label": "dev" })),
    );
    assert_eq!(st, 201, "{dev_tok}");
    let dev = dev_tok["token"].as_str().unwrap().to_string();
    let (st, ci_tok) = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({ "scopes": ["repo:read"], "repo": "webapp", "label": "ci" })),
    );
    assert_eq!(st, 201, "{ci_tok}");
    let ci = ci_tok["token"].as_str().unwrap().to_string();

    // Initial code lands over the wire, like a real first push (the
    // fresh repo is empty, so no fsck until content exists).
    let dev_clone = scratch.path().join("dev");
    gitcli::git(
        scratch.path(),
        &[
            "clone",
            "-q",
            &server.authed_url(&dev, "acme", "webapp"),
            dev_clone.to_str().unwrap(),
        ],
    );
    gitcli::git(&dev_clone, &["checkout", "-q", "-b", "main"]);
    std::fs::create_dir_all(dev_clone.join("src")).unwrap();
    std::fs::write(dev_clone.join("src/main.js"), "boot()\n").unwrap();
    std::fs::write(dev_clone.join("README.md"), "# webapp\n").unwrap();
    gitcli::git(&dev_clone, &["add", "-A"]);
    gitcli::git(&dev_clone, &["commit", "-q", "-m", "initial import"]);
    gitcli::git(&dev_clone, &["push", "-q", "origin", "main"]);
    let main0 = head_of(&server, &admin, rp, "main");

    // CI's read-only token clones and fscks; it cannot write.
    let ci_clone = scratch.path().join("ci-1");
    gitcli::clone_and_fsck(&server.authed_url(&ci, "acme", "webapp"), &ci_clone);
    let (st, _) = commit_ops(
        &server,
        &ci,
        rp,
        serde_json::json!({
            "message": "ci must not write",
            "operations": [{ "op": "put", "path": "x", "content": "x" }],
        }),
    );
    // Denials are masked as 404 (the R8 convention: a token is never
    // told what exists beyond what it can act on).
    assert_eq!(st, 404, "read-only CI token wrote");

    // Feature branch: created from main, iterated over REST with CAS.
    let (st, _) = server.req(
        "POST",
        &format!("{rp}/branches"),
        &dev,
        Some(serde_json::json!({ "name": "feature-auth", "from": main0 })),
    );
    assert_eq!(st, 201);
    let (st, f1) = commit_ops(
        &server,
        &dev,
        rp,
        serde_json::json!({
            "branch": "feature-auth",
            "expected_parent": main0,
            "message": "add login route",
            "operations": [
                { "op": "put", "path": "src/auth/login.js", "content": "login()\n" },
            ],
        }),
    );
    assert_eq!(st, 201, "{f1}");
    let f1_oid = f1["commit"].as_str().unwrap().to_string();
    let (st, f2) = commit_ops(
        &server,
        &dev,
        rp,
        serde_json::json!({
            "branch": "feature-auth",
            "expected_parent": f1_oid,
            "message": "wire login into main",
            "operations": [
                { "op": "put", "path": "src/main.js", "content": "boot()\nlogin()\n" },
            ],
        }),
    );
    assert_eq!(st, 201, "{f2}");
    let f2_oid = f2["commit"].as_str().unwrap().to_string();

    // Review: the branch diff against main names exactly the touched files.
    let (st, diff) = server.req(
        "GET",
        &format!("{rp}/diff?from={main0}&to={f2_oid}"),
        &admin,
        None,
    );
    assert_eq!(st, 200);
    let changed: Vec<&str> = diff["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(changed.len(), 2, "{diff}");
    assert!(changed.contains(&"src/auth/login.js") && changed.contains(&"src/main.js"));

    // Meanwhile main hasn't moved, so the merge is a fast-forward: reset
    // main to the reviewed tip under CAS.
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/reset"),
        &dev,
        Some(serde_json::json!({ "branch": "main", "to": f2_oid, "expected_head": main0 })),
    );
    assert_eq!(st, 200, "{out}");

    // CI picks up the release-to-be: fresh clone sees the feature, fsck-clean.
    let ci2 = scratch.path().join("ci-2");
    let head = gitcli::clone_and_fsck(&server.authed_url(&ci, "acme", "webapp"), &ci2);
    assert_eq!(head, f2_oid);
    assert!(ci2.join("src/auth/login.js").exists());

    // Tag the release; the tag serves over the wire.
    let (st, _) = server.req(
        "POST",
        &format!("{rp}/tags"),
        &dev,
        Some(serde_json::json!({ "name": "v1.0.0", "target": f2_oid })),
    );
    assert_eq!(st, 201);
    gitcli::git(&ci2, &["fetch", "-q", "origin", "--tags"]);
    assert_eq!(
        gitcli::git(&ci2, &["rev-parse", "v1.0.0^{commit}"]).trim(),
        f2_oid
    );

    // Export the release: job → bundle → offline clone → fsck.
    let (st, job) = server.req("POST", &format!("{rp}/export"), &admin, None);
    assert_eq!(st, 202, "{job}");
    let job_id = job["job"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, status) = server.req("GET", &format!("{rp}/export/{job_id}"), &admin, None);
        match status["state"].as_str().unwrap_or("") {
            "done" => break,
            "failed" => panic!("export failed: {status}"),
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            _ => panic!("export never finished"),
        }
    }
    let bundle = scratch.path().join("release.bundle");
    let resp = ureq::get(&format!("{}{rp}/export/{job_id}/download", server.base))
        .set("Authorization", &format!("Bearer {admin}"))
        .call()
        .unwrap();
    let mut buf = Vec::new();
    use std::io::Read;
    resp.into_reader().read_to_end(&mut buf).unwrap();
    assert!(!buf.is_empty(), "empty bundle download");
    std::fs::write(&bundle, &buf).unwrap();
    let offline = scratch.path().join("offline");
    gitcli::git(
        scratch.path(),
        &[
            "clone",
            "-q",
            bundle.to_str().unwrap(),
            offline.to_str().unwrap(),
        ],
    );
    gitcli::fsck(&offline);
    assert_eq!(
        gitcli::git(&offline, &["rev-parse", "HEAD"]).trim(),
        f2_oid,
        "bundle head is the released tip"
    );
}

/// An origin incident in the middle of a working session: the mirror
/// serves fresh content, the origin dies, reads keep working on
/// last-known state, and recovery resumes the feed — the exact sequence
/// an on-call engineer lives through.
#[test]
fn origin_incident_during_active_mirror_use() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wf-incident");
    let scratch = Scratch::new("wf-incident");
    let origins = scratch.path().join("origins");

    // Origin with history, mirrored and cloned.
    let work = origins.join("work");
    let tip1 = gitcli::fixture_repo(&work, 8);
    let bare = origins.join("upstream.git");
    gitcli::git(
        &origins,
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

    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(
            serde_json::json!({ "name": "upstream", "provider": "generic", "origin": origin_url }),
        ),
    );
    assert_eq!(st, 202, "{out}");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors/upstream/sync",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");

    let url = server.authed_url(&admin, "acme", "upstream");
    let consumer = scratch.path().join("consumer");
    assert_eq!(gitcli::clone_and_fsck(&url, &consumer), tip1);

    // Normal development continues at the origin; the consumer fetches
    // the new commit explicitly (want-miss → synchronous sync).
    std::fs::write(work.join("hotfix.txt"), "fix\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "hotfix"]);
    gitcli::git(&work, &["push", "-q", bare.to_str().unwrap(), "main:main"]);
    let tip2 = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    gitcli::git(&consumer, &["fetch", "-q", "origin", &tip2]);
    assert_eq!(
        gitcli::git(&consumer, &["rev-parse", "FETCH_HEAD"]).trim(),
        tip2
    );

    // INCIDENT: the origin disappears mid-session.
    let hidden = origins.join("upstream.gone");
    std::fs::rename(&bare, &hidden).unwrap();
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors/upstream/sync",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 502, "sync against dead origin fails loudly: {out}");

    // Known content still serves during the outage — a fresh CI job can
    // still clone what the mirror has, fsck-clean.
    let during = scratch.path().join("during-outage");
    assert_eq!(gitcli::clone_and_fsck(&url, &during), tip2);

    // RECOVERY: origin returns and moves forward; the mirror resumes.
    std::fs::rename(&hidden, &bare).unwrap();
    std::fs::write(work.join("postmortem.md"), "what happened\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "postmortem"]);
    gitcli::git(&work, &["push", "-q", bare.to_str().unwrap(), "main:main"]);
    let tip3 = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors/upstream/sync",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "recovery sync: {out}");
    gitcli::git(&during, &["fetch", "-q", "origin"]);
    assert_eq!(
        gitcli::git(&during, &["rev-parse", "origin/main"]).trim(),
        tip3
    );
    gitcli::fsck(&during);
}

/// A bad commit lands, is caught, reverted, then a deeper mistake forces
/// a reset — while the credential doing the work is rotated mid-stream.
/// History stays honest: the revert preserves it, the reset's orphan
/// stays reachable by SHA, and the old token dies instantly.
#[test]
fn mistake_recovery_with_mid_stream_token_rotation() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wf-recover");
    let scratch = Scratch::new("wf-recover");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");

    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "config" })),
    );
    assert_eq!(st, 201);
    let rp = "/v1/orgs/acme/repos/config";

    let (st, tok1) = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({ "scopes": ["repo:write"], "repo": "config", "label": "bot-v1" })),
    );
    assert_eq!(st, 201);
    let bot1 = tok1["token"].as_str().unwrap().to_string();
    let bot1_id = tok1["id"].as_str().unwrap().to_string();

    // Good state, then a bad deploy config lands.
    let (st, c1) = commit_ops(
        &server,
        &bot1,
        rp,
        serde_json::json!({
            "message": "known-good config",
            "operations": [
                { "op": "put", "path": "deploy.yaml", "content": "replicas: 3\n" },
                { "op": "put", "path": "app.toml", "content": "debug = false\n" },
            ],
        }),
    );
    assert_eq!(st, 201, "{c1}");
    let c1_oid = c1["commit"].as_str().unwrap().to_string();
    let (st, c2) = commit_ops(
        &server,
        &bot1,
        rp,
        serde_json::json!({
            "expected_parent": c1_oid,
            "message": "scale up (fat-fingered)",
            "operations": [
                { "op": "put", "path": "deploy.yaml", "content": "replicas: 3000\n" },
            ],
        }),
    );
    assert_eq!(st, 201, "{c2}");
    let c2_oid = c2["commit"].as_str().unwrap().to_string();

    // Caught in review of the log: revert the head; the mistake stays in
    // history, the content is back to known-good.
    let (st, rev) = server.req(
        "POST",
        &format!("{rp}/revert"),
        &bot1,
        Some(serde_json::json!({ "branch": "main" })),
    );
    assert_eq!(st, 201, "{rev}");
    let (st, cfg) = server.req("GET", &format!("{rp}/files/deploy.yaml"), &admin, None);
    assert_eq!(st, 200);
    assert_eq!(cfg.as_str().unwrap(), "replicas: 3\n");
    let (_, log) = server.req("GET", &format!("{rp}/log?limit=10"), &admin, None);
    assert_eq!(
        log["entries"].as_array().unwrap().len(),
        3,
        "revert preserves history: {log}"
    );

    // Credential rotation mid-stream: mint v2, revoke v1. The old token
    // dies on its very next use; work continues on the new one.
    let (st, tok2) = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({ "scopes": ["repo:write"], "repo": "config", "label": "bot-v2" })),
    );
    assert_eq!(st, 201);
    let bot2 = tok2["token"].as_str().unwrap().to_string();
    let (st, _) = server.req(
        "DELETE",
        &format!("/v1/orgs/acme/tokens/{bot1_id}"),
        &admin,
        None,
    );
    assert_eq!(st, 204);
    let (st, _) = commit_ops(
        &server,
        &bot1,
        rp,
        serde_json::json!({
            "message": "zombie token",
            "operations": [{ "op": "put", "path": "x", "content": "x" }],
        }),
    );
    assert_eq!(st, 401, "revoked token must die instantly");

    // Deeper mistake discovered: the whole day was wrong. Reset to the
    // known-good commit under CAS; the abandoned tip stays SHA-reachable
    // for forensics until GC.
    let head_now = head_of(&server, &admin, rp, "main");
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/reset"),
        &bot2,
        Some(serde_json::json!({ "branch": "main", "to": c1_oid, "expected_head": head_now })),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(head_of(&server, &admin, rp, "main"), c1_oid);

    let clone = scratch.path().join("audit-clone");
    assert_eq!(
        gitcli::clone_and_fsck(&server.authed_url(&bot2, "acme", "config"), &clone),
        c1_oid
    );
    gitcli::git(&clone, &["fetch", "-q", "origin", &c2_oid]);
    assert_eq!(
        gitcli::git(&clone, &["rev-parse", "FETCH_HEAD"]).trim(),
        c2_oid,
        "orphaned mistake reachable by SHA for forensics"
    );

    // Work continues on the rotated credential; the final state clones
    // clean.
    let (st, c4) = commit_ops(
        &server,
        &bot2,
        rp,
        serde_json::json!({
            "expected_parent": c1_oid,
            "message": "correct scale-up",
            "operations": [
                { "op": "put", "path": "deploy.yaml", "content": "replicas: 6\n" },
            ],
        }),
    );
    assert_eq!(st, 201, "{c4}");
    let c4_oid = c4["commit"].as_str().unwrap().to_string();
    let fin = scratch.path().join("final");
    assert_eq!(
        gitcli::clone_and_fsck(&server.authed_url(&admin, "acme", "config"), &fin),
        c4_oid
    );
    assert_eq!(
        std::fs::read_to_string(fin.join("deploy.yaml")).unwrap(),
        "replicas: 6\n"
    );
}
