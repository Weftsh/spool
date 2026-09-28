//! Signing up and signing in with GitHub.
//!
//! The point of the feature is that an account made this way skips our
//! confirmation mail, so the first thing this suite proves is the thing
//! that makes that safe — an address GitHub has *not* proved makes no
//! account at all — and the second is the thing that makes it worth
//! doing: an account made this way can create something immediately,
//! which is the gate `require_verified` gets right everywhere else only
//! because a link was clicked.
//!
//! The rest is about a URL a stranger can construct. The callback is
//! reachable by anyone, so most of what there is to test is the ways it
//! can be forged: without the browser's own state cookie, with somebody
//! else's, with a code GitHub refuses, and with nothing at all.

use stratum_testkit::browser::Browser;
use stratum_testkit::gitcli::Scratch;
use stratum_testkit::{fake_github, Minio, Server};

const PASSWORD: &str = "a long enough password";

fn builder(
    store_url: &str,
    scratch: &Scratch,
    hint: &str,
    api_base: &str,
) -> stratum_testkit::server::ServerBuilder {
    let key_path = scratch.path().join("app-key.pem");
    std::fs::write(&key_path, fake_github::TEST_APP_KEY_PEM).unwrap();
    let origins = scratch.path().join("origins");
    std::fs::create_dir_all(&origins).unwrap();
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_GITHUB_APP_ID", "12345")
        .env("STRATUM_GITHUB_APP_KEY_PEM", key_path.display().to_string())
        .env("STRATUM_GITHUB_API_BASE", api_base)
        .env(
            "STRATUM_GITHUB_GIT_BASE",
            format!("file://{}", origins.display()),
        )
}

/// A server whose App has an OAuth client, which is what signing in
/// through GitHub needs at all.
fn spawn(store_url: &str, scratch: &Scratch, hint: &str, api_base: &str) -> Server {
    builder(store_url, scratch, hint, api_base)
        .env("STRATUM_GITHUB_CLIENT_ID", "Iv1.test")
        .env("STRATUM_GITHUB_CLIENT_SECRET", "test-client-secret")
        .env("STRATUM_GITHUB_OAUTH_BASE", api_base)
        .start()
}

/// A browser holding the session the callback issued.
fn as_person<'a>(server: &'a Server, session: &str) -> Browser<'a> {
    let mut b = Browser::new(server);
    b.cookie = Some(format!("stratum_session={session}"));
    b
}

/// The headline. An account made through GitHub is proved on arrival,
/// and proved means it can create — which is the whole difference
/// between this and the password path's "check your email".
#[test]
fn signing_up_with_github_makes_a_proved_account_that_can_create_at_once() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-ok");
    let scratch = Scratch::new("ghsignup-ok");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignup_ok", &gh.base_url);

    let out = server.github_signin("code_as_501_ada");
    assert_eq!(out.status, 303, "{}", out.location);
    // `new`, not `ok`: the dashboard has something left to ask them.
    assert_eq!(out.outcome, "new", "{}", out.location);
    let session = out.session.expect("a session cookie");

    let mut person = as_person(&server, &session);
    let (st, me) = person.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    assert_eq!(me["email"], "ada@example.com");
    // GitHub's display name, not the login, when it has one.
    assert_eq!(me["name"], "Person ada");
    // Proved without anybody clicking anything.
    assert!(!me["verified_at"].is_null(), "{me}");
    // The login became the namespace, which is the name in every URL
    // from here on.
    assert_eq!(me["handle"], "ada");
    assert_eq!(me["orgs"][0]["name"], "ada");
    assert_eq!(me["orgs"][0]["role"], "owner");

    // And the part that would be a lie if `verified_at` were decorative.
    let (st, repo) = person.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "notebook" })),
    );
    assert_eq!(st, 201, "a proved account could not create: {repo}");

    assert!(server.healthy(), "still serving");
}

