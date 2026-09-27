//! Sharded ref store (gap 1 of the scaling analysis): refs live in
//! sorted, non-overlapping name-range pages — immutable content-addressed
//! objects under the epoch — so the manifest stays small at any ref count,
//! a push rewrites one page (O(page), not O(refs)), and `ls-refs` with
//! ref-prefixes loads only the overlapping pages.
//!
//! Page format: sorted "oid<SP>refname<LF>" lines. Page objects are named
//! by content hash, so a rewrite is a new key and the manifest CAS is the
//! only commit point (invariants I7/I9 hold unchanged).

use crate::manifest::{Manifest, RefPage};
use crate::store::{ObjectStore, PutCond};
use sha2::{Digest, Sha256};

pub const DEFAULT_PAGE_MAX: usize = 1000;

/// Max refs per page before a split. Env-tunable so tests exercise splits
/// with small ref counts.
pub fn page_max() -> usize {
    std::env::var("STRATUM_REF_PAGE_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n >= 2)
        .unwrap_or(DEFAULT_PAGE_MAX)
}

/// Parse a page into (name, oid) entries, sorted by name.
pub fn parse_page(data: &[u8]) -> Result<Vec<(String, String)>, String> {
    let text = std::str::from_utf8(data).map_err(|_| "ref page: not utf-8")?;
    let mut out = Vec::new();
    for line in text.lines() {
        let (oid, name) = line.split_once(' ').ok_or("ref page: malformed line")?;
        if oid.len() != 40 || name.is_empty() {
            return Err("ref page: malformed entry".into());
        }
        out.push((name.to_string(), oid.to_string()));
    }
    Ok(out)
}

pub fn encode_page(entries: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, oid) in entries {
        out.extend_from_slice(oid.as_bytes());
        out.push(b' ');
        out.extend_from_slice(name.as_bytes());
        out.push(b'\n');
    }
    out
}

/// The content address of a page body: the hex sha256 that names it.
/// Both minting sites (`put_page` here and the ingest builder) go through
/// this shape, so it is also what `verify_page` checks a body against.
pub fn page_digest(body: &[u8]) -> String {
    crate::pack::hex(&Sha256::digest(body))
}

// STRATUM-CORE DIVERGENCE: the research code trusted whatever bytes came
// back for a page key. A short read that happens to land on a line
// boundary parses cleanly and yields *fewer refs*, and since `update`
// loads a page, mutates it and splices the result into
// `manifest.ref_pages`, the next manifest CAS makes the lost refs
// permanent — the manifest is the only ref truth (I9), so nothing
// downstream can notice. Pages are content-addressed, so the body can be
// checked against the hash in its own key for the cost of one sha256;
// `count` and `bytes` are free and say more precisely what went wrong.
fn verify_page(page: &RefPage, body: &[u8]) -> Result<Vec<(String, String)>, String> {
    if body.len() as u64 != page.bytes {
        return Err(format!(
            "ref page {}: {} bytes, manifest says {}",
            page.key,
            body.len(),
            page.bytes
        ));
    }
    // Keys minted anywhere in this workspace are "page-<sha256hex>.txt".
    // A key of any other shape is not something we can content-check, so
    // it keeps the length/count checks rather than being refused outright:
    // failing closed here would take out a repository over a key shape no
    // producer in the tree emits.
    if let Some(expect) = page
        .key
        .rsplit('/')
        .next()
        .and_then(|f| f.strip_prefix("page-"))
        .and_then(|f| f.strip_suffix(".txt"))
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        let actual = page_digest(body);
        if actual != expect {
            return Err(format!(
                "ref page {}: content hash {actual}, key says {expect}",
                page.key
            ));
        }
    }
    let entries = parse_page(body)?;
    if entries.len() as u64 != page.count {
        return Err(format!(
            "ref page {}: {} refs, manifest says {}",
            page.key,
            entries.len(),
            page.count
        ));
    }
    Ok(entries)
}

pub fn load_page(store: &ObjectStore, page: &RefPage) -> Result<Vec<(String, String)>, String> {
    verify_page(page, &store.get(&page.key)?)
}

