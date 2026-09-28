//! Signing in with GitHub — to an account this server already has.
//!
//! Accounts here are made by an invitation or by an operator, and GitHub
//! is not a way around that. So the first thing this suite proves is the
//! refusal the whole design rests on: a GitHub identity that matches
//! nobody here signs nobody in and **leaves no account behind**, asked
//! of the control plane itself rather than of an HTTP answer that could
//! be uniform for the wrong reason. The second is what the feature is
//! for: somebody who already has an account, arriving with GitHub's
//! proof of the address they sign in with, lands on it — and is linked by
//! GitHub's numeric id, so the address stops mattering after the first
//! trip.
//!
//! The rest is about a URL a stranger can construct. The callback is
//! reachable by anyone, so most of what there is to test is the ways it
//! can be forged: without the browser's own state cookie, with somebody
//! else's, with a code GitHub refuses, and with nothing at all.

use stratum_testkit::browser::{Browser, PASSWORD};
use stratum_testkit::gitcli::Scratch;
use stratum_testkit::{fake_github, Minio, Server};

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

/// How many accounts the server holds, asked of the control plane.
///
/// Not over HTTP: every refusal here is a redirect with one word in it,
/// and "no account was made" is a claim about a table, which only the
/// table can answer. A count rather than a lookup by address, so an
/// account made under an address nobody thought to ask about is still
/// caught.
fn accounts(server: &Server) -> i64 {
    let mut db =
        postgres::Client::connect(&server.db_url, postgres::NoTls).expect("the control plane");
    db.query_one("SELECT count(*) FROM users", &[])
        .expect("count users")
        .get(0)
}

/// The account a sign-in address belongs to, if any.
fn account_for(server: &Server, email: &str) -> Option<stratum_control::users::User> {
    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    stratum_control::users::by_email(&db, email).expect("read users")
}

/// An account the way this server makes one — an operator's
/// `user-create`, into `org` — and its id.
fn operator_made(server: &Server, org: &str, email: &str) -> String {
    server.bootstrap_org(org);
    server.admin_json(&[
        "admin",
        "user-create",
        "--org",
        org,
        "--email",
        email,
        "--password",
        PASSWORD,
        "--role",
        "owner",
    ])["user"]["id"]
        .as_str()
        .expect("an id")
        .to_string()
}

/// A browser holding the session the callback issued.
fn as_person<'a>(server: &'a Server, session: &str) -> Browser<'a> {
    let mut b = Browser::new(server);
    b.cookie = Some(format!("stratum_session={session}"));
    b
}

/// The account a session is signed in to.
fn me(server: &Server, session: &str) -> serde_json::Value {
    let (st, me) = as_person(server, session).req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    me
}

/// The headline. GitHub proving who somebody is does not make them
/// somebody here.
///
/// The identity is as good as one gets — a primary address GitHub itself
/// has proved — and it is still refused, because nobody on this server
/// signs in with that address. Refused with the word the dashboard turns
/// into "ask for an invitation", with no session, and with nothing
/// written: the table is counted before and after.
#[test]
fn an_identity_that_matches_nobody_here_makes_no_account() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignin-nobody");
    let scratch = Scratch::new("ghsignin-nobody");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignin_nobody", &gh.base_url);
    // Somebody is here, so "no account" is not simply an empty table.
    operator_made(&server, "acme", "someone@acme.test");
    let before = accounts(&server);
    assert_eq!(before, 1);

    for _ in 0..2 {
        let out = server.github_signin("code_as_501_ada");
        assert_eq!(out.status, 303, "{}", out.location);
        assert_eq!(out.outcome, "noaccount", "{}", out.location);
        assert!(
            out.session.is_none(),
            "signed somebody in: {}",
            out.location
        );
        // The second trip is refused exactly like the first: nothing was
        // linked or remembered on the way out of the first.
    }
    assert_eq!(
        accounts(&server),
        before,
        "a GitHub sign-in made an account"
    );
    assert!(account_for(&server, "ada@example.com").is_none());

    // The probe is not a helper that cannot say yes: the same identity,
    // once there is an account for its address, is signed in.
    operator_made(&server, "adaco", "ada@example.com");
    let out = server.github_signin("code_as_501_ada");
    assert_eq!(out.outcome, "ok", "{}", out.location);
    assert!(out.session.is_some());

    assert!(server.healthy(), "still serving");
}

