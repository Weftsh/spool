//! Review ergonomics end to end: per-file viewed marks (and, above all,
//! what a new patchset does to them), and author associations.
//!
//! Everything is asserted the way a reviewer would find out — by asking
//! the API with a credential — never by reading tables.

use std::time::Duration;
use stratum_testkit::browser::Browser;
use stratum_testkit::gitcli::Scratch;
use stratum_testkit::mailbox::Mailbox;
use stratum_testkit::{Minio, Server};

const PASSWORD: &str = "a long enough password";

fn spawn_server(store_url: &str, scratch: &Scratch) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_LAND_POLL_SECS", "1")
        .db_hint("change-views-e2e")
        .start()
}

fn make_user(server: &Server, admin: &str, email: &str, name: &str, role: &str) -> String {
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

fn commit(
    server: &Server,
    token: &str,
    branch_name: &str,
    message: &str,
    files: &[(&str, &str)],
) -> String {
    let ops: Vec<serde_json::Value> = files
        .iter()
        .map(|(p, c)| serde_json::json!({"op": "put", "path": p, "content": c}))
        .collect();
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        token,
        Some(serde_json::json!({
            "branch": branch_name,
            "message": message,
            "operations": ops,
        })),
    );
    assert_eq!(st, 201, "commit to {branch_name}: {out}");
    out["commit"].as_str().unwrap().to_string()
}

fn branch(server: &Server, token: &str, name: &str, from: &str) {
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/branches",
        token,
        Some(serde_json::json!({"name": name, "from": from})),
    );
    assert_eq!(st, 201, "branch {name} from {from}: {out}");
}

/// Register the feature tip as a change (or as the next patchset of one).
fn register(server: &Server, token: &str) {
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        token,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "register change: {out}");
}

/// Sign somebody up the way a stranger arrives: an address, a mailed
/// link, and a personal namespace of their own. Deliberately not
/// `admin user-create`, which makes org members — the whole point of the
/// fork path is somebody who is *not* one.
fn signup<'a>(server: &'a Server, mail: &Mailbox, handle: &str, email: &str) -> Browser<'a> {
    let (st, body) = server.req(
        "POST",
        "/v1/auth/signup",
        "",
        Some(serde_json::json!({
            "handle": handle,
            "email": email,
            "name": handle,
            "password": PASSWORD,
        })),
    );
    assert_eq!(st, 202, "signup {handle}: {body}");
    let msg = mail.wait_for(email, Duration::from_secs(10));
    let link = msg.link().unwrap_or_else(|| panic!("no link in {msg:?}"));
    let token = link
        .split_once("#verify=")
        .unwrap_or_else(|| panic!("{link} carries no #verify="))
        .1
        .to_string();
    let mut b = Browser::new(server);
    let (st, body) = b.req(
        "POST",
        "/v1/auth/verify",
        Some(serde_json::json!({ "token": token })),
    );
    assert_eq!(st, 200, "verify {handle}: {body}");
    b
}

/// Wait for the fork worker, then report the state it reached.
fn await_fork(browser: &mut Browser, path: &str) -> String {
    for _ in 0..100 {
        let (st, body) = browser.req("GET", path, None);
        assert_eq!(st, 200, "{body}");
        match body["fork_state"].as_str() {
            Some("pending") | None => std::thread::sleep(Duration::from_millis(100)),
            Some(other) => return other.to_string(),
        }
    }
    panic!("fork never left pending");
}

struct World {
    server: Server,
    admin: String,
}

/// An org with an owner, two members, and one repository. No OWNERS file
/// anywhere, so any writer's approval suffices — this suite is about
/// viewed marks and badges, not about sufficiency.
fn world(server: Server) -> World {
    let admin = server.bootstrap_org("acme");
    make_user(&server, &admin, "olive@acme.test", "Olive", "owner");
    make_user(&server, &admin, "alice@acme.test", "Alice", "member");
    make_user(&server, &admin, "dev@acme.test", "Dev", "member");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    assert_eq!(st, 201, "{out}");
    World { server, admin }
}

