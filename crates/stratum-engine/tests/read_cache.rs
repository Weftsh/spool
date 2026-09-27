//! What a read costs in store round trips, and what the shared cache
//! saves. Every assertion here is an exact GET count, because "faster"
//! is not a property a test can hold and "one GET per WAL entry, then
//! none" is.
//!
//! The production number behind this file: a mirror whose WAL had
//! grown to 102 entries answered a three-byte file in 10.8 s, because
//! the reader downloaded every pack — two GETs each, sequentially —
//! before consulting the plane the blob actually lived in.

use std::sync::Arc;
use std::time::{Duration, Instant};
use stratum_engine::ingest::{publish, PublishMode};
use stratum_engine::objwrite::{hash_object, hex, parse_hex};
use stratum_engine::read::{is_read_budget_exhausted, LayoutReader, ReadCache};
use stratum_engine::refops::{transact, Expect, NewPack, RefUpdate};
use stratum_engine::{build_locator, ingest, IngestConfig};
use stratum_store::pack::OBJ_BLOB;
use stratum_store::{LatencyModel, ObjectStore};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::Minio;

fn deflate(data: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
    z.write_all(data).unwrap();
    z.finish().unwrap()
}

fn entry_header(typ: u8, mut size: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut b = (typ << 4) | (size & 0x0f) as u8;
    size >>= 4;
    while size > 0 {
        out.push(b | 0x80);
        b = (size & 0x7f) as u8;
        size >>= 7;
    }
    out.push(b);
    out
}

/// Land one full (non-delta) blob as its own WAL push on `branch`.
fn land_blob(store: &ObjectStore, prefix: &str, branch: &str, content: &[u8]) -> String {
    let oid = hex(&hash_object(OBJ_BLOB, content));
    let mut payload = entry_header(OBJ_BLOB, content.len());
    payload.extend(deflate(content));
    transact(
        store,
        prefix,
        &[RefUpdate {
            name: format!("refs/heads/{branch}"),
            expect: Expect::Any,
            new: Some(oid.clone()),
        }],
        Some(&NewPack {
            payload,
            oids: vec![parse_hex(&oid).unwrap()],
            entries: 1,
        }),
    )
    .unwrap_or_else(|_| panic!("landing {branch}"));
    oid
}

struct Layout {
    store: ObjectStore,
    prefix: &'static str,
    org_prefix: &'static str,
    /// A blob the plane serves.
    plane_oid: String,
    plane_bytes: Vec<u8>,
}

fn layout(name: &str, prefix: &'static str, org_prefix: &'static str) -> Layout {
    let minio = Minio::shared();
    let bucket = minio.bucket(name);
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let scratch = Scratch::new(name);
    let repo = scratch.path().join("fixture");
    gitcli::fixture_repo(&repo, 3);
    let mut out = ingest(
        &repo,
        prefix,
        "main",
        &IngestConfig::default(),
        &scratch.path().join("s"),
    )
    .unwrap();
    let hdr = build_locator(&repo, &mut out, prefix, 0).unwrap();
    publish(&store, prefix, &out, &hdr, PublishMode::Create).unwrap();
    let manifest = stratum_store::load_manifest(&store, org_prefix, "prod").unwrap();
    let reader = LayoutReader::new(&store, prefix, &manifest).unwrap();
    let tip = manifest.tip().unwrap().to_string();
    let readme = reader.entry_at(&tip, "README.md").unwrap().unwrap();
    let plane_oid = hex(&readme.oid);
    let (_, plane_bytes) = reader.object(&plane_oid).unwrap();
    Layout {
        store,
        prefix,
        org_prefix,
        plane_oid,
        plane_bytes,
    }
}

/// GETs one fresh reader spends opening the layout and reading `oid`.
fn cost(l: &Layout, cache: Option<Arc<ReadCache>>, oid: &str) -> (u64, Vec<u8>) {
    let manifest = stratum_store::load_manifest(&l.store, l.org_prefix, "prod").unwrap();
    let before = l.store.request_count();
    let reader = match cache {
        Some(c) => LayoutReader::with_cache(&l.store, l.prefix, &manifest, c).unwrap(),
        None => LayoutReader::new(&l.store, l.prefix, &manifest).unwrap(),
    };
    let (_, bytes) = reader.object(oid).unwrap();
    (l.store.request_count() - before, bytes)
}

/// A plane object behind N WAL entries costs N sidecar GETs over what it
/// cost with no WAL at all — not 2N. The packs are never fetched, because
/// their sidecars say the object is not in them.
#[test]
fn a_plane_read_pays_one_sidecar_per_wal_entry_and_never_a_pack() {
    let l = layout("engine-readcost", "o/t/r/readcost/prod", "o/t/r/readcost");
    let (bare, bytes) = cost(&l, None, &l.plane_oid);
    assert_eq!(bytes, l.plane_bytes);

    land_blob(&l.store, l.prefix, "wal-a", b"first push\n");
    land_blob(&l.store, l.prefix, "wal-b", b"second push\n");
    land_blob(&l.store, l.prefix, "wal-c", b"third push\n");
    let manifest = stratum_store::load_manifest(&l.store, l.org_prefix, "prod").unwrap();
    assert_eq!(manifest.wal.len(), 3);

    let (with_wal, bytes) = cost(&l, None, &l.plane_oid);
    assert_eq!(bytes, l.plane_bytes);
    assert_eq!(
        with_wal - bare,
        3,
        "three WAL entries cost three sidecar reads; a pack download would make it six"
    );
}

