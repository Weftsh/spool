//! `GET`/`PUT` a person's subscription to one repository.
//!
//! Deliberately about a *person*, not a principal: a service token has
//! no mailbox, so "what does this token want to hear about" is not a
//! question with an answer. A token-authenticated caller is refused
//! here rather than silently given somebody's settings.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use stratum_control::auth::Scope;
use stratum_control::watches::{self, Level};

#[derive(Serialize)]
pub struct WatchView {
    pub level: String,
}

#[derive(Deserialize)]
pub struct SetWatch {
    pub level: String,
}

/// The person behind this request, or a response explaining why there
/// isn't one. Reading a subscription needs no more than read access to
/// the repository — you may watch anything you may see.
fn person(
    state: &SharedState,
    headers: &HeaderMap,
    org: &str,
    repo: &str,
) -> Result<(stratum_control::Repo, String), Response> {
    let (_, repo_row, _) = crate::app::rest_repo_auth(state, headers, org, repo, Scope::RepoRead)?;
    // Resolved separately from the principal `rest_repo_auth` returns.
    // That principal answers "what authority has this caller on this
    // repository", which is a different question from who they are: for
    // a session or an unbound token the two name the same person, but a
    // repo-bound token carries authority here and is still nobody — it
    // was minted to reach one repository, and a subscription is a
    // person's.
    let user_id = match authx::principal_opt(&state.db, headers, authx::Challenge::None)? {
        Some(p) if p.repo_id.is_none() => p.user_id,
        Some(_) => None,
        None => crate::app::session_user(state, headers)?,
    };
    let user_id = user_id.ok_or_else(|| {
        json_error(
            StatusCode::UNAUTHORIZED,
            "watching is a person's setting — sign in, or use a session rather than a token",
        )
    })?;
    Ok((repo_row, user_id))
}

/// GET …/watch — what this person has chosen, defaulting to
/// `participating` when they have chosen nothing.
pub async fn get(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (repo_row, user_id) = match person(&state, &headers, &org, &repo) {
        Ok(x) => x,
        Err(r) => return r,
    };
    match watches::level_for(&state.db, &repo_row.id, &user_id) {
        Ok(level) => Json(WatchView {
            level: level.as_str().to_string(),
        })
        .into_response(),
        Err(e) => internal(e),
    }
}

/// PUT …/watch — choose. Choosing the default clears the row rather than
/// storing it, so "never decided" and "decided on the default" stay one
/// state instead of two that behave identically and read differently.
pub async fn put(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SetWatch>,
) -> Response {
    let (repo_row, user_id) = match person(&state, &headers, &org, &repo) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let level = match Level::parse(&body.level) {
        Ok(l) => l,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, e),
    };
    match watches::set_level(&state.db, &repo_row.id, &user_id, level) {
        Ok(()) => Json(WatchView {
            level: level.as_str().to_string(),
        })
        .into_response(),
        Err(e) => internal(e),
    }
}
