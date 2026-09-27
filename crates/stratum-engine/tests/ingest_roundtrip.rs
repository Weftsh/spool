//! Engine gate: ingest a fixture repo into MinIO, then prove the layout
//! serves correct bytes — entry counts exact (I6), point reads hash-clean,
//! and the full clone plan reassembles into a pack that git accepts and
//! fscks clean (I11's in-process cousin; the wire version lives in the
//! server e2e).

use std::collections::HashSet;
use std::io::Read;
use stratum_engine::ingest::{publish, Pointer, PublishError, PublishMode};
use stratum_engine::{build_locator, ingest, IngestConfig};
use stratum_store::{LatencyModel, ObjectStore, Plane};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::Minio;

fn small_cfg() -> IngestConfig {
    IngestConfig {
        budget_bytes: 4 * 1024, // force several cold segments on a tiny repo
        hot_commits: 8,
        hot_budget_bytes: 8 * 1024,
        hot_anchor: 4,
        ..IngestConfig::default()
    }
}

#[test]
fn ingest_publish_and_verify() {
    let minio = Minio::shared();
    let bucket = minio.bucket("engine-roundtrip");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);

    let scratch = Scratch::new("engine");
    let repo = scratch.path().join("fixture");
    let tip = gitcli::fixture_repo(&repo, 30);

    let prefix = "o/testorg/r/testrepo/prod";
    let staging_root = scratch.path().join("staging");
    let mut out = ingest(&repo, prefix, "main", &small_cfg(), &staging_root).unwrap();
    let hdr = build_locator(&repo, &mut out, prefix, 0).unwrap();
    publish(&store, prefix, &out, &hdr, PublishMode::Create).unwrap();

    // The manifest pointer round-trips and plans exactly the repo's object
    // count: closure(all refs) with no duplicates (I5/I6).
    let manifest = stratum_store::load_manifest(&store, "o/testorg/r/testrepo", "prod").unwrap();
    assert_eq!(manifest.tip(), Some(tip.as_str()));
    let all = gitcli::git(&repo, &["rev-list", "--objects", "--all"]);
    let expected: HashSet<&str> = all.lines().map(|l| l.split(' ').next().unwrap()).collect();
    assert_eq!(manifest.total_entries(), expected.len() as u64);
    assert!(
        manifest.cold_segments.len() > 1,
        "want multiple cold segments"
    );
    assert!(!manifest.spine.is_empty());
    assert!(manifest.snapshot.is_some());

    // Point reads resolve and hash-verify through the locator plane.
    let plane = Plane::load(&store, prefix).unwrap();
    let mut checked = 0;
    for oid in expected.iter().take(50) {
        let (typ, data, _gets) = plane
            .read_object(&store, &store, oid)
            .unwrap_or_else(|e| panic!("read {oid}: {e}"));
        let mut h = sha1::Sha1::new();
        use sha1::Digest;
        h.update(format!("{} {}\0", stratum_store::pack::type_name(typ), data.len()).as_bytes());
        h.update(&data);
        assert_eq!(stratum_store::pack::hex(&h.finalize()), **oid);
        checked += 1;
    }
    assert!(checked > 10);

    // Reassemble the clone plan into one pack; a fresh repo must index it
    // (all REF bases prefix-closed, I3) and fsck clean with refs set.
    let mut pack = Vec::new();
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&(manifest.total_entries() as u32).to_be_bytes());
    for part in manifest.clone_plan() {
        let mut r = store.get_stream(&part.key, part.range).unwrap();
        let before = pack.len();
        r.read_to_end(&mut pack).unwrap();
        assert_eq!((pack.len() - before) as u64, part.expect_bytes);
    }
    use sha1::Digest;
    let digest = sha1::Sha1::digest(&pack);
    pack.extend_from_slice(&digest);

    let clone_dir = scratch.path().join("reassembled.git");
    gitcli::git(scratch.path(), &["init", "-q", "--bare", "reassembled.git"]);
    let mut c = std::process::Command::new("git");
    c.arg("-C")
        .arg(&clone_dir)
        .args(["index-pack", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = c.spawn().unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(&pack).unwrap();
    let st = child.wait_with_output().unwrap();
    assert!(
        st.status.success(),
        "index-pack rejected the reassembled clone stream: {}",
        String::from_utf8_lossy(&st.stderr)
    );
    for (name, oid) in &manifest.refs {
        gitcli::git(&clone_dir, &["update-ref", name, oid]);
    }
    gitcli::fsck(&clone_dir);
}

#[test]
fn small_repo_below_hot_window_still_ingests() {
    let minio = Minio::shared();
    let bucket = minio.bucket("engine-small");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);

    let scratch = Scratch::new("engine-small");
    let repo = scratch.path().join("tiny");
    // 3 commits < hot window of 8: spine spans depth-1 commits.
    gitcli::fixture_repo(&repo, 3);

    let prefix = "o/t/r/tiny/prod";
    let mut out = ingest(
        &repo,
        prefix,
        "main",
        &small_cfg(),
        &scratch.path().join("staging"),
    )
    .unwrap();
    let hdr = build_locator(&repo, &mut out, prefix, 0).unwrap();
    publish(&store, prefix, &out, &hdr, PublishMode::Create).unwrap();

    let manifest = stratum_store::load_manifest(&store, "o/t/r/tiny", "prod").unwrap();
    // fixture_repo makes 3 commits on main + 1 on side: first-parent depth
    // 3 -> spine of 2, boundary closure in cold.
    assert_eq!(manifest.spine.len(), 2);
    let all = gitcli::git(&repo, &["rev-list", "--objects", "--all"]);
    let expected: HashSet<&str> = all.lines().map(|l| l.split(' ').next().unwrap()).collect();
    assert_eq!(manifest.total_entries(), expected.len() as u64);
}

#[test]
fn create_mode_publish_refuses_to_clobber() {
    let minio = Minio::shared();
    let bucket = minio.bucket("engine-clobber");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);

    let scratch = Scratch::new("engine-clobber");
    let repo = scratch.path().join("fixture");
    gitcli::fixture_repo(&repo, 5);

    let prefix = "o/t/r/c/prod";
    let mut out = ingest(
        &repo,
        prefix,
        "main",
        &small_cfg(),
        &scratch.path().join("staging"),
    )
    .unwrap();
    let hdr = build_locator(&repo, &mut out, prefix, 0).unwrap();
    publish(&store, prefix, &out, &hdr, PublishMode::Create).unwrap();
    // Same epoch data, second Create must 412 on the manifest pointer.
    let err = publish(&store, prefix, &out, &hdr, PublishMode::Create).unwrap_err();
    // Typed, not sniffed: the manifest pointer is the one that lost.
    assert_eq!(err, PublishError::LostRace(Pointer::Manifest), "{err}");
    // Replace mode succeeds.
    publish(&store, prefix, &out, &hdr, PublishMode::Replace).unwrap();
}
