//! REST validation edges: every 4xx the handlers define, exercised over
//! the real server — commit operation validation, read type errors, undo
//! preconditions, token scope parsing, mirror registration errors,
//! public-repo anonymous reads, and asset serving corner cases.

use stratum_testkit::browser::Browser;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::{Minio, Server};

fn spawn_server(store_url: &str, scratch: &Scratch, extra: &[(&str, String)]) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint("api-edges")
        .data_dir(scratch.path().join("data"))
        .envs(extra)
        .env("STRATUM_COMPACT_POLL_SECS", "86400")
        .start()
}

fn commit_ops(
    server: &Server,
    token: &str,
    rp: &str,
    ops: serde_json::Value,
) -> (u16, serde_json::Value) {
    server.req(
        "POST",
        &format!("{rp}/commits"),
        token,
        Some(serde_json::json!({ "message": "edge", "operations": ops })),
    )
}

#[test]
fn commit_operation_validation() {
    let minio = Minio::shared();
    let bucket = minio.bucket("api-commits");
    let scratch = Scratch::new("api-commits");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";

    // Operation-list validation.
    let (st, _) = commit_ops(&server, &admin, rp, serde_json::json!([]));
    assert_eq!(st, 400, "no operations");
    let many: Vec<serde_json::Value> = (0..10_001)
        .map(|i| serde_json::json!({"op": "put", "path": format!("f{i}"), "content": "x"}))
        .collect();
    let (st, _) = commit_ops(&server, &admin, rp, serde_json::json!(many));
    assert_eq!(st, 400, "too many operations");

    // Path validation: traversal and .git are refused.
    for bad in ["../up", "a/../../b", ".git/hooks/pwn", "nul\0byte"] {
        let (st, out) = commit_ops(
            &server,
            &admin,
            rp,
            serde_json::json!([{"op": "put", "path": bad, "content": "x"}]),
        );
        assert_eq!(st, 400, "path {bad:?} accepted: {out}");
    }

    // Base64 content: good round-trips, bad answers 400.
    let (st, out) = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([{"op": "put_base64", "path": "bin/blob", "content": "aGVsbG8+P2==" }]),
    );
    assert_eq!(st, 201, "{out}");
    let (st, body) = server.req("GET", &format!("{rp}/files/bin/blob"), &admin, None);
    assert_eq!(st, 200);
    assert_eq!(body.as_str().unwrap(), "hello>?");
    let (st, _) = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([{"op": "put_base64", "path": "bin/bad", "content": "!!!not-b64" }]),
    );
    assert_eq!(st, 400);

    // expected_parent: null asserts repo emptiness → conflict once a
    // commit exists; a blob oid as parent is rejected as non-commit.
    let (st, first) = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([{"op": "put", "path": "seed", "content": "s"}]),
    );
    assert_eq!(st, 201, "{first}");
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/commits"),
        &admin,
        Some(serde_json::json!({
            "message": "must be first",
            "expected_parent": null,
            "operations": [{"op": "put", "path": "x", "content": "x"}],
        })),
    );
    assert_eq!(st, 409, "{out}");
    let (_, tree) = server.req("GET", &format!("{rp}/tree"), &admin, None);
    let blob_oid = tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "seed")
        .unwrap()["oid"]
        .as_str()
        .unwrap()
        .to_string();
    // A wrong (non-tip) expected_parent — even a non-commit oid — is a
    // CAS conflict first: the tip comparison guards before any type check.
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/commits"),
        &admin,
        Some(serde_json::json!({
            "message": "blob parent",
            "expected_parent": blob_oid,
            "operations": [{"op": "put", "path": "x", "content": "x"}],
        })),
    );
    assert_eq!(st, 409, "non-tip parent conflicts: {out}");

    // Writing through a file as if it were a directory replaces the file
    // with a directory — last write wins, the agent-API contract.
    let (st, out) = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([{"op": "put", "path": "seed/child", "content": "x"}]),
    );
    assert_eq!(st, 201, "{out}");
    let (st, body) = server.req("GET", &format!("{rp}/files/seed/child"), &admin, None);
    assert_eq!(st, 200);
    assert_eq!(body.as_str().unwrap(), "x");
    // Put the file back for the no-op check below.
    let (st, _) = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([
            {"op": "delete", "path": "seed"},
            {"op": "put", "path": "seed", "content": "s"}
        ]),
    );
    assert_eq!(st, 201);

    // A no-op commit (same content) acks with the current commit instead
    // of writing an empty pack.
    let (st, again) = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([{"op": "put", "path": "seed", "content": "s"}]),
    );
    assert_eq!(st, 201);
    let (_, log) = server.req("GET", &format!("{rp}/log?limit=1"), &admin, None);
    assert_eq!(
        log["entries"][0]["commit"], again["commit"],
        "no-op commit returns the standing tip"
    );

    // A mirror's REST writes are forwarded to its origin. One that
    // cannot be reached is the refusal, as a 502 naming why — not a
    // 403 pretending the mirror is read-only.
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(serde_json::json!({ "name": "m", "provider": "generic", "origin": "file:///nonexistent" })),
    );
    assert_eq!(st, 202);
    let (st, out) = commit_ops(
        &server,
        &admin,
        "/v1/orgs/acme/repos/m",
        serde_json::json!([{"op": "put", "path": "x", "content": "x"}]),
    );
    // Its origin does not exist, so its first sync never landed and it
    // has no layout to build a commit on. A clean 502 saying so, not a
    // leaked `manifest.json: HTTP 404` as a 500.
    assert_eq!(st, 502, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no layout yet"),
        "{out}"
    );
    // A branch on the same never-synced mirror is the same clean 502,
    // not a 500: there is no layout to resolve `main` against.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/m/branches",
        &admin,
        Some(serde_json::json!({ "name": "b", "from": "main" })),
    );
    assert_eq!(st, 502, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no layout yet"),
        "{out}"
    );
}

