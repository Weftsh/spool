//! Pure-Rust git object construction for the Repos commit API (R2): no git
//! subprocess on the REST hot path. Blobs, canonical trees, commits, tags,
//! and a minimal non-delta pack writer whose output stock git accepts.
//!
//! Brutally simple by design — delta search stays offline where the engine
//! does it (H3); a REST commit's handful of objects ships full.

use sha1::{Digest, Sha1};

pub const OBJ_COMMIT: u8 = 1;
pub const OBJ_TREE: u8 = 2;
pub const OBJ_BLOB: u8 = 3;
pub const OBJ_TAG: u8 = 4;

pub fn kind_name(kind: u8) -> &'static str {
    stratum_store::pack::type_name(kind)
}

/// SHA-1 of a git object: `"<kind> <len>\0" + payload`.
pub fn hash_object(kind: u8, data: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(format!("{} {}\0", kind_name(kind), data.len()).as_bytes());
    h.update(data);
    h.finalize().into()
}

#[derive(Debug, Clone, PartialEq)]
pub struct TreeEntry {
    /// "100644", "100755", "120000", "40000" (tree), "160000" (gitlink).
    pub mode: String,
    pub name: String,
    pub oid: [u8; 20],
}

/// Canonical tree encoding. git sorts entries by name with directories
/// compared as `name + "/"` — getting this wrong changes every tree hash.
pub fn encode_tree(entries: &mut [TreeEntry]) -> Vec<u8> {
    entries.sort_by_key(sort_key);
    let mut out = Vec::new();
    for e in entries.iter() {
        out.extend_from_slice(e.mode.as_bytes());
        out.push(b' ');
        out.extend_from_slice(e.name.as_bytes());
        out.push(0);
        out.extend_from_slice(&e.oid);
    }
    out
}

fn sort_key(e: &TreeEntry) -> Vec<u8> {
    let mut k = e.name.clone().into_bytes();
    if e.mode == "40000" || e.mode == "040000" {
        k.push(b'/');
    }
    k
}

/// Parse a tree payload into entries.
pub fn parse_tree(data: &[u8]) -> Result<Vec<TreeEntry>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let sp = data[i..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or("tree: no mode terminator")?
            + i;
        let mode = std::str::from_utf8(&data[i..sp])
            .map_err(|_| "tree: bad mode")?
            .to_string();
        let nul = data[sp + 1..]
            .iter()
            .position(|&b| b == 0)
            .ok_or("tree: no name terminator")?
            + sp
            + 1;
        let name = String::from_utf8(data[sp + 1..nul].to_vec()).map_err(|_| "tree: bad name")?;
        let oid: [u8; 20] = data
            .get(nul + 1..nul + 21)
            .ok_or("tree: truncated oid")?
            .try_into()
            .unwrap();
        out.push(TreeEntry { mode, name, oid });
        i = nul + 21;
    }
    Ok(out)
}

pub struct CommitInfo {
    pub tree: [u8; 20],
    pub parents: Vec<[u8; 20]>,
    /// "Name <email>"
    pub author: String,
    pub committer: String,
    /// Unix seconds + offset, e.g. "+0000".
    pub timestamp: i64,
    pub message: String,
}

pub fn encode_commit(c: &CommitInfo) -> Vec<u8> {
    let mut s = format!("tree {}\n", hex(&c.tree));
    for p in &c.parents {
        s.push_str(&format!("parent {}\n", hex(p)));
    }
    s.push_str(&format!("author {} {} +0000\n", c.author, c.timestamp));
    s.push_str(&format!(
        "committer {} {} +0000\n",
        c.committer, c.timestamp
    ));
    s.push('\n');
    s.push_str(&c.message);
    if !c.message.ends_with('\n') {
        s.push('\n');
    }
    s.into_bytes()
}

/// Parse the headers of a commit payload.
pub struct ParsedCommit {
    pub tree: String,
    pub parents: Vec<String>,
    pub author: String,
    pub committer: String,
    pub message: String,
}

