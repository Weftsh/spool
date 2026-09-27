//! `GET`/`PUT`/`DELETE /v1/users/:handle/follow`, and the two listings.
//!
//! Following is a **person's** act, refused for a service token exactly
//! as starring and watching are. A token was minted to reach a
//! repository; "which people does this token admire" is not a question
//! with an answer, and a follower count machines can move is a count
//! that means nothing.
//!
//! The counts are public, because they are counts of a public act. The
//! *listings* are public for the same reason, and both are bounded —
//! `follows` has no natural ceiling, so the page has one rather than
//! the database discovering it.
//!
//! Nothing here surfaces a repository, private or otherwise. That is
//! worth saying out loud because every other new surface in this release
//! is a way to leak that a private repository exists, and the way this
//! one avoids being another is by having no repository in it at all.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use stratum_control::follows::{self, FollowError};
use stratum_control::profiles;

/// The person this handle names, or the same 404 the profile route
/// gives for an absent, suspended, or impossible name.
fn target(state: &SharedState, handle: &str) -> Result<String, Response> {
    match profiles::by_handle(&state.db, handle) {
        Ok(Some((p, _))) => Ok(p.user_id),
        Ok(None) => Err(authx::not_found()),
        Err(e) => Err(internal(e)),
    }
}

/// The person asking, when a person is asking.
fn person(state: &SharedState, headers: &HeaderMap) -> Result<String, Response> {
    crate::api::caller_person(state, headers)?.ok_or_else(|| {
        json_error(
            StatusCode::UNAUTHORIZED,
            "following is a person's act — sign in, or use a session rather than a service token",
        )
    })
}

/// `GET …/follow` — the counts, and the asker's own edge if they have
/// an account. Anonymous is welcome and simply sees `you_follow: false`.
pub async fn get(
    State(state): State<SharedState>,
    Path(handle): Path<String>,
    headers: HeaderMap,
) -> Response {
    let user_id = match target(&state, &handle) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let viewer = match crate::api::caller_person(&state, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    match follows::state(&state.db, &user_id, viewer.as_deref()) {
        Ok(s) => Json(s).into_response(),
        Err(e) => internal(e),
    }
}

/// `PUT …/follow` — follow. Following twice is following once.
///
/// Following yourself is a 400 with a sentence. The table's
/// `CHECK (follower_id <> followed_id)` would refuse it too, but a check
/// violation reaches a person as a 500 and a paragraph of PostgreSQL,
/// which reads as "the site is broken" rather than "that is not a thing
/// you can do".
pub async fn put(
    State(state): State<SharedState>,
    Path(handle): Path<String>,
    headers: HeaderMap,
) -> Response {
    let user_id = match target(&state, &handle) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let me = match person(&state, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match follows::follow(&state.db, &me, &user_id) {
        Ok(s) => Json(s).into_response(),
        Err(FollowError::Yourself) => json_error(StatusCode::BAD_REQUEST, follows::SELF_FOLLOW),
        Err(FollowError::Failed(e)) => internal(e),
    }
}

/// `DELETE …/follow` — unfollow. Unfollowing somebody you never
/// followed is a request for the state you are already in.
pub async fn delete(
    State(state): State<SharedState>,
    Path(handle): Path<String>,
    headers: HeaderMap,
) -> Response {
    let user_id = match target(&state, &handle) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let me = match person(&state, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match follows::unfollow(&state.db, &me, &user_id) {
        Ok(s) => Json(s).into_response(),
        Err(e) => internal(e),
    }
}

/// `GET …/followers` — who follows this person, bounded.
pub async fn followers(State(state): State<SharedState>, Path(handle): Path<String>) -> Response {
    listing(state, handle, follows::followers).await
}

/// `GET …/following` — who this person follows, bounded.
pub async fn following(State(state): State<SharedState>, Path(handle): Path<String>) -> Response {
    listing(state, handle, follows::following).await
}

/// Both listings differ only in direction, and a second copy of the
/// resolve-then-read-then-serialize shape is a second place for the
/// bound to be forgotten.
async fn listing(
    state: SharedState,
    handle: String,
    read: fn(&stratum_control::ControlDb, &str) -> Result<Vec<follows::Person>, String>,
) -> Response {
    let user_id = match target(&state, &handle) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match read(&state.db, &user_id) {
        Ok(people) => Json(serde_json::json!({ "people": people })).into_response(),
        Err(e) => internal(e),
    }
}
