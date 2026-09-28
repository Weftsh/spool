//! The five calls a runner makes, and nothing else.
//!
//! This is the only surface a runner's job credential can reach, and it
//! is reached from a machine that is executing somebody's untrusted
//! `run:` lines.
//! Everything about its shape follows from that:
//!
//! * **The credential is per-attempt, per-job.** A job token is minted
//!   at claim with `repo:read` on one repository and an expiry, and
//!   `workflow_jobs.token_id` records exactly which token it was. A
//!   token that is right for the repository but wrong for the job is a
//!   403 — otherwise one build could report another's verdict, which is
//!   how a green check gets forged for code that never compiled.
//! * **410 is the interesting status.** When a job is cancelled, or
//!   superseded, or already reported, every call here answers 410 with
//!   the state in the body, and the runner's contract is to kill what it
//!   is doing and go back to waiting for work. That is the whole
//!   cancellation mechanism: nothing listens on the runner's network, so
//!   "stop" is something the runner learns by being told on its next
//!   call.
//! * **Every body is bounded** before it is read anywhere. A chunk is
//!   256 KiB, the final log is 16 MiB, and both are refused with 413
//!   rather than truncated — a silently truncated log is worse than a
//!   refused one, because it looks complete.
//!
//! The refusal order is 401 → 404 → 403 → 410, and that order is the
//! contract rather than an accident: the state of a job is only ever
//! disclosed to the credential that owns it.
//!
//! With one deliberate exception, which is what makes cancellation
//! work at all: a token that no longer authenticates is 401 *unless* it
//! is the token this job was claimed with and the job has stopped
//! running, in which case it is 410 with the state. Cancelling revokes
//! the job's token, so the runner we are trying to stop is always
//! holding a dead credential — and a 401 is something a runner retries.
//! See `dead_job_token`.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::workflow::{logs, mirror};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use stratum_control::auth::{self, Mint, Scope};
use stratum_control::registry::Repo;
use stratum_control::workflows::{self, Run, WorkflowJob};
use stratum_store::{LatencyModel, ObjectStore};

/// How far a lease is pushed out by any call from the runner.
///
/// Generous next to the dispatcher's own lease, because these calls are
/// evidence the runner is alive and the cost of being wrong upward is a
/// dead job holding a slot for a minute longer.
const LEASE_MS: i64 = 120_000;

/// The bearer token, or nothing.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// The job this request is for, or the refusal that stops it.
///
/// One function because the order of these checks *is* the security
/// property, and five handlers each deciding it for themselves is five
/// chances to put 410 before 403 and turn a job's state into something
/// any valid token can read.
fn authed_job(
    state: &SharedState,
    headers: &HeaderMap,
    job_id: &str,
) -> Result<(WorkflowJob, Run, Repo), Response> {
    let Some(token) = bearer(headers) else {
        return Err(json_error(
            StatusCode::UNAUTHORIZED,
            "a job token is required",
        ));
    };
    let principal = match stratum_control::auth::verify(&state.db, token).map_err(internal)? {
        Some(p) => p,
        // A token that no longer authenticates is usually a stranger's,
        // and 401 is the answer. It is *this job's own* token in
        // exactly one situation, and it is the situation 410 exists
        // for — see `dead_job_token`.
        None => return Err(dead_job_token(state, token, job_id)),
    };

    let job = workflows::job(&state.db, job_id)
        .map_err(internal)?
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "no such job"))?;

    // The token must be *this attempt's* token, on *this job's*
    // repository. Either half alone is not enough: a repo-scoped token
    // is legitimately held by every job in the repository, and a job
    // token from a previous attempt is legitimately this job's.
    if principal.token_id != job.token_id.clone().unwrap_or_default()
        || principal.repo_id.as_deref() != Some(job.repo_id.as_str())
    {
        return Err(json_error(
            StatusCode::FORBIDDEN,
            "that token was not minted for this job",
        ));
    }

    let run = workflows::run_by_id(&state.db, &job.run_id)
        .map_err(internal)?
        .ok_or_else(|| internal("the job's run is missing".to_string()))?;

    // A repository that has been deleted takes its builds with it. The
    // row is tombstoned rather than removed, so the job still says
    // `running` and every call here would otherwise carry on — the spec
    // read answering 500 (which a runner *retries*), the log writes
    // filing objects under a prefix that is being swept. 410 is the only
    // status that means stop, so a deletion has to be reported as one:
    // otherwise the container keeps asking, holding a task and a live
    // repository token, until the overdue sweep gets to it hours later.
    // `cancelled` is what the runner should do about it, which is the
    // same word `reread_state` uses for a job that vanished.
    let repo = stratum_control::registry::repo_by_id_any(&state.db, &job.repo_id)
        .map_err(internal)?
        .ok_or_else(|| gone("cancelled"))?;

    if job.state != "running" {
        return Err(gone(&job.state));
    }
    Ok((job, run, repo))
}

