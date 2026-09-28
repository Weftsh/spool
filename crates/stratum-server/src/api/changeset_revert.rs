//! Revert a landed changeset with one call.
//!
//! In every repository a member landed in, a commit is made that puts
//! back the paths that member changed; each is registered as a change
//! exactly as if a person had pushed it, and the changes are composed
//! into a new changeset whose edges are the original's reversed — what
//! landed last is undone first. That changeset is reviewed and landed
//! like any other: nothing about the landed one moves here, and trunk
//! moves only when the revert lands, through the same protocol and the
//! same OWNERS.
//!
//! The revert is **path-granular, not a rewind.** For each path the
//! landed member changed, its pre-landing entry is put back on top of
//! the trunk *as it is now*, so work that has landed since on other
//! paths is kept. A path somebody has changed since is a conflict, and
//! the whole call is refused before anything is written: a revert that
//! quietly undid somebody's later work would be a regression in the
//! shape of a fix, and a revert of three members out of four is a
//! half-landed changeset by another name.

use crate::api::changes_api;
use crate::api::changesets_api::{self, Loaded};
use crate::api::commits::{self, Leaf};
use crate::api::{internal, json_error};
use crate::app::SharedState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::Scope;
use stratum_control::changesets::{self, Candidate, Step, StepState};
use stratum_engine::objwrite::{
    self, encode_commit, hash_object, hex, new_object, parse_hex, CommitInfo, OBJ_BLOB, OBJ_COMMIT,
};
use stratum_engine::read::LayoutReader;
use stratum_engine::refops::{transact, Expect, NewPack, RefUpdate, TxnError};
use stratum_engine::treediff;
use stratum_store::{LatencyModel, Manifest, ObjectStore};

#[derive(Deserialize)]
pub struct RevertChangeset {
    /// Client-supplied key for the new changeset, like any other.
    pub key: String,
    /// Defaults to `Revert "<the original's title>"`.
    #[serde(default)]
    pub title: Option<String>,
    /// Defaults to a sentence naming what is reverted.
    #[serde(default)]
    pub body: Option<String>,
}

/// One member's revert commit, made and not yet written anywhere.
struct Made {
    /// The trunk tip it sits on — the parent, and the tip the branch
    /// write and the eventual landing are both judged against.
    parent: String,
    commit: String,
    pack: NewPack,
}

/// Why one member cannot be reverted as things stand.
enum Refused {
    /// These paths have changed on trunk since the member landed.
    Changed(Vec<String>),
    /// The target branch is gone.
    NoBranch,
    /// The revert branch is already there — an earlier attempt under
    /// this key got as far as writing it.
    BranchExists,
}

impl Refused {
    fn sentence(&self, step: &Step, branch: &str) -> String {
        match self {
            Refused::Changed(paths) => format!(
                "{} has changed since it landed at {}",
                step.ref_name,
                paths.join(", ")
            ),
            Refused::NoBranch => format!("{} no longer exists", step.ref_name),
            Refused::BranchExists => format!("{branch} already exists"),
        }
    }
    fn json(&self) -> serde_json::Value {
        match self {
            Refused::Changed(paths) => serde_json::json!({ "changed": paths }),
            Refused::NoBranch => serde_json::json!({ "no_branch": true }),
            Refused::BranchExists => serde_json::json!({ "branch_exists": true }),
        }
    }
}

