//! Crafted-byte edge cases for the vendored formats: pack entry and
//! delta corners, locator header validation, idx/object parsers, SigV4
//! session tokens, ranged-GET status discipline, and the sha1-only
//! manifest guard.

use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::Write;
use stratum_store::pack::{
    apply_delta, hex, resolve, scan_pack, type_name, Resolved, OBJ_BLOB, OBJ_TAG,
};
use stratum_store::plane::{parse_header, rebase_header, write_header, LocatorHeader};
use stratum_store::{gitobj, LatencyModel, ObjectStore, Plane};
use stratum_testkit::httpfake::{FakeHttp, Reply, Scripted};

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
    z.write_all(data).unwrap();
    z.finish().unwrap()
}

/// One stripped-pack entry: type/size varint header + zlib body.
fn entry(typ: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut size = data.len() as u64;
    let mut hb = ((typ & 7) << 4) | (size & 0x0f) as u8;
    size >>= 4;
    while size > 0 {
        out.push(hb | 0x80);
        hb = (size & 0x7f) as u8;
        size >>= 7;
    }
    out.push(hb);
    out.extend_from_slice(&deflate(data));
    out
}

#[test]
fn type_names_cover_every_tag() {
    assert_eq!(type_name(OBJ_TAG), "tag");
    assert_eq!(type_name(0), "unknown");
    assert_eq!(type_name(9), "unknown");
}

#[test]
fn delta_and_entry_corner_cases() {
    // A copy op with len bytes all zero means 0x10000 — build a 64KiB+
    // base and copy the first 0x10000 bytes with a zero-length encoding.
    let base = vec![0xabu8; 0x10000 + 8];
    let mut delta = Vec::new();
    delta.push(0x80 | 0x01); // src size varint: multi… keep simple sizes below
                             // Rebuild properly: src size then dst size as varints.
    delta.clear();
    let push_varint = |v: u64, d: &mut Vec<u8>| {
        let mut v = v;
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v > 0 {
                b |= 0x80;
                d.push(b);
            } else {
                d.push(b);
                break;
            }
        }
    };
    push_varint(base.len() as u64, &mut delta);
    push_varint(0x10000, &mut delta);
    // copy op: offset 0 (no offset bytes), size bytes absent -> 0x10000.
    delta.push(0x80);
    let out = apply_delta(&base, &delta).unwrap();
    assert_eq!(out.len(), 0x10000);

    // A size varint with endless continuation bits errors out.
    let junk = vec![0x80u8; 12];
    let err = apply_delta(b"base", &junk).unwrap_err();
    assert!(err.contains("varint too long"), "{err}");

    // Unexpected entry types are named in errors: type 5 is unassigned.
    let seg = entry(5, b"payload");
    let err = resolve(&seg, 0, 0).err().unwrap();
    assert!(err.contains("unexpected entry type 5"), "{err}");
    let err = scan_pack(&seg, 1).err().unwrap();
    assert!(err.contains("unexpected entry type 5"), "{err}");

    // An OFS delta whose base would lie before the slice reports the
    // escape instead of reading foreign bytes.
    let blob = entry(OBJ_BLOB, b"0123456789abcdef0123");
    let delta_body = {
        let mut d = Vec::new();
        push_varint(20, &mut d);
        push_varint(4, &mut d);
        d.push(0x91); // copy: one offset byte, one size byte
        d.push(0); // offset 0
        d.push(4); // size 4
        d
    };
    let ofs_entry = |distance: u8, body: &[u8]| -> Vec<u8> {
        let mut e = Vec::new();
        let mut size = body.len() as u64;
        let mut hb = ((stratum_store::pack::OBJ_OFS_DELTA & 7) << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            e.push(hb | 0x80);
            hb = (size & 0x7f) as u8;
            size >>= 7;
        }
        e.push(hb);
        e.push(distance); // single-byte ofs varint (< 128)
        e.extend_from_slice(&deflate(body));
        e
    };
    let mut seg = blob.clone();
    let ofs_at = seg.len() as u64;
    seg.extend_from_slice(&ofs_entry(ofs_at as u8 + 1, &delta_body));
    let err = resolve(&seg, 1 << 20, (1 << 20) + ofs_at).err().unwrap();
    assert!(err.contains("chain leaves slice"), "{err}");

    // scan_pack walks over OFS-delta entries (skipping their distance
    // bytes) and the delta resolves against its in-slice base.
    let mut seg2 = blob.clone();
    let dist = seg2.len() as u8;
    seg2.extend_from_slice(&ofs_entry(dist, &delta_body));
    let scanned = scan_pack(&seg2, 2).unwrap();
    assert_eq!(scanned.len(), 2);
    match resolve(&seg2, 0, scanned[1].offset).unwrap() {
        Resolved::Object(t, d) => {
            assert_eq!(t, OBJ_BLOB);
            assert_eq!(d, b"0123");
        }
        _ => panic!("expected resolved delta"),
    }
}

