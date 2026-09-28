//! Forks, end to end against a real server.
//!
//! The case that matters is deliberately the awkward one: **a person
//! whose fork lands somewhere the source's organization cannot see**.
//! Every repository is private to its organization, so a fork is two
//! authorities at once — reading the source's org and writing the
//! destination's — and a test written the obvious way has the
//! repository's own owner fork into the same org, where one principal
//! holds both and a guard that refused every real forker would pass.
//!
//! So the forker here is **bob**, a *viewer* of `acme`: he may read
//! `acme/widget` and may not push to it, and his fork goes to his own
//! namespace, which nobody at acme can read. Three properties are
//! load-bearing and each is pinned from both sides:
//!
//! * the forker must be able to **read the source** — a person with no
//!   role in its org is told it does not exist, and the same person
//!   forks once invited and cannot once removed;
//! * a fork crosses two organizations, so it takes a **person's
//!   session**: a token is bound to one org and cannot carry the forker
//!   across, while a token forking within its own org still works;
//! * a fork's storage is upstream's storage — so the fork must actually
//!   serve content it never copied, and it must be as private as every
//!   other repository.

use stratum_testkit::browser::Browser;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::mailbox::Mailbox;
use stratum_testkit::{Minio, Server};

const PASSWORD: &str = "a long enough password";

/// What asking for a public repository is told, word for word.
const NO_PUBLIC: &str = "this server has no public repositories: every repository is private \
                         to its organization — omit \"public\" or set it to false";

fn spawn(store_url: &str, scratch: &Scratch, hint: &str, mail: &Mailbox) -> Server {
    // Both fork jobs are background workers, and this suite waits on
    // their results. At the production intervals (5s and 30s) that is
    // most of the runtime of every test here, spent asleep.
    spawn_with(
        store_url,
        scratch,
        hint,
        mail,
        &[
            ("STRATUM_FORK_POLL_SECS", "1"),
            ("STRATUM_PROMOTE_POLL_SECS", "1"),
        ],
    )
}

/// A server with the given environment — the worker switched off, so a
/// fork can be observed in the state it passes through rather than the
/// one it lands in.
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

/// A token minted by `browser` in `org`, optionally bound to one repo.
fn mint(browser: &mut Browser, org: &str, scopes: &[&str], repo: Option<&str>) -> String {
    let mut body = serde_json::json!({ "scopes": scopes, "label": "cli" });
    if let Some(r) = repo {
        body["repo"] = serde_json::json!(r);
    }
    let (st, minted) = browser.req("POST", &format!("/v1/orgs/{org}/tokens"), Some(body));
    assert_eq!(st, 201, "mint a token in {org}: {minted}");
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

/// An organization owned by the person signed in to `owner`.
fn create_org(owner: &mut Browser, name: &str) {
    let (st, body) = owner.req(
        "POST",
        "/v1/orgs",
        Some(serde_json::json!({ "name": name })),
    );
    assert_eq!(st, 201, "create org {name}: {body}");
}

/// A repository in `org`, with one commit in it when `seed` is set.
fn repo(owner: &mut Browser, org: &str, name: &str, seed: bool) {
    let (st, body) = owner.req(
        "POST",
        &format!("/v1/orgs/{org}/repos"),
        Some(serde_json::json!({ "name": name })),
    );
    assert_eq!(st, 201, "{body}");
    if seed {
        let (st, body) = owner.req(
            "POST",
            &format!("/v1/orgs/{org}/repos/{name}/commits"),
            Some(serde_json::json!({
                "message": "first commit",
                "operations": [{ "op": "put", "path": "README.md", "content": "# seed\n" }],
            })),
        );
        assert_eq!(st, 201, "{body}");
    }
}

/// ada, who owns the organization `acme`, and bob, a *viewer* of it.
///
/// bob may read everything in acme and push to none of it. His own
/// namespace is `bob`, where nobody from acme has a role — which is
/// where his forks land, and the whole reason a fork is two
/// authorities rather than one.
fn acme_with_a_viewer<'a>(server: &'a Server, mail: &Mailbox) -> (Browser<'a>, Browser<'a>) {
    let mut ada = signup(server, mail, "ada", "ada@example.com");
    create_org(&mut ada, "acme");
    let bob = signup(server, mail, "bob", "bob@example.com");
    ada.invite_and_accept("acme", "bob@example.com", "viewer");
    (ada, bob)
}