/// Make the revert of one landed step against its repository as it is
/// now. Reads only. The outer `Err` is the store failing; the inner one
/// is a refusal in the reader's words.
fn make(
    store: &ObjectStore,
    prefix: &str,
    step: &Step,
    branch_ref: &str,
    author: &str,
    message: &str,
) -> Result<Result<Made, Refused>, String> {
    let mbytes = store.get(&format!("{prefix}/manifest.json"))?;
    let manifest: Manifest =
        serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;
    let reader = LayoutReader::new(store, prefix, &manifest)?;
    if reader.ref_oid(branch_ref)?.is_some() {
        return Ok(Err(Refused::BranchExists));
    }
    let Some(tip) = reader.ref_oid(&step.ref_name)? else {
        return Ok(Err(Refused::NoBranch));
    };
    // What the member changed, and whether each of those paths is still
    // as the member left it. A path that is not — edited, deleted, or
    // turned into a directory — is somebody's later work.
    let mut changes: Vec<(Vec<String>, Option<Leaf>)> = Vec::new();
    let mut changed = Vec::new();
    for e in treediff::diff_commits(&reader, step.old.as_deref(), &step.new)? {
        let now = reader.entry_at(&tip, &e.path)?.map(|t| hex(&t.oid));
        if now != e.new_oid {
            changed.push(e.path);
            continue;
        }
        let leaf = match (&e.old_oid, &e.old_mode) {
            (Some(oid), Some(mode)) => Some(Leaf {
                oid: parse_hex(oid)?,
                mode: mode.clone(),
            }),
            _ => None,
        };
        let parts = commits::split_path(&e.path).map_err(|_| format!("bad path {:?}", e.path))?;
        changes.push((parts, leaf));
    }
    if !changed.is_empty() {
        return Ok(Err(Refused::Changed(changed)));
    }
    let (k, data) = reader.object(&tip)?;
    if k != OBJ_COMMIT {
        return Err(format!("{tip} is not a commit"));
    }
    let tip_tree = objwrite::parse_commit(&data)?.tree;
    let mut objects = Vec::new();
    let tree = commits::build_tree(&reader, Some(&tip_tree), &changes, &mut objects)?;
    let commit = new_object(
        OBJ_COMMIT,
        encode_commit(&CommitInfo {
            tree,
            parents: vec![parse_hex(&tip)?],
            author: author.to_string(),
            committer: author.to_string(),
            timestamp: commits::stratum_control_free_now(),
            message: message.to_string(),
        }),
    );
    let commit_hex = hex(&commit.oid);
    objects.push(commit);
    // A tree put back to how it was is often a tree the layout already
    // holds (I5): ship only what is new.
    let to_pack = commits::absent_from_layout(store, prefix, &manifest, objects)?;
    let (payload, entries) = objwrite::build_pack_payload(&to_pack)?;
    let mut oids: Vec<[u8; 20]> = to_pack.iter().map(|o| o.oid).collect();
    oids.sort();
    Ok(Ok(Made {
        parent: tip,
        commit: commit_hex,
        pack: NewPack {
            payload,
            oids,
            entries,
        },
    }))
}

/// The Change-Id a revert commit carries: a function of what it reverts
/// and the changeset it reverts it for, so the same revert asked for
/// twice under one key is one change and not two.
fn change_id_for(new_key: &str, step: &Step) -> String {
    let seed = format!("revert {new_key} {} {}", step.label, step.new);
    format!("I{}", hex(&hash_object(OBJ_BLOB, seed.as_bytes())))
}

