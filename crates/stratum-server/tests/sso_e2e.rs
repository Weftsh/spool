//! Single sign-on with the company's OpenID Connect provider.
//!
//! What a business turning SSO on is promised, each against the real
//! server and a provider it can only reach over HTTP:
//!
//! * anybody the provider signs in gets an account, made whole on the
//!   first sign-in, in the organization SSO people join — and the same
//!   account every time after, whatever their address becomes;
//! * the account the operator made before SSO is found by its address and
//!   keeps its role;
//! * SSO is the only way in by default: passwords, password resets and
//!   GitHub sign-in are off, an invitation link signs nobody in — and
//!   tokens, which CI runs on, still work;
//! * nothing the provider did not really say gets anybody in. Every check
//!   on the ID token has a token wrong in only that way; every forgery of
//!   the round trip that a stranger can make is refused; an address the
//!   provider did not vouch for, or outside the domains it speaks for,
//!   makes no account. Each attack ends with the server still serving.

use stratum_testkit::browser::{Browser, PASSWORD};
use stratum_testkit::gitcli::Scratch;
use stratum_testkit::oidc::{self as idp, Person, Tamper};
use stratum_testkit::{Minio, Server};

const CLIENT: &str = "spool-e2e";
const SECRET: &str = "e2e-secret: with odd%chars";

fn builder(
    store_url: &str,
    scratch: &Scratch,
    hint: &str,
    fake: &idp::FakeOidc,
) -> stratum_testkit::server::ServerBuilder {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_OIDC_ISSUER", fake.issuer.clone())
        .env("STRATUM_OIDC_CLIENT_ID", CLIENT)
        .env("STRATUM_OIDC_CLIENT_SECRET", SECRET)
        .env("STRATUM_OIDC_ORG", "acme")
        .env("STRATUM_OIDC_NAME", "Acme SSO")
}

fn spawn(store_url: &str, scratch: &Scratch, hint: &str, fake: &idp::FakeOidc) -> Server {
    let server = builder(store_url, scratch, hint, fake).start();
    server.bootstrap_org("acme");
    server
}

fn accounts(server: &Server) -> i64 {
    let mut db = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    db.query_one("SELECT count(*) FROM users", &[])
        .unwrap()
        .get(0)
}

fn as_person<'a>(server: &'a Server, session: &str) -> Browser<'a> {
    let mut b = Browser::new(server);
    b.cookie = Some(format!("stratum_session={session}"));
    b
}

fn me(server: &Server, session: &str) -> serde_json::Value {
    let (st, me) = as_person(server, session).req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    me
}

fn role_in(me: &serde_json::Value, org: &str) -> Option<String> {
    me["orgs"].as_array()?.iter().find_map(|o| {
        (o["name"] == org).then(|| o["role"].as_str().unwrap_or_default().to_string())
    })
}