/// The refusal the whole feature rests on. GitHub knowing an address is
/// not GitHub having *proved* it, and only the second one may skip our
/// own confirmation.
#[test]
fn an_address_github_has_not_proved_makes_no_account() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-unproved");
    let scratch = Scratch::new("ghsignup-unproved");
    let gh = fake_github::spawn();
    let server = spawn(
        &bucket.base_url,
        &scratch,
        "ghsignup_unproved",
        &gh.base_url,
    );

    // A primary address with `verified: false` — somebody who typed it
    // into a profile and never clicked the link.
    let out = server.github_signin("code_unverified_502_bob");
    assert_eq!(out.outcome, "noemail", "{}", out.location);
    assert!(out.session.is_none(), "signed somebody in anyway");

    // An App that may not read addresses at all: the 403 GitHub answers
    // when the `Email addresses` permission was never granted. Same
    // answer, because the same thing is missing.
    let out = server.github_signin("code_noemail_503_carol");
    assert_eq!(out.outcome, "noemail", "{}", out.location);
    assert!(out.session.is_none(), "signed somebody in anyway");

    // Neither one left an account behind. `bob` would own the namespace
    // `bob` if it had, so asking for it is asking whether it exists.
    for handle in ["bob", "carol"] {
        let (st, out) = server.req("GET", &format!("/v1/orgs/{handle}/repos"), "", None);
        assert_eq!(st, 404, "{handle} exists: {out}");
    }

    assert!(server.healthy(), "still serving");
}

/// Coming back is not signing up again.
#[test]
fn returning_through_github_lands_on_the_same_account() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-return");
    let scratch = Scratch::new("ghsignup-return");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignup_return", &gh.base_url);

    let first = server.github_signin("code_as_501_ada");
    assert_eq!(first.outcome, "new");
    let id_first = as_person(&server, &first.session.unwrap())
        .req("GET", "/v1/auth/me", None)
        .1["id"]
        .as_str()
        .unwrap()
        .to_string();

    let second = server.github_signin("code_as_501_ada");
    // `ok`, not `new`: nothing to onboard, and a second namespace was
    // not claimed.
    assert_eq!(second.outcome, "ok", "{}", second.location);
    let me = as_person(&server, second.session.as_ref().unwrap())
        .req("GET", "/v1/auth/me", None)
        .1;
    assert_eq!(me["id"], id_first.as_str());
    assert_eq!(me["orgs"].as_array().unwrap().len(), 1, "{me}");

    assert!(server.healthy(), "still serving");
}

/// The reason the identity is keyed on the numeric id. A login is
/// renameable on GitHub, and the account behind it does not change.
#[test]
fn a_renamed_github_login_still_reaches_the_same_account() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-rename");
    let scratch = Scratch::new("ghsignup-rename");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignup_rename", &gh.base_url);

    let first = server.github_signin("code_as_501_ada");
    assert_eq!(first.outcome, "new");
    let me = as_person(&server, &first.session.unwrap())
        .req("GET", "/v1/auth/me", None)
        .1;
    let id_first = me["id"].as_str().unwrap().to_string();

    // Same person, same GitHub account, new login — and so a different
    // primary address too, since the fake's follows the login. Neither
    // is what resolves them: the numeric id is.
    let second = server.github_signin("code_as_501_ada-lovelace");
    assert_eq!(second.outcome, "ok", "{}", second.location);
    let me = as_person(&server, second.session.as_ref().unwrap())
        .req("GET", "/v1/auth/me", None)
        .1;
    assert_eq!(me["id"], id_first.as_str(), "a rename made a new account");
    // The namespace does not follow the rename: it is a URL other people
    // have, and silently moving it would break every one of them.
    assert_eq!(me["handle"], "ada");

    assert!(server.healthy(), "still serving");
}

/// Somebody who already has a password account here, arriving through
/// GitHub with the same proved address, lands on the account they
/// already have rather than being told the address is taken.
#[test]
fn a_proved_address_links_the_account_that_already_holds_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-link");
    let scratch = Scratch::new("ghsignup-link");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignup_link", &gh.base_url);
    server.bootstrap_org("acme");
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "ada@example.com",
            "--password",
            PASSWORD,
            "--role",
            "owner",
        ])
        .expect("user-create");

    let mut pw = Browser::signed_in(&server, "ada@example.com", PASSWORD);
    let id_before = pw.req("GET", "/v1/auth/me", None).1["id"]
        .as_str()
        .unwrap()
        .to_string();

    let out = server.github_signin("code_as_501_ada");
    // Not `new`, and not `emailtaken`: it is their account.
    assert_eq!(out.outcome, "ok", "{}", out.location);
    let me = as_person(&server, out.session.as_ref().unwrap())
        .req("GET", "/v1/auth/me", None)
        .1;
    assert_eq!(me["id"], id_before.as_str());
    // They keep the org they were in.
    assert_eq!(me["orgs"][0]["name"], "acme");

    // The password still works: this account had already proved the
    // address, so there was nothing to defend against.
    assert_eq!(
        Browser::new(&server).login("ada@example.com", PASSWORD),
        200,
        "an already-proved account lost its password"
    );

    assert!(server.healthy(), "still serving");
}