/// The second reader over a shared cache spends exactly one GET — the
/// locator header the layout is opened with — whatever it reads: the
/// sidecars, the pack and the object itself are all remembered.
#[test]
fn a_shared_cache_makes_the_second_read_free_of_object_traffic() {
    let l = layout(
        "engine-readcache",
        "o/t/r/readcache/prod",
        "o/t/r/readcache",
    );
    let wal_oid = land_blob(&l.store, l.prefix, "wal-a", b"in the wal\n");
    let cache = Arc::new(ReadCache::new(64 << 20));

    // Cold: sidecar, pack, and — for the plane blob — locator + segment.
    let (cold_wal, got) = cost(&l, Some(cache.clone()), &wal_oid);
    assert_eq!(got, b"in the wal\n");
    assert!(
        cold_wal > 1,
        "a cold WAL read reaches the store: {cold_wal}"
    );
    let (cold_plane, got) = cost(&l, Some(cache.clone()), &l.plane_oid);
    assert_eq!(got, l.plane_bytes);
    assert!(
        cold_plane > 1,
        "a cold plane read reaches the store: {cold_plane}"
    );

    // Warm: the layout header and nothing else.
    let (warm_wal, got) = cost(&l, Some(cache.clone()), &wal_oid);
    assert_eq!(got, b"in the wal\n");
    assert_eq!(
        warm_wal, 1,
        "a cached WAL object costs only the plane header"
    );
    let (warm_plane, got) = cost(&l, Some(cache.clone()), &l.plane_oid);
    assert_eq!(got, l.plane_bytes);
    assert_eq!(
        warm_plane, 1,
        "a cached plane object costs only the plane header"
    );

    // And a reader with no cache is exactly as expensive as it ever was:
    // the cache is an argument, not an ambient. One more than the cold
    // cached read, because that one already had the sidecar from the WAL
    // read before it.
    let (uncached, _) = cost(&l, None, &l.plane_oid);
    assert_eq!(uncached, cold_plane + 1);

    assert!(cache.bytes() > 0);
}

/// A reader past its deadline refuses to go to the store, and says so in
/// the one phrase a history walk turns into "truncated". What the cache
/// already holds still answers — a walk on a clock is only useful if the
/// objects it has already paid for stay readable.
#[test]
fn a_deadline_refuses_store_reads_but_not_cache_hits() {
    let l = layout("engine-readdeadline", "o/t/r/readdl/prod", "o/t/r/readdl");
    let wal_oid = land_blob(&l.store, l.prefix, "wal-a", b"wal object\n");
    let cache = Arc::new(ReadCache::new(64 << 20));
    let manifest = stratum_store::load_manifest(&l.store, l.org_prefix, "prod").unwrap();

    // Warm the cache with the plane blob only.
    let reader = LayoutReader::with_cache(&l.store, l.prefix, &manifest, cache.clone()).unwrap();
    reader.object(&l.plane_oid).unwrap();

    let reader = LayoutReader::with_cache(&l.store, l.prefix, &manifest, cache.clone()).unwrap();
    reader.set_deadline(Some(Instant::now() - Duration::from_millis(1)));
    // Cached: answers.
    assert_eq!(reader.object(&l.plane_oid).unwrap().1, l.plane_bytes);
    // Not cached, and the sidecar has to come from the store: refused.
    let err = reader.object(&wal_oid).unwrap_err();
    assert!(is_read_budget_exhausted(&err), "{err}");
    // Lifting the deadline lets the same reader finish the read.
    reader.set_deadline(None);
    assert_eq!(reader.object(&wal_oid).unwrap().1, b"wal object\n");

    // A reader with no cache and a spent deadline refuses even a plane
    // object: the locator lookup is a store read too.
    let reader = LayoutReader::new(&l.store, l.prefix, &manifest).unwrap();
    reader.set_deadline(Some(Instant::now() - Duration::from_millis(1)));
    let err = reader.object(&l.plane_oid).unwrap_err();
    assert!(is_read_budget_exhausted(&err), "{err}");
    // And one whose deadline is still ahead reads normally.
    reader.set_deadline(Some(Instant::now() + Duration::from_secs(60)));
    assert_eq!(reader.object(&l.plane_oid).unwrap().1, l.plane_bytes);
}

/// An oid is looked up in the form the cache stores it. Upper-case hex
/// is legal input to `object` and must hit the same entry.
#[test]
fn a_cache_hit_does_not_depend_on_the_case_the_oid_was_spelled_in() {
    let l = layout("engine-readcase", "o/t/r/readcase/prod", "o/t/r/readcase");
    let cache = Arc::new(ReadCache::new(64 << 20));
    let (_, _) = cost(&l, Some(cache.clone()), &l.plane_oid);
    let (warm, got) = cost(&l, Some(cache.clone()), &l.plane_oid.to_uppercase());
    assert_eq!(got, l.plane_bytes);
    assert_eq!(warm, 1);
}