/// The whole promise, once: a newcomer arrives and is somebody — a
/// member of the SSO organization, with a handle, a personal namespace
/// and a proved address — and arrives as the same somebody next time,
/// even with a different address.
#[test]
fn a_newcomer_is_made_a_member_and_is_the_same_person_next_time() {
    let minio = Minio::shared();
    let bucket = minio.bucket("sso-newcomer");
    let scratch = Scratch::new("sso-newcomer");
    let fake = idp::spawn(CLIENT, SECRET);
    let server = spawn(&bucket.base_url, &scratch, "sso-newcomer", &fake);

    let (st, methods) = Browser::new(&server).req("GET", "/v1/auth/methods", None);
    assert_eq!(st, 200);
    assert_eq!(
        methods,
        serde_json::json!({
            "password": false, "github": false,
            "sso": { "name": "Acme SSO", "start": "/v1/auth/sso/start" },
        })
    );

    let before = accounts(&server);
    fake.sign_in_as(Person {
        name: Some("Dana Scully".into()),
        ..Person::verified("sub-dana", "Dana.Scully@acme.test")
    });
    let first = idp::round_trip(&server.base);
    assert_eq!(first.outcome, "ok", "{first:?}");
    let session = first.session.clone().expect("a session");
    let who = me(&server, &session);
    assert_eq!(who["email"], "dana.scully@acme.test");
    assert_eq!(who["name"], "Dana Scully");
    assert_eq!(who["handle"], "dana-scully");
    assert_eq!(role_in(&who, "acme").as_deref(), Some("member"));
    assert_eq!(role_in(&who, "dana-scully").as_deref(), Some("owner"));
    assert_eq!(accounts(&server), before + 1);

    // The session is an SSO session: hours, not the fortnight a password
    // session gets, so somebody switched off at the provider is out by
    // the next working day.
    let mut db = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    let life: i64 = db
        .query_one(
            "SELECT expires_at - created_at FROM sessions WHERE user_id = $1",
            &[&who["id"].as_str().unwrap()],
        )
        .unwrap()
        .get(0);
    assert_eq!(life, 12 * 3600 * 1000);

    // The provider changed their address; the account is the same one.
    fake.sign_in_as(Person::verified("sub-dana", "dana@acme.test"));
    let again = idp::round_trip(&server.base);
    assert_eq!(again.outcome, "ok");
    assert_eq!(me(&server, &again.session.unwrap())["id"], who["id"]);
    assert_eq!(accounts(&server), before + 1);
    assert!(server.healthy());
}

/// The operator who bootstrapped the server gets in with SSO once it is
/// switched on — found by their address, linked, and still an owner.
#[test]
fn the_operator_made_owner_is_found_by_address_and_keeps_their_role() {
    let minio = Minio::shared();
    let bucket = minio.bucket("sso-owner");
    let scratch = Scratch::new("sso-owner");
    let fake = idp::spawn(CLIENT, SECRET);
    let server = spawn(&bucket.base_url, &scratch, "sso-owner", &fake);
    // Made before anybody signs in, with no password: on an SSO server
    // the operator has no secret to invent.
    server.admin_json(&[
        "admin",
        "user-create",
        "--org",
        "acme",
        "--email",
        "owner@acme.test",
        "--no-password",
        "--role",
        "owner",
    ]);
    let before = accounts(&server);
    fake.sign_in_as(Person::verified("sub-owner", "owner@acme.test"));
    let landed = idp::round_trip(&server.base);
    assert_eq!(landed.outcome, "ok", "{landed:?}");
    let who = me(&server, &landed.session.unwrap());
    assert_eq!(who["email"], "owner@acme.test");
    assert_eq!(role_in(&who, "acme").as_deref(), Some("owner"));
    assert_eq!(accounts(&server), before, "linked, not duplicated");
}

