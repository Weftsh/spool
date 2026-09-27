//! Minimal git object introspection for the write path: pack .idx parsing
//! (oid ↔ offset) and extraction of the OIDs a commit/tree/tag references,
//! used by the push connectivity check.

/// Parse a v2 pack .idx: -> sorted (oid, pack offset) pairs.
pub fn parse_idx(idx: &[u8]) -> Result<Vec<([u8; 20], u64)>, String> {
    if idx.len() < 8 + 256 * 4 || &idx[..4] != b"\xfftOc" {
        return Err("bad idx magic".into());
    }
    if u32::from_be_bytes(idx[4..8].try_into().unwrap()) != 2 {
        return Err("only idx v2 supported".into());
    }
    let n = u32::from_be_bytes(idx[8 + 255 * 4..8 + 256 * 4].try_into().unwrap()) as usize;
    let oids_at = 8 + 256 * 4;
    let offs_at = oids_at + n * 20 + n * 4; // skip crc table
    let large_at = offs_at + n * 4;
    if idx.len() < large_at {
        return Err("idx truncated".into());
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut oid = [0u8; 20];
        oid.copy_from_slice(&idx[oids_at + i * 20..oids_at + (i + 1) * 20]);
        let raw = u32::from_be_bytes(
            idx[offs_at + i * 4..offs_at + (i + 1) * 4]
                .try_into()
                .unwrap(),
        );
        let off = if raw & 0x8000_0000 != 0 {
            let j = (raw & 0x7fff_ffff) as usize;
            let at = large_at + j * 8;
            if idx.len() < at + 8 {
                return Err("idx large-offset table truncated".into());
            }
            u64::from_be_bytes(idx[at..at + 8].try_into().unwrap())
        } else {
            raw as u64
        };
        out.push((oid, off));
    }
    Ok(out)
}

/// OIDs referenced by a commit object: tree + parents.
pub fn commit_refs(body: &[u8]) -> Result<Vec<[u8; 20]>, String> {
    let mut out = Vec::new();
    for line in body.split(|&b| b == b'\n') {
        if line.is_empty() {
            break; // header/message boundary
        }
        let hexpart = if let Some(r) = line.strip_prefix(b"tree ") {
            r
        } else if let Some(r) = line.strip_prefix(b"parent ") {
            r
        } else {
            continue;
        };
        out.push(parse_hex20(hexpart)?);
    }
    Ok(out)
}

/// OIDs referenced by a tree object (every entry).
pub fn tree_refs(body: &[u8]) -> Result<Vec<[u8; 20]>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let nul = body[i..]
            .iter()
            .position(|&b| b == 0)
            .ok_or("tree entry missing NUL")?;
        let at = i + nul + 1;
        if body.len() < at + 20 {
            return Err("tree entry truncated".into());
        }
        // **Gitlinks are not our objects.** A `160000` entry is a
        // submodule: it names a commit that lives in the submodule's own
        // repository and is never present here, by design.
        //
        // Every entry used to be returned regardless of mode, so the
        // receive path's connectivity walk queued the gitlink and then
        // refused the whole push with "missing object <sha> (push
        // incomplete)". A repository with a submodule in it simply could
        // not be pushed, and the message pointed at an object nobody
        // could supply.
        //
        // The mode is the bytes before the first space of the entry.
        let is_gitlink = body[i..i + nul]
            .split(|&b| b == b' ')
            .next()
            .map(|m| m == b"160000")
            .unwrap_or(false);
        if !is_gitlink {
            let mut oid = [0u8; 20];
            oid.copy_from_slice(&body[at..at + 20]);
            out.push(oid);
        }
        i = at + 20;
    }
    Ok(out)
}

/// The object a tag points at.
pub fn tag_refs(body: &[u8]) -> Result<Vec<[u8; 20]>, String> {
    for line in body.split(|&b| b == b'\n') {
        if let Some(r) = line.strip_prefix(b"object ") {
            return Ok(vec![parse_hex20(r)?]);
        }
        if line.is_empty() {
            break;
        }
    }
    Ok(vec![])
}

fn parse_hex20(h: &[u8]) -> Result<[u8; 20], String> {
    if h.len() < 40 {
        return Err("short oid".into());
    }
    let s = std::str::from_utf8(&h[..40]).map_err(|_| "bad oid utf8")?;
    let mut out = [0u8; 20];
    for i in 0..20 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| "bad oid hex")?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_and_tag_refs_parse() {
        let c = b"tree aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
parent bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
parent cccccccccccccccccccccccccccccccccccccccc\n\
author a <a@a> 0 +0000\n\ncommit msg with tree deadbeef inside\n";
        let refs = commit_refs(c).unwrap();
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0], [0xaa; 20]);
        assert_eq!(refs[1], [0xbb; 20]);

        let t = b"object dddddddddddddddddddddddddddddddddddddddd\ntype commit\n\nmsg\n";
        assert_eq!(tag_refs(t).unwrap(), vec![[0xdd; 20]]);
    }

    #[test]
    fn tree_refs_parse() {
        let mut t = Vec::new();
        t.extend_from_slice(b"100644 file.txt\0");
        t.extend_from_slice(&[0x11; 20]);
        t.extend_from_slice(b"40000 dir\0");
        t.extend_from_slice(&[0x22; 20]);
        let refs = tree_refs(&t).unwrap();
        assert_eq!(refs, vec![[0x11; 20], [0x22; 20]]);
        assert!(tree_refs(&t[..t.len() - 1]).is_err());
    }

    /// A `160000` entry names a commit in **another** repository.
    ///
    /// Returning it made the receive path's connectivity walk demand an
    /// object nobody could supply, and every push of a repository with a
    /// submodule in it was refused with "missing object … (push
    /// incomplete)".
    ///
    /// The modes either side are asserted in the same tree, because the
    /// bug to guard against now is the opposite one: a prefix match that
    /// is too eager and drops a real entry. `100644` and `120000` are
    /// the ordinary file and the symlink; both are ours and both must
    /// still come back.
    #[test]
    fn tree_refs_skips_gitlinks_and_keeps_everything_else() {
        let mut t = Vec::new();
        t.extend_from_slice(b"100644 file.txt\0");
        t.extend_from_slice(&[0x11; 20]);
        t.extend_from_slice(b"160000 vendor/lib\0");
        t.extend_from_slice(&[0x99; 20]);
        t.extend_from_slice(b"120000 link\0");
        t.extend_from_slice(&[0x33; 20]);
        t.extend_from_slice(b"40000 dir\0");
        t.extend_from_slice(&[0x22; 20]);
        assert_eq!(
            tree_refs(&t).unwrap(),
            vec![[0x11; 20], [0x33; 20], [0x22; 20]],
            "a gitlink was walked as one of our objects, or a real entry \
             was dropped with it"
        );

        // A name that merely starts with the digits is still an entry.
        let mut t = Vec::new();
        t.extend_from_slice(b"100644 160000.txt\0");
        t.extend_from_slice(&[0x44; 20]);
        assert_eq!(tree_refs(&t).unwrap(), vec![[0x44; 20]]);
    }
}
