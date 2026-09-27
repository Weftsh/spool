//! Epoch GC + deleted-repo sweeps. The grace window must exceed the
//! longest compaction and the longest clone (I8's operational clause) —
//! default 24h, configurable.

use crate::app::SharedState;
use std::collections::HashSet;
use std::time::Duration;
use stratum_control::{epoch_refs, jobs, registry, ControlDb};
use stratum_engine::gc::EpochRefs;
use stratum_store::{LatencyModel, ObjectStore};

/// The live-set question the engine cannot answer for itself: which of
/// this repository's epochs some *other* repository is reading.
///
/// Zero-copy forks share upstream's immutable objects, so an epoch can
/// be unreferenced by upstream's own manifest and locator header and
/// still be the thing a fork serves every clone from. Without this,
/// upstream compacting plus one grace window silently deletes it.
///
/// Bound to one repository at construction, because the engine names a
/// layout by its store prefix and the control plane names one by its
/// repo id — and the moment those two identities are matched up in the
/// wrong place, GC starts answering about the wrong repository.
pub struct DbEpochRefs {
    db: ControlDb,
    repo_id: String,
}

impl DbEpochRefs {
    pub fn new(db: ControlDb, repo_id: impl Into<String>) -> Self {
        DbEpochRefs {
            db,
            repo_id: repo_id.into(),
        }
    }
}

impl EpochRefs for DbEpochRefs {
    fn pinned_epochs(&self) -> Result<HashSet<String>, String> {
        // Read fresh every time, including on the re-read GC does
        // immediately before deleting — that re-read is what closes the
        // window where a fork is created mid-sweep, and a cached answer
        // would silently reopen it.
        Ok(epoch_refs::pinned_epochs(&self.db, &self.repo_id)?
            .into_iter()
            .collect())
    }
}

/// Name of the fleet-wide lock this worker runs under. See
/// [`jobs::try_lock`].
const WORKER: &str = "gc-sweep";

pub fn spawn(state: SharedState) {
    let every = super::env_secs("STRATUM_GC_SECS", 0);
    if every == 0 {
        return;
    }
    let grace = super::env_secs("STRATUM_GC_GRACE_SECS", 86_400);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(every));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = sweep_all(&state, grace).await {
                eprintln!("weft: gc sweep: {e}");
            }
        }
    });
}

/// One GC pass — at most one in the fleet at a time.
///
/// Unleased, every node ran the whole sweep on every tick: N times the
/// LIST/DELETE traffic against one bucket, N nodes deciding what is past
/// the grace window from N unsynchronised clocks, and N `mark_purged`
/// writes for the same repo. Nothing here is destructive on its own —
/// deleting an already-deleted object is a no-op — but a sweep whose
/// answer depends on which node happened to run it is not one anybody can
/// reason about during an incident.
///
/// Losing the lock means another node is sweeping; that is a pass, not a
/// missed tick.
pub async fn sweep_all(state: &SharedState, grace_secs: u64) -> Result<(), String> {
    let db = state.db.clone();
    let lock = tokio::task::spawn_blocking(move || jobs::try_lock(&db, WORKER))
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")))?;
    // Held for the whole sweep, released on drop — including on the `?`
    // paths below.
    let Some(_lock) = lock else {
        return Ok(());
    };
    // Active repos: epoch GC. (Org list via a full repos scan is fine at
    // this scale tier; the jobs table takes over when it isn't.)
    let db = state.db.clone();
    let repos = tokio::task::spawn_blocking(move || registry::all_active_repos(&db))
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")))?;
    for repo in repos {
        let store_url = state.store_url.clone();
        let prefix = repo.prefix().as_str().to_string();
        let refs = DbEpochRefs::new(state.db.clone(), repo.id.clone());
        let out = tokio::task::spawn_blocking(move || {
            let store = ObjectStore::new(&store_url, LatencyModel::None);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            stratum_engine::gc::gc_epochs_with_refs(&store, &prefix, grace_secs, now, &refs)
        })
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")));
        if let Err(e) = out {
            eprintln!("weft: epoch gc of {}: {e}", repo.id);
        }
    }
    // Deleted repos past the grace window: storage sweep.
    let db = state.db.clone();
    let grace_ms = grace_secs as i64 * 1000;
    let doomed = tokio::task::spawn_blocking(move || {
        registry::deleted_repos_older_than(&db, stratum_control::ids::now_ms() - grace_ms)
    })
    .await
    .unwrap_or_else(|e| Err(format!("join: {e}")))?;
    for (org_id, repo_id) in doomed {
        let store_url = state.store_url.clone();
        let prefix = format!("o/{org_id}/r/{repo_id}");
        // A deleted repository whose data somebody else is still reading
        // is not sweepable: its objects are the bytes behind every fork
        // of it. The database enforces this too — `epoch_refs` RESTRICTs
        // on the owner — but reaching a constraint violation from a
        // background sweeper says far less than declining to sweep and
        // naming the dependents. Promotion (which re-ingests the forks
        // onto their own storage) is what clears this.
        let db = state.db.clone();
        let held = tokio::task::spawn_blocking({
            let repo_id = repo_id.clone();
            move || epoch_refs::dependents(&db, &repo_id)
        })
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")))?;
        if !held.is_empty() {
            // Not sweepable yet, and saying so on every tick forever is
            // not a plan. Enqueue the promotions that will make it
            // sweepable — idempotent, so a repository whose promotions
            // are already queued or running simply stays here until they
            // finish, and one that was deleted before promotion existed
            // gets picked up on the next pass.
            eprintln!(
                "weft: deferring sweep of deleted repo {repo_id}: promoting {} dependent{}",
                held.len(),
                if held.len() == 1 { "" } else { "s" }
            );
            crate::workers::promoter::enqueue_dependents(state, &repo_id);
            continue;
        }
        let out = tokio::task::spawn_blocking(move || {
            let store = ObjectStore::new(&store_url, LatencyModel::None);
            stratum_engine::gc::sweep_prefix(&store, &prefix)
        })
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")));
        match out {
            Ok(_) => {
                let _ = registry::mark_purged(&state.db, &repo_id);
            }
            Err(e) => eprintln!("weft: deleted-repo sweep {repo_id}: {e}"),
        }
    }
    Ok(())
}
