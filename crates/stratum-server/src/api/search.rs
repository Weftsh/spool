//! Finding a repository you have not been given a link to.
//!
//! Every other read route in this API starts from a namespace and a name
//! the caller already knows. This one starts from a word, and crosses
//! namespaces — which makes it the only route where getting the
//! visibility rule wrong leaks the *existence* of private work rather
//! than its contents.
//!
//! So visibility here is defined once, in
//! [`stratum_control::registry::Viewer`], and it is the same rule
//! `members::effective_role` enforces everywhere else: a namespace you
//! belong to. A per-repo grant does not appear, because a
//! grant without membership is not access — if that ever changes, it
//! changes in one place and this follows.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use stratum_control::registry::{self, Viewer};

#[derive(Deserialize)]
pub struct SearchParams {
    #[serde(default)]
    pub q: Option<String>,
    /// Narrow to repositories carrying exactly this topic.
    ///
    /// Separate from `q` rather than a qualifier inside it, because the
    /// two mean different things: `q` is what somebody typed and matches
    /// a topic as loosely as it matches a description, while this is
    /// what a topic *pill* means and has to be exact. A pill that also
    /// matched repositories merely mentioning the word in prose would
    /// make the facet useless for the one job a facet has.
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub after: Option<String>,
}

/// Who is asking, owned so the borrowed [`Viewer`] can be built from it.
///
/// Extracted when `/v1/search/topics` arrived: both routes have to scope
/// to the same person, and resolving that twice is how two endpoints end
/// up disagreeing about who may see what — the exact failure this
/// module's header warns about for the repo search.
enum ViewerOwned {
    User(String),
    Org(String),
}

impl ViewerOwned {
    fn as_viewer(&self) -> Viewer<'_> {
        match self {
            ViewerOwned::User(u) => Viewer::User(u),
            ViewerOwned::Org(o) => Viewer::Org(o),
        }
    }
}

fn resolve_viewer(state: &SharedState, headers: &HeaderMap) -> Result<ViewerOwned, Response> {
    // A bad token is still a 401 — a caller with a typo'd credential
    // must be told, not quietly downgraded to anonymous and shown a
    // shorter list they cannot explain.
    let principal = authx::principal_opt(&state.db, headers, authx::Challenge::None)?;
    let user_from_session = match principal {
        Some(_) => None,
        None => crate::app::session_user(state, headers)?,
    };
    // Nothing here is searchable without signing in. A repo-bound
    // service token is refused rather than treated as its org: it was
    // minted to reach one repository, and a search is not that
    // repository.
    match (&principal, &user_from_session) {
        (Some(p), _) if p.repo_id.is_none() => Ok(match p.user_id.as_deref() {
            Some(u) => ViewerOwned::User(u.to_string()),
            None => ViewerOwned::Org(p.org_id.clone()),
        }),
        (Some(_), _) => Err(authx::forbidden(
            "a token bound to one repository cannot search; use a personal or organization token",
        )),
        (None, Some(user_id)) => Ok(ViewerOwned::User(user_id.clone())),
        (None, None) => Err(authx::unauthorized(authx::Challenge::None)),
    }
}

/// `GET /v1/search/repos`
///
/// Signed in only: a person sees the namespaces they belong to, and an
/// organization token its own.
pub async fn repos(
    State(state): State<SharedState>,
    Query(params): Query<SearchParams>,
    headers: HeaderMap,
) -> Response {
    let q = params.q.unwrap_or_default();
    let limit = params.limit.unwrap_or(25);
    let viewer_owned = match resolve_viewer(&state, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let viewer = viewer_owned.as_viewer();
    let after = params.after.as_deref();
    let topic = params.topic.as_deref().filter(|t| !t.is_empty());
    match registry::search_repos(&state.db, &q, topic, &viewer, after, limit) {
        Ok(hits) => {
            let effective = limit.clamp(1, 100);
            let next = (hits.len() == effective)
                .then(|| hits.last().map(registry::cursor_of))
                .flatten();
            Json(serde_json::json!({ "repos": hits, "next": next })).into_response()
        }
        Err(e) if e.starts_with("invalid") => json_error(StatusCode::BAD_REQUEST, e),
        Err(e) => internal(e),
    }
}

/// `GET /v1/search/topics`
///
/// The topics people are actually using, most-used first, scoped to what
/// the caller may see — a topic carried only by another namespace's
/// repositories is invisible, and so is the fact that it exists.
///
/// Beside repo search rather than under a repository, because a topic is
/// not a property of one: the question this answers is "what is in the
/// namespaces I work in".
pub async fn topics(
    State(state): State<SharedState>,
    Query(params): Query<TopicsParams>,
    headers: HeaderMap,
) -> Response {
    let viewer_owned = match resolve_viewer(&state, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let viewer = viewer_owned.as_viewer();
    match registry::topics_in_use(&state.db, &viewer, params.limit.unwrap_or(50)) {
        Ok(ts) => Json(serde_json::json!({
            "topics": ts
                .into_iter()
                .map(|(name, repos)| serde_json::json!({ "name": name, "repos": repos }))
                .collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct TopicsParams {
    #[serde(default)]
    pub limit: Option<usize>,
}
