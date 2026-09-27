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
    /// What the request was. The vocabulary:
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
    ///   `clone` / `fetch` / `cdn_pack`, by a **runner** fetching the
    ///   repository it is about to build, so a build is not counted as a
    ///   person cloning.
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

/// Org-day rollup for the usage page: repos with any activity, request
/// count, egress bytes over the UTC day containing `day_start_ms`.
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

/// Fold one org's day into `usage_daily`. An upsert that **replaces**,
/// so the day can be recomputed on every tick that still covers it.
pub fn upsert_usage(
    db: &ControlDb,
    org_id: &str,
    day: &str,
    active_repos: u64,
    total_repos: u64,
    requests: u64,
    bytes_out: u64,
) -> Result<(), String> {
    db.lock()
        .execute(
            "INSERT INTO usage_daily (org_id, day, active_repos, total_repos, requests, bytes_out) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (org_id, day) DO UPDATE SET active_repos=$3, total_repos=$4, \
             requests=$5, bytes_out=$6",
            &[
                &org_id,
                &day,
                &(active_repos as i64),
                &(total_repos as i64),
                &(requests as i64),
                &(bytes_out as i64),
            ],
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
}

pub fn usage_days(db: &ControlDb, org_id: &str, limit: usize) -> Result<Vec<UsageDay>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT day, active_repos, total_repos, requests, bytes_out \
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

    /// A day folds into `usage_daily` and comes back off it, and a
    /// second fold of the same day replaces the first rather than adding
    /// to it — the rollup re-folds today on every tick.
    #[test]
    fn a_day_folds_and_reads_back_and_a_refold_replaces_it() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-usage")).unwrap();
        let org = registry::create_org(&db, "o").unwrap();
        let day = "2027-01-15";
        upsert_usage(&db, &org.id, day, 1, 1, 4, 4_000).unwrap();
        upsert_usage(&db, &org.id, day, 1, 2, 5, 5_000).unwrap();
        let days = usage_days(&db, &org.id, 10).unwrap();
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].requests, 5, "the upsert replaces, it does not add");
        assert_eq!(days[0].bytes_out, 5_000);
        assert_eq!(days[0].total_repos, 2);
        assert!(usage_days(&db, "ghost", 10).unwrap().is_empty());
    }
}
