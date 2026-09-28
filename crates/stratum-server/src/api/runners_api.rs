//! Self-hosted runners: what an organisation allows, which machines it
//! has, and the two calls a machine makes for itself.
//!
//! Two audiences on one page, and they authenticate completely
//! differently, which is why they are one module rather than two:
//!
//! * **`/v1/orgs/:org/…`** is a person in the dashboard. Ordinary org
//!   membership through [`authx::require`], reads at `OrgRead`, writes
//!   at `OrgAdmin`, every mutation audited, and a namespace the caller
//!   is not in answers 404 like every other org route.
//! * **`/v1/runners/register`** and **`/v1/runners/claim`** are the
//!   machine. Neither is a session and neither is an API token: the
//!   first carries a one-hour registration secret, the second the
//!   runner's own credential, and both are resolved by
//!   [`stratum_control::runners`] rather than by `auth::verify`. They
//!   are deliberately **not** under `/v1/orgs/:org/`, for the reason the
//!   job routes are not: a runner should not have to know, or be able to
//!   assert, which organisation it belongs to — its credential says so.
//!
//! ## The claim is a long poll, and that is the whole scaling story
//!
//! A registered machine has nothing listening on it, so work reaches it
//! only by its asking. Asking every second is one query per runner per
//! second forever, almost all of which answers "nothing"; asking every
//! thirty is half a minute of latency on every build. So the server
//! holds the request open for `STRATUM_RUNNER_CLAIM_WAIT_MS` and
//! re-runs the claim every 500 ms inside it, and a 204 at the end is the
//! runner's cue to ask again. Past a few hundred runners this should
//! become a Postgres `LISTEN/NOTIFY` wait rather than a poll; the wire
//! contract does not change when it does, which is why it is shaped this
//! way now.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::{self, Mint, Scope};
use stratum_control::ids::now_ms;
use stratum_control::runners::{self, Group, Policy, RunnerView};
use stratum_control::workflows::{self, RunnerRoute};

/// How often the held-open claim re-runs its query.
const CLAIM_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// Turn a control-plane refusal into the status it means.
///
/// The mapping lives here and not in each handler because the three
/// variants exist precisely so that a handler never has to decide by
/// matching prose — 422 for a value outside the enumeration, 409 for a
/// name already taken, 500 for a database that said no.
fn refused(e: runners::Error) -> Response {
    match e {
        runners::Error::Invalid(m) => json_error(StatusCode::UNPROCESSABLE_ENTITY, m),
        runners::Error::Conflict(m) => json_error(StatusCode::CONFLICT, m),
        runners::Error::Db(m) => internal(m),
    }
}

/// The bearer token, or nothing. A copy of `runner_api`'s, because the
/// two surfaces take different *kinds* of bearer and sharing the reader
/// would be the first step toward sharing the verifier.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

// ---------------------------------------------------------------------
// Shapes
// ---------------------------------------------------------------------

fn policy_json(p: &Policy) -> serde_json::Value {
    serde_json::json!({
        "self_hosted": p.self_hosted,
        "self_hosted_repos": p.self_hosted_repos,
    })
}

fn group_json(g: &Group) -> serde_json::Value {
    serde_json::json!({
        "id": g.id,
        "name": g.name,
        "repo_access": g.repo_access,
        "is_default": g.is_default,
        "repos": g.repos,
        "runners": g.runners,
        "created_at": g.created_at,
        "updated_at": g.updated_at,
    })
}

fn runner_json(v: &RunnerView) -> serde_json::Value {
    serde_json::json!({
        "id": v.runner.id,
        "name": v.runner.name,
        "labels": v.runner.labels,
        "os": v.runner.os,
        "arch": v.runner.arch,
        "version": v.runner.version,
        "ephemeral": v.runner.ephemeral,
        "group": { "id": v.runner.group_id, "name": v.runner.group_name },
        "state": v.state,
        "last_seen_at": v.runner.last_seen_at,
        "created_at": v.runner.created_at,
        // What it is running right now, or null. `job_id` is the
        // workflow job's **row** id — the thing a link into the run
        // needs — `key` is the cell name a person reads, and `repo` is
        // the repository's *name*, because a run is addressed by
        // `owner/repo` and the page cannot build that from an id.
        "job": v.job.as_ref().map(|j| serde_json::json!({
            "run_id": j.run_id, "job_id": j.job_id, "key": j.key, "repo": j.repo,
        })),
    })
}

