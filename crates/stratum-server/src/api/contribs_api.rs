//! One repository's contributors: who has worked on it, most first.

use crate::api::{internal, json_error, present};
use crate::app::SharedState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::collections::HashMap;
use stratum_control::auth::Scope;
use stratum_control::contribs;

/// `GET /v1/orgs/:org/repos/:repo/contributors?limit=N`
///
/// Who has worked on this repository, most first — the About rail's
/// avatar strip and the Insights tab read the same rows.
///
/// **Everything about who may see this is decided here, and nowhere
/// else.** [`stratum_control::contribs::contributors`] says so in its own
/// doc comment: it has no `public` filter, deliberately, because
/// `contributions.public` is a snapshot the walker wrote and filtering on
/// it would silently drop the rows a member reads back from their *own*
/// private repository. That makes this line the whole of the guard, so it
/// is the same `rest_repo_auth` every other repo-scoped read uses, with
/// the same `Scope::RepoRead` — which is what makes a private repository
/// answer a stranger exactly as a repository that does not exist answers
/// them. A new endpoint is a new way to learn that something exists, and
/// the only defence that survives is not having a second opinion about
/// visibility in the first place.
///
/// A repository nobody has committed to answers `{"contributors": []}`
/// with a 200. An empty list is a fact about a real repository — a 404
/// there would say "no such repository", which is a different and false
/// statement, and one a client would render as a broken page.
///
/// `?limit=` is passed through and clamped rather than refused, for the
/// reason `contributors` gives: a large number is somebody asking a good
/// question and wanting more of the answer than we serve. A `limit` that
/// is not a number is a different thing entirely — nobody meant it, so
/// answering the default would quietly serve a page nobody asked for.
/// That is named, in a sentence, with a 400.
///
/// A *blank* `?limit=` is neither: it is a cleared control, and it
/// reads as absence. See [`crate::api::present`].
pub async fn contributors(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    // The default is the ceiling: a caller who names no limit wants the
    // rail filled, and `contributors` will not hand back more than
    // `MAX_CONTRIBUTORS` however it is asked.
    let limit = match present(&params, "limit") {
        None => contribs::MAX_CONTRIBUTORS,
        Some(s) => match s.parse::<i32>() {
            Ok(n) => n,
            Err(_) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    format!("limit must be a whole number, not {s:?}"),
                )
            }
        },
    };
    match contribs::contributors(&state.db, &repo_row.id, limit) {
        // A named array rather than a bare one, as everywhere else here:
        // a top-level array is a response shape that can never grow a
        // field without breaking every client that reads it.
        Ok(people) => Json(serde_json::json!({ "contributors": people })).into_response(),
        Err(e) => internal(e),
    }
}
