//! What a person asks about a run: what happened, stop it, show me the log.
//!
//! Four reads and one write, all on the ordinary `rest_repo_auth` door,
//! all repository-scoped. Nothing here talks to a runner — that surface
//! is `runner_api`, it has its own credential, and keeping them apart is
//! what makes "a job token cannot cancel a run" true by construction
//! rather than by a check somebody has to remember.
//!
//! The log is the part with a design in it. A build log is produced
//! slowly and read impatiently, so there are two routes over the same
//! bytes: `…/log` answers with whatever exists right now, and
//! `…/log/stream` is a server-sent-event feed that keeps answering until
//! the job ends. The feed reads the same chunk objects the plain route
//! does, which is what stops the two from ever disagreeing — there is no
//! separate live buffer to fall out of step with the stored truth.

use crate::api::{internal, json_error, present};
use crate::app::SharedState;
use crate::workflow::{logs, mirror};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::collections::HashMap;
use std::convert::Infallible;
use stratum_control::auth::Scope;
use stratum_control::workflows::{self, Run, WorkflowJob};
use stratum_store::{LatencyModel, ObjectStore};
use tokio_stream::wrappers::ReceiverStream;

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;

/// How often the live feed looks for new output. A second is what a
/// person reads as "live"; polling faster costs a query per viewer per
/// tick for output that arrives no faster than the runner flushes it,
/// which is also once a second.
const POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// The longest a stream is held open — the same order as the longest job
/// we allow. A connection that outlives every job it could be watching
/// is a leak, not a feature: browsers reconnect an EventSource by
/// themselves, so the cost of ending one is a reconnect and the cost of
/// never ending one is a socket per tab forever.
const MAX_STREAM: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// The runner that took a job, by id — `None` for a hosted job, and for
/// a self-hosted job nobody has claimed yet.
///
/// Resolved per job rather than joined into `jobs_of`, because it is one
/// point lookup on a page that shows at most a few dozen jobs, and
/// because the row is a *tombstone*: a runner removed last week still
/// has to name itself next to the build it ran, and a join that dropped
/// removed runners would silently blank exactly the history somebody is
/// looking at.
fn runner_json(state: &SharedState, job: &WorkflowJob) -> serde_json::Value {
    let Some(id) = job.runner_id.as_deref() else {
        return serde_json::Value::Null;
    };
    match stratum_control::runners::by_id(&state.db, id) {
        Ok(Some(r)) => serde_json::json!({ "id": r.id, "name": r.name }),
        Ok(None) => serde_json::Value::Null,
        Err(e) => {
            eprintln!("weft: read runner {id} for job {}: {e}", job.id);
            serde_json::Value::Null
        }
    }
}

/// The changeset a composed run belongs to, by key — `null` for every
/// other run, and for a changeset that has been deleted out from under
/// one. An id would be no use to a reader: `key` is what the changeset
/// routes take and what the dashboard links to.
fn changeset_ref(state: &SharedState, run: &Run) -> serde_json::Value {
    let Some(id) = run.changeset_id.as_deref() else {
        return serde_json::Value::Null;
    };
    match stratum_control::changesets::by_id(&state.db, id) {
        Ok(Some(cs)) => serde_json::json!({ "key": cs.key }),
        Ok(None) => serde_json::Value::Null,
        Err(e) => {
            eprintln!("weft: read changeset {id} for run {}: {e}", run.id);
            serde_json::Value::Null
        }
    }
}

