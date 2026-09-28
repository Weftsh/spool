//! Connecting GitHub once, and picking a repository instead of typing an
//! id.
//!
//! The install round trip goes out through a browser and comes back as a
//! URL anybody can construct, so most of this file is about the ways
//! that callback can be abused: a forged state, a replayed one, an
//! expired one, and an installation that belongs to somebody else.

use std::path::{Path, PathBuf};
use stratum_testkit::{fake_github, gitcli, gitcli::Scratch, Minio, Server};

/// A bare origin on disk, the way `mirror_e2e` makes one: a working repo
/// with real commits, cloned bare, with HEAD pointed at main.
fn make_origin(root: &Path, full_name: &str, commits: usize) -> (PathBuf, String) {
    let work = root.join("work").join(full_name.replace('/', "-"));
    let tip = gitcli::fixture_repo(&work, commits);
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
    (bare, tip)
}

/// A server with a GitHub App pointed at the fake, and its git base at a
/// local directory so nothing in here reaches the network.
fn spawn(store_url: &str, scratch: &Scratch, hint: &str, api_base: &str) -> Server {
    builder(store_url, scratch, hint, api_base)
        // The App requests user authorization during installation, so
        // the callback can ask GitHub whose installation it was handed.
        .env("STRATUM_GITHUB_CLIENT_ID", "Iv1.test")
        .env("STRATUM_GITHUB_CLIENT_SECRET", "test-client-secret")
        .env("STRATUM_GITHUB_OAUTH_BASE", api_base)
        .start()
}

/// The same App with no OAuth client: a private, single-tenant install
/// where the callback trusts the installation id it is handed.
fn spawn_without_user_auth(
    store_url: &str,
    scratch: &Scratch,
    hint: &str,
    api_base: &str,
) -> Server {
    builder(store_url, scratch, hint, api_base).start()
}

fn builder(
    store_url: &str,
    scratch: &Scratch,
    hint: &str,
    api_base: &str,
) -> stratum_testkit::server::ServerBuilder {
    let key_path = scratch.path().join("app-key.pem");
    std::fs::write(&key_path, fake_github::TEST_APP_KEY_PEM).unwrap();
    let origins = scratch.path().join("origins");
    std::fs::create_dir_all(&origins).unwrap();
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_GITHUB_APP_ID", "12345")
        .env("STRATUM_GITHUB_APP_KEY_PEM", key_path.display().to_string())
        .env("STRATUM_GITHUB_API_BASE", api_base)
        .env(
            "STRATUM_GITHUB_GIT_BASE",
            format!("file://{}", origins.display()),
        )
        .env(
            "STRATUM_GITHUB_INSTALL_URL",
            "https://github.com/apps/stratum/installations/new",
        )
}

/// A person who owns `org`, signed in — the shape a real
/// connect arrives in, since the callback is a browser landing.
fn owner_browser<'a>(
    server: &'a Server,
    org: &str,
    email: &str,
) -> stratum_testkit::browser::Browser<'a> {
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            org,
            "--email",
            email,
            "--password",
            "a long enough password",
            "--role",
            "owner",
        ])
        .unwrap_or_else(|e| panic!("user-create: {e}"));
    stratum_testkit::browser::Browser::signed_in(server, email, "a long enough password")
}

/// Pull the `state` out of the URL the start call hands back, the way a
/// browser would by following it.
fn state_of(url: &str) -> String {
    url.split("state=")
        .nth(1)
        .expect("install url carries a state")
        .split('&')
        .next()
        .unwrap()
        .to_string()
}

