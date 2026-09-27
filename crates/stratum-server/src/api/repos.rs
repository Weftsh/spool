//! Repos lifecycle API (R1): create in <100 ms (one row + one conditional
//! PUT), delete (instant tombstone), list (keyset pagination), batch
//! create/delete (1k per call).

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::Scope;
use stratum_control::registry::{self, NewRepo, Repo, RepoKind};
use stratum_store::{LatencyModel, ObjectStore};

#[derive(Deserialize)]
pub struct CreateRepoBody {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Refused when `true` — see [`crate::api::NO_PUBLIC_REPOS`].
    #[serde(default)]
    pub public: Option<bool>,
    #[serde(default = "default_branch")]
    pub default_branch: String,
}

fn default_branch() -> String {
    "main".into()
}

#[derive(Serialize)]
pub struct RepoView {
    #[serde(flatten)]
    pub repo: Repo,
    /// The namespace this repository is in, by name.
    ///
    /// The row carries `org_id`, which is no use to a client: after
    /// forking into "wherever I belong", the answer to "where did it
    /// go" is a name, and resolving an id to one is a lookup the client
    /// should not have to make to follow its own action.
    pub org: String,
    pub clone_url: String,
    /// Null unless the deployment exposes an SSH endpoint
    /// (STRATUM_SSH_PUBLIC_URL); clients must handle both.
    pub ssh_clone_url: Option<String>,
    /// What this repository holds in the object store, in bytes, as of
    /// the last write or storage sweep — the number a private
    /// repository's organization is metered on. Zero for a repository
    /// nothing has been written to yet, and for a zero-copy fork until
    /// it is promoted. See `crate::storage`.
    pub stored_bytes: i64,
    /// Fork preparation state, `null` on a repository that is not a
    /// fork.
    ///
    /// Always serialized, deliberately. "Absent" and "null" would be two
    /// encodings of one fact — this is not a fork — and a client given
    /// two encodings handles one of them and quietly drops the other.
    /// `StarView.origin` next door made the same call for the same
    /// reason, and the two sit on the same masthead.
    ///
    /// It is also what keeps the OpenAPI gate meaningful: that check is
    /// two-way, so an optional field can be neither documented (phantom
    /// documentation while the server omits it) nor left undocumented
    /// (red the first time a fixture is a fork). Always-present is the
    /// only shape that is honest in both directions.
    pub fork_state: Option<String>,
    /// `owner/name` of what this was forked from, when the viewer may
    /// see it. A fork that does not say so is a repository claiming
    /// somebody else's history as its own, which is the one thing a
    /// fork must never do — GitHub prints "forked from …" under the name
    /// for exactly this reason.
    ///
    /// `None` covers three different facts on purpose: not a fork,
    /// upstream deleted and this promoted, or an upstream this viewer
    /// may not see. The third is why it cannot be derived client-side
    /// from an id.
    pub fork_parent: Option<String>,
    /// How many commits are reachable from the default branch's tip, as
    /// of the last time the compaction job counted — `null` until it
    /// has. `commits_tip` names the commit the number was true for, so
    /// a client that knows the current tip can tell "current" from "as
    /// of the last fold" without asking anything else; `commits_exact`
    /// is `false` when the walk stopped at its cap and the true number
    /// is at least this.
    ///
    /// Stored, not computed here, for the reason `stored_bytes` is: a
    /// repository page must not walk history to draw a number, and the
    /// dashboard used to — the log, limit a thousand, clamped to five
    /// hundred first-parent commits, nine seconds on a real mirror.
    pub commits: Option<i64>,
    pub commits_tip: Option<String>,
    pub commits_exact: bool,
    /// Whether the caller may administer this repository: change its
    /// visibility, its default branch, its branch protections and who
    /// may reach it.
    ///
    /// `false` rather than absent when the answer is no — a client that had to distinguish "not
    /// admin" from "not told" would end up guessing, and guessing here
    /// means either hiding a control from somebody who may use it or
    /// showing one that will refuse them.
    pub viewer_admin: bool,
    /// Whether the caller holds a role *here* — an organization
    /// membership, or a per-repo grant. Insights is gated on it: a
    /// repository's traffic belongs to the people who own it.
    ///
    /// Weaker than `viewer_write`, and deliberately: somebody with the
    /// `viewer` role is a member who may not push, and they are exactly
    /// who this is for. `false` rather than absent, for the reason
    /// `viewer_admin` is.
    pub viewer_member: bool,
    /// Whether the caller may push to this repository — and so whether
    /// they may open a change whose commits are already in it.
    ///
    /// Distinct from `viewer_admin`, and the distinction is the whole
    /// point: a member may write here without being able to administer
    /// anything, and an outside contributor may read without either. The
    /// Changes tab needs *write*, because that is the scope
    /// `changes_api::create` actually checks before it refuses a change
    /// with no `source`.
    ///
    /// Asked as the server's own question for the same reason
    /// `viewer_admin` is: a client inferring it from whether some write
    /// endpoint happened to answer ends up either hiding a form from
    /// somebody who may use it, or — as the Changes tab did — offering
    /// one to a signed-out stranger and refusing them after they filled
    /// it in.
    pub viewer_write: bool,
    /// For a mirror: whether a push here reaches the origin, and if not
    /// why — `{ forwarding, blocked, needs_permission, approve_url }`,
    /// see `mirrors::push_view`. `null` for a native repository.
    pub push: Option<serde_json::Value>,
    /// Forks made directly from this repository that the caller may
    /// read. Not the stored total: see [`visible_forks`] for why a
    /// visible count and a stored one are different numbers.
    pub fork_count: usize,
    /// People subscribed to everything that happens here.
    ///
    /// Beside the fork count rather than on `…/watch`, because those are
    /// two different questions: this is public and that one is the
    /// caller's own setting, which refuses a stranger. Putting the count
    /// there would have meant the masthead had no number to draw until
    /// somebody signed in, and then grew one — a control that changes
    /// width after auth resolves is the reflow the identity row exists
    /// to avoid.
    ///
    /// See `watches::watching_count` for why this counts only `all`.
    pub watcher_count: i64,
}