#[derive(Deserialize, Default)]
pub struct PolicyBody {
    #[serde(default)]
    pub self_hosted: Option<String>,
    /// Repository **names**. Absent leaves the list alone; `[]` clears it.
    #[serde(default)]
    pub self_hosted_repos: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
pub struct GroupBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub repo_access: Option<String>,
    #[serde(default)]
    pub repos: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
pub struct TokenBody {
    #[serde(default)]
    pub group: Option<String>,
}

#[derive(Deserialize)]
pub struct RegisterBody {
    pub name: String,
    #[serde(default)]
    pub labels: Vec<String>,
    pub os: String,
    pub arch: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub ephemeral: bool,
}

// ---------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------

pub async fn get_policy(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = authx::require(&state.db, &headers, &org.id, None, Scope::OrgRead) {
        return r;
    }
    match runners::policy(&state.db, &org.id) {
        Ok(p) => Json(policy_json(&p)).into_response(),
        Err(e) => refused(e),
    }
}

pub async fn patch_policy(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<PolicyBody>,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match runners::set_policy(
        &state.db,
        &org.id,
        body.self_hosted.as_deref(),
        body.self_hosted_repos.as_deref(),
        &actx,
    ) {
        Ok(p) => Json(policy_json(&p)).into_response(),
        Err(e) => refused(e),
    }
}

// ---------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------

pub async fn list_groups(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = authx::require(&state.db, &headers, &org.id, None, Scope::OrgRead) {
        return r;
    }
    match runners::list_groups(&state.db, &org.id) {
        Ok(gs) => Json(serde_json::json!({
            "groups": gs.iter().map(group_json).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(e) => refused(e),
    }
}

pub async fn create_group(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<GroupBody>,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some(name) = body.name.as_deref() else {
        return json_error(StatusCode::UNPROCESSABLE_ENTITY, "name is required");
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match runners::create_group(
        &state.db,
        &org.id,
        name,
        body.repo_access.as_deref(),
        body.repos.as_deref(),
        &actx,
    ) {
        Ok(g) => (StatusCode::CREATED, Json(group_json(&g))).into_response(),
        Err(e) => refused(e),
    }
}

pub async fn patch_group(
    State(state): State<SharedState>,
    Path((org_name, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<GroupBody>,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match runners::update_group(
        &state.db,
        &org.id,
        &id,
        body.name.as_deref(),
        body.repo_access.as_deref(),
        body.repos.as_deref(),
        &actx,
    ) {
        Ok(Some(g)) => Json(group_json(&g)).into_response(),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "no such runner group"),
        Err(e) => refused(e),
    }
}

pub async fn delete_group(
    State(state): State<SharedState>,
    Path((org_name, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match runners::delete_group(&state.db, &org.id, &id, &actx) {
        Ok(Some(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "no such runner group"),
        Err(e) => refused(e),
    }
}

// ---------------------------------------------------------------------
// Runners
// ---------------------------------------------------------------------

pub async fn list(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = authx::require(&state.db, &headers, &org.id, None, Scope::OrgRead) {
        return r;
    }
    match runners::list(&state.db, &org.id, now_ms()) {
        Ok(rs) => Json(serde_json::json!({
            "runners": rs.iter().map(runner_json).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(e) => refused(e),
    }
}

/// Remove a runner, and fail whatever it was in the middle of.
///
/// **Failed, not re-queued**, and that is a choice rather than an
/// oversight: an operator who removes a runner has decided the machine
/// should stop, and handing its work straight to the next machine in the
/// group would be the opposite of what they asked for. The build shows a
/// red check saying exactly what happened, which is something a person
/// can act on; a silent re-run somewhere else is not.
pub async fn remove(
    State(state): State<SharedState>,
    Path((org_name, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    let removed = match runners::remove(&state.db, &org.id, &id, &actx) {
        Ok(Some(r)) => r,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "no such runner"),
        Err(e) => return refused(e),
    };
    if let Some(job_id) = removed.running_job {
        match workflows::job(&state.db, &job_id) {
            Ok(Some(job)) => {
                crate::workers::runner::fail(&state, &job, runners::REMOVED_MID_JOB).await;
            }
            Ok(None) => {}
            Err(e) => eprintln!("weft: read job {job_id} for removed runner: {e}"),
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

pub async fn registration_token(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    body: Option<Json<TokenBody>>,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let group = body.and_then(|Json(b)| b.group);
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match runners::mint_registration_token(&state.db, &org.id, group.as_deref(), &actx) {
        Ok(t) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "token": t.token,
                "expires_at": t.expires_at,
                "group": t.group,
                // The command, spelled out, because the alternative is a
                // reader assembling it from three fields and getting the
                // URL wrong — and a wrong `--url` fails at registration
                // with a network error that says nothing about which
                // part was wrong.
                "command": format!(
                    "weft-runner register --url {} --token {}",
                    state.public_url, t.token
                ),
            })),
        )
            .into_response(),
        Err(e) => refused(e),
    }
}

// ---------------------------------------------------------------------
// The machine's own two calls
// ---------------------------------------------------------------------

/// Exchange a registration token for the runner's own credential.
///
/// Every way the token can be wrong — malformed, unknown, already spent,
/// expired — is the same 401, so it cannot be used to learn which
/// organisations or which groups exist. Registering under a name that is
/// already live **replaces** that runner: the old row is tombstoned in
/// the same transaction, its credential dies, and this is how a
/// credential is rotated and how a reimaged machine comes back without
/// somebody having to remove it by hand first.
pub async fn register(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<RegisterBody>,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "a runner registration token is required",
        );
    };
    let dead = || {
        json_error(
            StatusCode::UNAUTHORIZED,
            "that registration token is unknown, already used or expired",
        )
    };
    let (org_id, group_id) = match runners::consume_registration_token(&state.db, token) {
        Ok(Some(x)) => x,
        Ok(None) => return dead(),
        Err(e) => return refused(e),
    };
    let version = if body.version.is_empty() {
        "unknown"
    } else {
        &body.version
    };
    let registered = match runners::register(
        &state.db,
        &org_id,
        &group_id,
        &body.name,
        &body.labels,
        &body.os,
        &body.arch,
        version,
        body.ephemeral,
    ) {
        Ok(r) => r,
        Err(e) => return refused(e),
    };
    let org_name = crate::app::org_name_of(&state, &org_id).unwrap_or_default();
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "runner_id": registered.runner.id,
            // The only time this exists. It is not recoverable, and
            // re-registering is the only way to get another.
            "credential": registered.credential,
            "org": org_name,
            "group": registered.runner.group_name,
            "labels": registered.runner.labels,
        })),
    )
        .into_response()
}

/// Ask for work, and wait a while for some.
///
/// The four answers are the whole protocol, and the runner branches on
/// the status rather than on a body:
///
/// * **200** — a job, its per-job token, and the URL to report on. From
///   here the runner uses exactly the five calls a hosted runner uses,
///   with exactly the same credential shape, which is why none of that
///   surface changed for this feature.
/// * **204** — nothing after the wait. Ask again.
/// * **401** — this credential is dead. The runner prints that it has
///   been removed and exits, because there is no way for it to become
///   live again without registering.
/// * **409** — this runner already holds a running job. A runner runs
///   one job at a time, and handing it a second would produce work
///   nobody would ever report on.
pub async fn claim(State(state): State<SharedState>, headers: HeaderMap, _body: Bytes) -> Response {
    let Some(credential) = bearer(&headers) else {
        return json_error(StatusCode::UNAUTHORIZED, "a runner credential is required");
    };
    let runner = match runners::authenticate(&state.db, credential) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return json_error(
                StatusCode::UNAUTHORIZED,
                "this runner has been removed; register it again",
            )
        }
        Err(e) => return refused(e),
    };
    // Before anything else, and again on every pass of the wait: a
    // machine that is polling is a machine that is alive, and the list's
    // `online` reading is only as good as this write.
    if let Err(e) = runners::touch(&state.db, &runner.id, now_ms()) {
        eprintln!("weft: touch runner {}: {e}", runner.id);
    }
    match workflows::running_job_for_runner(&state.db, &runner.id) {
        Ok(Some(_)) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "busy"})),
            )
                .into_response()
        }
        Ok(None) => {}
        Err(e) => return internal(e),
    }
    let group = match runners::group(&state.db, &runner.org_id, &runner.group_id) {
        Ok(Some(g)) => g,
        // Its group was deleted between authenticating and here.
        // `delete_group` moves runners to the default one, so this is a
        // race rather than a state, and the honest answer is "nothing
        // for you this time" — the next poll resolves the new group.
        Ok(None) => return StatusCode::NO_CONTENT.into_response(),
        Err(e) => return refused(e),
    };
    let route = RunnerRoute {
        runner_id: &runner.id,
        org_id: &runner.org_id,
        group_id: &group.id,
        labels: &runner.labels,
        all_repos: group.repo_access == "all",
    };

    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(state.runner_claim_wait_ms as u64);
    loop {
        match take_one(&state, &runner.id, &route).await {
            Taken::Job(body) => return Json(body).into_response(),
            // A job was claimed and then given up on — over its attempt
            // cap, or cancelled between the claim and the token. Try
            // again immediately rather than sleeping: there may be
            // another job behind it, and the runner is waiting.
            Taken::Again => continue,
            Taken::Nothing => {}
            Taken::Failed(r) => return r,
        }
        if std::time::Instant::now() >= deadline {
            return StatusCode::NO_CONTENT.into_response();
        }
        tokio::time::sleep(CLAIM_POLL).await;
        if let Err(e) = runners::touch(&state.db, &runner.id, now_ms()) {
            eprintln!("weft: touch runner {}: {e}", runner.id);
        }
    }
}

