//! What each repository holds, and what each organization held over a
//! billing period: the storage meter's control-plane truth.
//!
//! Two tables. `storage_usage` has one row per owner — a repository
//! today, a hosted package tomorrow, distinguished by `owner_kind` and
//! nothing else — carrying the **logical** bytes its manifest names and,
//! when an inventory has run, the **physical** bytes the bucket holds
//! under its prefix. `storage_daily` folds hourly samples of an
//! organization's private bytes into one row per day, and the period's
//! GB-month is the average of the days.
//!
//! Every write here is a **SET**, never an increment. The number a
//! repository is worth is derived from its manifest after each write
//! and by the sweep (`Manifest::stored_bytes`), so a process that died
//! between a manifest CAS and the row it should have written leaves a
//! stale value that the next write or sweep replaces, not a drift that
//! compounds. Only `sample_day` accumulates, and what it accumulates is
//! samples of an already-idempotent value.
//!
//! Only private bytes bill. Public repositories are free and mirrors
//! count when private; a row's `private` column is the visibility at
//! the time of the last SET and is resynced from `repos.public` by the
//! sweep, so a flip that raced a write is corrected within the hour.

use crate::db::ControlDb;
use serde::Serialize;

/// One binary gigabyte — the unit the allowance and the overage price
/// are quoted in. Binary rather than decimal because the meter's
/// smallest reported unit is a binary megabyte (`period_mb_days`) and
/// the two have to nest.
pub const GIB: u64 = 1 << 30;
/// One binary megabyte: the unit `period_mb_days` reports in.
pub const MIB: u64 = 1 << 20;

/// Owner kinds the table admits.
///
/// The kind is **not** cosmetic, and an earlier version of this comment
/// claimed a package registry could be added "with nothing else
/// changing". That was wrong in the one way that costs money: the org
/// sums below are what the storage cap and the storage meter read, so a
/// second kind summed into them bills package bytes on the git storage
/// line and counts them against the cap that gates pushes. Every sum
/// here therefore names the kind it wants, and there is no kind-blind
/// total to reach for by accident.
pub const OWNER_REPO: &str = "repo";
/// The package registry's bytes. One row per organization rather than
/// one per package — see `crate::storage::refresh_packages` in the
/// server for why.
pub const OWNER_PACKAGE: &str = "package";

/// SET the logical bytes of one owner, creating its row if this is the
/// first write. `private` is recorded alongside so the org sum can be
/// answered from the index without a join.
pub fn set_logical(
    db: &ControlDb,
    owner_kind: &str,
    owner_id: &str,
    org_id: &str,
    private: bool,
    bytes: u64,
    now: i64,
) -> Result<(), String> {
    db.lock()
        .execute(
            "INSERT INTO storage_usage \
                 (owner_kind, owner_id, org_id, private, logical_bytes, sampled_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (owner_kind, owner_id) DO UPDATE SET \
                 org_id = $3, private = $4, logical_bytes = $5, sampled_at = $6",
            &[
                &owner_kind,
                &owner_id,
                &org_id,
                &private,
                &clamp(bytes),
                &now,
            ],
        )
        .map(|_| ())
        .map_err(|e| format!("set logical bytes: {e}"))
}

/// SET the physical bytes of an owner that already has a row. An owner
/// with no row has no logical bytes either, and an inventory of it
/// would be an inventory of nothing; the sweep refreshes logical bytes
/// before it inventories, so the row exists by then.
pub fn set_physical(
    db: &ControlDb,
    owner_kind: &str,
    owner_id: &str,
    bytes: u64,
    now: i64,
) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE storage_usage SET physical_bytes = $3, inventoried_at = $4 \
             WHERE owner_kind = $1 AND owner_id = $2",
            &[&owner_kind, &owner_id, &clamp(bytes), &now],
        )
        .map(|_| ())
        .map_err(|e| format!("set physical bytes: {e}"))
}

/// Move an owner's bytes between the private pool and the free one. A
/// no-op for an owner with no row, which is one that has never been
/// written to and holds nothing.
pub fn set_private(
    db: &ControlDb,
    owner_kind: &str,
    owner_id: &str,
    private: bool,
) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE storage_usage SET private = $3 WHERE owner_kind = $1 AND owner_id = $2",
            &[&owner_kind, &owner_id, &private],
        )
        .map(|_| ())
        .map_err(|e| format!("set private: {e}"))
}