/// What to answer a runner whose token has stopped authenticating.
///
/// Cancelling a job revokes its token: the moment "stop" is decided,
/// that token must not be able to read the repository or write a
/// verdict again. But it means the runner we are trying to stop is
/// holding a dead token, and 410 — the only status that tells a runner
/// to give up — sat behind `verify`, which answers `None` for a revoked
/// token. The runner
/// reads 401 as a refusal to retry against, so a superseded build ran
/// every remaining step to completion and only discovered it had been
/// cancelled when it tried to report: exactly the compute superseding
/// exists to save.
///
/// So a dead token gets one question asked of it: was it minted for
/// this job? Either it is the token the job was claimed with and the
/// job has stopped running, or it is an earlier attempt's — the job was
/// handed to another machine when this one's lease lapsed — and either
/// way the caller is a runner we need to stop, so tell it: 410, with the
/// state, or `reassigned`. Anything else — a stranger's token, an
/// unknown one, a composed member's, this job's current token while the
/// job is still running (a lapsed expiry, not a cancellation) — stays
/// 401, and the ladder is unchanged for every credential that is not
/// this job's own.
///
/// The disclosure is a job's state to a holder of that job's own token,
/// proven by the secret rather than by the id. That is information it
/// already had.
fn dead_job_token(state: &SharedState, token: &str, job_id: &str) -> Response {
    let refused = || {
        json_error(
            StatusCode::UNAUTHORIZED,
            "that job token is unknown, revoked or expired",
        )
    };
    let Ok(Some(token_id)) = auth::identify(&state.db, token) else {
        return refused();
    };
    let Ok(Some(job)) = workflows::job(&state.db, job_id) else {
        return refused();
    };
    if job.token_id.as_deref() == Some(token_id.as_str()) {
        if job.state == "running" {
            return refused();
        }
        return gone(&job.state);
    }
    // Not the job's current token — but it may be an earlier attempt's.
    // When a lease lapses and the job is handed to another machine, the
    // quiet one's token is revoked and the job carries the new one. If
    // that machine comes back, it must be told to stop: a 401 is what
    // its runner retries, and it would go on running the steps of a
    // build another machine is running too — a deploy step, twice.
    //
    // Only a token minted for *this* job, as a whole: `ci:<job>`. A
    // composed job's member tokens are `ci:<job>:<repo>` and a token
    // for another job is somebody else's business entirely.
    match auth::label_of(&state.db, &token_id) {
        Ok(Some(label)) if label == format!("ci:{}", job.id) => gone(if job.state == "running" {
            "reassigned"
        } else {
            &job.state
        }),
        _ => refused(),
    }
}

/// The runner's signal to stop. The state rides along so a container log
/// says *why* it exited rather than only that it did.
fn gone(state: &str) -> Response {
    (
        StatusCode::GONE,
        Json(serde_json::json!({
            "error": "job is no longer running",
            "state": state,
        })),
    )
        .into_response()
}

/// Run the closure against the object store off the async threads.
async fn with_store<T, F>(state: &SharedState, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&ObjectStore) -> Result<T, String> + Send + 'static,
{
    let url = state.store_url.clone();
    tokio::task::spawn_blocking(move || f(&ObjectStore::new(&url, LatencyModel::None)))
        .await
        .map_err(|e| format!("store task: {e}"))?
}

// ---------------------------------------------------------------------
// GET /v1/runner/jobs/:id — what to run
// ---------------------------------------------------------------------

