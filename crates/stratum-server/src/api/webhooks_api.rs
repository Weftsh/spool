//! Repos webhook subscriptions API.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::Scope;
use stratum_control::webhooks;

#[derive(Deserialize)]
pub struct CreateBody {
    pub url: String,
}

pub async fn create(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> Response {
    let (org, repo, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org_name, &repo_name, Scope::RepoWrite)
        {
            Ok(x) => x,
            Err(r) => return r,
        };
    if !body.url.starts_with("http://") && !body.url.starts_with("https://") {
        return json_error(StatusCode::BAD_REQUEST, "url must be http(s)");
    }
    match webhooks::create(&state.db, &org.id, &repo.id, &body.url) {
        Ok(sub) => {
            // Where a repo's events are sent is worth a trail entry: it
            // is an egress path somebody added.
            crate::api::record_or_warn(
                &state.db,
                &AuditCtx::of(&org.id, principal.as_ref()),
                Some(&repo.id),
                "webhook.create",
                Some(&serde_json::json!({ "id": sub.id, "url": sub.url })),
            );
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "id": sub.id,
                    "url": sub.url,
                    // Shown once; verify deliveries with it.
                    "secret": sub.secret,
                })),
            )
                .into_response()
        }
        Err(e) => internal(e),
    }
}

pub async fn list(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo, _) = match crate::app::rest_repo_auth(
        &state,
        &headers,
        &org_name,
        &repo_name,
        Scope::RepoRead,
    ) {
        Ok(x) => x,
        Err(r) => return r,
    };
    match webhooks::for_repo(&state.db, &repo.id) {
        Ok(subs) => Json(serde_json::json!({ "subscriptions": subs })).into_response(),
        Err(e) => internal(e),
    }
}

pub async fn delete(
    State(state): State<SharedState>,
    Path((org_name, repo_name, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, repo, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org_name, &repo_name, Scope::RepoWrite)
        {
            Ok(x) => x,
            Err(r) => return r,
        };
    match webhooks::delete(&state.db, &org.id, &id) {
        Ok(true) => {
            crate::api::record_or_warn(
                &state.db,
                &AuditCtx::of(&org.id, principal.as_ref()),
                Some(&repo.id),
                "webhook.delete",
                Some(&serde_json::json!({ "id": id })),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => authx::not_found(),
        Err(e) => internal(e),
    }
}
