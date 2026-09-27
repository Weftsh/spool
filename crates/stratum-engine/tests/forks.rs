//! Zero-copy forks, against real ingested storage.
//!
//! The claim a fork makes is strong and easy to get subtly wrong: the
//! fork's prefix contains **two small pointers and no objects**, and
//! every read it serves resolves into upstream's immutable data. So
//! these tests assert on the bytes, not on the row — a fork that looks
//! right in Postgres and 404s on `/files` is the exact failure this
//! slice exists to prevent.

use stratum_engine::fork::{self, ForkOutcome, Upstream};
use stratum_engine::ingest::{publish, PublishMode};
use stratum_engine::{build_locator, ingest, IngestConfig};
use stratum_store::manifest::Manifest;
use stratum_store::{LatencyModel, ObjectStore, Plane};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::Minio;

/// Ingest a real repository into `prefix` and return its manifest.
fn seed(store: &ObjectStore, scratch: &Scratch, prefix: &str, commits: usize) -> Manifest {
    let repo = scratch.path().join(format!("fixture-{commits}"));
    gitcli::fixture_repo(&repo, commits);
    let cfg = IngestConfig::default();
    let mut out = ingest(&repo, prefix, "main", &cfg, &scratch.path().join("stage")).unwrap();
    let hdr = build_locator(&repo, &mut out, prefix, 0).unwrap();
    publish(store, prefix, &out, &hdr, PublishMode::Create).unwrap();
    let bytes = store.get(&format!("{prefix}/manifest.json")).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn keys_under(store: &ObjectStore, prefix: &str) -> Vec<String> {
    let mut k: Vec<String> = store
        .list(&format!("{prefix}/"))
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    k.sort();
    k
}

#[test]
fn a_fork_stores_two_pointers_and_not_one_object() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-zerocopy");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let scratch = Scratch::new("forks-zerocopy");

    let up_prefix = "o/t/r/upstream/prod";
    let fork_prefix = "o/t/r/thefork/prod";
    let upstream_manifest = seed(&store, &scratch, up_prefix, 8);

    let up = fork::read_upstream(&store, up_prefix).unwrap().unwrap();
    let outcome = fork::publish_fork(&store, fork_prefix, &up, "thefork", "prod").unwrap();
    assert_eq!(outcome, ForkOutcome::Forked);

    // The whole claim, as a assertion about the bucket: the fork's
    // prefix holds its manifest and its locator header, and nothing
    // else. Not one segment, not one byte of object data.
    assert_eq!(
        keys_under(&store, fork_prefix),
        vec![
            format!("{fork_prefix}/locator.hdr"),
            format!("{fork_prefix}/manifest.json"),
        ]
    );

    // And the pointer is the new magic, carrying an absolute prefix.
    let hdr = store.get(&format!("{fork_prefix}/locator.hdr")).unwrap();
    assert_eq!(&hdr[..4], b"SLH4");

    // The fork's manifest names itself but keeps upstream's absolute
    // keys — rewriting any of them would turn a fork into a copy that
    // reads keys nobody ever wrote.
    let forked: Manifest =
        serde_json::from_slice(&store.get(&format!("{fork_prefix}/manifest.json")).unwrap())
            .unwrap();
    assert_eq!(forked.repo, "thefork");
    assert_eq!(forked.refs, upstream_manifest.refs);
    assert_eq!(forked.head, upstream_manifest.head);
    for s in &forked.cold_segments {
        assert!(
            s.key.starts_with(up_prefix),
            "fork rewrote a segment key: {}",
            s.key
        );
    }
}

