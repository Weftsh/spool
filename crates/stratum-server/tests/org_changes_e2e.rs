//! The org-wide open-changes list: one request that answers "what could
//! I compose?" across every repository the caller may read.
//!
//! Everything here is asserted the way the dashboard's changeset picker
//! finds out — by asking the API with a credential — and the point of
//! most of it is that this list and the per-repo list never disagree
//! about who may see what.

use stratum_testkit::{gitcli::Scratch, Minio, Server};

const ORG_CHANGES: &str = "/v1/orgs/acme/changes";

fn spawn_server(store_url: &str, scratch: &Scratch) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .data_dir(scratch.path().join("data"))
        .db_hint("org-changes-e2e")
        .start()
}

fn create_repo(server: &Server, admin: &str, name: &str, public: bool) {
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        admin,
        Some(serde_json::json!({"name": name, "public": public})),
    );
    assert_eq!(st, 201, "create repo {name}: {out}");
}

fn commit(server: &Server, token: &str, repo: &str, branch: &str, message: &str, path: &str) {
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/commits"),
        token,
        Some(serde_json::json!({
            "branch": branch,
            "message": message,
            "operations": [{"op": "put", "path": path, "content": message}],
        })),
    );
    assert_eq!(st, 201, "commit to {repo}/{branch}: {out}");
}

/// A repository with trunk, a feature branch, and one open change.
fn repo_with_change(server: &Server, admin: &str, repo: &str, public: bool, key: &str) {
    create_repo(server, admin, repo, public);
    commit(server, admin, repo, "main", "base", "README.md");
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/branches"),
        admin,
        Some(serde_json::json!({"name": "feature", "from": "main"})),
    );
    assert_eq!(st, 201, "branch feature in {repo}: {out}");
    commit(
        server,
        admin,
        repo,
        "feature",
        &format!("change {repo}\n\nChange-Id: {key}\n"),
        "feature.txt",
    );
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/changes"),
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "open change in {repo}: {out}");
    assert_eq!(out["change"]["key"], serde_json::json!(key), "{out}");
}

/// `repo/key` for every row of a list response, in the order returned.
fn labels(out: &serde_json::Value) -> Vec<String> {
    out["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("changes array in {out}"))
        .iter()
        .map(|c| {
            format!(
                "{}/{}",
                c["repo"].as_str().unwrap_or_else(|| panic!("repo in {c}")),
                c["key"].as_str().unwrap()
            )
        })
        .collect()
}

/// The `changeset` field of one row, by `repo/key`.
fn changeset_of(out: &serde_json::Value, label: &str) -> serde_json::Value {
    out["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| {
            format!(
                "{}/{}",
                c["repo"].as_str().unwrap(),
                c["key"].as_str().unwrap()
            ) == label
        })
        .unwrap_or_else(|| panic!("no row {label} in {out}"))["changeset"]
        .clone()
}

#[test]
fn one_request_answers_what_this_caller_could_compose() {
    let minio = Minio::shared();
    let bucket = minio.bucket("org-changes-list");
    let scratch = Scratch::new("org-changes-list");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    repo_with_change(&server, &admin, "api", false, "Iaa000001");
    repo_with_change(&server, &admin, "web", false, "Ibb000002");

    // Newest first, across repositories, each row naming its repository
    // and saying it is free to be composed.
    let (st, out) = server.get(ORG_CHANGES, &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        labels(&out),
        vec!["web/Ibb000002", "api/Iaa000001"],
        "{out}"
    );
    for c in out["changes"].as_array().unwrap() {
        assert_eq!(c["changeset"], serde_json::Value::Null, "{c}");
        // The row is a whole change, not a stub: the picker shows the
        // title and the tip it would compose.
        assert!(c["title"].as_str().is_some(), "{c}");
        assert!(c["patchset"]["commit"].as_str().is_some(), "{c}");
    }

    // A change that is spoken for says which changeset holds it, and
    // only that change does.
    let (st, cs) = server.post(
        "/v1/orgs/acme/changesets",
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000001", "title": "compose",
            "members": [{"repo": "api", "change": "Iaa000001"}],
        })),
    );
    assert_eq!(st, 201, "{cs}");
    let (_, out) = server.get(ORG_CHANGES, &admin);
    assert_eq!(
        changeset_of(&out, "api/Iaa000001"),
        serde_json::json!("Ic5000001"),
        "{out}"
    );
    assert_eq!(
        changeset_of(&out, "web/Ibb000002"),
        serde_json::Value::Null,
        "{out}"
    );

    // And the field means exactly what the composer means by it:
    // abandoning the changeset releases the member, so the picker offers
    // it again — which is right, because `POST members` would now take
    // it rather than answering 409.
    let (st, out) = server.post(
        "/v1/orgs/acme/changesets/Ic5000001/abandon",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 204, "{out}");
    let (_, out) = server.get(ORG_CHANGES, &admin);
    assert_eq!(
        changeset_of(&out, "api/Iaa000001"),
        serde_json::Value::Null,
        "{out}"
    );

    // The single change says the same thing, and it is the field that
    // keeps a Land button off a change that cannot land alone: a client
    // should not have to press it and read the 409 to find out.
    let one = "/v1/orgs/acme/repos/api/changes/Iaa000001";
    let (st, out) = server.get(one, &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["change"]["changeset"], serde_json::Value::Null, "{out}");
    let (st, cs) = server.post(
        "/v1/orgs/acme/changesets",
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000002", "title": "again",
            "members": [{"repo": "api", "change": "Iaa000001"}],
        })),
    );
    assert_eq!(st, 201, "{cs}");
    let (_, out) = server.get(one, &admin);
    assert_eq!(
        out["change"]["changeset"],
        serde_json::json!("Ic5000002"),
        "{out}"
    );
    // And it agrees with the refusal, which is the whole point of the
    // field: the same binding, said before the attempt instead of after.
    let (st, out) = server.post(&format!("{one}/land"), &admin, Some(serde_json::json!({})));
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("Ic5000002"),
        "the refusal names the changeset the field named: {out}"
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/changesets/Ic5000002/abandon",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 204, "{out}");
    let (_, out) = server.get(one, &admin);
    assert_eq!(out["change"]["changeset"], serde_json::Value::Null, "{out}");

    // The state filter is the per-repo list's vocabulary, and it filters.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/web/changes/Ibb000002/abandon",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 204, "{out}");
    let (_, out) = server.get(&format!("{ORG_CHANGES}?state=open"), &admin);
    assert_eq!(labels(&out), vec!["api/Iaa000001"], "{out}");
    let (_, out) = server.get(&format!("{ORG_CHANGES}?state=abandoned"), &admin);
    assert_eq!(labels(&out), vec!["web/Ibb000002"], "{out}");
    for known in ["landing", "landed"] {
        let (st, out) = server.get(&format!("{ORG_CHANGES}?state={known}"), &admin);
        assert_eq!(st, 200, "{known}: {out}");
        assert!(out["changes"].as_array().unwrap().is_empty(), "{out}");
    }
    // An unknown state is the caller's mistake, said in their words.
    let (st, out) = server.get(&format!("{ORG_CHANGES}?state=bogus"), &admin);
    assert_eq!(st, 400, "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("unknown state"),
        "{out}"
    );

    // The limit clamps rather than refusing, and the page is the newest.
    let (_, out) = server.get(&format!("{ORG_CHANGES}?limit=1"), &admin);
    assert_eq!(labels(&out), vec!["web/Ibb000002"], "{out}");
    for junk in ["0", "-3", "not a number"] {
        let (st, out) = server.get(&format!("{ORG_CHANGES}?limit={junk}"), &admin);
        assert_eq!(st, 200, "limit={junk}: {out}");
    }
}

