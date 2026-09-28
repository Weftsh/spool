//! Polyrepo changesets end to end against a real server: composing open
//! changes from several repositories into one unit, the landing order
//! its edges imply, who may see or shape one, and what a bound change
//! may no longer do on its own. Everything is asserted the way a person
//! would find out — by asking the API with a credential — never by
//! reading tables.

use stratum_testkit::adversarial::{percent_encode, INJECTIONS};
use stratum_testkit::faultproxy::{Fault, FaultPlan, FaultRule};
use stratum_testkit::{gitcli::Scratch, FaultProxy, Minio, Server};

const PASSWORD: &str = "a long enough password";

fn spawn_server(store_url: &str, scratch: &Scratch) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .data_dir(scratch.path().join("data"))
        .db_hint("changesets-e2e")
        .start()
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

/// A request carrying a session cookie rather than a bearer token.
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

fn create_repo(server: &Server, admin: &str, name: &str) {
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        admin,
        Some(serde_json::json!({"name": name})),
    );
    assert_eq!(st, 201, "create repo {name}: {out}");
}

/// Commit files to a branch through the commit API.
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

/// A repository with trunk, a feature branch based on it, and one open
/// change `key` registered from that branch.
fn repo_with_change(server: &Server, admin: &str, repo: &str, key: &str) {
    create_repo(server, admin, repo);
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

fn member(repo: &str, change: &str) -> serde_json::Value {
    serde_json::json!({"repo": repo, "change": change})
}

fn edge(from: (&str, &str), to: (&str, &str)) -> serde_json::Value {
    serde_json::json!({"from": member(from.0, from.1), "to": member(to.0, to.1)})
}

/// The `repo/key` labels of a changeset's landing order, in order.
fn order(cs: &serde_json::Value) -> Vec<String> {
    cs["order"]
        .as_array()
        .unwrap_or_else(|| panic!("order in {cs}"))
        .iter()
        .map(|m| {
            format!(
                "{}/{}",
                m["repo"].as_str().unwrap(),
                m["change"].as_str().unwrap()
            )
        })
        .collect()
}

fn member_labels(cs: &serde_json::Value) -> Vec<String> {
    cs["members"]
        .as_array()
        .unwrap_or_else(|| panic!("members in {cs}"))
        .iter()
        .map(|m| {
            format!(
                "{}/{}",
                m["repo"].as_str().unwrap(),
                m["change"]["key"].as_str().unwrap()
            )
        })
        .collect()
}

const CS: &str = "/v1/orgs/acme/changesets";

#[test]
fn a_changeset_composes_changes_across_repositories_and_orders_them() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changesets-compose");
    let scratch = Scratch::new("changesets-compose");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    for (repo, key) in [
        ("api", "Iaa000001"),
        ("web", "Ibb000002"),
        ("cli", "Icc000003"),
        ("docs", "Idd000004"),
    ] {
        repo_with_change(&server, &admin, repo, key);
    }

    // Refusals that need no repository at all are decided before any
    // member is looked at.
    for (body, why) in [
        (
            serde_json::json!({"key": "Ic5000001", "title": "t", "members": []}),
            "no members",
        ),
        (
            serde_json::json!({"key": "Ic5000001", "title": "t"}),
            "members absent",
        ),
    ] {
        let (st, out) = server.post(CS, &admin, Some(body));
        assert!(st == 400 || st == 422, "{why}: {st} {out}");
    }
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "not a key!", "title": "t",
            "members": [member("api", "Iaa000001")],
        })),
    );
    assert_eq!(st, 400, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("invalid changeset key"),
        "{out}"
    );
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000001", "title": "   ",
            "members": [member("api", "Iaa000001")],
        })),
    );
    assert_eq!(st, 400, "{out}");
    assert!(out["error"].as_str().unwrap().contains("title"), "{out}");

    // A member that is not a change is a 404 that names it.
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000001", "title": "t",
            "members": [member("api", "Iaa000001"), member("web", "Inope0001")],
        })),
    );
    assert_eq!(st, 404, "{out}");
    assert_eq!(out["error"], serde_json::json!("no change web/Inope0001"));

    // Two members from one repository cannot be one changeset.
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000001", "title": "t",
            "members": [member("api", "Iaa000001"), member("api", "Iaa000001")],
        })),
    );
    assert_eq!(st, 400, "{out}");
    assert!(out["error"].as_str().unwrap().contains("twice"), "{out}");

    // Compose api + web with web landing first.
    let (st, cs) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000001",
            "title": "Rename the customer field",
            "body": "web first, then api",
            "members": [member("api", "Iaa000001"), member("web", "Ibb000002")],
            "edges": [edge(("web", "Ibb000002"), ("api", "Iaa000001"))],
        })),
    );
    assert_eq!(st, 201, "{cs}");
    assert_eq!(cs["key"], serde_json::json!("Ic5000001"));
    assert_eq!(cs["state"], serde_json::json!("open"));
    assert_eq!(cs["title"], serde_json::json!("Rename the customer field"));
    assert_eq!(cs["body"], serde_json::json!("web first, then api"));
    assert_eq!(member_labels(&cs), ["api/Iaa000001", "web/Ibb000002"]);
    assert_eq!(order(&cs), ["web/Ibb000002", "api/Iaa000001"]);
    // Each member carries the change as the change API shows it.
    assert_eq!(
        cs["members"][0]["change"]["state"],
        serde_json::json!("open")
    );
    assert_eq!(
        cs["members"][0]["change"]["title"],
        serde_json::json!("change api")
    );
    assert_eq!(cs["edges"].as_array().unwrap().len(), 1);
    assert_eq!(cs["edges"][0]["from"], member("web", "Ibb000002"));

    // The same key again is a conflict, even with a free member.
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000001", "title": "again",
            "members": [member("cli", "Icc000003")],
        })),
    );
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("already exists"),
        "{out}"
    );

    // A change belongs to at most one open changeset.
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000002", "title": "steal api",
            "members": [member("api", "Iaa000001"), member("cli", "Icc000003")],
        })),
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"],
        serde_json::json!("api/Iaa000001 is already in changeset Ic5000001")
    );
    // ...and the refused attempt wrote nothing: cli is still free.
    let (st, out) = server.get(&format!("{CS}?state=open"), &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["changesets"].as_array().unwrap().len(), 1, "{out}");

    // Read it back, list it, and filter by state.
    let (st, got) = server.get(&format!("{CS}/Ic5000001"), &admin);
    assert_eq!(st, 200, "{got}");
    assert_eq!(got, cs);
    let (st, out) = server.get(&format!("{CS}?state=landed"), &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["changesets"], serde_json::json!([]));
    let (st, out) = server.get(&format!("{CS}?state=bogus"), &admin);
    assert_eq!(st, 400, "{out}");
    let (st, out) = server.get(&format!("{CS}/Ic5000009"), &admin);
    assert_eq!(st, 404, "{out}");
    assert_eq!(
        out["error"],
        serde_json::json!("no changeset \"Ic5000009\"")
    );

    // Bring cli in; it joins at the end with no edges.
    let (st, cs) = server.post(
        &format!("{CS}/Ic5000001/members"),
        &admin,
        Some(member("cli", "Icc000003")),
    );
    assert_eq!(st, 201, "{cs}");
    assert_eq!(
        member_labels(&cs),
        ["api/Iaa000001", "web/Ibb000002", "cli/Icc000003"]
    );
    assert_eq!(
        order(&cs),
        ["web/Ibb000002", "api/Iaa000001", "cli/Icc000003"]
    );
    let (st, out) = server.post(
        &format!("{CS}/Ic5000001/members"),
        &admin,
        Some(member("cli", "Icc000003")),
    );
    assert_eq!(st, 409, "adding a member twice: {out}");

    // Edges are replaced wholesale and the order follows them.
    let (st, cs) = server.req(
        "PUT",
        &format!("{CS}/Ic5000001/edges"),
        &admin,
        Some(serde_json::json!({"edges": [
            edge(("cli", "Icc000003"), ("web", "Ibb000002")),
            edge(("web", "Ibb000002"), ("api", "Iaa000001")),
        ]})),
    );
    assert_eq!(st, 200, "{cs}");
    assert_eq!(
        order(&cs),
        ["cli/Icc000003", "web/Ibb000002", "api/Iaa000001"]
    );
    // A cycle, an edge to a non-member and a self-edge are each refused
    // by name, and the previous edges stand.
    for (edges, expect) in [
        (
            vec![
                edge(("cli", "Icc000003"), ("web", "Ibb000002")),
                edge(("web", "Ibb000002"), ("cli", "Icc000003")),
            ],
            "cycle",
        ),
        (
            vec![edge(("docs", "Idd000004"), ("api", "Iaa000001"))],
            "docs/Idd000004 is not a member",
        ),
        (
            vec![edge(("api", "Iaa000001"), ("api", "Iaa000001"))],
            "before itself",
        ),
    ] {
        let (st, out) = server.req(
            "PUT",
            &format!("{CS}/Ic5000001/edges"),
            &admin,
            Some(serde_json::json!({"edges": edges})),
        );
        assert_eq!(st, 400, "{out}");
        assert!(out["error"].as_str().unwrap().contains(expect), "{out}");
    }
    let (st, cs) = server.get(&format!("{CS}/Ic5000001"), &admin);
    assert_eq!(st, 200);
    assert_eq!(cs["edges"].as_array().unwrap().len(), 2, "{cs}");

    // A member lands and is abandoned with the changeset, not alone.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/api/changes/Iaa000001/land",
        &admin,
        None,
    );
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("member of changeset Ic5000001"),
        "{out}"
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/api/changes/Iaa000001/abandon",
        &admin,
        None,
    );
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("member of changeset Ic5000001"),
        "{out}"
    );
    let (st, out) = server.get("/v1/orgs/acme/repos/api/changes/Iaa000001", &admin);
    assert_eq!(st, 200);
    assert_eq!(out["change"]["state"], serde_json::json!("open"), "{out}");

    // Removing a member takes its edges with it.
    let (st, out) = server.req(
        "DELETE",
        &format!("{CS}/Ic5000001/members/cli/Icc000003"),
        &admin,
        None,
    );
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.req(
        "DELETE",
        &format!("{CS}/Ic5000001/members/cli/Icc000003"),
        &admin,
        None,
    );
    assert_eq!(st, 404, "{out}");
    assert_eq!(
        out["error"],
        serde_json::json!("cli/Icc000003 is not a member of changeset Ic5000001")
    );
    let (st, cs) = server.get(&format!("{CS}/Ic5000001"), &admin);
    assert_eq!(st, 200);
    assert_eq!(member_labels(&cs), ["api/Iaa000001", "web/Ibb000002"]);
    assert_eq!(cs["edges"].as_array().unwrap().len(), 1, "{cs}");
    assert_eq!(order(&cs), ["web/Ibb000002", "api/Iaa000001"]);
    // ...and the released change may be abandoned on its own again.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/cli/changes/Icc000003/abandon",
        &admin,
        None,
    );
    assert_eq!(st, 204, "{out}");
    // An abandoned change cannot be brought back in.
    let (st, out) = server.post(
        &format!("{CS}/Ic5000001/members"),
        &admin,
        Some(member("cli", "Icc000003")),
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"],
        serde_json::json!("cli/Icc000003 is abandoned")
    );
    // The last member cannot be removed: abandon the changeset instead.
    let (st, out) = server.req(
        "DELETE",
        &format!("{CS}/Ic5000001/members/web/Ibb000002"),
        &admin,
        None,
    );
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.req(
        "DELETE",
        &format!("{CS}/Ic5000001/members/api/Iaa000001"),
        &admin,
        None,
    );
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("abandon it instead"),
        "{out}"
    );

    // Abandoning releases the members; the record stays, closed.
    let (st, out) = server.post(&format!("{CS}/Ic5000001/abandon"), &admin, None);
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.post(&format!("{CS}/Ic5000001/abandon"), &admin, None);
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["error"], serde_json::json!("changeset is abandoned"));
    let (st, cs) = server.get(&format!("{CS}/Ic5000001"), &admin);
    assert_eq!(st, 200);
    assert_eq!(cs["state"], serde_json::json!("abandoned"));
    assert_eq!(member_labels(&cs), ["api/Iaa000001"]);
    for (method, path, body) in [
        ("POST", "members", Some(member("web", "Ibb000002"))),
        ("PUT", "edges", Some(serde_json::json!({"edges": []}))),
        ("DELETE", "members/api/Iaa000001", None),
    ] {
        let (st, out) = server.req(method, &format!("{CS}/Ic5000001/{path}"), &admin, body);
        assert_eq!(st, 409, "{method} {path} on an abandoned changeset: {out}");
        assert_eq!(out["error"], serde_json::json!("changeset is abandoned"));
    }
    let (st, out) = server.get(&format!("{CS}?state=abandoned"), &admin);
    assert_eq!(st, 200);
    assert_eq!(out["changesets"][0]["key"], serde_json::json!("Ic5000001"));
    let (st, out) = server.get(&format!("{CS}?state=open"), &admin);
    assert_eq!(st, 200);
    assert_eq!(out["changesets"], serde_json::json!([]));
    // A released member is free to be a member again, and to land alone.
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000002", "title": "second try",
            "members": [member("api", "Iaa000001"), member("docs", "Idd000004")],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/web/changes/Ibb000002/abandon",
        &admin,
        None,
    );
    assert_eq!(st, 204, "{out}");

    // A member that does not exist cannot be added, and a changeset that
    // does not exist has no members to remove.
    let (st, out) = server.post(
        &format!("{CS}/Ic5000002/members"),
        &admin,
        Some(member("web", "Inope0001")),
    );
    assert_eq!(st, 404, "{out}");
    assert_eq!(out["error"], serde_json::json!("no change web/Inope0001"));
    let (st, out) = server.req(
        "DELETE",
        &format!("{CS}/Ic5000009/members/api/Iaa000001"),
        &admin,
        None,
    );
    assert_eq!(st, 404, "{out}");
    assert_eq!(
        out["error"],
        serde_json::json!("no changeset \"Ic5000009\"")
    );

    // An org that does not exist answers 404 at every changeset door.
    for (method, path, body) in [
        (
            "POST",
            "",
            Some(
                serde_json::json!({"key": "Ic5", "title": "t", "members": [member("api", "Iaa000001")]}),
            ),
        ),
        ("GET", "", None),
        ("GET", "/Ic5000001", None),
        (
            "POST",
            "/Ic5000001/members",
            Some(member("api", "Iaa000001")),
        ),
        ("DELETE", "/Ic5000001/members/api/Iaa000001", None),
        (
            "PUT",
            "/Ic5000001/edges",
            Some(serde_json::json!({"edges": []})),
        ),
        ("POST", "/Ic5000001/abandon", None),
    ] {
        let (st, out) = server.req(
            method,
            &format!("/v1/orgs/nobody/changesets{path}"),
            &admin,
            body,
        );
        assert_eq!(st, 404, "{method} {path} on a missing org: {out}");
    }

    // Hostile keys in the path are refused, not executed, and the server
    // is still serving afterwards.
    for inj in INJECTIONS {
        let path = format!("{CS}/{}", percent_encode(inj));
        let (st, out) = server.get(&path, &admin);
        assert!(st == 404 || st == 400, "{path} answered {st}: {out}");
        let (st, out) = server.post(
            CS,
            &admin,
            Some(serde_json::json!({
                "key": inj, "title": inj,
                "members": [member(inj, inj)],
            })),
        );
        assert!(
            st == 400 || st == 404,
            "create with {inj:?} answered {st}: {out}"
        );
    }
    assert!(server.healthy());
}

