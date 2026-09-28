//! Issues: filing, reading, commenting, closing, labelling.
//!
//! **Filing takes `RepoRead`, not `RepoWrite`, and that is the whole
//! feature.** Every other write in this API is gated on write access to
//! the repository, because every other write changes the repository.
//! An issue does not. Gating it on `RepoWrite` would mean only people
//! who can already push may report a bug, which on an open-source
//! project is everybody who does not need the feature and nobody who
//! does. Commenting is the same act and takes the same scope.
//!
//! Labels are the exception and go the other way: `RepoWrite` only.
//! Labels are triage, triage is a maintainer's map of their own
//! backlog, and a stranger relabelling it is precisely the
//! attention-theft this product exists to refuse.
//!
//! Reads take `RepoRead` through `app::rest_repo_auth`, which answers a
//! signed-out caller 401 and one with no role on the repository the
//! masked 404 a missing one gets. Nothing here reimplements masking; a
//! surface that decides for itself who may see a repository is a
//! surface that will disagree with the rest of them.

use crate::api::{caller_person, internal, json_error};
use crate::app::{self, SharedState};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use stratum_control::auth::Scope;
use stratum_control::issues::{self, Comment, Counts, Filter, Issue, Label};

/// The caller as a **verified** person, or a response explaining why
/// there is none.
///
/// An issue has an author, and a service token is not one. The sentence
/// says which of the two fixes apply — sign in, or stop using a
/// machine credential for a human act — because "unauthorized" alone
/// sends people to re-mint a token that will fail in exactly the same
/// way.
///
/// **The verification check is the second half and it is not optional.**
/// Every other create path in this API calls `authx::require_verified`
/// before letting a caller make something that costs storage. Filing an
/// issue is a create path that needs nothing stronger than `RepoRead` —
/// a viewer may file — which makes it the widest create door on the
/// server, and it would have been the only one of them not to check. It
/// costs a legitimate contributor a click they have already made.
///
/// It is checked on the resolved **person** rather than through
/// `authx::require_verified`, and the reason is not that the latter is
/// weaker — it gates sessions perfectly well at the five call sites
/// that use it, because `authx::require` builds a `Principal` with a
/// `user_id` for a cookie caller. (An earlier version of this comment
/// said the opposite. It was wrong, and it was wrong in the direction
/// that would have sent somebody to add redundant checks at five sites
/// that already work.)
///
/// The reason is that this helper is about a *person*, and resolves one
/// through `caller_person` rather than reading it off the principal
/// `rest_repo_auth` returned: a token's person, or else the browser
/// session's, and nobody for a repo-bound token. For a session or an
/// unbound token that is the person the principal names; for a
/// repo-bound token it is deliberately nobody, because filing is a
/// person's act. The resolved person is the one thing every door here
/// produces, so it is what the check is made on.
fn person(state: &SharedState, headers: &HeaderMap) -> Result<String, Response> {
    let who = caller_person(state, headers)?.ok_or_else(|| {
        json_error(
            StatusCode::UNAUTHORIZED,
            "filing and commenting are a person's acts — sign in, or use a session rather than a service token",
        )
    })?;
    match stratum_control::usertokens::is_verified(&state.db, &who) {
        Ok(true) => Ok(who),
        Ok(false) => Err(json_error(
            StatusCode::FORBIDDEN,
            "confirm your email address before filing or commenting — check your \
             inbox for the link we sent, or ask for another one",
        )),
        Err(e) => Err(internal(e)),
    }
}

#[derive(Serialize)]
pub struct LabelView {
    pub id: String,
    pub name: String,
    /// A design-system token name, never a hex. See `stratum_control::issues`.
    pub color: String,
    pub description: String,
}

#[derive(Serialize)]
pub struct IssueView {
    pub number: i32,
    pub title: String,
    pub body: String,
    pub state: String,
    /// The author's handle, or `null` when the author has no account
    /// here. Never a guess: an imported issue keeps `author_label`
    /// instead, and the two are separate fields so that nothing can
    /// render an unmapped name as one of ours.
    pub author: Option<String>,
    pub author_label: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
    /// Objects, not names. A row renders a pill with a colour, and a
    /// client that got names would have to fetch the label list and
    /// join it per row to draw one.
    pub labels: Vec<LabelView>,
    pub comment_count: i64,
}

#[derive(Serialize)]
pub struct CountsView {
    pub open: i64,
    pub closed: i64,
}

#[derive(Serialize)]
pub struct ListView {
    pub issues: Vec<IssueView>,
    /// Both numbers, always, and never derived from the page above.
    /// The index shows "12 Open / 40 Closed" as a pair, and computing
    /// either from a filtered list is how that number goes wrong.
    pub counts: CountsView,
    pub next: Option<i32>,
}

