//! Mail, end to end against a real server.
//!
//! The property under test is not "a mail was sent" — it is that **the
//! link in the message a person actually receives works**. A test that
//! asserts on the API's returned link and never opens the captured
//! message proves the API, not the mail: a template with a wrong base
//! URL, a token mangled on its way into a URL, or a message sent to the
//! wrong address would all pass it.
//!
//! So every assertion here reads the mail out of the capture directory
//! and uses what it finds.

use std::time::Duration;
use stratum_testkit::browser::Browser;
use stratum_testkit::mailbox::Mailbox;
use stratum_testkit::{gitcli::Scratch, Minio, Server};

const PASSWORD: &str = "a long enough password";
const SOON: Duration = Duration::from_secs(5);

fn spawn(store_url: &str, scratch: &Scratch, hint: &str, mail: Option<&Mailbox>) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        // A link in a message has to be absolute, and has to point at
        // this server — which is the thing a request header must not be
        // allowed to decide.
        .env("STRATUM_PUBLIC_URL", "http://stratum.test:9999");
    if let Some(m) = mail {
        for (k, v) in m.env() {
            b = b.env(k, v);
        }
    }
    b.start()
}

fn org_with_owner(server: &Server, org: &str, email: &str) {
    server.bootstrap_org(org);
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            org,
            "--email",
            email,
            "--name",
            "Owner",
            "--password",
            PASSWORD,
            "--role",
            "owner",
        ])
        .unwrap_or_else(|e| panic!("user-create: {e}"));
}

/// The invitation a person receives is the invitation that works.
#[test]
fn an_invitation_arrives_by_mail_and_the_emailed_link_is_the_one_that_works() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-invite");
    let scratch = Scratch::new("mail-invite");
    let mailbox = Mailbox::temp("invite-e2e");
    let server = spawn(&bucket.base_url, &scratch, "mail-e2e", Some(&mailbox));
    org_with_owner(&server, "acme", "owner@acme.test");
    let mut owner = Browser::signed_in(&server, "owner@acme.test", PASSWORD);

    let (st, inv) = owner.req(
        "POST",
        "/v1/orgs/acme/invites",
        Some(serde_json::json!({"email": "New@Acme.test", "role": "member"})),
    );
    assert_eq!(st, 201, "{inv}");
    assert_eq!(inv["mail"]["sent"], true, "{inv}");
    assert!(inv["mail"].get("error").is_none(), "{inv}");

    // Addressed to the normalized address, and says what it is for.
    let mail = mailbox.wait_for("new@acme.test", SOON);
    assert_eq!(mail.subject, "You've been invited to acme on Weft");
    assert!(mail.text.contains("as member"), "{}", mail.text);

    // The link is absolute, points at the configured public URL, and
    // carries the token in the fragment.
    let link = mail
        .link()
        .unwrap_or_else(|| panic!("no link in {}", mail.text));
    assert!(
        link.starts_with("http://stratum.test:9999/dashboard/#invite="),
        "{link}"
    );
    let token = link.split("#invite=").nth(1).unwrap().to_string();

    // …and it is *the* token: accepting with it creates the account.
    let mut newbie = Browser::new(&server);
    let (st, body) = newbie.req(
        "POST",
        "/v1/auth/accept-invite",
        Some(serde_json::json!({
            "invite": token, "name": "New Person", "password": PASSWORD
        })),
    );
    assert_eq!(st, 201, "{body}");
    assert_eq!(body["email"], "new@acme.test");
    let mut joined = Browser::signed_in(&server, "new@acme.test", PASSWORD);
    assert_eq!(joined.req("GET", "/v1/orgs/acme/repos", None).0, 200);

    // Exactly one message: an invitation is not a broadcast.
    assert_eq!(mailbox.all().len(), 1, "{:?}", mailbox.all());
}

/// A transport that is down must not stop an admin onboarding somebody.
///
/// The invitation is still created and the link still returned — the
/// admin can carry it by hand, as they did before mail existed — and the
/// response says plainly that nothing was delivered rather than leaving
/// them to wonder.
#[test]
fn a_broken_transport_reports_the_failure_without_blocking_the_invitation() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-broken");
    let scratch = Scratch::new("mail-broken");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("mail-broken")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_MAIL_TRANSPORT", "smtp")
        .env("STRATUM_MAIL_FROM", "no-reply@stratum.test")
        // Port 0 is never connectable, so every send fails at connect.
        .env("STRATUM_MAIL_SMTP_HOST", "127.0.0.1:0")
        .start();
    org_with_owner(&server, "acme", "owner@acme.test");
    let mut owner = Browser::signed_in(&server, "owner@acme.test", PASSWORD);

    let (st, inv) = owner.req(
        "POST",
        "/v1/orgs/acme/invites",
        Some(serde_json::json!({"email": "new@acme.test", "role": "member"})),
    );
    assert_eq!(st, 201, "{inv}");
    assert_eq!(inv["mail"]["sent"], false, "{inv}");
    assert!(
        inv["mail"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("smtp"),
        "{inv}"
    );

    // The link still works — that is the point of returning it.
    let link = inv["invite_link"].as_str().unwrap().to_string();
    let (st, body) = Browser::new(&server).req(
        "POST",
        "/v1/auth/accept-invite",
        Some(serde_json::json!({
            "invite": link, "name": "New Person", "password": PASSWORD
        })),
    );
    assert_eq!(st, 201, "{body}");
    assert!(server.healthy(), "server still serving after a failed send");
}

