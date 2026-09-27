//! The fork worker: make one repository readable from another's bytes.
//!
//! A fork here copies **no objects**. It writes two small pointers into
//! its own prefix — a manifest whose segment keys are already absolute,
//! and an `SLH4` `locator.hdr` naming an absolute data prefix — and from
//! that moment it serves clones out of upstream's immutable storage.
//! Milliseconds, and free until the histories diverge, at which point the
//! fork's own pushes land as new segments in its **own** prefix and the
//! manifest simply names both.
//!
//! **The write order is the correctness argument, and it is I7's shape.**
//! There is no transaction spanning Postgres and an S3 conditional PUT,
//! so:
//!
//! 1. `registry::create_repo` — the fork's row (done by the API).
//! 2. `forks::attach` — link it to its parent, state `pending`.
//! 3. **`epoch_refs::register` — pin every epoch the fork will read.**
//! 4. write `locator.hdr`, then `manifest.json`.
//! 5. `forks::set_state(Ready)`.
//!
//! Step 3 strictly before step 4. A crash between them leaves references
//! pinning epochs no pointer uses — storage held slightly too long,
//! reconcilable, and **safe**. The reverse order leaves a live fork
//! pointing at collectable data, which is corruption and is not
//! recoverable. Within step 4 the locator goes first and the manifest
//! last, which is the order every other publisher here uses: the manifest
//! is the only ref truth (I9), so a crash before it leaves a fork that
//! looks empty rather than one that looks broken, and `fork_state` says
//! which of those it is.
//!
//! Every step is idempotent, so a job that dies part-way is simply run
//! again.

use crate::app::SharedState;
use stratum_control::{epoch_refs, forks, jobs, registry};
use stratum_engine::fork::{self, ForkOutcome};
use stratum_store::{LatencyModel, ObjectStore};

/// Enqueue the fork of `fork_repo_id` from its attached parent.
///
/// Keyed on the **target**: one fork job per repository being created.
/// Repository-name uniqueness already stops a double-clicked button
/// creating two forks, so this is the second line rather than the first —
/// the one that matters is two nodes claiming two rows for the same
/// target and both writing its pointer.
pub fn enqueue(state: &SharedState, org_id: &str, fork_repo_id: &str) -> Result<(), String> {
    jobs::enqueue_unique(&state.db, org_id, fork_repo_id, "fork", None)?;
    Ok(())
}

pub fn spawn(state: SharedState) {
    let poll = super::env_period("STRATUM_FORK_POLL_SECS", 5);
    if poll.is_zero() {
        return;
    }
    // Configurable like every other worker's, and load-bearing for
    // recovery: a node that dies mid-fork holds its claim until the
    // lease expires, so the lease is how long a crashed fork stays
    // stuck before another node finishes it.
    let lease_ms = super::lease_ms("STRATUM_FORK_LEASE_SECS", 300);
    tokio::spawn(async move {
        loop {
            let claimed = {
                let db = state.db.clone();
                tokio::task::spawn_blocking(move || jobs::claim(&db, "fork", lease_ms))
                    .await
                    .unwrap_or_else(|e| Err(format!("join: {e}")))
            };
            match claimed {
                Ok(Some(job)) => match run_one(&state, &job).await {
                    Ok(o) => {
                        let _ = jobs::complete(&state.db, &job.id, Some(&format!("{o:?}")));
                    }
                    Err(e) => {
                        eprintln!("weft: fork failed: {e}");
                        // The repository stays, and says why it is not
                        // readable. A fork that silently 404s is worse
                        // than one that reports a failed state.
                        if let Some(repo_id) = job.repo_id.as_deref() {
                            let _ = forks::set_state(&state.db, repo_id, forks::State::Failed);
                        }
                        let _ = jobs::fail(&state.db, &job.id, &e);
                    }
                },
                Ok(None) => tokio::time::sleep(poll).await,
                Err(e) => {
                    eprintln!("weft: fork claim: {e}");
                    tokio::time::sleep(poll).await;
                }
            }
        }
    });
}

pub async fn run_one(state: &SharedState, job: &jobs::Job) -> Result<ForkOutcome, String> {
    let Some(repo_id) = job.repo_id.clone() else {
        return Err("fork job without repo".into());
    };
    let Some(fork) = registry::repo_by_id(&state.db, &job.org_id, &repo_id)? else {
        return Ok(ForkOutcome::NothingToDo);
    };
    let Some(info) = forks::info(&state.db, &repo_id)? else {
        return Ok(ForkOutcome::NothingToDo);
    };
    let Some(parent_id) = info.parent_id.clone() else {
        // Not a fork — or promoted out of being one while this job sat
        // in the queue. Either way there is nothing to point anywhere.
        return Ok(ForkOutcome::NothingToDo);
    };
    // The parent is looked up under **its own** org, not this job's.
    // A fork lives in the forker's namespace and its parent almost never
    // does — that is the entire point of forking — and every registry
    // lookup is org-scoped, so asking for it under the fork's org finds
    // nothing and reports the parent as gone.
    let Some((parent_org, _)) = forks::parent_of(&state.db, &repo_id)? else {
        return Ok(ForkOutcome::NothingToDo);
    };
    let Some(parent) = registry::repo_by_id(&state.db, &parent_org, &parent_id)? else {
        return Err(format!("fork {repo_id}: parent {parent_id} is gone"));
    };

    let store_url = state.store_url.clone();
    let parent_prefix = parent.prefix().as_str().to_string();
    let fork_prefix = fork.prefix().as_str().to_string();

    // 1. Read upstream's two pointers.
    let up = {
        let store_url = store_url.clone();
        let parent_prefix = parent_prefix.clone();
        tokio::task::spawn_blocking(move || {
            let store = ObjectStore::new(&store_url, LatencyModel::None);
            fork::read_upstream(&store, &parent_prefix)
        })
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")))?
    };
    let Some(up) = up else {
        // Forking an empty repository gives an empty repository.
        forks::set_state(&state.db, &repo_id, forks::State::Ready)?;
        return Ok(ForkOutcome::Empty);
    };

    // 2. Register every reference BEFORE any pointer is published.
    //
    // Every one, not just the parent's: a fork of a fork whose parent has
    // already diverged reads the root's shared base *and* the parent's
    // own pushes, and its manifest names keys in both prefixes. Pinning
    // only one of them leaves the other collectable, and the failure is
    // silent until a clone stops passing fsck.
    for (owner_repo_id, epoch) in fork::references_in(&up, &fork.id) {
        epoch_refs::register(&state.db, &owner_repo_id, &epoch, &fork.id)?;
    }

    // 3. Only now may the fork's pointers exist.
    let repo_name = fork.name.clone();
    // The layout is the last segment of the prefix — one place decides
    // what it is, and it is `registry`.
    let layout = fork_prefix
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let published = tokio::task::spawn_blocking(move || {
        let store = ObjectStore::new(&store_url, LatencyModel::None);
        fork::publish_fork(&store, &fork_prefix, &up, &repo_name, &layout)
    })
    .await
    .unwrap_or_else(|e| Err(format!("join: {e}")))?;

    forks::set_state(&state.db, &repo_id, forks::State::Ready)?;
    // Recorded as what it is: a manifest naming upstream's keys, which
    // `Manifest::stored_bytes` sums to zero. The row exists so the org's
    // listing is complete, and stays zero until promotion.
    crate::storage::refresh_after_write(state, &fork).await;
    Ok(published)
}
