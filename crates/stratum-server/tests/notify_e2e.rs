//! Notifications, end to end against a real server and a real mailbox.
//!
//! The property under test is not "a mail was sent". It is **who gets
//! one and who does not**, because both halves of that are the feature.
//! A forge that mails everybody about everything trains people to filter
//! it out, and then the one change that needed a human sits in the queue
//! behind the noise. A forge that mails nobody is the state we were in
//! before this existed: a change sits in the land queue until somebody
//! happens to look.
//!
//! So every assertion here reads the capture directory, and the ones
//! that matter most are the absences.

use std::time::Duration;
use stratum_testkit::browser::Browser;
use stratum_testkit::mailbox::Mailbox;
use stratum_testkit::{gitcli::Scratch, Minio, Server};

const PASSWORD: &str = "a long enough password";
const SOON: Duration = Duration::from_secs(10);

fn spawn(store_url: &str, scratch: &Scratch, hint: &str, mail: &Mailbox) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_PUBLIC_URL", "http://stratum.test:9999")
        // Poll fast: the point of a notification is timeliness, and a
        // test that waits five seconds per event to prove it teaches
        // nobody anything.
        .env("STRATUM_NOTIFY_POLL_SECS", "1")
        // The lander, so a change can actually land in a test that is
        // about being told it did.
        .env("STRATUM_LAND_POLL_SECS", "1");
    for (k, v) in mail.env() {
        b = b.env(k, v);
    }
    b.start()
}

fn member(server: &Server, org: &str, email: &str, name: &str, role: &str) {
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            org,
            "--email",
            email,
            "--name",
            name,
            "--password",
            PASSWORD,
            "--role",
            role,
        ])
        .unwrap_or_else(|e| panic!("user-create {email}: {e}"));
}

/// Nobody is told what they just did, and everybody involved is.
#[test]
fn a_comment_reaches_the_author_and_never_the_person_who_wrote_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("notify-basic");
    let scratch = Scratch::new("notify-basic");
    let mailbox = Mailbox::temp("notify-e2e");
    let server = spawn(&bucket.base_url, &scratch, "notify-e2e", &mailbox);
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");

    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    let mut bo = Browser::signed_in(&server, "bo@acme.test", PASSWORD);

    let (st, _) = ada.req(
        "POST",
        "/v1/orgs/acme/repos",
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201);
    let (st, _) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/commits",
        Some(serde_json::json!({
            "message": "seed",
            "operations": [{"op": "put", "path": "readme", "content": "hello\n"}],
        })),
    );
    assert_eq!(st, 201);

    // Ada opens a change; she is its author.
    let (st, made) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/commits",
        Some(serde_json::json!({
            "message": "a change\n\nChange-Id: I0000000000000000000000000000000000000001\n",
            "branch": "review/one",
            "operations": [{"op": "put", "path": "readme", "content": "hello again\n"}],
        })),
    );
    assert_eq!(st, 201, "{made}");
    let (st, change) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/changes",
        Some(serde_json::json!({ "from": "review/one" })),
    );
    assert!(st == 201 || st == 200, "{change}");
    let key = change["change"]["key"]
        .as_str()
        .expect("change key")
        .to_string();

    // Bo comments. Ada is the author, so Ada hears about it.
    let (st, out) = bo.req(
        "POST",
        &format!("/v1/orgs/acme/repos/app/changes/{key}/comments"),
        Some(serde_json::json!({ "body": "one question about this" })),
    );
    assert_eq!(st, 201, "{out}");

    let got = mailbox.wait_for("ada@acme.test", SOON);
    assert!(
        got.text.contains("commented on"),
        "the author was told something, but not what: {}",
        got.text
    );
    assert!(
        got.subject.contains("acme/app"),
        "the subject does not say which repository, which is what a \
         maintainer filters on: {}",
        got.subject
    );

    // ...and Bo, who wrote it, is not told about his own comment. This
    // is the line that decides whether people keep notifications on.
    assert!(
        mailbox.to("bo@acme.test").is_empty(),
        "the commenter was mailed about his own comment: {:?}",
        mailbox
            .to("bo@acme.test")
            .iter()
            .map(|m| m.subject.clone())
            .collect::<Vec<_>>()
    );
}

