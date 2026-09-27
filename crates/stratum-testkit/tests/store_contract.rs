//! The store contract, run against MinIO on every CI run.
//!
//! This is the half of the contract a hermetic suite can prove. The other
//! half — that *real S3* answers the same way — cannot be run here,
//! because credentials do not belong in CI. That is a documented manual
//! gate: `scripts/manual-s3.sh check`. `scripts/ci-local.sh` names it as a
//! SKIP rather than counting it as a pass, for the reason CLAUDE.md gives
//! about skips generally.
//!
//! Running the same cases against MinIO is still worth it. It keeps the
//! contract honest as the engine grows new store dependencies, and it
//! means the manual S3 run is a *comparison* against a known-good
//! baseline rather than a first look.

use stratum_store::{LatencyModel, ObjectStore};
use stratum_testkit::contract;
use stratum_testkit::Minio;

#[test]
fn the_store_contract_holds_against_minio() {
    let minio = Minio::shared();
    let bucket = minio.bucket("contract");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);

    let report = contract::run_all(&store, "contract");

    for (name, why) in &report.failed {
        let case = contract::cases().iter().find(|c| c.name == *name);
        eprintln!(
            "FAIL {name}\n     {why}\n     relied on by: {}",
            case.map(|c| c.relied_on_by).unwrap_or("?")
        );
    }
    assert!(
        report.ok(),
        "{} of {} contract cases failed against MinIO",
        report.failed.len(),
        contract::cases().len()
    );

    // Every case must have run. A case that silently stops being executed
    // is worse than one that fails: the contract looks green while the
    // semantic it pinned goes unchecked.
    assert_eq!(
        report.passed.len(),
        contract::cases().len(),
        "not every contract case ran"
    );
}

/// The case names are part of the contract's readability — a failure in
/// CI is read by someone who has never opened `contract.rs`. Keep them
/// unique and descriptive rather than numbered.
#[test]
fn every_contract_case_is_uniquely_named_and_explains_its_dependents() {
    let mut names: Vec<&str> = contract::cases().iter().map(|c| c.name).collect();
    let before = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(before, names.len(), "duplicate contract case name");

    for case in contract::cases() {
        assert!(
            !case.relied_on_by.trim().is_empty(),
            "case {} does not say what relies on it — the rule for adding a \
             case is that you can name the code that breaks without it",
            case.name
        );
    }
}
