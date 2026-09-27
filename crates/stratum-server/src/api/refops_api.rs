//! Undo primitives (R4): branch, reset (soft ref move), revert (new
//! commit), tags — direct manifest CAS through the engine's ref
//! transactions. Reset keeps orphaned commits reachable by SHA until GC.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::mirror::forward::Refusal;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::Scope;
use stratum_control::registry::{Repo, RepoKind};
use stratum_engine::objwrite::{self, encode_commit, hex, new_object, CommitInfo, OBJ_COMMIT};
use stratum_engine::read::LayoutReader;
use stratum_engine::refops::{transact, Expect, NewPack, RefUpdate, TxnError};
use stratum_proto::receive::Update;
use stratum_store::{LatencyModel, Manifest, ObjectStore};

struct Ctx {
    prefix: String,
    repo_id: String,
    /// Who is acting, resolved once at the seam so every refop records
    /// the same answer.
    actx: AuditCtx,
    /// The repository row itself, wanted by two things at once: a
    /// mirror's refop is forwarded to its origin and the forward needs
    /// the row, and a ref that moves here is a push as far as CI and
    /// site publishing are concerned.
    repo: Repo,
}

fn setup(state: &SharedState, headers: &HeaderMap, org: &str, repo: &str) -> Result<Ctx, Response> {
    let (org_row, repo_row, principal) =
        crate::app::rest_repo_auth(state, headers, org, repo, Scope::RepoWrite)?;
    Ok(Ctx {
        prefix: repo_row.prefix().as_str().to_string(),
        actx: AuditCtx::of(&org_row.id, principal.as_ref()),
        repo_id: repo_row.id.clone(),
        repo: repo_row,
    })
}

/// Why a ref transaction did not land: the engine's own answer for a
/// native repository, or the origin's for a mirror.
pub(crate) enum LandError {
    Txn(TxnError),
    Forward(Refusal),
}

impl From<TxnError> for LandError {
    fn from(e: TxnError) -> LandError {
        LandError::Txn(e)
    }
}

impl From<String> for LandError {
    fn from(e: String) -> LandError {
        LandError::Txn(TxnError::Other(e))
    }
}

/// Land a ref transaction where it belongs: in this layout for a native
/// repository, at the origin first for a mirror.
///
/// The mirror half runs the async forward from this blocking thread
/// with `block_on`, which is what a blocking-pool thread is for; the
/// REST doors already do their tree work there.
pub(crate) fn land(
    state: &SharedState,
    repo: &Repo,
    updates: Vec<RefUpdate>,
    pack: Option<NewPack>,
) -> Result<(), LandError> {
    if repo.kind != RepoKind::Mirror {
        let store = ObjectStore::new(&state.store_url, LatencyModel::None);
        let prefix = repo.prefix().as_str().to_string();
        return transact(&store, &prefix, &updates, pack.as_ref())
            .map(|_| ())
            .map_err(LandError::Txn);
    }
    forward_rest(state, repo, updates, pack).map_err(LandError::Forward)
}

/// Forward a REST write to a mirror's origin. The transaction's
/// preconditions become the push's leases: an exact expectation is the
/// old value; "must not exist" is the empty lease; "whatever it is now"
/// is read from the manifest, so that nothing is ever forced.
pub(crate) fn forward_rest(
    state: &SharedState,
    repo: &Repo,
    updates: Vec<RefUpdate>,
    pack: Option<NewPack>,
) -> Result<(), Refusal> {
    const ZERO: &str = "0000000000000000000000000000000000000000";
    let store = ObjectStore::new(&state.store_url, LatencyModel::None);
    let prefix = repo.prefix().as_str().to_string();
    let mbytes = store.get(&format!("{prefix}/manifest.json")).map_err(|e| {
        if stratum_engine::errclass::is_absent(&e) {
            Refusal::Unreachable(crate::mirror::forward::NO_LAYOUT_YET.into())
        } else {
            Refusal::Unreachable(e)
        }
    })?;
    let manifest: Manifest = serde_json::from_slice(&mbytes)
        .map_err(|e| Refusal::Unreachable(format!("manifest: {e}")))?;
    let mut commands = Vec::with_capacity(updates.len());
    for u in updates {
        let old = match u.expect {
            Expect::Equals(v) => v,
            Expect::Absent => ZERO.to_string(),
            Expect::Any => {
                match stratum_engine::refops::current_ref(&store, &manifest, &u.name)
                    .map_err(Refusal::Unreachable)?
                {
                    Some(cur) => cur,
                    // Deleting what is not there: the answer a native
                    // repository gives, before the origin is asked.
                    None if u.new.is_none() => {
                        return Err(Refusal::Local(format!("unknown rev {:?}", u.name)))
                    }
                    None => ZERO.to_string(),
                }
            }
        };
        commands.push(Update {
            old,
            new: u.new.unwrap_or_else(|| ZERO.to_string()),
            name: u.name,
        });
    }
    let sealed = pack.map(|p| objwrite::seal_pack(&p.payload, p.entries));
    tokio::runtime::Handle::current()
        .block_on(
            state
                .sync
                .forward(repo, commands, sealed, state.freshness_timeout),
        )
        .map(|_| ())
}

