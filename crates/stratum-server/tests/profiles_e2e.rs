//! Profiles and email addresses end to end.
//!
//! Two surfaces with opposite rules share one route family, so most of
//! this file is negative. The class being tested is the one the plan
//! names: **does surface X let an anonymous or under-privileged
//! principal learn something they should not** — a private repository's
//! existence, or somebody's `git config user.email` values.
//!
//! A profile is the directory entry for a person or an organization on
//! this server: anybody signed in reads it, nobody reads it anonymously,
//! and it carries nothing about repositories — every repository is
//! private to its organization, so there is no public count or pin for a
//! profile to show.
//!
//! Every attack case ends with a health check. A server that survives an
//! attack by refusing everything has not passed, it has failed
//! differently.

use stratum_testkit::adversarial::INJECTIONS;
use stratum_testkit::browser::Browser;
use stratum_testkit::mailbox::Mailbox;
use stratum_testkit::{gitcli::Scratch, Minio, Server};

fn spawn(store_url: &str, scratch: &Scratch, hint: &str, mail: &Mailbox) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .envs(&mail.env())
        .env("STRATUM_COMPACT_POLL_SECS", "86400")
        .start()
}

/// Percent-decode a mailed link's fragment: templates encode the token
/// so a `:` or `#` in it cannot truncate the credential silently.
fn urldecode(s: &str) -> String {
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

/// The token out of the newest mail sent to `address`, for a link whose
/// fragment key is `key`.
fn mailed_token(mail: &Mailbox, address: &str, key: &str) -> String {
    let msg = mail.wait_for(address, std::time::Duration::from_secs(5));
    let link = msg.link().unwrap_or_else(|| panic!("no link in {msg:?}"));
    let marker = format!("#{key}=");
    let raw = link
        .split_once(&marker)
        .unwrap_or_else(|| panic!("{link} carries no #{key}="))
        .1;
    urldecode(raw)
}

fn make_repo(b: &mut Browser, org: &str, name: &str) {
    let (st, body) = b.req(
        "POST",
        &format!("/v1/orgs/{org}/repos"),
        Some(serde_json::json!({ "name": name })),
    );
    assert_eq!(st, 201, "create {org}/{name}: {body}");
}

/// An anonymous GET — no token, no cookie — which this whole surface
/// now refuses.
fn anon(server: &Server, path: &str) -> (u16, serde_json::Value) {
    server.req("GET", path, "", None)
}

// ---------------------------------------------------------------------

/// The happy path, and the shape somebody else signed in sees. A
/// logged-out visitor sees nothing: the profile answers them 401, for a
/// handle that exists and one that does not alike.
#[test]
fn a_profile_is_read_by_anybody_signed_in_and_reads_back_what_its_owner_wrote() {
    let minio = Minio::shared();
    let bucket = minio.bucket("profiles-happy");
    let scratch = Scratch::new("profiles-happy");
    let mail = Mailbox::temp("profiles-happy");
    let server = spawn(&bucket.base_url, &scratch, "profiles-happy", &mail);
    let mut ada = Browser::stranger(&server, "ada", "ada@example.test");
    // Somebody else on the server, who belongs to nothing of Ada's.
    let mut bea = Browser::stranger(&server, "bea", "bea@example.test");

    // Nobody reads a profile anonymously, and the refusal is the same
    // for a handle nobody has — it comes before the handle is looked up.
    for handle in ["ada", "nobody"] {
        let (st, body) = anon(&server, &format!("/v1/users/{handle}"));
        assert_eq!(st, 401, "{handle}: {body}");
        assert!(!body.to_string().contains("ada@example.test"), "{body}");
    }

    // Empty but present from the moment the account exists — no
    // "create your profile" step, and no second state for a page to
    // handle.
    let (st, body) = bea.req("GET", "/v1/users/ada", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["handle"], "ada");
    assert_eq!(body["kind"], "human");
    assert_eq!(body["bio"], serde_json::Value::Null);
    assert_eq!(body["links"].as_array().unwrap().len(), 0);
    // A profile says nothing about repositories: there is no public
    // repository to count or to pin.
    assert!(body.get("public_repos").is_none(), "{body}");
    assert!(body.get("pins").is_none(), "{body}");
    // Never, under any circumstances, an address.
    assert!(
        !body.to_string().contains("ada@example.test"),
        "a profile carried an email address: {body}"
    );

    let (st, body) = ada.req(
        "PATCH",
        "/v1/users/ada",
        Some(serde_json::json!({
            "display_name": "Ada Lovelace",
            "bio": "Analytical engines.",
            "location": "London",
            "pronouns": "she/her",
            "profile_repo": "ada",
            "links": [
                { "label": "Home", "url": "https://ada.example" },
                { "url": "https://notes.example/ada" }
            ],
        })),
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["display_name"], "Ada Lovelace");

    let (st, body) = bea.req("GET", "/v1/users/ada", None);
    assert_eq!(st, 200);
    assert_eq!(body["display_name"], "Ada Lovelace");
    assert_eq!(body["bio"], "Analytical engines.");
    assert_eq!(body["pronouns"], "she/her");
    assert_eq!(body["profile_repo"], "ada");
    let links = body["links"].as_array().unwrap();
    assert_eq!(links.len(), 2);
    assert_eq!(links[0]["label"], "Home");
    assert_eq!(links[1]["label"], serde_json::Value::Null);

    // A patch that names one field leaves the rest alone — including
    // the links, which are replaced only when the key is present.
    let (st, _) = ada.req(
        "PATCH",
        "/v1/users/ada",
        Some(serde_json::json!({ "location": "Kent" })),
    );
    assert_eq!(st, 200);
    let (_, body) = bea.req("GET", "/v1/users/ada", None);
    assert_eq!(body["location"], "Kent");
    assert_eq!(body["bio"], "Analytical engines.");
    assert_eq!(body["links"].as_array().unwrap().len(), 2);

    // …and an explicit null clears one, which an absent key cannot.
    let (st, _) = ada.req(
        "PATCH",
        "/v1/users/ada",
        Some(serde_json::json!({ "bio": null, "links": [] })),
    );
    assert_eq!(st, 200);
    let (_, body) = bea.req("GET", "/v1/users/ada", None);
    assert_eq!(body["bio"], serde_json::Value::Null);
    assert_eq!(body["links"].as_array().unwrap().len(), 0);

    // Everything the control plane refuses is something Ada typed and
    // can retype, so it comes back as a 400 and not a 500 — a cap, a
    // control character, a scheme we will not render as a link. The
    // last one is the one that matters: a profile renders on a page
    // anybody loads, so a `javascript:` link is stored XSS.
    for bad in [
        serde_json::json!({ "bio": "x".repeat(601) }),
        serde_json::json!({ "display_name": "Ada\u{0007}Lovelace" }),
        serde_json::json!({ "links": [{ "url": "javascript:alert(1)" }] }),
    ] {
        let (st, out) = ada.req("PATCH", "/v1/users/ada", Some(bad.clone()));
        assert_eq!(st, 400, "{bad} answered {st}: {out}");
    }
    let (_, body) = bea.req("GET", "/v1/users/ada", None);
    assert_eq!(
        body["bio"],
        serde_json::Value::Null,
        "a refused patch landed anyway: {body}"
    );
    assert_eq!(body["links"].as_array().unwrap().len(), 0, "{body}");

    assert!(server.healthy());
}

/// The self-only routes, against everybody who is not that self.
///
/// The addresses are the surface that matters most here: they decide
/// which commits in the world count as this person's work, so a read by
/// anybody else is the leak.
#[test]
fn every_self_only_route_refuses_a_stranger_and_an_anonymous_caller() {
    let minio = Minio::shared();
    let bucket = minio.bucket("profiles-self");
    let scratch = Scratch::new("profiles-self");
    let mail = Mailbox::temp("profiles-self");
    let server = spawn(&bucket.base_url, &scratch, "profiles-self", &mail);
    let mut ada = Browser::stranger(&server, "ada", "ada@example.test");
    let mut mallory = Browser::stranger(&server, "mallory", "mallory@example.test");

    // Ada has an address on file — the one her account was made with,
    // proved on the way in.
    let (st, body) = ada.req("GET", "/v1/users/ada/emails", None);
    assert_eq!(st, 200, "{body}");
    let mine = body["emails"].as_array().unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["address"], "ada@example.test");
    assert_eq!(mine[0]["primary"], true);
    assert!(mine[0]["verified_at"].is_i64(), "{body}");

    let writes: &[(&str, &str, serde_json::Value)] = &[
        (
            "PATCH",
            "/v1/users/ada",
            serde_json::json!({ "bio": "owned" }),
        ),
        (
            "POST",
            "/v1/users/ada/emails",
            serde_json::json!({ "email": "mallory@evil.test" }),
        ),
        (
            "POST",
            "/v1/users/ada/emails/verify",
            serde_json::json!({ "token": "ada@example.test:whatever" }),
        ),
    ];
    let reads: &[&str] = &["/v1/users/ada/emails"];

    // Signed in as somebody else: forbidden, plainly. A handle is a
    // public URL, so masking here would only puzzle the person who is
    // signed in as the wrong account.
    for (method, path, body) in writes {
        let (st, out) = mallory.req(method, path, Some(body.clone()));
        assert_eq!(st, 403, "{method} {path} as a stranger: {out}");
    }
    for path in reads {
        let (st, out) = mallory.req("GET", path, None);
        assert_eq!(st, 403, "GET {path} as a stranger: {out}");
        assert!(
            !out.to_string().contains("ada@example.test"),
            "a stranger was shown an address: {out}"
        );
    }
    let (st, out) = mallory.req("DELETE", "/v1/users/ada/emails/ada@example.test", None);
    assert_eq!(st, 403, "{out}");

    // Anonymous: 401, which a browser can act on.
    for (method, path, body) in writes {
        let st = server.req(method, path, "", Some(body.clone())).0;
        assert_eq!(st, 401, "{method} {path} anonymously");
    }
    for path in reads {
        let (st, out) = anon(&server, path);
        assert_eq!(st, 401, "GET {path} anonymously");
        assert!(!out.to_string().contains("ada@example.test"), "{out}");
    }
    assert_eq!(
        server
            .req("DELETE", "/v1/users/ada/emails/ada@example.test", "", None)
            .0,
        401
    );

    // Nothing landed: Ada's profile and addresses are untouched.
    let (_, body) = ada.req("GET", "/v1/users/ada", None);
    assert_eq!(
        body["bio"],
        serde_json::Value::Null,
        "a stranger wrote: {body}"
    );
    let (_, body) = ada.req("GET", "/v1/users/ada/emails", None);
    assert_eq!(body["emails"].as_array().unwrap().len(), 1, "{body}");

    // A handle nobody has is a 404 on every self-only route, before
    // authority is even considered — the same 404 signed in or not. A
    // handle is not a secret: accepting an invitation refuses a taken
    // one by name. The profile itself is read only by somebody signed
    // in, who gets the same 404; anonymous is told to sign in first,
    // whatever the handle.
    assert_eq!(anon(&server, "/v1/users/nobody/emails").0, 404);
    assert_eq!(mallory.req("GET", "/v1/users/nobody/emails", None).0, 404);
    assert_eq!(mallory.req("GET", "/v1/users/nobody", None).0, 404);
    assert_eq!(anon(&server, "/v1/users/nobody").0, 401);

    // Pins went with public repositories. The route is not a refusal
    // waiting for the right caller; it is not there, for anybody.
    let (st, _) = ada.req("GET", "/v1/users/ada/pins", None);
    assert_eq!(st, 404, "the pins read is still served");
    let (st, _) = ada.req(
        "PUT",
        "/v1/users/ada/pins",
        Some(serde_json::json!({ "pins": [] })),
    );
    assert!(
        st == 404 || st == 405,
        "the pins write is still served: {st}"
    );

    // Hostile handles are settled by shape and never reach a query —
    // for somebody signed in, who gets past the sign-in gate to the
    // lookup, and for somebody who is not, who does not.
    for injection in INJECTIONS {
        let encoded = injection
            .replace('%', "%25")
            .replace(' ', "%20")
            .replace('#', "%23")
            .replace('?', "%3F")
            .replace('/', "%2F");
        let path = format!("/v1/users/{encoded}");
        let (st, _) = mallory.req("GET", &path, None);
        assert!(
            st == 404 || st == 400,
            "{path} answered {st} for {injection:?}"
        );
        let (st, _) = anon(&server, &path);
        assert!(
            st == 401 || st == 404 || st == 400,
            "{path} answered {st} anonymously for {injection:?}"
        );
    }

    assert!(server.healthy());
}

