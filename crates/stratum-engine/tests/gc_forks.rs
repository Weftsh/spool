//! Epoch GC against locator pointers that name data somewhere else.
//!
//! A zero-copy fork's `locator.hdr` is `SLH4`: it carries an absolute
//! data prefix, and for a fork that prefix is another repository's. GC
//! decides what to delete from the *live epoch set*, so a pointer whose
//! epoch is not ours to keep and not ours to sweep is exactly the input
//! that gets this wrong in a way nobody notices until data is gone.
//!
//! These are the deterministic siblings the chaos suite is not allowed
//! to stand in for (CLAUDE.md: a chaos test may never be the sole cover
//! for a product line).

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use stratum_engine::gc::{self, EpochRefs, NoEpochRefs};
use stratum_store::plane::{write_header, LocatorHeader};
use stratum_store::{LatencyModel, ObjectStore, PutCond};
use stratum_testkit::Minio;

/// The smallest manifest `live_epochs` will parse. It references no
/// keys, so the epoch field is the only thing keeping anything alive —
/// which is what makes the pointer's contribution observable.
fn manifest_at(epoch: &str) -> String {
    format!(
        r#"{{"schema":3,"repo":"r","layout":"prod","refs":[],"head":"refs/heads/main","epoch":"{epoch}"}}"#
    )
}

fn hdr(epoch: &str, generation: Option<u32>, data_prefix: Option<&str>) -> Vec<u8> {
    write_header(&LocatorHeader {
        epoch: epoch.to_string(),
        generation,
        data_prefix: data_prefix.map(str::to_string),
        records: 0,
        n_cold: 0,
        buckets: vec![0; 4097],
    })
}

fn put(store: &ObjectStore, key: &str, body: &[u8]) {
    store.put(key, body, PutCond::None).unwrap();
}

fn epochs_present(store: &ObjectStore, prefix: &str) -> Vec<String> {
    let mut seen: Vec<String> = store
        .list(&format!("{prefix}/"))
        .unwrap()
        .into_iter()
        .filter_map(|(k, _)| {
            let rest = k.strip_prefix(&format!("{prefix}/"))?.to_string();
            let (epoch, _) = rest.split_once('/')?;
            Some(epoch.to_string())
        })
        .collect();
    seen.sort();
    seen.dedup();
    seen
}

/// Far enough in the future that every object is past any grace window,
/// so liveness is the only thing deciding what survives.
const LATER: u64 = 4_000_000_000;

#[test]
fn an_slh4_pointer_does_not_keep_a_local_epoch_of_the_same_name_alive() {
    // The fork's header names upstream's epoch, because that is the
    // epoch its data is in. If GC folds that name into *this* layout's
    // live set, then a local epoch that happens to share the name is
    // pinned forever — and, far worse, the same confusion in the other
    // direction is how you conclude that somebody else's live epoch is
    // yours to delete.
    let minio = Minio::shared();
    let bucket = minio.bucket("gcfork-alias");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let prefix = "o/t/r/gcfork-alias/prod";

    put(
        &store,
        &format!("{prefix}/manifest.json"),
        manifest_at("e-fork").as_bytes(),
    );
    put(
        &store,
        &format!("{prefix}/e-fork/cold-0000.seg"),
        b"fork data",
    );
    // A local epoch directory whose name collides with upstream's.
    put(
        &store,
        &format!("{prefix}/e-up/cold-0000.seg"),
        b"stale local data",
    );
    put(
        &store,
        &format!("{prefix}/locator.hdr"),
        &hdr("e-up", Some(1), Some("o/t/r/gcfork-upstream/prod/e-up")),
    );

    let report = gc::gc_epochs_with_refs(&store, prefix, 60, LATER, &NoEpochRefs).unwrap();
    assert_eq!(report.epochs_deleted, 1, "{report:?}");
    assert_eq!(
        epochs_present(&store, prefix),
        vec!["e-fork".to_string()],
        "the stale local epoch was kept alive by a pointer that names upstream's"
    );
}

#[test]
fn a_lagging_slh3_pointer_still_pins_its_own_epoch() {
    // The reason the pointer is consulted at all (I15): `locator.hdr`
    // may lag `manifest.json`, and a reader that loaded the older
    // pointer is still streaming from it. This is the regression guard
    // on the SLH4 work — the fork case must not cost the ordinary case.
    let minio = Minio::shared();
    let bucket = minio.bucket("gcfork-lag");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let prefix = "o/t/r/gcfork-lag/prod";

    put(
        &store,
        &format!("{prefix}/manifest.json"),
        manifest_at("e2").as_bytes(),
    );
    put(
        &store,
        &format!("{prefix}/e1/cold-0000.seg"),
        b"older epoch",
    );
    put(
        &store,
        &format!("{prefix}/e2/cold-0000.seg"),
        b"newer epoch",
    );
    // The pointer still names e1.
    put(
        &store,
        &format!("{prefix}/locator.hdr"),
        &hdr("e1", Some(0), None),
    );

    let report = gc::gc_epochs_with_refs(&store, prefix, 60, LATER, &NoEpochRefs).unwrap();
    assert_eq!(report.epochs_deleted, 0, "{report:?}");
    assert_eq!(
        epochs_present(&store, prefix),
        vec!["e1".to_string(), "e2".to_string()]
    );
}

