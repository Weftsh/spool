//! The forge pages: `/{owner}` and `/{owner}/{repo}/…` render the
//! dashboard SPA, served from the router's fallback rather than from a
//! route of their own.
//!
//! Doing it in the fallback is what keeps every real route in front of
//! the forge: a root `.route("/:owner")` would have matchit prefer a
//! parameterised segment over the git wire and anything else the
//! fallback decides, so a reserved name would quietly have become
//! somebody's profile. These tests hold that ordering, and hold the two
//! things it must not disturb: a typo is still a real 404, and the git
//! wire is untouched.
//!
//! The shell is content-free. Every repository is private to its
//! organisation, so what a signed-out visitor sees on one of these pages
//! is decided by the SPA's API calls — which answer them 401 — and not
//! by the shell, which is the same bytes for everybody.

use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::{Minio, Server};

/// A server with the dashboard configured, the way a real deployment
/// runs. The contents stand in for the built SPA: what is being tested
/// is which file the fallback chooses, not what Vite emits.
fn spawn_with_assets(store_url: &str, scratch: &Scratch) -> Server {
    let dash = scratch.path().join("dashboard");
    std::fs::create_dir_all(&dash).unwrap();
    std::fs::write(dash.join("index.html"), "<div id=\"root\">SPA SHELL</div>").unwrap();

    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint("forge-pages")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_DASHBOARD_DIR", dash.display().to_string())
        .env("STRATUM_COMPACT_POLL_SECS", "86400")
        .start()
}

/// GET with no credentials at all, reporting `(status, body)` — a
/// signed-out browser, the visitor the shell has to work for before it
/// can send them to sign in. `Server::req` parses JSON; these answers
/// are HTML.
fn anon_get(server: &Server, path: &str) -> (u16, String) {
    get_as(server, path, "")
}

/// The same GET with a bearer token, or none when `token` is empty.
fn get_as(server: &Server, path: &str, token: &str) -> (u16, String) {
    let mut r = ureq::get(&format!("{}{path}", server.base));
    if !token.is_empty() {
        r = r.set("Authorization", &format!("Bearer {token}"));
    }
    let resp = match r.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("transport GET {path}: {e}"),
    };
    let status = resp.status();
    (status, resp.into_string().unwrap_or_default())
}

