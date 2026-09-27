//! Promotion: give a fork storage of its own so its upstream can go.
//!
//! A zero-copy fork reads upstream's immutable objects, and `epoch_refs`
//! stops those objects being collected while it does. That protection is
//! deliberately absolute — the `owner_repo_id` foreign key RESTRICTs — so
//! a deleted upstream with forks is *never* sweepable on its own. Without
//! this worker the deleted-repo sweep declines it on every tick, forever,
//! and the storage is held for a repository nobody can reach.
//!
//! So deleting an upstream does not delete anything derived from it. It
//! enqueues one promotion per dependent, each of which materializes that
//! fork and re-ingests it under its own prefix, and only once the last
//! reference is released does the upstream's storage become collectable.
//!
//! **The write order is I7's shape pointing the other way.** Forking
//! registers the reference *before* publishing its pointer, because the
//! danger is a live fork reading unprotected data. Promotion is the
//! mirror: give the fork its own bytes *first*, release the reference
//! *after*. A crash between them leaves a fork that owns its storage and
//! still holds a claim on upstream's — storage pinned slightly too long,
//! reconcilable by re-running this job, and safe. The reverse would drop
//! the claim while the fork was still reading, which is the corruption
//! the whole slice exists to prevent.
//!
//! Every step is idempotent, so a job that dies part-way simply runs
//! again.

use crate::app::SharedState;
use stratum_control::{epoch_refs, forks, jobs, registry};
use stratum_engine::fork::{promote, PromoteOutcome};
use stratum_store::{LatencyModel, ObjectStore};

/// Enqueue promotion of one fork. Keyed on the fork, which is the
/// repository whose storage the job rewrites.
pub fn enqueue(state: &SharedState, org_id: &str, fork_repo_id: &str) -> Result<(), String> {
    jobs::enqueue_unique(&state.db, org_id, fork_repo_id, "promote", None)?;
    Ok(())
}

/// Enqueue promotion for everything reading `repo_id`'s data.
///
/// Called when an upstream is deleted. Best-effort by design: a
/// dependent that cannot be enqueued is logged and the others still go,
/// because one unreachable fork must not strand the rest — and the
/// upstream's storage is safe either way, since the reference it could
/// not release is exactly what keeps it protected.
pub fn enqueue_dependents(state: &SharedState, repo_id: &str) {
    let dependents = match epoch_refs::dependents(&state.db, repo_id) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("weft: promote: reading dependents of {repo_id}: {e}");
            return;
        }
    };
    for fork_id in dependents {
        // The fork is in its own namespace, which is the whole point of
        // forking and the reason this cannot use the deleted repo's org.
        let org_id = match forks::owner_org_of(&state.db, &fork_id) {
            Ok(Some(o)) => o,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("weft: promote: locating fork {fork_id}: {e}");
                continue;
            }
        };
        if let Err(e) = enqueue(state, &org_id, &fork_id) {
            eprintln!("weft: promote: enqueue {fork_id}: {e}");
        }
    }
}

pub fn spawn(state: SharedState) {
    let poll = super::env_period("STRATUM_PROMOTE_POLL_SECS", 30);
    if poll.is_zero() {
        return;
    }
    // Long by default: promotion materializes and re-ingests a whole
    // repository, and a lease that expires mid-re-ingest hands the same
    // work to a second node to race the first.
    let lease_ms = super::lease_ms("STRATUM_PROMOTE_LEASE_SECS", 1800);
    tokio::spawn(async move {
        loop {
            let claimed = {
                let db = state.db.clone();
                tokio::task::spawn_blocking(move || jobs::claim(&db, "promote", lease_ms))
                    .await
                    .unwrap_or_else(|e| Err(format!("join: {e}")))
            };
            match claimed {
                Ok(Some(job)) => match run_one(&state, &job).await {
                    Ok(o) => {
                        let _ = jobs::complete(&state.db, &job.id, Some(&format!("{o:?}")));
                    }
                    Err(e) => {
                        eprintln!("weft: promote failed: {e}");
                        let _ = jobs::fail(&state.db, &job.id, &e);
                    }
                },
                Ok(None) => tokio::time::sleep(poll).await,
                Err(e) => {
                    eprintln!("weft: promote claim: {e}");
                    tokio::time::sleep(poll).await;
                }
            }
        }
    });
}

pub async fn run_one(state: &SharedState, job: &jobs::Job) -> Result<PromoteOutcome, String> {
    let Some(repo_id) = job.repo_id.clone() else {
        return Err("promote job without repo".into());
    };
    let Some(repo) = registry::repo_by_id(&state.db, &job.org_id, &repo_id)? else {
        // Gone. Its references died with it (`referencing_repo_id`
        // CASCADEs), so there is nothing left holding upstream either.
        return Ok(PromoteOutcome::NotNeeded);
    };

    // 1. The fork's own storage, first and always.
    let store_url = state.store_url.clone();
    let prefix = repo.prefix().as_str().to_string();
    let work = super::job_work_dir(&state.data_dir, "promote", &repo_id, &job.id);
    let outcome = tokio::task::spawn_blocking(move || {
        let store = ObjectStore::new(&store_url, LatencyModel::None);
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let out = promote(
            &store,
            &prefix,
            &crate::app::ingest_config_from_env(),
            &work,
        );
        // This run's own directory; see `workers::job_work_dir`.
        let _ = std::fs::remove_dir_all(&work);
        out
    })
    .await
    .unwrap_or_else(|e| Err(format!("join: {e}")))?;

    if outcome == PromoteOutcome::LostRace {
        // A push landed while we were re-ingesting and won the manifest
        // CAS, which is correct — the push is the newer truth. Leave the
        // references in place and let the job be retried against it.
        return Ok(outcome);
    }
    // The fork now stands on bytes of its own, under its own prefix:
    // from here they are its bill and not upstream's.
    crate::storage::refresh_after_write(state, &repo).await;

    // 2. Only now may the claims go. Everything this fork was reading is
    //    either rewritten under its own prefix (Promoted) or was never
    //    there to begin with (NotNeeded — an empty fork holds nothing).
    for held in epoch_refs::references_held_by(&state.db, &repo_id)? {
        epoch_refs::release(
            &state.db,
            &held.owner_repo_id,
            &held.epoch,
            &held.referencing_repo_id,
        )?;
    }

    // 3. And it is nobody's fork now.
    forks::detach(&state.db, &repo_id)?;
    Ok(outcome)
}
