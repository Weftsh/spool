//! The compactor: claims `compact` jobs (enqueued after accepted writes)
//! and folds WALs into fresh epochs. Concurrent pushes always win the CAS;
//! a lost race re-queues once.

use crate::app::SharedState;
use stratum_control::jobs;
use stratum_engine::compact::{compact, CompactOutcome, CompactionThresholds};
use stratum_store::{LatencyModel, ObjectStore};

/// Enqueue a maybe-compact job for a repo after a write.
///
/// The dedup is the `jobs_active_per_repo` index inside the insert, not a
/// preceding read: two nodes accepting two pushes at the same instant
/// both used to see "nothing active" and both insert, and two workers
/// then folded the same prefix concurrently. `Ok(None)` means somebody
/// else's job already covers this write, which is the whole point of a
/// sweep — it reads the repo's current state when it runs.
pub fn enqueue(state: &SharedState, org_id: &str, repo_id: &str) {
    if let Err(e) = jobs::enqueue_unique(&state.db, org_id, repo_id, "compact", None) {
        eprintln!("weft: compact enqueue: {e}");
    }
}

/// What the worker owes a repository once its fold has answered.
///
/// `refresh` because a fold rewrites the tiers and the bytes the manifest
/// names moved with them. `requeue` because a fold does not necessarily
/// leave the WAL empty, and nothing else is guaranteed to come along and
/// notice.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Follow {
    pub refresh: bool,
    pub requeue: bool,
}

/// **Every fold leaves a follow-up.**
///
/// A write that lands *while* a fold is running is not covered by it: the
/// fold read the manifest before that write arrived, and `enqueue`
/// deduplicates on `jobs_active_per_repo`, so the write's own enqueue
/// found this job already active and added nothing. The entry is left in
/// the WAL with nothing scheduled to fold it.
///
/// Only `LostRace` used to requeue. So on a repository that went quiet
/// after a burst — which is what a repository does after a push finishes
/// — those entries waited for the next write, which is to say for nobody,
/// and every later read paid for materialising them. That is the same
/// family as the fold that switched itself off: compaction silently stops
/// keeping up and only the read latency says so.
///
/// This terminates rather than folding in a circle: the follow-up job
/// re-reads the manifest and answers `NotNeeded` as soon as the WAL is
/// back under the thresholds, and `NotNeeded` queues nothing.
pub(crate) fn follow(outcome: CompactOutcome) -> Follow {
    match outcome {
        CompactOutcome::Compacted => Follow {
            refresh: true,
            requeue: true,
        },
        CompactOutcome::LostRace => Follow {
            refresh: false,
            requeue: true,
        },
        CompactOutcome::NotNeeded => Follow {
            refresh: false,
            requeue: false,
        },
    }
}

/// Is this repository's WAL past the point where a fold is owed?
///
/// The same comparison `compact()` makes, on purpose: a sweep that used
/// a different rule would either enqueue folds that immediately answer
/// `NotNeeded`, or skip repositories the fold would have taken.
fn behind(m: &stratum_store::Manifest, t: &CompactionThresholds) -> bool {
    let bytes: u64 = m.wal.iter().map(|w| w.bytes).sum();
    m.wal.len() >= t.wal_entries || bytes >= t.wal_bytes
}

/// A periodic pass that folds repositories nobody is writing to.
///
/// Compaction is otherwise enqueued only by a write — a push, a REST
/// commit, a mirror sync. That is enough while writes keep coming, and
/// it is nothing at all for a repository that got ahead and then went
/// quiet: its WAL stays long, and the next person to push or read pays
/// for every entry in it. Two repositories have reached that state in
/// production. One was a mirror that hit 102 entries against a threshold
/// of 8 and answered reads in 5-11 s; the other had a HEAD that named no
/// branch, so every fold refused, and a push into it measured 79x what
/// the same push cost with an empty WAL.
///
/// Both of those are fixed at the source now. This is the net underneath:
/// whatever future path lets a WAL get ahead, a repository is at most one
/// sweep away from being folded. It enqueues rather than folding inline,
/// so the work goes through the same claim, lease and dedup as every
/// other fold, and a repository already below the thresholds costs one
/// manifest read to skip.
pub fn spawn_sweep(state: SharedState) {
    let every = super::env_secs("STRATUM_COMPACT_SWEEP_SECS", 3600);
    if every == 0 {
        return;
    }
    tokio::spawn(async move {
        let period = std::time::Duration::from_secs(every);
        // Start one period out, not now. `interval` fires its first tick
        // immediately, which would make every process start — and every
        // deploy, on every node — begin by reading one manifest per
        // repository to learn what a write would have told it anyway.
        // Nothing is owed at boot: a repository that got ahead did so
        // before this process existed and will still be behind an hour
        // from now.
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = sweep_once(&state).await {
                eprintln!("weft: compact sweep: {e}");
            }
        }
    });
}