#[derive(Serialize)]
pub struct CommentView {
    pub id: String,
    pub body: String,
    pub author: Option<String>,
    pub author_label: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn label_view(l: Label) -> LabelView {
    LabelView {
        id: l.id,
        name: l.name,
        color: l.color,
        description: l.description,
    }
}

fn issue_view(i: Issue) -> IssueView {
    IssueView {
        number: i.number,
        title: i.title,
        body: i.body,
        state: i.state,
        author: i.author,
        author_label: i.author_label,
        created_at: i.created_at,
        updated_at: i.updated_at,
        closed_at: i.closed_at,
        labels: i.labels.into_iter().map(label_view).collect(),
        comment_count: i.comment_count,
    }
}

fn comment_view(c: Comment) -> CommentView {
    CommentView {
        id: c.id,
        body: c.body,
        author: c.author,
        author_label: c.author_label,
        created_at: c.created_at,
        updated_at: c.updated_at,
    }
}

fn counts_view(c: Counts) -> CountsView {
    CountsView {
        open: c.open,
        closed: c.closed,
    }
}

/// A refusal the caller can act on.
///
/// The control plane's bounds are refusals with a sentence rather than
/// truncations (I13), and those sentences say which limit was hit and
/// what it is. They reach the client as 400 because they are all
/// statements about the request; anything else is ours and is a 500.
fn bad_request(e: String) -> Response {
    json_error(StatusCode::BAD_REQUEST, &e)
}

#[derive(Deserialize)]
pub struct NewIssue {
    pub title: String,
    #[serde(default)]
    pub body: String,
}

/// POST …/issues — file one. `RepoRead`, deliberately.
pub async fn create(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<NewIssue>,
) -> Response {
    let (_, r, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let author = match person(&state, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    match issues::open(&state.db, &r.id, &author, &body.title, &body.body) {
        Ok(i) => (StatusCode::CREATED, Json(issue_view(i))).into_response(),
        Err(e) => bad_request(e),
    }
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    pub state: Option<String>,
    pub sort: Option<String>,
    pub label: Option<String>,
    pub author: Option<String>,
    pub q: Option<String>,
    pub limit: Option<i64>,
    pub before: Option<i32>,
}

/// GET …/issues — the index.
pub async fn list(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    let (_, r, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let f = Filter {
        state: q.state,
        label: q.label,
        author: q.author,
        q: q.q,
        limit: q.limit.unwrap_or(30),
        before: q.before,
        sort: q.sort,
    };
    match issues::list(&state.db, &r.id, &f) {
        Ok((rows, counts)) => {
            // The cursor is the last number on the page, and `None` when
            // the page was not full — a client that got a cursor for a
            // short page would make one more request to learn nothing.
            let next = (rows.len() as i64 >= f.limit)
                .then(|| rows.last().map(|i| i.number))
                .flatten();
            Json(ListView {
                issues: rows.into_iter().map(issue_view).collect(),
                counts: counts_view(counts),
                next,
            })
            .into_response()
        }
        Err(e) => bad_request(e),
    }
}

/// GET …/issues/:number
pub async fn get(
    State(state): State<SharedState>,
    Path((org, repo, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
) -> Response {
    let (_, r, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match issues::get(&state.db, &r.id, number) {
        Ok(Some(i)) => Json(issue_view(i)).into_response(),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "no such issue"),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct PatchIssue {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

/// PATCH …/issues/:number — edit, close or reopen.
///
/// Two different authorities in one handler, and they are not the same
/// question. Editing the text is the author's or a writer's. Closing is
/// a writer's, **or** the author's on their own issue — GitHub's rule,
/// and the right one: somebody who filed a bug and worked out it was
/// their own mistake should be able to say so without waiting for a
/// maintainer to spend attention agreeing.
pub async fn patch(
    State(state): State<SharedState>,
    Path((org, repo, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    Json(body): Json<PatchIssue>,
) -> Response {
    let (_, r, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let existing = match issues::get(&state.db, &r.id, number) {
        Ok(Some(i)) => i,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "no such issue"),
        Err(e) => return internal(e),
    };
    let caller = match caller_person(&state, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    // Asked once, and asked as a question about this repository rather
    // than about the org: a per-repo grant is exactly how an outside
    // contributor gets triage rights on one project.
    let writer = app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoWrite).is_ok();
    let author = match (&caller, &existing.author_id) {
        (Some(c), Some(a)) => c == a,
        _ => false,
    };
    if !writer && !author {
        return json_error(
            StatusCode::FORBIDDEN,
            "only the issue's author or somebody with write access may change it",
        );
    }
    if body.title.is_some() || body.body.is_some() {
        if let Err(e) = issues::edit(
            &state.db,
            &r.id,
            number,
            body.title.as_deref(),
            body.body.as_deref(),
        ) {
            return bad_request(e);
        }
    }
    if let Some(s) = body.state.as_deref() {
        let now = stratum_control::ids::now_ms();
        if let Err(e) = issues::set_state(&state.db, &r.id, number, s, now) {
            return bad_request(e);
        }
    }
    match issues::get(&state.db, &r.id, number) {
        Ok(Some(i)) => Json(issue_view(i)).into_response(),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "no such issue"),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct NewComment {
    pub body: String,
}

/// POST …/issues/:number/comments — `RepoRead`, same as filing.
pub async fn add_comment(
    State(state): State<SharedState>,
    Path((org, repo, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    Json(body): Json<NewComment>,
) -> Response {
    let (_, r, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let author = match person(&state, &headers) {
        Ok(u) => u,
        Err(e) => return e,
    };
    let issue = match issues::get(&state.db, &r.id, number) {
        Ok(Some(i)) => i,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "no such issue"),
        Err(e) => return internal(e),
    };
    match issues::comment(&state.db, &issue.id, &author, &body.body) {
        Ok(c) => (StatusCode::CREATED, Json(comment_view(c))).into_response(),
        Err(e) => bad_request(e),
    }
}

/// GET …/issues/:number/comments — in `seq` order, which is insertion
/// order. See `stratum_control::issues` for why that is not the id.
pub async fn comments(
    State(state): State<SharedState>,
    Path((org, repo, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
) -> Response {
    let (_, r, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let issue = match issues::get(&state.db, &r.id, number) {
        Ok(Some(i)) => i,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "no such issue"),
        Err(e) => return internal(e),
    };
    match issues::comments(&state.db, &issue.id) {
        Ok(cs) => Json(serde_json::json!({
            "comments": cs.into_iter().map(comment_view).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct SetLabels {
    pub labels: Vec<String>,
}

/// PUT …/issues/:number/labels — write access. Triage is a maintainer's.
///
/// Resolved with `RepoRead` and then checked for write, rather than
/// asked for `RepoWrite` outright. Both refuse the same people; they
/// differ in what they say, and on a **public** repository the
/// difference matters.
///
/// `rest_repo_auth` masks an insufficient caller as 404, which is right
/// and deliberate: a status code must never become an existence oracle
/// for a private repository. But a reporter on a public tracker can
/// already see the repository and the issue, so answering "no such
/// thing" tells them something false about a thing in front of them,
/// and hides a refusal they could have understood. It also made Issues
/// disagree with itself — `PATCH …/issues/:number` says 403 for exactly
/// this caller.
///
/// Nothing is leaked by the 403, because reaching it requires passing
/// `RepoRead` first: on a private repository an outsider is still
/// masked, one line above, before write is ever considered.
pub async fn set_labels(
    State(state): State<SharedState>,
    Path((org, repo, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    Json(body): Json<SetLabels>,
) -> Response {
    let (_, r, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoWrite).is_err() {
        return json_error(
            StatusCode::FORBIDDEN,
            "labelling needs write access — labels are a maintainer's triage of their own backlog",
        );
    }
    match issues::set_labels(&state.db, &r.id, number, &body.labels) {
        Ok(i) => Json(issue_view(i)).into_response(),
        Err(e) => bad_request(e),
    }
}

/// GET …/labels — readable by anyone who may read the repository, since
/// the index renders a pill per label and a filter menu of them.
pub async fn labels(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, r, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match issues::labels(&state.db, &r.id) {
        Ok(ls) => Json(serde_json::json!({
            "labels": ls.into_iter().map(label_view).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

/// One milestone on the wire.
fn milestone_view(m: issues::Milestone) -> serde_json::Value {
    serde_json::json!({
        "number": m.number,
        "title": m.title,
        "description": m.description,
        "state": m.state,
        "due_on": m.due_on,
        // Both counts, never a single "progress" fraction: a milestone
        // with nothing in it and one whose every issue is closed both
        // compute to the same number, and they are opposite situations.
        "open_issues": m.open_issues,
        "closed_issues": m.closed_issues,
    })
}

/// `GET /v1/orgs/:org/repos/:repo/milestones` — every milestone, by the
/// number it came with.
///
/// New because the import had been writing milestones into a table with
/// no reader since it was added: no list function, no route, no view,
/// and no link from the issues that belong to them. The import status
/// page reported "Milestones: done" over data nobody could see.
pub async fn milestones(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, r, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match issues::milestones(&state.db, &r.id) {
        Ok(ms) => Json(serde_json::json!({
            "milestones": ms.into_iter().map(milestone_view).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct NewLabel {
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub description: String,
}

/// POST …/labels — `RepoWrite`.
pub async fn create_label(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<NewLabel>,
) -> Response {
    let (_, r, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoWrite) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match issues::create_label(&state.db, &r.id, &body.name, &body.color, &body.description) {
        Ok(l) => (StatusCode::CREATED, Json(label_view(l))).into_response(),
        Err(e) => bad_request(e),
    }
}

/// DELETE …/labels/:label — `RepoWrite`. Removes it from every issue
/// carrying it, by the cascade on `issue_labels`.
pub async fn delete_label(
    State(state): State<SharedState>,
    Path((org, repo, label)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, r, _) = match app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoWrite) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match issues::delete_label(&state.db, &r.id, &label) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => bad_request(e),
    }
}