/// With no transport configured the response says so, so an operator
/// reading it knows the link is theirs to deliver.
#[test]
fn an_unconfigured_server_says_nothing_was_sent() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-null");
    let scratch = Scratch::new("mail-null");
    let server = spawn(&bucket.base_url, &scratch, "mail-null", None);
    org_with_owner(&server, "acme", "owner@acme.test");
    let mut owner = Browser::signed_in(&server, "owner@acme.test", PASSWORD);

    let (st, inv) = owner.req(
        "POST",
        "/v1/orgs/acme/invites",
        Some(serde_json::json!({"email": "new@acme.test", "role": "member"})),
    );
    assert_eq!(st, 201, "{inv}");
    assert_eq!(inv["mail"]["sent"], false, "{inv}");
    assert!(inv["mail"].get("error").is_none(), "not an error: {inv}");
    assert!(inv["invite_link"].as_str().is_some(), "{inv}");
}

/// A misconfigured transport is a boot failure, not a running server
/// that quietly drops every message.
#[test]
fn a_misconfigured_transport_refuses_to_boot() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-misconfigured");
    for (env, needle) in [
        (
            vec![("STRATUM_MAIL_TRANSPORT", "carrier-pigeon")],
            "null, capture, smtp, ses",
        ),
        (
            vec![("STRATUM_MAIL_TRANSPORT", "smtp")],
            "STRATUM_MAIL_FROM",
        ),
        (
            vec![
                ("STRATUM_MAIL_TRANSPORT", "smtp"),
                ("STRATUM_MAIL_FROM", "no-reply@stratum.test"),
            ],
            "STRATUM_MAIL_SMTP_HOST",
        ),
        (
            vec![("STRATUM_MAIL_TRANSPORT", "capture")],
            "STRATUM_MAIL_DIR",
        ),
    ] {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("AWS_ACCESS_KEY_ID", "minioadmin")
            .env("AWS_SECRET_ACCESS_KEY", "minioadmin")
            .env("AWS_REGION", "us-east-1")
            .env("STRATUM_STORE_URL", &bucket.base_url)
            .env(
                "STRATUM_DB_URL",
                stratum_testkit::pg::test_db_url("mail-boot"),
            )
            .env("STRATUM_BIND", "127.0.0.1:0");
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run server");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{env:?} booted: {err}");
        assert!(err.contains(needle), "{env:?}: {err}");
    }
}