/// Whether this caller may administer this repository.
///
/// Asked as the server's own question rather than inferred by the client
/// from whether some admin-only endpoint happened to answer.
///
/// The client used to probe `GET …/access` and read success as "you may
/// administer this". That probe requires **org-wide** admin, while
/// `patch` and the protections routes accept a **per-repo** admin grant
/// — `authx::require` refines a principal against `repo_grants` when it
/// is given a repo id. So somebody holding `admin` on one repository
/// could legitimately change its visibility and its branch policy, and
/// was shown no settings surface at all, because the only question the
/// client could ask was the wrong one.
///
/// Answering it here makes the client's gate exact instead of
/// conservative, and — more to the point — means there is one opinion
/// about who may administer a repository rather than one per surface.
fn viewer_admin(state: &SharedState, headers: &HeaderMap, org_id: &str, repo_id: &str) -> bool {
    authx::require(&state.db, headers, org_id, Some(repo_id), Scope::OrgAdmin).is_ok()
}

/// Whether this caller holds a role on this repository.
///
/// `Scope::RepoRead` and not something stronger: the weakest role on a
/// repository is one that may read it, so requiring anything more would
/// answer `false` for members this is meant to include.
///
/// The same call the metrics route makes, so the tab the client draws
/// and the answer the server gives cannot come apart — the failure that
/// costs here is a tab offered to somebody the route then refuses, or
/// withheld from somebody it would have answered.
pub(crate) fn viewer_member(
    state: &SharedState,
    headers: &HeaderMap,
    org_id: &str,
    repo_id: &str,
) -> bool {
    authx::require(&state.db, headers, org_id, Some(repo_id), Scope::RepoRead).is_ok()
}

/// Whether this caller may push to this repository.
///
/// `authx::require` is the same seam `rest_repo_auth` reaches for when
/// `changes_api::create` asks whether a change may be opened without a
/// `source` — principal from token or session, refined against the
/// per-repo grants, then `allows`. Asking it here rather than
/// reimplementing the rule is what keeps the form the client draws and
/// the answer the server gives from disagreeing: one opinion about who
/// may write, not one per surface.
pub(crate) fn viewer_write(state: &SharedState, headers: &HeaderMap, repo: &Repo) -> bool {
    authx::require(
        &state.db,
        headers,
        &repo.org_id,
        Some(&repo.id),
        Scope::RepoWrite,
    )
    .is_ok()
}

