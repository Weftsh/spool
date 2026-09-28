//! Surface completion end-to-end: the API paths the flow suites don't
//! reach — batch delete, name validation, keyset pagination, tag/branch
//! lifecycle and their conflict answers, org-wide bulk export, audit
//! query filters, and the operator CLI's failure modes.

use std::process::Command;
use std::time::{Duration, Instant};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::{Minio, Server};

fn spawn_server_with(store_url: &str, scratch: &Scratch, extra: &[(&str, String)]) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint("surface")
        .data_dir(scratch.path().join("data"))
        .envs(extra)
        .stop_with_term()
        .start()
}

fn spawn_server(store_url: &str, scratch: &Scratch) -> Server {
    spawn_server_with(store_url, scratch, &[])
}

fn commit(server: &Server, token: &str, rp: &str, path: &str, content: &str) -> String {
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/commits"),
        token,
        Some(serde_json::json!({
            "message": format!("add {path}"),
            "operations": [ { "op": "put", "path": path, "content": content } ],
        })),
    );
    assert_eq!(st, 201, "{out}");
    out["commit"].as_str().unwrap().to_string()
}

#[test]
fn repo_lifecycle_validation_batches_and_pagination() {
    let minio = Minio::shared();
    let bucket = minio.bucket("surface-repos");
    let scratch = Scratch::new("surface-repos");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");

    // Name validation: path metacharacters and empties are refused.
    for bad in ["", "has space", "../escape", "UPPER/case", &"x".repeat(300)] {
        let (st, out) = server.req(
            "POST",
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": bad })),
        );
        assert_eq!(st, 400, "name {bad:?} accepted: {out}");
    }
    // Duplicate names conflict rather than shadow.
    for _ in 0..2 {
        server.req(
            "POST",
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": "dup" })),
        );
    }
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "dup" })),
    );
    assert_eq!(st, 409, "{out}");

    // Keyset pagination walks the whole set exactly once.
    let names: Vec<String> = (0..7).map(|i| format!("page-{i}")).collect();
    let (st, created) = server.req(
        "POST",
        "/v1/orgs/acme/repos/batch/create",
        &admin,
        Some(serde_json::json!({ "repos": names.iter().map(|n| serde_json::json!({"name": n})).collect::<Vec<_>>() })),
    );
    assert_eq!(st, 200, "{created}");
    let mut seen = Vec::new();
    let mut after = String::new();
    loop {
        let path = if after.is_empty() {
            "/v1/orgs/acme/repos?limit=3".to_string()
        } else {
            format!("/v1/orgs/acme/repos?limit=3&after={after}")
        };
        let (st, page) = server.req("GET", &path, &admin, None);
        assert_eq!(st, 200);
        let repos = page["repos"].as_array().unwrap();
        for r in repos {
            seen.push(r["name"].as_str().unwrap().to_string());
        }
        match page["next_after"].as_str() {
            Some(n) if !repos.is_empty() => after = n.to_string(),
            _ => break,
        }
    }
    assert_eq!(seen.len(), 8, "7 batch + dup, no repeats: {seen:?}");
    let mut sorted = seen.clone();
    sorted.dedup();
    assert_eq!(sorted.len(), seen.len(), "pagination repeated a row");

    // Batch delete: mixed hits and misses, per-item results.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/batch/delete",
        &admin,
        Some(serde_json::json!({ "names": ["page-0", "page-1", "never-existed"] })),
    );
    assert_eq!(st, 200, "{out}");
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert!(results[0]["ok"].as_bool().unwrap());
    assert!(results[1]["ok"].as_bool().unwrap());
    assert!(!results[2]["ok"].as_bool().unwrap());
    assert_eq!(results[2]["error"], "not found");
    // Deleting again: already tombstoned → not found.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/batch/delete",
        &admin,
        Some(serde_json::json!({ "names": ["page-0"] })),
    );
    assert_eq!(st, 200);
    assert!(!out["results"][0]["ok"].as_bool().unwrap());

    // Batch caps hold in both directions.
    let big: Vec<String> = (0..1001).map(|i| format!("n{i}")).collect();
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/repos/batch/delete",
        &admin,
        Some(serde_json::json!({ "names": big })),
    );
    assert_eq!(st, 400);
}

