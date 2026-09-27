//! The check reads: a repository's run history, and one run.
//!
//! `api::checks_intake` is the other half of this surface and says at
//! length why Stratum accepts verdicts and never produces one. This half
//! only has to hand them back, and almost all of the thinking is about
//! two things: who may see them, and what a filter the caller got wrong
//! should do.
//!
//! **Visibility is not decided here.** Both handlers open with the same
//! `app::rest_repo_auth(.., Scope::RepoRead)` every other repo-scoped
//! read uses, and neither of them has a second opinion about it
//! afterwards. That is what makes a private repository answer a stranger
//! exactly as a repository that was never created — and a new endpoint
//! is a new way to learn that something exists, so the only defence that
//! survives contact with the next endpoint is not having a second
//! opinion in the first place. It also means the check happens *before*
//! any argument is parsed: a 400 about a malformed `limit` on a private
//! repository would be a 400 a stranger could only have got from a
//! repository that exists.
//!
//! **A filter the caller got wrong is refused, not dropped.** A `state`
//! of `"success"` is somebody's honest guess at our vocabulary, and the
//! two available answers are a sentence naming the six we accept, or a
//! page of runs that do not match what they asked for. The second reads
//! as the filter being broken, and it reads that way to a person who has
//! no way to find out otherwise. [`stratum_control::checks::RunState::parse`]
//! already writes the sentence; this module's whole contribution is to
//! carry it out as a 400 rather than swallow it.
//!
//! `limit` is the deliberate exception and is **clamped rather than
//! refused** — [`stratum_control::checks::MAX_LIMIT`] argues that one
//! and it is not re-litigated here. The distinction is between a value
//! that means something we cannot fully honour (a thousand rows: give
//! them the biggest page we have and a cursor) and a value that means
//! nothing at all (`"lots"`: nobody meant it, and answering the default
//! serves a page nobody asked for).

use crate::api::{internal, json_error};
use crate::app::{self, SharedState};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use std::collections::HashMap;
use stratum_control::auth::Scope;
use stratum_control::checks::{self, CheckRun, RunQuery, RunState};

/// One page of history, everything the Checks tab needs to draw itself.
#[derive(Serialize)]
pub struct RunsView {
    pub runs: Vec<CheckRun>,
    /// Every workflow name the repository has ever reported, riding
    /// along with **every** page rather than living behind a second
    /// endpoint.
    ///
    /// The left rail's filter list has to be drawn before the reader can
    /// pick a filter, so a client that had to fetch it separately would
    /// make two requests to render one screen, and the rail would pop in
    /// after the table on every load. It is a `DISTINCT` over an index
    /// and costs far less than the round trip it saves.
    ///
    /// It is deliberately **not** narrowed by the current filter: a rail
    /// that only lists the workflow you already picked is a rail you
    /// cannot use to pick another one.
    pub workflows: Vec<String>,
    /// The cursor for the next page — a `created_at` — or `null` on the
    /// last one. Computed by `checks::list`, which is the only place
    /// that knows where the page boundary actually fell.
    pub next_before: Option<i64>,
}

/// A query-string value, with blank read as absent.
///
/// `?branch=` is what a `<select>` set back to "All branches" submits,
/// and it means the reader cleared the filter. Taking it literally would
/// match `ref_name = ''`, which matches nothing, so the page would empty
/// itself the moment somebody stopped filtering — the exact opposite of
/// what they asked for, with no error to explain it.
fn present<'a>(params: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .map(String::as_str)
        .filter(|s| !s.is_empty())
}

/// A numeric query parameter, or a 400 that names the value.
///
/// The message quotes what arrived rather than only what was expected:
/// the caller is usually a client assembling a URL, and "not `"NaN"`"
/// points straight at the bug where "must be a whole number" alone sends
/// somebody to read our documentation about a parameter they got right
/// in every other respect.
fn whole(params: &HashMap<String, String>, key: &str) -> Result<Option<i64>, Response> {
    match present(params, key) {
        None => Ok(None),
        Some(s) => s.parse::<i64>().map(Some).map_err(|_| {
            json_error(
                StatusCode::BAD_REQUEST,
                format!("{key} must be a whole number, not {s:?}"),
            )
        }),
    }
}

