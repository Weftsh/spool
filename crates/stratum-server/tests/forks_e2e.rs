//! Forks, end to end against a real server.
//!
//! The case that matters is deliberately the awkward one: **a person who
//! is not a member of the org that owns the repository**. That is the
//! whole point of forking — it is how somebody with no push credential
//! contributes at all — and it is also the case the natural test misses,
//! because a test written the obvious way has the repository's own
//! creator do the forking, and they are a member, so it passes against a
//! guard that would refuse every real user.
//!
//! Two more properties are load-bearing and both are negatives: a fork
//! of a private repository may never be published, and a fork's storage
//! is upstream's storage — so the fork must actually serve content it
//! never copied.

use stratum_testkit::browser::Browser;
use stratum_testkit::gitcli::{self, Scratch};
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
    // Both fork jobs are background workers, and this suite waits on
    // their results. At the production intervals (5s and 30s) that is
    // most of the runtime of every test here, spent asleep.
    b = b.env("STRATUM_FORK_POLL_SECS", "1");
    b = b.env("STRATUM_PROMOTE_POLL_SECS", "1");
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

/// A push credential for a repository the caller owns.
fn push_token(browser: &mut Browser, org: &str, repo: &str) -> String {
    let (st, minted) = browser.req(
        "POST",
        &format!("/v1/orgs/{org}/tokens"),
        Some(serde_json::json!({
            "scopes": ["repo:read", "repo:write"],
            "repo": repo,
            "label": "seed",
        })),
    );
    assert_eq!(st, 201, "mint token: {minted}");
    minted["token"].as_str().expect("token").to_string()
}

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

/// Wait for the fork worker to finish, then report the state it reached.
fn await_fork(browser: &mut Browser, path: &str) -> String {
    for _ in 0..100 {
        let (st, body) = browser.req("GET", path, None);
        assert_eq!(st, 200, "{body}");
        match body["fork_state"].as_str() {
            Some("pending") | None => std::thread::sleep(std::time::Duration::from_millis(100)),
            Some(other) => return other.to_string(),
        }
    }
    panic!("fork never left pending");
}

/// Sign somebody up and deliberately leave the link unredeemed.
///
/// `admin user-create` cannot produce this: it marks its accounts
/// verified. Only an unredeemed signup leaves a person who owns a
/// namespace while `verified_at` is still null — which is every new
/// account on the way in, and the only shape that reaches
/// `require_verified` on the fork path.
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
    let (st, _) = b.req(
        "POST",
        "/v1/auth/login",
        Some(serde_json::json!({ "email": email, "password": PASSWORD })),
    );
    assert_eq!(
        st, 200,
        "an unverified account must still be able to sign in"
    );
    b
}

/// A public repository with something in it, in `owner`'s namespace.
fn public_repo(server: &Server, owner: &mut Browser, handle: &str, name: &str) {
    let (st, body) = owner.req(
        "POST",
        &format!("/v1/orgs/{handle}/repos"),
        Some(serde_json::json!({ "name": name, "public": true })),
    );
    assert_eq!(st, 201, "{body}");
    let (st, body) = owner.req(
        "POST",
        &format!("/v1/orgs/{handle}/repos/{name}/commits"),
        Some(serde_json::json!({
            "message": "first commit",
            "operations": [{ "op": "put", "path": "README.md", "content": "# seed\n" }],
        })),
    );
    assert_eq!(st, 201, "{body}");
    let _ = server;
}