#[test]
fn tags_branches_and_conflict_answers() {
    let minio = Minio::shared();
    let bucket = minio.bucket("surface-refs");
    let scratch = Scratch::new("surface-refs");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    let c1 = commit(&server, &admin, rp, "a.txt", "one\n");
    let c2 = commit(&server, &admin, rp, "b.txt", "two\n");

    // Tag lifecycle: create at a rev, visible in refs, delete, gone.
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/tags"),
        &admin,
        Some(serde_json::json!({ "name": "v1", "target": c1 })),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["oid"].as_str().unwrap(), c1);
    let (_, refs) = server.req("GET", &format!("{rp}/refs"), &admin, None);
    assert!(refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "refs/tags/v1"));
    // Re-tagging the same name conflicts (tags are create-only).
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/tags"),
        &admin,
        Some(serde_json::json!({ "name": "v1", "target": c2 })),
    );
    assert_eq!(st, 409, "{out}");
    let (st, _) = server.req("DELETE", &format!("{rp}/tags/v1"), &admin, None);
    assert_eq!(st, 204);
    let (_, refs) = server.req("GET", &format!("{rp}/refs"), &admin, None);
    assert!(!refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "refs/tags/v1"));

    // Branch lifecycle + conflicts: create, duplicate → 409 with current,
    // from an unknown rev → 404, delete → gone.
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/branches"),
        &admin,
        Some(serde_json::json!({ "name": "topic", "from": c1 })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/branches"),
        &admin,
        Some(serde_json::json!({ "name": "topic", "from": c2 })),
    );
    assert_eq!(st, 409, "{out}");
    assert!(out["current"].is_string());
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/branches"),
        &admin,
        Some(serde_json::json!({ "name": "ghost", "from": "0".repeat(40) })),
    );
    assert_eq!(st, 404, "{out}");
    let (st, _) = server.req("DELETE", &format!("{rp}/branches/topic"), &admin, None);
    assert_eq!(st, 204);
    let (_, refs) = server.req("GET", &format!("{rp}/refs"), &admin, None);
    assert!(!refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "refs/heads/topic"));

    // Reset with a wrong expected_head answers 409 + current, and the
    // branch does not move.
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/reset"),
        &admin,
        Some(serde_json::json!({ "branch": "main", "to": c1, "expected_head": c1 })),
    );
    assert_eq!(st, 409, "{out}");
    let (_, log) = server.req("GET", &format!("{rp}/log?limit=1"), &admin, None);
    assert_eq!(log["entries"][0]["commit"].as_str().unwrap(), c2);
}

#[test]
fn bulk_export_and_job_status_paths() {
    let minio = Minio::shared();
    let bucket = minio.bucket("surface-export");
    let scratch = Scratch::new("surface-export");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    for name in ["one", "two"] {
        server.req(
            "POST",
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name })),
        );
        commit(
            &server,
            &admin,
            &format!("/v1/orgs/acme/repos/{name}"),
            "f.txt",
            "data\n",
        );
    }

    // Unknown job id → 404, not a hang or a 500.
    let (st, _) = server.req(
        "GET",
        "/v1/orgs/acme/repos/one/export/no-such-job",
        &admin,
        None,
    );
    assert_eq!(st, 404);

    // Org-wide export starts one job per repo; each completes and its
    // bundle clones clean.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/export",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 202, "{out}");
    let exports = out["exports"].as_array().unwrap();
    assert_eq!(exports.len(), 2);
    for e in exports {
        let repo = e["repo"].as_str().unwrap();
        let job = e["job"].as_str().unwrap();
        let path = format!("/v1/orgs/acme/repos/{repo}/export/{job}");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let (st, s) = server.req("GET", &path, &admin, None);
            assert_eq!(st, 200, "{s}");
            match s["state"].as_str().unwrap() {
                "done" => break,
                "failed" => panic!("export failed: {s}"),
                _ => {
                    assert!(Instant::now() < deadline, "export never finished");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
        let bundle = scratch.path().join(format!("{repo}.bundle"));
        let resp = ureq::get(&format!("{}{path}/download", server.base))
            .set("Authorization", &format!("Bearer {admin}"))
            .call()
            .unwrap();
        let mut buf = Vec::new();
        use std::io::Read;
        resp.into_reader().read_to_end(&mut buf).unwrap();
        std::fs::write(&bundle, &buf).unwrap();
        let dest = scratch.path().join(format!("{repo}-from-bundle"));
        gitcli::git(
            scratch.path(),
            &[
                "clone",
                "-q",
                bundle.to_str().unwrap(),
                dest.to_str().unwrap(),
            ],
        );
        gitcli::fsck(&dest);
    }
}

#[test]
fn audit_query_filters() {
    let minio = Minio::shared();
    let bucket = minio.bucket("surface-audit");
    let scratch = Scratch::new("surface-audit");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "other" })),
    );
    commit(&server, &admin, "/v1/orgs/acme/repos/app", "f", "x\n");

    // Unfiltered: creates + commit, newest set visible.
    let (st, all) = server.req("GET", "/v1/orgs/acme/audit", &admin, None);
    assert_eq!(st, 200);
    let n_all = all["entries"].as_array().unwrap().len();
    assert!(n_all >= 3, "{all}");

    // Repo filter narrows to that repo's actions; unknown repo is empty.
    let (_, app_only) = server.req("GET", "/v1/orgs/acme/audit?repo=app", &admin, None);
    let entries = app_only["entries"].as_array().unwrap();
    assert!(!entries.is_empty() && entries.len() < n_all);
    assert!(entries.iter().any(|e| e["action"] == "repo.commit"));
    let (_, none) = server.req("GET", "/v1/orgs/acme/audit?repo=ghost", &admin, None);
    assert!(none["entries"].as_array().unwrap().is_empty());

    // limit + after paginate; since=far-future is empty.
    let (_, page1) = server.req("GET", "/v1/orgs/acme/audit?limit=1", &admin, None);
    assert_eq!(page1["entries"].as_array().unwrap().len(), 1);
    let after = page1["next_after"].as_u64().unwrap();
    let (_, page2) = server.req(
        "GET",
        &format!("/v1/orgs/acme/audit?limit=1&after={after}"),
        &admin,
        None,
    );
    assert_eq!(page2["entries"].as_array().unwrap().len(), 1);
    assert_ne!(page1["entries"][0]["seq"], page2["entries"][0]["seq"]);
    let (_, future) = server.req(
        "GET",
        "/v1/orgs/acme/audit?since=99999999999999",
        &admin,
        None,
    );
    assert!(future["entries"].as_array().unwrap().is_empty());

    // Principal filter matches the acting token's audit id.
    let principal = all["entries"][0]["principal"].as_str().unwrap().to_string();
    let (_, by_p) = server.req(
        "GET",
        &format!("/v1/orgs/acme/audit?principal={principal}"),
        &admin,
        None,
    );
    assert_eq!(by_p["entries"].as_array().unwrap().len(), n_all);
}