/// POST /v1/orgs/:org/changesets/:key/revert — a new changeset that
/// undoes every landed member of this one. `repo:write` on every member,
/// as composing one needs.
pub async fn revert(
    State(state): State<SharedState>,
    Path((org_name, key)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RevertChangeset>,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (cs, members) =
        match changesets_api::load(&state, &headers, &org_name, &org.id, &key, Scope::RepoWrite) {
            Ok(x) => x,
            Err(r) => return r,
        };
    if cs.state != "landed" && cs.state != "failed" {
        return json_error(
            StatusCode::CONFLICT,
            format!("changeset is {}: nothing of it has landed", cs.state),
        );
    }
    // The new key is checked here, before a single branch is written,
    // rather than left to `changesets::create` at the end: a refusal
    // there would leave the revert changes made and homeless.
    if !stratum_control::changes::valid_change_key(&body.key) {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("invalid changeset key {:?}", body.key),
        );
    }
    match changesets::get(&state.db, &org.id, &body.key) {
        Ok(None) => {}
        Ok(Some(_)) => {
            return json_error(
                StatusCode::CONFLICT,
                format!("changeset {} already exists", body.key),
            )
        }
        Err(e) => return internal(e.to_string()),
    }
    // What landed is what the landing's record says landed: a `Done`
    // step is a member whose trunk this changeset moved and that nobody
    // has put back — in a `landed` changeset that is every member, in a
    // `failed` one the members the unwind could not revert.
    let landing = match changesets::latest_landing(&state.db, &cs.id) {
        Ok(Some(l)) => l,
        Ok(None) => {
            return internal(format!(
                "changeset {key} is {} but has no landing record",
                cs.state
            ))
        }
        Err(e) => return internal(e.to_string()),
    };
    let landed: Vec<&Step> = landing
        .progress
        .iter()
        .filter(|s| s.state == StepState::Done)
        .collect();
    if landed.is_empty() {
        return json_error(
            StatusCode::CONFLICT,
            format!("nothing of changeset {key} is landed: every member that landed was reverted"),
        );
    }
    let by_change: HashMap<&str, &Loaded> =
        members.iter().map(|m| (m.change.id.as_str(), m)).collect();
    let principal = members.last().map(|m| m.principal.clone());
    let author = commits::acting_author(&state, principal.as_ref(), None);
    let branch = format!("revert/{}", body.key);
    let branch_ref = format!("refs/heads/{branch}");

    // Make every revert first, writing nothing; refuse them all if any
    // one cannot be made.
    let mut made: Vec<(&Loaded, &Step, String, Made)> = Vec::with_capacity(landed.len());
    let mut refused: Vec<(&Step, Refused)> = Vec::new();
    for &step in &landed {
        // A member's repository deleted since the landing: the record
        // names a trunk that is not there to revert.
        let Some(&m) = by_change.get(step.change_id.as_str()) else {
            return json_error(
                StatusCode::CONFLICT,
                format!("{}: the repository no longer exists", step.label),
            );
        };
        let message = format!(
            "Revert \"{title}\"\n\n\
             Reverts {label}, landed by changeset {cs_key} as {new}.\n\n\
             Change-Id: {cid}\n",
            title = m.change.title,
            label = step.label,
            cs_key = cs.key,
            new = step.new,
            cid = change_id_for(&body.key, step),
        );
        let store_url = state.store_url.clone();
        let prefix = m.repo.prefix().as_str().to_string();
        let out = tokio::task::spawn_blocking({
            let step = step.clone();
            let branch_ref = branch_ref.clone();
            let author = author.clone();
            let message = message.clone();
            move || {
                let store = ObjectStore::new(&store_url, LatencyModel::None);
                make(&store, &prefix, &step, &branch_ref, &author, &message)
            }
        })
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")));
        match out {
            Ok(Ok(r)) => made.push((m, step, message, r)),
            Ok(Err(why)) => refused.push((step, why)),
            Err(e) => return internal(format!("{}: {e}", step.label)),
        }
    }
    if let Some((first, why)) = refused.first() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("{}: {}", first.label, why.sentence(first, &branch)),
                "conflicts": refused.iter().map(|(s, why)| {
                    let (repo, change) = s.label.split_once('/').unwrap_or((s.label.as_str(), ""));
                    let mut j = why.json();
                    j["repo"] = serde_json::json!(repo);
                    j["change"] = serde_json::json!(change);
                    j["why"] = serde_json::json!(why.sentence(s, &branch));
                    j
                }).collect::<Vec<_>>(),
            })),
        )
            .into_response();
    }

    // Write the branches — each a CAS from "absent", so a racing second
    // call under the same key loses here and is told so.
    for (m, step, _, r) in &made {
        let store_url = state.store_url.clone();
        let prefix = m.repo.prefix().as_str().to_string();
        let update = RefUpdate {
            name: branch_ref.clone(),
            expect: Expect::Absent,
            new: Some(r.commit.clone()),
        };
        let pack = NewPack {
            payload: r.pack.payload.clone(),
            oids: r.pack.oids.clone(),
            entries: r.pack.entries,
        };
        let written = tokio::task::spawn_blocking(move || {
            let store = ObjectStore::new(&store_url, LatencyModel::None);
            transact(&store, &prefix, &[update], Some(&pack)).map(|_| ())
        })
        .await
        .unwrap_or_else(|e| Err(TxnError::Other(format!("join: {e}"))));
        match written {
            Ok(()) => {}
            Err(TxnError::Conflict(..)) => {
                return json_error(
                    StatusCode::CONFLICT,
                    format!("{}: {branch} already exists", step.label),
                )
            }
            Err(TxnError::Other(e)) => return internal(format!("{}: {e}", step.label)),
        }
    }

    // Register each as a change on the member's target, the way a push
    // would have: pinned, given its CI, its owners told.
    let mut new_members: Vec<Loaded> = Vec::with_capacity(made.len());
    for (m, _, message, r) in &made {
        match changes_api::register_patchset(
            &state,
            &org,
            &m.repo,
            principal.as_ref(),
            &m.change.target_branch,
            &r.commit,
            Some(&r.parent),
            message,
            None,
        )
        .await
        {
            Ok((change, patchset, _)) => new_members.push(Loaded {
                repo: m.repo.clone(),
                change,
                latest: Some(patchset),
                principal: m.principal.clone(),
            }),
            Err(r) => return r,
        }
    }

    // Compose them, with the original's edges reversed: what landed
    // after its dependency is undone before it.
    let edges = match changesets::edges(&state.db, &cs.id) {
        Ok(e) => e,
        Err(e) => return internal(e.to_string()),
    };
    let reverted_of: HashMap<&str, usize> = made
        .iter()
        .enumerate()
        .map(|(i, (m, ..))| (m.change.id.as_str(), i))
        .collect();
    let labels: Vec<String> = new_members.iter().map(Loaded::label).collect();
    let cands: Vec<Candidate> = new_members
        .iter()
        .zip(&labels)
        .map(|(m, label)| Candidate {
            change_id: m.change.id.as_str(),
            label: label.as_str(),
        })
        .collect();
    let edge_cands: Vec<(Candidate, Candidate)> = edges
        .iter()
        .filter_map(|e| {
            let from = reverted_of.get(e.from_change_id.as_str())?;
            let to = reverted_of.get(e.to_change_id.as_str())?;
            Some((cands[*to], cands[*from]))
        })
        .collect();
    let title = body
        .title
        .unwrap_or_else(|| format!("Revert \"{}\"", cs.title));
    let title = if title.len() <= 200 {
        title
    } else {
        format!("Revert changeset {}", cs.key)
    };
    let text = body
        .body
        .unwrap_or_else(|| format!("Reverts changeset {}.", cs.key));
    let actx = AuditCtx::of(&org.id, principal.as_ref());
    let created_by = principal.as_ref().and_then(|p| p.user_id.clone());
    let new_cs = match changesets::create(
        &state.db,
        &org.id,
        &body.key,
        &title,
        &text,
        created_by.as_deref(),
        &cands,
        &edge_cands,
        Some(&cs.id),
        &actx,
    ) {
        Ok(cs) => cs,
        Err(e) => return changesets_api::refusal(e),
    };
    // A revert is an ordinary changeset from here on, and that includes
    // its CI: the combination of reverting changes is exactly the thing
    // nobody has built yet.
    crate::workflow::trigger::on_changeset(&state, &new_cs).await;
    match changesets_api::changeset_json(&state, &new_cs, &new_members) {
        Ok(v) => (StatusCode::CREATED, Json(v)).into_response(),
        Err(r) => r,
    }
}
