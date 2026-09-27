//! Workflow runs: the queue a `.weft` workflow becomes.
//!
//! One table per level — the run, its jobs, and the edges between them —
//! and every interesting decision is in [`claim_self_hosted`], which has
//! to answer one question cheaply and correctly: which job may this
//! machine start now?
//!
//! **Readiness is computed, never stored.** There is no `blocked` state
//! for something to forget to clear. The claim asks whether any
//! dependency has not passed, so a job becomes runnable the instant its
//! last dependency does. The alternative — flip dependents to `queued`
//! when a job passes — has a window between the two writes, and a
//! process that dies inside it strands the rest of the run forever. That
//! is the classic way a build queue wedges, and there is nothing here to
//! miss.

use crate::db::ControlDb;
use crate::ids::{now_ms, ulid, valid_id};
use serde::Serialize;

/// A run, as created.
#[derive(Debug, Clone)]
pub struct NewRun<'a> {
    /// The path it was read from, e.g. `.weft/ci.yml`.
    pub file: &'a str,
    /// What the workflow called itself.
    pub name: &'a str,
    pub commit_sha: &'a str,
    pub ref_name: Option<&'a str>,
    /// `push`, `change` or `changeset`.
    pub event: &'a str,
    /// Set when a change caused this run, so the verdict can reach the
    /// review as well as the commit.
    pub change_key: Option<&'a str>,
    /// The changeset this run is one member's part of, for a `changeset`
    /// run. `None` for every other event.
    pub changeset_id: Option<&'a str>,
    /// What that changeset was when the run started — see migration
    /// 0048. Moves with `changeset_id`, in both directions.
    pub composition: Option<&'a str>,
    /// Whether the commit this run builds came from a fork. Recorded at
    /// creation because it is the trigger that knows, and nothing
    /// downstream can work it out again without guessing.
    pub from_fork: bool,
}

/// One planned job on the way in.
///
/// `needs` is by **index into the slice being created**, not by id:
/// the ids do not exist until the insert, and asking a caller to
/// pre-generate them would put id minting in the planner, which has no
/// business knowing this table exists.
#[derive(Debug, Clone)]
pub struct NewJob<'a> {
    pub job_id: &'a str,
    pub key: &'a str,
    /// The cell's matrix bindings, already serialised. Opaque here.
    pub matrix: &'a str,
    pub needs: &'a [usize],
    /// The JobSpec as JSON — image, timeout, env, matrix, steps. Opaque
    /// to this module except for `timeout_minutes`, which [`overdue_jobs`]
    /// reads, and frozen at creation so a retry runs what the commit
    /// said rather than what `.weft/` says by the time it happens.
    pub spec: &'a str,
    /// Which fleet this job may run on. Always `self_hosted` here: the
    /// column also admits `hosted`, which this edition never writes — a
    /// file that asks for a hosted runner is refused at trigger time.
    pub pool: &'a str,
    /// The `runs-on` list as written — lowercased, deduped, file order,
    /// always containing `self-hosted`. Routed on by the claim, and
    /// carried into the run JSON.
    pub labels: &'a [String],
}

/// The pool every job is in, in the one spelling the column, the router
/// and the API all use.
pub const POOL_SELF_HOSTED: &str = "self_hosted";

/// `self_hosted`, no labels — what a caller that says nothing about pools
/// still means. An empty label list is admitted by every runner.
///
/// Written out rather than derived, because a derived `&str::default()`
/// is `""`, which fails the column's CHECK at insert time — a much later
/// and much less legible place to discover a field somebody forgot.
impl Default for NewJob<'_> {
    fn default() -> Self {
        NewJob {
            job_id: "",
            key: "",
            matrix: "{}",
            needs: &[],
            spec: "{}",
            pool: POOL_SELF_HOSTED,
            labels: &[],
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub id: String,
    pub repo_id: String,
    pub file: String,
    pub name: String,
    pub commit_sha: String,
    pub ref_name: Option<String>,
    pub event: String,
    pub change_key: Option<String>,
    /// See [`NewRun::changeset_id`].
    pub changeset_id: Option<String>,
    /// See [`NewRun::composition`].
    pub composition: Option<String>,
    /// Whether the commit this run builds came from a fork. The run is
    /// the truth; every job denormalises it, because the credential is
    /// minted where the job is.
    pub from_fork: bool,
    /// `running`, `passed`, `failed`, `cancelled`, or `blocked` — the
    /// last being a run that exists but may not start yet.
    pub state: String,
    /// Why it ended that way, when the reason is the run's rather than
    /// any one job's: a refused workflow file, a fork change awaiting
    /// approval, a cancellation and who asked for it.
    pub error: Option<String>,
    /// Which refusal blocked it — `fork`, `budget` or `suspended` — for
    /// a caller that has to *decide* something rather than display it.
    /// `None` unless `state` is `blocked`. See [`BlockedReason`].
    pub blocked_reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
}

/// Why a run is `blocked`, in the one spelling every layer uses.
///
/// An enum rather than a free string because the reader is told what to
/// do by it: a fork change needs a maintainer to press a button. The
/// sentence that carries that to a person lives in the trigger and will
/// be rewritten; this will not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BlockedReason {
    /// A change pushed from a fork, waiting for approval.
    Fork,
}

impl BlockedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockedReason::Fork => "fork",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowJob {
    pub id: String,
    pub run_id: String,
    pub org_id: String,
    pub repo_id: String,
    pub job_id: String,
    pub key: String,
    pub matrix: String,
    pub state: String,
    pub attempts: i64,
    pub detail_url: Option<String>,
    pub error: Option<String>,
    /// The highest log chunk the runner has uploaded for this attempt.
    /// The chunks themselves are objects in the store; this is the only
    /// part a reader needs from the database to find them.
    pub log_chunks: i32,
    /// What is executing it — `runner:<id>`, the machine that claimed it.
    pub task_ref: Option<String>,
    /// The job token minted for this attempt, and the only token the
    /// runner API will accept reports from.
    pub token_id: Option<String>,
    /// The `check_runs` row mirroring this job.
    pub check_run_id: Option<String>,
    /// The read tokens minted for this job's *other* members, for a
    /// composed run — one per member repository that is not this job's
    /// own. Empty for every push and change job, and for a composed job
    /// whose spec has not been fetched yet: they are minted when the
    /// runner asks for the spec, which is the last moment before they
    /// are used and the earliest one at which they can be revoked as a
    /// set. See [`set_member_tokens`].
    pub member_token_ids: Vec<String>,
    /// Whether the code this job builds came from a fork. Carried from
    /// the trigger's own gate rather than re-derived here.
    pub from_fork: bool,
    /// See [`NewJob::spec`].
    pub spec: String,
    /// `hosted` or `self_hosted`. See [`NewJob::pool`].
    pub pool: String,
    /// See [`NewJob::labels`].
    pub labels: Vec<String>,
    /// The self-hosted runner that claimed this attempt. `None` for
    /// every hosted job, and for a self-hosted job nobody has taken.
    pub runner_id: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
}

const RUN_COLS: &str = "id, repo_id, file, name, commit_sha, ref_name, event, change_key, \
                        changeset_id, composition, from_fork, \
                        state, error, blocked_reason, created_at, updated_at, completed_at";

const JOB_COLS: &str = "id, run_id, org_id, repo_id, job_id, key, matrix, state, attempts, \
                        detail_url, error, log_chunks, task_ref, token_id, check_run_id, \
                        member_token_ids, from_fork, spec, pool, labels, \
                        runner_id, created_at, started_at, completed_at";

fn row_to_run(r: &postgres::Row) -> Run {
    Run {
        id: r.get("id"),
        repo_id: r.get("repo_id"),
        file: r.get("file"),
        name: r.get("name"),
        commit_sha: r.get("commit_sha"),
        ref_name: r.get("ref_name"),
        event: r.get("event"),
        change_key: r.get("change_key"),
        changeset_id: r.get("changeset_id"),
        composition: r.get("composition"),
        from_fork: r.get("from_fork"),
        state: r.get("state"),
        error: r.get("error"),
        blocked_reason: r.get("blocked_reason"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        completed_at: r.get("completed_at"),
    }
}

fn row_to_job(r: &postgres::Row) -> WorkflowJob {
    WorkflowJob {
        id: r.get("id"),
        run_id: r.get("run_id"),
        org_id: r.get("org_id"),
        repo_id: r.get("repo_id"),
        job_id: r.get("job_id"),
        key: r.get("key"),
        matrix: r.get("matrix"),
        state: r.get("state"),
        attempts: r.get("attempts"),
        detail_url: r.get("detail_url"),
        error: r.get("error"),
        log_chunks: r.get("log_chunks"),
        task_ref: r.get("task_ref"),
        token_id: r.get("token_id"),
        check_run_id: r.get("check_run_id"),
        member_token_ids: r.get("member_token_ids"),
        from_fork: r.get("from_fork"),
        spec: r.get("spec"),
        pool: r.get("pool"),
        labels: r.get("labels"),
        runner_id: r.get("runner_id"),
        created_at: r.get("created_at"),
        started_at: r.get("started_at"),
        completed_at: r.get("completed_at"),
    }
}

/// Create a run and every job in it, atomically.
///
/// One transaction, because a run whose jobs are half-written is a run
/// that will never finish: the completion check asks whether anything is
/// still queued or running, and a partial insert can satisfy it while
/// the rest of the work has not been recorded yet.
pub fn create_run(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    run: &NewRun,
    jobs: &[NewJob],
) -> Result<Run, String> {
    if jobs.is_empty() {
        return Err("a run needs at least one job".into());
    }
    // Every edge must point inside this run. A caller passing an index
    // past the end has a planner bug, and writing the rows anyway would
    // turn it into a job that can never become ready.
    for j in jobs {
        for &n in j.needs {
            if n >= jobs.len() {
                return Err(format!("job {:?} needs job #{n}, which is not here", j.key));
            }
        }
    }
    let run_id = ulid();
    let ids: Vec<String> = (0..jobs.len()).map(|_| ulid()).collect();
    let now = now_ms();

    let row = db
        .lock()
        .transaction(|tx| {
            let row = tx.query_one(
                &format!(
                    "INSERT INTO workflow_runs \
                       (id, org_id, repo_id, file, name, commit_sha, ref_name, event, \
                        change_key, changeset_id, composition, from_fork, state, \
                        created_at, updated_at) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$13,'running',$12,$12) \
                     RETURNING {RUN_COLS}"
                ),
                &[
                    &run_id,
                    &org_id,
                    &repo_id,
                    &run.file,
                    &run.name,
                    &run.commit_sha,
                    &run.ref_name,
                    &run.event,
                    &run.change_key,
                    &run.changeset_id,
                    &run.composition,
                    &now,
                    &run.from_fork,
                ],
            )?;
            for (i, j) in jobs.iter().enumerate() {
                tx.execute(
                    "INSERT INTO workflow_jobs \
                       (id, run_id, org_id, repo_id, job_id, key, matrix, ordinal, \
                        spec, pool, labels, from_fork, state, created_at, updated_at) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$13,'queued',$12,$12)",
                    &[
                        &ids[i],
                        &run_id,
                        &org_id,
                        &repo_id,
                        &j.job_id,
                        &j.key,
                        &j.matrix,
                        &(i as i32),
                        &j.spec,
                        &j.pool,
                        &j.labels,
                        &now,
                        &run.from_fork,
                    ],
                )?;
            }
            for (i, j) in jobs.iter().enumerate() {
                for &n in j.needs {
                    tx.execute(
                        "INSERT INTO workflow_job_deps (job_id, needs_id) VALUES ($1,$2) \
                         ON CONFLICT DO NOTHING",
                        &[&ids[i], &ids[n]],
                    )?;
                }
            }
            Ok(row)
        })
        .map_err(|e| format!("create workflow run: {e}"))?;
    Ok(row_to_run(&row))
}

/// Record a run that is over — or blocked — before it ever had a job.
///
/// Three real situations produce one: a workflow file we refused to
/// parse, a change pushed from a fork that a maintainer has not let run
/// yet, and a deployment with no runner configured at all. Each is a
/// fact somebody needs to see next to their commit, and the alternative
/// — write nothing — is the worst answer available: a push that
/// silently produces no CI at all looks exactly like a push whose CI has
/// not started, and a reader waits for a check that will never come.
///
/// [`create_run`] deliberately refuses an empty job list, so this is a
/// separate door rather than a flag: a *running* run with no jobs can
/// never complete, and that refusal is worth keeping sharp.
pub fn create_settled_run(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    run: &NewRun,
    state: &str,
    error: Option<&str>,
    blocked_reason: Option<BlockedReason>,
) -> Result<Run, String> {
    if !matches!(state, "failed" | "blocked" | "cancelled") {
        return Err(format!("{state:?} is not a state a run can be created in"));
    }
    // Exactly one, in both directions. A `blocked` run with no code is
    // one the dashboard cannot decide anything about, and a code on a
    // run that is not waiting is a lie a later reader will branch on.
    if (state == "blocked") != blocked_reason.is_some() {
        return Err(format!(
            "a run in {state:?} may not carry a blocked reason of {blocked_reason:?}"
        ));
    }
    let code = blocked_reason.map(BlockedReason::as_str);
    let id = ulid();
    let now = now_ms();
    // `completed_at` only for the states that are actually over.
    // `blocked` is waiting, not finished, and stamping it would make a
    // run that has not happened look like one that has.
    let completed: Option<i64> = (state != "blocked").then_some(now);
    let row = db
        .lock()
        .query_one(
            &format!(
                "INSERT INTO workflow_runs \
                   (id, org_id, repo_id, file, name, commit_sha, ref_name, event, \
                    change_key, changeset_id, composition, state, error, blocked_reason, \
                    created_at, updated_at, completed_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$15,$16) \
                 RETURNING {RUN_COLS}"
            ),
            &[
                &id,
                &org_id,
                &repo_id,
                &run.file,
                &run.name,
                &run.commit_sha,
                &run.ref_name,
                &run.event,
                &run.change_key,
                &run.changeset_id,
                &run.composition,
                &state,
                &error,
                &code,
                &now,
                &completed,
            ],
        )
        .map_err(|e| format!("create settled workflow run: {e}"))?;
    Ok(row_to_run(&row))
}

/// What one self-hosted runner needs to be routed a job.
///
/// Passed as a struct rather than as six arguments because every field
/// is part of one question — "may this machine take this build" — and
/// because the caller reads them off a `runners::Runner` in one go.
#[derive(Debug, Clone)]
pub struct RunnerRoute<'a> {
    pub runner_id: &'a str,
    pub org_id: &'a str,
    pub group_id: &'a str,
    /// Every label this runner has. A job is routable when its own
    /// labels are a **subset** of these.
    pub labels: &'a [String],
    /// Whether the runner's group admits every repository, or only the
    /// ones in `runner_group_repos`.
    pub all_repos: bool,
}

