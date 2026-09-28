//! Changeset notifications, end to end against a real server and a real
//! mailbox.
//!
//! A review tool where the reviewer is never told they are the reviewer
//! is a review tool nobody uses twice, and that is precisely what a
//! changeset was: composing one review across four repositories told
//! nobody at all, while the single-repository path kept working and kept
//! every mailbox looking alive.
//!
//! So the assertions here are about **who**, and the ones that matter
//! most are the absences: the person who composed it is not mailed about
//! their own request, and a person who cannot read one member repository
//! is not mailed at all — a changeset mail names its members, so sending
//! one to somebody outside a private member publishes that repository's
//! existence.

use std::time::Duration;
use stratum_testkit::browser::Browser;
use stratum_testkit::mailbox::{CapturedMail, Mailbox};
use stratum_testkit::{gitcli::Scratch, FaultProxy, Minio, Server};

const PASSWORD: &str = "a long enough password";
const SOON: Duration = Duration::from_secs(20);

/// A server with both notifiers polling fast, a mailbox, and a dashboard
/// on disk.
///
/// The dashboard is not decoration: the mail carries one link, and the
/// last assertion in `a_landed_changeset_...` asks this server for
/// exactly the address that link contains. Without a
/// `STRATUM_DASHBOARD_DIR` every dashboard path 404s and the check would
/// pass or fail for a reason that has nothing to do with the mail.
fn spawn(store_url: &str, scratch: &Scratch, hint: &str, mail: &Mailbox) -> Server {
    spawn_with(store_url, scratch, hint, mail, &[])
}

