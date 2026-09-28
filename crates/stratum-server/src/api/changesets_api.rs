//! Changesets API: compose one review and one landing over changes in
//! several repositories of an organization, and read it back with every
//! member at its current patchset.
//!
//! Authority is per member, through the same `rest_repo_auth` every
//! repository surface uses. Reading a changeset needs read on **every**
//! member repository — a changeset is one review, and a view with a
//! member missing is a view of a different review — and anything less
//! is masked as "no changeset", the way a private repository is masked.
//! Composing one needs write on every member, because a changeset is a
//! landing plan rather than a proposal: a contributor without write
//! opens the changes and asks a maintainer to compose them.

use crate::api::changes_api::{change_json, compute_verdict, source_name};
use crate::api::{internal, json_error, owners_api, reads};
use crate::app::SharedState;
use crate::review;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::{Principal, Scope};
use stratum_control::changes::{self, Change, Patchset};
use stratum_control::changesets::{
    self, Candidate, Changeset, Error, Step, StepState, MAX_MEMBERS,
};
use stratum_control::Repo;
use stratum_engine::ancestry::{self, Ancestry};
use stratum_engine::read::LayoutReader;
use stratum_store::{LatencyModel, Manifest, ObjectStore};

/// `repo/change` — how a member is named on the wire, in both directions.
#[derive(Deserialize, Clone)]
pub struct MemberRef {
    pub repo: String,
    pub change: String,
}

/// `from` lands first.
#[derive(Deserialize, Clone)]
pub struct EdgeRef {
    pub from: MemberRef,
    pub to: MemberRef,
}

#[derive(Deserialize)]
pub struct CreateChangeset {
    /// Client-supplied, like a change key; unique within the org.
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub members: Vec<MemberRef>,
    #[serde(default)]
    pub edges: Vec<EdgeRef>,
}

#[derive(Deserialize)]
pub struct SetEdges {
    pub edges: Vec<EdgeRef>,
}

/// A member with everything the view and the checks need about it, and
/// the principal whose authority on its repository admitted it.
pub(crate) struct Loaded {
    pub(crate) repo: Repo,
    pub(crate) change: Change,
    pub(crate) latest: Option<Patchset>,
    pub(crate) principal: Principal,
}

impl Loaded {
    pub(crate) fn label(&self) -> String {
        format!("{}/{}", self.repo.name, self.change.change_key)
    }
    fn reference(&self) -> serde_json::Value {
        serde_json::json!({ "repo": self.repo.name, "change": self.change.change_key })
    }
}

fn label_of(m: &MemberRef) -> String {
    format!("{}/{}", m.repo, m.change)
}

/// Resolve one named member with `need` on its repository, masked like
/// every other repository lookup.
fn resolve(
    state: &SharedState,
    headers: &HeaderMap,
    org: &str,
    m: &MemberRef,
    need: Scope,
) -> Result<Loaded, Response> {
    let (_, repo, principal) = crate::app::rest_repo_auth(state, headers, org, &m.repo, need)?;
    let change = match changes::by_key(&state.db, &repo.id, &m.change) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Err(json_error(
                StatusCode::NOT_FOUND,
                format!("no change {}", label_of(m)),
            ))
        }
        Err(e) => return Err(internal(e)),
    };
    let latest = changes::latest_patchset(&state.db, &change.id).map_err(internal)?;
    Ok(Loaded {
        repo,
        change,
        latest,
        principal,
    })
}

/// The changeset named by `key`, with every member loaded under `need`.
///
/// One answer for "no such key" and "a member you may not see": the
/// existence of a changeset over a private repository is a fact about
/// that repository, and is masked the same way.
pub(crate) fn load(
    state: &SharedState,
    headers: &HeaderMap,
    org_name: &str,
    org_id: &str,
    key: &str,
    need: Scope,
) -> Result<(Changeset, Vec<Loaded>), Response> {
    // Before the key is looked up: an anonymous caller is told to sign
    // in whether or not the key exists.
    crate::authx::require_authenticated(&state.db, headers)?;
    let masked = || json_error(StatusCode::NOT_FOUND, format!("no changeset {key:?}"));
    let cs = match changesets::get(&state.db, org_id, key) {
        Ok(Some(cs)) => cs,
        Ok(None) => return Err(masked()),
        Err(e) => return Err(internal(e.to_string())),
    };
    // A member the caller may not reach makes the whole changeset not
    // exist for them — the same masking each repository applies alone.
    // Anything else a member's authority check says is about *this*
    // caller and must reach them.
    let members = load_members(state, headers, org_name, &cs, need).map_err(|r| {
        if matches!(r.status(), StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED) {
            masked()
        } else {
            r
        }
    })?;
    Ok((cs, members))
}