#[test]
fn a_fork_reads_objects_out_of_upstreams_storage() {
    // The bug that made this slice dangerous: a byte-copied manifest
    // clones correctly and then 404s on /files, /tree, /diff and /log,
    // because every point-read key came from the *caller's* prefix.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-reads");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let scratch = Scratch::new("forks-reads");

    let up_prefix = "o/t/r/upstream/prod";
    let fork_prefix = "o/t/r/thefork/prod";
    let manifest = seed(&store, &scratch, up_prefix, 8);

    let up = fork::read_upstream(&store, up_prefix).unwrap().unwrap();
    fork::publish_fork(&store, fork_prefix, &up, "thefork", "prod").unwrap();

    // Load the plane from the *fork's* prefix and resolve a real object.
    let plane = Plane::load(&store, fork_prefix).unwrap();
    assert!(
        plane.data_prefix.starts_with(up_prefix),
        "fork's plane points at {} — it should be upstream's data",
        plane.data_prefix
    );

    let tip = manifest.tip().expect("fixture has a tip").to_string();
    let (kind, bytes, _gets) = plane.read_object(&store, &store, &tip).unwrap();
    assert_eq!(kind, stratum_store::pack::OBJ_COMMIT);
    assert!(
        String::from_utf8_lossy(&bytes).contains("tree "),
        "did not read a commit out of upstream's segments"
    );

    // Reading the same object through upstream's own plane gives the
    // identical bytes — which is the point: there is one copy.
    let up_plane = Plane::load(&store, up_prefix).unwrap();
    let (_, up_bytes, _) = up_plane.read_object(&store, &store, &tip).unwrap();
    assert_eq!(bytes, up_bytes);
}

#[test]
fn promoting_a_fork_gives_it_storage_of_its_own() {
    // What promotion has to achieve, stated as a property of the bucket:
    // after it, **no key in the fork's manifest lives under upstream**.
    // Until that is true the fork is still reading somebody else's bytes
    // and its `epoch_refs` may not be released; once it is true, the
    // upstream is finally sweepable.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-promote");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let scratch = Scratch::new("forks-promote");

    let up_prefix = "o/t/r/upstream/prod";
    let fork_prefix = "o/t/r/thefork/prod";
    let upstream = seed(&store, &scratch, up_prefix, 8);

    let up = fork::read_upstream(&store, up_prefix).unwrap().unwrap();
    fork::publish_fork(&store, fork_prefix, &up, "thefork", "prod").unwrap();

    // Before: the fork owns two pointers and reads upstream for everything.
    let before: Manifest =
        serde_json::from_slice(&store.get(&format!("{fork_prefix}/manifest.json")).unwrap())
            .unwrap();
    assert!(
        before
            .cold_segments
            .iter()
            .all(|s| s.key.starts_with(up_prefix)),
        "fixture is wrong: the fork was not sharing upstream's segments"
    );

    let outcome = fork::promote(
        &store,
        fork_prefix,
        &IngestConfig::default(),
        &scratch.path().join("promote-work"),
    )
    .unwrap();
    assert_eq!(outcome, fork::PromoteOutcome::Promoted);

    // After: every key is the fork's own, and upstream is named nowhere.
    let after: Manifest =
        serde_json::from_slice(&store.get(&format!("{fork_prefix}/manifest.json")).unwrap())
            .unwrap();
    let mut keys: Vec<&str> = Vec::new();
    keys.extend(after.segments.iter().map(|s| s.key.as_str()));
    keys.extend(after.cold_segments.iter().map(|s| s.key.as_str()));
    keys.extend(after.hot_segments.iter().map(|s| s.key.as_str()));
    if let Some(l) = &after.locator {
        keys.push(&l.key);
        keys.push(&l.chains_key);
    }
    keys.extend(after.ref_pages.iter().map(|p| p.key.as_str()));
    for w in &after.wal {
        keys.push(&w.key);
        keys.push(&w.oids_key);
    }
    assert!(!keys.is_empty(), "a promoted fork has no keys at all");
    for k in &keys {
        assert!(
            k.starts_with(fork_prefix),
            "a promoted fork still reads {k}, which is not its own"
        );
    }

    // And it is the same repository afterwards — promotion moves bytes,
    // it does not rewrite history.
    assert_eq!(after.refs, upstream.refs);
    assert_eq!(after.head, upstream.head);

    // The fork can still be read, now from its own storage.
    let plane = Plane::load(&store, fork_prefix).unwrap();
    assert!(
        plane.data_prefix.starts_with(fork_prefix),
        "{}",
        plane.data_prefix
    );
    let tip = after.tip().unwrap().to_string();
    let (kind, _bytes, _) = plane.read_object(&store, &store, &tip).unwrap();
    assert_eq!(kind, stratum_store::pack::OBJ_COMMIT);
}