/// What the emailed link says it is for, before anybody commits to it.
///
/// The screen it lands on asks a stranger to choose a password. It has to
/// be able to say *what they are joining* — and, when the link is spent,
/// to say that instead of taking a password and then refusing.
///
/// The second half is the security property: the token is the credential,
/// so its holder learns nothing new, but every other shape of input must
/// answer identically or the endpoint becomes a way to ask "does this
/// invitation exist?".
#[test]
fn an_invitation_previews_for_its_holder_and_looks_identical_for_everyone_else() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-preview");
    let scratch = Scratch::new("mail-preview");
    let mailbox = Mailbox::temp("preview-e2e");
    let server = spawn(&bucket.base_url, &scratch, "mail-preview", Some(&mailbox));
    org_with_owner(&server, "acme", "owner@acme.test");
    let mut owner = Browser::signed_in(&server, "owner@acme.test", PASSWORD);

    let (st, inv) = owner.req(
        "POST",
        "/v1/orgs/acme/invites",
        Some(serde_json::json!({"email": "preview@acme.test", "role": "admin"})),
    );
    assert_eq!(st, 201, "{inv}");
    let mail = mailbox.wait_for("preview@acme.test", SOON);
    let link = mail.link().expect("a link in the message");
    let token = link.split("#invite=").nth(1).unwrap().to_string();

    // The token out of the *message* previews — not the one out of the
    // API response, which is the thing the recipient never sees.
    let mut anon = Browser::new(&server);
    let (st, body) = anon.req(
        "POST",
        "/v1/auth/invite/preview",
        Some(serde_json::json!({ "invite": token })),
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["org"], "acme");
    assert_eq!(body["role"], "admin");
    assert_eq!(body["email"], "preview@acme.test");
    assert!(body["expires_at"].as_i64().unwrap() > 0, "{body}");
    // It is a question, not an action: previewing does not spend the
    // link, so a person can open the mail twice.
    assert_eq!(
        anon.req(
            "POST",
            "/v1/auth/invite/preview",
            Some(serde_json::json!({ "invite": token })),
        )
        .0,
        200
    );

    // Every dead shape answers identically — same status, same body.
    let good_id = token
        .strip_prefix("stinv_")
        .and_then(|r| r.split_once('_'))
        .map(|(id, _)| id.to_string())
        .expect("a stinv_<id>_<secret> token");
    let mut refusals = Vec::new();
    for bad in [
        "".to_string(),
        "nonsense".to_string(),
        "stinv_".to_string(),
        "stinv_onlyid".to_string(),
        // A real invitation id with the wrong secret: the id alone is
        // not the credential.
        format!("stinv_{good_id}_{}", "x".repeat(52)),
        // A well-formed token for an invitation that does not exist.
        format!("stinv_01zzzzzzzzzzzzzzzzzzzzzzzz_{}", "x".repeat(52)),
        // The genuine token with one character changed.
        format!("{token}x"),
    ] {
        let (st, body) = anon.req(
            "POST",
            "/v1/auth/invite/preview",
            Some(serde_json::json!({ "invite": bad })),
        );
        assert_eq!(st, 404, "{bad:?} answered {st}: {body}");
        refusals.push(body.to_string());
    }
    assert_eq!(
        refusals
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        1,
        "refusals differ from each other, which distinguishes them: {refusals:?}"
    );

    // Accepting spends it, and the preview then joins the dead shapes —
    // so the screen can say "already used" rather than taking a password
    // first and refusing afterwards.
    let mut newbie = Browser::new(&server);
    assert_eq!(
        newbie
            .req(
                "POST",
                "/v1/auth/accept-invite",
                Some(serde_json::json!({
                    "invite": token, "name": "Preview Person", "password": PASSWORD
                })),
            )
            .0,
        201
    );
    let (st, body) = anon.req(
        "POST",
        "/v1/auth/invite/preview",
        Some(serde_json::json!({ "invite": token })),
    );
    assert_eq!(st, 404, "a spent link still previewed: {body}");
    assert_eq!(
        body.to_string(),
        refusals[0],
        "spent looks different: {body}"
    );

    // Expiry is the one dead shape the API cannot produce — every
    // invitation it mints is live for a week — so it is pinned where the
    // single gate lives, in `invites::verify`'s own tests. Both paths run
    // through that one function, which is why there is one place to pin.
    assert!(server.healthy(), "server still serving after all that");
}