/// Everything the runner needs to check out and execute, in one call.
///
/// One call rather than three because a runner that has to make a
/// sequence of them has a sequence of places to fail halfway, in a
/// container nobody is watching, holding a credential.
pub async fn spec(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (job, run, repo) = match authed_job(&state, &headers, &id) {
        Ok(x) => x,
        Err(r) => return r,
    };
    // The clone URL is built from the org and repo *names*, which the
    // runner needs anyway to make a sensible directory — and never
    // carries a credential: the token goes in a header the runner sets
    // itself, so it cannot end up in a remote's config, a reflog, or a
    // `ps` listing.
    let org = match stratum_control::registry::org_by_id(&state.db, &job.org_id) {
        Ok(Some(o)) => o,
        Ok(None) => return internal("the job's organisation is missing".to_string()),
        Err(e) => return internal(e),
    };

    // A change's commits are not on a branch — they live under
    // `refs/patchsets/<sha>` — so what to fetch depends on what caused
    // the run rather than on anything the job itself knows.
    let fetch_ref = if run.event == "change" {
        format!("refs/patchsets/{}", run.commit_sha)
    } else {
        format!("refs/heads/{}", run.ref_name.clone().unwrap_or_default())
    };

    let mut out = match serde_json::from_str::<serde_json::Value>(&job.spec) {
        Ok(serde_json::Value::Object(m)) => m,
        // A spec that is not an object is a dispatcher bug, and the
        // runner would rather have the identity fields and no steps
        // (which it reports as a job that did nothing) than a 500 it
        // retries five times.
        _ => serde_json::Map::new(),
    };
    out.insert("id".into(), job.id.clone().into());
    out.insert("run_id".into(), job.run_id.clone().into());
    out.insert("attempt".into(), job.attempts.into());
    out.insert("key".into(), job.key.clone().into());
    out.insert("job".into(), job.job_id.clone().into());
    // The runner clones from the address it already reaches the control
    // plane at, not the public one. The two are the same behind
    // CloudFront and differ everywhere else — a runner on a private
    // network with `STRATUM_RUNNER_URL` pointed at an internal listener
    // was being handed a clone URL it could not resolve.
    out.insert(
        "clone_url".into(),
        format!("{}/{}/{}.git", state.runner_url, org.name, repo.name).into(),
    );
    out.insert("fetch_ref".into(), fetch_ref.into());
    out.insert("commit_sha".into(), run.commit_sha.clone().into());
    out.insert("ref_name".into(), run.ref_name.clone().into());
    out.insert("event".into(), run.event.clone().into());
    out.insert("change_key".into(), run.change_key.clone().into());
    // The matrix comes from the column the planner wrote rather than
    // from the spec's copy: the column is what the job's `key` was built
    // from, so it is the one that cannot disagree with the cell's name.
    out.insert(
        "matrix".into(),
        serde_json::from_str(&job.matrix).unwrap_or_else(|_| serde_json::json!({})),
    );
    if run.event == "changeset" {
        match changeset_spec(&state, &job, &run, &org.name).await {
            Ok(Some(spec)) => {
                out.insert("changeset".into(), spec);
            }
            // A composed run whose changeset has gone, or cannot be
            // resolved, is answered *without* the object rather than
            // with a 500. The runner then behaves as it does for any
            // ordinary job — one repository, its own — which is a
            // truthful build of this member and a verdict somebody can
            // read, where a 500 is five retries and a job that dies
            // holding a credential.
            Ok(None) => {}
            Err(r) => return r,
        }
    }
    Json(serde_json::Value::Object(out)).into_response()
}

