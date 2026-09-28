//! The public forge pages: `/{owner}` and `/{owner}/{repo}/…` render the
//! dashboard SPA to a signed-out visitor, served from the router's
//! fallback rather than from a route of their own.
//!
//! Doing it in the fallback is what keeps the marketing site in front of
//! the forge: a root `.route("/:owner")` would have matchit prefer a
//! parameterised segment over the Astro files answered by the same
//! fallback, so `/mirror`, `/repos`, `/monorepo`, `/gitfarm` and
//! `/discover` would have quietly become somebody's profile. These tests
//! hold that ordering, and hold the two things it must not disturb: a
//! typo is still a real 404, and the git wire is untouched.

use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::{Minio, Server};

/// A server with both asset dirs configured, the way a real deployment
/// runs. The contents stand in for the built site and SPA: what is being
/// tested is which file the fallback chooses, not what Astro or Vite
/// emit.
fn spawn_with_assets(store_url: &str, scratch: &Scratch) -> Server {
    let site = scratch.path().join("site");
    std::fs::create_dir_all(site.join("monorepo")).unwrap();
    std::fs::create_dir_all(site.join("unreserved")).unwrap();
    std::fs::write(site.join("index.html"), "<h1>marketing home</h1>").unwrap();
    std::fs::write(
        site.join("monorepo/index.html"),
        "<h1>the monorepo product page</h1>",
    )
    .unwrap();
    // A page that ships *without* a denylist entry behind it — the state
    // the site was actually in for `monorepo`. See the ordering test.
    std::fs::write(
        site.join("unreserved/index.html"),
        "<h1>a page nobody reserved</h1>",
    )
    .unwrap();

    let dash = scratch.path().join("dashboard");
    std::fs::create_dir_all(&dash).unwrap();
    std::fs::write(dash.join("index.html"), "<div id=\"root\">SPA SHELL</div>").unwrap();

    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint("forge-pages")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_SITE_DIR", site.display().to_string())
        .env("STRATUM_DASHBOARD_DIR", dash.display().to_string())
        .env("STRATUM_COMPACT_POLL_SECS", "86400")
        .start()
}

/// GET with no credentials at all, reporting `(status, body)` — an
/// anonymous browser, which is the visitor this whole surface exists
/// for. `Server::req` parses JSON; these answers are HTML.
fn anon_get(server: &Server, path: &str) -> (u16, String) {
    let resp = match ureq::get(&format!("{}{path}", server.base)).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("transport GET {path}: {e}"),
    };
    let status = resp.status();
    (status, resp.into_string().unwrap_or_default())
}