fn view(state: &SharedState, headers: &HeaderMap, org_name: &str, repo: Repo) -> RepoView {
    let clone_url = format!("{}/{org_name}/{}.git", state.public_url, repo.name);
    let ssh_clone_url = state
        .ssh_public_url
        .as_ref()
        .map(|base| format!("{base}/{org_name}/{}.git", repo.name));
    // Both reads are indexed and neither is allowed to fail the view: a
    // repository page that 500s because a fork count could not be read
    // is a worse answer than one that omits the count.
    let fork_state = stratum_control::forks::info(&state.db, &repo.id)
        .ok()
        .flatten()
        .and_then(|i| i.state.map(|s| s.as_str().to_string()));
    // Resolved to `owner/name` here, not shipped as an id: a client
    // holding an id would have to look it up, and looking it up is
    // exactly the operation that must respect whether the viewer may see
    // the parent at all.
    let fork_parent = stratum_control::forks::parent_of(&state.db, &repo.id)
        .ok()
        .flatten()
        .and_then(|(org_id, parent_id)| {
            let org = registry::org_by_id(&state.db, &org_id).ok().flatten()?;
            let parent = registry::repo_by_id(&state.db, &org_id, &parent_id)
                .ok()
                .flatten()?;
            // An upstream is not named to somebody who could not
            // otherwise know it exists. The fork is still a fork; it
            // simply does not say whose.
            authx::require(
                &state.db,
                headers,
                &org_id,
                Some(&parent_id),
                Scope::RepoRead,
            )
            .is_ok()
            .then(|| format!("{}/{}", org.name, parent.name))
        });
    let fork_count = visible_forks(state, headers, &repo.id)
        .map(|f| f.len())
        .unwrap_or(0);
    // Same rule as the fork count above: an indexed read that is not
    // allowed to fail the view. A masthead missing one number beats a
    // repository page that 500s.
    let watcher_count = stratum_control::watches::watching_count(&state.db, &repo.id).unwrap_or(0);
    let viewer_admin = viewer_admin(state, headers, &repo.org_id, &repo.id);
    let viewer_member = viewer_member(state, headers, &repo.org_id, &repo.id);
    let viewer_write = viewer_write(state, headers, &repo);
    // An indexed point read that is not allowed to fail the view, like
    // the counts above it: a page that 500s over a byte count is worse
    // than one that shows zero until the next sweep.
    let stored_bytes = stratum_control::storage::owner_bytes(
        &state.db,
        stratum_control::storage::OWNER_REPO,
        &repo.id,
    )
    .ok()
    .flatten()
    .map(|(logical, _)| logical)
    .unwrap_or(0);
    // Same rule again: a point read that may not fail the view.
    let counted = stratum_control::commit_counts::get(&state.db, &repo.id)
        .ok()
        .flatten();
    let push =
        (repo.kind == RepoKind::Mirror).then(|| crate::api::mirrors::push_view(state, &repo));
    RepoView {
        commits: counted.as_ref().map(|c| c.count),
        commits_tip: counted.as_ref().map(|c| c.tip.clone()),
        commits_exact: counted.as_ref().map(|c| c.exact).unwrap_or(true),
        viewer_admin,
        viewer_member,
        viewer_write,
        push,
        repo,
        org: org_name.to_string(),
        clone_url,
        ssh_clone_url,
        stored_bytes,
        fork_state,
        fork_parent,
        fork_count,
        watcher_count,
    }
}

pub async fn create(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateRepoBody>,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let principal = match authx::require(&state.db, &headers, &org.id, None, Scope::RepoWrite) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // Creating costs storage and outbound fetches, so it is the line an
    // unproved address does not cross.
    if let Err(r) = authx::require_verified(&state.db, &principal) {
        return r;
    }
    if let Err(r) = crate::api::refuse_public(body.public) {
        return r;
    }
    let actx = AuditCtx::of(&org.id, Some(&principal));
    match create_one(state.clone(), org.id.clone(), actx, body).await {
        Ok(repo) => (
            StatusCode::CREATED,
            Json(view(&state, &headers, &org_name, repo)),
        )
            .into_response(),
        Err(e) if stratum_engine::errclass::is_already_exists(&e) => {
            json_error(StatusCode::CONFLICT, e)
        }
        Err(e) if stratum_engine::errclass::is_invalid_input(&e) => {
            json_error(StatusCode::BAD_REQUEST, e)
        }
        Err(e) => internal(e),
    }
}

async fn create_one(
    state: SharedState,
    org_id: String,
    ctx: AuditCtx,
    body: CreateRepoBody,
) -> Result<Repo, String> {
    let description = registry::clean_description(body.description.as_deref().unwrap_or(""))?;
    let repo = registry::create_repo(
        &state.db,
        &org_id,
        &NewRepo {
            name: &body.name,
            description: description.as_deref(),
            kind: RepoKind::Native,
            default_branch: &body.default_branch,
            origin_url: None,
            origin_provider: None,
            origin_installation: None,
        },
    )?;
    let store_url = state.store_url.clone();
    let prefix = repo.prefix().as_str().to_string();
    let branch = repo.default_branch.clone();
    let init = tokio::task::spawn_blocking(move || {
        let store = ObjectStore::new(&store_url, LatencyModel::None);
        stratum_engine::repoinit::create_empty(&store, &prefix, &branch)
    })
    .await
    .unwrap_or_else(|e| Err(format!("task join: {e}")));
    if let Err(e) = init {
        // Roll the row back so the name is immediately reusable.
        let _ = registry::purge_repo(&state.db, &org_id, &repo.id);
        return Err(format!("storage init failed: {e}"));
    }
    stratum_control::audit::record(&state.db, &ctx, Some(&repo.id), "repo.create", None)?;
    Ok(repo)
}

pub async fn get(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, repo) = match crate::app::repo_or_masked(&state, &headers, &org_name, &repo_name) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = authx::require(
        &state.db,
        &headers,
        &org.id,
        Some(&repo.id),
        Scope::RepoRead,
    ) {
        return r;
    }
    Json(view(&state, &headers, &org_name, repo)).into_response()
}

