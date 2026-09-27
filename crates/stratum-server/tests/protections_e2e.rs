//! Branch protection, situationally: not "does the fence exist" (the
//! changes suite proves that) but the interleavings and authority edges
//! where a fence either holds or turns out to be decoration — races
//! with the queue, multi-ref pushes, divergent lands, repo-scoped
//! admins, repo lifecycle, and a second server sharing the database.
//! Everything is asserted the way a person would find out — by asking
//! the API or running `git` — never by reading tables.

use std::time::Duration;
use stratum_testkit::{gitcli, gitcli::Scratch, Minio, Server};

const PASSWORD: &str = "a long enough password";
const REFUSAL: &str = "branch 'main' is protected: land through review";

fn spawn_with(store_url: &str, scratch: &Scratch, db_url: Option<&str>, poll: &str) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_LAND_POLL_SECS", poll);
    b = match db_url {
        Some(u) => b.db_url(u),
        None => b.db_hint("protections-e2e"),
    };
    b.start()
}

fn commit(
    server: &Server,
    token: &str,
    repo: &str,
    branch: &str,
    message: &str,
    files: &[(&str, &str)],
) -> String {
    let ops: Vec<serde_json::Value> = files
        .iter()
        .map(|(p, c)| serde_json::json!({"op": "put", "path": p, "content": c}))
        .collect();
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/commits"),
        token,
        Some(serde_json::json!({
            "branch": branch,
            "message": message,
            "operations": ops,
        })),
    );
    assert_eq!(st, 201, "commit to {repo}/{branch}: {out}");
    out["commit"].as_str().unwrap().to_string()
}

fn branch(server: &Server, token: &str, repo: &str, name: &str, from: &str) {
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/branches"),
        token,
        Some(serde_json::json!({"name": name, "from": from})),
    );
    assert_eq!(st, 201, "branch {name} from {from}: {out}");
}

fn protect(server: &Server, token: &str, repo: &str, b: &str) {
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/protections"),
        token,
        Some(serde_json::json!({"branch": b})),
    );
    assert!(st == 201 || st == 200, "protect {b}: {st} {out}");
}

fn register(server: &Server, token: &str, repo: &str, from: &str) -> String {
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/changes"),
        token,
        Some(serde_json::json!({"from": from})),
    );
    assert_eq!(st, 201, "{out}");
    out["change"]["key"].as_str().unwrap().to_string()
}

fn sign_in(server: &Server, email: &str) -> String {
    let resp = ureq::post(&format!("{}/v1/auth/login", server.base))
        .set("Content-Type", "application/json")
        .send_string(&serde_json::json!({"email": email, "password": PASSWORD}).to_string())
        .unwrap_or_else(|e| panic!("login {email}: {e}"));
    resp.header("set-cookie")
        .and_then(|c| c.split(';').next())
        .expect("a session cookie")
        .to_string()
}