#[test]
fn a_changeset_is_visible_only_to_someone_who_can_read_every_member() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changesets-visible");
    let scratch = Scratch::new("changesets-visible");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    make_user(&server, "vic@acme.test", "Vic", "viewer");
    make_user(&server, "dev@acme.test", "Dev", "member");
    repo_with_change(&server, &admin, "openbook", "I0b000001");
    repo_with_change(&server, &admin, "openapi", "I0a000002");
    repo_with_change(&server, &admin, "vault", "Iee000003");

    // Composing needs write on every member; a viewer's attempt is masked
    // the way any write to a repository they cannot write is.
    let vic = sign_in(&server, "vic@acme.test");
    let (st, out) = as_person(
        &server,
        &vic,
        "POST",
        CS,
        Some(serde_json::json!({
            "key": "Ic5000001", "title": "the pair",
            "members": [member("openbook", "I0b000001"), member("openapi", "I0a000002")],
        })),
    );
    assert_eq!(st, 404, "a viewer composing: {out}");

    // A member composes, and the record carries who did it.
    let dev = sign_in(&server, "dev@acme.test");
    let (st, out) = as_person(
        &server,
        &dev,
        "POST",
        CS,
        Some(serde_json::json!({
            "key": "Ic5000001", "title": "the pair",
            "members": [member("openbook", "I0b000001"), member("openapi", "I0a000002")],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = as_person(
        &server,
        &dev,
        "POST",
        CS,
        Some(serde_json::json!({
            "key": "Ic5000002", "title": "with the vault",
            "members": [member("vault", "Iee000003")],
        })),
    );
    assert_eq!(st, 201, "{out}");

    // Every repository is private to its organisation, so a changeset
    // is too. No credential is a 401 whether or not the key exists; a
    // token from another organisation meets the same 404 an unknown key
    // does, on reads and writes alike.
    let keys_of = |out: &serde_json::Value| -> Vec<String> {
        out["changesets"]
            .as_array()
            .unwrap_or_else(|| panic!("changesets in {out}"))
            .iter()
            .map(|c| c["key"].as_str().unwrap().to_string())
            .collect()
    };
    for key in ["Ic5000001", "Ic5000002", "Ic5999999"] {
        let (st, out) = server.get(&format!("{CS}/{key}"), "");
        assert_eq!(st, 401, "anonymous reading {key}: {out}");
    }
    let (st, out) = server.get(CS, "");
    assert_eq!(st, 401, "anonymous listing: {out}");
    let rival = server.bootstrap_org("rival");
    for key in ["Ic5000001", "Ic5000002"] {
        let (st, _) = server.get(&format!("{CS}/{key}"), &rival);
        assert_eq!(st, 404, "a rival org reading {key}");
        let (st, _) = server.post(&format!("{CS}/{key}/abandon"), &rival, None);
        assert_eq!(st, 404, "a rival org abandoning {key}");
    }
    let (st, _) = server.req(
        "PUT",
        &format!("{CS}/Ic5000001/edges"),
        &rival,
        Some(serde_json::json!({"edges": []})),
    );
    assert_eq!(st, 404, "a rival org setting edges");

    // Inside the organisation, a credential that can read only *one*
    // member does not see the combination: a token bound to `openbook`
    // reads the changeset over the vault as absent, and the pair as
    // absent too, because the pair names a repository it cannot read.
    let (st, minted) = server.post(
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({"scopes": ["repo:read"], "repo": "openbook", "label": "one"})),
    );
    assert_eq!(st, 201, "{minted}");
    let one = minted["token"].as_str().unwrap().to_string();
    for key in ["Ic5000001", "Ic5000002"] {
        let (st, out) = server.get(&format!("{CS}/{key}"), &one);
        assert_eq!(st, 404, "a one-repository token reading {key}: {out}");
    }

    // The viewer, inside the org, sees both — newest first.
    let (st, out) = as_person(&server, &vic, "GET", CS, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(keys_of(&out), ["Ic5000002", "Ic5000001"]);

    // Every changeset says whether *this* reader may land, revert,
    // abandon or edit it — write on every member — so a page never
    // offers a button the write routes will answer with a masked 404.
    // The viewer sees both and may act on neither; the composer may.
    let writes = |out: &serde_json::Value| -> Vec<bool> {
        out["changesets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["viewer_write"].as_bool().expect("viewer_write is a bool"))
            .collect()
    };
    assert_eq!(writes(&out), [false, false], "a viewer may write nothing");
    let (_, out) = as_person(&server, &vic, "GET", &format!("{CS}/Ic5000001"), None);
    assert_eq!(out["viewer_write"], serde_json::json!(false), "{out}");
    let (_, out) = as_person(&server, &dev, "GET", &format!("{CS}/Ic5000001"), None);
    assert_eq!(out["viewer_write"], serde_json::json!(true), "{out}");
    // It is the repository's answer, not the org role's: a per-repo grant
    // that raises the viewer to a writer on *one* member is not enough,
    // and on both it is — at which point the route the field speaks for
    // agrees. A dashboard reading the org role instead would have hidden
    // the controls from somebody entitled to them.
    let (st, roster) = server.get("/v1/orgs/acme/members", &admin);
    assert_eq!(st, 200, "{roster}");
    let vic_id = roster["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["email"] == "vic@acme.test")
        .expect("vic on the roster")["user_id"]
        .clone();
    let grant = |repo: &str| {
        let (st, out) = server.post(
            &format!("/v1/orgs/acme/repos/{repo}/grants"),
            &admin,
            Some(serde_json::json!({"user_id": vic_id, "role": "member"})),
        );
        assert_eq!(st, 204, "grant {repo}: {out}");
    };
    grant("openbook");
    let (_, out) = as_person(&server, &vic, "GET", &format!("{CS}/Ic5000001"), None);
    assert_eq!(
        out["viewer_write"],
        serde_json::json!(false),
        "one member of two: {out}"
    );
    let (st, _) = as_person(
        &server,
        &vic,
        "PUT",
        &format!("{CS}/Ic5000001/edges"),
        Some(serde_json::json!({"edges": []})),
    );
    assert_eq!(st, 404, "and the write route agrees");
    grant("openapi");
    let (_, out) = as_person(&server, &vic, "GET", &format!("{CS}/Ic5000001"), None);
    assert_eq!(
        out["viewer_write"],
        serde_json::json!(true),
        "both members: {out}"
    );
    let (st, out) = as_person(
        &server,
        &vic,
        "PUT",
        &format!("{CS}/Ic5000001/edges"),
        Some(serde_json::json!({"edges": []})),
    );
    assert_eq!(st, 200, "and the write route agrees: {out}");
    // Still nothing on the vault's, which the grants did not touch.
    let (_, out) = as_person(&server, &vic, "GET", &format!("{CS}/Ic5000002"), None);
    assert_eq!(out["viewer_write"], serde_json::json!(false), "{out}");
    // Unknown org: masked like every other org surface.
    let (st, _) = server.get("/v1/orgs/nobody/changesets", &admin);
    assert_eq!(st, 404);
    assert!(server.healthy());
}

/// Like `repo_with_change`, but trunk carries a root OWNERS file naming
/// `owner`, so the change needs that person's approval to land.
fn owned_repo_with_change(server: &Server, admin: &str, repo: &str, owner: &str, key: &str) {
    create_repo(server, admin, repo);
    commit(server, admin, repo, "main", &format!("{owner}\n"), "OWNERS");
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
}

fn approve(server: &Server, email: &str, repo: &str, key: &str) {
    let cookie = sign_in(server, email);
    let (st, out) = as_person(
        server,
        &cookie,
        "POST",
        &format!("/v1/orgs/acme/repos/{repo}/changes/{key}/approve"),
        None,
    );
    assert_eq!(st, 204, "{email} approving {repo}/{key}: {out}");
}

fn post_check(server: &Server, admin: &str, repo: &str, key: &str, state: &str) {
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/changes/{key}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/tests", "state": state})),
    );
    assert!(
        st == 201 || st == 200,
        "check {state} on {repo}/{key}: {st} {out}"
    );
}

/// The changeset verdict is every member's own review at once: each
/// member's sufficiency verdict and check gate exactly as its own
/// endpoints give them, in landing order, and one answer over the whole
/// that names the first member standing in the way. Approval blocks
/// before checks do; a failing required check refuses; an unreported
/// one only waits, and the changeset says what it is waiting on.
#[test]
fn a_changeset_verdict_is_every_member_review_at_once() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changesets-verdict");
    let scratch = Scratch::new("changesets-verdict");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    make_user(&server, "vic@acme.test", "Vic", "viewer");

    owned_repo_with_change(&server, &admin, "api", "oa@acme.test", "Iaa000001");
    owned_repo_with_change(&server, &admin, "web", "ow@acme.test", "Ibb000002");
    // web's trunk requires a check; api's does not.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/web/protections",
        &admin,
        Some(serde_json::json!({"branch": "main"})),
    );
    assert!(st == 201 || st == 200, "protect web/main: {st} {out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/web/required-checks/main",
        &admin,
        Some(serde_json::json!({"name": "ci/tests"})),
    );
    assert_eq!(st, 201, "require ci/tests on web/main: {out}");

    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000001",
            "title": "verdict",
            "members": [member("web", "Ibb000002"), member("api", "Iaa000001")],
            "edges": [edge(("api", "Iaa000001"), ("web", "Ibb000002"))],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let verdict = || {
        let (st, out) = server.get(&format!("{CS}/Ic5000001/verdict"), &admin);
        assert_eq!(st, 200, "{out}");
        out
    };
    let str_of = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();

    // Nothing approved, nothing reported: the first member in landing
    // order — api, by the edge, though web joined first — is the blocker,
    // and its explanation is the per-change one with the member's name
    // in front. The gate is "waiting" for web's unreported check, and
    // that is reported separately from landability.
    let v = verdict();
    assert_eq!(v["changeset"], "Ic5000001");
    assert_eq!(v["state"], "open");
    assert_eq!(v["landable"], false, "{v}");
    assert_eq!(v["gate"], "waiting", "{v}");
    let expl = str_of(&v["explanation"]);
    assert!(expl.starts_with("api/Iaa000001: "), "{v}");
    assert!(expl.contains("oa@acme.test"), "{v}");
    assert_eq!(
        v["waiting_on"],
        serde_json::json!(["web/Ibb000002: ci/tests"]),
        "{v}"
    );
    let members = v["members"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["repo"], "api");
    assert_eq!(members[1]["repo"], "web");
    assert_eq!(members[0]["change"], "Iaa000001");
    assert_eq!(members[0]["state"], "open");
    assert_eq!(members[0]["patchset"], 1);
    assert!(members[0]["commit"].as_str().unwrap().len() == 40, "{v}");
    assert_eq!(members[0]["landable"], false);
    assert_eq!(members[0]["gate"], "ready", "api requires no check: {v}");
    assert_eq!(members[0]["waiting_on"], serde_json::json!([]));
    assert!(members[0]["reason"].is_null());
    assert_eq!(members[0]["approvals"], serde_json::json!([]));
    assert_eq!(members[0]["verdict"]["landable"], false);
    assert_eq!(
        members[0]["explanation"], members[0]["verdict"]["explanation"],
        "an unapproved open member's explanation is its own verdict's: {v}"
    );
    assert_eq!(
        members[0]["verdict"]["per_path"][0]["path"], "feature.txt",
        "{v}"
    );
    assert_eq!(members[1]["gate"], "waiting", "{v}");
    assert_eq!(members[1]["waiting_on"], serde_json::json!(["ci/tests"]));
    // The per-change endpoint and the member's row agree, word for word.
    let (st, own) = server.get("/v1/orgs/acme/repos/api/changes/Iaa000001/verdict", &admin);
    assert_eq!(st, 200, "{own}");
    assert_eq!(members[0]["verdict"], own["verdict"], "{v}\n{own}");

    // api's owner approves: the blocker moves to web, and api's row
    // carries the approval.
    approve(&server, "oa@acme.test", "api", "Iaa000001");
    let v = verdict();
    assert_eq!(v["landable"], false, "{v}");
    let expl = str_of(&v["explanation"]);
    assert!(expl.starts_with("web/Ibb000002: "), "{v}");
    assert!(expl.contains("ow@acme.test"), "{v}");
    assert_eq!(v["members"][0]["landable"], true, "{v}");
    assert_eq!(
        v["members"][0]["approvals"][0]["email"], "oa@acme.test",
        "{v}"
    );
    assert_eq!(v["members"][1]["landable"], false, "{v}");

    // web's owner approves: every member is approved; the changeset is
    // landable in the sense the button means, and says what it waits on.
    approve(&server, "ow@acme.test", "web", "Ibb000002");
    let v = verdict();
    assert_eq!(v["landable"], true, "{v}");
    assert_eq!(v["gate"], "waiting", "{v}");
    assert_eq!(
        v["explanation"], "ok: all 2 member(s) approved; waiting on 1 check(s)",
        "{v}"
    );
    assert_eq!(v["members"][1]["landable"], true, "{v}");
    assert!(
        str_of(&v["members"][1]["explanation"]).ends_with("; waiting on ci/tests"),
        "{v}"
    );

    // The check fails: refused, in the check's words, at web.
    post_check(&server, &admin, "web", "Ibb000002", "failing");
    let v = verdict();
    assert_eq!(v["landable"], false, "{v}");
    assert_eq!(v["gate"], "blocked", "{v}");
    assert_eq!(
        v["explanation"], "web/Ibb000002: blocked: required check 'ci/tests' is failing",
        "{v}"
    );
    assert_eq!(v["waiting_on"], serde_json::json!([]), "{v}");
    assert_eq!(v["members"][1]["gate"], "blocked");
    assert_eq!(
        v["members"][1]["reason"], "required check 'ci/tests' is failing",
        "{v}"
    );
    assert_eq!(v["members"][0]["landable"], true, "api is unaffected: {v}");

    // Green: ready, with nothing outstanding.
    post_check(&server, &admin, "web", "Ibb000002", "passing");
    let v = verdict();
    assert_eq!(v["landable"], true, "{v}");
    assert_eq!(v["gate"], "ready", "{v}");
    assert_eq!(v["explanation"], "ok: all 2 member(s) approved", "{v}");
    assert!(
        v["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["landable"] == true),
        "{v}"
    );

    // Who may read it: the same rule as the changeset itself. Anonymous
    // is told to sign in, for this key and for one that does not exist;
    // a stranger meets the sentence an unknown key gets; a viewer of the
    // org sees everything.
    for key in ["Ic5000001", "Ic5009999"] {
        let (st, out) = server.get(&format!("{CS}/{key}/verdict"), "");
        assert_eq!(st, 401, "anonymous reading {key}'s verdict: {out}");
    }
    let rival = server.bootstrap_org("rival");
    let (st, out) = server.get(&format!("{CS}/Ic5000001/verdict"), &rival);
    assert_eq!(st, 404, "{out}");
    assert_eq!(
        out["error"],
        serde_json::json!("no changeset \"Ic5000001\""),
        "a stranger was told the changeset exists: {out}"
    );
    let vic = sign_in(&server, "vic@acme.test");
    let (st, out) = as_person(
        &server,
        &vic,
        "GET",
        &format!("{CS}/Ic5000001/verdict"),
        None,
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["landable"], true, "{out}");
    let (st, out) = server.get(&format!("{CS}/Ic5000009/verdict"), &admin);
    assert_eq!(st, 404, "{out}");
    assert_eq!(out["error"], "no changeset \"Ic5000009\"", "{out}");
    let (st, out) = server.get("/v1/orgs/nobody/changesets/Ic5000001/verdict", &admin);
    assert_eq!(st, 404, "{out}");
    for inj in INJECTIONS {
        let (st, out) = server.get(&format!("{CS}/{}/verdict", percent_encode(inj)), &admin);
        assert!(st == 400 || st == 404, "{inj:?}: {st} {out}");
    }

    // Abandoned: the members' answers stand, the changeset's does not.
    let (st, out) = server.post(&format!("{CS}/Ic5000001/abandon"), &admin, None);
    assert_eq!(st, 204, "{out}");
    let v = verdict();
    assert_eq!(v["state"], "abandoned");
    assert_eq!(v["landable"], false, "{v}");
    assert_eq!(v["explanation"], "changeset is abandoned", "{v}");
    assert_eq!(v["members"].as_array().unwrap().len(), 2, "{v}");
    assert_eq!(v["members"][0]["landable"], true, "{v}");
    assert!(server.healthy());
}

/// A repository whose trunk holds `before` at `path` and whose one open
/// change replaces it with `after` — so the diffstat has numbers a
/// reader can check by hand.
fn repo_with_edit(
    server: &Server,
    admin: &str,
    repo: &str,
    key: &str,
    // One tuple rather than three neighbouring `&str` arguments: `path`,
    // `before` and `after` are the same type, and two of them swapped
    // would still compile and would still produce a diffstat — just the
    // wrong one.
    edit: (&str, &str, &str),
) {
    let (path, before, after) = edit;
    create_repo(server, admin, repo);
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/commits"),
        admin,
        Some(serde_json::json!({
            "branch": "main", "message": "base",
            "operations": [{"op": "put", "path": path, "content": before}],
        })),
    );
    assert_eq!(st, 201, "base commit in {repo}: {out}");
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/branches"),
        admin,
        Some(serde_json::json!({"name": "feature", "from": "main"})),
    );
    assert_eq!(st, 201, "branch feature in {repo}: {out}");
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/commits"),
        admin,
        Some(serde_json::json!({
            "branch": "feature",
            "message": format!("edit {path}\n\nChange-Id: {key}\n"),
            "operations": [{"op": "put", "path": path, "content": after}],
        })),
    );
    assert_eq!(st, 201, "feature commit in {repo}: {out}");
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/changes"),
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "open change in {repo}: {out}");
    assert_eq!(out["change"]["key"], serde_json::json!(key), "{out}");
}

