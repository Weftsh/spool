//! Mirror registration + manual sync API (M1).

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::Scope;
use stratum_control::installations;
use stratum_control::registry::{self, NewRepo, RepoKind};

#[derive(Deserialize)]
pub struct CreateMirrorBody {
    pub name: String,
    /// "github" (origin = owner/name, installation_id required for private
    /// origins) or "generic" (origin = any git-fetchable URL).
    #[serde(default = "default_provider")]
    pub provider: String,
    /// GitHub full name ("owner/name") or a git URL, per provider.
    pub origin: String,
    #[serde(default)]
    pub installation_id: Option<String>,
    /// Refused when true: see [`crate::api::NO_PUBLIC_REPOS`].
    #[serde(default)]
    pub public: Option<bool>,
    #[serde(default)]
    pub description: Option<String>,
}

fn default_provider() -> String {
    "github".into()
}

/// The same bound the standalone probe endpoint uses. A creation that
/// hangs on a slow origin is a form that hangs.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub async fn create(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateMirrorBody>,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let principal = match authx::require(&state.db, &headers, &org.id, None, Scope::RepoWrite) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if let Err(r) = crate::api::refuse_public(body.public) {
        return r;
    }
    if state.sync.provider_by_name(&body.provider).is_none() {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!(
                "origin provider {:?} not configured on this server",
                body.provider
            ),
        );
    }
    // An installation id in this body is what the sync worker will trade
    // for a token, and the App mints one for any installation it is
    // asked about — so this id is a bearer token to somebody's private
    // source until the org is shown to have connected it. The picker
    // route asked `org_may_use`; this route, the one that actually
    // spends the id, did not, and any org could mirror through any
    // installation of a public App by typing its number. A stranger's
    // and a made-up one get the picker's answer: 404, which says
    // nothing about whether it exists.
    if let Some(id) = body.installation_id.as_deref() {
        if !installations::plausible_id(id) {
            return json_error(StatusCode::NOT_FOUND, "no such installation");
        }
        match installations::org_may_use(&state.db, &org.id, &body.provider, id) {
            Ok(true) => {}
            Ok(false) => return json_error(StatusCode::NOT_FOUND, "no such installation"),
            Err(e) => return internal(e),
        }
    }
    // A GitHub origin is stored as `owner/name`, whatever was typed: the
    // sync builds its fetch URL from this field, and a pasted
    // `github.com/owner/name` used to be stored verbatim and fetched as
    // `https://github.com/github.com/owner/name.git`.
    let origin = if body.provider == "github" {
        match crate::mirror::probe::github_full_name(&body.origin) {
            Ok(full) => full,
            Err(e) => return json_error(StatusCode::UNPROCESSABLE_ENTITY, e),
        }
    } else {
        body.origin.clone()
    };
    // Checked before the probe: a description we are going to refuse
    // should not cost an outbound fetch first, and the person should
    // hear about the field they got wrong rather than about the origin.
    let description = match registry::clean_description(body.description.as_deref().unwrap_or("")) {
        Ok(d) => d,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, e),
    };
    // Check the origin before accepting it, while the person is still
    // looking at the field. Creation used to answer 202 for anything,
    // and a typo surfaced minutes later as `sync_error` on a repo that
    // looked broken.
    //
    // Skipped in two cases, both deliberate. With an installation id the
    // caller already has credentials, and a private origin refusing an
    // anonymous probe is the expected answer, not a reason to refuse
    // creation. And a local origin — `file://`, or a server whose git
    // base is one — has no network to ask.
    if body.installation_id.is_none() && !state.sync.origin_is_local(&body.provider, &origin) {
        let origin = origin.clone();
        let found = tokio::task::spawn_blocking(move || {
            crate::mirror::probe::probe(&origin, PROBE_TIMEOUT)
        })
        .await
        .unwrap_or_else(|e| crate::mirror::probe::Probe::refused(e.to_string()));
        if !found.reachable {
            // 422, not 400: the request was well-formed and we
            // understood it — the origin is what did not check out. The
            // probe's own answer rides along so the screen can offer the
            // GitHub install when `private` is what happened.
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": found
                        .reason
                        .clone()
                        .unwrap_or_else(|| "that origin is not reachable".into()),
                    "probe": found,
                })),
            )
                .into_response();
        }
    }
    let repo = match registry::create_repo(
        &state.db,
        &org.id,
        &NewRepo {
            name: &body.name,
            description: description.as_deref(),
            kind: RepoKind::Mirror,
            default_branch: "main", // corrected from origin HEAD at first sync
            origin_url: Some(&origin),
            origin_provider: Some(&body.provider),
            origin_installation: body.installation_id.as_deref(),
        },
    ) {
        Ok(r) => r,
        Err(e) if stratum_engine::errclass::is_already_exists(&e) => {
            return json_error(StatusCode::CONFLICT, e)
        }
        Err(e) if stratum_engine::errclass::is_invalid_input(&e) => {
            return json_error(StatusCode::BAD_REQUEST, e)
        }
        Err(e) => return internal(e),
    };
    let ctx = AuditCtx::of(&org.id, Some(&principal));
    crate::api::record_or_warn(
        &state.db,
        &ctx,
        Some(&repo.id),
        "mirror.create",
        Some(&serde_json::json!({ "origin": origin, "provider": body.provider })),
    );
    // Initial ingest runs in the background; GET the repo (last_sync_at /
    // sync_error) or POST …/sync to follow progress.
    {
        let state = state.clone();
        let repo = repo.clone();
        tokio::spawn(async move {
            if let Err(e) = state.sync.sync(&repo).await {
                eprintln!("weft: initial sync of {} failed: {e}", repo.id);
            }
        });
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "repo": repo,
            "clone_url": format!("{}/{org_name}/{}.git", state.public_url, repo.name),
            "status": "initial sync started",
        })),
    )
        .into_response()
}