/// Take the next self-hosted job this runner may run, or `None`.
///
/// One `UPDATE … WHERE id = (SELECT … FOR UPDATE SKIP LOCKED)`, so a
/// hundred runners polling at once step over each other's rows instead
/// of double-claiming, and readiness, freeness and *admission* are all
/// evaluated inside the one lock.
///
/// The three admission clauses are the security property of this
/// feature, and each is a different owner's decision:
///
/// * **`runner.labels ⊇ job.labels`** — what the file asked for. A
///   `TEXT[] @>` against the GIN index, not a scan: a job asking for
///   `[self-hosted, gpu]` is invisible to a runner without `gpu`.
/// * **the group admits the repository** — the runner owner's decision.
/// * **the organisation's policy admits the repository** — the org
///   owner's decision, re-read here and not only at trigger time,
///   because a job can sit in the queue across a policy change and the
///   answer that matters is the one at the moment it would start.
///
/// `FOR UPDATE OF j` rather than a bare `FOR UPDATE`, because the
/// subquery joins `repos` and `orgs`: locking those too would make one
/// runner's poll block another's on rows neither of them is claiming.
pub fn claim_self_hosted(
    db: &ControlDb,
    runner: &RunnerRoute,
    lease_ms: i64,
) -> Result<Option<WorkflowJob>, String> {
    let now = now_ms();
    let labels = runner.labels.to_vec();
    let row = db
        .lock()
        .query_opt(
            &format!(
                "UPDATE workflow_jobs SET \
                   state = 'running', lease_until = $2, attempts = attempts + 1, \
                   runner_id = $3, updated_at = $1, started_at = COALESCE(started_at, $1) \
                 WHERE id = ( \
                   SELECT j.id FROM workflow_jobs j \
                   JOIN orgs o ON o.id = j.org_id \
                   WHERE j.pool = 'self_hosted' AND j.org_id = $4 \
                     AND (j.state = 'queued' \
                          OR (j.state = 'running' AND j.lease_until < $1)) \
                     AND NOT EXISTS ( \
                       SELECT 1 FROM workflow_job_deps d \
                       JOIN workflow_jobs n ON n.id = d.needs_id \
                       WHERE d.job_id = j.id AND n.state <> 'passed') \
                     AND $5::TEXT[] @> j.labels \
                     AND ($6 OR EXISTS ( \
                       SELECT 1 FROM runner_group_repos gr \
                       WHERE gr.group_id = $7 AND gr.repo_id = j.repo_id)) \
                     AND o.runner_self_hosted <> 'disabled' \
                     AND (o.runner_self_hosted = 'all' OR EXISTS ( \
                       SELECT 1 FROM org_self_hosted_repos s \
                       WHERE s.org_id = j.org_id AND s.repo_id = j.repo_id)) \
                   ORDER BY j.created_at, j.ordinal, j.id \
                   LIMIT 1 FOR UPDATE OF j SKIP LOCKED) \
                 RETURNING {JOB_COLS}"
            ),
            &[
                &now,
                &(now + lease_ms),
                &runner.runner_id,
                &runner.org_id,
                &labels,
                &runner.all_repos,
                &runner.group_id,
            ],
        )
        .map_err(|e| format!("claim self-hosted workflow job: {e}"))?;
    Ok(row.as_ref().map(row_to_job))
}

/// Whether this runner already holds a job that is running.
///
/// Asked before a claim, and answered with a 409 rather than a second
/// job: a runner executes one job at a time, and a `run` loop that
/// somehow asked twice would otherwise be handed work it will never
/// report on, which reads to everybody else as a job that hung.
pub fn running_job_for_runner(db: &ControlDb, runner_id: &str) -> Result<Option<String>, String> {
    let row = db
        .lock()
        .query_opt(
            "SELECT id FROM workflow_jobs WHERE runner_id = $1 AND state = 'running' LIMIT 1",
            &[&runner_id],
        )
        .map_err(|e| format!("read running job for runner: {e}"))?;
    Ok(row.map(|r| r.get("id")))
}

/// Keep a long build's claim alive.
///
/// Scoped to the job *and* to `running`, so a heartbeat arriving after
/// the job was cancelled cannot resurrect it.
pub fn renew(db: &ControlDb, job_id: &str, lease_ms: i64) -> Result<bool, String> {
    let now = now_ms();
    db.lock()
        .execute(
            "UPDATE workflow_jobs SET lease_until = $2, updated_at = $3 \
             WHERE id = $1 AND state = 'running'",
            &[&job_id, &(now + lease_ms), &now],
        )
        .map(|n| n > 0)
        .map_err(|e| format!("renew workflow job: {e}"))
}

/// Record how a job ended, cascade the consequences, and finish the run
/// if that was the last of it.
///
/// All three in one transaction. A verdict recorded without its cascade
/// leaves dependents waiting on a job that will never pass; a cascade
/// without the run's completion leaves a run that is over but says it is
/// running.
pub fn finish(
    db: &ControlDb,
    job_id: &str,
    state: &str,
    detail_url: Option<&str>,
    error: Option<&str>,
) -> Result<(), String> {
    if !matches!(state, "passed" | "failed" | "cancelled") {
        return Err(format!("{state:?} is not a verdict a job can end with"));
    }
    if !valid_id(job_id) {
        return Ok(());
    }
    let now = now_ms();
    db.lock()
        .transaction(|tx| {
            let updated = tx.query_opt(
                "UPDATE workflow_jobs SET state = $2, detail_url = $3, error = $4, \
                   updated_at = $5, completed_at = $5 \
                 WHERE id = $1 AND state = 'running' RETURNING run_id",
                &[&job_id, &state, &detail_url, &error, &now],
            )?;
            // Not running any more — a duplicate report, or a job
            // cancelled while its runner was still talking. Neither is
            // an error and neither may overwrite the verdict already
            // recorded.
            let Some(row) = updated else {
                return Ok(());
            };
            let run_id: String = row.get("run_id");

            if state != "passed" {
                // Everything downstream is skipped, transitively. Only
                // `queued` rows: a dependent already running is doing
                // real work and will report its own verdict, and marking
                // it skipped would throw away a result we are about to
                // be given.
                tx.execute(
                    "WITH RECURSIVE down(id) AS ( \
                       SELECT d.job_id FROM workflow_job_deps d WHERE d.needs_id = $1 \
                       UNION \
                       SELECT d.job_id FROM workflow_job_deps d \
                         JOIN down ON d.needs_id = down.id) \
                     UPDATE workflow_jobs SET state = 'skipped', error = $3, \
                       updated_at = $2, completed_at = $2 \
                     WHERE id IN (SELECT id FROM down) AND state = 'queued'",
                    &[&job_id, &now, &"a job it needs did not pass"],
                )?;
            }

            // The run is over when nothing is left to do. `failed` if
            // anything failed, whatever else happened after it.
            let left: i64 = tx
                .query_one(
                    "SELECT COUNT(*) FROM workflow_jobs \
                     WHERE run_id = $1 AND state IN ('queued','running')",
                    &[&run_id],
                )?
                .get(0);
            if left == 0 {
                let failed: i64 = tx
                    .query_one(
                        "SELECT COUNT(*) FROM workflow_jobs \
                         WHERE run_id = $1 AND state IN ('failed','cancelled')",
                        &[&run_id],
                    )?
                    .get(0);
                let verdict = if failed > 0 { "failed" } else { "passed" };
                tx.execute(
                    "UPDATE workflow_runs SET state = $2, updated_at = $3, completed_at = $3 \
                     WHERE id = $1 AND state = 'running'",
                    &[&run_id, &verdict, &now],
                )?;
            }
            Ok(())
        })
        .map_err(|e| format!("finish workflow job: {e}"))
}

/// One run, scoped to the repository that owns it.
///
/// `repo_id` is in the `WHERE` rather than compared afterwards, for the
/// same reason `checks::get` does it: a run belonging to another
/// repository has to be indistinguishable from an id nobody minted.
pub fn run(db: &ControlDb, repo_id: &str, id: &str) -> Result<Option<Run>, String> {
    if !valid_id(id) {
        return Ok(None);
    }
    let row = db
        .lock()
        .query_opt(
            &format!("SELECT {RUN_COLS} FROM workflow_runs WHERE repo_id = $1 AND id = $2"),
            &[&repo_id, &id],
        )
        .map_err(|e| format!("read workflow run: {e}"))?;
    Ok(row.as_ref().map(row_to_run))
}

/// A run's jobs, in the order they were planned.
pub fn jobs_of(db: &ControlDb, run_id: &str) -> Result<Vec<WorkflowJob>, String> {
    if !valid_id(run_id) {
        return Ok(Vec::new());
    }
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {JOB_COLS} FROM workflow_jobs WHERE run_id = $1 \
                 ORDER BY created_at, ordinal, id"
            ),
            &[&run_id],
        )
        .map_err(|e| format!("read workflow jobs: {e}"))?;
    Ok(rows.iter().map(row_to_job).collect())
}

