//! Starting an issue import, and asking how it is going.
//!
//! The import itself is a job; this is the door to it. Two things are
//! decided here rather than in the worker, because both are answers a
//! person needs *before* anything long-running starts:
//!
//! * whether this repository may be imported into at all — a tracker
//!   with issues in it is refused, because an import keeps the numbers
//!   it came with;
//! * whether there is an upstream to import *from* — the same GitHub
//!   origin the repository already mirrors its commits from.
//!
//! Both are refusals with sentences. An import that starts and fails an
//! hour later, in a log, is a much worse answer than one that never
//! starts and says why.

use crate::api::{internal, json_error};
use crate::app::{self, SharedState};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use stratum_control::auth::Scope;
use stratum_control::imports;

/// `POST /v1/orgs/:org/repos/:repo/import` — begin importing issues.
///
/// Admin, not write: an import writes a project's entire history into a
/// tracker and cannot be undone by hand. It is the same authority that
/// changes a repository's visibility, and for the same reason.
pub async fn start(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org_row, repo_row, _) =
        match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::OrgAdmin) {
            Ok(v) => v,
            Err(r) => return r,
        };
    if repo_row.origin_url.is_none() || repo_row.origin_installation.is_none() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "this repository has no GitHub origin to import from. Connect it as a \
             mirror first, so the import reads the same upstream the commits do.",
        );
    }
    match imports::may_import(&state.db, &repo_row.id) {
        Ok(Ok(())) => {}
        Ok(Err(why)) => return json_error(StatusCode::CONFLICT, why),
        Err(e) => return internal(e),
    }
    if let Err(e) = crate::workers::importer::enqueue(&state, &org_row.id, &repo_row.id) {
        return internal(e);
    }
    // **202, not 201.** Nothing has been imported yet: a job exists.
    // Answering `Created` would name a thing the caller could go and
    // look at, and there is not one for some minutes.
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "queued",
            "detail": "Issues are being imported. Numbers are preserved, so an \
                       old #reference keeps meaning what it says.",
        })),
    )
        .into_response()
}

/// `GET /v1/orgs/:org/repos/:repo/import` — how far it has got.
///
/// Reported per phase rather than as one percentage, because the phases
/// are the thing that resumes and a single number would be invented.
pub async fn status(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org_row, repo_row, _) =
        match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(v) => v,
            Err(r) => return r,
        };
    let phase =
        |p: &str| -> Option<String> { imports::cursor(&state.db, &repo_row.id, p).ok().flatten() };
    // **What happened, not only how far it got.**
    //
    // The three cursors below are identical for an import still walking
    // its first page and an import that was refused on it — so for as
    // long as this route answered cursors alone, "we were not allowed to
    // look" and "still working" were the same reply. That is the
    // confusion `workers/importer.rs` opens by naming: an import that
    // quietly produces nothing looks exactly like a project that never
    // had issues, and the sentence explaining a missing `issues: read`
    // was written, recorded on the job, and read by nobody.
    //
    // A read failure here is reported as an absent job rather than a
    // 500: the phases are the answer to the question that was asked, and
    // losing them because the job table was briefly unavailable would be
    // the worse trade.
    let job =
        stratum_control::jobs::latest_for_repo(&state.db, &org_row.id, &repo_row.id, "import")
            .ok()
            .flatten();
    Json(serde_json::json!({
        "labels": phase("labels"),
        "milestones": phase("milestones"),
        // `"done"`, a URL meaning "resuming from here", or null for not
        // started. The URL is deliberately visible: an operator watching
        // a large import should be able to see it move.
        "issues": phase("issues"),
        // `queued`/`running`/`done`/`failed`, or null when no import has
        // ever been asked for.
        "state": job.as_ref().map(|j| j.state.clone()),
        // The reason, in the words the worker recorded — which for a
        // refusal is a sentence naming the permission to grant, not a
        // status code.
        "error": job.as_ref().and_then(|j| j.error.clone()),
        "updated_at": job.as_ref().map(|j| j.updated_at),
    }))
    .into_response()
}
