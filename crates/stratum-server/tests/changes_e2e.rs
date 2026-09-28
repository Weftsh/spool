//! Monorepo review end to end against a real server: OWNERS resolution,
//! the sufficiency preview, and (in later stages) changes, approvals and
//! landing. Everything is asserted the way a person would find out — by
//! asking the API with a credential — never by reading tables.

use std::time::Duration;
use stratum_testkit::adversarial::{percent_encode, INJECTIONS};
use stratum_testkit::{gitcli, gitcli::Scratch, wait_for, wait_until, FaultProxy, Minio, Server};

const PASSWORD: &str = "a long enough password";

fn spawn_server(store_url: &str, scratch: &Scratch) -> Server {
    // A fifth of a second, not a whole one: every wait in this file is
    // on an observable the lander produces, so this knob is only the
    // *latency* of that observable and nothing waits on the clock.
    // Measured over the whole binary, 1s -> 0.2s took it from 21.1s to
    // 14.6s of wall for +1.6s of CPU; 0.1s bought no further wall and
    // cost another 2s of CPU, which on a two-core runner is the wrong
    // side of the trade.
    spawn_with(store_url, scratch, None, "0.2")
}

/// A server whose land wait budget and recheck interval are short enough
/// to watch, and the database it was pointed at.
///
/// Both budgets are minutes in production, which is the right scale and
/// an impossible one to assert against; the knobs exist partly so that
/// these can be real tests rather than `#[ignore]`d ones.
///
/// The URL comes back because [`assert_holding`] needs it: a *recheck*
/// of a held change has no API-visible transition (`set_land_waiting`
/// deliberately leaves `updated_at` alone), and the only place the
/// lander records that it looked again is the change's `land_job_id`.
fn spawn_holding(
    store_url: &str,
    scratch: &Scratch,
    wait_secs: &str,
    recheck_secs: &str,
) -> (Server, String) {
    let db_url = stratum_testkit::pg::test_db_url("changes-e2e");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_LAND_POLL_SECS", "0.2")
        .env("STRATUM_LAND_WAIT_SECS", wait_secs)
        .env("STRATUM_LAND_RECHECK_SECS", recheck_secs)
        .db_url(&db_url)
        .start();
    (server, db_url)
}

fn spawn_with(store_url: &str, scratch: &Scratch, db_url: Option<&str>, poll: &str) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_LAND_POLL_SECS", poll);
    b = match db_url {
        Some(u) => b.db_url(u),
        None => b.db_hint("changes-e2e"),
    };
    b.start()
}

/// Poll the change until it leaves `not_state` (the observable the next
/// interaction needs), with a deadline that names what never happened.
fn wait_until_not(server: &Server, admin: &str, key: &str, not_state: &str) -> serde_json::Value {
    let path = format!("/v1/orgs/acme/repos/app/changes/{key}");
    wait_for(
        &format!("change {key} to leave state {not_state:?}"),
        Duration::from_secs(30),
        || {
            let (st, out) = server.get(&path, admin);
            assert_eq!(st, 200, "{out}");
            (out["change"]["state"] != serde_json::json!(not_state)).then_some(out)
        },
    )
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

/// Commit files to a branch through the commit API; returns the commit oid.
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

/// Create a branch from a rev through the API, so feature work is based
/// on trunk the way real work is (a commit to an absent branch would be
/// a root commit with no OWNERS in its tree and no ancestry to land).
fn branch(server: &Server, token: &str, repo: &str, name: &str, from: &str) {
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/branches"),
        token,
        Some(serde_json::json!({"name": name, "from": from})),
    );
    assert_eq!(st, 201, "branch {name} from {from}: {out}");
}

struct World {
    server: Server,
    admin: String,
}

/// An org with people in every posture the rules distinguish: alice owns
/// payments (directly and through the team), casey owns the root, dev is
/// a plain writer, vic can only look.
fn world(server: Server) -> World {
    let admin = server.bootstrap_org("acme");
    let alice_id = make_user(&server, &admin, "alice@acme.test", "Alice", "member");
    make_user(&server, &admin, "casey@acme.test", "Casey", "member");
    make_user(&server, &admin, "dev@acme.test", "Dev", "member");
    make_user(&server, &admin, "vic@acme.test", "Vic", "viewer");
    let (st, team) = server.post(
        "/v1/orgs/acme/teams",
        &admin,
        Some(serde_json::json!({"name": "payments"})),
    );
    assert_eq!(st, 201, "{team}");
    let team_id = team["id"].as_str().unwrap();
    let (st, out) = server.req(
        "PUT",
        &format!("/v1/orgs/acme/teams/{team_id}/members/{alice_id}"),
        &admin,
        None,
    );
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    assert_eq!(st, 201, "{out}");
    World { server, admin }
}

#[test]
fn owners_resolution_walks_the_hierarchy_at_a_rev() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-owners");
    let scratch = Scratch::new("changes-owners");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);

    // Rev 1: only a root OWNERS.
    let c1 = commit(
        server,
        admin,
        "app",
        "main",
        "root owners",
        &[("OWNERS", "casey@acme.test\n"), ("README.md", "hi")],
    );
    // Rev 2: payments gets its own OWNERS; sealed/ cuts inheritance.
    let c2 = commit(
        server,
        admin,
        "app",
        "main",
        "payments owners",
        &[
            ("payments/OWNERS", "alice@acme.test\n@payments # the team\n"),
            ("payments/gateway.rs", "fn main() {}"),
            ("sealed/OWNERS", "casey@acme.test\nset noparent\n"),
            ("sealed/keys.rs", "k"),
        ],
    );

    // At rev 1 the path is governed by the root file alone.
    let (st, out) = server.get(
        &format!("/v1/orgs/acme/repos/app/owners?path=payments/gateway.rs&at={c1}"),
        admin,
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["at"], serde_json::json!(c1));
    let rules = out["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 1, "{out}");
    assert_eq!(rules[0]["dir"], "");
    assert_eq!(rules[0]["entries"], serde_json::json!(["casey@acme.test"]));

    // At HEAD the chain is payments then root, deepest first, and the
    // team resolves to its current roster.
    let (st, out) = server.get(
        "/v1/orgs/acme/repos/app/owners?path=payments/gateway.rs",
        admin,
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["at"], serde_json::json!(c2));
    let rules = out["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 2, "{out}");
    assert_eq!(rules[0]["dir"], "payments");
    assert_eq!(
        rules[0]["entries"],
        serde_json::json!(["alice@acme.test", "@payments"])
    );
    assert_eq!(rules[1]["dir"], "");
    let users: Vec<&str> = out["resolved"]["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["email"].as_str().unwrap())
        .collect();
    assert!(users.contains(&"alice@acme.test"), "{out}");
    assert!(users.contains(&"casey@acme.test"), "{out}");
    assert_eq!(
        out["resolved"]["teams"],
        serde_json::json!([{ "name": "payments", "member_count": 1 }])
    );
    assert_eq!(
        out["resolved"]["anyone_with_write"],
        serde_json::json!(false)
    );

    // `set noparent` seals sealed/ off from the root file.
    let (st, out) = server.get("/v1/orgs/acme/repos/app/owners?path=sealed/keys.rs", admin);
    assert_eq!(st, 200, "{out}");
    let rules = out["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 1, "{out}");
    assert_eq!(rules[0]["dir"], "sealed");
    assert_eq!(rules[0]["noparent"], serde_json::json!(true));
}

#[test]
fn the_sufficiency_preview_names_what_blocks_and_who_satisfied() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-check");
    let scratch = Scratch::new("changes-check");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);

    let c1 = commit(
        server,
        admin,
        "app",
        "main",
        "base",
        &[("OWNERS", "casey@acme.test\n*\n"), ("README.md", "hi")],
    );
    let c2 = commit(
        server,
        admin,
        "app",
        "main",
        "payments work",
        &[
            ("payments/OWNERS", "alice@acme.test\n@payments\n"),
            ("payments/gateway.rs", "fn main() {}"),
        ],
    );

    let check = |approvers: &str| -> serde_json::Value {
        let q = if approvers.is_empty() {
            String::new()
        } else {
            format!("&approvers={approvers}")
        };
        let (st, out) = server.get(
            &format!("/v1/orgs/acme/repos/app/owners/check?from={c1}&to={c2}{q}"),
            admin,
        );
        assert_eq!(st, 200, "{out}");
        out
    };

    // No approvals: blocked, and the explanation names the owner list.
    let out = check("");
    assert_eq!(
        out["changed_paths"],
        serde_json::json!(["payments/OWNERS", "payments/gateway.rs"])
    );
    assert_eq!(out["verdict"]["landable"], serde_json::json!(false));
    assert_eq!(
        out["verdict"]["explanation"],
        serde_json::json!(
            "blocked: needs an owner of /payments/OWNERS (owners: alice@acme.test, @payments, casey@acme.test, *)"
        )
    );

    // A viewer's approval cannot satisfy even the `*` entry.
    let out = check("vic@acme.test");
    assert_eq!(out["verdict"]["landable"], serde_json::json!(false));

    // A plain writer satisfies `*` from the root file.
    let out = check("dev@acme.test");
    assert_eq!(out["verdict"]["landable"], serde_json::json!(true), "{out}");

    // The payments owner satisfies by ownership, and the per-path
    // explanations say who.
    let out = check("alice@acme.test");
    assert_eq!(out["verdict"]["landable"], serde_json::json!(true));
    assert_eq!(
        out["verdict"]["per_path"][1]["explanation"],
        serde_json::json!("ok: /payments/gateway.rs approved by alice@acme.test")
    );

    // Unknown approver emails are reported, never silently dropped.
    let out = check("ghost@acme.test,alice@acme.test");
    assert_eq!(
        out["unknown_approvers"],
        serde_json::json!(["ghost@acme.test"])
    );
    assert_eq!(out["verdict"]["landable"], serde_json::json!(true));
}

#[test]
fn ungoverned_repos_need_a_writer_and_say_so() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-plain");
    let scratch = Scratch::new("changes-plain");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos",
        admin,
        Some(serde_json::json!({"name": "plain"})),
    );
    assert_eq!(st, 201);
    let c1 = commit(server, admin, "plain", "main", "base", &[("a.txt", "1")]);
    let c2 = commit(server, admin, "plain", "main", "more", &[("b.txt", "2")]);

    let (st, out) = server.get(
        &format!(
            "/v1/orgs/acme/repos/plain/owners/check?from={c1}&to={c2}&approvers=vic@acme.test"
        ),
        admin,
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["verdict"]["landable"], serde_json::json!(false));
    assert_eq!(
        out["verdict"]["explanation"],
        serde_json::json!(
            "blocked: /b.txt needs any approval with write access (no OWNERS rule governs it)"
        )
    );
    let (st, out) = server.get(
        &format!(
            "/v1/orgs/acme/repos/plain/owners/check?from={c1}&to={c2}&approvers=dev@acme.test"
        ),
        admin,
    );
    assert_eq!(st, 200);
    assert_eq!(out["verdict"]["landable"], serde_json::json!(true), "{out}");

    // The owners endpoint reports the ungoverned state honestly.
    let (st, out) = server.get("/v1/orgs/acme/repos/plain/owners?path=b.txt", admin);
    assert_eq!(st, 200);
    assert_eq!(out["rules"], serde_json::json!([]));
    assert_eq!(out["resolved"]["users"], serde_json::json!([]));
}

#[test]
fn an_owners_parse_error_blocks_and_names_the_file_and_line() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-badowners");
    let scratch = Scratch::new("changes-badowners");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);

    let c1 = commit(server, admin, "app", "main", "base", &[("README.md", "hi")]);
    let c2 = commit(
        server,
        admin,
        "app",
        "main",
        "typo in the rule",
        &[
            ("bad/OWNERS", "alice@acme.test\nnot-an-email\n"),
            ("bad/thing.rs", "x"),
        ],
    );

    // Even the org's strongest approvals cannot pass a rule nobody can
    // read: the file must be fixed first.
    let (st, out) = server.get(
        &format!(
            "/v1/orgs/acme/repos/app/owners/check?from={c1}&to={c2}&approvers=alice@acme.test,casey@acme.test,dev@acme.test"
        ),
        admin,
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["verdict"]["landable"], serde_json::json!(false));
    assert_eq!(
        out["verdict"]["explanation"],
        serde_json::json!(
            "blocked: OWNERS parse error at bad/OWNERS:2 (unrecognized owner \"not-an-email\" (expected an email, @team, or *))"
        )
    );

    // The owners endpoint reports the same poison for the path.
    let (st, out) = server.get("/v1/orgs/acme/repos/app/owners?path=bad/thing.rs", admin);
    assert_eq!(st, 200);
    assert_eq!(out["error"]["dir"], serde_json::json!("bad"));
    assert_eq!(out["error"]["line"], serde_json::json!(2));
    assert!(server.healthy());
}

#[test]
fn owners_endpoints_refuse_bad_input_and_mask_cross_org_probes() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-edges");
    let scratch = Scratch::new("changes-edges");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(server, admin, "app", "main", "base", &[("a.txt", "1")]);

    // Parameter validation, each with the status a client can act on.
    let cases = [
        ("/v1/orgs/acme/repos/app/owners", 400), // path missing
        ("/v1/orgs/acme/repos/app/owners?path=../etc", 400),
        ("/v1/orgs/acme/repos/app/owners?path=/abs", 400),
        ("/v1/orgs/acme/repos/app/owners?path=a.txt&at=nonesuch", 404),
        ("/v1/orgs/acme/repos/app/owners/check", 400), // from/to missing
        (
            "/v1/orgs/acme/repos/app/owners/check?from=nonesuch&to=HEAD",
            404,
        ),
        (
            "/v1/orgs/acme/repos/app/owners/check?from=HEAD&to=nonesuch",
            404,
        ),
    ];
    for (path, want) in cases {
        let (st, out) = server.get(path, admin);
        assert_eq!(st, want, "{path}: {out}");
    }
    let too_many = vec!["x@y.z"; 101].join(",");
    let (st, _) = server.get(
        &format!("/v1/orgs/acme/repos/app/owners/check?from=HEAD&to=HEAD&approvers={too_many}"),
        admin,
    );
    assert_eq!(st, 400);

    // Existence masking: a stranger org's token sees 404, never 403,
    // for both endpoints; no credential at all sees 401.
    let rival = server.bootstrap_org("rival");
    for path in [
        "/v1/orgs/acme/repos/app/owners?path=a.txt",
        "/v1/orgs/acme/repos/app/owners/check?from=HEAD&to=HEAD",
    ] {
        let (st, _) = server.get(path, &rival);
        assert_eq!(st, 404, "{path} with a rival token");
        assert_eq!(server.status_get(path, None), 401, "{path} anonymous");
    }

    // The hostile-input corpus through every new query field. The
    // answers vary (400/404); what must hold is that nothing 500s and
    // the server stays healthy.
    for inj in INJECTIONS {
        let enc = percent_encode(inj);
        for path in [
            format!("/v1/orgs/acme/repos/app/owners?path={enc}"),
            format!("/v1/orgs/acme/repos/app/owners?path=a.txt&at={enc}"),
            format!("/v1/orgs/acme/repos/app/owners/check?from={enc}&to=HEAD"),
            format!("/v1/orgs/acme/repos/app/owners/check?from=HEAD&to=HEAD&approvers={enc}"),
        ] {
            let st = server.status_get(&path, Some(admin));
            assert!(st == 400 || st == 404 || st == 200, "{path} answered {st}");
        }
    }
    assert!(server.healthy());
}

/// Sign in and keep the cookie: approvals are made by people, and a
/// person's authority is resolved on every request.
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

#[test]
fn a_branch_tip_becomes_a_change_and_revisions_become_patchsets() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-create");
    let scratch = Scratch::new("changes-create");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(server, admin, "app", "main", "base", &[("README.md", "hi")]);
    commit(
        server,
        admin,
        "app",
        "feature",
        "add gateway\n\nChange-Id: Icafe1234\n",
        &[("payments/gateway.rs", "v1")],
    );

    // The tip registers under its trailer; the same tip again is an ack.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["change"]["key"], serde_json::json!("Icafe1234"));
    assert_eq!(out["change"]["title"], serde_json::json!("add gateway"));
    assert_eq!(out["change"]["target_branch"], serde_json::json!("main"));
    assert_eq!(out["patchset"]["number"], serde_json::json!(1));
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 200);

    // A revision under the same Change-Id becomes patchset 2 and the
    // title follows the newest message.
    commit(
        server,
        admin,
        "app",
        "feature",
        "add gateway, reviewed\n\nChange-Id: Icafe1234\n",
        &[("payments/gateway.rs", "v2")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["patchset"]["number"], serde_json::json!(2));
    assert_eq!(
        out["change"]["title"],
        serde_json::json!("add gateway, reviewed")
    );

    // The detail view carries the whole patchset history.
    let (st, out) = server.get("/v1/orgs/acme/repos/app/changes/Icafe1234", admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["patchsets"].as_array().unwrap().len(), 2);
    assert_eq!(out["change"]["state"], serde_json::json!("open"));

    // A commit with no trailer still gets a change, keyed from its oid —
    // an identity that dies with the commit, as documented.
    let tip = commit(
        server,
        admin,
        "app",
        "untagged",
        "no trailer here",
        &[("x.txt", "x")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "untagged"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["change"]["key"], serde_json::json!(format!("g{tip}")));

    // The list shows both, newest first.
    let (st, out) = server.get("/v1/orgs/acme/repos/app/changes", admin);
    assert_eq!(st, 200);
    let list = out["changes"].as_array().unwrap();
    assert_eq!(list.len(), 2, "{out}");
    assert_eq!(list[0]["key"], serde_json::json!(format!("g{tip}")));
}

#[test]
fn approvals_attach_to_the_latest_patchset_and_never_carry_forward() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-approve");
    let scratch = Scratch::new("changes-approve");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n")],
    );
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "work\n\nChange-Id: Ibeef9999\n",
        &[("core.rs", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);
    let cp = "/v1/orgs/acme/repos/app/changes/Ibeef9999";

    // Anyone with read may approve — but a viewer's approval does not
    // make the change landable, because sufficiency wants the owner.
    let vic = sign_in(server, "vic@acme.test");
    let (st, out) = as_person(server, &vic, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(st, 200);
    assert_eq!(
        out["verdict"]["landable"],
        serde_json::json!(false),
        "{out}"
    );

    // The owner's approval flips it.
    let alice = sign_in(server, "alice@acme.test");
    let (st, _) = as_person(server, &alice, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204);
    let (st, out) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(st, 200);
    assert_eq!(out["verdict"]["landable"], serde_json::json!(true), "{out}");
    assert_eq!(
        out["verdict"]["explanation"],
        serde_json::json!("ok: all 1 changed path(s) approved")
    );

    // A new patchset resets the question: yesterday's approval says
    // nothing about today's revision.
    commit(
        server,
        admin,
        "app",
        "feature",
        "work, revised\n\nChange-Id: Ibeef9999\n",
        &[("core.rs", "v2")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);
    let (st, out) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(st, 200);
    assert_eq!(
        out["verdict"]["landable"],
        serde_json::json!(false),
        "{out}"
    );
    let (st, out) = server.get(cp, admin);
    assert_eq!(st, 200);
    assert_eq!(out["approvals"], serde_json::json!([]));

    // Approve again, revoke, and revoking twice is a 404 — there is
    // nothing left to take back.
    let (st, _) = as_person(server, &alice, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204);
    let (st, out) = server.get(cp, admin);
    assert_eq!(st, 200);
    assert_eq!(
        out["approvals"][0]["email"],
        serde_json::json!("alice@acme.test")
    );
    let (st, _) = as_person(server, &alice, "DELETE", &format!("{cp}/approve"), None);
    assert_eq!(st, 204);
    let (st, _) = as_person(server, &alice, "DELETE", &format!("{cp}/approve"), None);
    assert_eq!(st, 404);
}

/// The reviewer set is *computed*, and the verdict read is where a
/// person finds that out.
///
/// Everything here is asserted through the API a client actually calls,
/// because the point of the field is that a client cannot work it out
/// for itself: the verdict's `per_path` carries the OWNERS entries as
/// written — `@core`, a team — and only the server can say which people
/// that is. The three shapes are kept apart deliberately, since two of
/// them produce an empty list for opposite reasons: a `*` path requires
/// nobody in particular but does need a writer, and an ungoverned path
/// is governed by nothing at all.
#[test]
fn the_verdict_names_the_people_owners_requires_and_who_has_approved() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-reviewers");
    let scratch = Scratch::new("changes-reviewers");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);

    // A team with two people in it, named by OWNERS as a team. Neither
    // address appears in the file, so a reviewer list that merely echoed
    // the file back would name nobody.
    let (st, members) = server.get("/v1/orgs/acme/members", admin);
    assert_eq!(st, 200, "{members}");
    let id_of = |email: &str| -> String {
        members["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["email"] == email)
            .unwrap_or_else(|| panic!("{email} is a member"))["user_id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let (st, team) = server.post(
        "/v1/orgs/acme/teams",
        admin,
        Some(serde_json::json!({"name": "core"})),
    );
    assert_eq!(st, 201, "{team}");
    let team_id = team["id"].as_str().unwrap();
    for email in ["alice@acme.test", "casey@acme.test"] {
        let (st, out) = server.req(
            "PUT",
            &format!("/v1/orgs/acme/teams/{team_id}/members/{}", id_of(email)),
            admin,
            None,
        );
        assert_eq!(st, 204, "{out}");
    }

    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[
            ("payments/OWNERS", "@core\n"),
            ("payments/gateway.rs", "v1"),
            // A named owner *and* `*`. A bare `*` resolves to no owner
            // ids at all, so a fixture with only that one cannot tell a
            // correct filter apart from no filter — the mutation that
            // drops the `*` check passes against it. This shape is also
            // the one people actually write: a maintainer to ask first,
            // and an escape hatch so the docs are not blocked on them.
            ("docs/OWNERS", "casey@acme.test\n*\n"),
            ("docs/readme.md", "v1"),
            ("scratch.txt", "v1"),
        ],
    );

    // Open a change on its own branch touching exactly one file.
    let open = |branch_name: &str, key: &str, path: &str| -> String {
        branch(server, admin, "app", branch_name, "main");
        commit(
            server,
            admin,
            "app",
            branch_name,
            &format!("work\n\nChange-Id: {key}\n"),
            &[(path, "v2")],
        );
        let (st, out) = server.post(
            "/v1/orgs/acme/repos/app/changes",
            admin,
            Some(serde_json::json!({"from": branch_name})),
        );
        assert_eq!(st, 201, "{out}");
        format!("/v1/orgs/acme/repos/app/changes/{key}")
    };

    // An owned path: the team is expanded to the people in it, ordered
    // by display name, and nobody has approved yet.
    let owned = open("owned", "Ic0de0001", "payments/gateway.rs");
    let (st, out) = server.get(&format!("{owned}/verdict"), admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        out["reviewers"]["required"],
        serde_json::json!([
            {"user_id": id_of("alice@acme.test"), "name": "Alice",
             "email": "alice@acme.test", "approved": false},
            {"user_id": id_of("casey@acme.test"), "name": "Casey",
             "email": "casey@acme.test", "approved": false},
        ]),
        "{out}"
    );
    assert_eq!(
        out["reviewers"]["anyone_with_write"],
        serde_json::json!(false)
    );

    // One of them approves. The same read now reports that person
    // satisfied and the other still outstanding — which is the sentence
    // the sidebar renders, and the reason `approved` is per-person
    // rather than a count.
    let alice = sign_in(server, "alice@acme.test");
    let (st, out) = as_person(server, &alice, "POST", &format!("{owned}/approve"), None);
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.get(&format!("{owned}/verdict"), admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["verdict"]["landable"], serde_json::json!(true), "{out}");
    assert_eq!(
        out["reviewers"]["required"][0]["approved"],
        serde_json::json!(true),
        "{out}"
    );
    assert_eq!(
        out["reviewers"]["required"][1]["approved"],
        serde_json::json!(false),
        "an approval satisfies the change, not everyone the file names: {out}"
    );

    // A viewer's approval is on the record but satisfies nothing, and
    // — the thing worth pinning — a bystander who is not a required
    // reviewer never appears in the list at all.
    let vic = sign_in(server, "vic@acme.test");
    let (st, out) = as_person(server, &vic, "POST", &format!("{owned}/approve"), None);
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.get(&format!("{owned}/verdict"), admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["reviewers"]["required"].as_array().unwrap().len(), 2);
    assert_eq!(
        out["reviewers"]["required"][1]["approved"],
        serde_json::json!(false),
        "{out}"
    );

    // A `*` path requires nobody in particular but is not ungoverned:
    // the empty list comes with the flag that lets the page say "anyone
    // with write access" instead of "nobody".
    let starred = open("starred", "Ic0de0002", "docs/readme.md");
    let (st, out) = server.get(&format!("{starred}/verdict"), admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["reviewers"]["required"], serde_json::json!([]), "{out}");
    assert_eq!(
        out["reviewers"]["anyone_with_write"],
        serde_json::json!(true),
        "{out}"
    );

    // An ungoverned path: nobody required, and no `*` rule either. The
    // two empty lists are told apart by the flag alone.
    let plain = open("plain", "Ic0de0003", "scratch.txt");
    let (st, out) = server.get(&format!("{plain}/verdict"), admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["reviewers"]["required"], serde_json::json!([]), "{out}");
    assert_eq!(
        out["reviewers"]["anyone_with_write"],
        serde_json::json!(false),
        "{out}"
    );

    // The reviewer set is people's names and addresses, so it must mask
    // exactly as the rest of the repository does: a rival org learns
    // nothing, and an unauthenticated caller is told to authenticate
    // rather than that the change is absent.
    let rival = server.bootstrap_org("rival");
    let (st, _) = server.get(&format!("{owned}/verdict"), &rival);
    assert_eq!(st, 404);
    assert_eq!(
        server.status_get(&format!("{owned}/verdict"), None),
        401,
        "an anonymous read of a private repo is a 401, not a leak"
    );
    assert!(server.healthy());
}

#[test]
fn service_tokens_cannot_approve_and_change_edges_are_refused() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-edges2");
    let scratch = Scratch::new("changes-edges2");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(server, admin, "app", "main", "base", &[("a.txt", "1")]);
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "t\n\nChange-Id: Iaaaa0001\n",
        &[("b.txt", "2")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);

    // The bootstrap credential is an org service token: no person, no
    // approval. Sufficiency counts people.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes/Iaaaa0001/approve",
        admin,
        None,
    );
    assert_eq!(st, 403, "{out}");
    assert!(out["error"].as_str().unwrap().contains("person"), "{out}");

    // Absent and invalid-shaped keys are the same 404.
    for key in ["Inothere1", "not%20a%20key", "no"] {
        let (st, _) = server.get(&format!("/v1/orgs/acme/repos/app/changes/{key}"), admin);
        assert_eq!(st, 404, "{key}");
    }
    // Unknown state filter and unknown rev are told apart from success.
    let (st, _) = server.get("/v1/orgs/acme/repos/app/changes?state=zombie", admin);
    assert_eq!(st, 400);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "nonesuch"})),
    );
    assert_eq!(st, 404);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature", "target": "bad target"})),
    );
    assert_eq!(st, 400);

    // Cross-org: every change surface answers 404 to a rival, 401 to
    // nobody.
    let rival = server.bootstrap_org("rival");
    for path in [
        "/v1/orgs/acme/repos/app/changes",
        "/v1/orgs/acme/repos/app/changes/Iaaaa0001",
        "/v1/orgs/acme/repos/app/changes/Iaaaa0001/verdict",
    ] {
        let (st, _) = server.get(path, &rival);
        assert_eq!(st, 404, "{path}");
        assert_eq!(server.status_get(path, None), 401, "{path}");
    }
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes/Iaaaa0001/approve",
        &rival,
        None,
    );
    assert_eq!(st, 404);

    // Hostile bytes in the key position: refused or absent, never a 500,
    // and the server keeps serving.
    for inj in INJECTIONS {
        let enc = percent_encode(inj);
        for path in [
            format!("/v1/orgs/acme/repos/app/changes/{enc}"),
            format!("/v1/orgs/acme/repos/app/changes/{enc}/verdict"),
            format!("/v1/orgs/acme/repos/app/changes?state={enc}"),
        ] {
            let st = server.status_get(&path, Some(admin));
            assert!(st == 400 || st == 404, "{path} answered {st}");
        }
    }
    assert!(server.healthy());
}

#[test]
fn an_approved_change_lands_fast_forward_and_the_clone_is_sound() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-land");
    let scratch = Scratch::new("changes-land");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n")],
    );
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "gateway\n\nChange-Id: I1a2d0001\n",
        &[("gateway.rs", "v1")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let ps_commit = out["patchset"]["commit"].as_str().unwrap().to_string();

    // Unapproved: the land refusal carries the verdict's words.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes/I1a2d0001/land",
        admin,
        None,
    );
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .starts_with("blocked: needs an owner of /gateway.rs"),
        "{out}"
    );

    let alice = sign_in(server, "alice@acme.test");
    let (st, _) = as_person(
        server,
        &alice,
        "POST",
        "/v1/orgs/acme/repos/app/changes/I1a2d0001/approve",
        None,
    );
    assert_eq!(st, 204);
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes/I1a2d0001/land",
        admin,
        None,
    );
    assert_eq!(st, 202, "{out}");
    assert_eq!(out["queued"], serde_json::json!(true));

    // The lander promotes trunk to exactly the patchset commit.
    let out = wait_until_not(server, admin, "I1a2d0001", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("landed"), "{out}");
    assert_eq!(out["change"]["land_verdict"], serde_json::json!("landed"));
    assert_eq!(out["change"]["landed_commit"], serde_json::json!(ps_commit));
    let (st, refs) = server.get("/v1/orgs/acme/repos/app/refs", admin);
    assert_eq!(st, 200);
    let main_tip = refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "refs/heads/main")
        .unwrap()["oid"]
        .as_str()
        .unwrap();
    assert_eq!(main_tip, ps_commit, "trunk is the landed commit");

    // The repo the queue produced is a repo git trusts (I11).
    let url = server.authed_url(admin, "acme", "app");
    let clone = scratch.path().join("clone-after-land");
    gitcli::clone_and_fsck(&url, &clone);

    // Landed is terminal: land again, approve, abandon all say so.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes/I1a2d0001/land",
        admin,
        None,
    );
    assert_eq!(st, 409);
    assert_eq!(out["error"], serde_json::json!("change is landed"));
    let (st, _) = as_person(
        server,
        &alice,
        "POST",
        "/v1/orgs/acme/repos/app/changes/I1a2d0001/approve",
        None,
    );
    assert_eq!(st, 409);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes/I1a2d0001/abandon",
        admin,
        None,
    );
    assert_eq!(st, 409);
}