/// `GET /v1/orgs/:org/repos/:repo/checks/runs?branch=&state=&event=&actor=&workflow=&limit=&before=`
///
/// A repository's check runs, newest first.
///
/// A repository nobody has ever reported a run for answers
/// `{"runs": [], "workflows": [], "next_before": null}` with a 200.
/// Empty is a fact about a repository that exists; a 404 there would be
/// the different, false statement that it does not, and a client would
/// render it as a broken page rather than as an empty tab.
pub async fn list(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead)
    {
        Ok(x) => x,
        Err(r) => return r,
    };
    let run_state = match present(&params, "state") {
        None => None,
        Some(s) => match RunState::parse(s) {
            Ok(v) => Some(v),
            Err(e) => return json_error(StatusCode::BAD_REQUEST, e),
        },
    };
    let limit = match whole(&params, "limit") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let before = match whole(&params, "before") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let q = RunQuery {
        branch: present(&params, "branch"),
        state: run_state,
        event: present(&params, "event"),
        actor: present(&params, "actor"),
        workflow: present(&params, "workflow"),
        // The default is the ceiling, as it is for the contributors
        // rail: a caller who names no limit wants the page filled, and
        // `checks::list` will not hand back more than `MAX_LIMIT`
        // however it is asked.
        limit: limit.unwrap_or(checks::MAX_LIMIT),
        before,
    };
    let (runs, next_before) = match checks::list(&state.db, &repo_row.id, &q) {
        Ok(x) => x,
        Err(e) => return internal(e),
    };
    let workflows = match checks::workflow_names(&state.db, &repo_row.id) {
        Ok(w) => w,
        Err(e) => return internal(e),
    };
    Json(RunsView {
        runs,
        workflows,
        next_before,
    })
    .into_response()
}

/// `GET /v1/orgs/:org/repos/:repo/checks/runs/:id`
///
/// One run, read through the repository it belongs to.
///
/// A run in *another* repository is a 404 with the same body as an id
/// that was never issued, and the two are indistinguishable because
/// `checks::get` scopes on `repo_id` inside the query rather than
/// fetching the row and comparing afterwards. That distinction is not
/// stylistic: a fetch-then-compare has the answer in memory before it
/// decides, and every later edit to this handler is one mistake away
/// from letting a field out of it.
pub async fn get(
    State(state): State<SharedState>,
    Path((org, repo, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead)
    {
        Ok(x) => x,
        Err(r) => return r,
    };
    match checks::get(&state.db, &repo_row.id, &id) {
        Ok(Some(run)) => Json(run).into_response(),
        // `checks::get` returns one `None` for an id of the wrong shape,
        // an id from another repository, and an id nobody ever minted,
        // and it cannot tell us which. That is deliberate on its side,
        // so this arm must not undo it: no branch on the reason, and
        // nothing logged about the distinction. A message that said
        // "wrong repository" would turn the scoped query back into the
        // existence oracle it exists to avoid being.
        Ok(None) => json_error(StatusCode::NOT_FOUND, "no such check run"),
        Err(e) => internal(e),
    }
}

/// The verdicts standing beside one commit.
#[derive(Serialize)]
pub struct CommitChecksView {
    pub runs: Vec<CheckRun>,
}

/// `GET /v1/orgs/:org/repos/:repo/commits/:sha/checks`
///
/// One verdict per workflow for a single commit — the newest of each,
/// not every run that ever reported against it.
///
/// **That collapse is the whole reason this is not `list` with a filter,
/// and it belongs to the reader rather than to the query.** A provider
/// polls, so one build arrives as `queued`, then `running`, then
/// `failing`, then — after somebody pushes a fix and re-runs — `passing`.
/// Rendering all of them puts "build: failing" from twenty minutes ago
/// above "build: passing" from two, and a reader who scans the first
/// line concludes the commit is broken.
/// [`stratum_control::checks::latest_for_commit`] makes that argument at
/// length and keys on the workflow's `name`, because the name is what
/// the reader recognises and two rows both labelled "build" beside one
/// commit is exactly the ambiguity being removed.
///
/// The `sha` is lowercased on the way in; see the comment at the call.
///
/// A commit with no runs is `{"runs": []}` and a 200. So is a `sha` that
/// could never have been stored — the control plane treats a value
/// carrying a control byte as "nothing matched" rather than letting it
/// reach the query, where it would surface as a 500 instead of an
/// honest empty answer. Neither case is a 404: the 404 on this route
/// belongs to the *repository*, and spending it on a commit as well
/// would make "no such project" and "nothing has built this yet"
/// indistinguishable to a client that can only see a status code.
///
/// Nothing here validates that the sha names a reachable object, and
/// that is deliberate for the reason the column's own doc comment gives:
/// a run can be reported for a commit that has since been rewritten
/// away, and refusing to show it would lose the only record that it
/// happened.
pub async fn for_commit(
    State(state): State<SharedState>,
    Path((org, repo, sha)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead)
    {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Lowercased before it reaches the query, because the write side
    // lowercases too (`checks_intake::normalise_commit`) and one half of
    // that pair without the other orphans rows. Hex is conventionally
    // case-insensitive and an uppercase sha reaches a URL easily — a
    // `tr` in a pipeline, a Windows toolchain, a person retyping one —
    // and this route's honest empty answer is exactly the shape a
    // case-sensitive miss takes, so nothing would ever have reported it.
    // Non-ASCII is untouched by `to_ascii_lowercase`, so the control
    // plane's own "unmatchable" guard still sees what was sent.
    match checks::latest_for_commit(&state.db, &repo_row.id, &sha.to_ascii_lowercase()) {
        Ok(runs) => Json(CommitChecksView { runs }).into_response(),
        Err(e) => internal(e),
    }
}