/// One pass, at most one in the fleet at a time — the same discipline as
/// the GC sweep, and for the same reason: N nodes enqueueing the same
/// folds is N times the manifest reads to reach one answer.
pub async fn sweep_once(state: &SharedState) -> Result<usize, String> {
    let db = state.db.clone();
    let lock = tokio::task::spawn_blocking(move || jobs::try_lock(&db, "compact-sweep"))
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")))?;
    let Some(_lock) = lock else {
        return Ok(0);
    };
    let db = state.db.clone();
    let repos =
        tokio::task::spawn_blocking(move || stratum_control::registry::all_active_repos(&db))
            .await
            .unwrap_or_else(|e| Err(format!("join: {e}")))?;

    let mut found = 0usize;
    for repo in repos {
        let store_url = state.store_url.clone();
        let prefix = repo.prefix().as_str().to_string();
        let thresholds = CompactionThresholds::default();
        let over = tokio::task::spawn_blocking(move || -> Result<bool, String> {
            let store = ObjectStore::new(&store_url, LatencyModel::None);
            let raw = store.get(&format!("{prefix}/manifest.json"))?;
            let m: stratum_store::Manifest =
                serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
            Ok(behind(&m, &thresholds))
        })
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")));
        match over {
            // A repository with no layout yet is not behind; it is new.
            Err(_) => continue,
            Ok(false) => continue,
            Ok(true) => {
                found += 1;
                // Loud on purpose. Both times a WAL has run away in
                // production it was found by somebody outside timing a
                // push, because nothing here ever said so.
                eprintln!(
                    "weft: compact sweep: {}/{} is past the fold thresholds",
                    repo.org_id, repo.id
                );
                enqueue(state, &repo.org_id, &repo.id);
            }
        }
    }
    Ok(found)
}

