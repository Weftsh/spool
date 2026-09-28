//! The contribution path: somebody proposes code to a project they
//! cannot push to.
//!
//! Until this existed, a project hosted here could be **read** by people
//! who could not push to it and **contributed to** by none of them.
//! `changes_api::create` required `repo:write`, so the only people who
//! could open a change were the people who could already push — which
//! is everybody who does not need a review tool and nobody who does.
//! Forks shipped, and the door they were supposed to open stayed shut.
//!
//! GitHub states the whole shape in one sentence at the top of a pull
//! request: *"DaZuiZui wants to merge 1 commit into vitejs:main from
//! DaZuiZui:codex/fix-worker-manifest"*. The contributor never had write
//! access to `vitejs/vite`. That sentence is what this file makes true.
//!
//! Every repository is private to its organization, so the contributor
//! is **bob, a viewer of `acme`**: he may read `acme/widget`, fork it into
//! his own namespace, and propose from there, and he may not push to it.
//! The awkward case is the only one worth testing, for the same reason
//! `forks_e2e` insists on it: a test where the repository's own owner
//! opens the change passes against a guard that would refuse every real
//! contributor. And because that contributor's `run:` lines are about to
//! meet somebody's machine, the last test here is the fork gate.

use std::time::{Duration, Instant};
use stratum_testkit::browser::Browser;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::{Minio, Server};

fn spawn(store_url: &str, scratch: &Scratch, hint: &str) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        // The contribution walker, so a landed change can be followed
        // all the way to the credit it is supposed to produce.
        .env("STRATUM_CONTRIB_POLL_SECS", "1")
        .env("STRATUM_FORK_POLL_SECS", "1")
        // A runner's claim is a long poll; shortened so "nothing for
        // you" is a fast 204 rather than twenty seconds of test.
        .env("STRATUM_RUNNER_POLL_SECS", "1")
        .env("STRATUM_RUNNER_CLAIM_WAIT_MS", "700")
        .start()
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

/// A repository in `org` with one commit in it.
fn seeded_repo(owner: &mut Browser, org: &str, name: &str) {
    let (st, body) = owner.req(
        "POST",
        &format!("/v1/orgs/{org}/repos"),
        Some(serde_json::json!({ "name": name })),
    );
    assert_eq!(st, 201, "{body}");
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

/// ada, who owns `acme` and its seeded `widget`, and bob, a *viewer* of
/// acme: he may read `widget` and may not push to it.
fn acme_with_a_viewer<'a>(server: &'a Server) -> (Browser<'a>, Browser<'a>) {
    let mut ada = Browser::stranger(server, "ada", "ada@example.com");
    create_org(&mut ada, "acme");
    seeded_repo(&mut ada, "acme", "widget");
    let bob = Browser::stranger(server, "bob", "bob@example.com");
    ada.invite_and_accept("acme", "bob@example.com", "viewer");
    (ada, bob)
}

/// A token minted by `browser` in `org`.
fn mint(browser: &mut Browser, org: &str, scopes: &[&str]) -> String {
    let (st, minted) = browser.req(
        "POST",
        &format!("/v1/orgs/{org}/tokens"),
        Some(serde_json::json!({ "scopes": scopes, "label": "laptop" })),
    );
    assert_eq!(st, 201, "mint in {org}: {minted}");
    minted["token"].as_str().expect("token").to_string()
}

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

/// bob forks `acme/widget`, branches off its trunk and commits there —
/// on a branch, because a commit on a *new* branch with no base is a
/// root commit, and a root commit is correctly not a fast-forward of
/// anything. Returns the contributed commit.
fn contribute(bob: &mut Browser, files: &[(&str, &str)]) -> String {
    let (st, body) = bob.req("POST", "/v1/orgs/acme/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    assert_eq!(await_fork(bob, "/v1/orgs/bob/repos/widget"), "ready");
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/bob/repos/widget/branches",
        Some(serde_json::json!({ "name": "fix-empty-config", "from": "main" })),
    );
    assert!(
        st == 200 || st == 201,
        "branch from the fork's trunk: {body}"
    );
    let ops: Vec<serde_json::Value> = files
        .iter()
        .map(|(p, c)| serde_json::json!({ "op": "put", "path": p, "content": c }))
        .collect();
    let (st, pushed) = bob.req(
        "POST",
        "/v1/orgs/bob/repos/widget/commits",
        Some(serde_json::json!({
            "message": "fix: default an empty config",
            "branch": "fix-empty-config",
            "operations": ops,
        })),
    );
    assert_eq!(st, 201, "{pushed}");
    pushed["commit"].as_str().expect("commit").to_string()
}

