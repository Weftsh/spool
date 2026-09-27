//! Just enough packfile entry parsing to resolve one object from a segment
//! byte slice: entry headers, zlib inflation, and git delta application.
//! (gitoxide provides equivalents, but the subset needed here is ~150 lines
//! and keeping it dependency-light makes the read path easy to audit.)

use flate2::read::ZlibDecoder;
use std::io::Read;

pub const OBJ_COMMIT: u8 = 1;
pub const OBJ_TREE: u8 = 2;
pub const OBJ_BLOB: u8 = 3;
pub const OBJ_TAG: u8 = 4;
pub const OBJ_OFS_DELTA: u8 = 6;
pub const OBJ_REF_DELTA: u8 = 7;

pub fn type_name(t: u8) -> &'static str {
    match t {
        OBJ_COMMIT => "commit",
        OBJ_TREE => "tree",
        OBJ_BLOB => "blob",
        OBJ_TAG => "tag",
        _ => "unknown",
    }
}

/// Parse an entry header at `pos`: (object type, inflated size, bytes used).
/// Varints are capped: corrupt data must produce an error, never a shift
/// overflow or a silently-wrapped size.
pub fn entry_header(buf: &[u8], pos: usize) -> Result<(u8, u64, usize), String> {
    let mut i = pos;
    let b = *buf.get(i).ok_or("header out of range")?;
    i += 1;
    let typ = (b >> 4) & 7;
    let mut size = (b & 0x0f) as u64;
    let mut shift = 4;
    let mut cont = b & 0x80 != 0;
    while cont {
        if shift > 60 {
            return Err(format!("entry header varint too long at {pos}"));
        }
        let b = *buf.get(i).ok_or("header out of range")?;
        i += 1;
        size |= ((b & 0x7f) as u64) << shift;
        shift += 7;
        cont = b & 0x80 != 0;
    }
    Ok((typ, size, i - pos))
}

/// Parse the OFS_DELTA negative-offset varint at `pos`.
pub fn ofs_delta_distance(buf: &[u8], pos: usize) -> Result<(u64, usize), String> {
    let mut i = pos;
    let mut b = *buf.get(i).ok_or("ofs out of range")?;
    i += 1;
    let mut value = (b & 0x7f) as u64;
    while b & 0x80 != 0 {
        if i - pos > 9 {
            return Err(format!("ofs varint too long at {pos}"));
        }
        b = *buf.get(i).ok_or("ofs out of range")?;
        i += 1;
        value = value
            .checked_add(1)
            .and_then(|v| v.checked_shl(7))
            .map(|v| v | (b & 0x7f) as u64)
            .ok_or("ofs varint overflow")?;
    }
    Ok((value, i - pos))
}

/// Inflate the zlib stream starting at `pos`, expecting `size` bytes out.
/// The read is bounded at size+1 so a corrupt header can't drive an
/// unbounded allocation before the length check fires.
pub fn inflate(buf: &[u8], pos: usize, size: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(size.min(64 * 1024 * 1024) as usize);
    ZlibDecoder::new(&buf[pos..])
        .take(size + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("inflate at {pos}: {e}"))?;
    if out.len() as u64 != size {
        return Err(format!("inflated {} bytes, header says {size}", out.len()));
    }
    Ok(out)
}

fn delta_size(delta: &[u8], i: &mut usize) -> Result<u64, String> {
    let mut size = 0u64;
    let mut shift = 0;
    loop {
        if shift > 60 {
            return Err("delta size varint too long".into());
        }
        let b = *delta.get(*i).ok_or("delta size out of range")?;
        *i += 1;
        size |= ((b & 0x7f) as u64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            return Ok(size);
        }
    }
}