/// Account pre-hijacking, refused.
///
/// Anyone can sign up with an address they do not own and never confirm
/// it. That account can do nothing — the confirmation gate sees to that
/// — but it is sitting on the address with a password its maker knows.
/// When the real owner of the mailbox proves it through GitHub, handing
/// them that account as-is would hand them one the first person can
/// still open.
#[test]
fn adopting_an_unconfirmed_account_kills_the_password_waiting_on_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-hijack");
    let scratch = Scratch::new("ghsignup-hijack");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignup_hijack", &gh.base_url);

    // Mallory claims Ada's address and never confirms it — she cannot,
    // the link goes to Ada.
    let (st, body) = Browser::new(&server).req(
        "POST",
        "/v1/auth/signup",
        Some(serde_json::json!({
            "email": "ada@example.com", "name": "Not Ada",
            "password": PASSWORD, "handle": "nota",
        })),
    );
    assert_eq!(st, 202, "{body}");
    // The waiting credential works, for an account that can do nothing.
    let mut mallory = Browser::new(&server);
    assert_eq!(mallory.login("ada@example.com", PASSWORD), 200);
    let session_before = mallory.cookie.clone().expect("mallory holds a session");

    // Ada arrives, and GitHub proves the address is hers.
    let out = server.github_signin("code_as_501_ada");
    assert_eq!(out.outcome, "ok", "{}", out.location);
    assert!(out.session.is_some(), "Ada was not signed in");

    // Mallory's password is now worthless…
    assert_eq!(
        Browser::new(&server).login("ada@example.com", PASSWORD),
        401,
        "the waiting password still opens the account"
    );
    // …and so is the session she was already holding.
    let mut held = Browser::new(&server);
    held.cookie = Some(session_before);
    let (st, body) = held.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 401, "the waiting session survived: {body}");

    // Ada has the account, proved, and can use it.
    let mut ada = as_person(&server, &out_session(&server, "code_as_501_ada"));
    let (st, me) = ada.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    assert!(!me["verified_at"].is_null(), "{me}");
    assert_eq!(me["orgs"][0]["name"], "nota", "{me}");

    assert!(server.healthy(), "still serving");
}

/// Sign in again and hand back just the session, for a test that needs a
/// second trip.
fn out_session(server: &Server, code: &str) -> String {
    let out = server.github_signin(code);
    out.session
        .unwrap_or_else(|| panic!("no session: {}", out.location))
}

/// A namespace somebody else already holds does not stop a sign-up.
#[test]
fn a_taken_namespace_gets_a_suffix_rather_than_a_refusal() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-taken");
    let scratch = Scratch::new("ghsignup-taken");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignup_taken", &gh.base_url);
    // An org named `ada` already exists here, and it is not hers.
    server.bootstrap_org("ada");

    let out = server.github_signin("code_as_501_ada");
    assert_eq!(out.outcome, "new", "{}", out.location);
    let me = as_person(&server, &out.session.unwrap())
        .req("GET", "/v1/auth/me", None)
        .1;
    let handle = me["handle"].as_str().expect("a handle");
    assert!(handle.starts_with("ada-"), "handle is {handle:?}");
    assert_ne!(handle, "ada");
    // Half-made accounts are the failure this is avoiding: a handle and
    // a namespace, or nothing.
    assert_eq!(me["orgs"][0]["name"], handle);

    assert!(server.healthy(), "still serving");
}

/// A reserved name is the same story with a different cause.
#[test]
fn a_reserved_namespace_gets_a_suffix_too() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-reserved");
    let scratch = Scratch::new("ghsignup-reserved");
    let gh = fake_github::spawn();
    let server = spawn(
        &bucket.base_url,
        &scratch,
        "ghsignup_reserved",
        &gh.base_url,
    );

    // `dashboard` is the sharp one: a namespace by that name would have
    // the SPA answer every git request for it, forever.
    let out = server.github_signin("code_as_504_dashboard");
    assert_eq!(out.outcome, "new", "{}", out.location);
    let me = as_person(&server, &out.session.unwrap())
        .req("GET", "/v1/auth/me", None)
        .1;
    let handle = me["handle"].as_str().expect("a handle");
    assert!(handle.starts_with("dashboard-"), "handle is {handle:?}");

    assert!(server.healthy(), "still serving");
}