/// The happy path, end to end: connect, then list what the installation
/// can read.
#[test]
fn connecting_github_binds_an_installation_and_lists_what_it_can_read() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-ok");
    let scratch = Scratch::new("connect-ok");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_ok", &gh.base_url);
    let admin = server.bootstrap_org("acme");

    // Nothing connected yet.
    let (st, out) = server.get("/v1/orgs/acme/github/installations", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["installations"].as_array().unwrap().len(), 0);

    let (st, out) = server.post(
        "/v1/orgs/acme/github/install",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let url = out["url"].as_str().unwrap().to_string();
    assert!(
        url.starts_with("https://github.com/apps/stratum/installations/new?state="),
        "install url: {url}"
    );
    let state = state_of(&url);

    // GitHub sends the browser back. The callback answers a redirect,
    // not JSON — the person is looking at a page.
    let (st, loc) = server.follow_setup("4001", &state);
    assert_eq!(st, 303, "callback answered {st}");
    assert!(loc.contains("connect=ok"), "redirected to {loc}");

    let (st, out) = server.get("/v1/orgs/acme/github/installations", &admin);
    assert_eq!(st, 200, "{out}");
    let list = out["installations"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{out}");
    assert_eq!(list[0]["installation_id"], "4001");
    // The account name is what makes two installations tellable apart.
    assert_eq!(list[0]["account"], "acme-inc");

    // And the point of all of it: a list to pick from.
    let (st, out) = server.get("/v1/orgs/acme/github/installations/4001/repos", &admin);
    assert_eq!(st, 200, "{out}");
    let repos = out["repositories"].as_array().unwrap();
    assert!(repos.len() >= 4, "{out}");
    let widget = repos
        .iter()
        .find(|r| r["full_name"] == "acme-inc/widget")
        .expect("the public one is listed");
    assert_eq!(widget["private"], false);
    assert_eq!(widget["default_branch"], "main");
    assert!(repos.iter().any(|r| r["private"] == true), "{out}");

    // Paging is real, not decorative.
    let (st, out) = server.get(
        "/v1/orgs/acme/github/installations/4001/repos?per_page=2&page=2",
        &admin,
    );
    assert_eq!(st, 200, "{out}");
    let page2 = out["repositories"].as_array().unwrap();
    assert_eq!(page2.len(), 2, "{out}");
    assert_ne!(page2[0]["full_name"], widget["full_name"]);

    assert!(server.healthy(), "still serving");
}

/// Every way the callback can be forged or replayed.
#[test]
fn a_callback_without_a_live_state_binds_nothing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-bad");
    let scratch = Scratch::new("connect-bad");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_bad", &gh.base_url);
    let admin = server.bootstrap_org("acme");

    // Shapes that are not a state at all, and a well-shaped one that was
    // never issued. Every one of them lands on the same answer, because
    // a callback is a URL a stranger can hit and telling them which kind
    // of wrong it was tells them about a flow that is not theirs. The
    // installation named is one the arriving person does control, so
    // the state is the only thing being refused.
    for forged in [
        "",
        "stinst_",
        "stinst_nope",
        "stinst_01hxxxxxxxxxxxxxxxxxxxxxxx_deadbeef",
        "not-even-close",
        "../../etc/passwd",
    ] {
        let (st, loc) = server.follow_setup("4001", forged);
        assert_eq!(st, 303, "state {forged:?} answered {st}");
        // An empty state is no state — the shape GitHub sends after an
        // edit, or after an install begun on its side — and with the
        // installation proved the person's it is parked for an org
        // rather than refused. Nothing is bound by it (checked below).
        let expected = if forged.is_empty() {
            "connect=claim"
        } else {
            "connect=expired"
        };
        assert!(
            loc.contains(expected) || loc.contains("connect=missing"),
            "state {forged:?} redirected to {loc}"
        );
    }

    // A real state, spent once, cannot be spent again. This is the
    // interesting one: a state in a browser's history is a credential
    // until it is used, and replaying it would bind a second
    // installation to the org on somebody else's say-so.
    let (st, out) = server.post(
        "/v1/orgs/acme/github/install",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let state = state_of(out["url"].as_str().unwrap());
    let (st, loc) = server.follow_setup("4001", &state);
    assert_eq!(st, 303);
    assert!(loc.contains("connect=ok"), "first use: {loc}");
    let (st, loc) = server.follow_setup("4002", &state);
    assert_eq!(st, 303);
    assert!(loc.contains("connect=expired"), "replay: {loc}");

    // The replay bound nothing.
    let (st, out) = server.get("/v1/orgs/acme/github/installations", &admin);
    assert_eq!(st, 200, "{out}");
    let list = out["installations"].as_array().unwrap();
    assert_eq!(list.len(), 1, "replay added an installation: {out}");
    assert_eq!(list[0]["installation_id"], "4001");

    assert!(server.healthy(), "still serving after all that");
}

/// Half an OAuth client is a boot failure, not a server that trusts
/// every installation id because the secret it needed went missing
/// in a deploy.
#[test]
fn half_an_oauth_client_refuses_to_boot() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-half-oauth");
    let scratch = Scratch::new("connect-half-oauth");
    let key_path = scratch.path().join("app-key.pem");
    std::fs::write(&key_path, fake_github::TEST_APP_KEY_PEM).unwrap();
    for only in ["STRATUM_GITHUB_CLIENT_ID", "STRATUM_GITHUB_CLIENT_SECRET"] {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("AWS_ACCESS_KEY_ID", "minioadmin")
            .env("AWS_SECRET_ACCESS_KEY", "minioadmin")
            .env("AWS_REGION", "us-east-1")
            .env("STRATUM_STORE_URL", &bucket.base_url)
            .env(
                "STRATUM_DB_URL",
                stratum_testkit::pg::test_db_url("connect-half-oauth"),
            )
            .env("STRATUM_BIND", "127.0.0.1:0")
            .env("STRATUM_GITHUB_APP_ID", "12345")
            .env("STRATUM_GITHUB_APP_KEY_PEM", key_path.display().to_string())
            .env(only, "half");
        // `env_clear` would also drop the coverage run's profile path,
        // and a child that writes its profile nowhere reads as an arm
        // that never ran.
        if let Ok(profile) = std::env::var("LLVM_PROFILE_FILE") {
            cmd.env("LLVM_PROFILE_FILE", profile);
        }
        let out = cmd.output().expect("run server");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{only} alone booted: {err}");
        assert!(err.contains("set together or not at all"), "{only}: {err}");
    }
}