/// Claiming, proving and never disclosing an address.
#[test]
fn an_address_is_claimed_proved_and_never_attributed_to_a_stranger() {
    let minio = Minio::shared();
    let bucket = minio.bucket("profiles-mail");
    let scratch = Scratch::new("profiles-mail");
    let mail = Mailbox::temp("profiles-mail");
    let server = spawn(&bucket.base_url, &scratch, "profiles-mail", &mail);
    let mut ada = Browser::stranger(&server, "ada", "ada@example.test");
    let mut mallory = Browser::stranger(&server, "mallory", "mallory@example.test");

    // Claiming mails a link and does not put it in the response — a
    // claim confirmed over the API would prove nothing at all.
    let (st, body) = ada.req(
        "POST",
        "/v1/users/ada/emails",
        Some(serde_json::json!({ "email": "  Ada@Work.test " })),
    );
    assert_eq!(st, 202, "{body}");
    assert_eq!(body["address"], "ada@work.test");
    let token = mailed_token(&mail, "ada@work.test", "verify-email");
    assert!(
        !body.to_string().contains(token.split(':').nth(1).unwrap()),
        "the confirmation secret was in the response body: {body}"
    );

    // Unproved, so it counts for nothing and stays private.
    let (_, body) = ada.req("GET", "/v1/users/ada/emails", None);
    let claimed = body["emails"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["address"] == "ada@work.test")
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(claimed["verified_at"], serde_json::Value::Null);
    assert_eq!(claimed["private"], true);
    assert_eq!(claimed["primary"], false);

    // The link is bound to the account. Pasted into somebody else's
    // session it is inert — and it does not become spent, so the real
    // owner's link still works afterwards.
    let (st, out) = mallory.req(
        "POST",
        "/v1/users/mallory/emails/verify",
        Some(serde_json::json!({ "token": token })),
    );
    assert_eq!(st, 404, "{out}");

    // …and it is bound to the address, so the secret cannot be aimed at
    // a mailbox the holder never proved.
    let secret = token.split_once(':').unwrap().1;
    let (st, out) = ada.req(
        "POST",
        "/v1/users/ada/emails/verify",
        Some(serde_json::json!({ "token": format!("someone@else.test:{secret}") })),
    );
    assert_eq!(st, 404, "{out}");

    let (st, out) = ada.req(
        "POST",
        "/v1/users/ada/emails/verify",
        Some(serde_json::json!({ "token": token })),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["address"], "ada@work.test");
    // Exactly once.
    assert_eq!(
        ada.req(
            "POST",
            "/v1/users/ada/emails/verify",
            Some(serde_json::json!({ "token": token })),
        )
        .0,
        404
    );

    // An address belongs to at most one account, and the refusal names
    // nobody — not the handle, not the account, nothing.
    let (st, out) = mallory.req(
        "POST",
        "/v1/users/mallory/emails",
        Some(serde_json::json!({ "email": "ada@work.test" })),
    );
    assert_eq!(st, 409, "{out}");
    let text = out.to_string();
    for leak in ["ada", "Ada"] {
        assert!(
            !text.contains(leak),
            "the conflict disclosed who holds the address: {out}"
        );
    }
    // Including somebody's *sign-in* address, which is the one an
    // impersonator would reach for first.
    let (st, out) = mallory.req(
        "POST",
        "/v1/users/mallory/emails",
        Some(serde_json::json!({ "email": "ada@example.test" })),
    );
    assert_eq!(st, 409, "{out}");
    assert!(!out.to_string().contains("ada@example.test"), "{out}");
    // No mail was sent to a mailbox Mallory does not own. Ada's account
    // was made by an operator, so nothing has ever been mailed there and
    // any message at all is the refused claim's.
    assert_eq!(
        mail.to("ada@example.test").len(),
        0,
        "a failed claim mailed the address's real owner: {:?}",
        mail.to("ada@example.test")
    );

    // Adding your own again is a conflict too, and mints no second link.
    let before = mail.to("ada@work.test").len();
    let (st, _) = ada.req(
        "POST",
        "/v1/users/ada/emails",
        Some(serde_json::json!({ "email": "ADA@Work.test" })),
    );
    assert_eq!(st, 409);
    assert_eq!(mail.to("ada@work.test").len(), before);

    // Removing works for a secondary address and never for the one the
    // account signs in with.
    assert_eq!(
        ada.req("DELETE", "/v1/users/ada/emails/ada@work.test", None)
            .0,
        204
    );
    let (st, out) = ada.req("DELETE", "/v1/users/ada/emails/ada@example.test", None);
    assert_eq!(st, 404, "the sign-in address was removable: {out}");
    let (_, body) = ada.req("GET", "/v1/users/ada/emails", None);
    assert_eq!(body["emails"].as_array().unwrap().len(), 1);

    // Nonsense in, refusal out — not a 500.
    for bad in ["not-an-address", "", "  ", "a@b"] {
        let (st, out) = ada.req(
            "POST",
            "/v1/users/ada/emails",
            Some(serde_json::json!({ "email": bad })),
        );
        assert_eq!(st, 400, "{bad:?} answered {st}: {out}");
    }
    for bad in ["", "nonsense", "a:b", ":", "ada@work.test:"] {
        let (st, _) = ada.req(
            "POST",
            "/v1/users/ada/emails/verify",
            Some(serde_json::json!({ "token": bad })),
        );
        assert_eq!(st, 404, "{bad:?}");
    }

    assert!(server.healthy());
}