#[test]
fn read_type_errors_and_diff_shapes() {
    let minio = Minio::shared();
    let bucket = minio.bucket("api-reads");
    let scratch = Scratch::new("api-reads");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    let (st, c1) = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([
            {"op": "put", "path": "file.txt", "content": "f\n"},
            {"op": "put", "path": "dir/a.txt", "content": "a\n"},
        ]),
    );
    assert_eq!(st, 201);
    let c1 = c1["commit"].as_str().unwrap().to_string();

    // Walking a path *through* a file answers not-found, not a 500.
    let (st, _) = server.req("GET", &format!("{rp}/files/file.txt/child"), &admin, None);
    assert_eq!(st, 404, "file in the middle of a path");

    // /files of a directory → type error; /tree of a file → type error.
    let (st, _) = server.req("GET", &format!("{rp}/files/dir"), &admin, None);
    assert!(st >= 400, "directory is not a blob");
    let (st, _) = server.req("GET", &format!("{rp}/tree/file.txt"), &admin, None);
    assert!(st >= 400, "file is not a tree");
    // Unknown revs and paths 404.
    let (st, _) = server.req("GET", &format!("{rp}/files/file.txt?at=zzz"), &admin, None);
    assert_eq!(st, 404);
    let (st, _) = server.req(
        "GET",
        &format!("{rp}/log?rev={}", "a".repeat(40)),
        &admin,
        None,
    );
    assert!(st >= 400, "log from an unknown commit");

    // diff parameter validation.
    let (st, _) = server.req("GET", &format!("{rp}/diff"), &admin, None);
    assert_eq!(st, 400, "from and to required");
    let (st, _) = server.req(
        "GET",
        &format!("{rp}/diff?from={c1}&to={}", "a".repeat(40)),
        &admin,
        None,
    );
    assert!(st >= 400, "unknown commit in diff");

    // Type-change diffs: file→directory and directory→file, plus a
    // directory deletion — every recursion arm of the tree differ.
    let (st, c2) = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([
            {"op": "delete", "path": "file.txt"},
            {"op": "put", "path": "file.txt/nested.txt", "content": "n\n"},
            {"op": "delete", "path": "dir/a.txt"},
            {"op": "put", "path": "dir", "content": "now a file\n"},
        ]),
    );
    assert_eq!(st, 201, "{c2}");
    let c2 = c2["commit"].as_str().unwrap().to_string();
    let (st, diff) = server.req("GET", &format!("{rp}/diff?from={c1}&to={c2}"), &admin, None);
    assert_eq!(st, 200, "{diff}");
    let changes = diff["changes"].as_array().unwrap();
    let paths: Vec<&str> = changes.iter().filter_map(|c| c["path"].as_str()).collect();
    assert!(paths.contains(&"file.txt"), "{paths:?}");
    assert!(paths.contains(&"dir/a.txt"), "{paths:?}");
    assert!(paths.contains(&"dir"), "{paths:?}");
    // Identical from/to short-circuits to an empty diff.
    let (st, same) = server.req("GET", &format!("{rp}/diff?from={c2}&to={c2}"), &admin, None);
    assert_eq!(st, 200);
    assert!(same["changes"].as_array().unwrap().is_empty());
    // Directory deletion in reverse direction covers the tree-removal arm.
    let (st, back) = server.req("GET", &format!("{rp}/diff?from={c2}&to={c1}"), &admin, None);
    assert_eq!(st, 200, "{back}");
}

#[test]
fn undo_preconditions_and_revert_of_root() {
    let minio = Minio::shared();
    let bucket = minio.bucket("api-undo");
    let scratch = Scratch::new("api-undo");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    let (_, c1) = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([{"op": "put", "path": "a", "content": "1"}]),
    );
    let c1 = c1["commit"].as_str().unwrap().to_string();

    // Revert with a wrong expected_head is a conflict.
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/revert"),
        &admin,
        Some(serde_json::json!({ "commit": c1, "expected_head": "a".repeat(40) })),
    );
    assert_eq!(st, 409, "{out}");

    // Reverting the root commit produces the empty tree.
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/revert"),
        &admin,
        Some(serde_json::json!({ "commit": c1 })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, tree) = server.req("GET", &format!("{rp}/tree"), &admin, None);
    assert_eq!(st, 200);
    assert!(tree["entries"].as_array().unwrap().is_empty(), "{tree}");

    // Reverting a non-root commit restores its parent's tree.
    let (st, mid) = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([{"op": "put", "path": "kept", "content": "keep"}]),
    );
    assert_eq!(st, 201, "{mid}");
    let (st, bad) = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([{"op": "put", "path": "mistake", "content": "oops"}]),
    );
    assert_eq!(st, 201, "{bad}");
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/revert"),
        &admin,
        Some(serde_json::json!({ "commit": bad["commit"] })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, _) = server.req("GET", &format!("{rp}/files/kept"), &admin, None);
    assert_eq!(st, 200, "prior content survives the revert");
    let (st, _) = server.req("GET", &format!("{rp}/files/mistake"), &admin, None);
    assert_eq!(st, 404, "reverted content is gone");

    // Reset without a branch field defaults to main.
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/reset"),
        &admin,
        Some(serde_json::json!({ "to": c1 })),
    );
    assert_eq!(st, 200, "{out}");
    let (_, log) = server.req("GET", &format!("{rp}/log?limit=1"), &admin, None);
    assert_eq!(log["entries"][0]["commit"].as_str().unwrap(), c1);

    // Deleting an absent branch/tag is idempotent (Expect::Any delete).
    let (st, _) = server.req("DELETE", &format!("{rp}/branches/ghost"), &admin, None);
    assert_eq!(st, 204);
    let (st, _) = server.req("DELETE", &format!("{rp}/tags/ghost"), &admin, None);
    assert_eq!(st, 204);
}