/// The proof that was missing: a state proves the org, and only the
/// installer's own GitHub authorization proves the installation.
///
/// Before this, any org admin could mint a state for their own org and
/// arrive at the callback with *another customer's* installation id —
/// they are sequential integers — and the org would be bound to it,
/// with every private repository the App could read there one picker
/// click from a mirror. The `code` GitHub appends when the App requests
/// user authorization is exchanged for the installer's token, and the
/// installation must be in that person's own list.
#[test]
fn a_state_for_your_org_does_not_bind_somebody_elses_installation() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-notyours");
    let scratch = Scratch::new("connect-notyours");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_notyours", &gh.base_url);
    let admin = server.bootstrap_org("mallory");

    let start = || {
        let (st, out) = server.post(
            "/v1/orgs/mallory/github/install",
            &admin,
            Some(serde_json::json!({})),
        );
        assert_eq!(st, 200, "{out}");
        state_of(out["url"].as_str().unwrap())
    };
    // Arriving as somebody who controls installation 4002 (ada), naming
    // 4001 (acme-inc): a valid state, a valid person, the wrong
    // installation.
    let state = start();
    let (st, loc) = server.follow_setup_as(
        &format!("?installation_id=4001&state={state}&code=code_owning_4002"),
        None,
    );
    assert_eq!(st, 303);
    assert!(loc.contains("connect=notyours"), "{loc}");
    // No code at all — the App requests one, so its absence is a
    // constructed URL, not GitHub's redirect.
    let state = start();
    let (st, loc) = server.follow_setup_as(&format!("?installation_id=4001&state={state}"), None);
    assert_eq!(st, 303);
    assert!(loc.contains("connect=notyours"), "{loc}");
    // A spent or forged code is what GitHub answers with an `error`
    // field and a 200, and is refused the same way.
    let state = start();
    let (st, loc) = server.follow_setup_as(
        &format!("?installation_id=4001&state={state}&code=nonsense"),
        None,
    );
    assert_eq!(st, 303);
    assert!(loc.contains("connect=notyours"), "{loc}");
    // Somebody who controls nothing of the App's.
    let state = start();
    let (st, loc) = server.follow_setup_as(
        &format!("?installation_id=4001&state={state}&code=code_for_nobody"),
        None,
    );
    assert_eq!(st, 303);
    assert!(loc.contains("connect=notyours"), "{loc}");

    // Nothing was bound by any of it, and the picker offers nothing.
    let (st, out) = server.get("/v1/orgs/mallory/github/installations", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["installations"].as_array().unwrap().len(), 0, "{out}");
    let (st, _) = server.get("/v1/orgs/mallory/github/installations/4001/repos", &admin);
    assert_eq!(st, 404, "the stranger's installation was usable");

    // The refusals cost the person nothing but the trip: a state refused
    // for the installation is not spent, so the same one still binds
    // the installation that *is* theirs.
    let (st, loc) = server.follow_setup_as(
        &format!("?installation_id=4001&state={state}&code=code_owning_4001"),
        None,
    );
    assert_eq!(st, 303);
    assert!(loc.contains("connect=ok"), "{loc}");
    assert!(server.healthy());
}

/// GitHub's return after an installation is *edited* carries no state.
/// The person's own sign-in stands in: the org they began a connect for
/// is the org, if there is exactly one.
///
/// This is how the first real install on weft.sh came back — the
/// repository selection was edited after installing — and it used to
/// dead-end on `connect=missing` with nothing on screen.
#[test]
fn a_return_without_a_state_binds_to_the_org_the_person_began_a_connect_for() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-nostate");
    let scratch = Scratch::new("connect-nostate");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_nostate", &gh.base_url);
    let admin = server.bootstrap_org("acme");
    let other = server.bootstrap_org("beta");
    let mut owner = owner_browser(&server, "acme", "owner@example.test");
    let cookie = owner.cookie.clone().expect("signed in");

    // Nobody signed in, nothing started: the installation is proved the
    // person's and parked for an org to be chosen after sign-in.
    let (st, loc) = server.follow_setup_as("?installation_id=4001&code=code_owning_4001", None);
    assert_eq!(st, 303);
    assert!(loc.contains("connect=claim"), "{loc}");
    // Signed in, but no connect begun: parked the same way.
    let (st, loc) =
        server.follow_setup_as("?installation_id=4001&code=code_owning_4001", Some(&cookie));
    assert_eq!(st, 303);
    assert!(loc.contains("connect=claim"), "{loc}");
    let (st, out) = server.get("/v1/orgs/acme/github/installations", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["installations"].as_array().unwrap().len(), 0, "{out}");

    // One connect begun, from acme: the return binds to acme, and the
    // start it consumed cannot be spent again.
    let (st, out) = owner.req(
        "POST",
        "/v1/orgs/acme/github/install",
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let state = state_of(out["url"].as_str().unwrap());
    let (st, loc) =
        server.follow_setup_as("?installation_id=4001&code=code_owning_4001", Some(&cookie));
    assert_eq!(st, 303);
    assert!(loc.contains("connect=ok&org=acme"), "{loc}");
    let (st, out) = server.get("/v1/orgs/acme/github/installations", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["installations"][0]["installation_id"], "4001", "{out}");
    let (st, loc) = server.follow_setup("4002", &state);
    assert_eq!(st, 303);
    assert!(
        loc.contains("connect=expired"),
        "the consumed start was spendable: {loc}"
    );
    // The proof of control still applies without a state.
    let (st, out) = owner.req(
        "POST",
        "/v1/orgs/acme/github/install",
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, loc) =
        server.follow_setup_as("?installation_id=4002&code=code_owning_4001", Some(&cookie));
    assert_eq!(st, 303);
    assert!(loc.contains("connect=notyours"), "{loc}");

    // Connects begun from two orgs by the same person: ambiguous, so
    // parked rather than guessed.
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "beta",
            "--email",
            "owner@example.test",
            "--password",
            "a long enough password",
            "--role",
            "owner",
        ])
        .unwrap();
    let (st, out) = owner.req(
        "POST",
        "/v1/orgs/beta/github/install",
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, loc) =
        server.follow_setup_as("?installation_id=4002&code=code_owning_4002", Some(&cookie));
    assert_eq!(st, 303);
    assert!(loc.contains("connect=claim"), "{loc}");
    let (_, out) = server.get("/v1/orgs/beta/github/installations", &other);
    assert_eq!(out["installations"].as_array().unwrap().len(), 0, "{out}");
    assert!(server.healthy());
}