/// One run as the API spells it. `pub(crate)` because the fork-approval
/// route hands back the runs it just started, and a second spelling of a
/// run would be a second thing to keep in step with the dashboard.
pub(crate) fn run_json(state: &SharedState, run: &Run, jobs: &[WorkflowJob]) -> serde_json::Value {
    serde_json::json!({
        "id": run.id,
        "file": run.file,
        "name": run.name,
        "commit_sha": run.commit_sha,
        "ref_name": run.ref_name,
        "event": run.event,
        "change_key": run.change_key,
        // For a composed run: which changeset it belongs to, by the key
        // a person types, and which combination of member tips it was
        // started for. Null on every push and change run. The dashboard
        // reads the pair to say "this build is of changeset X as it
        // stood", which a commit sha alone cannot express — the commit
        // is one member's, and the build saw all of them.
        "changeset": changeset_ref(state, run),
        "composition": run.composition,
        "state": run.state,
        "error": run.error,
        // Why it is blocked, as a word rather than as prose: `fork`,
        // `budget` or `suspended`, null otherwise. The dashboard offers
        // "approve these workflows" on `fork` alone, and must never
        // reach that decision by matching `error`.
        "blocked_reason": run.blocked_reason,
        "created_at": run.created_at,
        "updated_at": run.updated_at,
        "completed_at": run.completed_at,
        "jobs": jobs.iter().map(|j| job_json(state, j)).collect::<Vec<_>>(),
    })
}

fn job_json(state: &SharedState, job: &WorkflowJob) -> serde_json::Value {
    serde_json::json!({
        "id": job.id,
        "job_id": job.job_id,
        "key": job.key,
        // An object rather than the stored string: every caller parses
        // it, and a client that has to `JSON.parse` a field out of a
        // JSON document is a client that will forget to somewhere.
        "matrix": serde_json::from_str::<serde_json::Value>(&job.matrix)
            .unwrap_or_else(|_| serde_json::json!({})),
        "state": job.state,
        "attempts": job.attempts,
        "error": job.error,
        "detail_url": job.detail_url,
        "log_chunks": job.log_chunks,
        // Which fleet ran it, what it asked for, and which machine took
        // it. `runner` is null for every hosted job, which is what a
        // reader should see: "ran on <name>" is only a sentence about
        // hardware somebody chose.
        "pool": job.pool,
        "labels": job.labels,
        "runner": runner_json(state, job),
        "started_at": job.started_at,
        "completed_at": job.completed_at,
    })
}

// ---------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------

pub async fn list(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    // Clamped rather than refused, for the reason `checks::MAX_LIMIT`
    // gives: `limit` is a hint about how much the renderer wants, and a
    // 400 turns a harmless over-request into a broken page whose only
    // recovery is guessing a maximum the caller cannot see.
    let limit = present(&params, "limit")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);

    // Narrowing to a commit or a change is the server's job: the
    // approval panel wants "the runs at this change's tip", and a
    // client that gets them by filtering a window loses them on a busy
    // repository — a page that looks fine and has lost its button.
    let commit_sha = present(&params, "commit_sha");
    let change_key = present(&params, "change_key");
    // `event=changeset` is how the Checks tab finds this repository's
    // composed runs: they are deliberately kept out of `check_runs`
    // (`changeset_checks` says why), so without this the runs of every
    // changeset a repository is a member of were reachable only from
    // the changeset's own page, and its Checks tab had never heard of
    // them.
    let event = present(&params, "event");
    let runs = match workflows::runs_for_repo(
        &state.db,
        &repo_row.id,
        commit_sha,
        change_key,
        event,
        limit,
    ) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let mut out = Vec::with_capacity(runs.len());
    for run in &runs {
        match workflows::jobs_of(&state.db, &run.id) {
            Ok(jobs) => out.push(run_json(&state, run, &jobs)),
            Err(e) => return internal(e),
        }
    }
    Json(serde_json::json!({ "runs": out })).into_response()
}

/// One run. A run belonging to another repository answers 404 rather
/// than 403, the same as every other id in this API: an id that resolves
/// differently for a stranger is an existence oracle.
pub async fn get(
    State(state): State<SharedState>,
    Path((org, repo, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let run = match workflows::run(&state.db, &repo_row.id, &id) {
        Ok(Some(r)) => r,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "no such run"),
        Err(e) => return internal(e),
    };
    match workflows::jobs_of(&state.db, &run.id) {
        Ok(jobs) => Json(run_json(&state, &run, &jobs)).into_response(),
        Err(e) => internal(e),
    }
}

/// A job reached through the repository that owns it.
///
/// `workflows::job` is unscoped — the runner API needs it that way — so
/// the scoping is here, and it is a 404 rather than a 403 for the same
/// masking reason as above.
fn scoped_job(
    state: &SharedState,
    repo_id: &str,
    id: &str,
) -> Result<(WorkflowJob, Run), Response> {
    let job = workflows::job(&state.db, id)
        .map_err(internal)?
        .filter(|j| j.repo_id == repo_id)
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "no such job"))?;
    let run = workflows::run_by_id(&state.db, &job.run_id)
        .map_err(internal)?
        .ok_or_else(|| internal("the job's run is missing".to_string()))?;
    Ok((job, run))
}