#[test]
fn token_scope_parsing_and_repo_scoping() {
    let minio = Minio::shared();
    let bucket = minio.bucket("api-tokens");
    let scratch = Scratch::new("api-tokens");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );

    // Unknown scope names 400; unknown repo names 404.
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({ "scopes": ["root:everything"] })),
    );
    assert_eq!(st, 400);
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({ "scopes": ["repo:read"], "repo": "ghost" })),
    );
    assert_eq!(st, 404);

    // repo:write scoped token minted via the API works on its repo.
    let (st, minted) = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({ "scopes": ["repo:read", "repo:write"], "repo": "app", "label": "ci" })),
    );
    assert_eq!(st, 201, "{minted}");
    let tok = minted["token"].as_str().unwrap();
    let (st, _) = commit_ops(
        &server,
        tok,
        "/v1/orgs/acme/repos/app",
        serde_json::json!([{"op": "put", "path": "w", "content": "w"}]),
    );
    assert_eq!(st, 201);

    // Revoking an unknown token id: not found.
    let (st, _) = server.req("DELETE", "/v1/orgs/acme/tokens/no-such-id", &admin, None);
    assert_eq!(st, 404);
}

#[test]
fn mirror_registration_validation() {
    let minio = Minio::shared();
    let bucket = minio.bucket("api-mirrors");
    let scratch = Scratch::new("api-mirrors");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");

    // Provider defaults to github, which is not configured here → 400
    // naming the provider.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(serde_json::json!({ "name": "gh", "origin": "acme/widget" })),
    );
    assert_eq!(st, 400, "{out}");
    assert!(out["error"].as_str().unwrap().contains("github"), "{out}");

    // Invalid name and duplicate registration.
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(
            serde_json::json!({ "name": "Bad Name", "provider": "generic", "origin": "file:///x" }),
        ),
    );
    assert!(st == 400 || st == 500, "invalid mirror name: {st}");
    for _ in 0..2 {
        server.req(
            "POST",
            "/v1/orgs/acme/mirrors",
            &admin,
            Some(
                serde_json::json!({ "name": "dup", "provider": "generic", "origin": "file:///y" }),
            ),
        );
    }
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(serde_json::json!({ "name": "dup", "provider": "generic", "origin": "file:///y" })),
    );
    assert_eq!(st, 409);

    // sync_now on a native repo is a 400.
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "native" })),
    );
    let (st, _) = server.req("POST", "/v1/orgs/acme/mirrors/native/sync", &admin, None);
    assert_eq!(st, 400);

    // Unknown inbound webhook provider is a 404.
    let resp = ureq::post(&format!("{}/webhooks/bogus-provider", server.base)).send_string("{}");
    assert!(matches!(resp, Err(ureq::Error::Status(404, _))));
}

#[test]
fn public_repos_read_anonymously_and_basic_auth_variants() {
    let minio = Minio::shared();
    let bucket = minio.bucket("api-public");
    let scratch = Scratch::new("api-public");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "open", "public": true })),
    );
    let rp = "/v1/orgs/acme/repos/open";
    commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([{"op": "put", "path": "readme", "content": "open\n"}]),
    );

    // Anonymous REST reads of a public repo succeed; writes still 401.
    let (st, body) = server.req("GET", &format!("{rp}/files/readme"), "", None);
    assert_eq!(st, 200, "{body}");
    let (st, _) = server.req("GET", rp, "", None);
    assert_eq!(st, 200);
    let (st, _) = server.req(
        "POST",
        &format!("{rp}/commits"),
        "",
        Some(serde_json::json!({ "message": "x", "operations": [{"op":"put","path":"x","content":"x"}] })),
    );
    assert_eq!(st, 401);

    // ...but not its metrics, which are members-only whatever the
    // repository's visibility.
    //
    // Publishing the code does not publish how the code is used: how
    // often it is cloned, how many bytes it serves, how far behind its
    // origin a mirror runs. Those belong to the people who own it, and
    // on a public repository they were world-readable to anyone who
    // guessed the path — the route took `Scope::RepoRead` through
    // `rest_repo_auth`, whose public-read fallback answered an
    // anonymous caller in full. Nothing in the UI linked to it, which
    // is exactly why it went unnoticed: an endpoint nothing points at
    // is still an endpoint.
    //
    // Both shapes, because the CSV is the same handler behind a query
    // parameter and would have been the way round a gate on the JSON.
    for path in [format!("{rp}/metrics"), format!("{rp}/metrics?format=csv")] {
        let (st, body) = server.req("GET", &path, "", None);
        assert_eq!(st, 401, "GET {path} answered anonymously: {body}");
        assert!(
            !body.to_string().contains("kinds"),
            "GET {path} leaked metrics to an anonymous caller: {body}"
        );
    }
    // The owner still gets them, so this is a gate and not an outage.
    let (st, body) = server.req("GET", &format!("{rp}/metrics"), &admin, None);
    assert_eq!(st, 200, "the owner was refused their own metrics: {body}");

    // And the row says so, in the field the repository page draws its
    // Insights tab from — one answer, so the tab and the route cannot
    // disagree about who is a member.
    let (_, row) = server.req("GET", rp, &admin, None);
    assert_eq!(row["viewer_member"], true, "owner: {row}");
    let (_, row) = server.req("GET", rp, "", None);
    assert_eq!(
        row["viewer_member"], false,
        "an anonymous reader of a public repository is not a member: {row}"
    );

    // Anonymous git clone of the public repo.
    let base = server.base.strip_prefix("http://").unwrap();
    let anon_url = format!("http://{base}/acme/open.git");
    gitcli::clone_and_fsck(&anon_url, &scratch.path().join("anon"));

    // Basic auth with the token in the *username* position also works
    // (some CI systems put it there).
    let tok_user_url = format!("http://{admin}:@{base}/acme/open.git");
    gitcli::clone_and_fsck(&tok_user_url, &scratch.path().join("tokuser"));

    // Unknown git service names are refused.
    let resp = ureq::get(&format!(
        "{}/acme/open/info/refs?service=git-frobnicate",
        server.base
    ))
    .call();
    match resp {
        Err(ureq::Error::Status(code, _)) => assert!(code >= 400),
        Ok(r) => panic!("unknown service accepted: {}", r.status()),
        Err(e) => panic!("transport: {e}"),
    }
}

