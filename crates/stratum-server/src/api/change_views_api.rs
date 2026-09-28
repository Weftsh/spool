//! Review ergonomics: per-file "viewed" marks, and author associations.
//!
//! Two small read surfaces that make a large change reviewable.
//!
//! **Viewed marks** are the reviewer's own bookkeeping, and the whole
//! value is in the invalidation. A mark is stored against the patchset it
//! was made at; when a new patchset arrives, a path whose content moved
//! between the two is *not* viewed any more. A flag that survived a
//! revision would be worse than no flag: it would tell a reviewer they
//! had read code they had never seen. A path the new patchset did not
//! touch keeps its mark, because the reviewer really has read what is
//! there.
//!
//! Nobody can reach anybody else's marks. The user is taken from the
//! authenticated principal and never from the request, so there is no
//! parameter to tamper with — a shape rather than a rule.
//!
//! **Associations** answer "how should I weigh this opinion" on a comment
//! and on the change header. Derived on every read from
//! [`stratum_control::association`], which reuses the one effective-role
//! resolver; nothing new is stored.

use crate::api::{internal, json_error, reads};
use crate::app::SharedState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use stratum_control::auth::{Principal, Scope};
use stratum_control::{association, changes, fileviews};
use stratum_engine::treediff;

/// The change, the acting person, and the repo — everything all three
/// handlers need, with the same masking every other change surface uses.
///
/// A viewed mark is a note one *person* wrote to themselves, so a service
/// token is refused rather than quietly given an empty set: an org-level
/// token has no reviewer whose marks these could be, and pretending
/// otherwise would let two machines share one set of ticks.
///
/// The person is whoever `rest_repo_auth` says is reading: somebody
/// holding a role on this repository that allows `repo:read` — a signed-
/// out caller never gets this far. That includes a viewer who may not
/// push, back to tick off the files of a change they opened from a fork.
fn person_and_change(
    state: &SharedState,
    headers: &HeaderMap,
    org: &str,
    repo: &str,
    key: &str,
) -> Result<(stratum_control::Repo, String, changes::Change), Response> {
    let (_, repo_row, principal) =
        crate::app::rest_repo_auth(state, headers, org, repo, Scope::RepoRead)?;
    let user = acting_person(&principal)?;
    let change = change_or_404(state, &repo_row.id, key)?;
    Ok((repo_row, user, change))
}

fn acting_person(principal: &Principal) -> Result<String, Response> {
    match &principal.user_id {
        Some(user) => Ok(user.clone()),
        None => Err(json_error(
            StatusCode::FORBIDDEN,
            "viewed state belongs to a person, not a service token",
        )),
    }
}

/// Existence masking: an unknown or invalid change key is absent, like
/// anything else nobody may see.
fn change_or_404(
    state: &SharedState,
    repo_id: &str,
    key: &str,
) -> Result<changes::Change, Response> {
    match changes::by_key(&state.db, repo_id, key) {
        Ok(Some(c)) => Ok(c),
        Ok(None) => Err(json_error(
            StatusCode::NOT_FOUND,
            format!("no change {key:?}"),
        )),
        Err(e) => Err(internal(e)),
    }
}