/// The log as it stands — complete if the job is over, so far if it is
/// not. `text/plain`, because it is text and a reader may well be
/// `curl`ing it.
pub async fn log(
    State(state): State<SharedState>,
    Path((org, repo, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let (job, run) = match scoped_job(&state, &repo_row.id, &id) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let url = state.store_url.clone();
    let (run_id, job_id, attempt, chunks) =
        (run.id.clone(), job.id.clone(), job.attempts, job.log_chunks);
    let text = tokio::task::spawn_blocking(move || {
        let store = ObjectStore::new(&url, LatencyModel::None);
        logs::read_log(&store, &run_id, &job_id, attempt, chunks)
    })
    .await;
    match text {
        Ok(Ok(t)) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            t,
        )
            .into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(format!("store task: {e}")),
    }
}

/// The same log, as it arrives.
///
/// The feed is built by polling the job row and the chunk objects rather
/// than by a channel from the runner, and that is the design rather than
/// a shortcut: the server that a runner posts to is not necessarily the
/// server a reader is connected to. A fleet with an in-memory fan-out
/// would show the log to whoever happened to land on the right node and
/// a spinner to everybody else, and the failure would only appear in
/// production, under a load balancer, on the days it mattered.
pub async fn log_stream(
    State(state): State<SharedState>,
    Path((org, repo, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let (job, run) = match scoped_job(&state, &repo_row.id, &id) {
        Ok(x) => x,
        Err(r) => return r,
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(16);
    let store_url = state.store_url.clone();
    let db = state.db.clone();
    let (run_id, job_id, repo_id) = (run.id.clone(), job.id.clone(), repo_row.id.clone());

    tokio::spawn(async move {
        let deadline = std::time::Instant::now() + MAX_STREAM;
        // What we have already sent. Chunk numbering restarts per
        // attempt, so a retry that begins mid-stream resets this too —
        // otherwise a reader watching a retry would be shown nothing,
        // having "already seen" sequence numbers from the attempt that
        // died.
        let mut sent: i32 = 0;
        let mut attempt: i64 = job.attempts;
        let mut said_queued = false;

        loop {
            let (state_now, chunks, attempts) = {
                let db = db.clone();
                let jid = job_id.clone();
                let rid = repo_id.clone();
                // The repository is re-read alongside the job, and not
                // only when the stream is opened. Deleting a repository
                // tombstones the row rather than removing it, so the job
                // keeps saying `running` and this feed would go on
                // serving the log of a repository every other route in
                // the API has started answering 404 for — for as long as
                // `MAX_STREAM`, one socket per reader who had the page
                // open.
                match tokio::task::spawn_blocking(move || {
                    (
                        workflows::job(&db, &jid),
                        stratum_control::registry::repo_by_id_any(&db, &rid),
                    )
                })
                .await
                {
                    Ok((Ok(Some(j)), Ok(Some(_)))) => (j.state, j.log_chunks, j.attempts),
                    // The job vanished, the repository is gone, or the
                    // database is unhappy. Either way this feed has
                    // nothing more to say, and saying so is better than
                    // holding the socket open.
                    _ => break,
                }
            };
            if attempts != attempt {
                attempt = attempts;
                sent = 0;
            }

            if state_now == "queued" {
                if !said_queued
                    && tx
                        .send(Ok(Event::default().event("queued").data("{}")))
                        .await
                        .is_err()
                {
                    return;
                }
                said_queued = true;
            }

            while sent < chunks {
                let next = sent + 1;
                let url = store_url.clone();
                let (r, j) = (run_id.clone(), job_id.clone());
                let text = tokio::task::spawn_blocking(move || {
                    ObjectStore::new(&url, LatencyModel::None)
                        .get(&logs::chunk_key(&r, &j, attempt, next))
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                })
                .await;
                // A chunk the counter promised but the store does not
                // have yet is skipped rather than waited on: the runner
                // writes the object before it reports the number, so a
                // miss here means it is genuinely gone, and blocking the
                // whole feed on it would stop a live log dead.
                if let Ok(Ok(text)) = text {
                    // The chunk goes out as JSON — `{"text": "…"}` —
                    // rather than as the raw bytes, and that is a
                    // correctness matter rather than a style one. An SSE
                    // `data:` field cannot carry a newline: the wire
                    // format splits a multi-line payload across several
                    // `data:` lines and the reader rejoins them with
                    // `\n`, which silently **loses the trailing one**.
                    // Every log chunk ends in a newline, so raw text
                    // would run the last line of each chunk into the
                    // first line of the next — a log that is subtly
                    // wrong rather than obviously broken, and only in
                    // the live view, so the plain `…/log` route would
                    // disagree with the page beside it. JSON has no such
                    // hole and matches how `done` already reports.
                    let payload = serde_json::json!({ "text": text }).to_string();
                    if tx
                        .send(Ok(Event::default().event("chunk").data(payload)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                sent = next;
            }

            if state_now != "running" && state_now != "queued" {
                let _ = tx
                    .send(Ok(Event::default()
                        .event("done")
                        .data(serde_json::json!({ "state": state_now }).to_string())))
                    .await;
                return;
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    });

    Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ---------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------

/// Stop a run.
///
/// `RepoWrite`, because it destroys work: cancelling somebody's build is
/// not a read, and a `repo:read` token — which is what a job token is —
/// must not be able to do it.
pub async fn cancel(
    State(state): State<SharedState>,
    Path((org, repo, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoWrite) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let run = match workflows::run(&state.db, &repo_row.id, &id) {
        Ok(Some(r)) => r,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "no such run"),
        Err(e) => return internal(e),
    };
    // 409 rather than a silent no-op: the caller asked to stop something
    // and it is worth telling them it had already stopped, because the
    // page they are looking at is now out of date and they should
    // reload rather than wonder whether the button worked.
    if run.state != "running" {
        return json_error(
            StatusCode::CONFLICT,
            format!("this run is already {}", run.state),
        );
    }

    // Who to name in the reason. A person's own name if there is one;
    // otherwise the credential is a service token and nobody's name
    // belongs on it.
    let who = principal
        .user_id
        .as_deref()
        .and_then(|u| stratum_control::users::by_id(&state.db, u).ok().flatten())
        .map(|u| u.name)
        .unwrap_or_else(|| "a service token".to_string());
    let reason = format!("cancelled by {who}");

    let running_jobs = match workflows::cancel_run(&state.db, &run.id, &reason) {
        Ok(j) => j,
        Err(e) => return internal(e),
    };

    // Every job's check row has to follow, including the queued ones the
    // cancel just settled — a check left saying `queued` for a job that
    // will never run holds the land gate shut forever.
    let run_now = match workflows::run_by_id(&state.db, &run.id) {
        Ok(Some(r)) => r,
        Ok(None) => run,
        Err(e) => return internal(e),
    };
    let jobs = match workflows::jobs_of(&state.db, &run_now.id) {
        Ok(j) => j,
        Err(e) => return internal(e),
    };
    let page = mirror::run_page_by_id(&state.db, &state.public_url, &repo_row.id, &run_now.id);
    for j in &jobs {
        if let Err(e) = mirror::mirror_job(&state.db, &repo_row.id, &run_now, j, page.as_deref()) {
            return internal(e);
        }
    }

    // The rows say cancelled; the tasks behind them are still burning
    // CPU until somebody stops them. `running_jobs` is exactly the set
    // that has one.
    crate::workflow::credentials::stop_jobs(&state, &running_jobs, &reason).await;

    Json(run_json(&state, &run_now, &jobs)).into_response()
}