pub async fn delete(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, repo) = match crate::app::repo_or_masked(&state, &headers, &org_name, &repo_name) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let principal = match authx::require(
        &state.db,
        &headers,
        &org.id,
        Some(&repo.id),
        Scope::RepoWrite,
    ) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match registry::delete_repo(&state.db, &org.id, &repo.id) {
        Ok(true) => {
            // Its bill stops now, not when the sweeper gets to its prefix.
            if let Err(e) = stratum_control::storage::remove(
                &state.db,
                stratum_control::storage::OWNER_REPO,
                &repo.id,
            ) {
                eprintln!("weft: storage row on delete {}: {e}", repo.id);
            }
            // A deleted fork is not a fork of anything, so cut the link
            // and give the parent its count back.
            //
            // This is **not** what keeps the visible count honest — that
            // is the `state = 'active'` filter the list and the count
            // are both read through, and forks_e2e passes with this call
            // removed. What it maintains is the denormalised
            // `repos.fork_count` column, which the promotion and
            // re-pointing jobs read and nothing on the read path does.
            //
            // The epoch references are untouched and must be: the data
            // is still there until the sweeper takes it, and it is still
            // upstream's to protect until then.
            if let Err(e) = stratum_control::forks::detach(&state.db, &repo.id) {
                eprintln!("weft: detach fork {} on delete: {e}", repo.id);
            }
            // Deleting an upstream does not delete what was forked from
            // it. Everything reading this repository's data is promoted
            // onto storage of its own first; only when the last
            // reference is released does this prefix become sweepable.
            //
            // Deliberately not a refusal. GitHub lets you delete a
            // repository that has forks, and so do we — what changes is
            // that the forks survive it.
            crate::workers::promoter::enqueue_dependents(&state, &repo.id);
            // CI does not survive the repository it was building.
            //
            // The row is tombstoned rather than removed, so nothing
            // about the runs themselves would ever notice: the jobs go
            // on saying `running`, the dispatcher goes on claiming the
            // queued ones and launching containers to clone a repository
            // that answers 404, and every one of them holds a slot of
            // the organisation's concurrency until the overdue sweep
            // gets to it — up to a job's whole timeout later, while
            // other repositories in the org queue behind it.
            //
            // Cancelling settles the runs and their jobs in one
            // transaction, which is also what moves the mirrored checks
            // off `queued`, and `stop_jobs` takes down the tasks that
            // were already up and revokes their tokens. A runner that is
            // mid-step learns on its next call, which now answers 410.
            cancel_live_ci(&state, &org.name, &repo.name, &repo.id).await;
            let ctx = AuditCtx::of(&org.id, Some(&principal));
            crate::api::record_or_warn(&state.db, &ctx, Some(&repo.id), "repo.delete", None);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => authx::not_found(),
        Err(e) => internal(e),
    }
}

/// Stop everything CI is doing for a repository that has just been
/// deleted. Best effort throughout: a failure here must not turn a
/// successful deletion into a 500 the caller would retry against a
/// repository that is already gone.
async fn cancel_live_ci(state: &SharedState, org_name: &str, repo_name: &str, repo_id: &str) {
    let runs = match stratum_control::workflows::live_runs_for_repo(&state.db, repo_id) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("weft: live runs on delete of {repo_id}: {e}");
            return;
        }
    };
    for run in &runs {
        let jobs = match stratum_control::workflows::cancel_run(
            &state.db,
            &run.id,
            "the repository was deleted",
        ) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("weft: cancel run {} on delete: {e}", run.id);
                continue;
            }
        };
        // The link on each row is built from the names in hand: the
        // repository row is already tombstoned, so resolving them from
        // its id would come back empty and leave the rows with nothing
        // to point at. The run page outlives the repository — it is
        // where a reader finds out what happened to a build that
        // stopped mid-step.
        let page =
            crate::workflow::mirror::run_page(&state.public_url, org_name, repo_name, &run.id);
        // Every job's check row has to follow, not only the ones that
        // were running: a check left saying `queued` for a job that will
        // never run is what holds a land gate shut.
        if let Ok(all) = stratum_control::workflows::jobs_of(&state.db, &run.id) {
            for j in &all {
                if let Err(e) =
                    crate::workflow::mirror::mirror_job(&state.db, repo_id, run, j, Some(&page))
                {
                    eprintln!("weft: mirror job {} on delete: {e}", j.id);
                }
            }
        }
        crate::workflow::credentials::stop_jobs(state, &jobs, "the repository was deleted").await;
    }
}