/// Forget an owner. Deleting a repository stops its bill at once, even
/// though its objects are swept from the bucket later: the customer
/// deleted it, and what the GC has not got round to is not theirs.
pub fn remove(db: &ControlDb, owner_kind: &str, owner_id: &str) -> Result<(), String> {
    db.lock()
        .execute(
            "DELETE FROM storage_usage WHERE owner_kind = $1 AND owner_id = $2",
            &[&owner_kind, &owner_id],
        )
        .map(|_| ())
        .map_err(|e| format!("remove storage row: {e}"))
}

/// Forget every repository row whose repository is no longer active.
/// The delete handlers call [`remove`] as they tombstone; this is the
/// sweep's floor under them, so nothing deleted by another path is
/// billed forever. Returns how many rows went.
pub fn prune_missing_repos(db: &ControlDb) -> Result<u64, String> {
    db.lock()
        .execute(
            "DELETE FROM storage_usage WHERE owner_kind = 'repo' AND owner_id NOT IN \
                 (SELECT id FROM repos WHERE state = 'active')",
            &[],
        )
        .map_err(|e| format!("prune storage rows: {e}"))
}

/// Logical bytes in the org's private owners **of one kind**: the number
/// the cap is checked against and the number the daily sample records.
///
/// The kind is required rather than defaulted. Summing every kind is
/// what would quietly bill package bytes as git storage, and a parameter
/// somebody has to supply is the cheapest way to make that a decision
/// instead of an oversight.
pub fn org_private_bytes(db: &ControlDb, org_id: &str, owner_kind: &str) -> Result<i64, String> {
    db.lock()
        .query_one(
            "SELECT COALESCE(SUM(logical_bytes), 0)::BIGINT FROM storage_usage \
             WHERE org_id = $1 AND owner_kind = $2 AND private",
            &[&org_id, &owner_kind],
        )
        .map(|r| r.get(0))
        .map_err(|e| format!("org private bytes: {e}"))
}

/// Logical bytes in every owner of the org of one kind, public and
/// private.
pub fn org_total_bytes(db: &ControlDb, org_id: &str, owner_kind: &str) -> Result<i64, String> {
    db.lock()
        .query_one(
            "SELECT COALESCE(SUM(logical_bytes), 0)::BIGINT FROM storage_usage \
             WHERE org_id = $1 AND owner_kind = $2",
            &[&org_id, &owner_kind],
        )
        .map(|r| r.get(0))
        .map_err(|e| format!("org total bytes: {e}"))
}

/// One owner's `(logical, physical)` bytes; `None` for an owner never
/// written to. Physical is `None` until an inventory has run.
pub fn owner_bytes(
    db: &ControlDb,
    owner_kind: &str,
    owner_id: &str,
) -> Result<Option<(i64, Option<i64>)>, String> {
    db.lock()
        .query_opt(
            "SELECT logical_bytes, physical_bytes FROM storage_usage \
             WHERE owner_kind = $1 AND owner_id = $2",
            &[&owner_kind, &owner_id],
        )
        .map(|r| r.map(|r| (r.get(0), r.get(1))))
        .map_err(|e| format!("owner bytes: {e}"))
}

/// Fold one sample of the org's bytes into its day. Samples count up,
/// the private sum accumulates towards an average, the maximum is kept
/// for the billing page, and the total is simply the latest.
pub fn sample_day(
    db: &ControlDb,
    org_id: &str,
    day: &str,
    owner_kind: &str,
    private_bytes: i64,
    total_bytes: i64,
) -> Result<(), String> {
    db.lock()
        .execute(
            "INSERT INTO storage_daily \
                 (org_id, day, owner_kind, samples, private_bytes_sum, private_bytes_max, \
                  total_bytes_last) \
             VALUES ($1, $2, $5, 1, $3, $3, $4) \
             ON CONFLICT (org_id, day, owner_kind) DO UPDATE SET \
                 samples = storage_daily.samples + 1, \
                 private_bytes_sum = storage_daily.private_bytes_sum + $3, \
                 private_bytes_max = GREATEST(storage_daily.private_bytes_max, $3), \
                 total_bytes_last = $4",
            &[&org_id, &day, &private_bytes, &total_bytes, &owner_kind],
        )
        .map(|_| ())
        .map_err(|e| format!("sample storage day: {e}"))
}