#[test]
fn locator_header_validation() {
    // Bad magic / truncated headers / short oids are loud.
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(200, b"XXXX rest"))]);
    let store = ObjectStore::new(&fake.url, LatencyModel::None);
    let err = Plane::load(&store, "p").err().unwrap();
    assert!(err.contains("bad magic"), "{err}");

    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(200, b"SLH3\x00"))]);
    let store = ObjectStore::new(&fake.url, LatencyModel::None);
    let err = Plane::load(&store, "p").err().unwrap();
    assert!(err.contains("truncated"), "{err}");

    // Valid prefix but a directory shorter than 4097 buckets.
    let mut short = Vec::new();
    short.extend_from_slice(b"SLH3");
    short.extend_from_slice(&2u16.to_be_bytes());
    short.extend_from_slice(b"e1");
    short.extend_from_slice(&[0u8; 64]);
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(200, &short))]);
    let store = ObjectStore::new(&fake.url, LatencyModel::None);
    let err = Plane::load(&store, "p").err().unwrap();
    assert!(err.contains("truncated"), "{err}");

    assert!(Plane::parse_oid("short").is_err());
    assert!(Plane::parse_oid(&"g".repeat(40)).is_err());
}

/// A header with distinguishable buckets, so a round trip that silently
/// dropped or reordered the directory cannot pass.
fn sample(generation: Option<u32>, data_prefix: Option<&str>) -> LocatorHeader {
    LocatorHeader {
        epoch: "20250826T101500-4018".to_string(),
        generation,
        data_prefix: data_prefix.map(str::to_string),
        records: 12_345,
        n_cold: 7,
        buckets: (0..4097).map(|b| b as u64 * 150).collect(),
    }
}

#[test]
fn locator_header_round_trips_every_magic() {
    // SLH2 (legacy, no generation), SLH3 (generation), SLH4 (generation
    // + absolute data prefix). The magic follows the content, so a
    // header that decodes and re-encodes must be byte-identical — that
    // is what lets a fork copy upstream's header and change one field.
    for (want_magic, h) in [
        (&b"SLH2"[..], sample(None, None)),
        (&b"SLH3"[..], sample(Some(3), None)),
        (
            &b"SLH4"[..],
            sample(Some(3), Some("o/1/r/2/tiered/20250826T101500-4018")),
        ),
        // SLH4 is legal without a generation on the wire too; the magic
        // is chosen by the prefix, which is the field that changes how
        // keys resolve.
        (&b"SLH4"[..], sample(None, Some("o/1/r/2/tiered/e9"))),
    ] {
        let bytes = write_header(&h);
        assert_eq!(&bytes[..4], want_magic);
        let back = parse_header(&bytes).unwrap();
        assert_eq!(h, back);
        assert_eq!(write_header(&back), bytes, "re-encode is not byte-stable");
    }
}

#[test]
fn slh2_and_slh3_decode_exactly_as_they_did() {
    // Pinned against hand-built bytes in the shape formats.md documents,
    // not against our own encoder — an encoder bug that the decoder
    // mirrored would round-trip happily and still break every existing
    // repository in the bucket.
    for (magic, generation) in [(&b"SLH2"[..], None), (&b"SLH3"[..], Some(9u32))] {
        let mut hdr = Vec::new();
        hdr.extend_from_slice(magic);
        hdr.extend_from_slice(&2u16.to_be_bytes());
        hdr.extend_from_slice(b"e1");
        if let Some(g) = generation {
            hdr.extend_from_slice(&g.to_be_bytes());
        }
        hdr.extend_from_slice(&41u64.to_be_bytes()); // records
        hdr.extend_from_slice(&5u64.to_be_bytes()); // n_cold
        for b in 0..4097u64 {
            hdr.extend_from_slice(&(b * 8).to_be_bytes());
        }
        let h = parse_header(&hdr).unwrap();
        assert_eq!(h.epoch, "e1");
        assert_eq!(h.generation, generation);
        assert_eq!(h.records, 41);
        assert_eq!(h.n_cold, 5);
        // The field that matters: a legacy header claims nothing about
        // where its data lives, so the reader's own prefix still wins.
        assert_eq!(h.data_prefix, None);
        assert_eq!(h.buckets.len(), 4097);
        assert_eq!(h.buckets[4096], 4096 * 8);
    }
}

