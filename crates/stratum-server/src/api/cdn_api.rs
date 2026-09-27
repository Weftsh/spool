//! The CDN pack **origin route**: `GET /v1/orgs/:org/repos/:repo/cdn/:pack`.
//!
//! This is the serving half of the offload for deployments whose CDN
//! fronts this server rather than the bucket (local compose, self-hosted,
//! any edge without CloudFront key pairs). It is also the only part of
//! the offload path whose authorization can be exercised end to end in
//! CI — CloudFront's own signed-URL validation happens at the edge.
//!
//! It authorizes by a short-lived token in the query string, never by a
//! header, because git fetches an advertised `packfile-uri` with **no
//! credentials at all** (measured). The token is HMAC'd over the full
//! pack key, so it grants exactly one object in exactly one tenant.

use crate::api::internal;
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use stratum_store::{LatencyModel, ObjectStore};

/// Packs are named `<tip>-<hash>.pack` by the worker. Anything else is
/// refused before it can reach the store — the segment lands in an object
/// key, so traversal and injection are rejected by construction.
fn valid_pack_name(name: &str) -> bool {
    name.len() <= 128
        && name.ends_with(".pack")
        && name
            .trim_end_matches(".pack")
            .split('-')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_hexdigit()))
}

pub async fn pack(
    State(state): State<SharedState>,
    Path((org_name, repo_name, pack_name)): Path<(String, String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(cfg) = state.cdn.as_ref() else {
        return authx::not_found();
    };
    let Some(secret) = cfg.origin_secret.as_deref() else {
        // Store-origin deployments serve packs from the bucket; this route
        // is not a second, unsigned way in.
        return authx::not_found();
    };
    if !valid_pack_name(&pack_name) {
        return authx::not_found();
    }
    // Resolution is deliberately unauthenticated *and* masked: a wrong or
    // expired token is indistinguishable from a repo that does not exist,
    // so the route leaks nothing about other tenants.
    let (_, repo) = match crate::app::repo_or_404(&state, &org_name, &repo_name) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let key = format!("{}/cdn/{pack_name}", repo.prefix().as_str());
    let exp = params.get("exp").and_then(|v| v.parse::<u64>().ok());
    let sig = params.get("sig").map(String::as_str).unwrap_or_default();
    let ok = exp.is_some_and(|exp| {
        crate::cdn::verify_origin_token(secret, &key, exp, sig, crate::cdn::now_secs())
    });
    if !ok {
        return authx::not_found();
    }
    let store_url = state.store_url.clone();
    let fetch = key.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        ObjectStore::new(&store_url, LatencyModel::None).get(&fetch)
    })
    .await
    .unwrap_or_else(|e| Err(format!("task join: {e}")));
    let body = match bytes {
        Ok(b) => b,
        // A pack that is gone is a 404, not a 500: the descriptor may have
        // been superseded between advertisement and fetch.
        Err(e) if stratum_engine::errclass::is_absent(&e) => return authx::not_found(),
        Err(e) => return internal(e),
    };
    // Packs are immutable (named by tip + content hash), so a public repo's
    // pack is cacheable at the edge for as long as the edge likes. A
    // private repo's must never be served from a shared cache — the token
    // is what authorizes it, and tokens expire.
    let cache = if repo.public {
        "public, max-age=31536000, immutable".to_string()
    } else {
        "private, no-store".to_string()
    };
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/x-git-packfile".to_string(),
            ),
            (header::CACHE_CONTROL, cache),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_worker_shaped_pack_names_are_accepted() {
        assert!(valid_pack_name(&format!(
            "{}-{}.pack",
            "a".repeat(40),
            "b".repeat(40)
        )));
        assert!(valid_pack_name("deadbeef-cafe.pack"));
        // Traversal, absolute keys, and non-pack objects are all refused
        // before the name can reach the object store.
        assert!(!valid_pack_name("../../current.json"));
        assert!(!valid_pack_name("a/b.pack"));
        assert!(!valid_pack_name("/etc/passwd"));
        assert!(!valid_pack_name("current.json"));
        assert!(!valid_pack_name(".pack"));
        assert!(!valid_pack_name("-.pack"));
        assert!(!valid_pack_name("zz.pack"));
        assert!(!valid_pack_name("abc-.pack"));
        assert!(!valid_pack_name(&format!("{}.pack", "a".repeat(200))));
    }
}