/// A repository must not reach a profile by any route, and a profile is
/// read by a *person*.
///
/// This used to be about pins and the public-repository count, the two
/// routes a private repository could reach a public profile by. Both are
/// gone with public repositories, so the property that remains is the
/// plain one: nothing Ada holds — not a repository's name, not how many
/// she has — is in what somebody else reads about her.
///
/// And the credential half, which is about who counts as a person: a
/// token wins over a session when both are presented, so a developer
/// pasting one into a request gets that token's authority and not their
/// own browser's — and a *repo-bound* token is nobody on this surface,
/// because it was minted to reach one repository and a person's profile
/// is not that repository.
#[test]
fn a_private_repository_never_reaches_a_profile() {
    let minio = Minio::shared();
    let bucket = minio.bucket("profiles-private");
    let scratch = Scratch::new("profiles-private");
    let mail = Mailbox::temp("profiles-private");
    let server = spawn(&bucket.base_url, &scratch, "profiles-private", &mail);
    let mut ada = Browser::stranger(&server, "ada", "ada@example.test");
    let mut mallory = Browser::stranger(&server, "mallory", "mallory@example.test");

    make_repo(&mut ada, "ada", "secret");
    let (st, body) = mallory.req("GET", "/v1/users/ada", None);
    assert_eq!(st, 200, "{body}");
    let text = body.to_string();
    assert!(
        !text.contains("secret") && body.get("public_repos").is_none(),
        "a profile told a stranger about a repository: {body}"
    );
    // Nor is the repository itself any more reachable for having an
    // owner with a profile.
    let (st, _) = mallory.req("GET", "/v1/orgs/ada/repos/secret", None);
    assert_eq!(st, 404);

    // The same read, made with a token instead of a cookie.
    let (st, minted) = ada.req(
        "POST",
        "/v1/orgs/ada/tokens",
        Some(serde_json::json!({ "scopes": ["repo:read"], "label": "laptop" })),
    );
    assert_eq!(st, 201, "{minted}");
    let personal = minted["token"].as_str().unwrap().to_string();
    let (st, minted) = ada.req(
        "POST",
        "/v1/orgs/ada/tokens",
        Some(serde_json::json!({ "scopes": ["repo:read"], "repo": "secret", "label": "ci" })),
    );
    assert_eq!(st, 201, "{minted}");
    let repo_bound = minted["token"].as_str().unwrap().to_string();

    let (st, body) = server.req("GET", "/v1/users/mallory", &personal, None);
    assert_eq!(st, 200, "Ada's own token did not speak for Ada: {body}");
    let (st, body) = server.req("GET", "/v1/users/mallory", &repo_bound, None);
    assert_eq!(
        st, 401,
        "a repo-bound token was read as the person who minted it: {body}"
    );
    // …and it is nobody on the self-only routes either, so it can never
    // be the way an address list leaks out of a CI job.
    let (st, body) = server.req("GET", "/v1/users/ada/emails", &repo_bound, None);
    assert_eq!(st, 401, "{body}");
    assert!(!body.to_string().contains("ada@example.test"), "{body}");
    let (st, body) = server.req("GET", "/v1/users/ada/emails", &personal, None);
    assert_eq!(st, 200, "{body}");

    // A credential that is not a credential is a 401 and never a quiet
    // fall back to anonymous: the caller has a typo'd or revoked token
    // and has to be told to fix it.
    let (st, out) = server.req("GET", "/v1/users/ada", "not-a-token", None);
    assert_eq!(st, 401, "{out}");

    assert!(server.healthy());
}