/// What a person may change about a repository after making it.
///
/// Both fields are three-state on purpose: absent leaves the value
/// alone, present changes it, and `"description": null` (or an empty
/// string) clears it.
#[derive(Deserialize)]
pub struct PatchRepoBody {
    #[serde(default, deserialize_with = "double_option")]
    pub description: Option<Option<String>>,
    /// Refused when `true` — see [`crate::api::NO_PUBLIC_REPOS`].
    #[serde(default)]
    pub public: Option<bool>,
    /// The branch clones start on and changes land on by default. Repo
    /// policy: the move takes an admin, and the branch must exist.
    #[serde(default)]
    pub default_branch: Option<String>,
    /// The project's own address on the web, for the About rail.
    ///
    /// Double-optional like `description`, and for the same reason:
    /// "leave it alone" and "clear it" are different requests, and
    /// serde collapses them without it. `null` clears; an empty or
    /// whitespace-only string clears too, so there is one way to say
    /// "no homepage" rather than two that store differently.
    #[serde(default, deserialize_with = "double_option")]
    pub homepage: Option<Option<String>>,
    /// The GitHub App installation a mirror forwards pushes through.
    ///
    /// A mirror registered from a pasted public URL has none, and
    /// cannot push to its origin. Attaching one here makes it
    /// push-capable without re-creating it. Org admin, and the org
    /// must have connected the installation — the same 404 mask as
    /// creation, because the id is a bearer of somebody's source.
    #[serde(default)]
    pub installation_id: Option<String>,
}

/// `null` and "not present" are different requests here, and serde
/// collapses them to `None` without this.
fn double_option<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(d).map(Some)
}

pub async fn patch(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<PatchRepoBody>,
) -> Response {
    let (org, repo) = match crate::app::repo_or_masked(&state, &headers, &org_name, &repo_name) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = crate::api::refuse_public(body.public) {
        return r;
    }
    // Moving the default branch and attaching an installation are
    // authority, not preference: both take an admin. A description alone
    // takes write.
    let need = if body.default_branch.is_some() || body.installation_id.is_some() {
        Scope::OrgAdmin
    } else {
        Scope::RepoWrite
    };
    let principal = match authx::require(&state.db, &headers, &org.id, Some(&repo.id), need) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if let Some(inst) = body.installation_id.as_deref() {
        if repo.kind != RepoKind::Mirror || repo.origin_provider.as_deref() != Some("github") {
            return json_error(
                StatusCode::BAD_REQUEST,
                "only a GitHub mirror forwards pushes through an installation",
            );
        }
        if !stratum_control::installations::plausible_id(inst) {
            return json_error(StatusCode::NOT_FOUND, "no such installation");
        }
        match stratum_control::installations::org_may_use(&state.db, &org.id, "github", inst) {
            Ok(true) => {}
            Ok(false) => return json_error(StatusCode::NOT_FOUND, "no such installation"),
            Err(e) => return internal(e),
        }
    }
    if let Some(db_branch) = body.default_branch.as_deref() {
        // A mirror's default branch follows its origin HEAD; the sync
        // worker owns it and a manual edit would be silently undone.
        if repo.kind == RepoKind::Mirror {
            return json_error(
                StatusCode::FORBIDDEN,
                "a mirror's default branch follows its origin",
            );
        }
        if !stratum_control::protections::valid_branch(db_branch) {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!("invalid branch {db_branch:?}"),
            );
        }
        // The branch must exist — a default nobody can clone from is a
        // typo, caught here rather than by the next empty checkout.
        let prefix = repo.prefix().as_str().to_string();
        let branch_ref = format!("refs/heads/{db_branch}");
        let exists = crate::api::reads::with_reader(&state, prefix, move |reader| {
            reader.ref_oid(&branch_ref)
        })
        .await;
        match exists {
            Ok(Some(_)) => {}
            Ok(None) => {
                return json_error(
                    StatusCode::NOT_FOUND,
                    format!("unknown branch {db_branch:?}"),
                )
            }
            Err(e) => return crate::api::reads::err_to_response(e),
        }
    }
    let description = match body.description.as_ref() {
        None => None,
        Some(raw) => match registry::clean_description(raw.as_deref().unwrap_or("")) {
            Ok(d) => Some(d),
            Err(e) => return json_error(StatusCode::BAD_REQUEST, e),
        },
    };
    // Validated before anything is written, like the description above
    // it. A homepage that is refused must not leave a repository whose
    // description changed and whose link did not.
    let homepage = match body.homepage.as_ref() {
        None => None,
        Some(raw) => match registry::clean_homepage(raw.as_deref().unwrap_or("")) {
            Ok(h) => Some(h),
            Err(e) => return json_error(StatusCode::BAD_REQUEST, e),
        },
    };
    let updated = registry::update_repo_meta(
        &state.db,
        &org.id,
        &repo.id,
        description.as_ref().map(|d| d.as_deref()),
        homepage.as_ref().map(|h| h.as_deref()),
    );
    match updated {
        Ok(true) => {}
        // The row was resolved a moment ago, so "no rows" means it was
        // deleted in between: the same answer as never having existed.
        Ok(false) => return authx::masked(&state.db, &headers),
        Err(e) => return internal(e),
    }
    if let Some(inst) = body.installation_id.as_deref() {
        if let Err(e) = registry::set_origin_installation(&state.db, &repo.id, inst) {
            return internal(e);
        }
        let ctx = AuditCtx::of(&org.id, Some(&principal));
        crate::api::record_or_warn(
            &state.db,
            &ctx,
            Some(&repo.id),
            "mirror.installation",
            Some(&serde_json::json!({ "installation_id": inst })),
        );
    }
    if let Some(db_branch) = body.default_branch.as_deref() {
        if let Err(e) = registry::set_default_branch(&state.db, &repo.id, db_branch) {
            return internal(e);
        }
        let ctx = AuditCtx::of(&org.id, Some(&principal));
        crate::api::record_or_warn(
            &state.db,
            &ctx,
            Some(&repo.id),
            "repo.default_branch",
            Some(&serde_json::json!({ "default_branch": db_branch })),
        );
    }
    match registry::repo_by_id(&state.db, &org.id, &repo.id) {
        Ok(Some(r)) => Json(view(&state, &headers, &org_name, r)).into_response(),
        Ok(None) => authx::masked(&state.db, &headers),
        Err(e) => internal(e),
    }
}