/// Apply a git delta to `base`.
pub fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, String> {
    let mut i = 0;
    let base_size = delta_size(delta, &mut i)?;
    if base_size != base.len() as u64 {
        return Err(format!("delta base size {base_size} != {}", base.len()));
    }
    let result_size = delta_size(delta, &mut i)?;
    let mut out = Vec::with_capacity(result_size as usize);
    while i < delta.len() {
        let op = delta[i];
        i += 1;
        if op & 0x80 != 0 {
            // copy from base — operand bytes bounds-checked so a truncated
            // delta errors instead of panicking
            let mut off = 0u64;
            let mut len = 0u64;
            for bit in 0..4 {
                if op & (1 << bit) != 0 {
                    off |= (*delta.get(i).ok_or("truncated copy operand")? as u64) << (8 * bit);
                    i += 1;
                }
            }
            for bit in 0..3 {
                if op & (1 << (4 + bit)) != 0 {
                    len |= (*delta.get(i).ok_or("truncated copy operand")? as u64) << (8 * bit);
                    i += 1;
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            let (off, len) = (off as usize, len as usize);
            out.extend_from_slice(base.get(off..off + len).ok_or("copy out of range")?);
        } else if op != 0 {
            // insert literal
            let len = op as usize;
            out.extend_from_slice(delta.get(i..i + len).ok_or("insert out of range")?);
            i += len;
        } else {
            return Err("delta opcode 0".into());
        }
    }
    if out.len() as u64 != result_size {
        return Err(format!(
            "delta produced {} bytes, expected {result_size}",
            out.len()
        ));
    }
    Ok(out)
}

/// Resolution outcome for an entry inside a fetched slice.
pub enum Resolved {
    Object(u8, Vec<u8>),
    /// The chain bottoms out at a REF_DELTA to an object outside the slice:
    /// (base oid hex, deltas to apply bottom-up once the base is fetched).
    External(String, Vec<Vec<u8>>),
}

/// Resolve the entry at absolute segment offset `abs`, given `slice` which
/// covers absolute offsets [slice_base, slice_base + slice.len()).
pub fn resolve(slice: &[u8], slice_base: u64, abs: u64) -> Result<Resolved, String> {
    let pos = (abs - slice_base) as usize;
    let (typ, size, used) = entry_header(slice, pos)?;
    match typ {
        OBJ_OFS_DELTA => {
            let (dist, ofs_used) = ofs_delta_distance(slice, pos + used)?;
            let delta = inflate(slice, pos + used + ofs_used, size)?;
            let base_abs = abs.checked_sub(dist).ok_or("ofs base before segment")?;
            if base_abs < slice_base {
                return Err(format!(
                    "chain leaves slice: base at {base_abs} < {slice_base}"
                ));
            }
            match resolve(slice, slice_base, base_abs)? {
                Resolved::Object(bt, base) => Ok(Resolved::Object(bt, apply_delta(&base, &delta)?)),
                Resolved::External(oid, mut stack) => {
                    stack.push(delta);
                    Ok(Resolved::External(oid, stack))
                }
            }
        }
        OBJ_REF_DELTA => {
            let oid_bytes = slice
                .get(pos + used..pos + used + 20)
                .ok_or("ref oid out of range")?;
            let oid = hex(oid_bytes);
            let delta = inflate(slice, pos + used + 20, size)?;
            Ok(Resolved::External(oid, vec![delta]))
        }
        OBJ_COMMIT | OBJ_TREE | OBJ_BLOB | OBJ_TAG => {
            Ok(Resolved::Object(typ, inflate(slice, pos + used, size)?))
        }
        other => Err(format!("unexpected entry type {other} at {abs}")),
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One entry found by `scan_pack`.
pub struct ScannedEntry {
    pub offset: u64,
    pub typ: u8,
    /// REF_DELTA base oid, if this entry is one.
    pub ref_base: Option<[u8; 20]>,
}

/// Walk a pack *payload* (no 12-byte header, no trailer) entry by entry,
/// without resolving deltas: parse each header, then step over the zlib
/// stream using a streaming inflater to learn its compressed length.
/// Yields every entry's offset/type and REF_DELTA base oids — the write
/// path uses this to prefetch thin bases before quarantine verification.
pub fn scan_pack(payload: &[u8], expect_entries: u64) -> Result<Vec<ScannedEntry>, String> {
    use flate2::{Decompress, FlushDecompress, Status};
    let mut out = Vec::new();
    let mut pos: usize = 0;
    let mut scratch = vec![0u8; 64 * 1024];
    for _ in 0..expect_entries {
        let offset = pos as u64;
        let (typ, _size, used) = entry_header(payload, pos)?;
        pos += used;
        let mut ref_base = None;
        match typ {
            OBJ_OFS_DELTA => {
                let (_, ofs_used) = ofs_delta_distance(payload, pos)?;
                pos += ofs_used;
            }
            OBJ_REF_DELTA => {
                let oid: [u8; 20] = payload
                    .get(pos..pos + 20)
                    .ok_or("ref base out of range")?
                    .try_into()
                    .unwrap();
                ref_base = Some(oid);
                pos += 20;
            }
            OBJ_COMMIT | OBJ_TREE | OBJ_BLOB | OBJ_TAG => {}
            other => return Err(format!("unexpected entry type {other} at {offset}")),
        }
        // Step over the zlib stream, discarding output.
        let mut z = Decompress::new(true);
        loop {
            let input = payload
                .get(pos + z.total_in() as usize..)
                .ok_or("truncated zlib")?;
            let before_out = z.total_out();
            let status = z
                .decompress(input, &mut scratch, FlushDecompress::None)
                .map_err(|e| format!("zlib at {offset}: {e}"))?;
            match status {
                Status::StreamEnd => break,
                Status::Ok | Status::BufError => {
                    if z.total_out() == before_out && input.is_empty() {
                        return Err(format!("truncated zlib stream at {offset}"));
                    }
                }
            }
        }
        pos += z.total_in() as usize;
        out.push(ScannedEntry {
            offset,
            typ,
            ref_base,
        });
    }
    if pos != payload.len() {
        return Err(format!(
            "pack scan ended at {pos}, payload is {} bytes",
            payload.len()
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write as _;

    fn deflate(data: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// Build a full (non-delta) pack entry: header + zlib payload.
    fn full_entry(typ: u8, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut size = data.len() as u64;
        let mut b = (typ << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            out.push(b | 0x80);
            b = (size & 0x7f) as u8;
            size >>= 7;
        }
        out.push(b);
        out.extend_from_slice(&deflate(data));
        out
    }

    #[test]
    fn entry_header_roundtrip() {
        for (typ, len) in [
            (OBJ_BLOB, 0usize),
            (OBJ_TREE, 15),
            (OBJ_COMMIT, 16),
            (OBJ_BLOB, 300_000),
        ] {
            let e = full_entry(typ, &vec![b'x'; len]);
            let (t, s, _) = entry_header(&e, 0).unwrap();
            assert_eq!((t, s), (typ, len as u64));
        }
    }

    #[test]
    fn entry_header_rejects_runaway_varint() {
        // continuation bit set forever
        let e = vec![0xff; 32];
        assert!(entry_header(&e, 0).is_err());
    }

    #[test]
    fn ofs_varint_rejects_overflow() {
        let mut e = vec![0x66]; // OFS_DELTA header (type 6), size 6, no cont.
        e.extend_from_slice(&[0xff; 16]); // runaway offset varint
        let (typ, _, used) = entry_header(&e, 0).unwrap();
        assert_eq!(typ, OBJ_OFS_DELTA);
        assert!(ofs_delta_distance(&e, used).is_err());
    }

    #[test]
    fn inflate_rejects_size_mismatch() {
        let z = deflate(b"hello world");
        // header claims 5 bytes; stream inflates to 11 -> must error, and
        // must not read unboundedly.
        assert!(inflate(&z, 0, 5).is_err());
        assert_eq!(inflate(&z, 0, 11).unwrap(), b"hello world");
    }

    #[test]
    fn delta_apply_roundtrip() {
        // delta: base "abcdef" -> "abcXYZef" (copy 3, insert 3, copy 2 @4)
        let base = b"abcdef";
        let mut d = vec![6, 8]; // base size, result size (single-byte varints)
        d.extend_from_slice(&[0x90, 3]); // copy off=0(implicit) len=3
        d.extend_from_slice(&[3, b'X', b'Y', b'Z']); // insert 3
        d.extend_from_slice(&[0x91, 4, 2]); // copy off=4 len=2
        assert_eq!(apply_delta(base, &d).unwrap(), b"abcXYZef");
    }

    #[test]
    fn delta_apply_rejects_truncation_and_bad_sizes() {
        let base = b"abcdef";
        assert!(apply_delta(base, &[6, 8, 0x91]).is_err()); // missing operands
        assert!(apply_delta(base, &[5, 8]).is_err()); // wrong base size
        assert!(apply_delta(base, &[6, 9, 0x90, 3]).is_err()); // short result
        assert!(apply_delta(base, &[6, 3, 0x90, 6, 0]).is_err()); // opcode 0
    }

    #[test]
    fn scan_pack_walks_entries_and_finds_ref_bases() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&full_entry(OBJ_BLOB, b"hello"));
        // a REF_DELTA entry
        let delta = vec![5, 5, 0x90, 5]; // base 5, result 5, copy all
        let mut size = delta.len() as u64;
        let mut hb = (OBJ_REF_DELTA << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            payload.push(hb | 0x80);
            hb = (size & 0x7f) as u8;
            size >>= 7;
        }
        payload.push(hb);
        payload.extend_from_slice(&[0x42; 20]);
        payload.extend_from_slice(&deflate(&delta));
        payload.extend_from_slice(&full_entry(OBJ_TREE, b""));

        let entries = scan_pack(&payload, 3).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].typ, OBJ_BLOB);
        assert_eq!(entries[1].ref_base, Some([0x42; 20]));
        assert_eq!(entries[2].typ, OBJ_TREE);
        // wrong count and truncation both error
        assert!(scan_pack(&payload, 2).is_err());
        assert!(scan_pack(&payload[..payload.len() - 3], 3).is_err());
    }

    #[test]
    fn resolve_full_ofs_and_ref_chains() {
        // entry A (full blob) at 0, entry B = OFS delta onto A.
        let a = full_entry(OBJ_BLOB, b"abcdef");
        let b_off = a.len() as u64;
        let mut b_entry = Vec::new();
        let delta: Vec<u8> = {
            let mut d = vec![6, 8];
            d.extend_from_slice(&[0x90, 3, 3, b'X', b'Y', b'Z', 0x91, 4, 2]);
            d
        };
        let mut size = delta.len() as u64;
        let mut hb = (OBJ_OFS_DELTA << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            b_entry.push(hb | 0x80);
            hb = (size & 0x7f) as u8;
            size >>= 7;
        }
        b_entry.push(hb);
        b_entry.push(b_off as u8); // ofs distance (< 128)
        b_entry.extend_from_slice(&deflate(&delta));

        let mut seg = a.clone();
        seg.extend_from_slice(&b_entry);
        match resolve(&seg, 0, 0).unwrap() {
            Resolved::Object(t, d) => assert_eq!((t, d.as_slice()), (OBJ_BLOB, &b"abcdef"[..])),
            _ => panic!("expected object"),
        }
        match resolve(&seg, 0, b_off).unwrap() {
            Resolved::Object(t, d) => assert_eq!((t, d.as_slice()), (OBJ_BLOB, &b"abcXYZef"[..])),
            _ => panic!("expected resolved delta"),
        }

        // REF delta returns External with the base oid and one delta.
        let oid = [0xabu8; 20];
        let mut r = Vec::new();
        let mut size = delta.len() as u64;
        let mut hb = (OBJ_REF_DELTA << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            r.push(hb | 0x80);
            hb = (size & 0x7f) as u8;
            size >>= 7;
        }
        r.push(hb);
        r.extend_from_slice(&oid);
        r.extend_from_slice(&deflate(&delta));
        match resolve(&r, 0, 0).unwrap() {
            Resolved::External(o, stack) => {
                assert_eq!(o, hex(&oid));
                assert_eq!(stack.len(), 1);
            }
            _ => panic!("expected external"),
        }
    }
}