fn load_members(
    state: &SharedState,
    headers: &HeaderMap,
    org_name: &str,
    cs: &Changeset,
    need: Scope,
) -> Result<Vec<Loaded>, Response> {
    let mut out = Vec::new();
    for m in changesets::members(&state.db, &cs.id).map_err(|e| internal(e.to_string()))? {
        let change = match changes::by_id(&state.db, &m.change_id) {
            Ok(Some(c)) => c,
            Ok(None) => continue,
            Err(e) => return Err(internal(e)),
        };
        let repo = match stratum_control::registry::repo_by_id_any(&state.db, &change.repo_id) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(e) => return Err(internal(e)),
        };
        out.push(resolve(
            state,
            headers,
            org_name,
            &MemberRef {
                repo: repo.name,
                change: change.change_key,
            },
            need,
        )?);
    }
    Ok(out)
}

/// The changeset's edges, and its members in the order the lander walks
/// them: dependencies first, ties by the order they joined.
fn ordered<'a>(
    state: &SharedState,
    cs: &Changeset,
    members: &'a [Loaded],
) -> Result<(Vec<changesets::Edge>, Vec<&'a Loaded>), Response> {
    let edges = changesets::edges(&state.db, &cs.id).map_err(|e| internal(e.to_string()))?;
    let by_id: HashMap<&str, &Loaded> = members.iter().map(|m| (m.change.id.as_str(), m)).collect();
    let stored: Vec<changesets::Member> = members
        .iter()
        .enumerate()
        .map(|(i, m)| changesets::Member {
            change_id: m.change.id.clone(),
            position: i as i64 + 1,
        })
        .collect();
    let order = changesets::landing_order(&stored, &edges)
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).copied())
        .collect();
    Ok((edges, order))
}

/// The changeset's own check gate, at the composition it is at *now*.
///
/// "Now" and not "when the runs started": a member pushed since the last
/// composed build has moved the changeset to a combination nothing has
/// checked, and the honest answer there is `Ready` — the trigger has
/// already superseded the stale runs and started fresh ones, so the
/// waits that appear a moment later are waits for the build of the thing
/// being landed. Reading the old composition's rows would instead report
/// a verdict about a combination the reader is not looking at.
///
/// An un-composable changeset — a member with no patchset, a member
/// whose repository is gone — is `Ready` here, and that is not a hole:
/// such a changeset cannot land for reasons the member loop reaches
/// first, and refusing it twice would only replace a sentence naming the
/// member with one naming nothing.
fn composed_gate(state: &SharedState, cs: &Changeset) -> Result<changes::LandGate, Response> {
    let Some((composition, _)) = crate::workflow::trigger::compose(state, cs) else {
        return Ok(changes::LandGate::Ready);
    };
    changesets::composed_gate(&state.db, &cs.id, &composition).map_err(|e| internal(e.to_string()))
}

pub(crate) fn changeset_json(
    state: &SharedState,
    cs: &Changeset,
    members: &[Loaded],
) -> Result<serde_json::Value, Response> {
    let (edges, order) = ordered(state, cs, members)?;
    let by_id: HashMap<&str, &Loaded> = members.iter().map(|m| (m.change.id.as_str(), m)).collect();
    let named = |id: &str| by_id.get(id).map(|m| m.reference());
    // The changeset this one reverts, and the ones made to revert this
    // one — by key, since ids are nobody's to type. A revert whose
    // original is somehow gone is still a revert; it just names nothing.
    let reverts = match &cs.reverts {
        Some(id) => changesets::by_id(&state.db, id)
            .map_err(|e| internal(e.to_string()))?
            .map(|c| c.key),
        None => None,
    };
    let reverted_by: Vec<String> = changesets::reverted_by(&state.db, &cs.id)
        .map_err(|e| internal(e.to_string()))?
        .into_iter()
        .map(|c| c.key)
        .collect();
    // What the changeset is right now, and what its composed runs have
    // said about exactly that. Both together, or neither: a `checks`
    // list without the composition it belongs to is a set of verdicts a
    // reader cannot tell are current.
    let (composition, checks) = composed_checks(state, cs, members)?;
    // Whether this caller may land, revert, abandon or edit the set:
    // write on *every* member, which is what each of those routes
    // demands through `load(.., Scope::RepoWrite)`. Read off the
    // principal each member was loaded under — already refined against
    // that repository's grants by `rest_repo_auth`, so this is the same
    // answer `repos::viewer_write` gives for the repository alone, not a
    // second opinion. The server's own question, for the reason the
    // repository row asks it: the dashboard used to offer a viewer
    // "Revert…" and "Land all members", and answered each press with the
    // masked `no changeset` the write routes give somebody who may not
    // write — about a changeset they were looking at.
    let viewer_write = members
        .iter()
        .all(|m| m.principal.allows(Scope::RepoWrite, Some(&m.repo.id)));
    Ok(serde_json::json!({
        "key": cs.key,
        "title": cs.title,
        "body": cs.body,
        "state": cs.state,
        "viewer_write": viewer_write,
        "created_at": cs.created_at,
        "updated_at": cs.updated_at,
        "reverts": reverts,
        "reverted_by": reverted_by,
        "members": members.iter().map(|m| serde_json::json!({
            "repo": m.repo.name,
            "change": change_json(&m.change, m.latest.as_ref(), source_name(state, &m.change)),
        })).collect::<Vec<_>>(),
        "edges": edges.iter().map(|e| serde_json::json!({
            "from": named(&e.from_change_id),
            "to": named(&e.to_change_id),
        })).collect::<Vec<_>>(),
        // Dependencies first: the sequence the lander walks.
        "order": order.iter().map(|m| m.reference()).collect::<Vec<_>>(),
        "composition": composition,
        "checks": checks,
        "landing": landing_json(state, cs, &named)?,
    }))
}