/// A forwarded write's refusal, as a REST answer. Each kind of refusal
/// is a different status because each asks a different thing of the
/// caller: rebase, ask an owner, approve a permission, wait, or give
/// up.
pub(crate) fn forward_response(state: &SharedState, repo: &Repo, refusal: Refusal) -> Response {
    match refusal {
        Refusal::Each(verdicts) => {
            let refused: Vec<serde_json::Value> = verdicts
                .iter()
                .filter_map(|v| v.as_ref().err().map(|e| serde_json::json!(e)))
                .collect();
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": refused
                        .first()
                        .and_then(|v| v.as_str())
                        .unwrap_or("the origin refused the push"),
                    "refused": refused,
                })),
            )
                .into_response()
        }
        Refusal::Behind(s) => {
            let current = stratum_control::registry::repo_by_id(&state.db, &repo.org_id, &repo.id)
                .ok()
                .flatten()
                .and_then(|r| r.last_synced_commit);
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": s, "current_tip": current })),
            )
                .into_response()
        }
        Refusal::Local(s) if stratum_engine::errclass::is_unknown_rev(&s) => {
            json_error(StatusCode::NOT_FOUND, s)
        }
        Refusal::Local(s) => json_error(StatusCode::BAD_REQUEST, s),
        Refusal::PermissionDenied(s) => {
            let approve_url = state.sync.github_app().and_then(|app| {
                repo.origin_installation
                    .as_deref()
                    .and_then(|inst| app.installation(inst).ok().flatten())
                    .map(|detail| app.approve_url(&detail))
            });
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": s,
                    "needs_permission": true,
                    "approve_url": approve_url,
                })),
            )
                .into_response()
        }
        Refusal::NoCredential(s) => json_error(StatusCode::FORBIDDEN, s),
        Refusal::Unreachable(s) => json_error(StatusCode::BAD_GATEWAY, s),
        Refusal::Busy(s) => json_error(StatusCode::SERVICE_UNAVAILABLE, s),
    }
}

fn land_response(state: &SharedState, repo: &Repo, e: LandError) -> Response {
    match e {
        LandError::Txn(e) => txn_response(e),
        LandError::Forward(r) => forward_response(state, repo, r),
    }
}

/// A ref this route moved may be what a site publishes from.
///
/// Site publishing rides the push trigger for the commit route and the
/// two push doors; reset and branch creation armed nothing, so a deploy
/// that staged its chunks on another branch and then moved the published
/// branch in one step left the site on the old tree until the next push
/// (`weftsh/deploy-site` had to make a no-op commit after its reset to
/// get published at all). Only the publish job is armed here, not CI: a
/// branch created at an already-built commit is not a new build, and
/// the fork suite pins that a stranger's branch creation launches
/// nothing. The job reads the tip when it runs, so one enqueue covers
/// whatever the ref now points at.
fn published(state: &SharedState, ctx: &Ctx) {
    crate::workers::sitepublish::enqueue(state, &ctx.repo.org_id, &ctx.repo_id);
}

