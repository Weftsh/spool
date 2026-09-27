//! The confirm-your-email gate, which had no coverage and is live.
//!
//! `authx::require_verified` guards every create path in the API —
//! repositories, mirrors, connected origins — across five call sites,
//! and **nothing exercised any of them**. That absence let two sessions
//! read the function and reach opposite conclusions about whether it
//! does anything for a browser caller, with only prose to arbitrate.
//!
//! It does. `Principal::for_user` sets `user_id: Some(..)`, so a cookie
//! caller reaches `usertokens::is_verified`; the `None` arm is a service
//! token, which has no person to confirm and is exempt by design.
//!
//! The reachable case is narrower than it looks and it took two wrong
//! guesses to find. `admin user-create` marks its account verified, so a
//! seeded member cannot exercise the gate. The account that can is the
//! one signup makes: **signing up creates the personal namespace
//! immediately**, so between signing up and clicking the link a person
//! is the `owner` of an org while `verified_at` is still null. That is
//! the only shape in the product that reaches this gate, and it is the
//! most ordinary one there is — every new account passes through it.
//!
//! So the gate is not defence in depth against a state nobody can reach.
//! It is the thing standing between an unproved address and a namespace
//! full of repositories, and until now the suite said nothing about it.

use stratum_testkit::{gitcli::Scratch, Minio, Server};