#[test]
fn admin_cli_failure_modes() {
    let minio = Minio::shared();
    let bucket = minio.bucket("surface-admin");
    let scratch = Scratch::new("surface-admin");
    let server = spawn_server(&bucket.base_url, &scratch);
    server.bootstrap_org("acme");

    assert!(server
        .admin_expect_err(&["admin", "bootstrap"])
        .contains("--org"));
    assert!(server
        .admin_expect_err(&["admin", "frobnicate"])
        .contains("unknown"));
    assert!(server
        .admin_expect_err(&["admin", "mint", "--org", "acme", "--scopes", "root:all"])
        .contains("unknown scope"));
    assert!(server
        .admin_expect_err(&["admin", "mint", "--org", "ghost", "--scopes", "org:read"])
        .contains("not found"));
    assert!(server
        .admin_expect_err(&[
            "admin",
            "mint",
            "--org",
            "acme",
            "--scopes",
            "repo:read",
            "--repo",
            "ghost"
        ])
        .contains("not found"));
    // Plans went with billing: the command is not quietly accepted as a
    // no-op an operator would take for a plan change that happened.
    assert!(server
        .admin_expect_err(&["admin", "set-plan", "--org", "acme", "--plan", "team"])
        .contains("unknown admin command"));

    // Forgetting the word `admin` used to fall through to *serving*: the
    // process bound a port and sat there, so an operator saw a command
    // that never returned and created no account, and a test harness saw
    // a subprocess that never exited and a suite that hung until CI's
    // timeout with nothing saying why. It has to be an error, and the
    // error has to name the fix.
    for stray in [
        vec!["user-create", "--email", "a@b.test"],
        vec!["bootstrap", "--org", "acme"],
        vec!["mint"],
        vec!["serve"],
    ] {
        let err = server.admin_expect_err(&stray);
        assert!(
            err.contains("unexpected argument") && err.contains("admin"),
            "{stray:?} answered {err:?}"
        );
    }
}