/// Resolve one ref name through the page store.
pub fn lookup(
    store: &ObjectStore,
    manifest: &Manifest,
    name: &str,
) -> Result<Option<String>, String> {
    let Some(idx) = manifest.page_index(name) else {
        return Ok(None);
    };
    Ok(load_page(store, &manifest.ref_pages[idx])?
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, oid)| oid))
}

fn put_page(
    store: &ObjectStore,
    data_prefix: &str,
    entries: &[(String, String)],
) -> Result<RefPage, String> {
    let body = encode_page(entries);
    let key = format!("{data_prefix}/refs/page-{}.txt", page_digest(&body));
    store
        .put(&key, &body, PutCond::None) // content-addressed: idempotent
        .map_err(|e| e.to_string())?;
    Ok(RefPage {
        first: entries.first().map(|(n, _)| n.clone()).unwrap_or_default(),
        last: entries.last().map(|(n, _)| n.clone()).unwrap_or_default(),
        key,
        count: entries.len() as u64,
        bytes: body.len() as u64,
    })
}

/// Set `name = oid` in the page store: rewrite the covering page (splitting
/// when it outgrows `page_max`) and update `manifest.ref_pages` in place.
/// The new page objects are PUT immediately (data before pointer); the
/// change commits only when the caller CASes the manifest.
pub fn update(
    store: &ObjectStore,
    manifest: &mut Manifest,
    data_prefix: &str,
    name: &str,
    oid: &str,
) -> Result<(), String> {
    let idx = manifest
        .page_index(name)
        .ok_or("ref pages absent — caller must use the flat refs list")?;
    let mut entries = load_page(store, &manifest.ref_pages[idx])?;
    match entries.binary_search_by(|(n, _)| n.as_str().cmp(name)) {
        Ok(i) => entries[i].1 = oid.to_string(),
        Err(i) => entries.insert(i, (name.to_string(), oid.to_string())),
    }
    let replacement = if entries.len() > page_max() {
        let right = entries.split_off(entries.len() / 2);
        vec![
            put_page(store, data_prefix, &entries)?,
            put_page(store, data_prefix, &right)?,
        ]
    } else {
        vec![put_page(store, data_prefix, &entries)?]
    };
    manifest.ref_pages.splice(idx..idx + 1, replacement);
    Ok(())
}