/// The whole funnel a stranger runs, and the wall an unproved address
/// hits halfway through it.
#[test]
fn a_stranger_signs_up_is_blocked_until_they_confirm_and_then_is_not() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-signup");
    let scratch = Scratch::new("mail-signup");
    let mailbox = Mailbox::temp("signup-e2e");
    let server = spawn(&bucket.base_url, &scratch, "mail-signup", Some(&mailbox));

    let mut anon = Browser::new(&server);
    let (st, body) = anon.req(
        "POST",
        "/v1/auth/signup",
        Some(serde_json::json!({
            "email": "Stranger@Example.test", "name": "A Stranger",
            "password": PASSWORD, "handle": "stranger",
        })),
    );
    assert_eq!(st, 202, "{body}");
    // The answer says nothing about whether an account was made.
    assert!(
        !body.to_string().contains("stranger@example.test"),
        "the response echoes the address: {body}"
    );

    // The confirmation message arrives at the normalized address.
    let mail = mailbox.wait_for("stranger@example.test", SOON);
    assert_eq!(mail.subject, "Confirm your email address");
    let link = mail.link().expect("a link");
    assert!(
        link.starts_with("http://stratum.test:9999/dashboard/#verify="),
        "{link}"
    );
    let token = link.split("#verify=").nth(1).unwrap().to_string();

    // They can sign in and look around straight away…
    let mut person = Browser::signed_in(&server, "stranger@example.test", PASSWORD);
    let (st, me) = person.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    assert_eq!(
        me["orgs"][0]["name"], "stranger",
        "personal namespace: {me}"
    );
    assert_eq!(me["orgs"][0]["role"], "owner");
    assert_eq!(person.req("GET", "/v1/orgs/stranger/repos", None).0, 200);

    // …but creating anything is refused until the address is proved,
    // and the refusal says what to do about it.
    let (st, refusal) = person.req(
        "POST",
        "/v1/orgs/stranger/repos",
        Some(serde_json::json!({ "name": "first" })),
    );
    assert_eq!(st, 403, "{refusal}");
    assert!(
        refusal["error"]
            .as_str()
            .unwrap_or_default()
            .contains("confirm your email address"),
        "{refusal}"
    );
    // The batch path is the same wall — a gate on one door only is no gate.
    let (st, batch) = person.req(
        "POST",
        "/v1/orgs/stranger/repos/batch/create",
        Some(serde_json::json!({ "repos": [{ "name": "bulk" }] })),
    );
    assert_eq!(st, 403, "{batch}");
    // …and so is mirroring, which is the expensive one: it makes the
    // server fetch a stranger's URL on their say-so.
    let (st, mirror) = person.req(
        "POST",
        "/v1/orgs/stranger/mirrors",
        Some(serde_json::json!({
            "name": "m", "provider": "generic", "origin": "https://x.test/a.git"
        })),
    );
    assert_eq!(st, 403, "{mirror}");

    // Confirming signs them in and opens the door.
    let mut fresh = Browser::new(&server);
    let (st, who) = fresh.req(
        "POST",
        "/v1/auth/verify",
        Some(serde_json::json!({ "token": token })),
    );
    assert_eq!(st, 200, "{who}");
    assert_eq!(who["email"], "stranger@example.test");
    let (st, repo) = fresh.req(
        "POST",
        "/v1/orgs/stranger/repos",
        Some(serde_json::json!({ "name": "first" })),
    );
    assert_eq!(st, 201, "{repo}");
    assert_eq!(repo["name"], "first", "{repo}");

    // The link is spent.
    assert_eq!(
        Browser::new(&server)
            .req(
                "POST",
                "/v1/auth/verify",
                Some(serde_json::json!({ "token": token })),
            )
            .0,
        404
    );
    assert!(server.healthy());
}

/// Signup must not become a way to ask who has an account here.
#[test]
fn signup_answers_identically_whether_or_not_the_address_is_known() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-oracle");
    let scratch = Scratch::new("mail-oracle");
    let mailbox = Mailbox::temp("oracle-e2e");
    let server = spawn(&bucket.base_url, &scratch, "mail-oracle", Some(&mailbox));
    org_with_owner(&server, "acme", "owner@acme.test");

    let mut anon = Browser::new(&server);
    let signup = |b: &mut Browser, email: &str, handle: &str| {
        b.req(
            "POST",
            "/v1/auth/signup",
            Some(serde_json::json!({
                "email": email, "name": "Someone",
                "password": PASSWORD, "handle": handle,
            })),
        )
    };

    // A brand-new address and one that already has an account: same
    // status, same body, byte for byte.
    let (new_st, new_body) = signup(&mut anon, "brand-new@example.test", "brandnew");
    let (old_st, old_body) = signup(&mut anon, "owner@acme.test", "takenaddress");
    assert_eq!(new_st, 202);
    assert_eq!(old_st, 202);
    assert_eq!(
        new_body.to_string(),
        old_body.to_string(),
        "responses differ"
    );

    // What differs is only what lands in the mailbox — and the message
    // to the address that already exists goes to its owner, telling them
    // nothing was created.
    let existing = mailbox.wait_for("owner@acme.test", SOON);
    assert_eq!(existing.subject, "You already have a Weft account");
    assert!(
        existing.text.contains("Nothing was created"),
        "{}",
        existing.text
    );
    // No namespace was made for the handle that lost.
    assert_eq!(
        Browser::new(&server)
            .req("GET", "/v1/orgs/takenaddress/repos", None)
            .0,
        404
    );

    // The same uniformity on the two endpoints that take a bare address.
    for path in ["/v1/auth/resend-verification", "/v1/auth/forgot-password"] {
        let known = anon.req(
            "POST",
            path,
            Some(serde_json::json!({ "email": "owner@acme.test" })),
        );
        let unknown = anon.req(
            "POST",
            path,
            Some(serde_json::json!({ "email": "nobody@example.test" })),
        );
        let malformed = anon.req(
            "POST",
            path,
            Some(serde_json::json!({ "email": "not-an-address" })),
        );
        assert_eq!(known.0, 202, "{path}");
        assert_eq!(known.1.to_string(), unknown.1.to_string(), "{path}");
        assert_eq!(known.1.to_string(), malformed.1.to_string(), "{path}");
    }
    assert!(server.healthy());
}