/// "Reads with **any** valid credential" is what the authentication page
/// promises of a public repository, and the git wire has always kept it:
/// a token minted in your own namespace clones somebody else's public
/// repository, which is how a fork follows its upstream. REST did not
/// keep it. A personal token from another namespace was masked as 404
/// where an anonymous request got 200, and a signed-in person with no
/// role in the org came back as *nobody* — so the outside contributor's
/// change had no author, their comment was refused with "sign in", and
/// their viewed marks with "not a service token".
///
/// Asserted through the front door, per credential: a stranger's session,
/// a stranger's personal token, and a foreign service token all read; a
/// person is attributed as themselves when they act; nothing writes; and
/// a private repository stays masked from every one of them.
#[test]
fn a_public_repository_reads_with_any_valid_credential_and_a_person_acts_as_themselves() {
    let minio = Minio::shared();
    let bucket = minio.bucket("api-any-cred");
    let scratch = Scratch::new("api-any-cred");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    for (name, public) in [("open", true), ("vault", false)] {
        let (st, body) = server.req(
            "POST",
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name, "public": public })),
        );
        assert_eq!(st, 201, "{body}");
        commit_ops(
            &server,
            &admin,
            &format!("/v1/orgs/acme/repos/{name}"),
            serde_json::json!([{"op": "put", "path": "readme", "content": "hi\n"}]),
        );
    }
    // A change on the public repository, for the stranger to talk about.
    let (st, body) = server.req(
        "POST",
        "/v1/orgs/acme/repos/open/branches",
        &admin,
        Some(serde_json::json!({ "name": "feature", "from": "main" })),
    );
    assert_eq!(st, 201, "{body}");
    let (st, body) = server.req(
        "POST",
        "/v1/orgs/acme/repos/open/commits",
        &admin,
        Some(serde_json::json!({
            "branch": "feature",
            "message": "work\n\nChange-Id: I0a11c0de\n",
            "operations": [{"op": "put", "path": "b.txt", "content": "2"}],
        })),
    );
    assert_eq!(st, 201, "{body}");
    let (st, body) = server.req(
        "POST",
        "/v1/orgs/acme/repos/open/changes",
        &admin,
        Some(serde_json::json!({ "from": "feature" })),
    );
    assert_eq!(st, 201, "{body}");

    // The stranger: a person in another organisation entirely, with a
    // session and a personal token minted there.
    let rival = server.bootstrap_org("rival");
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "rival",
            "--email",
            "sam@rival.test",
            "--name",
            "Sam",
            "--password",
            "a long enough password",
            "--role",
            "member",
        ])
        .expect("user-create sam");
    let mut sam = Browser::signed_in(&server, "sam@rival.test", "a long enough password");
    let (st, me) = sam.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    let sam_id = me["id"].as_str().expect("sam has an id").to_string();
    let (st, minted) = sam.req(
        "POST",
        "/v1/orgs/rival/tokens",
        Some(serde_json::json!({ "scopes": ["repo:write"], "label": "laptop" })),
    );
    assert_eq!(st, 201, "{minted}");
    let sam_token = minted["token"].as_str().expect("a token").to_string();

    // Every credential reads the public repository the way anonymous
    // does. `tree` and `changes` are the routes that answered 404 to a
    // foreign token; the repo row already answered 200 to everyone, so a
    // client saw the repository exist and then fail to open.
    for path in [
        "/v1/orgs/acme/repos/open",
        "/v1/orgs/acme/repos/open/tree",
        "/v1/orgs/acme/repos/open/files/readme",
        "/v1/orgs/acme/repos/open/changes",
        "/v1/orgs/acme/repos/open/changes/I0a11c0de",
    ] {
        for (who, token) in [
            ("anonymous", ""),
            ("a foreign personal token", sam_token.as_str()),
            ("a foreign service token", rival.as_str()),
        ] {
            let (st, body) = server.req("GET", path, token, None);
            assert_eq!(st, 200, "{who} reads {path}: {body}");
        }
        let (st, body) = sam.req("GET", path, None);
        assert_eq!(st, 200, "a foreign session reads {path}: {body}");
    }

    // Reading is not writing. A public repository the stranger may read
    // is one they may not commit to, whatever they hold.
    let write = serde_json::json!({
        "message": "x",
        "operations": [{"op": "put", "path": "x", "content": "x"}],
    });
    for (who, token, expect) in [
        ("anonymous", "", 401),
        ("a foreign personal token", sam_token.as_str(), 404),
        ("a foreign service token", rival.as_str(), 404),
    ] {
        let (st, body) = server.req(
            "POST",
            "/v1/orgs/acme/repos/open/commits",
            token,
            Some(write.clone()),
        );
        assert_eq!(st, expect, "{who} writes to a public repo: {body}");
    }
    let (st, body) = sam.req("POST", "/v1/orgs/acme/repos/open/commits", Some(write));
    assert_eq!(st, 404, "a foreign session writes to a public repo: {body}");

    // And the private repository does not exist for any of them.
    for path in [
        "/v1/orgs/acme/repos/vault",
        "/v1/orgs/acme/repos/vault/tree",
    ] {
        for (token, expect) in [("", 401), (sam_token.as_str(), 404), (rival.as_str(), 404)] {
            let (st, body) = server.req("GET", path, token, None);
            assert_eq!(st, expect, "{path} with {token:?}: {body}");
        }
        let (st, body) = sam.req("GET", path, None);
        assert_eq!(st, 404, "a foreign session reads a private repo: {body}");
    }

    // When the person acts, it is as themselves — by session and by
    // token alike — and a service token from elsewhere is nobody: it may
    // read, and the conversation is for people and the org's own
    // machines.
    let comments = "/v1/orgs/acme/repos/open/changes/I0a11c0de/comments";
    let (st, body) = sam.req(
        "POST",
        comments,
        Some(serde_json::json!({ "body": "from a browser" })),
    );
    assert_eq!(st, 201, "{body}");
    assert_eq!(
        body["author_principal"],
        serde_json::json!(format!("user:{sam_id}")),
        "{body}"
    );
    let (st, body) = server.req(
        "POST",
        comments,
        &sam_token,
        Some(serde_json::json!({ "body": "from a laptop" })),
    );
    assert_eq!(st, 201, "{body}");
    assert_eq!(
        body["author_principal"],
        serde_json::json!(format!("user:{sam_id}")),
        "{body}"
    );
    let (st, body) = server.req(
        "POST",
        comments,
        &rival,
        Some(serde_json::json!({ "body": "from a machine" })),
    );
    assert_eq!(st, 401, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("sign in to comment"),
        "{body}"
    );

    assert!(server.healthy());
}

