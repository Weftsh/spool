//! Audit shipper: batches each org's new audit rows to object storage as
//! write-once JSONL (`o/<org>/audit/<first>-<last>.jsonl`) — the
//! immutability story behind R7 (Postgres is the queryable index; S3 is
//! the durable record).

use crate::app::SharedState;
use std::time::Duration;
use stratum_control::audit::{self, AuditQuery};
use stratum_control::jobs;
use stratum_control::webhooks::meta_get;
use stratum_control::ControlDb;
use stratum_store::{LatencyModel, ObjectStore, PutCond};

/// Name of the fleet-wide lock this worker runs under. See
/// [`jobs::try_lock`] for why the periodic workers take a lock rather
/// than manufacturing a job row per tick.
const WORKER: &str = "audit-shipper";

/// Rows per shard. Also the size of the window two racing nodes would
/// disagree about, which is what made the unleased version produce
/// overlapping shards.
const BATCH: usize = 1000;

pub fn spawn(state: SharedState) {
    let every = super::env_secs("STRATUM_AUDIT_SHIP_SECS", 3600);
    if every == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(every));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let db = state.db.clone();
            let store_url = state.store_url.clone();
            let out = tokio::task::spawn_blocking(move || ship_all(&db, &store_url)).await;
            if let Ok(Err(e)) = out {
                eprintln!("weft: audit shipper: {e}");
            }
        }
    });
}

/// One shipping pass over every org — at most one in the fleet at a time.
///
/// Every node used to run this on every tick, which is not merely N times
/// the work: two nodes reading one cursor moments apart see different row
/// counts, so they write two *different* keys covering overlapping
/// ranges. `If-None-Match: *` never fires (the keys differ), and both
/// then write the cursor, so the slower one rewinds it and the next pass
/// ships the same entries a third time. That is duplicated and
/// overlapping content in a record sold as immutable.
///
/// Losing the lock is not an error and not a missed tick — another node
/// is doing it, which is the point.
pub fn ship_all(db: &ControlDb, store_url: &str) -> Result<(), String> {
    let Some(_lock) = jobs::try_lock(db, WORKER)? else {
        return Ok(());
    };
    let store = ObjectStore::new(store_url, LatencyModel::None);
    for org in stratum_control::registry::all_org_ids(db)? {
        ship_org(db, &store, &org)?;
    }
    Ok(())
}

/// Ship one org's next batch. Split out so the shipping rules can be
/// driven directly by tests without a poll loop or a lock in the way.
fn ship_org(db: &ControlDb, store: &ObjectStore, org: &str) -> Result<(), String> {
    let cursor_key = cursor_key(org);
    let after: i64 = meta_get(db, &cursor_key)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let entries = audit::query(
        db,
        org,
        &AuditQuery {
            after_seq: Some(after),
            limit: BATCH,
            ..AuditQuery::default()
        },
    )?;
    if entries.is_empty() {
        return Ok(());
    }
    let first = entries.first().unwrap().seq;
    let last = entries.last().unwrap().seq;
    let mut body = String::new();
    for e in &entries {
        body.push_str(&serde_json::to_string(e).map_err(|x| x.to_string())?);
        body.push('\n');
    }
    // Write-once: a replay of the same batch key is create-only, so a
    // crashed shipper can never rewrite history.
    let key = shard_key(org, first, last);
    match store.put(&key, body.as_bytes(), PutCond::IfNoneMatchStar) {
        Ok(()) | Err(stratum_store::PutError::Conflict) => {}
        Err(e) => return Err(e.to_string()),
    }
    // Monotonic, never `meta_set`. The lock above already makes two
    // concurrent passes rare, but it is session state and a mid-pass
    // reconnect drops it — and a cursor that can move backwards turns
    // that survivable duplicate into re-shipped history. Belt and
    // braces, because the record is the product.
    jobs::advance_cursor(db, &cursor_key, last)?;
    Ok(())
}

fn cursor_key(org: &str) -> String {
    format!("audit_cursor:{org}")
}

