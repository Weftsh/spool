//! The fast-forward walk against a real layout: linear history, merge
//! ancestry, unrelated tips, the unborn-branch case, and the budget.

use stratum_engine::ancestry::{
    descent, descent_capped, is_fast_forward, is_fast_forward_capped, Ancestry, Descent,
};
use stratum_engine::ingest::{publish, PublishMode};
use stratum_engine::read::LayoutReader;
use stratum_engine::{build_locator, ingest, IngestConfig};
use stratum_store::{LatencyModel, ObjectStore};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::Minio;

#[test]
fn ancestry_answers_every_shape_the_lander_asks_about() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ancestry");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let scratch = Scratch::new("ancestry");
    let repo = scratch.path().join("fixture");
    let main_tip = gitcli::fixture_repo(&repo, 6);
    let side_tip = gitcli::git(&repo, &["rev-parse", "side"])
        .trim()
        .to_string();
    // A merge commit on main: fast-forward must hold through either
    // parent, because landing a stack top merges nothing but the walk
    // still crosses merges other people landed.
    gitcli::git(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    let merged_tip = gitcli::git(&repo, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let old_main = gitcli::git(&repo, &["rev-parse", "main~3"])
        .trim()
        .to_string();

    let prefix = "o/t/r/ancestry/prod";
    let mut out = ingest(
        &repo,
        prefix,
        "main",
        &IngestConfig::default(),
        &scratch.path().join("staging"),
    )
    .unwrap();
    let hdr = build_locator(&repo, &mut out, prefix, 0).unwrap();
    publish(&store, prefix, &out, &hdr, PublishMode::Create).unwrap();
    let manifest = stratum_store::load_manifest(&store, "o/t/r/ancestry", "prod").unwrap();
    let reader = LayoutReader::new(&store, prefix, &manifest).unwrap();

    // Linear ancestor, the everyday landing.
    assert_eq!(
        is_fast_forward(&reader, Some(&old_main), &merged_tip).unwrap(),
        Ancestry::FastForward
    );
    // Through the merge's second parent.
    assert_eq!(
        is_fast_forward(&reader, Some(&side_tip), &merged_tip).unwrap(),
        Ancestry::FastForward
    );
    // The same commit: a no-op move is a fast-forward.
    assert_eq!(
        is_fast_forward(&reader, Some(&merged_tip), &merged_tip).unwrap(),
        Ancestry::FastForward
    );
    // An unborn branch accepts anything.
    assert_eq!(
        is_fast_forward(&reader, None, &merged_tip).unwrap(),
        Ancestry::FastForward
    );
    // Backwards: the old main tip does not contain the merge.
    assert_eq!(
        is_fast_forward(&reader, Some(&merged_tip), &main_tip).unwrap(),
        Ancestry::NotAncestor
    );
    // Sideways: the side tip's history never contained main's pre-branch
    // work... it does share the root, so use a tip main never had.
    assert_eq!(
        is_fast_forward(&reader, Some(&merged_tip), &side_tip).unwrap(),
        Ancestry::NotAncestor
    );
    // The budget answers rather than walking forever.
    assert_eq!(
        is_fast_forward_capped(&reader, Some(&old_main), &merged_tip, 2).unwrap(),
        Ancestry::CapExceeded
    );
    // A tip that is not a commit is an error, not a verdict.
    let tree = gitcli::git(&repo, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .to_string();
    assert!(is_fast_forward(&reader, Some(&old_main), &tree).is_err());

    // The same walk, asked the changeset lander's way: is the commit we
    // wrote still in this trunk's history, and what sits directly on it?
    let above_old = gitcli::git(&repo, &["rev-parse", "main~2"])
        .trim()
        .to_string();
    assert_eq!(
        descent(&reader, &merged_tip, &old_main).unwrap(),
        Descent::Contains {
            child: Some(above_old)
        }
    );
    // Reached through the merge's second parent: the child is the merge.
    assert_eq!(
        descent(&reader, &merged_tip, &side_tip).unwrap(),
        Descent::Contains {
            child: Some(merged_tip.clone())
        }
    );
    // The tip itself has nothing on it.
    assert_eq!(
        descent(&reader, &merged_tip, &merged_tip).unwrap(),
        Descent::Contains { child: None }
    );
    // Reset behind, or never there.
    assert_eq!(
        descent(&reader, &main_tip, &merged_tip).unwrap(),
        Descent::NotFound
    );
    assert_eq!(
        descent_capped(&reader, &merged_tip, &old_main, 2).unwrap(),
        Descent::CapExceeded
    );
    assert!(descent(&reader, &tree, &old_main).is_err());
}