/// GET /changes/:change/views — the caller's own viewed paths, as they
/// apply to the latest patchset.
///
/// Marks made at older patchsets are carried forward only for paths the
/// newer patchsets left alone. That comparison is made here, at read
/// time, against the stored commits — so the answer is derived from what
/// the patchsets actually contain rather than from a flag somebody
/// remembered to clear.
///
/// It also carries `since`: the newest patchset this person has marked
/// anything at, which is the patchset their last pass was against and so
/// the left-hand side of "what changed since I last looked". Additive and
/// nullable, in the shape `required_checks` established — an older client
/// that ignores the field is unaffected, and a `null` says "we cannot
/// tell you" rather than naming a patchset that is not the one they read.
pub async fn get(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (repo_row, user, change) = match person_and_change(&state, &headers, &org, &repo, &key) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let patchsets = match changes::patchsets(&state.db, &change.id) {
        Ok(p) => p,
        Err(e) => return internal(e),
    };
    let Some(latest) = patchsets.last() else {
        return Json(serde_json::json!({ "patchset": null, "since": null, "viewed": [] }))
            .into_response();
    };
    let rows = match fileviews::list(&state.db, &change.id, &user) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    // The patchset this reviewer's last pass was against, so the client
    // does not have to derive it — and, more to the point, so that two
    // clients cannot derive it two different ways. `null` when they have
    // marked nothing; a client with no answer here must ask for no range
    // rather than invent one.
    //
    // Taken before the staleness pass below, because the two questions
    // are different: `viewed` is "which files are still read", which a
    // store we cannot reach answers conservatively as none, while
    // `since` is "when did I last look", which is a fact about marks
    // made and is not affected by whether the store answered. Silence
    // here would mean "you have never looked", and that is the one thing
    // it must not say to somebody who has.
    let since = fileviews::last_marked(&rows);

    // Marks at the latest patchset stand as they are. Older ones have to
    // be checked against what changed since.
    let mut viewed: BTreeSet<String> = BTreeSet::new();
    let mut stale: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for row in rows {
        if i64::from(row.patchset) == latest.number {
            viewed.insert(row.path);
        } else {
            stale
                .entry(i64::from(row.patchset))
                .or_default()
                .push(row.path);
        }
    }
    if !stale.is_empty() {
        let by_number: BTreeMap<i64, String> = patchsets
            .iter()
            .map(|p| (p.number, p.commit_oid.clone()))
            .collect();
        // Only patchsets still on record can be compared. A mark against
        // one that is gone stays unviewed — the fail-safe direction.
        let pairs: Vec<(String, Vec<String>)> = stale
            .into_iter()
            .filter_map(|(n, paths)| by_number.get(&n).map(|oid| (oid.clone(), paths)))
            .collect();
        let newest = latest.commit_oid.clone();
        let prefix = repo_row.prefix().as_str().to_string();
        let out = reads::with_reader(&state, prefix, move |reader| {
            let mut kept: Vec<String> = Vec::new();
            for (old_commit, paths) in pairs {
                let touched: BTreeSet<String> =
                    treediff::diff_commits(reader, Some(&old_commit), &newest)?
                        .into_iter()
                        .map(|c| c.path)
                        .collect();
                kept.extend(paths.into_iter().filter(|p| !touched.contains(p)));
            }
            Ok(kept)
        })
        .await;
        // A store the reader could not answer from means "unviewed", not
        // a 500: the reviewer sees an unticked box and reads the file,
        // which is the safe answer to give when we cannot tell.
        viewed.extend(out.unwrap_or_default());
    }

    Json(serde_json::json!({
        "patchset": latest.number,
        "since": since,
        "viewed": viewed.into_iter().collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct SetView {
    /// A path the patchset touches, exactly as the diff names it.
    pub path: String,
    /// Tick or untick.
    pub viewed: bool,
}

/// PUT /changes/:change/views — tick or untick one path for the caller,
/// against the change's latest patchset.
pub async fn put(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<SetView>,
) -> Response {
    let (_, user, change) = match person_and_change(&state, &headers, &org, &repo, &key) {
        Ok(x) => x,
        Err(r) => return r,
    };
    // The same path rule the review surfaces use — hostile bytes are
    // refused at the door rather than stored and rendered later.
    if !crate::review::owners::valid_repo_path(&body.path) {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("invalid path {:?}", body.path),
        );
    }
    let latest = match changes::latest_patchset(&state.db, &change.id) {
        Ok(Some(p)) => p,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "change has no patchsets"),
        Err(e) => return internal(e),
    };
    // Patchset numbers are small by construction; a value that could not
    // fit is pinned rather than branched on, because there is no useful
    // second behaviour to give it.
    let number = i32::try_from(latest.number).unwrap_or(i32::MAX);
    let out = if body.viewed {
        fileviews::mark(&state.db, &change.id, &user, &body.path, number).map(|_| ())
    } else {
        fileviews::unmark(&state.db, &change.id, &user, &body.path).map(|_| ())
    };
    match out {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

/// GET /changes/:change/associations — how to weigh the voices in this
/// thread: the change's author, and every commenter, by principal.
///
/// Keyed by the same `author_principal` string the comments endpoint
/// returns, so the client joins on what it already has. Service
/// principals are absent rather than labelled: a build robot has no
/// standing in the org, and inventing one for it would be a lie in a
/// place people trust.
pub async fn associations(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org_row, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let comments = match changes::comments_for(&state.db, &change.id) {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let mut users: BTreeSet<String> = comments.iter().filter_map(user_of).collect();
    if let Some(author) = change.created_by.clone() {
        users.insert(author);
    }
    let mut by_user: BTreeMap<String, &'static str> = BTreeMap::new();
    for user in users {
        match association::of(&state.db, &org_row.id, &repo_row.id, &user) {
            Ok(a) => {
                by_user.insert(user, a.as_str());
            }
            Err(e) => return internal(e),
        }
    }
    let authors: serde_json::Map<String, serde_json::Value> = by_user
        .iter()
        .map(|(u, a)| (format!("user:{u}"), serde_json::json!(a)))
        .collect();
    Json(serde_json::json!({
        "author": change
            .created_by
            .as_deref()
            .and_then(|u| by_user.get(u))
            .map(|a| serde_json::json!(a)),
        // Which principal that is, so a reader can tell the author's own
        // comments apart from everybody else's in a long thread. The
        // author disagreeing with a reviewer and a third party doing the
        // same are different sentences, exactly as association is.
        "author_principal": change.created_by.as_deref().map(|u| format!("user:{u}")),
        "authors": authors,
    }))
    .into_response()
}

/// The user behind a comment, when there is one. `user:<id>` is what
/// [`stratum_control::auth::Principal::audit_id`] writes for a person;
/// anything else is a service principal and has no association.
fn user_of(c: &changes::Comment) -> Option<String> {
    c.author_principal
        .strip_prefix("user:")
        .map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a person's principal yields a user; a token's does not, and
    /// neither does a string that merely mentions one.
    #[test]
    fn only_a_person_has_an_association() {
        let comment = |principal: &str| changes::Comment {
            id: "01c".into(),
            change_id: "01ch".into(),
            patchset_number: 1,
            author_principal: principal.into(),
            body: "b".into(),
            // Threading, anchors and resolution (migration 0050) say
            // nothing about which principals name a person, which is
            // all this test is about.
            ..changes::Comment::default()
        };
        assert_eq!(
            user_of(&comment("user:01abc")),
            Some("01abc".to_string()),
            "a person's principal names the user"
        );
        assert_eq!(user_of(&comment("token:01ci")), None);
        assert_eq!(user_of(&comment("agent user:01abc")), None);
        assert_eq!(user_of(&comment("")), None);
    }
}