/// Export corner cases: an empty repo has nothing to bundle; a pending
/// job's download answers 409 with its state; paged repos export through
/// the page store; GC honors grace windows and skips shared prefixes.
#[test]
fn export_corners_and_gc_windows() {
    let minio = Minio::shared();
    let bucket = minio.bucket("surface-corners");
    let scratch = Scratch::new("surface-corners");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");

    // Empty repo export fails with the named reason.
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "empty" })),
    );
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/empty/export",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 202, "{out}");
    let job = out["job"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, s) = server.req(
            "GET",
            &format!("/v1/orgs/acme/repos/empty/export/{job}"),
            &admin,
            None,
        );
        match s["state"].as_str().unwrap() {
            "failed" => {
                assert!(
                    s["error"].as_str().unwrap().contains("nothing to bundle"),
                    "{s}"
                );
                // Download of a failed job is a 409 naming the state.
                let (st, d) = server.req(
                    "GET",
                    &format!("/v1/orgs/acme/repos/empty/export/{job}/download"),
                    &admin,
                    None,
                );
                assert_eq!(st, 409, "{d}");
                break;
            }
            "done" => panic!("empty export must fail: {s}"),
            _ => {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }

    // Paged repo (compaction under a tiny page size) exports and the
    // bundle clones with every branch present.
    let scratch2 = Scratch::new("surface-paged-export");
    let server2 = spawn_server_with(
        &bucket.base_url,
        &scratch2,
        &[("STRATUM_REF_PAGE_SIZE", "2".into())],
    );
    let admin2 = server2.bootstrap_org("acme");
    server2.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin2,
        Some(serde_json::json!({ "name": "paged" })),
    );
    let rp = "/v1/orgs/acme/repos/paged";
    let mut last = String::new();
    for i in 0..9 {
        last = commit(
            &server2,
            &admin2,
            rp,
            &format!("f{}.txt", i % 3),
            &format!("v{i}\n"),
        );
    }
    for i in 0..3 {
        server2.req(
            "POST",
            &format!("{rp}/branches"),
            &admin2,
            Some(serde_json::json!({ "name": format!("b{i}"), "from": last })),
        );
    }
    let (st, out) = server2.req("POST", &format!("{rp}/compact"), &admin2, None);
    assert_eq!(st, 200, "{out}");
    let (st, out) = server2.req(
        "POST",
        &format!("{rp}/export"),
        &admin2,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 202);
    let job = out["job"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, s) = server2.req("GET", &format!("{rp}/export/{job}"), &admin2, None);
        match s["state"].as_str().unwrap() {
            "done" => break,
            "failed" => panic!("paged export failed: {s}"),
            _ => {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    let bundle = scratch2.path().join("paged.bundle");
    let resp = ureq::get(&format!("{}{rp}/export/{job}/download", server2.base))
        .set("Authorization", &format!("Bearer {admin2}"))
        .call()
        .unwrap();
    let mut buf = Vec::new();
    use std::io::Read;
    resp.into_reader().read_to_end(&mut buf).unwrap();
    std::fs::write(&bundle, &buf).unwrap();
    let dest = scratch2.path().join("from-paged-bundle");
    gitcli::git(
        scratch2.path(),
        &[
            "clone",
            "-q",
            bundle.to_str().unwrap(),
            dest.to_str().unwrap(),
        ],
    );
    gitcli::fsck(&dest);
    let branches = gitcli::git(&dest, &["branch", "-r"]);
    for i in 0..3 {
        assert!(branches.contains(&format!("b{i}")), "{branches}");
    }

    // GC with a huge grace window sweeps nothing; with exports and audit
    // batches present under the org prefix, epoch GC skips them.
    let (st, out) = server2.req(
        "POST",
        &format!("{rp}/gc"),
        &admin2,
        Some(serde_json::json!({ "grace_secs": 9999999 })),
    );
    assert_eq!(st, 200, "{out}");
    let (st, out) = server2.req(
        "POST",
        &format!("{rp}/gc"),
        &admin2,
        Some(serde_json::json!({ "grace_secs": 0 })),
    );
    assert_eq!(st, 200, "{out}");
    // Exported bundle still downloads after GC (exports/ was skipped).
    let (st, _) = server2.req("GET", &format!("{rp}/export/{job}"), &admin2, None);
    assert_eq!(st, 200);
    let resp = ureq::get(&format!("{}{rp}/export/{job}/download", server2.base))
        .set("Authorization", &format!("Bearer {admin2}"))
        .call()
        .unwrap();
    assert_eq!(resp.status(), 200, "export survived epoch GC");
}

/// Operator CLI against a broken environment: no store URL exits 2 with
/// guidance; a locked control DB fails bootstrap and mint loudly.
#[test]
fn admin_cli_environment_failures() {
    let minio = Minio::shared();
    let bucket = minio.bucket("surface-admincli");
    let scratch = Scratch::new("surface-admincli");
    let server = spawn_server(&bucket.base_url, &scratch);
    server.bootstrap_org("acme");

    // Server binary with no STRATUM_STORE_URL: exit 2, error on stderr.
    let out = Command::new(env!("CARGO_BIN_EXE_stratum-server"))
        .env_clear()
        .envs(std::env::var("LLVM_PROFILE_FILE").map(|p| ("LLVM_PROFILE_FILE", p)))
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("run without env");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("STRATUM_STORE_URL"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Invalid org names are refused at bootstrap.
    assert!(server
        .admin_expect_err(&["admin", "bootstrap", "--org", "Bad Name!"])
        .contains("invalid org name"));

    // With the tables write-locked, bootstrap and mint fail without
    // hanging (the admin CLI session's lock_timeout expires).
    let mut lock = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    let mut tx = lock.transaction().unwrap();
    tx.batch_execute("LOCK TABLE orgs, tokens IN EXCLUSIVE MODE")
        .unwrap();
    let err = server.admin_expect_err(&["admin", "bootstrap", "--org", "late"]);
    assert!(err.contains("timeout") || !err.is_empty());
    let err = server.admin_expect_err(&["admin", "mint", "--org", "acme", "--scopes", "org:read"]);
    assert!(err.contains("timeout") || !err.is_empty());
    drop(tx);
}

/// Requests against an org that does not exist: every surface answers
/// 404 through the same resolution arm, with valid credentials.
#[test]
fn unknown_org_answers_404_everywhere() {
    let minio = Minio::shared();
    let bucket = minio.bucket("surface-ghostorg");
    let scratch = Scratch::new("surface-ghostorg");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");

    let gets = [
        "/v1/orgs/ghost/repos",
        "/v1/orgs/ghost/usage",
        "/v1/orgs/ghost/audit",
    ];
    for path in gets {
        let (st, _) = server.req("GET", path, &admin, None);
        assert_eq!(st, 404, "GET {path}");
    }
    let posts = [
        ("/v1/orgs/ghost/repos", serde_json::json!({ "name": "x" })),
        (
            "/v1/orgs/ghost/repos/batch/create",
            serde_json::json!({ "repos": [] }),
        ),
        (
            "/v1/orgs/ghost/repos/batch/delete",
            serde_json::json!({ "names": [] }),
        ),
        (
            "/v1/orgs/ghost/tokens",
            serde_json::json!({ "scopes": ["org:read"] }),
        ),
        ("/v1/orgs/ghost/export", serde_json::json!({})),
        (
            "/v1/orgs/ghost/mirrors",
            serde_json::json!({ "name": "m", "provider": "generic", "origin": "file:///x" }),
        ),
    ];
    for (path, body) in posts {
        let (st, _) = server.req("POST", path, &admin, Some(body));
        assert_eq!(st, 404, "POST {path}");
    }
    let (st, _) = server.req("DELETE", "/v1/orgs/ghost/tokens/tid", &admin, None);
    assert_eq!(st, 404);
}

/// Batch-create validation: per-item name errors, the 1000 cap, and
/// repeated single deletes.
#[test]
fn batch_create_errors_and_repeat_delete() {
    let minio = Minio::shared();
    let bucket = minio.bucket("surface-batcherr");
    let scratch = Scratch::new("surface-batcherr");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");

    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/batch/create",
        &admin,
        Some(serde_json::json!({ "repos": [
            { "name": "ok-one" },
            { "name": "BAD NAME" },
            { "name": "ok-two" },
        ]})),
    );
    assert_eq!(st, 200, "{out}");
    let results = out["results"].as_array().unwrap();
    assert!(results[0]["ok"].as_bool().unwrap());
    assert!(!results[1]["ok"].as_bool().unwrap());
    assert!(results[1]["error"].is_string());
    assert!(results[2]["ok"].as_bool().unwrap());

    let big: Vec<serde_json::Value> = (0..1001)
        .map(|i| serde_json::json!({"name": format!("n{i}")}))
        .collect();
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/repos/batch/create",
        &admin,
        Some(serde_json::json!({ "repos": big })),
    );
    assert_eq!(st, 400);

    // Deleting the same repo twice: 204 then not-found.
    let (st, _) = server.req("DELETE", "/v1/orgs/acme/repos/ok-one", &admin, None);
    assert_eq!(st, 204);
    let (st, _) = server.req("DELETE", "/v1/orgs/acme/repos/ok-one", &admin, None);
    assert_eq!(st, 404);
}