/// One job by id, unscoped.
///
/// Unlike [`run`] there is no `repo_id` in the `WHERE`, because the two
/// callers — the runner API and the dispatcher — hold a job id and
/// nothing else. Scoping is theirs to do: the runner API compares the
/// presented token against `token_id`, and the read routes compare
/// `repo_id` against the repository the caller reached through. Doing it
/// here would mean every caller passing a repo id it had to look up
/// *from this row*.
pub fn job(db: &ControlDb, job_id: &str) -> Result<Option<WorkflowJob>, String> {
    if !valid_id(job_id) {
        return Ok(None);
    }
    let row = db
        .lock()
        .query_opt(
            &format!("SELECT {JOB_COLS} FROM workflow_jobs WHERE id = $1"),
            &[&job_id],
        )
        .map_err(|e| format!("read workflow job: {e}"))?;
    Ok(row.as_ref().map(row_to_job))
}

/// One run by id, unscoped — the dispatcher's read, for the same reason
/// [`job`] is unscoped. Anything a person reaches goes through [`run`].
pub fn run_by_id(db: &ControlDb, run_id: &str) -> Result<Option<Run>, String> {
    if !valid_id(run_id) {
        return Ok(None);
    }
    let row = db
        .lock()
        .query_opt(
            &format!("SELECT {RUN_COLS} FROM workflow_runs WHERE id = $1"),
            &[&run_id],
        )
        .map_err(|e| format!("read workflow run by id: {e}"))?;
    Ok(row.as_ref().map(row_to_run))
}

/// The run this exact trigger already produced, if it did.
///
/// The trigger's idempotency guard, and it is needed because the same
/// commit legitimately arrives more than once: a branch pushed and then
/// pushed again with no new commits, a change re-uploaded, a retried
/// webhook. Without this, one commit collects a fresh run per delivery,
/// each mirroring its own `check_runs` rows, and the Checks tab fills up
/// with duplicates of one build.
///
/// `IS NOT DISTINCT FROM` rather than `=` on `change_key`, because a
/// push has none and `NULL = NULL` is not true — which would make the
/// guard silently never match for exactly the most common case.
///
/// **A `cancelled` run does not count, but only for a composed one** —
/// a lookup carrying a `composition`. A composition is cancelled for
/// exactly one reason: it stopped being the changeset's current
/// combination. It can become current again — remove a member and add it
/// back and the tips are what they were — and at that point nobody has
/// built the combination the changeset is now at, so a guard that
/// matched the cancelled row would refuse to start the run *and* leave
/// the changeset gated on a cancelled composed check with nothing able
/// to replace it. Add, remove, add used to block a changeset forever.
///
/// Narrow on purpose. A cancelled `push` or `change` run still counts:
/// there the cancellation is a person's decision or a newer commit's,
/// and restarting the same build behind their back is not what either
/// asked for. `blocked` counts in both — that is the fork placeholder,
/// and it is deliberately in the way.
pub fn run_for_commit(
    db: &ControlDb,
    repo_id: &str,
    event: &str,
    commit_sha: &str,
    change_key: Option<&str>,
    file: &str,
    composition: Option<&str>,
) -> Result<Option<Run>, String> {
    let row = db
        .lock()
        .query_opt(
            &format!(
                "SELECT {RUN_COLS} FROM workflow_runs \
                 WHERE repo_id = $1 AND event = $2 AND commit_sha = $3 \
                   AND change_key IS NOT DISTINCT FROM $4 AND file = $5 \
                   AND composition IS NOT DISTINCT FROM $6 \
                   AND ($6::text IS NULL OR state <> 'cancelled') \
                 ORDER BY created_at DESC, id DESC LIMIT 1"
            ),
            &[
                &repo_id,
                &event,
                &commit_sha,
                &change_key,
                &file,
                &composition,
            ],
        )
        .map_err(|e| format!("read workflow run for commit: {e}"))?;
    Ok(row.as_ref().map(row_to_run))
}

/// A repository's runs, newest first, optionally narrowed to one commit,
/// one change, or one kind of event.
///
/// The filters are in the query rather than in the caller because the
/// caller cannot do it correctly: a page that wants "the runs at this
/// change's tip" and gets them by fetching a window and keeping the
/// matches loses them the moment the repository is busy enough to push
/// the tip's runs past `limit` — and what it renders then is not an
/// error, it is a page that looks fine and is missing the button.
///
/// `event` exists for the same reason, for the Checks tab's "changeset
/// builds" panel: a member repository's composed runs — `event =
/// 'changeset'` — never mirror into `check_runs` (see
/// `changeset_checks`), so that panel is the only place on the
/// repository they are visible at all, and a busy repository's push
/// runs must not be able to hide them.
///
/// `NULL`-or-equal rather than a built-up `WHERE`, so there is one
/// statement and one plan for every combination.
pub fn runs_for_repo(
    db: &ControlDb,
    repo_id: &str,
    commit_sha: Option<&str>,
    change_key: Option<&str>,
    event: Option<&str>,
    limit: i64,
) -> Result<Vec<Run>, String> {
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {RUN_COLS} FROM workflow_runs WHERE repo_id = $1 \
                   AND ($2::TEXT IS NULL OR commit_sha = $2) \
                   AND ($3::TEXT IS NULL OR change_key = $3) \
                   AND ($4::TEXT IS NULL OR event = $4) \
                 ORDER BY created_at DESC, id DESC LIMIT $5"
            ),
            &[&repo_id, &commit_sha, &change_key, &event, &limit],
        )
        .map_err(|e| format!("list workflow runs: {e}"))?;
    Ok(rows.iter().map(row_to_run).collect())
}

pub fn set_member_tokens(
    db: &ControlDb,
    job_id: &str,
    token_ids: &[String],
) -> Result<Vec<String>, String> {
    if !valid_id(job_id) {
        return Ok(Vec::new());
    }
    let now = now_ms();
    db.lock()
        .transaction(|tx| {
            let previous: Vec<String> = match tx.query_opt(
                "SELECT member_token_ids FROM workflow_jobs WHERE id = $1 FOR UPDATE",
                &[&job_id],
            )? {
                Some(r) => r.get("member_token_ids"),
                None => return Ok(Vec::new()),
            };
            tx.execute(
                "UPDATE workflow_jobs SET member_token_ids = $2, updated_at = $3 WHERE id = $1",
                &[&job_id, &token_ids, &now],
            )?;
            Ok(previous)
        })
        .map_err(|e| format!("record composed job member tokens: {e}"))
}

/// Record that a job's task actually started, with the three handles the
/// rest of the system needs to reach it again.
///
/// One write rather than three, and guarded on `running`: a launch that
/// lands after the job was cancelled must not attach a live token to a
/// dead row. That is the whole reason this is not three setters — a
/// runner holding a token for a cancelled job is a credential nobody is
/// watching. (The token is also bound by [`mark_token`] before the
/// launch; restating it here keeps the row whole in one write.)
pub fn mark_launched(
    db: &ControlDb,
    job_id: &str,
    task_ref: &str,
    token_id: &str,
    check_run_id: &str,
) -> Result<bool, String> {
    db.lock()
        .execute(
            "UPDATE workflow_jobs SET task_ref = $2, token_id = $3, check_run_id = $4, \
               updated_at = $5 WHERE id = $1 AND state = 'running'",
            &[&job_id, &task_ref, &token_id, &check_run_id, &now_ms()],
        )
        .map(|n| n > 0)
        .map_err(|e| format!("mark workflow job launched: {e}"))
}

/// Note that a log chunk arrived, and treat it as a heartbeat.
///
/// `GREATEST` rather than `= $2`, because a chunk may be retried: the
/// runner uploads the object and then reports it, and a report that is
/// lost and re-sent must not walk the counter backwards and hide chunks
/// a reader has already been shown.
///
/// `None` means the job is not running any more, and the runner API
/// turns that into a **410** — which is the runner's signal to kill what
/// it is doing and exit. Renewing the lease here rather than in a
/// separate call is deliberate: a job that is producing output is a job
/// that is alive, and a build that logs steadily should never need to
/// heartbeat separately to keep its own claim.
pub fn record_log_chunk(
    db: &ControlDb,
    job_id: &str,
    seq: i32,
    lease_ms: i64,
) -> Result<Option<i64>, String> {
    if !valid_id(job_id) {
        return Ok(None);
    }
    let now = now_ms();
    let until = now + lease_ms;
    let row = db
        .lock()
        .query_opt(
            "UPDATE workflow_jobs SET log_chunks = GREATEST(log_chunks, $2), \
               lease_until = $3, updated_at = $4 \
             WHERE id = $1 AND state = 'running' RETURNING lease_until",
            &[&job_id, &seq, &until, &now],
        )
        .map_err(|e| format!("record log chunk: {e}"))?;
    Ok(row.map(|r| r.get("lease_until")))
}

/// Runs still going for a branch and trigger — what a new push supersedes.
pub fn live_runs_for_ref(
    db: &ControlDb,
    repo_id: &str,
    ref_name: &str,
    event: &str,
) -> Result<Vec<Run>, String> {
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {RUN_COLS} FROM workflow_runs \
                 WHERE repo_id = $1 AND ref_name = $2 AND event = $3 AND state = 'running' \
                 ORDER BY created_at DESC, id DESC"
            ),
            &[&repo_id, &ref_name, &event],
        )
        .map_err(|e| format!("live workflow runs for ref: {e}"))?;
    Ok(rows.iter().map(row_to_run).collect())
}

/// Runs still going for a change — what a new patchset supersedes.
pub fn live_runs_for_change(
    db: &ControlDb,
    repo_id: &str,
    change_key: &str,
) -> Result<Vec<Run>, String> {
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {RUN_COLS} FROM workflow_runs \
                 WHERE repo_id = $1 AND change_key = $2 AND state = 'running' \
                 ORDER BY created_at DESC, id DESC"
            ),
            &[&repo_id, &change_key],
        )
        .map_err(|e| format!("live workflow runs for change: {e}"))?;
    Ok(rows.iter().map(row_to_run).collect())
}

/// Runs still going for a changeset — what a recomposition supersedes.
///
/// `blocked` as well as `running`, which is the difference from
/// [`live_runs_for_change`] and not an oversight. A composed run over a
/// fork member sits `blocked` waiting for a maintainer, and if the
/// changeset is recomposed while it waits, that placeholder is a
/// standing offer to approve a combination that no longer exists.
/// Cancelling it is what makes the approval button mean the current
/// composition; leaving it would let a maintainer approve, in one press,
/// a build of tips nobody is proposing any more.
///
/// Not scoped to a repository: a changeset spans several, and its runs
/// are one set.
pub fn live_runs_for_changeset(db: &ControlDb, changeset_id: &str) -> Result<Vec<Run>, String> {
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {RUN_COLS} FROM workflow_runs \
                 WHERE changeset_id = $1 AND state IN ('running','blocked') \
                 ORDER BY created_at DESC, id DESC"
            ),
            &[&changeset_id],
        )
        .map_err(|e| format!("live workflow runs for changeset: {e}"))?;
    Ok(rows.iter().map(row_to_run).collect())
}