/// Delete `name` from the page store, rewriting its covering page.
///
/// The page is kept even when it empties, rather than being spliced out
/// of `ref_pages`. `page_index` picks a page by comparing `first` against
/// the name it is looking for, so the list has to keep covering the whole
/// key space in order: dropping a page in the middle leaves the names
/// that used to live in it resolving to a neighbour, which reads as
/// "that ref does not exist" for every ref in the gap, not just the one
/// that was deleted. An empty page costs one small object and keeps the
/// ranges honest; compaction rebuilds the pages from scratch anyway.
///
/// Returns whether the ref was there to begin with, so a caller can tell
/// a real deletion from a no-op.
pub fn remove(
    store: &ObjectStore,
    manifest: &mut Manifest,
    data_prefix: &str,
    name: &str,
) -> Result<bool, String> {
    let idx = manifest
        .page_index(name)
        .ok_or("ref pages absent — caller must use the flat refs list")?;
    let mut entries = load_page(store, &manifest.ref_pages[idx])?;
    let Ok(i) = entries.binary_search_by(|(n, _)| n.as_str().cmp(name)) else {
        return Ok(false);
    };
    entries.remove(i);
    let mut page = put_page(store, data_prefix, &entries)?;
    // **An emptied page keeps the range it covered.**
    //
    // `put_page` derives `first`/`last` from the entries, so a page with
    // nothing left in it gets `first: ""` — and `page_index` locates a
    // page with `partition_point(|p| p.first <= name)`, which assumes
    // `first` ascends across `ref_pages`. An empty string sorts before
    // everything, so a blank page sitting in the middle of the list
    // breaks that ordering and the partition starts returning the wrong
    // page: refs resolve to a neighbour that does not hold them, new
    // refs are inserted into the wrong range, and a clone of the
    // repository fails outright.
    //
    // Keeping the bounds is what makes "keep the page" mean anything.
    // The page is retained precisely so its range stays covered — see
    // the note above — and a range with no ends covers nothing.
    if entries.is_empty() {
        page.first = manifest.ref_pages[idx].first.clone();
        page.last = manifest.ref_pages[idx].last.clone();
    }
    manifest.ref_pages[idx] = page;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_roundtrip() {
        let entries = vec![
            ("refs/heads/a".to_string(), "a".repeat(40)),
            ("refs/heads/b".to_string(), "b".repeat(40)),
        ];
        assert_eq!(parse_page(&encode_page(&entries)).unwrap(), entries);
    }

    /// A page exactly as `put_page` would have minted it.
    fn minted(entries: &[(String, String)]) -> (RefPage, Vec<u8>) {
        let body = encode_page(entries);
        let page = RefPage {
            first: entries.first().map(|(n, _)| n.clone()).unwrap_or_default(),
            last: entries.last().map(|(n, _)| n.clone()).unwrap_or_default(),
            key: format!("d/refs/page-{}.txt", page_digest(&body)),
            count: entries.len() as u64,
            bytes: body.len() as u64,
        };
        (page, body)
    }

    fn sample() -> Vec<(String, String)> {
        vec![
            ("refs/heads/a".to_string(), "a".repeat(40)),
            ("refs/heads/b".to_string(), "b".repeat(40)),
        ]
    }

    #[test]
    fn verifies_an_intact_page() {
        let entries = sample();
        let (page, body) = minted(&entries);
        assert_eq!(verify_page(&page, &body).unwrap(), entries);
    }

    /// The hazard: a short read that stops on a line boundary parses
    /// cleanly and silently drops refs. It must not survive `load_page`.
    #[test]
    fn rejects_a_page_truncated_on_a_line_boundary() {
        let entries = sample();
        let (page, body) = minted(&entries);
        let short = &body[..encode_page(&entries[..1]).len()];
        assert!(parse_page(short).is_ok(), "truncation parses — that is why");
        let err = verify_page(&page, short).unwrap_err();
        assert!(err.contains(&page.key), "{err}");
        assert!(err.contains("bytes"), "{err}");
    }

    /// Same-length substitution: only the content address catches it.
    #[test]
    fn rejects_a_page_whose_body_is_not_the_one_its_key_names() {
        let (page, _) = minted(&sample());
        let other = encode_page(&[("refs/heads/a".to_string(), "c".repeat(40))]);
        let mut swapped = page.clone();
        swapped.bytes = other.len() as u64;
        swapped.count = 1;
        let err = verify_page(&swapped, &other).unwrap_err();
        assert!(err.contains("content hash"), "{err}");
    }

    /// `count` disagreeing with the body is a manifest/page mismatch even
    /// when the bytes are self-consistent.
    #[test]
    fn rejects_a_page_whose_count_disagrees() {
        let (mut page, body) = minted(&sample());
        page.count = 3;
        let err = verify_page(&page, &body).unwrap_err();
        assert!(err.contains("2 refs, manifest says 3"), "{err}");
    }

    /// A key that is not "page-<sha256>.txt" keeps the cheap checks rather
    /// than being refused: no producer in the tree mints one, and failing
    /// closed on a shape we have never seen costs availability for nothing.
    #[test]
    fn tolerates_a_key_of_another_shape() {
        let entries = sample();
        let (mut page, body) = minted(&entries);
        page.key = "d/refs/legacy.txt".to_string();
        assert_eq!(verify_page(&page, &body).unwrap(), entries);
        page.bytes += 1;
        assert!(verify_page(&page, &body).is_err());
    }

    #[test]
    fn rejects_malformed_pages() {
        assert!(parse_page(b"tooshort refs/heads/x\n").is_err());
        assert!(parse_page(b"nospace\n").is_err());
        assert!(parse_page(&[0xff, 0xfe]).is_err());
    }
}
