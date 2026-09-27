//! The storage sweep: every repository's logical bytes re-derived from
//! its manifest each hour, and — once a day — the bucket itself listed
//! to see what is physically there.
//!
//! **SET, not increment.** The post-write hooks (`crate::storage`) set
//! each row from the manifest the moment a write lands, and this sweep
//! sets it again from the same manifest. Neither adds anything to
//! anything, so a hook that died between the manifest CAS and its row,
//! a node that was SIGKILLed mid-fold, or a row somebody edited by hand
//! is corrected by the next tick rather than compounded by it. The
//! sweep also resyncs `private` from `repos.public`, so a visibility
//! flip that raced a write is right within the hour.
//!
//! **Physical bytes are the operator's.** The inventory sums `<Size>`
//! over every key under the repository's prefix: the manifest's objects,
//! and also exports, CDN packs, audit shards and whatever a GC has not
//! yet swept. It is recorded beside the logical figure and compared with
//! it — a prefix holding twice what its manifest names, or less than it,
//! is printed as drift for an operator to look at — while a repository
//! shows what the manifest names, because that is the number a person
//! can reason about by looking at their repository.
//!
//! `STRATUM_STORAGE_SWEEP_SECS` (3600; 0 disables) is the tick, and
//! `STRATUM_STORAGE_INVENTORY_SECS` (86400) how often a tick also
//! inventories. Both run under one fleet-wide lock.

use crate::app::SharedState;
use stratum_control::ids::now_ms;
use stratum_control::storage as ctl;
use stratum_control::{jobs, registry, ControlDb};
use stratum_store::{LatencyModel, ObjectStore};

/// Name of the fleet-wide lock this worker runs under. See
/// [`jobs::try_lock`].
const WORKER: &str = "storage-sweep";
/// The cursor the last inventory's moment is kept under, in `meta`.
const INVENTORY_CURSOR: &str = "storage_inventory_at";

pub fn spawn(state: SharedState) {
    let every = super::env_secs("STRATUM_STORAGE_SWEEP_SECS", 3600);
    if every == 0 {
        return;
    }
    let inventory_secs = super::env_secs("STRATUM_STORAGE_INVENTORY_SECS", 86_400);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(every));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let db = state.db.clone();
            let store_url = state.store_url.clone();
            let out = tokio::task::spawn_blocking(move || {
                sweep(&db, &store_url, inventory_secs, now_ms())
            })
            .await;
            if let Ok(Err(e)) = out {
                eprintln!("weft: storage sweep: {e}");
            }
        }
    });
}