/// A protected branch moves only through the land queue; reset, revert
/// and delete are exactly the history rewrites protection exists to
/// stop. Same sentence as the push doors.
fn protected_guard(state: &SharedState, repo_id: &str, branch: &str) -> Result<(), Response> {
    match stratum_control::protections::is_protected(&state.db, repo_id, branch) {
        Ok(true) => Err(json_error(
            StatusCode::FORBIDDEN,
            stratum_control::protections::refusal(branch),
        )),
        Ok(false) => Ok(()),
        Err(e) => Err(internal(e)),
    }
}

fn audit(state: &SharedState, ctx: &Ctx, action: &str, blob: serde_json::Value) {
    crate::api::record_or_warn(
        &state.db,
        &ctx.actx,
        Some(&ctx.repo_id),
        action,
        Some(&blob),
    );
}

/// Resolve a rev and require it to exist in this layout.
fn resolve_existing(store_url: &str, prefix: &str, rev: &str) -> Result<String, String> {
    let store = ObjectStore::new(store_url, LatencyModel::None);
    let mbytes = store.get(&format!("{prefix}/manifest.json"))?;
    let manifest: Manifest =
        serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;
    let reader = LayoutReader::new(&store, prefix, &manifest)?;
    let oid = reader
        .resolve_rev(rev)?
        .ok_or_else(|| format!("unknown rev {rev:?}"))?;
    // The object must actually be present (a reset target from another
    // repo must not slip in).
    reader.object(&oid)?;
    Ok(oid)
}

fn txn_response(e: TxnError) -> Response {
    match e {
        TxnError::Conflict(name, cur) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("precondition failed on {name}"),
                "current": cur,
            })),
        )
            .into_response(),
        TxnError::Other(e)
            if stratum_engine::errclass::is_unknown_rev(&e)
                || stratum_engine::errclass::is_missing_path(&e) =>
        {
            json_error(StatusCode::NOT_FOUND, e)
        }
        TxnError::Other(e) if stratum_engine::errclass::is_absent(&e) => json_error(
            StatusCode::BAD_GATEWAY,
            crate::mirror::forward::NO_LAYOUT_YET,
        ),
        TxnError::Other(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct CreateBranchBody {
    pub name: String,
    pub from: String,
}

pub async fn create_branch(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<CreateBranchBody>,
) -> Response {
    let ctx = match setup(&state, &headers, &org, &repo) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let store_url = state.store_url.clone();
    let prefix = ctx.prefix.clone();
    let state2 = state.clone();
    let repo_row = ctx.repo.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<String, LandError> {
        let oid = resolve_existing(&store_url, &prefix, &body.from).map_err(TxnError::Other)?;
        land(
            &state2,
            &repo_row,
            vec![RefUpdate {
                name: format!("refs/heads/{}", body.name),
                expect: Expect::Absent,
                new: Some(oid.clone()),
            }],
            None,
        )?;
        Ok(oid)
    })
    .await
    .unwrap_or_else(|e| Err(LandError::from(format!("task join: {e}"))));
    match out {
        Ok(oid) => {
            audit(
                &state,
                &ctx,
                "repo.branch.create",
                serde_json::json!({"oid": oid}),
            );
            published(&state, &ctx);
            (StatusCode::CREATED, Json(serde_json::json!({ "oid": oid }))).into_response()
        }
        Err(e) => land_response(&state, &ctx.repo, e),
    }
}