/// The viewed paths this cookie's owner holds on the change, and the
/// patchset they apply to.
fn views(server: &Server, cookie: &str) -> (serde_json::Value, Vec<String>) {
    let (st, out) = as_person(
        server,
        cookie,
        "GET",
        "/v1/orgs/acme/repos/app/changes/I0000d00d/views",
        None,
    );
    assert_eq!(st, 200, "{out}");
    let paths = out["viewed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    (out["patchset"].clone(), paths)
}

fn set_viewed(server: &Server, cookie: &str, path: &str, viewed: bool) -> (u16, serde_json::Value) {
    as_person(
        server,
        cookie,
        "PUT",
        "/v1/orgs/acme/repos/app/changes/I0000d00d/views",
        Some(serde_json::json!({"path": path, "viewed": viewed})),
    )
}

/// The property the whole feature rests on.
///
/// A file marked viewed at patchset 1 must be *unviewed* at patchset 2 if
/// patchset 2 changed it — a tick that survives a revision tells the
/// reviewer they have read code they have never seen, which is worse than
/// having no tick at all. And the other half, which is what makes the
/// feature usable rather than merely safe: a file the revision did not
/// touch keeps its tick, because the reviewer really has read what is
/// there.
#[test]
fn a_new_patchset_unviews_the_files_it_changed_and_only_those() {
    let minio = Minio::shared();
    let bucket = minio.bucket("views-patchset");
    let scratch = Scratch::new("views-patchset");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let alice = sign_in(server, "alice@acme.test");

    let base = commit(
        server,
        admin,
        "main",
        "base",
        &[("a.txt", "a1"), ("b.txt", "b1"), ("c.txt", "c1")],
    );
    branch(server, admin, "feature", &base);
    commit(
        server,
        admin,
        "feature",
        "work\n\nChange-Id: I0000d00d\n",
        &[("a.txt", "a2"), ("b.txt", "b2")],
    );
    register(server, admin);

    // Read all three and tick them.
    for path in ["a.txt", "b.txt", "c.txt"] {
        let (st, out) = set_viewed(server, &alice, path, true);
        assert_eq!(st, 204, "{out}");
    }
    let (patchset, paths) = views(server, &alice);
    assert_eq!(patchset, serde_json::json!(1));
    assert_eq!(paths, vec!["a.txt", "b.txt", "c.txt"]);

    // Patchset 2 rewrites a.txt, leaves b.txt and c.txt exactly as they
    // were.
    commit(
        server,
        admin,
        "feature",
        "revision\n\nChange-Id: I0000d00d\n",
        &[("a.txt", "a3")],
    );
    register(server, admin);

    let (patchset, paths) = views(server, &alice);
    assert_eq!(
        patchset,
        serde_json::json!(2),
        "the change is on patchset 2"
    );
    assert_eq!(
        paths,
        vec!["b.txt", "c.txt"],
        "a.txt moved under the reviewer and must come back unviewed; \
         b.txt and c.txt did not and must not"
    );

    // Re-reading a.txt at the new patchset ticks it again, and this time
    // the mark is against the patchset that is actually there.
    let (st, out) = set_viewed(server, &alice, "a.txt", true);
    assert_eq!(st, 204, "{out}");
    let (_, paths) = views(server, &alice);
    assert_eq!(paths, vec!["a.txt", "b.txt", "c.txt"]);

    // Unticking is the reviewer changing their mind, and it sticks.
    let (st, out) = set_viewed(server, &alice, "b.txt", false);
    assert_eq!(st, 204, "{out}");
    let (_, paths) = views(server, &alice);
    assert_eq!(paths, vec!["a.txt", "c.txt"]);
}

/// A viewed mark is the viewer's own and nobody else's, and there is no
/// request shape that could ask for somebody else's.
///
/// Ends by proving the server is still healthy and serving: a server that
/// survives an attack by refusing everything has not passed.
#[test]
fn viewed_marks_are_private_to_the_person_who_made_them() {
    let minio = Minio::shared();
    let bucket = minio.bucket("views-private");
    let scratch = Scratch::new("views-private");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let stranger_admin = server.bootstrap_org("other");
    let alice = sign_in(server, "alice@acme.test");
    let dev = sign_in(server, "dev@acme.test");

    let base = commit(
        server,
        admin,
        "main",
        "base",
        &[("a.txt", "a1"), ("b.txt", "b1")],
    );
    branch(server, admin, "feature", &base);
    commit(
        server,
        admin,
        "feature",
        "work\n\nChange-Id: I0000d00d\n",
        &[("a.txt", "a2")],
    );
    register(server, admin);

    assert_eq!(set_viewed(server, &alice, "a.txt", true).0, 204);
    assert_eq!(set_viewed(server, &dev, "b.txt", true).0, 204);

    // Each reviewer sees their own ticks, and only those. Alice ticking
    // a.txt does not tick it for Dev — being told a colleague read it is
    // not the same as having read it.
    assert_eq!(views(server, &alice).1, vec!["a.txt"]);
    assert_eq!(views(server, &dev).1, vec!["b.txt"]);

    // A service token has no reviewer behind it, so it is refused rather
    // than handed a shared set of ticks.
    let (st, out) = server.get("/v1/orgs/acme/repos/app/changes/I0000d00d/views", admin);
    assert_eq!(st, 403, "{out}");
    assert_eq!(
        out["error"],
        serde_json::json!("viewed state belongs to a person, not a service token")
    );
    let (st, out) = server.req(
        "PUT",
        "/v1/orgs/acme/repos/app/changes/I0000d00d/views",
        admin,
        Some(serde_json::json!({"path": "a.txt", "viewed": true})),
    );
    assert_eq!(st, 403, "{out}");

    // A credential from another org is masked, not merely refused: it
    // must not learn that this repository or this change exists.
    for path in [
        "/v1/orgs/acme/repos/app/changes/I0000d00d/views",
        "/v1/orgs/acme/repos/app/changes/I0000d00d/associations",
    ] {
        let (st, out) = server.get(path, &stranger_admin);
        assert_eq!(st, 404, "{path}: {out}");
    }

    // No credential at all, on a private repo: 401, and nothing leaked.
    assert_eq!(
        server.status_get("/v1/orgs/acme/repos/app/changes/I0000d00d/views", None),
        401
    );

    // Hostile shapes are refused in words at the door, not stored.
    for bad in ["../etc/passwd", "/a.txt", "a\0.txt", ""] {
        let (st, out) = set_viewed(server, &alice, bad, true);
        assert_eq!(st, 400, "{bad:?}: {out}");
    }
    // An unknown change is absent, whoever asks.
    let (st, out) = as_person(
        server,
        &alice,
        "GET",
        "/v1/orgs/acme/repos/app/changes/Ideadbeef/views",
        None,
    );
    assert_eq!(st, 404, "{out}");

    // Still healthy, still serving, and none of the above disturbed a
    // single stored mark.
    assert!(server.healthy(), "the server is still up");
    assert_eq!(views(server, &alice).1, vec!["a.txt"]);
    assert_eq!(views(server, &dev).1, vec!["b.txt"]);
}

/// Author associations: who is speaking, weighed by their standing here.
///
/// Owner and member come from the effective-role resolver; contributor
/// and first-time come from whether this person has landed anything on
/// this repository before.
#[test]
fn associations_say_how_to_weigh_each_voice() {
    let minio = Minio::shared();
    let bucket = minio.bucket("views-assoc");
    let scratch = Scratch::new("views-assoc");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let olive_id = make_user(server, admin, "olive@acme.test", "Olive", "owner");
    let dev_id = make_user(server, admin, "dev@acme.test", "Dev", "member");
    let olive = sign_in(server, "olive@acme.test");
    let alice = sign_in(server, "alice@acme.test");
    let dev = sign_in(server, "dev@acme.test");

    let base = commit(server, admin, "main", "base", &[("a.txt", "a1")]);
    branch(server, admin, "feature", &base);
    commit(
        server,
        admin,
        "feature",
        "work\n\nChange-Id: I0000d00d\n",
        &[("a.txt", "a2")],
    );
    // Dev registers the change, so dev is its author.
    let (st, out) = as_person(
        server,
        &dev,
        "POST",
        "/v1/orgs/acme/repos/app/changes",
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");

    // The org owner and a plain member both say something.
    for (cookie, body) in [(&olive, "ship it"), (&alice, "one question")] {
        let (st, out) = as_person(
            server,
            cookie,
            "POST",
            "/v1/orgs/acme/repos/app/changes/I0000d00d/comments",
            Some(serde_json::json!({"body": body})),
        );
        assert_eq!(st, 201, "{out}");
    }

    let assoc = |cookie: &str| -> serde_json::Value {
        let (st, out) = as_person(
            server,
            cookie,
            "GET",
            "/v1/orgs/acme/repos/app/changes/I0000d00d/associations",
            None,
        );
        assert_eq!(st, 200, "{out}");
        out
    };
    let out = assoc(&alice);
    assert_eq!(
        out["authors"][format!("user:{olive_id}")],
        serde_json::json!("owner")
    );
    assert_eq!(out["author"], serde_json::json!("member"), "{out}");
    assert_eq!(
        out["authors"][format!("user:{dev_id}")],
        serde_json::json!("member"),
        "the change's author is in the map too, not only in `author`"
    );
    // And the reader is told *which* principal that is, so the author's
    // own comments can be marked as theirs in a thread of strangers.
    assert_eq!(
        out["author_principal"],
        serde_json::json!(format!("user:{dev_id}")),
        "{out}"
    );

    // Land dev's change: alice's approval is enough with no OWNERS file.
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        "/v1/orgs/acme/repos/app/changes/I0000d00d/approve",
        None,
    );
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes/I0000d00d/land",
        admin,
        None,
    );
    assert_eq!(st, 202, "{out}");
    let mut landed = false;
    for _ in 0..150 {
        let (st, out) = server.get("/v1/orgs/acme/repos/app/changes/I0000d00d", admin);
        assert_eq!(st, 200, "{out}");
        if out["change"]["state"] == serde_json::json!("landed") {
            landed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(landed, "the change never landed");

    // Dev leaves the org. Their standing is now derived from what they
    // have landed here, which is the whole point of the distinction: a
    // former colleague with work in the tree is not a stranger.
    let (st, out) = server.delete(&format!("/v1/orgs/acme/members/{dev_id}"), admin);
    assert_eq!(st, 204, "{out}");
    let out = assoc(&olive);
    assert_eq!(out["author"], serde_json::json!("contributor"), "{out}");

    // And somebody with no role here and nothing landed here is a
    // first-time author — the case a reviewer most wants flagged, and the
    // one that separates "new" from "gone".
    let newcomer = make_user(server, admin, "new@acme.test", "New", "member");
    let new_cookie = sign_in(server, "new@acme.test");
    let (st, out) = as_person(
        server,
        &new_cookie,
        "POST",
        "/v1/orgs/acme/repos/app/changes/I0000d00d/comments",
        Some(serde_json::json!({"body": "my first review here"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.delete(&format!("/v1/orgs/acme/members/{newcomer}"), admin);
    assert_eq!(st, 204, "{out}");
    let out = assoc(&olive);
    assert_eq!(
        out["authors"][format!("user:{newcomer}")],
        serde_json::json!("first-time"),
        "nothing landed here, so nothing to weigh: {out}"
    );
    // Which is a different answer from dev's, on the same read — the two
    // halves of the rule, both constrained.
    assert_eq!(
        out["authors"][format!("user:{dev_id}")],
        serde_json::json!("contributor"),
        "{out}"
    );

    assert!(server.healthy());
}

/// A change whose commits live in a **fork**.
///
/// This is the population the badges and the viewed marks are actually
/// for: somebody with no push access at all, whose work a reviewer has
/// no prior reason to trust. It is also where the naive implementation
/// is quietly wrong — a fork's objects are in the fork's own prefix, so
/// a reader pointed at the target's plane finds neither patchset, and
/// this endpoint's fail-safe turns that into "nothing is viewed" on
/// every single load. The feature would look implemented and do nothing,
/// on the one path where a reviewer needs it most.
#[test]
fn viewed_marks_follow_a_change_that_came_from_a_fork() {
    let minio = Minio::shared();
    let bucket = minio.bucket("views-fork");
    let scratch = Scratch::new("views-fork");
    let mail = Mailbox::temp("views-fork");
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .data_dir(scratch.path().join("data"))
        .db_hint("change-views-fork");
    for (k, v) in mail.env() {
        b = b.env(k, v);
    }
    let server = b.env("STRATUM_FORK_POLL_SECS", "1").start();

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");

    // Ada's project, in an organisation — a personal namespace has no
    // members, and somebody else has to be able to read it to fork it —
    // with two files on trunk. Bob is a viewer: he may read, and he may
    // not push.
    let (st, body) = ada.req("POST", "/v1/orgs", Some(serde_json::json!({ "name": "acme" })));
    assert_eq!(st, 201, "{body}");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/acme/repos",
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{body}");
    ada.invite_and_accept("acme", "bob@example.com", "viewer");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/commits",
        Some(serde_json::json!({
            "message": "seed",
            "operations": [
                { "op": "put", "path": "a.txt", "content": "a1" },
                { "op": "put", "path": "b.txt", "content": "b1" },
            ],
        })),
    );
    assert_eq!(st, 201, "{body}");

    // Bob, who may not push here, forks it and does the work in his copy.
    let (st, body) = bob.req("POST", "/v1/orgs/acme/repos/app/forks", None);
    assert_eq!(st, 202, "{body}");
    assert_eq!(await_fork(&mut bob, "/v1/orgs/bob/repos/app"), "ready");
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/bob/repos/app/branches",
        Some(serde_json::json!({ "name": "feature", "from": "main" })),
    );
    assert_eq!(st, 201, "{body}");
    let commit_in_fork = |bob: &mut Browser, message: &str, ops: serde_json::Value| {
        let (st, body) = bob.req(
            "POST",
            "/v1/orgs/bob/repos/app/commits",
            Some(serde_json::json!({
                "branch": "feature",
                "message": message,
                "operations": ops,
            })),
        );
        assert_eq!(st, 201, "commit in the fork: {body}");
    };
    commit_in_fork(
        &mut bob,
        "work\n\nChange-Id: I0000f00d\n",
        serde_json::json!([
            { "op": "put", "path": "a.txt", "content": "a2" },
            { "op": "put", "path": "b.txt", "content": "b2" },
        ]),
    );
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/app/changes",
        Some(serde_json::json!({ "from": "feature", "source": "bob/app" })),
    );
    assert_eq!(st, 201, "open a change from the fork: {body}");

    // Ada reads both files and ticks them.
    let views = "/v1/orgs/acme/repos/app/changes/I0000f00d/views";
    for path in ["a.txt", "b.txt"] {
        let (st, body) = ada.req(
            "PUT",
            views,
            Some(serde_json::json!({ "path": path, "viewed": true })),
        );
        assert_eq!(st, 204, "{body}");
    }
    let (st, body) = ada.req("GET", views, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["viewed"], serde_json::json!(["a.txt", "b.txt"]));

    // Bob revises, touching a.txt only, and registers patchset 2.
    commit_in_fork(
        &mut bob,
        "revision\n\nChange-Id: I0000f00d\n",
        serde_json::json!([{ "op": "put", "path": "a.txt", "content": "a3" }]),
    );
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/app/changes",
        Some(serde_json::json!({ "from": "feature", "source": "bob/app" })),
    );
    assert_eq!(st, 201, "{body}");

    // The comparison has to happen in the fork's prefix. Read against
    // the target's, both patchsets are absent, the fail-safe drops every
    // mark, and this reads `[]`.
    let (st, body) = ada.req("GET", views, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["patchset"], serde_json::json!(2), "{body}");
    assert_eq!(
        body["viewed"],
        serde_json::json!(["b.txt"]),
        "b.txt was untouched by patchset 2 and must keep its mark; \
         a.txt moved and must not: {body}"
    );

    // The change is Bob's, and the badge says so. This used to read
    // `null`: `changes_api::create` took the author from the repository
    // principal, which `rest_repo_auth` left `None` for a signed-in person
    // with no role in the org — exactly the contributor the fork path
    // exists to serve. The FINDING sat in this test asserting "absent, not
    // invented" until the same seam refused Bob's ticks and his comments
    // too; a public repository's reader is now a principal with their own
    // id and `repo:read`, so every surface that reads identity off it
    // agrees about who he is.
    let (st, me) = bob.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    let bob_id = me["id"].as_str().expect("bob has an id").to_string();
    let (st, body) = ada.req(
        "GET",
        "/v1/orgs/acme/repos/app/changes/I0000f00d/associations",
        None,
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        body["author_principal"],
        serde_json::json!(format!("user:{bob_id}")),
        "{body}"
    );
    assert_eq!(
        body["author"],
        serde_json::json!("first-time"),
        "nothing of Bob's has landed here yet, which is the badge a \
         reviewer most wants to see: {body}"
    );

    // Bob reads his own change as himself. His ticks are his — Ada's
    // marks above are not disturbed, and the sentence that used to greet
    // him here ("viewed state belongs to a person, not a service token")
    // was about a credential he was not holding.
    let (st, body) = bob.req(
        "PUT",
        views,
        Some(serde_json::json!({ "path": "a.txt", "viewed": true })),
    );
    assert_eq!(st, 204, "{body}");
    let (st, body) = bob.req("GET", views, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["viewed"], serde_json::json!(["a.txt"]), "{body}");
    let (st, body) = ada.req("GET", views, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["viewed"], serde_json::json!(["b.txt"]), "{body}");

    // He joins the conversation, as himself — "sign in to comment" is for
    // somebody who has not.
    let comments = "/v1/orgs/acme/repos/app/changes/I0000f00d/comments";
    let (st, body) = bob.req(
        "POST",
        comments,
        Some(serde_json::json!({ "body": "revised a.txt as asked" })),
    );
    assert_eq!(st, 201, "{body}");
    assert_eq!(body["author"], serde_json::json!("bob"), "{body}");
    assert_eq!(
        body["author_principal"],
        serde_json::json!(format!("user:{bob_id}")),
        "{body}"
    );

    // His opinion of his own work is recorded like any reader's, and
    // counts for nothing: sufficiency reads write access, which he does
    // not have. The verdict still names Ada as the one who must approve.
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/app/changes/I0000f00d/approve",
        None,
    );
    assert_eq!(st, 204, "{body}");
    let (st, body) = ada.req(
        "GET",
        "/v1/orgs/acme/repos/app/changes/I0000f00d/verdict",
        None,
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        body["verdict"]["landable"],
        serde_json::json!(false),
        "an outsider's approval must not make a change landable: {body}"
    );

    // Somebody with no session at all is told to sign in, not that they
    // are a machine.
    let mut nobody = Browser::new(&server);
    let (st, body) = nobody.req("GET", views, None);
    assert_eq!(st, 401, "{body}");
    assert_eq!(
        body["error"],
        serde_json::json!("sign in to mark files as viewed")
    );
    let (st, body) = nobody.req(
        "POST",
        "/v1/orgs/acme/repos/app/changes/I0000f00d/approve",
        None,
    );
    assert_eq!(st, 401, "{body}");
    assert_eq!(body["error"], serde_json::json!("sign in to approve"));

    // The change is Bob's to withdraw. He holds no write access on Ada's
    // repository, and abandoning is otherwise a writer's action — but a
    // contributor who could open a change and never close it would be
    // left asking a maintainer to tidy up after them. A third person,
    // equally a stranger to the repository, gets the writer's refusal:
    // the change is not theirs, and the door does not say why.
    let abandon = "/v1/orgs/acme/repos/app/changes/I0000f00d/abandon";
    let mut cam = signup(&server, &mail, "cam", "cam@example.com");
    let (st, body) = cam.req("POST", abandon, None);
    assert_eq!(
        st, 404,
        "a stranger cannot abandon another's change: {body}"
    );
    let (st, body) = nobody.req("POST", abandon, None);
    assert_eq!(st, 401, "{body}");
    let (st, body) = bob.req("POST", abandon, None);
    assert_eq!(st, 204, "the author may withdraw their own change: {body}");
    let (st, body) = ada.req("GET", "/v1/orgs/acme/repos/app/changes/I0000f00d", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        body["change"]["state"],
        serde_json::json!("abandoned"),
        "{body}"
    );
    // Once, and only while it is open: the second attempt is the same
    // conflict a writer would meet, not a fresh refusal.
    let (st, body) = bob.req("POST", abandon, None);
    assert_eq!(st, 409, "{body}");

    assert!(server.healthy());
}