/// SSO is the only way in, and every other door says so — while the
/// credentials machines use keep working.
#[test]
fn sso_only_closes_every_other_door_and_leaves_tokens_working() {
    let minio = Minio::shared();
    let bucket = minio.bucket("sso-only");
    let scratch = Scratch::new("sso-only");
    let fake = idp::spawn(CLIENT, SECRET);
    let server = spawn(&bucket.base_url, &scratch, "sso-only", &fake);
    let admin = server.bootstrap_org("globex");
    server.admin_json(&[
        "admin",
        "user-create",
        "--org",
        "acme",
        "--email",
        "pw@acme.test",
        "--password",
        PASSWORD,
        "--role",
        "owner",
    ]);
    let mut anon = Browser::new(&server);
    for (path, body) in [
        (
            "/v1/auth/login",
            serde_json::json!({"email": "pw@acme.test", "password": PASSWORD}),
        ),
        (
            "/v1/auth/forgot-password",
            serde_json::json!({"email": "pw@acme.test"}),
        ),
        (
            "/v1/auth/reset-password",
            serde_json::json!({"token": "weftrs_x_y", "new_password": "a new long password"}),
        ),
        (
            "/v1/auth/password",
            serde_json::json!({"current_password": PASSWORD, "new_password": "a new long password"}),
        ),
    ] {
        let (st, out) = anon.req("POST", path, Some(body));
        assert_eq!(st, 403, "{path}: {out}");
        assert!(
            out["error"].as_str().unwrap().contains("Acme SSO"),
            "{path}: {out}"
        );
    }
    // GitHub sign-in is off too, even on a server with a GitHub App.
    let r = ureq::AgentBuilder::new()
        .redirects(0)
        .build()
        .get(&format!("{}/v1/auth/github/start", server.base))
        .call()
        .unwrap();
    assert!(
        r.header("location")
            .unwrap()
            .ends_with("?github=unavailable"),
        "{:?}",
        r.header("location")
    );

    // An invitation link signs nobody in: a newcomer is told to sign in
    // with SSO first.
    fake.sign_in_as(Person::verified("sub-owner", "pw@acme.test"));
    let owner = idp::round_trip(&server.base)
        .session
        .expect("owner session");
    let mut owner_b = as_person(&server, &owner);
    let (st, inv) = owner_b.req(
        "POST",
        "/v1/orgs/acme/invites",
        Some(serde_json::json!({"email": "later@acme.test", "role": "member"})),
    );
    assert_eq!(st, 201, "{inv}");
    let link = inv["invite_link"].as_str().unwrap().to_string();
    let (st, out) = anon.req(
        "POST",
        "/v1/auth/accept-invite",
        Some(serde_json::json!({"invite": link, "name": "Later", "password": PASSWORD})),
    );
    assert_eq!(st, 403, "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("later@acme.test"),
        "{out}"
    );
    // …signed in as somebody else, still no.
    let (st, _) = owner_b.req(
        "POST",
        "/v1/auth/accept-invite",
        Some(serde_json::json!({"invite": link})),
    );
    assert_eq!(st, 403);
    // …signed in with SSO as the invited person: accepted, and no new
    // session is minted around the provider.
    fake.sign_in_as(Person::verified("sub-later", "later@acme.test"));
    let later = idp::round_trip(&server.base)
        .session
        .expect("later session");
    let mut later_b = as_person(&server, &later);
    let (st, out) = later_b.req(
        "POST",
        "/v1/auth/accept-invite",
        Some(serde_json::json!({"invite": link})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(
        later_b.cookie.as_deref(),
        Some(format!("stratum_session={later}").as_str())
    );

    // Tokens are untouched: CI keeps running.
    let (st, out) = server.req("GET", "/v1/orgs/globex/repos", &admin, None);
    assert_eq!(st, 200, "{out}");
    assert!(server.healthy());
}

/// With SSO-only turned off, both ways in are offered and work.
#[test]
fn sso_alongside_passwords_when_the_operator_says_so() {
    let minio = Minio::shared();
    let bucket = minio.bucket("sso-both");
    let scratch = Scratch::new("sso-both");
    let fake = idp::spawn(CLIENT, SECRET);
    let server = builder(&bucket.base_url, &scratch, "sso-both", &fake)
        .env("STRATUM_SSO_ONLY", "false")
        .start();
    server.bootstrap_org("acme");
    let (_, methods) = Browser::new(&server).req("GET", "/v1/auth/methods", None);
    assert_eq!(methods["password"], true);
    assert_eq!(methods["sso"]["name"], "Acme SSO");
    server.admin_json(&[
        "admin",
        "user-create",
        "--org",
        "acme",
        "--email",
        "pw@acme.test",
        "--password",
        PASSWORD,
    ]);
    Browser::signed_in(&server, "pw@acme.test", PASSWORD);
    fake.sign_in_as(Person::verified("sub-x", "x@acme.test"));
    assert_eq!(idp::round_trip(&server.base).outcome, "ok");
}

/// Every check on the ID token, each with a token wrong in only that
/// way. None signs anybody in or makes an account.
#[test]
fn a_token_the_provider_did_not_really_issue_signs_nobody_in() {
    let minio = Minio::shared();
    let bucket = minio.bucket("sso-tokens");
    let scratch = Scratch::new("sso-tokens");
    let fake = idp::spawn(CLIENT, SECRET);
    let server = spawn(&bucket.base_url, &scratch, "sso-tokens", &fake);
    fake.sign_in_as(Person::verified("sub-mallory", "mallory@acme.test"));
    let before = accounts(&server);
    for (tamper, want) in [
        (Tamper::AlgNone, "error"),
        (Tamper::AlgHs256, "error"),
        (Tamper::UnknownKid, "error"),
        (Tamper::BadSignature, "error"),
        (Tamper::Issuer("https://evil.test".into()), "error"),
        (Tamper::Audience("another-app".into()), "error"),
        (Tamper::ExtraAudienceNoAzp, "error"),
        (Tamper::Expired, "error"),
        (Tamper::IssuedInTheFuture, "error"),
        (Tamper::Nonce("somebody-elses".into()), "expired"),
        (Tamper::NoSubject, "error"),
    ] {
        fake.set_tamper(tamper.clone());
        let landed = idp::round_trip(&server.base);
        assert_eq!(landed.outcome, want, "{tamper:?}: {landed:?}");
        assert!(landed.session.is_none(), "{tamper:?} signed somebody in");
        assert_eq!(accounts(&server), before, "{tamper:?} made an account");
        assert!(server.healthy(), "{tamper:?}");
    }
    // A forged `kid` makes the server fetch the key set again, but not
    // once per forgery: the refetch is rate-limited.
    let fetched = fake.jwks_calls.load(std::sync::atomic::Ordering::SeqCst);
    assert!(fetched <= 2, "the key set was fetched {fetched} times");
    // And the provider being honest again works.
    fake.set_tamper(Tamper::None);
    assert_eq!(idp::round_trip(&server.base).outcome, "ok");
}

/// Every way a stranger can forge the round trip's last leg.
#[test]
fn a_callback_that_is_not_this_browsers_round_trip_is_refused() {
    let minio = Minio::shared();
    let bucket = minio.bucket("sso-forged");
    let scratch = Scratch::new("sso-forged");
    let fake = idp::spawn(CLIENT, SECRET);
    let server = spawn(&bucket.base_url, &scratch, "sso-forged", &fake);
    fake.sign_in_as(Person::verified("sub-victim", "victim@acme.test"));
    let before = accounts(&server);

    // No cookie — somebody handed a victim's browser a callback URL.
    let s = idp::start(&server.base);
    let back = idp::visit_provider(&s.location);
    assert_eq!(idp::finish(&back, None).outcome, "expired");
    // The cookie of a different round trip.
    let other = idp::start(&server.base);
    let back = idp::visit_provider(&idp::start(&server.base).location);
    assert_eq!(
        idp::finish(&back, other.cookie.as_deref()).outcome,
        "expired"
    );
    // Empty state against an empty cookie never compares equal.
    let blank = format!("{}/v1/auth/sso/callback?code=x&state=", server.base);
    assert_eq!(idp::finish(&blank, Some("weft_sso=..")).outcome, "expired");
    assert_eq!(idp::finish(&blank, Some("weft_sso=")).outcome, "expired");
    // A code presented twice: the second is refused by the provider.
    let s = idp::start(&server.base);
    let back = idp::visit_provider(&s.location);
    let first = idp::finish(&back, s.cookie.as_deref());
    assert_eq!(first.outcome, "ok");
    let replay = idp::finish(&back, s.cookie.as_deref());
    assert_eq!(replay.outcome, "expired");
    assert!(replay.session.is_none());
    // The person cancelling at the provider.
    fake.with(|st| st.deny = true);
    assert_eq!(idp::round_trip(&server.base).outcome, "denied");
    fake.with(|st| st.deny = false);
    // Garbage in every parameter.
    for q in [
        "",
        "?code=",
        "?state=x",
        "?error=server_error",
        "?code=%00&state=%00",
    ] {
        let l = idp::finish(
            &format!("{}/v1/auth/sso/callback{q}", server.base),
            s.cookie.as_deref(),
        );
        assert!(l.session.is_none(), "{q}");
        assert!(
            ["expired", "error"].contains(&l.outcome.as_str()),
            "{q}: {l:?}"
        );
    }
    // Every answer spends the round-trip cookie.
    assert!(first
        .set_cookies
        .iter()
        .any(|c| c.starts_with("weft_sso=;")));
    assert_eq!(
        accounts(&server),
        before + 1,
        "only the one honest trip made an account"
    );
    assert!(server.healthy());
}

/// An address the provider did not vouch for makes no account; one
/// outside the domains it speaks for is refused as such; and Entra ID's
/// shape — no address in the token, none marked verified — works when
/// the operator names the domain.
#[test]
fn only_addresses_the_provider_vouches_for_make_an_account() {
    let minio = Minio::shared();
    let bucket = minio.bucket("sso-trust");
    let scratch = Scratch::new("sso-trust");
    let fake = idp::spawn(CLIENT, SECRET);
    let open = spawn(&bucket.base_url, &scratch, "sso-trust", &fake);
    let before = accounts(&open);
    fake.sign_in_as(Person {
        email_verified: None,
        ..Person::verified("sub-u", "u@acme.test")
    });
    assert_eq!(idp::round_trip(&open.base).outcome, "noemail");
    fake.sign_in_as(Person {
        email: None,
        ..Person::verified("sub-v", "v@acme.test")
    });
    assert_eq!(idp::round_trip(&open.base).outcome, "noemail");
    assert_eq!(accounts(&open), before);
    drop(open);

    let scratch = Scratch::new("sso-trust-domains");
    let listed = builder(&bucket.base_url, &scratch, "sso-trust-domains", &fake)
        .env("STRATUM_OIDC_ALLOWED_DOMAINS", "acme.test")
        .start();
    listed.bootstrap_org("acme");
    fake.sign_in_as(Person::verified("sub-evil", "someone@evil.test"));
    assert_eq!(idp::round_trip(&listed.base).outcome, "domain");
    // Entra ID's default: the token has no address, userinfo has one
    // with no `email_verified`, and the operator named the domain.
    fake.with(|st| st.email_only_in_userinfo = true);
    fake.sign_in_as(Person {
        email_verified: None,
        ..Person::verified("sub-entra", "entra.person@acme.test")
    });
    let landed = idp::round_trip(&listed.base);
    assert_eq!(landed.outcome, "ok", "{landed:?}");
    assert_eq!(
        me(&listed, &landed.session.unwrap())["email"],
        "entra.person@acme.test"
    );
    // Userinfo that cannot be believed is the provider's fault, not the
    // person's: a newcomer is told `error`, not `noemail` — and somebody
    // already linked needs no address, so is let in regardless.
    for broken in [idp::Userinfo::OtherSubject, idp::Userinfo::Down] {
        fake.with(|st| st.userinfo = broken.clone());
        fake.sign_in_as(Person {
            email_verified: None,
            ..Person::verified("sub-entra-new", "entra.new@acme.test")
        });
        let refused = idp::round_trip(&listed.base);
        assert_eq!(refused.outcome, "error", "{broken:?}: {refused:?}");
        fake.sign_in_as(Person {
            email_verified: None,
            ..Person::verified("sub-entra", "entra.person@acme.test")
        });
        assert_eq!(idp::round_trip(&listed.base).outcome, "ok", "{broken:?}");
    }
    fake.with(|st| st.userinfo = idp::Userinfo::Answers);
    // …and a provider that authenticates clients only in the body.
    fake.with(|st| st.post_auth_only = true);
    let scratch = Scratch::new("sso-trust-post");
    let post = builder(&bucket.base_url, &scratch, "sso-trust-post", &fake)
        .env("STRATUM_OIDC_ALLOWED_DOMAINS", "acme.test")
        .start();
    post.bootstrap_org("acme");
    assert_eq!(idp::round_trip(&post.base).outcome, "ok");
    assert!(listed.healthy() && post.healthy());
}

/// A disabled account is refused; a missing SSO organization fails the
/// sign-in, not the boot; and a provider that is down at boot does not
/// stop the server starting.
#[test]
fn what_is_refused_at_sign_in_rather_than_at_boot() {
    let minio = Minio::shared();
    let bucket = minio.bucket("sso-late");
    let scratch = Scratch::new("sso-late");
    let fake = idp::spawn(CLIENT, SECRET);
    // No `acme` yet: the server must boot anyway.
    let server = builder(&bucket.base_url, &scratch, "sso-late", &fake).start();
    fake.sign_in_as(Person::verified("sub-early", "early@acme.test"));
    assert_eq!(idp::round_trip(&server.base).outcome, "error");
    server.bootstrap_org("acme");
    assert_eq!(idp::round_trip(&server.base).outcome, "ok");
    server
        .admin(&["admin", "user-disable", "--email", "early@acme.test"])
        .unwrap();
    assert_eq!(idp::round_trip(&server.base).outcome, "disabled");
    assert!(server.healthy());

    // A secret the provider does not take is this server's fault, and
    // every sign-in would meet it: `error`, not the `expired` that sends
    // the person round again for nothing.
    let scratch = Scratch::new("sso-late-secret");
    let wrong = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("sso-late-secret")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_OIDC_ISSUER", fake.issuer.clone())
        .env("STRATUM_OIDC_CLIENT_ID", CLIENT)
        .env("STRATUM_OIDC_CLIENT_SECRET", "not-the-secret")
        .env("STRATUM_OIDC_ORG", "acme")
        .start();
    wrong.bootstrap_org("acme");
    fake.sign_in_as(Person::verified("sub-late", "late@acme.test"));
    assert_eq!(idp::round_trip(&wrong.base).outcome, "error");
    assert!(wrong.healthy());

    // A provider nobody is listening at.
    let gone = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let scratch = Scratch::new("sso-late-down");
    let down = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("sso-late-down")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_OIDC_ISSUER", gone)
        .env("STRATUM_OIDC_CLIENT_ID", CLIENT)
        .env("STRATUM_OIDC_CLIENT_SECRET", SECRET)
        .env("STRATUM_OIDC_ORG", "acme")
        .start();
    assert!(down.healthy());
    assert_eq!(idp::round_trip(&down.base).outcome, "error");
}

/// `admin sso-check` asks the provider everything that needs no person
/// at a browser, with the server's own configuration, and says which
/// part is wrong — before the first person meets it.
#[test]
fn sso_check_finds_what_is_wrong_before_anybody_signs_in() {
    let minio = Minio::shared();
    let bucket = minio.bucket("sso-check");
    let scratch = Scratch::new("sso-check");
    let fake = idp::spawn(CLIENT, SECRET);
    let check = |server: &Server| {
        let out = server.admin_in_server_env(&["admin", "sso-check"]);
        let line = String::from_utf8_lossy(&out.stdout).to_string();
        let v: serde_json::Value = serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("sso-check printed {line:?}: {e}"));
        assert_eq!(out.status.success(), v["ok"] == true, "{v}");
        v
    };

    // No organization yet: the one thing wrong, and named.
    let server = builder(&bucket.base_url, &scratch, "sso-check", &fake).start();
    let v = check(&server);
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["organization"]["ok"], false, "{v}");
    assert!(
        v["organization"]["error"]
            .as_str()
            .unwrap()
            .contains("acme"),
        "{v}"
    );
    for part in ["discovery", "keys", "client"] {
        assert_eq!(v[part]["ok"], true, "{part}: {v}");
    }
    assert_eq!(v["keys"]["count"], 1, "{v}");
    assert!(
        v["client"]["answer"]
            .as_str()
            .unwrap()
            .contains("invalid_grant"),
        "the made-up code is what was refused, not the client: {v}"
    );
    assert_eq!(v["sso_only"], true);
    assert_eq!(
        v["callback"],
        format!("{}/v1/auth/sso/callback", server.base)
    );
    server.bootstrap_org("acme");
    assert_eq!(check(&server)["ok"], true);

    // A secret the provider does not take.
    let scratch = Scratch::new("sso-check-secret");
    let wrong = builder(&bucket.base_url, &scratch, "sso-check-secret", &fake)
        .env("STRATUM_OIDC_CLIENT_SECRET", "not-the-secret")
        .start();
    wrong.bootstrap_org("acme");
    let v = check(&wrong);
    assert_eq!(v["client"]["ok"], false, "{v}");
    assert!(
        v["client"]["error"]
            .as_str()
            .unwrap()
            .contains("invalid_client"),
        "{v}"
    );

    // A provider nobody is listening at: discovery says so, and what
    // needs discovery is not asked rather than failed for another reason.
    let gone = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let scratch = Scratch::new("sso-check-down");
    let down = builder(&bucket.base_url, &scratch, "sso-check-down", &fake)
        .env("STRATUM_OIDC_ISSUER", gone)
        .start();
    let v = check(&down);
    assert_eq!(v["discovery"]["ok"], false, "{v}");
    assert!(
        v["keys"]["error"].as_str().unwrap().contains("not asked"),
        "{v}"
    );

    // And a server with no SSO at all is told so, not handed a report.
    let out = server.admin(&["admin", "sso-check"]);
    assert!(out.unwrap_err().contains("not configured"));
}

