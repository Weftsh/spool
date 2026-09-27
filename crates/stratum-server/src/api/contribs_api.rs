//! The contribution reads: one person's graph, and one repository's
//! contributors.
//!
//! `GET /v1/users/:handle/contributions` — the graph.
//!
//! Anonymous by design. A contribution graph is the first thing a
//! stranger looks at when they land on somebody's page, and it is the
//! single strongest argument for moving a decade of work here: it is
//! rendered from *their commits*, not from activity on this platform,
//! so it is already true the moment their repositories mirror over.
//!
//! **What this route must never leak, in one place.** It is a new way to
//! ask "does this person have a private repository", so:
//!
//! * repository names come only from
//!   [`stratum_control::contribs::graph`], which nulls them out for a
//!   private repository inside the query — a private name never reaches
//!   a Rust struct here, let alone a serializer;
//! * private work is a number added into the day's `count` and nothing
//!   else: no name, no id, no title, no link, and no separate field
//!   holding it, because a separate field is a subtraction away from
//!   being a private-repository detector;
//! * `private_included` is `false` both for somebody who has not opted
//!   in and for somebody with no private work at all. Those two are
//!   deliberately indistinguishable — distinguishing them would publish
//!   the existence of private work, which is the fact being protected.
//!
//! The answer does not depend on who is asking, and that is a decision
//! rather than an oversight: a public page that shows different numbers
//! to different readers is a page nobody can quote.

use crate::api::{internal, json_error, present};
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::collections::HashMap;
use stratum_control::auth::Scope;
use stratum_control::contribs::{self, GraphError};
use stratum_control::profiles;

/// The window a graph renders by default: the year ending today, which
/// is the shape every reader already recognises.
const DEFAULT_SPAN_DAYS: i32 = 364;

/// Today, in days since the epoch, in UTC.
///
/// The server's day, not the reader's: a graph whose last square moves
/// depending on the browser's timezone is a graph two people cannot
/// agree about. Individual *squares* are in the author's own timezone —
/// that is where the person's own day matters — but the window is one
/// window for everybody.
fn today() -> i32 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (secs / 86_400) as i32
}

/// `GET /v1/users/:handle/contributions?from=YYYY-MM-DD&to=YYYY-MM-DD`
pub async fn get(
    State(state): State<SharedState>,
    Path(handle): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let profile = match profiles::by_handle(&state.db, &handle) {
        Ok(Some((p, _))) => p,
        // Absent, suspended, or a name that could never have existed —
        // one answer for all three, the same one the profile route
        // gives.
        Ok(None) => return authx::not_found(),
        // A database that will not answer is a 500 and says so. Folding
        // it into the 404 above would make an outage read as "this
        // person does not exist", which is both a lie and the kind of
        // lie somebody acts on.
        Err(e) => return internal(e),
    };
    // Blank is absent, exactly as it is for `limit` below and for every
    // filter on `/checks/runs` — see [`crate::api::present`], which is
    // where that rule lives so every route can see it. A cleared date
    // picker submits `?from=&to=`, and reading that as two malformed
    // dates would 400 the graph for somebody who just stopped filtering.
    let to = match present(&params, "to") {
        Some(s) => match contribs::day_from_iso(s) {
            Ok(d) => d,
            Err(e) => return json_error(StatusCode::BAD_REQUEST, e),
        },
        None => today(),
    };
    let from = match present(&params, "from") {
        Some(s) => match contribs::day_from_iso(s) {
            Ok(d) => d,
            Err(e) => return json_error(StatusCode::BAD_REQUEST, e),
        },
        None => to - DEFAULT_SPAN_DAYS,
    };
    match contribs::graph(&state.db, &profile.user_id, from, to) {
        Ok(g) => Json(g).into_response(),
        // A window that is not a window is the caller's mistake and
        // says so. The two cases are told apart by their type, never by
        // matching on the text — a message somebody improves later must
        // not silently turn a 400 into a 500.
        Err(GraphError::BadRange(m)) => json_error(StatusCode::BAD_REQUEST, m),
        Err(GraphError::Failed(e)) => internal(e),
    }
}

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