/// An address somebody else already holds — as a *secondary*, so
/// `users.email` says it is free and only `user_emails` knows better.
///
/// The 500 this would otherwise be is the point: the person is told the
/// one thing they can act on, and told it plainly, because GitHub has
/// just proved they own the mailbox. They are not probing for somebody
/// else's account; they are locked out of their own.
#[test]
fn an_address_held_as_somebody_elses_secondary_is_refused_plainly() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-secondary");
    let scratch = Scratch::new("ghsignup-secondary");
    let gh = fake_github::spawn();
    let server = spawn(
        &bucket.base_url,
        &scratch,
        "ghsignup_secondary",
        &gh.base_url,
    );

    // Bob signs up through GitHub, then claims Carol's address as a
    // second one on his account. He never proves it — he cannot — but
    // the row occupies the platform-wide key either way.
    let bob = server.github_signin("code_as_502_bob");
    assert_eq!(bob.outcome, "new", "{}", bob.location);
    let mut bob = as_person(&server, &bob.session.unwrap());
    let (st, body) = bob.req(
        "POST",
        "/v1/users/bob/emails",
        Some(serde_json::json!({ "email": "carol@example.com" })),
    );
    assert_eq!(st, 202, "{body}");

    // Carol arrives with GitHub's proof and is told what is wrong.
    let out = server.github_signin("code_as_503_carol");
    assert_eq!(out.outcome, "emailtaken", "{}", out.location);
    assert!(out.session.is_none(), "signed her in anyway");

    // Nothing half-made was left behind: no account, and no namespace
    // holding the name she would have got.
    let (st, body) = server.req("GET", "/v1/orgs/carol/repos", "", None);
    assert_eq!(st, 404, "a namespace was claimed anyway: {body}");

    assert!(server.healthy(), "still serving");
}

/// Every way the callback can be arrived at without having started the
/// flow in this browser.
#[test]
fn a_callback_without_the_browsers_own_state_signs_nobody_in() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-forged");
    let scratch = Scratch::new("ghsignup-forged");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignup_forged", &gh.base_url);

    // No cookie at all — the shape a link mailed to somebody arrives in.
    let out = server.github_callback("code=code_as_501_ada&state=whatever", None);
    assert_eq!(out.outcome, "expired", "{}", out.location);
    assert!(out.session.is_none());

    // A cookie that does not match the state in the URL: the browser
    // that started a flow is not the one this URL was built for.
    let out = server.github_callback("code=code_as_501_ada&state=whatever", Some("somethingelse"));
    assert_eq!(out.outcome, "expired", "{}", out.location);
    assert!(out.session.is_none());

    // A matching state but no code. Nothing to exchange, so nothing is
    // learned about anybody.
    let out = server.github_callback("state=matching", Some("matching"));
    assert_eq!(out.outcome, "expired", "{}", out.location);
    assert!(out.session.is_none());

    // An empty state on both sides must not be a match — it is what an
    // absent cookie and an absent parameter both look like.
    let out = server.github_callback("code=code_as_501_ada&state=", Some(""));
    assert_eq!(out.outcome, "expired", "{}", out.location);
    assert!(out.session.is_none());

    // Nothing whatsoever, which is what a bookmark looks like.
    let out = server.github_callback("", None);
    assert_eq!(out.outcome, "expired", "{}", out.location);
    assert!(out.session.is_none());

    // And no account was made by any of it.
    let (st, body) = server.req("GET", "/v1/orgs/ada/repos", "", None);
    assert_eq!(st, 404, "{body}");

    assert!(server.healthy(), "still serving");
}

/// A code GitHub refuses — spent, forged, or minted for another App.
///
/// GitHub answers that with a **200** carrying an `error` field, not a
/// status, which is the trap: a client checking only the status would
/// carry an empty token into the next call and read the refusal
/// somewhere it cannot explain it.
#[test]
fn a_code_github_refuses_is_not_a_sign_in() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-badcode");
    let scratch = Scratch::new("ghsignup-badcode");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignup_badcode", &gh.base_url);

    let out = server.github_signin("not-a-code");
    assert_eq!(out.outcome, "expired", "{}", out.location);
    assert!(out.session.is_none());

    assert!(server.healthy(), "still serving");
}

/// GitHub not answering at all, at each of the two calls that can meet
/// it.
///
/// Told apart from a *refusal* on purpose. A refused code is final for
/// this trip and reads as `expired`; an unanswered GitHub is nobody's
/// doing and clears by itself, so it reads as `error` — "try again in a
/// moment" — and nothing is created either way.
#[test]
fn a_github_that_does_not_answer_is_an_error_rather_than_a_refusal() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-silent");
    let scratch = Scratch::new("ghsignup-silent");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignup_silent", &gh.base_url);

    // Silent at the token exchange: the first call.
    let out = server.github_signin("code_silent");
    assert_eq!(out.outcome, "error", "{}", out.location);
    assert!(out.session.is_none());

    // Silent at the identity read: the exchange worked, and the call
    // after it did not. A different arm, so it needs its own trip.
    let out = server.github_signin("code_for_silent");
    assert_eq!(out.outcome, "error", "{}", out.location);
    assert!(out.session.is_none());

    assert!(server.healthy(), "still serving");
}