#[derive(Serialize)]
pub struct RepoList {
    pub repos: Vec<RepoView>,
    /// Pass back as `?after=` to continue; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after: Option<String>,
}

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
    if let Err(r) = authx::require(&state.db, &headers, &org.id, None, Scope::OrgRead) {
        return r;
    }
    let limit: usize = params
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(100);
    let after = params.get("after").map(String::as_str);
    match registry::list_repos(&state.db, &org.id, after, limit) {
        Ok(repos) => {
            let next_after = (repos.len() == limit.clamp(1, 1000))
                .then(|| repos.last().map(|r| r.id.clone()))
                .flatten();
            let repos = repos
                .into_iter()
                .map(|r| view(&state, &headers, &org_name, r))
                .collect();
            Json(RepoList { repos, next_after }).into_response()
        }
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct BatchCreateBody {
    pub repos: Vec<CreateRepoBody>,
}

#[derive(Serialize)]
pub struct BatchResult {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Batch create, ≤1000 per call (R1). Rows land in one pass; storage init
/// fans out over blocking threads.
pub async fn batch_create(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<BatchCreateBody>,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let principal = match authx::require(&state.db, &headers, &org.id, None, Scope::RepoWrite) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // Creating costs storage and outbound fetches, so it is the line an
    // unproved address does not cross.
    if let Err(r) = authx::require_verified(&state.db, &principal) {
        return r;
    }
    if body.repos.len() > 1000 {
        return json_error(StatusCode::BAD_REQUEST, "batch limited to 1000 repos");
    }
    // Refused as a whole, before anything is created: one repository in
    // a thousand asking to be public is a script written for somewhere
    // else, and half a batch is harder to clean up than none.
    if let Some(r) = body
        .repos
        .iter()
        .find_map(|r| crate::api::refuse_public(r.public).err())
    {
        return r;
    }
    // Bounded fan-out: 32 creates in flight, results in request order.
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(32));
    let mut set = tokio::task::JoinSet::new();
    for (i, item) in body.repos.into_iter().enumerate() {
        let state = state.clone();
        let org_id = org.id.clone();
        let actx = AuditCtx::of(&org_id, Some(&principal));
        let sem = sem.clone();
        set.spawn(async move {
            let _p = sem.acquire_owned().await;
            let name = item.name.clone();
            (i, name, create_one(state, org_id, actx, item).await)
        });
    }
    let mut results: Vec<Option<BatchResult>> = Vec::new();
    while let Some(joined) = set.join_next().await {
        let (i, name, out) = match joined {
            Ok(x) => x,
            Err(e) => {
                return internal(format!("batch task: {e}"));
            }
        };
        if results.len() <= i {
            results.resize_with(i + 1, || None);
        }
        results[i] = Some(match out {
            Ok(r) => BatchResult {
                name,
                id: Some(r.id),
                ok: true,
                error: None,
            },
            Err(e) => BatchResult {
                name,
                id: None,
                ok: false,
                error: Some(e),
            },
        });
    }
    let results: Vec<BatchResult> = results.into_iter().flatten().collect();
    Json(serde_json::json!({ "results": results })).into_response()
}

#[derive(Deserialize)]
pub struct BatchDeleteBody {
    pub names: Vec<String>,
}

pub async fn batch_delete(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<BatchDeleteBody>,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let principal = match authx::require(&state.db, &headers, &org.id, None, Scope::RepoWrite) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if body.names.len() > 1000 {
        return json_error(StatusCode::BAD_REQUEST, "batch limited to 1000 repos");
    }
    let ctx = AuditCtx::of(&org.id, Some(&principal));
    let mut results = Vec::with_capacity(body.names.len());
    for name in &body.names {
        let out = match registry::repo_by_name(&state.db, &org.id, name) {
            Ok(Some(repo)) => match registry::delete_repo(&state.db, &org.id, &repo.id) {
                Ok(true) => {
                    if let Err(e) = stratum_control::storage::remove(
                        &state.db,
                        stratum_control::storage::OWNER_REPO,
                        &repo.id,
                    ) {
                        eprintln!("weft: storage row on delete {}: {e}", repo.id);
                    }
                    crate::api::record_or_warn(
                        &state.db,
                        &ctx,
                        Some(&repo.id),
                        "repo.delete",
                        None,
                    );
                    BatchResult {
                        name: name.clone(),
                        id: Some(repo.id),
                        ok: true,
                        error: None,
                    }
                }
                Ok(false) => not_found_result(name),
                Err(e) => err_result(name, e),
            },
            Ok(None) => not_found_result(name),
            Err(e) => err_result(name, e),
        };
        results.push(out);
    }
    Json(serde_json::json!({ "results": results })).into_response()
}

fn not_found_result(name: &str) -> BatchResult {
    BatchResult {
        name: name.to_string(),
        id: None,
        ok: false,
        error: Some("not found".into()),
    }
}

fn err_result(name: &str, e: String) -> BatchResult {
    BatchResult {
        name: name.to_string(),
        id: None,
        ok: false,
        error: Some(e),
    }
}

/// Where a fork goes. Both fields optional: the common case is a button
/// with nothing to fill in.
#[derive(Deserialize, Default)]
pub struct ForkRepoBody {
    /// Namespace to fork into. Defaults to the caller's own.
    #[serde(default)]
    pub org: Option<String>,
    /// Defaults to the source repository's name.
    #[serde(default)]
    pub name: Option<String>,
}

/// Fork a repository.
///
/// **202, not 201.** The row exists synchronously; the storage pointers
/// are written by a job a moment later. Claiming `Created` would assert
/// something not yet true, and a caller who cloned immediately would get
/// an empty repository and no explanation. `fork_state` is the honest
/// version: `pending`, then `ready` — or `failed`, because a fork that
/// says it is broken beats one that silently serves nothing.
pub async fn fork(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<Json<ForkRepoBody>>,
) -> Response {
    let (src_org, src) = match crate::app::repo_or_masked(&state, &headers, &org_name, &repo_name) {
        Ok(x) => x,
        Err(r) => return r,
    };
    // You may fork what you may read.
    if let Err(r) = authx::require(
        &state.db,
        &headers,
        &src_org.id,
        Some(&src.id),
        Scope::RepoRead,
    ) {
        return r;
    }
    let body = body.map(|Json(b)| b).unwrap_or_default();

    // Who is forking. Resolved as a *person*, deliberately not from the
    // repository principal: that answers "what authority has this caller
    // in this org", and a person with a per-repo grant is not a member —
    // which means "not a member", not "not a person". Reading identity
    // off it would refuse a caller forking exists to serve.
    let user_id = match crate::api::caller_person(&state, &headers) {
        Ok(Some(u)) => u,
        Ok(None) => {
            return json_error(StatusCode::UNAUTHORIZED, "forking takes a signed-in person")
        }
        Err(r) => return r,
    };

    // Target namespace: named, or the forker's own.
    let target = match body.org.as_deref() {
        Some(name) => match crate::app::org_or_404(&state, name) {
            Ok(o) => o,
            Err(r) => return r,
        },
        None => match stratum_control::forks::personal_namespace(&state.db, &user_id) {
            Ok(Some(id)) => match registry::org_by_id(&state.db, &id) {
                Ok(Some(o)) => o,
                Ok(None) => return internal("personal namespace vanished".to_string()),
                Err(e) => return internal(e),
            },
            Ok(None) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "no personal namespace to fork into",
                )
            }
            Err(e) => return internal(e),
        },
    };

    // Creating in the target costs storage and is gated exactly as an
    // ordinary create is — the source being readable says nothing about
    // where the caller may put things.
    let principal = match authx::require(&state.db, &headers, &target.id, None, Scope::RepoWrite) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if let Err(r) = authx::require_verified(&state.db, &principal) {
        return r;
    }

    let name = body.name.clone().unwrap_or_else(|| src.name.clone());
    // Is the name already taken in the target, and by what? Decided
    // before anything is made, because the answer is not always a
    // refusal. The commonest way to arrive with a taken name is
    // pressing Fork on something you forked last week — found by doing
    // exactly that in the app: the answer was `repo "widget" already
    // exists` and the page stayed on upstream, which reads as a failure
    // and leaves the person to hunt for their own fork by hand. GitHub
    // takes you to it. So: taken by *your fork of this very
    // repository*, that fork is the answer — 200, not 202, because
    // nothing was made. Taken by anything else, it is a real
    // collision, and the refusal says which repository is in the way
    // and that a name gets past it.
    match registry::repo_by_name(&state.db, &target.id, &name) {
        Ok(Some(existing)) => {
            return match stratum_control::forks::parent_of(&state.db, &existing.id) {
                Ok(Some((_, parent))) if parent == src.id => (
                    StatusCode::OK,
                    Json(view(&state, &headers, &target.name, existing)),
                )
                    .into_response(),
                Ok(_) => json_error(
                    StatusCode::CONFLICT,
                    format!(
                        "{}/{name} already exists and is not a fork of {}/{}; \
                         fork it under another name",
                        target.name, src_org.name, src.name
                    ),
                ),
                Err(e) => internal(e),
            };
        }
        Ok(None) => {}
        Err(e) => return internal(e),
    }
    let created = registry::create_repo(
        &state.db,
        &target.id,
        &NewRepo {
            name: &name,
            description: src.description.as_deref(),
            kind: RepoKind::Native,
            default_branch: &src.default_branch,
            origin_url: None,
            origin_provider: None,
            origin_installation: None,
        },
    );
    // NOTE: no `repoinit::create_empty` here, and that is load-bearing.
    // An empty repository gets a manifest written at creation; a fork
    // must not, because the fork job publishes upstream's manifest with
    // a create-only conditional PUT. An empty one already sitting there
    // would make that PUT conflict, and the job would read the conflict
    // as "somebody already published this" and report success over a
    // fork that is permanently empty.
    let repo = match created {
        Ok(r) => r,
        Err(e) if stratum_engine::errclass::is_already_exists(&e) => {
            // The name was free at the lookup above and is taken now:
            // another create raced this one. The refusal is the true
            // one for the request that was made.
            return json_error(StatusCode::CONFLICT, e);
        }
        Err(e) if stratum_engine::errclass::is_invalid_input(&e) => {
            return json_error(StatusCode::BAD_REQUEST, e)
        }
        Err(e) => return internal(e),
    };

    if let Err(e) = stratum_control::forks::attach(&state.db, &repo.id, &src.id) {
        // The row exists but is not a fork of anything, and its storage
        // was never initialised — so roll it back rather than leave a
        // repository nobody can use holding a name somebody wants.
        let _ = registry::purge_repo(&state.db, &target.id, &repo.id);
        return internal(e);
    }
    let ctx = AuditCtx::of(&target.id, Some(&principal));
    crate::api::record_or_warn(
        &state.db,
        &ctx,
        Some(&repo.id),
        "repo.fork",
        Some(&serde_json::json!({ "from": src.id })),
    );
    if let Err(e) = crate::workers::forker::enqueue(&state, &target.id, &repo.id) {
        return internal(e);
    }
    (
        StatusCode::ACCEPTED,
        Json(view(&state, &headers, &target.name, repo)),
    )
        .into_response()
}