/// An org's profile: its members read it, by session or by token; only
/// an administrator writes it; and to anybody outside it — signed out,
/// or signed in to another organization — it answers exactly as an
/// organization that does not exist.
///
/// It used to be readable by anybody signed in at all, which made it the
/// one door where a person from elsewhere could tell a real organization
/// (200) from a name nobody holds (404), and it refused a token minted
/// in the organization as "not signed in".
#[test]
fn an_org_profile_is_read_by_its_members_and_written_by_an_admin() {
    let minio = Minio::shared();
    let bucket = minio.bucket("profiles-org");
    let scratch = Scratch::new("profiles-org");
    let mail = Mailbox::temp("profiles-org");
    let server = spawn(&bucket.base_url, &scratch, "profiles-org", &mail);
    let admin = server.bootstrap_org("acme");
    let mut vera = Browser::person(&server, "acme", "viewer", "vera", "vera@acme.test");
    let mut mallory = Browser::stranger(&server, "mallory", "mallory@example.test");

    // Anonymous is told to sign in, for an org that exists and one that
    // does not.
    for org in ["acme", "nobody"] {
        let (st, body) = anon(&server, &format!("/v1/orgs/{org}/profile"));
        assert_eq!(st, 401, "{org}: {body}");
    }
    // Somebody from another organization is told what a missing
    // organization tells them, word for word.
    let outside = mallory.req("GET", "/v1/orgs/acme/profile", None);
    let missing = mallory.req("GET", "/v1/orgs/nobody/profile", None);
    assert_eq!(outside, missing);
    assert_eq!(outside.0, 404, "{}", outside.1);

    // Empty and present before anybody edits it — to the least of its
    // members, and to a token minted in it.
    let (st, body) = vera.req("GET", "/v1/orgs/acme/profile", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["org"], "acme");
    assert_eq!(body["display_name"], serde_json::Value::Null);
    assert!(body.get("public_repos").is_none(), "{body}");
    let (st, body) = server.req("GET", "/v1/orgs/acme/profile", &admin, None);
    assert_eq!(st, 200, "a token minted in the org was refused: {body}");

    let (st, body) = server.req(
        "PATCH",
        "/v1/orgs/acme/profile",
        &admin,
        Some(serde_json::json!({
            "display_name": "Acme Corp",
            "description": "We make things.",
            "website": "https://acme.example",
            "contact_email": "  Hello@Acme.Example ",
        })),
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["contact_email"], "hello@acme.example");

    let (_, body) = vera.req("GET", "/v1/orgs/acme/profile", None);
    assert_eq!(body["display_name"], "Acme Corp");
    assert_eq!(body["website"], "https://acme.example");

    // A second patch merges rather than replaces.
    let (st, _) = server.req(
        "PATCH",
        "/v1/orgs/acme/profile",
        &admin,
        Some(serde_json::json!({ "location": "Bath" })),
    );
    assert_eq!(st, 200);
    let (_, body) = vera.req("GET", "/v1/orgs/acme/profile", None);
    assert_eq!(body["location"], "Bath");
    assert_eq!(body["display_name"], "Acme Corp");

    // Refusals, for the same reason the personal profile makes them:
    // both render on a page other people load.
    for bad in [
        serde_json::json!({ "website": "javascript:alert(1)" }),
        serde_json::json!({ "contact_email": "not-an-address" }),
        serde_json::json!({ "display_name": "x".repeat(101) }),
        serde_json::json!({ "description": "x".repeat(601) }),
        serde_json::json!({ "location": "Bath\u{0007}" }),
    ] {
        let (st, out) = server.req("PATCH", "/v1/orgs/acme/profile", &admin, Some(bad.clone()));
        assert_eq!(st, 400, "{bad} answered {st}: {out}");
    }
    assert_eq!(
        vera.req("GET", "/v1/orgs/acme/profile", None).1["website"],
        "https://acme.example",
        "a refused patch landed anyway"
    );

    // A stranger cannot write it, and is masked rather than forbidden —
    // `authx::require` treats a non-member as somebody who cannot know
    // the org's shape, which is the rule every other admin route uses.
    let (st, out) = mallory.req(
        "PATCH",
        "/v1/orgs/acme/profile",
        Some(serde_json::json!({ "display_name": "Owned" })),
    );
    assert_eq!(st, 404, "{out}");
    let st = server
        .req(
            "PATCH",
            "/v1/orgs/acme/profile",
            "",
            Some(serde_json::json!({ "display_name": "Owned" })),
        )
        .0;
    assert_eq!(st, 401, "anonymous write");
    assert_eq!(
        vera.req("GET", "/v1/orgs/acme/profile", None).1["display_name"],
        "Acme Corp",
        "a refused write landed anyway"
    );

    // An org nobody has — on the write as well as the read, and the
    // name is settled before authority is even considered, so an
    // administrator of some *other* org gets the same 404 a signed-in
    // reader does rather than a 403 that would confirm the absence.
    assert_eq!(mallory.req("GET", "/v1/orgs/nobody/profile", None).0, 404);
    let (st, out) = server.req(
        "PATCH",
        "/v1/orgs/nobody/profile",
        &admin,
        Some(serde_json::json!({ "display_name": "Ghost" })),
    );
    assert_eq!(st, 404, "{out}");

    assert!(server.healthy());
}