#[test]
fn asset_mime_types_and_dirless_servers() {
    let minio = Minio::shared();
    let bucket = minio.bucket("api-assets");
    let scratch = Scratch::new("api-assets");

    // Site dir with every remaining MIME family.
    let site = scratch.path().join("site");
    std::fs::create_dir_all(&site).unwrap();
    for (name, content) in [
        ("feed.xml", "<x/>"),
        ("font.woff2", "wf2"),
        ("app.webmanifest", "{}"),
        ("mod.wasm", "\0asm"),
        ("blob.dat", "raw"),
    ] {
        std::fs::write(site.join(name), content).unwrap();
    }
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_SITE_DIR", site.display().to_string())],
    );
    for (path, want_ct) in [
        ("/feed.xml", "application/xml"),
        ("/font.woff2", "font/woff2"),
        ("/app.webmanifest", "application/manifest+json"),
        ("/mod.wasm", "application/wasm"),
        ("/blob.dat", "application/octet-stream"),
    ] {
        let resp = ureq::get(&format!("{}{path}", server.base)).call().unwrap();
        assert_eq!(resp.header("Content-Type").unwrap_or(""), want_ct, "{path}");
    }
    // Literal dot-dot traversal answers 404 straight from the sanitizer.
    let err = ureq::get(&format!("{}/../../etc/passwd", server.base))
        .call()
        .unwrap_err();
    assert!(matches!(err, ureq::Error::Status(404, _)));

    // A server with no asset dirs 404s both surfaces.
    let scratch2 = Scratch::new("api-assets-none");
    let bare = spawn_server(&bucket.base_url, &scratch2, &[]);
    for path in ["/", "/dashboard", "/dashboard/deep/link", "/llms.txt"] {
        let r = ureq::get(&format!("{}{path}", bare.base)).call();
        assert!(
            matches!(r, Err(ureq::Error::Status(404, _))),
            "{path} on a dir-less server"
        );
    }
}