/// Resetting a password, and every way of doing it that must not work.
#[test]
fn a_reset_link_restores_access_once_and_ends_every_other_session() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-reset");
    let scratch = Scratch::new("mail-reset");
    let mailbox = Mailbox::temp("reset-e2e");
    let server = spawn(&bucket.base_url, &scratch, "mail-reset", Some(&mailbox));
    org_with_owner(&server, "acme", "owner@acme.test");

    // Two sessions for the same person, both live.
    let mut laptop = Browser::signed_in(&server, "owner@acme.test", PASSWORD);
    let mut desktop = Browser::signed_in(&server, "owner@acme.test", PASSWORD);
    assert_eq!(laptop.req("GET", "/v1/auth/me", None).0, 200);
    assert_eq!(desktop.req("GET", "/v1/auth/me", None).0, 200);

    let mut anon = Browser::new(&server);
    assert_eq!(
        anon.req(
            "POST",
            "/v1/auth/forgot-password",
            Some(serde_json::json!({ "email": "owner@acme.test" })),
        )
        .0,
        202
    );
    let mail = mailbox.wait_for("owner@acme.test", SOON);
    assert_eq!(mail.subject, "Reset your Weft password");
    let token = mail
        .link()
        .and_then(|l| l.split("#reset=").nth(1).map(str::to_string))
        .expect("a reset token");

    // A password that fails the strength rule does not spend the link —
    // a typo must cost a retry, not another trip through the inbox.
    let mut fresh = Browser::new(&server);
    let (st, weak) = fresh.req(
        "POST",
        "/v1/auth/reset-password",
        Some(serde_json::json!({ "token": token, "new_password": "short" })),
    );
    assert_eq!(st, 400, "{weak}");

    const NEW: &str = "an even longer password";
    let (st, who) = fresh.req(
        "POST",
        "/v1/auth/reset-password",
        Some(serde_json::json!({ "token": token, "new_password": NEW })),
    );
    assert_eq!(st, 200, "{who}");
    assert_eq!(who["email"], "owner@acme.test");

    // Both older sessions are gone — whoever asked for this may have
    // done so because somebody else was signed in.
    assert_eq!(laptop.req("GET", "/v1/auth/me", None).0, 401);
    assert_eq!(desktop.req("GET", "/v1/auth/me", None).0, 401);
    // …and the one the reset created works.
    assert_eq!(fresh.req("GET", "/v1/auth/me", None).0, 200);

    // The old password is dead, the new one works, and the link is spent.
    assert_eq!(
        Browser::new(&server).login("owner@acme.test", PASSWORD),
        401
    );
    assert_eq!(Browser::new(&server).login("owner@acme.test", NEW), 200);
    assert_eq!(
        Browser::new(&server)
            .req(
                "POST",
                "/v1/auth/reset-password",
                Some(serde_json::json!({ "token": token, "new_password": NEW })),
            )
            .0,
        404
    );

    // Every forged shape, and a verification token presented here.
    assert_eq!(
        anon.req(
            "POST",
            "/v1/auth/signup",
            Some(serde_json::json!({
                "email": "other@acme.test", "name": "Other",
                "password": PASSWORD, "handle": "other",
            })),
        )
        .0,
        202
    );
    let verify_token = mailbox
        .wait_for("other@acme.test", SOON)
        .link()
        .and_then(|l| l.split("#verify=").nth(1).map(str::to_string))
        .unwrap();
    for bad in [
        String::new(),
        "nonsense".into(),
        "weftrs_".into(),
        format!("weftrs_01zzzzzzzzzzzzzzzzzzzzzzzz_{}", "x".repeat(52)),
        // A live verification link is not a password reset.
        verify_token.clone(),
        verify_token.replace("weftv_", "weftrs_"),
    ] {
        let (st, body) = anon.req(
            "POST",
            "/v1/auth/reset-password",
            Some(serde_json::json!({ "token": bad, "new_password": NEW })),
        );
        assert_eq!(st, 404, "{bad:?} was accepted: {body}");
    }
    // …and the verification link still works afterwards, so a failed
    // attack does not cost its owner anything.
    assert_eq!(
        Browser::new(&server)
            .req(
                "POST",
                "/v1/auth/verify",
                Some(serde_json::json!({ "token": verify_token })),
            )
            .0,
        200
    );
    assert!(server.healthy());
}

