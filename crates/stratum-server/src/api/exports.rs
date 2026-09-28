//! Export (R6): async bundle jobs — no lock-in is a stated product
//! principle. The bundle is a standard `git bundle` any git can clone,
//! built from a materialized layout (which itself passes the fsck gate).

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::Scope;
use stratum_control::jobs;
use stratum_store::{LatencyModel, ObjectStore};

/// POST /…/export — start a bundle export job.
pub async fn start(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, repo, principal) = match crate::app::rest_repo_auth(
        &state,
        &headers,
        &org_name,
        &repo_name,
        Scope::RepoRead,
    ) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let job = match jobs::create(&state.db, &org.id, Some(&repo.id), "export", None) {
        Ok(j) => j,
        Err(e) => return internal(e),
    };
    let ctx = AuditCtx::of(&org.id, Some(&principal));
    crate::api::record_or_warn(
        &state.db,
        &ctx,
        Some(&repo.id),
        "repo.export",
        Some(&serde_json::json!({ "job": job.id })),
    );
    spawn_export(
        state.clone(),
        job.id.clone(),
        repo.prefix().as_str().to_string(),
    );
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "job": job.id, "state": "queued" })),
    )
        .into_response()
}

pub fn spawn_export(state: SharedState, job_id: String, prefix: String) {
    tokio::spawn(async move {
        let db = state.db.clone();
        let store_url = state.store_url.clone();
        let jid = job_id.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let store = ObjectStore::new(&store_url, LatencyModel::None);
            let work = std::env::temp_dir().join(format!("stratum-export-{jid}"));
            std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
            let bundle = work.join("export.bundle");
            let result =
                stratum_engine::materialize::export_bundle(&store, &prefix, &work, &bundle);
            let key = format!("{prefix}/exports/{jid}.bundle");
            let out = result.and_then(|()| {
                let bytes = std::fs::read(&bundle).map_err(|e| e.to_string())?;
                store
                    .put(&key, &bytes, stratum_store::PutCond::None)
                    .map_err(|e| e.to_string())?;
                Ok(key)
            });
            let _ = std::fs::remove_dir_all(&work);
            out
        })
        .await
        .unwrap_or_else(|e| Err(format!("task join: {e}")));
        match out {
            Ok(key) => {
                let _ = jobs::complete(&db, &job_id, Some(&key));
            }
            Err(e) => {
                let _ = jobs::fail(&db, &job_id, &e);
            }
        }
    });
}

/// GET /…/export/:job — job status.
pub async fn status(
    State(state): State<SharedState>,
    Path((org_name, repo_name, job_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, _repo, _) = match crate::app::rest_repo_auth(
        &state,
        &headers,
        &org_name,
        &repo_name,
        Scope::RepoRead,
    ) {
        Ok(x) => x,
        Err(r) => return r,
    };
    match jobs::get(&state.db, &org.id, &job_id) {
        Ok(Some(job)) => Json(serde_json::json!({
            "job": job.id,
            "state": job.state,
            "error": job.error,
            "download": (job.state == "done").then(|| format!(
                "{}/v1/orgs/{org_name}/repos/{repo_name}/export/{}/download",
                state.public_url, job.id
            )),
        }))
        .into_response(),
        Ok(None) => authx::not_found(),
        Err(e) => internal(e),
    }
}

/// GET /…/export/:job/download — the bundle bytes.
pub async fn download(
    State(state): State<SharedState>,
    Path((org_name, repo_name, job_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, _repo, _) = match crate::app::rest_repo_auth(
        &state,
        &headers,
        &org_name,
        &repo_name,
        Scope::RepoRead,
    ) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let job = match jobs::get(&state.db, &org.id, &job_id) {
        Ok(Some(j)) => j,
        Ok(None) => return authx::not_found(),
        Err(e) => return internal(e),
    };
    if job.state != "done" {
        return json_error(StatusCode::CONFLICT, format!("export is {}", job.state));
    }
    let Some(key) = job.result else {
        return internal("done job without result key".into());
    };
    let store_url = state.store_url.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        ObjectStore::new(&store_url, LatencyModel::None).get(&key)
    })
    .await
    .unwrap_or_else(|e| Err(format!("task join: {e}")));
    match bytes {
        Ok(b) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{repo_name}.bundle\""),
                ),
            ],
            b,
        )
            .into_response(),
        Err(e) => internal(e),
    }
}

/// POST /v1/orgs/:org/export — org-wide bulk export: one job per repo.
pub async fn bulk(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        return r;
    }
    let repos = match stratum_control::registry::list_repos(&state.db, &org.id, None, 1000) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let mut started = Vec::new();
    for repo in repos {
        match jobs::create(&state.db, &org.id, Some(&repo.id), "export", None) {
            Ok(job) => {
                spawn_export(
                    state.clone(),
                    job.id.clone(),
                    repo.prefix().as_str().to_string(),
                );
                started.push(serde_json::json!({ "repo": repo.name, "job": job.id }));
            }
            Err(e) => {
                started.push(serde_json::json!({ "repo": repo.name, "error": e }));
            }
        }
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "exports": started })),
    )
        .into_response()
}