/// An install that begins on GitHub — the Marketplace listing, the App's
/// own page — arrives with no state and, usually, nobody signed in. It
/// used to dead-end on `connect=missing`. Now the callback proves the
/// installation is the person's, parks it, and the dashboard connects it
/// to the org they pick after signing in.
#[test]
fn an_install_begun_on_github_is_parked_and_claimed_after_sign_in() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-claim");
    let scratch = Scratch::new("connect-claim");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_claim", &gh.base_url);
    let admin = server.bootstrap_org("acme");
    let other = server.bootstrap_org("beta");

    // Nobody signed in. The redirect carries a claim cookie; nothing is
    // bound yet, and the parked installation can be read back.
    let (st, loc, set) =
        server.follow_setup_full("?installation_id=4001&code=code_owning_4001", None);
    assert_eq!(st, 303);
    assert!(loc.contains("connect=claim"), "{loc}");
    let set = set.expect("the callback set a claim cookie");
    assert!(set.starts_with("weft_install=stclaim_"), "{set}");
    assert!(set.contains("HttpOnly"), "{set}");
    let claim = set.split(';').next().unwrap().to_string();
    let mut anon = stratum_testkit::browser::Browser::new(&server);
    anon.cookie = Some(claim.clone());
    let (st, out) = anon.req("GET", "/v1/github/pending-install", None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["installation_id"], "4001", "{out}");
    // Without the cookie there is nothing to read.
    let (st, _) = server.get("/v1/github/pending-install", &admin);
    assert_eq!(st, 404);

    // A viewer cannot claim it for an org: the org half is org:admin,
    // and below the needed scope the answer is the masked 404 every
    // org route gives (`authx::require`), never a 403 that confirms
    // what exists.
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "viewer@example.test",
            "--password",
            "a long enough password",
            "--role",
            "viewer",
        ])
        .unwrap();
    let mut viewer = stratum_testkit::browser::Browser::signed_in(
        &server,
        "viewer@example.test",
        "a long enough password",
    );
    let session = viewer.cookie.clone().unwrap();
    viewer.cookie = Some(format!("{session}; {claim}"));
    let (st, out) = viewer.req("POST", "/v1/orgs/acme/github/install/claim", None);
    assert_eq!(st, 404, "{out}");

    // The owner signs in (or up) and picks acme: bound, audited, and
    // the claim is spent.
    let mut owner = owner_browser(&server, "acme", "owner@example.test");
    let session = owner.cookie.clone().unwrap();
    owner.cookie = Some(format!("{session}; {claim}"));
    let (st, out) = owner.req("POST", "/v1/orgs/acme/github/install/claim", None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["installation_id"], "4001", "{out}");
    let (st, out) = server.get("/v1/orgs/acme/github/installations", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["installations"][0]["installation_id"], "4001", "{out}");
    owner.cookie = Some(format!("{session}; {claim}"));
    let (st, out) = owner.req("POST", "/v1/orgs/acme/github/install/claim", None);
    assert_eq!(st, 404, "a spent claim: {out}");
    let (st, out) = server.get("/v1/orgs/acme/audit?limit=5", &admin);
    assert_eq!(st, 200, "{out}");
    assert!(
        out["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "github.connect"),
        "{out}"
    );

    // A second parked install of the same installation cannot be given
    // to another org: the unique index refuses, and the claim says so.
    let (_, _, set) = server.follow_setup_full("?installation_id=4001&code=code_owning_4001", None);
    let claim2 = set.unwrap().split(';').next().unwrap().to_string();
    let mut beta_owner = owner_browser(&server, "beta", "beta@example.test");
    let session = beta_owner.cookie.clone().unwrap();
    beta_owner.cookie = Some(format!("{session}; {claim2}"));
    let (st, out) = beta_owner.req("POST", "/v1/orgs/beta/github/install/claim", None);
    assert_eq!(st, 409, "{out}");
    let (_, out) = server.get("/v1/orgs/beta/github/installations", &other);
    assert_eq!(out["installations"].as_array().unwrap().len(), 0, "{out}");

    // A forged claim is nothing, in every shape: well-formed but never
    // issued, no secret half, no prefix at all — and the same for a
    // read of it. An org that does not exist is the org's 404, and a
    // signed-in owner with no cookie at all has nothing to claim.
    let mut forger = owner_browser(&server, "beta", "forger@example.test");
    let session = forger.cookie.clone().unwrap();
    for forged in ["stclaim_nope_nope", "stclaim_nounderscore", "garbage"] {
        forger.cookie = Some(format!("{session}; weft_install={forged}"));
        let (st, _) = forger.req("POST", "/v1/orgs/beta/github/install/claim", None);
        assert_eq!(st, 404, "claim {forged:?}");
        forger.cookie = Some(format!("weft_install={forged}"));
        let (st, _) = forger.req("GET", "/v1/github/pending-install", None);
        assert_eq!(st, 404, "peek {forged:?}");
    }
    forger.cookie = Some(format!("{session}; weft_install={claim2}"));
    let (st, _) = forger.req("POST", "/v1/orgs/nowhere/github/install/claim", None);
    assert_eq!(st, 404);
    forger.cookie = Some(session.clone());
    let (st, out) = forger.req("POST", "/v1/orgs/beta/github/install/claim", None);
    assert_eq!(st, 404, "{out}");

    assert!(server.healthy());
}