/// The signed-in person's account id, which is what the member routes
/// are addressed by.
fn user_id(browser: &mut Browser) -> String {
    let (st, me) = browser.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    me["id"].as_str().expect("id").to_string()
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

/// The refusals, which are most of what an API is.
///
/// Forking's happy path was covered from the day it landed; none of the
/// ways it says no were. They are product behaviour rather than error
/// propagation, so they want tests.
#[test]
fn forking_into_a_named_namespace_obeys_that_namespace_s_rules() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-named");
    let scratch = Scratch::new("forks-named");
    let mail = Mailbox::temp("forks-named");
    let server = spawn(&bucket.base_url, &scratch, "forks_named", &mail);

    let (mut ada, mut bob) = acme_with_a_viewer(&server, &mail);
    repo(&mut ada, "acme", "widget", true);

    // Naming a target you own works, and is a different code path from
    // the default: every other test in this file lets the target
    // default to the caller's own namespace, so `org` being present had
    // never been exercised at all.
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/forks",
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

    // An organization bob administers is a target too, and this one is
    // the session doing what no token can: reading `acme`, writing
    // `bobco`, as one person, in one request. ada — who owns the
    // source — has no role in bobco and cannot see what landed there.
    create_org(&mut bob, "bobco");
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/forks",
        Some(serde_json::json!({ "org": "bobco" })),
    );
    assert_eq!(st, 202, "a fork into the forker's own org: {body}");
    assert_eq!(await_fork(&mut bob, "/v1/orgs/bobco/repos/widget"), "ready");
    let (st, _) = ada.req("GET", "/v1/orgs/bobco/repos/widget", None);
    assert_eq!(st, 404, "a fork in bob's org was readable from acme");

    // A target that does not exist is a 404 rather than a 500 or a
    // silently-defaulted namespace.
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/forks",
        Some(serde_json::json!({ "org": "no-such-namespace" })),
    );
    assert_eq!(st, 404, "a missing target namespace: {body}");

    // And a target bob may read but not write is refused. Being able to
    // *read* the source says nothing about where the caller may put
    // things — the two authorities are separate, and a viewer of acme
    // is exactly the person who holds one and not the other.
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/forks",
        Some(serde_json::json!({ "org": "acme" })),
    );
    // 404 exactly, not "404 or 403".
    //
    // `authx::require` masks by both doors — `SessionAuth::NoAccess` and
    // the `allows` check each return `not_found()` — so 403 from this
    // seam would be the anomaly rather than an alternative. And the
    // handler resolves `org_or_404` before the authority check, so a
    // namespace that does not exist and one the caller may not write to
    // are indistinguishable *by construction*. An assertion that
    // accepted either code could not tell that version from one that
    // leaks, and a 403 would turn this write endpoint into an existence
    // oracle for organizations anybody is willing to guess the names of.
    assert_eq!(
        st, 404,
        "a viewer forked into the org they only read: {body}"
    );
    // Nothing was created there.
    let (st, _) = ada.req("GET", "/v1/orgs/acme/repos/widget-copy", None);
    assert_eq!(st, 404, "a refused fork left a repository behind");

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

    let (mut ada, mut bob) = acme_with_a_viewer(&server, &mail);
    repo(&mut ada, "acme", "widget", true);

    let (st, first) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "{first}");
    assert_eq!(await_fork(&mut bob, "/v1/orgs/bob/repos/widget"), "ready");

    // The second press is answered with the fork that already exists:
    // 200, not 202, because nothing was created, and the same row, so a
    // client that navigates to `org/name` on success lands on the fork
    // either way.
    let (st, again) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(
        st, 200,
        "forking what you already forked must hand back the fork: {again}"
    );
    assert_eq!(again["id"], first["id"], "{again}");
    assert_eq!(again["org"], "bob", "{again}");
    assert_eq!(again["name"], "widget", "{again}");
    assert_eq!(again["fork_parent"], "acme/widget", "{again}");
    assert_eq!(again["fork_state"], "ready", "{again}");
    // And nothing else appeared: one fork, not a second one under some
    // derived name. Counted from bob's side, because bob is the one
    // person who can read both the source and the fork.
    let (st, forks) = bob.req("GET", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 200, "{forks}");
    assert_eq!(forks["count"], 1, "{forks}");

    // A repository of Bob's that merely shares the name is a genuine
    // collision. The refusal names it and the way past it, instead of
    // a bare "already exists" that reads the same as the case above.
    repo(&mut bob, "bob", "gadget", false);
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/forks",
        Some(serde_json::json!({ "name": "gadget" })),
    );
    assert_eq!(st, 409, "forking onto an unrelated repository: {body}");
    let why = body["error"].as_str().unwrap_or_default();
    assert!(
        why.contains("bob/gadget already exists") && why.contains("not a fork of acme/widget"),
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
        "/v1/orgs/acme/repos/widget/forks",
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
    create_org(&mut ada, "acme");
    repo(&mut ada, "acme", "widget", true);

    // Given read access the only way there is — a role in acme — so the
    // refusal below cannot be the read check wearing another hat.
    let mut unproved = signup_unverified(&server, "unproved", "unproved@example.com");
    ada.invite_and_accept("acme", "unproved@example.com", "viewer");
    let (st, me) = unproved.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    // Read *after* accepting: an invitation to an existing account does
    // not prove its address, and if it ever starts to, this fixture
    // stops proving anything and must say so.
    assert!(
        me["verified_at"].is_null(),
        "the fixture is verified, so this proves nothing: {me}"
    );
    // The invariant: they own a namespace already, so the handler's
    // "nowhere to fork into" refusal is not what stops them.
    assert_eq!(me["handle"], "unproved", "{me}");
    assert!(
        me["orgs"]
            .as_array()
            .is_some_and(|a| a.iter().any(|o| o["name"] == "unproved")),
        "an account arrived without a namespace to fork into: {me}"
    );

    let (st, body) = unproved.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 403, "an unproved address forked a repository: {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("confirm your email"),
        "the refusal must say what to do about it: {body}"
    );
    let (st, _) = unproved.req("GET", "/v1/orgs/unproved/repos/widget", None);
    assert_eq!(st, 404, "a refused fork left a repository behind");

    // Reading is untouched — the gate is on creating, not on existing.
    let (st, body) = unproved.req("GET", "/v1/orgs/acme/repos/widget", None);
    assert_eq!(st, 200, "an unproved member was refused a read: {body}");

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
    // `"public": false` is still accepted — it is what every repository
    // is — so a client that always sends it keeps working.
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

    // A signed-in person from another namespace gets the answer a
    // repository that does not exist gets them — the fork listing must
    // not become a way to ask whether a repository is there.
    let (outsider, _) = bob.req("GET", "/v1/orgs/ada/repos/ledger/forks", None);
    let (outsider_absent, _) = bob.req("GET", "/v1/orgs/ada/repos/no-such/forks", None);
    assert_eq!(outsider, 404, "a signed-in outsider listed a repo's forks");
    assert_eq!(outsider, outsider_absent);
    // Nobody at all is told to sign in, for a real name and a made-up
    // one alike.
    let (stranger, _) = server.req("GET", "/v1/orgs/ada/repos/ledger/forks", "", None);
    let (absent, _) = server.req("GET", "/v1/orgs/ada/repos/no-such/forks", "", None);
    assert_eq!(stranger, 401, "an anonymous caller listed a repo's forks");
    assert_eq!(
        stranger, absent,
        "the fork listing tells a stranger a repository exists: \
         real {stranger}, missing {absent}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A reader forks, and the fork serves history it never copied.
///
/// Was `a_stranger_can_fork_a_public_repository…`: there are no public
/// repositories and no strangers who may read one, so the forker is a
/// viewer of the source's organization — the least authority that can
/// read it at all — and the fork lands in a namespace the source's org
/// has no role in.
#[test]
fn a_reader_forks_and_clones_what_they_never_copied() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-e2e");
    let scratch = Scratch::new("forks-e2e");
    let mail = Mailbox::temp("forks");
    let server = spawn(&bucket.base_url, &scratch, "forks", &mail);

    let (mut ada, mut bob) = acme_with_a_viewer(&server, &mail);
    repo(&mut ada, "acme", "widget", false);

    // Give it real content, so the fork has something to actually serve.
    let work = scratch.path().join("seed");
    gitcli::fixture_repo(&work, 6);
    let tok = mint(
        &mut ada,
        "acme",
        &["repo:read", "repo:write"],
        Some("widget"),
    );
    let url = server.authed_url(&tok, "acme", "widget");
    gitcli::git(&work, &["push", "-q", &url, "main"]);

    // bob may read it and may not push to it.
    let (st, body) = bob.req("GET", "/v1/orgs/acme/repos/widget", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["viewer_write"], false, "{body}");

    // Bob forks it, naming nothing: it goes to his own namespace under
    // the same name.
    let (st, body) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "a reader could not fork: {body}");
    assert_eq!(body["org"], "bob");
    assert_eq!(body["name"], "widget");
    assert_eq!(
        body["fork_state"], "pending",
        "a fork's storage is written by a job; 202 and pending are the honest answer: {body}"
    );

    let state = await_fork(&mut bob, "/v1/orgs/bob/repos/widget");
    assert_eq!(state, "ready", "the fork job did not finish cleanly");

    // The claim, proven the only way that counts: clone the fork over
    // the wire and fsck it. Every object in that pack came out of acme's
    // storage — bob's prefix holds two pointers and nothing else — and
    // I11 says a produced clone must pass `git fsck --full --strict`.
    // With bob's own credential, minted in *his* namespace: it could not
    // read acme, and it does not need to.
    let bob_tok = mint(&mut bob, "bob", &["repo:read"], None);
    let dest = scratch.path().join("cloned-fork");
    gitcli::clone_and_fsck(&server.authed_url(&bob_tok, "bob", "widget"), &dest);
    let log = gitcli::git(&dest, &["log", "--oneline"]);
    assert_eq!(
        log.lines().count(),
        6,
        "the fork did not serve upstream's history: {log}"
    );

    // Serving upstream's bytes does not make the fork upstream's to
    // read: nobody signed out may fetch it, and neither may acme's own
    // credential, which answers exactly as a missing name does.
    let advert = "/bob/widget.git/info/refs?service=git-upload-pack";
    assert_eq!(server.status_get(advert, None), 401);
    assert_eq!(server.status_get(advert, Some(&tok)), 404);
    assert_eq!(
        server.status_get(
            "/bob/nothing.git/info/refs?service=git-upload-pack",
            Some(&tok)
        ),
        404
    );

    // The listing shows it to bob, and the count agrees with the list.
    let (st, body) = bob.req("GET", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["count"], 1, "{body}");
    assert_eq!(body["forks"][0]["org"], "bob");
    assert_eq!(body["forks"][0]["name"], "widget");
    assert_eq!(
        body["forks"].as_array().unwrap().len(),
        body["count"].as_u64().unwrap() as usize,
        "count disagrees with the list beside it: {body}"
    );
    // And the source repository reports the same number to him.
    let (st, body) = bob.req("GET", "/v1/orgs/acme/repos/widget", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["fork_count"], 1, "{body}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// Forking reads one organization and writes another, so it takes a
/// person — a signed-in session — and not a token.
///
/// Was `a_personal_token_can_fork_as_well_as_a_browser_session`. A token
/// is bound to one organization, and a fork into your own namespace is
/// two: a token from the destination cannot read the source, and a
/// token from the source cannot write the destination. Both are refused
/// with the masked 404 — the same answer a name that does not exist
/// gets — and nothing is left behind. The session, which is the person
/// across every org they belong to, is what forks.
///
/// The other half keeps this from being "tokens cannot fork": a token
/// forking *within* its own organization holds both authorities and
/// still works, so the API and CLI are not shut out of forking — only
/// out of crossing an org boundary on a credential that was bound to
/// one.
#[test]
fn forking_crosses_two_organizations_so_it_takes_a_session_not_a_token() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-token");
    let scratch = Scratch::new("forks-token");
    let mail = Mailbox::temp("forks-token");
    let server = spawn(&bucket.base_url, &scratch, "forks-token", &mail);

    let (mut ada, mut bob) = acme_with_a_viewer(&server, &mail);
    repo(&mut ada, "acme", "widget", true);

    // bob's token from his own namespace: it can write where the fork
    // would land and cannot read the source.
    let from_bob = mint(&mut bob, "bob", &["repo:read", "repo:write"], None);
    let (st, out) = server.req("POST", "/v1/orgs/acme/repos/widget/forks", &from_bob, None);
    assert_eq!(
        st, 404,
        "a token bound to bob's namespace read acme's repository: {out}"
    );
    let (absent, _) = server.req("POST", "/v1/orgs/acme/repos/no-such/forks", &from_bob, None);
    assert_eq!(
        absent, st,
        "a foreign token can tell a real name from a made-up one"
    );

    // bob's token from acme: it can read the source and cannot write
    // where the fork would land, whether that is left to default or
    // named.
    let from_acme = mint(&mut bob, "acme", &["repo:read"], None);
    // It does read the source — so the refusals below are the
    // destination's, not a read check wearing another hat.
    let (st, out) = server.req("GET", "/v1/orgs/acme/repos/widget", &from_acme, None);
    assert_eq!(st, 200, "bob's acme token cannot read acme: {out}");
    for body in [None, Some(serde_json::json!({ "org": "bob" }))] {
        let (st, out) = server.req(
            "POST",
            "/v1/orgs/acme/repos/widget/forks",
            &from_acme,
            body.clone(),
        );
        assert_eq!(
            st, 404,
            "a token bound to acme wrote into bob's namespace ({body:?}): {out}"
        );
    }
    // Nothing was made by either attempt.
    let (st, _) = bob.req("GET", "/v1/orgs/bob/repos/widget", None);
    assert_eq!(st, 404, "a refused token fork left a repository behind");

    // A repository-bound token is an automation attached to one
    // repository, not a person with a namespace to fork into, so it is
    // correctly nobody here — even though it carries a real user id and
    // can read the source.
    let bound = mint(
        &mut ada,
        "acme",
        &["repo:read", "repo:write"],
        Some("widget"),
    );
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/widget/forks",
        &bound,
        Some(serde_json::json!({ "org": "acme", "name": "widget-ci-fork" })),
    );
    assert_eq!(st, 401, "a repository-bound token forked: {out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("signed-in person"),
        "{out}"
    );
    let (st, _) = ada.req("GET", "/v1/orgs/acme/repos/widget-ci-fork", None);
    assert_eq!(st, 404, "a refused token fork left a repository behind");

    // Within one organization a token holds both authorities, and forks.
    // Deliberately carrying no cookie: a handler that resolved identity
    // from the session alone would refuse the whole API here.
    let org_wide = mint(&mut ada, "acme", &["repo:read", "repo:write"], None);
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/widget/forks",
        &org_wide,
        Some(serde_json::json!({ "org": "acme", "name": "widget-experiment" })),
    );
    assert_eq!(st, 202, "a token could not fork within its own org: {out}");
    assert_eq!(
        await_fork(&mut ada, "/v1/orgs/acme/repos/widget-experiment"),
        "ready"
    );

    // And bob's session, which is bob in every org he belongs to, does
    // what neither of his tokens could.
    let (st, out) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "a person's session could not fork: {out}");
    assert_eq!(await_fork(&mut bob, "/v1/orgs/bob/repos/widget"), "ready");
    // The fork is his in every sense, so the token from his own
    // namespace — useless against acme — reads it.
    let dest = scratch.path().join("token-clone");
    gitcli::clone_and_fsck(&server.authed_url(&from_bob, "bob", "widget"), &dest);

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A fork is exactly as private as every other repository: private to
/// the namespace it lands in, and never publishable.
///
/// Was `a_fork_of_a_private_repository_is_private_and_stays_that_way`,
/// whose question — may a zero-copy fork of a private repository be
/// published? — no longer has a "yes" anywhere to guard against. What is
/// left is the same property made total: a fork serves upstream's
/// actual bytes, so a fork that could be read outside its namespace
/// would publish acme's objects to whoever could read it, and the
/// request that would do so is refused by name. The `fork_parent` half
/// survives unchanged in spirit — an upstream is named only to somebody
/// who may read it — and is now reached by the one road that exists:
/// losing the role that let you read it.
#[test]
fn a_fork_is_private_to_its_namespace_and_cannot_be_published() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-private");
    let scratch = Scratch::new("forks-private");
    let mail = Mailbox::temp("forks-private");
    let server = spawn(&bucket.base_url, &scratch, "forks-private", &mail);

    let (mut ada, mut bob) = acme_with_a_viewer(&server, &mail);
    repo(&mut ada, "acme", "widget", true);

    let (st, body) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    // There is no visibility to report, so none is: a `public: false`
    // on the wire would be a field that can only ever say one thing.
    assert!(body.get("public").is_none(), "{body}");
    assert!(body.get("write_blocked").is_none(), "{body}");
    assert_eq!(await_fork(&mut bob, "/v1/orgs/bob/repos/widget"), "ready");

    // Publishing is refused by name, on the fork and on its upstream
    // alike — and saying it is private is not a refusal.
    let (st, body) = bob.req(
        "PATCH",
        "/v1/orgs/bob/repos/widget",
        Some(serde_json::json!({ "public": true })),
    );
    assert_eq!(st, 400, "a fork was published: {body}");
    assert_eq!(body["error"], NO_PUBLIC, "{body}");
    let (st, body) = ada.req(
        "PATCH",
        "/v1/orgs/acme/repos/widget",
        Some(serde_json::json!({ "public": true })),
    );
    assert_eq!(st, 400, "an upstream was published: {body}");
    assert_eq!(body["error"], NO_PUBLIC, "{body}");
    let (st, body) = bob.req(
        "PATCH",
        "/v1/orgs/bob/repos/widget",
        Some(serde_json::json!({ "public": false })),
    );
    assert_eq!(st, 200, "saying a fork is private was refused: {body}");

    // Bob may read the upstream, so his fork says whose history it is
    // carrying. Found in the app: a member forking their own org's
    // repository got a fork page that said "Forking the upstream…"
    // without naming the repository they had pressed the button on.
    let (st, body) = bob.req("GET", "/v1/orgs/bob/repos/widget", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["fork_parent"], "acme/widget", "{body}");

    // The fork is bob's namespace's and nobody else's — not even the
    // owner of the repository it was forked from, and not anybody
    // signed out.
    let (st, _) = ada.req("GET", "/v1/orgs/bob/repos/widget", None);
    assert_eq!(st, 404, "the upstream's owner read somebody's fork");
    let (st, _) = server.req("GET", "/v1/orgs/bob/repos/widget", "", None);
    assert_eq!(st, 401, "an anonymous caller read a fork");

    // ada takes bob's role in acme away. His fork stays his, and stops
    // naming a repository he can no longer read: the fork is still a
    // fork, it simply does not say whose.
    let bob_id = user_id(&mut bob);
    let (st, out) = ada.req("DELETE", &format!("/v1/orgs/acme/members/{bob_id}"), None);
    assert_eq!(st, 204, "removing bob from acme: {out}");
    let (st, _) = bob.req("GET", "/v1/orgs/acme/repos/widget", None);
    assert_eq!(st, 404, "a removed member still reads acme");
    let (st, body) = bob.req("GET", "/v1/orgs/bob/repos/widget", None);
    assert_eq!(st, 200, "{body}");
    assert!(
        body["fork_parent"].is_null(),
        "an upstream was named to somebody who may no longer read it: {body}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The fork count is "the forks you may read", and nothing else.
///
/// Was `a_private_fork_is_not_advertised_to_strangers`, which toggled a
/// fork private and watched it leave the list. Every fork is private
/// now, so the property is sharper: the count and the list follow the
/// *reader's* roles, not the rows. A fork in bob's namespace is never
/// counted for acme's own owner — publishing a number larger than the
/// list would say precisely how many forks she is not being shown — and
/// a fork in an org she is let into appears the moment she is, and
/// leaves the moment she is not.
#[test]
fn a_fork_is_counted_only_for_people_who_may_read_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-hidden");
    let scratch = Scratch::new("forks-hidden");
    let mail = Mailbox::temp("forks-hidden");
    let server = spawn(&bucket.base_url, &scratch, "forks-hidden", &mail);

    let (mut ada, mut bob) = acme_with_a_viewer(&server, &mail);
    repo(&mut ada, "acme", "widget", true);
    create_org(&mut bob, "bobco");

    // Two forks: one in bob's own namespace, one in an org of his.
    let (st, body) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    await_fork(&mut bob, "/v1/orgs/bob/repos/widget");
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/forks",
        Some(serde_json::json!({ "org": "bobco" })),
    );
    assert_eq!(st, 202, "{body}");
    await_fork(&mut bob, "/v1/orgs/bobco/repos/widget");

    let listing = |b: &mut Browser| -> (Vec<String>, u64, u64) {
        let (st, list) = b.req("GET", "/v1/orgs/acme/repos/widget/forks", None);
        assert_eq!(st, 200, "{list}");
        let (st, repo) = b.req("GET", "/v1/orgs/acme/repos/widget", None);
        assert_eq!(st, 200, "{repo}");
        let mut names: Vec<String> = list["forks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                format!(
                    "{}/{}",
                    f["org"].as_str().unwrap(),
                    f["name"].as_str().unwrap()
                )
            })
            .collect();
        names.sort();
        assert_eq!(
            names.len() as u64,
            list["count"].as_u64().unwrap(),
            "the count disagrees with the list beside it: {list}"
        );
        (
            names,
            list["count"].as_u64().unwrap(),
            repo["fork_count"].as_u64().unwrap(),
        )
    };

    // bob reads both.
    assert_eq!(
        listing(&mut bob),
        (
            vec!["bob/widget".to_string(), "bobco/widget".to_string()],
            2,
            2
        )
    );
    // ada owns the upstream and reads neither, so neither is counted.
    assert_eq!(listing(&mut ada), (vec![], 0, 0));

    // Let her into bobco, and exactly that fork appears.
    bob.invite_and_accept("bobco", "ada@example.com", "viewer");
    assert_eq!(listing(&mut ada), (vec!["bobco/widget".to_string()], 1, 1));
    // And leaves with her role.
    let ada_id = user_id(&mut ada);
    let (st, out) = bob.req("DELETE", &format!("/v1/orgs/bobco/members/{ada_id}"), None);
    assert_eq!(st, 204, "{out}");
    assert_eq!(listing(&mut ada), (vec![], 0, 0));

    // Somebody with no role in acme does not get a smaller list; they
    // get the answer a missing repository gets.
    let mut carl = signup(&server, &mail, "carl", "carl@example.com");
    let (st, _) = carl.req("GET", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 404);
    let (st, _) = server.req("GET", "/v1/orgs/acme/repos/widget/forks", "", None);
    assert_eq!(st, 401);

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn deleting_a_fork_takes_it_out_of_the_count_immediately() {
    // A tombstoned repository leaves the listing at once, so a count
    // that outlived it would be wrong for the whole grace window — the
    // same leak as a fork the reader may not see, where a number larger
    // than the list says how many things you are not being shown.
    //
    // What actually guarantees this is the `state = 'active'` filter in
    // `forks::forks_of`, which both the list and the count are read
    // from. It is deliberately **not** `repos.rs::delete` calling
    // `forks::detach`: `detach` keeps the denormalised `repos.fork_count`
    // column in step for the promotion and re-pointing jobs, and nothing
    // on the read path depends on it.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-deleted");
    let scratch = Scratch::new("forks-deleted");
    let mail = Mailbox::temp("forks-deleted");
    let server = spawn(&bucket.base_url, &scratch, "forks-deleted", &mail);

    let (mut ada, mut bob) = acme_with_a_viewer(&server, &mail);
    repo(&mut ada, "acme", "widget", true);

    let (st, body) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    await_fork(&mut bob, "/v1/orgs/bob/repos/widget");
    // Read from bob's side: he is the one person who may read both the
    // upstream and the fork, so his is the view the fork is counted in.
    assert_eq!(
        bob.req("GET", "/v1/orgs/acme/repos/widget", None).1["fork_count"],
        1
    );

    // Bob deletes his fork.
    let (st, _) = bob.req("DELETE", "/v1/orgs/bob/repos/widget", None);
    assert_eq!(st, 204);

    let (st, body) = bob.req("GET", "/v1/orgs/acme/repos/widget", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        body["fork_count"], 0,
        "a deleted fork was still counted against its upstream: {body}"
    );
    let (st, body) = bob.req("GET", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["count"], 0, "{body}");
    assert!(body["forks"].as_array().unwrap().is_empty(), "{body}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn deleting_an_upstream_promotes_its_forks_instead_of_stranding_them() {
    // A zero-copy fork reads upstream's objects and `epoch_refs` stops
    // them being collected — absolutely, because the foreign key
    // RESTRICTs. So before promotion existed, deleting an upstream that
    // had forks produced a repository the sweeper declined on every
    // tick, forever, holding storage for something nobody could reach.
    //
    // What must be true afterwards: the fork still serves its full
    // history, and it does so from storage of its own.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-promote-e2e");
    let scratch = Scratch::new("forks-promote-e2e");
    let mail = Mailbox::temp("forks-promote-e2e");
    let server = spawn(&bucket.base_url, &scratch, "forks-promote-e2e", &mail);

    let (mut ada, mut bob) = acme_with_a_viewer(&server, &mail);
    repo(&mut ada, "acme", "widget", false);

    let work = scratch.path().join("seed");
    gitcli::fixture_repo(&work, 6);
    let tok = mint(
        &mut ada,
        "acme",
        &["repo:read", "repo:write"],
        Some("widget"),
    );
    let url = server.authed_url(&tok, "acme", "widget");
    gitcli::git(&work, &["push", "-q", &url, "main"]);

    let (st, body) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    assert_eq!(await_fork(&mut bob, "/v1/orgs/bob/repos/widget"), "ready");

    // Ada deletes the upstream. This must succeed — what changes is that
    // bob's fork survives it.
    let (st, _) = ada.req("DELETE", "/v1/orgs/acme/repos/widget", None);
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
    let bob_tok = mint(&mut bob, "bob", &["repo:read"], None);
    let dest = scratch.path().join("promoted-clone");
    gitcli::clone_and_fsck(&server.authed_url(&bob_tok, "bob", "widget"), &dest);
    let log = gitcli::git(&dest, &["log", "--oneline"]);
    assert_eq!(
        log.lines().count(),
        6,
        "a promoted fork lost history its upstream used to hold: {log}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn forking_something_that_is_not_there_and_forking_with_a_bad_credential() {
    // Two refusals that happen before any of the interesting logic, and
    // both are the first thing a client hitting this endpoint by hand
    // will do. A repository that does not exist must answer exactly as
    // one you may not see does, and a malformed credential must be
    // refused rather than quietly treated as anonymous.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-absent");
    let scratch = Scratch::new("forks-absent");
    let mail = Mailbox::temp("forks-absent");
    let server = spawn(&bucket.base_url, &scratch, "forks-absent", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    create_org(&mut ada, "acme");
    repo(&mut ada, "acme", "widget", true);

    // No such repository, in a namespace that does exist.
    let (st, _) = ada.req("POST", "/v1/orgs/acme/repos/nosuchthing/forks", None);
    assert_eq!(
        st, 404,
        "forking a repository that is not there was not a 404"
    );

    // No such namespace either.
    let (st, _) = ada.req("POST", "/v1/orgs/nobody/repos/widget/forks", None);
    assert_eq!(st, 404);

    // A credential that is not one: 401, told to fix it — and told so
    // identically for a repository that exists and one that does not,
    // or a made-up token would be an existence oracle.
    let (real, _) = server.req(
        "POST",
        "/v1/orgs/acme/repos/widget/forks",
        "weft_not_a_real_token",
        None,
    );
    let (missing, _) = server.req(
        "POST",
        "/v1/orgs/acme/repos/nosuchthing/forks",
        "weft_not_a_real_token",
        None,
    );
    assert_eq!(real, 401, "a malformed credential produced {real}");
    assert_eq!(missing, real, "a malformed credential can probe names");

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

    let (mut ada, mut bob) = acme_with_a_viewer(&server, &mail);
    repo(&mut ada, "acme", "widget", false);

    let (st, body) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    assert_eq!(
        await_fork(&mut bob, "/v1/orgs/bob/repos/widget"),
        "ready",
        "forking an empty repository left the fork unfinished"
    );

    // And it holds no reference against upstream: there was no epoch to
    // pin, so deleting upstream needs no promotion at all.
    let (st, _) = ada.req("DELETE", "/v1/orgs/acme/repos/widget", None);
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

    let (mut ada, mut bob) = acme_with_a_viewer(&server, &mail);
    repo(&mut ada, "acme", "widget", true);

    let (st, body) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    // A negative with no observable of its own: two seconds is twice
    // the interval every other test in this file runs the worker at.
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

    let (mut ada, mut bob) = acme_with_a_viewer(&server, &mail);
    repo(&mut ada, "acme", "widget", true);
    // Minted before the restart: a token outlives the process where a
    // session cookie's `Browser` does not. Minted in bob's own
    // namespace, which is where the fork is and all it needs to read.
    let bob_tok = mint(&mut bob, "bob", &["repo:read", "repo:write"], None);

    let (st, body) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    let (st, _) = ada.req("DELETE", "/v1/orgs/acme/repos/widget", None);
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

    let (mut ada, mut bob) = acme_with_a_viewer(&server, &mail);
    repo(&mut ada, "acme", "widget", true);

    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/forks",
        Some(serde_json::json!({ "name": "not a valid name!!" })),
    );
    assert_eq!(st, 400, "a rejected fork name was not a refusal: {body}");
    // And nothing was left behind under it.
    let (st, _) = bob.req("GET", "/v1/orgs/bob/repos/not%20a%20valid%20name!!", None);
    assert_eq!(st, 404);
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// You may fork what you may read, and reading is a role in the
/// source's organization.
///
/// The door this whole feature hangs on, pinned from both sides and
/// across a change of role, so neither "forking is refused" nor "forking
/// is open" can pass it: carl, with no role in acme, is told the
/// repository does not exist — in the words a made-up name gets him;
/// the same carl, invited as a viewer, forks; the same carl, removed,
/// cannot fork it again.
#[test]
fn forking_takes_a_person_and_a_repository_you_may_read() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-refusals");
    let scratch = Scratch::new("forks-refusals");
    let mail = Mailbox::temp("forks-refusals");
    let server = spawn(&bucket.base_url, &scratch, "forks-refusals", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    create_org(&mut ada, "acme");
    repo(&mut ada, "acme", "widget", true);
    let mut carl = signup(&server, &mail, "carl", "carl@example.com");

    // Somebody signed in with no role in acme cannot fork what they
    // cannot see, and is told the same thing a missing name tells them.
    let (st, _) = carl.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 404, "a repository was forkable by a non-member");
    let (absent, _) = carl.req("POST", "/v1/orgs/acme/repos/no-such/forks", None);
    assert_eq!(absent, st);
    let (st, _) = carl.req("GET", "/v1/orgs/carl/repos/widget", None);
    assert_eq!(st, 404, "a refused fork left a repository behind");

    // Nobody at all is told to sign in, for a real name and a made-up
    // one alike.
    let (st, _) = server.req("POST", "/v1/orgs/acme/repos/widget/forks", "", None);
    assert_eq!(st, 401, "an anonymous fork was not told to sign in");
    let (absent, _) = server.req("POST", "/v1/orgs/acme/repos/no-such/forks", "", None);
    assert_eq!(absent, st);

    // A viewer's role is enough to read, and so enough to fork.
    ada.invite_and_accept("acme", "carl@example.com", "viewer");
    let (st, body) = carl.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "a viewer could not fork what they read: {body}");
    assert_eq!(await_fork(&mut carl, "/v1/orgs/carl/repos/widget"), "ready");

    // Take the role away and the door shuts again, in the same words.
    let carl_id = user_id(&mut carl);
    let (st, out) = ada.req("DELETE", &format!("/v1/orgs/acme/members/{carl_id}"), None);
    assert_eq!(st, 204, "{out}");
    let (st, body) = carl.req(
        "POST",
        "/v1/orgs/acme/repos/widget/forks",
        Some(serde_json::json!({ "name": "widget-again" })),
    );
    assert_eq!(st, 404, "a removed member forked again: {body}");
    let (st, _) = carl.req("GET", "/v1/orgs/carl/repos/widget-again", None);
    assert_eq!(st, 404, "a refused fork left a repository behind");

    // Still healthy after every refusal.
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
