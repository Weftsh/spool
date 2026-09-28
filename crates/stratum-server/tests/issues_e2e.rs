//! Issues, end to end against a real server.
//!
//! The rule this suite exists to hold is the one that makes an issue
//! tracker useful on open source at all: **filing takes `repo:read`,
//! not `repo:write`.** Every other write in the API is gated on write
//! access, because every other write changes the repository. An issue
//! does not. Gated on write, the tracker would be available to exactly
//! the people who do not need it and closed to everybody it exists for.
//!
//! Every repository is private to its organization, so the reader who
//! files is a **viewer** of it: a member who may read and may not push.
//! The load-bearing test is deliberately that awkward one — a person
//! with no write role and no push credential filing on somebody else's
//! repository — for the same reason `forks_e2e` makes its contributor a
//! viewer. A test written the obvious way has the repository's owner
//! file the issue, and they can do anything, so it passes against a
//! guard that would refuse every real reporter.
//!
//! Three negatives carry as much weight as the happy path, and each is
//! a different question:
//!
//! - somebody with no role in the org can neither read the tracker nor
//!   learn that it exists, and nobody signed out is told anything but to
//!   sign in;
//! - a service token may not author anything, because an issue has an
//!   author and a token is not a person;
//! - an unverified account may not file, because this is the most
//!   spammable create path on the server and the only one that would
//!   otherwise skip the check every sibling makes.
//!
//! And one that is about triage rather than authorship: **labels take
//! `repo:write`.** A reporter describes their problem; they do not get
//! to sort the maintainer's backlog.

use stratum_testkit::browser::Browser;
use stratum_testkit::gitcli::Scratch;
use stratum_testkit::mailbox::Mailbox;
use stratum_testkit::{Minio, Server};

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
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
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

/// Sign up and confirm the address.
fn signup<'a>(server: &'a Server, mail: &Mailbox, handle: &str, email: &str) -> Browser<'a> {
    let mut b = signup_unverified(server, handle, email);
    let token = mailed_token(mail, email, "verify");
    let (st, body) = b.req(
        "POST",
        "/v1/auth/verify",
        Some(serde_json::json!({ "token": token })),
    );
    assert_eq!(st, 200, "verify {handle}: {body}");
    b
}

/// Sign up and stop, leaving the address unconfirmed.
///
/// Its own helper rather than a flag, because the unverified state is a
/// thing this suite tests rather than a step it skips.
fn signup_unverified<'a>(server: &'a Server, handle: &str, email: &str) -> Browser<'a> {
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
    let mut b = Browser::new(server);
    // Signing in is what gives the browser its session; verification is
    // a separate act and is deliberately not performed here.
    let (st, body) = b.req(
        "POST",
        "/v1/auth/login",
        Some(serde_json::json!({ "email": email, "password": PASSWORD })),
    );
    assert_eq!(st, 200, "login {handle}: {body}");
    b
}

/// A repository in `org`, ready to be reported against.
fn repo(owner: &mut Browser, org: &str, name: &str) {
    let (st, body) = owner.req(
        "POST",
        &format!("/v1/orgs/{org}/repos"),
        Some(serde_json::json!({ "name": name })),
    );
    assert_eq!(st, 201, "create repo: {body}");
}

/// ada, who owns the organization `acme` and its repository `widget`.
fn acme<'a>(server: &'a Server, mail: &Mailbox) -> Browser<'a> {
    let mut ada = signup(server, mail, "ada", "ada@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs",
        Some(serde_json::json!({ "name": "acme" })),
    );
    assert_eq!(st, 201, "create org: {body}");
    repo(&mut ada, "acme", "widget");
    ada
}

/// A signed-up person invited into acme as a viewer: they may read
/// `widget` and may not push to it.
fn viewer<'a>(
    server: &'a Server,
    mail: &Mailbox,
    ada: &mut Browser,
    handle: &str,
    email: &str,
) -> Browser<'a> {
    let b = signup(server, mail, handle, email);
    ada.invite_and_accept("acme", email, "viewer");
    b
}