/// A changeset can say how big it is — and says so **per member**,
/// because "one review over four repositories" is the thing a changeset
/// is: +12 in the API and +900 in the generated client is a different
/// review from +450 in each, and one total says the same thing about
/// both.
///
/// The truncated member is the half worth testing hardest. A file this
/// server declines to read has no honest line count, and the wrong
/// answer here is a plausible number nobody can tell is wrong — so it
/// reports `truncated` and leaves the counts to the files it did read,
/// with `files` still exact because that comes from the tree walk.
#[test]
fn a_changeset_says_how_big_it_is_and_admits_what_it_did_not_count() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changesets-diffstat");
    let scratch = Scratch::new("changesets-diffstat");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    // Three lines in, one of them rewritten and one appended: +2 −1, by
    // hand, on a diff small enough to argue about.
    repo_with_edit(
        &server,
        &admin,
        "api",
        "Iaa000001",
        ("core.rs", "a\nb\nc\n", "a\nB\nc\nd\n"),
    );
    // Two members that cannot be counted, for the two *different*
    // reasons — kept apart on purpose, because either one alone would
    // hide a hole in the other. A single member that is both enormous
    // and wholly rewritten reports `truncated` even if the size limit is
    // deleted, so the test would pass against a server that had lost it.
    //
    // One 600 KiB line: past the 512 KiB the diff view itself refuses to
    // fetch as text, and yet a one-line-against-one-line diff that costs
    // almost nothing to match.
    let one_huge_line = format!("{}\n", "z".repeat(600_000));
    repo_with_edit(
        &server,
        &admin,
        "web",
        "Ibb000002",
        ("generated.txt", "small\n", &one_huge_line),
    );
    // Small enough to read, and rewritten so completely that lining the
    // two versions up runs past the request's work bound.
    let before: String = (0..3000).map(|i| format!("old-{i}\n")).collect();
    let after: String = (0..3000).map(|i| format!("new-{i}\n")).collect();
    repo_with_edit(
        &server,
        &admin,
        "churn",
        "Icc000003",
        ("churn.txt", &before, &after),
    );
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000001", "title": "one review, two sizes",
            "members": [
                member("api", "Iaa000001"),
                member("web", "Ibb000002"),
                member("churn", "Icc000003"),
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");

    let path = format!("{CS}/Ic5000001/diffstat");
    let (st, out) = server.get(&path, &admin);
    assert_eq!(st, 200, "{out}");
    let members = out["members"].as_array().expect("members");
    assert_eq!(members.len(), 3, "{out}");
    assert_eq!(
        members[0],
        serde_json::json!({
            "repo": "api", "change": "Iaa000001", "patchset": 1,
            "files": 1, "insertions": 2, "deletions": 1, "truncated": false,
        }),
        "one line replaced is one in and one out, and the appended line is \
         the second insertion: {out}"
    );
    assert_eq!(
        members[1],
        serde_json::json!({
            "repo": "web", "change": "Ibb000002", "patchset": 1,
            "files": 1, "insertions": 0, "deletions": 0, "truncated": true,
        }),
        "a file too large to read is reported as uncounted, never as a \
         number nobody can check: {out}"
    );
    assert_eq!(
        members[2],
        serde_json::json!({
            "repo": "churn", "change": "Icc000003", "patchset": 1,
            "files": 1, "insertions": 0, "deletions": 0, "truncated": true,
        }),
        "a rewrite past the work bound is refused rather than estimated: {out}"
    );
    // The total is the sum, and `truncated` on it means *some* member's
    // is — the only reading that cannot overstate what was counted.
    assert_eq!(
        out["total"],
        serde_json::json!({
            "files": 3, "insertions": 2, "deletions": 1, "truncated": true,
        }),
        "{out}"
    );

    // Authority is the changeset's own: read on **every** member, and a
    // member you cannot see makes the whole thing not exist for you —
    // the size of a review over a private repository is a fact about
    // that repository.
    let rival = server.bootstrap_org("rival");
    let (st, out) = server.get(&path, &rival);
    assert_eq!(st, 404, "a rival org read the member's size: {out}");
    assert_eq!(
        out["error"],
        serde_json::json!("no changeset \"Ic5000001\""),
        "a rival org was told the changeset exists: {out}"
    );
    // Anonymous is told to sign in before anything is looked up, so the
    // answer is the same for a key that does not exist.
    for key in ["Ic5000001", "Ic5009999"] {
        let (st, out) = server.get(&format!("{CS}/{key}/diffstat"), "");
        assert_eq!(st, 401, "anonymous reading {key}'s size: {out}");
    }
    // A key that is not a changeset is the same sentence, so neither
    // answer tells the other apart.
    let (st, out) = server.get(&format!("{CS}/Ic5009999/diffstat"), &admin);
    assert_eq!(st, 404, "{out}");
    // ...and an organization that is not one is refused before the
    // changeset is looked for at all, which is what keeps a size read
    // from being a way to ask whether an org exists.
    let (st, out) = server.get("/v1/orgs/nobody/changesets/Ic5000001/diffstat", &admin);
    assert_eq!(st, 404, "{out}");
    for inj in INJECTIONS {
        let enc = percent_encode(inj);
        let st = server.status_get(&format!("{CS}/{enc}/diffstat"), Some(&admin));
        assert!(st == 400 || st == 404, "{inj:?} answered {st}");
    }
    assert!(server.healthy());
}