/// The refusals, which are most of what an API is.
///
/// Forking's happy path was covered from the day it landed; none of the
/// ways it says no were, and the coverage gate is what said so — every
/// one of these lines sat in `repos.rs`'s unexplained list. They are
/// product behaviour rather than error propagation, so they wanted
/// tests rather than ledger entries.
#[test]
fn forking_into_a_named_namespace_obeys_that_namespace_s_rules() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-named");
    let scratch = Scratch::new("forks-named");
    let mail = Mailbox::temp("forks-named");
    let server = spawn(&bucket.base_url, &scratch, "forks_named", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    public_repo(&server, &mut ada, "ada", "widget");

    // Naming a target you own works, and is a different code path from
    // the default: every other test in this file lets the target
    // default to the caller's own namespace, so `org` being present had
    // never been exercised at all.
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/forks",
        Some(serde_json::json!({ "org": "bob", "name": "widget-copy" })),
    );
    assert_eq!(
        st, 202,
        "a named target the caller owns was refused: {body}"
    );
    assert_eq!(body["name"], "widget-copy");
    assert_eq!(
        await_fork(&mut bob, "/v1/orgs/bob/repos/widget-copy"),
        "ready"
    );

    // A target that does not exist is a 404 rather than a 500 or a
    // silently-defaulted namespace.
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/forks",
        Some(serde_json::json!({ "org": "no-such-namespace" })),
    );
    assert_eq!(st, 404, "a missing target namespace: {body}");

    // And a target that exists and belongs to somebody else is refused.
    // Being able to *read* the source says nothing about where the
    // caller may put things — the two authorities are separate and this
    // is the one nothing checked.
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/forks",
        Some(serde_json::json!({ "org": "ada" })),
    );
    // 404 exactly, not "404 or 403".
    //
    // `authx::require` masks by both doors — `SessionAuth::NoAccess` and
    // the `allows` check each return `not_found()` — so 403 from this
    // seam would be the anomaly rather than an alternative. And the
    // handler resolves `org_or_404` before the authority check, so a
    // namespace that does not exist and one the caller may not write to
    // are indistinguishable *by construction*.
    //
    // An assertion that accepted either code could not tell that
    // version from one that leaks, which is the whole property it is
    // here to hold. A 403 would confirm the namespace exists — and
    // while personal namespaces are already public at `/{owner}`,
    // company orgs are not, so it would turn this write endpoint into
    // an existence oracle for anybody willing to guess names.
    assert_eq!(st, 404, "bob forked into ada's namespace: {body}");
    // Nothing was created there.
    let (st, _) = ada.req("GET", "/v1/orgs/ada/repos/widget-copy", None);
    assert_eq!(st, 404, "a refused fork left a repository behind");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A fork counts against the free-tier repository cap exactly as a