/// A configuration that could only go wrong later is refused now.
#[test]
fn a_dangerous_configuration_refuses_to_boot() {
    let minio = Minio::shared();
    let bucket = minio.bucket("sso-boot");
    let full = |issuer: &str| {
        vec![
            ("STRATUM_OIDC_ISSUER", issuer.to_string()),
            ("STRATUM_OIDC_CLIENT_ID", "c".to_string()),
            ("STRATUM_OIDC_CLIENT_SECRET", "s".to_string()),
            ("STRATUM_OIDC_ORG", "acme".to_string()),
        ]
    };
    let mut cases: Vec<(Vec<(&str, String)>, &str)> = vec![
        (
            vec![("STRATUM_OIDC_ISSUER", "https://idp.acme.test".into())],
            "set together or not at all",
        ),
        (
            full("https://login.microsoftonline.com/common/v2.0"),
            "Microsoft's shared endpoints",
        ),
        (
            full("https://accounts.google.com"),
            "STRATUM_OIDC_ALLOWED_DOMAINS",
        ),
        (full("http://idp.acme.test"), "must be https://"),
        (
            vec![("STRATUM_SSO_ONLY", "true".into())],
            "nobody could sign in",
        ),
    ];
    let mut no_org = full("https://idp.acme.test");
    no_org.retain(|(k, _)| *k != "STRATUM_OIDC_ORG");
    cases.push((no_org, "STRATUM_OIDC_ORG"));
    for (env, needle) in cases {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("AWS_ACCESS_KEY_ID", "minioadmin")
            .env("AWS_SECRET_ACCESS_KEY", "minioadmin")
            .env("AWS_REGION", "us-east-1")
            .env("STRATUM_STORE_URL", &bucket.base_url)
            .env(
                "STRATUM_DB_URL",
                stratum_testkit::pg::test_db_url("sso-boot"),
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