/// A bucket reached through a fault proxy, so a store outage can be
/// arranged under one request and lifted under the next.
fn proxied(hint: &str) -> (FaultProxy, Scratch) {
    let minio = Minio::shared();
    let bucket = minio.bucket(hint);
    let upstream = bucket
        .base_url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let proxy = FaultProxy::start(&upstream);
    let bucket_name = bucket.base_url.rsplit('/').next().unwrap().to_string();
    (
        FaultProxy {
            url: format!("{}/{bucket_name}", proxy.url),
            handle: proxy.handle,
        },
        Scratch::new(hint),
    )
}

/// A store that will not answer makes the size unknown, and unknown is
/// said as a server error rather than as a number.
///
/// The counts come from a walk of the members' trees, so a store outage
/// is the one failure this endpoint cannot compute around — and the
/// wrong answer is the plausible one: `+0 −0`, or a total that silently
/// leaves out the member that could not be read, both of which a reader
/// would take for the size of the review. `truncated` is a member's
/// honesty flag about a file it *did* reach, not a way to report a
/// missing store.
///
/// The recovery half is what makes it an outage rather than a verdict:
/// the same request, once the store answers again, is a 200 with the
/// numbers, from a server nothing restarted.
#[test]
fn a_store_that_will_not_answer_makes_the_diffstat_a_server_error_not_a_number() {
    let (proxy, scratch) = proxied("changesets-diffstat-store");
    let server = spawn_server(&proxy.url, &scratch);
    let admin = server.bootstrap_org("acme");
    repo_with_edit(
        &server,
        &admin,
        "api",
        "Iaa000001",
        ("core.rs", "a\nb\nc\n", "a\nB\nc\nd\n"),
    );
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic500000d", "title": "how big is it, really",
            "members": [member("api", "Iaa000001")],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let path = format!("{CS}/Ic500000d/diffstat");

    // Every read of the store is refused. `ErrorAfter` answers 503 to
    // each attempt, so the store client's own retries are spent too.
    proxy.handle.set_plan(
        FaultPlan::new(7).with(FaultRule::new(Fault::ErrorAfter, 1.0).only_methods(["GET"])),
    );
    let (st, out) = server.get(&path, &admin);
    assert!(
        st >= 500,
        "a store outage was reported as a size, got {st}: {out}"
    );
    assert!(
        out["total"].is_null() && out["members"].is_null(),
        "a request that could not read the store answered with counts: {out}"
    );
    assert!(
        proxy.handle.stats().count(Fault::ErrorAfter) > 0,
        "the store was never actually refused, so nothing was under test"
    );

    // The same request, once the store is back.
    proxy.handle.clear_plan();
    let (st, out) = server.get(&path, &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        out["total"],
        serde_json::json!({
            "files": 1, "insertions": 2, "deletions": 1, "truncated": false,
        }),
        "{out}"
    );
    assert!(server.healthy());
}