const PASSWORD: &str = "a long enough password";

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
fn an_unproved_address_cannot_make_itself_a_namespace() {
    let minio = Minio::shared();
    let bucket = minio.bucket("verified-gate");
    let scratch = Scratch::new("verified-gate");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("verified_gate")
        .data_dir(scratch.path().join("data"))
        .start();

    // Signup, and deliberately no link redeemed.
    let (status, body) = server.req(
        "POST",
        "/v1/auth/signup",
        "",
        Some(serde_json::json!({
            "handle": "unproved",
            "email": "unproved@example.com",
            "name": "Un Proved",
            "password": PASSWORD,
        })),
    );
    assert_eq!(status, 202, "{body}");

    // They can sign in. That is deliberate: refusing the login would
    // strand somebody who mistyped their address with no way back in to
    // ask for another link.
    let cookie = sign_in(&server, "unproved@example.com");
    let (status, me) = as_person(&server, &cookie, "GET", "/v1/auth/me", None);
    assert_eq!(status, 200, "an unproved account cannot sign in: {me}");
    assert!(
        me["verified_at"].is_null(),
        "the fixture is verified, so this test proves nothing: {me}"
    );
    // They already own a namespace: signup mints the personal one
    // immediately, before any link is clicked. This is what makes the
    // gate reachable rather than theoretical, and it is worth asserting
    // rather than assuming — the first two versions of this test
    // guessed wrong about it in both directions.
    assert_eq!(
        me["orgs"].as_array().map(|a| a.len()),
        Some(1),
        "signup no longer mints a namespace; the gate below may be \
         unreachable and this test may be proving nothing: {me}"
    );
    assert_eq!(me["orgs"][0]["name"], "unproved");
    assert_eq!(me["orgs"][0]["role"], "owner");

    // The gate. Owner of the namespace, full authority there, refused on
    // the address alone — and told what to do about it rather than given
    // a bare 403.
    let (status, body) = as_person(
        &server,
        &cookie,
        "POST",
        "/v1/orgs/unproved/repos",
        Some(serde_json::json!({ "name": "widget" })),
    );
    assert_eq!(
        status, 403,
        "an unproved address created a repository: {body}"
    );
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("confirm your email"),
        "the refusal must say what to do about it: {body}"
    );

    // The same door, for a mirror: it costs storage and outbound
    // fetches, so it is at least as gated as a repository.
    let (status, body) = as_person(
        &server,
        &cookie,
        "POST",
        "/v1/orgs/unproved/mirrors",
        Some(serde_json::json!({
            "name": "imported",
            "provider": "generic",
            "origin": "file:///nowhere",
        })),
    );
    assert_eq!(status, 403, "an unproved address created a mirror: {body}");

    // Reading is untouched. The gate is on creating, not on existing: an
    // account that has not proved its address is not thereby a stranger,
    // and a gate that shut them out of everything would be a worse
    // product than the one it protects.
    let (status, body) = as_person(&server, &cookie, "GET", "/v1/orgs/unproved/repos", None);
    assert_eq!(
        status, 200,
        "an unproved account was refused a read: {body}"
    );

    // The server is still healthy and serving.
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The seeded path does not smuggle an unverified owner in.
///
/// `admin user-create` attaches somebody to an org with no email round
/// trip, which makes it the obvious place for an unverified owner to
/// appear. It marks them verified precisely so one does not — and this
/// pins that, because the first version of the test above used this
/// path and passed vacuously against a verified fixture.
#[test]
fn an_account_placed_in_an_org_is_already_verified() {
    let minio = Minio::shared();
    let bucket = minio.bucket("verified-gate-admin");
    let scratch = Scratch::new("verified-gate-admin");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("verified_gate_admin")
        .data_dir(scratch.path().join("data"))
        .start();
    server.bootstrap_org("acme");
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "seeded@acme.dev",
            "--name",
            "Seeded Person",
            "--password",
            PASSWORD,
            "--role",
            "owner",
        ])
        .expect("user-create");

    let cookie = sign_in(&server, "seeded@acme.dev");
    let (status, me) = as_person(&server, &cookie, "GET", "/v1/auth/me", None);
    assert_eq!(status, 200, "{me}");
    assert!(
        me["verified_at"].is_i64(),
        "an account was placed in an org without being verified: {me}"
    );

    // And being verified, they can do the thing the gate protects.
    let (status, body) = as_person(
        &server,
        &cookie,
        "POST",
        "/v1/orgs/acme/repos",
        Some(serde_json::json!({ "name": "widget" })),
    );
    assert_eq!(
        status, 201,
        "a verified owner was refused a repository: {body}"
    );
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A confirmation mail that never arrived does not strand the account:
/// an operator can mint the same link by hand.
///
/// The token's hash is all that is stored, so the link cannot be read
/// back; `admin verify-link` issues a fresh one and prints the URL the
/// mail would have carried. It is the same one-use token redeemed at the
/// same route — nothing is marked verified by fiat — so the gate opens
/// exactly as it would have from the inbox, and the link is spent
/// afterwards.
#[test]
fn an_operator_can_mint_the_confirmation_link_a_mail_never_delivered() {
    let minio = Minio::shared();
    let bucket = minio.bucket("verify-link");
    let scratch = Scratch::new("verify-link");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("verify_link")
        .data_dir(scratch.path().join("data"))
        .start();

    let (status, body) = server.req(
        "POST",
        "/v1/auth/signup",
        "",
        Some(serde_json::json!({
            "handle": "unmailed",
            "email": "unmailed@example.com",
            "name": "Un Mailed",
            "password": PASSWORD,
        })),
    );
    assert_eq!(status, 202, "{body}");
    let cookie = sign_in(&server, "unmailed@example.com");
    let (status, body) = as_person(
        &server,
        &cookie,
        "POST",
        "/v1/orgs/unmailed/repos",
        Some(serde_json::json!({ "name": "widget" })),
    );
    assert_eq!(status, 403, "the gate is not there to open: {body}");

    // The operator's link: the address, whether it was already
    // confirmed, and a URL shaped like the mail's — the token in the
    // fragment, where access logs cannot see it.
    let out = server.admin_json(&[
        "admin",
        "verify-link",
        "--email",
        "unmailed@example.com",
        "--public-url",
        "https://weft.test/",
    ]);
    assert_eq!(out["user"]["email"], "unmailed@example.com", "{out}");
    assert_eq!(out["already_verified"], false, "{out}");
    let url = out["url"].as_str().expect("a url");
    let token = url
        .strip_prefix("https://weft.test/dashboard/#verify=")
        .unwrap_or_else(|| panic!("the link is not the mail's shape: {url}"));
    assert!(!token.is_empty());

    // Redeemed exactly as the dashboard redeems the mailed one.
    let (status, body) = server.req(
        "POST",
        "/v1/auth/verify",
        "",
        Some(serde_json::json!({ "token": token })),
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = as_person(
        &server,
        &cookie,
        "POST",
        "/v1/orgs/unmailed/repos",
        Some(serde_json::json!({ "name": "widget" })),
    );
    assert_eq!(
        status, 201,
        "confirmed by the minted link, still refused: {body}"
    );
    // One use, like the mailed one.
    let (status, body) = server.req(
        "POST",
        "/v1/auth/verify",
        "",
        Some(serde_json::json!({ "token": token })),
    );
    assert_eq!(status, 404, "the minted link worked twice: {body}");
    // Minting again for a confirmed address says so, rather than
    // pretending the account is still waiting.
    let again = server.admin_json(&[
        "admin",
        "verify-link",
        "--email",
        "unmailed@example.com",
        "--public-url",
        "https://weft.test",
    ]);
    assert_eq!(again["already_verified"], true, "{again}");

    // No account, no link — and the URL is required, because a link to
    // the wrong host is worse than no link.
    let err = server.admin_expect_err(&[
        "admin",
        "verify-link",
        "--email",
        "nobody@example.com",
        "--public-url",
        "https://weft.test",
    ]);
    assert!(err.contains("no account for"), "{err}");
    let err = server.admin_expect_err(&["admin", "verify-link", "--email", "unmailed@example.com"]);
    assert!(err.contains("--public-url"), "{err}");
    // On the fleet the flag is left out and the task's own
    // STRATUM_PUBLIC_URL is the host, so a link cannot be typed to the
    // wrong one.
    let out = server
        .admin_with_env(
            &["admin", "verify-link", "--email", "unmailed@example.com"],
            &[("STRATUM_PUBLIC_URL", "https://weft.example")],
        )
        .expect("verify-link from the environment");
    let out: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert!(
        out["url"]
            .as_str()
            .unwrap()
            .starts_with("https://weft.example/dashboard/#verify="),
        "{out}"
    );
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