#[test]
fn rebase_points_a_copied_header_at_another_repository() {
    // The whole of a zero-copy fork's read path. Everything except the
    // data prefix has to survive, or the fork reads the right bytes from
    // the wrong offsets.
    let upstream = write_header(&sample(Some(2), None));
    let forked = rebase_header(&upstream, "o/1/r/7/tiered/20250826T101500-4018").unwrap();

    assert_eq!(&forked[..4], b"SLH4");
    let a = parse_header(&upstream).unwrap();
    let b = parse_header(&forked).unwrap();
    assert_eq!(
        b.data_prefix.as_deref(),
        Some("o/1/r/7/tiered/20250826T101500-4018")
    );
    assert_eq!(a.epoch, b.epoch);
    assert_eq!(a.generation, b.generation);
    assert_eq!(a.records, b.records);
    assert_eq!(a.n_cold, b.n_cold);
    assert_eq!(a.buckets, b.buckets);

    // Rebasing an already-rebased header re-points it rather than
    // nesting: a fork of a fork reads from wherever the bytes really are.
    let again = rebase_header(&forked, "o/1/r/9/tiered/e2").unwrap();
    assert_eq!(
        parse_header(&again).unwrap().data_prefix.as_deref(),
        Some("o/1/r/9/tiered/e2")
    );

    // A trailing slash is the obvious way to build this string wrong,
    // and it would double every separator in every data key.
    let slashed = rebase_header(&upstream, "o/1/r/7/tiered/e1/").unwrap();
    assert_eq!(
        parse_header(&slashed).unwrap().data_prefix.as_deref(),
        Some("o/1/r/7/tiered/e1")
    );
    assert!(rebase_header(&upstream, "").is_err());
    assert!(rebase_header(&upstream, "///").is_err());
}

#[test]
fn forking_a_legacy_repository_keeps_its_legacy_file_names() {
    // The case that made SLH4 carry an explicit generation flag rather
    // than inferring one from the magic. Upstream is an old SLH2
    // repository: no generation, data files named locator.bin and
    // chains.bin. Its fork is SLH4 — the prefix is absolute — but it
    // still has no generation, and if the reader assumes one because the
    // magic is SLH4 it misreads the header by four bytes and every
    // point read goes to a garbage key.
    //
    // Refusing to fork legacy repositories would have "fixed" this too,
    // and silently: the oldest repositories on the fleet are exactly the
    // ones most likely to be forked.
    let legacy = write_header(&sample(None, None));
    assert_eq!(&legacy[..4], b"SLH2");

    let forked = rebase_header(&legacy, "o/1/r/2/tiered/e-old").unwrap();
    assert_eq!(&forked[..4], b"SLH4");
    let h = parse_header(&forked).unwrap();
    assert_eq!(h.generation, None, "a fork invented a generation");
    assert_eq!(h.data_prefix.as_deref(), Some("o/1/r/2/tiered/e-old"));
    assert_eq!(h.n_cold, 7);
    assert_eq!(h.buckets, sample(None, None).buckets);

    // And the plane names upstream's files the legacy way.
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(200, &forked))]);
    let store = ObjectStore::new(&fake.url, LatencyModel::None);
    let plane = Plane::load(&store, "fork/9/tiered").unwrap();
    assert_eq!(plane.chains_key(), "o/1/r/2/tiered/e-old/chains.bin");
    assert_eq!(plane.seg_key(0), "o/1/r/2/tiered/e-old/cold-0000.seg");
}