/// Commit-path corners on real layouts: first-commit-with-null-parent,
/// commits after compaction (locator-plane dedup), duplicate blobs in
/// one request, idempotent replay, and the sha1-only guard on commits.
#[test]
fn commit_corners_on_compacted_and_empty_repos() {
    let minio = Minio::shared();
    let bucket = minio.bucket("api-commit2");
    let scratch = Scratch::new("api-commit2");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "fresh" })),
    );

    // expected_parent: null on an actually-empty repo is the way to
    // assert "I create the first commit".
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/fresh/commits",
        &admin,
        Some(serde_json::json!({
            "message": "first",
            "expected_parent": null,
            "operations": [{"op": "put", "path": "a", "content": "1"}],
        })),
    );
    assert_eq!(st, 201, "{out}");

    // A commit whose operations produce the same blob twice dedups it.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/fresh/commits",
        &admin,
        Some(serde_json::json!({
            "message": "twins",
            "operations": [
                {"op": "put", "path": "x1", "content": "same-bytes"},
                {"op": "put", "path": "x2", "content": "same-bytes"},
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");

    // Compacted repo: later commits dedup new objects via the locator.
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "packed" })),
    );
    let rp = "/v1/orgs/acme/repos/packed";
    for i in 0..9 {
        let (st, _) = commit_ops(
            &server,
            &admin,
            rp,
            serde_json::json!([{"op": "put", "path": format!("f{}.txt", i % 3), "content": format!("v{i}")}]),
        );
        assert_eq!(st, 201);
    }
    let (st, _) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    assert_eq!(st, 200);
    // Re-adding content that already lives in the compacted layout: the
    // blob dedups against the locator, only the tree/commit are new.
    let (st, out) = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([{"op": "put", "path": "resurrect.txt", "content": "v0"}]),
    );
    assert_eq!(st, 201, "{out}");

    // Idempotent replay: the identical request against the same parent
    // within the commit-timestamp resolution acks the standing commit.
    let (_, log) = server.req("GET", &format!("{rp}/log?limit=2"), &admin, None);
    let parent = log["entries"][1]["commit"].as_str().unwrap().to_string();
    let body = serde_json::json!({
        "message": "replayed",
        "expected_parent": log["entries"][0]["commit"],
        "operations": [{"op": "put", "path": "replay.txt", "content": "r"}],
    });
    let _ = parent;
    let (st, first) = server.req("POST", &format!("{rp}/commits"), &admin, Some(body.clone()));
    assert_eq!(st, 201, "{first}");
    // Replay is timing-dependent — a same-second replay acks the standing
    // commit, a second boundary mints a fresh one — so the loop asserts
    // the invariant every answer must satisfy (201 or 409, never anything
    // else) and stops early on a confirmed replay hit. (A prior version
    // ended with `assert!(replay_hit || true)`, which asserted nothing.)
    for _ in 0..3 {
        let mut b = body.clone();
        b["expected_parent"] = first["parent"].clone();
        let (st, second) = server.req("POST", &format!("{rp}/commits"), &admin, Some(b));
        if st == 201 && second["commit"] == first["commit"] {
            break;
        }
        if st != 201 && st != 409 {
            panic!("replay answered {st}: {second}");
        }
    }

    // sha1-only guard on the commit path.
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "future" })),
    );
    commit_ops(
        &server,
        &admin,
        "/v1/orgs/acme/repos/future",
        serde_json::json!([{"op": "put", "path": "a", "content": "x"}]),
    );
    let store =
        stratum_store::ObjectStore::new(&bucket.base_url, stratum_store::LatencyModel::None);
    let (_, repo) = server.req("GET", "/v1/orgs/acme/repos/future", &admin, None);
    let mkey = format!(
        "o/{}/r/{}/prod/manifest.json",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&store.get(&mkey).unwrap()).unwrap();
    manifest["object_format"] = serde_json::json!("sha256");
    store
        .put(
            &mkey,
            &serde_json::to_vec(&manifest).unwrap(),
            stratum_store::PutCond::None,
        )
        .unwrap();
    let (st, out) = commit_ops(
        &server,
        &admin,
        "/v1/orgs/acme/repos/future",
        serde_json::json!([{"op": "put", "path": "b", "content": "y"}]),
    );
    assert!(st >= 500, "sha256 layout refuses commits: {out}");
}

