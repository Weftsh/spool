//! How an account comes to exist, end to end against a real server.
//!
//! There are exactly three ways: an operator's `admin user-create`,
//! accepting an organization's invitation, and — when the operator
//! configured one — the company's identity provider signing somebody in
//! (`sso_e2e.rs`). Nobody signs themselves up.
//! That is a claim about **absence**, and absence is easy to assert
//! badly — a refused request that left a row behind reads exactly like
//! one that did not — so the negative here asks the control plane's
//! `users` table directly, before and after, rather than trusting any
//! HTTP answer.
//!
//! The positive half is the invitation, which is the only way into a
//! server without SSO for somebody with no account, and which now makes
//! a *whole* account in one transaction: the user, their proved address,
//! and a handle with the personal namespace behind it. The handle is the new
//! part and most of the cases are about it, because it is a name in every
//! clone URL the person will ever hand out:
//!
//! * left out, it is made from the address, and never fails the
//!   invitation over a name the person did not pick;
//! * asked for, it is theirs or refused — `409` when somebody has it,
//!   `400` for a shape or a word this server keeps — and a refusal writes
//!   nothing, so **the same link works again** with another name. An
//!   invitation that a typo could spend would strand the one person it
//!   exists for.
//! * for somebody who already has an account it is ignored: they have a
//!   handle, and an invitation is not a way to mint a second namespace.
//!
//! Every case ends by proving the server is still healthy and serving.

use stratum_testkit::browser::{Browser, PASSWORD};
use stratum_testkit::mailbox::Mailbox;
use stratum_testkit::{gitcli::Scratch, Minio, Server};

fn spawn(store_url: &str, scratch: &Scratch, hint: &str, mail: &Mailbox) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .envs(&mail.env())
        .start()
}

/// Every account on the server, by sign-in address — asked of the
/// control plane, because "nothing was created" is a fact about a table.
fn accounts(server: &Server) -> Vec<String> {
    let mut db =
        postgres::Client::connect(&server.db_url, postgres::NoTls).expect("the control plane");
    db.query("SELECT email FROM users ORDER BY email", &[])
        .expect("list users")
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// Whether a namespace by this name exists, asked of the control plane:
/// over HTTP a stranger is told the same thing for a namespace that
/// exists and one that does not, which is the point of that answer.
fn namespace_exists(server: &Server, name: &str) -> bool {
    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    stratum_control::registry::org_by_name(&db, name)
        .expect("read orgs")
        .is_some()
}

/// `acme`, with an owner signed in.
fn acme(server: &Server) -> Browser<'_> {
    server.bootstrap_org("acme");
    Browser::person(server, "acme", "owner", "olive", "olive@acme.test")
}

/// An invitation into acme for `email`, from the owner's session.
fn invite(owner: &mut Browser, email: &str, role: &str) -> String {
    let (st, inv) = owner.req(
        "POST",
        "/v1/orgs/acme/invites",
        Some(serde_json::json!({ "email": email, "role": role })),
    );
    assert_eq!(st, 201, "invite {email}: {inv}");
    inv["invite_link"]
        .as_str()
        .unwrap_or_else(|| panic!("the invite carries its link: {inv}"))
        .to_string()
}

/// Accept `link` from `browser`, as a new person, asking for `handle`.
fn accept(browser: &mut Browser, link: &str, handle: Option<&str>) -> (u16, serde_json::Value) {
    let mut body = serde_json::json!({
        "invite": link, "name": "Someone New", "password": PASSWORD,
    });
    if let Some(h) = handle {
        body["handle"] = serde_json::json!(h);
    }
    browser.req("POST", "/v1/auth/accept-invite", Some(body))
}