pub async fn delete_branch(
    State(state): State<SharedState>,
    Path((org, repo, name)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let ctx = match setup(&state, &headers, &org, &repo) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Err(r) = protected_guard(&state, &ctx.repo_id, &name) {
        return r;
    }
    let state2 = state.clone();
    let repo_row = ctx.repo.clone();
    let ref_name = format!("refs/heads/{name}");
    let out = tokio::task::spawn_blocking(move || {
        land(
            &state2,
            &repo_row,
            vec![RefUpdate {
                name: ref_name,
                expect: Expect::Any,
                new: None,
            }],
            None,
        )
    })
    .await
    .unwrap_or_else(|e| Err(LandError::from(format!("task join: {e}"))));
    match out {
        Ok(()) => {
            audit(
                &state,
                &ctx,
                "repo.branch.delete",
                serde_json::json!({"name": name}),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => land_response(&state, &ctx.repo, e),
    }
}

#[derive(Deserialize)]
pub struct ResetBody {
    #[serde(default = "default_branch")]
    pub branch: String,
    pub to: String,
    #[serde(default)]
    pub expected_head: Option<String>,
}

fn default_branch() -> String {
    "main".into()
}

/// Soft ref move — the "undo the agent's last changes" primitive. Given
/// commits A→B→C and reset to A, B and C stay reachable by SHA until GC.
pub async fn reset(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<ResetBody>,
) -> Response {
    let ctx = match setup(&state, &headers, &org, &repo) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Err(r) = protected_guard(&state, &ctx.repo_id, &body.branch) {
        return r;
    }
    let store_url = state.store_url.clone();
    let prefix = ctx.prefix.clone();
    let state2 = state.clone();
    let repo_row = ctx.repo.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<String, LandError> {
        let oid = resolve_existing(&store_url, &prefix, &body.to).map_err(TxnError::Other)?;
        land(
            &state2,
            &repo_row,
            vec![RefUpdate {
                name: format!("refs/heads/{}", body.branch),
                expect: match body.expected_head {
                    Some(h) => Expect::Equals(h),
                    None => Expect::Any,
                },
                new: Some(oid.clone()),
            }],
            None,
        )?;
        Ok(oid)
    })
    .await
    .unwrap_or_else(|e| Err(LandError::from(format!("task join: {e}"))));
    match out {
        Ok(oid) => {
            audit(&state, &ctx, "repo.reset", serde_json::json!({"to": oid}));
            published(&state, &ctx);
            Json(serde_json::json!({ "oid": oid })).into_response()
        }
        Err(e) => land_response(&state, &ctx.repo, e),
    }
}

/// The oid of git's empty tree.
pub(crate) const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A commit on top of `head` whose tree is `tree` — undo by append, the
/// way `git revert` leaves history intact — packed and ready to travel
/// with the ref update that installs it. Answers the pack and the new
/// commit's oid.
///
/// Shared by the revert route and the changeset lander's unwind, which
/// is what the landing protocol asks for: the operation that puts a
/// repository back after a member of a failed landing had already moved
/// its trunk is the same one a person reaches for, so it is tested by
/// both.
pub(crate) fn restoring_commit(
    reader: &LayoutReader,
    head: &str,
    tree: &str,
    who: &str,
    message: &str,
) -> Result<(NewPack, String), String> {
    let mut objects = Vec::new();
    if tree == EMPTY_TREE {
        // The empty tree may not exist in the layout; ship it.
        let empty = new_object(objwrite::OBJ_TREE, Vec::new());
        if reader.object(&hex(&empty.oid)).is_err() {
            objects.push(empty);
        }
    }
    let commit = new_object(
        OBJ_COMMIT,
        encode_commit(&CommitInfo {
            tree: objwrite::parse_hex(tree)?,
            parents: vec![objwrite::parse_hex(head)?],
            author: format!("{who} <{who}@stratum.local>"),
            committer: format!("{who} <{who}@stratum.local>"),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            message: message.to_string(),
        }),
    );
    let commit_hex = hex(&commit.oid);
    objects.push(commit);
    let (payload, entries) = objwrite::build_pack_payload(&objects)?;
    let mut oids: Vec<[u8; 20]> = objects.iter().map(|o| o.oid).collect();
    oids.sort();
    Ok((
        NewPack {
            payload,
            oids,
            entries,
        },
        commit_hex,
    ))
}

#[derive(Deserialize)]
pub struct RevertBody {
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(default)]
    pub expected_head: Option<String>,
}

/// Revert the branch head: a NEW commit whose tree is the head's first
/// parent's tree (history preserved — this is undo-by-append; v1 reverts
/// the head only, arbitrary-commit revert needs merge machinery).
pub async fn revert(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RevertBody>,
) -> Response {
    let ctx = match setup(&state, &headers, &org, &repo) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Err(r) = protected_guard(&state, &ctx.repo_id, &body.branch) {
        return r;
    }
    let store_url = state.store_url.clone();
    let prefix = ctx.prefix.clone();
    let state2 = state.clone();
    let repo_row = ctx.repo.clone();
    let who = ctx.actx.principal.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<String, LandError> {
        let store = ObjectStore::new(&store_url, LatencyModel::None);
        let mbytes = store
            .get(&format!("{prefix}/manifest.json"))
            .map_err(TxnError::Other)?;
        let manifest: Manifest = serde_json::from_slice(&mbytes)
            .map_err(|e| TxnError::Other(format!("manifest: {e}")))?;
        let reader = LayoutReader::new(&store, &prefix, &manifest).map_err(TxnError::Other)?;
        let branch_ref = format!("refs/heads/{}", body.branch);
        let head = reader
            .ref_oid(&branch_ref)
            .map_err(TxnError::Other)?
            .ok_or_else(|| TxnError::Other(format!("unknown branch {:?}", body.branch)))?;
        if let Some(want) = &body.expected_head {
            if want != &head {
                return Err(TxnError::Conflict(branch_ref, Some(head)).into());
            }
        }
        let (k, data) = reader.object(&head).map_err(TxnError::Other)?;
        if k != OBJ_COMMIT {
            return Err(TxnError::Other(format!("{head} is not a commit")).into());
        }
        let parsed = objwrite::parse_commit(&data).map_err(TxnError::Other)?;
        let target_tree = match parsed.parents.first() {
            Some(p) => {
                let (_, pdata) = reader.object(p).map_err(TxnError::Other)?;
                objwrite::parse_commit(&pdata)
                    .map_err(TxnError::Other)?
                    .tree
            }
            // Reverting the root commit: empty tree.
            None => EMPTY_TREE.to_string(),
        };
        let (pack, commit_hex) = restoring_commit(
            &reader,
            &head,
            &target_tree,
            &who,
            &format!("Revert {}\n\nReverts commit {head}.", &head[..12]),
        )
        .map_err(TxnError::Other)?;
        land(
            &state2,
            &repo_row,
            vec![RefUpdate {
                name: branch_ref,
                expect: Expect::Equals(head),
                new: Some(commit_hex.clone()),
            }],
            Some(pack),
        )?;
        Ok(commit_hex)
    })
    .await
    .unwrap_or_else(|e| Err(LandError::from(format!("task join: {e}"))));
    match out {
        Ok(commit) => {
            audit(
                &state,
                &ctx,
                "repo.revert",
                serde_json::json!({"commit": commit}),
            );
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "commit": commit })),
            )
                .into_response()
        }
        Err(e) => land_response(&state, &ctx.repo, e),
    }
}

