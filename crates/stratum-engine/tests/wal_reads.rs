//! Reads that cross WAL packs. A push's thin pack deltas against whatever
//! the client knows the server has, which after a fold is the locator
//! plane — and the *next* push deltas against the one before it. Resolving
//! either has to find bases in the plane, in an earlier pack, and in a
//! later pack, in any order the packs happen to be listed.

use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::Write;
use stratum_engine::ingest::{publish, PublishMode};
use stratum_engine::objwrite::{hash_object, hex, parse_hex};
use stratum_engine::read::LayoutReader;
use stratum_engine::refops::{transact, Expect, NewPack, RefUpdate};
use stratum_engine::{build_locator, ingest, IngestConfig};
use stratum_store::pack::{OBJ_BLOB, OBJ_REF_DELTA};
use stratum_store::{LatencyModel, ObjectStore};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::Minio;

fn varint(mut n: usize) -> Vec<u8> {
    let mut v = Vec::new();
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            v.push(b);
            return v;
        }
        v.push(b | 0x80);
    }
}

/// A git delta that copies the whole base and appends `extra`.
fn append_delta(base: &[u8], extra: &[u8]) -> Vec<u8> {
    assert!(extra.len() < 128);
    let mut d = varint(base.len());
    d.extend(varint(base.len() + extra.len()));
    let n = base.len();
    let mut op = 0x80u8;
    let mut args = Vec::new();
    for (bit, shift) in [(0x10u8, 0u32), (0x20, 8), (0x40, 16)] {
        let byte = ((n >> shift) & 0xff) as u8;
        if byte != 0 {
            op |= bit;
            args.push(byte);
        }
    }
    d.push(op);
    d.extend(args);
    d.push(extra.len() as u8);
    d.extend_from_slice(extra);
    d
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

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// One REF_DELTA entry against `base_oid`, plus the oid the entry
/// resolves to.
fn ref_delta_entry(
    base_oid: &str,
    kind: u8,
    base: &[u8],
    extra: &[u8],
) -> (Vec<u8>, String, Vec<u8>) {
    let delta = append_delta(base, extra);
    let mut e = entry_header(OBJ_REF_DELTA, delta.len());
    e.extend_from_slice(&parse_hex(base_oid).unwrap());
    e.extend(deflate(&delta));
    let mut result = base.to_vec();
    result.extend_from_slice(extra);
    let oid = hex(&hash_object(kind, &result));
    (e, oid, result)
}

fn land(
    store: &ObjectStore,
    prefix: &str,
    branch: &str,
    tip: &str,
    payload: Vec<u8>,
    oids: &[&str],
    entries: u64,
) {
    let mut sorted: Vec<[u8; 20]> = oids.iter().map(|o| parse_hex(o).unwrap()).collect();
    sorted.sort();
    let pack = NewPack {
        payload,
        oids: sorted,
        entries,
    };
    transact(
        store,
        prefix,
        &[RefUpdate {
            name: format!("refs/heads/{branch}"),
            expect: Expect::Any,
            new: Some(tip.to_string()),
        }],
        Some(&pack),
    )
    .unwrap_or_else(|_| panic!("landing {branch}"));
}

/// The shape a compaction leaves behind. Pack A deltas against an object
/// that has since been folded into the plane; pack B deltas against an
/// object in pack A. Reading A's object walks the WAL for its plane base,
/// and that walk opens B — whose own base is the very object being read.
///
/// The reader used to skip any pack it was mid-indexing, so B's base
/// lookup fell through to the plane, which had never held it: "not in
/// locator", for an object the manifest plainly lists. Every push after
/// the first fold that touched a file the previous push touched became
/// unreadable through the API — `.weft` for the workflow trigger, the
/// lander, the commits API — while `git clone` (a different reader) kept
/// serving it, so nothing looked lost.
#[test]
fn a_delta_chain_across_two_wal_packs_resolves_after_a_fold() {
    let minio = Minio::shared();
    let bucket = minio.bucket("engine-walchain");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let scratch = Scratch::new("walchain");
    let repo = scratch.path().join("fixture");
    gitcli::fixture_repo(&repo, 4);

    let prefix = "o/t/r/walchain/prod";
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

    // The folded base: a blob the plane serves.
    let manifest = stratum_store::load_manifest(&store, "o/t/r/walchain", "prod").unwrap();
    let reader = LayoutReader::new(&store, prefix, &manifest).unwrap();
    let tip = manifest.tip().unwrap().to_string();
    let readme = reader.entry_at(&tip, "README.md").unwrap().unwrap();
    let plane_oid = hex(&readme.oid);
    let (kind, plane_bytes) = reader.object(&plane_oid).unwrap();

    // Pack A: one entry, a delta against the plane blob.
    let (a_entry, a_oid, a_bytes) =
        ref_delta_entry(&plane_oid, kind, &plane_bytes, b"pushed once\n");
    land(&store, prefix, "wal-a", &a_oid, a_entry, &[&a_oid], 1);
    // Pack B: one entry, a delta against pack A's object.
    let (b_entry, b_oid, b_bytes) = ref_delta_entry(&a_oid, kind, &a_bytes, b"pushed twice\n");
    land(&store, prefix, "wal-b", &b_oid, b_entry, &[&b_oid], 1);

    let manifest = stratum_store::load_manifest(&store, "o/t/r/walchain", "prod").unwrap();
    assert_eq!(manifest.wal.len(), 2, "both packs are listed");

    // A's object, read cold: this is the read the trigger makes.
    let reader = LayoutReader::new(&store, prefix, &manifest).unwrap();
    let (_, got) = reader
        .object(&a_oid)
        .unwrap_or_else(|e| panic!("reading pack A's object: {e}"));
    assert_eq!(got, a_bytes);
    // B's object through A through the plane, on a fresh reader.
    let reader = LayoutReader::new(&store, prefix, &manifest).unwrap();
    let (_, got) = reader
        .object(&b_oid)
        .unwrap_or_else(|e| panic!("reading pack B's object: {e}"));
    assert_eq!(got, b_bytes);
    // And the plane base itself still reads through the same reader.
    let (_, got) = reader.object(&plane_oid).unwrap();
    assert_eq!(got, plane_bytes);
}

/// A REF_DELTA whose base sits *later* in the same pack. Legal in the
/// pack format, and a shape a thin pack can take; the index must be able
/// to identify entries forward of the one being resolved.
#[test]
fn a_ref_delta_may_precede_its_base_in_the_same_pack() {
    let minio = Minio::shared();
    let bucket = minio.bucket("engine-walorder");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let scratch = Scratch::new("walorder");
    let repo = scratch.path().join("fixture");
    gitcli::fixture_repo(&repo, 2);

    let prefix = "o/t/r/walorder/prod";
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

    let base_bytes = b"a full object, listed second\n".to_vec();
    let base_oid = hex(&hash_object(OBJ_BLOB, &base_bytes));
    let (delta_entry, delta_oid, delta_bytes) =
        ref_delta_entry(&base_oid, OBJ_BLOB, &base_bytes, b"and a delta onto it\n");
    let mut payload = delta_entry;
    payload.extend(entry_header(OBJ_BLOB, base_bytes.len()));
    payload.extend(deflate(&base_bytes));
    land(
        &store,
        prefix,
        "wal-c",
        &delta_oid,
        payload,
        &[&delta_oid, &base_oid],
        2,
    );

    let manifest = stratum_store::load_manifest(&store, "o/t/r/walorder", "prod").unwrap();
    let reader = LayoutReader::new(&store, prefix, &manifest).unwrap();
    let (_, got) = reader
        .object(&delta_oid)
        .unwrap_or_else(|e| panic!("delta before its base: {e}"));
    assert_eq!(got, delta_bytes);
    let (_, got) = reader.object(&base_oid).unwrap();
    assert_eq!(got, base_bytes);

    // The other order on one reader: the base is already identified when
    // the delta asks for it, and is taken by offset without a lookup.
    let reader = LayoutReader::new(&store, prefix, &manifest).unwrap();
    assert_eq!(reader.object(&base_oid).unwrap().1, base_bytes);
    assert_eq!(reader.object(&delta_oid).unwrap().1, delta_bytes);
}

/// The sidecar is the pack's table of contents, and the reader trusts it
/// for membership. One that names an object the entries do not hash to
/// must be reported as that — not searched for in the plane and reported
/// missing from a place it was never claimed to be.
#[test]
fn a_sidecar_that_lists_an_object_the_pack_lacks_is_named() {
    let minio = Minio::shared();
    let bucket = minio.bucket("engine-walliar");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let scratch = Scratch::new("walliar");
    let repo = scratch.path().join("fixture");
    gitcli::fixture_repo(&repo, 2);

    let prefix = "o/t/r/walliar/prod";
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

    let real = b"the one object actually in the pack\n".to_vec();
    let real_oid = hex(&hash_object(OBJ_BLOB, &real));
    let phantom = hex(&hash_object(OBJ_BLOB, b"never packed"));
    let mut payload = entry_header(OBJ_BLOB, real.len());
    payload.extend(deflate(&real));
    land(
        &store,
        prefix,
        "wal-d",
        &real_oid,
        payload,
        &[&real_oid, &phantom],
        1,
    );

    let manifest = stratum_store::load_manifest(&store, "o/t/r/walliar", "prod").unwrap();
    let reader = LayoutReader::new(&store, prefix, &manifest).unwrap();
    assert_eq!(reader.object(&real_oid).unwrap().1, real);
    let err = reader.object(&phantom).unwrap_err();
    assert!(
        err.contains("listed by") && err.contains("not among its entries"),
        "{err}"
    );
}