/// The `changeset` object: every member of the composition, in position
/// order, with a credential for each one this job cannot already read.
///
/// **Minted here, not at dispatch.** The launch environment is exactly
/// three variables and stays that way — a credential per member in a
/// task definition's environment is a credential in the platform's
/// console, its API and its logs — so the tokens are made at the last
/// moment before they are used, over a connection the runner has already
/// authenticated on.
///
/// **One token per member repository, never one org-wide token.** A
/// composed job executes `run:` lines its member's author wrote, and an
/// org-wide `repo:read` would let those lines read every repository in
/// the organisation, including ones that author cannot see. Per member,
/// the worst a stranger's script can reach is the repositories they have
/// already proposed a change into.
///
/// The job's own member gets `"token": null`: the runner already holds a
/// credential for that repository — the job token — and handing it a
/// second one for the same repository would be a second thing to revoke
/// for no gain.
///
/// The previous set is revoked, always. A runner that fetches its spec
/// twice gets fresh tokens both times, and the first set is otherwise
/// live credentials on other people's repositories that nothing is
/// watching; `set_member_tokens` returns exactly what is no longer
/// recorded, so this cannot leak by forgetting.
async fn changeset_spec(
    state: &SharedState,
    job: &WorkflowJob,
    run: &Run,
    org_name: &str,
) -> Result<Option<serde_json::Value>, Response> {
    let Some(cs_id) = run.changeset_id.as_deref() else {
        return Ok(None);
    };
    let cs = match stratum_control::changesets::by_id(&state.db, cs_id) {
        Ok(Some(cs)) => cs,
        Ok(None) => return Ok(None),
        Err(e) => return Err(internal(e.to_string())),
    };
    let Some((_, members)) = crate::workflow::trigger::compose(state, &cs) else {
        return Ok(None);
    };
    let expires = crate::workflow::credentials::token_expiry(&job.spec);
    let mut minted: Vec<String> = Vec::new();
    let mut out = Vec::with_capacity(members.len());
    for m in &members {
        let token = if m.repo.id == job.repo_id {
            None
        } else {
            let label = format!("ci:{}:{}", job.id, m.repo.name);
            match auth::mint_for(
                &state.db,
                &job.org_id,
                &[Scope::RepoRead],
                Mint {
                    repo_id: Some(&m.repo.id),
                    label: Some(&label),
                    user_id: None,
                    expires_at: Some(expires),
                },
                None,
            ) {
                Ok(t) => {
                    minted.push(t.id);
                    Some(t.plaintext)
                }
                Err(e) => {
                    // Whatever has been minted so far is handed to
                    // nobody, so it is revoked here rather than left to
                    // expire: the response this call is building will
                    // not contain it.
                    for id in &minted {
                        if let Err(e) = auth::revoke(&state.db, &job.org_id, id, None) {
                            eprintln!("weft: revoke member token for {}: {e}", job.id);
                        }
                    }
                    return Err(internal(format!(
                        "could not mint a read token for {}: {e}",
                        m.repo.name
                    )));
                }
            }
        };
        out.push(serde_json::json!({
            "repo": m.repo.name,
            "change": m.change.change_key,
            "clone_url": format!("{}/{}/{}.git", state.runner_url, org_name, m.repo.name),
            // A patchset is not on a branch, so every member is fetched
            // the same way a change run fetches its own tip.
            "fetch_ref": format!("refs/patchsets/{}", m.commit_sha),
            "commit_sha": m.commit_sha,
            "token": token,
        }));
    }
    match workflows::set_member_tokens(&state.db, &job.id, &minted) {
        Ok(previous) => {
            for id in previous {
                if let Err(e) = auth::revoke(&state.db, &job.org_id, &id, None) {
                    eprintln!("weft: revoke previous member token for {}: {e}", job.id);
                }
            }
        }
        // Recorded nowhere is the one outcome that must not stand: these
        // are live credentials on other repositories and nothing else
        // would ever revoke them. Give them back and refuse the spec —
        // the runner retries, and a retry that works hands out a set
        // that *is* recorded.
        Err(e) => {
            for id in &minted {
                if let Err(e) = auth::revoke(&state.db, &job.org_id, id, None) {
                    eprintln!("weft: revoke member token for {}: {e}", job.id);
                }
            }
            return Err(internal(e));
        }
    }
    Ok(Some(serde_json::json!({
        "key": cs.key,
        "members": out,
    })))
}

// ---------------------------------------------------------------------
// POST /v1/runner/jobs/:id/log — one flush
// ---------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ChunkBody {
    seq: i32,
    text: String,
}

pub async fn log_chunk(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (job, _, _) = match authed_job(&state, &headers, &id) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let chunk: ChunkBody = match serde_json::from_slice(&body) {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("bad chunk: {e}")),
    };
    if chunk.seq < 1 {
        return json_error(StatusCode::BAD_REQUEST, "seq starts at 1");
    }
    if chunk.text.len() > logs::MAX_CHUNK {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("a log chunk may be at most {} bytes", logs::MAX_CHUNK),
        );
    }

    // The object first, then the counter. A chunk the database has been
    // told about but the store does not have is a hole a reader sees; a
    // chunk in the store that nothing points at is invisible and the
    // lifecycle rule takes it. Only one of those is a defect.
    let (run_id, job_id, attempt) = (job.run_id.clone(), job.id.clone(), job.attempts);
    if let Err(e) = with_store(&state, move |store| {
        logs::put_chunk(store, &run_id, &job_id, attempt, chunk.seq, &chunk.text)
    })
    .await
    {
        return internal(e);
    }

    match workflows::record_log_chunk(&state.db, &job.id, chunk.seq, LEASE_MS) {
        Ok(Some(until)) => Json(serde_json::json!({ "lease_until": until })).into_response(),
        // It ended between the auth check and here — the runner is told
        // to stop, exactly as if it had been cancelled a moment earlier.
        Ok(None) => gone(&reread_state(&state, &job.id)),
        Err(e) => internal(e),
    }
}