#[test]
fn promoting_an_empty_fork_is_not_needed_and_not_an_error() {
    // An empty fork holds no references either, so there is nothing to
    // release and nothing to copy. Reporting this as a failure would
    // wedge the promotion queue behind a repository with no content.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-promote-empty");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let scratch = Scratch::new("forks-promote-empty");
    let prefix = "o/t/r/emptyfork/prod";
    store
        .put(
            &format!("{prefix}/manifest.json"),
            br#"{"schema":3,"repo":"r","layout":"prod","refs":[],"head":"refs/heads/main","epoch":"e1"}"#,
            stratum_store::PutCond::None,
        )
        .unwrap();

    let outcome = fork::promote(
        &store,
        prefix,
        &IngestConfig::default(),
        &scratch.path().join("w"),
    )
    .unwrap();
    assert_eq!(outcome, fork::PromoteOutcome::NotNeeded);
}

#[test]
fn publishing_a_fork_twice_does_not_clobber_the_first() {
    // The retry path. The fork worker registers its epoch references
    // before publishing its pointer precisely so that a crash between
    // the two is retried from the top — so this runs, routinely, and
    // must not overwrite a manifest the fork may have moved on from.
    // The manifest changes only by CAS (I9); "already published" is
    // success, not a failure to be retried into a clobber.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-retry");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let scratch = Scratch::new("forks-retry");

    let up_prefix = "o/t/r/upstream/prod";
    let fork_prefix = "o/t/r/thefork/prod";
    seed(&store, &scratch, up_prefix, 6);
    let up = fork::read_upstream(&store, up_prefix).unwrap().unwrap();

    fork::publish_fork(&store, fork_prefix, &up, "thefork", "prod").unwrap();
    let first = store.get(&format!("{fork_prefix}/manifest.json")).unwrap();

    // Pretend the fork has since taken a push of its own.
    let mut moved: Manifest = serde_json::from_slice(&first).unwrap();
    moved.head = "refs/heads/divergent".into();
    let moved_bytes = serde_json::to_vec(&moved).unwrap();
    store
        .put(
            &format!("{fork_prefix}/manifest.json"),
            &moved_bytes,
            stratum_store::PutCond::None,
        )
        .unwrap();

    // The retry reports success and leaves the fork's own manifest be.
    let again = fork::publish_fork(&store, fork_prefix, &up, "thefork", "prod").unwrap();
    assert_eq!(again, ForkOutcome::Forked);
    assert_eq!(
        store.get(&format!("{fork_prefix}/manifest.json")).unwrap(),
        moved_bytes,
        "a retried fork job overwrote the fork's own history"
    );
}

#[test]
fn forking_a_repository_with_nothing_in_it_is_not_an_error() {
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-empty");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    // Nothing was ever pushed here.
    assert!(fork::read_upstream(&store, "o/t/r/never-pushed/prod")
        .unwrap()
        .is_none());
}