/// Was `an_outsider_forks_pushes_and_opens_a_change_against_a_repo_they_cannot_write`.
/// There is no outsider who may read a repository any more; the least
/// authority that can is a viewer, and that is who contributes here.
/// The outsider survives as the negative: somebody with no role in acme
/// cannot learn the repository is there, let alone propose to it.
#[test]
fn a_reader_forks_pushes_and_opens_a_change_against_a_repo_they_cannot_write() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forkpr-open");
    let scratch = Scratch::new("forkpr-open");
    let server = spawn(&bucket.base_url, &scratch, "forkpr_open");

    let (mut ada, mut bob) = acme_with_a_viewer(&server);

    // Bob holds a viewer's role in acme and no grant on the repository.
    // He may read it, and that is all — the server says so itself.
    let (st, view) = bob.req("GET", "/v1/orgs/acme/repos/widget", None);
    assert_eq!(st, 200, "a viewer could not read the repository: {view}");
    assert_eq!(view["viewer_member"], true, "{view}");
    assert_eq!(view["viewer_write"], false, "{view}");

    // Somebody with no role in acme is told nothing: the repository, its
    // forks and its changes answer as a missing name does, and a caller
    // who is not signed in is told to sign in.
    let mut carl = Browser::stranger(&server, "carl", "carl@example.com");
    for path in [
        "/v1/orgs/acme/repos/widget",
        "/v1/orgs/acme/repos/widget/forks",
        "/v1/orgs/acme/repos/widget/changes",
    ] {
        let (st, _) = carl.req("GET", path, None);
        assert_eq!(st, 404, "a non-member reached {path}");
        let (st, _) = server.req("GET", path, "", None);
        assert_eq!(st, 401, "an anonymous caller reached {path}");
    }
    let (st, _) = carl.req(
        "POST",
        "/v1/orgs/acme/repos/widget/changes",
        Some(serde_json::json!({ "from": "main", "source": "carl/widget" })),
    );
    assert_eq!(st, 404, "a non-member proposed a change");

    // He writes to the repository he owns — an ordinary authorised push
    // to his own fork. Nothing about push authorisation changes here,
    // and that is the point: the contribution never touches upstream's
    // write path.
    contribute(&mut bob, &[("src/config.rs", "// defaults\n")]);

    // And proposes it upstream. This is the request that used to be
    // impossible.
    let (st, opened) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/changes",
        Some(serde_json::json!({
            "from": "fix-empty-config",
            "source": "bob/widget",
        })),
    );
    assert_eq!(
        st, 201,
        "a reader could not open a change from their own fork — this is \
         the contribution path, not an edge case: {opened}"
    );
    assert_eq!(opened["change"]["state"], "open", "{opened}");

    // Upstream sees it, and sees where it came from. GitHub prints this
    // as "bob wants to merge 1 commit into acme:main from bob:…"; the
    // fields that sentence is built from have to be on the wire.
    let (st, listed) = ada.req("GET", "/v1/orgs/acme/repos/widget/changes", None);
    assert_eq!(st, 200, "{listed}");
    let first = &listed["changes"][0];
    assert_eq!(first["title"], "fix: default an empty config", "{listed}");
    assert_eq!(
        first["source"], "bob/widget",
        "a change from a fork must say which fork: {listed}"
    );
    assert_eq!(first["target_branch"], "main", "{listed}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The same contribution path over the git wire, with the contributor's
/// own credential in the URL — which is how every real clone arrives.
///
/// Was `a_forker_reads_upstream_with_their_own_token_…`, whose "own
/// token" lived in the forker's namespace and read a public upstream.
/// A token is bound to one organization now, so the credential that
/// reads acme is one bob minted in acme — his viewer's role, and not a
/// byte more. The rest stands: a push by somebody who can read but not
/// write is refused with the reason and the way forward, not with "not
/// found", because the repository is not hidden from *him*. And the
/// token from bob's own namespace, the one in his fork's remote, is a
/// credential for another organization — so acme's repository is absent
/// to it, in both directions, exactly as a repository in an org bob has
/// no role in is absent to his acme token.
#[test]
fn a_forker_reads_upstream_with_a_token_from_its_org_and_is_told_to_fork_on_push() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forkpr-wire");
    let scratch = Scratch::new("forkpr-wire");
    let server = spawn(&bucket.base_url, &scratch, "forkpr_wire");

    let (mut ada, mut bob) = acme_with_a_viewer(&server);
    // An organization bob has no role in, with a repository in it.
    create_org(&mut ada, "globex");
    seeded_repo(&mut ada, "globex", "vault");

    let acme_tok = mint(&mut bob, "acme", &["repo:read"]);
    let bob_tok = mint(&mut bob, "bob", &["repo:read", "repo:write"]);

    // Read: the clone works *with* the credential, and fscks clean.
    let work = Scratch::new("forkpr-wire-clone");
    let dir = work.path().join("widget");
    let head = gitcli::clone_and_fsck(&server.authed_url(&acme_tok, "acme", "widget"), &dir);
    assert!(!head.is_empty(), "clone of upstream with a viewer's token");

    // Push: refused with a sentence, not with a 404. The advert is where
    // git first hears it, so the refusal travels in-band and git prints
    // it as `remote error:`.
    std::fs::write(dir.join("PATCH.md"), "a patch\n").unwrap();
    gitcli::git(&dir, &["add", "PATCH.md"]);
    gitcli::git(&dir, &["commit", "-q", "-m", "patch"]);
    let err = gitcli::git_expect_err(&dir, &["push", "-q", "origin", "HEAD:main"])
        .expect("a reader's push to upstream must be refused");
    assert!(
        err.contains("you can read acme/widget but not push to it"),
        "a reader pushing upstream must be told why and what to do, \
         not that the repository is missing:\n{err}"
    );
    assert!(err.contains("fork it"), "{err}");
    assert!(!err.contains("not found"), "{err}");

    // A client that skips the advert and posts the RPC directly meets a
    // 403 with the same sentence — the door is refused, not hidden.
    let resp = ureq::post(&format!("{}/acme/widget/git-receive-pack", server.base))
        .set("Authorization", &format!("Bearer {acme_tok}"))
        .set("Content-Type", "application/x-git-receive-pack-request")
        .send_bytes(b"0000");
    match resp {
        Err(ureq::Error::Status(403, r)) => {
            let t = r.into_string().unwrap();
            assert!(
                t.contains("you can read acme/widget but not push to it"),
                "{t}"
            );
        }
        other => panic!("a reader's raw receive-pack RPC: {other:?}"),
    }

    // A credential for another organization reads nothing here, and is
    // told so in the words a missing name gets: bob's acme token against
    // globex, and bob's own-namespace token against acme. The read and
    // the write door answer alike, or the write door would confirm the
    // name.
    for (tok, org, repo) in [
        (&acme_tok, "globex", "vault"),
        (&acme_tok, "globex", "nothing-here"),
        (&bob_tok, "acme", "widget"),
    ] {
        for service in ["git-upload-pack", "git-receive-pack"] {
            let st = server.status_get(
                &format!("/{org}/{repo}.git/info/refs?service={service}"),
                Some(tok),
            );
            assert_eq!(
                st, 404,
                "{service} advert on {org}/{repo} for another org's token"
            );
        }
        let err = gitcli::git_expect_err(
            work.path(),
            &["clone", "-q", &server.authed_url(tok, org, repo), "absent"],
        )
        .expect("another org's token must not clone");
        assert!(err.contains("not found"), "{err}");
        assert!(
            !err.contains("you can read"),
            "a repository admitted it exists to another org's token: {err}"
        );
    }
    // Nobody at all is asked for credentials, whatever the name.
    for repo in ["widget", "nothing-here"] {
        let st = server.status_get(
            &format!("/acme/{repo}.git/info/refs?service=git-upload-pack"),
            None,
        );
        assert_eq!(st, 401, "anonymous advert on acme/{repo}");
    }

    // Upstream is untouched by any of it: trunk is still the seed commit
    // the clone started from, not the commit Bob tried to push.
    let tried = gitcli::git(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    let (st, branches) = ada.req("GET", "/v1/orgs/acme/repos/widget/branches", None);
    assert_eq!(st, 200, "{branches}");
    let main = branches["branches"]
        .as_array()
        .expect("branches")
        .iter()
        .find(|b| b["name"] == "main")
        .expect("main");
    assert_eq!(main["oid"], head, "{branches}");
    assert_ne!(main["oid"], tried, "{branches}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn proposing_from_the_target_itself_still_needs_write_access() {
    // The other half of the rule, and the half that keeps this from
    // being a hole. Opening a change with no `source` says "these
    // commits are already in your repository" — which could only be
    // true if somebody pushed them, and pushing is authorised. Reading
    // a repository must not become a way to create review objects on it.
    let minio = Minio::shared();
    let bucket = minio.bucket("forkpr-nosource");
    let scratch = Scratch::new("forkpr-nosource");
    let server = spawn(&bucket.base_url, &scratch, "forkpr_nosource");

    let (mut ada, mut bob) = acme_with_a_viewer(&server);

    let (st, refused) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/changes",
        Some(serde_json::json!({ "from": "main" })),
    );
    assert_eq!(
        st, 403,
        "a reader opened a change on somebody else's repository: {refused}"
    );
    // The refusal has to teach the way through, or it is a dead end for
    // exactly the person the feature is for.
    let message = refused["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("fork"),
        "the refusal did not tell a contributor what to do instead: {message}"
    );

    // A person with no role at all gets no sentence: the repository is
    // not there for them, as a missing one is not.
    let mut carl = Browser::stranger(&server, "carl", "carl@example.com");
    let (st, _) = carl.req(
        "POST",
        "/v1/orgs/acme/repos/widget/changes",
        Some(serde_json::json!({ "from": "main" })),
    );
    let (absent, _) = carl.req(
        "POST",
        "/v1/orgs/acme/repos/no-such/changes",
        Some(serde_json::json!({ "from": "main" })),
    );
    assert_eq!(st, 404, "a non-member was told the repository exists");
    assert_eq!(absent, st);

    // The owner is unaffected — the enterprise flow does not change.
    let (st, ok) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/widget/changes",
        Some(serde_json::json!({ "from": "main" })),
    );
    assert!(st == 200 || st == 201, "the owner's own flow broke: {ok}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_source_must_be_a_readable_fork_of_this_repository() {
    // Landing copies objects out of the source's prefix into the
    // target's, so naming an arbitrary repository here is a request to
    // move somebody else's bytes into a project they do not own. Each
    // refusal below is a different question, and answering them alike
    // would make this endpoint an existence oracle.
    let minio = Minio::shared();
    let bucket = minio.bucket("forkpr-source");
    let scratch = Scratch::new("forkpr-source");
    let server = spawn(&bucket.base_url, &scratch, "forkpr_source");

    let (mut ada, mut bob) = acme_with_a_viewer(&server);
    seeded_repo(&mut ada, "acme", "other");

    // A bare name, which is ambiguous between a repo and a namespace.
    let (st, refused) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/changes",
        Some(serde_json::json!({ "from": "main", "source": "widget" })),
    );
    assert_eq!(st, 400, "{refused}");

    // A real repository the caller can read, that is not a fork of this
    // one.
    let (st, refused) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/changes",
        Some(serde_json::json!({ "from": "main", "source": "acme/other" })),
    );
    assert_eq!(
        st, 400,
        "an unrelated repository was accepted as a source: {refused}"
    );
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not a fork"),
        "{refused}"
    );

    // A repository the caller cannot read is **masked**, not refused
    // with a reason — 404 and never 403, because a 403 would confirm it
    // exists. carl's own repository is one bob has no role anywhere near,
    // and it answers exactly as a name nobody took.
    let mut carl = Browser::stranger(&server, "carl", "carl@example.com");
    seeded_repo(&mut carl, "carl", "secret");
    for source in ["carl/secret", "carl/no-such"] {
        let (st, masked) = bob.req(
            "POST",
            "/v1/orgs/acme/repos/widget/changes",
            Some(serde_json::json!({ "from": "main", "source": source })),
        );
        assert_eq!(
            st, 404,
            "naming {source} as a source told the caller something: {masked}"
        );
    }

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// Poll the change until the lander has finished with it.
fn await_landed(ada: &mut Browser, key: &str) -> serde_json::Value {
    let path = format!("/v1/orgs/acme/repos/widget/changes/{key}");
    for _ in 0..200 {
        let (st, out) = ada.req("GET", &path, None);
        assert_eq!(st, 200, "{out}");
        if out["change"]["state"] != serde_json::json!("landing") {
            return out;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    panic!("the change never left landing");
}

/// A change from a fork lands, and the repository it landed into is
/// still a sound git repository.
///
/// This is the end of the contribution path and the assertion that
/// matters is the last one: **I11, every clone passes
/// `git fsck --full --strict`.** A fork is zero-copy, so its new commits
/// live in its own prefix while `refops::transact` writes refs in the
/// target's. Landing without moving the objects would produce a CAS that
/// succeeds and a repository whose trunk points at bytes that are not
/// there — a clone that fails on the next `fsck` rather than an error
/// anybody sees at land time.
#[test]
fn a_change_from_a_fork_lands_and_the_clone_is_still_sound() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forkpr-land");
    let scratch = Scratch::new("forkpr-land");
    let server = spawn(&bucket.base_url, &scratch, "forkpr_land");

    let (mut ada, mut bob) = acme_with_a_viewer(&server);
    let contributed = contribute(&mut bob, &[("src/config.rs", "// defaults\n")]);

    let (st, opened) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/changes",
        Some(serde_json::json!({ "from": "fix-empty-config", "source": "bob/widget" })),
    );
    assert_eq!(st, 201, "{opened}");
    let key = opened["change"]["key"].as_str().expect("key").to_string();

    // The maintainer approves and lands. Bob cannot: approving and
    // landing stay owner-governed, which is the whole reason opening a
    // change can be opened up safely. 404, because `rest_repo_auth`
    // masks a repository from anybody who cannot do the thing asked.
    let (st, refused) = bob.req(
        "POST",
        &format!("/v1/orgs/acme/repos/widget/changes/{key}/land"),
        None,
    );
    assert_eq!(st, 404, "a contributor landed their own change: {refused}");

    let (st, _) = ada.req(
        "POST",
        &format!("/v1/orgs/acme/repos/widget/changes/{key}/approve"),
        None,
    );
    assert_eq!(st, 204);
    let (st, queued) = ada.req(
        "POST",
        &format!("/v1/orgs/acme/repos/widget/changes/{key}/land"),
        None,
    );
    assert_eq!(st, 202, "{queued}");

    let landed = await_landed(&mut ada, &key);
    assert_eq!(
        landed["change"]["state"], "landed",
        "a change from a fork did not land: {landed}"
    );
    assert_eq!(landed["change"]["landed_commit"], contributed, "{landed}");

    // The whole point, proved the only way it can be: clone the
    // upstream repository with the real git CLI and fsck it. The commit
    // came from a prefix this repository never owned.
    let tok = mint(&mut ada, "acme", &["repo:read"]);
    let work = Scratch::new("forkpr-land-clone");
    let dir = work.path().join("clone");
    // `clone_and_fsck` is the house gate: it clones with the real git
    // CLI and runs `fsck --full --strict`, and returns HEAD.
    let head = gitcli::clone_and_fsck(&server.authed_url(&tok, "acme", "widget"), &dir);
    assert_eq!(head, contributed, "trunk is not the landed commit");
    // And the file the contributor added is actually in the clone —
    // fsck proves the objects are sound, not that the right ones came.
    assert!(
        dir.join("src/config.rs").exists(),
        "the contributed file is missing from a clone that fsck'd clean"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// Poll a repository's contributor rail, as `who`, until it says what
/// we are waiting for.
fn contributors_until(
    who: &mut Browser,
    what: &str,
    want: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    let mut last = serde_json::Value::Null;
    while std::time::Instant::now() < deadline {
        let (st, body) = who.req("GET", "/v1/orgs/acme/repos/widget/contributors", None);
        assert_eq!(st, 200, "contributors of acme/widget: {body}");
        if want(&body) {
            return body;
        }
        last = body;
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    panic!("{what}; last answer was {last}");
}

fn commits_of(rail: &serde_json::Value, handle: &str) -> Option<i64> {
    rail["contributors"]
        .as_array()?
        .iter()
        .find(|c| c["handle"] == serde_json::json!(handle))
        .and_then(|c| c["commits"].as_i64())
}

/// **The claim the whole contribution path rests on: your name stays on
/// it.**
///
/// `a_change_from_a_fork_lands_and_the_clone_is_still_sound` proves a
/// contributor's change lands. Neither it nor anything about contributors
/// crosses over, so the sentence a contributor actually cares about — *I
/// contributed to that project and it says so* — had no test at all,
/// and the join is exactly where it can break.
///
/// Landing today fast-forwards the target to the contributor's own
/// commit, so authorship survives by construction. That is precisely
/// why this needs pinning rather than assuming: the day somebody adds
/// squash-on-land, or a merge commit, or rewrites the author to the
/// person who pressed the button, every contributor's credit silently
/// moves to the maintainer who reviewed it.
///
/// The per-person contribution graph this used to read as well is gone
/// with the social layer; the repository's contributor rail is what is
/// left, and it is read here by the people who may read the repository
/// — and refused to the people who may not, the same as the repository
/// itself.
#[test]
fn a_change_landed_from_a_fork_credits_the_contributor_and_not_the_maintainer() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forkpr-credit");
    let scratch = Scratch::new("forkpr-credit");
    let server = spawn(&bucket.base_url, &scratch, "forkpr_credit");

    // Every account's address is proved when the account is made, which
    // is what makes a commit count for a person rather than for an
    // address nobody owns.
    let (mut ada, mut bob) = acme_with_a_viewer(&server);

    // ada's own seed commit is hers, and bob has nothing yet. Taken
    // before anything else happens, so the assertions below are about
    // what *this* change did rather than about a number that was
    // already there.
    let before = contributors_until(&mut ada, "ada's own seed never counted", |r| {
        commits_of(r, "ada").is_some()
    });
    let ada_before = commits_of(&before, "ada").unwrap();
    assert_eq!(commits_of(&before, "bob"), None, "{before}");

    let contributed = contribute(&mut bob, &[("src/config.rs", "// defaults\n")]);
    let (st, opened) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/changes",
        Some(serde_json::json!({ "from": "fix-empty-config", "source": "bob/widget" })),
    );
    assert_eq!(st, 201, "{opened}");
    let key = opened["change"]["key"].as_str().expect("key").to_string();

    // Ada reviews and lands it. She is the one who presses the button
    // and she must not be the one who gets the credit.
    let (st, _) = ada.req(
        "POST",
        &format!("/v1/orgs/acme/repos/widget/changes/{key}/approve"),
        None,
    );
    assert_eq!(st, 204);
    let (st, queued) = ada.req(
        "POST",
        &format!("/v1/orgs/acme/repos/widget/changes/{key}/land"),
        None,
    );
    assert_eq!(st, 202, "{queued}");
    let landed = await_landed(&mut ada, &key);
    assert_eq!(landed["change"]["state"], "landed", "{landed}");
    assert_eq!(landed["change"]["landed_commit"], contributed, "{landed}");

    // --- the claim -----------------------------------------------------

    // Bob is on the rail with the one commit he wrote…
    let rail = contributors_until(
        &mut ada,
        "the contributor never appeared on the repository's rail",
        |r| commits_of(r, "bob").is_some(),
    );
    assert_eq!(
        commits_of(&rail, "bob"),
        Some(1),
        "the rail credits the contributor with the wrong number: {rail}"
    );
    // …and ada's count has **not** moved. Landing is review, not
    // authorship — a maintainer who merges a hundred contributions has
    // written none of them. Read after bob's row has appeared, so this is
    // a settled state and not a race with a walker that had not got there
    // yet.
    assert_eq!(
        commits_of(&rail, "ada"),
        Some(ada_before),
        "the maintainer was credited for a contribution they only reviewed: {rail}"
    );

    // The contributor reads the same thing: a credit its owner cannot
    // see is not a credit.
    let seen = contributors_until(&mut bob, "the contributor cannot see their credit", |r| {
        commits_of(r, "bob") == Some(1)
    });
    assert_eq!(commits_of(&seen, "ada"), Some(ada_before), "{seen}");

    // And the rail is exactly as private as the repository: a person
    // with no role in acme is told it is not there, and nobody signed in
    // is told to sign in.
    let mut carl = Browser::stranger(&server, "carl", "carl@example.com");
    let (st, _) = carl.req("GET", "/v1/orgs/acme/repos/widget/contributors", None);
    assert_eq!(st, 404, "a non-member read who contributes to acme");
    let (st, _) = server.req("GET", "/v1/orgs/acme/repos/widget/contributors", "", None);
    assert_eq!(st, 401, "an anonymous caller read who contributes to acme");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

// ---------------------------------------------------------------------
// The fork gate
// ---------------------------------------------------------------------

/// A workflow with no `runs-on`, which means `[self-hosted]`: the one
/// kind of machine there is.
const CI: &str = "\
name: ci
on: [push, change]
jobs:
  test:
    steps:
      - name: Work
        run: echo hi
";

/// Ask for a job the way a registered machine does. `None` is the long
/// poll coming back with nothing for it.
fn claim(server: &Server, credential: &str) -> Option<(String, String)> {
    let (st, out) = server.post("/v1/runners/claim", credential, None);
    match st {
        200 => Some((
            out["job_id"].as_str().unwrap().to_string(),
            out["token"].as_str().unwrap().to_string(),
        )),
        204 => None,
        other => panic!("claim answered {other}: {out}"),
    }
}

fn claim_within(server: &Server, credential: &str, what: &str) -> (String, String) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(job) = claim(server, credential) {
            return job;
        }
        assert!(Instant::now() < deadline, "waited 30s for {what}");
    }
}