/// Handles are public names, so their refusals are plain — and they are
/// checked before anything about the address is, so a taken handle
/// cannot be used to probe addresses either.
#[test]
fn a_handle_must_be_free_valid_and_not_one_the_platform_keeps() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-handle");
    let scratch = Scratch::new("mail-handle");
    let mailbox = Mailbox::temp("handle-e2e");
    let server = spawn(&bucket.base_url, &scratch, "mail-handle", Some(&mailbox));
    org_with_owner(&server, "acme", "owner@acme.test");

    let mut anon = Browser::new(&server);
    let try_handle = |b: &mut Browser, handle: &str, email: &str| {
        b.req(
            "POST",
            "/v1/auth/signup",
            Some(serde_json::json!({
                "email": email, "name": "Someone",
                "password": PASSWORD, "handle": handle,
            })),
        )
    };

    // A router path segment can never be a namespace: an org called
    // `dashboard` could never be cloned, because the SPA answers first.
    for reserved in [
        "dashboard",
        "v1",
        "healthz",
        "admin",
        "support",
        "DASHBOARD",
    ] {
        let (st, body) = try_handle(&mut anon, reserved, "x@example.test");
        assert_eq!(st, 400, "{reserved} was allowed: {body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("reserved"),
            "{reserved}: {body}"
        );
    }
    // An existing org's name is taken, in any case.
    for taken in ["acme", "ACME", "Acme"] {
        let (st, body) = try_handle(&mut anon, taken, "y@example.test");
        assert_eq!(st, 409, "{taken}: {body}");
    }
    // Shapes that are not names at all.
    for bad in ["", "  ", "a b", "a/b", "..", &"n".repeat(300)] {
        assert_eq!(
            try_handle(&mut anon, bad, "z@example.test").0,
            400,
            "{bad:?}"
        );
    }
    // Nothing above sent any mail: none of them got as far as an address.
    assert!(mailbox.all().is_empty(), "{:?}", mailbox.all());

    // A free one works, and is then taken for everybody else.
    assert_eq!(try_handle(&mut anon, "newcomer", "new@example.test").0, 202);
    mailbox.wait_for("new@example.test", SOON);
    let (st, body) = try_handle(&mut anon, "NewComer", "second@example.test");
    assert_eq!(st, 409, "case-folded collision: {body}");
    assert!(server.healthy());
}

/// The refusals on the way in, and the two states an account can be in
/// that make a mailed link useless when it arrives.
#[test]
fn signup_refuses_bad_input_rate_limits_by_address_and_respects_a_disabled_account() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-limits");
    let scratch = Scratch::new("mail-limits");
    let mailbox = Mailbox::temp("limits-e2e");
    let server = spawn(&bucket.base_url, &scratch, "mail-limits", Some(&mailbox));

    let mut anon = Browser::new(&server);
    let signup = |b: &mut Browser, v: serde_json::Value| b.req("POST", "/v1/auth/signup", Some(v));

    // Each field is checked, and each says which one it was.
    for (body, needle) in [
        (
            serde_json::json!({"email":"a@example.test","name":"A","password":"short","handle":"h1"}),
            "password",
        ),
        (
            serde_json::json!({"email":"not-an-address","name":"A","password":PASSWORD,"handle":"h2"}),
            "email",
        ),
        (
            serde_json::json!({"email":"b@example.test","name":"   ","password":PASSWORD,"handle":"h3"}),
            "name",
        ),
        (
            serde_json::json!({"email":"c@example.test","name":"n".repeat(201),"password":PASSWORD,"handle":"h4"}),
            "name",
        ),
    ] {
        let (st, out) = signup(&mut anon, body);
        assert_eq!(st, 400, "{out}");
        assert!(
            out["error"].as_str().unwrap_or_default().contains(needle),
            "expected {needle:?}: {out}"
        );
    }
    // None of those got as far as sending anything.
    assert!(mailbox.all().is_empty(), "{:?}", mailbox.all());

    // One address cannot be used to flood a mailbox. The limit answers
    // exactly like a success, because saying "slow down" to one address
    // and not another is itself an oracle.
    let flood = "flood@example.test";
    for i in 0..3 {
        let (st, out) = signup(
            &mut anon,
            serde_json::json!({
                "email": flood, "name": "Flood", "password": PASSWORD,
                "handle": format!("flood{i}"),
            }),
        );
        assert_eq!(st, 202, "attempt {i}: {out}");
    }
    let (st, limited) = signup(
        &mut anon,
        serde_json::json!({
            "email": flood, "name": "Flood", "password": PASSWORD, "handle": "flood3",
        }),
    );
    assert_eq!(st, 202, "{limited}");
    // Only the first attempt created anything; the rest were refused
    // silently, so the mailbox holds one confirmation and two "you
    // already have an account" notes, not four of anything.
    let to_flood = mailbox.to(flood);
    assert_eq!(to_flood.len(), 3, "{to_flood:?}");
    assert_eq!(to_flood[0].subject, "Confirm your email address");
    assert_eq!(to_flood[1].subject, "You already have a Weft account");
    // The fourth was rate-limited before any message was composed.
    assert_eq!(
        mailbox.to(flood).len(),
        3,
        "the rate-limited attempt still sent mail"
    );
    // …and the handles from the refused attempts were never claimed.
    for handle in ["flood1", "flood2", "flood3"] {
        assert_eq!(
            Browser::new(&server)
                .req("GET", &format!("/v1/orgs/{handle}/repos"), None)
                .0,
            404,
            "{handle} was created"
        );
    }

    // A confirmation link for an account an operator has since switched
    // off marks the address proved and stops there: the row becomes
    // accurate, and they still cannot get in.
    let token = mailbox.to(flood)[0]
        .link()
        .and_then(|l| l.split("#verify=").nth(1).map(str::to_string))
        .unwrap();
    server
        .admin(&["admin", "user-disable", "--email", flood])
        .unwrap_or_else(|e| panic!("user-disable: {e}"));
    let (st, out) = Browser::new(&server).req(
        "POST",
        "/v1/auth/verify",
        Some(serde_json::json!({ "token": token })),
    );
    assert_eq!(st, 401, "{out}");
    assert!(server.healthy());
}