/// The composition the changeset is at, and the composed verdicts for
/// it, ordered by repository then check name.
///
/// The repository is named rather than identified, because the reader of
/// a failing composed check wants to know which repository's job it was
/// — the gate's sentence says the same thing and the two must agree. A
/// row whose repository is not among the members (a repository removed
/// from the changeset since the run) keeps its id as its name rather
/// than being dropped: an unexplained verdict is better than a missing
/// one, and the gate is counting it either way.
///
/// `(null, [])` for a changeset that cannot be composed, which is the
/// same answer [`composed_gate`] gives and for the same reason.
fn composed_checks(
    state: &SharedState,
    cs: &Changeset,
    members: &[Loaded],
) -> Result<(serde_json::Value, Vec<serde_json::Value>), Response> {
    let Some((composition, _)) = crate::workflow::trigger::compose(state, cs) else {
        return Ok((serde_json::Value::Null, Vec::new()));
    };
    let rows = stratum_control::changeset_checks::for_composition(&state.db, &cs.id, &composition)
        .map_err(internal)?;
    let name_of = |repo_id: &str| {
        members
            .iter()
            .find(|m| m.repo.id == repo_id)
            .map_or_else(|| repo_id.to_string(), |m| m.repo.name.clone())
    };
    let mut out: Vec<(String, String, serde_json::Value)> = rows
        .iter()
        .map(|c| {
            let repo = name_of(&c.repo_id);
            let json = serde_json::json!({
                "repo": repo,
                "name": c.name,
                "state": c.state,
                "detail_url": c.detail_url,
                "run": c.run_id,
            });
            (repo, c.name.clone(), json)
        })
        .collect();
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    Ok((
        serde_json::Value::String(composition),
        out.into_iter().map(|(_, _, j)| j).collect(),
    ))
}

/// The most recent landing attempt, step by step — how a reader of a
/// `failed` changeset learns which member did not land and why, and
/// which members were landed and then put back. `null` until the first
/// `POST …/land` is accepted.
fn landing_json(
    state: &SharedState,
    cs: &Changeset,
    named: &dyn Fn(&str) -> Option<serde_json::Value>,
) -> Result<serde_json::Value, Response> {
    let Some(l) =
        changesets::latest_landing(&state.db, &cs.id).map_err(|e| internal(e.to_string()))?
    else {
        return Ok(serde_json::Value::Null);
    };
    Ok(serde_json::json!({
        "id": l.id,
        "attempt": l.attempt,
        "started_at": l.started_at,
        "finished_at": l.finished_at,
        "outcome": l.outcome,
        "members": l.progress.iter().map(|s| {
            // Members outlive the landing in `changeset_members`, so the
            // name comes from the live change; the plan's own label is
            // the fallback for a change deleted since.
            let (repo, change) = s.label.split_once('/').unwrap_or((s.label.as_str(), ""));
            let mut j = named(&s.change_id).unwrap_or_else(|| serde_json::json!({
                "repo": repo,
                "change": change,
            }));
            j["ref"] = serde_json::json!(s.ref_name);
            j["old"] = serde_json::json!(s.old);
            j["new"] = serde_json::json!(s.new);
            j["state"] = serde_json::json!(s.state);
            j["note"] = serde_json::json!(s.note);
            j
        }).collect::<Vec<_>>(),
    }))
}