/// One day of an org's storage **of one kind**, as the billing page
/// shows it.
///
/// The kind is a parameter for the same reason [`org_private_bytes`]
/// takes one: the table holds a row per kind per day now, and a query
/// that forgot to say which would return two rows for one day and draw
/// a chart nobody could reconcile with an invoice.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StorageDay {
    pub day: String,
    pub samples: i64,
    /// Average private bytes over the day's samples — what the meter
    /// reports.
    pub private_bytes_avg: i64,
    pub private_bytes_max: i64,
    pub total_bytes_last: i64,
}

/// The org's most recent `limit` days, newest first.
pub fn storage_days(
    db: &ControlDb,
    org_id: &str,
    owner_kind: &str,
    limit: usize,
) -> Result<Vec<StorageDay>, String> {
    let limit = limit.clamp(1, 1000) as i64;
    let rows = db
        .lock()
        .query(
            "SELECT day, samples, private_bytes_sum, private_bytes_max, total_bytes_last \
             FROM storage_daily WHERE org_id = $1 AND owner_kind = $3 \
             ORDER BY day DESC LIMIT $2",
            &[&org_id, &limit, &owner_kind],
        )
        .map_err(|e| format!("storage days: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| {
            let samples: i64 = r.get(1);
            let sum: i64 = r.get(2);
            StorageDay {
                day: r.get(0),
                samples,
                private_bytes_avg: if samples > 0 { sum / samples } else { 0 },
                private_bytes_max: r.get(3),
                total_bytes_last: r.get(4),
            }
        })
        .collect())
}

/// Megabyte-days of private storage over `from_day..=to_day`: the sum,
/// over each day in the range, of that day's average private bytes in
/// binary megabytes, rounded down per day. This integer is the unit
/// reported to the provider's storage meter; the metered price divides
/// it by the days in the month to arrive at GB-months.
pub fn period_mb_days(
    db: &ControlDb,
    org_id: &str,
    from_day: &str,
    to_day: &str,
    owner_kind: &str,
) -> Result<i64, String> {
    db.lock()
        .query_one(
            "SELECT COALESCE(SUM( \
                 CASE WHEN samples > 0 THEN (private_bytes_sum / samples) / $4 ELSE 0 END \
             ), 0)::BIGINT \
             FROM storage_daily \
             WHERE org_id = $1 AND day >= $2 AND day <= $3 AND owner_kind = $5",
            &[&org_id, &from_day, &to_day, &(MIB as i64), &owner_kind],
        )
        .map(|r| r.get(0))
        .map_err(|e| format!("period mb-days: {e}"))
}

/// What `seats` seats include, in bytes; `None` when the deployment
/// sets no per-seat allowance (zero), which means unlimited.
pub fn included_bytes(seats: i64, gb_per_seat: i64) -> Option<u64> {
    if gb_per_seat <= 0 {
        return None;
    }
    Some((seats.max(0) as u64).saturating_mul(gb_per_seat as u64 * GIB))
}

/// How many more bytes fit under `cap` given `used`; `None` when there
/// is no cap. Saturates at zero: an org already past its cap has no
/// room, not negative room.
pub fn room(cap: Option<u64>, used: u64) -> Option<u64> {
    cap.map(|c| c.saturating_sub(used))
}