#[test]
fn a_revoked_approval_ejects_at_claim_time_with_the_verdict_recorded() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-eject");
    let scratch = Scratch::new("changes-eject");
    let db_url = stratum_testkit::pg::test_db_url("changes-eject");
    // Server A has no lander at all, so the enqueue → revoke ordering is
    // a fact, not a race.
    let a = spawn_with(&bucket.base_url, &scratch, Some(&db_url), "0");
    let w = world(a);
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n")],
    );
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "risky\n\nChange-Id: Ie1ec1001\n",
        &[("risky.rs", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);
    let alice = sign_in(server, "alice@acme.test");
    let (st, _) = as_person(
        server,
        &alice,
        "POST",
        "/v1/orgs/acme/repos/app/changes/Ie1ec1001/approve",
        None,
    );
    assert_eq!(st, 204);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes/Ie1ec1001/land",
        admin,
        None,
    );
    assert_eq!(st, 202);
    // The queue shows it landing.
    let (st, q) = server.get("/v1/orgs/acme/repos/app/land-queue", admin);
    assert_eq!(st, 200);
    assert_eq!(q["queue"][0]["key"], serde_json::json!("Ie1ec1001"));
    // The approval goes away while the job sits queued.
    let (st, _) = as_person(
        server,
        &alice,
        "DELETE",
        "/v1/orgs/acme/repos/app/changes/Ie1ec1001/approve",
        None,
    );
    assert_eq!(st, 204);

    // A second queue item whose repo is deleted while the job waits: the
    // lander must retire it quietly, never wedge on it.
    let (st, _) = server.post(
        "/v1/orgs/acme/repos",
        admin,
        Some(serde_json::json!({"name": "doomed"})),
    );
    assert_eq!(st, 201);
    commit(server, admin, "doomed", "main", "base", &[("a.txt", "1")]);
    branch(server, admin, "doomed", "feature", "main");
    commit(
        server,
        admin,
        "doomed",
        "feature",
        "gone soon\n\nChange-Id: Id00d0001\n",
        &[("gone.rs", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/doomed/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);
    let dev = sign_in(server, "dev@acme.test");
    let (st, _) = as_person(
        server,
        &dev,
        "POST",
        "/v1/orgs/acme/repos/doomed/changes/Id00d0001/approve",
        None,
    );
    assert_eq!(st, 204);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/doomed/changes/Id00d0001/land",
        admin,
        None,
    );
    assert_eq!(st, 202);
    // The doomed job's id, taken **before** the repository is deleted —
    // afterwards every route answers 404 and `repo_by_name` filters the
    // tombstone, so there is nothing left to ask about it. This is the
    // observable the settle below used to be a proxy for: the claim is
    // that the lander *retires* this job, and a sleep followed by "the
    // repo is still gone" would have passed against a lander that never
    // woke up at all.
    let db = stratum_control::ControlDb::open(&db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .expect("the org")
        .id;
    let doomed_repo = stratum_control::registry::repo_by_name(&db, &org_id, "doomed")
        .unwrap()
        .expect("the repo")
        .id;
    let doomed_job = stratum_control::changes::by_key(&db, &doomed_repo, "Id00d0001")
        .unwrap()
        .expect("the change")
        .land_job_id
        .expect("landing means a job");

    let (st, _) = server.req("DELETE", "/v1/orgs/acme/repos/doomed", admin, None);
    assert_eq!(st, 204, "delete doomed");

    let admin_token = admin.clone();
    drop(w);

    // Server B — same database, same store — brings a live lander to the
    // queued job, exactly like a restart would. Its claim-time check is
    // the one that counts.
    let scratch_b = Scratch::new("changes-eject-b");
    let server = spawn_with(&bucket.base_url, &scratch_b, Some(&db_url), "0.2");
    let out = wait_until_not(&server, &admin_token, "Ie1ec1001", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("open"), "{out}");
    let verdict = out["change"]["land_verdict"].as_str().unwrap();
    assert!(
        verdict.starts_with("ejected: sufficiency lost — blocked: needs an owner of /risky.rs"),
        "{verdict}"
    );
    // And the queue is empty again.
    let (st, q) = server.get("/v1/orgs/acme/repos/app/land-queue", &admin_token);
    assert_eq!(st, 200);
    assert_eq!(q["queue"], serde_json::json!([]));
    // The doomed repo's job retired without wedging anything: the job
    // row settles, the repo stays gone, and the server keeps serving.
    // Retiring a job whose repository is not there any more is a settled
    // outcome, not an error — a queue that keeps re-leasing a dead row
    // never drains.
    let retired = wait_for("the doomed job to settle", Duration::from_secs(60), || {
        let j = stratum_control::jobs::get(&db, &org_id, &doomed_job)
            .expect("read job")
            .expect("the job row outlives the repository");
        (j.state == "done" || j.state == "failed").then_some(j)
    });
    assert_eq!(
        retired.state, "done",
        "a queued landing whose repository was deleted under it was an \
         error rather than a settled no-op: {retired:?}"
    );
    let (st, _) = server.get("/v1/orgs/acme/repos/doomed", &admin_token);
    assert_eq!(st, 404);
    assert!(server.healthy());
}

#[test]
fn a_moved_trunk_ejects_with_a_fast_forward_verdict() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-ff");
    let scratch = Scratch::new("changes-ff");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(server, admin, "app", "main", "base", &[("a.txt", "1")]);
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "stale work\n\nChange-Id: I51a1e001\n",
        &[("stale.rs", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);
    // No OWNERS anywhere: any writer's approval suffices — the admin
    // token cannot approve, but dev can.
    let dev = sign_in(server, "dev@acme.test");
    let (st, _) = as_person(
        server,
        &dev,
        "POST",
        "/v1/orgs/acme/repos/app/changes/I51a1e001/approve",
        None,
    );
    assert_eq!(st, 204);
    // Trunk moves on before the land is asked for: the change's commit
    // no longer descends from the tip.
    let new_tip = commit(server, admin, "app", "main", "unrelated", &[("b.txt", "2")]);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes/I51a1e001/land",
        admin,
        None,
    );
    assert_eq!(st, 202);
    let out = wait_until_not(server, admin, "I51a1e001", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("open"), "{out}");
    assert_eq!(
        out["change"]["land_verdict"],
        serde_json::json!(format!("ejected: not fast-forward from {}", &new_tip[..12]))
    );
    // Trunk did not move: the tip is still the commit that ejected us.
    let (_, refs) = server.get("/v1/orgs/acme/repos/app/refs", admin);
    let main_tip = refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "refs/heads/main")
        .unwrap()["oid"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(main_tip, new_tip);
}

#[test]
fn landing_the_stack_top_lands_the_whole_stack() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-stack");
    let scratch = Scratch::new("changes-stack");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n")],
    );
    // A two-change stack on one branch: refactor, then the feature on
    // top of it.
    branch(server, admin, "app", "feature", "main");
    let c1 = commit(
        server,
        admin,
        "app",
        "feature",
        "refactor\n\nChange-Id: I57ac0001\n",
        &[("lib.rs", "refactored")],
    );
    let c2 = commit(
        server,
        admin,
        "app",
        "feature",
        "feature\n\nChange-Id: I57ac0002\n",
        &[("feature.rs", "new")],
    );
    for from in [c1.as_str(), "feature"] {
        let (st, out) = server.post(
            "/v1/orgs/acme/repos/app/changes",
            admin,
            Some(serde_json::json!({"from": from})),
        );
        assert_eq!(st, 201, "{out}");
    }
    let alice = sign_in(server, "alice@acme.test");
    for key in ["I57ac0001", "I57ac0002"] {
        let (st, _) = as_person(
            server,
            &alice,
            "POST",
            &format!("/v1/orgs/acme/repos/app/changes/{key}/approve"),
            None,
        );
        assert_eq!(st, 204, "{key}");
    }
    // Two bystanders reconciliation must leave alone: an unrelated open
    // change on its own branch, and one aimed at a different target.
    branch(server, admin, "app", "sidework", "main");
    commit(
        server,
        admin,
        "app",
        "sidework",
        "unrelated\n\nChange-Id: I51de0001\n",
        &[("side.rs", "s")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "sidework"})),
    );
    assert_eq!(st, 201);
    branch(server, admin, "app", "elsewhere", "main");
    commit(
        server,
        admin,
        "app",
        "elsewhere",
        "other target\n\nChange-Id: Ie15e0001\n",
        &[("other.rs", "o")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "elsewhere", "target": "release"})),
    );
    assert_eq!(st, 201);

    // Land the top; the queue lands the chain and reconciles the base.
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes/I57ac0002/land",
        admin,
        None,
    );
    assert_eq!(st, 202);
    let top = wait_until_not(server, admin, "I57ac0002", "landing");
    assert_eq!(top["change"]["state"], serde_json::json!("landed"), "{top}");
    assert_eq!(top["change"]["landed_commit"], serde_json::json!(c2));
    let base = wait_until_not(server, admin, "I57ac0001", "open");
    assert_eq!(
        base["change"]["state"],
        serde_json::json!("landed"),
        "{base}"
    );
    assert_eq!(
        base["change"]["land_verdict"],
        serde_json::json!(format!("landed: included in {}", &c2[..12]))
    );
    assert_eq!(base["change"]["landed_commit"], serde_json::json!(c1));
    // The bystanders are untouched: not ancestors (or not even aimed at
    // this branch), so reconciliation left them open with no verdict.
    for key in ["I51de0001", "Ie15e0001"] {
        let (st, out) = server.get(&format!("/v1/orgs/acme/repos/app/changes/{key}"), admin);
        assert_eq!(st, 200);
        assert_eq!(out["change"]["state"], serde_json::json!("open"), "{out}");
        assert_eq!(
            out["change"]["land_verdict"],
            serde_json::Value::Null,
            "{out}"
        );
    }
    // One clone carries the whole stack, fsck-clean.
    let clone = scratch.path().join("stack-clone");
    gitcli::clone_and_fsck(&server.authed_url(admin, "acme", "app"), &clone);
    assert!(clone.join("lib.rs").exists() && clone.join("feature.rs").exists());
}

#[test]
fn abandoned_changes_refuse_further_review_actions() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-abandon");
    let scratch = Scratch::new("changes-abandon");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(server, admin, "app", "main", "base", &[("a.txt", "1")]);
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "dead end\n\nChange-Id: Idead0001\n",
        &[("dead.rs", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);
    let cp = "/v1/orgs/acme/repos/app/changes/Idead0001";
    let (st, _) = server.post(&format!("{cp}/abandon"), admin, None);
    assert_eq!(st, 204);
    let (st, out) = server.get(cp, admin);
    assert_eq!(st, 200);
    assert_eq!(out["change"]["state"], serde_json::json!("abandoned"));
    // Everything after is a 409 that names the state: approve, land,
    // a new patchset, another abandon.
    let alice = sign_in(server, "alice@acme.test");
    let (st, out) = as_person(server, &alice, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["error"], serde_json::json!("change is abandoned"));
    let (st, _) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 409);
    commit(
        server,
        admin,
        "app",
        "feature",
        "necromancy\n\nChange-Id: Idead0001\n",
        &[("dead.rs", "v2")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["error"], serde_json::json!("change is abandoned"));
    let (st, _) = server.post(&format!("{cp}/abandon"), admin, None);
    assert_eq!(st, 409);
}

#[test]
fn change_lifecycle_webhooks_are_signed_and_named() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    // Minimal webhook receiver: captures (signature, body) pairs.
    fn webhook_receiver() -> (String, mpsc::Receiver<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let mut header_end = 0;
                    while header_end == 0 {
                        let Ok(n) = s.read(&mut tmp) else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = p + 4;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let clen: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    while buf.len() < header_end + clen {
                        let Ok(n) = s.read(&mut tmp) else { return };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let body = String::from_utf8_lossy(&buf[header_end..]).to_string();
                    let sig = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .starts_with("x-weft-signature-256:")
                                .then(|| l.split_once(':').unwrap().1.trim().to_string())
                        })
                        .unwrap_or_default();
                    let _ = s.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    let _ = tx.send((sig, body));
                });
            }
        });
        (format!("http://{addr}/hook"), rx)
    }

    let minio = Minio::shared();
    let bucket = minio.bucket("changes-hooks");
    let scratch = Scratch::new("changes-hooks");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let (hook_url, rx) = webhook_receiver();
    let (st, sub) = server.post(
        "/v1/orgs/acme/repos/app/webhooks",
        admin,
        Some(serde_json::json!({ "url": hook_url })),
    );
    assert_eq!(st, 201, "{sub}");
    let secret = sub["secret"].as_str().unwrap().to_string();

    commit(server, admin, "app", "main", "base", &[("a.txt", "1")]);
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "hooked\n\nChange-Id: Ib00c0001\n",
        &[("hook.rs", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);
    let dev = sign_in(server, "dev@acme.test");
    let (st, _) = as_person(
        server,
        &dev,
        "POST",
        "/v1/orgs/acme/repos/app/changes/Ib00c0001/approve",
        None,
    );
    assert_eq!(st, 204);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes/Ib00c0001/land",
        admin,
        None,
    );
    assert_eq!(st, 202);
    wait_until_not(server, admin, "Ib00c0001", "landing");

    // The commits above also fire "push" deliveries; drain until the
    // landing's own event arrives, and verify its signature.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let (sig, body) = loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .expect("change.landed delivery never arrived");
        let (sig, body) = rx.recv_timeout(remaining).expect("a delivery");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        if v["event"] == serde_json::json!("change.landed") {
            break (sig, body);
        }
    };
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    use hmac::Mac;
    mac.update(body.as_bytes());
    let want = format!(
        "sha256={}",
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    assert_eq!(sig, want, "change.landed must be HMAC-signed");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["payload"]["change"], serde_json::json!("Ib00c0001"));
    assert_eq!(v["payload"]["branch"], serde_json::json!("main"));
    assert_eq!(v["payload"]["patchset"], serde_json::json!(1));
}

#[test]
fn a_landing_and_concurrent_pushes_serialize_on_the_cas() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-race");
    let scratch = Scratch::new("changes-race");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(server, admin, "app", "main", "base", &[("a.txt", "1")]);
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "racer\n\nChange-Id: Iace10001\n",
        &[("race.rs", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);
    let dev = sign_in(server, "dev@acme.test");
    let (st, _) = as_person(
        server,
        &dev,
        "POST",
        "/v1/orgs/acme/repos/app/changes/Iace10001/approve",
        None,
    );
    assert_eq!(st, 204);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes/Iace10001/land",
        admin,
        None,
    );
    assert_eq!(st, 202);
    // Trunk keeps moving while the lander works. Whatever interleaving
    // happens, the CAS decides: the change either lands after retrying
    // or ejects with the fast-forward verdict — never a forced ref,
    // never a corrupt history.
    for i in 0..5 {
        commit(
            server,
            admin,
            "app",
            "main",
            &format!("racing {i}"),
            &[("noise.txt", &format!("{i}"))],
        );
    }
    let out = wait_until_not(server, admin, "Iace10001", "landing");
    let state = out["change"]["state"].as_str().unwrap();
    match state {
        "landed" => assert_eq!(out["change"]["land_verdict"], serde_json::json!("landed")),
        "open" => {
            let v = out["change"]["land_verdict"].as_str().unwrap();
            assert!(v.starts_with("ejected: not fast-forward"), "{v}");
        }
        other => panic!("unexpected state {other}: {out}"),
    }
    let clone = scratch.path().join("race-clone");
    gitcli::clone_and_fsck(&server.authed_url(admin, "acme", "app"), &clone);
    assert!(server.healthy());
}

#[test]
fn mirrors_refuse_review_and_land_routes_mask_and_survive_hostile_input() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-landedges");
    let scratch = Scratch::new("changes-landedges");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(server, admin, "app", "main", "base", &[("a.txt", "1")]);
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "t\n\nChange-Id: Iedec0001\n",
        &[("b.txt", "2")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);

    // A mirror's trunk belongs to its origin: review is refused at the
    // door, so no change can ever exist to land.
    let origin_work = scratch.path().join("origin-work");
    gitcli::fixture_repo(&origin_work, 2);
    let bare = scratch.path().join("origins/acme/mirrored.git");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    gitcli::git(
        bare.parent().unwrap(),
        &["init", "-q", "--bare", "mirrored.git"],
    );
    gitcli::git(
        &origin_work,
        &["push", "-q", bare.to_str().unwrap(), "main:main"],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors",
        admin,
        Some(serde_json::json!({
            "name": "mirrored",
            "provider": "generic",
            "origin": format!("file://{}", bare.display()),
        })),
    );
    assert_eq!(st, 202, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/mirrored/changes",
        admin,
        Some(serde_json::json!({"from": "main"})),
    );
    assert_eq!(st, 403, "{out}");
    assert_eq!(out["error"], serde_json::json!("mirrors are read-only"));
    // Protection has nothing to defend on a mirror, and its default
    // branch follows the origin HEAD — both refused in words.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/mirrored/protections",
        admin,
        Some(serde_json::json!({"branch": "main"})),
    );
    assert_eq!(st, 403, "{out}");
    assert_eq!(out["error"], serde_json::json!("mirrors are read-only"));
    let (st, out) = server.req(
        "PATCH",
        "/v1/orgs/acme/repos/mirrored",
        admin,
        Some(serde_json::json!({"default_branch": "main"})),
    );
    assert_eq!(st, 403, "{out}");
    assert_eq!(
        out["error"],
        serde_json::json!("a mirror's default branch follows its origin")
    );

    // Cross-org masking and anonymous refusal on every land surface.
    let rival = server.bootstrap_org("rival");
    let (st, _) = server.get("/v1/orgs/acme/repos/app/land-queue", &rival);
    assert_eq!(st, 404);
    assert_eq!(
        server.status_get("/v1/orgs/acme/repos/app/land-queue", None),
        401
    );
    for path in [
        "/v1/orgs/acme/repos/app/changes/Iedec0001/land",
        "/v1/orgs/acme/repos/app/changes/Iedec0001/abandon",
    ] {
        let (st, _) = server.req("POST", path, &rival, None);
        assert_eq!(st, 404, "{path}");
    }
    let (st, _) = server.req(
        "DELETE",
        "/v1/orgs/acme/repos/app/changes/Iedec0001/approve",
        &rival,
        None,
    );
    assert_eq!(st, 404, "rival unapprove");

    // Hostile bytes through the land surfaces: never a 500, still serving.
    for inj in INJECTIONS {
        let enc = percent_encode(inj);
        for path in [
            format!("/v1/orgs/acme/repos/app/changes/{enc}/land"),
            format!("/v1/orgs/acme/repos/app/changes/{enc}/abandon"),
        ] {
            let st = server.status_post(&path, admin, serde_json::json!({}));
            assert!(st == 400 || st == 404, "{path} answered {st}");
        }
    }
    assert!(server.healthy());
}

#[test]
fn odd_shapes_type_changes_owners_directories_and_root_commits() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-odd");
    let scratch = Scratch::new("changes-odd");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    // A directory named OWNERS is not a rule file: it governs nothing.
    let c1 = commit(
        server,
        admin,
        "app",
        "main",
        "base",
        &[
            ("shape/a.txt", "1"),
            ("OWNERS/readme.md", "not a rule file"),
        ],
    );
    let (st, out) = server.get("/v1/orgs/acme/repos/app/owners?path=shape/a.txt", admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["rules"], serde_json::json!([]));

    // A path that flips from directory to file is reported as its parts:
    // the old tree's files deleted, the new file added.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        admin,
        Some(serde_json::json!({
            "branch": "main",
            "message": "flatten shape",
            "operations": [
                {"op": "delete", "path": "shape/a.txt"},
                {"op": "put", "path": "shape", "content": "now a file"},
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let c2 = out["commit"].as_str().unwrap().to_string();
    let (st, diff) = server.get(
        &format!("/v1/orgs/acme/repos/app/diff?from={c1}&to={c2}"),
        admin,
    );
    assert_eq!(st, 200, "{diff}");
    let changes: Vec<(String, String)> = diff["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["status"].as_str().unwrap().to_string(),
                c["path"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(
        changes.contains(&("deleted".into(), "shape/a.txt".into())),
        "{changes:?}"
    );
    assert!(
        changes.contains(&("added".into(), "shape".into())),
        "{changes:?}"
    );

    // A directory deleted outright (nothing taking its name) reports
    // its files deleted, recursively.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        admin,
        Some(serde_json::json!({
            "branch": "main",
            "message": "temp dir",
            "operations": [{"op": "put", "path": "tmp/inner/x.txt", "content": "x"}],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let c3 = out["commit"].as_str().unwrap().to_string();
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        admin,
        Some(serde_json::json!({
            "branch": "main",
            "message": "drop temp dir",
            // Deleting under a directory that never existed is a no-op,
            // not a new empty directory.
            "operations": [
                {"op": "delete", "path": "tmp/inner/x.txt"},
                {"op": "delete", "path": "never/was/here.txt"},
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let c4 = out["commit"].as_str().unwrap().to_string();
    let (st, diff) = server.get(
        &format!("/v1/orgs/acme/repos/app/diff?from={c3}&to={c4}"),
        admin,
    );
    assert_eq!(st, 200);
    assert_eq!(diff["changes"][0]["status"], serde_json::json!("deleted"));
    assert_eq!(
        diff["changes"][0]["path"],
        serde_json::json!("tmp/inner/x.txt")
    );
    // Deleting a directory's last file removes the directory: git never
    // records an empty tree, so a clone could not reproduce one, and the
    // commit API used to leave `tmp/inner/` and `tmp/` behind as empty
    // trees that `git fsck` tolerated and a revert then had to undo.
    let (st, root) = server.get(&format!("/v1/orgs/acme/repos/app/tree?at={c4}"), admin);
    assert_eq!(st, 200, "{root}");
    let names: Vec<&str> = root["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(!names.contains(&"tmp"), "{names:?}");
    assert!(!names.contains(&"never"), "{names:?}");
    assert!(names.contains(&"shape"), "{names:?}");
    let (st, _) = server.get(&format!("/v1/orgs/acme/repos/app/tree/tmp?at={c4}"), admin);
    assert_eq!(st, 404, "an emptied directory is gone, not empty");
    // The same subtree disappearing against the grain — diff from a
    // commit that has the directory to one that predates it.
    let (st, rev) = server.get(
        &format!("/v1/orgs/acme/repos/app/diff?from={c3}&to={c1}"),
        admin,
    );
    assert_eq!(st, 200, "{rev}");
    let reversed: Vec<(String, String)> = rev["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["status"].as_str().unwrap().to_string(),
                c["path"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(
        reversed.contains(&("deleted".into(), "tmp/inner/x.txt".into())),
        "{reversed:?}"
    );

    // Taking back an approval is as personal as giving one: a service
    // token is refused in the same words.
    let (st, out) = server.req(
        "DELETE",
        "/v1/orgs/acme/repos/app/changes/Iffffff99/approve",
        admin,
        None,
    );
    assert_eq!(st, 403, "{out}");
    assert!(out["error"].as_str().unwrap().contains("person"), "{out}");

    // A root commit's change diffs against the empty tree, and its
    // verdict works like any other.
    commit(
        server,
        admin,
        "app",
        "orphan",
        "rootwork",
        &[("solo.txt", "s")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "orphan"})),
    );
    assert_eq!(st, 201, "{out}");
    let key = out["change"]["key"].as_str().unwrap().to_string();
    let (st, v) = server.get(
        &format!("/v1/orgs/acme/repos/app/changes/{key}/verdict"),
        admin,
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["verdict"]["landable"], serde_json::json!(false));
    assert_eq!(
        v["verdict"]["per_path"][0]["path"],
        serde_json::json!("solo.txt")
    );

    // Review actions against a change that does not exist answer 404 for
    // people just as they do for tokens.
    let alice = sign_in(server, "alice@acme.test");
    for (method, path) in [
        ("POST", "/v1/orgs/acme/repos/app/changes/Iffffff99/approve"),
        (
            "DELETE",
            "/v1/orgs/acme/repos/app/changes/Iffffff99/approve",
        ),
    ] {
        let (st, out) = as_person(server, &alice, method, path, None);
        assert_eq!(st, 404, "{method} {path}: {out}");
    }
    for path in [
        "/v1/orgs/acme/repos/app/changes/Iffffff99/land",
        "/v1/orgs/acme/repos/app/changes/Iffffff99/abandon",
    ] {
        let (st, _) = server.post(path, admin, None);
        assert_eq!(st, 404, "{path}");
    }

    // Registering a tree as a change is refused, not recorded: the
    // defensive arm answers an error rather than minting a change for
    // something that cannot land.
    let (st, tree) = server.get(&format!("/v1/orgs/acme/repos/app/tree?at={c1}"), admin);
    assert_eq!(st, 200);
    let tree_oid = tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "tree")
        .unwrap()["oid"]
        .as_str()
        .unwrap();
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": tree_oid})),
    );
    assert!(st >= 400, "a tree registered as a change answered {st}");
    assert!(server.healthy());
}

#[test]
fn landing_creates_the_target_branch_when_it_is_unborn() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-unborn");
    let scratch = Scratch::new("changes-unborn");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(server, admin, "app", "main", "base", &[("a.txt", "1")]);
    branch(server, admin, "app", "feature", "main");
    let tip = commit(
        server,
        admin,
        "app",
        "feature",
        "first on release\n\nChange-Id: Iab5e0001\n",
        &[("release-notes.md", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature", "target": "release"})),
    );
    assert_eq!(st, 201);
    // A bystander aimed at main: the release landing's reconciliation
    // must skip it entirely — different target, nothing to include.
    branch(server, admin, "app", "mainline", "main");
    commit(
        server,
        admin,
        "app",
        "mainline",
        "main work\n\nChange-Id: Ib57a0001\n",
        &[("main-work.rs", "m")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "mainline"})),
    );
    assert_eq!(st, 201);
    let dev = sign_in(server, "dev@acme.test");
    let (st, _) = as_person(
        server,
        &dev,
        "POST",
        "/v1/orgs/acme/repos/app/changes/Iab5e0001/approve",
        None,
    );
    assert_eq!(st, 204);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes/Iab5e0001/land",
        admin,
        None,
    );
    assert_eq!(st, 202);
    let out = wait_until_not(server, admin, "Iab5e0001", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("landed"), "{out}");
    // An unborn target branch is born pointing at the patchset.
    let (_, refs) = server.get("/v1/orgs/acme/repos/app/refs", admin);
    let release = refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "refs/heads/release")
        .expect("release exists")["oid"]
        .as_str()
        .unwrap();
    assert_eq!(release, tip);
    // The main-targeted bystander is untouched by the release landing.
    let (st, out) = server.get("/v1/orgs/acme/repos/app/changes/Ib57a0001", admin);
    assert_eq!(st, 200);
    assert_eq!(out["change"]["state"], serde_json::json!("open"), "{out}");
}

#[test]
fn the_conversation_carries_the_review_not_just_the_verdict() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-comments");
    let scratch = Scratch::new("changes-comments");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n")],
    );
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "talkative\n\nChange-Id: Ic0dec0de\n",
        &[("core.rs", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);
    let cp = "/v1/orgs/acme/repos/app/changes/Ic0dec0de";

    // A person comments; a service token comments; both are readable in
    // order, each honestly attributed.
    let alice = sign_in(server, "alice@acme.test");
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({"body": "needs a test for the error arm", "path": "core.rs"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["author"], serde_json::json!("Alice"));
    assert_eq!(out["patchset"], serde_json::json!(1));
    let (st, out) = server.post(
        &format!("{cp}/comments"),
        admin,
        Some(serde_json::json!({"body": "ci: perf suite green on patchset 1"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["author"], serde_json::json!("service"));
    let (st, out) = server.get(&format!("{cp}/comments"), admin);
    assert_eq!(st, 200);
    let list = out["comments"].as_array().unwrap();
    assert_eq!(list.len(), 2, "{out}");
    assert_eq!(
        list[0]["body"],
        serde_json::json!("needs a test for the error arm")
    );
    assert_eq!(list[0]["path"], serde_json::json!("core.rs"));
    assert_eq!(list[1]["author"], serde_json::json!("service"));

    // A comment posted before a revision still says which patchset it
    // was about, after the revision arrives.
    commit(
        server,
        admin,
        "app",
        "feature",
        "talkative v2\n\nChange-Id: Ic0dec0de\n",
        &[("core.rs", "v2")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({"body": "much better"})),
    );
    assert_eq!(st, 201);
    assert_eq!(out["patchset"], serde_json::json!(2));
    let (_, out) = server.get(&format!("{cp}/comments"), admin);
    assert_eq!(out["comments"][0]["patchset"], serde_json::json!(1));
    assert_eq!(out["comments"][2]["patchset"], serde_json::json!(2));

    // Refusals: empty words, too many words, a hostile path, an absent
    // change, and nobody at all.
    let (st, _) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({"body": "   "})),
    );
    assert_eq!(st, 400);
    let long = "x".repeat(4001);
    let (st, _) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({"body": long})),
    );
    assert_eq!(st, 400);
    let (st, _) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({"body": "x", "path": "../etc"})),
    );
    assert_eq!(st, 400);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes/Iab5e9999/comments",
        admin,
        Some(serde_json::json!({"body": "into the void"})),
    );
    assert_eq!(st, 404);
    assert_eq!(
        server.status_get(&format!("{cp}/comments"), None),
        401,
        "anonymous read of a private conversation"
    );
    // Absent changes answer 404 on the read side too.
    let (st, _) = server.get("/v1/orgs/acme/repos/app/changes/Iab5e9999/comments", admin);
    assert_eq!(st, 404);

    // Nor may anonymous join one. The refusal names the fix — sign in —
    // and is made at the repository's door, before the change is looked
    // up, so it is the same for a change that does not exist.
    for path in [
        format!("{cp}/comments"),
        "/v1/orgs/acme/repos/app/changes/Iab5e9999/comments".to_string(),
    ] {
        let anon = ureq::post(&format!("{}{path}", server.base))
            .set("Content-Type", "application/json")
            .send_string(&serde_json::json!({"body": "drive-by"}).to_string());
        match anon {
            Err(ureq::Error::Status(st, resp)) => {
                assert_eq!(st, 401, "{path}");
                let text = resp.into_string().unwrap_or_default();
                assert!(text.contains("authentication required"), "{path}: {text}");
            }
            other => panic!("anonymous comment on {path} answered {other:?}"),
        }
    }

    // Cross-org masking, and hostile bytes through the body.
    let rival = server.bootstrap_org("rival");
    let (st, _) = server.post(
        &format!("{cp}/comments"),
        &rival,
        Some(serde_json::json!({"body": "sneaking in"})),
    );
    assert_eq!(st, 404);
    for inj in INJECTIONS {
        let st = server.status_post(
            &format!("{cp}/comments"),
            admin,
            serde_json::json!({"body": inj}),
        );
        assert!(st == 201 || st == 400, "injection body answered {st}");
    }
    assert!(server.healthy());
}

/// Protection is the fence around review: with it up, every write door
/// refuses in the same words, the land queue still moves trunk, and
/// taking it down restores direct pushes.
#[test]
fn protected_branches_move_only_through_the_land_queue() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-protect");
    let scratch = Scratch::new("changes-protect");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n"), ("README.md", "hello")],
    );
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "gateway\n\nChange-Id: I90c00001\n",
        &[("gateway.rs", "v1")],
    );

    // Protect main: 201 the first time, an ack the second.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/protections",
        admin,
        Some(serde_json::json!({"branch": "main"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/protections",
        admin,
        Some(serde_json::json!({"branch": "main"})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, out) = server.get("/v1/orgs/acme/repos/app/protections", admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["protections"][0]["branch"], serde_json::json!("main"));

    const REFUSAL: &str = "branch 'main' is protected: land through review";

    // Door 1: the commits API.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        admin,
        Some(serde_json::json!({
            "branch": "main",
            "message": "sneak",
            "operations": [{"op": "put", "path": "sneak.rs", "content": "x"}],
        })),
    );
    assert_eq!(st, 403, "{out}");
    assert_eq!(out["error"], serde_json::json!(REFUSAL));

    // Doors 2–4: reset, revert, branch deletion.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/reset",
        admin,
        Some(serde_json::json!({"branch": "main", "to": "main"})),
    );
    assert_eq!(st, 403, "{out}");
    assert_eq!(out["error"], serde_json::json!(REFUSAL));
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/revert",
        admin,
        Some(serde_json::json!({"branch": "main"})),
    );
    assert_eq!(st, 403, "{out}");
    assert_eq!(out["error"], serde_json::json!(REFUSAL));
    let (st, out) = server.req(
        "DELETE",
        "/v1/orgs/acme/repos/app/branches/main",
        admin,
        None,
    );
    assert_eq!(st, 403, "{out}");
    assert_eq!(out["error"], serde_json::json!(REFUSAL));

    // Door 5: the real git CLI over smart HTTP. The refusal is in-band
    // (`ng` → "[remote rejected]"), quoting the same sentence.
    let url = server.authed_url(admin, "acme", "app");
    let clone = scratch.path().join("wire-clone");
    gitcli::clone_and_fsck(&url, &clone);
    std::fs::write(clone.join("direct.rs"), "pub fn nope() {}\n").unwrap();
    gitcli::git(&clone, &["add", "."]);
    gitcli::git(&clone, &["commit", "-q", "-m", "direct to trunk"]);
    let err = gitcli::git_expect_err(&clone, &["push", "-q", "origin", "main"]).unwrap();
    assert!(err.contains(REFUSAL), "{err}");
    assert!(err.contains("remote rejected"), "{err}");
    // An unprotected branch from the same clone still lands fine.
    gitcli::git(
        &clone,
        &["push", "-q", "origin", "HEAD:refs/heads/scratchpad"],
    );

    // The queue is the one writer left: approve and land the change.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let ps_commit = out["patchset"]["commit"].as_str().unwrap().to_string();
    let alice = sign_in(server, "alice@acme.test");
    let (st, _) = as_person(
        server,
        &alice,
        "POST",
        "/v1/orgs/acme/repos/app/changes/I90c00001/approve",
        None,
    );
    assert_eq!(st, 204);
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes/I90c00001/land",
        admin,
        None,
    );
    assert_eq!(st, 202, "{out}");
    let out = wait_until_not(server, admin, "I90c00001", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("landed"), "{out}");
    let (st, refs) = server.get("/v1/orgs/acme/repos/app/refs", admin);
    assert_eq!(st, 200);
    let main_tip = refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "refs/heads/main")
        .unwrap()["oid"]
        .as_str()
        .unwrap();
    assert_eq!(main_tip, ps_commit, "the queue moved protected trunk");
    let clone2 = scratch.path().join("clone-after-protected-land");
    gitcli::clone_and_fsck(&url, &clone2);

    // Down comes the fence: direct writes work again, and a second
    // removal says there was nothing to remove.
    let (st, _) = server.req(
        "DELETE",
        "/v1/orgs/acme/repos/app/protections/main",
        admin,
        None,
    );
    assert_eq!(st, 204);
    commit(
        server,
        admin,
        "app",
        "main",
        "direct again",
        &[("direct.rs", "ok")],
    );
    let (st, out) = server.req(
        "DELETE",
        "/v1/orgs/acme/repos/app/protections/main",
        admin,
        None,
    );
    assert_eq!(st, 404, "{out}");

    // Both authority moves are on the audit trail.
    let (st, audit) = server.get("/v1/orgs/acme/audit?limit=100", admin);
    assert_eq!(st, 200, "{audit}");
    let actions: Vec<&str> = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["action"].as_str())
        .collect();
    assert!(actions.contains(&"repo.protect"), "{actions:?}");
    assert!(actions.contains(&"repo.unprotect"), "{actions:?}");
    assert!(server.healthy());
}