/// created repository does.
///
/// Found while walking the plan boundaries by hand: `create_one` refuses
/// the (N+1)th repository on a `free` namespace with a 402, and the fork
/// handler never asked. Every fork the dashboard makes lands in the
/// forker's personal namespace, and a personal namespace is always
/// `free` — so the one limit the billing docs name for it was one no
/// fork had ever met. A fork is the larger of the two asks, too: it
/// carries real history where a created repository starts empty.
///
/// Pressing Fork on something already forked must still hand the fork
/// back at the cap: nothing is being made, so nothing is being refused.
#[test]
fn a_fork_counts_against_the_free_tier_cap_like_any_other_repository() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-cap");
    let scratch = Scratch::new("forks-cap");
    let mail = Mailbox::temp("forks-cap");
    let server = spawn_with(
        &bucket.base_url,
        &scratch,
        "forks_cap",
        &mail,
        &[("STRATUM_FREE_TIER_REPOS", "1")],
    );

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut carl = signup(&server, &mail, "carl", "carl@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    public_repo(&server, &mut ada, "ada", "widget");
    public_repo(&server, &mut carl, "carl", "gizmo");
    // The cap holds on create — Ada is full…
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "second" })),
    );
    assert_eq!(st, 402, "{body}");
    // …and is told the way out that exists for a personal namespace. It
    // used to read "upgrade to create more", which is the organization's
    // sentence: a personal namespace is free for good and Billing sells
    // it nothing, so the person it sent there found no upgrade to buy.
    const PERSONAL_CAP: &str =
        "quota: a personal namespace holds up to 1 repository — create an organization to \
         hold more";
    assert_eq!(body["error"], PERSONAL_CAP);

    // Bob's first fork fills his one slot.
    let (st, body) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    assert_eq!(await_fork(&mut bob, "/v1/orgs/bob/repos/widget"), "ready");

    // …and a second fork is refused with the 402 and the sentence a
    // create gets, so the dashboard's quota handling reads it the same.
    let (st, body) = bob.req("POST", "/v1/orgs/carl/repos/gizmo/forks", None);
    assert_eq!(
        st, 402,
        "a fork past the free-tier cap was admitted: {body}"
    );
    assert_eq!(body["error"], PERSONAL_CAP);
    // It left nothing behind.
    let (st, _) = bob.req("GET", "/v1/orgs/bob/repos/gizmo", None);
    assert_eq!(st, 404, "a refused fork left a repository behind");

    // At the cap, the fork he already has is still handed back.
    let (st, body) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["name"], "widget");

    // Deleting the fork makes room: the cap is on what the namespace
    // holds, not on how many times it has been asked.
    let (st, _) = bob.req("DELETE", "/v1/orgs/bob/repos/widget", None);
    assert!(st == 200 || st == 204, "delete: {st}");
    let (st, body) = bob.req("POST", "/v1/orgs/carl/repos/gizmo/forks", None);
    assert_eq!(st, 202, "{body}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// Forking the same repository twice hands back the fork you already
/// have; a same-named repository that is *not* your fork of it is a
/// collision that says which repository is in the way.
///
/// Found by pressing Fork in the app on a repository already forked: the
/// answer was `repo "widget" already exists`, and the page stayed on
/// upstream. A person who has forgotten they forked something — the
/// commonest reason to press the button twice — is told they failed and
/// left to go looking for a repository the server could name. GitHub
/// takes you to your fork. So does this.
#[test]
fn forking_twice_hands_back_the_fork_you_already_have() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-twice");
    let scratch = Scratch::new("forks-twice");
    let mail = Mailbox::temp("forks-twice");
    let server = spawn(&bucket.base_url, &scratch, "forks_twice", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    public_repo(&server, &mut ada, "ada", "widget");

    let (st, first) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 202, "{first}");
    assert_eq!(await_fork(&mut bob, "/v1/orgs/bob/repos/widget"), "ready");

    // The second press is answered with the fork that already exists:
    // 200, not 202, because nothing was created, and the same row, so a
    // client that navigates to `org/name` on success lands on the fork
    // either way.
    let (st, again) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(
        st, 200,
        "forking what you already forked must hand back the fork: {again}"
    );
    assert_eq!(again["id"], first["id"], "{again}");
    assert_eq!(again["org"], "bob", "{again}");
    assert_eq!(again["name"], "widget", "{again}");
    assert_eq!(again["fork_parent"], "ada/widget", "{again}");
    assert_eq!(again["fork_state"], "ready", "{again}");
    // And nothing else appeared: one fork, not a second one under
    // some derived name.
    let (st, forks) = ada.req("GET", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 200, "{forks}");
    assert_eq!(forks["count"], 1, "{forks}");

    // A repository of Bob's that merely shares the name is a genuine
    // collision. The refusal names it and the way past it, instead of
    // a bare "already exists" that reads the same as the case above.
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/bob/repos",
        Some(serde_json::json!({ "name": "gadget" })),
    );
    assert_eq!(st, 201, "{body}");
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/forks",
        Some(serde_json::json!({ "name": "gadget" })),
    );
    assert_eq!(st, 409, "forking onto an unrelated repository: {body}");
    let why = body["error"].as_str().unwrap_or_default();
    assert!(
        why.contains("bob/gadget already exists") && why.contains("not a fork of ada/widget"),
        "{body}"
    );
    assert!(why.contains("another name"), "{body}");
    // The unrelated repository was not quietly turned into a fork.
    let (st, gadget) = bob.req("GET", "/v1/orgs/bob/repos/gadget", None);
    assert_eq!(st, 200, "{gadget}");
    assert!(gadget["fork_parent"].is_null(), "{gadget}");

    // Naming a free one succeeds, which proves the refusal was about
    // the name rather than about forking twice.
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/forks",
        Some(serde_json::json!({ "name": "widget-2" })),
    );
    assert_eq!(st, 202, "a differently-named second fork: {body}");
    assert_eq!(await_fork(&mut bob, "/v1/orgs/bob/repos/widget-2"), "ready");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// An unproved address cannot fork, and every account has somewhere to