/// Somebody who already has an account here, arriving through GitHub
/// with the address they sign in with, proved, lands on that account.
#[test]
fn a_proved_address_links_the_account_that_already_holds_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignin-link");
    let scratch = Scratch::new("ghsignin-link");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignin_link", &gh.base_url);
    let id = operator_made(&server, "acme", "ada@example.com");
    let before = accounts(&server);

    let out = server.github_signin("code_as_501_ada");
    assert_eq!(out.outcome, "ok", "{}", out.location);
    let me = me(&server, out.session.as_ref().unwrap());
    assert_eq!(me["id"], id.as_str(), "{me}");
    // Their own account, with the organization they were in.
    assert!(
        me["orgs"]
            .as_array()
            .is_some_and(|a| a.iter().any(|o| o["name"] == "acme")),
        "{me}"
    );
    assert_eq!(accounts(&server), before, "{me}");

    // A second door, not a replacement: the password still works.
    assert_eq!(
        Browser::new(&server).login("ada@example.com", PASSWORD),
        200,
        "linking GitHub cost the account its password"
    );

    assert!(server.healthy(), "still serving");
}

/// After the first trip the address no longer matters: the link is
/// GitHub's numeric id.
///
/// A login is renameable on GitHub, and so is a primary address, and
/// neither changes who somebody is. The fake's address follows the
/// login, so the second trip arrives with an address nobody here signs
/// in with — and lands on the same account, because the link is what
/// resolves it. The third trip is the other side of the same claim:
/// that address under a *different* id is nobody.
#[test]
fn a_changed_github_address_still_reaches_the_linked_account() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignin-rename");
    let scratch = Scratch::new("ghsignin-rename");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignin_rename", &gh.base_url);
    let id = operator_made(&server, "acme", "ada@example.com");

    let first = server.github_signin("code_as_501_ada");
    assert_eq!(first.outcome, "ok", "{}", first.location);
    let handle = me(&server, first.session.as_ref().unwrap())["handle"].clone();

    let second = server.github_signin("code_as_501_ada-lovelace");
    assert_eq!(second.outcome, "ok", "{}", second.location);
    let again = me(&server, second.session.as_ref().unwrap());
    assert_eq!(again["id"], id.as_str(), "a changed address lost the link");
    // Nothing about the account followed GitHub: not its address, and
    // not its handle, which is a URL other people hold.
    assert_eq!(again["email"], "ada@example.com", "{again}");
    assert_eq!(again["handle"], handle, "{again}");

    // The address alone gets nobody in; the id is what did.
    let stranger = server.github_signin("code_as_502_ada-lovelace");
    assert_eq!(stranger.outcome, "noaccount", "{}", stranger.location);
    assert!(stranger.session.is_none());

    assert!(server.healthy(), "still serving");
}

/// The refusal the address path rests on. GitHub knowing an address is
/// not GitHub having *proved* it, and an account here is found by a
/// proved address or not at all.
///
/// Bob has an account, and the identity arriving names his address as
/// its primary — unproved. If that were enough, anybody who typed Bob's
/// address into a GitHub profile would be signed in as Bob.
#[test]
fn an_unproved_primary_reaches_no_account() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignin-unproved");
    let scratch = Scratch::new("ghsignin-unproved");
    let gh = fake_github::spawn();
    let server = spawn(
        &bucket.base_url,
        &scratch,
        "ghsignin_unproved",
        &gh.base_url,
    );
    operator_made(&server, "acme", "bob@example.com");
    let before = accounts(&server);

    let out = server.github_signin("code_unverified_502_bob");
    assert_eq!(out.outcome, "noemail", "{}", out.location);
    assert!(out.session.is_none(), "signed somebody in anyway");
    // Nor was the identity linked on the way past: a link is consulted
    // before the address, so a second trip would sign in if one had been
    // made.
    let out = server.github_signin("code_unverified_502_bob");
    assert_eq!(out.outcome, "noemail", "{}", out.location);
    assert!(out.session.is_none(), "the first trip linked Bob's account");

    // An App that may not read addresses at all: the 403 GitHub answers
    // when the `Email addresses` permission was never granted. Same
    // answer, because the same thing is missing.
    let out = server.github_signin("code_noemail_503_carol");
    assert_eq!(out.outcome, "noemail", "{}", out.location);
    assert!(out.session.is_none(), "signed somebody in anyway");

    assert_eq!(accounts(&server), before);
    assert!(server.healthy(), "still serving");
}