/// The fence has a gate with a lock: only an admin moves protection or
/// the default branch, shapes are checked at the door, and mirrors have
/// nothing here to configure.
#[test]
fn protection_and_default_branch_are_admin_only_and_shape_checked() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-protadmin");
    let scratch = Scratch::new("changes-protadmin");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(server, admin, "app", "main", "seed", &[("README.md", "x")]);

    // A member (write, not admin) is masked away from both controls,
    // exactly like a foreign token — settings must not leak shape.
    let dev = sign_in(server, "dev@acme.test");
    let (st, _) = as_person(
        server,
        &dev,
        "POST",
        "/v1/orgs/acme/repos/app/protections",
        Some(serde_json::json!({"branch": "main"})),
    );
    assert_eq!(st, 404, "a non-admin protect is masked");
    let (st, _) = as_person(
        server,
        &dev,
        "DELETE",
        "/v1/orgs/acme/repos/app/protections/main",
        None,
    );
    assert_eq!(st, 404);
    let (st, _) = as_person(
        server,
        &dev,
        "PATCH",
        "/v1/orgs/acme/repos/app",
        Some(serde_json::json!({"default_branch": "main"})),
    );
    assert_eq!(st, 404);
    // Reading the list only takes repo read — a reviewer should see the
    // fence that will refuse them before they push.
    let (st, out) = as_person(
        server,
        &dev,
        "GET",
        "/v1/orgs/acme/repos/app/protections",
        None,
    );
    assert_eq!(st, 200, "{out}");

    // Shapes and existence, refused in words.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/protections",
        admin,
        Some(serde_json::json!({"branch": "no/such/branch"})),
    );
    assert_eq!(st, 404, "{out}");
    assert!(out["error"].as_str().unwrap().contains("unknown branch"));
    for bad in ["", "a..b", "/lead", "b~1", "sp ace"] {
        let (st, out) = server.post(
            "/v1/orgs/acme/repos/app/protections",
            admin,
            Some(serde_json::json!({"branch": bad})),
        );
        assert_eq!(st, 400, "{bad:?}: {out}");
    }
    for inj in INJECTIONS {
        let st = server.status_post(
            "/v1/orgs/acme/repos/app/protections",
            admin,
            serde_json::json!({"branch": inj}),
        );
        assert!(st == 400 || st == 404, "injection branch answered {st}");
        let (st, _) = server.req(
            "PATCH",
            "/v1/orgs/acme/repos/app",
            admin,
            Some(serde_json::json!({"default_branch": inj})),
        );
        assert!(st == 400 || st == 404, "injection default answered {st}");
    }

    // Unprotecting something never protected is a 404, not a shrug.
    let (st, _) = server.req(
        "DELETE",
        "/v1/orgs/acme/repos/app/protections/never",
        admin,
        None,
    );
    assert_eq!(st, 404);

    // Cross-org: everything here is masked for a rival org's token.
    let rival = server.bootstrap_org("rival");
    let (st, _) = server.get("/v1/orgs/acme/repos/app/protections", &rival);
    assert_eq!(st, 404);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/protections",
        &rival,
        Some(serde_json::json!({"branch": "main"})),
    );
    assert_eq!(st, 404);
    assert!(server.healthy());
}

/// The default branch is where clones start and changes land by
/// default; moving it is admin work and the move is visible everywhere.
#[test]
fn the_default_branch_governs_where_changes_land() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-defbranch");
    let scratch = Scratch::new("changes-defbranch");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "seed",
        &[("OWNERS", "alice@acme.test\n")],
    );
    branch(server, admin, "app", "trunk", "main");
    branch(server, admin, "app", "feature", "trunk");
    commit(
        server,
        admin,
        "app",
        "feature",
        "work\n\nChange-Id: Idb010001\n",
        &[("w.rs", "w")],
    );

    // Unknown and invalid targets are refused before anything moves.
    let (st, out) = server.req(
        "PATCH",
        "/v1/orgs/acme/repos/app",
        admin,
        Some(serde_json::json!({"default_branch": "ghost"})),
    );
    assert_eq!(st, 404, "{out}");
    let (st, out) = server.req(
        "PATCH",
        "/v1/orgs/acme/repos/app",
        admin,
        Some(serde_json::json!({"default_branch": "a..b"})),
    );
    assert_eq!(st, 400, "{out}");

    // The move itself, reflected in the repo JSON.
    let (st, out) = server.req(
        "PATCH",
        "/v1/orgs/acme/repos/app",
        admin,
        Some(serde_json::json!({"default_branch": "trunk"})),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["default_branch"], serde_json::json!("trunk"));
    let (st, out) = server.get("/v1/orgs/acme/repos/app", admin);
    assert_eq!(st, 200);
    assert_eq!(out["default_branch"], serde_json::json!("trunk"));

    // A change registered with no explicit target now aims at trunk.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(
        out["change"]["target_branch"],
        serde_json::json!("trunk"),
        "{out}"
    );

    // And the audit trail shows who moved the default.
    let (st, audit) = server.get("/v1/orgs/acme/audit?limit=50", admin);
    assert_eq!(st, 200);
    assert!(audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["action"] == "repo.default_branch"));
    assert!(server.healthy());
}

/// A line comment is review at the resolution the work happens at:
/// anchored to a file and a line of the patchset, bounded, and refused
/// in words when the anchor is nonsense.
#[test]
fn line_comments_anchor_to_a_file_and_line_of_the_patchset() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-lines");
    let scratch = Scratch::new("changes-lines");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "seed",
        &[("OWNERS", "alice@acme.test\n")],
    );
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "fees\n\nChange-Id: I11de0001\n",
        &[("fees.rs", "fn fee() -> u32 {\n    41\n}\n")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/I11de0001";

    // The anchor rules, refused in words at the door.
    let (st, out) = server.post(
        &format!("{cp}/comments"),
        admin,
        Some(serde_json::json!({"body": "why 41?", "line": 2})),
    );
    assert_eq!(st, 400, "{out}");
    assert!(out["error"].as_str().unwrap().contains("needs a path"));
    for bad in [0i64, -3, 1_000_001] {
        let (st, out) = server.post(
            &format!("{cp}/comments"),
            admin,
            Some(serde_json::json!({"body": "x", "path": "fees.rs", "line": bad})),
        );
        assert_eq!(st, 400, "{bad}: {out}");
        assert!(out["error"].as_str().unwrap().contains("line must be"));
    }

    // A person, on the line with the bug.
    let alice = sign_in(server, "alice@acme.test");
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(
            serde_json::json!({"body": "off by one — should be 42", "path": "fees.rs", "line": 2}),
        ),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["path"], serde_json::json!("fees.rs"));
    assert_eq!(out["line"], serde_json::json!(2));
    assert_eq!(out["author"], serde_json::json!("Alice"));
    assert_eq!(out["patchset"], serde_json::json!(1));

    // A plain prose comment coexists, unanchored.
    let (st, out) = server.post(
        &format!("{cp}/comments"),
        admin,
        Some(serde_json::json!({"body": "overall shape looks right"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["line"], serde_json::Value::Null);

    // The conversation carries the anchors back.
    let (st, out) = server.get(&format!("{cp}/comments"), admin);
    assert_eq!(st, 200, "{out}");
    let comments = out["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 2);
    assert_eq!(comments[0]["line"], serde_json::json!(2));
    assert_eq!(comments[1]["line"], serde_json::Value::Null);
    assert!(server.healthy());
}

/// The regression CI caught in the racing-pushes test: an *unpinned*
/// commit ("on top of whatever the branch points at now") answered 409
/// when the lander moved trunk between the handler's read and its CAS.
/// The caller pinned nothing a 409 could tell them about, so the API
/// now re-reads and rebuilds, exactly like the wire path's CAS loop.
/// Two rival writers hammering one branch must all land; a caller who
/// DID pin a parent still gets the strict conflict.
#[test]
fn unpinned_commits_from_rival_writers_all_land() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-hammer");
    let scratch = Scratch::new("changes-hammer");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(server, admin, "app", "main", "base", &[("a.txt", "0")]);

    // Two writers, eight unpinned commits each, same branch, no
    // coordination. Without the retry loop the losing side of each CAS
    // window answers 409; with it, every commit must land.
    std::thread::scope(|scope| {
        for writer in 0..2 {
            let server = &w.server;
            let admin = w.admin.clone();
            scope.spawn(move || {
                for i in 0..8 {
                    let (st, out) = server.post(
                        "/v1/orgs/acme/repos/app/commits",
                        &admin,
                        Some(serde_json::json!({
                            "branch": "main",
                            "message": format!("writer {writer} commit {i}"),
                            "operations": [{
                                "op": "put",
                                "path": format!("w{writer}/f{i}.txt"),
                                "content": format!("{writer}:{i}"),
                            }],
                        })),
                    );
                    assert_eq!(st, 201, "writer {writer} commit {i}: {out}");
                }
            });
        }
    });

    // All sixteen commits are on the branch, serialized by the CAS.
    let (st, log) = server.get("/v1/orgs/acme/repos/app/log?limit=50", admin);
    assert_eq!(st, 200, "{log}");
    let messages: Vec<String> = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["message"].as_str().map(str::to_string))
        .collect();
    for writer in 0..2 {
        for i in 0..8 {
            let want = format!("writer {writer} commit {i}");
            assert!(
                messages.iter().any(|m| m.starts_with(&want)),
                "{want} missing from the log: {messages:?}"
            );
        }
    }

    // A pinned parent keeps strict optimistic concurrency: staleness is
    // the answer, not something to paper over with retries.
    let stale = "1".repeat(40);
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        admin,
        Some(serde_json::json!({
            "branch": "main",
            "expected_parent": stale,
            "message": "stale pin",
            "operations": [{"op": "put", "path": "x.txt", "content": "x"}],
        })),
    );
    assert_eq!(st, 409, "{out}");
    assert!(out["current_tip"].is_string(), "{out}");

    // The history the hammer produced is one git trusts (I11).
    let clone = scratch.path().join("hammer-clone");
    gitcli::clone_and_fsck(&server.authed_url(admin, "acme", "app"), &clone);
    assert!(server.healthy());
}

/// CI is a reviewer with a badge: it reports checks per patchset, a
/// failing check refuses the land request in words, greening it opens
/// the gate, and a revision resets the machine's verdict exactly like
/// the humans'.
#[test]
fn ci_checks_gate_landing_and_reset_per_patchset() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-cichecks");
    let scratch = Scratch::new("changes-cichecks");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n")],
    );
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "gateway\n\nChange-Id: Ic1c40001\n",
        &[("gateway.rs", "v1")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/Ic1c40001";

    // The org service token reports checks — reporting a build is a
    // machine's job (the same token is refused for approvals).
    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "pending"})),
    );
    assert_eq!(st, 201, "{out}");
    // Re-posting the same name updates in place: 200, not a second row.
    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({
            "name": "ci/tests", "state": "failing",
            "url": "https://ci.example.com/run/812",
        })),
    );
    assert_eq!(st, 200, "{out}");
    let (st, out) = server.get(&format!("{cp}/checks"), admin);
    assert_eq!(st, 200, "{out}");
    let checks = out["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 1, "{out}");
    assert_eq!(checks[0]["state"], serde_json::json!("failing"));
    assert_eq!(
        checks[0]["url"],
        serde_json::json!("https://ci.example.com/run/812")
    );

    // Approved by the owner — the human verdict passes — but the
    // machine's red still refuses, in exactly these words.
    let alice = sign_in(server, "alice@acme.test");
    let (st, _) = as_person(server, &alice, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204);
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"],
        serde_json::json!("blocked: check 'ci/tests' is failing")
    );

    // Green it and the same request queues; the change lands.
    let (st, _) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "passing"})),
    );
    assert_eq!(st, 200);
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202, "{out}");
    let out = wait_until_not(server, admin, "Ic1c40001", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("landed"), "{out}");

    // Checks on a landed change are reports about nothing: refused.
    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "passing"})),
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["error"], serde_json::json!("change is landed"));

    // A new change with a revision: checks pin to the patchset that was
    // built, and patchset 2 starts with none.
    branch(server, admin, "app", "feature2", "main");
    commit(
        server,
        admin,
        "app",
        "feature2",
        "fees\n\nChange-Id: Ic1c40002\n",
        &[("fees.rs", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature2"})),
    );
    assert_eq!(st, 201);
    let cp2 = "/v1/orgs/acme/repos/app/changes/Ic1c40002";
    let (st, _) = server.post(
        &format!("{cp2}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "passing"})),
    );
    assert_eq!(st, 201);
    commit(
        server,
        admin,
        "app",
        "feature2",
        "fees v2\n\nChange-Id: Ic1c40002\n",
        &[("fees.rs", "v2")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature2"})),
    );
    assert_eq!(st, 201, "second patchset registers");
    let (st, out) = server.get(&format!("{cp2}/checks"), admin);
    assert_eq!(st, 200);
    assert_eq!(out["patchset"], serde_json::json!(2));
    assert_eq!(out["checks"].as_array().unwrap().len(), 0, "{out}");

    // Shapes and masking: hostile names and urls are refused in words;
    // rivals and anonymous callers see nothing.
    for (body, why) in [
        (
            serde_json::json!({"name": "sp ace", "state": "passing"}),
            "name",
        ),
        (
            serde_json::json!({"name": "ci/x", "state": "green"}),
            "state",
        ),
        (
            serde_json::json!({"name": "ci/x", "state": "passing", "url": "ftp://x"}),
            "url",
        ),
    ] {
        let (st, out) = server.post(&format!("{cp2}/checks"), admin, Some(body));
        assert_eq!(st, 400, "{why}: {out}");
    }
    for inj in INJECTIONS {
        let st = server.status_post(
            &format!("{cp2}/checks"),
            admin,
            serde_json::json!({"name": inj, "state": "passing"}),
        );
        assert!(st == 201 || st == 200 || st == 400, "injection name: {st}");
    }
    let rival = server.bootstrap_org("rival");
    let (st, _) = server.post(
        &format!("{cp2}/checks"),
        &rival,
        Some(serde_json::json!({"name": "ci/x", "state": "failing"})),
    );
    assert_eq!(st, 404);
    let (st, _) = server.get(&format!("{cp2}/checks"), &rival);
    assert_eq!(st, 404);
    // An unknown change answers absence to a caller who CAN see the
    // repo, too — the mask is about the change, not just the org.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes/Iabsent99/checks",
        admin,
        Some(serde_json::json!({"name": "ci/x", "state": "failing"})),
    );
    assert_eq!(st, 404, "{out}");
    let (st, _) = server.get("/v1/orgs/acme/repos/app/changes/Iabsent99/checks", admin);
    assert_eq!(st, 404);
    assert!(server.healthy());
}

/// CI that turns red between enqueue and claim counts: the lander
/// re-reads the checks at claim time and ejects with the check's name.
///
/// The check here is one nobody *required*, and a failing one of those
/// still blocks — that is the promise that nothing which blocked before
/// required checks existed stops blocking now.
#[test]
fn a_check_turning_red_ejects_at_claim_time() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-redcheck");
    let scratch = Scratch::new("changes-redcheck");
    let db_url = stratum_testkit::pg::test_db_url("changes-redcheck");
    // Server A has no lander, so enqueue → red is an ordering, not a race.
    let a = spawn_with(&bucket.base_url, &scratch, Some(&db_url), "0");
    let w = world(a);
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n")],
    );
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "risky\n\nChange-Id: Ic1c40003\n",
        &[("risky.rs", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);
    let cp = "/v1/orgs/acme/repos/app/changes/Ic1c40003";
    let alice = sign_in(server, "alice@acme.test");
    let (st, _) = as_person(server, &alice, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204);
    let (st, _) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202);
    // Now CI reports red — while the change is already in the queue.
    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/perf", "state": "failing"})),
    );
    assert_eq!(st, 201, "{out}");

    // Server B's lander claims the job and must see the red.
    let scratch_b = Scratch::new("changes-redcheck-b");
    let server_b = spawn_with(&bucket.base_url, &scratch_b, Some(&db_url), "0.2");
    let out = wait_until_not(server, admin, "Ic1c40003", "landing");
    drop(server_b);
    assert_eq!(out["change"]["state"], serde_json::json!("open"), "{out}");
    assert_eq!(
        out["change"]["land_verdict"],
        // The wording moved with the gate: the ejection reason is now
        // `LandGate::Blocked`'s sentence verbatim, so one vocabulary
        // covers required and non-required checks alike instead of the
        // lander inventing a second phrasing for half the cases.
        serde_json::json!("ejected: check 'ci/perf' is failing")
    );
    assert!(server.healthy());
}

// --- required checks: the queue holds rather than guessing -----------

/// Require a check on a branch, the way an admin would.
fn require_check(server: &Server, admin: &str, repo: &str, branch: &str, name: &str) {
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/required-checks/{branch}"),
        admin,
        Some(serde_json::json!({ "name": name })),
    );
    assert!(st == 201 || st == 200, "require {name} on {branch}: {out}");
}

/// Protect `main` and require `checks` on it, then open an approved
/// change on `feature` — the common ground under every test below.
fn approved_change_on_protected_main(
    server: &Server,
    admin: &str,
    key: &str,
    checks: &[&str],
) -> String {
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/protections",
        admin,
        Some(serde_json::json!({"branch": "main"})),
    );
    assert!(st == 201 || st == 200, "protect main: {out}");
    for name in checks {
        require_check(server, admin, "app", "main", name);
    }
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        &format!("work\n\nChange-Id: {key}\n"),
        &[("gateway.rs", "v1")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = format!("/v1/orgs/acme/repos/app/changes/{key}");
    let alice = sign_in(server, "alice@acme.test");
    let (st, out) = as_person(server, &alice, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204, "{out}");
    cp
}

/// The change is *holding*: still `landing` across enough lander cycles
/// that a lander which was going to move it would have, and saying why
/// on every single one of them.
///
/// Asserted on every sample rather than once at the end, because "did
/// not land" is the assertion that is easiest to write vacuously — a
/// single poll that catches the change mid-flight would pass against a
/// lander that ejected it a millisecond later.
///
/// **A cycle is a recheck, and it is waited for, not slept through.**
/// This used to sleep 1.4 s per cycle, chosen against the one-second
/// poll — but the interval that actually moves a held change is
/// `STRATUM_LAND_RECHECK_SECS`: a hold completes its job and says
/// nothing, and `lander::reap` re-enqueues once the change has been idle
/// that long (see the module docs on `readopt`). Recheck plus poll can
/// exceed 1.4 s on a loaded machine, so the old wait could contain no
/// recheck at all and the "N cycles" claim was decoration.
///
/// The observable is the change's `land_job_id`: `readopt` creates a
/// fresh job and CASes it onto the change, so a new id is the lander
/// saying "I looked again". There is no API for it — `set_land_waiting`
/// deliberately leaves `updated_at` alone, precisely because a change
/// that is merely waiting has not been modified — so this is one of the
/// few reads in this file that goes to the control plane rather than
/// through a credential. Everything asserted about the *product* still
/// comes from the API, once per cycle, as it was.
fn assert_holding(server: &Server, admin: &str, key: &str, note: &str, cycles: u32, db_url: &str) {
    let path = format!("/v1/orgs/acme/repos/app/changes/{key}");
    let db = stratum_control::ControlDb::open(db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .expect("the org")
        .id;
    let repo_id = stratum_control::registry::repo_by_name(&db, &org_id, "app")
        .unwrap()
        .expect("the repo")
        .id;
    let land_job = || {
        stratum_control::changes::by_key(&db, &repo_id, key)
            .unwrap()
            .expect("the change")
            .land_job_id
    };
    // First, wait for the lander to have looked at all — the observable
    // the rest of this depends on, rather than a sleep guessed against
    // the poll interval. Until the note is there, "still landing" is
    // just "the job has not been claimed yet", which proves nothing.
    wait_until(
        &format!("the lander to record that it is waiting on {note:?}"),
        Duration::from_secs(30),
        || {
            let (st, out) = server.get(&path, admin);
            assert_eq!(st, 200, "{out}");
            if out["change"]["land_verdict"] == serde_json::json!(note) {
                return true;
            }
            assert_eq!(
                out["change"]["state"],
                serde_json::json!("landing"),
                "the change left the queue before ever saying it was waiting: {out}"
            );
            false
        },
    );
    // Now hold it under observation. Every sample is asserted, not just
    // the last: a lander that ejected a held change one cycle after the
    // note appeared would pass a single end-of-wait check.
    let mut seen = land_job();
    for i in 0..cycles {
        // The wait probes the control plane, not the API: it runs every
        // few milliseconds and an HTTP round trip per probe would make
        // the test its own load. The *product* is then asked once per
        // cycle, through a credential, exactly as before.
        wait_until(
            &format!("recheck {i} of a change held on {note:?}"),
            Duration::from_secs(60),
            || {
                let now = land_job();
                let rechecked = now != seen;
                if rechecked {
                    seen = now;
                }
                rechecked
            },
        );
        let (st, out) = server.get(&path, admin);
        assert_eq!(st, 200, "{out}");
        assert_eq!(
            out["change"]["state"],
            serde_json::json!("landing"),
            "cycle {i}: the queue gave up on a change whose check is still running: {out}"
        );
        assert_eq!(
            out["change"]["land_verdict"],
            serde_json::json!(note),
            "cycle {i}: a held change must say what it is holding for: {out}"
        );
    }
}

/// The checks route reports **what the branch requires**, not only what
/// reported — because a required check that never ran has no row.
///
/// This is the defect the review page shipped with. `checks_for_change`
/// answers with the merged rows, and a required name nobody has posted
/// against is not among them: that is what never-reported means. So a
/// page counting the rows saw one green check and announced "all checks
/// have passed" over a change the land queue was holding on a second
/// name it had never heard from — and enabled the Land button under it.
/// The two statements came from one server, one request apart.
///
/// The gate has always read both halves (`merged_checks` returns them
/// together for exactly this reason). This asserts a *reader* can too.
#[test]
fn the_checks_route_names_a_required_check_that_has_never_reported() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-reqmissing");
    let scratch = Scratch::new("changes-reqmissing");
    let (a, _db_url) = spawn_holding(&bucket.base_url, &scratch, "1800", "1");
    let w = world(a);
    let (server, admin) = (&w.server, &w.admin);
    // Two required checks. Only one of them will ever say anything.
    let cp =
        approved_change_on_protected_main(server, admin, "Ic1c50101", &["ci/local", "ci/tests"]);

    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/local", "state": "passing"})),
    );
    assert_eq!(st, 201, "{out}");

    let (st, out) = server.get(&format!("{cp}/checks"), admin);
    assert_eq!(st, 200, "{out}");

    // The rows are still only what reported — that part was never wrong.
    let names: Vec<&str> = out["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .map(|c| c["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["ci/local"], "{out}");

    // And the requirement list carries the name that has no row, which
    // is the only place a reader can learn it exists.
    let mut required: Vec<&str> = out["required_checks"]
        .as_array()
        .expect("required_checks must be on the wire: {out}")
        .iter()
        .map(|c| c.as_str().expect("required name"))
        .collect();
    required.sort_unstable();
    assert_eq!(
        required,
        vec!["ci/local", "ci/tests"],
        "the route must name every check the target branch requires, \
         including the one that has never reported: {out}"
    );

    // The gate agrees, which is the point: the page and the queue are
    // now reading the same fact rather than two different ones.
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202, "{out}");
    assert_eq!(out["gate"], serde_json::json!("waiting"), "{out}");
    assert_eq!(out["waiting_on"], serde_json::json!(["ci/tests"]), "{out}");

    assert!(server.healthy());
}