#[test]
fn an_slh4_pointer_at_our_own_prefix_still_pins_its_epoch() {
    // SLH4 does not always mean "somebody else's data". A header written
    // with an absolute prefix that happens to be ours must behave
    // exactly like SLH3, or compaction under the new format would start
    // dropping the epoch a live pointer is still serving.
    let minio = Minio::shared();
    let bucket = minio.bucket("gcfork-selfabs");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let prefix = "o/t/r/gcfork-selfabs/prod";

    put(
        &store,
        &format!("{prefix}/manifest.json"),
        manifest_at("e2").as_bytes(),
    );
    put(
        &store,
        &format!("{prefix}/e1/cold-0000.seg"),
        b"older epoch",
    );
    put(
        &store,
        &format!("{prefix}/e2/cold-0000.seg"),
        b"newer epoch",
    );
    put(
        &store,
        &format!("{prefix}/locator.hdr"),
        &hdr("e1", Some(0), Some(&format!("{prefix}/e1"))),
    );

    let report = gc::gc_epochs_with_refs(&store, prefix, 60, LATER, &NoEpochRefs).unwrap();
    assert_eq!(report.epochs_deleted, 0, "{report:?}");
    assert_eq!(
        epochs_present(&store, prefix),
        vec!["e1".to_string(), "e2".to_string()]
    );
}

#[test]
fn a_corrupt_locator_pointer_stops_the_sweep_instead_of_narrowing_it() {
    // The dangerous failure mode is not an error, it is a *silently
    // smaller* live set: every epoch the unreadable pointer would have
    // protected becomes collectable. Refusing to sweep is the only safe
    // reading of a pointer we cannot parse.
    let minio = Minio::shared();
    let bucket = minio.bucket("gcfork-corrupt");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let prefix = "o/t/r/gcfork-corrupt/prod";

    put(
        &store,
        &format!("{prefix}/manifest.json"),
        manifest_at("e2").as_bytes(),
    );
    put(
        &store,
        &format!("{prefix}/e1/cold-0000.seg"),
        b"the epoch at risk",
    );
    put(&store, &format!("{prefix}/e2/cold-0000.seg"), b"current");
    put(
        &store,
        &format!("{prefix}/locator.hdr"),
        b"NOPE and then some bytes",
    );

    let err = gc::gc_epochs_with_refs(&store, prefix, 60, LATER, &NoEpochRefs).unwrap_err();
    assert!(err.contains("bad magic"), "{err}");
    assert!(err.contains("locator.hdr"), "{err}");
    // Nothing was deleted on the way to that error.
    assert_eq!(
        epochs_present(&store, prefix),
        vec!["e1".to_string(), "e2".to_string()]
    );
}

#[test]
fn a_layout_with_no_manifest_at_all_is_left_alone_and_says_nothing() {
    // A repository whose storage does not exist yet — a fork between its
    // row being created and its job publishing pointers. Reading that as
    // an error made the sweeper log a failure for it on every tick.
    //
    // Left alone rather than swept: an empty live set is the correct
    // reading of "no pointers" and the catastrophic reading of "manifest
    // unreachable", and nothing distinguishes them from here.
    let minio = Minio::shared();
    let bucket = minio.bucket("gcfork-nomanifest");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let prefix = "o/t/r/gcfork-nomanifest/prod";
    put(
        &store,
        &format!("{prefix}/e1/cold-0000.seg"),
        b"not ours to judge",
    );

    let report = gc::gc_epochs_with_refs(&store, prefix, 60, LATER, &NoEpochRefs).unwrap();
    assert_eq!(report, gc::GcReport::default(), "{report:?}");
    assert_eq!(epochs_present(&store, prefix), vec!["e1".to_string()]);
}

#[test]
fn a_layout_with_no_pointer_yet_is_still_sweepable() {
    // An *absent* header is ordinary — a layout whose plane has not been
    // built — and must stay tolerated, or the first GC pass on a fresh
    // repository fails instead of doing nothing.
    let minio = Minio::shared();
    let bucket = minio.bucket("gcfork-nohdr");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let prefix = "o/t/r/gcfork-nohdr/prod";

    put(
        &store,
        &format!("{prefix}/manifest.json"),
        manifest_at("e2").as_bytes(),
    );
    put(
        &store,
        &format!("{prefix}/e1/cold-0000.seg"),
        b"unreferenced",
    );
    put(&store, &format!("{prefix}/e2/cold-0000.seg"), b"current");

    let report = gc::gc_epochs_with_refs(&store, prefix, 60, LATER, &NoEpochRefs).unwrap();
    assert_eq!(report.epochs_deleted, 1, "{report:?}");
    assert_eq!(epochs_present(&store, prefix), vec!["e2".to_string()]);
}