enum Taken {
    Job(serde_json::Value),
    /// A row was claimed and then discarded; ask again at once.
    Again,
    Nothing,
    Failed(Response),
}

/// One pass of the claim: take a job if there is one, and make it ready
/// for a runner to execute.
///
/// The steps after the claim are the dispatcher's, minus the launch —
/// which is the point of the whole design. A self-hosted runner is
/// already running, so "start a task" is the only thing that differs
/// between the two pools, and everything that makes a job safe to hand
/// over (a per-attempt credential bound to the row before it is handed
/// out, a mirrored check row, a lease sized for a cold checkout) is
/// identical.
async fn take_one(state: &SharedState, runner_id: &str, route: &RunnerRoute<'_>) -> Taken {
    let job = match workflows::claim_self_hosted(&state.db, route, state.runner_start_lease_ms) {
        Ok(Some(j)) => j,
        Ok(None) => return Taken::Nothing,
        Err(e) => return Taken::Failed(internal(e)),
    };
    // The attempt cap, applied exactly where the dispatcher applies it:
    // after the claim, because `attempts` is what the claim increments,
    // and a job whose runner has vanished twice is a job that is not
    // going to work on the third machine either.
    if job.attempts > state.runner_max_attempts {
        crate::workers::runner::fail(
            state,
            &job,
            &format!(
                "the runner was lost {} times (it took the job but stopped reporting back)",
                job.attempts - 1
            ),
        )
        .await;
        return Taken::Again;
    }
    // A reclaim after a lease expired: the previous machine may still be
    // alive and holding a live repository credential, so retire it
    // before minting the next one. There is nothing to StopTask — see
    // `executor::stop_jobs` — and the old runner learns it is over from
    // the 410 its next call gets.
    // Its member tokens go with it, if it is a composed job that got as
    // far as fetching a spec: they read *other* member repositories, and
    // the machine that is holding them is the one being retired.
    crate::workflow::credentials::revoke_job_tokens(state, &job);
    let run = match workflows::run_by_id(&state.db, &job.run_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            crate::workers::runner::fail(
                state,
                &job,
                "the run this job belongs to no longer exists",
            )
            .await;
            return Taken::Again;
        }
        Err(e) => return Taken::Failed(internal(e)),
    };
    let label = format!("ci:{}", job.id);
    let expires = crate::workflow::credentials::token_expiry(&job.spec);
    let token = match auth::mint_for(
        &state.db,
        &job.org_id,
        &[Scope::RepoRead],
        Mint {
            repo_id: Some(&job.repo_id),
            label: Some(&label),
            user_id: None,
            expires_at: Some(expires),
        },
        None,
    ) {
        Ok(t) => t,
        Err(e) => {
            crate::workers::runner::fail(
                state,
                &job,
                &format!("could not mint the job's token: {e}"),
            )
            .await;
            return Taken::Again;
        }
    };
    let page = crate::workflow::mirror::run_page_by_id(
        &state.db,
        &state.public_url,
        &run.repo_id,
        &run.id,
    );
    let check_run_id = match crate::workflow::mirror::mirror_job(
        &state.db,
        &run.repo_id,
        &run,
        &job,
        page.as_deref(),
    ) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("weft: mirror job {}: {e}", job.id);
            String::new()
        }
    };
    // `runner:<id>` in `task_ref` keeps the column's meaning — what is
    // executing this — for a reader chasing a build, and is never handed
    // to an executor: `stop_jobs` skips self-hosted jobs by pool.
    let task_ref = format!("runner:{runner_id}");
    match workflows::mark_launched(&state.db, &job.id, &task_ref, &token.id, &check_run_id) {
        Ok(true) => {}
        Ok(false) => {
            // Cancelled between the claim and here. Nothing was handed
            // out, so the credential goes back — and so does anything a
            // previous attempt of this job left recorded.
            if let Err(e) = auth::revoke(&state.db, &job.org_id, &token.id, None) {
                eprintln!("weft: revoke job token for {}: {e}", job.id);
            }
            crate::workflow::credentials::revoke_job_tokens(state, &job);
            return Taken::Again;
        }
        Err(e) => return Taken::Failed(internal(e)),
    }
    Taken::Job(serde_json::json!({
        "job_id": job.id,
        "token": token.plaintext,
        // The address the runner already reached us on, not the public
        // one: they are the same behind a CDN and differ on a private
        // network, and the runner has to be able to reach whatever we
        // name here.
        "runner_url": state.runner_url,
    }))
}