#[derive(Serialize)]
pub struct ForkList {
    pub forks: Vec<ForkEntry>,
    /// How many forks the **caller** can see, which is the length of the
    /// list beside it. Never the stored total: a repository's forks can
    /// be made private after the fact, and publishing a larger number
    /// than the list would say precisely how many private ones exist.
    pub count: usize,
}

#[derive(Serialize)]
pub struct ForkEntry {
    pub org: String,
    pub name: String,
}

/// The forks of this repository the caller may read.
///
/// A fork lives in its owner's namespace, and its name is theirs: the
/// stored `fork_count` counts every fork, but a caller is told only about
/// the ones they could open. One authorization check per fork, through
/// the same seam every repository read uses — a repository's forks are
/// few, and a second opinion about who may read one is how two
/// surfaces come to disagree.
pub(crate) fn visible_forks(
    state: &SharedState,
    headers: &HeaderMap,
    repo_id: &str,
) -> Result<Vec<stratum_control::forks::ForkRow>, String> {
    Ok(stratum_control::forks::forks_of(&state.db, repo_id)?
        .into_iter()
        .filter(|f| {
            authx::require(&state.db, headers, &f.org_id, Some(&f.id), Scope::RepoRead).is_ok()
        })
        .collect())
}

/// Repositories forked directly from this one that the caller may read.
pub async fn list_forks(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, repo) = match crate::app::repo_or_masked(&state, &headers, &org_name, &repo_name) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = authx::require(
        &state.db,
        &headers,
        &org.id,
        Some(&repo.id),
        Scope::RepoRead,
    ) {
        return r;
    }
    let rows = match visible_forks(&state, &headers, &repo.id) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let mut forks = Vec::with_capacity(rows.len());
    for r in rows {
        let org_name = match registry::org_by_id(&state.db, &r.org_id) {
            Ok(Some(o)) => o.name,
            Ok(None) => continue,
            Err(e) => return internal(e),
        };
        forks.push(ForkEntry {
            org: org_name,
            name: r.name,
        });
    }
    let count = forks.len();
    Json(ForkList { forks, count }).into_response()
}