/// One installation, two orgs.
#[test]
fn an_installation_bound_to_one_org_cannot_be_used_by_another() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-cross");
    let scratch = Scratch::new("connect-cross");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_cross", &gh.base_url);
    let acme = server.bootstrap_org("acme");
    let other = server.bootstrap_org("other");

    let (st, out) = server.post(
        "/v1/orgs/acme/github/install",
        &acme,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, loc) = server.follow_setup("4001", &state_of(out["url"].as_str().unwrap()));
    assert_eq!(st, 303);
    assert!(loc.contains("connect=ok"), "{loc}");

    // `other` knows the id — it is a number, and numbers leak — and asks
    // for its repositories. An id is a bearer token to somebody's source
    // unless something checks who may use it.
    let (st, out) = server.get("/v1/orgs/other/github/installations/4001/repos", &other);
    assert_eq!(st, 404, "another org reached installation 4001: {out}");
    let (st, out) = server.get("/v1/orgs/other/github/installations", &other);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["installations"].as_array().unwrap().len(), 0, "{out}");

    // And it cannot claim it either: the unique index says an
    // installation belongs to exactly one org.
    let (st, out) = server.post(
        "/v1/orgs/other/github/install",
        &other,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, loc) = server.follow_setup("4001", &state_of(out["url"].as_str().unwrap()));
    assert_eq!(st, 303);
    assert!(loc.contains("connect=taken"), "{loc}");
    let (st, out) = server.get("/v1/orgs/other/github/installations", &other);
    assert_eq!(
        out["installations"].as_array().unwrap().len(),
        0,
        "{st} {out}"
    );

    // acme still has it.
    let (st, out) = server.get("/v1/orgs/acme/github/installations", &acme);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["installations"].as_array().unwrap().len(), 1, "{out}");

    assert!(server.healthy(), "still serving");
}

/// The route that actually spends an installation id is mirror
/// creation, not the picker: `origin_installation` is what the sync
/// worker exchanges for a token. Every installation of a public App is
/// reachable through it, so an id a caller types into that body has to
/// be one this org connected — a stranger's is a bearer token to their
/// private source, and a made-up one is a guess that costs nothing to
/// repeat. The picker route had this check; the create route did not.
#[test]
fn a_mirror_cannot_be_created_through_an_installation_the_org_did_not_connect() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-mirror");
    let scratch = Scratch::new("connect-mirror");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_mirror", &gh.base_url);
    let acme = server.bootstrap_org("acme");
    let other = server.bootstrap_org("other");
    make_origin(&scratch.path().join("origins"), "acme-inc/ledger", 3);

    let mirror = |org: &str, token: &str, name: &str, installation: &str| {
        server.post(
            &format!("/v1/orgs/{org}/mirrors"),
            token,
            Some(serde_json::json!({
                "name": name,
                "provider": "github",
                "origin": "acme-inc/ledger",
                "installation_id": installation,
            })),
        )
    };

    // Nobody has connected anything: an id is just a number somebody
    // typed, and it is refused the same way the picker refuses it.
    let (st, out) = mirror("acme", &acme, "guessed", "4001");
    assert_eq!(st, 404, "unconnected installation accepted: {out}");
    assert_eq!(out["error"], "no such installation", "{out}");
    let (st, out) = server.get("/v1/orgs/acme/repos/guessed", &acme);
    assert_eq!(st, 404, "a refused creation left a repo behind: {out}");

    // Same for an id that could never be one — refused before the
    // database sees it, and without the hint that anything exists.
    let (st, out) = mirror("acme", &acme, "nonsense", "4001; DROP TABLE repos");
    assert_eq!(st, 404, "{out}");
    let (st, out) = mirror("acme", &acme, "nul", "4001\u{0}");
    assert_eq!(st, 404, "{out}");

    // acme connects 4001 the honest way, and now the same body works.
    server.connect_installation("acme", &acme, "4001");
    let (st, out) = mirror("acme", &acme, "ledger", "4001");
    assert_eq!(st, 202, "connected installation refused: {out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors/ledger/sync",
        &acme,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");

    // `other` knows the number too. It is acme's, so `other` cannot
    // mirror through it — and learns nothing about whether it exists.
    let (st, out) = mirror("other", &other, "stolen", "4001");
    assert_eq!(
        st, 404,
        "another org mirrored through acme's installation: {out}"
    );
    assert_eq!(out["error"], "no such installation", "{out}");
    let (st, out) = server.get("/v1/orgs/other/repos/stolen", &other);
    assert_eq!(st, 404, "{out}");

    // Forgetting releases it for new mirrors here as well as in the
    // picker; the mirror already made keeps working, as documented.
    let (st, out) = server.delete("/v1/orgs/acme/github/installations/4001", &acme);
    assert_eq!(st, 204, "{out}");
    let (st, out) = mirror("acme", &acme, "after-forget", "4001");
    assert_eq!(st, 404, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors/ledger/sync",
        &acme,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");

    // A public origin still needs no installation at all.
    let (st, out) = server.post(
        "/v1/orgs/other/mirrors",
        &other,
        Some(serde_json::json!({
            "name": "public",
            "provider": "github",
            "origin": "acme-inc/ledger",
        })),
    );
    assert_eq!(st, 202, "{out}");

    assert!(server.healthy(), "still serving");
}

/// A state belongs to the org that minted it, and connecting is an
/// org-admin act.
#[test]
fn starting_a_connect_needs_admin_and_a_state_names_its_own_org() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-authz");
    let scratch = Scratch::new("connect-authz");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_authz", &gh.base_url);
    let acme = server.bootstrap_org("acme");
    let other = server.bootstrap_org("other");

    // A different org's admin is not this org's admin. 404, not 403: an
    // org they cannot administer is an org they cannot see.
    let (st, out) = server.post(
        "/v1/orgs/acme/github/install",
        &other,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 404, "{out}");
    let (st, out) = server.get("/v1/orgs/acme/github/installations", &other);
    assert_eq!(st, 404, "{out}");

    // Anonymous cannot start one at all.
    let (st, out) = server.post(
        "/v1/orgs/acme/github/install",
        "",
        Some(serde_json::json!({})),
    );
    assert!(
        st == 401 || st == 404,
        "anonymous start answered {st}: {out}"
    );

    // A state minted for acme binds to acme however it is redeemed —
    // the callback carries no session, so the state is the only thing
    // that says whose flow this is.
    let (st, out) = server.post(
        "/v1/orgs/acme/github/install",
        &acme,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, loc) = server.follow_setup("4002", &state_of(out["url"].as_str().unwrap()));
    assert_eq!(st, 303);
    assert!(
        loc.contains("connect=ok") && loc.contains("org=acme"),
        "{loc}"
    );
    let (st, out) = server.get("/v1/orgs/other/github/installations", &other);
    assert_eq!(
        out["installations"].as_array().unwrap().len(),
        0,
        "{st} {out}"
    );

    assert!(server.healthy(), "still serving");
}