/// fork into.
///
/// Two claims in one test because they are two halves of the same door.
/// The first is the gate: forking creates a repository, and creating is
/// what `require_verified` protects. The second is the invariant that
/// keeps the handler's "no personal namespace to fork into" refusal
/// unreachable — every account arrives with a handle and a namespace in
/// the same transaction, from either door. That guard stays because it
/// sits on a path where being wrong means a fork with nowhere to go;
/// this asserts the invariant rather than the arm, so a third door that
/// ever mints a user without a namespace is named by a test instead of
/// being caught silently in production.
#[test]
fn forking_takes_a_proved_address_and_a_namespace_to_land_in() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-unproved");
    let scratch = Scratch::new("forks-unproved");
    let mail = Mailbox::temp("forks-unproved");
    let server = spawn(&bucket.base_url, &scratch, "forks_unproved", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    public_repo(&server, &mut ada, "ada", "widget");

    let mut unproved = signup_unverified(&server, "unproved", "unproved@example.com");
    let (st, me) = unproved.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    assert!(
        me["verified_at"].is_null(),
        "the fixture is verified, so this proves nothing: {me}"
    );
    // The invariant: they own a namespace already, so the handler's
    // "nowhere to fork into" refusal is not what stops them.
    assert_eq!(
        me["orgs"].as_array().map(|a| a.len()),
        Some(1),
        "an account arrived without a namespace to fork into: {me}"
    );

    let (st, body) = unproved.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 403, "an unproved address forked a repository: {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("confirm your email"),
        "the refusal must say what to do about it: {body}"
    );

    // Reading is untouched — the gate is on creating, not on existing.
    let (st, body) = unproved.req("GET", "/v1/orgs/ada/repos/widget", None);
    assert_eq!(st, 200, "an unproved account was refused a read: {body}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The fork listing obeys the repository's own visibility.
#[test]
fn listing_forks_of_a_repository_you_may_not_read_is_refused() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-listing");
    let scratch = Scratch::new("forks-listing");
    let mail = Mailbox::temp("forks-listing");
    let server = spawn(&bucket.base_url, &scratch, "forks_listing", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "ledger", "public": false })),
    );
    assert_eq!(st, 201, "{body}");

    // Its owner may list them.
    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/ledger/forks", None);
    assert_eq!(st, 200, "an owner could not list forks: {body}");
    assert_eq!(body["count"], 0);
    assert_eq!(body["forks"].as_array().map(|a| a.len()), Some(0));

    // A signed-in outsider and a stranger both get the same answer, and
    // it is the same answer a repository that does not exist gives —
    // the fork listing must not become a way to ask whether a private
    // repository is there.
    let (outsider, _) = bob.req("GET", "/v1/orgs/ada/repos/ledger/forks", None);
    let (stranger, _) = server.req("GET", "/v1/orgs/ada/repos/ledger/forks", "", None);
    let (absent, _) = server.req("GET", "/v1/orgs/ada/repos/no-such/forks", "", None);
    assert!(outsider >= 400, "an outsider listed a private repo's forks");
    assert_eq!(
        stranger, absent,
        "the fork listing tells a stranger a private repository exists: \
         private {stranger}, missing {absent}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_stranger_can_fork_a_public_repository_and_clone_what_they_never_copied() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-e2e");
    let scratch = Scratch::new("forks-e2e");
    let mail = Mailbox::temp("forks");
    let server = spawn(&bucket.base_url, &scratch, "forks", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    // Bob is in his own namespace and is not a member of ada's. This is
    // the case a guard built on the repository principal refuses, and
    // the case forking exists for.
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");

    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");

    // Give it real content, so the fork has something to actually serve.
    let work = scratch.path().join("seed");
    gitcli::fixture_repo(&work, 6);
    let tok = push_token(&mut ada, "ada", "widget");
    let url = server.authed_url(&tok, "ada", "widget");
    gitcli::git(&work, &["push", "-q", &url, "main"]);

    // Bob forks it, naming nothing: it goes to his own namespace under
    // the same name.
    let (st, body) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 202, "a stranger could not fork a public repo: {body}");
    assert_eq!(body["name"], "widget");
    assert_eq!(
        body["fork_state"], "pending",
        "a fork's storage is written by a job; 202 and pending are the honest answer: {body}"
    );

    let state = await_fork(&mut bob, "/v1/orgs/bob/repos/widget");
    assert_eq!(state, "ready", "the fork job did not finish cleanly");

    // The claim, proven the only way that counts: clone the fork over
    // the wire and fsck it. Every object in that pack came out of ada's
    // storage — bob's prefix holds two pointers and nothing else — and
    // I11 says a produced clone must pass `git fsck --full --strict`.
    // Anonymously, because the fork is public and a stranger is who this
    // is for.
    let dest = scratch.path().join("cloned-fork");
    let fork_url = format!("{}/bob/widget.git", server.base);
    gitcli::clone_and_fsck(&fork_url, &dest);
    let log = gitcli::git(&dest, &["log", "--oneline"]);
    assert_eq!(
        log.lines().count(),
        6,
        "the fork did not serve upstream's history: {log}"
    );

    // The listing shows it, and the count agrees with the list.
    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["count"], 1, "{body}");
    assert_eq!(body["forks"][0]["org"], "bob");
    assert_eq!(body["forks"][0]["name"], "widget");
    assert_eq!(
        body["forks"].as_array().unwrap().len(),
        body["count"].as_u64().unwrap() as usize,
        "count disagrees with the list beside it: {body}"
    );

    // And the source repository reports the same number.
    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/widget", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["fork_count"], 1, "{body}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_personal_token_can_fork_as_well_as_a_browser_session() {
    // Every other test here drives a browser session, so every one of
    // them would pass against a handler that resolved identity from the
    // session cookie alone — and that handler would refuse the entire
    // API and CLI, holding a perfectly good credential, with a 401 that
    // says "forking takes a signed-in person".
    //
    // Same shape as the second-person-in-another-namespace point: the
    // natural test passes against the wrong code. So this one carries no
    // cookie at all.
    //
    // The token is deliberately **not** repo-bound. A repository-scoped
    // token is an automation attached to one repository, not a person
    // with a namespace to fork into, and it is correctly nobody here.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-token");
    let scratch = Scratch::new("forks-token");
    let mail = Mailbox::temp("forks-token");
    let server = spawn(&bucket.base_url, &scratch, "forks-token", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");

    // Bob's own personal token, no repo scope.
    let (st, minted) = bob.req(
        "POST",
        "/v1/orgs/bob/tokens",
        Some(serde_json::json!({ "scopes": ["repo:read", "repo:write"], "label": "cli" })),
    );
    assert_eq!(st, 201, "mint personal token: {minted}");
    let tok = minted["token"].as_str().expect("token");

    // Forked with the token and no session cookie anywhere.
    let (st, body) = server.req("POST", "/v1/orgs/ada/repos/widget/forks", tok, None);
    assert_eq!(
        st, 202,
        "a personal token could not fork — the API and CLI are refused: {body}"
    );
    assert_eq!(body["name"], "widget");

    let state = await_fork(&mut bob, "/v1/orgs/bob/repos/widget");
    assert_eq!(state, "ready", "the fork job did not finish cleanly");

    // The other half, and it is the reason `caller_person` checks
    // `repo_id.is_none()` rather than simply taking any principal's
    // user: a **repository-scoped** token is an automation attached to
    // one repository. It has no namespace of its own to fork into, so it
    // is correctly nobody here — even though it carries a real user id.
    let (st, bound) = ada.req(
        "POST",
        "/v1/orgs/ada/tokens",
        Some(serde_json::json!({
            "scopes": ["repo:read", "repo:write"], "repo": "widget", "label": "ci",
        })),
    );
    assert_eq!(st, 201, "mint repo-bound token: {bound}");
    let ci = bound["token"].as_str().expect("token");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/ada/repos/widget/forks",
        ci,
        Some(serde_json::json!({ "org": "ada", "name": "widget-ci-fork" })),
    );
    assert_ne!(
        st, 202,
        "a repository-bound token forked, and it belongs to no namespace: {out}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_fork_of_a_private_repository_is_private_and_stays_that_way() {
    // The security property of the whole slice. A zero-copy fork serves
    // upstream's actual bytes, so a fork that could be published would
    // publish a private repository's objects without ever copying them.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-private");
    let scratch = Scratch::new("forks-private");
    let mail = Mailbox::temp("forks-private");
    let server = spawn(&bucket.base_url, &scratch, "forks-private", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "secret", "public": false })),
    );
    assert_eq!(st, 201, "{body}");

    // Ada forks her own private repo into her own namespace.
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos/secret/forks",
        Some(serde_json::json!({ "name": "secret-fork" })),
    );
    assert_eq!(st, 202, "{body}");
    assert_eq!(
        body["public"], false,
        "a fork of a private repository was created public: {body}"
    );
    // Ada may read the upstream, so her fork says whose history it is
    // carrying. Found in the app: this was gated on the upstream being
    // *public*, and a member forking their own org's private repository
    // got a fork page that said "Forking the upstream…" without naming
    // the repository they had pressed the button on.
    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/secret-fork", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["fork_parent"], "ada/secret", "{body}");

    // And it may not be published, because its root is private.
    let (st, body) = ada.req(
        "PATCH",
        "/v1/orgs/ada/repos/secret-fork",
        Some(serde_json::json!({ "public": true })),
    );
    assert_eq!(
        st, 403,
        "a fork of a private repository was allowed to go public: {body}"
    );

    // The source itself is still ada's to publish — the rule applies to
    // forks, and must not have broken ordinary visibility edits.
    let (st, body) = ada.req(
        "PATCH",
        "/v1/orgs/ada/repos/secret",
        Some(serde_json::json!({ "public": true })),
    );
    assert_eq!(st, 200, "publishing an ordinary repo was refused: {body}");

    // With the root public, the fork may now be published too.
    let (st, body) = ada.req(
        "PATCH",
        "/v1/orgs/ada/repos/secret-fork",
        Some(serde_json::json!({ "public": true })),
    );
    assert_eq!(st, 200, "{body}");

    // Ada takes the upstream private again, leaving a public fork of a
    // private repository. A stranger reading the fork is not told what
    // it was forked from — that would name a private repository to
    // somebody who could not otherwise know it exists — while ada,
    // who may read it, still is.
    let (st, body) = ada.req(
        "PATCH",
        "/v1/orgs/ada/repos/secret",
        Some(serde_json::json!({ "public": false })),
    );
    assert_eq!(st, 200, "{body}");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    let (st, body) = bob.req("GET", "/v1/orgs/ada/repos/secret-fork", None);
    assert_eq!(st, 200, "{body}");
    assert!(
        body["fork_parent"].is_null(),
        "a private upstream was named to a stranger: {body}"
    );
    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/secret-fork", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["fork_parent"], "ada/secret", "{body}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_private_fork_is_not_advertised_to_strangers() {
    // The count and the list are both "what you may see". Reporting a
    // stored total beside a shorter list would say precisely how many
    // private forks exist.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-hidden");
    let scratch = Scratch::new("forks-hidden");
    let mail = Mailbox::temp("forks-hidden");
    let server = spawn(&bucket.base_url, &scratch, "forks-hidden", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");

    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");

    let (st, body) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    await_fork(&mut bob, "/v1/orgs/bob/repos/widget");

    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["count"], 1, "{body}");

    // Bob takes his fork private. It leaves the listing, and the count
    // leaves with it.
    let (st, body) = bob.req(
        "PATCH",
        "/v1/orgs/bob/repos/widget",
        Some(serde_json::json!({ "public": false })),
    );
    assert_eq!(st, 200, "{body}");

    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        body["count"], 0,
        "a private fork was still counted, which says it exists: {body}"
    );
    assert!(body["forks"].as_array().unwrap().is_empty(), "{body}");

    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/widget", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["fork_count"], 0, "{body}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn deleting_a_fork_takes_it_out_of_the_count_immediately() {
    // A tombstoned repository leaves the listing at once, so a count
    // that outlived it would be wrong for the whole grace window — the
    // same leak as a private fork, where a number larger than the list
    // says how many things you are not being shown.
    //
    // What actually guarantees this is the `state = 'active'` filter in
    // `forks::public_forks_of`, which both the list and the count are
    // read from. It is deliberately **not** `repos.rs::delete` calling
    // `forks::detach`: this test passes with that call removed, which I
    // checked rather than assumed after writing a comment here claiming
    // the opposite. `detach` keeps the denormalised `repos.fork_count`
    // column in step for the promotion and re-pointing jobs, and nothing
    // on the read path depends on it.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-deleted");
    let scratch = Scratch::new("forks-deleted");
    let mail = Mailbox::temp("forks-deleted");
    let server = spawn(&bucket.base_url, &scratch, "forks-deleted", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");

    let (st, body) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    await_fork(&mut bob, "/v1/orgs/bob/repos/widget");
    assert_eq!(
        ada.req("GET", "/v1/orgs/ada/repos/widget", None).1["fork_count"],
        1
    );

    // Bob deletes his fork.
    let (st, _) = bob.req("DELETE", "/v1/orgs/bob/repos/widget", None);
    assert_eq!(st, 204);

    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/widget", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        body["fork_count"], 0,
        "a deleted fork was still counted against its upstream: {body}"
    );
    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["count"], 0, "{body}");
    assert!(body["forks"].as_array().unwrap().is_empty(), "{body}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn deleting_an_upstream_promotes_its_forks_instead_of_stranding_them() {
    // The item 6c exists for. A zero-copy fork reads upstream's objects
    // and `epoch_refs` stops them being collected — absolutely, because
    // the foreign key RESTRICTs. So before promotion existed, deleting an
    // upstream that had forks produced a repository the sweeper declined
    // on every tick, forever, holding storage for something nobody could
    // reach.
    //
    // What must be true afterwards: the fork still serves its full
    // history, and it does so from storage of its own.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-promote-e2e");
    let scratch = Scratch::new("forks-promote-e2e");
    let mail = Mailbox::temp("forks-promote-e2e");
    let server = spawn(&bucket.base_url, &scratch, "forks-promote-e2e", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");

    let work = scratch.path().join("seed");
    gitcli::fixture_repo(&work, 6);
    let tok = push_token(&mut ada, "ada", "widget");
    let url = server.authed_url(&tok, "ada", "widget");
    gitcli::git(&work, &["push", "-q", &url, "main"]);

    let (st, body) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    assert_eq!(await_fork(&mut bob, "/v1/orgs/bob/repos/widget"), "ready");

    // Ada deletes the upstream. This must succeed — GitHub allows it and
    // so do we; what changes is that bob's fork survives it.
    let (st, _) = ada.req("DELETE", "/v1/orgs/ada/repos/widget", None);
    assert_eq!(st, 204, "deleting an upstream with forks was refused");

    // Promotion runs in the background. Wait for bob's fork to stop
    // being a fork — `fork_state` going null is the observable.
    let mut promoted = false;
    for _ in 0..150 {
        let (st, body) = bob.req("GET", "/v1/orgs/bob/repos/widget", None);
        assert_eq!(
            st, 200,
            "the fork stopped answering after its upstream went: {body}"
        );
        if body["fork_state"].is_null() {
            promoted = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(
        promoted,
        "the fork was never promoted after its upstream was deleted"
    );

    // The proof: clone the fork and fsck it. Every object it serves is
    // now its own, and upstream's prefix is no longer holding it up.
    let dest = scratch.path().join("promoted-clone");
    let fork_url = format!("{}/bob/widget.git", server.base);
    gitcli::clone_and_fsck(&fork_url, &dest);
    let log = gitcli::git(&dest, &["log", "--oneline"]);
    assert_eq!(
        log.lines().count(),
        6,
        "a promoted fork lost history its upstream used to hold: {log}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A server whose fork worker is switched off, so a fork can be observed
/// in the state it passes through rather than the one it lands in.
fn spawn_with(
    store_url: &str,
    scratch: &Scratch,
    hint: &str,
    mail: &Mailbox,
    extra: &[(&str, &str)],
) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"));
    for (k, v) in mail.env() {
        b = b.env(k, v);
    }
    for (k, v) in extra {
        b = b.env(k, *v);
    }
    b.start()
}

#[test]
fn forking_something_that_is_not_there_and_forking_with_a_bad_credential() {
    // Two refusals that happen before any of the interesting logic, and
    // both are the first thing a client hitting this endpoint by hand
    // will do. A repository that does not exist must answer exactly as
    // one you may not see does, and a malformed credential must be
    // refused rather than quietly treated as anonymous — an anonymous
    // caller and a caller presenting a broken token are different
    // situations and only one of them is innocent.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-absent");
    let scratch = Scratch::new("forks-absent");
    let mail = Mailbox::temp("forks-absent");
    let server = spawn(&bucket.base_url, &scratch, "forks-absent", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");

    // No such repository, in a namespace that does exist.
    let (st, _) = ada.req("POST", "/v1/orgs/ada/repos/nosuchthing/forks", None);
    assert_eq!(
        st, 404,
        "forking a repository that is not there was not a 404"
    );

    // No such namespace either.
    let (st, _) = ada.req("POST", "/v1/orgs/nobody/repos/widget/forks", None);
    assert_eq!(st, 404);

    // A credential that is not one. Refused, and specifically not
    // treated as an anonymous caller who simply has no rights.
    let (st, _) = server.req(
        "POST",
        "/v1/orgs/ada/repos/widget/forks",
        "weft_not_a_real_token",
        None,
    );
    assert!(
        st == 401 || st == 403 || st == 404,
        "a malformed credential produced {st}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn forking_an_empty_repository_gives_an_empty_repository() {
    // An ordinary thing to do — fork a project the moment it is created,
    // before anybody has pushed — and the fork worker has a branch for
    // it that nothing exercised. Upstream has no manifest, so there is
    // nothing to point at and nothing to pin, and the honest result is a
    // fork that is ready and empty rather than one stuck pending.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-empty-src");
    let scratch = Scratch::new("forks-empty-src");
    let mail = Mailbox::temp("forks-empty-src");
    let server = spawn(&bucket.base_url, &scratch, "forks-empty-src", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");

    let (st, body) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    assert_eq!(
        await_fork(&mut bob, "/v1/orgs/bob/repos/widget"),
        "ready",
        "forking an empty repository left the fork unfinished"
    );

    // And it holds no reference against upstream: there was no epoch to
    // pin, so deleting upstream needs no promotion at all.
    let (st, _) = ada.req("DELETE", "/v1/orgs/ada/repos/widget", None);
    assert_eq!(st, 204);
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_fork_stays_pending_while_the_worker_is_switched_off() {
    // `STRATUM_FORK_POLL_SECS=0` disables the worker, which is how an
    // operator stops forks being prepared without stopping the forge.
    // The repository must still be created and must say `pending` —
    // 202 with a state that never moves is the honest answer, and a
    // fork that reported `ready` here would be one nothing had written.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-worker-off");
    let scratch = Scratch::new("forks-worker-off");
    let mail = Mailbox::temp("forks-worker-off");
    let server = spawn_with(
        &bucket.base_url,
        &scratch,
        "forks-worker-off",
        &mail,
        &[
            ("STRATUM_FORK_POLL_SECS", "0"),
            ("STRATUM_PROMOTE_POLL_SECS", "0"),
        ],
    );

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");

    let (st, body) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (st, body) = bob.req("GET", "/v1/orgs/bob/repos/widget", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        body["fork_state"], "pending",
        "a fork advanced with its worker switched off: {body}"
    );
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_fork_whose_upstream_vanishes_first_reports_failed_rather_than_pending() {
    // The race an operator can actually cause: the fork is accepted, the
    // upstream is deleted before the job runs, and the job finds nothing
    // to fork from. It must say so. A fork left `pending` forever is
    // indistinguishable from a queue that has stopped, and the person
    // waiting on it has no way to tell.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-upstream-gone");
    let scratch = Scratch::new("forks-upstream-gone");
    let mail = Mailbox::temp("forks-upstream-gone");
    let mut server = spawn_with(
        &bucket.base_url,
        &scratch,
        "forks-upstream-gone",
        &mail,
        // Off, so the delete lands between the fork's acceptance and its
        // job — which is the window this is about.
        &[("STRATUM_FORK_POLL_SECS", "0")],
    );

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");
    // Minted before the restart: a token outlives the process where a
    // session cookie's `Browser` does not.
    let (st, minted) = bob.req(
        "POST",
        "/v1/orgs/bob/tokens",
        Some(serde_json::json!({ "scopes": ["repo:read", "repo:write"], "label": "cli" })),
    );
    assert_eq!(st, 201, "mint: {minted}");
    let bob_tok = minted["token"].as_str().expect("token").to_string();

    let (st, body) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    let (st, _) = ada.req("DELETE", "/v1/orgs/ada/repos/widget", None);
    assert_eq!(st, 204);

    // Now let the worker run against an upstream that is gone. Same
    // process, same database — a second `spawn_with` would give it a
    // fresh one and the fork under test would not be in it.
    server.restart_with(&[("STRATUM_FORK_POLL_SECS", "1".into())]);
    let mut state = String::new();
    for _ in 0..150 {
        let (st, b) = server.req("GET", "/v1/orgs/bob/repos/widget", &bob_tok, None);
        assert_eq!(st, 200, "{b}");
        match b["fork_state"].as_str() {
            Some("pending") | None => std::thread::sleep(std::time::Duration::from_millis(100)),
            Some(other) => {
                state = other.to_string();
                break;
            }
        }
    }
    assert_eq!(
        state, "failed",
        "a fork whose upstream had gone did not report failure"
    );
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn forking_under_a_name_the_forge_will_not_take_is_a_refusal_not_a_crash() {
    // The name is the caller's to choose, so it is the caller's to get
    // wrong — and a rejected name must come back as a refusal rather
    // than a 500 from somewhere inside repository creation.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-badname");
    let scratch = Scratch::new("forks-badname");
    let mail = Mailbox::temp("forks-badname");
    let server = spawn(&bucket.base_url, &scratch, "forks-badname", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");

    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/forks",
        Some(serde_json::json!({ "name": "not a valid name!!" })),
    );
    assert_eq!(st, 400, "a rejected fork name was not a refusal: {body}");
    // And nothing was left behind under it.
    let (st, _) = bob.req("GET", "/v1/orgs/bob/repos/not%20a%20valid%20name!!", None);
    assert_eq!(st, 404);
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn forking_takes_a_person_and_a_repository_you_may_read() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-refusals");
    let scratch = Scratch::new("forks-refusals");
    let mail = Mailbox::temp("forks-refusals");
    let server = spawn(&bucket.base_url, &scratch, "forks-refusals", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "secret", "public": false })),
    );
    assert_eq!(st, 201, "{body}");

    // A stranger cannot fork what they cannot see, and is told the same
    // thing they would be told if it did not exist.
    let (st, _) = bob.req("POST", "/v1/orgs/ada/repos/secret/forks", None);
    assert_eq!(st, 404, "a private repo was forkable by a stranger");

    // Nobody at all cannot fork anything.
    let (st, _) = server.req("POST", "/v1/orgs/ada/repos/secret/forks", "", None);
    assert!(
        st == 401 || st == 404,
        "an anonymous fork was accepted: {st}"
    );

    // Forking into a namespace that is not yours is refused.
    let (st, _) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/secret/forks",
        Some(serde_json::json!({ "org": "ada" })),
    );
    assert!(st == 403 || st == 404, "{st}");

    // Still healthy after every refusal.
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