/// Where a mirror has got to.
///
/// Creation answers 202 and the first sync runs in the background, so
/// there has to be something to *watch*. The repo record already carries
/// everything this reports; what was missing was a shape a screen could
/// poll without reading four nullable columns and inferring a state
/// machine from them.
///
/// The states, and what each one means to the person waiting:
///
/// * `syncing` — accepted, nothing ingested yet. The first sync of a
///   large origin sits here for a while, which is exactly when somebody
///   is most likely to think it is broken.
/// * `ready` — at least one sync finished and the last one worked.
/// * `failed` — the last attempt failed, and `error` says why. A mirror
///   that has synced before and failed since is still `failed`: it is
///   serving stale content and somebody should know.
pub async fn sync_status(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, repo) = match crate::app::repo_or_masked(&state, &headers, &org_name, &repo_name) {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Reading progress is a read. A viewer who can see the repository
    // can see whether it is working.
    if let Err(r) = authx::require(
        &state.db,
        &headers,
        &org.id,
        Some(&repo.id),
        Scope::RepoRead,
    ) {
        return r;
    }
    if repo.kind != RepoKind::Mirror {
        return json_error(StatusCode::BAD_REQUEST, "not a mirror");
    }
    let phase = match (repo.sync_error.as_deref(), repo.last_sync_at) {
        (Some(_), _) => "failed",
        (None, Some(_)) => "ready",
        (None, None) => "syncing",
    };
    Json(serde_json::json!({
        "state": phase,
        "origin": repo.origin_url,
        "provider": repo.origin_provider,
        "last_sync_at": repo.last_sync_at,
        "commit": repo.last_synced_commit,
        "error": repo.sync_error,
        "clone_url": format!("{}/{org_name}/{}.git", state.public_url, repo.name),
        "push": push_view(&state, &repo),
    }))
    .into_response()
}

/// Whether a push to this mirror reaches its origin, and if not, why
/// and what would fix it. Said on the repository page and the sync
/// status so a person learns it before the first refused push, not
/// from it.
///
/// `forwarding` is the answer; `blocked` is the sentence a push would
/// be refused with; `needs_permission` and `approve_url` are the case
/// a person can fix on GitHub: an installation that predates
/// `Contents: write`. An installation GitHub cannot be asked about
/// right now reads as forwarding — the push itself will say otherwise
/// if it must, and a page that says "cannot push" on the strength of
/// not having checked would be wrong far more often than right.
pub(crate) fn push_view(
    state: &SharedState,
    repo: &stratum_control::registry::Repo,
) -> serde_json::Value {
    use crate::mirror::forward::CONTENTS_WRITE_DENIED;
    if let Err(refusal) = state.sync.push_credential(repo) {
        let blocked = refusal.sentence().unwrap_or_default().to_string();
        return serde_json::json!({
            "forwarding": false,
            "blocked": blocked,
            "needs_permission": false,
            "approve_url": null,
        });
    }
    if let (Some(app), Some(inst), Some("github")) = (
        state.sync.github_app(),
        repo.origin_installation.as_deref(),
        repo.origin_provider.as_deref(),
    ) {
        if let Ok(Some(detail)) = app.installation(inst) {
            if !detail.contents_write {
                return serde_json::json!({
                    "forwarding": false,
                    "blocked": CONTENTS_WRITE_DENIED,
                    "needs_permission": true,
                    "approve_url": app.approve_url(&detail),
                });
            }
        }
    }
    serde_json::json!({
        "forwarding": true,
        "blocked": null,
        "needs_permission": false,
        "approve_url": null,
    })
}

/// Synchronous sync trigger (ops/tests): waits for the sync to finish and
/// reports the outcome.
pub async fn sync_now(
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
    if repo.kind != RepoKind::Mirror {
        return json_error(StatusCode::BAD_REQUEST, "not a mirror");
    }
    match state.sync.sync(&repo).await {
        Ok(outcome) => {
            let fresh = registry::repo_by_id(&state.db, &org.id, &repo.id)
                .ok()
                .flatten();
            crate::api::record_or_warn(
                &state.db,
                &AuditCtx::of(&org.id, Some(&principal)),
                Some(&repo.id),
                "mirror.sync",
                Some(&serde_json::json!({
                    "outcome": format!("{outcome:?}"),
                    "commit": fresh.as_ref().and_then(|r| r.last_synced_commit.clone()),
                })),
            );
            Json(serde_json::json!({
                "outcome": format!("{outcome:?}"),
                "last_synced_commit": fresh.as_ref().and_then(|r| r.last_synced_commit.clone()),
                "sync_error": fresh.as_ref().and_then(|r| r.sync_error.clone()),
            }))
            .into_response()
        }
        Err(e) => json_error(StatusCode::BAD_GATEWAY, e),
    }
}