/// Disconnecting, and what it does not do.
#[test]
fn forgetting_an_installation_stops_offering_it_here() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-forget");
    let scratch = Scratch::new("connect-forget");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_forget", &gh.base_url);
    let admin = server.bootstrap_org("acme");

    let (st, out) = server.post(
        "/v1/orgs/acme/github/install",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, _) = server.follow_setup("4001", &state_of(out["url"].as_str().unwrap()));
    assert_eq!(st, 303);

    let (st, out) = server.delete("/v1/orgs/acme/github/installations/4001", &admin);
    assert_eq!(st, 204, "{out}");
    // Gone, and the repos it offered are no longer reachable through it.
    let (st, out) = server.get("/v1/orgs/acme/github/installations/4001/repos", &admin);
    assert_eq!(st, 404, "{out}");
    // Forgetting twice is a 404, not a second success.
    let (st, out) = server.delete("/v1/orgs/acme/github/installations/4001", &admin);
    assert_eq!(st, 404, "{out}");

    // And now it can be connected again — by this org or another one,
    // because forgetting really released it.
    let (st, out) = server.post(
        "/v1/orgs/acme/github/install",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, loc) = server.follow_setup("4001", &state_of(out["url"].as_str().unwrap()));
    assert_eq!(st, 303);
    assert!(loc.contains("connect=ok"), "{loc}");

    assert!(server.healthy(), "still serving");
}

/// The injection corpus through the two path segments a caller controls.
#[test]
fn hostile_installation_ids_are_refused_without_wedging_the_server() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-inject");
    let scratch = Scratch::new("connect-inject");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_inject", &gh.base_url);
    let admin = server.bootstrap_org("acme");

    for bad in stratum_testkit::adversarial::INJECTIONS {
        let encoded = urlencode(bad);
        let (st, out) = server.get(
            &format!("/v1/orgs/acme/github/installations/{encoded}/repos"),
            &admin,
        );
        assert!(
            st == 400 || st == 404,
            "installation {bad:?} answered {st}: {out}"
        );
        let (st, _) = server.follow_setup(&encoded, &urlencode(bad));
        assert_eq!(st, 303, "callback with {bad:?} answered {st}");
    }
    assert!(server.healthy(), "still serving after all that");
}

/// Creation checks the origin, and reports what a mirror is doing.
#[test]
fn creation_refuses_an_origin_it_cannot_reach_and_then_reports_progress() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-create");
    let scratch = Scratch::new("connect-create");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_create", &gh.base_url);
    let admin = server.bootstrap_org("acme");

    // A generic origin that is not a git repository, and not local, so
    // creation really does check it. `.invalid` is reserved by RFC 2606
    // and never resolves — no packet leaves this machine.
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(serde_json::json!({
            "name": "nowhere",
            "provider": "generic",
            "origin": "https://nothing.invalid/acme/nowhere.git",
        })),
    );
    assert_eq!(st, 422, "unreachable origin was accepted: {out}");
    // The reason is written for a person, and the probe rides along so
    // the screen knows whether to offer the GitHub install.
    assert!(
        out["error"].as_str().unwrap_or_default().len() > 10,
        "{out}"
    );
    assert_eq!(out["probe"]["reachable"], false, "{out}");
    // And nothing was created: a refused origin must not leave a repo
    // behind for somebody to wonder about.
    let (st, out) = server.get("/v1/orgs/acme/repos/nowhere", &admin);
    assert_eq!(st, 404, "a refused mirror was created anyway: {out}");

    // A local origin is not probed — there is no network to ask — so a
    // hermetic mirror still creates, and its status is followable.
    make_origin(&scratch.path().join("origins"), "acme/widget", 3);
    server.connect_installation("acme", &admin, "4001");
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(serde_json::json!({
            "name": "widget",
            "provider": "github",
            "origin": "acme/widget",
            "installation_id": "4001",
        })),
    );
    assert_eq!(st, 202, "{out}");

    // Before any sync has finished: syncing, not failed, and not ready.
    // This is the state somebody stares at, so it has to be the one
    // reported rather than an empty record.
    let (st, out) = server.get("/v1/orgs/acme/repos/widget/sync-status", &admin);
    assert_eq!(st, 200, "{out}");
    assert!(
        ["syncing", "ready"].contains(&out["state"].as_str().unwrap_or_default()),
        "{out}"
    );
    assert_eq!(out["origin"], "acme/widget", "{out}");
    assert!(
        out["clone_url"]
            .as_str()
            .unwrap_or_default()
            .ends_with("/acme/widget.git"),
        "{out}"
    );

    // After a sync it is ready, with the commit it landed on.
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors/widget/sync",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, out) = server.get("/v1/orgs/acme/repos/widget/sync-status", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["state"], "ready", "{out}");
    assert!(out["commit"].as_str().is_some(), "{out}");
    assert!(out["error"].is_null(), "{out}");

    // A repo that is not a mirror has no sync to report.
    let (st, _) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "plain" })),
    );
    assert_eq!(st, 201);
    let (st, out) = server.get("/v1/orgs/acme/repos/plain/sync-status", &admin);
    assert_eq!(st, 400, "{out}");

    assert!(server.healthy(), "still serving");
}