#[test]
fn public_forge_urls_render_the_spa_and_typos_still_404() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forge-pages");
    let scratch = Scratch::new("forge-pages");
    let server = spawn_with_assets(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "widget" })),
    );
    server.req(
        "POST",
        "/v1/orgs/acme/repos/widget/commits",
        &admin,
        Some(serde_json::json!({
            "message": "seed",
            "operations": [{"op": "put", "path": "readme", "content": "hello\n"}],
        })),
    );

    // The namespace root and every depth beneath it are the SPA, to a
    // visitor holding no session and no token.
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

    // A private repo is the SPA too, and that is deliberate: the shell
    // carries no repo content, and its API call gets the existing
    // masking. A 404 here would be a *worse* answer — it would tell an
    // anonymous visitor that a name is absent when a member browsing the
    // same URL must be able to load the page.
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "secret" })),
    );
    let (st, body) = anon_get(&server, "/acme/secret");
    assert_eq!(st, 200);
    assert!(body.contains("SPA SHELL"));

    // A namespace nobody has taken is a real 404, not a soft one. This
    // is the reason the fallback pays for a lookup instead of gating on
    // name shape alone: a 200 here would make every typo and every
    // crawler probe an indexable page indistinguishable from a profile.
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

    // The site keeps precedence, and `monorepo` cannot show that on its
    // own: it is *also* reserved, so the SPA branch would decline it
    // whichever order the two were tried in. The claim that needs its
    // own evidence is the one the design rests on — the site is looked
    // up **first**, so a page that shipped before anyone reserved its
    // name is safe even against a namespace that already exists. That is
    // precisely the state `/monorepo` was in, and the reason not to do
    // this with a root `.route("/:owner")`.
    server.bootstrap_org("unreserved");
    let (st, body) = anon_get(&server, "/unreserved");
    assert_eq!(st, 200);
    assert!(
        body.contains("a page nobody reserved"),
        "an existing namespace shadowed a live site page: {body:?}"
    );

    let (st, body) = anon_get(&server, "/monorepo");
    assert_eq!(st, 200);
    assert!(
        body.contains("the monorepo product page"),
        "the SPA shadowed a marketing page: {body:?}"
    );
    let (st, body) = anon_get(&server, "/");
    assert_eq!(st, 200);
    assert!(body.contains("marketing home"), "{body:?}");

    // And the denylist is the second lock on the same door: `monorepo`
    // cannot be taken as a namespace at all. Asserted through the org
    // creation path rather than against `is_reserved` directly, because
    // what matters is that the refusal reaches a person typing a name.
    let taken = server.admin(&["admin", "bootstrap", "--org", "monorepo"]);
    assert!(
        taken.is_err(),
        "monorepo was available as a namespace: {taken:?}"
    );

    // The git wire is registered as real routes, so it matches before
    // the fallback is ever consulted: an anonymous clone of the public
    // repo still works and still fscks (I11).
    let base = server.base.strip_prefix("http://").unwrap();
    gitcli::clone_and_fsck(
        &format!("http://{base}/acme/widget.git"),
        &scratch.path().join("anon-clone"),
    );
    // Including the un-suffixed form, which shares its first two
    // segments with the SPA's repo URL and must still be git.
    gitcli::clone_and_fsck(
        &format!("http://{base}/acme/widget"),
        &scratch.path().join("anon-clone-bare"),
    );
    // A ref advert reached by hand — no `Git-Protocol` header — is
    // refused by the git handler for wanting protocol v2. That refusal
    // is the evidence: the route answered, the fallback never saw it.
    // Had the SPA claimed this path it would have been a cheerful 200
    // of HTML, which stock git would have tried to parse as pkt-lines.
    let (st, body) = anon_get(&server, "/acme/widget/info/refs?service=git-upload-pack");
    assert_eq!(st, 400, "{body:?}");
    assert!(
        body.contains("protocol v2 required"),
        "the SPA answered a git request: {body:?}"
    );

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

/// A stranger discovers a namespace's public repositories through
/// search, and never sees its private ones.
///
/// This is the public profile page's data source, and it is search
/// rather than `GET /v1/orgs/{org}/repos` on purpose. That listing is a
/// protected endpoint — three suites depend on it refusing both absent
/// and bad credentials (`api_edges_e2e` uses it to prove REST sends no
/// Basic challenge, `http_pentest_e2e` uses it as the target where
/// hostile headers must never yield 200, and `git_e2e` uses it for the
/// cross-tenant 404). Widening it to serve anonymous callers tripped all
/// three, which is a contract saying so three times.
///
/// The page was reading the wrong endpoint. Search already applies
/// `registry::Viewer` for every kind of caller and was built for exactly
/// this question.
#[test]
fn a_stranger_finds_only_the_public_repositories_of_a_namespace() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forge-list");
    let scratch = Scratch::new("forge-list");
    let server = spawn_with_assets(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    for (name, public) in [("open-one", true), ("open-two", true), ("secret", false)] {
        server.req(
            "POST",
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name })),
        );
    }

    let (status, body) = anon_get(&server, "/v1/search/repos?q=acme&limit=25");
    assert_eq!(status, 200, "a stranger was refused the search: {body}");
    let found: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    let names: Vec<&str> = found["repos"]
        .as_array()
        .expect("repos array")
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"open-one") && names.contains(&"open-two"),
        "public repositories missing from what a stranger can find: {names:?}"
    );
    assert!(
        !names.contains(&"secret"),
        "a private repository leaked to an anonymous search: {names:?}"
    );

    // The protected listing stays protected — this is the property the
    // other three suites rest on, restated here so a future change to
    // this page cannot quietly reopen it.
    let (status, _) = anon_get(&server, "/v1/orgs/acme/repos");
    assert_eq!(
        status, 401,
        "the namespace listing stopped refusing strangers"
    );
}