/// A file that arrived, a file that left, and an entry there is no
/// content to count.
///
/// The three shapes the ordinary diffstat test cannot produce, and each
/// has a plausible wrong answer. An added file has nothing on the left
/// and a deleted one has nothing on the right: read as "no lines to
/// compare" rather than as "an empty side", both come out `+0 −0` — so a
/// change that adds a 300-line file would report a review of nothing. A
/// **gitlink** has no blob at all: its oid names a commit in another
/// repository, and counting it as content would either report `+1` for
/// the oid or fail the whole request with a missing-object error that
/// looks exactly like a store outage.
///
/// So this asserts the numbers on both, and asserts them *per member*:
/// the member that could be counted is counted exactly and is not
/// truncated, and the member that could not says so and claims no
/// number. A `truncated` that leaked onto the countable member would
/// make the honest flag meaningless.
#[test]
fn a_diffstat_counts_a_file_that_arrived_or_left_and_declines_a_submodule() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changesets-diffstat-edges");
    let scratch = Scratch::new("changesets-diffstat-edges");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");

    // One member through the API: `added.txt` arrives, `gone.txt`
    // leaves, and `keep.txt` is untouched so the file count cannot come
    // from the tree's size.
    create_repo(&server, &admin, "api");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/api/commits",
        &admin,
        Some(serde_json::json!({
            "branch": "main", "message": "base",
            "operations": [
                {"op": "put", "path": "keep.txt", "content": "k\n"},
                {"op": "put", "path": "gone.txt", "content": "x\ny\nz\n"},
            ],
        })),
    );
    assert_eq!(st, 201, "base commit: {out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/api/branches",
        &admin,
        Some(serde_json::json!({"name": "feature", "from": "main"})),
    );
    assert_eq!(st, 201, "branch: {out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/api/commits",
        &admin,
        Some(serde_json::json!({
            "branch": "feature",
            "message": "one in, one out\n\nChange-Id: Iaa000001\n",
            "operations": [
                {"op": "put", "path": "added.txt", "content": "1\n2\n3\n"},
                {"op": "delete", "path": "gone.txt"},
            ],
        })),
    );
    assert_eq!(st, 201, "feature commit: {out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/api/changes",
        &admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "open change in api: {out}");

    // The other member with the real git CLI, because the commit API
    // only ever writes plain files and a gitlink is the thing under
    // test. Written straight into the index: `git submodule add` would
    // need a second repository on disk and a fetch, and the entry is
    // what matters, not the porcelain.
    create_repo(&server, &admin, "vend");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/vend/commits",
        &admin,
        Some(serde_json::json!({
            "branch": "main", "message": "base",
            "operations": [{"op": "put", "path": "readme", "content": "hello\n"}],
        })),
    );
    assert_eq!(st, 201, "base commit in vend: {out}");
    let url = server.authed_url(&admin, "acme", "vend");
    let work = scratch.path().join("vend-work");
    stratum_testkit::gitcli::clone_and_fsck(&url, &work);
    stratum_testkit::gitcli::git(&work, &["checkout", "-q", "-b", "feature"]);
    stratum_testkit::gitcli::git(
        &work,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},vendor/lib", "1".repeat(40)),
        ],
    );
    stratum_testkit::gitcli::git(
        &work,
        &[
            "commit",
            "-q",
            "-m",
            "vendor the client\n\nChange-Id: Ibb000002\n",
        ],
    );
    stratum_testkit::gitcli::git(&work, &["push", "-q", "origin", "feature"]);
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/vend/changes",
        &admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "open change in vend: {out}");

    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic500000e", "title": "what a size can and cannot be",
            "members": [member("api", "Iaa000001"), member("vend", "Ibb000002")],
        })),
    );
    assert_eq!(st, 201, "{out}");

    let (st, out) = server.get(&format!("{CS}/Ic500000e/diffstat"), &admin);
    assert_eq!(st, 200, "{out}");
    let members = out["members"].as_array().expect("members");
    assert_eq!(members.len(), 2, "{out}");
    assert_eq!(
        members[0],
        serde_json::json!({
            "repo": "api", "change": "Iaa000001", "patchset": 1,
            "files": 2, "insertions": 3, "deletions": 3, "truncated": false,
        }),
        "an arriving file is its whole length in, a departing one its \
         whole length out, and neither is a file this server declined \
         to read: {out}"
    );
    assert_eq!(
        members[1],
        serde_json::json!({
            "repo": "vend", "change": "Ibb000002", "patchset": 1,
            "files": 1, "insertions": 0, "deletions": 0, "truncated": true,
        }),
        "a submodule pointer is a commit id in another repository, not a \
         line of content: it is reported as uncounted, never as +1: {out}"
    );
    assert_eq!(
        out["total"],
        serde_json::json!({
            "files": 3, "insertions": 3, "deletions": 3, "truncated": true,
        }),
        "{out}"
    );

    assert!(server.healthy());
}