/// One sweep — at most one in the fleet at a time. `Ok(false)` when
/// another node held the lock.
///
/// A repository whose manifest cannot be read is logged and skipped,
/// not allowed to stop the sweep: the rest of the fleet's rows are
/// still worth setting, and that one keeps its last value until the
/// store answers again.
pub fn sweep(
    db: &ControlDb,
    store_url: &str,
    inventory_secs: u64,
    now: i64,
) -> Result<bool, String> {
    let Some(_lock) = jobs::try_lock(db, WORKER)? else {
        return Ok(false);
    };
    // A repository tombstoned by any path that did not clear its row —
    // the delete handlers do, but a row that outlives its repository
    // would be counted forever, so the sweep is the floor.
    ctl::prune_missing_repos(db)?;
    let repos = registry::all_active_repos(db)?;
    for repo in &repos {
        if let Err(e) = crate::storage::refresh_repo_with(db, store_url, repo) {
            eprintln!("weft: storage sweep {}: {e}", repo.id);
        }
    }
    let last = stratum_control::webhooks::meta_get(db, INVENTORY_CURSOR)?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let due = i64::try_from(inventory_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    if now.saturating_sub(last) >= due {
        for (repo_id, why, logical, physical) in inventory(db, store_url, &repos, now) {
            eprintln!(
                "weft: storage drift {repo_id}: {why} (logical {logical} B, physical {physical} B)"
            );
        }
        jobs::advance_cursor(db, INVENTORY_CURSOR, now)?;
    }
    Ok(true)
}

/// LIST each repository's prefix and record what the bucket holds. A
/// prefix that will not list is logged and skipped like a manifest that
/// will not read. Answers every repository whose prefix disagrees with
/// its manifest — see [`drift`] — as `(repo id, why, logical, physical)`
/// for the caller to log: the disagreement is the whole point of the
/// listing, and a test can watch for it here where it cannot watch a
/// log line.
fn inventory(
    db: &ControlDb,
    store_url: &str,
    repos: &[registry::Repo],
    now: i64,
) -> Vec<(String, &'static str, u64, u64)> {
    let store = ObjectStore::new(store_url, LatencyModel::None);
    let mut drifted = Vec::new();
    for repo in repos {
        let prefix = repo.prefix().as_str().to_string();
        let physical: u64 = match store.list_sized(&format!("{prefix}/")) {
            Ok(keys) => keys.iter().map(|(_, size)| size).sum(),
            Err(e) => {
                eprintln!("weft: storage inventory {}: {e}", repo.id);
                continue;
            }
        };
        if let Err(e) = ctl::set_physical(db, ctl::OWNER_REPO, &repo.id, physical, now) {
            eprintln!("weft: storage inventory {}: {e}", repo.id);
            continue;
        }
        let logical = ctl::owner_bytes(db, ctl::OWNER_REPO, &repo.id)
            .ok()
            .flatten()
            .map(|(l, _)| l.max(0) as u64)
            .unwrap_or(0);
        if let Some(why) = drift(logical, physical) {
            drifted.push((repo.id.clone(), why, logical, physical));
        }
    }
    drifted
}

/// Sidecars a prefix always carries that no manifest counts — the
/// manifest itself, the locator header, a CDN pack's index. Below this
/// much excess "twice the manifest" is a small repository, not drift.
const DRIFT_FLOOR: u64 = 1 << 20;

/// The two shapes of disagreement worth a line in the log. A prefix
/// holding less than its manifest names is a manifest pointing at
/// something that is not there; one holding more than twice as much,
/// by more than a megabyte, is a sweep that has stopped deleting, or an
/// export nobody cleaned up. Anything in between is the ordinary
/// overhead of packs and shards.
fn drift(logical: u64, physical: u64) -> Option<&'static str> {
    if physical < logical {
        Some("the bucket holds less than the manifest names")
    } else if physical > logical.saturating_mul(2) && physical - logical > DRIFT_FLOOR {
        Some("the bucket holds more than twice what the manifest names")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One node sweeps; the rest of the fleet stands down. Without the
    /// lock every node re-read every manifest every tick.
    #[test]
    fn a_node_that_loses_the_lock_does_not_sweep() {
        let url = stratum_testkit::pg::test_db_url("storage-sweep-lock");
        let a = ControlDb::open(&url).unwrap();
        let b = ControlDb::open(&url).unwrap();
        registry::create_org(&a, "acme").unwrap();
        // No repositories, so no store is ever spoken to.
        let store = "http://127.0.0.1:1/never";

        let held = jobs::try_lock(&b, WORKER).unwrap().expect("b is sweeping");
        assert!(!sweep(&a, store, 86_400, now_ms()).unwrap());
        assert_eq!(
            stratum_control::webhooks::meta_get(&a, INVENTORY_CURSOR).unwrap(),
            None,
            "a locked-out node swept anyway"
        );

        drop(held);
        assert!(sweep(&a, store, 86_400, now_ms()).unwrap());
        // The inventory ran on the first sweep (nothing had ever run)
        // and not again inside its interval, which the cursor records.
        let first = stratum_control::webhooks::meta_get(&a, INVENTORY_CURSOR)
            .unwrap()
            .expect("the first sweep inventoried");
        assert!(sweep(&a, store, 86_400, now_ms()).unwrap());
        assert_eq!(
            stratum_control::webhooks::meta_get(&a, INVENTORY_CURSOR).unwrap(),
            Some(first),
            "the inventory ran again inside its interval"
        );
    }

    /// A store that cannot be reached costs one line per repository and
    /// not the sweep: the row keeps whatever it last said, and the
    /// inventory skips what it cannot list.
    #[test]
    fn a_repository_whose_store_is_unreachable_is_skipped_not_fatal() {
        let url = stratum_testkit::pg::test_db_url("storage-sweep-unreachable");
        let db = ControlDb::open(&url).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let repo = registry::create_repo(
            &db,
            &org.id,
            &registry::NewRepo {
                description: None,
                name: "app",
                kind: registry::RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        ctl::set_logical(&db, ctl::OWNER_REPO, &repo.id, &org.id, true, 500, 1).unwrap();
        assert!(sweep(&db, "http://127.0.0.1:1/never", 0, now_ms()).unwrap());
        assert_eq!(
            ctl::owner_bytes(&db, ctl::OWNER_REPO, &repo.id).unwrap(),
            Some((500, None)),
            "the row kept its last value, and an inventory that could not \
             list recorded nothing"
        );
    }

    /// The inventory finds what the manifest does not name. A prefix
    /// holding four mebibytes of something no manifest points at — a
    /// pack the sweep stopped deleting, an export nobody cleaned up —
    /// is the drift the daily listing exists to notice, and it is
    /// answered by name so an operator's log line has the repository
    /// and both numbers in it.
    #[test]
    fn an_inventory_names_a_prefix_holding_far_more_than_its_manifest() {
        let url = stratum_testkit::pg::test_db_url("storage-inventory-drift");
        let db = ControlDb::open(&url).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let repo = registry::create_repo(
            &db,
            &org.id,
            &registry::NewRepo {
                description: None,
                name: "app",
                kind: registry::RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        let bucket = stratum_testkit::minio::Minio::shared().bucket("storage-drift");
        let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
        let prefix = repo.prefix().as_str().to_string();
        // No manifest, so the sweep records zero logical bytes — and a
        // stray object four times the drift floor under the same prefix.
        store
            .put(
                &format!("{prefix}/exports/forgotten.bundle"),
                &vec![0u8; 4 * DRIFT_FLOOR as usize],
                stratum_store::PutCond::None,
            )
            .unwrap();
        // The sweep reads every manifest before it lists: the row the
        // inventory writes physical bytes beside is the one that read
        // made. Same order here, or there is no row to write beside.
        assert_eq!(
            crate::storage::refresh_repo_with(&db, &bucket.base_url, &repo).unwrap(),
            0,
            "no manifest reads as nothing stored"
        );
        // The sweep itself, with the inventory due at once: it reads
        // the manifest, samples the day, lists, and reports the drift.
        assert!(sweep(&db, &bucket.base_url, 0, now_ms()).unwrap());
        let repos = registry::all_active_repos(&db).unwrap();
        let found = inventory(&db, &bucket.base_url, &repos, now_ms());
        assert_eq!(found.len(), 1, "{found:?}");
        let (id, why, logical, physical) = &found[0];
        assert_eq!(id, &repo.id);
        assert_eq!(*logical, 0);
        assert!(*physical >= 4 * DRIFT_FLOOR, "{physical}");
        assert!(why.contains("twice") || why.contains("more"), "{why}");
        assert_eq!(
            ctl::owner_bytes(&db, ctl::OWNER_REPO, &repo.id)
                .unwrap()
                .and_then(|(_, p)| p),
            Some(*physical as i64),
            "the physical bytes were recorded beside the logical ones"
        );
        // A prefix in step with its manifest is not named.
        ctl::set_logical(
            &db,
            ctl::OWNER_REPO,
            &repo.id,
            &org.id,
            true,
            3 * DRIFT_FLOOR,
            1,
        )
        .unwrap();
        assert!(inventory(&db, &bucket.base_url, &repos, now_ms()).is_empty());
    }

    #[test]
    fn drift_is_less_than_the_manifest_or_well_over_twice_it() {
        assert_eq!(drift(0, 0), None);
        assert_eq!(
            drift(0, 5_000),
            None,
            "an empty repository's prefix still holds its manifest"
        );
        assert_eq!(
            drift(243, 1_106),
            None,
            "a tiny repository is mostly sidecars"
        );
        assert_eq!(
            drift(1_000, 100_000),
            None,
            "far over twice, but under a megabyte"
        );
        let mib = DRIFT_FLOOR;
        assert_eq!(drift(mib, 2 * mib), None, "exactly twice is not over");
        assert!(drift(mib, 2 * mib + 1).is_some());
        assert!(drift(100, 99).is_some());
    }
}