#[test]
fn slh4_refuses_headers_it_cannot_honour() {
    // A reader that sees a header it cannot honour must fail loudly,
    // never guess — an empty data prefix would resolve every key to the
    // bucket root, which is somebody else's data or nothing at all.
    let mut empty = Vec::new();
    empty.extend_from_slice(b"SLH4");
    empty.extend_from_slice(&2u16.to_be_bytes());
    empty.extend_from_slice(b"e1");
    empty.push(1); // generation present
    empty.extend_from_slice(&0u32.to_be_bytes());
    empty.extend_from_slice(&0u16.to_be_bytes()); // zero-length prefix
    empty.extend_from_slice(&[0u8; 16]);
    empty.extend_from_slice(&[0u8; 4097 * 8]);
    let err = parse_header(&empty).unwrap_err();
    assert!(err.contains("empty data prefix"), "{err}");

    // Truncated in the prefix itself, which is the field the old
    // fixed-offset reader had no way to run off the end of.
    let mut cut = Vec::new();
    cut.extend_from_slice(b"SLH4");
    cut.extend_from_slice(&2u16.to_be_bytes());
    cut.extend_from_slice(b"e1");
    cut.push(1);
    cut.extend_from_slice(&0u32.to_be_bytes());
    cut.extend_from_slice(&64u16.to_be_bytes()); // claims 64 bytes
    cut.extend_from_slice(b"short");
    let err = parse_header(&cut).unwrap_err();
    assert!(err.contains("truncated"), "{err}");

    // Non-UTF-8 prefix: a key we cannot even name.
    let mut bad = Vec::new();
    bad.extend_from_slice(b"SLH4");
    bad.extend_from_slice(&2u16.to_be_bytes());
    bad.extend_from_slice(b"e1");
    bad.push(1);
    bad.extend_from_slice(&0u32.to_be_bytes());
    bad.extend_from_slice(&2u16.to_be_bytes());
    bad.extend_from_slice(&[0xff, 0xfe]);
    bad.extend_from_slice(&[0u8; 16]);
    bad.extend_from_slice(&[0u8; 4097 * 8]);
    let err = parse_header(&bad).unwrap_err();
    assert!(err.contains("not UTF-8"), "{err}");

    // A generation flag that is neither 0 nor 1 means we are reading a
    // header some future writer produced, at offsets we would guess at.
    let mut flag = Vec::new();
    flag.extend_from_slice(b"SLH4");
    flag.extend_from_slice(&2u16.to_be_bytes());
    flag.extend_from_slice(b"e1");
    flag.push(7);
    flag.extend_from_slice(&[0u8; 4 + 2 + 16]);
    flag.extend_from_slice(&[0u8; 4097 * 8]);
    let err = parse_header(&flag).unwrap_err();
    assert!(err.contains("generation flag"), "{err}");

    // And the magic nobody has invented.
    let err = parse_header(b"SLH9________").unwrap_err();
    assert!(err.contains("bad magic"), "{err}");
}

#[test]
fn a_plane_loaded_from_slh4_reads_out_of_its_own_prefix() {
    // The bug this format exists to fix: a byte-copied manifest clones
    // correctly and then 404s on /files, /tree, /diff and /log, because
    // every point-read key was derived from the *caller's* prefix.
    let hdr = write_header(&sample(Some(4), Some("upstream/1/tiered/e-up")));
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(200, &hdr))]);
    let store = ObjectStore::new(&fake.url, LatencyModel::None);

    // Loaded from the *fork's* prefix, pointing at upstream's data.
    let plane = Plane::load(&store, "fork/2/tiered").unwrap();
    assert_eq!(plane.seg_key(0), "upstream/1/tiered/e-up/cold-0000.seg");
    assert_eq!(plane.seg_key(7), "upstream/1/tiered/e-up/hot-0000.seg");
    assert_eq!(
        plane.chains_key(),
        "upstream/1/tiered/e-up/chains-g0004.bin"
    );

    // A legacy header from the same prefix still resolves locally.
    let hdr = write_header(&sample(Some(4), None));
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(200, &hdr))]);
    let store = ObjectStore::new(&fake.url, LatencyModel::None);
    let plane = Plane::load(&store, "fork/2/tiered").unwrap();
    assert_eq!(
        plane.seg_key(0),
        "fork/2/tiered/20250826T101500-4018/cold-0000.seg"
    );
}