/// The distinction the whole gate exists for: a required check that has
/// not reached a verdict is neither a pass nor a refusal. The change
/// waits — visibly, saying what it waits on — and lands the moment the
/// check goes green, without anybody re-requesting the landing.
#[test]
fn a_change_holds_for_a_running_required_check_and_lands_when_it_passes() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-holdrun");
    let scratch = Scratch::new("changes-holdrun");
    // A long budget and a one-second recheck: this test is about the
    // hold resuming, not about it expiring.
    let (a, db_url) = spawn_holding(&bucket.base_url, &scratch, "1800", "1");
    let w = world(a);
    let (server, admin) = (&w.server, &w.admin);
    let cp = approved_change_on_protected_main(server, admin, "Ic1c50001", &["ci/tests"]);

    // The build has been queued and has said nothing yet — the state
    // that used to read as "nothing has failed" and let the change
    // straight through before its build had finished. (`pending` and not
    // `running`: the intake accepts three states, and the gate treats
    // every one of them that is not a verdict the same way.)
    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "pending"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202, "{out}");

    // Five lander cycles at a one-second poll: it is holding, not racing.
    assert_holding(
        server,
        admin,
        "Ic1c50001",
        "waiting on ci/tests",
        5,
        &db_url,
    );

    // Green, and it lands on its own — the hold is a queue, not a
    // refusal, so nobody has to press land again.
    //
    // This half is also the reaper's test, and the only one it can
    // have. A held change ends its job and enqueues nothing; the reaper
    // sweeping `landing` changes with no live job is the *only* thing
    // that can bring this one back. If the reaper does not run, does not
    // find it, or fails to adopt the job it makes, this change waits
    // here until the deadline and the test says so.
    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "passing"})),
    );
    assert_eq!(st, 200, "{out}");
    let out = wait_until_not(server, admin, "Ic1c50001", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("landed"), "{out}");
    assert_eq!(out["change"]["land_verdict"], serde_json::json!("landed"));
    assert!(server.healthy());
}

/// A hold is bounded. A check that reports `pending` and then never
/// speaks again — a deleted workflow, an installation that lost
/// `actions: read`, a CI system that was turned off — must not park a
/// change in the queue forever, and when the queue gives up it says how
/// long it waited and which check never arrived.
#[test]
fn a_required_check_that_never_reports_is_ejected_by_the_timeout_naming_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-holdtimeout");
    let scratch = Scratch::new("changes-holdtimeout");
    let (a, _db_url) = spawn_holding(&bucket.base_url, &scratch, "4", "1");
    let w = world(a);
    let (server, admin) = (&w.server, &w.admin);
    let cp = approved_change_on_protected_main(server, admin, "Ic1c50002", &["ci/tests"]);
    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "pending"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202, "{out}");

    let out = wait_until_not(server, admin, "Ic1c50002", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("open"), "{out}");
    let verdict = out["change"]["land_verdict"].as_str().unwrap_or_default();
    assert!(
        verdict.starts_with("ejected: waited "),
        "the eject must say it timed out, not just that it failed: {verdict:?}"
    );
    assert!(
        verdict.ends_with("for ci/tests, which never reported"),
        "the eject must name the check that never arrived: {verdict:?}"
    );
    // The budget is measured across the whole hold, not restarted by
    // each re-enqueue — a wait that reset every cycle would never
    // expire, and this is the assertion that would catch it.
    assert!(
        !verdict.contains("waited 0s"),
        "the wait was measured from the last hop, not from the landing: {verdict:?}"
    );
    assert!(server.healthy());
}

/// Waiting never rescues a change something has already said no to. A
/// required check that turns red while the change is *holding* ejects it
/// at the next look, naming the check.
#[test]
fn a_required_check_that_turns_red_while_holding_ejects_naming_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-holdred");
    let scratch = Scratch::new("changes-holdred");
    let (a, db_url) = spawn_holding(&bucket.base_url, &scratch, "1800", "1");
    let w = world(a);
    let (server, admin) = (&w.server, &w.admin);
    let cp = approved_change_on_protected_main(server, admin, "Ic1c50003", &["ci/perf"]);
    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/perf", "state": "pending"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202, "{out}");
    assert_holding(server, admin, "Ic1c50003", "waiting on ci/perf", 2, &db_url);

    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/perf", "state": "failing"})),
    );
    assert_eq!(st, 200, "{out}");
    let out = wait_until_not(server, admin, "Ic1c50003", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("open"), "{out}");
    assert_eq!(
        out["change"]["land_verdict"],
        serde_json::json!("ejected: required check 'ci/perf' is failing"),
        "{out}"
    );
    assert!(server.healthy());
}

/// The regression guard, and the most important test here: a repository
/// that has not opted in must behave *exactly* as it did before required
/// checks existed. A protected branch with no required checks, and a
/// change with no checks at all, lands — it does not wait for a verdict
/// nobody asked for, which would turn this work into a silent outage for
/// every project already using the queue.
#[test]
fn a_protected_branch_with_no_required_checks_lands_exactly_as_before() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-noreq");
    let scratch = Scratch::new("changes-noreq");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let cp = approved_change_on_protected_main(server, admin, "Ic1c50004", &[]);
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202, "{out}");
    let out = wait_until_not(server, admin, "Ic1c50004", "landing");
    assert_eq!(
        out["change"]["state"],
        serde_json::json!("landed"),
        "a repository that never opted in was held by a gate it did not \
         ask for: {out}"
    );

    // And a check that is merely *reported* — not required — does not
    // gate either, in any of the states that are not `failing`. A
    // pending check nobody required has never blocked a landing and
    // must not start.
    branch(server, admin, "app", "feature2", "main");
    commit(
        server,
        admin,
        "app",
        "feature2",
        "more\n\nChange-Id: Ic1c50005\n",
        &[("fees.rs", "v1")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature2"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp2 = "/v1/orgs/acme/repos/app/changes/Ic1c50005";
    let alice = sign_in(server, "alice@acme.test");
    let (st, _) = as_person(server, &alice, "POST", &format!("{cp2}/approve"), None);
    assert_eq!(st, 204);
    let (st, out) = server.post(
        &format!("{cp2}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/optional", "state": "pending"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(&format!("{cp2}/land"), admin, None);
    assert_eq!(st, 202, "{out}");
    let out = wait_until_not(server, admin, "Ic1c50005", "landing");
    assert_eq!(
        out["change"]["state"],
        serde_json::json!("landed"),
        "a check nobody required held the queue: {out}"
    );
    assert!(server.healthy());
}

/// A land job that outlives the **sweep** that purged its repository.
///
/// Written to reach the lander's "change vanished" guard, and what it
/// found instead is the finding: it cannot. That guard's comment claimed
/// the repo and its changes vanish by cascade, and `changes.repo_id`
/// does carry `ON DELETE CASCADE` — but nothing in the product deletes a
/// `repos` row. `delete_repo` writes a tombstone and the GC sweep purges
/// the object storage and marks the row; the row itself, and every
/// change hanging off it, stays for ever. The comment was an untested
/// claim about what the database does, and it was wrong.
///
/// So this pins what actually happens: the sweep runs to completion, the
/// repository is gone from every read, and the lander retires the job on
/// the **repo** guard. What must happen is nothing dramatic — the job
/// settles, and the server that claimed it is still serving afterwards.
/// A worker that wedges on a row somebody deleted takes every later
/// landing with it.
#[test]
fn a_land_job_whose_change_was_swept_away_retires_without_wedging_the_lander() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-swept");
    let scratch = Scratch::new("changes-swept");
    let db_url = stratum_testkit::pg::test_db_url("changes-swept");

    // No lander, and a sweep that does not wait: the ordering below is a
    // fact rather than a race.
    let a = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .data_dir(scratch.path().join("data"))
        .db_url(&db_url)
        .env("STRATUM_LAND_POLL_SECS", "0")
        .env("STRATUM_GC_SECS", "1")
        .env("STRATUM_GC_GRACE_SECS", "0")
        .start();
    let w = world(a);
    let (server, admin) = (&w.server, &w.admin);

    let (st, _) = server.post(
        "/v1/orgs/acme/repos",
        admin,
        Some(serde_json::json!({"name": "swept"})),
    );
    assert_eq!(st, 201);
    commit(server, admin, "swept", "main", "base", &[("a.txt", "1")]);
    branch(server, admin, "swept", "feature", "main");
    commit(
        server,
        admin,
        "swept",
        "feature",
        "swept away\n\nChange-Id: Ic0ffee01\n",
        &[("gone.rs", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/swept/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);
    let dev = sign_in(server, "dev@acme.test");
    let (st, _) = as_person(
        server,
        &dev,
        "POST",
        "/v1/orgs/acme/repos/swept/changes/Ic0ffee01/approve",
        None,
    );
    assert_eq!(st, 204);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/swept/changes/Ic0ffee01/land",
        admin,
        None,
    );
    assert_eq!(
        st, 202,
        "the job has to exist before the change stops doing"
    );

    // The job's id, taken **before** the change is gone. Without it the
    // only observable left is another repository's queue, which is empty
    // whether or not this job was ever claimed — an assertion that would
    // have passed against a lander that never woke up.
    let db = stratum_control::ControlDb::open(&db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .unwrap()
        .id;
    let swept_repo = stratum_control::registry::repo_by_name(&db, &org_id, "swept")
        .unwrap()
        .expect("the repo")
        .id;
    let land_job = stratum_control::changes::by_key(&db, &swept_repo, "Ic0ffee01")
        .unwrap()
        .expect("the change")
        .land_job_id
        .expect("landing means a job");

    let (st, _) = server.req("DELETE", "/v1/orgs/acme/repos/swept", admin, None);
    assert_eq!(st, 204, "delete swept");

    // Wait until the repository is gone from every read the product
    // makes — `repo_by_name` filters tombstones — so the lander below is
    // claiming against a repository that no longer exists as far as
    // anything can tell. Asserted against the control plane because
    // there is no API for it: after the delete every route answers 404
    // whether the sweep has run or not.
    wait_until(
        "the sweep to take the swept repository's row",
        Duration::from_secs(60),
        || {
            stratum_control::registry::repo_by_name(&db, &org_id, "swept")
                .expect("read repos")
                .is_none()
        },
    );

    let admin_token = admin.clone();
    drop(w);

    // A lander arrives at a job whose change is not there any more.
    let scratch_b = Scratch::new("changes-swept-b");
    let server = spawn_with(&bucket.base_url, &scratch_b, Some(&db_url), "0.2");

    // It retires the job rather than failing it or holding the lease:
    // "there is nothing left to eject" is a settled outcome, not an
    // error, and a queue that keeps re-leasing a dead row never drains.
    let done = wait_for(
        "the lander to claim the job whose change is gone",
        Duration::from_secs(60),
        || {
            let j = stratum_control::jobs::get(&db, &org_id, &land_job)
                .expect("read job")
                .expect("the job row outlives the change");
            (j.state == "done" || j.state == "failed").then_some(j)
        },
    );
    assert_eq!(
        done.state, "done",
        "a change that is simply not there any more was an error rather \
         than a settled no-op, so the queue would keep re-leasing it: {done:?}"
    );

    // And the lander is still landing things — the claim that matters,
    // because a worker that wedges on one dead row takes every later
    // landing with it.
    commit(
        &server,
        &admin_token,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n")],
    );
    branch(&server, &admin_token, "app", "next", "main");
    commit(
        &server,
        &admin_token,
        "app",
        "next",
        "still working\n\nChange-Id: Ic0ffee02\n",
        &[("next.rs", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        &admin_token,
        Some(serde_json::json!({"from": "next"})),
    );
    assert_eq!(st, 201);
    let alice = sign_in(&server, "alice@acme.test");
    let (st, _) = as_person(
        &server,
        &alice,
        "POST",
        "/v1/orgs/acme/repos/app/changes/Ic0ffee02/approve",
        None,
    );
    assert_eq!(st, 204);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes/Ic0ffee02/land",
        &admin_token,
        None,
    );
    assert_eq!(st, 202);
    let out = wait_until_not(&server, &admin_token, "Ic0ffee02", "landing");
    assert_eq!(
        out["change"]["state"],
        serde_json::json!("landed"),
        "the lander never recovered from the swept job: {out}"
    );
    assert!(server.healthy());
}

/// A **second** land job for a change that has already landed.
///
/// The state row is the truth and the job just goes away. This is what a
/// crash leaves behind: a lander claims a job, lands the change, and
/// dies before completing the row, so the lease expires and the job is
/// claimed again — against a change that is no longer `landing`. There
/// is no operator route out of that state (POST /land is the only door),
/// so a duplicate job is the honest way to stand the case up, and it is
/// the case the guard's own comment names.
///
/// What must not happen is a second landing or an eject. Landing twice
/// would apply the same patchset to a branch that already has it, and
/// ejecting would overwrite a completed landing's verdict with a refusal
/// nobody made.
#[test]
fn a_second_land_job_for_a_landed_change_is_a_no_op_not_a_second_landing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-release");
    let scratch = Scratch::new("changes-release");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n")],
    );
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "lands once\n\nChange-Id: Ibeef0001\n",
        &[("once.rs", "v1")],
    );
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201);
    let alice = sign_in(server, "alice@acme.test");
    let (st, _) = as_person(
        server,
        &alice,
        "POST",
        "/v1/orgs/acme/repos/app/changes/Ibeef0001/approve",
        None,
    );
    assert_eq!(st, 204);
    let (st, _) = server.post(
        "/v1/orgs/acme/repos/app/changes/Ibeef0001/land",
        admin,
        None,
    );
    assert_eq!(st, 202);
    let landed = wait_until_not(server, admin, "Ibeef0001", "landing");
    assert_eq!(
        landed["change"]["state"],
        serde_json::json!("landed"),
        "{landed}"
    );
    let tip_after_first = landed["change"]["landed_commit"]
        .as_str()
        .expect("a landed commit")
        .to_string();

    // The job a crashed lander would leave behind: same change, same
    // payload, claimed after the change has already moved on.
    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .unwrap()
        .id;
    let repo_id = stratum_control::registry::repo_by_name(&db, &org_id, "app")
        .unwrap()
        .expect("the repo")
        .id;
    let change_id = stratum_control::changes::by_key(&db, &repo_id, "Ibeef0001")
        .unwrap()
        .expect("the change")
        .id;
    let payload = serde_json::json!({ "change_id": change_id }).to_string();
    let again = stratum_control::jobs::create(&db, &org_id, Some(&repo_id), "land", Some(&payload))
        .expect("a re-leased job")
        .id;

    let done = wait_for(
        "the lander to claim the duplicate land job",
        Duration::from_secs(60),
        || {
            let j = stratum_control::jobs::get(&db, &org_id, &again)
                .expect("read job")
                .expect("the job exists");
            (j.state == "done" || j.state == "failed").then_some(j)
        },
    );
    assert_eq!(done.state, "done", "{done:?}");
    assert!(
        done.result
            .as_deref()
            .unwrap_or_default()
            .contains("landed"),
        "the job did not record the state it found: {done:?}"
    );

    // The change is untouched: still landed, same commit, and no
    // ejection verdict for a refusal nobody made.
    let (st, out) = server.get("/v1/orgs/acme/repos/app/changes/Ibeef0001", admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["change"]["state"], serde_json::json!("landed"), "{out}");
    assert_eq!(
        out["change"]["landed_commit"].as_str(),
        Some(tip_after_first.as_str()),
        "the change was landed a second time: {out}"
    );
    let verdict = out["change"]["land_verdict"].as_str().unwrap_or_default();
    assert!(
        !verdict.starts_with("ejected:"),
        "a completed landing was overwritten with an ejection: {verdict}"
    );
    assert!(server.healthy());
}

/// **A rebased review branch: force push, and what the review knows
/// afterwards.**
///
/// This interaction did not exist until force push did — a branch could
/// not be rewritten at all — so nothing here has ever exercised it, and
/// it is the single most common thing a contributor does after reading
/// feedback: rebase, force-push, ask for another look.
///
/// Three claims:
///
/// 1. the rewritten commit becomes a **new patchset**, and the old one
///    stays on the record — the history of what was reviewed is the
///    point of a review tool;
/// 2. the approval given to the old patchset **stops counting**, because
///    the code is not the code that was approved;
/// 3. a branch rewritten *while a landing is queued* cannot smuggle the
///    rewrite onto trunk. Two things stop it, and both are asserted
///    because either alone would be thin: re-registering is refused
///    while the change is landing, and the lander lands the **patchset**
///    it was queued for rather than whatever the branch now points at.
///    That second one is the load-bearing half — a lander that resolved
///    the branch tip at claim time would put code on trunk that nobody
///    has read, and the approval on the record would make it look
///    reviewed.
#[test]
fn a_force_pushed_review_branch_opens_a_new_patchset_and_never_lands_unread_code() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-forcepush");
    let scratch = Scratch::new("changes-forcepush");
    // No lander: the queued-then-rewritten case below needs the job to
    // sit still until this test decides otherwise.
    let db_url = stratum_testkit::pg::test_db_url("changes-forcepush");
    let a = spawn_with(&bucket.base_url, &scratch, Some(&db_url), "0");
    let w = world(a);
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n")],
    );

    let url = server.authed_url(admin, "acme", "app");
    let work = scratch.path().join("work");
    gitcli::clone_and_fsck(&url, &work);
    gitcli::git(&work, &["checkout", "-q", "-b", "review"]);
    std::fs::write(work.join("feature.rs"), "// first attempt\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(
        &work,
        &[
            "commit",
            "-q",
            "-m",
            "add a feature\n\nChange-Id: Ifeed0001\n",
        ],
    );
    gitcli::git(&work, &["push", "-q", "origin", "review"]);
    let first = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "review"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/Ifeed0001";

    // Alice reads it and approves.
    let alice = sign_in(server, "alice@acme.test");
    let (st, _) = as_person(server, &alice, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204);

    // The rebase: same idea, different commit.
    std::fs::write(work.join("feature.rs"), "// second attempt, per review\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(
        &work,
        &[
            "commit",
            "-q",
            "--amend",
            "-m",
            "add a feature\n\nChange-Id: Ifeed0001\n",
        ],
    );
    let second = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    assert_ne!(first, second);
    gitcli::git(&work, &["push", "-q", "-f", "origin", "review"]);
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "review"})),
    );
    assert!(st == 200 || st == 201, "{st} {out}");

    // 1. A second patchset, with the first still on the record.
    let (st, after) = server.get(cp, admin);
    assert_eq!(st, 200, "{after}");
    let sets = after["patchsets"].as_array().expect("patchsets");
    assert_eq!(
        sets.len(),
        2,
        "the rewritten commit did not open a new patchset: {after}"
    );
    assert_eq!(sets[0]["commit"], serde_json::json!(first), "{after}");
    assert_eq!(sets[1]["commit"], serde_json::json!(second), "{after}");
    assert_eq!(after["change"]["patchset"]["number"], serde_json::json!(2));

    // 2. The approval was for patchset 1 and does not carry over.
    let (st, verdict) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(st, 200, "{verdict}");
    assert_ne!(
        verdict["state"],
        serde_json::json!("sufficient"),
        "an approval of the code that was replaced still counts: {verdict}"
    );

    // Alice reads the rewrite and approves *it*, and the land is queued.
    let (st, _) = as_person(server, &alice, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204);
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202, "{out}");

    // 3. A third rewrite, pushed while the landing sits queued.
    std::fs::write(work.join("feature.rs"), "// snuck in, unreviewed\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(
        &work,
        &[
            "commit",
            "-q",
            "--amend",
            "-m",
            "add a feature\n\nChange-Id: Ifeed0001\n",
        ],
    );
    let third = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    gitcli::git(&work, &["push", "-q", "-f", "origin", "review"]);

    // Registering it is refused: the change is spoken for.
    let (st, refused) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "review"})),
    );
    assert_eq!(st, 409, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .contains("landing"),
        "{refused}"
    );

    // And the lander lands the patchset it was queued for.
    let admin_token = admin.clone();
    drop(w);
    let scratch_b = Scratch::new("changes-forcepush-b");
    let server = spawn_with(&bucket.base_url, &scratch_b, Some(&db_url), "0.2");
    let out = wait_until_not(&server, &admin_token, "Ifeed0001", "landing");
    assert_eq!(
        out["change"]["state"],
        serde_json::json!("landed"),
        "the approved patchset did not land: {out}"
    );
    assert_eq!(
        out["change"]["landed_commit"],
        serde_json::json!(second),
        "the lander followed the branch rather than the patchset it was \
         queued for, so a rewrite pushed after approval reached trunk: {out}"
    );

    // Trunk carries the code Alice read, not the one pushed after her.
    let check = scratch_b.path().join("check");
    let head = gitcli::clone_and_fsck(&server.authed_url(&admin_token, "acme", "app"), &check);
    assert_ne!(head, third, "the unreviewed rewrite reached trunk");
    assert_eq!(
        std::fs::read_to_string(check.join("feature.rs")).unwrap(),
        "// second attempt, per review\n",
        "trunk is not the code that was approved"
    );
    assert!(server.healthy());
}

/// **A force-pushed patchset, after compaction and GC.**
///
/// Force push is the first thing that makes an object genuinely
/// unreachable here: until it existed the layout only ever grew, so
/// nothing had to think about what happens to a commit no ref points at
/// any more. A review branch that has been rebased leaves exactly that —
/// patchset 1's commit is on no branch, and the change still refers to
/// it by oid.
///
/// The two collectors are both epoch-shaped, and between them they can
/// drop it. Compaction rebuilds the layout by re-ingesting a seed
/// materialized from `manifest.refs`, so a commit on no ref is simply not
/// in the new epoch; GC then deletes the old epoch once no pointer names
/// it. Neither step consults the review tables.
///
/// If the commit goes, the change's own history goes with it — the
/// record says "patchset 1 was <oid>" and there is nothing behind the
/// oid, so a reviewer cannot see what they were originally shown and the
/// audit trail of the review is a set of dangling references. That is
/// silent, and it happens minutes later, at a WAL threshold nobody is
/// watching.
#[test]
fn a_rebased_patchsets_commit_survives_compaction_and_gc() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-fp-gc");
    let scratch = Scratch::new("changes-fp-gc");
    let db_url = stratum_testkit::pg::test_db_url("changes-fp-gc");
    let a = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .data_dir(scratch.path().join("data"))
        .db_url(&db_url)
        .env("STRATUM_LAND_POLL_SECS", "0")
        // The churn below crosses the WAL threshold, which *enqueues* a
        // compaction; the background compactor claims those jobs on its
        // own clock. This test then asks for a compaction itself and
        // asserts the outcome is `Compacted` — so if the worker gets
        // there first the explicit call correctly answers `LostRace` and
        // the assertion fails on a race rather than on the behaviour it
        // is about. Nobody but this test compacts here.
        .env("STRATUM_COMPACT_POLL_SECS", "0")
        // A sweep that does not wait, so the state under test is the one
        // after collection rather than before it.
        .env("STRATUM_GC_SECS", "1")
        .env("STRATUM_GC_GRACE_SECS", "0")
        .start();
    let w = world(a);
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n")],
    );

    let url = server.authed_url(admin, "acme", "app");
    let work = scratch.path().join("work");
    gitcli::clone_and_fsck(&url, &work);
    gitcli::git(&work, &["checkout", "-q", "-b", "review"]);
    std::fs::write(work.join("feature.rs"), "// first attempt\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(
        &work,
        &["commit", "-q", "-m", "a feature\n\nChange-Id: Ida1a0001\n"],
    );
    gitcli::git(&work, &["push", "-q", "origin", "review"]);
    let first = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "review"})),
    );
    assert_eq!(st, 201, "{out}");

    // Rebase and force-push: `first` is now on no ref at all.
    std::fs::write(work.join("feature.rs"), "// second attempt\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(
        &work,
        &[
            "commit",
            "-q",
            "--amend",
            "-m",
            "a feature\n\nChange-Id: Ida1a0001\n",
        ],
    );
    gitcli::git(&work, &["push", "-q", "-f", "origin", "review"]);
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "review"})),
    );
    assert!(st == 200 || st == 201, "{st} {out}");

    // The review still names the old commit.
    let cp = "/v1/orgs/acme/repos/app/changes/Ida1a0001";
    let (st, view) = server.get(cp, admin);
    assert_eq!(st, 200, "{view}");
    assert_eq!(
        view["patchsets"][0]["commit"],
        serde_json::json!(first),
        "{view}"
    );

    // Readable before anything collects.
    let (st, before) = server.get(
        &format!("/v1/orgs/acme/repos/app/log?rev={first}&limit=1"),
        admin,
    );
    assert_eq!(
        st, 200,
        "the rewritten patchset is unreadable already: {before}"
    );

    // The keys the store holds *before* compaction — the old epoch's
    // objects, which are what GC is here to collect. Snapshotted now
    // rather than after the compaction, because with `STRATUM_GC_SECS=1`
    // and no grace the sweep can run in between; taken before, nothing
    // in this set is collectable yet, so its first shrink is the sweep
    // having actually deleted something.
    let store =
        stratum_store::ObjectStore::new(&bucket.base_url, stratum_store::LatencyModel::None);
    let before_compaction: std::collections::BTreeSet<String> = store
        .list("o/")
        .expect("list the bucket")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert!(
        !before_compaction.is_empty(),
        "the repository's objects are not in the store at all"
    );

    // Churn past the WAL threshold and compact, which rebuilds the epoch
    // from the refs alone.
    for i in 0..12 {
        commit(
            server,
            admin,
            "app",
            "main",
            &format!("churn {i}"),
            &[("churn.txt", "x")],
        );
    }
    let (st, out) = server.post("/v1/orgs/acme/repos/app/compact", admin, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["outcome"], "Compacted", "{out}");

    // Wait for the sweep to have *collected*, not for four seconds to
    // have passed. This test is named for surviving GC, and a sleep
    // proves nothing about whether GC ran: with the worker disabled, or
    // wedged, or never reaching this repository, the assertion below
    // would have been just as green. The observable is a key that was
    // there before the compaction and is not there now — the old epoch
    // going away is the whole event being waited on.
    wait_until(
        "the epoch sweep to collect the pre-compaction epoch",
        Duration::from_secs(60),
        || {
            let now: std::collections::BTreeSet<String> = store
                .list("o/")
                .expect("list the bucket")
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            before_compaction.difference(&now).next().is_some()
        },
    );

    // The claim: patchset 1 is still readable, so the review still has
    // the code it was about.
    let (st, after) = server.get(
        &format!("/v1/orgs/acme/repos/app/log?rev={first}&limit=1"),
        admin,
    );
    assert_eq!(
        st, 200,
        "a rebased patchset's commit was collected: the change still \
         says patchset 1 was {first}, and there is nothing behind the \
         oid. Response: {after}"
    );
    assert!(server.healthy());
}

/// **The monorepo claim, end to end: one change across two owned
/// subtrees needs both owners.**
///
/// `sufficiency::evaluate` enforces this and has a unit test for it, but
/// that test hands `evaluate` two `PathRequirement`s it constructed
/// itself. The step it cannot reach is the one before: deriving those
/// requirements from a real diff — which paths changed, which OWNERS
/// file governs each, and whether `set noparent` cut the inheritance.
///
/// If that derivation collapsed a diff to a single requirement — the
/// first changed path, the deepest common directory, the root file —
/// `evaluate` would be handed one path, answer "satisfied", and the unit
/// test would still be green. What that buys in a monorepo is an owner
/// of `payments/` landing a change to `search/` on their own approval.
/// That is the whole reason per-path ownership exists, and nothing was
/// checking the join.
///
/// `set noparent` in both subtrees is what makes the domains genuinely
/// disjoint: without it the root file's owner satisfies everything and
/// the test proves nothing.
#[test]
fn a_change_across_two_owned_subtrees_needs_an_owner_from_each() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-twodomains");
    let scratch = Scratch::new("changes-twodomains");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);

    commit(
        server,
        admin,
        "app",
        "main",
        "two domains",
        &[
            // The root owner must not be able to satisfy either subtree,
            // or a single approval would land the lot.
            ("OWNERS", "dev@acme.test\n"),
            ("payments/OWNERS", "alice@acme.test\nset noparent\n"),
            ("payments/fees.rs", "// fees\n"),
            ("search/OWNERS", "casey@acme.test\nset noparent\n"),
            ("search/index.rs", "// index\n"),
        ],
    );
    branch(server, admin, "app", "wide", "main");
    commit(
        server,
        admin,
        "app",
        "wide",
        "touch both domains\n\nChange-Id: Ic0de0001\n",
        &[
            ("payments/fees.rs", "// fees, revised\n"),
            ("search/index.rs", "// index, revised\n"),
        ],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "wide"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/Ic0de0001";

    // Both domains are named up front, so an author knows who to ask
    // before anybody has approved anything.
    let (st, verdict) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(st, 200, "{verdict}");
    let per_path = verdict["verdict"]["per_path"].as_array().expect("per_path");
    assert_eq!(
        per_path.len(),
        2,
        "the diff did not resolve to one requirement per owned subtree — \
         a single requirement here is what lets one team's owner land \
         another team's code: {verdict}"
    );

    // Alice owns payments and only payments.
    let alice = sign_in(server, "alice@acme.test");
    let (st, _) = as_person(server, &alice, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204);
    let (st, half) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(st, 200, "{half}");
    assert_eq!(
        half["verdict"]["landable"],
        serde_json::json!(false),
        "one subtree's owner satisfied a change that touches two: {half}"
    );
    assert!(
        half["verdict"]["explanation"]
            .as_str()
            .unwrap_or_default()
            .contains("search"),
        "the block does not name the domain still waiting: {half}"
    );

    // …and the land queue agrees, which is the half that actually
    // guards trunk. A preview that refuses while landing accepts is a
    // preview, not a gate.
    let (st, refused) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(
        st, 409,
        "a change missing one domain's approval entered the land queue: {refused}"
    );

    // Casey owns search. With both, it lands.
    let casey = sign_in(server, "casey@acme.test");
    let (st, _) = as_person(server, &casey, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204);
    let (st, full) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(st, 200, "{full}");
    assert_eq!(
        full["verdict"]["landable"],
        serde_json::json!(true),
        "{full}"
    );

    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202, "{out}");
    let landed = wait_until_not(server, admin, "Ic0de0001", "landing");
    assert_eq!(
        landed["change"]["state"],
        serde_json::json!("landed"),
        "{landed}"
    );

    // Both edits are on trunk, and the clone is sound.
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&server.authed_url(admin, "acme", "app"), &clone);
    assert_eq!(
        std::fs::read_to_string(clone.join("payments/fees.rs")).unwrap(),
        "// fees, revised\n"
    );
    assert_eq!(
        std::fs::read_to_string(clone.join("search/index.rs")).unwrap(),
        "// index, revised\n"
    );
    assert!(server.healthy());
}

