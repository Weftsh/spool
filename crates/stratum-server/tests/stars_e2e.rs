//! Stars, end to end against a real server.
//!
//! The feature is one number and one rule, and the rule is the part
//! worth testing: **our count and somebody else's are never added
//! together.** A mirrored project's honest four of ours beside a
//! separately labelled "60.3k on GitHub" is the whole argument that
//! this forge does not inflate a counter on the day it opens.
//!
//! What is covered here is the native half — the half with HTTP routes.
//! There is deliberately no endpoint that writes an imported count: it
//! is set by the mirror path from what the upstream said, and inventing
//! an API to type one in would be a way to fabricate the exact number
//! the rule exists to protect. The imported half is covered by the
//! control-plane tests in `stratum_control::stars`, including that
//! importing 60,300 leaves our own count at 1 and that "the origin said
//! zero" and "we never asked" stay different facts.
//!
//! Two negatives matter as much as the happy path: a service token
//! cannot star anything, and a stranger reading a public repository is
//! answered rather than refused.

use std::path::Path;
use stratum_testkit::browser::Browser;
use stratum_testkit::fake_github;
use stratum_testkit::gitcli;
use stratum_testkit::mailbox::Mailbox;
use stratum_testkit::{gitcli::Scratch, Minio, Server};

const PASSWORD: &str = "a long enough password";

fn spawn(store_url: &str, scratch: &Scratch, hint: &str, mail: &Mailbox) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"));
    for (k, v) in mail.env() {
        b = b.env(k, v);
    }
    b.start()
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v as char);
                    i += 3;
                    continue;
                }
                out.push('%');
                i += 1;
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

fn mailed_token(mail: &Mailbox, address: &str, key: &str) -> String {
    let msg = mail.wait_for(address, std::time::Duration::from_secs(10));
    let link = msg.link().unwrap_or_else(|| panic!("no link in {msg:?}"));
    let marker = format!("#{key}=");
    let raw = link
        .split_once(&marker)
        .unwrap_or_else(|| panic!("{link} carries no #{key}="))
        .1;
    urldecode(raw)
}

/// Sign somebody up through the product's own flow and hand back a
/// browser holding their session — a *person*, which is the only kind
/// of caller that may star anything.
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
    let token = mailed_token(mail, email, "verify");
    let mut b = Browser::new(server);
    let (st, body) = b.req(
        "POST",
        "/v1/auth/verify",
        Some(serde_json::json!({ "token": token })),
    );
    assert_eq!(st, 200, "verify {handle}: {body}");
    b
}