/// Every refusal on the connect routes, including the ones that answer
/// before any GitHub call is made.
///
/// These are the arms a person hits by mistyping a URL or by being
/// signed in somewhere else, so they are the ones most likely to be
/// wrong and least likely to be noticed.
#[test]
fn every_connect_route_refuses_the_org_it_should() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-refuse");
    let scratch = Scratch::new("connect-refuse");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_refuse", &gh.base_url);
    let acme = server.bootstrap_org("acme");
    let other = server.bootstrap_org("other");

    // An organization that does not exist, asked about by somebody with
    // a perfectly good token for one that does.
    let (st, out) = server.post(
        "/v1/orgs/nope/github/install",
        &acme,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 404, "{out}");
    let (st, out) = server.get("/v1/orgs/nope/github/installations", &acme);
    assert_eq!(st, 404, "{out}");
    let (st, out) = server.get("/v1/orgs/nope/github/installations/4001/repos", &acme);
    assert_eq!(st, 404, "{out}");
    let (st, out) = server.delete("/v1/orgs/nope/github/installations/4001", &acme);
    assert_eq!(st, 404, "{out}");

    // An organization that does exist, asked about by somebody who is
    // not in it. Same answer: an org you cannot administer is an org you
    // cannot see.
    let (st, out) = server.get("/v1/orgs/acme/github/installations/4001/repos", &other);
    assert_eq!(st, 404, "{out}");
    let (st, out) = server.delete("/v1/orgs/acme/github/installations/4001", &other);
    assert_eq!(st, 404, "{out}");

    // An installation this org has genuinely never connected.
    let (st, out) = server.delete("/v1/orgs/acme/github/installations/4001", &acme);
    assert_eq!(st, 404, "{out}");

    // A callback with nothing in it at all — a bookmark, or somebody
    // poking the URL. It says what it can and sends them somewhere.
    let (st, loc) = server.follow_setup_raw("");
    assert_eq!(st, 303, "bare callback answered {st}");
    assert!(loc.contains("connect=missing"), "{loc}");

    assert!(server.healthy(), "still serving");
}

/// A server with no GitHub App says so, rather than offering a dead
/// link or a 500.
#[test]
fn without_a_github_app_the_connect_routes_say_so() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-noapp");
    let scratch = Scratch::new("connect-noapp");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("connect_noapp")
        .data_dir(scratch.path().join("data"))
        .start();
    let admin = server.bootstrap_org("acme");

    let (st, out) = server.post(
        "/v1/orgs/acme/github/install",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 501, "{out}");
    let said = out["error"].as_str().unwrap_or_default();
    assert!(
        said.contains("GitHub App"),
        "a self-hosted server should say what is missing: {out}"
    );
    // Nothing is connected, so asking what one can read is a 404 before
    // it is anything else.
    let (st, out) = server.get("/v1/orgs/acme/github/installations/4001/repos", &admin);
    assert_eq!(st, 404, "{out}");
    assert!(server.healthy(), "still serving");
}

/// GitHub not answering is not this server breaking.
#[test]
fn an_unreachable_github_is_a_bad_gateway_not_an_internal_error() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-down");
    let scratch = Scratch::new("connect-down");
    // Port 1 is privileged and nothing listens on it: every connection
    // is refused immediately, which is what a dead upstream looks like
    // without waiting out a timeout.
    let server = spawn(
        &bucket.base_url,
        &scratch,
        "connect_down",
        "http://127.0.0.1:1",
    );
    let admin = server.bootstrap_org("acme");

    let (st, out) = server.post(
        "/v1/orgs/acme/github/install",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let state = state_of(out["url"].as_str().unwrap());
    // The callback cannot bind: the proof that the installation is this
    // person's is GitHub's to give, and GitHub did not answer. That is
    // `error` — try again — not `notyours`, and the state is not spent,
    // so the same redirect works once GitHub is back.
    let (st, loc) = server.follow_setup("4001", &state);
    assert_eq!(st, 303);
    assert!(loc.contains("connect=error"), "{loc}");
    let (st, out) = server.get("/v1/orgs/acme/github/installations", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["installations"].as_array().unwrap().len(), 0, "{out}");
    let (st, loc) = server.follow_setup("4001", &state);
    assert_eq!(st, 303);
    assert!(
        loc.contains("connect=error"),
        "the state was spent on an unanswered trip: {loc}"
    );
    assert!(server.healthy(), "still serving");

    // With the proof waived — an App without an OAuth client — the
    // callback binds, and losing the account name (a convenience) does
    // not lose the connection. Asking what it can read then says GitHub
    // did not answer, rather than reporting this server broken.
    let server = spawn_without_user_auth(
        &bucket.base_url,
        &scratch,
        "connect_down_legacy",
        "http://127.0.0.1:1",
    );
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.post(
        "/v1/orgs/acme/github/install",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, loc) = server.follow_setup("4001", &state_of(out["url"].as_str().unwrap()));
    assert_eq!(st, 303);
    assert!(loc.contains("connect=ok"), "{loc}");
    let (st, out) = server.get("/v1/orgs/acme/github/installations", &admin);
    assert_eq!(st, 200, "{out}");
    assert!(out["installations"][0]["account"].is_null(), "{out}");
    let (st, out) = server.get("/v1/orgs/acme/github/installations/4001/repos", &admin);
    assert_eq!(st, 502, "{out}");
    assert!(server.healthy(), "still serving");
}

