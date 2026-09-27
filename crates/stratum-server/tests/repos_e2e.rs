//! Repos SKU end-to-end (R1–R7): REST commits interoperating with stock
//! git over the wire, reads with exact ETags, undo primitives, export
//! bundles, and the audit trail.

use std::time::{Duration, Instant};
use stratum_testkit::closure::assert_closed;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::{Minio, Server};

fn spawn_server_with(store_url: &str, scratch: &Scratch, extra: &[(&str, String)]) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint("repos")
        .data_dir(scratch.path().join("data"))
        .envs(extra)
        .start()
}

fn spawn_server(store_url: &str, scratch: &Scratch) -> Server {
    spawn_server_with(store_url, scratch, &[])
}

#[test]
fn rest_commits_reads_undo_and_wire_interop() {
    let minio = Minio::shared();
    let bucket = minio.bucket("repos-e2e");
    let scratch = Scratch::new("repos");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("agents");
    let base = "/v1/orgs/agents/repos";

    let (st, _, _) = server.req_full(
        "POST",
        base,
        &admin,
        Some(serde_json::json!({ "name": "session-1" })),
    );
    assert_eq!(st, 201);
    let rp = format!("{base}/session-1");

    // R2: first commit — tree built server-side, no git subprocess.
    let (st, c1, _) = server.req_full(
        "POST",
        &format!("{rp}/commits"),
        &admin,
        Some(serde_json::json!({
            "branch": "main",
            "message": "initial state",
            "context": { "agent_run": "run-42" },
            "operations": [
                { "op": "put", "path": "README.md", "content": "# session\n" },
                { "op": "put", "path": "src/app.js", "content": "console.log(1)\n" },
                { "op": "put", "path": "docs/guide.md", "content": "guide v1\n" },
            ],
        })),
    );
    assert_eq!(st, 201, "{c1}");
    let c1_oid = c1["commit"].as_str().unwrap().to_string();

    // R5 interop: stock git clones what REST committed — fsck-clean.
    let url = server.authed_url(&admin, "agents", "session-1");
    let clone = scratch.path().join("clone");
    let head = gitcli::clone_and_fsck(&url, &clone);
    assert_eq!(head, c1_oid);
    assert_eq!(
        std::fs::read_to_string(clone.join("src/app.js")).unwrap(),
        "console.log(1)\n"
    );

    // Second commit: modify + delete, CAS on the parent.
    let (st, c2, _) = server.req_full(
        "POST",
        &format!("{rp}/commits"),
        &admin,
        Some(serde_json::json!({
            "branch": "main",
            "expected_parent": c1_oid,
            "message": "iterate",
            "operations": [
                { "op": "put", "path": "src/app.js", "content": "console.log(2)\n" },
                { "op": "delete", "path": "docs/guide.md" },
                { "op": "put", "path": "src/lib/util.js", "content": "export {}\n" },
            ],
        })),
    );
    assert_eq!(st, 201, "{c2}");
    let c2_oid = c2["commit"].as_str().unwrap().to_string();

    // Stale parent → 409 with the current tip (R2 optimistic concurrency).
    let (st, conflict, _) = server.req_full(
        "POST",
        &format!("{rp}/commits"),
        &admin,
        Some(serde_json::json!({
            "branch": "main",
            "expected_parent": c1_oid,
            "message": "stale",
            "operations": [{ "op": "put", "path": "x", "content": "x" }],
        })),
    );
    assert_eq!(st, 409, "{conflict}");
    assert_eq!(conflict["current_tip"].as_str().unwrap(), c2_oid);

    // R3 reads: file at HEAD and at an old rev; exact ETag → 304.
    let (st, body, hdrs) = server.req_full("GET", &format!("{rp}/files/src/app.js"), &admin, None);
    assert_eq!(st, 200);
    assert_eq!(body.as_str().unwrap(), "console.log(2)\n");
    let etag = hdrs.get("etag").cloned().expect("content-addressed ETag");
    let resp = ureq::get(&format!("{}{rp}/files/src/app.js", server.base))
        .set("Authorization", &format!("Bearer {admin}"))
        .set("If-None-Match", &etag)
        .call();
    match resp {
        Err(ureq::Error::Status(304, _)) => {}
        Ok(r) => assert_eq!(r.status(), 304),
        Err(e) => panic!("{e}"),
    }
    let (st, old, _) = server.req_full(
        "GET",
        &format!("{rp}/files/src/app.js?at={c1_oid}"),
        &admin,
        None,
    );
    assert_eq!(st, 200);
    assert_eq!(old.as_str().unwrap(), "console.log(1)\n");
    let (st, _, _) = server.req_full("GET", &format!("{rp}/files/docs/guide.md"), &admin, None);
    assert_eq!(st, 404, "deleted file must 404 at HEAD");

    // Tree, log (with pagination), diff.
    let (st, tree, _) = server.req_full("GET", &format!("{rp}/tree/src"), &admin, None);
    assert_eq!(st, 200);
    let names: Vec<&str> = tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"app.js") && names.contains(&"lib"));

    let (st, log1, _) = server.req_full("GET", &format!("{rp}/log?limit=1"), &admin, None);
    assert_eq!(st, 200);
    assert_eq!(log1["entries"].as_array().unwrap().len(), 1);
    assert_eq!(log1["entries"][0]["commit"].as_str().unwrap(), c2_oid);
    let next = log1["next_after"].as_str().unwrap();
    let (_, log2, _) = server.req_full(
        "GET",
        &format!("{rp}/log?limit=10&after={next}"),
        &admin,
        None,
    );
    assert_eq!(log2["entries"][0]["commit"].as_str().unwrap(), c1_oid);

    let (st, diff, _) = server.req_full(
        "GET",
        &format!("{rp}/diff?from={c1_oid}&to={c2_oid}"),
        &admin,
        None,
    );
    assert_eq!(st, 200);
    let by_path: std::collections::HashMap<&str, &str> = diff["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["path"].as_str().unwrap(), c["status"].as_str().unwrap()))
        .collect();
    assert_eq!(by_path["src/app.js"], "modified");
    assert_eq!(by_path["docs/guide.md"], "deleted");
    assert_eq!(by_path["src/lib/util.js"], "added");

    // R4 undo: branch at c2, reset main to c1, orphan stays SHA-reachable.
    let (st, _, _) = server.req_full(
        "POST",
        &format!("{rp}/branches"),
        &admin,
        Some(serde_json::json!({ "name": "checkpoint", "from": c2_oid })),
    );
    assert_eq!(st, 201);
    let (st, _, _) = server.req_full(
        "POST",
        &format!("{rp}/reset"),
        &admin,
        Some(serde_json::json!({ "branch": "main", "to": c1_oid, "expected_head": c2_oid })),
    );
    assert_eq!(st, 200);
    let (_, refs, _) = server.req_full("GET", &format!("{rp}/refs"), &admin, None);
    let main_oid = refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "refs/heads/main")
        .unwrap()["oid"]
        .as_str()
        .unwrap();
    assert_eq!(main_oid, c1_oid);
    // The orphaned tip is still fetchable by SHA over the wire.
    gitcli::git(&clone, &["fetch", "-q", "origin", &c2_oid]);
    assert_eq!(
        gitcli::git(&clone, &["rev-parse", "FETCH_HEAD"]).trim(),
        c2_oid
    );

    // Revert: new commit undoing the head (history preserved).
    let (st, rev, _) = server.req_full(
        "POST",
        &format!("{rp}/revert"),
        &admin,
        Some(serde_json::json!({ "branch": "main" })),
    );
    assert_eq!(st, 201, "{rev}");
    let (_, log3, _) = server.req_full("GET", &format!("{rp}/log?limit=3"), &admin, None);
    let entries = log3["entries"].as_array().unwrap();
    assert!(entries[0]["message"]
        .as_str()
        .unwrap()
        .starts_with("Revert"));
    assert_eq!(entries[1]["commit"].as_str().unwrap(), c1_oid);

    // Tags + wire visibility.
    let (st, _, _) = server.req_full(
        "POST",
        &format!("{rp}/tags"),
        &admin,
        Some(serde_json::json!({ "name": "v1", "target": c1_oid })),
    );
    assert_eq!(st, 201);

    // Wire push lands and is visible to REST (WAL read path).
    gitcli::git(&clone, &["checkout", "-q", "main"]);
    gitcli::git(&clone, &["fetch", "-q", "origin"]);
    gitcli::git(&clone, &["reset", "-q", "--hard", "origin/main"]);
    std::fs::write(clone.join("pushed.txt"), "from git\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "pushed via wire"]);
    gitcli::git(&clone, &["push", "-q", "origin", "main"]);
    let (st, pushed, _) = server.req_full("GET", &format!("{rp}/files/pushed.txt"), &admin, None);
    assert_eq!(st, 200);
    assert_eq!(pushed.as_str().unwrap(), "from git\n");

    // R7: the audit trail carries the commit context blob.
    let (_, audit, _) = server.req_full("GET", "/v1/orgs/agents/audit", &admin, None);
    let has_ctx =
        audit["entries"].as_array().unwrap().iter().any(|e| {
            e["action"] == "repo.commit" && e["context"]["context"]["agent_run"] == "run-42"
        });
    assert!(has_ctx, "audit must carry the client context blob: {audit}");

    // REST commits and a wire push wrote this repo's whole layout, so
    // every pointer the manifest now carries has to resolve.
    assert_closed(&bucket.base_url);
}