#[test]
fn the_org_wide_list_shows_exactly_what_the_per_repo_list_would() {
    let minio = Minio::shared();
    let bucket = minio.bucket("org-changes-acl");
    let scratch = Scratch::new("org-changes-acl");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    repo_with_change(&server, &admin, "api", false, "Iaa000001");
    repo_with_change(&server, &admin, "open", true, "Ibb000002");

    // A token bound to one repository sees that repository's changes and
    // no others — and it could never have passed an `org:read` gate, so
    // this is the case that decides how the route is authorized.
    let (st, minted) = server.post(
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({"scopes": ["repo:read"], "repo": "api", "label": "picker"})),
    );
    assert_eq!(st, 201, "{minted}");
    let scoped = minted["token"].as_str().unwrap().to_string();
    let (st, out) = server.get(ORG_CHANGES, &scoped);
    assert_eq!(st, 200, "{out}");
    assert_eq!(labels(&out), vec!["api/Iaa000001"], "{out}");
    // Each row says whether this caller could land what it composes —
    // the repository's own `viewer_write`, so the picker greys out a
    // change a reader could tick but never land. A read-only token may
    // read the row and write nothing; the admin may.
    assert_eq!(
        out["changes"][0]["viewer_write"],
        serde_json::json!(false),
        "{out}"
    );
    let (_, out) = server.get(ORG_CHANGES, &admin);
    for c in out["changes"].as_array().unwrap() {
        assert_eq!(c["viewer_write"], serde_json::json!(true), "{c}");
    }

    // Anonymous: the public repository's changes, and nothing private —
    // exactly what `GET /repos/{repo}/changes` answers one at a time.
    let (st, out) = server.get(ORG_CHANGES, "");
    assert_eq!(st, 200, "{out}");
    assert_eq!(labels(&out), vec!["open/Ibb000002"], "{out}");
    assert_eq!(
        out["changes"][0]["viewer_write"],
        serde_json::json!(false),
        "{out}"
    );
    let (st, _) = server.get("/v1/orgs/acme/repos/open/changes", "");
    assert_eq!(st, 200);
    let (st, _) = server.get("/v1/orgs/acme/repos/api/changes", "");
    assert_eq!(st, 401, "a private repo asks anonymous callers to identify");

    // Another organization's token reads what anyone does — the public
    // repository's changes, as nobody — because a public repository reads
    // with any valid credential, on the REST API as on the wire. This
    // used to be a masked 404 that hid nothing the anonymous page did not
    // already show, and disagreed with `GET …/changesets`, which never
    // refused it.
    let rival = server.bootstrap_org("rival");
    let (st, out) = server.get(ORG_CHANGES, &rival);
    assert_eq!(st, 200, "{out}");
    assert_eq!(labels(&out), vec!["open/Ibb000002"], "{out}");
    assert_eq!(
        out["changes"][0]["viewer_write"],
        serde_json::json!(false),
        "{out}"
    );
    // A token that does not resolve at all is a 401, never a 404: the
    // caller has to be told their credential is the problem.
    let (st, _) = server.get(ORG_CHANGES, "weft_nope_nope");
    assert_eq!(st, 401);

    // An organization that does not exist is a 404 whoever asks.
    let (st, _) = server.get("/v1/orgs/ghost/changes", &admin);
    assert_eq!(st, 404);
}