/// The three doors sign-up used to be are gone: each answers exactly as
/// a route that never existed, and none of them makes anybody.
#[test]
fn no_door_makes_an_account() {
    let minio = Minio::shared();
    let bucket = minio.bucket("accounts-nodoor");
    let scratch = Scratch::new("accounts-nodoor");
    let mail = Mailbox::temp("accounts-nodoor");
    let server = spawn(&bucket.base_url, &scratch, "accounts_nodoor", &mail);
    // Somebody is here, so "nobody was made" is not an empty table.
    let _olive = acme(&server);
    let before = accounts(&server);
    assert_eq!(before, ["olive@acme.test"]);

    // What a route nobody ever wrote answers, for the same method and a
    // body: the reference the removed doors are held to. Asserted as an
    // equality so the removed routes cannot be told apart from never
    // having been there — and as a code, so the reference itself cannot
    // quietly become a success.
    //
    // 404. It used to be 405 for every method but GET on every unmatched
    // path — the router's fallback was registered GET-only — so a door
    // that was taken out answered "wrong method" as if it still stood.
    let mut anon = Browser::new(&server);
    let never = anon.req(
        "POST",
        "/v1/auth/no-such-door",
        Some(serde_json::json!({ "email": "x@example.test" })),
    );
    assert_eq!(never.0, 404, "the reference answer moved: {}", never.1);

    let sign_up = serde_json::json!({
        "email": "walk-in@example.test", "name": "Walk In",
        "password": PASSWORD, "handle": "walkin",
    });
    for (path, body) in [
        ("/v1/auth/signup", sign_up.clone()),
        (
            "/v1/auth/verify",
            serde_json::json!({ "token": "weftv_01zzzzzzzzzzzzzzzzzzzzzzzz_x" }),
        ),
        (
            "/v1/auth/resend-verification",
            serde_json::json!({ "email": "walk-in@example.test" }),
        ),
        // Case and a trailing slash are not a way round it.
        ("/v1/auth/SIGNUP", sign_up.clone()),
        ("/v1/auth/signup/", sign_up.clone()),
    ] {
        let (st, out) = anon.req("POST", path, Some(body));
        assert_eq!(
            (st, &out),
            (never.0, &never.1),
            "{path} answers differently from a route that never existed"
        );
        assert!(anon.cookie.is_none(), "{path} handed out a session");
    }

    // Nothing was made: no row, no namespace, no mail, no way in.
    assert_eq!(accounts(&server), before, "a removed door made an account");
    assert!(!namespace_exists(&server, "walkin"));
    assert!(mail.all().is_empty(), "{:?}", mail.all());
    assert_eq!(
        Browser::new(&server).login("walk-in@example.test", PASSWORD),
        401
    );

    assert!(server.healthy());
}

/// `user-create` makes an account with no password only when told to in
/// so many words — on an SSO server that is the right account for the
/// first owner, and anywhere else it is one nobody can sign in to.
#[test]
fn user_create_is_told_whether_there_is_a_password() {
    let minio = Minio::shared();
    let bucket = minio.bucket("accounts-nopw");
    let scratch = Scratch::new("accounts-nopw");
    let mail = Mailbox::temp("accounts-nopw");
    let server = spawn(&bucket.base_url, &scratch, "accounts_nopw", &mail);
    server.bootstrap_org("acme");
    let base = ["admin", "user-create", "--org", "acme", "--email"];

    let err = server.admin_expect_err(&[&base[..], &["neither@acme.test"]].concat());
    assert!(
        err.contains("--password SECRET required") && err.contains("--no-password"),
        "{err}"
    );
    let err = server.admin_expect_err(
        &[
            &base[..],
            &["both@acme.test", "--password", PASSWORD, "--no-password"],
        ]
        .concat(),
    );
    assert!(err.contains("together"), "{err}");
    assert!(
        accounts(&server).is_empty(),
        "a refused command made somebody"
    );

    let made = server.admin_json(&[&base[..], &["sso@acme.test", "--no-password"]].concat());
    assert_eq!(made["user"]["email"], "sso@acme.test", "{made}");
    assert_eq!(accounts(&server), ["sso@acme.test"]);
    assert!(
        namespace_exists(&server, "sso"),
        "a whole account, handle and all"
    );
    for guess in ["", PASSWORD] {
        assert_eq!(Browser::new(&server).login("sso@acme.test", guess), 401);
    }
    assert!(server.healthy());
}