/// The object an annotated tag points at — its `object` header — or
/// `None` for anything that is not shaped like a tag. A walk over ref
/// tips that treats every tip as a commit dies on the first release tag
/// it meets; this is the one line it needs to step through instead.
pub fn parse_tag_target(data: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    let (headers, _) = text.split_once("\n\n").unwrap_or((text.as_ref(), ""));
    headers
        .lines()
        .find_map(|l| l.strip_prefix("object "))
        .map(str::to_string)
}

pub fn parse_commit(data: &[u8]) -> Result<ParsedCommit, String> {
    let text = String::from_utf8_lossy(data);
    let (headers, message) = text.split_once("\n\n").unwrap_or((text.as_ref(), ""));
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = String::new();
    let mut committer = String::new();
    for line in headers.lines() {
        if let Some(v) = line.strip_prefix("tree ") {
            tree = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("parent ") {
            parents.push(v.to_string());
        } else if let Some(v) = line.strip_prefix("author ") {
            author = v.to_string();
        } else if let Some(v) = line.strip_prefix("committer ") {
            committer = v.to_string();
        }
    }
    Ok(ParsedCommit {
        tree: tree.ok_or("commit without tree")?,
        parents,
        author,
        committer,
        message: message.to_string(),
    })
}

/// A new object headed for a pack.
pub struct NewObject {
    pub kind: u8,
    pub data: Vec<u8>,
    pub oid: [u8; 20],
}

pub fn new_object(kind: u8, data: Vec<u8>) -> NewObject {
    let oid = hash_object(kind, &data);
    NewObject { kind, data, oid }
}

/// Build a v2 pack *payload* (header/trailer stripped — the layout's
/// segment format) of full, zlib-deflated entries. Returns (payload,
/// entries). Objects must be pre-deduplicated by the caller.
pub fn build_pack_payload(objects: &[NewObject]) -> Result<(Vec<u8>, u64), String> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut payload = Vec::new();
    for o in objects {
        // Entry header: type in bits 4-6 of the first byte, size varint.
        let mut size = o.data.len() as u64;
        let mut byte = ((o.kind & 7) << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            payload.push(byte | 0x80);
            byte = (size & 0x7f) as u8;
            size >>= 7;
        }
        payload.push(byte);
        let mut enc = ZlibEncoder::new(&mut payload, Compression::default());
        enc.write_all(&o.data).map_err(|e| e.to_string())?;
        enc.finish().map_err(|e| e.to_string())?;
    }
    Ok((payload, objects.len() as u64))
}

/// A complete pack (header + payload + trailer) — what `git index-pack`
/// or a bundle wants.
pub fn seal_pack(payload: &[u8], entries: u64) -> Vec<u8> {
    let mut pack = Vec::with_capacity(payload.len() + 32);
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&(entries as u32).to_be_bytes());
    pack.extend_from_slice(payload);
    let d = Sha1::digest(&pack);
    pack.extend_from_slice(&d);
    pack
}

pub fn hex(oid: &[u8; 20]) -> String {
    stratum_store::pack::hex(oid)
}