/// Saying "never" means never, whatever else would have put you on the
/// list.
#[test]
fn somebody_ignoring_a_repository_is_not_mailed_and_the_default_needs_no_choice() {
    let minio = Minio::shared();
    let bucket = minio.bucket("notify-ignore");
    let scratch = Scratch::new("notify-ignore");
    let mailbox = Mailbox::temp("notify-ignore");
    let server = spawn(&bucket.base_url, &scratch, "notify-ignore", &mailbox);
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "cy@acme.test", "Cy", "member");

    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    let mut cy = Browser::signed_in(&server, "cy@acme.test", PASSWORD);

    // The default is a real answer nobody had to choose.
    let (st, w) = cy.req("GET", "/v1/orgs/acme/repos", None);
    assert_eq!(st, 200, "{w}");

    ada.req(
        "POST",
        "/v1/orgs/acme/repos",
        Some(serde_json::json!({ "name": "app" })),
    );
    let (st, w) = cy.req("GET", "/v1/orgs/acme/repos/app/watch", None);
    assert_eq!(st, 200, "{w}");
    assert_eq!(
        w["level"], "participating",
        "the default was not participating: {w}"
    );

    // Cy opts out.
    let (st, w) = cy.req(
        "PUT",
        "/v1/orgs/acme/repos/app/watch",
        Some(serde_json::json!({ "level": "ignore" })),
    );
    assert_eq!(st, 200, "{w}");
    assert_eq!(w["level"], "ignore", "{w}");

    // Cy comments — which would ordinarily make him a participant on
    // everything after — and then Ada replies.
    ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/commits",
        Some(serde_json::json!({
            "message": "seed",
            "operations": [{"op": "put", "path": "readme", "content": "hello\n"}],
        })),
    );
    ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/commits",
        Some(serde_json::json!({
            "message": "a change\n\nChange-Id: I0000000000000000000000000000000000000002\n",
            "branch": "review/two",
            "operations": [{"op": "put", "path": "readme", "content": "hello again\n"}],
        })),
    );
    let (_, change) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/changes",
        Some(serde_json::json!({ "from": "review/two" })),
    );
    let key = change["change"]["key"]
        .as_str()
        .expect("change key")
        .to_string();
    cy.req(
        "POST",
        &format!("/v1/orgs/acme/repos/app/changes/{key}/comments"),
        Some(serde_json::json!({ "body": "a note" })),
    );
    ada.req(
        "POST",
        &format!("/v1/orgs/acme/repos/app/changes/{key}/comments"),
        Some(serde_json::json!({ "body": "a reply" })),
    );

    // Ada's reply would reach Cy on any other setting: he commented, so
    // he is a participant. He said never, and never wins.
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        mailbox.to("cy@acme.test").is_empty(),
        "somebody who said never was mailed anyway: {:?}",
        mailbox
            .to("cy@acme.test")
            .iter()
            .map(|m| m.subject.clone())
            .collect::<Vec<_>>()
    );
}

/// A service token has no mailbox, so it has no opinion to record.
#[test]
fn watching_is_a_persons_setting_and_a_token_is_refused() {
    let minio = Minio::shared();
    let bucket = minio.bucket("notify-token");
    let scratch = Scratch::new("notify-token");
    let mailbox = Mailbox::temp("notify-token");
    let server = spawn(&bucket.base_url, &scratch, "notify-token", &mailbox);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );

    let (st, out) = server.req("GET", "/v1/orgs/acme/repos/app/watch", &admin, None);
    assert_eq!(
        st, 401,
        "a service token was given somebody's notification settings: {out}"
    );
}