#[test]
fn forge_urls_render_the_spa_and_typos_still_404() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forge-pages");
    let scratch = Scratch::new("forge-pages");
    let server = spawn_with_assets(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "widget" })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/widget/commits",
        &admin,
        Some(serde_json::json!({
            "message": "seed",
            "operations": [{"op": "put", "path": "readme", "content": "hello\n"}],
        })),
    );
    assert_eq!(st, 201, "{out}");

    // The namespace root and every depth beneath it are the SPA, to a
    // visitor holding no session and no token — the shell is how they
    // are sent to sign in.
    for path in [
        "/acme",
        "/acme/",
        "/acme/widget",
        "/acme/widget/changes",
        "/acme/widget/changes/7/files",
    ] {
        let (st, body) = anon_get(&server, path);
        assert_eq!(st, 200, "{path} did not render for a signed-out visitor");
        assert!(body.contains("SPA SHELL"), "{path} served {body:?}");
    }

    // A repository that does not exist is the SPA too, and that is
    // deliberate: the repo is never looked up here. The shell carries no
    // repo content, and its API call gets the existing masking — a 404
    // here for an absent name and a 200 for a present one would be an
    // oracle for which private repositories a namespace holds.
    let (st, body) = anon_get(&server, "/acme/no-such-repo");
    assert_eq!(st, 200);
    assert!(body.contains("SPA SHELL"));
    // …and the API behind it is where the visitor is refused, the same
    // way for the real repository and the absent one.
    let (real, _) = anon_get(&server, "/v1/orgs/acme/repos/widget");
    let (absent, _) = anon_get(&server, "/v1/orgs/acme/repos/no-such-repo");
    assert_eq!((real, absent), (401, 401));

    // A namespace nobody has taken is a real 404, not a soft one. This
    // is the reason the fallback pays for a lookup instead of gating on
    // name shape alone: a 200 here would make every typo and every
    // crawler probe an indexable page indistinguishable from a real one.
    for path in ["/typo-here", "/typo-here/widget", "/nobody/at/all"] {
        let (st, body) = anon_get(&server, path);
        assert_eq!(st, 404, "{path} answered {st} with {body:?}");
    }

    // …and an ill-formed first segment never reaches Postgres at all;
    // it is settled by shape. Observable only as the same 404, which is
    // the point — scanner noise costs the control plane nothing.
    for path in ["/.env", "/has%20space", &format!("/{}", "x".repeat(300))] {
        let (st, _) = anon_get(&server, path);
        assert_eq!(st, 404, "{path}");
    }

    // A reserved name is nobody's page: it is a 404 whether or not
    // anything is served under it, and it cannot be taken as a namespace
    // at all. Asserted through the org creation path rather than against
    // `is_reserved` directly, because what matters is that the refusal
    // reaches a person typing a name.
    let (st, _) = anon_get(&server, "/monorepo");
    assert_eq!(st, 404, "a reserved name rendered as a namespace");
    let taken = server.admin(&["admin", "bootstrap", "--org", "monorepo"]);
    assert!(
        taken.is_err(),
        "monorepo was available as a namespace: {taken:?}"
    );
    // The SPA's own top-level pages are reserved *so that* nobody can
    // shadow them, and they are served rather than refused.
    for path in ["/search", "/login", "/orgs", "/issues", "/notifications"] {
        let (st, body) = anon_get(&server, path);
        assert_eq!(st, 200, "{path}");
        assert!(body.contains("SPA SHELL"), "{path} served {body:?}");
    }

    // The git wire is registered as real routes, so it matches before
    // the fallback is ever consulted: a clone with a credential still
    // works and still fscks (I11)…
    let base = server.base.strip_prefix("http://").unwrap();
    gitcli::clone_and_fsck(
        &format!("http://x:{admin}@{base}/acme/widget.git"),
        &scratch.path().join("clone"),
    );
    // …including the un-suffixed form, which shares its first two
    // segments with the SPA's repo URL and must still be git.
    gitcli::clone_and_fsck(
        &format!("http://x:{admin}@{base}/acme/widget"),
        &scratch.path().join("clone-bare"),
    );
    // A ref advert reached by hand — no `Git-Protocol` header — is
    // refused by the git handler for wanting protocol v2. That refusal
    // is the evidence: the route answered, the fallback never saw it.
    // Had the SPA claimed this path it would have been a cheerful 200
    // of HTML, which stock git would have tried to parse as pkt-lines.
    let advert = "/acme/widget/info/refs?service=git-upload-pack";
    let (st, body) = get_as(&server, advert, &admin);
    assert_eq!(st, 400, "{body:?}");
    assert!(
        body.contains("protocol v2 required"),
        "the SPA answered a git request: {body:?}"
    );
    // Without one it is git's challenge, not the shell.
    let (st, body) = anon_get(&server, advert);
    assert_eq!(st, 401, "{body:?}");
    assert!(!body.contains("SPA SHELL"), "{body:?}");

    // The attack-case rule: the server is still healthy and serving.
    assert!(server.healthy());
}