/// Was `an_outsider_can_file_and_comment_on_a_public_repository`. There
/// is no public repository for an outsider to read; the reporter is the
/// least authority that can read one — a viewer — and the outsider is
/// the negative at the end.
#[test]
fn a_reader_can_file_and_comment_on_a_repository_they_cannot_write() {
    let minio = Minio::shared();
    let bucket = minio.bucket("issues-outsider");
    let scratch = Scratch::new("issues-outsider");
    let mail = Mailbox::temp("issues-outsider");
    let server = spawn(&bucket.base_url, &scratch, "issues-outsider", &mail);

    let mut ada = acme(&server, &mail);
    // A viewer of acme, holding no grant on the repository and no write
    // role anywhere. This is the whole test.
    let mut bob = viewer(&server, &mail, &mut ada, "bob", "bob@example.com");
    let (st, view) = bob.req("GET", "/v1/orgs/acme/repos/widget", None);
    assert_eq!(st, 200, "{view}");
    assert_eq!(view["viewer_write"], false, "{view}");

    let (st, filed) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/issues",
        Some(serde_json::json!({
            "title": "Crashes on an empty config",
            "body": "Running with no config file panics rather than defaulting.",
        })),
    );
    assert_eq!(
        st, 201,
        "a reader could not file on a repository they cannot write — this \
         is the feature, not an edge case: {filed}"
    );
    assert_eq!(filed["number"], 1, "{filed}");
    assert_eq!(filed["state"], "open", "{filed}");
    assert_eq!(filed["author"], "bob", "{filed}");
    assert_eq!(filed["comment_count"], 0, "{filed}");

    // And can carry on the conversation, which is the same act and
    // therefore the same scope.
    let (st, c) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/issues/1/comments",
        Some(serde_json::json!({ "body": "Reproduced on 0.4.2 as well." })),
    );
    assert_eq!(st, 201, "{c}");
    assert_eq!(c["author"], "bob", "{c}");

    let (st, listed) = ada.req("GET", "/v1/orgs/acme/repos/widget/issues", None);
    assert_eq!(st, 200, "{listed}");
    assert_eq!(listed["counts"]["open"], 1, "{listed}");
    assert_eq!(listed["counts"]["closed"], 0, "{listed}");
    assert_eq!(listed["issues"][0]["comment_count"], 1, "{listed}");

    // The issue on its own, and the conversation on it. The index and
    // the detail page are different reads and the second one had no
    // test at all — the count above says a comment exists, which is not
    // the same as being able to read it back.
    let (st, one) = ada.req("GET", "/v1/orgs/acme/repos/widget/issues/1", None);
    assert_eq!(st, 200, "{one}");
    assert_eq!(one["number"], 1, "{one}");
    assert_eq!(one["title"], "Crashes on an empty config", "{one}");
    assert_eq!(
        one["body"],
        "Running with no config file panics rather than defaulting."
    );
    assert_eq!(one["author"], "bob", "{one}");

    let (st, thread) = ada.req("GET", "/v1/orgs/acme/repos/widget/issues/1/comments", None);
    assert_eq!(st, 200, "{thread}");
    let bodies: Vec<&str> = thread["comments"]
        .as_array()
        .expect("comments")
        .iter()
        .map(|c| c["body"].as_str().expect("body"))
        .collect();
    assert_eq!(bodies, vec!["Reproduced on 0.4.2 as well."], "{thread}");
    assert_eq!(thread["comments"][0]["author"], "bob", "{thread}");

    // A number nobody used is 404 rather than an empty issue.
    let (st, missing) = ada.req("GET", "/v1/orgs/acme/repos/widget/issues/9999", None);
    assert_eq!(st, 404, "{missing}");
    let (st, missing) = ada.req(
        "GET",
        "/v1/orgs/acme/repos/widget/issues/9999/comments",
        None,
    );
    assert_eq!(st, 404, "{missing}");

    // Somebody with no role in acme can neither file nor read, and is
    // told what a name nobody took would tell them.
    let mut carl = signup(&server, &mail, "carl", "carl@example.com");
    for repo in ["widget", "no-such"] {
        let (st, _) = carl.req(
            "POST",
            &format!("/v1/orgs/acme/repos/{repo}/issues"),
            Some(serde_json::json!({ "title": "from outside" })),
        );
        assert_eq!(st, 404, "a non-member filed on acme/{repo}");
        let (st, _) = carl.req("GET", &format!("/v1/orgs/acme/repos/{repo}/issues/1"), None);
        assert_eq!(st, 404, "a non-member read acme/{repo}'s issue");
    }
    let (st, listed) = ada.req("GET", "/v1/orgs/acme/repos/widget/issues", None);
    assert_eq!(st, 200, "{listed}");
    assert_eq!(
        listed["counts"]["open"], 1,
        "a refused filing landed: {listed}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn numbers_are_allocated_once_each_under_concurrent_filing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("issues-numbers");
    let scratch = Scratch::new("issues-numbers");
    let mail = Mailbox::temp("issues-numbers");
    let server = spawn(&bucket.base_url, &scratch, "issues-numbers", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    repo(&mut ada, "ada", "widget");

    // Ten in a row rather than two: a read-then-write allocator can win
    // a race twice by luck, and a `UNIQUE (repo_id, number)` violation
    // shows up as a 400 or a 500 rather than a duplicate, so the
    // assertion is on the *set* of numbers and not just their count.
    let mut numbers = Vec::new();
    for i in 0..10 {
        let (st, body) = ada.req(
            "POST",
            "/v1/orgs/ada/repos/widget/issues",
            Some(serde_json::json!({ "title": format!("issue {i}") })),
        );
        assert_eq!(st, 201, "{body}");
        numbers.push(body["number"].as_i64().expect("number"));
    }
    let mut sorted = numbers.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted,
        (1..=10).collect::<Vec<i64>>(),
        "issue numbers were not 1..=10 exactly once each: {numbers:?}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// Was `a_stranger_reads_a_public_tracker_and_cannot_write_to_it`. A
/// stranger reads nothing now, so the test is the new contract in the
/// same spirit: nobody outside acme can read the tracker, write to it,
/// or tell it from a tracker that does not exist — and a viewer, who
/// may, reads it.
#[test]
fn a_stranger_cannot_learn_a_tracker_exists_and_a_reader_reads_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("issues-stranger");
    let scratch = Scratch::new("issues-stranger");
    let mail = Mailbox::temp("issues-stranger");
    let server = spawn(&bucket.base_url, &scratch, "issues-stranger", &mail);

    let mut ada = acme(&server, &mail);
    let (st, _) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/widget/issues",
        Some(serde_json::json!({ "title": "a real bug" })),
    );
    assert_eq!(st, 201);
    let mut bob = viewer(&server, &mail, &mut ada, "bob", "bob@example.com");
    let mut carl = signup(&server, &mail, "carl", "carl@example.com");

    // Every read and the write, against the real tracker and one that
    // was never there: no credential is told to sign in, a person with
    // no role is told it does not exist, and the two names answer alike
    // for each of them.
    for (method, suffix, body) in [
        ("GET", "/issues", None),
        ("GET", "/issues/1", None),
        ("GET", "/issues/1/comments", None),
        ("GET", "/labels", None),
        (
            "POST",
            "/issues",
            Some(serde_json::json!({ "title": "from nobody" })),
        ),
    ] {
        let real = format!("/v1/orgs/acme/repos/widget{suffix}");
        let absent = format!("/v1/orgs/acme/repos/no-such{suffix}");
        let (st, out) = server.req(method, &real, "", body.clone());
        assert_eq!(st, 401, "anonymous {method} {suffix}: {out}");
        assert_eq!(server.req(method, &absent, "", body.clone()).0, st);
        let (st, out) = carl.req(method, &real, body.clone());
        assert_eq!(st, 404, "a non-member {method} {suffix}: {out}");
        assert_eq!(carl.req(method, &absent, body.clone()).0, st);
    }

    // The reader reads it, and nothing above was filed.
    let (st, listed) = bob.req("GET", "/v1/orgs/acme/repos/widget/issues", None);
    assert_eq!(st, 200, "a viewer could not read the tracker: {listed}");
    assert_eq!(listed["issues"][0]["title"], "a real bug", "{listed}");
    assert_eq!(
        listed["counts"]["open"], 1,
        "a refused filing landed: {listed}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_service_token_cannot_author_an_issue() {
    let minio = Minio::shared();
    let bucket = minio.bucket("issues-token");
    let scratch = Scratch::new("issues-token");
    let mail = Mailbox::temp("issues-token");
    let server = spawn(&bucket.base_url, &scratch, "issues-token", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    repo(&mut ada, "ada", "widget");
    // A repository-bound token: the most authority a deploy credential
    // ever has, and still not a person.
    let (st, minted) = ada.req(
        "POST",
        "/v1/orgs/ada/tokens",
        Some(serde_json::json!({
            "scopes": ["repo:read", "repo:write"],
            "repo": "widget",
            "label": "ci",
        })),
    );
    assert_eq!(st, 201, "{minted}");
    let token = minted["token"].as_str().expect("token");

    let (st, refused) = server.req(
        "POST",
        "/v1/orgs/ada/repos/widget/issues",
        token,
        Some(serde_json::json!({ "title": "filed by a robot" })),
    );
    assert_eq!(
        st, 401,
        "a repo-bound service token authored an issue: {refused}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn an_unconfirmed_address_cannot_file_or_comment() {
    let minio = Minio::shared();
    let bucket = minio.bucket("issues-unverified");
    let scratch = Scratch::new("issues-unverified");
    let mail = Mailbox::temp("issues-unverified");
    let server = spawn(&bucket.base_url, &scratch, "issues-unverified", &mail);

    let mut ada = acme(&server, &mail);
    let (st, _) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/widget/issues",
        Some(serde_json::json!({ "title": "a real one" })),
    );
    assert_eq!(st, 201);

    // Signed in, able to read, and not confirmed. Filing is the most
    // spammable create path on the server — reachable by every reader of
    // a repository — and every sibling create path checks this. Invited
    // rather than left outside, so the refusal below cannot be the read
    // gate: accepting an invitation into an existing account proves
    // nothing about its address.
    let mut mallory = signup_unverified(&server, "mallory", "mallory@example.com");
    ada.invite_and_accept("acme", "mallory@example.com", "viewer");
    let (st, me) = mallory.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    assert!(
        me["verified_at"].is_null(),
        "the fixture is verified, so this proves nothing: {me}"
    );
    let (st, refused) = mallory.req(
        "POST",
        "/v1/orgs/acme/repos/widget/issues",
        Some(serde_json::json!({ "title": "buy cheap watches" })),
    );
    assert_eq!(st, 403, "an unconfirmed account filed an issue: {refused}");
    let (st, refused) = mallory.req(
        "POST",
        "/v1/orgs/acme/repos/widget/issues/1/comments",
        Some(serde_json::json!({ "body": "buy cheap watches" })),
    );
    assert_eq!(st, 403, "an unconfirmed account commented: {refused}");

    // Reading is not gated on it, and must not be: the check is about
    // creating things, not about being trusted to look.
    let (st, listed) = mallory.req("GET", "/v1/orgs/acme/repos/widget/issues", None);
    assert_eq!(st, 200, "{listed}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// Was `an_author_may_close_their_own_issue_and_a_stranger_may_not`: the
/// passer-by who may read an issue is a fellow viewer now.
#[test]
fn an_author_may_close_their_own_issue_and_another_reader_may_not() {
    let minio = Minio::shared();
    let bucket = minio.bucket("issues-close");
    let scratch = Scratch::new("issues-close");
    let mail = Mailbox::temp("issues-close");
    let server = spawn(&bucket.base_url, &scratch, "issues-close", &mail);

    let mut ada = acme(&server, &mail);
    let mut bob = viewer(&server, &mail, &mut ada, "bob", "bob@example.com");
    let mut carol = viewer(&server, &mail, &mut ada, "carol", "carol@example.com");

    let (st, _) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/issues",
        Some(serde_json::json!({ "title": "my own mistake" })),
    );
    assert_eq!(st, 201);

    // Carol has read access, as every viewer does, and no claim on this
    // issue.
    let (st, refused) = carol.req(
        "PATCH",
        "/v1/orgs/acme/repos/widget/issues/1",
        Some(serde_json::json!({ "state": "closed" })),
    );
    assert_eq!(
        st, 403,
        "somebody else's issue was closed by a passer-by: {refused}"
    );

    // Bob worked out it was his own mistake. Making him wait for a
    // maintainer to agree is the attention cost this product exists to
    // refuse.
    let (st, closed) = bob.req(
        "PATCH",
        "/v1/orgs/acme/repos/widget/issues/1",
        Some(serde_json::json!({ "state": "closed" })),
    );
    assert_eq!(
        st, 200,
        "an author could not close their own issue: {closed}"
    );
    assert_eq!(closed["state"], "closed", "{closed}");

    // And the maintainer can reopen it, which is the write-access half.
    let (st, reopened) = ada.req(
        "PATCH",
        "/v1/orgs/acme/repos/widget/issues/1",
        Some(serde_json::json!({ "state": "open" })),
    );
    assert_eq!(st, 200, "{reopened}");
    assert_eq!(reopened["state"], "open", "{reopened}");

    // Editing the text is a different authority from closing and goes
    // down a different arm of the same handler. Bob wrote it, so Bob
    // may fix his own title without a maintainer's help.
    let (st, edited) = bob.req(
        "PATCH",
        "/v1/orgs/acme/repos/widget/issues/1",
        Some(serde_json::json!({ "title": "my own mistake (sorry)" })),
    );
    assert_eq!(st, 200, "{edited}");
    assert_eq!(edited["title"], "my own mistake (sorry)", "{edited}");
    assert_eq!(
        edited["state"], "open",
        "editing the title changed the state: {edited}"
    );

    // A bound is a bound on this door too: refused, not truncated.
    let (st, refused) = bob.req(
        "PATCH",
        "/v1/orgs/acme/repos/widget/issues/1",
        Some(serde_json::json!({ "title": "x".repeat(401) })),
    );
    assert_eq!(
        st, 400,
        "an over-long title was accepted on edit: {refused}"
    );

    // A number nobody used is 404 rather than a silent no-op.
    let (st, missing) = ada.req(
        "PATCH",
        "/v1/orgs/acme/repos/widget/issues/9999",
        Some(serde_json::json!({ "state": "closed" })),
    );
    assert_eq!(st, 404, "{missing}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn labelling_is_a_maintainers_act() {
    let minio = Minio::shared();
    let bucket = minio.bucket("issues-labels");
    let scratch = Scratch::new("issues-labels");
    let mail = Mailbox::temp("issues-labels");
    let server = spawn(&bucket.base_url, &scratch, "issues-labels", &mail);

    let mut ada = acme(&server, &mail);
    let mut bob = viewer(&server, &mail, &mut ada, "bob", "bob@example.com");

    let (st, made) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/widget/labels",
        Some(serde_json::json!({
            "name": "good first issue",
            "color": "series-1",
            "description": "A gentle way in",
        })),
    );
    assert_eq!(st, 201, "{made}");

    // A hex is refused, and the refusal names what would have worked.
    // A stored hex is unfixable later: change the palette and it is
    // silently wrong in both themes with no migration able to know what
    // was meant.
    let (st, refused) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/widget/labels",
        Some(serde_json::json!({ "name": "bug", "color": "#ff0000" })),
    );
    assert_eq!(st, 400, "a hex colour was stored: {refused}");
    let message = refused["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("series-1"),
        "the refusal did not name the valid set, so it costs the caller \
         a round trip to find out: {message}"
    );

    let (st, _) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/issues",
        Some(serde_json::json!({ "title": "a bug bob found" })),
    );
    assert_eq!(st, 201);

    // Bob may report. He may not sort ada's backlog.
    let (st, refused) = bob.req(
        "PUT",
        "/v1/orgs/acme/repos/widget/issues/1/labels",
        Some(serde_json::json!({ "labels": ["good first issue"] })),
    );
    assert_eq!(
        st, 403,
        "a reporter relabelled a maintainer's issue: {refused}"
    );

    let (st, labelled) = ada.req(
        "PUT",
        "/v1/orgs/acme/repos/widget/issues/1/labels",
        Some(serde_json::json!({ "labels": ["good first issue"] })),
    );
    assert_eq!(st, 200, "{labelled}");

    // The label list itself, which the index reads to build its filter
    // menu. Readable by anybody who may read the repository — a reader
    // deciding whether to file needs to see that "good first issue"
    // exists — and by nobody who may not.
    let (st, listed) = bob.req("GET", "/v1/orgs/acme/repos/widget/labels", None);
    assert_eq!(st, 200, "{listed}");
    let (st, _) = server.req("GET", "/v1/orgs/acme/repos/widget/labels", "", None);
    assert_eq!(st, 401, "an anonymous caller read the labels");
    assert_eq!(listed["labels"][0]["name"], "good first issue", "{listed}");
    assert_eq!(listed["labels"][0]["color"], "series-1", "{listed}");
    assert_eq!(
        labelled["labels"][0]["name"], "good first issue",
        "{labelled}"
    );
    assert_eq!(labelled["labels"][0]["color"], "series-1", "{labelled}");

    // Deleting a label takes it off every issue carrying it, and a
    // reporter cannot do that either.
    let (st, refused) = bob.req(
        "DELETE",
        "/v1/orgs/acme/repos/widget/labels/good%20first%20issue",
        None,
    );
    assert_eq!(
        st, 404,
        "a reporter deleted a maintainer's label: {refused}"
    );
    let (st, _) = ada.req(
        "DELETE",
        "/v1/orgs/acme/repos/widget/labels/good%20first%20issue",
        None,
    );
    assert_eq!(st, 204);
    let (st, after) = ada.req("GET", "/v1/orgs/acme/repos/widget/issues/1", None);
    assert_eq!(st, 200, "{after}");
    assert!(
        after["labels"].as_array().expect("labels").is_empty(),
        "a deleted label is still on the issue that carried it: {after}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn the_refusals_on_every_write_door_say_which_one_was_hit() {
    // The gate found these: every one is a refusal arm the suite above
    // walked past, and each is something a person actually does — a
    // typo in a state, an empty comment, a label that is not there.
    // "The happy path works" is not the same claim as "the refusals
    // refuse", and only the first had tests.
    let minio = Minio::shared();
    let bucket = minio.bucket("issues-refusals");
    let scratch = Scratch::new("issues-refusals");
    let mail = Mailbox::temp("issues-refusals");
    let server = spawn(&bucket.base_url, &scratch, "issues-refusals", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    repo(&mut ada, "ada", "widget");
    let (st, _) = ada.req(
        "POST",
        "/v1/orgs/ada/repos/widget/issues",
        Some(serde_json::json!({ "title": "a real issue" })),
    );
    assert_eq!(st, 201);

    // A state that is not a state. Refused rather than stored, because
    // `issues.state` has a CHECK and a 500 from a constraint is a worse
    // answer than a 400 naming the problem.
    let (st, refused) = ada.req(
        "PATCH",
        "/v1/orgs/ada/repos/widget/issues/1",
        Some(serde_json::json!({ "state": "banana" })),
    );
    assert_eq!(st, 400, "an unknown state was accepted: {refused}");

    // Anonymous is stopped at the read gate now — there is nothing it
    // may read — and told to sign in.
    let (st, refused) = server.req(
        "PATCH",
        "/v1/orgs/ada/repos/widget/issues/1",
        "",
        Some(serde_json::json!({ "state": "closed" })),
    );
    assert_eq!(st, 401, "an anonymous caller edited an issue: {refused}");
    // Past the read gate, and neither the author nor a writer, and not a
    // person at all: a read-only token bound to the repository. This is
    // the `_ => false` arm of the authorship check, which an anonymous
    // script used to reach on a public repository and an automation
    // holding a read credential reaches now.
    let (st, minted) = ada.req(
        "POST",
        "/v1/orgs/ada/tokens",
        Some(serde_json::json!({ "scopes": ["repo:read"], "repo": "widget", "label": "ro" })),
    );
    assert_eq!(st, 201, "{minted}");
    let (st, refused) = server.req(
        "PATCH",
        "/v1/orgs/ada/repos/widget/issues/1",
        minted["token"].as_str().expect("token"),
        Some(serde_json::json!({ "state": "closed" })),
    );
    assert_eq!(st, 403, "a read-only token edited an issue: {refused}");
    let (st, still) = ada.req("GET", "/v1/orgs/ada/repos/widget/issues/1", None);
    assert_eq!(st, 200, "{still}");
    assert_eq!(
        still["state"], "open",
        "a refused edit landed anyway: {still}"
    );

    // An empty comment is not a comment. Whitespace especially: it
    // renders as a blank row nobody can read and nobody meant to send.
    let (st, refused) = ada.req(
        "POST",
        "/v1/orgs/ada/repos/widget/issues/1/comments",
        Some(serde_json::json!({ "body": "   " })),
    );
    assert_eq!(st, 400, "an empty comment was stored: {refused}");

    // Commenting on an issue that does not exist.
    let (st, refused) = ada.req(
        "POST",
        "/v1/orgs/ada/repos/widget/issues/9999/comments",
        Some(serde_json::json!({ "body": "hello?" })),
    );
    assert_eq!(st, 404, "{refused}");

    // A label nobody defined. Refused, so a typo does not silently
    // clear the labels it failed to apply — the API is PUT, so a
    // partial success would be a removal.
    let (st, refused) = ada.req(
        "PUT",
        "/v1/orgs/ada/repos/widget/issues/1/labels",
        Some(serde_json::json!({ "labels": ["no-such-label"] })),
    );
    assert_eq!(st, 400, "an undefined label was applied: {refused}");

    // Deleting a label that is not there.
    let (st, refused) = ada.req("DELETE", "/v1/orgs/ada/repos/widget/labels/ghost", None);
    assert_eq!(st, 400, "{refused}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_private_repositorys_tracker_is_masked_from_outsiders() {
    let minio = Minio::shared();
    let bucket = minio.bucket("issues-private");
    let scratch = Scratch::new("issues-private");
    let mail = Mailbox::temp("issues-private");
    let server = spawn(&bucket.base_url, &scratch, "issues-private", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "secret", "public": false })),
    );
    assert_eq!(st, 201, "{body}");
    let (st, _) = ada.req(
        "POST",
        "/v1/orgs/ada/repos/secret/issues",
        Some(serde_json::json!({ "title": "an internal bug" })),
    );
    assert_eq!(st, 201);

    // Anonymous is challenged; an authenticated outsider is told the
    // repository does not exist. Both come from `repo_or_masked`, and
    // the point of asserting them here is that the tracker did not
    // invent a third answer of its own.
    let (st, _) = server.req("GET", "/v1/orgs/ada/repos/secret/issues", "", None);
    assert_eq!(st, 401, "a private tracker answered a stranger");
    let (st, masked) = bob.req("GET", "/v1/orgs/ada/repos/secret/issues", None);
    assert_eq!(
        st, 404,
        "a private tracker confirmed its repository's existence: {masked}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn sorting_is_two_real_orders_and_a_named_refusal() {
    let minio = Minio::shared();
    let bucket = minio.bucket("issues-sort");
    let scratch = Scratch::new("issues-sort");
    let mail = Mailbox::temp("issues-sort");
    let server = spawn(&bucket.base_url, &scratch, "issues-sort", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    repo(&mut ada, "ada", "widget");
    for i in 1..=3 {
        let (st, body) = ada.req(
            "POST",
            "/v1/orgs/ada/repos/widget/issues",
            Some(serde_json::json!({ "title": format!("issue {i}") })),
        );
        assert_eq!(st, 201, "{body}");
    }

    // Default and `newest` are the same order, and it is newest-first.
    for query in ["", "?sort=newest"] {
        let (st, body) = ada.req(
            "GET",
            &format!("/v1/orgs/ada/repos/widget/issues{query}"),
            None,
        );
        assert_eq!(st, 200, "{body}");
        let got: Vec<i64> = body["issues"]
            .as_array()
            .expect("issues")
            .iter()
            .map(|i| i["number"].as_i64().expect("number"))
            .collect();
        assert_eq!(got, vec![3, 2, 1], "sort={query:?}: {body}");
    }

    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/widget/issues?sort=oldest", None);
    assert_eq!(st, 200, "{body}");
    let got: Vec<i64> = body["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .map(|i| i["number"].as_i64().expect("number"))
        .collect();
    assert_eq!(got, vec![1, 2, 3], "{body}");

    // `updated` is a real ordering we have not built, and it is refused
    // by name rather than silently answered in a different order. A sort
    // that quietly ignores the sort is worse than one that refuses: the
    // caller believes they got what they asked for.
    let (st, refused) = ada.req("GET", "/v1/orgs/ada/repos/widget/issues?sort=updated", None);
    assert_eq!(st, 400, "an unbuilt sort was silently answered: {refused}");
    let message = refused["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("compound cursor"),
        "the refusal did not say why, so it reads as a typo rather than a \
         missing feature: {message}"
    );

    // And a typo is refused differently, naming what would have worked.
    let (st, refused) = ada.req("GET", "/v1/orgs/ada/repos/widget/issues?sort=nweest", None);
    assert_eq!(st, 400, "{refused}");
    let message = refused["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("newest") && message.contains("oldest"),
        "the refusal did not name the valid sorts: {message}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn oversized_input_is_refused_rather_than_truncated() {
    let minio = Minio::shared();
    let bucket = minio.bucket("issues-bounds");
    let scratch = Scratch::new("issues-bounds");
    let mail = Mailbox::temp("issues-bounds");
    let server = spawn(&bucket.base_url, &scratch, "issues-bounds", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    repo(&mut ada, "ada", "widget");

    // I13. Truncating would store something the author did not write
    // and never told them so — a silent edit of somebody's words, which
    // is worse than a refusal they can act on.
    let (st, refused) = ada.req(
        "POST",
        "/v1/orgs/ada/repos/widget/issues",
        Some(serde_json::json!({ "title": "x".repeat(401) })),
    );
    assert_eq!(st, 400, "an over-long title was accepted: {refused}");

    let (st, ok) = ada.req(
        "POST",
        "/v1/orgs/ada/repos/widget/issues",
        Some(serde_json::json!({ "title": "x".repeat(400) })),
    );
    assert_eq!(st, 201, "the limit itself was refused: {ok}");
    assert_eq!(
        ok["title"].as_str().map(str::len),
        Some(400),
        "the title came back a different length than it went in: {ok}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