/// A reader may watch a repository, and somebody with no role in its
/// organization cannot even learn it is there to be watched.
///
/// Was `an_outsider_can_watch_a_public_repository`, which pinned a real
/// bug: the identity was read off the principal `rest_repo_auth`
/// returns, which answers "what authority has this caller in *this
/// org*", and it refused the most ordinary outsider action there was.
/// There is no outsider who may read a repository any more, so the
/// person here is the least authority that can — a viewer, who may not
/// push — and the outsider is the negative: `…/watch` answers them
/// exactly as a repository that does not exist does, for a read and for
/// a write, or the watch door would be an existence oracle.
#[test]
fn a_reader_can_watch_and_a_stranger_cannot_learn_the_repository_exists() {
    let minio = Minio::shared();
    let bucket = minio.bucket("notify-outsider");
    let scratch = Scratch::new("notify-outsider");
    let mailbox = Mailbox::temp("notify-outsider");
    let server = spawn(&bucket.base_url, &scratch, "notify-outsider", &mailbox);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "open" })),
    );
    assert_eq!(st, 201, "{out}");

    // A viewer: reads, may not push.
    member(&server, "acme", "val@acme.test", "Val", "viewer");
    let mut val = Browser::signed_in(&server, "val@acme.test", PASSWORD);
    let (st, out) = val.req("GET", "/v1/orgs/acme/repos/open/watch", None);
    assert_eq!(st, 200, "a reader was refused the watch: {out}");
    assert_eq!(out["level"], "participating", "{out}");
    let (st, out) = val.req(
        "PUT",
        "/v1/orgs/acme/repos/open/watch",
        Some(serde_json::json!({ "level": "all" })),
    );
    assert_eq!(st, 200, "a reader could not subscribe: {out}");
    assert_eq!(out["level"], "all", "{out}");

    // A person with an account and no role anywhere near acme.
    server.bootstrap_org("elsewhere");
    member(&server, "elsewhere", "zoe@elsewhere.test", "Zoe", "owner");
    let mut zoe = Browser::signed_in(&server, "zoe@elsewhere.test", PASSWORD);
    for repo in ["open", "never-was"] {
        let path = format!("/v1/orgs/acme/repos/{repo}/watch");
        let (st, out) = zoe.req("GET", &path, None);
        assert_eq!(st, 404, "an outsider read acme/{repo}'s watch: {out}");
        let (st, out) = zoe.req("PUT", &path, Some(serde_json::json!({ "level": "all" })));
        assert_eq!(st, 404, "an outsider subscribed to acme/{repo}: {out}");
        let (st, out) = server.req("GET", &path, "", None);
        assert_eq!(
            st, 401,
            "an anonymous caller read acme/{repo}'s watch: {out}"
        );
    }
    // The refused subscription was not recorded: val is the one watcher.
    let (st, view) = server.req("GET", "/v1/orgs/acme/repos/open", &admin, None);
    assert_eq!(st, 200, "{view}");
    assert_eq!(view["watcher_count"], 1, "{view}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The watcher count is on the repository view, and it counts the right
/// thing.
///
/// Two properties, and the second is the one that bit. **Where**: the
/// count rides on the repository view beside the fork count rather than
/// on `…/watch`, which answers "what did *you* choose". A masthead
/// reading it from there had no number to draw until the person's own
/// setting had loaded and then grew one, which changed the width of the
/// identity row after paint on every repository page. The view is read
/// by exactly the people who may read the repository — the count is not
/// a way round that.
///
/// **The right thing**: only `level = 'all'`. The default subscription
/// is stored as *no row at all*, so `count(*)` over `repo_watches` is
/// "people who changed their mind about the default in either
/// direction" — a number that goes **up** when somebody asks to be left
/// alone. That is the assertion below that a naive implementation
/// fails.
///
/// Was `the_watcher_count_is_public_and_counts_only_people_who_asked_for_everything`.
#[test]
fn the_watcher_count_is_on_the_view_and_counts_only_people_who_asked_for_everything() {
    let minio = Minio::shared();
    let bucket = minio.bucket("notify-watchers");
    let scratch = Scratch::new("notify-watchers");
    let mailbox = Mailbox::temp("notify-watchers");
    let server = spawn(&bucket.base_url, &scratch, "notify-watchers", &mailbox);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "open" })),
    );
    assert_eq!(st, 201, "{out}");

    let view = "/v1/orgs/acme/repos/open";
    let count = || -> serde_json::Value {
        let (st, body) = server.req("GET", view, &admin, None);
        assert_eq!(st, 200, "{body}");
        body["watcher_count"].clone()
    };
    assert_eq!(
        count(),
        0,
        "the count was absent or wrong before anybody watched"
    );
    // Nobody signed out reads it, since nobody signed out reads the
    // repository.
    let (st, body) = server.req("GET", view, "", None);
    assert_eq!(st, 401, "an anonymous caller read the view: {body}");

    member(&server, "acme", "zoe@acme.test", "Zoe", "viewer");
    member(&server, "acme", "rex@acme.test", "Rex", "member");
    let mut zoe = Browser::signed_in(&server, "zoe@acme.test", PASSWORD);
    let mut rex = Browser::signed_in(&server, "rex@acme.test", PASSWORD);

    let (st, out) = zoe.req(
        "PUT",
        "/v1/orgs/acme/repos/open/watch",
        Some(serde_json::json!({ "level": "all" })),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(count(), 1, "somebody watching everything was not counted");
    // The same number for the watcher, a reader like any other.
    assert_eq!(zoe.req("GET", view, None).1["watcher_count"], 1);

    // Rex asks to be left alone. That writes a row — and it must move
    // the count *down*, or rather leave it exactly where it was.
    let (st, out) = rex.req(
        "PUT",
        "/v1/orgs/acme/repos/open/watch",
        Some(serde_json::json!({ "level": "ignore" })),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        count(),
        1,
        "somebody who chose Ignore was published as a watcher"
    );

    // And back to the default, which stores no row: the count Zoe left
    // behind is still hers alone.
    zoe.req(
        "PUT",
        "/v1/orgs/acme/repos/open/watch",
        Some(serde_json::json!({ "level": "participating" })),
    );
    assert_eq!(count(), 0, "unsubscribing left the count where it was");

    // The server is still serving.
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// **Watching a project you do not work on, and hearing about a change
/// you had nothing to do with.**
///
/// Three features meet here and every one of them is tested alone:
/// `a_reader_can_watch_and_a_stranger_cannot_learn_the_repository_exists`
/// proves a reader may subscribe, `fork_pr_e2e` proves a reader's change
/// lands, and the tests above prove a notification reaches the people on
/// a change. What none of them covers is the join — and the join is the
/// entire point of watching a project. Somebody who is neither the
/// author, nor a reviewer, nor able to push asked to hear about this
/// repository, and a landing is exactly the event they asked for.
///
/// It is the case a participation-based notifier gets wrong by
/// construction: every other mail in this file goes to somebody with a
/// *relationship* to the change — its author, a commenter, an approver.
/// A watcher has none. If the recipient list is built from the change
/// rather than from the repository, this person is silently left off and
/// nothing fails: the change lands, the author is told, and the watcher
/// concludes the project is dead.
///
/// The absence is asserted too, because it is the half that keeps
/// notifications worth having: somebody who explicitly said *ignore*
/// hears nothing, on the same event, in the same run.
#[test]
fn a_watcher_hears_about_a_landing_they_had_nothing_to_do_with() {
    let minio = Minio::shared();
    let bucket = minio.bucket("notify-watchland");
    let scratch = Scratch::new("notify-watchland");
    let mailbox = Mailbox::temp("notify-watchland");
    let server = spawn(&bucket.base_url, &scratch, "notify-watchland", &mailbox);
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);

    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/repos",
        Some(serde_json::json!({ "name": "open" })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/open/commits",
        Some(serde_json::json!({
            "message": "seed",
            "operations": [{"op": "put", "path": "readme", "content": "hello\n"}],
        })),
    );
    assert_eq!(st, 201, "{out}");

    // Two readers, who may read the repository and push to nothing —
    // the closest there is to somebody following a project from outside
    // it. Zoe wants everything; Quinn has said to be left alone.
    member(&server, "acme", "zoe@acme.test", "Zoe", "viewer");
    member(&server, "acme", "quinn@acme.test", "Quinn", "viewer");
    let mut zoe = Browser::signed_in(&server, "zoe@acme.test", PASSWORD);
    let mut quinn = Browser::signed_in(&server, "quinn@acme.test", PASSWORD);

    let (st, out) = zoe.req(
        "PUT",
        "/v1/orgs/acme/repos/open/watch",
        Some(serde_json::json!({ "level": "all" })),
    );
    assert_eq!(st, 200, "a reader could not subscribe: {out}");
    let (st, out) = quinn.req(
        "PUT",
        "/v1/orgs/acme/repos/open/watch",
        Some(serde_json::json!({ "level": "ignore" })),
    );
    assert_eq!(st, 200, "{out}");

    // Ada opens a change and lands it. Zoe and Quinn are not involved
    // in it in any way — not author, not reviewer, and unable to push.
    // Branch from trunk first: a commit on a new branch with no base is
    // a root commit, and a root commit is correctly not a fast-forward
    // of anything — a real refusal, and not the one this test is about.
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/open/branches",
        Some(serde_json::json!({ "name": "review/tidy", "from": "main" })),
    );
    assert!(st == 200 || st == 201, "{out}");
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/open/commits",
        Some(serde_json::json!({
            "message": "tidy the readme\n\nChange-Id: I00000000000000000000000000000000000000a1\n",
            "branch": "review/tidy",
            "operations": [{"op": "put", "path": "readme", "content": "hello, world\n"}],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, change) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/open/changes",
        Some(serde_json::json!({ "from": "review/tidy" })),
    );
    assert!(st == 201 || st == 200, "{change}");
    let key = change["change"]["key"].as_str().expect("key").to_string();

    let (st, out) = ada.req(
        "POST",
        &format!("/v1/orgs/acme/repos/open/changes/{key}/approve"),
        None,
    );
    assert_eq!(st, 204, "{out}");
    let (st, out) = ada.req(
        "POST",
        &format!("/v1/orgs/acme/repos/open/changes/{key}/land"),
        None,
    );
    assert_eq!(st, 202, "{out}");

    // It landed…
    let path = format!("/v1/orgs/acme/repos/open/changes/{key}");
    let mut landed = serde_json::Value::Null;
    for _ in 0..150 {
        let (st, out) = ada.req("GET", &path, None);
        assert_eq!(st, 200, "{out}");
        if out["change"]["state"] != serde_json::json!("landing") {
            landed = out;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(landed["change"]["state"], "landed", "{landed}");

    // …and the watcher heard about it, by name of repository, which is
    // what somebody following several projects filters on.
    //
    // Waited for **by content**, not by "the first mail Zoe gets".
    // `level: all` means all: she is told about the approval too, and it
    // is sent first, so `wait_for` returned that one and this assertion
    // failed against a notifier that was working correctly. Waiting on
    // the observable this test is actually about is the fix — the same
    // rule the repository already writes down about polling a mock's
    // call count instead of the thing it feeds.
    let deadline = std::time::Instant::now() + SOON;
    let landing = loop {
        let found = mailbox
            .to("zoe@acme.test")
            .into_iter()
            .find(|m| m.subject.contains("landed") || m.text.contains("landed"));
        if let Some(m) = found {
            break m;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the watcher was never told the change landed; they got {:?}",
            mailbox
                .to("zoe@acme.test")
                .iter()
                .map(|m| m.subject.clone())
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(
        landing.subject.contains("acme/open"),
        "the mail does not say which repository: {}",
        landing.subject
    );

    // And the person who asked to be left alone was left alone.
    assert!(
        mailbox.to("quinn@acme.test").is_empty(),
        "somebody who said 'ignore' was mailed anyway: {:?}",
        mailbox
            .to("quinn@acme.test")
            .iter()
            .map(|m| m.subject.clone())
            .collect::<Vec<_>>()
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// Every worker survives a database that will not answer its claim, and
/// the notifier picks up where it left off when it does.
///
/// The worker's loop asks for a job every second, and the arm that
/// handles a *failed* ask — as opposed to "no work" — is three lines
/// nothing deterministic reached: it needs Postgres to refuse the claim
/// query itself, which the surrounding request path never does. It was
/// carried in the coverage ledger, and then one run covered it by
/// accident — a suite tearing its cluster down while the loop happened
/// to be mid-poll. An arm covered by luck is an arm nobody has tested:
/// what it does is sleep and go round again, and getting that wrong
/// gives you either a worker that exits on the first hiccup and stops
/// mailing anybody until the process restarts, or one that spins on the
/// error at full speed and takes the database down with it.
///
/// So the refusal is made on purpose — the `jobs` table is renamed out
/// from under the loops — and the two things that matter are asserted:
/// the server is still serving while nothing can claim, and once the
/// table is back a comment reaches its author with nothing restarted.
///
/// Every worker on purpose, not the notifier alone. They all share the
/// shape and they would all share the bug, and a table that is not there
/// is refused to all of them at once — so the test that pins one is the
/// test that pins the class, provided each is actually polling inside
/// the window. That is what the poll settings below are for: at the
/// default five seconds a four-second outage is a race, and a coverage
/// number won by a race is the accidental cover this test exists to
/// replace.
#[test]
fn no_worker_that_cannot_claim_takes_the_server_with_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("notify-claim-fails");
    let scratch = Scratch::new("notify-claim-fails");
    let mailbox = Mailbox::temp("notify-claim-fails");
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("notify-claimfail")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_PUBLIC_URL", "http://stratum.test:9999");
    // Every queue-driven worker, polling inside the outage below.
    for k in [
        "STRATUM_NOTIFY_POLL_SECS",
        "STRATUM_LAND_POLL_SECS",
        "STRATUM_COMPACT_POLL_SECS",
        "STRATUM_CDNPACK_POLL_SECS",
        "STRATUM_CONTRIB_POLL_SECS",
        "STRATUM_FORK_POLL_SECS",
        "STRATUM_IMPORT_POLL_SECS",
        "STRATUM_PROMOTE_POLL_SECS",
        "STRATUM_RUNNER_POLL_SECS",
        "STRATUM_CHECKS_POLL_SECS",
    ] {
        b = b.env(k, "1");
    }
    for (k, v) in mailbox.env() {
        b = b.env(k, v);
    }
    let server = b.start();
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");

    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    let mut bo = Browser::signed_in(&server, "bo@acme.test", PASSWORD);
    let (st, _) = ada.req(
        "POST",
        "/v1/orgs/acme/repos",
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201);
    let (st, _) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/commits",
        Some(serde_json::json!({
            "message": "seed",
            "operations": [{"op": "put", "path": "readme", "content": "hello\n"}],
        })),
    );
    assert_eq!(st, 201);
    let (st, made) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/commits",
        Some(serde_json::json!({
            "message": "a change\n\nChange-Id: I0000000000000000000000000000000000000001\n",
            "branch": "review/one",
            "operations": [{"op": "put", "path": "readme", "content": "hello again\n"}],
        })),
    );
    assert_eq!(st, 201, "{made}");
    let (st, change) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/changes",
        Some(serde_json::json!({ "from": "review/one" })),
    );
    assert!(st == 201 || st == 200, "{change}");
    let key = change["change"]["key"]
        .as_str()
        .expect("change key")
        .to_string();

    // The queues themselves go away — the background jobs table, and the
    // workflow jobs the runner claims from, which is a different table
    // and so a different worker's copy of the same arm. Every claim now
    // fails on the query and not on the connection, which is the case
    // the arm is for: the database is answering, and answering "no such
    // table".
    let mut db =
        postgres::Client::connect(&server.db_url, postgres::NoTls).expect("the control plane");
    for t in ["jobs", "workflow_jobs"] {
        db.execute(&format!("ALTER TABLE {t} RENAME TO {t}_hidden"), &[])
            .unwrap_or_else(|e| panic!("rename {t} away: {e}"));
    }
    // Several polls' worth at a one-second poll, so every loop has met
    // the refusal and gone round again rather than merely been caught
    // between two sleeps.
    std::thread::sleep(Duration::from_secs(6));
    assert!(
        server.healthy(),
        "a worker that cannot claim took the server with it"
    );
    // And it is still serving reads, not merely answering the probe.
    let (st, _) = ada.req(
        "GET",
        &format!("/v1/orgs/acme/repos/app/changes/{key}"),
        None,
    );
    assert_eq!(st, 200, "the API stopped while the queue was unreadable");

    // Now the other two halves of a worker's tick, and the order here is
    // the whole trick. `changes` goes away *before* the queues come
    // back, so for the whole of the next window the claims succeed and
    // the work fails:
    //
    //   * the lander reaps stranded landings only on an idle tick — a
    //     claim that succeeded and found nothing — which is why taking
    //     the queue away above cannot reach that read at all;
    //   * the notification queued by the change above has been sitting
    //     unclaimable since the queue vanished, so it is claimed the
    //     moment the queue returns and fails on the missing table.
    //
    // Left to chance both of those are races — a claim landing on one
    // side of a rename and its work on the other — and both showed up as
    // exactly that: the reap's error arm covered on one CI run and not
    // another, the notifier's covered here and not there, each reported
    // by the coverage gate as a ledger entry that was stale on one
    // machine only.
    db.execute("ALTER TABLE changes RENAME TO changes_hidden", &[])
        .expect("rename changes away");
    for t in ["jobs", "workflow_jobs"] {
        db.execute(&format!("ALTER TABLE {t}_hidden RENAME TO {t}"), &[])
            .unwrap_or_else(|e| panic!("put {t} back: {e}"));
    }
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        server.healthy(),
        "a worker whose work failed after a good claim took the server with it"
    );
    db.execute("ALTER TABLE changes_hidden RENAME TO changes", &[])
        .expect("put changes back");

    // Nothing was restarted. The same loop claims the comment's
    // notification and Ada is told about it.
    let (st, out) = bo.req(
        "POST",
        &format!("/v1/orgs/acme/repos/app/changes/{key}/comments"),
        Some(serde_json::json!({ "body": "one question about this" })),
    );
    assert_eq!(st, 201, "{out}");
    let got = mailbox.wait_for("ada@acme.test", SOON);
    assert!(
        got.text.contains("commented on"),
        "the notifier did not come back after the database did: {}",
        got.text
    );
}

/// A mail transport that is refusing costs the notification and nothing
/// else — this worker's copy of the arm `changeset_notify_e2e` pins for
/// the changeset notifier under the same name.
///
/// It was covered by accident before it was covered on purpose. That
/// suite takes its mailbox away to make the *changeset* transport
/// refuse, and this worker, polling in the same process, sometimes had
/// a change-opened mail in flight when the directory went: the push run
/// and the pull-request run of one sha disagreed about this line, and
/// the coverage gate reported a ledger entry stale on one of them only.
/// A refusal that happens by timing is one nobody has tested, so it is
/// made here, in order: a send that works, the directory gone, a send
/// that fails and is reported as `sent 0` on a job that is *done* — a
/// failed job is never retried, so the failure escaping the send would
/// lose the mail for everybody on the list — and then the directory
/// back and the next mail out of the same process.
#[test]
fn a_mail_transport_that_refuses_costs_the_notification_and_not_the_worker() {
    let minio = Minio::shared();
    let bucket = minio.bucket("notify-nomail");
    let scratch = Scratch::new("notify-nomail");
    let mailbox = Mailbox::temp("notify-nomail");
    let server = spawn(&bucket.base_url, &scratch, "notify-nomail", &mailbox);
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");

    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    let mut bo = Browser::signed_in(&server, "bo@acme.test", PASSWORD);
    let (st, _) = ada.req(
        "POST",
        "/v1/orgs/acme/repos",
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201);
    let (st, _) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/commits",
        Some(serde_json::json!({
            "message": "seed",
            "operations": [{"op": "put", "path": "readme", "content": "hello\n"}],
        })),
    );
    assert_eq!(st, 201);
    let (st, made) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/commits",
        Some(serde_json::json!({
            "message": "a change\n\nChange-Id: I0000000000000000000000000000000000000001\n",
            "branch": "review/one",
            "operations": [{"op": "put", "path": "readme", "content": "hello again\n"}],
        })),
    );
    assert_eq!(st, 201, "{made}");
    let (st, change) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/app/changes",
        Some(serde_json::json!({ "from": "review/one" })),
    );
    assert!(st == 201 || st == 200, "{change}");
    let key = change["change"]["key"]
        .as_str()
        .expect("change key")
        .to_string();

    // Bo comments and Ada, the author, is told: the transport works, and
    // the queue is empty when the directory goes, so the next job is the
    // one that meets the refusal.
    let comment = |bo: &mut Browser<'_>, body: &str| {
        let (st, out) = bo.req(
            "POST",
            &format!("/v1/orgs/acme/repos/app/changes/{key}/comments"),
            Some(serde_json::json!({ "body": body })),
        );
        assert_eq!(st, 201, "{out}");
    };
    comment(&mut bo, "one question about this");
    mailbox.wait_for("ada@acme.test", SOON);

    // The transport starts refusing. Removing the directory rather than
    // making it unwritable on purpose: a suite that runs as root would
    // write into a read-only directory anyway, and a test that passes
    // for that reason is a test of nothing.
    std::fs::remove_dir_all(mailbox.dir()).expect("take the mailbox away");
    comment(&mut bo, "and another");
    let mut db =
        postgres::Client::connect(&server.db_url, postgres::NoTls).expect("the control plane");
    let (state, result, error) = last_notification(&mut db);
    assert_eq!(
        (state.as_str(), error.as_deref()),
        ("done", None),
        "a send that failed was reported as the job failing, and a failed \
         job is never retried"
    );
    assert_eq!(
        result.as_deref(),
        Some("sent 0"),
        "the count claims mail that was never sent"
    );
    assert!(
        mailbox.all().is_empty(),
        "the transport was not actually refusing: {:?}",
        mailbox
            .all()
            .iter()
            .map(|m| m.subject.clone())
            .collect::<Vec<_>>()
    );

    // ...and the next one, once it can send again, goes out from the
    // same process with nothing restarted.
    std::fs::create_dir_all(mailbox.dir()).expect("give the mailbox back");
    comment(&mut bo, "last one");
    let (state, result, _) = last_notification(&mut db);
    assert_eq!(state, "done", "{result:?}");
    assert_eq!(result.as_deref(), Some("sent 1"), "{result:?}");
    let got = mailbox.wait_for("ada@acme.test", SOON);
    assert!(got.text.contains("commented on"), "{}", got.text);

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// Wait for the newest `notify-mail` job to be done with, and answer
/// with its state, result and error.
fn last_notification(db: &mut postgres::Client) -> (String, Option<String>, Option<String>) {
    let deadline = std::time::Instant::now() + SOON;
    loop {
        let row = db
            .query_one(
                "SELECT state, result, error FROM jobs WHERE kind = 'notify-mail' \
                 ORDER BY created_at DESC LIMIT 1",
                &[],
            )
            .expect("read the newest notification job");
        let state: String = row.get(0);
        if state != "queued" && state != "running" {
            return (state, row.get(1), row.get(2));
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the notification job never finished: {state}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