/// A bucket reached through a fault proxy, so the store can be made to
/// refuse exactly one kind of request for exactly as long as the test
/// says.
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

/// The land queue gives up. A landing whose job fails is rescued by the
/// reaper under a fresh job, and a fresh job has never been tried before,
/// so the queue's `max_attempts` — which counts one row's claims — never
/// applied to it: a change whose landing failed the same way every time
/// was re-adopted every `STRATUM_LAND_RECHECK_SECS` forever, and its
/// owner saw a change that had been "landing" for a week.
///
/// The lander now carries the count of failed drivers across the rescue
/// and ejects at the queue's cap, with the last error as the verdict. A
/// *hold* — the job completing "waiting on ci/tests" — is not a failure
/// and does not burn an attempt, which is asserted first: a cap that
/// counted holds would eject every change whose CI takes longer than
/// five polls.
/// The review routes that cannot answer without reading the store, with
/// the store refusing every read.
///
/// These are not database arms and the distinction is the whole point:
/// the OWNERS resolution behind "is this waiting on me", the tree diff
/// behind an interdiff, the path lookup behind "may this person settle
/// that thread" and the file a suggestion rewrites all reach the object
/// store, and a store is a
/// thing this harness can make fail on purpose. What they must not do is
/// answer *something* — an empty "waiting on me" page reads as "you are
/// all caught up", an empty interdiff reads as "these two patchsets are
/// identical", a resolve that quietly refuses reads as a permission the
/// person does not have, and an apply that refuses reads as a suggestion
/// gone stale. Each has to be a 500.
///
/// Each route is asked once with the store healthy, once with it
/// refusing, and once after healing. The first and last are what make
/// the middle mean anything: without them a 500 could just as well be a
/// broken fixture, and the recovery says the server took an outage
/// rather than a wound.
#[test]
fn a_store_that_refuses_reads_fails_the_routes_that_need_one_and_recovers() {
    let (proxy, scratch) = proxied("changes-storefault");
    let w = world(
        Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &proxy.url)
            .data_dir(scratch.path().join("data"))
            .env("STRATUM_LAND_POLL_SECS", "0.2")
            .db_hint("changes-e2e")
            .start(),
    );
    let (server, admin) = (&w.server, &w.admin);
    let base = commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n"), ("core.rs", "v0")],
    );
    branch(server, admin, "app", "feature", &base);
    let alice = sign_in(server, "alice@acme.test");
    let casey = sign_in(server, "casey@acme.test");
    for (n, body) in [(1, "v1"), (2, "v2")] {
        commit(
            server,
            admin,
            "app",
            "feature",
            "work\n\nChange-Id: Ifa17e001\n",
            &[("core.rs", body)],
        );
        let (st, out) = server.post(
            "/v1/orgs/acme/repos/app/changes",
            admin,
            Some(serde_json::json!({"from": "feature"})),
        );
        assert_eq!(st, 201, "{out}");
        assert_eq!(out["patchset"]["number"], serde_json::json!(n), "{out}");
    }
    let cp = "/v1/orgs/acme/repos/app/changes/Ifa17e001";
    // Casey's remark, so resolving it is a judgement about the *path*
    // rather than the author's own withdrawal — the branch that has to
    // consult OWNERS, and therefore the store.
    let (st, out) = as_person(
        server,
        &casey,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({"body": "why v2?", "path": "core.rs", "line": 1})),
    );
    assert_eq!(st, 201, "{out}");
    let cid = out["id"].as_str().expect("an id").to_string();

    let needs = percent_encode("needs:my-approval");
    let routes = [
        (
            "the per-repo waiting-on-me page",
            format!("/v1/orgs/acme/repos/app/changes?q={needs}"),
        ),
        (
            "the org-wide waiting-on-me page",
            format!("/v1/orgs/acme/changes?q={needs}"),
        ),
        ("the interdiff", format!("{cp}/interdiff?from=1&to=2")),
    ];
    let resolve = format!("{cp}/comments/{cid}/resolve");
    // And a suggestion of alice's, for the same reason one route down:
    // applying one reads the file out of the store before it can decide
    // anything at all.
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({
            "body": "```suggestion\nv3\n```",
            "path": "core.rs",
            "line": 1,
        })),
    );
    assert_eq!(st, 201, "{out}");
    let suggestion = out["id"].as_str().expect("an id").to_string();
    let apply = format!("{cp}/suggestions/apply");
    let take_it = serde_json::json!({"comments": [suggestion]});

    // Healthy first, so a 500 below is the fault and not the fixture.
    for (what, path) in &routes {
        let (st, out) = as_person(server, &alice, "GET", path, None);
        assert_eq!(st, 200, "{what} before the fault: {out}");
    }

    proxy.handle.inject("GET", 10_000, 503);
    for (what, path) in &routes {
        let (st, out) = as_person(server, &alice, "GET", path, None);
        assert_eq!(st, 500, "{what} answered anyway: {out}");
    }
    // Same for the one write on these routes that needs a read to decide
    // whether it is allowed at all.
    let (st, out) = as_person(server, &alice, "POST", &resolve, None);
    assert_eq!(
        st, 500,
        "a store outage read as a permission refusal: {out}"
    );
    // Applying a suggestion must not answer either: a refusal here
    // reads as "your suggestion no longer applies", which sends the
    // reviewer to rewrite a remark that was never stale.
    let (st, out) = as_person(server, &alice, "POST", &apply, Some(take_it.clone()));
    assert_eq!(st, 500, "a store outage read as a stale anchor: {out}");

    proxy.handle.heal();
    for (what, path) in &routes {
        let (st, out) = as_person(server, &alice, "GET", path, None);
        assert_eq!(st, 200, "{what} after healing: {out}");
    }
    // And the judgement the outage could not make is made now: alice is
    // OWNERS for core.rs, so the thread settles.
    let (st, out) = as_person(server, &alice, "POST", &resolve, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["resolved"], serde_json::json!(true), "{out}");
    // And so is the suggestion the outage could not apply.
    let (st, out) = as_person(server, &alice, "POST", &apply, Some(take_it));
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["patchset"]["number"], serde_json::json!(3), "{out}");
    assert!(server.healthy());
}

#[test]
fn a_change_whose_land_job_fails_the_same_way_every_time_is_ejected_at_the_queues_cap() {
    let (proxy, scratch) = proxied("changes-landcap");
    // A named database rather than a hint: `assert_holding` reads the
    // change's `land_job_id` to see the lander really recheck.
    let db_url = stratum_testkit::pg::test_db_url("changes-e2e");
    let w = world(
        Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &proxy.url)
            .data_dir(scratch.path().join("data"))
            .env("STRATUM_LAND_POLL_SECS", "0.2")
            .env("STRATUM_LAND_WAIT_SECS", "1800")
            .env("STRATUM_LAND_RECHECK_SECS", "1")
            .env("STRATUM_JOB_MAX_ATTEMPTS", "2")
            .db_url(&db_url)
            .start(),
    );
    let (server, admin) = (&w.server, &w.admin);
    let cp = approved_change_on_protected_main(server, admin, "Ic1c50a01", &["ci/tests"]);
    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "pending"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202, "{out}");

    // Six holds against a cap of two: a hold is not an attempt.
    assert_holding(
        server,
        admin,
        "Ic1c50a01",
        "waiting on ci/tests",
        6,
        &db_url,
    );

    // Now every write of the ref truth is refused, and the check clears.
    // The first driver fails; the reaper's rescue fails the same way;
    // that is the cap.
    proxy.handle.inject("PUT manifest.json", 1_000, 503);
    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "passing"})),
    );
    assert_eq!(st, 200, "{out}");
    let out = wait_until_not(server, admin, "Ic1c50a01", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("open"), "{out}");
    let verdict = out["change"]["land_verdict"].as_str().unwrap();
    let head = "ejected: the landing was given up after 2 attempts; the last failed with: ";
    assert!(verdict.starts_with(head), "{verdict}");
    assert!(
        verdict.contains("503"),
        "the verdict names the store's refusal: {verdict}"
    );

    // The store recovers; the change is landable again on request.
    proxy.handle.clear();
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202, "{out}");
    let out = wait_until_not(server, admin, "Ic1c50a01", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("landed"), "{out}");
    assert!(server.healthy());
}

/// The store fails between the lander's claim and its re-read of the
/// verdict, and the change is ejected rather than landed or retried.
///
/// `run_one` reads only the control plane until step 1, where
/// `compute_verdict` opens a reader on the repository: its first request
/// is `GET <prefix>/manifest.json`, the manifest the OWNERS files are
/// resolved from. This fails exactly that read, once, and nothing else.
/// It is deterministic because the change is *held* first: a held change
/// is rechecked every `STRATUM_LAND_RECHECK_SECS`, and every recheck
/// recomputes the verdict before it looks at the gate, so once the hold
/// note is visible the next manifest read is the lander's and nobody
/// else's. The background workers that also read the store are stopped,
/// the change route the wait polls reads only the database, and trunk is
/// not read through `/refs` until the ejection has been observed —
/// `/refs` opens the same reader, and would eat the fault.
///
/// This arm used to be covered once, by a seeded fault storm that
/// happened to time a 503 into this window, with nothing asserting the
/// outcome. What it must do: eject with the store's own words, so the
/// author reads "land error — GET …/manifest.json: HTTP 503" rather than
/// a change that silently reopened; move nothing; and settle the job, so
/// a store outage does not become a change that is "landing" forever.
/// A healed store lands the same change on request, which is what says
/// the ejection was a verdict about the outage and not a wound.
#[test]
fn a_store_failure_while_the_verdict_is_recomputed_ejects_the_change_and_lands_nothing() {
    let (proxy, scratch) = proxied("changes-verdictfault");
    // Nobody but the lander meets the fault: every other store-reading
    // worker is off, so a single injected refusal is consumed by the
    // read this test is about and not by a compactor's or a notifier's.
    let stopped: Vec<(&str, String)> = [
        "STRATUM_COMPACT_POLL_SECS",
        "STRATUM_CDNPACK_POLL_SECS",
        "STRATUM_CONTRIB_POLL_SECS",
        "STRATUM_NOTIFY_POLL_SECS",
        "STRATUM_CHANGESET_NOTIFY_POLL_SECS",
        "STRATUM_STORAGE_SWEEP_SECS",
    ]
    .iter()
    .map(|k| (*k, "0".to_string()))
    .collect();
    let w = world(
        Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &proxy.url)
            .data_dir(scratch.path().join("data"))
            .env("STRATUM_LAND_POLL_SECS", "0.2")
            .env("STRATUM_LAND_WAIT_SECS", "1800")
            .env("STRATUM_LAND_RECHECK_SECS", "1")
            .envs(&stopped)
            .db_hint("changes-e2e")
            .start(),
    );
    let (server, admin) = (&w.server, &w.admin);
    let key = "Ic0ff0001";
    let cp = approved_change_on_protected_main(server, admin, key, &["ci/tests"]);
    let path = format!("/v1/orgs/acme/repos/app/changes/{key}");
    let trunk = || {
        let (st, refs) = server.get("/v1/orgs/acme/repos/app/refs", admin);
        assert_eq!(st, 200, "{refs}");
        refs["refs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "refs/heads/main")
            .expect("trunk exists")["oid"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let before = trunk();

    // Hold the change on a check that has not reported, so the lander is
    // rechecking it on a clock this test can see.
    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "pending"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202, "{out}");
    wait_until(
        "the lander to record that it is waiting on ci/tests",
        Duration::from_secs(30),
        || {
            let (st, out) = server.get(&path, admin);
            assert_eq!(st, 200, "{out}");
            if out["change"]["land_verdict"] == serde_json::json!("waiting on ci/tests") {
                return true;
            }
            assert_eq!(
                out["change"]["state"],
                serde_json::json!("landing"),
                "the change left the queue before ever saying it was waiting: {out}"
            );
            false
        },
    );

    // One failed read, on the one the verdict makes first. Three
    // refusals and not one: `ObjectStore::get_stream` retries a 5xx
    // twice with backoff before it reports, so a single 503 is absorbed
    // and the recheck passes — which is right, and is what this arm is
    // *not* about. All three are met by the same read, back to back,
    // because nothing else is reading this manifest. The next recheck
    // claims the job, re-reads the verdict, and fails it.
    proxy.handle.inject("GET manifest.json", 3, 503);
    let out = wait_until_not(server, admin, key, "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("open"), "{out}");
    let verdict = out["change"]["land_verdict"].as_str().unwrap();
    assert!(
        verdict.starts_with("ejected: land error — "),
        "the ejection names the class: {verdict}"
    );
    assert!(
        verdict.contains("manifest.json") && verdict.contains("503"),
        "the ejection carries the store's own refusal: {verdict}"
    );
    assert_eq!(
        out["change"]["landed_commit"],
        serde_json::Value::Null,
        "nothing landed: {out}"
    );
    assert_eq!(trunk(), before, "trunk did not move");

    // The job settled. A change the queue had merely dropped and picked
    // up again would be `landing` once more within a recheck; this one
    // stays `open`, with its verdict, across several of them.
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(1200));
        let (st, out) = server.get(&path, admin);
        assert_eq!(st, 200, "{out}");
        assert_eq!(
            out["change"]["state"],
            serde_json::json!("open"),
            "the queue re-adopted an ejected change: {out}"
        );
        assert_eq!(out["change"]["land_verdict"], serde_json::json!(verdict));
    }

    // The store is fine now, the check is green, and the same change
    // lands on request: the ejection was about the outage.
    proxy.handle.heal();
    let (st, out) = server.post(
        &format!("{cp}/checks"),
        admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "passing"})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202, "{out}");
    let out = wait_until_not(server, admin, key, "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("landed"), "{out}");
    assert_ne!(trunk(), before, "trunk moved once the store answered");
    assert!(server.healthy());
}

/// The OWNERS rules that decide a landing are the target branch's, not
/// the patchset's. A patchset is the thing under review; if it could
/// bring its own rules, deleting or rewriting `OWNERS` would be the one
/// change nobody had to approve. Only a branch that does not exist yet
/// has no rules to read, and then the patchset's own files are the best
/// available answer.
#[test]
fn owners_are_read_at_the_target_branch_not_the_patchset() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-owners-base");
    let scratch = Scratch::new("changes-owners-base");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n"), ("core.rs", "v1")],
    );
    branch(server, admin, "app", "feature", "main");

    // Dev's patchset deletes the root OWNERS and edits a file under it.
    // Read at the patchset, no rule would govern either path and any
    // writer's approval would do — dev's own, for instance.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        admin,
        Some(serde_json::json!({
            "branch": "feature",
            "message": "drop the rules\n\nChange-Id: I0e0a0001\n",
            "operations": [
                {"op": "delete", "path": "OWNERS"},
                {"op": "put", "path": "core.rs", "content": "v2"},
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/I0e0a0001";

    let (st, out) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(st, 200);
    assert_eq!(
        out["verdict"]["landable"],
        serde_json::json!(false),
        "{out}"
    );
    assert_eq!(
        out["verdict"]["explanation"],
        serde_json::json!("blocked: needs an owner of /OWNERS (owners: alice@acme.test)"),
        "{out}"
    );

    // A writer who is not the owner cannot wave it through.
    let dev = sign_in(server, "dev@acme.test");
    let (st, out) = as_person(server, &dev, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(st, 200);
    assert_eq!(
        out["verdict"]["landable"],
        serde_json::json!(false),
        "{out}"
    );
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 409, "{out}");

    // The owner named on trunk can — and only then.
    let alice = sign_in(server, "alice@acme.test");
    let (st, out) = as_person(server, &alice, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(st, 200);
    assert_eq!(out["verdict"]["landable"], serde_json::json!(true), "{out}");
    assert_eq!(
        out["verdict"]["explanation"],
        serde_json::json!("ok: all 2 changed path(s) approved"),
        "{out}"
    );

    // A change aimed at a branch that does not exist yet has no trunk to
    // ask, so its own files govern: the same deletion, targeted at an
    // absent `release`, needs only a writer.
    branch(server, admin, "app", "feature2", "main");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        admin,
        Some(serde_json::json!({
            "branch": "feature2",
            "message": "drop the rules, elsewhere\n\nChange-Id: I0e0a0002\n",
            "operations": [{"op": "delete", "path": "OWNERS"}],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature2", "target": "release"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp2 = "/v1/orgs/acme/repos/app/changes/I0e0a0002";
    let (st, out) = as_person(server, &dev, "POST", &format!("{cp2}/approve"), None);
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.get(&format!("{cp2}/verdict"), admin);
    assert_eq!(st, 200);
    assert_eq!(out["verdict"]["landable"], serde_json::json!(true), "{out}");
    assert!(server.healthy());
}

/// Threading, anchors, and what survives a revision.
///
/// Every part of it is asserted the way a reviewer would find out — by
/// posting and reading back — because the interesting claims are all
/// about what the *reader* gets: that a reply lands in its root's
/// thread carrying the root's anchor, that "you should not have deleted
/// this" is expressible at all, that a range stays a range, and that a
/// comment written against patchset 1 still says so once patchset 2 has
/// rewritten the file underneath it.
#[test]
fn a_thread_keeps_one_anchor_across_replies_and_patchsets() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-threads");
    let scratch = Scratch::new("changes-threads");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n"), ("fees.rs", "old line\n")],
    );
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "fees\n\nChange-Id: I7bead001\n",
        &[("fees.rs", "fn fee() -> u32 {\n    41\n}\n")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/I7bead001";
    let alice = sign_in(server, "alice@acme.test");
    let casey = sign_in(server, "casey@acme.test");

    // A range on the new side: the remark is about the whole function,
    // so the anchor is the whole function.
    let (st, root) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({
            "body": "this whole thing should be a const",
            "path": "fees.rs",
            "line": 1,
            "line_end": 3,
        })),
    );
    assert_eq!(st, 201, "{root}");
    assert_eq!(root["line"], serde_json::json!(1));
    assert_eq!(root["line_end"], serde_json::json!(3));
    assert_eq!(root["side"], serde_json::json!("new"));
    assert_eq!(root["resolved"], serde_json::json!(false));
    assert_eq!(root["thread_id"], root["id"], "a root is its own thread");
    assert_eq!(root["original_line"], serde_json::json!(1));
    assert_eq!(root["original_patchset"], serde_json::json!(1));
    let root_id = root["id"].as_str().unwrap().to_string();

    // The old side: a line the change removed, which had no way of
    // being spoken about before — the anchor would have had to name
    // whatever ended up at that position instead.
    let (st, old) = as_person(
        server,
        &casey,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({
            "body": "why did this go?",
            "path": "fees.rs",
            "line": 1,
            "side": "old",
        })),
    );
    assert_eq!(st, 201, "{old}");
    assert_eq!(old["side"], serde_json::json!("old"));
    // A single line is stored as the range it is, so no reader has to
    // remember that a missing line_end means "same as line".
    assert_eq!(old["line_end"], serde_json::json!(1));

    // A reply joins the thread and inherits the whole anchor — it is
    // part of that remark, not a second one that happens to sit nearby.
    let (st, reply) = as_person(
        server,
        &casey,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({"body": "agreed, will fold it", "parent_id": root_id})),
    );
    assert_eq!(st, 201, "{reply}");
    assert_eq!(reply["parent_id"], serde_json::json!(root_id));
    assert_eq!(reply["thread_id"], serde_json::json!(root_id));
    assert_eq!(reply["path"], serde_json::json!("fees.rs"));
    assert_eq!(reply["line"], serde_json::json!(1));
    assert_eq!(reply["line_end"], serde_json::json!(3));
    assert_eq!(reply["side"], serde_json::json!("new"));
    let reply_id = reply["id"].as_str().unwrap().to_string();

    // A reply to a reply is a forum, and refused in words. So is a
    // reply that tries to carry its own anchor, and an unknown side.
    for (body, want) in [
        (
            serde_json::json!({"body": "and again", "parent_id": reply_id}),
            "one level deep",
        ),
        (
            serde_json::json!({"body": "elsewhere", "parent_id": root_id, "path": "OWNERS"}),
            "inherits its thread's anchor",
        ),
        (
            serde_json::json!({"body": "sideways", "path": "fees.rs", "line": 1, "side": "left"}),
            "unknown side",
        ),
        (
            serde_json::json!({"body": "backwards", "path": "fees.rs", "line": 3, "line_end": 1}),
            "must not precede",
        ),
        (
            serde_json::json!({"body": "unanchored", "side": "old"}),
            "old-side comment needs a path",
        ),
    ] {
        let (st, out) = as_person(
            server,
            &alice,
            "POST",
            &format!("{cp}/comments"),
            Some(body),
        );
        assert_eq!(st, 400, "{out}");
        assert!(out["error"].as_str().unwrap().contains(want), "{out}");
    }
    assert!(server.healthy());

    // A revision rewrites the file underneath every one of them. The
    // anchor as first written survives beside the live one, so a client
    // can say "written on patchset 1, line 1" rather than silently
    // drawing the comment at a line it was never about.
    commit(
        server,
        admin,
        "app",
        "feature",
        "fees v2\n\nChange-Id: I7bead001\n",
        &[(
            "fees.rs",
            "// header\n// header\nfn fee() -> u32 {\n    42\n}\n",
        )],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.get(&format!("{cp}/comments"), admin);
    assert_eq!(st, 200, "{out}");
    let list = out["comments"].as_array().unwrap();
    assert_eq!(list.len(), 3, "{out}");
    for c in list {
        assert_eq!(c["original_patchset"], serde_json::json!(1), "{c}");
        assert_eq!(c["original_line"], c["line"], "{c}");
        assert_eq!(c["patchset"], serde_json::json!(1), "{c}");
    }
    // Threads group by thread_id and stay in the order they were
    // spoken, which is the order a review reads in.
    assert_eq!(list[0]["thread_id"], serde_json::json!(root_id));
    assert_eq!(list[1]["thread_id"], list[1]["id"]);
    assert_eq!(list[2]["thread_id"], serde_json::json!(root_id));

    // A comment made *now* is against patchset 2, and says so.
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({"body": "42, good", "path": "fees.rs", "line": 4})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["original_patchset"], serde_json::json!(2));
    assert!(server.healthy());
}

/// Who may call a thread settled, and what settling it does *not* do.
///
/// This is the computed-reviewer thesis applied to resolution: the
/// repository already answers "whose opinion counts about this file",
/// so resolution asks that question rather than inventing a second
/// permission. The two refusals are the point — a writer with no
/// standing on the path cannot dismiss a remark about it, and neither
/// can the change's own author, because a review you can dismiss
/// yourself is not a review.
///
/// And the thing it must not do: an unresolved thread is a rendered
/// fact, not a land blocker. The change lands here with one still open.
#[test]
fn only_the_author_or_an_owner_of_the_path_resolves_and_landing_is_unmoved() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-resolve");
    let scratch = Scratch::new("changes-resolve");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    // Two governed areas with different owners, so "an owner" is never
    // the same person as "any owner in the repository".
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[
            ("OWNERS", "casey@acme.test\n"),
            // `set noparent`, so that Casey — who owns the root and
            // will write the comment below — is deliberately *not* an
            // owner of the path it is anchored to. Without that, the
            // inherited root entry would make Casey an owner here and
            // the author rule this test is about would never be reached.
            ("payments/OWNERS", "set noparent\n@payments\n"),
        ],
    );
    branch(server, admin, "app", "feature", "main");
    commit(
        server,
        admin,
        "app",
        "feature",
        "gateway\n\nChange-Id: I5e771ed1\n",
        &[("payments/gateway.rs", "fn charge() {}\n")],
    );
    let dev = sign_in(server, "dev@acme.test");
    let alice = sign_in(server, "alice@acme.test");
    let casey = sign_in(server, "casey@acme.test");
    let vic = sign_in(server, "vic@acme.test");
    // Dev opens the change, so the author of the change and the person
    // refused below are the same person.
    let (st, out) = as_person(
        server,
        &dev,
        "POST",
        "/v1/orgs/acme/repos/app/changes",
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let ps_commit = out["patchset"]["commit"].as_str().unwrap().to_string();
    let cp = "/v1/orgs/acme/repos/app/changes/I5e771ed1";

    // Casey owns the root but not payments/, and comments there anyway:
    // anyone who can read may speak. Who may declare it *dealt with* is
    // the separate question this test is about.
    let (st, out) = as_person(
        server,
        &casey,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({
            "body": "no idempotency key on the charge",
            "path": "payments/gateway.rs",
            "line": 1,
        })),
    );
    assert_eq!(st, 201, "{out}");
    let cid = out["id"].as_str().unwrap().to_string();
    let resolve = format!("{cp}/comments/{cid}/resolve");
    let unresolve = format!("{cp}/comments/{cid}/unresolve");

    // Refused, and each for its own reason:
    // - dev opened the change and can push to it, and owns nothing here;
    // - vic can read the repository and nothing more;
    // - a service token has no judgement to offer about a remark.
    for (who, label) in [(&dev, "the change's author"), (&vic, "a viewer")] {
        let (st, out) = as_person(server, who, "POST", &resolve, None);
        assert_eq!(st, 403, "{label}: {out}");
        assert!(
            out["error"].as_str().unwrap().contains("may resolve it"),
            "{label}: {out}"
        );
    }
    let (st, out) = server.post(&resolve, admin, None);
    assert_eq!(st, 403, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("person's judgement"),
        "{out}"
    );
    // Still open, and the server is still serving.
    let (st, out) = server.get(&format!("{cp}/comments"), admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["comments"][0]["resolved"], serde_json::json!(false));
    assert!(server.healthy());

    // Alice satisfies payments/ through the @payments team — the same
    // resolution that decides whether her approval counts.
    let (st, out) = as_person(server, &alice, "POST", &resolve, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["resolved"], serde_json::json!(true));
    assert_eq!(out["resolved_by"], serde_json::json!("Alice"));
    assert!(out["resolved_at"].as_i64().is_some(), "{out}");

    // Casey wrote it, so Casey may reopen it: withdrawing — or
    // un-withdrawing — your own remark needs nobody's permission.
    let (st, out) = as_person(server, &casey, "POST", &unresolve, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["resolved"], serde_json::json!(false));
    assert_eq!(out["resolved_by"], serde_json::Value::Null);

    // A reply cannot be resolved; the thread it is in can.
    let (st, reply) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({"body": "adding one", "parent_id": cid})),
    );
    assert_eq!(st, 201, "{reply}");
    let rid = reply["id"].as_str().unwrap();
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments/{rid}/resolve"),
        None,
    );
    assert_eq!(st, 400, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("resolve the thread"),
        "{out}"
    );

    // An id that names no comment of this change is absent, not an
    // error, and a well-shaped id from nowhere leaks nothing either.
    for bogus in ["not-an-id", "0000000000000000000000000"] {
        let (st, out) = as_person(
            server,
            &alice,
            "POST",
            &format!("{cp}/comments/{bogus}/resolve"),
            None,
        );
        assert_eq!(st, 404, "{bogus}: {out}");
    }
    // A change key this repository never had is absent too — the door
    // is read in order, so a real comment id under a fictional change
    // must not reach the comment at all.
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("/v1/orgs/acme/repos/app/changes/Ifade0001/comments/{cid}/resolve"),
        None,
    );
    assert_eq!(st, 404, "{out}");
    // And a caller with no credential at all never gets as far as the
    // thread: the repository door answers first, so nothing about the
    // comment — not even that it is there — is decided by an anonymous
    // request.
    let (st, out) = server.post(&resolve, "", None);
    assert_eq!(st, 401, "{out}");
    assert!(server.healthy());

    // The thread is still open — and the change lands anyway. This is
    // the decision the migration comment records: the land gate blocks
    // on things somebody said deliberately, and an unresolved nit is
    // not one of them.
    let (st, out) = as_person(server, &alice, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204, "{out}");
    let (st, out) = as_person(server, &casey, "POST", &format!("{cp}/approve"), None);
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["verdict"]["landable"], serde_json::json!(true), "{out}");
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 202, "{out}");
    let out = wait_until_not(server, admin, "I5e771ed1", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("landed"), "{out}");
    assert_eq!(out["change"]["landed_commit"], serde_json::json!(ps_commit));
    let (st, out) = server.get(&format!("{cp}/comments"), admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        out["comments"][0]["resolved"],
        serde_json::json!(false),
        "it landed with the thread still open, which is the point"
    );
    assert!(server.healthy());
}

