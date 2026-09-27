//! Metrics endpoints (M6): per-repo aggregates with p50/p99, JSON or CSV
//! (the renewal artifact is exportable by design), org usage rows, and
//! ops endpoints (compact / gc).

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::collections::HashMap;
use stratum_control::auth::Scope;
use stratum_control::ids::now_ms;
use stratum_control::metrics;

/// GET /v1/orgs/:org/repos/:repo/metrics?from=&to=&format=csv
///
/// **Members only, including on a public repository.** Publishing the
/// code does not publish how the code is used: how often it is cloned,
/// how many bytes it serves, how far behind its origin a mirror runs.
/// Those are the owner's numbers, and on a public repository they would
/// otherwise be world-readable by anyone who guessed the path.
///
/// This used to take `Scope::RepoRead` through `rest_repo_auth`, which
/// has a public-read fallback — so on a public repository it answered an
/// anonymous caller in full. The UI never linked to it, which is
/// precisely why it went unnoticed: an endpoint nothing points at is
/// still an endpoint.
///
/// Two calls rather than one, and the order matters. `repo_or_masked`
/// first, so a repository that is private-and-not-yours goes on
/// answering exactly as it did — the existence masking must not become
/// distinguishable from the new refusal. Then `authx::require`, which is
/// the same call `repos::viewer_member` makes for the row the client
/// draws its tab from, so the tab and the route cannot disagree about
/// who is a member.
pub async fn repo_metrics(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (org, repo) = match crate::app::repo_or_masked(&state, &headers, &org_name, &repo_name) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = authx::require(
        &state.db,
        &headers,
        &org.id,
        Some(&repo.id),
        Scope::RepoRead,
    ) {
        return r;
    }
    let to = params
        .get("to")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(now_ms);
    let from = params
        .get("from")
        .and_then(|v| v.parse().ok())
        .unwrap_or(to - 24 * 3600 * 1000);
    let db = state.db.clone();
    let repo_id = repo.id.clone();
    let out = tokio::task::spawn_blocking(move || metrics::query_repo(&db, &repo_id, from, to))
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")));
    let m = match out {
        Ok(m) => m,
        Err(e) => return internal(e),
    };
    if params.get("format").map(String::as_str) == Some("csv") {
        let mut csv = String::from("kind,count,bytes,ms_sum,p50_ms,p99_ms\n");
        for (kind, s) in &m.kinds {
            csv.push_str(&format!(
                "{kind},{},{},{},{},{}\n",
                s.count,
                s.bytes,
                s.ms_sum,
                s.p50_ms.map(|v| v.to_string()).unwrap_or_default(),
                s.p99_ms.map(|v| v.to_string()).unwrap_or_default(),
            ));
        }
        return (StatusCode::OK, [(header::CONTENT_TYPE, "text/csv")], csv).into_response();
    }
    Json(serde_json::json!({
        "repo": repo_name,
        "from": from,
        "to": to,
        "kinds": m.kinds,
        "sync": {
            "last_sync_at": repo.last_sync_at,
            "sync_error": repo.sync_error,
        },
    }))
    .into_response()
}

/// GET /v1/orgs/:org/usage — billing rollup rows (dashboard + renewal).
pub async fn org_usage(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = authx::require(&state.db, &headers, &org.id, None, Scope::OrgRead) {
        return r;
    }
    let db = state.db.clone();
    let org_id = org.id.clone();
    let rows = tokio::task::spawn_blocking(move || metrics::usage_days(&db, &org_id, 90))
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")));
    match rows {
        Ok(days) => Json(serde_json::json!({ "days": days })).into_response(),
        Err(e) => internal(e),
    }
}

/// POST /…/compact — synchronous fold (ops/tests; the worker does this
/// automatically after writes).
pub async fn compact_now(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, repo, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org_name, &repo_name, Scope::RepoWrite)
        {
            Ok(x) => x,
            Err(r) => return r,
        };
    let job = match stratum_control::jobs::create(
        &state.db,
        &org.id,
        Some(&repo.id),
        "compact-now",
        None,
    ) {
        Ok(j) => j,
        Err(e) => return internal(e),
    };
    match crate::workers::compactor::run_one(&state, &job).await {
        Ok(outcome) => {
            let _ =
                stratum_control::jobs::complete(&state.db, &job.id, Some(&format!("{outcome:?}")));
            Json(serde_json::json!({ "outcome": format!("{outcome:?}") })).into_response()
        }
        Err(e) => {
            let _ = stratum_control::jobs::fail(&state.db, &job.id, &e);
            internal(e)
        }
    }
}

/// POST /…/cdn-pack — synchronously rebuild the repo's CDN pack
/// (git `packfile-uri` offload). Same shape as compact_now: operators get
/// a manual trigger, and it is the deterministic entry point tests drive
/// instead of waiting on the poll loop.
pub async fn cdn_pack_now(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, repo, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org_name, &repo_name, Scope::RepoWrite)
        {
            Ok(x) => x,
            Err(r) => return r,
        };
    let job = match stratum_control::jobs::create(
        &state.db,
        &org.id,
        Some(&repo.id),
        "cdnpack-now",
        None,
    ) {
        Ok(j) => j,
        Err(e) => return internal(e),
    };
    match crate::workers::cdnpack::run_one(&state, &job).await {
        Ok(outcome) => {
            let _ =
                stratum_control::jobs::complete(&state.db, &job.id, Some(&format!("{outcome:?}")));
            Json(serde_json::json!({ "outcome": format!("{outcome:?}") })).into_response()
        }
        Err(e) => {
            let _ = stratum_control::jobs::fail(&state.db, &job.id, &e);
            internal(e)
        }
    }
}

/// POST /…/gc {grace_secs} — synchronous epoch GC (ops/tests).
pub async fn gc_now(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let (_, repo, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org_name, &repo_name, Scope::RepoWrite)
        {
            Ok(x) => x,
            Err(r) => return r,
        };
    let grace = body
        .as_ref()
        .and_then(|Json(v)| v["grace_secs"].as_u64())
        .unwrap_or(86_400);
    let store_url = state.store_url.clone();
    let prefix = repo.prefix().as_str().to_string();
    // Same resolver as the background sweeper. An operator-triggered
    // sweep that answered the liveness question differently from the
    // scheduled one would be a way to delete a fork's data by hand.
    let refs = crate::workers::gc::DbEpochRefs::new(state.db.clone(), repo.id.clone());
    let out = tokio::task::spawn_blocking(move || {
        let store = stratum_store::ObjectStore::new(&store_url, stratum_store::LatencyModel::None);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        stratum_engine::gc::gc_epochs_with_refs(&store, &prefix, grace, now, &refs)
    })
    .await
    .unwrap_or_else(|e| Err(format!("join: {e}")));
    match out {
        Ok(report) => Json(serde_json::json!({
            "epochs_seen": report.epochs_seen,
            "epochs_deleted": report.epochs_deleted,
            "objects_deleted": report.objects_deleted,
        }))
        .into_response(),
        Err(e) => json_error(StatusCode::BAD_GATEWAY, e),
    }
}