/// A resolver that answers from a fixed set and counts how often it was
/// asked. The count is the interesting part: GC must consult it again
/// immediately before deleting, or a fork created mid-sweep loses its
/// data.
struct Pinned {
    epochs: Vec<&'static str>,
    calls: AtomicUsize,
}

impl Pinned {
    fn new(epochs: &[&'static str]) -> Self {
        Pinned {
            epochs: epochs.to_vec(),
            calls: AtomicUsize::new(0),
        }
    }
}

impl EpochRefs for Pinned {
    fn pinned_epochs(&self) -> Result<HashSet<String>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.epochs.iter().map(|e| e.to_string()).collect())
    }
}

/// A resolver that cannot answer — the control plane is unreachable.
struct Unreachable;

impl EpochRefs for Unreachable {
    fn pinned_epochs(&self) -> Result<HashSet<String>, String> {
        Err("epoch_refs: connection refused".into())
    }
}

fn upstream_with_two_epochs(bucket: &str, prefix: &str) -> ObjectStore {
    let minio = Minio::shared();
    let b = minio.bucket(bucket);
    let store = ObjectStore::new(&b.base_url, LatencyModel::None);
    // Upstream has compacted: the manifest and pointer name e2, and e1
    // is unreferenced by anything upstream itself can see.
    put(
        &store,
        &format!("{prefix}/manifest.json"),
        manifest_at("e2").as_bytes(),
    );
    put(
        &store,
        &format!("{prefix}/e1/cold-0000.seg"),
        b"what the fork reads",
    );
    put(&store, &format!("{prefix}/e2/cold-0000.seg"), b"current");
    put(
        &store,
        &format!("{prefix}/locator.hdr"),
        &hdr("e2", Some(1), None),
    );
    store
}

#[test]
fn a_fork_keeps_upstreams_old_epoch_alive_across_a_compaction() {
    // The finding this whole seam exists for. Upstream compacts, the old
    // epoch stops being referenced by upstream's own two pointers, one
    // grace window passes, and without a resolver every object under it
    // is deleted while forks are still reading it.
    let prefix = "o/t/r/gcfork-pinned/prod";
    let store = upstream_with_two_epochs("gcfork-pinned", prefix);

    let refs = Pinned::new(&["e1"]);
    let report = gc::gc_epochs_with_refs(&store, prefix, 60, LATER, &refs).unwrap();
    assert_eq!(report.epochs_deleted, 0, "{report:?}");
    assert_eq!(
        epochs_present(&store, prefix),
        vec!["e1".to_string(), "e2".to_string()]
    );
}

#[test]
fn the_same_epoch_is_swept_once_the_last_fork_is_gone() {
    // The other half, and the one that proves the test above is not
    // simply asserting that GC never deletes anything: identical inputs,
    // empty reference set, epoch collected.
    let prefix = "o/t/r/gcfork-unpinned/prod";
    let store = upstream_with_two_epochs("gcfork-unpinned", prefix);

    let refs = Pinned::new(&[]);
    let report = gc::gc_epochs_with_refs(&store, prefix, 60, LATER, &refs).unwrap();
    assert_eq!(report.epochs_deleted, 1, "{report:?}");
    assert_eq!(epochs_present(&store, prefix), vec!["e2".to_string()]);
}

#[test]
fn references_are_re_read_immediately_before_deleting() {
    // A fork created after the scan began has registered its reference
    // by the time we are about to delete, and this re-read is where we
    // find out. It is the reason the fork worker registers the reference
    // *before* publishing its pointer: in that order, any fork that
    // could be reading the epoch is already visible here.
    let prefix = "o/t/r/gcfork-reread/prod";
    let store = upstream_with_two_epochs("gcfork-reread", prefix);

    let refs = Pinned::new(&[]);
    gc::gc_epochs_with_refs(&store, prefix, 60, LATER, &refs).unwrap();
    assert!(
        refs.calls.load(Ordering::SeqCst) >= 2,
        "resolver consulted {} time(s) — the pre-delete re-read is gone, \
         and with it the fork-creation race window",
        refs.calls.load(Ordering::SeqCst)
    );
}

#[test]
fn a_resolver_that_cannot_answer_aborts_the_sweep() {
    // "I could not find out what is referenced" must never be read as
    // "nothing is referenced". Deleting on an incomplete answer is
    // unrecoverable; not deleting costs storage until the next pass.
    let prefix = "o/t/r/gcfork-unreachable/prod";
    let store = upstream_with_two_epochs("gcfork-unreachable", prefix);

    let err = gc::gc_epochs_with_refs(&store, prefix, 60, LATER, &Unreachable).unwrap_err();
    assert!(err.contains("epoch_refs"), "{err}");
    assert_eq!(
        epochs_present(&store, prefix),
        vec!["e1".to_string(), "e2".to_string()]
    );
}