/// Commit arbitrary operations (the `commit` helper above only puts), so
/// a patchset can delete a file — which is half of what an interdiff has
/// to get right.
fn commit_ops(
    server: &Server,
    token: &str,
    branch_name: &str,
    message: &str,
    ops: Vec<serde_json::Value>,
) -> String {
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

/// `(status, path)` for every entry of an interdiff, sorted, so an
/// assertion says what a reviewer would see rather than what the planner
/// happened to emit first.
fn interdiff(server: &Server, token: &str, from: u32, to: u32) -> Vec<(String, String)> {
    let (st, out) = server.get(
        &format!("/v1/orgs/acme/repos/app/changes/Ideadbee1/interdiff?from={from}&to={to}"),
        token,
    );
    assert_eq!(st, 200, "interdiff {from}->{to}: {out}");
    assert_eq!(out["from_patchset"], serde_json::json!(from), "{out}");
    assert_eq!(out["to_patchset"], serde_json::json!(to), "{out}");
    let mut rows: Vec<(String, String)> = out["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("changes array: {out}"))
        .iter()
        .map(|c| {
            (
                c["status"].as_str().unwrap().to_string(),
                c["path"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    rows.sort();
    rows
}

/// The property the endpoint exists for.
///
/// A reviewer returning to patchset 3 wants the diff from the patchset
/// they last read, and that is not the diff of the newest patchset
/// against its parent. A file touched in patchset 2 and put back in
/// patchset 3 is in *that* diff and must not be in this one — the
/// reviewer never saw the detour, and showing it to them is asking them
/// to re-read code that is byte-identical to what they approved of.
/// The same holds for a file born and buried between the two ends.
#[test]
fn an_interdiff_spans_the_range_asked_for_not_the_newest_patchsets_parent() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-interdiff");
    let scratch = Scratch::new("changes-interdiff");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);

    let base = commit(
        server,
        admin,
        "app",
        "main",
        "base",
        &[
            ("README.md", "hi"),
            ("blip.txt", "one"),
            ("old.txt", "keep"),
        ],
    );
    branch(server, admin, "app", "feature", &base);

    // Patchset 1: the work as first proposed.
    commit(
        server,
        admin,
        "app",
        "feature",
        "add gateway\n\nChange-Id: Ideadbee1\n",
        &[("payments/gateway.rs", "v1")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["patchset"]["number"], serde_json::json!(1));

    // Patchset 2: a detour. `blip.txt` is edited and `temp.txt` appears;
    // neither survives to patchset 3.
    commit(
        server,
        admin,
        "app",
        "feature",
        "detour\n\nChange-Id: Ideadbee1\n",
        &[("blip.txt", "two"), ("temp.txt", "scratch")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["patchset"]["number"], serde_json::json!(2));

    // Patchset 3: the detour is undone byte for byte, the scratch file
    // is removed, a file that predates the change is deleted, and one
    // genuinely new file arrives.
    commit_ops(
        server,
        admin,
        "feature",
        "undo the detour\n\nChange-Id: Ideadbee1\n",
        vec![
            serde_json::json!({"op": "put", "path": "blip.txt", "content": "one"}),
            serde_json::json!({"op": "delete", "path": "temp.txt"}),
            serde_json::json!({"op": "delete", "path": "old.txt"}),
            serde_json::json!({"op": "put", "path": "docs.md", "content": "d"}),
        ],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["patchset"]["number"], serde_json::json!(3));

    // 1 → 2 lists the detour, because at patchset 2 it was really there.
    assert_eq!(
        interdiff(server, admin, 1, 2),
        vec![
            ("added".to_string(), "temp.txt".to_string()),
            ("modified".to_string(), "blip.txt".to_string()),
        ]
    );

    // 1 → 3 does not. `blip.txt` holds its patchset-1 content again and
    // `temp.txt` never existed at either end; `old.txt` really is gone
    // and `docs.md` really is new, so the answer is not merely empty.
    assert_eq!(
        interdiff(server, admin, 1, 3),
        vec![
            ("added".to_string(), "docs.md".to_string()),
            ("deleted".to_string(), "old.txt".to_string()),
        ],
        "a file touched and untouched between the ends is not in the range"
    );

    // And the contrast that makes the endpoint worth having: the diff a
    // naive client would show — newest patchset against the one before —
    // does contain both, which is exactly the re-reading it causes.
    let naive = interdiff(server, admin, 2, 3);
    assert!(
        naive.contains(&("modified".to_string(), "blip.txt".to_string())),
        "{naive:?}"
    );
    assert!(
        naive.contains(&("deleted".to_string(), "temp.txt".to_string())),
        "{naive:?}"
    );

    // Reversed is the "what would I be undoing" view, and is allowed.
    assert_eq!(
        interdiff(server, admin, 3, 1),
        vec![
            ("added".to_string(), "old.txt".to_string()),
            ("deleted".to_string(), "docs.md".to_string()),
        ]
    );

    // The commit oids ride along beside the numbers, and they are the
    // ones the patchset list records — a client can join the two without
    // a second request.
    let (st, detail) = server.get("/v1/orgs/acme/repos/app/changes/Ideadbee1", admin);
    assert_eq!(st, 200, "{detail}");
    let (st, out) = server.get(
        "/v1/orgs/acme/repos/app/changes/Ideadbee1/interdiff?from=1&to=3",
        admin,
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["from"], detail["patchsets"][0]["commit"], "{out}");
    assert_eq!(out["to"], detail["patchsets"][2]["commit"], "{out}");
    assert!(server.healthy());
}

/// Every way of asking for a range that is not a range, and a reader who
/// may not see the repository at all. Each refusal says which, and the
/// server is still serving afterwards.
#[test]
fn interdiff_refuses_bad_ranges_in_words_and_masks_a_stranger() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-interdiff-edges");
    let scratch = Scratch::new("changes-interdiff-edges");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);

    let base = commit(server, admin, "app", "main", "base", &[("README.md", "hi")]);
    branch(server, admin, "app", "feature", &base);
    commit(
        server,
        admin,
        "app",
        "feature",
        "one\n\nChange-Id: Ideadbee1\n",
        &[("a.txt", "1")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");

    let base_path = "/v1/orgs/acme/repos/app/changes/Ideadbee1/interdiff";
    let cases = [
        // Neither parameter, then each half alone: a range needs both
        // ends, and guessing the other one is how a client ships a bug.
        ("", 400),
        ("?from=1", 400),
        ("?to=1", 400),
        // A revision is what `/diff` takes; this endpoint takes numbers,
        // and says so rather than answering something.
        ("?from=HEAD&to=1", 400),
        ("?from=1&to=main", 400),
        ("?from=1.5&to=2", 400),
        // Nothing lies between a patchset and itself.
        ("?from=1&to=1", 400),
        // Numbers this change never had. Patchset 0 does not exist
        // either — numbering starts at 1 — and a negative is the same
        // absence, not a 500 on the way to a query.
        ("?from=1&to=2", 404),
        ("?from=7&to=1", 404),
        ("?from=0&to=1", 404),
        ("?from=-1&to=1", 404),
        ("?from=9999999999999999999999&to=1", 400),
    ];
    for (q, want) in cases {
        let (st, out) = server.get(&format!("{base_path}{q}"), admin);
        assert_eq!(st, want, "{q}: {out}");
        assert!(
            out["error"].as_str().is_some_and(|e| !e.is_empty()),
            "{q} refused without saying why: {out}"
        );
    }

    // An unknown change key is absent, exactly as it is on every other
    // change read — and asking for it does not first reveal whether the
    // range was valid.
    let (st, out) = server.get(
        "/v1/orgs/acme/repos/app/changes/Inosuchchange/interdiff?from=1&to=2",
        admin,
    );
    assert_eq!(st, 404, "{out}");

    // A credential from another organisation is masked with 404, never
    // 403: whether this repository exists is not a stranger's business.
    // No credential at all is 401.
    let rival = server.bootstrap_org("rival");
    let (st, out) = server.get(&format!("{base_path}?from=1&to=2"), &rival);
    assert_eq!(
        st, 404,
        "a rival token must not learn the repo exists: {out}"
    );
    assert_eq!(
        server.status_get(&format!("{base_path}?from=1&to=2"), None),
        401
    );

    // The hostile-input corpus through both new query fields. The status
    // may vary; what must hold is that nothing 500s.
    for inj in INJECTIONS {
        let enc = percent_encode(inj);
        for path in [
            format!("{base_path}?from={enc}&to=1"),
            format!("{base_path}?from=1&to={enc}"),
        ] {
            let st = server.status_get(&path, Some(admin));
            assert!(st == 400 || st == 404, "{path} answered {st}");
        }
    }
    assert!(server.healthy());
}

/// The hint that saves the client from computing the range itself: the
/// newest patchset this person has marked anything viewed at.
///
/// Absent — `null`, not patchset 1 — for somebody who has ticked no box,
/// because "no last pass" and "your last pass was against the first
/// revision" are different facts and only one of them is true of a
/// reviewer opening a change for the first time.
#[test]
fn the_views_read_says_which_patchset_the_last_pass_was_against() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-views-since");
    let scratch = Scratch::new("changes-views-since");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);

    let base = commit(server, admin, "app", "main", "base", &[("README.md", "hi")]);
    branch(server, admin, "app", "feature", &base);
    commit(
        server,
        admin,
        "app",
        "feature",
        "one\n\nChange-Id: Ideadbee1\n",
        &[("payments/gateway.rs", "v1")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");

    let alice = sign_in(server, "alice@acme.test");
    let views_path = "/v1/orgs/acme/repos/app/changes/Ideadbee1/views";

    let (st, out) = as_person(server, &alice, "GET", views_path, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["patchset"], serde_json::json!(1), "{out}");
    assert!(
        out["since"].is_null(),
        "a reviewer who has marked nothing has no last pass: {out}"
    );

    // One tick, and the hint is the patchset it was made at.
    let (st, out) = as_person(
        server,
        &alice,
        "PUT",
        views_path,
        Some(serde_json::json!({"path": "payments/gateway.rs", "viewed": true})),
    );
    assert_eq!(st, 204, "{out}");
    let (st, out) = as_person(server, &alice, "GET", views_path, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["since"], serde_json::json!(1), "{out}");

    // A second patchset. The mark was made against 1, so that is still
    // where the last pass was — the hint follows the reviewer, not the
    // change, which is the whole reason it is worth sending.
    commit(
        server,
        admin,
        "app",
        "feature",
        "two\n\nChange-Id: Ideadbee1\n",
        &[("payments/gateway.rs", "v2")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = as_person(server, &alice, "GET", views_path, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["patchset"], serde_json::json!(2), "{out}");
    assert_eq!(
        out["since"],
        serde_json::json!(1),
        "the mark was made against patchset 1: {out}"
    );
    // Which is exactly the range the client should now ask for, and it
    // holds the file the revision rewrote. Asked as the person, with the
    // session she is reading the change with — the range hint and the
    // range are one credential's journey, not two.
    let (st, out) = as_person(
        server,
        &alice,
        "GET",
        "/v1/orgs/acme/repos/app/changes/Ideadbee1/interdiff?from=1&to=2",
        None,
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        out["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["status"].as_str().unwrap(), c["path"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        vec![("modified", "payments/gateway.rs")]
    );

    // Somebody else's marks are not this reviewer's last pass. Dev has
    // ticked nothing, and reads null even though alice has ticked.
    let dev = sign_in(server, "dev@acme.test");
    let (st, out) = as_person(server, &dev, "GET", views_path, None);
    assert_eq!(st, 200, "{out}");
    assert!(out["since"].is_null(), "{out}");
    assert!(server.healthy());
}

/// A server with a real mailbox behind it, so "how many mails" is an
/// observable rather than an inference.
fn spawn_with_mail(
    store_url: &str,
    scratch: &Scratch,
    mail: &stratum_testkit::mailbox::Mailbox,
) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .data_dir(scratch.path().join("data"))
        .db_hint("changes-e2e")
        .env("STRATUM_LAND_POLL_SECS", "0.2")
        // The point of a notification is timeliness; a test that waits
        // five seconds an event to prove it teaches nobody anything.
        // A tenth of a second, not a whole one: every mail assertion
        // here waits on the mail, so this knob is only its latency.
        .env("STRATUM_NOTIFY_POLL_SECS", "0.1");
    for (k, v) in mail.env() {
        b = b.env(k, v);
    }
    b.start()
}

/// A change on a fresh branch of `repo`, opened by whoever holds
/// `cookie`. Returns the path prefix its routes hang off.
fn open_change(
    server: &Server,
    admin: &str,
    cookie: &str,
    repo: &str,
    key: &str,
    file: (&str, &str),
) -> String {
    branch(server, admin, repo, key, "main");
    let (st, out) = as_person(
        server,
        cookie,
        "POST",
        &format!("/v1/orgs/acme/repos/{repo}/commits"),
        Some(serde_json::json!({
            "branch": key,
            "message": format!("work\n\nChange-Id: {key}\n"),
            "operations": [{"op": "put", "path": file.0, "content": file.1}],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = as_person(
        server,
        cookie,
        "POST",
        &format!("/v1/orgs/acme/repos/{repo}/changes"),
        Some(serde_json::json!({"from": key})),
    );
    assert_eq!(st, 201, "{out}");
    format!("/v1/orgs/acme/repos/{repo}/changes/{key}")
}

/// The security case this whole feature turns on: a drafted comment
/// belongs to its author and to nobody else until they submit.
///
/// Asserted through every reader there is — a second signed-in person,
/// a service token, and a viewer who may read and not write — because a
/// leak through any one of them publishes somebody's half-formed first
/// reaction under their name, in the one place they cannot take it back
/// from. Anonymous reads nothing at all.
#[test]
fn a_drafted_comment_is_invisible_to_everybody_but_its_author() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-drafts");
    let scratch = Scratch::new("changes-drafts");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);

    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        admin,
        Some(serde_json::json!({"name": "openbook"})),
    );
    assert_eq!(st, 201, "{out}");
    commit(
        server,
        admin,
        "openbook",
        "main",
        "base",
        &[("OWNERS", "alice@acme.test\n"), ("a.txt", "1")],
    );
    let alice = sign_in(server, "alice@acme.test");
    let dev = sign_in(server, "dev@acme.test");
    let vic = sign_in(server, "vic@acme.test");
    let cp = open_change(server, admin, &dev, "openbook", "Id7af7001", ("a.txt", "2"));

    // One remark drafted, one said out loud.
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({
            "body": "not sure about this yet", "path": "a.txt", "pending": true,
        })),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["pending"], serde_json::json!(true));
    assert!(out["review_id"].is_string(), "{out}");
    let draft_id = out["id"].as_str().expect("an id").to_string();
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({"body": "this bit is fine"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["pending"], serde_json::json!(false));

    let bodies = |st_out: (u16, serde_json::Value)| -> Vec<String> {
        let (st, out) = st_out;
        assert_eq!(st, 200, "{out}");
        out["comments"]
            .as_array()
            .expect("comments")
            .iter()
            .map(|c| c["body"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    let published = vec!["this bit is fine".to_string()];
    assert_eq!(
        bodies(server.get(&format!("{cp}/comments"), admin)),
        published,
        "a service token must not see a person's draft"
    );
    assert_eq!(
        bodies(as_person(
            server,
            &dev,
            "GET",
            &format!("{cp}/comments"),
            None
        )),
        published,
        "a second person must not see another person's draft"
    );
    assert_eq!(
        bodies(as_person(
            server,
            &vic,
            "GET",
            &format!("{cp}/comments"),
            None
        )),
        published,
        "a viewer must not see another person's draft"
    );
    let (st, out) = server.get(&format!("{cp}/comments"), "");
    assert_eq!(st, 401, "anonymous read a conversation: {out}");
    assert!(!out.to_string().contains("not sure"), "{out}");
    let mine = bodies(as_person(
        server,
        &alice,
        "GET",
        &format!("{cp}/comments"),
        None,
    ));
    assert_eq!(mine.len(), 2, "the author sees her own draft: {mine:?}");

    // The draft is not a thread until it is published: replying into
    // one answers with the *absent* sentence, so an id cannot be used
    // to probe for the existence of somebody's unsent remark.
    let (st, out) = as_person(
        server,
        &dev,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({"body": "what?", "parent_id": draft_id})),
    );
    assert_eq!(st, 400, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no such comment"),
        "the refusal admits the draft exists: {out}"
    );

    // A token cannot hold a review, because there is no later in which
    // it submits one.
    let (st, out) = server.post(
        &format!("{cp}/comments"),
        admin,
        Some(serde_json::json!({"body": "ci notes", "pending": true})),
    );
    assert_eq!(st, 403, "{out}");
    for path in ["", "/submit", "/withdraw"] {
        let st = server.status_post(
            &format!("{cp}/review{path}"),
            admin,
            serde_json::json!({"verdict": "comment"}),
        );
        assert_eq!(st, 403, "a token reviewed through {path:?}");
    }
    // Reading and discarding one are the same door: a token has no
    // pending review to be shown or thrown away, so neither may answer
    // as though it might.
    for method in ["GET", "DELETE"] {
        let (st, out) = server.req(method, &format!("{cp}/review"), admin, None);
        assert_eq!(st, 403, "a token reached {method} /review: {out}");
    }

    // Her own view of the pending review names the draft, and nothing
    // else's.
    let (st, out) = as_person(server, &alice, "GET", &format!("{cp}/review"), None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["review"]["state"], serde_json::json!("draft"));
    assert_eq!(out["comments"].as_array().unwrap().len(), 1, "{out}");
    let (st, out) = as_person(server, &dev, "GET", &format!("{cp}/review"), None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        out["review"],
        serde_json::Value::Null,
        "somebody else's pending review is not a thing you can read"
    );

    // Submitting publishes it, at which point every reader has it.
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/review/submit"),
        Some(serde_json::json!({"verdict": "comment"})),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["published"], serde_json::json!(1));
    assert_eq!(
        bodies(as_person(
            server,
            &vic,
            "GET",
            &format!("{cp}/comments"),
            None
        ))
        .len(),
        2,
        "submitting is what publishes"
    );

    // Resolution is a person's judgement. Anonymous is told to sign in
    // rather than handed the thread, and the refusal comes before the
    // comment is looked at — the same answer for one that does not exist.
    let (_, out) = server.get(&format!("{cp}/comments"), admin);
    let cid = out["comments"][0]["id"]
        .as_str()
        .expect("a published comment")
        .to_string();
    for id in [cid.as_str(), "01zzzzzzzzzzzzzzzzzzzzzzzz"] {
        let (st, out) = server.post(&format!("{cp}/comments/{id}/resolve"), "", None);
        assert_eq!(st, 401, "{out}");
        assert!(out.to_string().contains("authentication required"), "{out}");
    }
    assert!(server.healthy());
}

/// The draft's own door, driven the way the review pane drives it:
/// open, save, read back, throw away.
///
/// The route matters on its own rather than as a side effect of drafting
/// a comment, because the pane opens a review before there is anything
/// in it — a reviewer who types a cover message and then closes the tab
/// has to find it there when they come back — and because *discarding*
/// has no other door at all. Everything here is asserted through the
/// API, so a draft that was handed back but never stored, or deleted
/// from under its comments, shows up as a wrong answer to the next
/// question rather than as a table nobody reads.
#[test]
fn a_pending_review_is_opened_saved_read_back_and_thrown_away() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-review-door");
    let scratch = Scratch::new("changes-review-door");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "base",
        &[("OWNERS", "alice@acme.test\n"), ("a.txt", "1")],
    );
    let alice = sign_in(server, "alice@acme.test");
    let dev = sign_in(server, "dev@acme.test");
    let cp = open_change(server, admin, &dev, "app", "Idaad0001", ("a.txt", "2"));
    let review = format!("{cp}/review");

    // Nothing open yet is a perfectly good negative answer, not a 404 —
    // every page load asks this question.
    let (st, out) = as_person(server, &alice, "GET", &review, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["review"], serde_json::Value::Null, "{out}");
    assert!(out["comments"].as_array().unwrap().is_empty(), "{out}");

    // Opened with no words at all: the pane opens a review before there
    // is anything in it.
    let (st, out) = as_person(server, &alice, "POST", &review, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["state"], serde_json::json!("draft"), "{out}");
    let id = out["id"].as_str().expect("an id").to_string();
    assert_eq!(out["body"], serde_json::Value::Null, "{out}");

    // Opening again with a cover message is the same draft with the
    // words saved — not a second half-review, and not a read that
    // quietly drops what was just typed.
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &review,
        Some(serde_json::json!({"body": "  the retry loop worries me  "})),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["id"], serde_json::json!(id), "a second draft: {out}");
    assert_eq!(out["body"], serde_json::json!("the retry loop worries me"));
    let (st, out) = as_person(server, &alice, "GET", &review, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["review"]["id"], serde_json::json!(id), "{out}");
    assert_eq!(
        out["review"]["body"],
        serde_json::json!("the retry loop worries me"),
        "the words were handed back but never stored: {out}"
    );

    // Words are prose here as everywhere: refused at the door, and the
    // draft is left exactly as it was.
    for bad in ["   ", "nul\u{0}here"] {
        let (st, out) = as_person(
            server,
            &alice,
            "POST",
            &review,
            Some(serde_json::json!({ "body": bad })),
        );
        assert_eq!(st, 400, "{bad:?} was accepted: {out}");
    }
    let (_, out) = as_person(server, &alice, "GET", &review, None);
    assert_eq!(
        out["review"]["body"],
        serde_json::json!("the retry loop worries me"),
        "a refused body edited the draft anyway: {out}"
    );

    // A drafted comment hangs off it, and discarding takes the comment
    // with it — a draft stranded under no review is one nothing can
    // ever publish and nobody can ever delete.
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({
            "body": "not sure yet", "path": "a.txt", "pending": true,
        })),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["review_id"], serde_json::json!(id), "{out}");
    let (_, out) = as_person(server, &alice, "GET", &review, None);
    assert_eq!(out["comments"].as_array().unwrap().len(), 1, "{out}");

    let (st, out) = as_person(server, &alice, "DELETE", &review, None);
    assert_eq!(st, 204, "{out}");
    let (st, out) = as_person(server, &alice, "GET", &review, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["review"], serde_json::Value::Null, "{out}");
    assert!(out["comments"].as_array().unwrap().is_empty(), "{out}");
    // Her own view of the conversation is empty too: the drafted
    // comment went with the review rather than outliving it.
    let (st, out) = as_person(server, &alice, "GET", &format!("{cp}/comments"), None);
    assert_eq!(st, 200, "{out}");
    assert!(out["comments"].as_array().unwrap().is_empty(), "{out}");
    // Nothing anybody else ever saw was lost, and nothing was published
    // on the way out.
    let (_, out) = server.get(&format!("{cp}/comments"), admin);
    assert!(out["comments"].as_array().unwrap().is_empty(), "{out}");

    // Discarding twice is nothing to discard, not a second act.
    let (st, out) = as_person(server, &alice, "DELETE", &review, None);
    assert_eq!(st, 404, "{out}");

    // The act is on the audit trail: a review that vanished with no
    // record of who threw it away is a hole in the review history.
    let (st, audit) = server.get("/v1/orgs/acme/audit?limit=100", admin);
    assert_eq!(st, 200, "{audit}");
    assert!(
        audit["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "change.review.discard"),
        "{audit}"
    );

    // A change nobody may review any more refuses the door outright,
    // and says which state it is in rather than 404ing a change the
    // caller can plainly read.
    let (st, out) = server.post(&format!("{cp}/abandon"), admin, None);
    assert_eq!(st, 204, "{out}");
    for path in ["", "/submit"] {
        let (st, out) = as_person(
            server,
            &alice,
            "POST",
            &format!("{review}{path}"),
            Some(serde_json::json!({"verdict": "comment"})),
        );
        assert_eq!(st, 409, "review{path:?} on an abandoned change: {out}");
        assert_eq!(out["error"], serde_json::json!("change is abandoned"));
    }
    assert!(server.healthy());
}

/// Rename one table out from under the running server, ask one question,
/// and put it back.
///
/// Surgery on a single table rather than a dropped database on purpose:
/// every statement on the request before the one that touches this table
/// succeeds, so what fails is exactly the step being aimed at. The
/// repository's own database-outage test has to assert `st >= 401`
/// because a dead database is taken at the credential door and the
/// handler is never reached; this is how a *handler's* own arm gets
/// tested.
fn without_table<T>(db_url: &str, table: &str, ask: impl FnOnce() -> T) -> T {
    let mut c = postgres::Client::connect(db_url, postgres::NoTls).expect("a second session");
    c.batch_execute("SET lock_timeout = '10s'").unwrap();
    c.batch_execute(&format!("ALTER TABLE {table} RENAME TO {table}_hidden"))
        .unwrap_or_else(|e| panic!("hide {table}: {e}"));
    let out = ask();
    c.batch_execute(&format!("ALTER TABLE {table}_hidden RENAME TO {table}"))
        .unwrap_or_else(|e| panic!("restore {table}: {e}"));
    out
}

/// Every review route, with the one table it needs taken away.
///
/// The thing being tested is not that a failure is possible — it is that
/// a failure never arrives dressed as an answer. Each of these routes has
/// a perfectly plausible empty reading: a changes page with no rows says
/// "you have nothing open", `{"review": null}` says "you have no draft"
/// and invites a client to start a second one over the top of the first,
/// a 404 from discard says "there was nothing to throw away", and an
/// empty interdiff says "these two patchsets are identical". Every one of
/// those is a lie a person would act on. So each has to be a 500.
///
/// The mutating routes carry a second claim, asserted after the tables
/// are back: the draft is still open with its remark still unsaid and the
/// thread still unresolved. A refusal that half-happened would be worse
/// than either outcome.
#[test]
fn a_missing_table_is_a_failure_on_every_review_route_and_never_an_empty_answer() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-dbfault");
    let scratch = Scratch::new("changes-dbfault");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let db = server.db_url.clone();
    let base = commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n"), ("core.rs", "v0")],
    );
    branch(server, admin, "app", "feature", &base);
    let alice = sign_in(server, "alice@acme.test");
    let casey = sign_in(server, "casey@acme.test");
    for n in [1, 2] {
        commit(
            server,
            admin,
            "app",
            "feature",
            "work\n\nChange-Id: Idbfa0001\n",
            &[("core.rs", &format!("v{n}"))],
        );
        let (st, out) = server.post(
            "/v1/orgs/acme/repos/app/changes",
            admin,
            Some(serde_json::json!({"from": "feature"})),
        );
        assert_eq!(st, 201, "{out}");
    }
    let cp = "/v1/orgs/acme/repos/app/changes/Idbfa0001";
    // Casey's remark, so resolving it is a judgement about the path
    // rather than the author withdrawing their own words.
    let (st, out) = as_person(
        server,
        &casey,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({"body": "why v2?", "path": "core.rs", "line": 1})),
    );
    assert_eq!(st, 201, "{out}");
    let cid = out["id"].as_str().expect("an id").to_string();
    // Alice holds a draft with one unsaid remark in it.
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/review"),
        Some(serde_json::json!({"body": "half a thought"})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({
            "body": "not yet", "path": "core.rs", "line": 1, "pending": true,
        })),
    );
    assert_eq!(st, 201, "{out}");

    let by_author = percent_encode("author:casey@acme.test");
    let needs = percent_encode("needs:my-approval");
    let org = "/v1/orgs/acme/changes";
    let repo_changes = "/v1/orgs/acme/repos/app/changes";

    // Every read, healthy, so a 500 below is the missing table and not
    // a request this server was never going to answer.
    let reads: Vec<(&str, String)> = vec![
        ("the org page, by author", format!("{org}?q={by_author}")),
        ("the org page", org.to_string()),
        ("the per-repo page", repo_changes.to_string()),
        (
            "the per-repo waiting-on-me page",
            format!("{repo_changes}?q={needs}"),
        ),
        ("the change", cp.to_string()),
        ("the interdiff", format!("{cp}/interdiff?from=1&to=2")),
        ("the pending review", format!("{cp}/review")),
    ];
    for (what, path) in &reads {
        let (st, out) = as_person(server, &alice, "GET", path, None);
        assert_eq!(st, 200, "{what} before the surgery: {out}");
    }

    // One table away, one question, one refusal. The table names are the
    // point: each is the first table on that route that the handler
    // itself reads, which is what makes the 500 that handler's own arm
    // rather than the credential door's.
    // The author term, asked anonymously. This used to be the one shape
    // where nothing read `users` before the term resolved, and healthy it
    // answered an empty page — so a failure answering the same page would
    // have been indistinguishable from the truth. Every repository is
    // private to its organisation now, and anonymous is refused at the
    // door before any term is read: a 401, healthy or not, and never a
    // page of any kind. The door needs no `users` to say so.
    let author_page = format!("{org}?q={by_author}");
    let (st, out) = server.get(&author_page, "");
    assert_eq!(st, 401, "{out}");
    let (st, out) = without_table(&db, "users", || server.get(&author_page, ""));
    assert_eq!(
        st, 401,
        "an anonymous author query was answered rather than refused: {st} {out}"
    );

    // The same surgery behind a session cookie fails at resolving who is
    // asking, and that refusal matters more than it looks. A signed-in
    // person whose session cannot be resolved must not be quietly
    // demoted to anonymous and told to sign in on a page they are signed
    // in on, with nothing anywhere saying why.
    let (st, out) = without_table(&db, "users", || {
        as_person(server, &alice, "GET", &author_page, None)
    });
    assert_eq!(
        st, 500,
        "a session that could not be resolved read as signed out: {st} {out}"
    );

    let cases: Vec<(&str, &str, &str, String, Option<serde_json::Value>)> = vec![
        // The list routes.
        ("repos", "the org page", "GET", org.to_string(), None),
        ("changes", "the org page", "GET", org.to_string(), None),
        (
            "changeset_members",
            "the org page",
            "GET",
            org.to_string(),
            None,
        ),
        ("patchsets", "the org page", "GET", org.to_string(), None),
        (
            "patchsets",
            "the per-repo page",
            "GET",
            repo_changes.to_string(),
            None,
        ),
        (
            "patchsets",
            "the per-repo waiting-on-me page",
            "GET",
            format!("{repo_changes}?q={needs}"),
            None,
        ),
        // The change, and the range between two patchsets of it.
        ("reviews", "the change", "GET", cp.to_string(), None),
        (
            "patchsets",
            "the interdiff",
            "GET",
            format!("{cp}/interdiff?from=1&to=2"),
            None,
        ),
        // The review's own door, every verb.
        (
            "reviews",
            "drafting a comment",
            "POST",
            format!("{cp}/comments"),
            Some(serde_json::json!({"body": "x", "pending": true})),
        ),
        (
            "patchsets",
            "opening a review",
            "POST",
            format!("{cp}/review"),
            None,
        ),
        (
            "reviews",
            "opening a review",
            "POST",
            format!("{cp}/review"),
            None,
        ),
        (
            "reviews",
            "reading the pending review",
            "GET",
            format!("{cp}/review"),
            None,
        ),
        (
            "change_comments",
            "reading the pending review",
            "GET",
            format!("{cp}/review"),
            None,
        ),
        (
            "reviews",
            "discarding the review",
            "DELETE",
            format!("{cp}/review"),
            None,
        ),
        (
            "patchsets",
            "submitting the review",
            "POST",
            format!("{cp}/review/submit"),
            Some(serde_json::json!({"verdict": "comment"})),
        ),
        (
            "reviews",
            "submitting the review",
            "POST",
            format!("{cp}/review/submit"),
            Some(serde_json::json!({"verdict": "comment"})),
        ),
        (
            "reviews",
            "withdrawing a block",
            "POST",
            format!("{cp}/review/withdraw"),
            None,
        ),
        // Settling somebody else's thread.
        (
            "change_comments",
            "resolving a thread",
            "POST",
            format!("{cp}/comments/{cid}/resolve"),
            None,
        ),
        (
            "patchsets",
            "resolving a thread",
            "POST",
            format!("{cp}/comments/{cid}/resolve"),
            None,
        ),
    ];
    for (table, what, method, path, body) in &cases {
        let (st, out) = without_table(&db, table, || {
            as_person(server, &alice, method, path, body.clone())
        });
        assert_eq!(st, 500, "{what} answered without {table}: {st} {out}");
    }

    // Nothing the refused writes were asked to do happened. The draft is
    // where it was, its remark is still unsaid, and casey's thread is
    // still open.
    let (st, out) = as_person(server, &alice, "GET", &format!("{cp}/review"), None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["review"]["state"], serde_json::json!("draft"), "{out}");
    assert_eq!(out["review"]["body"], serde_json::json!("half a thought"));
    assert_eq!(out["comments"].as_array().unwrap().len(), 1, "{out}");
    let (_, out) = server.get(&format!("{cp}/comments"), admin);
    let published = out["comments"].as_array().unwrap();
    assert_eq!(
        published.len(),
        1,
        "a draft was published by a failure: {out}"
    );
    assert_eq!(published[0]["resolved"], serde_json::json!(false), "{out}");

    // And with every table back, each of those acts goes through.
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments/{cid}/resolve"),
        None,
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["resolved"], serde_json::json!(true), "{out}");
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/review/submit"),
        Some(serde_json::json!({"verdict": "comment"})),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["published"], serde_json::json!(1), "{out}");
    assert!(server.healthy());
}