fn shard_key(org: &str, first: i64, last: i64) -> String {
    format!("o/{org}/audit/{first:012}-{last:012}.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use stratum_control::audit::AuditCtx;

    /// Parse the seq range back out of every shard this org has, sorted.
    fn shards(store: &ObjectStore, org: &str) -> Vec<(i64, i64)> {
        let mut out: Vec<(i64, i64)> = store
            .list(&format!("o/{org}/audit/"))
            .unwrap()
            .into_iter()
            .map(|(k, _)| {
                let name = k.rsplit('/').next().unwrap().trim_end_matches(".jsonl");
                let (a, b) = name.split_once('-').expect("shard key is first-last");
                (a.parse::<i64>().unwrap(), b.parse::<i64>().unwrap())
            })
            .collect();
        out.sort_unstable();
        out
    }

    /// The lock is not decoration: while another node holds it, a pass
    /// must ship nothing and leave the cursor exactly where it was.
    #[test]
    fn a_node_that_loses_the_lock_ships_nothing() {
        let minio = stratum_testkit::Minio::shared();
        let bucket = minio.bucket("shipper-locked-out");
        let url = stratum_testkit::pg::test_db_url("shipper-locked-out");
        let a = ControlDb::open(&url).unwrap();
        let b = ControlDb::open(&url).unwrap();
        let org = stratum_control::registry::create_org(&a, "acme").unwrap();
        let ctx = AuditCtx::system(&org.id, "test");
        for _ in 0..10 {
            audit::record(&a, &ctx, None, "test.event", None).unwrap();
        }

        let held = jobs::try_lock(&b, WORKER).unwrap().expect("b is sweeping");
        ship_all(&a, &bucket.base_url).unwrap();
        let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
        assert!(
            store
                .list(&format!("o/{}/audit/", org.id))
                .unwrap()
                .is_empty(),
            "a locked-out node shipped anyway"
        );
        assert!(meta_get(&a, &cursor_key(&org.id)).unwrap().is_none());

        // And it is a pass, not a failure: once the lock is free the
        // same call ships normally.
        drop(held);
        ship_all(&a, &bucket.base_url).unwrap();
        assert_eq!(
            store.list(&format!("o/{}/audit/", org.id)).unwrap().len(),
            1
        );
    }

    /// The fleet bug, reproduced: two nodes (two sessions against one
    /// database and one bucket) shipping one org while entries keep
    /// arriving — which is simply "two nodes, in production".
    ///
    /// Unleased, both read the same cursor and then query a table that
    /// has grown between them, so they write two *different* keys over
    /// overlapping ranges. `If-None-Match: *` cannot catch that: the keys
    /// differ. Both then write the cursor, and the slower one rewinds it,
    /// so the next pass ships the overlap a third time.
    ///
    /// What is asserted is the record itself, not the mechanism: shards
    /// are disjoint, they cover the org's seqs exactly once in order, and
    /// the cursor never decreases.
    #[test]
    fn two_nodes_shipping_one_org_never_overlap_or_rewind_the_cursor() {
        let minio = stratum_testkit::Minio::shared();
        let bucket = minio.bucket("shipper-race");
        let url = stratum_testkit::pg::test_db_url("shipper-race");
        // Three handles = three sessions. The lock is session state, so
        // this is two real nodes, not a simulation of them.
        let a = ControlDb::open(&url).unwrap();
        let b = ControlDb::open(&url).unwrap();
        let writer = ControlDb::open(&url).unwrap();

        let org = stratum_control::registry::create_org(&a, "acme").unwrap();
        let ctx = AuditCtx::system(&org.id, "test");
        let barrier = Arc::new(Barrier::new(3));
        let mut cursor_high_water = 0i64;

        for round in 0..6 {
            std::thread::scope(|s| {
                // Entries keep arriving *during* the pass. This is what
                // makes the two nodes disagree about where the batch
                // ends — with a static table they would compute the same
                // key and the create-only PUT would hide the bug.
                {
                    let barrier = Arc::clone(&barrier);
                    let writer = &writer;
                    let ctx = &ctx;
                    s.spawn(move || {
                        barrier.wait();
                        for _ in 0..600 {
                            audit::record(writer, ctx, None, "test.event", None).unwrap();
                        }
                    });
                }
                for db in [&a, &b] {
                    let barrier = Arc::clone(&barrier);
                    let store_url = bucket.base_url.clone();
                    s.spawn(move || {
                        barrier.wait();
                        ship_all(db, &store_url).unwrap();
                    });
                }
            });
            // The cursor moved, in one direction only, every round.
            let now: i64 = meta_get(&a, &cursor_key(&org.id))
                .unwrap()
                .unwrap_or_else(|| "0".into())
                .parse()
                .unwrap();
            assert!(
                now >= cursor_high_water,
                "cursor rewound from {cursor_high_water} to {now} in round {round}"
            );
            cursor_high_water = now;
        }

        // Drain what is left so the shards must cover everything.
        let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
        for _ in 0..12 {
            ship_all(&a, &bucket.base_url).unwrap();
        }

        // Paged, because `audit::query` clamps a page to BATCH — the
        // same ceiling the shipper works under.
        let mut expected: Vec<i64> = Vec::new();
        loop {
            let page = audit::query(
                &a,
                &org.id,
                &AuditQuery {
                    after_seq: expected.last().copied(),
                    limit: BATCH,
                    ..AuditQuery::default()
                },
            )
            .unwrap();
            if page.is_empty() {
                break;
            }
            expected.extend(page.iter().map(|e| e.seq));
        }
        assert!(expected.len() > BATCH, "the test must span several shards");

        let shards = shards(&store, &org.id);
        assert!(shards.len() > 1, "{shards:?}");
        // Disjoint and ordered: no shard may start inside its predecessor.
        for pair in shards.windows(2) {
            let (_, prev_last) = pair[0];
            let (next_first, next_last) = pair[1];
            assert!(
                next_first > prev_last,
                "overlapping shards {:?} and {:?} in {shards:?}",
                pair[0],
                pair[1]
            );
            assert!(next_last >= next_first, "{shards:?}");
        }
        // And contiguous over the entries themselves: every seq the org
        // recorded appears in exactly one shard, in order.
        let mut covered: Vec<i64> = Vec::new();
        for (f, l) in &shards {
            let body = String::from_utf8(store.get(&shard_key(&org.id, *f, *l)).unwrap()).unwrap();
            for line in body.lines() {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                covered.push(v["seq"].as_i64().unwrap());
            }
        }
        assert_eq!(
            covered, expected,
            "the shipped record must be each entry exactly once, in order"
        );
    }
}
