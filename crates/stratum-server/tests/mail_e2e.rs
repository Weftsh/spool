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

/// Percent-decode a mailed link's fragment: templates encode the token,
/// so an address inside one arrives as `%40`.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).expect("percent escape"));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).expect("utf8")
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

    // Every forged shape — and a live link minted for something else,
    // which is the one that matters: confirming an extra address is the
    // other link this server mails, and it must not reset a password.
    let (st, out) = fresh.req(
        "POST",
        "/v1/users/owner/emails",
        Some(serde_json::json!({ "email": "owner@home.test" })),
    );
    assert_eq!(st, 202, "{out}");
    let address_token = mailbox
        .wait_for("owner@home.test", SOON)
        .link()
        .and_then(|l| l.split("#verify-email=").nth(1).map(percent_decode))
        .expect("an address-confirmation link");
    for bad in [
        String::new(),
        "nonsense".into(),
        "weftrs_".into(),
        format!("weftrs_01zzzzzzzzzzzzzzzzzzzzzzzz_{}", "x".repeat(52)),
        address_token.clone(),
        format!("weftrs_{address_token}"),
    ] {
        let (st, body) = anon.req(
            "POST",
            "/v1/auth/reset-password",
            Some(serde_json::json!({ "token": bad, "new_password": NEW })),
        );
        assert_eq!(st, 404, "{bad:?} was accepted: {body}");
    }
    assert_eq!(Browser::new(&server).login("owner@acme.test", NEW), 200);
    // …and the address link still works afterwards, so a failed attack
    // does not cost its owner anything.
    let (st, out) = fresh.req(
        "POST",
        "/v1/users/owner/emails/verify",
        Some(serde_json::json!({ "token": address_token })),
    );
    assert_eq!(st, 200, "{out}");
    assert!(server.healthy());
}

/// Asking for a reset link must not become a way to ask who has an
/// account here, nor a way to flood somebody's inbox.
///
/// These were pinned on sign-up, which is gone; the endpoint that takes
/// a bare address and mails it is this one now, and every property
/// below is still its own.
#[test]
fn asking_for_a_reset_answers_alike_for_every_address_and_floods_nobody() {
    let minio = Minio::shared();
    let bucket = minio.bucket("mail-forgot");
    let scratch = Scratch::new("mail-forgot");
    let mailbox = Mailbox::temp("forgot-e2e");
    let server = spawn(&bucket.base_url, &scratch, "mail-forgot", Some(&mailbox));
    org_with_owner(&server, "acme", "owner@acme.test");
    org_with_owner(&server, "beta", "off@beta.test");
    server
        .admin(&["admin", "user-disable", "--email", "off@beta.test"])
        .unwrap_or_else(|e| panic!("user-disable: {e}"));

    let mut anon = Browser::new(&server);
    let mut ask = |email: &str| {
        anon.req(
            "POST",
            "/v1/auth/forgot-password",
            Some(serde_json::json!({ "email": email })),
        )
    };

    // An account, no account, a disabled account and not an address at
    // all: the same status and the same body, byte for byte.
    let known = ask("owner@acme.test");
    assert_eq!(known.0, 202, "{}", known.1);
    for other in ["nobody@example.test", "off@beta.test", "not-an-address"] {
        let (st, body) = ask(other);
        assert_eq!(
            (st, body.to_string()),
            (known.0, known.1.to_string()),
            "{other} is told apart from an address with an account"
        );
    }
    // What differs is only the mailbox. The disabled account is not
    // offered a way back in: recovering an account an operator switched
    // off would undo the switching off.
    mailbox.wait_for("owner@acme.test", SOON);
    assert!(
        mailbox.to("off@beta.test").is_empty(),
        "a disabled account was mailed a reset link: {:?}",
        mailbox.to("off@beta.test")
    );
    assert!(mailbox.to("nobody@example.test").is_empty());

    // One address cannot be used to flood a mailbox: three a window, and
    // the fourth answers exactly like a success, because "slow down" to
    // one address and not another is itself an oracle.
    for i in 0..3 {
        let (st, body) = ask("OWNER@acme.test");
        assert_eq!(
            (st, body.to_string()),
            (known.0, known.1.to_string()),
            "attempt {i}"
        );
    }
    let sent = mailbox.to("owner@acme.test");
    assert_eq!(
        sent.len(),
        3,
        "the per-address limit let a fourth message through: {sent:?}"
    );
    assert!(
        sent.iter().all(|m| m.subject == "Reset your Weft password"),
        "{sent:?}"
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
    org_with_owner(&server, "acme", "genuine@acme.test");

    // Walk a list of addresses that have no account. Each one is a fresh
    // address, so the per-address limit never fires; each costs a lookup
    // and nothing else, which is exactly what a relay abuser would do.
    let mut anon = Browser::new(&server);
    let mut first = None;
    for i in 0..60 {
        let (st, out) = anon.req(
            "POST",
            "/v1/auth/forgot-password",
            Some(serde_json::json!({ "email": format!("walk-{i}@example.test") })),
        );
        assert_eq!(st, 202, "attempt {i}: {out}");
        first.get_or_insert(out);
    }
    assert!(mailbox.all().is_empty(), "unknown addresses were mailed");

    // Past the global limit, a genuine account's request is refused —
    // silently, and with the same body as a success, because "you
    // personally are fine but the server is busy" is still a difference
    // somebody can read.
    let (st, out) = anon.req(
        "POST",
        "/v1/auth/forgot-password",
        Some(serde_json::json!({ "email": "genuine@acme.test" })),
    );
    assert_eq!(st, 202, "{out}");
    assert_eq!(Some(&out), first.as_ref(), "the limit answered differently");
    assert!(
        mailbox.all().is_empty(),
        "the global limit let a message through: {:?}",
        mailbox.all()
    );
    // The limit is on mail, not on the account: its password still works.
    assert_eq!(
        Browser::new(&server).login("genuine@acme.test", PASSWORD),
        200
    );
    assert!(server.healthy(), "still serving after 61 attempts");
}
