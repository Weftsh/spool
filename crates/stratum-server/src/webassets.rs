//! Web asset serving: the OpenAPI spec (compiled in — single source of
//! truth shared with the docs site), the marketing/docs site at `/`, and
//! the dashboard SPA at `/dashboard/` with an index.html fallback for
//! client-side routes. Both directories are optional: unset env vars mean
//! the server is API-only and unmatched paths 404 as before.

use crate::app::SharedState;
use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use std::path::{Path, PathBuf};

/// The OpenAPI 3.1 document. `include_str!` from the site's public dir so
/// the served spec and the one the docs site ships can never diverge.
const OPENAPI_JSON: &str = include_str!("../../../docs/openapi.json");

pub async fn openapi() -> Response {
    ([(header::CONTENT_TYPE, "application/json")], OPENAPI_JSON).into_response()
}

/// Resolve a URL path against a static root without ever letting the
/// request escape it: each segment is validated (no `..`, no absolute
/// jumps, no backslashes or NULs) and joined component-by-component, so
/// traversal is impossible by construction rather than by canonicalize.
fn safe_join(root: &Path, url_path: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for seg in url_path.split('/') {
        if seg.is_empty() {
            continue;
        }
        if seg == "." || seg == ".." || seg.contains('\\') || seg.contains('\0') {
            return None;
        }
        out.push(seg);
    }
    Some(out)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "woff2" => "font/woff2",
        "webmanifest" => "application/manifest+json",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

async fn serve_file(path: PathBuf) -> Option<Response> {
    let bytes = tokio::fs::read(&path).await.ok()?;
    let ct = content_type(&path);
    // Hashed bundle assets are immutable; HTML and everything else must
    // revalidate so deploys take effect immediately.
    let cache = if path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains('-') && (n.ends_with(".js") || n.ends_with(".css")))
    {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Some(
        (
            [(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, cache)],
            bytes,
        )
            .into_response(),
    )
}

/// Try `<root>/<path>`, then `<root>/<path>/index.html` (Astro emits
/// directory-per-page), then an optional SPA fallback to the root
/// `index.html` (the dashboard's client-side router owns unknown paths).
async fn serve_from(root: &Path, url_path: &str, spa_fallback: bool) -> Response {
    let Some(target) = safe_join(root, url_path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if target.is_file() {
        if let Some(r) = serve_file(target).await {
            return r;
        }
    } else {
        let index = target.join("index.html");
        if index.is_file() {
            if let Some(r) = serve_file(index).await {
                return r;
            }
        }
    }
    if spa_fallback {
        if let Some(r) = serve_file(root.join("index.html")).await {
            return r;
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

/// Router fallback: anything no API or git route claimed is looked up in
/// the site build (when configured), and failing that may be a public
/// forge page — `/{owner}` or `/{owner}/{repo}/…` — which the dashboard
/// SPA renders.
///
/// The forge pages are served *here*, from the fallback, rather than
/// from a root `.route("/:owner")`, and that is the whole design. matchit
/// prefers a parameterised segment to no route at all, so a root param
/// route would swallow `/mirror`, `/repos`, `/monorepo`, `/gitfarm` and
/// `/discover` — which are not routes but Astro files answered by this
/// same fallback. Looking the site up first means the static site keeps
/// precedence by construction, with nothing to remember when a page is
/// added.
///
/// The git wire needs no special handling and gets none: `/:org/:repo/
/// info/refs`, `git-upload-pack` and `git-receive-pack` are real routes
/// registered in `app::router`, so matchit answers them before anything
/// reaches the fallback. Nor is there any sniffing of `Accept` or the
/// method to tell a browser from a git client — the fallback is
/// registered `get(site)`, and by the time a request is here it has
/// already failed to be a git request by its *path*, which is a fact
/// about the URL rather than a guess about the client.
pub async fn site(State(state): State<SharedState>, uri: Uri) -> Response {
    if let Some(root) = &state.site_dir {
        let served = serve_from(root, uri.path(), false).await;
        if served.status() != StatusCode::NOT_FOUND {
            return served;
        }
    }
    forge_spa(&state, uri.path()).await
}

/// The top-level paths the dashboard SPA renders itself, as opposed to
/// the namespaces it renders *about*.
///
/// This is the server's half of `TOP_LEVEL` in
/// `web/dashboard/src/routes.ts`, and `docs_e2e` asserts the two lists
/// are identical — they are one decision written in two languages, and a
/// page that exists in one and not the other is either an unreachable
/// route or a namespace nobody can use. `dashboard` is absent because it
/// has real routes of its own and never reaches this fallback.
const SPA_SEGMENTS: &[&str] = &[
    "explore",
    "feed",
    "issues",
    "login",
    "notifications",
    "orgs",
    "search",
    "stars",
    "topics",
];

/// The dashboard shell for a public forge URL, or 404.
///
/// Three gates, in ascending cost, and the last one is a judgement call
/// worth stating. A name that is ill-formed or reserved is settled
/// without touching Postgres, which is what keeps scanner noise
/// (`/.env`, `/wp-login.php`) off the control plane. Only a name that
/// could actually be somebody's is looked up.
///
/// The lookup is deliberate rather than avoided. Gating on shape alone
/// would answer `200` with an SPA shell for *every* well-formed unknown
/// name, so every typo and every crawler probe becomes a soft 404 that
/// search engines index and a person cannot tell from a real profile.
/// One indexed lookup by name, on a request that already missed every
/// route and the entire site build, is less work than the route it would
/// otherwise have hit — and it leaks nothing, because namespace
/// existence is enumerable through signup already: taking a name that is
/// in use is refused by name.
///
/// The *repo* is not looked up, and must not be. Repo existence is
/// masked (401 anonymous / 404 authenticated-without-access) by
/// `app::repo_or_masked`, and a 404 here for a private repo the viewer
/// can actually read would break the masking in the other direction.
/// The shell is content-free; the SPA's own API call gets the masking
/// exactly as it stands.
async fn forge_spa(state: &crate::app::AppState, path: &str) -> Response {
    let Some(root) = &state.dashboard_dir else {
        // API-only deployment: nothing to serve, 404 as before.
        return StatusCode::NOT_FOUND.into_response();
    };
    let owner = path.trim_start_matches('/').split('/').next().unwrap_or("");
    if owner.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // The SPA's own pages come first, and they have to: every one of
    // these names is *reserved* precisely so no namespace can shadow it,
    // and the check below refuses reserved names. Those two rules were
    // written a few hours apart and quietly cancelled each other out —
    // `/explore` and `/search` 404'd at the server and the SPA never
    // loaded, so the header's search box led nowhere. Reserving a name
    // for a page and then refusing to serve that page is the kind of
    // thing only opening it in a browser finds.
    if SPA_SEGMENTS.contains(&owner) {
        return match serve_file(root.join("index.html")).await {
            Some(r) => r,
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    if !stratum_control::registry::valid_name(owner)
        || stratum_control::registry::is_reserved(owner)
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    // A control-plane failure lands in the same arm as "no such
    // namespace" on purpose: this function's only job is to choose
    // between the site's 404 and the SPA, and if we cannot establish
    // that the name is a namespace, the 404 we were already giving is
    // the honest answer.
    match stratum_control::registry::org_by_name(&state.db, owner) {
        Ok(Some(_)) => match serve_file(root.join("index.html")).await {
            Some(r) => r,
            None => StatusCode::NOT_FOUND.into_response(),
        },
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `/dashboard` and `/dashboard/*`: static assets from the SPA build,
/// index.html for anything else so client-side routes deep-link.
pub async fn dashboard(State(state): State<SharedState>, uri: Uri) -> Response {
    let Some(root) = &state.dashboard_dir else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let rest = uri
        .path()
        .strip_prefix("/dashboard")
        .unwrap_or("")
        .trim_start_matches('/');
    serve_from(root, rest, true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_blocks_traversal() {
        let root = Path::new("/srv/site");
        assert_eq!(
            safe_join(root, "/docs/index.html"),
            Some(PathBuf::from("/srv/site/docs/index.html"))
        );
        assert_eq!(
            safe_join(root, "//a//b"),
            Some(PathBuf::from("/srv/site/a/b"))
        );
        assert!(safe_join(root, "/../etc/passwd").is_none());
        assert!(safe_join(root, "/a/../../b").is_none());
        assert!(safe_join(root, "/a/./b").is_none());
        assert!(safe_join(root, "/a\\b").is_none());
        assert!(safe_join(root, "/a\0b").is_none());
    }

    #[test]
    fn openapi_is_valid_json_with_paths() {
        let v: serde_json::Value = serde_json::from_str(OPENAPI_JSON).expect("valid json");
        assert_eq!(v["openapi"].as_str().unwrap_or(""), "3.1.0");
        assert!(v["paths"].as_object().is_some_and(|p| p.len() > 10));
    }

    #[test]
    fn content_types_cover_bundles() {
        assert_eq!(
            content_type(Path::new("a/index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("a/app-B3sK.js")), "text/javascript");
        assert_eq!(
            content_type(Path::new("llms.txt")),
            "text/plain; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("openapi.json")), "application/json");
    }
}