/// A profile is not a way to take a name the platform has claimed.
///
/// `/v1/users/…` hangs under `v1`, which `registry::RESERVED` already
/// holds, so no namespace can shadow this family. That claim is worth
/// checking rather than asserting: the reason `monorepo` went unreserved
/// for so long is that everybody assumed somebody had checked.
#[test]
fn the_new_route_family_sits_under_a_reserved_first_segment() {
    assert!(
        stratum_control::registry::is_reserved("v1"),
        "`/v1/users/…` hangs under `v1`; if `v1` were free, a namespace \
         could be created that the router would shadow forever"
    );
    // And the family's own second segment is not a namespace anybody
    // could take either way — `users` is not a top-level path.
    assert!(stratum_control::registry::is_reserved("dashboard"));
}

/// `openapi.json` is a JSON *object*, and an object with the same key
/// twice is one where a reader and a parser disagree.
///
/// This is a real defect this slice found and fixed:
/// `/v1/orgs/{org}/repos/{repo}` carried two `"patch"` entries — the
/// default-branch one written first and the description/visibility one
/// written later. Every JSON parser keeps the last, so the
/// default-branch documentation was invisible to Swagger UI, to
/// `llms-full.txt`, and to anybody reading the served document, while
/// still sitting in the file looking maintained.
/// `docs_e2e::every_documented_route_exists_and_every_api_route_is_documented`
/// could not catch it: it compares *path* keys, and both entries were
/// under one path.
#[test]
fn the_openapi_document_declares_every_operation_exactly_once() {
    const SPEC: &str = include_str!("../../../docs/openapi.json");

    /// A `serde_json::Value` that refuses to be built from an object
    /// with the same key twice.
    ///
    /// It has to be a deserializer rather than an inspection of the
    /// parsed document, because by the time `Value` exists the evidence
    /// is gone: `serde_json` keeps the last of a duplicate pair, exactly
    /// as every other parser does, and the shadowed entry has already
    /// vanished. Checking the raw text instead would mean writing a
    /// second JSON parser out of `str::find`, which is how a test starts
    /// reporting on its own bugs rather than the document's.
    struct Unique(serde_json::Value);

    impl<'de> serde::Deserialize<'de> for Unique {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> serde::de::Visitor<'de> for V {
                type Value = serde_json::Value;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("any JSON value")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut out = serde_json::Map::new();
                    while let Some(key) = map.next_key::<String>()? {
                        let value: Unique = map.next_value()?;
                        if out.insert(key.clone(), value.0).is_some() {
                            return Err(serde::de::Error::custom(format!(
                                "the key {key:?} appears twice in one object — a parser \
                                 keeps the last and silently discards the earlier one"
                            )));
                        }
                    }
                    Ok(serde_json::Value::Object(out))
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut out = Vec::new();
                    while let Some(Unique(v)) = seq.next_element()? {
                        out.push(v);
                    }
                    Ok(serde_json::Value::Array(out))
                }
                // Scalars carry no keys, so they pass straight through.
                fn visit_unit<E>(self) -> Result<Self::Value, E> {
                    Ok(serde_json::Value::Null)
                }
                fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
                    Ok(v.into())
                }
                fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                    Ok(v.into())
                }
                fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                    Ok(v.into())
                }
                fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
                    Ok(v.into())
                }
                fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                    Ok(v.into())
                }
            }
            d.deserialize_any(V).map(Unique)
        }
    }

    let spec: Unique = serde_json::from_str(SPEC)
        .unwrap_or_else(|e| panic!("openapi.json is not a well-formed document: {e}"));
    // And it is still the document the other gates expect, so a parser
    // that silently accepted nothing could not pass this.
    assert_eq!(spec.0["openapi"], "3.1.0");
    assert!(
        spec.0["paths"].as_object().expect("paths object").len() >= 10,
        "the spec lost its paths"
    );
}