#[test]
fn gitobj_parsers_reject_malformed_input() {
    // parse_idx: bad magic, wrong version, truncated tables.
    assert!(gitobj::parse_idx(b"nope").is_err());
    let mut idx = Vec::new();
    idx.extend_from_slice(b"\xfftOc");
    idx.extend_from_slice(&3u32.to_be_bytes()); // v3: unsupported
    idx.extend_from_slice(&[0u8; 256 * 4]);
    assert!(gitobj::parse_idx(&idx).is_err());
    let mut idx = Vec::new();
    idx.extend_from_slice(b"\xfftOc");
    idx.extend_from_slice(&2u32.to_be_bytes());
    let mut fanout = [0u8; 256 * 4];
    fanout[255 * 4..].copy_from_slice(&5u32.to_be_bytes()); // claims 5 objects
    idx.extend_from_slice(&fanout);
    assert!(gitobj::parse_idx(&idx).unwrap_err().contains("truncated"));

    // A well-formed single-object idx with a large (8-byte) offset.
    let mut idx = Vec::new();
    idx.extend_from_slice(b"\xfftOc");
    idx.extend_from_slice(&2u32.to_be_bytes());
    let mut fanout = [0u8; 256 * 4];
    for i in 0..256 {
        fanout[i * 4..i * 4 + 4].copy_from_slice(&1u32.to_be_bytes());
    }
    idx.extend_from_slice(&fanout);
    idx.extend_from_slice(&[0x11u8; 20]); // oid
    idx.extend_from_slice(&[0u8; 4]); // crc
    idx.extend_from_slice(&0x8000_0000u32.to_be_bytes()); // MSB → large table 0
    idx.extend_from_slice(&(1u64 << 33).to_be_bytes()); // 8-byte offset
    let parsed = gitobj::parse_idx(&idx).unwrap();
    assert_eq!(parsed, vec![([0x11u8; 20], 1u64 << 33)]);
    // Truncate the large-offset table → loud error.
    let cut = &idx[..idx.len() - 4];
    assert!(gitobj::parse_idx(cut).unwrap_err().contains("large-offset"));

    // tag_refs: object line extracts the target; tags without one (or
    // with a short oid) are handled.
    let target = "a".repeat(40);
    let tag = format!("object {target}\ntype commit\ntag v1\n\nmsg\n");
    let refs = gitobj::tag_refs(tag.as_bytes()).unwrap();
    assert_eq!(hex(&refs[0]), target);
    assert!(gitobj::tag_refs(b"type commit\n\nbody\n")
        .unwrap()
        .is_empty());
    assert!(gitobj::tag_refs(b"object tooshort\n").is_err());
}

#[test]
fn plain_get_rejects_unexpected_success_statuses() {
    // 204 is 2xx but not the 200 a full GET requires.
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(204, b""))]);
    let store = ObjectStore::new(&fake.url, LatencyModel::None);
    let err = store.get("k/x").unwrap_err();
    assert!(err.contains("HTTP 204"), "{err}");
}

#[test]
fn sha256_manifests_are_refused_by_this_build() {
    let manifest = serde_json::json!({
        "schema": 4,
        "repo": "y",
        "layout": "prod",
        "object_format": "sha256",
        "epoch": "e1",
        "refs": [],
        "head": "",
    });
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(
        200,
        serde_json::to_vec(&manifest).unwrap().as_slice(),
    ))]);
    let store = ObjectStore::new(&fake.url, LatencyModel::None);
    let err = stratum_store::load_manifest(&store, "o/x/r/y", "prod").unwrap_err();
    assert!(err.contains("sha1 only"), "{err}");
}

#[test]
fn sigv4_includes_session_tokens_when_present() {
    // Own test process: env mutation is safe per-binary.
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret");
    std::env::set_var("AWS_REGION", "us-east-1");
    std::env::set_var("AWS_SESSION_TOKEN", "the-session-token");
    let signer = stratum_store::sig::SigV4::from_env().expect("creds set");
    let signed = signer.sign("GET", "example.com", "/k", stratum_store::sig::EMPTY_SHA256);
    let signed = signed.unwrap();
    let names: Vec<&str> = signed.headers.iter().map(|(n, _)| *n).collect();
    assert!(names.contains(&"x-amz-security-token"), "{names:?}");
    let auth = signed
        .headers
        .iter()
        .find(|(n, _)| *n == "Authorization")
        .unwrap();
    assert!(auth.1.contains("x-amz-security-token"), "{}", auth.1);
}
