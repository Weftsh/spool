//! Paged ref store: ingest with sharded refs, resolve every ref through
//! the pages, and grow a page past its max to force a split — the
//! scaling path a million-ref repo takes, exercised with tiny pages.
//! Own test binary so STRATUM_REF_PAGE_MAX can't race other tests.

use stratum_engine::ingest::{publish, PublishMode};
use stratum_engine::{build_locator, ingest, IngestConfig};
use stratum_store::{refpages, LatencyModel, ObjectStore};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::Minio;

#[test]
fn paged_refs_roundtrip_lookup_and_split() {
    let minio = Minio::shared();
    let bucket = minio.bucket("engine-refpages");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);

    let scratch = Scratch::new("refpages");
    let repo = scratch.path().join("fixture");
    let tip = gitcli::fixture_repo(&repo, 6);
    for i in 0..9 {
        gitcli::git(&repo, &["branch", &format!("topic-{i:02}"), &tip]);
    }

    let prefix = "o/testorg/r/paged/prod";
    let cfg = IngestConfig {
        paged_refs: true,
        page_size: 4,
        ..IngestConfig::default()
    };
    let staging = scratch.path().join("staging");
    let mut out = ingest(&repo, prefix, "main", &cfg, &staging).unwrap();
    let hdr = build_locator(&repo, &mut out, prefix, 0).unwrap();
    publish(&store, prefix, &out, &hdr, PublishMode::Create).unwrap();

    let manifest = stratum_store::load_manifest(&store, "o/testorg/r/paged", "prod").unwrap();
    assert!(
        manifest.ref_pages.len() >= 2,
        "10 refs at page_size=4 must shard into multiple pages, got {}",
        manifest.ref_pages.len()
    );

    // Every ref git knows resolves identically through the page store.
    let listing = gitcli::git(
        &repo,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
    );
    let mut checked = 0;
    for line in listing.lines() {
        let (oid, name) = line.split_once(' ').unwrap();
        let got = refpages::lookup(&store, &manifest, name).unwrap();
        assert_eq!(got.as_deref(), Some(oid), "lookup {name}");
        checked += 1;
    }
    assert!(checked >= 10, "expected the fixture refs, saw {checked}");
    // A name inside the covered range but absent resolves to None.
    assert_eq!(
        refpages::lookup(&store, &manifest, "refs/heads/topic-00x").unwrap(),
        None
    );

    // Update path: writing into a full page splits it; the new pages are
    // in the store before the manifest would commit (data-first).
    std::env::set_var("STRATUM_REF_PAGE_MAX", "4");
    let mut m2 = manifest.clone();
    let pages_before = m2.ref_pages.len();
    let counts_before: u64 = m2.ref_pages.iter().map(|p| p.count).sum();
    let data_prefix = format!("{prefix}/{}", m2.epoch);
    refpages::update(&store, &mut m2, &data_prefix, "refs/heads/topic-045", &tip).unwrap();
    let counts_after: u64 = m2.ref_pages.iter().map(|p| p.count).sum();
    assert_eq!(counts_after, counts_before + 1);
    assert!(
        m2.ref_pages.len() > pages_before,
        "inserting into a full page must split ({} -> {})",
        pages_before,
        m2.ref_pages.len()
    );
    for p in &m2.ref_pages {
        let entries = refpages::load_page(&store, p).unwrap();
        assert_eq!(entries.len() as u64, p.count);
        assert!(entries.windows(2).all(|w| w[0].0 < w[1].0), "sorted pages");
    }
    assert_eq!(
        refpages::lookup(&store, &m2, "refs/heads/topic-045").unwrap(),
        Some(tip.clone())
    );

    // Moving an existing ref rewrites in place — no growth, new value.
    let mut m3 = m2.clone();
    let pages = m3.ref_pages.len();
    let other = gitcli::git(&repo, &["rev-parse", "HEAD~1"]);
    refpages::update(
        &store,
        &mut m3,
        &data_prefix,
        "refs/heads/topic-00",
        other.trim(),
    )
    .unwrap();
    assert_eq!(m3.ref_pages.len(), pages);
    assert_eq!(
        refpages::lookup(&store, &m3, "refs/heads/topic-00").unwrap(),
        Some(other.trim().to_string())
    );
}