/// Revision-form resolution, base64 alphabet edges, basic-auth token
/// positions, scope-limited bulk/sync/receive answers, and empty-repo
/// compaction.
#[test]
fn rev_forms_auth_positions_and_scope_answers() {
    let minio = Minio::shared();
    let bucket = minio.bucket("api-revforms");
    let scratch = Scratch::new("api-revforms");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_REF_PAGE_SIZE", "2".into())],
    );
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    let c1 = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([{"op": "put", "path": "a", "content": "1"}]),
    )
    .1["commit"]
        .as_str()
        .unwrap()
        .to_string();
    server.req(
        "POST",
        &format!("{rp}/tags"),
        &admin,
        Some(serde_json::json!({ "name": "v1", "target": c1 })),
    );

    // Full ref name, short branch, and short tag all resolve.
    for rev in ["refs/heads/main", "main", "v1"] {
        let (st, log) = server.req("GET", &format!("{rp}/log?rev={rev}"), &admin, None);
        assert_eq!(st, 200, "rev {rev}");
        assert_eq!(
            log["entries"][0]["commit"].as_str().unwrap(),
            c1,
            "rev {rev}"
        );
    }
    // And on a paged manifest the same forms route through the pages.
    for i in 0..9 {
        commit_ops(
            &server,
            &admin,
            rp,
            serde_json::json!([{"op": "put", "path": format!("f{}", i % 3), "content": format!("{i}")}]),
        );
    }
    let (st, _) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    assert_eq!(st, 200);
    let (st, _) = server.req("GET", &format!("{rp}/log?rev=main"), &admin, None);
    assert_eq!(st, 200);

    // Base64 content covering the +/ alphabet arms.
    let (st, _) = commit_ops(
        &server,
        &admin,
        rp,
        serde_json::json!([{"op": "put_base64", "path": "bin/all", "content": "+/+/AA==" }]),
    );
    assert_eq!(st, 201);

    // Basic auth: token in the password slot and in the username slot.
    let creds_pass = |t: &str| {
        let raw = format!("x:{t}");
        let mut out = String::new();
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        for chunk in raw.as_bytes().chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(A[(n >> 18) as usize & 63] as char);
            out.push(A[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                A[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                A[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    };
    let get_with_basic = |b64: &str| match ureq::get(&format!("{}{rp}", server.base))
        .set("Authorization", &format!("Basic {b64}"))
        .call()
    {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(c, _)) => c,
        Err(e) => panic!("{e}"),
    };
    assert_eq!(
        get_with_basic(&creds_pass(&admin)),
        200,
        "token as password"
    );
    let user_slot = creds_pass(&admin).replace('=', ""); // reuse encoder shape
    let _ = user_slot;
    let raw = format!("{admin}:");
    let mut b64u = String::new();
    const A2: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in raw.as_bytes().chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        b64u.push(A2[(n >> 18) as usize & 63] as char);
        b64u.push(A2[(n >> 12) as usize & 63] as char);
        b64u.push(if chunk.len() > 1 {
            A2[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        b64u.push(if chunk.len() > 2 {
            A2[n as usize & 63] as char
        } else {
            '='
        });
    }
    assert_eq!(get_with_basic(&b64u), 200, "token as username");
    // Garbage base64 with +/ characters decodes to junk → 401.
    assert_eq!(get_with_basic("a+/a"), 401);

    // Scope answers: an org:read token cannot bulk-export or sync-drive.
    let minted = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({ "scopes": ["org:read"] })),
    );
    let read_tok = minted.1["token"].as_str().unwrap().to_string();
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/export",
        &read_tok,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 404, "bulk export needs org:admin");
    server.req(
        "POST",
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(
            serde_json::json!({ "name": "m", "provider": "generic", "origin": "file:///nowhere" }),
        ),
    );
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors/m/sync",
        &read_tok,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 404, "sync_now needs repo:write");

    // Raw POST git-receive-pack on a mirror with no commands: a mirror
    // forwards pushes now, so an empty push answers exactly as a native
    // repository's does — a flush, nothing to forward. (This used to be
    // a 403 naming the origin, when mirrors were read-only.)
    let resp = ureq::post(&format!("{}/acme/m/git-receive-pack", server.base))
        .set("Authorization", &format!("Bearer {admin}"))
        .set("Content-Type", "application/x-git-receive-pack-request")
        .send_bytes(b"0000")
        .expect("an empty push is not an error");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.into_string().unwrap(), "0000");

    // Compacting an empty repo is NotNeeded, not an error.
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "hollow" })),
    );
    let (st, out) = server.req("POST", "/v1/orgs/acme/repos/hollow/compact", &admin, None);
    assert_eq!(st, 200, "{out}");
    assert!(
        out["outcome"].as_str().unwrap().contains("NotNeeded")
            || out["outcome"].as_str().unwrap().contains("Skipped"),
        "{out}"
    );

    // Literal dot-dot path over raw TCP (no client normalization) is
    // refused by the path sanitizer.
    use std::io::{Read as _, Write as _};
    let hostport = server.base.strip_prefix("http://").unwrap();
    let mut sock = std::net::TcpStream::connect(hostport).unwrap();
    sock.write_all(b"GET /../../etc/passwd HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .unwrap();
    let mut resp = Vec::new();
    let _ = sock.read_to_end(&mut resp);
    let head = String::from_utf8_lossy(&resp);
    assert!(
        head.starts_with("HTTP/1.1 404") || head.starts_with("HTTP/1.1 400"),
        "{head}"
    );
}

/// A 401's `WWW-Authenticate` header is transport-specific, and both
/// halves are load-bearing.
///
/// The git wire must send `Basic`: it is what makes `git clone` retry
/// with credentials instead of failing. The REST API must NOT — a
/// browser handles a Basic challenge on a same-origin `fetch()` itself,
/// opening its native credential dialog, and the promise never settles.
/// That left the dashboard's sign-in button stuck on "Checking…" forever
/// with no error, found only by driving a real browser against a real
/// server; a mocked 401 cannot reproduce it.
#[test]
fn the_basic_auth_challenge_is_sent_to_git_and_withheld_from_the_rest_api() {
    let minio = Minio::shared();
    let bucket = minio.bucket("auth-challenge");
    let scratch = Scratch::new("auth-challenge");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    assert_eq!(
        server
            .req(
                "POST",
                "/v1/orgs/acme/repos",
                &admin,
                Some(serde_json::json!({ "name": "app" }))
            )
            .0,
        201
    );

    let challenge = |path: &str, token: &str| -> (u16, Option<String>) {
        let mut r = ureq::get(&format!("{}{path}", server.base));
        if !token.is_empty() {
            r = r.set("Authorization", &format!("Bearer {token}"));
        }
        match r.call() {
            Ok(x) => (x.status(), x.header("www-authenticate").map(str::to_string)),
            Err(ureq::Error::Status(c, x)) => (c, x.header("www-authenticate").map(str::to_string)),
            Err(e) => panic!("transport: {e}"),
        }
    };

    // REST, bad token and no token alike: 401 with NO challenge.
    for (path, token) in [
        ("/v1/orgs/acme/repos?limit=200", "weft_bogus"),
        ("/v1/orgs/acme/repos?limit=200", ""),
        ("/v1/orgs/acme/repos/app", "weft_bogus"),
        ("/v1/orgs/acme/usage", "weft_bogus"),
    ] {
        let (status, hdr) = challenge(path, token);
        assert_eq!(status, 401, "{path}");
        assert_eq!(
            hdr, None,
            "the REST API must not challenge {path}: a browser would open its own \
             credential dialog and the fetch would never settle"
        );
    }

    // The git wire keeps the challenge, so git retries with credentials.
    for path in [
        "/acme/app.git/info/refs?service=git-upload-pack",
        "/acme/app.git/info/refs?service=git-receive-pack",
    ] {
        let (status, hdr) = challenge(path, "");
        assert_eq!(status, 401, "{path}");
        assert_eq!(
            hdr.as_deref(),
            Some("Basic realm=\"stratum\""),
            "git needs the challenge on {path} or a private clone fails instead of \
             prompting"
        );
    }
}