/// Stop a run: every job that had not finished, and the run itself.
///
/// Returns the jobs that were **running**, because those are the ones
/// with something burning CPU behind them and the caller has to go and
/// stop it. The queued ones need nothing beyond the row.
///
/// One transaction, and the `RETURNING` happens before the run is
/// settled, so two cancels racing cannot both come back with the same
/// job to stop: the second sees a run that is no longer `running` and
/// returns nothing at all. A settled run is a no-op rather than an
/// error — cancelling something that just finished is a race a person
/// loses all the time, and it is not a mistake worth a refusal.
pub fn cancel_run(db: &ControlDb, run_id: &str, reason: &str) -> Result<Vec<WorkflowJob>, String> {
    if !valid_id(run_id) {
        return Ok(Vec::new());
    }
    let now = now_ms();
    let rows = db
        .lock()
        .transaction(|tx| {
            // The run is claimed first. Nothing below runs unless this
            // transaction is the one that moved it out of the states a
            // run can still be cancelled from.
            //
            // `blocked` is one of them, and only a composed run ever
            // reaches here in it: a changeset recomposed while one of
            // its members waits for a fork approval leaves a placeholder
            // offering to run a combination nobody is proposing any
            // more, and a maintainer pressing approve on it would start
            // exactly that. Every other caller reads its runs from a
            // `running`-only query or refuses a non-running run at the
            // door, so this widening is reachable from the changeset
            // supersession and nowhere else.
            let settled = tx.execute(
                "UPDATE workflow_runs SET state = 'cancelled', error = $2, \
                   updated_at = $3, completed_at = $3 \
                 WHERE id = $1 AND state IN ('running','blocked')",
                &[&run_id, &reason, &now],
            )?;
            if settled == 0 {
                return Ok(Vec::new());
            }
            // Which jobs were running has to be read *before* the
            // update, because `RETURNING` gives the new row and after
            // the write every one of them says `cancelled`. `FOR UPDATE`
            // holds them for the statement below, so nothing can start
            // between the read and the write and escape the cancel.
            let running: Vec<String> = tx
                .query(
                    "SELECT id FROM workflow_jobs WHERE run_id = $1 AND state = 'running' \
                     FOR UPDATE",
                    &[&run_id],
                )?
                .iter()
                .map(|r| r.get::<_, String>("id"))
                .collect();
            tx.execute(
                "UPDATE workflow_jobs SET state = 'cancelled', error = $2, \
                   lease_until = NULL, updated_at = $3, completed_at = $3 \
                 WHERE run_id = $1 AND state IN ('queued','running')",
                &[&run_id, &reason, &now],
            )?;
            tx.query(
                &format!(
                    "SELECT {JOB_COLS} FROM workflow_jobs WHERE id = ANY($1) \
                     ORDER BY created_at, ordinal, id"
                ),
                &[&running],
            )
        })
        .map_err(|e| format!("cancel workflow run: {e}"))?;
    Ok(rows.iter().map(row_to_job).collect())
}

/// Running jobs that have outstayed their own `timeout_minutes`.
///
/// The timeout is read out of the job's frozen spec rather than
/// recomputed from `.weft/`, so a job runs under the limit the commit
/// asked for even if the file has since changed. `slack_ms` is the grace
/// the *dispatcher* adds on top: the runner enforces its own wall clock
/// and reports a timeout itself, and this sweep exists only for the case
/// where the runner is gone and cannot. Failing at exactly the same
/// instant as the runner would race it and produce two verdicts for one
/// job, the second of which is a lie.
///
/// The timeout is parsed **here** rather than in SQL, which is worth a
/// sentence because the SQL is shorter and wrong. `spec::jsonb ->>
/// 'timeout_minutes'` inside a `WHERE` makes every running job's spec a
/// cast that can fail, and one row with malformed JSON in it aborts the
/// whole statement — so a single bad spec would stop the timeout sweep
/// for every tenant on the instance, and the symptom would be builds
/// that hang forever rather than anything pointing at the row that did
/// it. Reading the running jobs and deciding in Rust cannot fail that
/// way: an unparseable spec falls back to the default, which is the
/// answer for a spec with no timeout in it anyway. The set is small —
/// it is bounded by the fleet's total concurrency, not by history.
pub fn overdue_jobs(db: &ControlDb, now: i64, slack_ms: i64) -> Result<Vec<WorkflowJob>, String> {
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {JOB_COLS} FROM workflow_jobs \
                 WHERE state = 'running' AND started_at IS NOT NULL \
                 ORDER BY started_at"
            ),
            &[],
        )
        .map_err(|e| format!("overdue workflow jobs: {e}"))?;
    Ok(rows
        .iter()
        .map(row_to_job)
        .filter(|j| {
            let started = j.started_at.unwrap_or(now);
            started + timeout_minutes(&j.spec) * 60_000 + slack_ms < now
        })
        .collect())
}

/// A job's wall-clock limit in minutes, from its frozen spec.
///
/// Anything the spec does not say — no key, a non-number, JSON that does
/// not parse at all — is the default rather than a refusal. This runs on
/// a sweep that decides whether to *fail somebody's build*, and the
/// worst available answer is to treat an unreadable spec as a timeout of
/// zero and kill every job the moment it starts.
fn timeout_minutes(spec: &str) -> i64 {
    serde_json::from_str::<serde_json::Value>(spec)
        .ok()
        .and_then(|v| v.get("timeout_minutes")?.as_i64())
        .filter(|&m| m > 0)
        .unwrap_or(DEFAULT_TIMEOUT_MINUTES)
}

/// What `workflow::model::Job` defaults `timeout_minutes` to. Six hours:
/// long enough that no honest build meets it, short enough that a job
/// whose runner died does not hold a slot for a day.
pub const DEFAULT_TIMEOUT_MINUTES: i64 = 360;

/// Every run still going in a repository, whatever caused it.
///
/// Deliberately unbounded and deliberately not `runs_for_repo` with a
/// limit: the caller is deleting the repository and has to stop *all* of
/// them, and a limit would quietly leave the oldest ones running on a
/// repository with a busy queue — which is the case where it matters.
/// The `state = 'running'` index makes the set small in practice however
/// long the history is.
pub fn live_runs_for_repo(db: &ControlDb, repo_id: &str) -> Result<Vec<Run>, String> {
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {RUN_COLS} FROM workflow_runs \
                 WHERE repo_id = $1 AND state = 'running' \
                 ORDER BY created_at DESC, id DESC"
            ),
            &[&repo_id],
        )
        .map_err(|e| format!("live workflow runs for repo: {e}"))?;
    Ok(rows.iter().map(row_to_run).collect())
}

/// Every run still going for an organisation, across all its
/// repositories — what a suspension has to stop.
pub fn live_runs_for_org(db: &ControlDb, org_id: &str) -> Result<Vec<Run>, String> {
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {RUN_COLS} FROM workflow_runs \
                 WHERE org_id = $1 AND state = 'running' \
                 ORDER BY created_at DESC, id DESC"
            ),
            &[&org_id],
        )
        .map_err(|e| format!("live workflow runs for org: {e}"))?;
    Ok(rows.iter().map(row_to_run).collect())
}

/// Every run recorded for one commit of one change, whatever its state.
///
/// The approval route's whole working set: which runs are `blocked` at
/// the tip a maintainer is approving, and — after the trigger has run —
/// which runs now exist to hand back.
pub fn runs_for_change_commit(
    db: &ControlDb,
    repo_id: &str,
    change_key: &str,
    commit_sha: &str,
) -> Result<Vec<Run>, String> {
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {RUN_COLS} FROM workflow_runs \
                 WHERE repo_id = $1 AND change_key = $2 AND commit_sha = $3 \
                 ORDER BY created_at, id"
            ),
            &[&repo_id, &change_key, &commit_sha],
        )
        .map_err(|e| format!("workflow runs for change commit: {e}"))?;
    Ok(rows.iter().map(row_to_run).collect())
}