#[test]
fn a_star_is_one_persons_and_the_count_follows_the_rows() {
    let minio = Minio::shared();
    let bucket = minio.bucket("stars-e2e");
    let scratch = Scratch::new("stars");
    let mail = Mailbox::temp("stars");
    let server = spawn(&bucket.base_url, &scratch, "stars", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");
    let path = "/v1/orgs/ada/repos/widget/star";

    // Nobody has starred it, and "nobody" is an honest zero rather than
    // an absent field: our own count is always a number we can stand
    // behind. The imported one is absent, which is a different thing.
    let (st, body) = ada.req("GET", path, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["stars"], 0);
    assert_eq!(body["starred"], false);
    assert!(
        body["origin"].is_null(),
        "a native repo has no imported count, and null is not zero: {body}"
    );

    let (st, body) = ada.req("PUT", path, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["stars"], 1);
    assert_eq!(body["starred"], true);

    // Starring twice is starring once. A double-click, a retried
    // request and two open tabs are all this shape, and a count that
    // moved twice would be a number no row justifies.
    let (st, body) = ada.req("PUT", path, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["stars"], 1, "starring twice counted twice: {body}");

    // Somebody else's star is their own, and it moves the shared count.
    let (st, body) = bob.req("GET", path, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["stars"], 1);
    assert_eq!(
        body["starred"], false,
        "bob has not starred it; ada's star is not his: {body}"
    );
    let (st, body) = bob.req("PUT", path, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["stars"], 2);
    assert_eq!(body["starred"], true);

    // Unstarring removes one row, not the feature.
    let (st, body) = ada.req("DELETE", path, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["stars"], 1);
    assert_eq!(body["starred"], false);
    // ...and bob still has his.
    assert_eq!(bob.req("GET", path, None).1["starred"], true);

    // Unstarring what you never starred is a request for a state you
    // are already in, not an error — and it must not drive the count
    // below the rows.
    let (st, body) = ada.req("DELETE", path, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["stars"], 1, "an idle unstar moved the count: {body}");

    // The server is still healthy and serving after all of that.
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_stranger_is_told_the_count_and_a_service_token_cannot_move_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("stars-e2e-auth");
    let scratch = Scratch::new("stars-auth");
    let mail = Mailbox::temp("stars-auth");
    let server = spawn(&bucket.base_url, &scratch, "stars_auth", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    for (name, public) in [("widget", true), ("ledger", false)] {
        let (st, body) = ada.req(
            "POST",
            "/v1/orgs/ada/repos",
            Some(serde_json::json!({ "name": name, "public": public })),
        );
        assert_eq!(st, 201, "{body}");
    }
    let public = "/v1/orgs/ada/repos/widget/star";
    ada.req("PUT", public, None);

    // A stranger with no account reads the count of a public
    // repository. This is the whole reason the read is not behind a
    // session: a signed-out visitor deciding whether a project is alive
    // is exactly who the number is for.
    let (st, body) = server.req("GET", public, "", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["stars"], 1);
    assert_eq!(
        body["starred"], false,
        "a stranger has starred nothing: {body}"
    );

    // A stranger cannot star it, and is told to sign in rather than
    // silently ignored.
    let (st, body) = server.req("PUT", public, "", None);
    assert_eq!(st, 401, "{body}");
    assert_eq!(
        server.req("GET", public, "", None).1["stars"],
        1,
        "a refused star moved the count"
    );

    // Every new route is a new way to leak that something exists, and
    // the property that matters is not any particular status code — it
    // is that a stranger cannot tell a private repository from one that
    // was never created. Asserting a literal 404 here was my own first
    // guess and it was wrong: this surface answers an anonymous caller
    // 401 in both cases, which is the same answer, which is the point.
    // Pinning the *comparison* rather than the number also means a
    // deliberate change to the code stays green while an accidental
    // divergence — the real bug — goes red.
    let (private, _) = server.req("GET", "/v1/orgs/ada/repos/ledger/star", "", None);
    let (absent, _) = server.req("GET", "/v1/orgs/ada/repos/no-such-repo/star", "", None);
    assert_eq!(
        private, absent,
        "a stranger can tell a private repository from a missing one: \
         private answered {private}, missing answered {absent}"
    );
    // And neither of them is a success, which is the other half: two
    // matching 200s would satisfy the comparison above and leak
    // everything.
    assert!(
        private >= 400,
        "a private repository answered a stranger {private}"
    );

    // The same masking on the *writing* verbs, which is a different code
    // path and was reaching nothing: reading a star resolves the
    // repository and then asks who is asking, but starring resolves the
    // repository first and can be refused there, before the question of
    // whether the caller is a person ever arises.
    //
    // Worth its own assertions rather than assuming the GET covers it —
    // a route that masked a private repository on read and admitted it
    // on write would leak exactly what the masking is for, and would do
    // it to the verb an attacker would actually try.
    for verb in ["PUT", "DELETE"] {
        let (private, body) = server.req(verb, "/v1/orgs/ada/repos/ledger/star", "", None);
        let (absent, _) = server.req(verb, "/v1/orgs/ada/repos/no-such-repo/star", "", None);
        assert_eq!(
            private, absent,
            "{verb} tells a stranger a private repository exists: \
             private answered {private}, missing answered {absent} ({body})"
        );
        assert!(
            private >= 400,
            "{verb} on a private repository answered a stranger {private}"
        );
    }

    // A *service* token may read the repository and may not star it.
    // Starring is a person's act: a token has no opinion about a
    // project, and a count machines can move is a count that means
    // nothing.
    let (st, minted) = ada.req(
        "POST",
        "/v1/orgs/ada/tokens",
        Some(serde_json::json!({ "scopes": ["repo:read"], "repo": "widget", "label": "ci" })),
    );
    assert_eq!(st, 201, "{minted}");
    let service = minted["token"].as_str().unwrap().to_string();

    let (st, body) = server.req("GET", public, &service, None);
    assert_eq!(st, 200, "a token that may read may read the count: {body}");
    let (st, body) = server.req("PUT", public, &service, None);
    assert_eq!(st, 401, "a service token starred a repository: {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("person"),
        "the refusal should say why, in words somebody can act on: {body}"
    );
    let (st, body) = server.req("DELETE", public, &service, None);
    assert_eq!(st, 401, "a service token unstarred a repository: {body}");

    // The count is exactly where it was, and the server still serves.
    assert_eq!(server.req("GET", public, "", None).1["stars"], 1);
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A bare origin `git fetch` can reach, under a `file://` git base.
fn make_origin(root: &Path, full_name: &str) -> String {
    let work = root.join("work").join(full_name.replace('/', "-"));
    let tip = gitcli::fixture_repo(&work, 2);
    let bare = root.join(format!("{full_name}.git"));
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    gitcli::git(
        work.parent().unwrap(),
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    gitcli::git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    tip
}

/// The case the feature exists for: a mirrored project's real
/// reputation shown beside our own honest count, and never inside it.
///
/// This is the argument the gap audit made against building stars at
/// all — that "4" next to a project's real 116k makes our own UI say a
/// migrated project is dead. Honest provenance is the answer to it, so
/// the assertion that matters is not that 60,300 appears anywhere: it
/// is that our number stays 1 while it does, in a *different field*.
#[test]
fn a_mirror_shows_the_origins_count_beside_ours_and_never_inside_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("stars-e2e-mirror");
    let scratch = Scratch::new("stars-mirror");
    let mail = Mailbox::temp("stars-mirror");
    let origins = scratch.path().join("origins");
    make_origin(&origins, "acme/widget");
    make_origin(&origins, "stars-none/widget");

    let gh = fake_github::spawn();
    let key_path = scratch.path().join("app-key.pem");
    std::fs::write(&key_path, fake_github::TEST_APP_KEY_PEM).unwrap();

    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("stars_mirror")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_GITHUB_APP_ID", "12345")
        .env("STRATUM_GITHUB_APP_KEY_PEM", key_path.display().to_string())
        .env("STRATUM_GITHUB_API_BASE", gh.base_url.clone())
        .env(
            "STRATUM_GITHUB_GIT_BASE",
            format!("file://{}", origins.display()),
        );
    for (k, v) in mail.env() {
        b = b.env(k, v);
    }
    let server = b.start();

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/ada/mirrors",
        Some(serde_json::json!({
            "name": "widget",
            "provider": "github",
            "origin": "acme/widget",
            "public": true,
        })),
    );
    assert_eq!(st, 202, "{out}");

    let path = "/v1/orgs/ada/repos/widget/star";
    let (st, body) = ada.req("GET", path, None);
    assert_eq!(st, 200, "{body}");

    // Ours, on the day it was imported: nobody here has starred it.
    assert_eq!(body["stars"], 0, "an import moved our own count: {body}");
    // Theirs, in its own field, with the provenance to check it.
    assert_eq!(
        body["origin"]["stars"], 60_300,
        "the origin's count was not imported: {body}"
    );
    assert!(
        body["origin"]["at"].as_i64().unwrap_or(0) > 0,
        "an imported count with no date quietly becomes a lie as the mirror ages: {body}"
    );

    // Now star it here, and watch the two numbers stay apart. This is
    // the whole rule in one assertion: 1 and 60300, never 60301.
    let (st, body) = ada.req("PUT", path, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["stars"], 1);
    assert_eq!(body["origin"]["stars"], 60_300);
    assert_ne!(
        body["stars"].as_i64(),
        Some(60_301),
        "the counts were summed — the one outcome this feature exists to prevent: {body}"
    );

    // An origin that sends no count at all leaves `origin` null rather
    // than zero. "We never found out" and "nobody starred it there" are
    // different facts, and a confident 0 under a mirror is the second
    // one asserted without evidence.
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/ada/mirrors",
        Some(serde_json::json!({
            "name": "quiet-meta",
            "provider": "github",
            "origin": "stars-none/widget",
            "public": true,
        })),
    );
    assert_eq!(st, 202, "{out}");
    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/quiet-meta/star", None);
    assert_eq!(st, 200, "{body}");
    assert!(
        body["origin"].is_null(),
        "an origin that reported no count produced one anyway: {body}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The **signed-in outsider**, which is a different caller from the
/// signed-out one and a different path through the guard.
///
/// `a_stranger_is_told_the_count_and_a_service_token_cannot_move_it`
/// covers the anonymous case, and it answers 401 to everything — so it
/// proves masking for a caller who was going to be refused whatever they
/// asked for. The interesting caller is the one who *is* authenticated
/// and simply has no business here: a person with a real account, in
/// another namespace, who can be told apart from a stranger by every
/// layer that looks. That is the caller a guard written as "is there a
/// session?" lets straight through, and no test here had one.
///
/// Two halves, and both are the feature:
///
/// * a public project of somebody else's org is **starrable** — that is
///   what stars are *for*, and a forge where only members can star their
///   own projects has a counter that measures nothing;
/// * a private one is indistinguishable from one that does not exist,
///   for reads and for writes alike.
///
/// The masking is asserted as a comparison rather than a literal status,
/// for the reason the anonymous test gives: pinning the number turns a
/// deliberate change red and leaves the real bug — the two answers
/// diverging — green.
#[test]
fn a_signed_in_outsider_may_star_what_is_public_and_cannot_see_what_is_not() {
    let minio = Minio::shared();
    let bucket = minio.bucket("stars-outsider");
    let scratch = Scratch::new("stars-outsider");
    let mail = Mailbox::temp("stars-outsider");
    let server = spawn(&bucket.base_url, &scratch, "stars_outsider", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    for (name, public) in [("widget", true), ("ledger", false)] {
        let (st, body) = ada.req(
            "POST",
            "/v1/orgs/ada/repos",
            Some(serde_json::json!({ "name": name, "public": public })),
        );
        assert_eq!(st, 201, "{body}");
    }

    // Zoe has an account and nothing to do with ada.
    let mut zoe = signup(&server, &mail, "zoe", "zoe@example.com");

    // The public one: she can read the count and add to it.
    let public = "/v1/orgs/ada/repos/widget/star";
    let (st, body) = zoe.req("GET", public, None);
    assert_eq!(st, 200, "an outsider was refused a public count: {body}");
    assert_eq!(body["starred"], false, "{body}");
    let (st, body) = zoe.req("PUT", public, None);
    assert_eq!(
        st, 200,
        "an outsider could not star a public project: {body}"
    );
    assert_eq!(body["stars"], 1, "{body}");
    assert_eq!(body["starred"], true, "{body}");

    // Starring twice is one star, not two. A counter that a held-down
    // key can inflate is not a counter.
    let (_, body) = zoe.req("PUT", public, None);
    assert_eq!(body["stars"], 1, "a repeated star counted twice: {body}");

    // And it is *hers*: ada, who owns the repo, has not starred it.
    let (_, body) = ada.req("GET", public, None);
    assert_eq!(body["stars"], 1, "{body}");
    assert_eq!(
        body["starred"], false,
        "somebody else's star was reported as the owner's own: {body}"
    );

    // Unstarring something she never starred is not an error and moves
    // nothing — the idempotence a double-click depends on.
    let (st, body) = ada.req("DELETE", public, None);
    assert!(st == 200 || st == 204, "{st} {body}");
    let (_, body) = zoe.req("GET", public, None);
    assert_eq!(
        body["stars"], 1,
        "unstarring on behalf of somebody who had not starred moved the count: {body}"
    );

    // The private one: masked from her exactly as a missing repository
    // is, on every verb the route answers. A signed-in caller is the one
    // who can tell the difference if anybody can.
    for verb in ["GET", "PUT", "DELETE"] {
        let (private, _) = zoe.req(verb, "/v1/orgs/ada/repos/ledger/star", None);
        let (absent, _) = zoe.req(verb, "/v1/orgs/ada/repos/no-such-repo/star", None);
        assert_eq!(
            private, absent,
            "{verb}: a signed-in outsider can tell a private repository \
             from a missing one — private answered {private}, missing \
             answered {absent}"
        );
        assert_ne!(
            private, 200,
            "{verb}: a private repository answered an outsider successfully"
        );
    }

    // The private repository's count is untouched by any of that, read
    // by somebody who may actually read it.
    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/ledger/star", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["stars"], 0, "a refused star reached the row: {body}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