/// A provider that answers something we could not store.
///
/// `not-an-address` marked primary and verified is not a sign-in — and
/// the person hears the accurate sentence, not "something went wrong".
/// Without the check where the address is read this would have fallen
/// through to `users::create` and surfaced as a 500 nobody could act on.
#[test]
fn a_primary_address_that_is_not_an_address_proves_nothing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-badmail");
    let scratch = Scratch::new("ghsignup-badmail");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignup_badmail", &gh.base_url);

    let out = server.github_signin("code_badmail_505_dave");
    assert_eq!(out.outcome, "noemail", "{}", out.location);
    assert!(out.session.is_none());

    let (st, body) = server.req("GET", "/v1/orgs/dave/repos", "", None);
    assert_eq!(st, 404, "an account was made anyway: {body}");

    assert!(server.healthy(), "still serving");
}

/// Pressing Cancel on GitHub's authorization screen is a choice, and it
/// is reported as one.
#[test]
fn declining_at_github_is_not_an_error() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-denied");
    let scratch = Scratch::new("ghsignup-denied");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignup_denied", &gh.base_url);

    let out = server.github_callback(
        "error=access_denied&error_description=The+user+has+denied+your+application+access",
        None,
    );
    assert_eq!(out.outcome, "denied", "{}", out.location);
    assert!(out.session.is_none());

    assert!(server.healthy(), "still serving");
}

/// A disabled account is switched off on every door, not just the one
/// with a password on it.
#[test]
fn a_disabled_account_cannot_sign_in_through_github() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-disabled");
    let scratch = Scratch::new("ghsignup-disabled");
    let gh = fake_github::spawn();
    let server = spawn(
        &bucket.base_url,
        &scratch,
        "ghsignup_disabled",
        &gh.base_url,
    );

    // An account with the link already made, so the disabled check is
    // reached through the identity path rather than the create path.
    let first = server.github_signin("code_as_501_ada");
    assert_eq!(first.outcome, "new");
    server
        .admin(&["admin", "user-disable", "--email", "ada@example.com"])
        .expect("user-disable");

    let out = server.github_signin("code_as_501_ada");
    assert_eq!(out.outcome, "disabled", "{}", out.location);
    assert!(out.session.is_none(), "a disabled account got a session");

    assert!(server.healthy(), "still serving");
}

/// A deployment whose App has no OAuth client cannot do this at all, and
/// says so rather than sending somebody to a URL that cannot work.
#[test]
fn a_server_with_no_oauth_client_says_so_instead_of_leaving() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-unconfigured");
    let scratch = Scratch::new("ghsignup-unconfigured");
    let gh = fake_github::spawn();
    let server = builder(
        &bucket.base_url,
        &scratch,
        "ghsignup_unconfigured",
        &gh.base_url,
    )
    .start();

    let (st, loc) = server.follow_start();
    assert_eq!(st, 303, "{loc}");
    assert!(loc.contains("github=unavailable"), "{loc}");

    // And the callback refuses too, rather than half-running.
    let out = server.github_callback("code=code_as_501_ada&state=s", Some("s"));
    assert_eq!(out.outcome, "unavailable", "{}", out.location);
    assert!(out.session.is_none());

    assert!(server.healthy(), "still serving");
}

/// The start route parks a state and leaves for GitHub with it.
#[test]
fn starting_parks_a_state_and_leaves_for_github() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignup-start");
    let scratch = Scratch::new("ghsignup-start");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignup_start", &gh.base_url);

    let (st, loc) = server.follow_start();
    assert_eq!(st, 303, "{loc}");
    assert!(
        loc.starts_with(&format!("{}/login/oauth/authorize?", gh.base_url)),
        "{loc}"
    );
    assert!(loc.contains("client_id=Iv1.test"), "{loc}");
    // The callback the App has to be told about, spelled from the
    // deployment's own public URL so it cannot drift from the host the
    // person is on.
    assert!(
        loc.contains(&format!(
            "redirect_uri={}",
            server.base.replace(':', "%3A").replace('/', "%2F")
        )),
        "{loc}"
    );

    assert!(server.healthy(), "still serving");
}