#[test]
fn export_bundle_is_a_standard_clean_clone() {
    let minio = Minio::shared();
    let bucket = minio.bucket("repos-export");
    let scratch = Scratch::new("export");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("agents");
    let rp = "/v1/orgs/agents/repos/exportme";

    let (st, _, _) = server.req_full(
        "POST",
        "/v1/orgs/agents/repos",
        &admin,
        Some(serde_json::json!({ "name": "exportme" })),
    );
    assert_eq!(st, 201);
    let (st, _, _) = server.req_full(
        "POST",
        &format!("{rp}/commits"),
        &admin,
        Some(serde_json::json!({
            "message": "content",
            "operations": [{ "op": "put", "path": "data.txt", "content": "exported\n" }],
        })),
    );
    assert_eq!(st, 201);

    let (st, job, _) = server.req_full("POST", &format!("{rp}/export"), &admin, None);
    assert_eq!(st, 202, "{job}");
    let job_id = job["job"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, status, _) = server.req_full("GET", &format!("{rp}/export/{job_id}"), &admin, None);
        match status["state"].as_str().unwrap() {
            "done" => break,
            "failed" => panic!("export failed: {status}"),
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(150)),
            _ => panic!("export never finished"),
        }
    }
    // Download the bundle and clone it with nothing but stock git (R6).
    let bundle = scratch.path().join("out.bundle");
    let resp = ureq::get(&format!("{}{rp}/export/{job_id}/download", server.base))
        .set("Authorization", &format!("Bearer {admin}"))
        .call()
        .unwrap();
    let mut bytes = Vec::new();
    use std::io::Read;
    resp.into_reader().read_to_end(&mut bytes).unwrap();
    std::fs::write(&bundle, &bytes).unwrap();

    let restored = scratch.path().join("restored");
    gitcli::git(
        scratch.path(),
        &[
            "clone",
            "-q",
            bundle.to_str().unwrap(),
            restored.to_str().unwrap(),
        ],
    );
    gitcli::fsck(&restored);
    assert_eq!(
        std::fs::read_to_string(restored.join("data.txt")).unwrap(),
        "exported\n"
    );

    // Exporting reads the store, but the commit that fed it wrote one;
    // the bundle being clean says nothing about the layout it came from.
    assert_closed(&bucket.base_url);
}