/// A first sync that fails says so, and keeps saying so.
#[test]
fn a_mirror_that_cannot_reach_its_origin_reports_failed_with_a_reason() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-failed");
    let scratch = Scratch::new("connect-failed");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_failed", &gh.base_url);
    let admin = server.bootstrap_org("acme");

    // An installation is supplied, so creation does not probe — which
    // is the case this covers: the origin turns out to be wrong later,
    // and the only place that can say so is the sync.
    server.connect_installation("acme", &admin, "4001");
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(serde_json::json!({
            "name": "ghost",
            "provider": "github",
            "origin": "acme/ghost",
            "installation_id": "4001",
        })),
    );
    assert_eq!(st, 202, "{out}");
    let (st, _) = server.post(
        "/v1/orgs/acme/mirrors/ghost/sync",
        &admin,
        Some(serde_json::json!({})),
    );
    assert!(st == 200 || st == 502 || st == 500, "sync answered {st}");

    let (st, out) = server.get("/v1/orgs/acme/repos/ghost/sync-status", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["state"], "failed", "{out}");
    assert!(
        out["error"].as_str().unwrap_or_default().len() > 5,
        "a failure with no reason is a failure nobody can act on: {out}"
    );

    // A repository that does not exist, and one in another org.
    let (st, out) = server.get("/v1/orgs/acme/repos/nope/sync-status", &admin);
    assert_eq!(st, 404, "{out}");
    let stranger = server.bootstrap_org("other");
    let (st, out) = server.get("/v1/orgs/acme/repos/ghost/sync-status", &stranger);
    assert_eq!(st, 404, "{out}");

    assert!(server.healthy(), "still serving");
}

/// A local origin is not probed, because there is no network to ask.
#[test]
fn a_local_origin_is_created_without_being_probed() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-local");
    let scratch = Scratch::new("connect-local");
    let gh = fake_github::spawn();
    let server = spawn(&bucket.base_url, &scratch, "connect_local", &gh.base_url);
    let admin = server.bootstrap_org("acme");
    let (_bare, tip) = make_origin(&scratch.path().join("origins"), "acme/local", 2);

    // No installation id: without the local-origin seam this would be
    // probed, and `acme/local` would be normalised to a github.com URL
    // and checked over the network — which a hermetic suite must never
    // do, and which would refuse a perfectly good operator origin.
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(serde_json::json!({
            "name": "local",
            "provider": "github",
            "origin": "acme/local",
        })),
    );
    assert_eq!(st, 202, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors/local/sync",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, out) = server.get("/v1/orgs/acme/repos/local/sync-status", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["state"], "ready", "{out}");
    assert_eq!(out["commit"], tip, "{out}");

    assert!(server.healthy(), "still serving");
}

/// On an HTTPS deployment the claim cookie is `Secure`, like the session
/// cookie: a claim is a bearer credential for thirty minutes and must not
/// ride a plain-HTTP request. The local stack is HTTP and gets no flag.
#[test]
fn the_claim_cookie_is_secure_on_an_https_deployment() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-secure");
    let scratch = Scratch::new("connect-secure");
    let gh = fake_github::spawn();
    let server = builder(&bucket.base_url, &scratch, "connect_secure", &gh.base_url)
        .env("STRATUM_GITHUB_CLIENT_ID", "Iv1.test")
        .env("STRATUM_GITHUB_CLIENT_SECRET", "test-client-secret")
        .env("STRATUM_GITHUB_OAUTH_BASE", &gh.base_url)
        .env("STRATUM_PUBLIC_URL", "https://weft.example")
        .start();
    let (st, loc, set) =
        server.follow_setup_full("?installation_id=4001&code=code_owning_4001", None);
    assert_eq!(st, 303);
    assert!(
        loc.starts_with("https://weft.example/dashboard/?connect=claim"),
        "{loc}"
    );
    let set = set.expect("a claim cookie");
    assert!(set.contains("; Secure"), "{set}");
    assert!(set.contains("HttpOnly"), "{set}");
}

/// Half a GitHub App is not a GitHub App.
///
/// An operator who sets the install URL and forgets the app key gets a
/// working connect flow that then cannot ask GitHub anything. That is a
/// configuration mistake, and it has to say so rather than answer 500.
#[test]
fn an_install_url_without_an_app_key_is_reported_as_missing_configuration() {
    let minio = Minio::shared();
    let bucket = minio.bucket("connect-half");
    let scratch = Scratch::new("connect-half");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("connect_half")
        .data_dir(scratch.path().join("data"))
        .env(
            "STRATUM_GITHUB_INSTALL_URL",
            "https://github.com/apps/stratum/installations/new",
        )
        .start();
    let admin = server.bootstrap_org("acme");

    // Connecting works — none of it needs the app.
    let (st, out) = server.post(
        "/v1/orgs/acme/github/install",
        &admin,
        Some(serde_json::json!({})),
    );
    assert_eq!(st, 200, "{out}");
    let (st, loc) = server.follow_setup("4001", &state_of(out["url"].as_str().unwrap()));
    assert_eq!(st, 303);
    assert!(loc.contains("connect=ok"), "{loc}");

    // Asking what it can read cannot: that is the app's own view.
    let (st, out) = server.get("/v1/orgs/acme/github/installations/4001/repos", &admin);
    assert_eq!(st, 501, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("GitHub App"),
        "{out}"
    );

    // An id that could never be one is refused on the way out, too.
    let (st, out) = server.delete("/v1/orgs/acme/github/installations/not-a-number", &admin);
    assert_eq!(st, 404, "{out}");
    let (st, out) = server.delete("/v1/orgs/acme/github/installations/4001", &admin);
    assert_eq!(st, 204, "{out}");

    assert!(server.healthy(), "still serving");
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