fn as_person(
    server: &Server,
    cookie: &str,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let mut r = ureq::request(method, &format!("{}{path}", server.base)).set("Cookie", cookie);
    if body.is_some() {
        r = r.set("Content-Type", "application/json");
    }
    let resp = match body {
        Some(b) => r.send_string(&b.to_string()),
        None => r.call(),
    };
    let resp = match resp {
        Ok(x) => x,
        Err(ureq::Error::Status(_, x)) => x,
        Err(e) => panic!("transport {method} {path}: {e}"),
    };
    let status = resp.status();
    let text = resp.into_string().unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

fn make_user(server: &Server, email: &str, name: &str, role: &str) {
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

fn user_id(server: &Server, admin: &str, email: &str) -> String {
    let (st, members) = server.get("/v1/orgs/acme/members", admin);
    assert_eq!(st, 200, "{members}");
    members["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["email"] == email)
        .unwrap_or_else(|| panic!("{email} is a member"))["user_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn wait_until_not(
    server: &Server,
    admin: &str,
    repo: &str,
    key: &str,
    not: &str,
) -> serde_json::Value {
    let path = format!("/v1/orgs/acme/repos/{repo}/changes/{key}");
    for _ in 0..150 {
        let (st, out) = server.get(&path, admin);
        assert_eq!(st, 200, "{out}");
        if out["change"]["state"] != serde_json::json!(not) {
            return out;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("change {key} never left {not:?}");
}

fn approve_as_owner(server: &Server, repo: &str, key: &str) {
    let cookie = sign_in(server, "own@acme.test");
    let (st, _) = as_person(
        server,
        &cookie,
        "POST",
        &format!("/v1/orgs/acme/repos/{repo}/changes/{key}/approve"),
        None,
    );
    assert_eq!(st, 204);
}

/// One org, one repo with an owner-governed tree, a lander polling
/// every second unless the test says otherwise.
fn world(server: Server) -> (Server, String) {
    let admin = server.bootstrap_org("acme");
    make_user(&server, "own@acme.test", "Owner Ola", "member");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    assert_eq!(st, 201, "{out}");
    commit(
        &server,
        &admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "own@acme.test\n"), ("README.md", "hi")],
    );
    (server, admin)
}

/// Fence goes up while the change is already queued: the lander is the
/// one writer protection exempts, so the landing completes — and the
/// ejection paths stay reachable on a fenced trunk too.
#[test]
fn protection_arriving_mid_queue_neither_blocks_nor_weakens_the_lander() {
    let minio = Minio::shared();
    let bucket = minio.bucket("prot-midqueue");
    let scratch = Scratch::new("prot-midqueue");
    let db_url = stratum_testkit::pg::test_db_url("prot-midqueue");
    // No lander on server A: enqueue → protect is an ordering, not a race.
    let (server, admin) = world(spawn_with(&bucket.base_url, &scratch, Some(&db_url), "0"));
    // The tip before any landing — the stale base for the second act.
    let (st, refs) = server.get("/v1/orgs/acme/repos/app/refs", &admin);
    assert_eq!(st, 200);
    let old_tip = refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "refs/heads/main")
        .unwrap()["oid"]
        .as_str()
        .unwrap()
        .to_string();
    branch(&server, &admin, "app", "feature", "main");
    commit(
        &server,
        &admin,
        "app",
        "feature",
        "work\n\nChange-Id: I5170a001\n",
        &[("w.rs", "v1")],
    );
    let key = register(&server, &admin, "app", "feature");
    approve_as_owner(&server, "app", &key);
    let (st, _) = server.post(
        &format!("/v1/orgs/acme/repos/app/changes/{key}/land"),
        &admin,
        None,
    );
    assert_eq!(st, 202);
    // Fence up AFTER enqueue.
    protect(&server, &admin, "app", "main");

    // Server B's lander claims and lands straight through the fence.
    let scratch_b = Scratch::new("prot-midqueue-b");
    let b = spawn_with(&bucket.base_url, &scratch_b, Some(&db_url), "1");
    let out = wait_until_not(&server, &admin, "app", &key, "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("landed"), "{out}");

    // And an ejection on the fenced trunk still reports in words: a
    // second change based on the OLD tip is not fast-forward now.
    branch(&server, &admin, "app", "stale", &old_tip);
    commit(
        &server,
        &admin,
        "app",
        "stale",
        "stale\n\nChange-Id: I5170a002\n",
        &[("s.rs", "v1")],
    );
    let key2 = register(&server, &admin, "app", "stale");
    approve_as_owner(&server, "app", &key2);
    let (st, _) = server.post(
        &format!("/v1/orgs/acme/repos/app/changes/{key2}/land"),
        &admin,
        None,
    );
    assert_eq!(st, 202);
    let out = wait_until_not(&server, &admin, "app", &key2, "landing");
    drop(b);
    assert_eq!(out["change"]["state"], serde_json::json!("open"), "{out}");
    let v = out["change"]["land_verdict"].as_str().unwrap();
    assert!(v.starts_with("ejected: not fast-forward"), "{v}");
    assert!(server.healthy());
}

/// A stack lands through a fenced trunk exactly like a single change:
/// the top's landing marks the whole stack, no member needs a push.
#[test]
fn a_stack_lands_whole_on_a_protected_trunk() {
    let minio = Minio::shared();
    let bucket = minio.bucket("prot-stack");
    let scratch = Scratch::new("prot-stack");
    let (server, admin) = world(spawn_with(&bucket.base_url, &scratch, None, "1"));
    protect(&server, &admin, "app", "main");
    branch(&server, &admin, "app", "stack", "main");
    commit(
        &server,
        &admin,
        "app",
        "stack",
        "bottom\n\nChange-Id: I57ac0001\n",
        &[("bottom.rs", "v1")],
    );
    let bottom = register(&server, &admin, "app", "stack");
    commit(
        &server,
        &admin,
        "app",
        "stack",
        "top\n\nChange-Id: I57ac0002\n",
        &[("top.rs", "v1")],
    );
    let top = register(&server, &admin, "app", "stack");
    approve_as_owner(&server, "app", &top);
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/app/changes/{top}/land"),
        &admin,
        None,
    );
    assert_eq!(st, 202, "{out}");
    let out = wait_until_not(&server, &admin, "app", &top, "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("landed"), "{out}");
    // The bottom of the stack landed by inclusion — through the fence.
    for _ in 0..50 {
        let (_, b) = server.get(&format!("/v1/orgs/acme/repos/app/changes/{bottom}"), &admin);
        if b["change"]["state"] == serde_json::json!("landed") {
            let v = b["change"]["land_verdict"].as_str().unwrap();
            assert!(v.starts_with("landed: included in "), "{v}");
            let clone = scratch.path().join("stack-clone");
            gitcli::clone_and_fsck(&server.authed_url(&admin, "acme", "app"), &clone);
            assert!(server.healthy());
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("the stack's bottom never landed by inclusion");
}

/// git pushes are atomic per report: if any updated ref is protected,
/// the whole push is refused — a mixed push must not half-land.
#[test]
fn a_mixed_multi_ref_push_is_refused_whole() {
    let minio = Minio::shared();
    let bucket = minio.bucket("prot-mixed");
    let scratch = Scratch::new("prot-mixed");
    let (server, admin) = world(spawn_with(&bucket.base_url, &scratch, None, "0"));
    protect(&server, &admin, "app", "main");

    let url = server.authed_url(&admin, "acme", "app");
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&url, &clone);
    std::fs::write(clone.join("f.txt"), "work\n").unwrap();
    gitcli::git(&clone, &["add", "."]);
    gitcli::git(&clone, &["commit", "-q", "-m", "work"]);
    // One push, two refs: protected main and a fresh side branch.
    let err = gitcli::git_expect_err(
        &clone,
        &[
            "push",
            "-q",
            "origin",
            "HEAD:refs/heads/main",
            "HEAD:refs/heads/side",
        ],
    )
    .unwrap();
    assert!(err.contains(REFUSAL), "{err}");
    // Neither ref moved: side does not exist, main is at the base.
    let (st, refs) = server.get("/v1/orgs/acme/repos/app/refs", &admin);
    assert_eq!(st, 200);
    let names: Vec<&str> = refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert!(!names.contains(&"refs/heads/side"), "{names:?}");
    // The same work pushed to the side branch alone goes through.
    gitcli::git(&clone, &["push", "-q", "origin", "HEAD:refs/heads/side"]);
    assert!(server.healthy());
}

/// Authority is repo-scoped: a viewer raised to admin on one repo runs
/// that repo's policy — and only that repo's. The org role is the
/// default answer, not the final one.
#[test]
fn a_repo_scoped_admin_controls_policy_on_that_repo_only() {
    let minio = Minio::shared();
    let bucket = minio.bucket("prot-scoped");
    let scratch = Scratch::new("prot-scoped");
    let (server, admin) = world(spawn_with(&bucket.base_url, &scratch, None, "0"));
    let (st, _) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "sibling"})),
    );
    assert_eq!(st, 201);
    commit(
        &server,
        &admin,
        "sibling",
        "main",
        "seed",
        &[("a.txt", "1")],
    );
    make_user(&server, "vic@acme.test", "Vic", "viewer");
    let vic = sign_in(&server, "vic@acme.test");

    // A viewer is masked away from policy everywhere.
    let (st, _) = as_person(
        &server,
        &vic,
        "POST",
        "/v1/orgs/acme/repos/app/protections",
        Some(serde_json::json!({"branch": "main"})),
    );
    assert_eq!(st, 404, "a viewer cannot see the gate, let alone move it");

    // Raised to admin on `app` only, the same person runs its policy…
    let vic_id = user_id(&server, &admin, "vic@acme.test");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/grants",
        &admin,
        Some(serde_json::json!({"user_id": vic_id, "role": "admin"})),
    );
    assert_eq!(st, 204, "{out}");
    let (st, out) = as_person(
        &server,
        &vic,
        "POST",
        "/v1/orgs/acme/repos/app/protections",
        Some(serde_json::json!({"branch": "main"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = as_person(
        &server,
        &vic,
        "PATCH",
        "/v1/orgs/acme/repos/app",
        Some(serde_json::json!({"default_branch": "main"})),
    );
    assert_eq!(st, 200, "{out}");

    // …and still cannot touch the sibling repo's policy: the grant is
    // the boundary, not the org.
    let (st, _) = as_person(
        &server,
        &vic,
        "POST",
        "/v1/orgs/acme/repos/sibling/protections",
        Some(serde_json::json!({"branch": "main"})),
    );
    assert_eq!(st, 404, "authority must not leak across repos");
    let (st, _) = as_person(
        &server,
        &vic,
        "DELETE",
        "/v1/orgs/acme/repos/sibling/protections/main",
        None,
    );
    assert_eq!(st, 404);

    // Their own fence they can also take down.
    let (st, _) = as_person(
        &server,
        &vic,
        "DELETE",
        "/v1/orgs/acme/repos/app/protections/main",
        None,
    );
    assert_eq!(st, 204);
    assert!(server.healthy());
}

/// Protection follows the repo, not the name: delete the repo and a
/// new repo under the same name starts unfenced, with no ghost rows.
#[test]
fn protection_dies_with_the_repo_not_the_name() {
    let minio = Minio::shared();
    let bucket = minio.bucket("prot-lifecycle");
    let scratch = Scratch::new("prot-lifecycle");
    let (server, admin) = world(spawn_with(&bucket.base_url, &scratch, None, "0"));
    protect(&server, &admin, "app", "main");
    let (st, _) = server.req("DELETE", "/v1/orgs/acme/repos/app", &admin, None);
    assert_eq!(st, 204);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    assert_eq!(st, 201);
    commit(&server, &admin, "app", "main", "fresh", &[("a.txt", "2")]);
    let (st, out) = server.get("/v1/orgs/acme/repos/app/protections", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["protections"].as_array().unwrap().len(), 0, "{out}");
    // And the unfenced trunk takes commits directly again.
    commit(&server, &admin, "app", "main", "direct", &[("b.txt", "1")]);
    assert!(server.healthy());
}

/// Moving the default branch does not move the fence: each is its own
/// policy, and tags never answer to branch protection at all.
#[test]
fn the_fence_tracks_branches_not_defaults_and_ignores_tags() {
    let minio = Minio::shared();
    let bucket = minio.bucket("prot-default");
    let scratch = Scratch::new("prot-default");
    let (server, admin) = world(spawn_with(&bucket.base_url, &scratch, None, "1"));
    branch(&server, &admin, "app", "trunk", "main");
    protect(&server, &admin, "app", "main");
    let (st, out) = server.req(
        "PATCH",
        "/v1/orgs/acme/repos/app",
        &admin,
        Some(serde_json::json!({"default_branch": "trunk"})),
    );
    assert_eq!(st, 200, "{out}");

    // main stays fenced; trunk — the new default — is open.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        &admin,
        Some(serde_json::json!({
            "branch": "main", "message": "sneak",
            "operations": [{"op": "put", "path": "x.txt", "content": "x"}],
        })),
    );
    assert_eq!(st, 403, "{out}");
    assert_eq!(out["error"], serde_json::json!(REFUSAL));
    commit(
        &server,
        &admin,
        "app",
        "trunk",
        "open trunk",
        &[("t.txt", "1")],
    );

    // A change with no explicit target aims at the new default and
    // lands there while main stays fenced.
    branch(&server, &admin, "app", "feature", "trunk");
    commit(
        &server,
        &admin,
        "app",
        "feature",
        "work\n\nChange-Id: Idef50001\n",
        &[("w.rs", "v1")],
    );
    let key = register(&server, &admin, "app", "feature");
    let (st, out) = server.get(&format!("/v1/orgs/acme/repos/app/changes/{key}"), &admin);
    assert_eq!(st, 200);
    assert_eq!(
        out["change"]["target_branch"],
        serde_json::json!("trunk"),
        "{out}"
    );

    // Tags are not branches: both fenced and unfenced repos tag freely.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/tags",
        &admin,
        Some(serde_json::json!({"name": "v1.0", "target": "main"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, _) = server.req("DELETE", "/v1/orgs/acme/repos/app/tags/v1.0", &admin, None);
    assert_eq!(st, 204);
    assert!(server.healthy());
}

/// The fence is in the database, not a process: a second server over
/// the same control plane refuses the same push in the same words.
#[test]
fn protection_holds_across_servers_sharing_the_control_plane() {
    let minio = Minio::shared();
    let bucket = minio.bucket("prot-fleet");
    let scratch = Scratch::new("prot-fleet");
    let db_url = stratum_testkit::pg::test_db_url("prot-fleet");
    let (server_a, admin) = world(spawn_with(&bucket.base_url, &scratch, Some(&db_url), "0"));
    protect(&server_a, &admin, "app", "main");

    let scratch_b = Scratch::new("prot-fleet-b");
    let server_b = spawn_with(&bucket.base_url, &scratch_b, Some(&db_url), "0");
    let url = server_b.authed_url(&admin, "acme", "app");
    let clone = scratch.path().join("clone-b");
    gitcli::clone_and_fsck(&url, &clone);
    std::fs::write(clone.join("f.txt"), "x\n").unwrap();
    gitcli::git(&clone, &["add", "."]);
    gitcli::git(&clone, &["commit", "-q", "-m", "direct"]);
    let err = gitcli::git_expect_err(&clone, &["push", "-q", "origin", "main"]).unwrap();
    assert!(err.contains(REFUSAL), "{err}");
    // Unprotect through server A; server B honors it at once.
    let (st, _) = server_a.req(
        "DELETE",
        "/v1/orgs/acme/repos/app/protections/main",
        &admin,
        None,
    );
    assert_eq!(st, 204);
    gitcli::git(&clone, &["push", "-q", "origin", "main"]);
    assert!(server_a.healthy() && server_b.healthy());
}

// ---------------------------------------------------------------------
// Required checks, through the API a person actually drives.

/// Percent-encode a check name for the DELETE query string. Names are
/// prose — `Build and test (ubuntu-latest)` — so this has to be real
/// encoding and not a `format!`, or the test would only ever exercise
/// the names that happen to survive a raw URL.
fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn require(
    server: &Server,
    token: &str,
    repo: &str,
    b: &str,
    name: &str,
) -> (u16, serde_json::Value) {
    server.post(
        &format!("/v1/orgs/acme/repos/{repo}/required-checks/{b}"),
        token,
        Some(serde_json::json!({"name": name})),
    )
}

fn required(server: &Server, token: &str, repo: &str, b: &str) -> Vec<String> {
    let (st, out) = server.get(
        &format!("/v1/orgs/acme/repos/{repo}/required-checks/{b}"),
        token,
    );
    assert_eq!(st, 200, "list required on {b}: {out}");
    assert_eq!(out["branch"], serde_json::json!(b), "{out}");
    out["required_checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect()
}

fn unrequire(server: &Server, token: &str, repo: &str, b: &str, name: &str) -> u16 {
    server
        .delete(
            &format!(
                "/v1/orgs/acme/repos/{repo}/required-checks/{b}?name={}",
                enc(name)
            ),
            token,
        )
        .0
}

/// Requiring, listing and removing a check the way an admin would, on
/// a plain branch and on one whose name contains a slash — the case the
/// route's trailing wildcard exists for, and the one a second answer to
/// "where does the check name go" would have broken.
///
/// Also pins the two shape decisions: requiring twice acks rather than
/// erroring or duplicating, and a requirement on an unprotected branch
/// is refused rather than silently protecting the branch.
#[test]
fn required_checks_round_trip_through_the_api_on_slashed_branches_too() {
    let minio = Minio::shared();
    let bucket = minio.bucket("prot-req-rt");
    let scratch = Scratch::new("prot-req-rt");
    let (server, admin) = world(spawn_with(&bucket.base_url, &scratch, None, "0"));
    branch(&server, &admin, "app", "release/2.0", "main");
    protect(&server, &admin, "app", "main");
    protect(&server, &admin, "app", "release/2.0");

    assert!(required(&server, &admin, "app", "main").is_empty());

    let (st, out) = require(&server, &admin, "app", "main", "ci/tests");
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["required"], serde_json::json!(true), "{out}");
    // Requiring the same check again acks. A settings screen that
    // retries must not see an error, and must not create a second row.
    let (st, out) = require(&server, &admin, "app", "main", "ci/tests");
    assert_eq!(st, 200, "{out}");
    // A mirrored Actions workflow name: prose, with spaces and
    // parentheses, which the check *intake*'s name rule would refuse.
    let (st, out) = require(
        &server,
        &admin,
        "app",
        "main",
        "Build and test (ubuntu-latest)",
    );
    assert_eq!(st, 201, "{out}");
    // The slashed branch is a separate gate, addressed through the
    // wildcard rather than through a second route shape.
    let (st, out) = require(&server, &admin, "app", "release/2.0", "ci/tests");
    assert_eq!(st, 201, "{out}");

    assert_eq!(
        required(&server, &admin, "app", "main"),
        vec![
            "Build and test (ubuntu-latest)".to_string(),
            "ci/tests".to_string()
        ],
    );
    assert_eq!(
        required(&server, &admin, "app", "release/2.0"),
        vec!["ci/tests".to_string()],
    );

    // Requiring on a branch nobody has fenced is refused in words: the
    // land gate only has force where the queue is the only writer, so
    // the row would read as safety while being none.
    let (st, out) = require(&server, &admin, "app", "feature-x", "ci/tests");
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("is not protected"),
        "{out}"
    );
    // …and it did not protect the branch behind the admin's back.
    let (st, out) = server.get("/v1/orgs/acme/repos/app/protections", &admin);
    assert_eq!(st, 200, "{out}");
    let fenced: Vec<&str> = out["protections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["branch"].as_str().unwrap())
        .collect();
    assert_eq!(fenced, vec!["main", "release/2.0"], "{out}");

    // Removal takes exactly the one name, on exactly the one branch.
    assert_eq!(unrequire(&server, &admin, "app", "main", "ci/tests"), 204);
    assert_eq!(
        unrequire(&server, &admin, "app", "main", "ci/tests"),
        404,
        "removing what is not required is a 404, not a silent 204"
    );
    assert_eq!(
        required(&server, &admin, "app", "main"),
        vec!["Build and test (ubuntu-latest)".to_string()],
    );
    assert_eq!(
        required(&server, &admin, "app", "release/2.0"),
        vec!["ci/tests".to_string()],
        "removing a name on one branch removed it on another",
    );
    assert_eq!(
        unrequire(
            &server,
            &admin,
            "app",
            "main",
            "Build and test (ubuntu-latest)"
        ),
        204,
        "a prose name must survive the round trip through the query string",
    );

    // A requirement outlives the fence coming down, and is still
    // removable — otherwise a stale row would be unremovable and would
    // come back into force the moment the branch was re-protected.
    let (st, out) = require(&server, &admin, "app", "release/2.0", "ci/lint");
    assert_eq!(st, 201, "{out}");
    let (st, _) = server.req(
        "DELETE",
        "/v1/orgs/acme/repos/app/protections/release/2.0",
        &admin,
        None,
    );
    assert_eq!(st, 204);
    assert_eq!(
        required(&server, &admin, "app", "release/2.0"),
        vec!["ci/lint".to_string(), "ci/tests".to_string()],
    );
    assert_eq!(
        unrequire(&server, &admin, "app", "release/2.0", "ci/lint"),
        204
    );
    assert!(server.healthy());
}

/// A name a person reads has to be one line and one name, and the door
/// is where that is enforced: a newline in a required name makes one
/// row print as two in a settings list and in a land-gate reason.
#[test]
fn hostile_required_check_names_are_refused_at_the_rest_door() {
    let minio = Minio::shared();
    let bucket = minio.bucket("prot-req-shape");
    let scratch = Scratch::new("prot-req-shape");
    let (server, admin) = world(spawn_with(&bucket.base_url, &scratch, None, "0"));
    protect(&server, &admin, "app", "main");

    let long = "n".repeat(101);
    for bad in [
        "",
        "ci\nfake: passing",
        "ci\ttests",
        " ci",
        "ci ",
        "ci\u{7f}",
        long.as_str(),
    ] {
        let (st, out) = require(&server, &admin, "app", "main", bad);
        assert_eq!(st, 400, "{bad:?}: {out}");
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("invalid check name"),
            "{bad:?}: {out}"
        );
    }
    // A branch shape git could never name is refused as a branch, in
    // the branch's own words, before the check name is considered.
    let (st, out) = require(&server, &admin, "app", "a..b", "ci/tests");
    assert_eq!(st, 400, "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("invalid branch"),
        "{out}"
    );
    assert!(required(&server, &admin, "app", "main").is_empty());
    assert!(server.healthy());
}

/// Requiring a check is the same class of authority move as protecting
/// the branch, so it takes the same gate — and a refusal must not tell
/// a stranger the repository is there.
#[test]
fn required_checks_are_admin_only_and_the_refusal_does_not_admit_the_repo() {
    let minio = Minio::shared();
    let bucket = minio.bucket("prot-req-authz");
    let scratch = Scratch::new("prot-req-authz");
    let (server, admin) = world(spawn_with(&bucket.base_url, &scratch, None, "0"));
    protect(&server, &admin, "app", "main");
    let (st, out) = require(&server, &admin, "app", "main", "ci/tests");
    assert_eq!(st, 201, "{out}");

    make_user(&server, "vic@acme.test", "Vic", "viewer");
    let vic = sign_in(&server, "vic@acme.test");

    // A viewer on a private repo is masked from the repo entirely, so
    // the write and the read both answer 404…
    let (st, denied) = as_person(
        &server,
        &vic,
        "POST",
        "/v1/orgs/acme/repos/app/required-checks/main",
        Some(serde_json::json!({"name": "ci/mine"})),
    );
    assert_eq!(st, 404, "{denied}");
    let (st, _) = as_person(
        &server,
        &vic,
        "DELETE",
        "/v1/orgs/acme/repos/app/required-checks/main?name=ci%2Ftests",
        None,
    );
    assert_eq!(st, 404);

    // …and identically to a repository that is not there at all, which
    // is the whole point: the answer carries no evidence either way.
    let (st, absent) = as_person(
        &server,
        &vic,
        "POST",
        "/v1/orgs/acme/repos/no-such-repo/required-checks/main",
        Some(serde_json::json!({"name": "ci/mine"})),
    );
    assert_eq!(st, 404);
    assert_eq!(denied, absent, "the refusal distinguishes the two repos");

    // The requirement is untouched by any of that.
    assert_eq!(
        required(&server, &admin, "app", "main"),
        vec!["ci/tests".to_string()]
    );
    assert!(server.healthy());
}

/// Requirements follow the repo, not its name: delete the repo and a
/// new one under the same name starts with no gate at all. A ghost row
/// here would be a check nobody configured silently blocking landings.
#[test]
fn required_checks_die_with_the_repo_not_the_name() {
    let minio = Minio::shared();
    let bucket = minio.bucket("prot-req-life");
    let scratch = Scratch::new("prot-req-life");
    let (server, admin) = world(spawn_with(&bucket.base_url, &scratch, None, "0"));
    protect(&server, &admin, "app", "main");
    let (st, out) = require(&server, &admin, "app", "main", "ci/tests");
    assert_eq!(st, 201, "{out}");

    let (st, _) = server.req("DELETE", "/v1/orgs/acme/repos/app", &admin, None);
    assert_eq!(st, 204);
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    assert_eq!(st, 201, "{out}");
    commit(&server, &admin, "app", "main", "fresh", &[("a.txt", "2")]);
    assert!(required(&server, &admin, "app", "main").is_empty());
    assert!(server.healthy());
}

/// The land button says which required checks it is waiting on, and
/// refuses in words the ones that have already said no.
///
/// The old precheck asked only "is any check failing right now", and
/// answered every other situation with a bare 202 carrying nothing but
/// a job id. So a change waiting on CI and a change cleared to land
/// were the same answer to the author, and a required check reported
/// in any non-`failing` state was invisible to the button entirely.
///
/// Three situations, three answers, all of them named:
///
/// - a required check that has not reported, or is still running →
///   **202** with `waiting_on`. The queue exists precisely so that
///   "press Land and walk away" works; refusing here would make the
///   ordinary flow — push, press Land, CI has not started yet — an
///   immediate ejection and a second press. The lander's wait budget,
///   not the button, is what catches a name that will never report.
/// - a required check that is failing → **409**, naming it and why.
/// - everything green → **202** with an empty `waiting_on`, so a merge
///   box can tell "landing now" from "landing when CI finishes".
///
/// Each case gets its own change: the first accepted land moves that
/// change to `landing`, and a second press is a different refusal.
#[test]
fn a_land_request_names_the_required_checks_it_waits_on_or_refuses_for() {
    let minio = Minio::shared();
    let bucket = minio.bucket("prot-req-land");
    let scratch = Scratch::new("prot-req-land");
    // No lander: every verdict below is the *precheck's*, not a race
    // with a background claim.
    let (server, admin) = world(spawn_with(&bucket.base_url, &scratch, None, "0"));
    protect(&server, &admin, "app", "main");
    let (st, out) = require(&server, &admin, "app", "main", "ci/tests");
    assert_eq!(st, 201, "{out}");

    // A change ready for review but with nothing built yet.
    let ready = |n: u32| -> (String, String) {
        let br = format!("feature{n}");
        branch(&server, &admin, "app", &br, "main");
        commit(
            &server,
            &admin,
            "app",
            &br,
            &format!("work\n\nChange-Id: I5e9a000{n}\n"),
            &[("w.rs", &format!("v{n}"))],
        );
        let key = register(&server, &admin, "app", &br);
        approve_as_owner(&server, "app", &key);
        (
            format!("/v1/orgs/acme/repos/app/changes/{key}/land"),
            format!("/v1/orgs/acme/repos/app/changes/{key}/checks"),
        )
    };

    // Nothing has reported: accepted, and the answer says what it is
    // waiting for. The old precheck said 202 and nothing else, which is
    // indistinguishable from "landing right now".
    let (land, _) = ready(1);
    let (st, out) = server.post(&land, &admin, None);
    assert_eq!(
        st, 202,
        "a change waiting on CI belongs in the queue: {out}"
    );
    assert_eq!(out["gate"], serde_json::json!("waiting"), "{out}");
    assert_eq!(
        out["waiting_on"],
        serde_json::json!(["ci/tests"]),
        "the 202 must name the check it is waiting on: {out}"
    );

    // Reported and still running: the same situation to an author, and
    // the same answer — it arrives by a different path inside the gate.
    let (land, checks) = ready(2);
    let (st, out) = server.post(
        &checks,
        &admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "pending"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(&land, &admin, None);
    assert_eq!(st, 202, "{out}");
    assert_eq!(out["gate"], serde_json::json!("waiting"), "{out}");
    assert_eq!(out["waiting_on"], serde_json::json!(["ci/tests"]), "{out}");

    // Reported failing: refused at the button, naming the check and
    // what it did, rather than accepted and ejected seconds later.
    let (land, checks) = ready(3);
    let (st, out) = server.post(
        &checks,
        &admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "failing"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(&land, &admin, None);
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["gate"], serde_json::json!("blocked"), "{out}");
    assert!(
        out["reason"]
            .as_str()
            .unwrap()
            .contains("required check 'ci/tests' is failing"),
        "the refusal must name the check: {out}"
    );
    // And it is still the author's change to fix, not the queue's.
    let (st, out) = server.get(land.trim_end_matches("/land"), &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["change"]["state"], serde_json::json!("open"), "{out}");

    // Green: accepted with nothing outstanding, which is what lets a
    // merge box say "landing now" rather than "landing eventually".
    let (st, out) = server.post(
        &checks,
        &admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "passing"})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, out) = server.post(&land, &admin, None);
    assert_eq!(st, 202, "{out}");
    assert_eq!(out["gate"], serde_json::json!("ready"), "{out}");
    assert_eq!(out["waiting_on"], serde_json::json!([]), "{out}");
    assert!(server.healthy());
}

/// Reading the requirement list: masked the same way as writing it, and
/// refusing a branch name it will not carry.
///
/// The write side of `required-checks` has a negative suite; the read
/// side had none, so neither the masking arm nor the shape check on the
/// branch had ever run. Both matter for the same reason the writes do:
/// a read that answers differently for "not yours" and "not there" is an
/// existence oracle for private repositories, and a branch name carrying
/// git's own metacharacters is refused at the door here rather than
/// reaching a query that has to be careful.
#[test]
fn reading_required_checks_is_masked_and_refuses_a_branch_it_will_not_carry() {
    let minio = Minio::shared();
    let bucket = minio.bucket("protections-readgate");
    let scratch = Scratch::new("protections-readgate");
    let server = spawn_with(&bucket.base_url, &scratch, None, "0");
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        // Private, because masking is the claim: on a public repo a
        // viewer may legitimately read this and there is nothing to
        // mask.
        Some(serde_json::json!({"name": "app", "public": false})),
    );
    assert_eq!(st, 201, "{out}");

    // A branch name git itself would refuse. Checked before the branch
    // reaches a query, and the refusal names it rather than answering an
    // empty list — an empty list means "nothing required here", which is
    // a different and much more dangerous statement.
    for bad in ["ma:in", "ma?in", "ma*in", "main/", "up/../main", "ma\\in"] {
        let (st, out) = server.get(
            &format!("/v1/orgs/acme/repos/app/required-checks/{}", enc(bad)),
            &admin,
        );
        assert_eq!(st, 400, "{bad:?} was accepted: {out}");
        assert!(
            out["error"]
                .as_str()
                .unwrap_or_default()
                .contains("invalid branch"),
            "{bad:?}: {out}"
        );
    }

    // A stranger with no credential is masked from a private repo
    // entirely, and the answer must carry no evidence that it exists.
    //
    // A *viewer* is deliberately not the case tested here: an org
    // viewer may read this list, and asserting a refusal for one would
    // pin the wrong rule. The refusal that matters is the one that
    // cannot tell an outsider whether the repository is there.
    let (st, denied) = server.req(
        "GET",
        "/v1/orgs/acme/repos/app/required-checks/main",
        "",
        None,
    );
    assert_eq!(st, 401, "{denied}");
    let (st, absent) = server.req(
        "GET",
        "/v1/orgs/acme/repos/no-such-repo/required-checks/main",
        "",
        None,
    );
    assert_eq!(st, 401);
    assert_eq!(
        denied, absent,
        "the refusal tells a stranger which of the two repositories exists"
    );

    assert!(server.healthy());
}
