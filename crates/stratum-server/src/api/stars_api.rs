//! `GET`/`PUT`/`DELETE` one person's star on one repository.
//!
//! Reading is open to anyone who may read the repository, including a
//! stranger with no account: a public project's star count is part of
//! what a visitor is deciding on, and hiding it behind a session would
//! make every signed-out view of the forge look emptier than it is.
//!
//! Writing is a **person's** act, refused for a service token exactly
//! as `watch_api` refuses one. A token has no opinion about a project,
//! and a count that machines can move is a count that means nothing.
//!
//! The response keeps our number and the origin's in separate fields
//! and never adds them. `origin` is an object or it is `null` — there
//! is no zero standing in for "we have no imported number", because a
//! client that could not tell those apart would have to render one as
//! the other, and rendering "0 on GitHub" under a project with sixty
//! thousand stars is precisely the dishonesty this shape prevents.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use stratum_control::auth::Scope;
use stratum_control::stars::{self, Stars};

/// Somebody else's count, with enough beside it to be checkable.
#[derive(Serialize)]
pub struct OriginStars {
    pub stars: i32,
    /// When this was true. An imported count is a snapshot.
    pub at: Option<i64>,
    /// Where it came from, so a reader can go and verify it rather than
    /// take our word. `None` only if a count was imported for a
    /// repository with no origin recorded, which should not happen.
    pub url: Option<String>,
}

#[derive(Serialize)]
pub struct StarView {
    /// Ours. Always present, and `0` is an honest answer.
    pub stars: i32,
    /// Whether the person asking has starred it; `false` for a stranger.
    pub starred: bool,
    /// The origin's, or `null` when there is no imported number.
    /// Never summed with `stars`, at any layer.
    pub origin: Option<OriginStars>,
}

fn view(s: Stars, origin_url: Option<String>) -> StarView {
    StarView {
        stars: s.stars,
        starred: s.starred,
        origin: s.origin_stars.map(|stars| OriginStars {
            stars,
            at: s.origin_stars_at,
            url: origin_url,
        }),
    }
}

/// The same, or a response saying why there is no person. A service
/// token has no account, and "which repositories does this token like"
/// is not a question with an answer.
fn person(state: &SharedState, headers: &HeaderMap) -> Result<String, Response> {
    crate::api::caller_person(state, headers)?.ok_or_else(|| {
        json_error(
            StatusCode::UNAUTHORIZED,
            "starring is a person's act — sign in, or use a session rather than a service token",
        )
    })
}

/// GET …/star — both counts, and the asker's own state if they have one.
pub async fn get(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    // `rest_repo_auth` has already decided whether this caller may see
    // the repository at all. Who they *are* is a separate question, and
    // asked separately for the reason in `caller_user`: a signed-in
    // non-member reading a public project is a person, and must be told
    // whether they starred it.
    let user_id = match crate::api::caller_person(&state, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match stars::state(&state.db, &repo_row.id, user_id.as_deref()) {
        Ok(s) => Json(view(s, repo_row.origin_url)).into_response(),
        Err(e) => internal(e),
    }
}

/// PUT …/star — star it. Starring twice is starring once.
pub async fn put(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let user_id = match person(&state, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match stars::star(&state.db, &repo_row.id, &user_id) {
        Ok(s) => Json(view(s, repo_row.origin_url)).into_response(),
        Err(e) => internal(e),
    }
}

/// DELETE …/star — unstar it. Unstarring something you never starred
/// is a request for a state you are already in, not an error.
pub async fn delete(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let user_id = match person(&state, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match stars::unstar(&state.db, &repo_row.id, &user_id) {
        Ok(s) => Json(view(s, repo_row.origin_url)).into_response(),
        Err(e) => internal(e),
    }
}