/// Poll `path` (a workflow-runs listing) until a run for `sha` has left
/// `queued`/`running`, and return it.
fn settled_run(who: &mut Browser, path: &str, sha: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (st, out) = who.req("GET", &format!("{path}?commit_sha={sha}"), None);
        assert_eq!(st, 200, "{out}");
        if let Some(r) = out["runs"].as_array().and_then(|r| r.first()) {
            if r["state"] != "queued" && r["state"] != "running" {
                return r.clone();
            }
        }
        assert!(Instant::now() < deadline, "no settled run at {sha}: {out}");
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// A change from a fork is held for a maintainer before its workflows
/// reach anybody's machine — and a maintainer, and only a maintainer,
/// lets it go.
///
/// Every machine here is somebody's own: registered by an organization,
/// on its hardware, on its network. A fork's change carries `run:` lines
/// its author wrote, and bob — a viewer who may read `widget` and fork
/// it — is exactly who that author is. acme's machine is listening
/// throughout and takes ada's own push first, so "nothing ran" is the
/// server deciding and not a machine that was absent; and bob's push to
/// his own fork, where nobody has a machine, settles failed in the words
/// any repository without one gets — the gate is not a stand-in for
/// "no runner".
#[test]
fn a_change_from_a_fork_is_held_for_a_maintainer_before_it_reaches_a_machine() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forkpr-gate");
    let scratch = Scratch::new("forkpr-gate");
    let server = spawn(&bucket.base_url, &scratch, "forkpr_gate");

    let (mut ada, mut bob) = acme_with_a_viewer(&server);

    // acme's machine, in the default group, which admits every
    // repository — so the only thing that can hold bob's change is the
    // fork gate.
    let (st, minted) = ada.req("POST", "/v1/orgs/acme/runners/registration-token", None);
    assert_eq!(st, 201, "{minted}");
    let (st, reg) = server.post(
        "/v1/runners/register",
        minted["token"].as_str().unwrap(),
        Some(serde_json::json!({
            "name": "acme-box", "labels": [], "os": "linux", "arch": "x64",
            "version": "0.1.0-test", "ephemeral": false,
        })),
    );
    assert_eq!(st, 201, "{reg}");
    let machine = reg["credential"].as_str().unwrap().to_string();

    // ada's own push runs on it — the stack is proven good before the
    // fork case, so a held change cannot be a misconfiguration passing
    // as a gate.
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/widget/commits",
        Some(serde_json::json!({
            "message": "add ci",
            "operations": [{ "op": "put", "path": ".weft/ci.yml", "content": CI }],
        })),
    );
    assert_eq!(st, 201, "{body}");
    let (job, token) = claim_within(&server, &machine, "ada's own push");
    let (st, out) = server.post(
        &format!("/v1/runner/jobs/{job}/finish"),
        &token,
        Some(serde_json::json!({ "state": "passed" })),
    );
    assert_eq!(st, 200, "{out}");

    let sha = contribute(&mut bob, &[("src/lib.rs", "// hi\n")]);
    // bob's push to his own fork ran nowhere: acme's machine is acme's,
    // and his namespace has none.
    let own = settled_run(&mut bob, "/v1/orgs/bob/repos/widget/workflow-runs", &sha);
    assert_eq!(own["state"], "failed", "{own}");
    assert_eq!(
        own["error"], "no runner with labels [self-hosted] is registered for this repository",
        "{own}"
    );

    let (st, opened) = bob.req(
        "POST",
        "/v1/orgs/acme/repos/widget/changes",
        Some(serde_json::json!({ "from": "fix-empty-config", "source": "bob/widget" })),
    );
    assert_eq!(st, 201, "{opened}");
    let key = opened["change"]["key"].as_str().expect("key").to_string();

    // Held, with the fork's own commit and sentence, and no job made.
    let held = settled_run(&mut ada, "/v1/orgs/acme/repos/widget/workflow-runs", &sha);
    assert_eq!(held["state"], "blocked", "{held}");
    assert_eq!(held["event"], "change", "{held}");
    assert_eq!(held["blocked_reason"], "fork", "{held}");
    assert_eq!(
        held["error"],
        "this change comes from a fork; a maintainer has to approve its workflows before they run",
        "{held}"
    );
    assert!(
        held["jobs"].as_array().is_some_and(|j| j.is_empty()),
        "{held}"
    );
    // And the listening machine is offered nothing. Three long polls:
    // the negative has no observable of its own.
    for _ in 0..3 {
        assert_eq!(
            claim(&server, &machine),
            None,
            "a fork's change reached acme's machine"
        );
    }

    // The contributor cannot let their own contribution run, and is
    // refused exactly as landing it would be.
    let approve = format!("/v1/orgs/acme/repos/widget/changes/{key}/workflows/approve");
    let (st, out) = bob.req("POST", &approve, None);
    assert_eq!(st, 404, "bob approved his own change's workflows: {out}");
    let (land, _) = bob.req(
        "POST",
        &format!("/v1/orgs/acme/repos/widget/changes/{key}/land"),
        None,
    );
    assert_eq!(land, st, "approval and landing must refuse alike");
    assert_eq!(
        claim(&server, &machine),
        None,
        "a refused approval ran something"
    );

    // The maintainer approves, and the machine is handed bob's commit.
    let (st, out) = ada.req("POST", &approve, None);
    assert_eq!(st, 202, "{out}");
    let (job, token) = claim_within(&server, &machine, "the approved fork change");
    let (st, spec) = server.get(&format!("/v1/runner/jobs/{job}"), &token);
    assert_eq!(st, 200, "{spec}");
    assert_eq!(spec["commit_sha"], sha, "{spec}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
