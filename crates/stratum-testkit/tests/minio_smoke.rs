//! The testkit's own gate: MinIO starts, buckets are per-test, and every
//! request goes through the store's SigV4 path (no anonymous access).

use stratum_store::{LatencyModel, ObjectStore, PutCond, PutError};
use stratum_testkit::Minio;

#[test]
fn signed_put_get_roundtrip_and_conditional_writes() {
    let minio = Minio::shared();
    let bucket = minio.bucket("smoke");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);

    // Basic roundtrip.
    store.put("a/b/c.txt", b"hello", PutCond::None).unwrap();
    assert_eq!(store.get("a/b/c.txt").unwrap(), b"hello");

    // Range GET must be honored (invariant I13: 206 or fail loudly).
    let mut r = store.get_stream("a/b/c.txt", Some((1, 3))).unwrap();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut r, &mut buf).unwrap();
    assert_eq!(buf, b"ell");

    // Create-only semantics.
    store
        .put("once.txt", b"first", PutCond::IfNoneMatchStar)
        .unwrap();
    match store.put("once.txt", b"second", PutCond::IfNoneMatchStar) {
        Err(PutError::Conflict) => {}
        other => panic!("expected Conflict, got {other:?}"),
    }

    // CAS semantics.
    let (_, etag) = store.get_with_etag("once.txt").unwrap();
    store
        .put("once.txt", b"third", PutCond::IfMatch(etag))
        .unwrap();
    let (body, _) = store.get_with_etag("once.txt").unwrap();
    assert_eq!(body, b"third");
    match store.put(
        "once.txt",
        b"stale",
        PutCond::IfMatch("\"deadbeef\"".into()),
    ) {
        Err(PutError::Conflict) => {}
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[test]
fn buckets_are_isolated_per_test() {
    let minio = Minio::shared();
    let b1 = minio.bucket("iso1");
    let b2 = minio.bucket("iso2");
    let s1 = ObjectStore::new(&b1.base_url, LatencyModel::None);
    let s2 = ObjectStore::new(&b2.base_url, LatencyModel::None);
    s1.put("k", b"v1", PutCond::None).unwrap();
    assert!(s2.get("k").is_err());
}

#[test]
fn list_and_delete_roundtrip() {
    let minio = Minio::shared();
    let bucket = minio.bucket("listdel");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    for i in 0..5 {
        store
            .put(&format!("e1/seg-{i}.bin"), b"x", PutCond::None)
            .unwrap();
    }
    store.put("e2/other.bin", b"y", PutCond::None).unwrap();
    let listed = store.list("e1/").unwrap();
    assert_eq!(listed.len(), 5);
    assert!(listed
        .iter()
        .all(|(k, lm)| k.starts_with("e1/") && !lm.is_empty()));
    store.delete("e1/seg-0.bin").unwrap();
    store.delete("e1/seg-0.bin").unwrap(); // idempotent
    assert_eq!(store.list("e1/").unwrap().len(), 4);
    assert_eq!(store.list("").unwrap().len(), 5);
}