/// What the job says now, for a 410 body. A job that vanished under us
/// is reported as `cancelled`, which is what the runner should do about
/// it.
fn reread_state(state: &SharedState, job_id: &str) -> String {
    match workflows::job(&state.db, job_id) {
        Ok(Some(j)) => j.state,
        _ => "cancelled".to_string(),
    }
}

// ---------------------------------------------------------------------
// POST /v1/runner/jobs/:id/lease — the idle heartbeat
// ---------------------------------------------------------------------

/// For a job that is working and not talking — a long compile produces
/// nothing to log for minutes at a time, and without this its lease
/// would expire and the dispatcher would hand the work to somebody else
/// while the first runner was still doing it.
pub async fn lease(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (job, _, _) = match authed_job(&state, &headers, &id) {
        Ok(x) => x,
        Err(r) => return r,
    };
    match workflows::renew(&state.db, &job.id, LEASE_MS) {
        Ok(true) => Json(serde_json::json!({
            "lease_until": stratum_control::ids::now_ms() + LEASE_MS
        }))
        .into_response(),
        Ok(false) => gone(&reread_state(&state, &job.id)),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------
// PUT /v1/runner/jobs/:id/log — the authoritative whole log
// ---------------------------------------------------------------------

/// The complete file, uploaded once at the end.
///
/// This is the copy that is trusted: a chunk POST that failed twice is
/// dropped by the runner rather than retried forever, so the chunk
/// sequence may have holes and this object does not.
pub async fn put_log(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (job, _, _) = match authed_job(&state, &headers, &id) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if body.len() > logs::MAX_LOG {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("a job log may be at most {} bytes", logs::MAX_LOG),
        );
    }
    let text = String::from_utf8_lossy(&body).into_owned();
    let (run_id, job_id) = (job.run_id.clone(), job.id.clone());
    match with_store(&state, move |store| {
        logs::put_log(store, &run_id, &job_id, &text)
    })
    .await
    {
        Ok(()) => Json(serde_json::json!({})).into_response(),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------
// POST /v1/runner/jobs/:id/finish — the verdict
// ---------------------------------------------------------------------

#[derive(Deserialize)]
pub struct FinishBody {
    state: String,
    #[serde(default)]
    error: Option<String>,
    /// What the runner caught the job doing, when it stopped the job
    /// because of it. `"mining"` is the only value today, and the field
    /// is optional because every honest verdict omits it.
    ///
    /// This is a *report*, not an instruction: it is accepted from a
    /// credential that a stranger's `run:` lines were executing next to,
    /// so the only thing it may cause is this organisation's own work
    /// being stopped. It cannot reach another tenant, and there is no
    /// value of it that turns anything back on.
    #[serde(default)]
    abuse: Option<String>,
}

pub async fn finish(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (job, run, repo) = match authed_job(&state, &headers, &id) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let verdict: FinishBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("bad verdict: {e}")),
    };
    // `cancelled` is deliberately not accepted here. A runner reports
    // what its steps did; a cancellation is a decision somebody else
    // made, and letting a job announce itself cancelled would let a
    // build escape a red verdict by claiming it was stopped.
    if !matches!(verdict.state.as_str(), "passed" | "failed") {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("{:?} is not a verdict; use passed or failed", verdict.state),
        );
    }

    if let Err(e) = workflows::finish(
        &state.db,
        &job.id,
        &verdict.state,
        None,
        verdict.error.as_deref(),
    ) {
        return internal(e);
    }

    // The verdict is the last call this credential is entitled to make,
    // so it stops being a credential here. It was minted to outlive the
    // job's whole timeout plus slack, and the steps that just ran were
    // untrusted code executing as the same uid as the runner — the token
    // in its environment has to be assumed read. Leaving it live would
    // leave a `repo:read` on this repository usable for hours after the
    // build that needed it is gone. Best effort, and deliberately after
    // the verdict is recorded: a revoke that fails is a token that
    // expires on its own, while refusing the verdict over it would lose
    // the one thing the runner called to say.
    // Every credential, not only the job's own: a composed job was
    // handed a read token per *other* member repository when it fetched
    // its spec, and those are the ones that must not be left behind —
    // they reach repositories this build's author may have no access to
    // at all.
    crate::workflow::credentials::revoke_job_tokens(&state, &job);

    // A runner that stopped the job because of what it was doing: the
    // job is already killed and failed with the runner's own sentence —
    // the watch protects the machine's owner from a stranger's change —
    // and the event is recorded so an operator reading the trail sees it
    // happened. Nothing is suspended: the machine is the organisation's
    // own, and switching its CI off would punish it for what happened on
    // hardware it owns.
    if let Some(kind) = verdict.abuse.as_deref().filter(|k| !k.is_empty()) {
        record_self_hosted_abuse(&state, &job, &repo, kind, verdict.error.as_deref());
    }

    // An ephemeral runner exists for exactly one job, and that job is
    // over however it ended. Retired here rather than left to the sweep
    // because the runner is about to exit on its own — GitHub's
    // ephemeral+autoscale shape — and a row that says `offline` for a
    // day afterwards is a list an operator has to learn to ignore.
    if let Some(runner_id) = job.runner_id.as_deref() {
        match stratum_control::runners::retire_ephemeral(
            &state.db,
            runner_id,
            stratum_control::ids::now_ms(),
        ) {
            Ok(true) => {
                let actx = stratum_control::audit::AuditCtx::of(&job.org_id, None);
                crate::api::record_or_warn(
                    &state.db,
                    &actx,
                    Some(&repo.id),
                    "runner.removed",
                    Some(&serde_json::json!({
                        "id": runner_id, "reason": "ephemeral", "job": job.id,
                    })),
                );
            }
            Ok(false) => {}
            Err(e) => eprintln!("weft: retire ephemeral runner {runner_id}: {e}"),
        }
    }

    // Mirror **every** job in the run, not only this one. `finish`
    // cascades: a failure marks everything downstream `skipped` in the
    // same transaction, and those jobs have check rows of their own that
    // still say `queued`. Leaving them is not cosmetic — the land gate
    // reads those rows, so a change would sit unlandable behind checks
    // for jobs that will never run, with nothing on the page to explain
    // it. Re-reading is also why this is not a patch of the copy we
    // hold: the run may have settled here too.
    let run_now = match workflows::run_by_id(&state.db, &job.run_id) {
        Ok(Some(r)) => r,
        Ok(None) => run,
        Err(e) => return internal(e),
    };
    let siblings = match workflows::jobs_of(&state.db, &job.run_id) {
        Ok(js) => js,
        Err(e) => return internal(e),
    };
    let page = mirror::run_page_by_id(&state.db, &state.public_url, &job.repo_id, &run_now.id);
    for j in &siblings {
        if let Err(e) = mirror::mirror_job(&state.db, &job.repo_id, &run_now, j, page.as_deref()) {
            return internal(e);
        }
    }

    // The chunks are redundant once the whole log is stored. Best
    // effort: failing the verdict because a DELETE did not go through
    // would throw away the one part of this request that cannot be
    // reconstructed.
    let (run_id, job_id, attempt) = (job.run_id.clone(), job.id.clone(), job.attempts);
    let _ = with_store(&state, move |store| {
        logs::delete_chunks(store, &run_id, &job_id, attempt);
        Ok(())
    })
    .await;

    Json(serde_json::json!({})).into_response()
}

/// Record that a self-hosted runner stopped a job for abuse, and do
/// nothing else.
///
/// The layers before this one already fired: the parser refused what it
/// could see, the runner's watch killed the process group, and the
/// verdict recorded just before this is a failure with the runner's own
/// sentence in it. The trail entry names the pool so that it reads the
/// same as it does on a server that also runs a hosted fleet.
fn record_self_hosted_abuse(
    state: &SharedState,
    job: &WorkflowJob,
    repo: &Repo,
    kind: &str,
    error: Option<&str>,
) {
    let reason = error
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .unwrap_or("a self-hosted runner stopped this job for abuse");
    let actx = stratum_control::audit::AuditCtx::of(&job.org_id, None);
    crate::api::record_or_warn(
        &state.db,
        &actx,
        Some(&repo.id),
        "workflow.abuse",
        Some(&serde_json::json!({
            "abuse": kind,
            "reason": reason,
            "pool": "self_hosted",
            "runner": job.runner_id,
            "job": job.id,
            "run": job.run_id,
        })),
    );
}