/// Somebody new, accepting with no handle, gets a whole account: a
/// handle made from their address, the namespace behind it, and their
/// address on file as proved — and they are signed in by the answer.
#[test]
fn an_invitation_makes_a_whole_account_with_a_handle_from_the_address() {
    let minio = Minio::shared();
    let bucket = minio.bucket("accounts-derived");
    let scratch = Scratch::new("accounts-derived");
    let mail = Mailbox::temp("accounts-derived");
    let server = spawn(&bucket.base_url, &scratch, "accounts_derived", &mail);
    let mut olive = acme(&server);

    // Mixed case and a dot: the address is normalised, and the dot —
    // which a namespace may not carry — becomes a dash.
    let link = invite(&mut olive, "Grace.Hopper@Example.test", "member");
    let mut grace = Browser::new(&server);
    let (st, me) = accept(&mut grace, &link, None);
    assert_eq!(st, 201, "{me}");
    assert_eq!(me["email"], "grace.hopper@example.test", "{me}");
    assert_eq!(me["handle"], "grace-hopper", "{me}");
    let roles: Vec<(String, String)> = me["orgs"]
        .as_array()
        .expect("orgs")
        .iter()
        .map(|o| (o["name"].to_string(), o["role"].to_string()))
        .collect();
    assert!(
        roles.contains(&("\"acme\"".into(), "\"member\"".into()))
            && roles.contains(&("\"grace-hopper\"".into(), "\"owner\"".into())),
        "not a member of acme and owner of their own namespace: {me}"
    );

    // The answer signed them in, and the namespace is theirs to use.
    let (st, body) = grace.req(
        "POST",
        "/v1/orgs/grace-hopper/repos",
        Some(serde_json::json!({ "name": "notes" })),
    );
    assert_eq!(st, 201, "{body}");
    // Their address is on the account, proved — which is what makes
    // their commits theirs (`contribs_e2e` follows that all the way).
    let (st, body) = grace.req("GET", "/v1/users/grace-hopper/emails", None);
    assert_eq!(st, 200, "{body}");
    let emails = body["emails"].as_array().expect("emails");
    assert_eq!(emails.len(), 1, "{body}");
    assert_eq!(emails[0]["address"], "grace.hopper@example.test");
    assert_eq!(emails[0]["primary"], true, "{body}");
    assert!(emails[0]["verified_at"].is_i64(), "{body}");

    // Nobody else holds the address now: it cannot be claimed onto
    // another account, and the refusal says nothing about whose it is.
    let (st, body) = olive.req(
        "POST",
        "/v1/users/olive/emails",
        Some(serde_json::json!({ "email": "grace.hopper@example.test" })),
    );
    assert_eq!(st, 409, "{body}");

    // A name the person did not pick never fails the invitation: a
    // derived handle somebody has already got takes a suffix instead.
    let link = invite(&mut olive, "grace.hopper@elsewhere.test", "viewer");
    let (st, me) = accept(&mut Browser::new(&server), &link, None);
    assert_eq!(st, 201, "{me}");
    let handle = me["handle"].as_str().expect("a handle");
    assert!(
        handle.starts_with("grace-hopper-") && handle.len() == "grace-hopper-".len() + 6,
        "{me}"
    );
    assert!(namespace_exists(&server, handle), "{handle}");

    assert!(server.healthy());
}

