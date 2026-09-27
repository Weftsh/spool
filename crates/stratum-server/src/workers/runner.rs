//! The workflow sweeper: settles jobs nothing is going to report on.
//!
//! Every job in this edition runs on a self-hosted runner, which claims
//! it over `POST /v1/runners/claim` and reports its own verdict. What a
//! runner cannot do is report for itself when it has gone — the machine
//! was switched off mid-build, the network is gone — so one loop, on
//! every node, does the two sweeps that stand in for it:
//!
//! 1. fail jobs that have outrun their timeout by more than the slack.
//!    The runner enforces the timeout itself and reports it; the slack
//!    keeps the sweep from racing it to a different verdict;
//! 2. tombstone self-hosted runners nobody has heard from for long
//!    enough, so the runner list does not keep a decommissioned machine.

use crate::app::SharedState;
use crate::workflow::mirror;
use stratum_control::ids::now_ms;
use stratum_control::workflows::{self, WorkflowJob};

/// The default grace: five minutes (`STRATUM_RUNNER_OVERDUE_SLACK_SECS`).
/// The runner reports a timeout itself at the wire; the sweep is the
/// backstop, and it must not fire while a healthy runner is still busy
/// uploading its final log.
const OVERDUE_SLACK_SECS: u64 = 5 * 60;

pub fn spawn(state: SharedState) {
    let poll = super::env_period("STRATUM_RUNNER_POLL_SECS", 5);
    if poll.is_zero() {
        return;
    }
    let slack_ms = super::lease_ms("STRATUM_RUNNER_OVERDUE_SLACK_SECS", OVERDUE_SLACK_SECS);
    tokio::spawn(async move {
        loop {
            tick(&state, slack_ms).await;
            tokio::time::sleep(poll).await;
        }
    });
}

/// One pass: the overdue sweep, then the runner sweep.
pub async fn tick(state: &SharedState, overdue_slack_ms: i64) {
    sweep_overdue(state, overdue_slack_ms).await;
    sweep_runners(state);
}

/// Tombstone self-hosted runners nobody has heard from for long enough.
///
/// It rides on the sweeper's tick rather than on a worker of its own
/// because it is one statement against an index and it needs no
/// scheduling of its own. (`STRATUM_RUNNER_POLL_SECS=0` turns the
/// sweeper off entirely; a runner list that then keeps a decommissioned
/// machine is cosmetic, and the alternative is a second always-on loop
/// for a fourteen-day deadline.)
fn sweep_runners(state: &SharedState) {
    match stratum_control::runners::sweep(&state.db, now_ms()) {
        Ok(0) => {}
        Ok(n) => eprintln!("weft: runner sweep: removed {n} runner(s) not seen recently"),
        Err(e) => eprintln!("weft: runner sweep: {e}"),
    }
}

async fn sweep_overdue(state: &SharedState, slack_ms: i64) {
    let overdue = match workflows::overdue_jobs(&state.db, now_ms(), slack_ms) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("weft: runner sweep: {e}");
            return;
        }
    };
    for job in overdue {
        let minutes = timeout_minutes(&job.spec);
        let reason =
            format!("timed out after {minutes} minutes and the runner did not report back");
        eprintln!("weft: runner: job {} overdue — {reason}", job.id);
        fail(state, &job, &reason).await;
    }
}

fn timeout_minutes(spec: &str) -> i64 {
    serde_json::from_str::<serde_json::Value>(spec)
        .ok()
        .and_then(|v| v["timeout_minutes"].as_i64())
        .unwrap_or(360)
}

/// Record a failure the sweeper decided, revoke what the job holds, and
/// bring the mirrored check along.
///
/// `pub(crate)` because the self-hosted claim gives up on a job for the
/// same reasons and must leave the same wreckage behind it: a verdict, a
/// revoked credential, and every mirrored check row in the run brought
/// up to date with the cascade.
pub(crate) async fn fail(state: &SharedState, job: &WorkflowJob, error: &str) {
    if let Err(e) = workflows::finish(&state.db, &job.id, "failed", None, Some(error)) {
        eprintln!("weft: runner: fail job {}: {e}", job.id);
        return;
    }
    crate::workflow::credentials::stop_jobs(state, std::slice::from_ref(job), error).await;
    mirror_run(state, &job.run_id).await;
}

/// Re-mirror every job of a run after a transition. `finish` cascades
/// — a failed job skips its dependents and may settle the run — and the
/// checks have to show the cascade, not just the job that moved.
async fn mirror_run(state: &SharedState, run_id: &str) {
    let Ok(Some(run)) = workflows::run_by_id(&state.db, run_id) else {
        return;
    };
    let Ok(jobs) = workflows::jobs_of(&state.db, run_id) else {
        return;
    };
    let page = mirror::run_page_by_id(&state.db, &state.public_url, &run.repo_id, &run.id);
    for job in &jobs {
        if let Err(e) = mirror::mirror_job(&state.db, &run.repo_id, &run, job, page.as_deref()) {
            eprintln!("weft: mirror job {}: {e}", job.id);
        }
    }
}