/// A minted credential can be given a deadline, and it dies at it.
///
/// The shape the build runner needs: a `repo:read` token bound to one
/// repository, handed to a container that is about to run somebody
/// else's code, and good for only as long as the job. Until now every
/// token was immortal until something remembered to revoke it — which
/// means a dispatcher that is deployed over, killed, or that simply
/// loses the race between handing the token out and revoking it leaves a
/// live credential inside a build with no deadline at all. Scoping to
/// one repository is undone by outliving the job.
///
/// Driven through HTTP rather than the control plane on purpose: the
/// deadline has to hold on the doors a runner actually knocks on, which
/// are the REST API and the git transport, not a Rust function.
#[test]
fn a_token_can_be_given_a_deadline_and_stops_working_at_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("api-token-ttl");
    let scratch = Scratch::new("api-token-ttl");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );

    // A duration, not an instant: a caller sending a deadline has to
    // agree with us about the clock, and the ones that get it wrong send
    // seconds where we read millis and mint something good for fifty
    // thousand years. Both nonsense directions are refused rather than
    // clamped, because each means somebody computed a duration wrongly
    // and a credential dead on arrival surfaces as a puzzling 401 later.
    for bad in [0, -60] {
        let (st, out) = server.req(
            "POST",
            "/v1/orgs/acme/tokens",
            &admin,
            Some(serde_json::json!({ "scopes": ["repo:read"], "expires_in_secs": bad })),
        );
        assert_eq!(st, 400, "expires_in_secs {bad} must be refused: {out}");
    }
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({
            "scopes": ["repo:read"],
            "expires_in_secs": 400 * 24 * 60 * 60_i64,
        })),
    );
    assert_eq!(st, 400, "beyond the cap: {out}");

    // The runner's credential: one repo, read only, two seconds of life.
    let (st, minted) = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({
            "scopes": ["repo:read"],
            "repo": "app",
            "label": "runner job",
            "expires_in_secs": 2,
        })),
    );
    assert_eq!(st, 201, "{minted}");
    let tok = minted["token"].as_str().expect("a token").to_string();
    assert!(
        minted["expires_at"].as_i64().is_some(),
        "the deadline comes back with the secret, so a caller need not \
         recompute our arithmetic from its own clock: {minted}"
    );

    // Alive: it reads its repository.
    let (st, out) = server.req("GET", "/v1/orgs/acme/repos/app/refs", &tok, None);
    assert_eq!(st, 200, "a live token must read its repo: {out}");

    // And clones with it. Asserted *before* the deadline because the
    // negative below is "git failed", which a mistyped URL or a missing
    // binary satisfies just as well — a failure assertion is only worth
    // the success assertion that makes it specific.
    let clone_url = server.base.replace("http://", &format!("http://x:{tok}@")) + "/acme/app.git";
    let out = std::process::Command::new("git")
        .args(["ls-remote", &clone_url])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git ls-remote");
    assert!(
        out.status.success(),
        "a live repo:read token must clone: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // And the list says when it ends, without saying anybody revoked it.
    let (st, listed) = server.req("GET", "/v1/orgs/acme/tokens", &admin, None);
    assert_eq!(st, 200, "{listed}");
    let row = listed["tokens"]
        .as_array()
        .expect("tokens array")
        .iter()
        .find(|t| t["id"] == minted["id"])
        .unwrap_or_else(|| panic!("the minted token is not listed: {listed}"));
    assert!(row["expires_at"].as_i64().is_some(), "{row}");
    assert!(row["revoked_at"].is_null(), "{row}");

    // Wait it out. Two seconds is the shortest thing worth asserting
    // against a real clock; a mocked one would prove the arithmetic and
    // not that the *reader* consults it.
    std::thread::sleep(std::time::Duration::from_millis(2_400));

    // Dead on the REST door.
    let (st, out) = server.req("GET", "/v1/orgs/acme/repos/app/refs", &tok, None);
    assert_eq!(st, 401, "an expired token must stop reading: {out}");

    // Dead on the git door too — the one a runner actually clones with,
    // and a different code path from the REST guard.
    let out = std::process::Command::new("git")
        .args(["ls-remote", &clone_url])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git ls-remote");
    assert!(
        !out.status.success(),
        "an expired token cloned the repository: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Nothing swept it. The refusal is the reader's, which is the whole
    // property: a credential that is only dead once a worker gets round
    // to it is alive for an unbounded time.
    let (st, listed) = server.req("GET", "/v1/orgs/acme/tokens", &admin, None);
    assert_eq!(st, 200, "{listed}");
    assert!(
        listed["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == minted["id"]),
        "the row is gone, so something swept it and the test proves less \
         than it claims: {listed}"
    );

    // A token minted with no deadline is unaffected — every existing
    // credential, and every personal access token.
    let (st, forever) = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({ "scopes": ["repo:read"], "repo": "app" })),
    );
    assert_eq!(st, 201, "{forever}");
    assert!(forever["expires_at"].is_null(), "{forever}");
    let (st, _) = server.req(
        "GET",
        "/v1/orgs/acme/repos/app/refs",
        forever["token"].as_str().unwrap(),
        None,
    );
    assert_eq!(st, 200, "an undated token still works");

    assert!(server.healthy());
}
