//! The credentials a workflow job holds, and taking them back.
//!
//! A job is handed a repository-read token when a runner claims it, and a
//! composed job one more per member repository when it fetches its spec.
//! Every one of them has to stop working when the job does — whether it
//! finished, was cancelled, or its runner vanished — and this is the one
//! place that knows the whole list.

/// Margin on a job credential's expiry over the job's own timeout, so a
/// runner uploading its final log at the wire is not refused.
const TOKEN_SLACK_MS: i64 = 30 * 60 * 1000;

/// When every credential minted for this job stops working by itself.
///
/// One function because a composed job has several — its own token and
/// one per other member repository — and they have to die together. A
/// member token outliving the job token would leave a read credential on
/// somebody else's repository after the build that needed it is over,
/// which is exactly what the revocation below exists to prevent and
/// would be the one case revocation could not reach: a job whose runner
/// vanished has nothing to revoke *from*, and expiry is the backstop.
///
/// The spec's timeout is used rather than the fleet cap, and a spec that
/// will not parse defaults rather than refusing: a credential that
/// expires the moment it is issued kills a build for a reason nobody can
/// see in their workflow file. Zero is treated as unreadable for the
/// same reason.
pub fn token_expiry(spec: &str) -> i64 {
    let minutes = serde_json::from_str::<serde_json::Value>(spec)
        .ok()
        .and_then(|v| v["timeout_minutes"].as_i64())
        .filter(|&m| m > 0)
        .unwrap_or(stratum_control::workflows::DEFAULT_TIMEOUT_MINUTES);
    stratum_control::ids::now_ms() + minutes * 60_000 + TOKEN_SLACK_MS
}

/// Revoke every credential a job holds: its own token, and the member
/// read tokens a composed job was handed at spec time.
///
/// One door, called from every place a job's credentials stop being its
/// own — [`stop_jobs`], the verdict, and the self-hosted claim's reclaim
/// and give-up paths — because "a finished job leaves no live token" is
/// only true if it is true at every one of them, and a composed job's
/// member tokens are the easy half to forget. They are
/// not this repository's credentials: they read *other people's*
/// repositories, which is the whole reason they are minted per member,
/// and one left live is a stranger's build's read access to a repository
/// its author may not be able to see.
///
/// Best effort and never fatal, the same as the single-token revoke it
/// replaces: a revoke that fails is a credential that expires on its own
/// (see [`token_expiry`]), where refusing the caller over it would lose
/// a verdict or strand a queue.
pub fn revoke_job_tokens(
    state: &crate::app::SharedState,
    job: &stratum_control::workflows::WorkflowJob,
) {
    // Every credential this attempt was given: the job token and the
    // per-member read tokens of a composed run. A token this list
    // forgets is a token that outlives its job.
    let all = job
        .token_id
        .iter()
        .map(String::as_str)
        .chain(job.member_token_ids.iter().map(String::as_str));
    for token in all {
        if let Err(e) = stratum_control::auth::revoke(&state.db, &job.org_id, token, None) {
            eprintln!("weft: revoke job token for {}: {e}", job.id);
        }
    }
}

/// Revoke every credential these jobs hold, because they are being
/// stopped.
///
/// There is nothing to *stop*: a self-hosted runner is somebody else's
/// machine, nothing listens on the operator's network, and it is never
/// reached. It learns the job is over from the **410** its next call
/// gets — `runner_api::dead_job_token` answers 410 rather than 401 for a
/// dead token that is *this job's own*, on a job that has stopped
/// running — and gives up there, which is why revoking is the whole of
/// stopping.
pub async fn stop_jobs(
    state: &crate::app::SharedState,
    jobs: &[stratum_control::workflows::WorkflowJob],
    _reason: &str,
) {
    for job in jobs {
        revoke_job_tokens(state, job);
    }
}