#[test]
fn a_repository_pushed_but_not_yet_compacted_is_forkable() {
    // A manifest with no plane is a repository whose locator has not
    // been built. Forking it would publish a fork that cannot point
    // anywhere — readable in the listing, broken on every object read.
    let minio = Minio::shared();
    let bucket = minio.bucket("forks-noplane");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let prefix = "o/t/r/noplane/prod";
    store
        .put(
            &format!("{prefix}/manifest.json"),
            br#"{"schema":3,"repo":"r","layout":"prod","refs":[],"head":"refs/heads/main","epoch":"e1"}"#,
            stratum_store::PutCond::None,
        )
        .unwrap();

    let up = fork::read_upstream(&store, prefix).unwrap().unwrap();
    assert!(up.hdr.is_none(), "a plane appeared from nowhere");
    assert!(up.data_prefix.is_none());

    let fork_prefix = "o/t/r/noplane-fork/prod";
    let outcome = fork::publish_fork(&store, fork_prefix, &up, "noplane-fork", "prod").unwrap();
    assert_eq!(outcome, ForkOutcome::Forked);

    // One pointer, not two: the fork has exactly what upstream has.
    assert_eq!(
        keys_under(&store, fork_prefix),
        vec![format!("{fork_prefix}/manifest.json")]
    );
}

/// A manifest whose keys span two repositories — which is what a fork of
/// an already-diverged fork actually copies.
fn manifest_spanning(a: &str, b: &str) -> Manifest {
    let json = format!(
        r#"{{"schema":3,"repo":"r","layout":"prod","refs":[],"head":"refs/heads/main",
             "epoch":"e-own",
             "cold_segments":[{{"key":"{a}/e-base/cold-0000.seg","entries":1,"bytes":1}}],
             "hot_segments":[{{"key":"{b}/e-own/hot-0000.seg","entries":1,"bytes":1,"commits":1}}]}}"#
    );
    serde_json::from_str(&json).unwrap()
}

#[test]
fn a_fork_of_a_diverged_fork_pins_every_repository_it_reads() {
    // The finding that changed this worker. A fork that has taken its
    // own pushes has a manifest naming keys in *two* prefixes: the
    // root's shared base and its own new segments. Forking that and
    // pinning only one of them leaves the other collectable, and the
    // failure is silent until a clone stops passing fsck in a repository
    // nobody touched.
    let up = Upstream {
        manifest: manifest_spanning("o/t/r/theroot/prod", "o/t/r/themiddle/prod"),
        hdr: None,
        data_prefix: Some("o/t/r/theroot/prod/e-base".into()),
    };

    let refs = fork::references_in(&up, "thenewfork");
    assert!(
        refs.contains(&("theroot".to_string(), "e-base".to_string())),
        "the shared base was not pinned: {refs:?}"
    );
    assert!(
        refs.contains(&("themiddle".to_string(), "e-own".to_string())),
        "the parent's own pushes were not pinned: {refs:?}"
    );
    assert_eq!(refs.len(), 2, "{refs:?}");
}

#[test]
fn a_fork_holds_no_reference_against_itself() {
    // Once a fork has pushed, its own segments are in its own manifest.
    // A self-reference would be refused by `epoch_refs::register`
    // anyway, but it would also make the `RESTRICT` on the owner refuse
    // to ever delete the repository — a row existing only to block its
    // own cleanup.
    let up = Upstream {
        manifest: manifest_spanning("o/t/r/theroot/prod", "o/t/r/myself/prod"),
        hdr: None,
        data_prefix: Some("o/t/r/theroot/prod/e-base".into()),
    };
    let refs = fork::references_in(&up, "myself");
    assert_eq!(refs, vec![("theroot".to_string(), "e-base".to_string())]);
}

#[test]
fn keys_that_cannot_be_attributed_are_ignored_rather_than_guessed_at() {
    // A key we cannot attribute is one we must not claim to have
    // pinned: inventing an owner would register a reference against the
    // wrong repository and leave the real one collectable.
    let mut manifest = manifest_spanning("o/t/r/theroot/prod", "o/t/r/theroot/prod");
    manifest.cold_segments[0].key = "some/other/shape.seg".into();
    let up = Upstream {
        manifest,
        hdr: None,
        data_prefix: Some("o/t/r/theroot/prod/e-base".into()),
    };
    let refs = fork::references_in(&up, "thefork");
    // Only the well-formed hot segment and the data prefix survive.
    assert!(refs.iter().all(|(repo, _)| repo == "theroot"), "{refs:?}");
}