/// A twelve-comment pass is one email, and drafting sends none at all.
///
/// This is the fix migration 0049 could only half make: it stopped
/// twelve *identical* jobs queuing at once, but a sender claiming one
/// mid-review started a fresh one, so the count was a race. Now there
/// is one act to report and one report of it.
#[test]
fn a_whole_review_is_one_notification_and_drafting_is_none() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-review-mail");
    let scratch = Scratch::new("changes-review-mail");
    let mailbox = stratum_testkit::mailbox::Mailbox::temp("changes-review-mail");
    let w = world(spawn_with_mail(&bucket.base_url, &scratch, &mailbox));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n"), ("core.rs", "v0")],
    );
    let alice = sign_in(server, "alice@acme.test");
    let dev = sign_in(server, "dev@acme.test");
    let cp = open_change(server, admin, &dev, "app", "Ifeed0001", ("core.rs", "v1"));

    for i in 0..4 {
        let (st, out) = as_person(
            server,
            &alice,
            "POST",
            &format!("{cp}/comments"),
            Some(serde_json::json!({
                "body": format!("note {i}"), "path": "core.rs", "line": 1, "pending": true,
            })),
        );
        assert_eq!(st, 201, "{out}");
    }
    // Drafting is asserted below rather than after a settle window here.
    // The notifier claims jobs oldest first (`jobs::claim` orders by
    // `created_at`), so a job any of these drafts enqueued is claimed —
    // and its mail written — before the one the review submit below
    // enqueues. The *first* mail this author ever receives therefore
    // says whether drafting mailed, and waiting for a mail that must
    // arrive beats sleeping past ticks a busy machine may not have run.

    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/review/submit"),
        Some(serde_json::json!({"verdict": "comment", "body": "a few notes"})),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["published"], serde_json::json!(4));
    mailbox.wait_for("dev@acme.test", Duration::from_secs(20));
    // The oldest mail in the box, not the newest: this is the drafting
    // claim. Four pending comments enqueued nothing, so the first thing
    // the author hears about is the review.
    let first = mailbox
        .to("dev@acme.test")
        .into_iter()
        .next()
        .expect("a mail just arrived");
    assert!(
        first.text.contains("reviewed"),
        "drafting mailed the author before the review was submitted: {} / {}",
        first.subject,
        first.text
    );

    // ...and a "no" reads as a different sentence, because only one of
    // the two is asking the author to do something. Somebody filtering
    // on the subject line has to be able to tell them apart without
    // opening either.
    let casey = sign_in(server, "casey@acme.test");
    let (st, out) = as_person(
        server,
        &casey,
        "POST",
        &format!("{cp}/review/submit"),
        Some(serde_json::json!({
            "verdict": "request_changes", "body": "not while the retry is unbounded",
        })),
    );
    assert_eq!(st, 200, "{out}");
    let said = wait_for(
        "the 'requested changes' mail",
        Duration::from_secs(20),
        || {
            mailbox
                .to("dev@acme.test")
                .into_iter()
                .find(|m| m.subject.contains("requested changes on"))
        },
    );
    assert!(
        said.text.contains("requested changes on"),
        "the body does not say what happened: {}",
        said.text
    );

    // And *now* count the first review's mails. This mail is the
    // barrier: it comes from a job enqueued after the four comments
    // were published, and the notifier claims oldest first, so every
    // mail the earlier review was ever going to send has been written
    // by the time this one exists. That is a happens-after the product
    // guarantees, where the settle window it replaces was a guess.
    let reviewed: Vec<_> = mailbox
        .to("dev@acme.test")
        .into_iter()
        .filter(|m| !m.subject.contains("requested changes on"))
        .collect();
    assert_eq!(
        reviewed.len(),
        1,
        "a four-comment review sent {} mails: {:?}",
        reviewed.len(),
        reviewed
            .iter()
            .map(|m| m.subject.clone())
            .collect::<Vec<_>>()
    );
    assert!(server.healthy());
}

/// A block from somebody OWNERS names stops the queue, survives the
/// next patchset, and ends only when its author withdraws it.
///
/// The middle one is the whole argument. An approval dies structurally
/// when new code arrives — the approver never saw it — and if a block
/// died the same way, the author would clear every objection by
/// force-pushing over it, which is exactly the move the objection
/// exists to stop.
#[test]
fn an_owners_block_stops_the_land_gate_and_survives_a_new_patchset() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-block");
    let scratch = Scratch::new("changes-block");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[
            ("OWNERS", "casey@acme.test\n"),
            ("payments/OWNERS", "alice@acme.test\n"),
            ("payments/gateway.rs", "v0"),
        ],
    );
    let alice = sign_in(server, "alice@acme.test");
    let casey = sign_in(server, "casey@acme.test");
    let dev = sign_in(server, "dev@acme.test");
    let cp = open_change(
        server,
        admin,
        &dev,
        "app",
        "Ib10c0001",
        ("payments/gateway.rs", "v1"),
    );

    // A review verdict of `approve` writes the very row the sufficiency
    // engine reads — no second engine, and no second answer.
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/review/submit"),
        Some(serde_json::json!({"verdict": "approve", "body": "ship it"})),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["approved"], serde_json::json!(true));
    let (st, out) = server.get(&cp, admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        out["approvals"].as_array().unwrap().len(),
        1,
        "the review left the approval the gate reads: {out}"
    );
    let reviews = out["reviews"].as_array().expect("reviews");
    assert_eq!(reviews.len(), 1, "{out}");
    assert_eq!(reviews[0]["verdict"], serde_json::json!("approve"));
    assert_eq!(reviews[0]["body"], serde_json::json!("ship it"));
    assert_eq!(reviews[0]["author"], serde_json::json!("Alice"));
    let (st, out) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["verdict"]["landable"], serde_json::json!(true), "{out}");

    // Casey owns the root, which governs this path through the chain,
    // so his "no" is authoritative.
    let (st, out) = as_person(
        server,
        &casey,
        "POST",
        &format!("{cp}/review/submit"),
        Some(serde_json::json!({
            "verdict": "request_changes", "body": "the retry loop is unbounded",
        })),
    );
    assert_eq!(st, 200, "{out}");
    let (_, out) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(
        out["verdict"]["landable"],
        serde_json::json!(false),
        "{out}"
    );
    let explanation = out["verdict"]["explanation"].as_str().unwrap_or_default();
    assert!(
        explanation.contains("casey@acme.test") && explanation.contains("asked for changes"),
        "the refusal does not say who or what: {explanation}"
    );
    let blocks = out["blocks"].as_array().expect("blocks");
    assert_eq!(blocks.len(), 1, "{out}");
    assert_eq!(blocks[0]["blocking"], serde_json::json!(true));
    assert_eq!(blocks[0]["verdict"], serde_json::json!("request_changes"));
    assert_eq!(
        blocks[0]["body"],
        serde_json::json!("the retry loop is unbounded")
    );

    // The button refuses in those words rather than accepting a landing
    // the queue would eject.
    let (st, out) = as_person(server, &dev, "POST", &format!("{cp}/land"), None);
    assert_eq!(st, 409, "{out}");

    // A new patchset. The approval dies with it; the block does not.
    let (st, out) = as_person(
        server,
        &dev,
        "POST",
        "/v1/orgs/acme/repos/app/commits",
        Some(serde_json::json!({
            "branch": "Ib10c0001",
            "message": "work\n\nChange-Id: Ib10c0001\n",
            "operations": [{"op": "put", "path": "payments/gateway.rs", "content": "v2"}],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = as_person(
        server,
        &dev,
        "POST",
        "/v1/orgs/acme/repos/app/changes",
        Some(serde_json::json!({"from": "Ib10c0001"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/review/submit"),
        Some(serde_json::json!({"verdict": "approve"})),
    );
    assert_eq!(st, 200, "{out}");
    let (_, out) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(
        out["verdict"]["landable"],
        serde_json::json!(false),
        "a force-push cleared a block: {out}"
    );
    assert_eq!(out["blocks"].as_array().unwrap().len(), 1, "{out}");

    // Nobody else can lift it — the route withdraws the caller's own
    // block and cannot name anybody else's.
    let (st, out) = as_person(server, &dev, "POST", &format!("{cp}/review/withdraw"), None);
    assert_eq!(st, 404, "{out}");
    let (_, out) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(
        out["verdict"]["landable"],
        serde_json::json!(false),
        "{out}"
    );

    let (st, out) = as_person(
        server,
        &casey,
        "POST",
        &format!("{cp}/review/withdraw"),
        None,
    );
    assert_eq!(st, 204, "{out}");
    let (_, out) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(out["verdict"]["landable"], serde_json::json!(true), "{out}");
    assert!(out["blocks"].as_array().unwrap().is_empty(), "{out}");

    // ...and the change lands, which is the proof that the lander asks
    // the same question the button did.
    let (st, out) = as_person(server, &dev, "POST", &format!("{cp}/land"), None);
    assert_eq!(st, 202, "{out}");
    let out = wait_until_not(server, admin, "Ib10c0001", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("landed"), "{out}");
    assert!(server.healthy());
}

/// A "no" from somebody the repository does not ask about these paths
/// is recorded, rendered, and moves nothing.
///
/// GitHub lets any passer-by wedge a pull request. We can do better
/// precisely because the reviewer set is computed rather than
/// nominated, so the answer to "does this opinion count here" is a fact
/// rather than a policy — and it is the same fact that decides whether
/// an approval counts.
#[test]
fn a_block_from_somebody_owners_does_not_name_is_advisory() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-advisory");
    let scratch = Scratch::new("changes-advisory");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[
            ("OWNERS", "casey@acme.test\n"),
            ("payments/OWNERS", "alice@acme.test\n"),
            ("payments/gateway.rs", "v0"),
        ],
    );
    let alice = sign_in(server, "alice@acme.test");
    let dev = sign_in(server, "dev@acme.test");
    let cp = open_change(
        server,
        admin,
        &dev,
        "app",
        "Ia0d15001",
        ("payments/gateway.rs", "v1"),
    );

    // Dev has write access and no standing here: no OWNERS rule on this
    // path names him, and neither rule says `*`.
    let (st, out) = as_person(
        server,
        &dev,
        "POST",
        &format!("{cp}/review/submit"),
        Some(serde_json::json!({"verdict": "request_changes", "body": "I would not"})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/review/submit"),
        Some(serde_json::json!({"verdict": "approve"})),
    );
    assert_eq!(st, 200, "{out}");

    let (_, out) = server.get(&format!("{cp}/verdict"), admin);
    assert_eq!(
        out["verdict"]["landable"],
        serde_json::json!(true),
        "a passer-by wedged the change: {out}"
    );
    let blocks = out["blocks"].as_array().expect("blocks");
    assert_eq!(blocks.len(), 1, "the objection is still recorded: {out}");
    assert_eq!(
        blocks[0]["blocking"],
        serde_json::json!(false),
        "and labelled advisory: {out}"
    );
    assert_eq!(blocks[0]["body"], serde_json::json!("I would not"));

    // Asking for changes with nothing to act on is refused: a wall with
    // no door tells the author no and never what would make it a yes.
    let (st, out) = as_person(
        server,
        &dev,
        "POST",
        &format!("{cp}/review/submit"),
        Some(serde_json::json!({"verdict": "request_changes"})),
    );
    assert_eq!(st, 400, "{out}");
    let (st, out) = as_person(
        server,
        &dev,
        "POST",
        &format!("{cp}/review/submit"),
        Some(serde_json::json!({"verdict": "reject"})),
    );
    assert_eq!(st, 400, "{out}");

    let (st, out) = as_person(server, &dev, "POST", &format!("{cp}/land"), None);
    assert_eq!(st, 202, "{out}");
    let out = wait_until_not(server, admin, "Ia0d15001", "landing");
    assert_eq!(out["change"]["state"], serde_json::json!("landed"), "{out}");
    assert!(server.healthy());
}

/// The keys of a list page, in the order returned.
fn keys(out: &serde_json::Value) -> Vec<String> {
    out["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("changes array in {out}"))
        .iter()
        .map(|c| c["key"].as_str().unwrap().to_string())
        .collect()
}

/// Open a change on its own branch off trunk, as `who` — a session
/// cookie, because who *opened* a change is the thing `author:` filters
/// on and a bearer token is nobody.
fn open_in(
    server: &Server,
    admin: &str,
    who: &str,
    repo: &str,
    key: &str,
    file: &str,
) -> serde_json::Value {
    let br = format!("f-{key}");
    branch(server, admin, repo, &br, "main");
    commit(
        server,
        admin,
        repo,
        &br,
        &format!("work on {file}\n\nChange-Id: {key}\n"),
        &[(file, "v1")],
    );
    let (st, out) = as_person(
        server,
        who,
        "POST",
        &format!("/v1/orgs/acme/repos/{repo}/changes"),
        Some(serde_json::json!({"from": br})),
    );
    assert_eq!(st, 201, "open {key}: {out}");
    assert_eq!(out["change"]["key"], serde_json::json!(key), "{out}");
    out
}

/// The query bar is the API: every term filters as it says, and a term
/// nothing understands is refused **by name**.
///
/// The refusal is the half worth testing hardest. A filter that quietly
/// did nothing would let somebody read an unfiltered wall of changes,
/// conclude none of them is theirs and close the tab — and the same URL,
/// pasted into a review thread, would mean something different to
/// whoever opened it next.
#[test]
fn the_query_bar_filters_what_it_claims_and_refuses_what_it_does_not_know() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-query");
    let scratch = Scratch::new("changes-query");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "casey@acme.test\n"), ("README.md", "hi")],
    );
    let alice = sign_in(server, "alice@acme.test");
    let dev = sign_in(server, "dev@acme.test");
    open_in(server, admin, &alice, "app", "Iaa0000001", "a.rs");
    open_in(server, admin, &dev, "app", "Iaa0000002", "b.rs");
    open_in(server, admin, &alice, "app", "Iaa0000003", "c.rs");
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        "/v1/orgs/acme/repos/app/changes/Iaa0000003/abandon",
        None,
    );
    assert_eq!(st, 204, "{out}");

    let list = "/v1/orgs/acme/repos/app/changes";
    let q = |terms: &str| format!("{list}?q={}", percent_encode(terms));

    // is: — the state, newest first, and the abandoned one only where it
    // was asked for.
    let (st, out) = server.get(&q("is:open"), admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(keys(&out), vec!["Iaa0000002", "Iaa0000001"], "{out}");
    let (_, out) = server.get(&q("is:abandoned"), admin);
    assert_eq!(keys(&out), vec!["Iaa0000003"], "{out}");

    // author: — @me is whoever is asking, and an address is whoever it
    // names.
    let (st, out) = as_person(server, &alice, "GET", &q("author:@me"), None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(keys(&out), vec!["Iaa0000003", "Iaa0000001"], "{out}");
    let (_, out) = as_person(server, &dev, "GET", &q("author:@me"), None);
    assert_eq!(keys(&out), vec!["Iaa0000002"], "{out}");
    let (_, out) = server.get(&q("author:dev@acme.test"), admin);
    assert_eq!(keys(&out), vec!["Iaa0000002"], "{out}");
    // Terms compose.
    let (_, out) = as_person(server, &alice, "GET", &q("is:open author:@me"), None);
    assert_eq!(keys(&out), vec!["Iaa0000001"], "{out}");

    // A well-shaped address that names nobody here is an empty page, not
    // an unfiltered one — and deliberately not a refusal, which would
    // turn a list a stranger can read into an address oracle.
    let (st, out) = server.get(&q("author:nobody@acme.test"), admin);
    assert_eq!(st, 200, "{out}");
    assert!(keys(&out).is_empty(), "{out}");

    // Every refusal names the thing it did not understand.
    for (terms, quoted) in [
        ("assignee:me", "assignee"),
        ("gateway", "gateway"),
        ("is:merged", "merged"),
        ("needs:review", "review"),
        ("author:not-an-email", "not-an-email"),
        ("is:open is:landed", "is"),
    ] {
        let (st, out) = server.get(&q(terms), admin);
        assert_eq!(st, 400, "{terms:?} was accepted: {out}");
        assert!(
            out["error"].as_str().unwrap_or_default().contains(quoted),
            "refusing {terms:?} did not name {quoted:?}: {out}"
        );
    }

    // `repo:` is a filter on the org-wide list; here it would be a
    // client believing it is filtering when this list is already one
    // repository, so it is refused rather than ignored.
    let (st, out) = server.get(&q("repo:app"), admin);
    assert_eq!(st, 400, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("/v1/orgs/acme/changes"),
        "{out}"
    );

    // `?state=` predates the query bar and still works — but the two
    // disagreeing is a bug in the caller, and answering one of them
    // would be the silent-filter failure in a new place.
    let (st, out) = server.get(&format!("{list}?state=open"), admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(keys(&out).len(), 2, "{out}");
    let (st, out) = server.get(&format!("{list}?state=open&q=is:landed"), admin);
    assert_eq!(st, 400, "{out}");

    // Both terms that are about a person refuse a caller who is not one,
    // rather than answering an empty page that reads as "nothing to do".
    for terms in ["author:@me", "needs:my-approval"] {
        let (st, out) = server.get(&q(terms), admin);
        assert_eq!(st, 403, "{terms} with a service token: {out}");
        assert_eq!(server.status_get(&q(terms), None), 401, "{terms} anonymous");
    }

    // The hostile-input corpus through the new query fields: the answers
    // vary, nothing 500s, and the server is still serving.
    for inj in INJECTIONS {
        let enc = percent_encode(inj);
        for path in [
            format!("{list}?q={enc}"),
            format!("{list}?q=is:{enc}"),
            format!("{list}?q=author:{enc}"),
            format!("{list}?after={enc}"),
            format!("{list}?limit={enc}"),
        ] {
            let st = server.status_get(&path, Some(admin));
            assert!(st == 400 || st == 200, "{path} answered {st}");
        }
    }
    assert!(server.healthy());
}

/// A keyset walk sees every change exactly once, even when somebody
/// opens one while it is halfway down.
///
/// The cursor names a row rather than counting them, which is the whole
/// argument for it: changes arrive at the *top* of this ordering, so an
/// offset of two after one new change re-reads a row it already returned
/// and never reaches the one below. Asserted as a walk of the whole list
/// rather than as two page fetches, because the failure this pins is a
/// gap, and a gap is invisible unless something counts what came back.
#[test]
fn paging_a_change_list_never_repeats_a_row_and_never_skips_one() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-paging");
    let scratch = Scratch::new("changes-paging");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(server, admin, "app", "main", "base", &[("README.md", "hi")]);
    let alice = sign_in(server, "alice@acme.test");
    // Collected then reversed, not `.map(…).rev()`: reversing the
    // iterator would reverse the *opening*, and the order these are
    // opened in is the order they are expected back in.
    let mut made: Vec<String> = (1..=5)
        .map(|i| {
            let key = format!("Ibb000000{i}");
            open_in(server, admin, &alice, "app", &key, &format!("f{i}.rs"));
            key
        })
        .collect();
    made.reverse();

    let list = "/v1/orgs/acme/repos/app/changes";
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut inserted_yet = false;
    for _ in 0..10 {
        let path = match &cursor {
            Some(c) => format!("{list}?limit=2&after={c}"),
            None => format!("{list}?limit=2"),
        };
        let (st, out) = server.get(&path, admin);
        assert_eq!(st, 200, "{out}");
        seen.extend(keys(&out));
        // A sixth change, opened while the reader is halfway down.
        if !inserted_yet {
            inserted_yet = true;
            open_in(server, admin, &alice, "app", "Ibb0000009", "late.rs");
        }
        match out["next"].as_str() {
            Some(n) => cursor = Some(n.to_string()),
            None => break,
        }
    }
    assert_eq!(
        seen, made,
        "the walk returned every pre-existing change exactly once, newest \
         first, and the one opened mid-walk — which sorts above the cursor \
         — displaced none of them"
    );

    // A cursor this list did not mint is a client bug and says so, where
    // silently answering page one would look like the walk restarting on
    // its own.
    let (st, out) = server.get(&format!("{list}?after=not-a-cursor"), admin);
    assert_eq!(st, 400, "{out}");
    assert!(server.healthy());
}

/// `needs:my-approval` is the term no other forge can answer, and it is
/// **computed**: OWNERS names who a change cannot land without.
///
/// Three properties, and each is one somebody would notice by the list
/// being useless: a change whose paths OWNERS gives to somebody else is
/// not mine to approve; a change on a `*` path requires *nobody* in
/// particular, so write access is not standing and listing it there
/// would put every ungoverned file in front of every writer in the
/// organisation; and a change that already has what it needs — or that I
/// have already spoken on — has stopped waiting for me.
#[test]
fn needs_my_approval_is_the_owners_requirement_and_not_write_access() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-needs");
    let scratch = Scratch::new("changes-needs");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[
            ("OWNERS", "casey@acme.test\n"),
            // Two owners: either satisfies the path, which is what lets
            // a change become landable without *me* having approved it.
            (
                "payments/OWNERS",
                "alice@acme.test\ncasey@acme.test\nset noparent\n",
            ),
            // Nobody in particular: whoever has write may approve, so
            // nobody is required.
            ("docs/OWNERS", "*\nset noparent\n"),
            ("sealed/OWNERS", "casey@acme.test\nset noparent\n"),
            ("README.md", "hi"),
        ],
    );
    let alice = sign_in(server, "alice@acme.test");
    let casey = sign_in(server, "casey@acme.test");
    let dev = sign_in(server, "dev@acme.test");
    open_in(
        server,
        admin,
        &dev,
        "app",
        "Icc000001",
        "payments/gateway.rs",
    );
    open_in(server, admin, &dev, "app", "Icc000002", "docs/guide.md");
    // Two paths, two owners lists: alice satisfies payments and only
    // casey can satisfy sealed, so alice's own approval leaves it open.
    let br = "f-two";
    branch(server, admin, "app", br, "main");
    commit(
        server,
        admin,
        "app",
        br,
        "two places\n\nChange-Id: Icc000003\n",
        &[("payments/a.rs", "v1"), ("sealed/k.rs", "v1")],
    );
    let (st, out) = as_person(
        server,
        &dev,
        "POST",
        "/v1/orgs/acme/repos/app/changes",
        Some(serde_json::json!({"from": br})),
    );
    assert_eq!(st, 201, "{out}");

    let needs = format!(
        "/v1/orgs/acme/repos/app/changes?q={}",
        percent_encode("needs:my-approval")
    );

    // Alice owns payments; the `*` change is not hers to be waiting on.
    let (st, out) = as_person(server, &alice, "GET", &needs, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        keys(&out),
        vec!["Icc000003", "Icc000001"],
        "the docs change is governed by `*`, where nobody is required: {out}"
    );

    // Dev has write everywhere and owns nothing, so nothing is waiting
    // on dev — including the `*` change dev could perfectly well
    // approve. Being *able* to approve is not being *required*, and that
    // distinction is the whole difference between a list somebody trusts
    // and one they filter away.
    let (st, out) = as_person(server, &dev, "GET", &needs, None);
    assert_eq!(st, 200, "{out}");
    assert!(keys(&out).is_empty(), "{out}");

    // Casey approves Icc000001. It is landable now, so it has stopped
    // waiting on alice — even though OWNERS still names her.
    let (st, out) = as_person(
        server,
        &casey,
        "POST",
        "/v1/orgs/acme/repos/app/changes/Icc000001/approve",
        None,
    );
    assert_eq!(st, 204, "{out}");
    let (_, out) = as_person(server, &alice, "GET", &needs, None);
    assert_eq!(keys(&out), vec!["Icc000003"], "{out}");

    // Alice approves Icc000003. It is still blocked — sealed/ needs casey
    // — but she has said her piece, so it is no longer waiting on her.
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        "/v1/orgs/acme/repos/app/changes/Icc000003/approve",
        None,
    );
    assert_eq!(st, 204, "{out}");
    let (_, out) = server.get("/v1/orgs/acme/repos/app/changes/Icc000003/verdict", admin);
    assert_eq!(
        out["verdict"]["landable"],
        serde_json::json!(false),
        "sealed/ still needs casey: {out}"
    );
    let (_, out) = as_person(server, &alice, "GET", &needs, None);
    assert!(keys(&out).is_empty(), "{out}");

    assert!(server.healthy());
}

/// The org-wide list takes the same query, plus the one term only it can
/// mean: `repo:`.
///
/// The masking is the half worth testing. A `repo:` naming something the
/// caller may not read is an **empty page**, never a refusal — a refusal
/// would confirm the repository exists, which is exactly the fact every
/// other surface in this product hides behind a 404.
#[test]
fn the_org_wide_query_narrows_by_repository_and_masks_what_it_cannot_see() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-orgquery");
    let scratch = Scratch::new("changes-orgquery");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    for name in ["web", "vault"] {
        let (st, out) = server.post(
            "/v1/orgs/acme/repos",
            admin,
            Some(serde_json::json!({"name": name})),
        );
        assert_eq!(st, 201, "{out}");
    }
    // `app` is where OWNERS lives, so it is the repository that can be
    // *waiting* on somebody.
    commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n"), ("README.md", "hi")],
    );
    for repo in ["web", "vault"] {
        commit(server, admin, repo, "main", "base", &[("README.md", "hi")]);
    }
    let alice = sign_in(server, "alice@acme.test");
    let dev = sign_in(server, "dev@acme.test");
    open_in(server, admin, &dev, "app", "Idd000001", "core.rs");
    open_in(server, admin, &dev, "web", "Idd000002", "site.rs");
    open_in(server, admin, &alice, "vault", "Idd000003", "keys.rs");

    let org = "/v1/orgs/acme/changes";
    let q = |terms: &str| format!("{org}?q={}", percent_encode(terms));
    let (st, out) = server.get(org, admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        keys(&out),
        vec!["Idd000003", "Idd000002", "Idd000001"],
        "{out}"
    );

    // `repo:` narrows to one, and the row still names its repository.
    let (st, out) = server.get(&q("repo:web"), admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(keys(&out), vec!["Idd000002"], "{out}");
    assert_eq!(out["changes"][0]["repo"], serde_json::json!("web"), "{out}");
    // Terms compose here too.
    let (_, out) = as_person(server, &dev, "GET", &q("is:open author:@me repo:app"), None);
    assert_eq!(keys(&out), vec!["Idd000001"], "{out}");

    // A repository this caller may not read, and one that does not
    // exist, answer the same empty page — so neither tells the other
    // apart, and neither is confirmed. Another organisation's token may
    // read none of acme's repositories, so every one of them is that
    // page for it.
    let rival = server.bootstrap_org("rival");
    for repo in ["vault", "web", "nosuchrepo"] {
        let (st, out) = server.get(&q(&format!("repo:{repo}")), &rival);
        assert_eq!(st, 200, "{out}");
        assert!(keys(&out).is_empty(), "repo:{repo} for a rival org: {out}");
    }
    let (st, out) = server.get(org, &rival);
    assert_eq!(st, 200, "{out}");
    assert!(
        keys(&out).is_empty(),
        "the whole org for a rival org: {out}"
    );
    // Anonymous is told to sign in rather than handed any page, for a
    // repository that exists and one that does not.
    for repo in ["vault", "nosuchrepo"] {
        let (st, out) = server.get(&q(&format!("repo:{repo}")), "");
        assert_eq!(st, 401, "repo:{repo} anonymously: {out}");
    }
    // …while a viewer of the org reads exactly what the per-repo list
    // shows them: the query changed nothing about who may see what.
    let vic = sign_in(server, "vic@acme.test");
    let (_, out) = as_person(server, &vic, "GET", &q("repo:web"), None);
    assert_eq!(keys(&out), vec!["Idd000002"], "{out}");

    // An address that names nobody is an empty page, not everything.
    let (st, out) = server.get(&q("author:nobody@acme.test"), admin);
    assert_eq!(st, 200, "{out}");
    assert!(keys(&out).is_empty(), "{out}");

    // `needs:my-approval` spans repositories the same way, and answers
    // only for the one whose OWNERS names her.
    let (st, out) = as_person(server, &alice, "GET", &q("needs:my-approval"), None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(keys(&out), vec!["Idd000001"], "{out}");

    // And the page walks with a cursor across repositories, because the
    // ordering is one sequence of changes rather than one per repo.
    let (_, first) = server.get(&format!("{org}?limit=2"), admin);
    assert_eq!(keys(&first), vec!["Idd000003", "Idd000002"], "{first}");
    let cursor = first["next"].as_str().expect("a cursor");
    let (_, second) = server.get(&format!("{org}?limit=2&after={cursor}"), admin);
    assert_eq!(keys(&second), vec!["Idd000001"], "{second}");
    assert_eq!(second["next"], serde_json::Value::Null, "{second}");

    // A change nobody can approve any more is not waiting on anybody.
    // The term carries no `is:open` of its own, so the abandoned row
    // arrives here and has to be dropped on the way out — otherwise the
    // one screen that answers "what is waiting on me" fills up with
    // work that ended.
    open_in(server, admin, &dev, "app", "Idd000004", "gone.rs");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes/Idd000004/abandon",
        admin,
        None,
    );
    assert_eq!(st, 204, "{out}");
    let (st, out) = as_person(server, &alice, "GET", &q("needs:my-approval"), None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        keys(&out),
        vec!["Idd000001"],
        "an abandoned change is still waiting on her: {out}"
    );

    // A credential that does not resolve at all is a 401 and never an
    // empty page: masking is for a caller whose credential is good and
    // whose reach is small, and a caller with a typo'd or revoked token
    // has to be told the token is the problem rather than shown an
    // organisation with nothing in it.
    let (st, out) = server.get(org, "not-a-real-token");
    assert_eq!(st, 401, "{out}");

    // `author:@me` asks who the caller *is*, not what they may do — so
    // it has to resolve for somebody signed in to this product and not
    // a member of this organisation. Their page is empty because every
    // repository is still decided one at a time, but the term itself
    // must not fall over: a signed-in stranger who got a 401 here would
    // be told to sign in on a page they are already signed in on.
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "rival",
            "--email",
            "stranger@rival.test",
            "--name",
            "Stranger",
            "--password",
            PASSWORD,
            "--role",
            "member",
        ])
        .expect("a person in another org");
    let stranger = sign_in(server, "stranger@rival.test");
    let (st, out) = as_person(server, &stranger, "GET", &q("author:@me"), None);
    assert_eq!(st, 200, "{out}");
    assert!(keys(&out).is_empty(), "{out}");
    // …and resolving who they are gave them no authority: acme's
    // repositories are as empty a page for them as for any other
    // organisation's credential, while the viewer above reads them.
    let (st, out) = as_person(server, &stranger, "GET", &q("repo:web"), None);
    assert_eq!(st, 200, "{out}");
    assert!(keys(&out).is_empty(), "{out}");

    let (st, out) = server.get(&q("assignee:me"), admin);
    assert_eq!(st, 400, "{out}");
    assert!(server.healthy());
}