/// GET /v1/orgs/:org/changesets/:key/verdict — landability of the whole
/// review, now, with every member's own answer beside it.
///
/// Each member's verdict is exactly the one `GET …/changes/:change/verdict`
/// gives and its gate exactly the one the per-change land button
/// consults; the changeset only composes them (`review::changeset`).
/// Members are in landing order, and the changeset's explanation is the
/// first member's that is not landable, named — so the reader of one
/// review over four repositories is told which repository to go to.
pub async fn verdict(
    State(state): State<SharedState>,
    Path((org_name, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (cs, members) = match load(&state, &headers, &org_name, &org.id, &key, Scope::RepoRead) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let (_, order) = match ordered(&state, &cs, &members) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let mut verdicts = Vec::with_capacity(order.len());
    let mut rendered = Vec::with_capacity(order.len());
    for m in &order {
        let Some(latest) = m.latest.as_ref() else {
            return json_error(
                StatusCode::NOT_FOUND,
                format!("change {} has no patchsets", m.label()),
            );
        };
        let v = match compute_verdict(&state, &m.change, &m.repo, latest).await {
            Ok(v) => v,
            Err(e) => return reads::err_to_response(e),
        };
        let gate = match changes::land_gate(&state.db, &m.change.id) {
            Ok(g) => g,
            Err(e) => return internal(e),
        };
        let approvals = match changes::approvals_for(&state.db, &latest.id) {
            Ok(a) => a,
            Err(e) => return internal(e),
        };
        rendered.push(serde_json::json!({
            "repo": m.repo.name,
            "change": m.change.change_key,
            "state": m.change.state,
            "patchset": latest.number,
            "commit": latest.commit_oid,
            "verdict": owners_api::verdict_json(&v),
            "approvals": approvals.iter().map(|a| serde_json::json!({
                "email": a.email,
                "name": a.name,
                "created_at": a.created_at,
            })).collect::<Vec<_>>(),
        }));
        verdicts.push(review::changeset::MemberVerdict {
            label: m.label(),
            state: m.change.state.clone(),
            verdict: v,
            gate,
        });
    }
    let gate = match composed_gate(&state, &cs) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let composed = review::changeset::compose(&cs.state, &verdicts, &gate);
    for (json, j) in rendered.iter_mut().zip(&composed.members) {
        json["landable"] = serde_json::json!(j.landable);
        json["explanation"] = serde_json::json!(j.explanation);
        json["gate"] = serde_json::json!(j.gate.as_str());
        json["waiting_on"] = serde_json::json!(j.waiting_on);
        json["reason"] = serde_json::json!(j.reason);
    }
    Json(serde_json::json!({
        "changeset": cs.key,
        "state": cs.state,
        "landable": composed.landable,
        "gate": composed.gate.as_str(),
        "explanation": composed.explanation,
        "waiting_on": composed.waiting_on,
        "members": rendered,
    }))
    .into_response()
}

pub(crate) fn refusal(e: Error) -> Response {
    match e {
        Error::Invalid(s) => json_error(StatusCode::BAD_REQUEST, s),
        Error::Conflict(s) => json_error(StatusCode::CONFLICT, s),
        Error::Db(s) => internal(s),
    }
}

/// The `repo/change` names of every edge endpoint, in wire order — the
/// words the module refuses an edge in.
fn edge_labels(edges: &[EdgeRef]) -> Vec<String> {
    edges
        .iter()
        .flat_map(|e| [label_of(&e.from), label_of(&e.to)])
        .collect()
}

/// Edges on the wire name members by `repo/change`; the module wants
/// change ids with those names attached. An endpoint that names no
/// member keeps its label and gets an id nothing can match, so the
/// module's own "is not a member" refusal names it.
fn edge_candidates<'a>(
    edges: &[EdgeRef],
    members: &'a [Loaded],
    labels: &'a [String],
) -> Vec<(Candidate<'a>, Candidate<'a>)> {
    let end = |m: &MemberRef, label: &'a str| -> Candidate<'a> {
        let id = members
            .iter()
            .find(|l| l.repo.name == m.repo && l.change.change_key == m.change)
            .map_or("", |l| l.change.id.as_str());
        Candidate {
            change_id: id,
            label,
        }
    };
    edges
        .iter()
        .zip(labels.chunks(2))
        .map(|(e, l)| (end(&e.from, &l[0]), end(&e.to, &l[1])))
        .collect()
}

/// POST /v1/orgs/:org/changesets
pub async fn create(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateChangeset>,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if body.members.is_empty() || body.members.len() > MAX_MEMBERS {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("a changeset has between 1 and {MAX_MEMBERS} members"),
        );
    }
    let mut members = Vec::with_capacity(body.members.len());
    for m in &body.members {
        // Write on every member: the composer is the person who will
        // land it, and the 402 a lapsed org answers here is the same
        // one its pushes get.
        match resolve(&state, &headers, &org_name, m, Scope::RepoWrite) {
            Ok(l) => members.push(l),
            Err(r) => return r,
        }
    }
    let principal = members.last().map(|m| m.principal.clone());
    let labels: Vec<String> = members.iter().map(Loaded::label).collect();
    let cands: Vec<Candidate> = members
        .iter()
        .zip(&labels)
        .map(|(m, label)| Candidate {
            change_id: &m.change.id,
            label,
        })
        .collect();
    let labels_for_edges = edge_labels(&body.edges);
    let edges = edge_candidates(&body.edges, &members, &labels_for_edges);
    let actx = AuditCtx::of(&org.id, principal.as_ref());
    let created_by = principal.as_ref().and_then(|p| p.user_id.clone());
    let cs = match changesets::create(
        &state.db,
        &org.id,
        &body.key,
        &body.title,
        &body.body,
        created_by.as_deref(),
        &cands,
        &edges,
        None,
        &actx,
    ) {
        Ok(cs) => cs,
        Err(e) => return refusal(e),
    };
    // Composing the changeset is what makes the combination exist, so
    // it is what starts the composed runs — before the body is
    // answered, so a client that reads the changeset back immediately
    // sees the checks it just caused rather than an empty list it has
    // to poll for.
    crate::workflow::trigger::on_changeset(&state, &cs).await;
    // ...and it is what asks people for the review. Composing is the
    // moment the combination exists, so it is the moment its reviewers
    // become reviewers — of a thing that is not any one of the changes
    // they may already have been told about. Without this a changeset
    // over four repositories was composed and nobody was told at all,
    // which is the single-change failure this product exists to fix,
    // hiding behind the per-repository path still working.
    //
    // The actor is the composer, so they are not mailed about their own
    // request. A service principal composes as nobody, and then nobody
    // is left off — which is right: a token has no mailbox to spare.
    crate::workers::changeset_notifier::enqueue(
        &state,
        &cs,
        crate::workers::changeset_notifier::Event::Composed,
        created_by.as_deref().unwrap_or_default(),
    );
    match changeset_json(&state, &cs, &members) {
        Ok(v) => (StatusCode::CREATED, Json(v)).into_response(),
        Err(r) => r,
    }
}

