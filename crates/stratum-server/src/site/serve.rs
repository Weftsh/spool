//! Answering a request for a customer's site.
//!
//! Called only from the dispatch layer, and only once that layer has
//! decided the `Host` belongs to the sites domain. Nothing here consults
//! a session, a token or a cookie: the whole point of the separate
//! domain is that there are no credentials on it, and a handler that
//! read one would be the bug that undoes it.
//!
//! What a response carries, and why:
//!
//! * **A strong `ETag`, the blob's own object id.** Content-addressed,
//!   so `If-None-Match` is exact rather than a heuristic, and a 304
//!   costs the edge a header exchange instead of a file.
//! * **`X-Content-Type-Options: nosniff`.** These bytes belong to a
//!   stranger. Without it a `.txt` containing markup is sniffed as HTML
//!   and runs as the customer's origin.
//! * **`Strict-Transport-Security`.** We terminate TLS for every site,
//!   so we are the ones who can promise this.
//!
//! And what it does not carry: no `Content-Security-Policy`. The page is
//! the customer's, and a default policy from us would break their own
//! scripts on their own domain. Isolation here is the domain boundary,
//! not a header.

use crate::app::SharedState;
use crate::site::{path as spath, tree};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use std::time::Instant;
use stratum_control::sites;

/// How long a browser is told to remember that this host is HTTPS-only.
/// A year, which is what the preload list requires.
const HSTS: &str = "max-age=31536000; includeSubDomains";

/// HTML must revalidate so a publish takes effect at once; everything
/// else may sit in a cache for a day. Both are floors rather than the
/// last word — a deploy invalidates the edge, and these only matter if
/// that invalidation is late or lost.
const CACHE_HTML: &str = "public, max-age=0, must-revalidate";
const CACHE_ASSET: &str = "public, max-age=86400";

fn is_html(ct: &str) -> bool {
    ct.starts_with("text/html")
}