// ---------------------------------------------------------------------
// Suggested changes: a reviewer writes a fenced suggestion block, and
// the author takes a set of them as one new patchset.
// ---------------------------------------------------------------------

/// Leave an anchored comment as a person and answer its id.
fn remark(server: &Server, cookie: &str, cp: &str, body: serde_json::Value) -> String {
    let (st, out) = as_person(
        server,
        cookie,
        "POST",
        &format!("{cp}/comments"),
        Some(body),
    );
    assert_eq!(st, 201, "{out}");
    out["id"].as_str().expect("a comment id").to_string()
}

/// How many patchsets the change has — the observable every refusal
/// below has to leave alone.
fn patchset_count(server: &Server, admin: &str, cp: &str) -> usize {
    let (st, out) = server.get(cp, admin);
    assert_eq!(st, 200, "{out}");
    out["patchsets"].as_array().expect("patchsets").len()
}

/// Two suggestions from one reviewer become **one** patchset, the files
/// read as suggested, and a clone taken afterwards passes I11.
///
/// The fsck is not decoration: this route makes blobs, trees and a
/// commit by hand, and an object graph this server wrote that real git
/// refuses is the one failure that cannot be allowed to ship. The new
/// patchset sits on no branch — it is pinned as `refs/patchsets/<oid>`,
/// like every other patchset — so the clone fetches that ref explicitly
/// and fscks again with the new objects in the repository.
/// The apply door masks a repository you cannot read, and 404s a change
/// that is not there.
///
/// Read access is checked *before* the write refusal on purpose: the
/// write sentence names the repository, so answering it to somebody who
/// cannot see the repository would confirm it exists. A stranger gets the
/// same absence they would get for a repository that was never created.
#[test]
fn the_apply_door_masks_what_you_cannot_read_and_404s_a_change_that_is_not_there() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-suggest-door");
    let scratch = Scratch::new("changes-suggest-door");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let base = commit(
        server,
        admin,
        "app",
        "main",
        "base",
        &[("OWNERS", "alice@acme.test\n"), ("core.rs", "one\ntwo\n")],
    );
    branch(server, admin, "app", "feature", &base);
    commit(
        server,
        admin,
        "app",
        "feature",
        "work\n\nChange-Id: I5099ee04\n",
        &[("core.rs", "one\ntwo\nthree\n")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");

    // A change key nothing here has: 404, not a 500 and not a guess.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes/I0000dead/suggestions/apply",
        admin,
        Some(serde_json::json!({"comments": ["01whatever"]})),
    );
    assert_eq!(st, 404, "{out}");

    // Somebody with no credential at all is told to authenticate, and
    // somebody from another organisation that the repository is absent
    // — never that they lack write access, which would name it. Each is
    // the answer a repository that does not exist gets.
    let rival = server.bootstrap_org("rival");
    for (who, token, expect) in [("anonymous", "", 401), ("a rival org", rival.as_str(), 404)] {
        let apply = |repo: &str| {
            server.req(
                "POST",
                &format!("/v1/orgs/acme/repos/{repo}/changes/I5099ee04/suggestions/apply"),
                token,
                Some(serde_json::json!({"comments": ["01whatever"]})),
            )
        };
        let (st, out) = apply("app");
        assert_eq!(
            st, expect,
            "an unreadable repository must be masked, not refused with a sentence \
             naming it — {who}: {out}"
        );
        assert_eq!((st, out), apply("no-such-repo"), "{who}");
    }
    assert_eq!(server.get("/healthz", admin).0, 200);
}

/// A store that will not answer inside `make` is a failure, not an empty
/// patchset.
///
/// `make` reads the repository's locator header on its way to working out
/// which objects are new (`absent_from_layout` -> `Plane::load` ->
/// `GET <prefix>/locator.hdr`). Nothing earlier in this request reads that
/// key — the handler's own manifest read is a different one — so refusing
/// it lands on `make`'s `?` and nowhere else. Aimed by experiment, not by
/// argument: a blanket `GET` refusal is taken by the manifest read first
/// and proves nothing about this arm.
#[test]
fn a_suggestion_whose_layout_cannot_be_read_is_a_failure_not_an_empty_patchset() {
    let (proxy, scratch) = proxied("changes-suggest-layout");
    let w = world(spawn_server(&proxy.url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let base = commit(
        server,
        admin,
        "app",
        "main",
        "base",
        &[("OWNERS", "alice@acme.test\n"), ("core.rs", "one\ntwo\n")],
    );
    branch(server, admin, "app", "feature", &base);
    commit(
        server,
        admin,
        "app",
        "feature",
        "work\n\nChange-Id: I5099ee03\n",
        &[("core.rs", "one\ntwo\nthree\n")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/I5099ee03";
    let alice = sign_in(server, "alice@acme.test");
    let c1 = remark(
        server,
        &alice,
        cp,
        serde_json::json!({
            "body": "plainly:\n```suggestion\nTWO\n```\n",
            "path": "core.rs",
            "line": 2,
        }),
    );

    proxy.handle.inject("locator.hdr", 1_000, 503);
    let (st, out) = server.post(
        &format!("{cp}/suggestions/apply"),
        admin,
        Some(serde_json::json!({"comments": [c1]})),
    );
    assert!(
        st >= 500,
        "a store that cannot answer must not read as an applied suggestion: {st} {out}"
    );

    proxy.handle.clear();
    let (st, out) = server.post(
        &format!("{cp}/suggestions/apply"),
        admin,
        Some(serde_json::json!({"comments": [c1]})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(server.get("/healthz", admin).0, 200);
}

/// A suggestion that cannot be pinned leaves nothing behind.
///
/// The point of applying several suggestions in one call is that the
/// author gets all of them or none. `make` writes the blobs, the tree and
/// the commit, and only then does `register_patchset` pin the result as
/// `refs/patchsets/<oid>` — so there is a window where the objects exist
/// and the patchset does not. If that window leaked, the change would
/// show a patchset count that did not match its history, or worse, the
/// author would be told their suggestions applied when nothing is
/// reachable.
///
/// **Which arm this actually reaches**, established by mutation rather
/// than by argument, because the first version of this comment was wrong.
/// Refusing `PUT manifest.json` does not reach `register_patchset`'s
/// error arm: `make` writes the pack and the ref through the store first,
/// so the refusal is answered by *its* `Err(e) => err_to_response(e)` a
/// few lines above. Turning `register_patchset`'s arm into a teapot left
/// this test green, which is the proof.
///
/// So this pins the store-failure half — an apply that cannot write its
/// objects leaves nothing on the record — and the abandoned-under-apply
/// test below pins `register_patchset`'s arm, which needs a fault aimed
/// at the change row mid-request rather than at the store.
#[test]
fn a_suggestion_that_cannot_be_pinned_applies_nothing_and_the_change_is_untouched() {
    let (proxy, scratch) = proxied("changes-suggest-fault");
    let w = world(spawn_server(&proxy.url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let base = commit(
        server,
        admin,
        "app",
        "main",
        "base",
        &[("OWNERS", "alice@acme.test\n"), ("core.rs", "one\ntwo\n")],
    );
    branch(server, admin, "app", "feature", &base);
    commit(
        server,
        admin,
        "app",
        "feature",
        "work\n\nChange-Id: I5099ee02\n",
        &[("core.rs", "one\ntwo\nthree\n")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/I5099ee02";
    let alice = sign_in(server, "alice@acme.test");
    let c1 = remark(
        server,
        &alice,
        cp,
        serde_json::json!({
            "body": "say it plainly:\n```suggestion\nTWO\n```\n",
            "path": "core.rs",
            "line": 2,
        }),
    );

    let before = server.get(cp, admin).1;
    let patchsets_before = before["patchsets"].as_array().map(|a| a.len()).unwrap_or(0);

    // Only the ref truth is refused; every object write still succeeds.
    proxy.handle.inject("PUT manifest.json", 1_000, 503);
    let (st, out) = server.post(
        &format!("{cp}/suggestions/apply"),
        admin,
        Some(serde_json::json!({"comments": [c1]})),
    );
    assert!(
        st >= 500,
        "a pin that could not be written must not read as success: {st} {out}"
    );

    // Nothing on the record: the author is not told a patchset exists
    // that nothing can reach.
    let after = server.get(cp, admin).1;
    assert_eq!(
        after["patchsets"].as_array().map(|a| a.len()).unwrap_or(0),
        patchsets_before,
        "a failed apply left a patchset behind: {after}"
    );

    // The store recovers and the same call goes through for real, which
    // is what makes the refusal a fault rather than the fixture.
    proxy.handle.clear();
    let (st, out) = server.post(
        &format!("{cp}/suggestions/apply"),
        admin,
        Some(serde_json::json!({"comments": [c1]})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["paths"], serde_json::json!(["core.rs"]), "{out}");
    assert_eq!(server.get("/healthz", admin).0, 200);
}

#[test]
fn two_suggestions_become_one_patchset_and_the_clone_fscks() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-suggest");
    let scratch = Scratch::new("changes-suggest");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let base = commit(
        server,
        admin,
        "app",
        "main",
        "base",
        &[("OWNERS", "alice@acme.test\n"), ("core.rs", "one\ntwo\n")],
    );
    branch(server, admin, "app", "feature", &base);
    commit(
        server,
        admin,
        "app",
        "feature",
        "work\n\nChange-Id: I5099ee01\n",
        &[
            ("core.rs", "one\ntwo\nthree\nfour\n"),
            ("extra.rs", "alpha\nbeta\n"),
        ],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/I5099ee01";
    let alice = sign_in(server, "alice@acme.test");

    // Two suggestions in **one** file, the earlier of which grows from
    // one line to two — which is what would move the later one's anchor
    // if they were applied top-down — and a third in another file, so
    // the grouping by path is exercised as well as the ordering.
    let c1 = remark(
        server,
        &alice,
        cp,
        serde_json::json!({
            "body": "name them:\n```suggestion\nTWO\nAND A HALF\n```\n",
            "path": "core.rs",
            "line": 2,
        }),
    );
    let c2 = remark(
        server,
        &alice,
        cp,
        serde_json::json!({
            "body": "this one too:\n```suggestion\nFOUR\n```\n",
            "path": "core.rs",
            "line": 4,
        }),
    );
    let c3 = remark(
        server,
        &alice,
        cp,
        serde_json::json!({
            "body": "and spell these out:\n```suggestion\nA\nB\nC\n```",
            "path": "extra.rs",
            "line": 1,
            "line_end": 2,
        }),
    );

    let (st, out) = server.post(
        &format!("{cp}/suggestions/apply"),
        admin,
        Some(serde_json::json!({"comments": [c1, c2, c3]})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(
        out["patchset"]["number"],
        serde_json::json!(2),
        "five suggestions are one act, not one revision each: {out}"
    );
    assert_eq!(
        out["change"]["key"],
        serde_json::json!("I5099ee01"),
        "the Change-Id trailer is what keeps this a patchset of the same change"
    );
    assert_eq!(out["paths"], serde_json::json!(["core.rs", "extra.rs"]));
    let applied = out["patchset"]["commit"].as_str().unwrap().to_string();
    assert_eq!(patchset_count(server, admin, cp), 2, "one new patchset");

    // I11, over the objects this route wrote, through the real git CLI.
    let url = server.authed_url(admin, "acme", "app");
    let clone = scratch.path().join("applied");
    gitcli::clone_and_fsck(&url, &clone);
    gitcli::git(
        &clone,
        &[
            "fetch",
            "-q",
            "origin",
            &format!("refs/patchsets/{applied}:refs/heads/applied"),
        ],
    );
    gitcli::fsck(&clone);
    assert_eq!(
        gitcli::git(&clone, &["show", "applied:core.rs"]),
        "one\nTWO\nAND A HALF\nthree\nFOUR\n",
        "the first suggestion's growth must not move the second's anchor"
    );
    assert_eq!(
        gitcli::git(&clone, &["show", "applied:extra.rs"]),
        "A\nB\nC\n"
    );
    // Nothing else moved: the parent is the patchset the reviewer read,
    // and the previous patchset is still there to read.
    assert_eq!(
        gitcli::git(&clone, &["rev-parse", "applied^"]).trim(),
        server.get(cp, admin).1["patchsets"][0]["commit"]
            .as_str()
            .unwrap()
    );
}

/// An empty suggestion block means *delete these lines*, and it must be
/// distinguishable from a comment carrying no block at all — which is
/// refused a few assertions down in the sibling test.
#[test]
fn an_empty_suggestion_deletes_the_lines() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-suggest-empty");
    let scratch = Scratch::new("changes-suggest-empty");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let base = commit(
        server,
        admin,
        "app",
        "main",
        "base",
        &[("OWNERS", "alice@acme.test\n"), ("core.rs", "keep\n")],
    );
    branch(server, admin, "app", "feature", &base);
    commit(
        server,
        admin,
        "app",
        "feature",
        "work\n\nChange-Id: I5099ee02\n",
        &[(
            "core.rs",
            "keep\ndebug!(\"x\");\ndebug!(\"y\");\nkeep too\n",
        )],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/I5099ee02";
    let alice = sign_in(server, "alice@acme.test");
    let c = remark(
        server,
        &alice,
        cp,
        serde_json::json!({
            "body": "these should go:\n```suggestion\n```\n",
            "path": "core.rs",
            "line": 2,
            "line_end": 3,
        }),
    );
    let (st, out) = server.post(
        &format!("{cp}/suggestions/apply"),
        admin,
        Some(serde_json::json!({"comments": [c]})),
    );
    assert_eq!(st, 201, "{out}");
    let applied = out["patchset"]["commit"].as_str().unwrap();
    let (st, body) = server.req(
        "GET",
        &format!("/v1/orgs/acme/repos/app/files/core.rs?at={applied}"),
        admin,
        None,
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body, serde_json::json!("keep\nkeep too\n"));
}

/// Every way a suggestion can be refused, in words, with the change left
/// exactly as it was — and the server still serving afterwards.
#[test]
fn a_suggestion_that_cannot_be_applied_is_refused_in_words() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-suggest-no");
    let scratch = Scratch::new("changes-suggest-no");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let base = commit(
        server,
        admin,
        "app",
        "main",
        "base",
        &[("OWNERS", "alice@acme.test\n"), ("core.rs", "one\ntwo\n")],
    );
    branch(server, admin, "app", "feature", &base);
    commit(
        server,
        admin,
        "app",
        "feature",
        "work\n\nChange-Id: I5099ee03\n",
        &[("core.rs", "one\ntwo\nthree\n")],
    );
    // A binary file in the same patchset: a suggestion replaces lines,
    // and this has none.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        admin,
        Some(serde_json::json!({
            "branch": "feature",
            "message": "work\n\nChange-Id: I5099ee03\n",
            "operations": [{"op": "put_base64", "path": "logo.png", "content": "//4="}],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/I5099ee03";
    let apply = format!("{cp}/suggestions/apply");
    let alice = sign_in(server, "alice@acme.test");

    // A second change, so "a comment on another change" is a real id and
    // not merely an unknown one.
    branch(server, admin, "app", "other", &base);
    commit(
        server,
        admin,
        "app",
        "other",
        "elsewhere\n\nChange-Id: I5099ee04\n",
        &[("side.rs", "s\n")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "other"})),
    );
    assert_eq!(st, 201, "{out}");
    let elsewhere = remark(
        server,
        &alice,
        "/v1/orgs/acme/repos/app/changes/I5099ee04",
        serde_json::json!({
            "body": "```suggestion\nS\n```",
            "path": "side.rs",
            "line": 1,
        }),
    );

    let sug = |body: &str, extra: serde_json::Value| -> String {
        let mut v = serde_json::json!({"body": body, "path": "core.rs"});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        remark(server, &alice, cp, v)
    };
    let unanchored = remark(
        server,
        &alice,
        cp,
        serde_json::json!({"body": "```suggestion\nX\n```"}),
    );
    let old_side = sug(
        "```suggestion\nX\n```",
        serde_json::json!({"line": 1, "side": "old"}),
    );
    let no_block = sug("just a thought", serde_json::json!({"line": 1}));
    let two_blocks = sug(
        "```suggestion\nA\n```\nor:\n```suggestion\nB\n```",
        serde_json::json!({"line": 1}),
    );
    let noop = sug("```suggestion\none\n```", serde_json::json!({"line": 1}));
    let binary = remark(
        server,
        &alice,
        cp,
        serde_json::json!({
            "body": "```suggestion\nX\n```",
            "path": "logo.png",
            "line": 1,
        }),
    );
    let past_end = sug("```suggestion\nX\n```", serde_json::json!({"line": 9}));
    let over_a = sug(
        "```suggestion\nA\n```",
        serde_json::json!({"line": 1, "line_end": 2}),
    );
    let over_b = sug(
        "```suggestion\nB\n```",
        serde_json::json!({"line": 2, "line_end": 3}),
    );
    // A draft belongs to its author until they submit it; committing one
    // publishes it where they cannot take it back.
    let (st, out) = as_person(
        server,
        &alice,
        "POST",
        &format!("{cp}/comments"),
        Some(serde_json::json!({
            "body": "```suggestion\nX\n```",
            "path": "core.rs",
            "line": 1,
            "pending": true,
        })),
    );
    assert_eq!(st, 201, "{out}");
    let draft = out["id"].as_str().unwrap().to_string();

    let many: Vec<String> = (0..51).map(|i| format!("comment-{i}")).collect();
    for (want, quoted, comments) in [
        (400, "at least one", serde_json::json!([])),
        (400, "more than the 50", serde_json::json!(many)),
        (
            404,
            "no comment",
            serde_json::json!(["01hxxxxxxxxxxxxxxxxxxxxxxx"]),
        ),
        (404, "no comment", serde_json::json!([elsewhere])),
        (400, "still a draft", serde_json::json!([draft])),
        (
            400,
            "not anchored to a line",
            serde_json::json!([unanchored]),
        ),
        (400, "old side of the diff", serde_json::json!([old_side])),
        (
            400,
            "carries no suggestion block",
            serde_json::json!([no_block]),
        ),
        (400, "2 suggestion blocks", serde_json::json!([two_blocks])),
        (
            409,
            "already read exactly as suggested",
            serde_json::json!([noop]),
        ),
        (409, "is not a text file", serde_json::json!([binary])),
        (409, "which has 3 lines", serde_json::json!([past_end])),
        (409, "overlap", serde_json::json!([over_a, over_b])),
    ] {
        let (st, out) = server.post(
            &apply,
            admin,
            Some(serde_json::json!({"comments": comments})),
        );
        assert_eq!(st, want, "{comments} was not refused: {out}");
        let said = out["error"].as_str().unwrap_or_default();
        assert!(said.contains(quoted), "refusing {comments} said {said:?}");
        assert_eq!(
            patchset_count(server, admin, cp),
            1,
            "a refused apply must leave the change alone: {comments}"
        );
    }

    // A failure never arrives dressed as an answer. With the one table
    // the route needs taken away, "no such comment" or "this change has
    // no patchsets" would read to the author as a suggestion that no
    // longer applies, and send them to rewrite a remark that is fine.
    for (table, what) in [
        ("patchsets", "the patchset list"),
        ("change_comments", "the comment"),
    ] {
        let (st, out) = without_table(&server.db_url, table, || {
            server.post(
                &apply,
                admin,
                Some(serde_json::json!({"comments": [noop.clone()]})),
            )
        });
        assert_eq!(
            st, 500,
            "{what} went missing and apply answered anyway: {out}"
        );
    }
    assert_eq!(patchset_count(server, admin, cp), 1);

    // A change whose commit carries no Change-Id has no identity to
    // attach a new patchset to — the docs already say so about rebase
    // and amend, and this is the same rewrite.
    branch(server, admin, "app", "loose", &base);
    let loose = commit(
        server,
        admin,
        "app",
        "loose",
        "no trailer",
        &[("core.rs", "x\n")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "loose"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/app/changes/g{loose}/suggestions/apply"),
        admin,
        Some(serde_json::json!({"comments": [no_block]})),
    );
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("has no Change-Id trailer"),
        "{out}"
    );

    // And a change that is no longer open has no next patchset to make.
    let (st, out) = server.post(&format!("{cp}/abandon"), admin, None);
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.post(&apply, admin, Some(serde_json::json!({"comments": [noop]})));
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["error"], serde_json::json!("change is abandoned"));

    // Still serving: a refusal that wedged the server would be a failure,
    // not a pass.
    assert_eq!(server.status_get("/healthz", None), 200);
    assert_eq!(patchset_count(server, admin, cp), 1);
}

/// A comment from patchset 1 whose file has moved on is refused, naming
/// the file and the patchset it was written against.
///
/// The line numbers a reviewer wrote are line numbers *of the patchset
/// they read*. Applying them to a file that has changed since would
/// commit the reviewer's text over whatever code happens to occupy those
/// lines now, and nothing downstream would notice.
#[test]
fn a_stale_anchor_is_refused_and_names_what_moved() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-suggest-stale");
    let scratch = Scratch::new("changes-suggest-stale");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let base = commit(
        server,
        admin,
        "app",
        "main",
        "base",
        &[("OWNERS", "alice@acme.test\n"), ("core.rs", "one\n")],
    );
    branch(server, admin, "app", "feature", &base);
    commit(
        server,
        admin,
        "app",
        "feature",
        "work\n\nChange-Id: I5099ee05\n",
        &[
            ("core.rs", "one\ntwo\n"),
            ("moved.rs", "here\n"),
            ("gone.rs", "for now\n"),
        ],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/I5099ee05";
    let alice = sign_in(server, "alice@acme.test");
    let on_moved = remark(
        server,
        &alice,
        cp,
        serde_json::json!({"body": "```suggestion\nHERE\n```", "path": "moved.rs", "line": 1}),
    );
    let on_gone = remark(
        server,
        &alice,
        cp,
        serde_json::json!({"body": "```suggestion\nX\n```", "path": "gone.rs", "line": 1}),
    );
    let on_steady = remark(
        server,
        &alice,
        cp,
        serde_json::json!({"body": "```suggestion\nTWO\n```", "path": "core.rs", "line": 2}),
    );

    // Patchset 2 rewrites one of the three files and deletes another.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        admin,
        Some(serde_json::json!({
            "branch": "feature",
            "message": "work\n\nChange-Id: I5099ee05\n",
            "operations": [
                {"op": "put", "path": "moved.rs", "content": "somewhere else\n"},
                {"op": "delete", "path": "gone.rs"},
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["patchset"]["number"], serde_json::json!(2));

    let apply = format!("{cp}/suggestions/apply");
    let (st, out) = server.post(
        &apply,
        admin,
        Some(serde_json::json!({"comments": [on_moved]})),
    );
    assert_eq!(st, 409, "{out}");
    let said = out["error"].as_str().unwrap_or_default();
    assert!(
        said.contains("moved.rs has changed since patchset 1"),
        "{said}"
    );

    let (st, out) = server.post(
        &apply,
        admin,
        Some(serde_json::json!({"comments": [on_gone]})),
    );
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("patchset 2 no longer has gone.rs"),
        "{out}"
    );

    // The file nobody touched is still applicable from patchset 1: the
    // rule is "this file moved", not "you are late".
    let (st, out) = server.post(
        &apply,
        admin,
        Some(serde_json::json!({"comments": [on_steady]})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["patchset"]["number"], serde_json::json!(3), "{out}");
}

/// A reader cannot commit a suggestion, and the sentence says who can.
///
/// A "commit suggestion" button in front of somebody who cannot push is
/// a control that leads nowhere, and the dashboard decides whether to
/// draw it from exactly this answer.
#[test]
fn a_reader_cannot_apply_a_suggestion_and_the_server_stays_healthy() {
    let minio = Minio::shared();
    let bucket = minio.bucket("changes-suggest-reader");
    let scratch = Scratch::new("changes-suggest-reader");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let (server, admin) = (&w.server, &w.admin);
    let base = commit(
        server,
        admin,
        "app",
        "main",
        "base",
        &[("OWNERS", "alice@acme.test\n"), ("core.rs", "one\n")],
    );
    branch(server, admin, "app", "feature", &base);
    commit(
        server,
        admin,
        "app",
        "feature",
        "work\n\nChange-Id: I5099ee06\n",
        &[("core.rs", "one\ntwo\n")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/I5099ee06";
    let alice = sign_in(server, "alice@acme.test");
    let c = remark(
        server,
        &alice,
        cp,
        serde_json::json!({"body": "```suggestion\nTWO\n```", "path": "core.rs", "line": 2}),
    );

    let vic = sign_in(server, "vic@acme.test");
    let (st, out) = as_person(
        server,
        &vic,
        "POST",
        &format!("{cp}/suggestions/apply"),
        Some(serde_json::json!({"comments": [c.clone()]})),
    );
    assert_eq!(st, 403, "{out}");
    let said = out["error"].as_str().unwrap_or_default();
    assert!(said.contains("needs write access"), "{said}");
    assert!(
        said.contains("the change's author applies it"),
        "the refusal has to say who can, or the reader is left guessing: {said}"
    );
    assert_eq!(patchset_count(server, admin, cp), 1);

    // The viewer can still read the change, and somebody who can push
    // can still apply it: the refusal was about the credential.
    assert_eq!(as_person(server, &vic, "GET", cp, None).0, 200);
    let (st, out) = server.post(
        &format!("{cp}/suggestions/apply"),
        admin,
        Some(serde_json::json!({"comments": [c]})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(server.status_get("/healthz", None), 200);
}

/// A change abandoned out from under an apply leaves **nothing** on the
/// record.
///
/// The whole promise of applying several suggestions at once is
/// all-or-none, so the interesting failure is not the store refusing a
/// read — that is answered before anything is built — but the change
/// moving between the state check at the door and the moment the
/// patchset is registered. `register_patchset` re-reads the change
/// inside its own transaction and refuses a change that is no longer
/// open, and this is the one arm that proves it: the request has already
/// built the commit and written its objects, and it still must not
/// record a patchset on an abandoned change.
///
/// The abandon is fired from inside the fault proxy's observer, on the
/// `.oids` PUT that only this apply's own pack write produces. That
/// makes the ordering exact rather than hopeful: by the time it runs the
/// door's state check has already passed, so a refusal here can only
/// have come from the registration.
#[test]
fn a_change_abandoned_under_an_apply_records_no_patchset() {
    let (proxy, scratch) = proxied("changes-suggest-race");
    let w = world(
        Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &proxy.url)
            .data_dir(scratch.path().join("data"))
            .env("STRATUM_LAND_POLL_SECS", "0.2")
            .db_hint("changes-e2e")
            .start(),
    );
    let (server, admin) = (&w.server, &w.admin);
    let base = commit(
        server,
        admin,
        "app",
        "main",
        "rules",
        &[("OWNERS", "alice@acme.test\n"), ("core.rs", "one\n")],
    );
    branch(server, admin, "app", "feature", &base);
    commit(
        server,
        admin,
        "app",
        "feature",
        "work\n\nChange-Id: I5099ee07\n",
        &[("core.rs", "one\ntwo\n")],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/changes",
        admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    let cp = "/v1/orgs/acme/repos/app/changes/I5099ee07";
    let alice = sign_in(server, "alice@acme.test");
    let c = remark(
        server,
        &alice,
        cp,
        serde_json::json!({"body": "```suggestion\nTWO\n```", "path": "core.rs", "line": 2}),
    );

    let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw = fired.clone();
    let base_url = server.base.clone();
    let token = admin.clone();
    proxy.handle.observe(move |line, _seq| {
        if !line.contains("PUT") || !line.contains(".oids") {
            return;
        }
        if saw.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let _ = ureq::post(&format!(
            "{base_url}/v1/orgs/acme/repos/app/changes/I5099ee07/abandon"
        ))
        .set("Authorization", &format!("Bearer {token}"))
        .call();
    });

    let (st, out) = server.post(
        &format!("{cp}/suggestions/apply"),
        admin,
        Some(serde_json::json!({"comments": [c]})),
    );
    proxy.handle.clear_observer();
    assert!(
        fired.load(std::sync::atomic::Ordering::SeqCst),
        "the apply never wrote its pack, so this proves nothing about the door it got past"
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"],
        serde_json::json!("change is abandoned"),
        "{out}"
    );
    let (st, after) = server.get(cp, admin);
    assert_eq!(st, 200, "{after}");
    assert_eq!(after["change"]["state"], serde_json::json!("abandoned"));
    assert_eq!(
        after["patchsets"].as_array().unwrap().len(),
        1,
        "a half-applied suggestion is worse than a refused one: {after}"
    );
    assert!(server.healthy());
}