/// GET /v1/orgs/:org/changesets?state=
pub async fn list(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = crate::authx::require_authenticated(&state.db, &headers) {
        return r;
    }
    let state_filter = params.get("state").map(String::as_str);
    if let Some(s) = state_filter {
        if !matches!(s, "open" | "landing" | "landed" | "abandoned" | "failed") {
            return json_error(StatusCode::BAD_REQUEST, format!("unknown state {s:?}"));
        }
    }
    let limit: i64 = params
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(50);
    let rows = match changesets::list(&state.db, &org.id, state_filter, limit) {
        Ok(r) => r,
        Err(e) => return internal(e.to_string()),
    };
    let mut out = Vec::with_capacity(rows.len());
    for cs in &rows {
        // A changeset with a member the caller may not see is not
        // theirs to list, the same as it is not theirs to read.
        let Ok(members) = load_members(&state, &headers, &org_name, cs, Scope::RepoRead) else {
            continue;
        };
        match changeset_json(&state, cs, &members) {
            Ok(v) => out.push(v),
            Err(r) => return r,
        }
    }
    Json(serde_json::json!({ "changesets": out })).into_response()
}

/// GET /v1/orgs/:org/changesets/:key
pub async fn get(
    State(state): State<SharedState>,
    Path((org_name, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (cs, members) = match load(&state, &headers, &org_name, &org.id, &key, Scope::RepoRead) {
        Ok(x) => x,
        Err(r) => return r,
    };
    match changeset_json(&state, &cs, &members) {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

/// GET /v1/orgs/:org/changesets/:key/workspace — the changeset as one
/// checkout: every member at its proposed head, and the clone URL of the
/// read-only repository that checks them all out there.
///
/// The members and the composition hash here are
/// [`crate::workflow::trigger::compose`]'s, not `load`'s: `load` is the
/// authority check (every member readable, or the changeset does not
/// exist for this caller) and `compose` is what the git repository at
/// `clone_url` is built from, so the view describes exactly the tree a
/// clone will get and names the same composition its CI runs are named
/// by. The two sets differ only for a member whose repository is gone —
/// `load` skips it silently too — or one with no patchset, which cannot
/// be pinned to any commit and makes the whole thing un-composable.
pub async fn workspace(
    State(state): State<SharedState>,
    Path((org_name, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (cs, _) = match load(&state, &headers, &org_name, &org.id, &key, Scope::RepoRead) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let built = crate::changeset_workspace::build(&state, &org_name, &cs);
    let members: Vec<serde_json::Value> = built
        .members
        .iter()
        .map(|m| {
            serde_json::json!({
                "repo": m.repo.name,
                "change": m.change.change_key,
                "title": m.change.title,
                "path": m.repo.name,
                "commit": m.commit_sha,
                "fetch_ref": format!("refs/patchsets/{}", m.commit_sha),
                "clone_url": format!("{}/{org_name}/{}.git", state.public_url, m.repo.name),
                "ssh_clone_url": state
                    .ssh_public_url
                    .as_ref()
                    .map(|base| format!("{base}/{org_name}/{}.git", m.repo.name)),
            })
        })
        .collect();
    // The honest-middle sentence from the landing protocol: between the
    // first member's CAS and the last, the trunks disagree with each
    // other, and a reader who clones them now should be told which
    // state this checkout is.
    let note = (cs.state == "landing").then_some(
        "This changeset is landing: some members may already be on their trunks \
         while others are not. The workspace is the proposed state, not the trunks.",
    );
    Json(serde_json::json!({
        "key": cs.key,
        "title": cs.title,
        "state": cs.state,
        "composition": built.composition,
        "tip": built.composition.as_ref().map(|_| built.ws.tip.clone()),
        "clone_url": crate::changeset_workspace::clone_url(&state, &org_name, &cs.key),
        "ssh_clone_url": crate::changeset_workspace::ssh_clone_url(&state, &org_name, &cs.key),
        "members": members,
        "note": note,
    }))
    .into_response()
}

/// GET /v1/orgs/:org/changesets/:key/diffstat — how big this review is,
/// member by member.
///
/// The one number a reader wants before they open a change — `+412 −77`
/// — and until now nothing in the product could produce it: the tree
/// diff says which paths moved and says nothing about how much of them
/// did, so a page that rendered a size would have been rendering a
/// guess. This counts, from the same walk `…/diff` reports path by path.
///
/// Per member and not only in total, because "one review over four
/// repositories" is the thing a changeset *is*: a set that is +12 in the
/// API and +900 in the generated client is a different review from one
/// that is +450 in each, and a single total says the same thing about
/// both.
///
/// **`truncated` is a member's honesty flag**, not an error. A file too
/// large to read, a binary, a submodule pointer or a rewrite past the
/// work bound is left out of the counts and says so — see
/// [`reads::DiffStat`]. `files` stays exact either way.
///
/// Authority is the changeset's own: read on **every** member repository
/// or the changeset does not exist for this caller, masked identically
/// to [`get`] and [`verdict`], because the size of a review over a
/// private repository is a fact about that repository.
pub async fn diffstat(
    State(state): State<SharedState>,
    Path((org_name, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (cs, members) = match load(&state, &headers, &org_name, &org.id, &key, Scope::RepoRead) {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Landing order, so the numbers read down the page in the sequence
    // the lander walks — the same order every other member list here is
    // in, rather than a second opinion about how a changeset is ordered.
    let (_, order) = match ordered(&state, &cs, &members) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let mut rendered = Vec::with_capacity(order.len());
    let mut total = reads::DiffStat {
        files: 0,
        insertions: 0,
        deletions: 0,
        truncated: false,
    };
    for m in &order {
        // The same refusal `verdict` and `land` give, in the same words:
        // a member with no patchset names no commit, and there is no
        // size of nothing.
        let Some(latest) = m.latest.as_ref() else {
            return json_error(
                StatusCode::NOT_FOUND,
                format!("change {} has no patchsets", m.label()),
            );
        };
        let prefix = m.repo.prefix().as_str().to_string();
        let commit = latest.commit_oid.clone();
        let parent = latest.parent_oid.clone();
        let stat = match reads::with_reader(&state, prefix, move |reader| {
            reads::diffstat(reader, parent.as_deref(), &commit)
        })
        .await
        {
            Ok(s) => s,
            Err(e) => return reads::err_to_response(e),
        };
        total.files += stat.files;
        total.insertions += stat.insertions;
        total.deletions += stat.deletions;
        total.truncated |= stat.truncated;
        rendered.push(serde_json::json!({
            "repo": m.repo.name,
            "change": m.change.change_key,
            "patchset": latest.number,
            "files": stat.files,
            "insertions": stat.insertions,
            "deletions": stat.deletions,
            "truncated": stat.truncated,
        }));
    }
    Json(serde_json::json!({
        "changeset": cs.key,
        // The sum, so the header can say the size of the review without
        // the client adding up a list it may be paginating or collapsing.
        // `truncated` here means *some* member's is, which is the only
        // reading of it that cannot overstate what was counted.
        "total": {
            "files": total.files,
            "insertions": total.insertions,
            "deletions": total.deletions,
            "truncated": total.truncated,
        },
        "members": rendered,
    }))
    .into_response()
}

/// The principal behind a mutation whose authority was established per
/// member: the one every member admitted.
fn acting(members: &[Loaded]) -> Option<Principal> {
    members.first().map(|m| m.principal.clone())
}

/// POST /v1/orgs/:org/changesets/:key/members — add one change.
pub async fn add_member(
    State(state): State<SharedState>,
    Path((org_name, key)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<MemberRef>,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (cs, mut members) = match load(&state, &headers, &org_name, &org.id, &key, Scope::RepoWrite)
    {
        Ok(x) => x,
        Err(r) => return r,
    };
    let added = match resolve(&state, &headers, &org_name, &body, Scope::RepoWrite) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&added.principal));
    let label = added.label();
    if let Err(e) = changesets::add_member(
        &state.db,
        &cs.id,
        Candidate {
            change_id: &added.change.id,
            label: &label,
        },
        &actx,
    ) {
        return refusal(e);
    }
    members.push(added);
    // A different set of members is a different combination: every
    // composed run of the old one is superseded, including the ones for
    // members that did not change.
    crate::workflow::trigger::on_changeset(&state, &cs).await;
    match changeset_json(&state, &cs, &members) {
        Ok(v) => (StatusCode::CREATED, Json(v)).into_response(),
        Err(r) => r,
    }
}

/// DELETE /v1/orgs/:org/changesets/:key/members/:repo/:change
pub async fn remove_member(
    State(state): State<SharedState>,
    Path((org_name, key, repo, change)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (cs, members) = match load(&state, &headers, &org_name, &org.id, &key, Scope::RepoWrite) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(gone) = members
        .iter()
        .find(|m| m.repo.name == repo && m.change.change_key == change)
    else {
        return json_error(
            StatusCode::NOT_FOUND,
            format!("{repo}/{change} is not a member of changeset {key}"),
        );
    };
    let actx = AuditCtx::of(&org.id, acting(&members).as_ref());
    match changesets::remove_member(&state.db, &cs.id, &gone.change.id, &actx) {
        Ok(true) => {
            // Same argument as `add_member`, and the removed member's
            // own composed run goes with it: it is a run of a
            // combination that no longer exists.
            crate::workflow::trigger::on_changeset(&state, &cs).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => json_error(
            StatusCode::NOT_FOUND,
            format!("{repo}/{change} is not a member of changeset {key}"),
        ),
        Err(e) => refusal(e),
    }
}

/// PUT /v1/orgs/:org/changesets/:key/edges — replace the edges.
pub async fn set_edges(
    State(state): State<SharedState>,
    Path((org_name, key)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SetEdges>,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (cs, members) = match load(&state, &headers, &org_name, &org.id, &key, Scope::RepoWrite) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let labels_for_edges = edge_labels(&body.edges);
    let edges = edge_candidates(&body.edges, &members, &labels_for_edges);
    let actx = AuditCtx::of(&org.id, acting(&members).as_ref());
    // The module cannot name a member the way the caller does — it reads
    // ids out of the database — so the labels travel with the call. The
    // cycle refusal is the one that needs them.
    let member_labels: Vec<(String, String)> = members
        .iter()
        .map(|l| {
            (
                l.change.id.clone(),
                format!("{}/{}", l.repo.name, l.change.change_key),
            )
        })
        .collect();
    if let Err(e) = changesets::set_edges(&state.db, &cs.id, &edges, &member_labels, &actx) {
        return refusal(e);
    }
    match changeset_json(&state, &cs, &members) {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

/// POST /v1/orgs/:org/changesets/:key/abandon — close without landing;
/// every member goes back to being an ordinary open change.
pub async fn abandon(
    State(state): State<SharedState>,
    Path((org_name, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (cs, members) = match load(&state, &headers, &org_name, &org.id, &key, Scope::RepoWrite) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, acting(&members).as_ref());
    match changesets::abandon(&state.db, &cs.id, &actx) {
        Ok(true) => {
            // Nothing is going to read these verdicts again, and a
            // composed run is several machines' worth of work: stop it
            // rather than paying for a build of a review nobody will
            // land.
            crate::workflow::trigger::cancel_changeset(&state, &cs, "changeset abandoned").await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => json_error(StatusCode::CONFLICT, format!("changeset is {}", cs.state)),
        Err(e) => refusal(e),
    }
}

/// The target ref's tip in one member repository, and whether the
/// patchset is a fast-forward of it. Runs on the blocking pool.
fn preflight_tip(
    store: &ObjectStore,
    prefix: &str,
    target_ref: &str,
    commit: &str,
) -> Result<Result<Option<String>, String>, String> {
    let mbytes = store.get(&format!("{prefix}/manifest.json"))?;
    let manifest: Manifest =
        serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;
    if manifest.object_format != "sha1" {
        return Err(format!(
            "object_format {:?} not supported by this build (sha1 only)",
            manifest.object_format
        ));
    }
    let reader = LayoutReader::new(store, prefix, &manifest)?;
    let tip = reader.ref_oid(target_ref)?;
    Ok(
        match ancestry::is_fast_forward(&reader, tip.as_deref(), commit)? {
            Ancestry::FastForward => Ok(tip),
            Ancestry::NotAncestor => Err(format!(
                "not fast-forward from {}",
                tip.as_deref().map(|t| &t[..12.min(t.len())]).unwrap_or("?")
            )),
            Ancestry::CapExceeded => Err("history walk exceeded bound".into()),
        },
    )
}

/// POST /v1/orgs/:org/changesets/:key/land — land every member, or none.
///
/// Pre-flight is the whole review at one instant: every member's gate
/// green, every member's patchset a fast-forward of its target as the
/// target is now. Anything short of that is a 409 that names the member,
/// and nothing has been written. Unlike a single change, a member whose
/// checks are still running is a refusal rather than a wait: a changeset
/// is judged as one thing, and "wait for two of five" is not one thing.
///
/// Then the commit point: the plan — each member's ref, the tip it was
/// judged against and the commit it lands — is recorded and the
/// changeset turns `landing` in one transaction. From that row on the
/// queue finishes the landing, on this node or any other; the response
/// is 202 with the plan and the job, and `GET …/changesets/:key` shows
/// the landing's progress, member by member, until it is `landed` or
/// `failed`.
pub async fn land(
    State(state): State<SharedState>,
    Path((org_name, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (cs, members) = match load(&state, &headers, &org_name, &org.id, &key, Scope::RepoWrite) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if cs.state != "open" {
        return json_error(StatusCode::CONFLICT, format!("changeset is {}", cs.state));
    }
    let (_, order) = match ordered(&state, &cs, &members) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let mut verdicts = Vec::with_capacity(order.len());
    let mut heads: Vec<(&Loaded, &Patchset)> = Vec::with_capacity(order.len());
    for m in &order {
        let Some(latest) = m.latest.as_ref() else {
            return json_error(
                StatusCode::NOT_FOUND,
                format!("change {} has no patchsets", m.label()),
            );
        };
        let v = match compute_verdict(&state, &m.change, &m.repo, latest).await {
            Ok(v) => v,
            Err(e) => return reads::err_to_response(e),
        };
        let gate = match changes::land_gate(&state.db, &m.change.id) {
            Ok(g) => g,
            Err(e) => return internal(e),
        };
        verdicts.push(review::changeset::MemberVerdict {
            label: m.label(),
            state: m.change.state.clone(),
            verdict: v,
            gate,
        });
        heads.push((m, latest));
    }
    // Refused unless every gate is *green*, and a check still running is
    // not green. A single change waiting on CI is held and lands
    // unattended when the check reports; a changeset cannot be, because
    // the plan below is made against the trunks as they stand now, and
    // "the trunk has not moved since CI ran" is one of the things the
    // pre-flight promises. So `waiting` is a refusal here that says what
    // to wait for, and the person lands again when it has reported.
    let gate = match composed_gate(&state, &cs) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let composed = review::changeset::compose(&cs.state, &verdicts, &gate);
    if !composed.landable || composed.gate != review::changeset::Gate::Ready {
        // `gate` here is what stands in the way, and a refusal never
        // has "ready" in it. `composed.gate` is the *check* gate alone —
        // the verdict view pairs it with `landable` and lets the client
        // read the two together — but a 409 has no `landable` beside
        // it, so the same pair used to arrive as `error: "…: blocked:
        // needs an owner …"` next to `gate: "ready"`: two fields in one
        // body disagreeing about whether anything is wrong.
        let gate = if composed.landable {
            composed.gate
        } else {
            review::changeset::Gate::Blocked
        };
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": composed.explanation,
                "gate": gate.as_str(),
                "waiting_on": composed.waiting_on,
            })),
        )
            .into_response();
    }

    // The tips, as they are now. Each is the `old` the lander will CAS
    // against; a target that moves between here and there fails the
    // landing rather than landing a combination nobody reviewed.
    let mut plan: Vec<Step> = Vec::with_capacity(heads.len());
    for (m, ps) in &heads {
        let store_url = state.store_url.clone();
        let prefix = m.repo.prefix().as_str().to_string();
        let target_ref = format!("refs/heads/{}", m.change.target_branch);
        let commit = ps.commit_oid.clone();
        let read = tokio::task::spawn_blocking({
            let target_ref = target_ref.clone();
            move || {
                let store = ObjectStore::new(&store_url, LatencyModel::None);
                preflight_tip(&store, &prefix, &target_ref, &commit)
            }
        })
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")));
        let old = match read {
            Ok(Ok(tip)) => tip,
            // The same shape as the gate refusal above: a member whose
            // trunk has moved stands in the way exactly as a failing
            // check does, and a client reading `gate` to decide between
            // "wait" and "act" should not have to notice the field is
            // missing.
            Ok(Err(why)) => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": format!("{}: {why}", m.label()),
                        "gate": review::changeset::Gate::Blocked.as_str(),
                        "waiting_on": [],
                    })),
                )
                    .into_response()
            }
            Err(e) => return internal(e),
        };
        plan.push(Step {
            change_id: m.change.id.clone(),
            repo_id: m.repo.id.clone(),
            label: m.label(),
            ref_name: target_ref,
            old,
            new: ps.commit_oid.clone(),
            state: StepState::Pending,
            note: None,
        });
    }

    // Job first, then the commit point that names it — the same order
    // as a single change, for the same reason: a job that finds no
    // landing is a no-op, while a landing with no job waits for the
    // reaper.
    let landing_id = stratum_control::ids::ulid();
    let payload = serde_json::json!({
        "changeset_id": cs.id,
        "landing_id": landing_id,
    })
    .to_string();
    let job = match stratum_control::jobs::create(&state.db, &org.id, None, "land", Some(&payload))
    {
        Ok(j) => j,
        Err(e) => return internal(e),
    };
    let actx = AuditCtx::of(&org.id, acting(&members).as_ref());
    match changesets::begin_landing(&state.db, &cs.id, &landing_id, &job.id, &plan, &actx) {
        Ok(_) => {}
        Err(e) => {
            if let Err(e2) = stratum_control::jobs::complete(
                &state.db,
                &job.id,
                Some("no-op: the changeset did not begin landing"),
            ) {
                eprintln!("weft: complete unused land job {}: {e2}", job.id);
            }
            return refusal(e);
        }
    }
    let by_id: HashMap<&str, &Loaded> = members.iter().map(|m| (m.change.id.as_str(), m)).collect();
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "queued": true,
            "job": job.id,
            "changeset": cs.key,
            "landing": landing_id,
            "plan": plan.iter().map(|s| {
                let mut j = by_id.get(s.change_id.as_str()).map(|m| m.reference()).unwrap_or_default();
                j["ref"] = serde_json::json!(s.ref_name);
                j["old"] = serde_json::json!(s.old);
                j["new"] = serde_json::json!(s.new);
                j
            }).collect::<Vec<_>>(),
        })),
    )
        .into_response()
}