/// Asking for another confirmation link, and what a disabled account is
/// and is not offered.
#[test]
fn a_second_confirmation_link_replaces_the_first_and_a_disabled_account_gets_neither() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-resend");
    let scratch = Scratch::new("mail-resend");
    let mailbox = Mailbox::temp("resend-e2e");
    let server = spawn(&bucket.base_url, &scratch, "mail-resend", Some(&mailbox));

    let mut anon = Browser::new(&server);
    assert_eq!(
        anon.req(
            "POST",
            "/v1/auth/signup",
            Some(serde_json::json!({
                "email": "again@example.test", "name": "Again",
                "password": PASSWORD, "handle": "again",
            })),
        )
        .0,
        202
    );
    let first = mailbox
        .wait_for("again@example.test", SOON)
        .link()
        .and_then(|l| l.split("#verify=").nth(1).map(str::to_string))
        .unwrap();

    // "Send it again" does send again — and kills the first link. Anyone
    // who asks for a new one has told you they no longer trust the old.
    assert_eq!(
        anon.req(
            "POST",
            "/v1/auth/resend-verification",
            Some(serde_json::json!({ "email": "AGAIN@example.test" })),
        )
        .0,
        202
    );
    let sent = mailbox.to("again@example.test");
    assert_eq!(sent.len(), 2, "{sent:?}");
    let second = sent[1]
        .link()
        .and_then(|l| l.split("#verify=").nth(1).map(str::to_string))
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(
        Browser::new(&server)
            .req(
                "POST",
                "/v1/auth/verify",
                Some(serde_json::json!({ "token": first })),
            )
            .0,
        404,
        "the superseded link still worked"
    );
    assert_eq!(
        Browser::new(&server)
            .req(
                "POST",
                "/v1/auth/verify",
                Some(serde_json::json!({ "token": second })),
            )
            .0,
        200
    );

    // Already confirmed: nothing more to send, and the caller cannot
    // tell that from an address with no account at all.
    assert_eq!(
        anon.req(
            "POST",
            "/v1/auth/resend-verification",
            Some(serde_json::json!({ "email": "again@example.test" })),
        )
        .0,
        202
    );
    assert_eq!(mailbox.to("again@example.test").len(), 2, "sent a third");

    // A disabled account is offered neither a confirmation link nor a
    // reset link: recovering an account an operator switched off would
    // undo the switching off.
    server
        .admin(&["admin", "user-disable", "--email", "again@example.test"])
        .unwrap_or_else(|e| panic!("user-disable: {e}"));
    for path in ["/v1/auth/resend-verification", "/v1/auth/forgot-password"] {
        assert_eq!(
            anon.req(
                "POST",
                path,
                Some(serde_json::json!({ "email": "again@example.test" })),
            )
            .0,
            202,
            "{path}"
        );
    }
    assert_eq!(
        mailbox.to("again@example.test").len(),
        2,
        "a disabled account was mailed a link: {:?}",
        mailbox.to("again@example.test")
    );
    assert!(server.healthy());
}