/// The same, with anything in the environment overridden — a
/// `STRATUM_CHANGESET_NOTIFY_POLL_SECS` of `0` is how a test asks for a
/// node that does not run this worker.
fn spawn_with(
    store_url: &str,
    scratch: &Scratch,
    hint: &str,
    mail: &Mailbox,
    extra: &[(&str, &str)],
) -> Server {
    let dash = scratch.path().join("dashboard");
    std::fs::create_dir_all(&dash).expect("dashboard dir");
    std::fs::write(
        dash.join("index.html"),
        "<!doctype html><title>dash</title>",
    )
    .expect("dashboard index");
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_PUBLIC_URL", "http://stratum.test:9999")
        .env("STRATUM_DASHBOARD_DIR", dash.display().to_string())
        // The point of a notification is timeliness; a test that waits
        // five seconds per event teaches nobody anything.
        .env("STRATUM_CHANGESET_NOTIFY_POLL_SECS", "1")
        .env("STRATUM_NOTIFY_POLL_SECS", "1")
        .env("STRATUM_LAND_POLL_SECS", "1");
    for (k, v) in mail.env() {
        b = b.env(k, v);
    }
    // Last, so a test can override anything above it.
    for (k, v) in extra {
        b = b.env(k, *v);
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

/// Every mail this address received *about a changeset*.
///
/// Filtered rather than counted whole, because the same run also sends
/// ordinary per-change notifications — "Ada opened a change" — to the
/// same people. Counting everything would make this suite pass or fail
/// on the other notifier's behaviour.
fn changeset_mails(mailbox: &Mailbox, addr: &str) -> Vec<CapturedMail> {
    mailbox
        .to(addr)
        .into_iter()
        .filter(|m| m.subject.contains("changeset"))
        .collect()
}

fn wait_for_changeset_mail(mailbox: &Mailbox, addr: &str, needle: &str) -> CapturedMail {
    let deadline = std::time::Instant::now() + SOON;
    loop {
        if let Some(m) = changeset_mails(mailbox, addr)
            .into_iter()
            .find(|m| m.subject.contains(needle))
        {
            return m;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{addr} was never told {needle:?}; they got {:?}",
            mailbox
                .to(addr)
                .iter()
                .map(|m| m.subject.clone())
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The same wait, over *every* mail rather than the changeset ones —
/// for the tests that prove the mail path works before asserting that a
/// changeset mail is missing.
fn wait_for_mail_about(mailbox: &Mailbox, addr: &str, needle: &str) -> CapturedMail {
    let deadline = std::time::Instant::now() + SOON;
    loop {
        if let Some(m) = mailbox
            .to(addr)
            .into_iter()
            .find(|m| m.subject.contains(needle))
        {
            return m;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{addr} was never told {needle:?}; they got {:?}",
            mailbox
                .to(addr)
                .iter()
                .map(|m| m.subject.clone())
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A repository with a root OWNERS naming `owner`, trunk, and one open
/// change on a feature branch. Returns the change key.
fn repo_with_change(ada: &mut Browser<'_>, repo: &str, owner: &str, change_id: &str) -> String {
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/repos",
        Some(serde_json::json!({ "name": repo })),
    );
    assert_eq!(st, 201, "create {repo}: {out}");
    let (st, out) = ada.req(
        "POST",
        &format!("/v1/orgs/acme/repos/{repo}/commits"),
        Some(serde_json::json!({
            "message": "seed",
            "operations": [
                {"op": "put", "path": "OWNERS", "content": format!("{owner}\n")},
                {"op": "put", "path": "readme", "content": "hello\n"},
            ],
        })),
    );
    assert_eq!(st, 201, "seed {repo}: {out}");
    // From trunk: a commit on a branch with no base is a root commit,
    // and a root commit is correctly not a fast-forward of anything.
    let (st, out) = ada.req(
        "POST",
        &format!("/v1/orgs/acme/repos/{repo}/branches"),
        Some(serde_json::json!({ "name": "feature", "from": "main" })),
    );
    assert!(st == 200 || st == 201, "branch in {repo}: {out}");
    let (st, out) = ada.req(
        "POST",
        &format!("/v1/orgs/acme/repos/{repo}/commits"),
        Some(serde_json::json!({
            "message": format!("work in {repo}\n\nChange-Id: {change_id}\n"),
            "branch": "feature",
            "operations": [{"op": "put", "path": "feature.txt", "content": "work\n"}],
        })),
    );
    assert_eq!(st, 201, "commit in {repo}: {out}");
    let (st, change) = ada.req(
        "POST",
        &format!("/v1/orgs/acme/repos/{repo}/changes"),
        Some(serde_json::json!({ "from": "feature" })),
    );
    assert!(st == 201 || st == 200, "open change in {repo}: {change}");
    change["change"]["key"]
        .as_str()
        .expect("change key")
        .to_string()
}

/// Composing a changeset asks its reviewers for the review — every
/// member's, once each — and never asks the person who composed it.
///
/// The union over members is the property a per-repository notifier
/// cannot express: Bo is named by `app`'s OWNERS and Cy by `lib`'s, and
/// both are reviewers of the *combination*, which is a thing neither of
/// their own repositories can tell them about. "Once each" is the other
/// half: two members means two chances to mail the same person twice.
#[test]
fn composing_a_changeset_tells_every_members_reviewers_once_and_never_the_composer() {
    let minio = Minio::shared();
    let bucket = minio.bucket("csnotify-compose");
    let scratch = Scratch::new("csnotify-compose");
    let mailbox = Mailbox::temp("csnotify-compose");
    let server = spawn(&bucket.base_url, &scratch, "csnotify-compose", &mailbox);
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");
    member(&server, "acme", "cy@acme.test", "Cy", "member");

    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    let app = repo_with_change(
        &mut ada,
        "app",
        "bo@acme.test",
        "I0000000000000000000000000000000000000a01",
    );
    let lib = repo_with_change(
        &mut ada,
        "lib",
        "cy@acme.test",
        "I0000000000000000000000000000000000000b01",
    );

    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/changesets",
        Some(serde_json::json!({
            "key": "CS-1",
            "title": "split the auth crate",
            "members": [
                {"repo": "app", "change": app},
                {"repo": "lib", "change": lib},
            ],
        })),
    );
    assert_eq!(st, 201, "compose: {out}");

    // Both reviewers are told, and the mail says what they are looking
    // at without their having to open it: the org, the key, the title,
    // how many repositories and which.
    for (addr, who) in [("bo@acme.test", "Bo"), ("cy@acme.test", "Cy")] {
        let m = wait_for_changeset_mail(&mailbox, addr, "composed changeset CS-1");
        assert!(
            m.subject.contains("[acme]") && m.subject.contains("split the auth crate"),
            "{who}'s subject does not say what this is: {}",
            m.subject
        );
        assert!(
            m.text.contains("over 2 repositories")
                && m.text.contains("acme/app")
                && m.text.contains("acme/lib"),
            "{who} was not told which repositories are in it: {}",
            m.text
        );
        assert!(
            m.text
                .contains("http://stratum.test:9999/dashboard/changesets/CS-1"),
            "{who}'s mail does not carry the changeset's address: {}",
            m.text
        );
    }

    // Once each. Two members is two chances to mail the same person
    // twice, and a notification that arrives in duplicate is the one
    // people write a filter for.
    for addr in ["bo@acme.test", "cy@acme.test"] {
        let composed: Vec<String> = changeset_mails(&mailbox, addr)
            .iter()
            .filter(|m| m.subject.contains("composed"))
            .map(|m| m.subject.clone())
            .collect();
        assert_eq!(
            composed.len(),
            1,
            "{addr} was told about one changeset more than once: {composed:?}"
        );
    }

    // And the composer is not told about her own request. She is the
    // author of both changes, so every other rule here puts her on the
    // list; this is the line that decides whether people keep
    // notifications on.
    assert!(
        changeset_mails(&mailbox, "ada@acme.test").is_empty(),
        "the person who composed the changeset was mailed about it: {:?}",
        changeset_mails(&mailbox, "ada@acme.test")
            .iter()
            .map(|m| m.subject.clone())
            .collect::<Vec<_>>()
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A changeset read needs read on **every** member, and so does a mail
/// about one.
///
/// Zoe has an account in another organization and was, until recently,
/// a viewer of acme: she commented on a change in `open`, which makes her
/// a participant by every other rule in this file. Then she left. The
/// changeset composed afterwards contains `open` and also `shut` — and
/// the mail names its members, so sending it would tell somebody who may
/// read nothing here that `shut` exists. `changesets_api::load` masks
/// exactly this on the wire; a notification is the same read by another
/// route.
///
/// This used to be an outsider commenting on a *public* member while a
/// private one sat beside it. There are no public repositories, and
/// every member of an organization reads every repository in it, so the
/// person who took part and may not read is the one who left.
#[test]
fn somebody_who_cannot_read_one_member_is_never_told_the_changeset_exists() {
    let minio = Minio::shared();
    let bucket = minio.bucket("csnotify-leak");
    let scratch = Scratch::new("csnotify-leak");
    let mailbox = Mailbox::temp("csnotify-leak");
    let server = spawn(&bucket.base_url, &scratch, "csnotify-leak", &mailbox);
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");
    server.bootstrap_org("elsewhere");
    member(&server, "elsewhere", "zoe@elsewhere.test", "Zoe", "owner");
    member(&server, "acme", "zoe@elsewhere.test", "Zoe", "viewer");

    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    let mut zoe = Browser::signed_in(&server, "zoe@elsewhere.test", PASSWORD);
    let open = repo_with_change(
        &mut ada,
        "open",
        "bo@acme.test",
        "I0000000000000000000000000000000000000c01",
    );
    let shut = repo_with_change(
        &mut ada,
        "shut",
        "bo@acme.test",
        "I0000000000000000000000000000000000000d01",
    );

    // A viewer taking part in the review, which is what a viewer is for.
    let (st, out) = zoe.req(
        "POST",
        &format!("/v1/orgs/acme/repos/open/changes/{open}/comments"),
        Some(serde_json::json!({ "body": "does this cover the retry path?" })),
    );
    assert_eq!(st, 201, "a viewer could not comment on a change: {out}");

    // And then she leaves, and reads nothing here any more — the wire
    // agrees before the notifier is asked to.
    let (st, me) = zoe.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    let zoe_id = me["id"].as_str().expect("zoe has an id").to_string();
    let (st, out) = ada.req("DELETE", &format!("/v1/orgs/acme/members/{zoe_id}"), None);
    assert_eq!(st, 204, "remove zoe: {out}");
    let (st, out) = zoe.req(
        "GET",
        &format!("/v1/orgs/acme/repos/open/changes/{open}"),
        None,
    );
    assert_eq!(st, 404, "a former member still reads the change: {out}");

    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/changesets",
        Some(serde_json::json!({
            "key": "CS-2",
            "title": "retry the retries",
            "members": [
                {"repo": "open", "change": open},
                {"repo": "shut", "change": shut},
            ],
        })),
    );
    assert_eq!(st, 201, "compose: {out}");

    // Bo can read both members, so he is told — which is what makes the
    // absence below mean something rather than meaning the worker never
    // ran.
    wait_for_changeset_mail(&mailbox, "bo@acme.test", "composed changeset CS-2");

    assert!(
        changeset_mails(&mailbox, "zoe@elsewhere.test").is_empty(),
        "somebody outside a private member was told a changeset over it \
         exists: {:?}",
        changeset_mails(&mailbox, "zoe@elsewhere.test")
            .iter()
            .map(|m| m.subject.clone())
            .collect::<Vec<_>>()
    );
    // Not by any route: the repository's *name* is the thing that leaks,
    // so no mail she has may carry it.
    for m in mailbox.to("zoe@elsewhere.test") {
        assert!(
            !m.text.contains("acme/shut") && !m.subject.contains("CS-2"),
            "a private member repository was named to an outsider: {} / {}",
            m.subject,
            m.text
        );
    }

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A landed changeset tells the people it was composed for, and the link
/// it carries is on the dashboard this server is serving.
///
/// **What the second half can and cannot prove.** The dashboard is an
/// SPA behind a catch-all, so *every* `/dashboard/…` path answers 200
/// with the same shell — `/dashboard/utter-nonsense` included. Fetching
/// the mailed URL therefore establishes that it is a dashboard address
/// on this server and that the server is configured to serve one; it
/// says nothing about whether the client router resolves it. That
/// distinction is exactly the defect FORGE-PARITY §4.1 records: a
/// notification URL that returned 200, served the shell, and then
/// rendered "We couldn't find changes".
///
/// The resolving half is a client-side test — a dashboard spec asserting
/// that this literal path renders the changeset view rather than
/// NotFound (`web/dashboard/tests/changesets.spec.ts`, once
/// `crossrepo-ui` releases those files). The two halves together are the
/// claim; neither is it alone.
/// OWNERS that cannot be read costs the reviewers, never the notification.
///
/// `required_reviewers` resolves each member's OWNERS out of the object
/// store, and a store that will not answer makes that resolution fail.
/// The worker logs it and carries on with the participants it already
/// has, because the alternative — failing the job — loses the mail for
/// everybody, including the author and the people who commented, over a
/// read that was only ever going to *add* names.
///
/// This arm had been covered by accident: the landing tests left this
/// worker running while they refused a manifest, so it hit the error path
/// on somebody else's fault. Silencing it there (it was miscounting their
/// faults) took the coverage with it, which is the argument for a
/// deterministic test rather than an incidental one.
#[test]
fn owners_that_cannot_be_read_costs_the_reviewers_and_not_the_notification() {
    let minio = Minio::shared();
    let bucket = minio.bucket("csnotify-owners");
    let upstream = bucket
        .base_url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let started = FaultProxy::start(&upstream);
    let bucket_name = bucket.base_url.rsplit('/').next().unwrap().to_string();
    let proxy = FaultProxy {
        url: format!("{}/{bucket_name}", started.url),
        handle: started.handle,
    };
    let scratch = Scratch::new("csnotify-owners");
    let mailbox = Mailbox::temp("csnotify-owners");
    let server = spawn(&proxy.url, &scratch, "csnotify-owners", &mailbox);
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");

    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    let change = repo_with_change(
        &mut ada,
        "app",
        "bo@acme.test",
        "I0000000000000000000000000000000000000f01",
    );

    // Bo is a **participant**, not an owner: Bo said something on the
    // change. That distinction is the whole test — with OWNERS
    // unreadable the people it names are correctly not told, so the only
    // way to prove the notification survived is somebody who is on the
    // list for a reason that does not go through the store.
    let mut bo = Browser::signed_in(&server, "bo@acme.test", PASSWORD);
    let (st, out) = bo.req(
        "POST",
        &format!("/v1/orgs/acme/repos/app/changes/{change}/comments"),
        Some(serde_json::json!({ "body": "worth a second look at the retry" })),
    );
    assert_eq!(st, 201, "bo could not comment: {out}");

    // The locator header is what `Plane::load` reads on the way to the
    // OWNERS blob, and nothing the compose request itself needs. Refusing
    // it makes the resolution fail and leaves everything else standing.
    proxy.handle.inject("locator.hdr", 1_000, 503);

    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/changesets",
        Some(serde_json::json!({
            "key": "owners-unreadable",
            "title": "OWNERS is unreadable",
            "members": [{"repo": "app", "change": change}],
        })),
    );
    assert_eq!(st, 201, "{out}");

    // Bo authored the change, so Bo is a participant and is told whatever
    // OWNERS says. The mail is the assertion: a read that could only have
    // added names must not be able to remove the notification.
    let mail = wait_for_mail_about(&mailbox, "bo@acme.test", "owners-unreadable");
    assert!(
        mail.subject.contains("owners-unreadable"),
        "the participant was not told: {}",
        mail.subject
    );

    proxy.handle.clear();
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_landed_changeset_mails_its_people_and_the_link_it_carries_resolves() {
    let minio = Minio::shared();
    let bucket = minio.bucket("csnotify-land");
    let scratch = Scratch::new("csnotify-land");
    let mailbox = Mailbox::temp("csnotify-land");
    let server = spawn(&bucket.base_url, &scratch, "csnotify-land", &mailbox);
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");

    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    let mut bo = Browser::signed_in(&server, "bo@acme.test", PASSWORD);
    let app = repo_with_change(
        &mut ada,
        "app",
        "bo@acme.test",
        "I0000000000000000000000000000000000000e01",
    );
    let lib = repo_with_change(
        &mut ada,
        "lib",
        "bo@acme.test",
        "I0000000000000000000000000000000000000f01",
    );
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/changesets",
        Some(serde_json::json!({
            "key": "CS-3",
            "title": "move the client",
            "members": [
                {"repo": "app", "change": app},
                {"repo": "lib", "change": lib},
            ],
        })),
    );
    assert_eq!(st, 201, "compose: {out}");

    // The owner both changes need approves both, and Ada lands the set.
    for (repo, key) in [("app", &app), ("lib", &lib)] {
        let (st, out) = bo.req(
            "POST",
            &format!("/v1/orgs/acme/repos/{repo}/changes/{key}/approve"),
            None,
        );
        assert_eq!(st, 204, "approve {repo}: {out}");
    }
    let (st, out) = ada.req("POST", "/v1/orgs/acme/changesets/CS-3/land", None);
    assert_eq!(st, 202, "land: {out}");

    let mut landed = serde_json::Value::Null;
    for _ in 0..150 {
        let (st, out) = ada.req("GET", "/v1/orgs/acme/changesets/CS-3", None);
        assert_eq!(st, 200, "{out}");
        if out["state"] != serde_json::json!("landing") {
            landed = out;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(landed["state"], "landed", "{landed}");

    // Everybody the review was addressed to hears how it ended — the
    // author included. A queue finished this, so there is nobody to
    // leave off the list, and "did my landing work?" is the one
    // notification the person who pressed Land actually wants.
    let m = wait_for_changeset_mail(&mailbox, "bo@acme.test", "landed changeset CS-3");
    wait_for_changeset_mail(&mailbox, "ada@acme.test", "landed changeset CS-3");
    assert!(
        m.text.contains("over 2 repositories"),
        "the landing mail does not say it was one unit: {}",
        m.text
    );

    // The one link, taken from the mail and asked of the server.
    let link = m
        .link()
        .unwrap_or_else(|| panic!("no link in the mail: {}", m.text));
    let path = link
        .strip_prefix("http://stratum.test:9999")
        .unwrap_or_else(|| panic!("the link is not on the configured public URL: {link}"));
    // The mailed address reaches the dashboard shell. A `/dashboard/…`
    // path that answered 404 would mean the mail points outside the SPA
    // altogether — at the forge, or at nothing — which is the failure
    // this catches. It does **not** catch a path the shell serves and
    // the client router then renders as NotFound: see the doc comment.
    let resp = ureq::get(&format!("{}{path}", server.base))
        .call()
        .unwrap_or_else(|e| panic!("the mailed address is not served at all: {e}"));
    assert_eq!(
        resp.status(),
        200,
        "the mail points outside the dashboard this server serves"
    );
    assert!(
        resp.into_string().unwrap_or_default().contains("<!doctype"),
        "the mailed address served something that is not the dashboard shell"
    );
    // The address itself, which is the half that is genuinely pinned
    // here: the forge has no `/{org}/changesets/{key}` yet, and when it
    // grows one this line is where the supersession is decided.
    assert_eq!(path, "/dashboard/changesets/CS-3", "{link}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The control plane, for the tests below that have to put something on
/// the queue the API cannot express, or take the queue away.
///
/// Everything above this line asks the product the way a person would.
/// These do not, and cannot: the states they are about — a job this
/// build cannot read, a changeset deleted between the enqueue and the
/// claim, a mail directory that has gone — are states no request can
/// reach, which is exactly why the arms that handle them had nothing
/// standing on them.
fn control(server: &Server) -> postgres::Client {
    postgres::Client::connect(&server.db_url, postgres::NoTls).expect("the control plane")
}

fn id_of(db: &mut postgres::Client, sql: &str, key: &str) -> String {
    db.query_one(sql, &[&key])
        .unwrap_or_else(|e| panic!("{sql} for {key}: {e}"))
        .get(0)
}

/// Put one `notify-changeset` job on the queue with a payload of our
/// choosing, and answer with its id.
fn queue_notification(
    db: &mut postgres::Client,
    org_id: &str,
    payload: serde_json::Value,
) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_millis() as i64;
    let id = format!("job-{now}-{}", payload["event"].as_str().unwrap_or("x"));
    let body = payload.to_string();
    db.execute(
        "INSERT INTO jobs (id, org_id, kind, payload, created_at, updated_at) \
         VALUES ($1, $2, 'notify-changeset', $3, $4, $4)",
        &[&id, &org_id.to_string(), &body, &now],
    )
    .unwrap_or_else(|e| panic!("queue {body}: {e}"));
    id
}

/// Wait for the worker to be done with a job, and answer with its
/// `(state, result, error)`.
fn finished(db: &mut postgres::Client, id: &str) -> (String, Option<String>, Option<String>) {
    let deadline = std::time::Instant::now() + SOON;
    loop {
        let row = db
            .query_one(
                "SELECT state, result, error FROM jobs WHERE id = $1",
                &[&id],
            )
            .unwrap_or_else(|e| panic!("read job {id}: {e}"));
        let state: String = row.get(0);
        if state != "queued" && state != "running" {
            return (state, row.get(1), row.get(2));
        }
        assert!(
            std::time::Instant::now() < deadline,
            "job {id} is still {state} after {SOON:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Everything in the mailbox, so an absence can be asserted about mail
/// this suite's own filter would not have looked at.
fn every_subject(mailbox: &Mailbox) -> Vec<String> {
    mailbox.all().into_iter().map(|m| m.subject).collect()
}

/// A job this build cannot read is failed loudly, and the worker takes
/// the next one.
///
/// Both halves are a rolling deploy, which is the only way these rows
/// appear: a newer node enqueues an event an older node has never heard
/// of, and a changeset can be gone by the time anybody gets to the job
/// naming it. The wrong answers are the ones that look like working
/// software — mailing everybody that somebody "updated" a changeset,
/// which says nothing and cannot be unsent, or a worker that stops on
/// the first row it does not understand and quietly tells nobody about
/// anything ever again.
#[test]
fn a_changeset_notification_this_build_cannot_read_fails_the_job_and_mails_no_guess() {
    let minio = Minio::shared();
    let bucket = minio.bucket("csnotify-poison");
    let scratch = Scratch::new("csnotify-poison");
    let mailbox = Mailbox::temp("csnotify-poison");
    let server = spawn(&bucket.base_url, &scratch, "csnotify-poison", &mailbox);
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");

    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    let app = repo_with_change(
        &mut ada,
        "app",
        "bo@acme.test",
        "I0000000000000000000000000000000000001a01",
    );
    let lib = repo_with_change(
        &mut ada,
        "lib",
        "bo@acme.test",
        "I0000000000000000000000000000000000001b01",
    );
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/changesets",
        Some(serde_json::json!({
            "key": "CS-4",
            "title": "the readable one",
            "members": [
                {"repo": "app", "change": app},
                {"repo": "lib", "change": lib},
            ],
        })),
    );
    assert_eq!(st, 201, "compose: {out}");
    // The worker is running and this fixture reaches it, so the two
    // absences below mean what they say rather than meaning nothing ran.
    wait_for_changeset_mail(&mailbox, "bo@acme.test", "composed changeset CS-4");

    let mut db = control(&server);
    let org_id = id_of(&mut db, "SELECT id FROM orgs WHERE name = $1", "acme");
    let cs_id = id_of(&mut db, "SELECT id FROM changesets WHERE key = $1", "CS-4");
    let ada_id = id_of(
        &mut db,
        "SELECT id FROM users WHERE email = $1",
        "ada@acme.test",
    );

    // An event from a version of this product that does not exist yet.
    let unknown = queue_notification(
        &mut db,
        &org_id,
        serde_json::json!({
            "changeset_id": cs_id,
            "event": "teleported",
            "actor": ada_id,
        }),
    );
    let (state, _, error) = finished(&mut db, &unknown);
    assert_eq!(state, "failed", "an unreadable job was not failed");
    let error = error.unwrap_or_default();
    assert!(
        error.contains("unknown changeset notification event"),
        "the failure does not say what was wrong with it: {error}"
    );

    // ...and a changeset that was deleted between the enqueue and the
    // claim. There is nothing to say about it and nothing wrong, so the
    // job is finished rather than failed and nobody hears anything.
    let vanished = queue_notification(
        &mut db,
        &org_id,
        serde_json::json!({
            "changeset_id": "01no-such-changeset",
            "event": "landed",
            "actor": ada_id,
        }),
    );
    let (state, result, error) = finished(&mut db, &vanished);
    assert_eq!(
        (state.as_str(), error.as_deref()),
        ("done", None),
        "a changeset that is gone is not an error"
    );
    assert_eq!(result.as_deref(), Some("sent 0"), "somebody was mailed");

    // Neither row put a word in anybody's inbox. "updated" is the
    // template's fallback verb, and its arriving would mean the worker
    // had guessed at an event instead of refusing it.
    for s in every_subject(&mailbox) {
        assert!(
            !s.contains("updated changeset"),
            "an unreadable event was mailed as a guess: {s}"
        );
    }
    assert_eq!(
        changeset_mails(&mailbox, "bo@acme.test").len(),
        1,
        "the poison rows put mail in a mailbox: {:?}",
        every_subject(&mailbox)
    );

    // And the worker took the next job rather than dying on those two,
    // which is the half a failed row alone does not prove.
    let after = queue_notification(
        &mut db,
        &org_id,
        serde_json::json!({
            "changeset_id": cs_id,
            "event": "failed",
            "actor": ada_id,
        }),
    );
    let (state, result, _) = finished(&mut db, &after);
    assert_eq!(state, "done", "the worker stopped at the poison rows");
    assert_eq!(result.as_deref(), Some("sent 1"), "{result:?}");
    wait_for_changeset_mail(&mailbox, "bo@acme.test", "could not land changeset CS-4");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// An address nobody proved is never mailed, however involved its owner
/// is.
///
/// Signing up mints the namespace immediately and leaves `verified_at`
/// null until the link is clicked, so between those two moments a person
/// can take part in a review with an address that is still only a claim.
/// Mailing a claim is how a forge becomes the thing that sends review
/// traffic to an address nobody confirmed — and the claim can be
/// somebody else's address.
///
/// The differential is what makes this test mean something: Eve and Dee
/// are both viewers of acme from somewhere else, they do exactly the same
/// thing on the same change, and the only difference between them is
/// that one of them proved her address.
#[test]
fn an_address_nobody_proved_is_not_mailed_about_a_changeset() {
    let minio = Minio::shared();
    let bucket = minio.bucket("csnotify-unproved");
    let scratch = Scratch::new("csnotify-unproved");
    let mailbox = Mailbox::temp("csnotify-unproved");
    let server = spawn(&bucket.base_url, &scratch, "csnotify-unproved", &mailbox);
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");
    server.bootstrap_org("elsewhere");
    member(&server, "elsewhere", "eve@elsewhere.test", "Eve", "owner");
    member(&server, "acme", "eve@elsewhere.test", "Eve", "viewer");

    // The unproved one: signed up, never clicked the link. This is an
    // ordinary state, not a contrived one — every account passes through
    // it, and some stay there. Accepting an invitation into acme makes
    // her a reader and leaves the account's address as it was: only
    // an account *created* by an invitation is proved by it.
    let (st, out) = server.req(
        "POST",
        "/v1/auth/signup",
        "",
        Some(serde_json::json!({
            "handle": "dee",
            "email": "dee@example.test",
            "name": "Dee",
            "password": PASSWORD,
        })),
    );
    assert_eq!(st, 202, "signup: {out}");

    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    ada.invite_and_accept("acme", "dee@example.test", "viewer");
    let mut eve = Browser::signed_in(&server, "eve@elsewhere.test", PASSWORD);
    let mut dee = Browser::signed_in(&server, "dee@example.test", PASSWORD);
    let (st, me) = dee.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    assert!(
        me["verified_at"].is_null(),
        "the fixture is verified, so this test proves nothing: {me}"
    );

    // Two members both of them may read, so nobody here is filtered by
    // the read rule the leak test above is about: what is under test is
    // the address.
    let open = repo_with_change(
        &mut ada,
        "open",
        "bo@acme.test",
        "I0000000000000000000000000000000000002a01",
    );
    let also = repo_with_change(
        &mut ada,
        "also",
        "bo@acme.test",
        "I0000000000000000000000000000000000002b01",
    );
    for (who, browser) in [
        ("eve", &mut eve as &mut Browser<'_>),
        ("dee", &mut dee as &mut Browser<'_>),
    ] {
        let (st, out) = browser.req(
            "POST",
            &format!("/v1/orgs/acme/repos/open/changes/{open}/comments"),
            Some(serde_json::json!({ "body": "does this cover the retry path?" })),
        );
        assert_eq!(st, 201, "{who} could not comment on a change: {out}");
    }

    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/changesets",
        Some(serde_json::json!({
            "key": "CS-5",
            "title": "two readable members",
            "members": [
                {"repo": "open", "change": open},
                {"repo": "also", "change": also},
            ],
        })),
    );
    assert_eq!(st, 201, "compose: {out}");

    // Eve took part with a proved address and is told, which is what
    // makes Dee's silence a statement about the address rather than
    // about viewers.
    wait_for_changeset_mail(&mailbox, "eve@elsewhere.test", "composed changeset CS-5");
    assert!(
        changeset_mails(&mailbox, "dee@example.test").is_empty(),
        "an unproved address was mailed: {:?}",
        every_subject(&mailbox)
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A mail transport that is refusing costs the notification and nothing
/// else.
///
/// The capture transport writes a file per message, so a mail directory
/// that has gone is a send that fails for every recipient — the shape of
/// a relay that is down, a credential that expired, an address a
/// provider rejects. What must not happen is the failure escaping the
/// send: the job would be failed, and a failed job is never retried, so
/// one bad moment at the transport would lose the notification for
/// everybody on the list rather than for nobody. The worker logs it,
/// carries on down the list, and reports how many it actually sent.
///
/// The per-change notifier is off on this node. It shares the mailbox,
/// and the changes opened above were queueing it mail of their own; when
/// the directory went, whether that worker was mid-send decided whether
/// *its* copy of this arm ran — one sha, two coverage verdicts. Its arm
/// is pinned on purpose in `notify_e2e`, under this test's name.
#[test]
fn a_mail_transport_that_refuses_costs_the_notification_and_not_the_worker() {
    let minio = Minio::shared();
    let bucket = minio.bucket("csnotify-nomail");
    let scratch = Scratch::new("csnotify-nomail");
    let mailbox = Mailbox::temp("csnotify-nomail");
    let server = spawn_with(
        &bucket.base_url,
        &scratch,
        "csnotify-nomail",
        &mailbox,
        &[("STRATUM_NOTIFY_POLL_SECS", "0")],
    );
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");

    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    let app = repo_with_change(
        &mut ada,
        "app",
        "bo@acme.test",
        "I0000000000000000000000000000000000003a01",
    );
    let lib = repo_with_change(
        &mut ada,
        "lib",
        "bo@acme.test",
        "I0000000000000000000000000000000000003b01",
    );
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/changesets",
        Some(serde_json::json!({
            "key": "CS-6",
            "title": "one review, no mailbox",
            "members": [
                {"repo": "app", "change": app},
                {"repo": "lib", "change": lib},
            ],
        })),
    );
    assert_eq!(st, 201, "compose: {out}");
    wait_for_changeset_mail(&mailbox, "bo@acme.test", "composed changeset CS-6");

    let mut db = control(&server);
    let org_id = id_of(&mut db, "SELECT id FROM orgs WHERE name = $1", "acme");
    let cs_id = id_of(&mut db, "SELECT id FROM changesets WHERE key = $1", "CS-6");
    let ada_id = id_of(
        &mut db,
        "SELECT id FROM users WHERE email = $1",
        "ada@acme.test",
    );

    // The transport starts refusing. Removing the directory rather than
    // making it unwritable on purpose: a suite that runs as root would
    // write into a read-only directory anyway, and a test that passes
    // for that reason is a test of nothing.
    std::fs::remove_dir_all(mailbox.dir()).expect("take the mailbox away");
    let refused = queue_notification(
        &mut db,
        &org_id,
        serde_json::json!({
            "changeset_id": cs_id,
            "event": "landed",
            "actor": ada_id,
        }),
    );
    let (state, result, error) = finished(&mut db, &refused);
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
        every_subject(&mailbox)
    );

    // ...and the next notification, once it can send again, goes out
    // from the same process with nothing restarted.
    std::fs::create_dir_all(mailbox.dir()).expect("give the mailbox back");
    let after = queue_notification(
        &mut db,
        &org_id,
        serde_json::json!({
            "changeset_id": cs_id,
            "event": "failed",
            "actor": ada_id,
        }),
    );
    let (state, result, _) = finished(&mut db, &after);
    assert_eq!(state, "done", "{result:?}");
    assert_eq!(result.as_deref(), Some("sent 1"), "{result:?}");
    wait_for_changeset_mail(&mailbox, "bo@acme.test", "could not land changeset CS-6");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A queue that cannot be written does not turn composing a changeset
/// into a failed request.
///
/// The trade is deliberate and it is worth pinning in both directions: a
/// changeset that was composed *has been composed*, and refusing the
/// response because the notification could not be queued would turn a
/// mail problem into a review problem. What it costs is that one
/// notification, which is the state the product was in before this
/// worker existed — so the mail for that changeset never arrives, and
/// the next one does, from the same process.
#[test]
fn a_queue_that_cannot_be_written_does_not_fail_composing_a_changeset() {
    let minio = Minio::shared();
    let bucket = minio.bucket("csnotify-noqueue");
    let scratch = Scratch::new("csnotify-noqueue");
    let mailbox = Mailbox::temp("csnotify-noqueue");
    let server = spawn(&bucket.base_url, &scratch, "csnotify-noqueue", &mailbox);
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");

    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    let app = repo_with_change(
        &mut ada,
        "app",
        "bo@acme.test",
        "I0000000000000000000000000000000000004a01",
    );
    let lib = repo_with_change(
        &mut ada,
        "lib",
        "bo@acme.test",
        "I0000000000000000000000000000000000004b01",
    );

    // The queue goes away — the table itself, so the INSERT is refused
    // by a database that is otherwise answering, which is what a
    // half-applied migration or a botched failover looks like.
    let mut db = control(&server);
    db.execute("ALTER TABLE jobs RENAME TO jobs_hidden", &[])
        .expect("rename the queue away");
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/changesets",
        Some(serde_json::json!({
            "key": "CS-7",
            "title": "composed into the dark",
            "members": [
                {"repo": "app", "change": app},
                {"repo": "lib", "change": lib},
            ],
        })),
    );
    assert_eq!(
        st, 201,
        "a notification that could not be queued failed the composition: {out}"
    );
    // ...and it is really there, which is the half that makes the
    // swallowed enqueue a trade rather than a lie.
    let (st, out) = ada.req("GET", "/v1/orgs/acme/changesets/CS-7", None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["state"], "open", "{out}");

    db.execute("ALTER TABLE jobs_hidden RENAME TO jobs", &[])
        .expect("give the queue back");

    // The next composition mails, from the same process: nothing had to
    // be restarted, and the only thing lost was the one notification.
    // A fresh change, because a change already bound to a changeset may
    // not join another.
    let web = repo_with_change(
        &mut ada,
        "web",
        "bo@acme.test",
        "I0000000000000000000000000000000000004c01",
    );
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/changesets",
        Some(serde_json::json!({
            "key": "CS-8",
            "title": "composed with a queue",
            "members": [{"repo": "web", "change": web}],
        })),
    );
    assert_eq!(st, 201, "compose: {out}");
    wait_for_changeset_mail(&mailbox, "bo@acme.test", "composed changeset CS-8");
    assert!(
        changeset_mails(&mailbox, "bo@acme.test")
            .iter()
            .all(|m| !m.subject.contains("CS-7")),
        "the notification that could not be queued arrived anyway: {:?}",
        every_subject(&mailbox)
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A node with the worker switched off leaves the work on the queue.
///
/// `STRATUM_*_POLL_SECS=0` means "do not run this worker here", which is
/// how a fleet keeps mail on the nodes that can send it. The property
/// that matters is not that no mail arrives — that is easy and would
/// also be true of a worker that dropped every job on the floor — but
/// that the row is still `queued`, so a node that *is* running the
/// worker will send it.
#[test]
fn a_node_with_the_changeset_notifier_off_leaves_the_work_on_the_queue() {
    let minio = Minio::shared();
    let bucket = minio.bucket("csnotify-off");
    let scratch = Scratch::new("csnotify-off");
    let mailbox = Mailbox::temp("csnotify-off");
    let server = spawn_with(
        &bucket.base_url,
        &scratch,
        "csnotify-off",
        &mailbox,
        &[("STRATUM_CHANGESET_NOTIFY_POLL_SECS", "0")],
    );
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");

    let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
    let app = repo_with_change(
        &mut ada,
        "app",
        "bo@acme.test",
        "I0000000000000000000000000000000000005a01",
    );
    // The single-change notifier is still on here, and Bo is the owner
    // this change needs: his getting *that* mail is what proves the
    // mailbox, the transport and the queue are all working, so the
    // absence below is about this worker and not about the fixture.
    wait_for_mail_about(&mailbox, "bo@acme.test", "[acme/app] Ada opened");

    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/changesets",
        Some(serde_json::json!({
            "key": "CS-9",
            "title": "nobody here can send this",
            "members": [{"repo": "app", "change": app}],
        })),
    );
    assert_eq!(st, 201, "compose: {out}");

    // The job is on the queue and stays there.
    let mut db = control(&server);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        let state: String = db
            .query_one(
                "SELECT state FROM jobs WHERE kind = 'notify-changeset'",
                &[],
            )
            .expect("the composition queued a notification")
            .get(0);
        assert_eq!(
            state, "queued",
            "a node that does not run this worker claimed the job anyway"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        changeset_mails(&mailbox, "bo@acme.test").is_empty(),
        "a node with the worker off sent the mail: {:?}",
        every_subject(&mailbox)
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A member whose repository is gone costs its own people the mail, and
/// nobody else's.
///
/// The row is real: deleting a repository is a soft delete, the change
/// and the membership stay behind it, and the queue is a queue — a
/// notification composed a moment before the delete is claimed a
/// moment after. The worker skips that member the way the changeset
/// read does, and the other members' reviewers still hear.
///
/// The ordering is made exact rather than raced: the worker is off
/// while the set is composed and the repository deleted, and switched
/// on afterwards. Before this test the arm was reached only when the
/// revert suite's worker happened to wake after a delete, which it did
/// on one CI runner and not the other in the same hour.
#[test]
fn a_member_whose_repository_is_gone_is_skipped_and_the_rest_are_still_told() {
    let minio = Minio::shared();
    let bucket = minio.bucket("csnotify-gone");
    let scratch = Scratch::new("csnotify-gone");
    let mailbox = Mailbox::temp("csnotify-gone");
    let mut server = spawn_with(
        &bucket.base_url,
        &scratch,
        "csnotify-gone",
        &mailbox,
        &[("STRATUM_CHANGESET_NOTIFY_POLL_SECS", "0")],
    );
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");
    member(&server, "acme", "cy@acme.test", "Cy", "member");

    {
        let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
        let app = repo_with_change(
            &mut ada,
            "app",
            "bo@acme.test",
            "I0000000000000000000000000000000000006a01",
        );
        let lib = repo_with_change(
            &mut ada,
            "lib",
            "cy@acme.test",
            "I0000000000000000000000000000000000006b01",
        );
        let (st, out) = ada.req(
            "POST",
            "/v1/orgs/acme/changesets",
            Some(serde_json::json!({
                "key": "CS-10",
                "title": "one of these is about to go",
                "members": [
                    {"repo": "app", "change": app},
                    {"repo": "lib", "change": lib},
                ],
            })),
        );
        assert_eq!(st, 201, "compose: {out}");

        // Bo's repository goes while the notification is still queued.
        let (st, out) = ada.req("DELETE", "/v1/orgs/acme/repos/app", None);
        assert_eq!(st, 204, "delete app: {out}");
    }

    let mut db = control(&server);
    let job: String = db
        .query_one("SELECT id FROM jobs WHERE kind = 'notify-changeset'", &[])
        .expect("the composition queued a notification")
        .get(0);

    // Now somebody is running the worker.
    server.restart_with(&[("STRATUM_CHANGESET_NOTIFY_POLL_SECS", "1".into())]);
    let (state, result, error) = finished(&mut db, &job);
    assert_eq!(state, "done", "{result:?} {error:?}");
    assert_eq!(result.as_deref(), Some("sent 1"), "{result:?}");

    // Cy owns the member that still exists, and hears about the set.
    wait_for_changeset_mail(&mailbox, "cy@acme.test", "composed");
    // Bo owned the one that is gone. He is not told about a review of a
    // repository he can no longer open.
    assert!(
        changeset_mails(&mailbox, "bo@acme.test").is_empty(),
        "the owner of a deleted member was mailed about it: {:?}",
        every_subject(&mailbox)
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// ...and a set whose *only* repository is gone tells nobody, and says
/// so as "sent 0" rather than as a failure.
///
/// The membership outlives the repository — a soft delete leaves the
/// change and the member row — so a job about it is claimed like any
/// other. There is nobody left to address it to, and no repository to
/// name in it. The job is done, not failed: failing it would leave a
/// poison row that says "retry", and there is nothing a retry would
/// find.
#[test]
fn a_set_whose_only_repository_is_gone_is_done_with_nobody_to_tell() {
    let minio = Minio::shared();
    let bucket = minio.bucket("csnotify-allgone");
    let scratch = Scratch::new("csnotify-allgone");
    let mailbox = Mailbox::temp("csnotify-allgone");
    let mut server = spawn_with(
        &bucket.base_url,
        &scratch,
        "csnotify-allgone",
        &mailbox,
        &[("STRATUM_CHANGESET_NOTIFY_POLL_SECS", "0")],
    );
    server.bootstrap_org("acme");
    member(&server, "acme", "ada@acme.test", "Ada", "owner");
    member(&server, "acme", "bo@acme.test", "Bo", "member");

    {
        let mut ada = Browser::signed_in(&server, "ada@acme.test", PASSWORD);
        let app = repo_with_change(
            &mut ada,
            "app",
            "bo@acme.test",
            "I0000000000000000000000000000000000007a01",
        );
        let (st, out) = ada.req(
            "POST",
            "/v1/orgs/acme/changesets",
            Some(serde_json::json!({
                "key": "CS-11",
                "title": "about to be about nothing",
                "members": [{"repo": "app", "change": app}],
            })),
        );
        assert_eq!(st, 201, "compose: {out}");
        let (st, out) = ada.req("DELETE", "/v1/orgs/acme/repos/app", None);
        assert_eq!(st, 204, "delete app: {out}");
    }

    let mut db = control(&server);
    let job: String = db
        .query_one("SELECT id FROM jobs WHERE kind = 'notify-changeset'", &[])
        .expect("the composition queued a notification")
        .get(0);
    server.restart_with(&[("STRATUM_CHANGESET_NOTIFY_POLL_SECS", "1".into())]);
    let (state, result, error) = finished(&mut db, &job);
    assert_eq!(state, "done", "{result:?} {error:?}");
    assert_eq!(result.as_deref(), Some("sent 0"), "{result:?}");
    assert!(
        changeset_mails(&mailbox, "bo@acme.test").is_empty(),
        "somebody was told about a set with no repositories in it: {:?}",
        every_subject(&mailbox)
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