/// BIGINT columns: a byte count past `i64::MAX` is not a repository
/// anybody has, but a cast that wraps would make it a negative one.
fn clamp(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry;

    fn org(db: &ControlDb, name: &str) -> String {
        registry::create_org(db, name).unwrap().id
    }

    /// The owner kind is what keeps two products' bytes apart.
    ///
    /// This is the regression the kind-blind version of these sums would
    /// have had, and it costs money in both directions: package bytes
    /// summed into the repository total bill on the git storage line and
    /// count against the cap that gates pushes, and repository bytes
    /// summed into the package total bill twice.
    #[test]
    fn one_kinds_bytes_are_never_the_other_kinds() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("storage-kinds")).unwrap();
        let acme = org(&db, "acme");

        set_logical(&db, OWNER_REPO, "r1", &acme, true, 1_000, 10).unwrap();
        set_logical(&db, OWNER_PACKAGE, &acme, &acme, true, 250, 10).unwrap();

        assert_eq!(org_private_bytes(&db, &acme, OWNER_REPO).unwrap(), 1_000);
        assert_eq!(org_private_bytes(&db, &acme, OWNER_PACKAGE).unwrap(), 250);
        assert_eq!(org_total_bytes(&db, &acme, OWNER_REPO).unwrap(), 1_000);
        assert_eq!(org_total_bytes(&db, &acme, OWNER_PACKAGE).unwrap(), 250);

        // …and the two rows are genuinely separate rows, keyed apart by
        // kind even when a repository and an organization share an id.
        assert_eq!(
            owner_bytes(&db, OWNER_REPO, "r1").unwrap(),
            Some((1_000, None))
        );
        assert_eq!(
            owner_bytes(&db, OWNER_PACKAGE, &acme).unwrap(),
            Some((250, None))
        );
        assert_eq!(owner_bytes(&db, OWNER_REPO, &acme).unwrap(), None);
    }

    #[test]
    fn writes_are_sets_and_the_org_sums_follow_visibility() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("storage-set")).unwrap();
        let acme = org(&db, "acme");
        let other = org(&db, "other");
        assert_eq!(owner_bytes(&db, OWNER_REPO, "r1").unwrap(), None);
        assert_eq!(org_private_bytes(&db, &acme, OWNER_REPO).unwrap(), 0);

        set_logical(&db, OWNER_REPO, "r1", &acme, true, 1_000, 10).unwrap();
        set_logical(&db, OWNER_REPO, "r1", &acme, true, 700, 11).unwrap();
        set_logical(&db, OWNER_REPO, "r2", &acme, false, 50, 12).unwrap();
        set_logical(&db, OWNER_REPO, "r3", &other, true, 9_999, 13).unwrap();
        assert_eq!(
            owner_bytes(&db, OWNER_REPO, "r1").unwrap(),
            Some((700, None)),
            "the second write replaced the first, not added to it"
        );
        assert_eq!(org_private_bytes(&db, &acme, OWNER_REPO).unwrap(), 700);
        assert_eq!(org_total_bytes(&db, &acme, OWNER_REPO).unwrap(), 750);

        set_private(&db, OWNER_REPO, "r1", false).unwrap();
        assert_eq!(org_private_bytes(&db, &acme, OWNER_REPO).unwrap(), 0);
        set_private(&db, OWNER_REPO, "r2", true).unwrap();
        assert_eq!(org_private_bytes(&db, &acme, OWNER_REPO).unwrap(), 50);
        // No row, no error: a never-written repository holds nothing.
        set_private(&db, OWNER_REPO, "never", true).unwrap();
        set_physical(&db, OWNER_REPO, "never", 5, 1).unwrap();
        assert_eq!(owner_bytes(&db, OWNER_REPO, "never").unwrap(), None);

        set_physical(&db, OWNER_REPO, "r1", 1_400, 20).unwrap();
        assert_eq!(
            owner_bytes(&db, OWNER_REPO, "r1").unwrap(),
            Some((700, Some(1_400)))
        );
        // A second kind in the same table, keyed apart from the repo —
        // and summed apart from it. This assertion used to read 753,
        // back when the org sums were kind-blind and a package's bytes
        // therefore landed on the git storage line.
        set_logical(&db, OWNER_PACKAGE, "r1", &acme, true, 3, 21).unwrap();
        assert_eq!(
            owner_bytes(&db, OWNER_PACKAGE, "r1").unwrap(),
            Some((3, None))
        );
        assert_eq!(org_total_bytes(&db, &acme, OWNER_REPO).unwrap(), 750);
        assert_eq!(org_total_bytes(&db, &acme, OWNER_PACKAGE).unwrap(), 3);

        remove(&db, OWNER_REPO, "r1").unwrap();
        assert_eq!(owner_bytes(&db, OWNER_REPO, "r1").unwrap(), None);
        assert_eq!(org_total_bytes(&db, &acme, OWNER_REPO).unwrap(), 50);
        assert_eq!(
            org_total_bytes(&db, &acme, OWNER_PACKAGE).unwrap(),
            3,
            "removing a repository took a package row with it"
        );
        remove(&db, OWNER_REPO, "r1").unwrap();
        // The floor under the delete handlers: a row whose repository
        // is gone, or never was, goes with the next prune; a live one
        // and a package row stay.
        let live = registry::create_repo(
            &db,
            &acme,
            &registry::NewRepo {
                description: None,
                name: "live",
                kind: registry::RepoKind::Native,
                public: false,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        set_logical(&db, OWNER_REPO, &live.id, &acme, true, 9, 40).unwrap();
        assert_eq!(
            prune_missing_repos(&db).unwrap(),
            2,
            "r2 and r3 had no repository"
        );
        assert_eq!(
            owner_bytes(&db, OWNER_REPO, &live.id).unwrap(),
            Some((9, None))
        );
        assert_eq!(owner_bytes(&db, "package", "r1").unwrap(), Some((3, None)));
        assert!(registry::delete_repo(&db, &acme, &live.id).unwrap());
        assert_eq!(prune_missing_repos(&db).unwrap(), 1);
        assert_eq!(owner_bytes(&db, OWNER_REPO, &live.id).unwrap(), None);
        // Bytes past BIGINT clamp rather than wrap negative.
        set_logical(&db, OWNER_REPO, "huge", &acme, true, u64::MAX, 30).unwrap();
        assert_eq!(
            owner_bytes(&db, OWNER_REPO, "huge").unwrap(),
            Some((i64::MAX, None))
        );
    }

    #[test]
    fn a_day_folds_its_samples_and_a_period_sums_its_days() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("storage-days")).unwrap();
        let acme = org(&db, "acme");
        let mib = MIB as i64;
        sample_day(&db, &acme, "2026-09-01", OWNER_REPO, 4 * mib, 10 * mib).unwrap();
        sample_day(&db, &acme, "2026-09-01", OWNER_REPO, 8 * mib, 11 * mib).unwrap();
        sample_day(&db, &acme, "2026-09-02", OWNER_REPO, 2 * mib, 12 * mib).unwrap();
        sample_day(&db, &acme, "2026-09-03", OWNER_REPO, 0, 0).unwrap();
        sample_day(&db, &acme, "2026-08-31", OWNER_REPO, 100 * mib, 100 * mib).unwrap();

        let days = storage_days(&db, &acme, OWNER_REPO, 3).unwrap();
        assert_eq!(
            days,
            vec![
                StorageDay {
                    day: "2026-09-03".into(),
                    samples: 1,
                    private_bytes_avg: 0,
                    private_bytes_max: 0,
                    total_bytes_last: 0,
                },
                StorageDay {
                    day: "2026-09-02".into(),
                    samples: 1,
                    private_bytes_avg: 2 * mib,
                    private_bytes_max: 2 * mib,
                    total_bytes_last: 12 * mib,
                },
                StorageDay {
                    day: "2026-09-01".into(),
                    samples: 2,
                    private_bytes_avg: 6 * mib,
                    private_bytes_max: 8 * mib,
                    total_bytes_last: 11 * mib,
                },
            ]
        );
        assert_eq!(
            storage_days(&db, &acme, OWNER_REPO, 0).unwrap().len(),
            1,
            "limit clamps to one"
        );
        assert_eq!(
            period_mb_days(&db, &acme, "2026-09-01", "2026-09-30", OWNER_REPO).unwrap(),
            6 + 2,
            "the period is the sum of each day's average, in MiB"
        );
        assert_eq!(
            period_mb_days(&db, &acme, "2026-08-01", "2026-08-31", OWNER_REPO).unwrap(),
            100
        );
        assert_eq!(
            period_mb_days(&db, &acme, "2026-10-01", "2026-10-31", OWNER_REPO).unwrap(),
            0
        );
        // Sub-megabyte days round down rather than bill a megabyte.
        sample_day(&db, &acme, "2026-09-04", OWNER_REPO, mib - 1, 0).unwrap();
        assert_eq!(
            period_mb_days(&db, &acme, "2026-09-04", "2026-09-04", OWNER_REPO).unwrap(),
            0
        );
        assert_eq!(storage_days(&db, "nobody", OWNER_REPO, 10).unwrap(), vec![]);
    }

    #[test]
    fn the_allowance_and_the_room_are_arithmetic() {
        assert_eq!(included_bytes(3, 5), Some(15 * GIB));
        assert_eq!(included_bytes(0, 5), Some(0));
        assert_eq!(included_bytes(-2, 5), Some(0));
        assert_eq!(included_bytes(3, 0), None, "zero per seat is unlimited");
        assert_eq!(included_bytes(3, -1), None);
        assert_eq!(included_bytes(i64::MAX, 5), Some(u64::MAX), "saturates");
        assert_eq!(room(None, 1), None);
        assert_eq!(room(Some(10), 3), Some(7));
        assert_eq!(room(Some(10), 10), Some(0));
        assert_eq!(
            room(Some(10), 11),
            Some(0),
            "past the cap is no room, not negative room"
        );
    }
}
