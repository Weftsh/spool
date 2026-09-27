//! The contribution path: a stranger proposes code to a project they
//! cannot push to.
//!
//! Until this existed, a project hosted here could be **read** by
//! anybody and **contributed to** by nobody outside its org.
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
//! The awkward case is the only one worth testing, for the same reason
//! `forks_e2e` insists on it: a test where the repository's own owner
//! opens the change passes against a guard that would refuse every real
//! contributor.

use stratum_testkit::browser::Browser;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::mailbox::Mailbox;
use stratum_testkit::{Minio, Server};

const PASSWORD: &str = "a long enough password";

fn spawn(store_url: &str, scratch: &Scratch, hint: &str, mail: &Mailbox) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        // The contribution walker, so a landed change can be followed
        // all the way to the credit it is supposed to produce.
        .env("STRATUM_CONTRIB_POLL_SECS", "1");
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

fn signup<'a>(server: &'a Server, mail: &Mailbox, handle: &str, email: &str) -> Browser<'a> {
    let (st, body) = server.req(
        "POST",
        "/v1/auth/signup",
        "",
        Some(serde_json::json!({
            "handle": handle, "email": email, "name": handle, "password": PASSWORD,
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

fn public_repo(owner: &mut Browser, handle: &str, name: &str) {
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

#[test]
fn an_outsider_forks_pushes_and_opens_a_change_against_a_repo_they_cannot_write() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forkpr-open");
    let scratch = Scratch::new("forkpr-open");
    let mail = Mailbox::temp("forkpr-open");
    let server = spawn(&bucket.base_url, &scratch, "forkpr_open", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    public_repo(&mut ada, "ada", "widget");

    // Bob has no membership in `ada` and no grant on the repository.
    // He may read it because it is public, and that is all.
    let (st, _) = bob.req("GET", "/v1/orgs/ada/repos/widget", None);
    assert_eq!(st, 200, "a public repository must be readable by anybody");

    let (st, body) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    assert_eq!(await_fork(&mut bob, "/v1/orgs/bob/repos/widget"), "ready");

    // He writes to the repository he owns — an ordinary authorised push
    // to his own fork. Nothing about push authorisation changes here,
    // and that is the point: the contribution never touches upstream's
    // write path.
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/bob/repos/widget/commits",
        Some(serde_json::json!({
            "message": "fix: handle an empty config\n\nIt panicked instead of defaulting.",
            "branch": "fix-empty-config",
            "operations": [
                { "op": "put", "path": "src/config.rs", "content": "// defaults\n" }
            ],
        })),
    );
    assert_eq!(st, 201, "push to own fork: {body}");

    // And proposes it upstream. This is the request that used to be
    // impossible.
    let (st, opened) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/changes",
        Some(serde_json::json!({
            "from": "fix-empty-config",
            "source": "bob/widget",
        })),
    );
    assert_eq!(
        st, 201,
        "an outsider could not open a change from their own fork — this is \
         the contribution path, not an edge case: {opened}"
    );
    assert_eq!(opened["change"]["state"], "open", "{opened}");

    // Upstream sees it, and sees where it came from. GitHub prints this
    // as "bob wants to merge 1 commit into ada:main from bob:…"; the
    // fields that sentence is built from have to be on the wire.
    let (st, listed) = ada.req("GET", "/v1/orgs/ada/repos/widget/changes", None);
    assert_eq!(st, 200, "{listed}");
    let first = &listed["changes"][0];
    assert_eq!(first["title"], "fix: handle an empty config", "{listed}");
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
/// Found by driving the app end to end: a stranger's token on a public
/// upstream got `fatal: repository … not found` from `git clone`, while
/// an anonymous clone of the same URL succeeded. The wire seam masked
/// every cross-org repository as absent, public or not — R8 written for
/// a world with no forks, where nobody outside an org had a reason to
/// hold a token and fetch from it. With forks, the two people who
/// most need that fetch are the forker keeping their fork current and
/// the maintainer fetching a contributor's branch to try it locally,
/// and both were told the repository did not exist.
///
/// A push by someone who may only read got the same "not found". The
/// REST seam already answers a public repository to anybody, so masking
/// the wire protected nothing; it just sent a would-be contributor
/// looking for a typo. Now: a verified principal reads any public
/// repository over HTTP and SSH; a push by somebody who can read but not
/// write is refused with the reason and the way forward; a private
/// repository they cannot read stays absent, in both directions.
#[test]
fn a_forker_reads_upstream_with_their_own_token_and_is_told_to_fork_on_push() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forkpr-wire");
    let scratch = Scratch::new("forkpr-wire");
    let mail = Mailbox::temp("forkpr-wire");
    let server = spawn(&bucket.base_url, &scratch, "forkpr_wire", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    public_repo(&mut ada, "ada", "widget");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "vault" })),
    );
    assert_eq!(st, 201, "{body}");

    // Bob's credential lives in *his* namespace. It says nothing about
    // `ada`, and that is exactly the token a forker has in their remote.
    let (st, minted) = bob.req(
        "POST",
        "/v1/orgs/bob/tokens",
        Some(serde_json::json!({ "scopes": ["repo:read", "repo:write"], "label": "laptop" })),
    );
    assert_eq!(st, 201, "{minted}");
    let bob_tok = minted["token"].as_str().expect("token").to_string();

    // Read: the clone works *with* the credential, and fscks clean.
    let work = Scratch::new("forkpr-wire-clone");
    let dir = work.path().join("widget");
    let head = gitcli::clone_and_fsck(&server.authed_url(&bob_tok, "ada", "widget"), &dir);
    assert!(
        !head.is_empty(),
        "clone of a public upstream with a foreign token"
    );

    // Push: refused with a sentence, not with a 404. The advert is where
    // git first hears it, so the refusal travels in-band and git prints
    // it as `remote error:`.
    std::fs::write(dir.join("PATCH.md"), "a patch\n").unwrap();
    gitcli::git(&dir, &["add", "PATCH.md"]);
    gitcli::git(&dir, &["commit", "-q", "-m", "patch"]);
    let err = gitcli::git_expect_err(&dir, &["push", "-q", "origin", "HEAD:main"])
        .expect("a reader's push to upstream must be refused");
    assert!(
        err.contains("you can read ada/widget but not push to it"),
        "a reader pushing to a public upstream must be told why and what to do, \
         not that the repository is missing:\n{err}"
    );
    assert!(err.contains("fork it"), "{err}");
    assert!(!err.contains("not found"), "{err}");

    // A client that skips the advert and posts the RPC directly meets a
    // 403 with the same sentence — the door is refused, not hidden.
    let resp = ureq::post(&format!("{}/ada/widget/git-receive-pack", server.base))
        .set("Authorization", &format!("Bearer {bob_tok}"))
        .set("Content-Type", "application/x-git-receive-pack-request")
        .send_bytes(b"0000");
    match resp {
        Err(ureq::Error::Status(403, r)) => {
            let t = r.into_string().unwrap();
            assert!(
                t.contains("you can read ada/widget but not push to it"),
                "{t}"
            );
        }
        other => panic!("a reader's raw receive-pack RPC: {other:?}"),
    }

    // Private stays private: Bob cannot read `vault`, so both doors say
    // it does not exist — the read and the write answer alike, or the
    // write door would confirm the name.
    for service in ["git-upload-pack", "git-receive-pack"] {
        let st = server.status_get(
            &format!("/ada/vault.git/info/refs?service={service}"),
            Some(&bob_tok),
        );
        assert_eq!(
            st, 404,
            "{service} advert on a private repo for an outsider"
        );
    }
    let err = gitcli::git_expect_err(
        work.path(),
        &[
            "clone",
            "-q",
            &server.authed_url(&bob_tok, "ada", "vault"),
            "vault",
        ],
    )
    .expect("a private repository must not clone for an outsider");
    assert!(err.contains("not found"), "{err}");
    assert!(
        !err.contains("you can read"),
        "a private repo must not admit it exists: {err}"
    );

    // Upstream is untouched by any of it: trunk is still the seed commit
    // the clone started from, not the commit Bob tried to push.
    let tried = gitcli::git(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    let (st, branches) = ada.req("GET", "/v1/orgs/ada/repos/widget/branches", None);
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
    let mail = Mailbox::temp("forkpr-nosource");
    let server = spawn(&bucket.base_url, &scratch, "forkpr_nosource", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    public_repo(&mut ada, "ada", "widget");

    let (st, refused) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/changes",
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

    // The owner is unaffected — the enterprise flow does not change.
    let (st, ok) = ada.req(
        "POST",
        "/v1/orgs/ada/repos/widget/changes",
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
    let mail = Mailbox::temp("forkpr-source");
    let server = spawn(&bucket.base_url, &scratch, "forkpr_source", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    public_repo(&mut ada, "ada", "widget");
    public_repo(&mut ada, "ada", "other");

    // A bare name, which is ambiguous between a repo and a namespace.
    let (st, refused) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/changes",
        Some(serde_json::json!({ "from": "main", "source": "widget" })),
    );
    assert_eq!(st, 400, "{refused}");

    // A real repository the caller can read, that is not a fork of this
    // one.
    let (st, refused) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/changes",
        Some(serde_json::json!({ "from": "main", "source": "ada/other" })),
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

    // A private repository the caller cannot read is **masked**, not
    // refused with a reason — 404 and never 403, because a 403 would
    // confirm it exists.
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "secret", "public": false })),
    );
    assert_eq!(st, 201, "{body}");
    let (st, masked) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/changes",
        Some(serde_json::json!({ "from": "main", "source": "ada/secret" })),
    );
    assert_eq!(
        st, 404,
        "naming a private repository as a source told the caller it exists: {masked}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
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
    let mail = Mailbox::temp("forkpr-land");
    let server = spawn(&bucket.base_url, &scratch, "forkpr_land", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    public_repo(&mut ada, "ada", "widget");

    let (st, body) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    assert_eq!(await_fork(&mut bob, "/v1/orgs/bob/repos/widget"), "ready");

    // Branch from the fork's trunk first. A commit on a *new* branch
    // with no base is a root commit, and a root commit is correctly not
    // a fast-forward of anything — which is a real refusal, just not the
    // one this test is about.
    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/bob/repos/widget/branches",
        Some(serde_json::json!({ "name": "fix-empty-config", "from": "main" })),
    );
    assert!(
        st == 200 || st == 201,
        "branch from the fork's trunk: {body}"
    );
    let (st, pushed) = bob.req(
        "POST",
        "/v1/orgs/bob/repos/widget/commits",
        Some(serde_json::json!({
            "message": "fix: default an empty config",
            "branch": "fix-empty-config",
            "operations": [
                { "op": "put", "path": "src/config.rs", "content": "// defaults\n" }
            ],
        })),
    );
    assert_eq!(st, 201, "{pushed}");
    let contributed = pushed["commit"].as_str().expect("commit").to_string();

    let (st, opened) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/changes",
        Some(serde_json::json!({ "from": "fix-empty-config", "source": "bob/widget" })),
    );
    assert_eq!(st, 201, "{opened}");
    let key = opened["change"]["key"].as_str().expect("key").to_string();

    // The maintainer approves and lands. Bob cannot: approving and
    // landing stay owner-governed, which is the whole reason opening a
    // change can be opened up safely.
    let (st, refused) = bob.req(
        "POST",
        &format!("/v1/orgs/ada/repos/widget/changes/{key}/land"),
        None,
    );
    assert!(
        st == 403 || st == 404,
        "a contributor landed their own change: {st} {refused}"
    );

    let (st, _) = ada.req(
        "POST",
        &format!("/v1/orgs/ada/repos/widget/changes/{key}/approve"),
        None,
    );
    assert_eq!(st, 204);
    let (st, queued) = ada.req(
        "POST",
        &format!("/v1/orgs/ada/repos/widget/changes/{key}/land"),
        None,
    );
    assert_eq!(st, 202, "{queued}");

    // Wait for the lander.
    let path = format!("/v1/orgs/ada/repos/widget/changes/{key}");
    let mut landed = serde_json::Value::Null;
    for _ in 0..150 {
        let (st, out) = ada.req("GET", &path, None);
        assert_eq!(st, 200, "{out}");
        if out["change"]["state"] != serde_json::json!("landing") {
            landed = out;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert_eq!(
        landed["change"]["state"], "landed",
        "a change from a fork did not land: {landed}"
    );
    assert_eq!(landed["change"]["landed_commit"], contributed, "{landed}");

    // The whole point, proved the only way it can be: clone the
    // upstream repository with the real git CLI and fsck it. The commit
    // came from a prefix this repository never owned.
    let work = Scratch::new("forkpr-land-clone");
    let url = format!("{}/ada/widget.git", server.base);
    let dir = work.path().join("clone");
    // `clone_and_fsck` is the house gate: it clones with the real git
    // CLI and runs `fsck --full --strict`, and returns HEAD.
    let head = gitcli::clone_and_fsck(&url, &dir);
    assert_eq!(head, contributed, "trunk is not the landed commit");
    // And the file the contributor added is actually in the clone —
    // fsck proves the objects are sound, not that the right ones came.
    assert!(
        dir.join("src/config.rs").exists(),
        "the contributed file is missing from a clone that fsck'd clean"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// Poll a contributions graph until it says what we are waiting for.
fn graph_until(
    server: &Server,
    handle: &str,
    what: &str,
    want: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    let mut last = serde_json::Value::Null;
    while std::time::Instant::now() < deadline {
        let (st, body) = server.req(
            "GET",
            &format!("/v1/users/{handle}/contributions"),
            "",
            None,
        );
        assert_eq!(st, 200, "graph for {handle}: {body}");
        if want(&body) {
            return body;
        }
        last = body;
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    panic!("{what}; last graph was {last}");
}

/// Poll a repository's contributor rail the same way.
fn contributors_until(
    server: &Server,
    org: &str,
    repo: &str,
    what: &str,
    want: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    let mut last = serde_json::Value::Null;
    while std::time::Instant::now() < deadline {
        let (st, body) = server.req(
            "GET",
            &format!("/v1/orgs/{org}/repos/{repo}/contributors"),
            "",
            None,
        );
        assert_eq!(st, 200, "contributors of {org}/{repo}: {body}");
        if want(&body) {
            return body;
        }
        last = body;
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    panic!("{what}; last answer was {last}");
}

fn total(graph: &serde_json::Value) -> i64 {
    graph["days"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|d| d["count"].as_i64())
        .sum()
}

/// **The claim the whole contribution path rests on: your name stays on
/// it.**
///
/// `a_change_from_a_fork_lands_and_the_clone_is_still_sound` proves an
/// outsider's change lands. `contribs_e2e` proves a proved address
/// colours a square and ranks in the contributor rail. Neither crosses
/// over, so the sentence a contributor actually cares about — *I
/// contributed to that project and it says so* — had no test at all,
/// and the join is exactly where it can break.
///
/// Landing today fast-forwards the target to the contributor's own
/// commit, so authorship survives by construction. That is precisely
/// why this needs pinning rather than assuming: the day somebody adds
/// squash-on-land, or a merge commit, or rewrites the author to the
/// person who pressed the button, every outside contributor's credit
/// silently moves to the maintainer who reviewed it — the graph goes
/// blank, the rail names the wrong person, and all twenty-seven fork and
/// contribution tests stay green, because not one of them looks at both
/// halves.
///
/// Both readings are asserted, because they come from different code:
/// the per-person graph is walked from the commit's author line, and the
/// repository's rail is aggregated per repo. A regression could easily
/// take one and leave the other.
#[test]
fn a_change_landed_from_a_fork_credits_the_contributor_and_not_the_maintainer() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forkpr-credit");
    let scratch = Scratch::new("forkpr-credit");
    let mail = Mailbox::temp("forkpr-credit");
    let server = spawn(&bucket.base_url, &scratch, "forkpr_credit", &mail);

    // Signup proves the address, which is what makes a square legal:
    // `contribs_e2e` covers the anti-gaming rule, and this test relies
    // on it rather than restating it.
    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    public_repo(&mut ada, "ada", "widget");

    // Ada's own seed commit is hers, and Bob has nothing yet. Taken
    // before anything else happens, so the assertions below are about
    // what *this* change did rather than about a number that was
    // already there.
    let ada_before = total(&graph_until(
        &server,
        "ada",
        "ada's own seed never landed",
        |g| total(g) >= 1,
    ));
    let bob_before = total(&graph_until(
        &server,
        "bob",
        "bob's graph never answered",
        |_| true,
    ));
    assert_eq!(bob_before, 0, "bob has contributed nothing yet");

    let (st, body) = bob.req("POST", "/v1/orgs/ada/repos/widget/forks", None);
    assert_eq!(st, 202, "{body}");
    assert_eq!(await_fork(&mut bob, "/v1/orgs/bob/repos/widget"), "ready");

    let (st, body) = bob.req(
        "POST",
        "/v1/orgs/bob/repos/widget/branches",
        Some(serde_json::json!({ "name": "fix-empty-config", "from": "main" })),
    );
    assert!(st == 200 || st == 201, "{body}");
    let (st, pushed) = bob.req(
        "POST",
        "/v1/orgs/bob/repos/widget/commits",
        Some(serde_json::json!({
            "message": "fix: default an empty config",
            "branch": "fix-empty-config",
            "operations": [
                { "op": "put", "path": "src/config.rs", "content": "// defaults\n" }
            ],
        })),
    );
    assert_eq!(st, 201, "{pushed}");
    let contributed = pushed["commit"].as_str().expect("commit").to_string();

    let (st, opened) = bob.req(
        "POST",
        "/v1/orgs/ada/repos/widget/changes",
        Some(serde_json::json!({ "from": "fix-empty-config", "source": "bob/widget" })),
    );
    assert_eq!(st, 201, "{opened}");
    let key = opened["change"]["key"].as_str().expect("key").to_string();

    // Ada reviews and lands it. She is the one who presses the button
    // and she must not be the one who gets the credit.
    let (st, _) = ada.req(
        "POST",
        &format!("/v1/orgs/ada/repos/widget/changes/{key}/approve"),
        None,
    );
    assert_eq!(st, 204);
    let (st, queued) = ada.req(
        "POST",
        &format!("/v1/orgs/ada/repos/widget/changes/{key}/land"),
        None,
    );
    assert_eq!(st, 202, "{queued}");

    let path = format!("/v1/orgs/ada/repos/widget/changes/{key}");
    let mut landed = serde_json::Value::Null;
    for _ in 0..200 {
        let (st, out) = ada.req("GET", &path, None);
        assert_eq!(st, 200, "{out}");
        if out["change"]["state"] != serde_json::json!("landing") {
            landed = out;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert_eq!(landed["change"]["state"], "landed", "{landed}");
    assert_eq!(landed["change"]["landed_commit"], contributed, "{landed}");

    // --- the claim -----------------------------------------------------

    // Bob's graph gains the commit he wrote.
    let bob_graph = graph_until(
        &server,
        "bob",
        "the contributor's graph never gained the change they wrote",
        |g| total(g) > bob_before,
    );
    assert_eq!(
        total(&bob_graph),
        bob_before + 1,
        "the contributor was credited for something other than their one commit: {bob_graph}"
    );

    // And Ada's does **not**. Landing is review, not authorship — a
    // maintainer who merges a hundred contributions has written none of
    // them, and a graph that says otherwise is a lie about who did the
    // work. Read after Bob's has already moved, so this is a settled
    // state and not a race with a walker that had not got there yet.
    let ada_graph = graph_until(&server, "ada", "ada's graph never answered", |_| true);
    assert_eq!(
        total(&ada_graph),
        ada_before,
        "the maintainer was credited for a contribution they only reviewed: {ada_graph}"
    );

    // The repository's own rail agrees, and it is aggregated by a
    // different query than the graph — one could regress without the
    // other.
    let rail = contributors_until(
        &server,
        "ada",
        "widget",
        "the contributor never appeared on the repository's rail",
        |b| {
            b["contributors"]
                .as_array()
                .map(|a| a.iter().any(|c| c["handle"] == serde_json::json!("bob")))
                .unwrap_or(false)
        },
    );
    let bob_row = rail["contributors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["handle"] == serde_json::json!("bob"))
        .cloned()
        .expect("bob is on the rail");
    assert_eq!(
        bob_row["commits"],
        serde_json::json!(1),
        "the rail credits the contributor with the wrong number: {rail}"
    );

    // A stranger reads the same thing, signed out. A credit only its
    // owner can see is not a credit.
    let (st, public) = server.req("GET", "/v1/orgs/ada/repos/widget/contributors", "", None);
    assert_eq!(st, 200, "{public}");
    assert!(
        public["contributors"]
            .as_array()
            .map(|a| a.iter().any(|c| c["handle"] == serde_json::json!("bob")))
            .unwrap_or(false),
        "a signed-out reader cannot see who contributed: {public}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