pub fn spawn(state: SharedState) {
    let poll = super::env_period("STRATUM_COMPACT_POLL_SECS", 5);
    if poll.is_zero() {
        return;
    }
    // How long a claimed fold is another worker's before anyone else may
    // retry it. Ten minutes is right for production — a fold materializes
    // and re-ingests a whole repo — and wrong for anything that wants to
    // watch a node die mid-fold and a second node finish the work, which
    // would otherwise sit idle for ten minutes proving nothing.
    let lease_ms = super::lease_ms("STRATUM_COMPACT_LEASE_SECS", 600);
    tokio::spawn(async move {
        loop {
            let claimed = {
                let db = state.db.clone();
                tokio::task::spawn_blocking(move || jobs::claim(&db, "compact", lease_ms))
                    .await
                    .unwrap_or_else(|e| Err(format!("join: {e}")))
            };
            match claimed {
                Ok(Some(job)) => {
                    let outcome = run_one(&state, &job).await;
                    let db = &state.db;
                    match outcome {
                        Ok(o) => {
                            let _ = jobs::complete(db, &job.id, Some(&format!("{o:?}")));
                            let todo = follow(o);
                            if let Some(repo_id) = &job.repo_id {
                                if todo.refresh {
                                    crate::storage::refresh_by_id(&state, &job.org_id, repo_id)
                                        .await;
                                }
                                // After `complete`, so the insert is not
                                // swallowed by this job's own row.
                                if todo.requeue {
                                    enqueue(&state, &job.org_id, repo_id);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("weft: compaction failed: {e}");
                            let _ = jobs::fail(db, &job.id, &e);
                        }
                    }
                }
                Ok(None) => tokio::time::sleep(poll).await,
                Err(e) => {
                    eprintln!("weft: compact claim: {e}");
                    tokio::time::sleep(poll).await;
                }
            }
        }
    });
}

pub async fn run_one(
    state: &SharedState,
    job: &stratum_control::jobs::Job,
) -> Result<CompactOutcome, String> {
    let Some(repo_id) = job.repo_id.clone() else {
        return Err("compact job without repo".into());
    };
    let Some(repo) = stratum_control::registry::repo_by_id(&state.db, &job.org_id, &repo_id)?
    else {
        // Deleted since; nothing to fold.
        return Ok(CompactOutcome::NotNeeded);
    };
    let store_url = state.store_url.clone();
    let prefix = repo.prefix().as_str().to_string();
    let work = super::job_work_dir(&state.data_dir, "compact", &repo_id, &job.id);
    let outcome = tokio::task::spawn_blocking(move || {
        let store = ObjectStore::new(&store_url, LatencyModel::None);
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let out = compact(
            &store,
            &prefix,
            &crate::app::ingest_config_from_env(),
            &CompactionThresholds::default(),
            &work,
        );
        // This run's directory, and only this run's: it is named after
        // the job (`workers::job_work_dir`) so removing it cannot take
        // another compaction's tree with it, and leaving it would grow
        // one directory per job forever.
        let _ = std::fs::remove_dir_all(&work);
        out
    })
    .await
    .unwrap_or_else(|e| Err(format!("join: {e}")))?;
    // The commit count rides on this job because this job already runs
    // after every accepted write. It is not allowed to fail the fold: a
    // count that could not be taken leaves the old row standing, and
    // the next write's job tries again.
    if let Err(e) = super::commit_count::refresh(state, &repo).await {
        eprintln!("weft: commit count for {}: {e}", repo.id);
    }
    Ok(outcome)
}

#[cfg(test)]
mod sweep_tests {
    use super::*;

    fn manifest(entries: usize, bytes_each: u64) -> stratum_store::Manifest {
        let mut m: stratum_store::Manifest = serde_json::from_str(
            r#"{"schema":1,"repo":"r","layout":"L","refs":[],"head":"refs/heads/main","epoch":"e1"}"#,
        )
        .unwrap();
        m.wal = (0..entries)
            .map(|i| stratum_store::manifest::WalEntry {
                key: format!("w{i}"),
                oids_key: format!("w{i}.oids"),
                bytes: bytes_each,
                entries: 1,
                updates: Vec::new(),
            })
            .collect();
        m
    }

    /// The sweep must ask exactly what the fold asks. A looser rule
    /// enqueues folds that answer `NotNeeded`; a stricter one leaves
    /// behind the repositories this exists to catch.
    #[test]
    fn the_sweep_asks_the_same_question_the_fold_does() {
        let t = CompactionThresholds::default();
        assert!(!behind(&manifest(0, 0), &t), "an empty WAL is not behind");
        assert!(
            !behind(&manifest(t.wal_entries - 1, 1), &t),
            "one under the entry threshold is not behind"
        );
        assert!(
            behind(&manifest(t.wal_entries, 1), &t),
            "at the entry threshold a fold is owed"
        );
        // The 61-entry repository that measured 79x: firmly behind.
        assert!(behind(&manifest(61, 1), &t));
        // Bytes are the other door, and either one is enough.
        assert!(
            behind(&manifest(1, t.wal_bytes), &t),
            "one large entry is past the byte threshold"
        );
        assert!(!behind(&manifest(1, t.wal_bytes - 1), &t));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A fold always leaves a follow-up; nothing else does.**
    ///
    /// The bug this pins: only `LostRace` used to requeue, so entries
    /// written while a fold was running stayed in the WAL with nothing
    /// scheduled to fold them. The rule has to hold for `Compacted`
    /// especially — that is the outcome a busy repository produces.
    ///
    /// `NotNeeded` must queue nothing, and that half is not decoration:
    /// it is what stops the follow-up from folding in a circle.
    #[test]
    fn a_fold_leaves_a_follow_up_and_a_no_op_leaves_nothing() {
        assert_eq!(
            follow(CompactOutcome::Compacted),
            Follow {
                refresh: true,
                requeue: true
            },
            "a fold moved the bytes and may have left WAL entries behind it"
        );
        assert_eq!(
            follow(CompactOutcome::LostRace),
            Follow {
                refresh: false,
                requeue: true
            },
            "a lost race folded nothing, so there is nothing to refresh"
        );
        assert_eq!(
            follow(CompactOutcome::NotNeeded),
            Follow {
                refresh: false,
                requeue: false
            },
            "a follow-up that requeues itself never stops"
        );
    }
}