/// An address somebody else merely *claimed* is not an account's.
///
/// Bob adds Carol's address to his own account and never proves it —
/// he cannot. Carol then arrives with GitHub's proof of it. Only an
/// account's sign-in address is matched, so she is nobody here — and,
/// the point of the test, she is not Bob.
#[test]
fn an_address_claimed_on_somebody_elses_account_does_not_reach_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignin-secondary");
    let scratch = Scratch::new("ghsignin-secondary");
    let gh = fake_github::spawn();
    let server = spawn(
        &bucket.base_url,
        &scratch,
        "ghsignin_secondary",
        &gh.base_url,
    );
    server.bootstrap_org("acme");
    let mut bob = Browser::person(&server, "acme", "owner", "bob", "bob@example.com");
    let (st, body) = bob.req(
        "POST",
        "/v1/users/bob/emails",
        Some(serde_json::json!({ "email": "carol@example.com" })),
    );
    assert_eq!(st, 202, "{body}");
    let before = accounts(&server);

    let out = server.github_signin("code_as_503_carol");
    assert_eq!(out.outcome, "noaccount", "{}", out.location);
    assert!(out.session.is_none(), "signed her in to Bob's account");
    assert_eq!(accounts(&server), before, "an account was made anyway");

    assert!(server.healthy(), "still serving");
}

/// Every way the callback can be arrived at without having started the
/// flow in this browser.
///
/// Ada has an account and the code names her, so each of these would be
/// a sign-in if the state check let it through — the last trip, started
/// properly, shows it.
#[test]
fn a_callback_without_the_browsers_own_state_signs_nobody_in() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignin-forged");
    let scratch = Scratch::new("ghsignin-forged");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignin_forged", &gh.base_url);
    operator_made(&server, "acme", "ada@example.com");

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

    // The same code, through the front door, is a sign-in.
    let out = server.github_signin("code_as_501_ada");
    assert_eq!(out.outcome, "ok", "{}", out.location);
    assert!(out.session.is_some());

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
    let bucket = minio.bucket("ghsignin-badcode");
    let scratch = Scratch::new("ghsignin-badcode");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignin_badcode", &gh.base_url);

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
    let bucket = minio.bucket("ghsignin-silent");
    let scratch = Scratch::new("ghsignin-silent");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignin_silent", &gh.base_url);

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
/// through to the account lookup and been answered as if it were one.
#[test]
fn a_primary_address_that_is_not_an_address_proves_nothing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignin-badmail");
    let scratch = Scratch::new("ghsignin-badmail");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignin_badmail", &gh.base_url);

    let before = accounts(&server);
    let out = server.github_signin("code_badmail_505_dave");
    assert_eq!(out.outcome, "noemail", "{}", out.location);
    assert!(out.session.is_none());
    assert_eq!(accounts(&server), before, "an account was made anyway");

    assert!(server.healthy(), "still serving");
}

/// Pressing Cancel on GitHub's authorization screen is a choice, and it
/// is reported as one.
#[test]
fn declining_at_github_is_not_an_error() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignin-denied");
    let scratch = Scratch::new("ghsignin-denied");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignin_denied", &gh.base_url);

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
    let bucket = minio.bucket("ghsignin-disabled");
    let scratch = Scratch::new("ghsignin-disabled");
    let gh = fake_github::spawn();
    let server = spawn(
        &bucket.base_url,
        &scratch,
        "ghsignin_disabled",
        &gh.base_url,
    );

    // Both ways GitHub reaches an account: by the address, before any
    // link exists (Bob), and by the link a first sign-in made (Ada).
    operator_made(&server, "acme", "ada@example.com");
    operator_made(&server, "bobco", "bob@example.com");
    let first = server.github_signin("code_as_501_ada");
    assert_eq!(first.outcome, "ok", "{}", first.location);
    for email in ["ada@example.com", "bob@example.com"] {
        server
            .admin(&["admin", "user-disable", "--email", email])
            .expect("user-disable");
    }

    for code in ["code_as_501_ada", "code_as_502_bob"] {
        let out = server.github_signin(code);
        assert_eq!(out.outcome, "disabled", "{code}: {}", out.location);
        assert!(out.session.is_none(), "a disabled account got a session");
    }

    assert!(server.healthy(), "still serving");
}

/// A deployment whose App has no OAuth client cannot do this at all, and
/// says so rather than sending somebody to a URL that cannot work.
#[test]
fn a_server_with_no_oauth_client_says_so_instead_of_leaving() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ghsignin-unconfigured");
    let scratch = Scratch::new("ghsignin-unconfigured");
    let gh = fake_github::spawn();
    let server = builder(
        &bucket.base_url,
        &scratch,
        "ghsignin_unconfigured",
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
    let bucket = minio.bucket("ghsignin-start");
    let scratch = Scratch::new("ghsignin-start");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "ghsignin_start", &gh.base_url);

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