/// A handle somebody asks for is theirs or refused — never quietly
/// swapped for another — and a refusal leaves the link as it was.
#[test]
fn a_chosen_handle_is_refused_plainly_and_the_link_survives_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("accounts-chosen");
    let scratch = Scratch::new("accounts-chosen");
    let mail = Mailbox::temp("accounts-chosen");
    let server = spawn(&bucket.base_url, &scratch, "accounts_chosen", &mail);
    let mut olive = acme(&server);
    let link = invite(&mut olive, "dev@acme.test", "member");
    let before = accounts(&server);

    let mut dev = Browser::new(&server);
    // Somebody's already: an organization, in any case, and a person's
    // own namespace. Namespace names are public, so "taken" is no secret.
    for taken in ["acme", "ACME", "olive"] {
        let (st, out) = accept(&mut dev, &link, Some(taken));
        assert_eq!(st, 409, "{taken:?}: {out}");
    }
    // Words this server keeps — a namespace called `dashboard` could
    // never be cloned, because the dashboard answers first — and shapes
    // that are not names at all. The caller's to fix, so 400, not 409.
    for bad in [
        "dashboard",
        "v1",
        "admin",
        "a b",
        "a/b",
        "..",
        ".dot",
        &"n".repeat(300),
    ] {
        let (st, out) = accept(&mut dev, &link, Some(bad));
        assert_eq!(st, 400, "{bad:?}: {out}");
    }
    // None of that wrote anything or signed anybody in.
    assert!(
        dev.cookie.is_none(),
        "a refused accept handed out a session"
    );
    assert_eq!(
        accounts(&server),
        before,
        "a refused accept made an account"
    );

    // The same link, with a name that is free: theirs, trimmed.
    let (st, me) = accept(&mut dev, &link, Some("  hopper "));
    assert_eq!(st, 201, "the link did not survive its refusals: {me}");
    assert_eq!(me["handle"], "hopper", "{me}");
    assert!(namespace_exists(&server, "hopper"));
    // …and now it is spent, and the name is taken for everybody else.
    let (st, out) = accept(&mut Browser::new(&server), &link, Some("another"));
    assert_eq!(st, 400, "a spent link was accepted again: {out}");
    let second = invite(&mut olive, "someone@acme.test", "member");
    let (st, out) = accept(&mut Browser::new(&server), &second, Some("Hopper"));
    assert_eq!(st, 409, "a handle was handed out twice: {out}");

    assert!(server.healthy());
}

/// Somebody who already has an account keeps it exactly as it is: the
/// invitation adds a role, and a handle in the request — even one that
/// would be refused for a new person — is not a way to mint a second
/// namespace.
#[test]
fn an_existing_account_accepting_keeps_its_own_handle() {
    let minio = Minio::shared();
    let bucket = minio.bucket("accounts-existing");
    let scratch = Scratch::new("accounts-existing");
    let mail = Mailbox::temp("accounts-existing");
    let server = spawn(&bucket.base_url, &scratch, "accounts_existing", &mail);
    let mut olive = acme(&server);
    let mut bob = Browser::stranger(&server, "bob", "bob@example.test");
    let before = accounts(&server);

    for asked in ["robert", "a b"] {
        let link = invite(&mut olive, "bob@example.test", "viewer");
        let (st, me) = bob.req(
            "POST",
            "/v1/auth/accept-invite",
            Some(serde_json::json!({ "invite": link, "name": "", "handle": asked })),
        );
        assert_eq!(st, 201, "{asked:?}: {me}");
        assert_eq!(me["handle"], "bob", "{me}");
        assert!(
            me["orgs"].as_array().is_some_and(|a| a
                .iter()
                .any(|o| o["name"] == "acme" && o["role"] == "viewer")),
            "{me}"
        );
        // Removed again, so the next invitation is a fresh one.
        let id = me["id"].as_str().unwrap().to_string();
        let (st, out) = olive.req("DELETE", &format!("/v1/orgs/acme/members/{id}"), None);
        assert_eq!(st, 204, "{out}");
    }
    assert!(!namespace_exists(&server, "robert"), "a second namespace");
    assert_eq!(accounts(&server), before);
    // Their password is their own still: an invitation to an account
    // that has one never sets it.
    assert_eq!(
        Browser::new(&server).login("bob@example.test", PASSWORD),
        200
    );

    assert!(server.healthy());
}