/// Remove a `blocked` placeholder run. `false` if it was not blocked.
///
/// Narrow on purpose: this is the one row a run is allowed to lose, and
/// only because approving a fork's workflows replaces it with the real
/// run for the same commit and the same file. `run_for_commit` — the
/// trigger's idempotency guard — matches a run in **any** state, so the
/// placeholder has to be gone before the trigger is asked to start the
/// real one, or "one run per workflow file per commit" would silently
/// mean "the blocked one, forever".
///
/// Guarded on `state = 'blocked'` so it can never delete a run that ran:
/// a build's history is evidence, and the only run this can reach is one
/// that never started.
pub fn delete_blocked_run(db: &ControlDb, run_id: &str) -> Result<bool, String> {
    if !valid_id(run_id) {
        return Ok(false);
    }
    db.lock()
        .execute(
            "DELETE FROM workflow_runs WHERE id = $1 AND state = 'blocked'",
            &[&run_id],
        )
        .map(|n| n > 0)
        .map_err(|e| format!("delete blocked workflow run: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, NewRepo, RepoKind};

    /// "Some runner asks for work": the next job any organisation has
    /// ready, taken by a runner whose labels, group and policy admit
    /// every job these tests create. Most tests here are about the queue
    /// — readiness, leases, the cascade — rather than routing, which the
    /// tests that call [`claim_self_hosted`] directly cover.
    ///
    /// Organisations are asked in the order their oldest live job was
    /// queued, so across two of them this is first come, first served.
    fn claim(db: &ControlDb, lease_ms: i64) -> Result<Option<WorkflowJob>, String> {
        let orgs: Vec<String> = db
            .lock()
            .query(
                "SELECT org_id FROM workflow_jobs WHERE state IN ('queued', 'running') \
                 GROUP BY org_id ORDER BY MIN(created_at), org_id",
                &[],
            )
            .map_err(|e| e.to_string())?
            .iter()
            .map(|r| r.get(0))
            .collect();
        let labels = vec!["self-hosted".to_string()];
        for org in &orgs {
            let route = RunnerRoute {
                runner_id: "test-runner",
                org_id: org,
                group_id: "",
                labels: &labels,
                all_repos: true,
            };
            if let Some(job) = claim_self_hosted(db, &route, lease_ms)? {
                return Ok(Some(job));
            }
        }
        Ok(None)
    }

    /// A second organisation exists in every world, because the
    /// interesting property of the claim is what it does to *other*
    /// tenants and a single-org test cannot see it.
    fn world(hint: &str) -> (ControlDb, String, String, String, String) {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap();
        let mk = |name: &str| {
            let org = registry::create_org(&db, name).unwrap();
            let repo = registry::create_repo(
                &db,
                &org.id,
                &NewRepo {
                    name: "app",
                    kind: RepoKind::Native,
                    description: None,
                    default_branch: "main",
                    origin_url: None,
                    origin_provider: None,
                    origin_installation: None,
                },
            )
            .unwrap();
            (org.id, repo.id)
        };
        let (o1, r1) = mk("acme");
        let (o2, r2) = mk("globex");
        (db, o1, r1, o2, r2)
    }

    fn newrun<'a>(commit: &'a str) -> NewRun<'a> {
        NewRun {
            file: ".weft/ci.yml",
            name: "ci",
            commit_sha: commit,
            ref_name: Some("main"),
            event: "push",
            change_key: None,
            changeset_id: None,
            composition: None,
            from_fork: false,
        }
    }

    fn newjob<'a>(key: &'a str, needs: &'a [usize]) -> NewJob<'a> {
        NewJob {
            job_id: key,
            key,
            matrix: "{}",
            needs,
            spec: "{}",
            ..Default::default()
        }
    }

    fn state_of(db: &ControlDb, run_id: &str, key: &str) -> String {
        jobs_of(db, run_id)
            .unwrap()
            .into_iter()
            .find(|j| j.key == key)
            .unwrap_or_else(|| panic!("no job {key:?}"))
            .state
    }

    /// A job is not claimable until everything it needs has passed, and
    /// becomes claimable the moment the last one does — with no
    /// transition in between for anything to miss.
    #[test]
    fn a_job_waits_for_its_dependencies_and_starts_when_they_pass() {
        let (db, org, repo, _, _) = world("wf-deps");
        let wr = create_run(
            &db,
            &org,
            &repo,
            &newrun("a".repeat(40).as_str()),
            &[
                newjob("build", &[]),
                newjob("test", &[0]),
                newjob("ship", &[1]),
            ],
        )
        .unwrap();

        // Only `build` is ready; the other two are behind it.
        let first = claim(&db, 60_000).unwrap().expect("build is ready");
        assert_eq!(first.key, "build");
        assert!(
            claim(&db, 60_000).unwrap().is_none(),
            "nothing else may start while build is running"
        );

        finish(&db, &first.id, "passed", None, None).unwrap();
        let second = claim(&db, 60_000).unwrap().expect("test is ready now");
        assert_eq!(second.key, "test");
        assert!(claim(&db, 60_000).unwrap().is_none(), "ship still waits");

        finish(&db, &second.id, "passed", None, None).unwrap();
        let third = claim(&db, 60_000).unwrap().expect("ship is ready");
        assert_eq!(third.key, "ship");
        finish(&db, &third.id, "passed", None, None).unwrap();

        // Nothing left, and the run says so.
        assert!(claim(&db, 60_000).unwrap().is_none());
        assert_eq!(run(&db, &repo, &wr.id).unwrap().unwrap().state, "passed");
    }

    /// Fan-out and fan-in: two independent jobs may run together, and
    /// the one that needs both waits for both.
    #[test]
    fn a_fan_in_waits_for_every_branch() {
        let (db, org, repo, _, _) = world("wf-fan");
        let wr = create_run(
            &db,
            &org,
            &repo,
            &newrun(&"b".repeat(40)),
            &[
                newjob("linux", &[]),
                newjob("mac", &[]),
                newjob("ship", &[0, 1]),
            ],
        )
        .unwrap();

        let a = claim(&db, 60_000).unwrap().unwrap();
        let b = claim(&db, 60_000).unwrap().unwrap();
        assert_ne!(a.key, b.key, "both branches start");
        assert!(claim(&db, 60_000).unwrap().is_none(), "ship waits for both");

        finish(&db, &a.id, "passed", None, None).unwrap();
        assert!(
            claim(&db, 60_000).unwrap().is_none(),
            "one branch is not enough"
        );
        finish(&db, &b.id, "passed", None, None).unwrap();
        assert_eq!(claim(&db, 60_000).unwrap().unwrap().key, "ship");
        assert_eq!(run(&db, &repo, &wr.id).unwrap().unwrap().state, "running");
    }

    /// A failure cascades to everything downstream, transitively, and
    /// the run ends failed — but a sibling that does not depend on the
    /// failure still runs, because it is still real work whose answer
    /// somebody wants.
    #[test]
    fn a_failure_skips_what_depended_on_it_and_spares_what_did_not() {
        let (db, org, repo, _, _) = world("wf-fail");
        let wr = create_run(
            &db,
            &org,
            &repo,
            &newrun(&"c".repeat(40)),
            &[
                newjob("build", &[]),
                newjob("test", &[0]),
                newjob("ship", &[1]),
                newjob("lint", &[]),
            ],
        )
        .unwrap();

        let build = claim(&db, 60_000).unwrap().unwrap();
        assert_eq!(build.key, "build");
        finish(&db, &build.id, "failed", None, Some("compile error")).unwrap();

        // Both descendants, not only the direct one.
        assert_eq!(state_of(&db, &wr.id, "test"), "skipped");
        assert_eq!(state_of(&db, &wr.id, "ship"), "skipped");
        // And the reason is recorded where a reader will look.
        let skipped = jobs_of(&db, &wr.id)
            .unwrap()
            .into_iter()
            .find(|j| j.key == "ship")
            .unwrap();
        assert!(skipped.error.as_deref().unwrap().contains("did not pass"));

        // `lint` is independent and still runs.
        let lint = claim(&db, 60_000).unwrap().expect("lint is unaffected");
        assert_eq!(lint.key, "lint");
        assert_eq!(
            run(&db, &repo, &wr.id).unwrap().unwrap().state,
            "running",
            "the run is not over while lint is going"
        );
        finish(&db, &lint.id, "passed", None, None).unwrap();
        assert_eq!(
            run(&db, &repo, &wr.id).unwrap().unwrap().state,
            "failed",
            "one failure makes the run failed however the rest went"
        );
    }

    /// A dependent that is already running keeps its own verdict — the
    /// cascade must not throw away a result we are about to be given.
    #[test]
    fn a_cascade_does_not_overwrite_a_job_already_running() {
        let (db, org, repo, _, _) = world("wf-cascade-run");
        let wr = create_run(
            &db,
            &org,
            &repo,
            &newrun(&"d".repeat(40)),
            &[newjob("a", &[]), newjob("b", &[])],
        )
        .unwrap();
        let a = claim(&db, 60_000).unwrap().unwrap();
        let b = claim(&db, 60_000).unwrap().unwrap();
        finish(&db, &a.id, "failed", None, None).unwrap();
        assert_eq!(state_of(&db, &wr.id, &b.key), "running");
        finish(&db, &b.id, "passed", None, None).unwrap();
        assert_eq!(state_of(&db, &wr.id, &b.key), "passed");
    }

    /// Jobs start in the order the plan put them in, not in whatever
    /// order their ids happened to sort.
    ///
    /// Every job in a run is written in one transaction, in one
    /// millisecond, so `created_at` ties for all of them — and a ULID
    /// tiebreak is random within a millisecond. Without an explicit
    /// ordinal two runs of one workflow start their jobs in different
    /// orders, which makes comparing two runs a reading of the
    /// scheduler's mood. Found by a test that expected `build` and got
    /// `lint`.
    #[test]
    fn independent_jobs_start_in_plan_order_every_time() {
        let (db, org, repo, _, _) = world("wf-order");
        let wr = create_run(
            &db,
            &org,
            &repo,
            &newrun(&"07".repeat(20)),
            &[
                newjob("first", &[]),
                newjob("second", &[]),
                newjob("third", &[]),
                newjob("fourth", &[]),
            ],
        )
        .unwrap();
        let order: Vec<String> = (0..4)
            .map(|_| claim(&db, 60_000).unwrap().unwrap().key)
            .collect();
        assert_eq!(order, vec!["first", "second", "third", "fourth"]);
        // And the same order when they are read back.
        let listed: Vec<String> = jobs_of(&db, &wr.id)
            .unwrap()
            .into_iter()
            .map(|j| j.key)
            .collect();
        assert_eq!(listed, vec!["first", "second", "third", "fourth"]);
    }

    /// A runner that dies holds its job only until the lease expires —
    /// otherwise one crashed machine wedges the run until somebody
    /// notices, which is exactly the failure a lease exists to prevent.
    #[test]
    fn a_dead_lease_hands_the_job_out_again() {
        let (db, org, repo, _, _) = world("wf-lease");
        create_run(
            &db,
            &org,
            &repo,
            &newrun(&"a1".repeat(20)),
            &[newjob("a", &[]), newjob("b", &[])],
        )
        .unwrap();

        // Claimed with a lease that is already over.
        let a = claim(&db, -1).unwrap().unwrap();
        assert_eq!(a.attempts, 1);
        // A dead lease holds nothing, so work continues rather than
        // stopping.
        let again = claim(&db, 60_000).unwrap().expect("the lease is dead");
        assert!(
            again.id == a.id || again.key == "b",
            "either the abandoned job is retried or the next one starts"
        );
    }

    /// A verdict for a job that is not running is ignored rather than
    /// applied: a duplicate report, or a runner still talking after its
    /// job was cancelled, must not overwrite what was recorded.
    #[test]
    fn a_late_or_duplicate_verdict_cannot_overwrite_one_already_given() {
        let (db, org, repo, _, _) = world("wf-late");
        let wr = create_run(
            &db,
            &org,
            &repo,
            &newrun(&"b2".repeat(20)),
            &[newjob("only", &[])],
        )
        .unwrap();
        let j = claim(&db, 60_000).unwrap().unwrap();
        finish(&db, &j.id, "passed", None, None).unwrap();
        // A second, contradictory report.
        finish(&db, &j.id, "failed", None, Some("too late")).unwrap();
        assert_eq!(state_of(&db, &wr.id, "only"), "passed");
        assert_eq!(run(&db, &repo, &wr.id).unwrap().unwrap().state, "passed");
    }

    #[test]
    fn a_run_is_refused_rather_than_written_half_formed() {
        let (db, org, repo, _, _) = world("wf-bad");
        assert!(create_run(&db, &org, &repo, &newrun(&"c3".repeat(20)), &[]).is_err());
        // An edge pointing past the end is a planner bug, and writing it
        // would make a job that can never become ready.
        let bad = create_run(
            &db,
            &org,
            &repo,
            &newrun(&"c3".repeat(20)),
            &[newjob("a", &[7])],
        );
        assert!(bad.unwrap_err().contains("not here"));
        // Neither attempt left a row behind.
        assert!(run(&db, &repo, "01nope").unwrap().is_none());
    }

    #[test]
    fn a_run_is_only_readable_through_the_repository_that_owns_it() {
        let (db, o1, r1, _, r2) = world("wf-scope");
        let wr = create_run(
            &db,
            &o1,
            &r1,
            &newrun(&"d4".repeat(20)),
            &[newjob("a", &[])],
        )
        .unwrap();
        assert!(run(&db, &r1, &wr.id).unwrap().is_some());
        assert!(
            run(&db, &r2, &wr.id).unwrap().is_none(),
            "another repository's run is indistinguishable from one that does not exist"
        );
        // A malformed id is the same answer, not a database error.
        assert!(run(&db, &r1, "../../etc/passwd").unwrap().is_none());
    }

    #[test]
    fn a_heartbeat_keeps_a_claim_and_cannot_resurrect_a_finished_job() {
        let (db, org, repo, _, _) = world("wf-renew");
        create_run(
            &db,
            &org,
            &repo,
            &newrun(&"e5".repeat(20)),
            &[newjob("a", &[])],
        )
        .unwrap();
        let j = claim(&db, 60_000).unwrap().unwrap();
        assert!(renew(&db, &j.id, 60_000).unwrap(), "a live claim renews");
        finish(&db, &j.id, "passed", None, None).unwrap();
        assert!(
            !renew(&db, &j.id, 60_000).unwrap(),
            "a finished job does not renew"
        );
    }

    #[test]
    fn a_verdict_that_is_not_a_verdict_is_refused() {
        let (db, org, repo, _, _) = world("wf-verdict");
        create_run(
            &db,
            &org,
            &repo,
            &newrun(&"f6".repeat(20)),
            &[newjob("a", &[])],
        )
        .unwrap();
        let j = claim(&db, 60_000).unwrap().unwrap();
        assert!(finish(&db, &j.id, "queued", None, None).is_err());
        assert!(finish(&db, &j.id, "banana", None, None).is_err());
        // A malformed id is a no-op, not an error and not a query.
        assert!(finish(&db, "../x", "passed", None, None).is_ok());
        assert_eq!(jobs_of(&db, "../x").unwrap().len(), 0);
    }

    // -----------------------------------------------------------------
    // What the dispatcher and the runner API need on top of the queue
    // -----------------------------------------------------------------

    /// The migration really did widen the run's CHECK constraint.
    ///
    /// This is the test the constraint-name guess rests on: 0028 wrote
    /// the check inline, so its name is whatever Postgres generated, and
    /// 0029 drops it by that name. If the name were wrong the drop would
    /// have failed and no test would run at all — but if the *rewrite*
    /// were wrong (a second constraint added beside the first, say) every
    /// other test would still pass and only this one would fail.
    #[test]
    fn a_run_can_be_created_already_settled_or_blocked() {
        let (db, org, repo, _, _) = world("wf-settled");
        let refused = create_settled_run(
            &db,
            &org,
            &repo,
            &newrun(&"1a".repeat(20)),
            "failed",
            Some("workflow file is not usable"),
            None,
        )
        .unwrap();
        assert_eq!(refused.state, "failed");
        assert_eq!(
            refused.error.as_deref(),
            Some("workflow file is not usable")
        );
        assert!(refused.completed_at.is_some(), "a failed run is over");
        // And it reads back through the repository-scoped door.
        let back = run(&db, &repo, &refused.id).unwrap().unwrap();
        assert_eq!(back.error, refused.error);
        assert!(jobs_of(&db, &refused.id).unwrap().is_empty());

        let blocked = create_settled_run(
            &db,
            &org,
            &repo,
            &newrun(&"2b".repeat(20)),
            "blocked",
            Some("a maintainer has not approved this fork's change"),
            Some(BlockedReason::Fork),
        )
        .unwrap();
        assert_eq!(blocked.state, "blocked");
        assert!(
            blocked.completed_at.is_none(),
            "blocked is waiting, not finished — stamping it would make a \
             run that has not happened look like one that has"
        );

        // A state that is not one of the three is refused rather than
        // handed to the CHECK constraint as a 500.
        assert!(
            create_settled_run(&db, &org, &repo, &newrun("aa"), "running", None, None).is_err()
        );
        assert!(create_settled_run(&db, &org, &repo, &newrun("aa"), "passed", None, None).is_err());
        // A code is a property of `blocked` and of nothing else, in
        // both directions: a blocked run a dashboard cannot classify,
        // and a classification on a run that is not waiting, are both
        // refused here rather than discovered by whoever reads them.
        assert!(
            create_settled_run(&db, &org, &repo, &newrun("aa"), "blocked", Some("x"), None)
                .is_err()
        );
        assert!(create_settled_run(
            &db,
            &org,
            &repo,
            &newrun("aa"),
            "failed",
            Some("x"),
            Some(BlockedReason::Fork)
        )
        .is_err());
        // And `create_run` still refuses to make a *running* run with no
        // jobs, which is the thing that could never complete.
        assert!(create_run(&db, &org, &repo, &newrun("aa"), &[]).is_err());
    }

    #[test]
    fn a_job_and_a_run_are_readable_by_id_and_a_repository_lists_its_runs() {
        let (db, o1, r1, o2, r2) = world("wf-byid");
        let first = create_run(
            &db,
            &o1,
            &r1,
            &newrun(&"3c".repeat(20)),
            &[newjob("a", &[])],
        )
        .unwrap();
        let second = create_run(
            &db,
            &o1,
            &r1,
            &newrun(&"4d".repeat(20)),
            &[newjob("a", &[])],
        )
        .unwrap();
        create_run(
            &db,
            &o2,
            &r2,
            &newrun(&"5e".repeat(20)),
            &[newjob("a", &[])],
        )
        .unwrap();

        assert_eq!(run_by_id(&db, &first.id).unwrap().unwrap().id, first.id);
        assert!(run_by_id(&db, "01nothing").unwrap().is_none());
        assert!(
            run_by_id(&db, "../../etc/passwd").unwrap().is_none(),
            "a malformed id is absent, not a query"
        );

        // Newest first, and only this repository's.
        let listed = runs_for_repo(&db, &r1, None, None, None, 20).unwrap();
        assert_eq!(
            listed.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            vec![second.id.clone(), first.id.clone()]
        );
        assert_eq!(
            runs_for_repo(&db, &r1, None, None, None, 1).unwrap().len(),
            1,
            "limit applies"
        );

        let j = jobs_of(&db, &first.id).unwrap().remove(0);
        let read = job(&db, &j.id).unwrap().expect("the job by id");
        assert_eq!(read.run_id, first.id);
        assert_eq!(read.log_chunks, 0);
        assert!(read.token_id.is_none() && read.task_ref.is_none());
        assert_eq!(read.spec, "{}");
        assert!(job(&db, "01nothing").unwrap().is_none());
        assert!(job(&db, "../x").unwrap().is_none());
    }

    /// Launch handles only attach to a job that is still running — a
    /// launch landing after a cancel must not hand a live token to a row
    /// nobody is watching.
    #[test]
    fn launch_handles_attach_only_while_the_job_is_running() {
        let (db, org, repo, _, _) = world("wf-launch");
        create_run(
            &db,
            &org,
            &repo,
            &newrun(&"6f".repeat(20)),
            &[newjob("a", &[])],
        )
        .unwrap();
        let j = claim(&db, 60_000).unwrap().unwrap();
        assert!(mark_launched(&db, &j.id, "pid:42", "tok1", "chk1").unwrap());
        let read = job(&db, &j.id).unwrap().unwrap();
        assert_eq!(read.task_ref.as_deref(), Some("pid:42"));
        assert_eq!(read.token_id.as_deref(), Some("tok1"));
        assert_eq!(read.check_run_id.as_deref(), Some("chk1"));

        finish(&db, &j.id, "passed", None, None).unwrap();
        assert!(
            !mark_launched(&db, &j.id, "pid:99", "tok2", "chk2").unwrap(),
            "a settled job takes no new token"
        );
        assert_eq!(
            job(&db, &j.id).unwrap().unwrap().token_id.as_deref(),
            Some("tok1"),
            "and the late launch changed nothing"
        );
    }

    /// A composed job's member tokens swap as a set, and the swap hands
    /// back exactly what is no longer recorded — which is what makes the
    /// caller's revoke unconditional.
    ///
    /// The re-fetch is the case that matters: a runner asking for its
    /// spec twice gets a fresh set both times, and the first set is
    /// otherwise live read credentials on *other people's* repositories
    /// that nothing is watching.
    #[test]
    fn member_tokens_swap_as_a_set_and_return_the_ones_they_replaced() {
        let (db, org, repo, _, _) = world("wf-member-tokens");
        create_run(
            &db,
            &org,
            &repo,
            &newrun(&"6a".repeat(20)),
            &[newjob("a", &[])],
        )
        .unwrap();
        let j = claim(&db, 60_000).unwrap().unwrap();
        assert!(
            j.member_token_ids.is_empty(),
            "an ordinary job carries none"
        );

        let first = vec!["tok-web".to_string(), "tok-cli".to_string()];
        assert!(
            set_member_tokens(&db, &j.id, &first).unwrap().is_empty(),
            "the first spec fetch replaces nothing"
        );
        assert_eq!(job(&db, &j.id).unwrap().unwrap().member_token_ids, first);

        // A second fetch: the previous set comes back, and only the new
        // one is recorded. Losing this is a credential leak, not a
        // bookkeeping slip.
        let second = vec!["tok-web-2".to_string()];
        assert_eq!(set_member_tokens(&db, &j.id, &second).unwrap(), first);
        assert_eq!(job(&db, &j.id).unwrap().unwrap().member_token_ids, second);

        // Clearing returns what was there, so a caller that is finishing
        // a job can revoke by the same one call.
        assert_eq!(set_member_tokens(&db, &j.id, &[]).unwrap(), second);
        assert!(job(&db, &j.id)
            .unwrap()
            .unwrap()
            .member_token_ids
            .is_empty());

        // A job that is not there is not an error and returns nothing:
        // the caller is revoking, and there is nothing to revoke.
        assert!(set_member_tokens(&db, "01nothing", &first)
            .unwrap()
            .is_empty());
        assert!(set_member_tokens(&db, "not an id\0", &first)
            .unwrap()
            .is_empty());
    }

    /// A log chunk is a heartbeat, a retry is idempotent, and once the
    /// job is over the answer is `None` — which the runner API turns into
    /// the 410 that tells a runner to stop.
    #[test]
    fn a_log_chunk_renews_the_lease_and_stops_being_accepted_when_the_job_is_over() {
        let (db, org, repo, _, _) = world("wf-chunk");
        create_run(
            &db,
            &org,
            &repo,
            &newrun(&"8b".repeat(20)),
            &[newjob("a", &[])],
        )
        .unwrap();
        let j = claim(&db, 1_000).unwrap().unwrap();

        let until = record_log_chunk(&db, &j.id, 1, 60_000)
            .unwrap()
            .expect("a running job takes chunks");
        assert!(until > now_ms() + 50_000, "the lease was pushed out");
        record_log_chunk(&db, &j.id, 2, 60_000).unwrap().unwrap();
        assert_eq!(job(&db, &j.id).unwrap().unwrap().log_chunks, 2);

        // A retry of an earlier chunk must not walk the counter back and
        // hide chunks a reader has already been shown.
        record_log_chunk(&db, &j.id, 1, 60_000).unwrap().unwrap();
        assert_eq!(job(&db, &j.id).unwrap().unwrap().log_chunks, 2);

        finish(&db, &j.id, "passed", None, None).unwrap();
        assert!(
            record_log_chunk(&db, &j.id, 3, 60_000).unwrap().is_none(),
            "a finished job takes no more output"
        );
        assert_eq!(job(&db, &j.id).unwrap().unwrap().log_chunks, 2);
        assert!(record_log_chunk(&db, "../x", 1, 60_000).unwrap().is_none());
    }

    /// What a new push supersedes: only runs that are still going, only
    /// for this ref and this trigger, only in this repository.
    #[test]
    fn live_runs_are_the_ones_still_going_for_that_ref_or_change() {
        let (db, o1, r1, o2, r2) = world("wf-live");
        let mk = |org: &str, repo: &str, sha: &str, r: Option<&str>, ev: &str, ch: Option<&str>| {
            let nr = NewRun {
                file: ".weft/ci.yml",
                name: "ci",
                commit_sha: sha,
                ref_name: r,
                event: ev,
                change_key: ch,
                changeset_id: None,
                composition: None,
                from_fork: false,
            };
            create_run(&db, org, repo, &nr, &[newjob("a", &[])]).unwrap()
        };
        let live = mk(&o1, &r1, &"9c".repeat(20), Some("main"), "push", None);
        let other_branch = mk(&o1, &r1, &"9d".repeat(20), Some("dev"), "push", None);
        let done = mk(&o1, &r1, &"9e".repeat(20), Some("main"), "push", None);
        let by_change = mk(
            &o1,
            &r1,
            &"9f".repeat(20),
            Some("main"),
            "change",
            Some("Ic0ffee"),
        );
        mk(&o2, &r2, &"9a".repeat(20), Some("main"), "push", None);

        // Settle one of the two `main` push runs.
        let dj = jobs_of(&db, &done.id).unwrap().remove(0);
        while let Some(c) = claim(&db, 60_000).unwrap() {
            if c.id == dj.id {
                finish(&db, &c.id, "passed", None, None).unwrap();
            }
        }

        let ids: Vec<String> = live_runs_for_ref(&db, &r1, "main", "push")
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(
            ids,
            vec![live.id.clone()],
            "not {other_branch:?} or a settled one"
        );

        let ids: Vec<String> = live_runs_for_change(&db, &r1, "Ic0ffee")
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec![by_change.id]);
        assert!(live_runs_for_change(&db, &r1, "Inothing")
            .unwrap()
            .is_empty());
        assert!(
            live_runs_for_ref(&db, &r2, "main", "push")
                .unwrap()
                .iter()
                .all(|r| r.repo_id == r2),
            "another tenant's run is never in this answer"
        );
    }

    /// Cancelling stops everything unfinished, hands back only the jobs
    /// that had something running behind them, and is a no-op the second
    /// time — because a person cancelling a run that just finished is a
    /// race they lose all the time, not a mistake worth a refusal.
    #[test]
    fn cancelling_a_run_stops_what_was_unfinished_and_names_what_was_running() {
        let (db, org, repo, _, _) = world("wf-cancel");
        let wr = create_run(
            &db,
            &org,
            &repo,
            &newrun(&"aa".repeat(20)),
            &[
                newjob("one", &[]),
                newjob("two", &[]),
                newjob("three", &[]),
                newjob("four", &[2]),
            ],
        )
        .unwrap();
        let a = claim(&db, 60_000).unwrap().unwrap();
        let b = claim(&db, 60_000).unwrap().unwrap();
        finish(&db, &a.id, "passed", None, None).unwrap();
        // `a` is done, `b` is running, `three` is queued, `four` waits.

        let stopped = cancel_run(&db, &wr.id, "cancelled by Ada").unwrap();
        assert_eq!(
            stopped.iter().map(|j| j.key.clone()).collect::<Vec<_>>(),
            vec![b.key.clone()],
            "only the job with a task behind it needs stopping"
        );
        assert_eq!(stopped[0].state, "cancelled", "already written, not a plan");

        let after: Vec<(String, String)> = jobs_of(&db, &wr.id)
            .unwrap()
            .into_iter()
            .map(|j| (j.key, j.state))
            .collect();
        assert_eq!(
            after,
            vec![
                (a.key.clone(), "passed".to_string()),
                (b.key.clone(), "cancelled".to_string()),
                ("three".to_string(), "cancelled".to_string()),
                ("four".to_string(), "cancelled".to_string()),
            ],
            "a finished job keeps its verdict; everything unfinished stops"
        );
        let run_row = run(&db, &repo, &wr.id).unwrap().unwrap();
        assert_eq!(run_row.state, "cancelled");
        assert_eq!(run_row.error.as_deref(), Some("cancelled by Ada"));
        assert!(run_row.completed_at.is_some());
        // The reason reached the jobs too, so a reader looking at one red
        // cell does not have to go up a level to find out why.
        assert_eq!(
            job(&db, &b.id).unwrap().unwrap().error.as_deref(),
            Some("cancelled by Ada")
        );

        // Second time: nothing to stop, nothing overwritten, no error.
        assert!(cancel_run(&db, &wr.id, "cancelled by someone else")
            .unwrap()
            .is_empty());
        assert_eq!(
            run(&db, &repo, &wr.id).unwrap().unwrap().error.as_deref(),
            Some("cancelled by Ada")
        );
        // Nothing may be claimed out of it afterwards.
        assert!(claim(&db, 60_000).unwrap().is_none());
        // A malformed id is empty, not a query.
        assert!(cancel_run(&db, "../x", "no").unwrap().is_empty());
    }

    /// A run that already passed is not re-opened by a cancel.
    #[test]
    fn cancelling_a_settled_run_changes_nothing() {
        let (db, org, repo, _, _) = world("wf-cancel-settled");
        let wr = create_run(
            &db,
            &org,
            &repo,
            &newrun(&"bb".repeat(20)),
            &[newjob("a", &[])],
        )
        .unwrap();
        let j = claim(&db, 60_000).unwrap().unwrap();
        finish(&db, &j.id, "passed", None, None).unwrap();
        assert!(cancel_run(&db, &wr.id, "too late").unwrap().is_empty());
        let after = run(&db, &repo, &wr.id).unwrap().unwrap();
        assert_eq!(after.state, "passed");
        assert!(after.error.is_none());
        assert_eq!(job(&db, &j.id).unwrap().unwrap().state, "passed");
    }

    /// The timeout comes out of the job's own frozen spec, and the sweep
    /// waits `slack` beyond it — the runner enforces its own clock first,
    /// and firing at the same instant would race it into two verdicts for
    /// one job, the second of which is a lie.
    #[test]
    fn overdue_is_measured_against_the_spec_the_job_was_created_with() {
        let (db, org, repo, _, _) = world("wf-overdue");
        create_run(
            &db,
            &org,
            &repo,
            &newrun(&"cc".repeat(20)),
            &[
                NewJob {
                    job_id: "quick",
                    key: "quick",
                    matrix: "{}",
                    needs: &[],
                    spec: r#"{"timeout_minutes":10}"#,
                    ..Default::default()
                },
                NewJob {
                    job_id: "slow",
                    key: "slow",
                    matrix: "{}",
                    needs: &[],
                    spec: r#"{"timeout_minutes":600}"#,
                    ..Default::default()
                },
                // No `timeout_minutes` at all: the 360-minute default.
                newjob("default", &[]),
            ],
        )
        .unwrap();
        let claimed: Vec<WorkflowJob> = (0..3)
            .map(|_| claim(&db, 60_000).unwrap().unwrap())
            .collect();

        // Pretend all three started twenty minutes ago.
        let started = now_ms() - 20 * 60_000;
        for j in &claimed {
            db.lock()
                .execute(
                    "UPDATE workflow_jobs SET started_at = $2 WHERE id = $1",
                    &[&j.id, &started],
                )
                .unwrap();
        }

        let now = now_ms();
        let over: Vec<String> = overdue_jobs(&db, now, 0)
            .unwrap()
            .into_iter()
            .map(|j| j.key)
            .collect();
        assert_eq!(
            over,
            vec!["quick"],
            "only the ten-minute job is past its limit"
        );

        // Slack is real: five minutes of it still catches a job ten
        // minutes over, an hour of it does not.
        assert_eq!(overdue_jobs(&db, now, 5 * 60_000).unwrap().len(), 1);
        assert!(
            overdue_jobs(&db, now, 60 * 60_000).unwrap().is_empty(),
            "slack past the overrun holds the sweep off"
        );

        // A job that is no longer running is never overdue, however long
        // ago it started.
        let quick = claimed.iter().find(|j| j.key == "quick").unwrap();
        finish(&db, &quick.id, "failed", None, Some("slow")).unwrap();
        assert!(overdue_jobs(&db, now, 0).unwrap().is_empty());

        // And a queued job — never started — is not swept either.
        create_run(
            &db,
            &org,
            &repo,
            &newrun(&"dd".repeat(20)),
            &[newjob("waiting", &[])],
        )
        .unwrap();
        assert!(overdue_jobs(&db, now + 10_000_000_000, 0)
            .unwrap()
            .iter()
            .all(|j| j.key != "waiting"));
    }

    /// One commit, one run per workflow file — because the same commit
    /// legitimately arrives more than once (a branch pushed twice with
    /// nothing new, a retried webhook) and a run per delivery fills the
    /// Checks tab with duplicates of one build.
    #[test]
    fn a_commit_finds_the_run_its_own_trigger_already_made() {
        let (db, org, repo, _, r2) = world("wf-forcommit");
        let sha = "ee".repeat(20);
        let push = NewRun {
            file: ".weft/ci.yml",
            name: "ci",
            commit_sha: &sha,
            ref_name: Some("main"),
            event: "push",
            change_key: None,
            changeset_id: None,
            composition: None,
            from_fork: false,
        };
        let made = create_run(&db, &org, &repo, &push, &[newjob("a", &[])]).unwrap();

        // The NULL change_key is the case `=` would silently miss, and
        // it is the most common one.
        assert_eq!(
            run_for_commit(&db, &repo, "push", &sha, None, ".weft/ci.yml", None)
                .unwrap()
                .unwrap()
                .id,
            made.id
        );
        // A second workflow file at the same commit is a different run.
        assert!(
            run_for_commit(&db, &repo, "push", &sha, None, ".weft/nightly.yml", None)
                .unwrap()
                .is_none()
        );
        // As is the same file for a different trigger, or a change.
        assert!(
            run_for_commit(&db, &repo, "change", &sha, None, ".weft/ci.yml", None)
                .unwrap()
                .is_none()
        );
        assert!(run_for_commit(
            &db,
            &repo,
            "push",
            &sha,
            Some("Ideadbeef"),
            ".weft/ci.yml",
            None
        )
        .unwrap()
        .is_none());
        // And never another repository's.
        assert!(
            run_for_commit(&db, &r2, "push", &sha, None, ".weft/ci.yml", None)
                .unwrap()
                .is_none()
        );

        let change = NewRun {
            event: "change",
            change_key: Some("Ideadbeef"),
            ..push.clone()
        };
        let made2 = create_run(&db, &org, &repo, &change, &[newjob("a", &[])]).unwrap();
        assert_eq!(
            run_for_commit(
                &db,
                &repo,
                "change",
                &sha,
                Some("Ideadbeef"),
                ".weft/ci.yml",
                None
            )
            .unwrap()
            .unwrap()
            .id,
            made2.id
        );
    }

    /// A composition that was cancelled has not been built.
    ///
    /// The whole bug in one test: remove a member and the composition is
    /// superseded and cancelled; add it back and the composition is what
    /// it was, with nothing built against it. A guard that counted the
    /// cancelled row would never start the replacement, and the
    /// changeset would sit blocked on a cancelled composed check with no
    /// route that could rerun it.
    ///
    /// The other two halves are what keeps the fix narrow: a cancelled
    /// **push** still counts — somebody or a newer commit stopped that
    /// build on purpose — and a **blocked** composed run still counts,
    /// because that is the fork placeholder and it is meant to be in the
    /// way.
    #[test]
    fn a_cancelled_composition_is_not_built_where_a_cancelled_push_is() {
        let (db, org, repo, _, _) = world("wf-recompose");
        let sha = "cd".repeat(20);
        let composed = NewRun {
            file: ".weft/ci.yml",
            name: "ci",
            commit_sha: &sha,
            ref_name: Some("main"),
            event: "changeset",
            change_key: Some("Iapi"),
            changeset_id: None,
            composition: Some("c1"),
            from_fork: false,
        };
        let found = |event: &str, file: &str, composition: Option<&str>| {
            run_for_commit(&db, &repo, event, &sha, Some("Iapi"), file, composition)
                .unwrap()
                .map(|r| r.id)
        };
        let made = create_run(&db, &org, &repo, &composed, &[newjob("a", &[])]).unwrap();
        assert_eq!(
            found("changeset", ".weft/ci.yml", Some("c1")).as_deref(),
            Some(made.id.as_str())
        );
        cancel_run(&db, &made.id, "superseded by a new composition").unwrap();
        assert_eq!(
            found("changeset", ".weft/ci.yml", Some("c1")),
            None,
            "a composition that came back has not been built by the run that was cancelled"
        );

        // A cancelled push at the same commit is still that commit's run.
        let push = NewRun {
            event: "push",
            change_key: None,
            composition: None,
            ..composed.clone()
        };
        let pushed = create_run(&db, &org, &repo, &push, &[newjob("a", &[])]).unwrap();
        cancel_run(&db, &pushed.id, "asked to stop").unwrap();
        assert_eq!(
            run_for_commit(&db, &repo, "push", &sha, None, ".weft/ci.yml", None)
                .unwrap()
                .map(|r| r.id)
                .as_deref(),
            Some(pushed.id.as_str())
        );

        // And a blocked composed placeholder is still in the way.
        let held = create_settled_run(
            &db,
            &org,
            &repo,
            &NewRun {
                file: ".weft/held.yml",
                ..composed.clone()
            },
            "blocked",
            Some("this change comes from a fork"),
            Some(BlockedReason::Fork),
        )
        .unwrap();
        assert_eq!(
            found("changeset", ".weft/held.yml", Some("c1")).as_deref(),
            Some(held.id.as_str())
        );
    }

    /// A spec the sweep cannot read is the *default* timeout, not zero
    /// and not a refusal.
    ///
    /// Written as a unit test as well as through the query below,
    /// because this decides whether to fail somebody's build: reading an
    /// unparseable spec as a timeout of zero would kill every job the
    /// instant it started, and that failure has no red test unless
    /// somebody writes this one.
    #[test]
    fn an_unreadable_timeout_falls_back_to_the_default_rather_than_to_zero() {
        assert_eq!(timeout_minutes(r#"{"timeout_minutes":10}"#), 10);
        for spec in [
            "{}",
            "",
            "not json at all",
            r#"{"timeout_minutes":null}"#,
            r#"{"timeout_minutes":"30"}"#,
            r#"{"timeout_minutes":0}"#,
            r#"{"timeout_minutes":-5}"#,
            r#"["a","list"]"#,
        ] {
            assert_eq!(
                timeout_minutes(spec),
                DEFAULT_TIMEOUT_MINUTES,
                "spec {spec:?} must not shorten anybody's build"
            );
        }
    }

    /// And the whole-fleet version of the same property: one job with a
    /// malformed spec must not stop the sweep for everybody else.
    ///
    /// This is the bug the SQL version of `overdue_jobs` had. Casting
    /// `spec::jsonb` inside the `WHERE` makes one bad row abort the
    /// statement, so a single unparseable spec would leave every
    /// overdue job on the instance running forever — and the symptom is
    /// builds that hang, with nothing pointing at the row that caused
    /// it.
    #[test]
    fn one_unreadable_spec_does_not_stop_the_sweep_for_every_other_job() {
        let (db, org, repo, _, _) = world("wf-overdue-bad");
        create_run(
            &db,
            &org,
            &repo,
            &newrun(&"fa".repeat(20)),
            &[
                NewJob {
                    job_id: "broken",
                    key: "broken",
                    matrix: "{}",
                    needs: &[],
                    spec: "}{ not json",
                    ..Default::default()
                },
                NewJob {
                    job_id: "fine",
                    key: "fine",
                    matrix: "{}",
                    needs: &[],
                    spec: r#"{"timeout_minutes":1}"#,
                    ..Default::default()
                },
            ],
        )
        .unwrap();
        let claimed: Vec<WorkflowJob> = (0..2)
            .map(|_| claim(&db, 60_000).unwrap().unwrap())
            .collect();
        let started = now_ms() - 120 * 60_000;
        for j in &claimed {
            db.lock()
                .execute(
                    "UPDATE workflow_jobs SET started_at = $2 WHERE id = $1",
                    &[&j.id, &started],
                )
                .unwrap();
        }

        // The sweep answers rather than erroring, and it names the job
        // that is genuinely overdue.
        let over: Vec<String> = overdue_jobs(&db, now_ms(), 0)
            .unwrap()
            .into_iter()
            .map(|j| j.key)
            .collect();
        assert_eq!(
            over,
            vec!["fine"],
            "the unreadable spec keeps the six-hour default and is not yet overdue"
        );
    }

    /// Everything still going in a repository, with no limit and no
    /// filter but the state — what a deletion has to stop.
    #[test]
    fn a_repository_reports_every_run_it_still_has_going() {
        let (db, org, repo, _, _) = world("wf-liverepo");
        let live = create_run(
            &db,
            &org,
            &repo,
            &newrun(&"8f".repeat(20)),
            &[newjob("a", &[])],
        )
        .unwrap();
        let settled = create_settled_run(
            &db,
            &org,
            &repo,
            &newrun(&"9f".repeat(20)),
            "failed",
            Some("no"),
            None,
        )
        .unwrap();
        // A second live one, on a different branch and a different
        // event: the ref-scoped and change-scoped lookups would each
        // miss one of these, which is the reason this exists.
        let sha3 = "af".repeat(20);
        let mut other_ref = newrun(&sha3);
        other_ref.ref_name = Some("release");
        other_ref.event = "change";
        other_ref.change_key = Some("C-1");
        let live2 = create_run(&db, &org, &repo, &other_ref, &[newjob("b", &[])])
            .unwrap()
            .id;

        let ids: Vec<String> = live_runs_for_repo(&db, &repo)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert!(ids.contains(&live.id) && ids.contains(&live2));
        assert!(
            !ids.contains(&settled.id),
            "a run that has already finished is not still going"
        );
        assert_eq!(ids.len(), 2);

        // Another repository's live run is not this one's business.
        let (_, _, repo2, _, _) = world("wf-liverepo2");
        assert!(live_runs_for_repo(&db, &repo2).unwrap().is_empty());
    }

    /// Only a `blocked` run may be deleted, and a run that ran never
    /// can.
    ///
    /// Approving a fork's workflows replaces the placeholder with the
    /// real run for the same commit and file, so the placeholder has to
    /// go — but a build's history is evidence, and this is the one door
    /// that can remove a run at all. It is guarded in SQL rather than by
    /// its caller for that reason.
    #[test]
    fn only_a_blocked_run_can_be_deleted() {
        let (db, org, repo, _, _) = world("wf-delete-blocked");
        let blocked = create_settled_run(
            &db,
            &org,
            &repo,
            &newrun(&"a".repeat(40)),
            "blocked",
            Some("waiting for a maintainer"),
            Some(BlockedReason::Fork),
        )
        .unwrap();
        let failed = create_settled_run(
            &db,
            &org,
            &repo,
            &newrun(&"b".repeat(40)),
            "failed",
            Some("the file does not parse"),
            None,
        )
        .unwrap();
        let running = create_run(
            &db,
            &org,
            &repo,
            &newrun(&"c".repeat(40)),
            &[newjob("test", &[])],
        )
        .unwrap();

        assert!(delete_blocked_run(&db, &blocked.id).unwrap());
        assert!(run_by_id(&db, &blocked.id).unwrap().is_none());
        // Gone once. A second approval must not be able to reach
        // anything.
        assert!(!delete_blocked_run(&db, &blocked.id).unwrap());

        assert!(!delete_blocked_run(&db, &failed.id).unwrap());
        assert!(run_by_id(&db, &failed.id).unwrap().is_some());
        assert!(!delete_blocked_run(&db, &running.id).unwrap());
        assert!(run_by_id(&db, &running.id).unwrap().is_some());
        // A hostile id is "nothing matched", not a database error.
        assert!(!delete_blocked_run(&db, "../../etc/passwd").unwrap());
    }

    /// What a suspension has to stop, and what an approval has to find.
    ///
    /// `live_runs_for_org` crosses repositories deliberately — a miner
    /// does not arrive on one branch of one repository — and stops at
    /// the organisation boundary just as deliberately.
    /// `runs_for_change_commit` is the opposite question: everything at
    /// one tip of one change, `blocked` rows included, because those are
    /// precisely the rows approval is about.
    #[test]
    fn a_suspension_can_find_every_live_run_and_an_approval_every_run_at_a_tip() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("wf-org-runs")).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let mk = |name: &str| {
            registry::create_repo(
                &db,
                &org.id,
                &NewRepo {
                    name,
                    kind: RepoKind::Native,
                    description: None,
                    default_branch: "main",
                    origin_url: None,
                    origin_provider: None,
                    origin_installation: None,
                },
            )
            .unwrap()
            .id
        };
        let (one, two) = (mk("app"), mk("lib"));
        let stranger = registry::create_org(&db, "globex").unwrap();
        let strange_repo = registry::create_repo(
            &db,
            &stranger.id,
            &NewRepo {
                name: "app",
                kind: RepoKind::Native,
                description: None,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();

        let a = create_run(
            &db,
            &org.id,
            &one,
            &newrun(&"a".repeat(40)),
            &[newjob("t", &[])],
        )
        .unwrap();
        let b = create_run(
            &db,
            &org.id,
            &two,
            &newrun(&"b".repeat(40)),
            &[newjob("t", &[])],
        )
        .unwrap();
        let settled = create_settled_run(
            &db,
            &org.id,
            &one,
            &newrun(&"c".repeat(40)),
            "failed",
            Some("nope"),
            None,
        )
        .unwrap();
        create_run(
            &db,
            &stranger.id,
            &strange_repo.id,
            &newrun(&"d".repeat(40)),
            &[newjob("t", &[])],
        )
        .unwrap();

        let live: Vec<String> = live_runs_for_org(&db, &org.id)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(
            live.len(),
            2,
            "both repositories, and only this org: {live:?}"
        );
        assert!(live.contains(&a.id) && live.contains(&b.id));
        assert!(!live.contains(&settled.id), "a settled run is not live");
        assert!(live_runs_for_org(&db, "no-such-org").unwrap().is_empty());

        // One tip of one change: the blocked placeholder is exactly what
        // the approval route is looking for.
        let tip = "e".repeat(40);
        let mut change_run = newrun(&tip);
        change_run.event = "change";
        change_run.change_key = Some("I1234");
        change_run.ref_name = Some("main");
        let held = create_settled_run(
            &db,
            &org.id,
            &one,
            &change_run,
            "blocked",
            Some("held"),
            Some(BlockedReason::Fork),
        )
        .unwrap();
        let mut other_file = change_run.clone();
        other_file.file = ".weft/lint.yml";
        let held2 = create_settled_run(
            &db,
            &org.id,
            &one,
            &other_file,
            "blocked",
            Some("held"),
            Some(BlockedReason::Fork),
        )
        .unwrap();
        let mut older = change_run.clone();
        let previous_tip = "f".repeat(40);
        older.commit_sha = &previous_tip;
        create_settled_run(
            &db,
            &org.id,
            &one,
            &older,
            "blocked",
            Some("held"),
            Some(BlockedReason::Fork),
        )
        .unwrap();

        // Sorted, not in insertion order, and that is not laziness.
        // `ulid()` is 48 bits of millisecond and 80 bits of randomness
        // with no per-millisecond counter, so two rows written inside
        // one millisecond — which is what two workflow files in one
        // push are — tie on `created_at` and then order by a random
        // suffix. Asserting insertion order here passes on an idle
        // machine and fails under load, and what the approval route
        // actually needs is the *set* at this tip.
        let mut at_tip: Vec<String> = runs_for_change_commit(&db, &one, "I1234", &tip)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        at_tip.sort();
        let mut want = vec![held.id.clone(), held2.id.clone()];
        want.sort();
        assert_eq!(at_tip, want, "both files at this tip, and only this tip");
        assert!(runs_for_change_commit(&db, &two, "I1234", &tip)
            .unwrap()
            .is_empty());
        assert!(runs_for_change_commit(&db, &one, "I9999", &tip)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_list_narrows_to_one_commit_or_one_change_and_never_leaves_its_repository() {
        // The approval panel asks for "the runs at this change's tip".
        // Doing that by fetching a window and filtering in the client
        // loses them on a busy repository — the page then looks fine
        // and is missing its button — so the narrowing is here.
        let (db, o1, r1, _o2, r2) = world("wf-filter");
        let tip = "1c".repeat(20);
        let older = "2c".repeat(20);

        let mut at_tip = newrun(&tip);
        at_tip.event = "change";
        at_tip.change_key = Some("I77");
        let held = create_settled_run(
            &db,
            &o1,
            &r1,
            &at_tip,
            "blocked",
            Some("held"),
            Some(BlockedReason::Fork),
        )
        .unwrap();

        let mut at_older = newrun(&older);
        at_older.event = "change";
        at_older.change_key = Some("I77");
        let previous = create_settled_run(
            &db,
            &o1,
            &r1,
            &at_older,
            "blocked",
            Some("held"),
            Some(BlockedReason::Fork),
        )
        .unwrap();

        // Same commit, a plain push with no change: it shares the tip
        // but not the change, which is what tells the two filters apart.
        let pushed =
            create_settled_run(&db, &o1, &r1, &newrun(&tip), "failed", Some("no"), None).unwrap();
        // And the same sha in a different repository, which must never
        // appear however the caller narrows.
        let stranger =
            create_settled_run(&db, &o1, &r2, &newrun(&tip), "failed", Some("no"), None).unwrap();

        let ids = |c: Option<&str>, k: Option<&str>| -> Vec<String> {
            runs_for_repo(&db, &r1, c, k, None, 20)
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect()
        };
        let all = ids(None, None);
        assert_eq!(all.len(), 3, "unfiltered is unchanged: {all:?}");
        assert!(!all.contains(&stranger.id));

        let by_commit = ids(Some(&tip), None);
        assert!(by_commit.contains(&held.id) && by_commit.contains(&pushed.id));
        assert!(!by_commit.contains(&previous.id));

        let by_change = ids(None, Some("I77"));
        assert!(by_change.contains(&held.id) && by_change.contains(&previous.id));
        assert!(!by_change.contains(&pushed.id), "a push has no change key");

        assert_eq!(
            ids(Some(&tip), Some("I77")),
            vec![held.id],
            "both together is the approval panel's question"
        );
        assert!(ids(Some(&"9d".repeat(20)), None).is_empty());
        assert!(ids(None, Some("I-nope")).is_empty());

        // And the code travels back out of the database, which is the
        // whole point of storing it.
        assert_eq!(
            runs_for_repo(&db, &r1, Some(&tip), Some("I77"), None, 20).unwrap()[0]
                .blocked_reason
                .as_deref(),
            Some("fork")
        );
        assert!(pushed.blocked_reason.is_none());

        // Narrowing by event: a composed run at the same tip is the
        // Checks tab's question, and a push run must not answer it.
        let mut composed = newrun(&tip);
        composed.event = "changeset";
        let composed =
            create_settled_run(&db, &o1, &r1, &composed, "failed", Some("no"), None).unwrap();
        let by_event: Vec<String> = runs_for_repo(&db, &r1, None, None, Some("changeset"), 20)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(by_event, vec![composed.id.clone()]);
        assert!(runs_for_repo(&db, &r1, None, None, Some("release"), 20)
            .unwrap()
            .is_empty());
        assert_eq!(ids(None, None).len(), 4, "unfiltered still lists it");
    }
}