/// A plain page for the cases where there is no customer content to
/// serve. Deliberately not the dashboard and not the marketing site:
/// this domain never renders product UI.
fn plain(status: StatusCode, title: &str, message: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title>\
         <style>body{{font:16px/1.5 system-ui,sans-serif;margin:4rem auto;max-width:34rem;padding:0 1rem;color:#111}}\
         h1{{font-size:1.25rem;margin:0 0 .5rem}}p{{margin:0;color:#555}}</style>\
         <h1>{title}</h1><p>{message}</p>"
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

fn not_a_site() -> Response {
    plain(
        StatusCode::NOT_FOUND,
        "No site here",
        "There is no site published at this address.",
    )
}

fn nothing_published() -> Response {
    plain(
        StatusCode::NOT_FOUND,
        "Nothing published yet",
        "This site exists but has not published a deploy.",
    )
}

/// Finish a response with the headers every site response carries.
fn finish(mut resp: Response, secure: bool) -> Response {
    let h = resp.headers_mut();
    h.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    if secure {
        h.insert(header::STRICT_TRANSPORT_SECURITY, HSTS.parse().unwrap());
    }
    resp
}

/// Are this deployment's sites served over TLS, so that promising HTTPS
/// is honest?
///
/// A property of the deployment, deliberately not of the request, and
/// the difference is not pedantry — reading it per request gets the
/// wrong answer for every real viewer.
///
/// CloudFront speaks plain **HTTP** to the load balancer, so the ALB
/// stamps `X-Forwarded-Proto: http`, which is true about its own hop and
/// says nothing about the viewer's. CloudFront's own
/// `CloudFront-Forwarded-Proto` arrives only if the origin request policy
/// forwards CloudFront headers, and the distribution uses
/// `Managed-AllViewer`, which does not. A browser sends no header saying
/// it used TLS. So a per-request check reads "not secure" for everybody
/// and `Strict-Transport-Security` is silently never sent — a promise
/// quietly not made is worse than one not attempted.
///
/// What is actually true: the sites distribution redirects HTTP to
/// HTTPS, so a request that reached this origin came from a viewer on
/// HTTPS. `public_url` is the same signal the session cookie's `Secure`
/// flag is derived from (`api::auth_api::set_cookie`), and it is right
/// for the local stack too, which is plain HTTP and makes no promise.
fn sites_are_https(public_url: &str) -> bool {
    public_url.starts_with("https://")
}

/// The middleware that sits in front of the whole router.
///
/// It is a layer rather than a route because the router already ends in
/// a fallback that serves the marketing site and then the dashboard's
/// single-page app. A site request that reached routing would therefore
/// not 404 — it would answer a customer's domain with our product's UI.
/// Pre-empting is the only placement that is correct.
///
/// Everything it does not recognise is passed straight through, so on a
/// deployment with no sites domain this is one string comparison and the
/// server behaves exactly as it did before the feature existed.
pub async fn layer(
    axum::extract::State(state): axum::extract::State<SharedState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match crate::site::dispatch::classify(host.as_deref(), state.sites_domain.as_deref()) {
        crate::site::dispatch::Dispatch::Product => next.run(req).await,
        crate::site::dispatch::Dispatch::Apex => {
            // The apex is ours, but it is not the product and must never
            // render product UI here — that would put the dashboard back
            // on the domain we separated for cookie isolation.
            let secure = sites_are_https(&state.public_url);
            finish(
                (
                    StatusCode::FOUND,
                    [(header::LOCATION, state.public_url.clone())],
                )
                    .into_response(),
                secure,
            )
        }
        crate::site::dispatch::Dispatch::Site(label) => {
            let secure = sites_are_https(&state.public_url);
            let (parts, _) = req.into_parts();
            // A published directory is readable and nothing else. The
            // layer pre-empts routing for *every* method, so without
            // this a `POST` to a site would be answered with the file —
            // no state changes, but it is not what a static host does
            // and it invites somebody to build on the mistake.
            if !matches!(parts.method, Method::GET | Method::HEAD) {
                return finish(method_not_allowed(), secure);
            }
            let head = parts.method == Method::HEAD;
            let resp = serve(&state, &label, &parts.uri, &parts.headers, secure).await;
            // A HEAD answers exactly what a GET would, without the body.
            // Routing would have done this; the layer bypassed routing,
            // so it is ours to do — and the headers are kept, because
            // the whole point of a HEAD is to learn them.
            if head {
                let (p, _) = resp.into_parts();
                return Response::from_parts(p, axum::body::Body::empty());
            }
            resp
        }
    }
}

fn method_not_allowed() -> Response {
    let mut r = plain(
        StatusCode::METHOD_NOT_ALLOWED,
        "Not allowed here",
        "A published site answers GET and HEAD.",
    );
    r.headers_mut()
        .insert(header::ALLOW, "GET, HEAD".parse().unwrap());
    r
}

/// Serve a request that dispatch has already decided is for `label`.
pub async fn serve(
    state: &SharedState,
    label: &str,
    uri: &Uri,
    headers: &HeaderMap,
    secure: bool,
) -> Response {
    let start = Instant::now();
    // Refuse before touching the store: a hostile path must not cost a
    // read, and a `..` has no legitimate answer to look up.
    let spath::Clean::Ok(clean) = spath::clean(uri.path()) else {
        return finish(not_a_site(), secure);
    };
    let trailing = spath::had_trailing_slash(uri.path());

    let Ok(Some(site)) = sites::by_host(&state.db, label) else {
        return finish(not_a_site(), secure);
    };
    let Some(current) = site.current.clone() else {
        return finish(nothing_published(), secure);
    };
    let Ok(Some(deploy)) = sites::deploy(&state.db, &current) else {
        return finish(nothing_published(), secure);
    };
    let Ok(Some(repo)) = stratum_control::registry::repo_by_id_any(&state.db, &site.repo_id) else {
        return finish(not_a_site(), secure);
    };
    let repo_id = repo.id.clone();
    let prefix = repo.prefix().as_str().to_string();

    let want = spath::candidates(&clean, trailing);
    let tree_oid = deploy.tree_oid.clone();
    let spa = deploy.spa;
    let not_found_page = deploy.not_found.clone();
    let clean_for_task = clean.clone();

    let found = crate::api::reads::with_reader(state, prefix, move |reader| {
        // The bare path first: if it is a directory, the answer is a
        // redirect and no candidate should be read at all.
        if !trailing && !clean_for_task.is_empty() {
            if let tree::Found::Dir = tree::find(reader, &tree_oid, &clean_for_task)? {
                return Ok(Outcome::RedirectToDir);
            }
        }
        for cand in &want {
            match tree::find(reader, &tree_oid, cand)? {
                tree::Found::File { oid, data } => {
                    return Ok(Outcome::File {
                        path: cand.clone(),
                        oid,
                        data,
                    })
                }
                tree::Found::Dir | tree::Found::Missing => continue,
            }
        }
        // Nothing matched. The fallback is part of the same read, so a
        // miss costs one round of lookups rather than two.
        if spa {
            if let tree::Found::File { oid, data } = tree::find(reader, &tree_oid, "index.html")? {
                return Ok(Outcome::Spa { oid, data });
            }
        } else if let Some(p) = &not_found_page {
            if let tree::Found::File { oid, data } = tree::find(reader, &tree_oid, p)? {
                return Ok(Outcome::NotFoundPage {
                    path: p.clone(),
                    oid,
                    data,
                });
            }
        }
        Ok(Outcome::Miss)
    })
    .await;

    let outcome = match found {
        Ok(o) => o,
        // A store failure is ours, not the visitor's, and must not be
        // reported as "no site here" — that would read as a deleted
        // site to somebody whose site is fine.
        Err(_) => {
            return finish(
                plain(
                    StatusCode::BAD_GATEWAY,
                    "Temporarily unavailable",
                    "This site could not be read just now. Please try again.",
                ),
                secure,
            )
        }
    };

    let resp = match outcome {
        Outcome::RedirectToDir => (
            StatusCode::MOVED_PERMANENTLY,
            [
                (header::LOCATION, spath::redirect_for(&clean)),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
        )
            .into_response(),
        Outcome::File { path, oid, data } => body(&path, &oid, data, StatusCode::OK, headers),
        Outcome::Spa { oid, data } => body("index.html", &oid, data, StatusCode::OK, headers),
        Outcome::NotFoundPage { path, oid, data } => {
            body(&path, &oid, data, StatusCode::NOT_FOUND, headers)
        }
        Outcome::Miss => plain(
            StatusCode::NOT_FOUND,
            "Page not found",
            "There is nothing at this address on this site.",
        ),
    };

    // Metered under its own kind from the first commit, so that a later
    // decision to price site traffic differently has history to set the
    // rate from. See `metering::egress_kind`.
    let resp = crate::metering::meter_response(
        resp,
        &state.meter,
        &repo_id,
        crate::metering::site_kind(),
        start,
    );
    finish(resp, secure)
}

enum Outcome {
    RedirectToDir,
    File {
        path: String,
        oid: String,
        data: Vec<u8>,
    },
    Spa {
        oid: String,
        data: Vec<u8>,
    },
    NotFoundPage {
        path: String,
        oid: String,
        data: Vec<u8>,
    },
    Miss,
}

/// One file's response, including the conditional-request shortcut.
fn body(path: &str, oid: &str, data: Vec<u8>, status: StatusCode, headers: &HeaderMap) -> Response {
    let ct = tree::content_type(path);
    let cache = if is_html(ct) { CACHE_HTML } else { CACHE_ASSET };
    let etag = format!("\"{oid}\"");
    // Content-addressed, so this is an identity check rather than a
    // guess: the same oid is the same bytes.
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|p| p.trim() == etag));
    if fresh && status == StatusCode::OK {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag.as_str()),
                (header::CACHE_CONTROL, cache),
            ],
        )
            .into_response();
    }
    (
        status,
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, cache),
            (header::ETAG, etag.as_str()),
        ],
        data,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_revalidates_and_assets_may_sit_in_a_cache() {
        assert!(is_html("text/html; charset=utf-8"));
        assert!(!is_html("text/css; charset=utf-8"));
        assert!(!is_html("application/octet-stream"));
    }

    #[test]
    fn a_page_we_render_ourselves_never_carries_product_ui_or_a_cache() {
        let r = not_a_site();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            r.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store",
            "our own pages must not be cached at a customer's address"
        );
    }

    #[test]
    fn every_site_response_says_nosniff_and_https_when_it_is_https() {
        let r = finish(not_a_site(), true);
        assert_eq!(
            r.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert_eq!(
            r.headers().get(header::STRICT_TRANSPORT_SECURITY).unwrap(),
            HSTS
        );

        // Over plain HTTP — the local stack — the promise would be a lie
        // and is not made.
        let r = finish(not_a_site(), false);
        assert_eq!(
            r.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert!(r.headers().get(header::STRICT_TRANSPORT_SECURITY).is_none());
    }

    /// The bug this replaced: reading `X-Forwarded-Proto` gave `http`
    /// for every real viewer, because that header describes
    /// CloudFront's plain-HTTP hop to the load balancer and not the
    /// viewer's connection. HSTS would have been silently never sent.
    #[test]
    fn https_is_a_fact_about_the_deployment_not_about_the_hop() {
        assert!(sites_are_https("https://weft.sh"));
        assert!(sites_are_https("https://weft.sh/"));
        assert!(!sites_are_https("http://127.0.0.1:8080"));
        assert!(!sites_are_https(""));
        // Not fooled by the scheme appearing later in the string.
        assert!(!sites_are_https("http://x/https://y"));
    }

    #[test]
    fn a_matching_etag_is_answered_without_the_body() {
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, "\"abc123\"".parse().unwrap());
        let r = body("a.html", "abc123", b"hello".to_vec(), StatusCode::OK, &h);
        assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
    }

    #[test]
    fn a_different_etag_gets_the_body() {
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, "\"other\"".parse().unwrap());
        let r = body("a.html", "abc123", b"hello".to_vec(), StatusCode::OK, &h);
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers().get(header::ETAG).unwrap(), "\"abc123\"");
    }

    /// A browser sends the whole list it holds.
    #[test]
    fn one_match_in_a_list_of_etags_is_enough() {
        let mut h = HeaderMap::new();
        h.insert(
            header::IF_NONE_MATCH,
            "\"x\", \"abc123\", \"y\"".parse().unwrap(),
        );
        let r = body("a.html", "abc123", b"hello".to_vec(), StatusCode::OK, &h);
        assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
    }

    /// A 404 page is still a 404. Answering 304 for it would leave the
    /// browser showing a cached *success* for a page that is missing.
    #[test]
    fn a_custom_not_found_page_is_never_downgraded_to_a_304() {
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, "\"abc123\"".parse().unwrap());
        let r = body(
            "404.html",
            "abc123",
            b"gone".to_vec(),
            StatusCode::NOT_FOUND,
            &h,
        );
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn html_and_assets_get_the_cache_headers_they_should() {
        let h = HeaderMap::new();
        let html = body("a.html", "o1", b"x".to_vec(), StatusCode::OK, &h);
        assert_eq!(
            html.headers().get(header::CACHE_CONTROL).unwrap(),
            CACHE_HTML
        );
        let css = body("a.css", "o2", b"x".to_vec(), StatusCode::OK, &h);
        assert_eq!(
            css.headers().get(header::CACHE_CONTROL).unwrap(),
            CACHE_ASSET
        );
    }
}