#[derive(Deserialize)]
pub struct CreateTagBody {
    pub name: String,
    pub target: String,
}

pub async fn create_tag(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<CreateTagBody>,
) -> Response {
    let ctx = match setup(&state, &headers, &org, &repo) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let store_url = state.store_url.clone();
    let prefix = ctx.prefix.clone();
    let state2 = state.clone();
    let repo_row = ctx.repo.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<String, LandError> {
        let oid = resolve_existing(&store_url, &prefix, &body.target).map_err(TxnError::Other)?;
        land(
            &state2,
            &repo_row,
            vec![RefUpdate {
                name: format!("refs/tags/{}", body.name),
                expect: Expect::Absent,
                new: Some(oid.clone()),
            }],
            None,
        )?;
        Ok(oid)
    })
    .await
    .unwrap_or_else(|e| Err(LandError::from(format!("task join: {e}"))));
    match out {
        Ok(oid) => {
            audit(
                &state,
                &ctx,
                "repo.tag.create",
                serde_json::json!({"oid": oid}),
            );
            (StatusCode::CREATED, Json(serde_json::json!({ "oid": oid }))).into_response()
        }
        Err(e) => land_response(&state, &ctx.repo, e),
    }
}

pub async fn delete_tag(
    State(state): State<SharedState>,
    Path((org, repo, name)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let ctx = match setup(&state, &headers, &org, &repo) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let state2 = state.clone();
    let repo_row = ctx.repo.clone();
    let ref_name = format!("refs/tags/{name}");
    let out = tokio::task::spawn_blocking(move || {
        land(
            &state2,
            &repo_row,
            vec![RefUpdate {
                name: ref_name,
                expect: Expect::Any,
                new: None,
            }],
            None,
        )
    })
    .await
    .unwrap_or_else(|e| Err(LandError::from(format!("task join: {e}"))));
    match out {
        Ok(()) => {
            audit(
                &state,
                &ctx,
                "repo.tag.delete",
                serde_json::json!({"name": name}),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => land_response(&state, &ctx.repo, e),
    }
}
