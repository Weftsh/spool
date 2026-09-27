//! Per-repo, per-minute metrics (M6): counts, bytes served, and log-scale
//! latency histograms — enough to print clone p50/p99 and "requests
//! absorbed" on the renewal dashboard without a timeseries database.

use crate::db::ControlDb;
use serde::Serialize;
use std::collections::BTreeMap;

/// Log-2 millisecond buckets: 1,2,4,…,2^29. Index = floor(log2(ms))+1,
/// clamped; ms=0 → bucket 0.
fn bucket_of(ms: u64) -> u32 {
    if ms == 0 {
        0
    } else {
        (64 - ms.leading_zeros()).min(30)
    }
}

fn bucket_upper_ms(b: u32) -> u64 {
    if b == 0 {
        1
    } else {
        1u64 << b
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub repo_id: String,
    /// What the request was, and — through [`crate::usage::EGRESS_KINDS`]
    /// — whether its bytes bill. The vocabulary:
    ///
    /// * `clone` / `fetch` — a full or incremental fetch served inline;
    ///   `bytes` is what left the server.
    /// * `cdn_clone` — a clone whose bulk went to the CDN; `bytes` is
    ///   only the thin inline top-up.
    /// * `cdn_pack` — the bulk pack that clone was sent to fetch from
    ///   the edge, recorded when the client opts in with the pack's
    ///   stored size, since the edge never reports back.
    /// * `push` — a receive-pack; `bytes` is the pack the client sent.
    /// * `api` — an advert or `ls-refs`: a request absorbed, no bytes.
    /// * `freshness` — a mirror's synchronous origin sync.
    /// * `runner_clone` / `runner_fetch` / `runner_pack` — the same as
    ///   `clone` / `fetch` / `cdn_pack`, by a **hosted runner** fetching
    ///   the repository it is about to build. Counted for the repository's
    ///   metrics and Prometheus, never for the transfer meter: that
    ///   traffic is ours, and the minutes already pay for it.
    /// * `package` — a package download, when there is a registry.
    pub kind: &'static str,
    pub count: u64,
    pub bytes: u64,
    pub ms: Option<u64>,
}

/// Record one event into its minute row (read-modify-write under the
/// single-writer connection; histograms merge as JSON maps).
pub fn record(db: &ControlDb, ev: &Event) -> Result<(), String> {
    let minute = crate::ids::now_ms() / 60_000;
    let mut conn = db.lock();
    let existing: Option<String> = conn
        .query_opt(
            "SELECT histogram FROM metrics_minute WHERE repo_id=$1 AND minute=$2 AND kind=$3",
            &[&ev.repo_id, &minute, &ev.kind],
        )
        .map_err(|e| e.to_string())?
        .and_then(|r| r.get(0));
    let mut hist: BTreeMap<String, u64> = existing
        .as_deref()
        .and_then(|h| serde_json::from_str(h).ok())
        .unwrap_or_default();
    if let Some(ms) = ev.ms {
        *hist.entry(bucket_of(ms).to_string()).or_insert(0) += ev.count;
    }
    let hist_json = serde_json::to_string(&hist).map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO metrics_minute (repo_id, minute, kind, count, bytes, ms_sum, histogram) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (repo_id, minute, kind) DO UPDATE SET \
           count = metrics_minute.count + $4, bytes = metrics_minute.bytes + $5, \
           ms_sum = metrics_minute.ms_sum + $6, histogram = $7",
        &[
            &ev.repo_id,
            &minute,
            &ev.kind,
            &(ev.count as i64),
            &(ev.bytes as i64),
            &(ev.ms.unwrap_or(0) as i64 * ev.count as i64),
            &hist_json,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Debug, Serialize, Default, Clone)]
pub struct KindSummary {
    pub count: u64,
    pub bytes: u64,
    pub ms_sum: u64,
    pub p50_ms: Option<u64>,
    pub p99_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct RepoMetrics {
    pub repo_id: String,
    pub from_minute: i64,
    pub to_minute: i64,
    pub kinds: BTreeMap<String, KindSummary>,
}

/// Aggregate a repo's metrics over [from_ms, to_ms).
pub fn query_repo(
    db: &ControlDb,
    repo_id: &str,
    from_ms: i64,
    to_ms: i64,
) -> Result<RepoMetrics, String> {
    let (from_min, to_min) = (from_ms / 60_000, to_ms / 60_000);
    let rows = db
        .lock()
        .query(
            "SELECT kind, count, bytes, ms_sum, histogram FROM metrics_minute \
             WHERE repo_id = $1 AND minute >= $2 AND minute <= $3",
            &[&repo_id, &from_min, &to_min],
        )
        .map_err(|e| e.to_string())?;
    let mut kinds: BTreeMap<String, (KindSummary, BTreeMap<u32, u64>)> = BTreeMap::new();
    for r in rows {
        let (kind, count, bytes, ms_sum, hist) = (
            r.get::<_, String>(0),
            r.get::<_, i64>(1),
            r.get::<_, i64>(2),
            r.get::<_, i64>(3),
            r.get::<_, Option<String>>(4),
        );
        let slot = kinds.entry(kind).or_default();
        slot.0.count += count as u64;
        slot.0.bytes += bytes as u64;
        slot.0.ms_sum += ms_sum as u64;
        if let Some(h) = hist
            .as_deref()
            .and_then(|h| serde_json::from_str::<BTreeMap<String, u64>>(h).ok())
        {
            for (b, c) in h {
                if let Ok(bi) = b.parse::<u32>() {
                    *slot.1.entry(bi).or_insert(0) += c;
                }
            }
        }
    }
    let kinds = kinds
        .into_iter()
        .map(|(k, (mut summary, hist))| {
            summary.p50_ms = percentile(&hist, 0.50);
            summary.p99_ms = percentile(&hist, 0.99);
            (k, summary)
        })
        .collect();
    Ok(RepoMetrics {
        repo_id: repo_id.to_string(),
        from_minute: from_min,
        to_minute: to_min,
        kinds,
    })
}

fn percentile(hist: &BTreeMap<u32, u64>, q: f64) -> Option<u64> {
    let total: u64 = hist.values().sum();
    if total == 0 {
        return None;
    }
    let target = ((total as f64) * q).ceil() as u64;
    let mut seen = 0;
    for (&b, &c) in hist {
        seen += c;
        if seen >= target {
            return Some(bucket_upper_ms(b));
        }
    }
    hist.keys().last().map(|&b| bucket_upper_ms(b))
}

/// Org-day rollup for billing: repos with any activity, request count,
/// egress bytes over the UTC day containing `day_start_ms`.
pub struct DayUsage {
    pub active_repos: u64,
    pub requests: u64,
    pub bytes_out: u64,
}

pub fn org_day_usage(db: &ControlDb, org_id: &str, day_start_ms: i64) -> Result<DayUsage, String> {
    let from_min = day_start_ms / 60_000;
    let to_min = from_min + 24 * 60 - 1;
    // SUM(bigint) is numeric in Postgres — cast back to int8 for the driver.
    let row = db
        .lock()
        .query_one(
            "SELECT COUNT(DISTINCT m.repo_id), COALESCE(SUM(m.count),0)::int8, \
             COALESCE(SUM(m.bytes),0)::int8 \
             FROM metrics_minute m JOIN repos r ON r.id = m.repo_id \
             WHERE r.org_id = $1 AND m.minute BETWEEN $2 AND $3",
            &[&org_id, &from_min, &to_min],
        )
        .map(|r| DayUsage {
            active_repos: r.get::<_, i64>(0) as u64,
            requests: r.get::<_, i64>(1) as u64,
            bytes_out: r.get::<_, i64>(2) as u64,
        })
        .map_err(|e| e.to_string())?;
    Ok(row)
}

/// The part of an org's day that reaches the bill: hosted minutes,
/// bytes out of private repositories, and the day's average private
/// bytes stored. `bytes_out` on the row is everything served, public
/// included; these three are what the meters sum over a period.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DayBilled {
    pub hosted_minutes: i64,
    pub private_bytes_out: i64,
    pub private_bytes_stored: i64,
}

#[allow(clippy::too_many_arguments)]
pub fn upsert_usage(
    db: &ControlDb,
    org_id: &str,
    day: &str,
    active_repos: u64,
    total_repos: u64,
    requests: u64,
    bytes_out: u64,
    billed: DayBilled,
) -> Result<(), String> {
    db.lock()
        .execute(
            "INSERT INTO usage_daily (org_id, day, active_repos, total_repos, requests, bytes_out, \
                                      hosted_minutes, private_bytes_out, private_bytes_stored) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (org_id, day) DO UPDATE SET active_repos=$3, total_repos=$4, \
             requests=$5, bytes_out=$6, hosted_minutes=$7, private_bytes_out=$8, \
             private_bytes_stored=$9",
            &[
                &org_id,
                &day,
                &(active_repos as i64),
                &(total_repos as i64),
                &(requests as i64),
                &(bytes_out as i64),
                &billed.hosted_minutes,
                &billed.private_bytes_out,
                &billed.private_bytes_stored,
            ],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Hosted-runner minutes of the jobs that **started** in the UTC day
/// beginning at `day_start_ms`, rounded up per job the way the budget
/// rounds them ([`crate::workflows::budget_since`]). A job still
/// running counts up to `now`; a job that ran across midnight is the
/// day it started on, whole, so the days add up to the period. GitHub
/// Actions jobs on our runners are in it at their multiplier, the same
/// as the budget counts them — the bill and the gate must agree.
pub fn org_day_hosted_minutes(
    db: &ControlDb,
    org_id: &str,
    day_start_ms: i64,
    now: i64,
) -> Result<i64, String> {
    let day_end = day_start_ms + 86_400_000;
    db.lock()
        .query_one(
            "SELECT (SELECT COALESCE(SUM(CEIL( \
               GREATEST(COALESCE(j.completed_at, $2) - j.started_at, 0)::numeric / 60000.0)), 0)::BIGINT \
             FROM workflow_jobs j \
             WHERE j.org_id = $1 AND j.pool = 'hosted' \
               AND j.started_at IS NOT NULL AND j.started_at >= $3 AND j.started_at < $4) \
             + (SELECT COALESCE(SUM(g.multiplier * CEIL( \
               GREATEST(COALESCE(g.completed_at, $2) - g.started_at, 0)::numeric / 60000.0)), 0)::BIGINT \
             FROM github_jobs g \
             WHERE g.org_id = $1 \
               AND g.started_at IS NOT NULL AND g.started_at >= $3 AND g.started_at < $4)",
            &[&org_id, &now, &day_start_ms, &day_end],
        )
        .map(|r| r.get(0))
        .map_err(|e| format!("day hosted minutes: {e}"))
}

/// The day's average private **repository** bytes stored, from the
/// storage sweep's samples (`storage_daily`); zero for a day with no
/// sample yet.
///
/// `owner_kind` is not optional here and the omission was a real bug.
/// `storage_daily` holds a row per kind per day, so the scalar subquery
/// below returns *two* rows for an organization that stores packages as
/// well as repositories, and Postgres answers that with an error rather
/// than a number. The rollup propagates it, so `upsert_usage` and
/// `mark_usage_reported` never ran — for every organization on the
/// fleet, not just the one with packages — and a whole day's usage
/// simply was not recorded. It failed at exactly the moment the
/// registry started being used, and it failed quietly: the tick logs
/// one line and the next hour tries again.
///
/// Repositories and not the total, deliberately: this number is the
/// `storage` meter's, and package bytes are metered and charged on
/// their own line. Adding them here would put them on the invoice
/// twice.
pub fn org_day_private_bytes_stored(
    db: &ControlDb,
    org_id: &str,
    day: &str,
) -> Result<i64, String> {
    db.lock()
        .query_one(
            "SELECT COALESCE(( \
               SELECT CASE WHEN samples > 0 THEN private_bytes_sum / samples ELSE 0 END \
               FROM storage_daily \
               WHERE org_id = $1 AND day = $2 AND owner_kind = $3), 0)::BIGINT",
            &[&org_id, &day, &crate::storage::OWNER_REPO],
        )
        .map(|r| r.get(0))
        .map_err(|e| format!("day private bytes stored: {e}"))
}

pub fn mark_usage_reported(db: &ControlDb, org_id: &str, day: &str) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE usage_daily SET reported_at = $3 WHERE org_id = $1 AND day = $2",
            &[&org_id, &day, &crate::ids::now_ms()],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[derive(Debug, Serialize)]
pub struct UsageDay {
    pub day: String,
    pub active_repos: i64,
    pub total_repos: i64,
    pub requests: i64,
    pub bytes_out: i64,
    pub hosted_minutes: i64,
    pub private_bytes_out: i64,
    pub private_bytes_stored: i64,
    pub reported_at: Option<i64>,
}

pub fn usage_days(db: &ControlDb, org_id: &str, limit: usize) -> Result<Vec<UsageDay>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT day, active_repos, total_repos, requests, bytes_out, reported_at, \
                    hosted_minutes, private_bytes_out, private_bytes_stored \
             FROM usage_daily WHERE org_id = $1 ORDER BY day DESC LIMIT $2",
            &[&org_id, &(limit.clamp(1, 400) as i64)],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|r| UsageDay {
            day: r.get(0),
            active_repos: r.get(1),
            total_repos: r.get(2),
            requests: r.get(3),
            bytes_out: r.get(4),
            hosted_minutes: r.get(6),
            private_bytes_out: r.get(7),
            private_bytes_stored: r.get(8),
            reported_at: r.get(5),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry;

    #[test]
    fn bucket_edges_and_percentile_tail() {
        // 0ms lands in bucket 0; bucket 0's upper bound is 1ms.
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_upper_ms(0), 1);
        assert!(bucket_upper_ms(bucket_of(1500)) >= 1500);
        // Empty histogram → no percentile; a percentile past every
        // cumulative count falls back to the last bucket.
        let empty: BTreeMap<u32, u64> = BTreeMap::new();
        assert_eq!(percentile(&empty, 0.5), None);
        let mut h = BTreeMap::new();
        h.insert(3u32, 0u64); // zero-count bucket exercises the fall-through
        assert_eq!(percentile(&h, 0.99), None);
        h.insert(2u32, 4u64);
        assert_eq!(percentile(&h, 0.5), Some(bucket_upper_ms(2)));
    }

    #[test]
    fn record_query_and_percentiles() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-metrics")).unwrap();
        let org = registry::create_org(&db, "o").unwrap();
        let repo = registry::create_repo(
            &db,
            &org.id,
            &registry::NewRepo {
                description: None,
                name: "r",
                kind: registry::RepoKind::Native,
                public: false,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        for ms in [10, 12, 14, 900] {
            record(
                &db,
                &Event {
                    repo_id: repo.id.clone(),
                    kind: "clone",
                    count: 1,
                    bytes: 1000,
                    ms: Some(ms),
                },
            )
            .unwrap();
        }
        let now = crate::ids::now_ms();
        let m = query_repo(&db, &repo.id, now - 60_000, now + 60_000).unwrap();
        let clone = &m.kinds["clone"];
        assert_eq!(clone.count, 4);
        assert_eq!(clone.bytes, 4000);
        // p50 lands in the 8-16ms bucket, p99 in the 512-1024ms bucket.
        assert_eq!(clone.p50_ms, Some(16));
        assert_eq!(clone.p99_ms, Some(1024));

        let usage = org_day_usage(&db, &org.id, now - now % 86_400_000).unwrap();
        assert_eq!(usage.active_repos, 1);
        assert_eq!(usage.requests, 4);
        assert_eq!(usage.bytes_out, 4000);
    }

    /// The billed part of a day: hosted minutes are the jobs that
    /// started that day (rounded up, a running one counted to `now`),
    /// stored bytes are the day's sample average, and both survive the
    /// upsert into `usage_daily` and come back off it.
    #[test]
    fn the_billed_columns_fold_and_read_back() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-billed")).unwrap();
        let org = registry::create_org(&db, "o").unwrap();
        let repo = registry::create_repo(
            &db,
            &org.id,
            &registry::NewRepo {
                description: None,
                name: "r",
                kind: registry::RepoKind::Native,
                public: false,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        let day_start = 1_800_000_000_000 - 1_800_000_000_000 % 86_400_000;
        let now = day_start + 10 * 3_600_000;
        {
            let mut c = db.lock();
            c.execute(
                "INSERT INTO workflow_runs (id, org_id, repo_id, file, name, commit_sha, event, \
                 state, created_at, updated_at) VALUES ('run1', $1, $2, 'ci', 'ci', 'abc', 'push', \
                 'passed', $3, $3)",
                &[&org.id, &repo.id, &day_start],
            )
            .unwrap();
            // 90 s → 2 minutes; still running for 30 s → 1 minute; one
            // started yesterday → not this day's; a self-hosted one → never.
            for (id, pool, started, done) in [
                (
                    "j1",
                    "hosted",
                    day_start + 60_000,
                    Some(day_start + 150_000),
                ),
                ("j2", "hosted", now - 30_000, None),
                ("j3", "hosted", day_start - 1, Some(day_start + 5_000)),
                (
                    "j4",
                    "self_hosted",
                    day_start + 60_000,
                    Some(day_start + 900_000),
                ),
            ] {
                c.execute(
                    "INSERT INTO workflow_jobs (id, run_id, org_id, repo_id, job_id, key, state, \
                     pool, created_at, updated_at, started_at, completed_at) \
                     VALUES ($1, 'run1', $2, $3, 'test', $1, 'passed', $4, $5, $5, $5, $6)",
                    &[&id, &org.id, &repo.id, &pool, &started, &done],
                )
                .unwrap();
            }
        }
        assert_eq!(
            org_day_hosted_minutes(&db, &org.id, day_start, now).unwrap(),
            3
        );
        assert_eq!(
            org_day_hosted_minutes(&db, &org.id, day_start - 86_400_000, now).unwrap(),
            1,
            "yesterday's job is yesterday's minute"
        );
        assert_eq!(
            org_day_hosted_minutes(&db, "ghost", day_start, now).unwrap(),
            0
        );

        let day = "2027-01-15";
        assert_eq!(org_day_private_bytes_stored(&db, &org.id, day).unwrap(), 0);
        crate::storage::sample_day(&db, &org.id, day, crate::storage::OWNER_REPO, 4_000, 9_000)
            .unwrap();
        crate::storage::sample_day(&db, &org.id, day, crate::storage::OWNER_REPO, 8_000, 9_000)
            .unwrap();
        assert_eq!(
            org_day_private_bytes_stored(&db, &org.id, day).unwrap(),
            6_000,
            "the day's average, not its last sample"
        );

        // A second owner kind on the same day. Without the `owner_kind`
        // filter this is not a wrong number — the scalar subquery
        // returns two rows and Postgres refuses outright, which took
        // the whole billing rollup down with it for every organization
        // on the fleet. And the number stays the repository one:
        // package bytes are charged on their own meter, so counting
        // them here would put them on the invoice twice.
        crate::storage::sample_day(
            &db,
            &org.id,
            day,
            crate::storage::OWNER_PACKAGE,
            50_000,
            50_000,
        )
        .unwrap();
        assert_eq!(
            org_day_private_bytes_stored(&db, &org.id, day).unwrap(),
            6_000,
            "package bytes leaked into the repository storage meter"
        );

        let billed = DayBilled {
            hosted_minutes: 3,
            private_bytes_out: 4_000,
            private_bytes_stored: 6_000,
        };
        upsert_usage(&db, &org.id, day, 1, 1, 4, 4_000, billed).unwrap();
        upsert_usage(&db, &org.id, day, 1, 1, 5, 5_000, billed).unwrap();
        let days = usage_days(&db, &org.id, 10).unwrap();
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].requests, 5, "the upsert replaces, it does not add");
        assert_eq!(days[0].hosted_minutes, 3);
        assert_eq!(days[0].private_bytes_out, 4_000);
        assert_eq!(days[0].private_bytes_stored, 6_000);
        assert_eq!(days[0].reported_at, None);
        let json = serde_json::to_value(&days[0]).unwrap();
        assert_eq!(json["private_bytes_stored"], 6_000, "{json}");
    }
}