/// An API-only deployment has no dashboard to fall back to, and the new
/// branch must not invent one — nor panic reaching for a directory that
/// is not configured. Nor for one that is configured and empty: a
/// deployment whose `STRATUM_DASHBOARD_DIR` points somewhere the build
/// never landed is a misconfiguration, and the honest answer to it is
/// the 404 we gave before, not a 500 or a hang.
#[test]
fn a_server_without_a_dashboard_dir_still_404s_forge_urls() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forge-pages-bare");
    let scratch = Scratch::new("forge-pages-bare");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("forge-pages-bare")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_COMPACT_POLL_SECS", "86400")
        .start();
    server.bootstrap_org("acme");

    // `acme` exists, so only the missing dashboard dir stands between
    // this and a 200 — which is exactly the case worth pinning.
    for path in ["/acme", "/acme/widget", "/"] {
        let (st, _) = anon_get(&server, path);
        assert_eq!(st, 404, "{path} on a server with no asset dirs");
    }

    // Configured but empty — the dir exists and holds no `index.html`,
    // which is what a deploy that shipped the env var and not the build
    // looks like.
    let empty = scratch.path().join("empty-dashboard");
    std::fs::create_dir_all(&empty).unwrap();
    let misconfigured = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("forge-pages-empty")
        .data_dir(scratch.path().join("data-empty"))
        .env("STRATUM_DASHBOARD_DIR", empty.display().to_string())
        .env("STRATUM_COMPACT_POLL_SECS", "86400")
        .start();
    misconfigured.bootstrap_org("acme");
    for path in ["/acme", "/acme/widget"] {
        let (st, _) = anon_get(&misconfigured, path);
        assert_eq!(st, 404, "{path} on a server with an empty dashboard dir");
    }
    assert!(misconfigured.healthy());
    assert!(server.healthy());
}

/// A namespace's repositories are found by the people who belong to it,
/// and by nobody else.
///
/// This used to be "a stranger finds only the public repositories of a
/// namespace": the public profile page read search, anonymously, for the
/// repositories anybody could see. There are no public repositories, so
/// the page has nothing to show a stranger — and search says so in the
/// two ways it can: no credential is a 401, and a credential from
/// another organisation finds nothing here, private repository names
/// included, while a member finds every one.
///
/// It is search rather than `GET /v1/orgs/{org}/repos` on purpose. That
/// listing is a protected endpoint — three suites depend on it refusing
/// both absent and bad credentials (`api_edges_e2e` uses it to prove
/// REST sends no Basic challenge, `http_pentest_e2e` uses it as the
/// target where hostile headers must never yield 200, and `git_e2e`
/// uses it for the cross-tenant 404).
#[test]
fn a_namespace_s_repositories_are_found_only_by_its_members() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forge-list");
    let scratch = Scratch::new("forge-list");
    let server = spawn_with_assets(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    let rival = server.bootstrap_org("rival");
    for name in ["one", "two", "secret"] {
        let (st, out) = server.req(
            "POST",
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name })),
        );
        assert_eq!(st, 201, "{out}");
    }
    let names = |body: &str| -> Vec<String> {
        let found: serde_json::Value = serde_json::from_str(body).expect("JSON");
        found["repos"]
            .as_array()
            .unwrap_or_else(|| panic!("repos array in {found}"))
            .iter()
            .map(|r| r["name"].as_str().unwrap().to_string())
            .collect()
    };
    let search = "/v1/search/repos?q=acme&limit=25";

    let (status, body) = anon_get(&server, search);
    assert_eq!(status, 401, "an anonymous search was answered: {body}");
    assert!(!body.contains("secret"), "{body}");

    let (status, body) = get_as(&server, search, &rival);
    assert_eq!(status, 200, "another org was refused a search: {body}");
    assert_eq!(
        names(&body),
        Vec::<String>::new(),
        "another org found acme's repositories"
    );

    let (status, body) = get_as(&server, search, &admin);
    assert_eq!(status, 200, "{body}");
    let mut found = names(&body);
    found.sort();
    assert_eq!(found, ["one", "secret", "two"], "a member's search");

    // The protected listing stays protected — this is the property the
    // other three suites rest on, restated here so a future change to
    // this page cannot quietly reopen it.
    let (status, _) = anon_get(&server, "/v1/orgs/acme/repos");
    assert_eq!(
        status, 401,
        "the namespace listing stopped refusing strangers"
    );
    let (status, _) = get_as(&server, "/v1/orgs/acme/repos", &rival);
    assert_eq!(status, 404, "the namespace listing answered another org");
    assert!(server.healthy());
}
