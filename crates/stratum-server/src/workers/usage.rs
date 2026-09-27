//! The usage rollup: each organization's day — active repositories,
//! requests, bytes served — folded from `metrics_minute` into
//! `usage_daily`, which is what the usage page draws. Dormant repos cost
//! nothing; an org with no traffic produces a zero row.
//!
//! One hourly pass (`STRATUM_USAGE_ROLLUP_SECS`, 0 disables) under one
//! fleet-wide lock. The upsert is idempotent, so N nodes folding the same
//! day would produce the right numbers — N times, each re-reading every
//! org's minute rows for two whole days. The lock makes the cost of a
//! pass independent of how many nodes are running.

use crate::app::SharedState;
use std::time::Duration;
use stratum_control::ids::now_ms;
use stratum_control::{jobs, metrics, registry, ControlDb};

/// Name of the fleet-wide lock this worker runs under. See
/// [`jobs::try_lock`].
const WORKER: &str = "usage-rollup";

pub fn spawn(state: SharedState) {
    let every = super::env_secs("STRATUM_USAGE_ROLLUP_SECS", 3600);
    if every == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(every));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let db = state.db.clone();
            let out = tokio::task::spawn_blocking(move || locked(&db, || rollup_all(&db))).await;
            if let Ok(Err(e)) = out {
                eprintln!("weft: usage rollup: {e}");
            }
        }
    });
}

/// Run `pass` if this node wins the worker's lock; do nothing if
/// another node holds it.
pub fn locked(db: &ControlDb, pass: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    let held = jobs::try_lock(db, WORKER)?;
    if held.is_some() {
        pass()?;
    }
    Ok(())
}

/// Fold every org's day into `usage_daily`. Called under the lock.
///
/// Folds yesterday as well as today. Requests that land between a day's
/// final tick and midnight would otherwise never be folded: the next
/// tick computes the *new* day only, and the tail would simply go
/// missing from `usage_daily` (up to an hour of it at the default
/// interval). Re-folding a finished day is idempotent — the upsert
/// overwrites the row with the recomputed totals.
pub fn rollup_all(db: &ControlDb) -> Result<(), String> {
    let now = now_ms();
    let today_start = now - now % 86_400_000;
    for day_start in [today_start - 86_400_000, today_start] {
        let day = day_label(day_start);
        for org in registry::all_org_ids(db)? {
            let usage = metrics::org_day_usage(db, &org, day_start)?;
            let total = registry::count_repos(db, &org)?;
            metrics::upsert_usage(
                db,
                &org,
                &day,
                usage.active_repos,
                total,
                usage.requests,
                usage.bytes_out,
            )?;
        }
    }
    Ok(())
}

/// `YYYY-MM-DD` for the UTC day starting at `day_start_ms`.
pub(crate) fn day_label(day_start_ms: i64) -> String {
    let secs = (day_start_ms / 1000) as u64;
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One node folds; the rest of the fleet stands down.
    #[test]
    fn a_node_that_loses_the_lock_does_not_fold() {
        let url = stratum_testkit::pg::test_db_url("rollup-lock");
        let a = ControlDb::open(&url).unwrap();
        let b = ControlDb::open(&url).unwrap();
        let org = registry::create_org(&a, "acme").unwrap();

        let held = jobs::try_lock(&b, WORKER).unwrap().expect("b is folding");
        locked(&a, || rollup_all(&a)).unwrap();
        assert!(
            metrics::usage_days(&a, &org.id, 90).unwrap().is_empty(),
            "a locked-out node folded anyway"
        );

        // A pass, not a failure: with the lock free the same call folds.
        drop(held);
        locked(&a, || rollup_all(&a)).unwrap();
        assert!(!metrics::usage_days(&a, &org.id, 90).unwrap().is_empty());
    }

    /// Activity that lands after a day's final tick sits in *yesterday's*
    /// window by the time the next tick runs. Folding today alone would
    /// leave it out of `usage_daily` forever; this fails with the fold
    /// loop reduced to today.
    #[test]
    fn the_rollup_folds_yesterdays_tail_not_just_today() {
        let url = stratum_testkit::pg::test_db_url("rollup-tail");
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
        let now = now_ms();
        let yesterday_minute = (now - now % 86_400_000 - 60_000) / 60_000;
        let mut direct = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        direct
            .execute(
                "INSERT INTO metrics_minute (repo_id, minute, kind, count, bytes, ms_sum, histogram) \
                 VALUES ($1, $2, 'clone', 3, 3000, 0, '{}')",
                &[&repo.id, &yesterday_minute],
            )
            .unwrap();
        rollup_all(&db).unwrap();
        let yesterday = day_label(now - now % 86_400_000 - 86_400_000);
        let days = metrics::usage_days(&db, &org.id, 90).unwrap();
        let row = days
            .iter()
            .find(|d| d.day == yesterday)
            .expect("yesterday was folded");
        assert_eq!(row.requests, 3);
        assert_eq!(row.bytes_out, 3000);
        assert_eq!(row.active_repos, 1);
    }

    #[test]
    fn a_day_label_is_the_utc_date() {
        assert_eq!(day_label(0), "1970-01-01");
        assert_eq!(day_label(951_782_400_000), "2000-02-29");
        assert_eq!(
            day_label(1_800_000_000_000 - 1_800_000_000_000 % 86_400_000),
            "2027-01-15"
        );
    }
}