pub fn parse_hex(oid: &str) -> Result<[u8; 20], String> {
    stratum_store::Plane::parse_oid(oid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::{Command, Stdio};

    fn git_out(dir: &Path, args: &[&str], stdin: &[u8]) -> String {
        let mut c = Command::new("git");
        c.arg("-C").arg(dir).args(args);
        c.env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@x")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@x");
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn scratch() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "objwrite-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        let st = Command::new("git")
            .args(["init", "-q"])
            .arg(&p)
            .status()
            .unwrap();
        assert!(st.success());
        p
    }

    #[test]
    fn blob_hash_matches_git() {
        let dir = scratch();
        let data = b"hello, stratum\n";
        let ours = hex(&hash_object(OBJ_BLOB, data));
        let theirs = git_out(&dir, &["hash-object", "--stdin"], data);
        assert_eq!(ours, theirs);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tree_encoding_matches_git_mktree_incl_dir_sort_rule() {
        let dir = scratch();
        let blob = hash_object(OBJ_BLOB, b"x");
        git_out(&dir, &["hash-object", "-w", "--stdin"], b"x");
        // Names chosen so plain sort and git's dir-aware sort disagree:
        // "sub-x" < "sub/" plain, but "sub/" sorts as "sub/" vs "sub-x".
        let sub_tree_ours = {
            let mut entries = vec![TreeEntry {
                mode: "100644".into(),
                name: "f".into(),
                oid: blob,
            }];
            encode_tree(&mut entries)
        };
        let sub_oid = hash_object(OBJ_TREE, &sub_tree_ours);
        let sub_hex = git_out(
            &dir,
            &["mktree"],
            format!("100644 blob {}\tf\n", hex(&blob)).as_bytes(),
        );
        assert_eq!(hex(&sub_oid), sub_hex);

        let mut entries = vec![
            TreeEntry {
                mode: "100644".into(),
                name: "sub-x".into(),
                oid: blob,
            },
            TreeEntry {
                mode: "40000".into(),
                name: "sub".into(),
                oid: sub_oid,
            },
            TreeEntry {
                mode: "100644".into(),
                name: "a.txt".into(),
                oid: blob,
            },
        ];
        let ours = hex(&hash_object(OBJ_TREE, &encode_tree(&mut entries)));
        let theirs = git_out(
            &dir,
            &["mktree"],
            format!(
                "100644 blob {b}\ta.txt\n040000 tree {t}\tsub\n100644 blob {b}\tsub-x\n",
                b = hex(&blob),
                t = sub_hex
            )
            .as_bytes(),
        );
        assert_eq!(ours, theirs);
        // Round-trip parse.
        let parsed = parse_tree(&encode_tree(&mut entries)).unwrap();
        assert_eq!(parsed.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_hash_matches_git_commit_tree() {
        let dir = scratch();
        git_out(&dir, &["hash-object", "-w", "--stdin"], b"x");
        let blob = hash_object(OBJ_BLOB, b"x");
        let mut entries = vec![TreeEntry {
            mode: "100644".into(),
            name: "f".into(),
            oid: blob,
        }];
        let tree_bytes = encode_tree(&mut entries);
        let tree = hash_object(OBJ_TREE, &tree_bytes);
        git_out(
            &dir,
            &["mktree"],
            format!("100644 blob {}\tf\n", hex(&blob)).as_bytes(),
        );

        let info = CommitInfo {
            tree,
            parents: vec![],
            author: "T <t@x>".into(),
            committer: "T <t@x>".into(),
            timestamp: 1_700_000_000,
            message: "test commit".into(),
        };
        let ours = hex(&hash_object(OBJ_COMMIT, &encode_commit(&info)));
        let theirs = {
            let mut c = Command::new("git");
            c.arg("-C").arg(&dir).args(["commit-tree", &hex(&tree)]);
            c.env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@x")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@x")
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .env("GIT_COMMITTER_DATE", "1700000000 +0000");
            let mut child = c
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"test commit\n")
                .unwrap();
            let out = child.wait_with_output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_eq!(ours, theirs);
        let parsed = parse_commit(&encode_commit(&info)).unwrap();
        assert_eq!(parsed.tree, hex(&tree));
        assert!(parsed.message.contains("test commit"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pack_payload_indexes_cleanly() {
        let dir = scratch();
        let objs = vec![
            new_object(OBJ_BLOB, b"one".to_vec()),
            new_object(OBJ_BLOB, vec![7u8; 100_000]), // multi-byte size varint
        ];
        let (payload, n) = build_pack_payload(&objs).unwrap();
        let pack = seal_pack(&payload, n);
        let mut c = Command::new("git");
        c.arg("-C").arg(&dir).args(["index-pack", "--stdin"]);
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(&pack).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Objects readable back by git under their computed ids.
        for o in &objs {
            git_out(&dir, &["cat-file", "-e", &hex(&o.oid)], b"");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