/// An account switched off between asking for a reset and using the
/// link. The link is spent either way — it must not stay live — but it
/// does not let anybody in.
#[test]
fn a_reset_link_stops_working_if_the_account_is_switched_off_first() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-reset-off");
    let scratch = Scratch::new("mail-reset-off");
    let mailbox = Mailbox::temp("reset-off-e2e");
    let server = spawn(&bucket.base_url, &scratch, "mail-reset-off", Some(&mailbox));
    org_with_owner(&server, "acme", "owner@acme.test");

    let mut anon = Browser::new(&server);
    assert_eq!(
        anon.req(
            "POST",
            "/v1/auth/forgot-password",
            Some(serde_json::json!({ "email": "owner@acme.test" })),
        )
        .0,
        202
    );
    let token = mailbox
        .wait_for("owner@acme.test", SOON)
        .link()
        .and_then(|l| l.split("#reset=").nth(1).map(str::to_string))
        .unwrap();

    server
        .admin(&["admin", "user-disable", "--email", "owner@acme.test"])
        .unwrap_or_else(|e| panic!("user-disable: {e}"));

    const NEW: &str = "an even longer password";
    let (st, out) = Browser::new(&server).req(
        "POST",
        "/v1/auth/reset-password",
        Some(serde_json::json!({ "token": token, "new_password": NEW })),
    );
    assert_eq!(st, 401, "{out}");
    // No session was handed out, and the account is still shut.
    assert_eq!(Browser::new(&server).login("owner@acme.test", NEW), 401);
    assert!(server.healthy());
}

/// A mail transport that is down must not take signup with it. The
/// account is made, the link exists, and the failure is the operator's
/// problem to see in the log — not the caller's to be told about, since
/// telling them would say whether an address is registered.
#[test]
fn a_broken_transport_does_not_break_signup() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-signup-broken");
    let scratch = Scratch::new("mail-signup-broken");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("mail-signup-broken")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_PUBLIC_URL", "http://stratum.test:9999")
        .env("STRATUM_MAIL_TRANSPORT", "smtp")
        .env("STRATUM_MAIL_FROM", "no-reply@stratum.test")
        .env("STRATUM_MAIL_SMTP_HOST", "127.0.0.1:0")
        .start();

    let mut anon = Browser::new(&server);
    let (st, out) = anon.req(
        "POST",
        "/v1/auth/signup",
        Some(serde_json::json!({
            "email": "quiet@example.test", "name": "Quiet",
            "password": PASSWORD, "handle": "quiet",
        })),
    );
    assert_eq!(st, 202, "{out}");
    // The account and its namespace exist; only the message did not go.
    let mut person = Browser::signed_in(&server, "quiet@example.test", PASSWORD);
    assert_eq!(person.req("GET", "/v1/orgs/quiet/repos", None).0, 200);
    // Still unconfirmed, so still walled off from creating.
    assert_eq!(
        person
            .req(
                "POST",
                "/v1/orgs/quiet/repos",
                Some(serde_json::json!({ "name": "nope" })),
            )
            .0,
        403
    );
    assert!(server.healthy());
}

/// The other half of the rate limit: a script walking an address list.
///
/// The per-address limit does nothing about that — every address is
/// fresh — so there is a global one, and it is what stops this server
/// being used as somebody else's spam relay.
#[test]
fn a_global_limit_stops_the_server_being_used_as_a_relay() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-global");
    let scratch = Scratch::new("mail-global");
    let mailbox = Mailbox::temp("global-e2e");
    let server = spawn(&bucket.base_url, &scratch, "mail-global", Some(&mailbox));

    // Walk a list of addresses that have no account. Each one is a fresh
    // address, so the per-address limit never fires; each costs a lookup
    // and nothing else, which is exactly what a relay abuser would do.
    let mut anon = Browser::new(&server);
    for i in 0..60 {
        let (st, out) = anon.req(
            "POST",
            "/v1/auth/resend-verification",
            Some(serde_json::json!({ "email": format!("walk-{i}@example.test") })),
        );
        assert_eq!(st, 202, "attempt {i}: {out}");
    }
    assert!(mailbox.all().is_empty(), "unknown addresses were mailed");

    // Past the global limit, a genuine signup is refused — silently, and
    // with the same body as a success, because "you personally are fine
    // but the server is busy" is still a difference somebody can read.
    let (st, out) = anon.req(
        "POST",
        "/v1/auth/signup",
        Some(serde_json::json!({
            "email": "genuine@example.test", "name": "Genuine",
            "password": PASSWORD, "handle": "genuine",
        })),
    );
    assert_eq!(st, 202, "{out}");
    assert!(mailbox.all().is_empty(), "{:?}", mailbox.all());
    // Nothing was created either: the limit is before the account, not
    // after it, so a refused signup leaves no half-made namespace.
    assert_eq!(
        Browser::new(&server)
            .req("GET", "/v1/orgs/genuine/repos", None)
            .0,
        404
    );
    assert_eq!(
        Browser::new(&server).login("genuine@example.test", PASSWORD),
        401
    );
    assert!(server.healthy(), "still serving after 61 attempts");
}
