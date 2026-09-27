//! The storage half of a zero-copy fork.
//!
//! A fork copies **no objects**. It writes two small pointers into its
//! own prefix — a manifest whose segment keys are already absolute, and
//! an `SLH4` `locator.hdr` naming an absolute data prefix — and from
//! that moment it serves clones out of upstream's immutable storage.
//! Milliseconds, and free until the histories diverge, at which point
//! the fork's own pushes land as new segments in its **own** prefix and
//! the manifest simply names both.
//!
//! This lives beside `ingest`, `compact` and `gc` rather than in the
//! server because it is an operation on storage, not on a request — and
//! because the server is a binary, so anything left in it can only be
//! tested through HTTP. The job that calls these functions, and the
//! write ordering it must obey, is `workers/forker.rs`.

use std::collections::BTreeSet;
use stratum_store::manifest::Manifest;
use stratum_store::plane;
use stratum_store::{ObjectStore, PutCond, PutError};

#[derive(Debug, PartialEq, Eq)]
pub enum ForkOutcome {
    /// Storage was pointed at upstream and the fork is readable.
    Forked,
    /// Upstream has nothing pushed to it yet, so the fork is an empty
    /// repository — which is exactly what forking an empty repository
    /// should produce, and needs no pointers at all.
    Empty,
    /// The row is gone, or was never a fork. Nothing to do and not a
    /// failure.
    NothingToDo,
}

/// What upstream looks like right now.
#[derive(Debug)]
pub struct Upstream {
    pub manifest: Manifest,
    /// Upstream's locator header, when it has one.
    ///
    /// `None` is ordinary, not broken: the point-read plane is built by
    /// compaction, so a repository that has been pushed to and not yet
    /// compacted has a manifest and no locator at all. Refusing to fork
    /// those would refuse the commonest repository on the server — one
    /// somebody just pushed.
    pub hdr: Option<Vec<u8>>,
    /// Where the locator's data actually lives — upstream's own epoch
    /// directory, or, when upstream is itself a fork, wherever *its*
    /// header points. A fork of a fork reads from where the bytes really
    /// are rather than chaining a hop through the middle repository.
    /// `None` whenever `hdr` is.
    pub data_prefix: Option<String>,
}

/// Read upstream's manifest and locator header, resolving where its data
/// really lives. `None` means upstream has nothing pushed yet.
pub fn read_upstream(store: &ObjectStore, prefix: &str) -> Result<Option<Upstream>, String> {
    let manifest_bytes = match store.get(&format!("{prefix}/manifest.json")) {
        Ok(b) => b,
        Err(e) if e.contains("HTTP 404") => return Ok(None),
        Err(e) => return Err(format!("read upstream manifest: {e}")),
    };
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).map_err(|e| format!("upstream manifest: {e}"))?;
    // A manifest with no locator is a repository that has been pushed
    // to and not yet compacted — the plane is compaction's output. The
    // fork inherits that state exactly: it clones from the manifest's
    // segments like upstream does, and gets a plane of its own the first
    // time either of them is compacted. Refusing here would have refused
    // to fork any repository somebody had just pushed.
    let (hdr, data_prefix) = match store.get(&format!("{prefix}/locator.hdr")) {
        Ok(b) => {
            let parsed = plane::parse_header(&b).map_err(|e| format!("upstream locator: {e}"))?;
            let dp = parsed
                .data_prefix
                .clone()
                .unwrap_or_else(|| format!("{prefix}/{}", parsed.epoch));
            (Some(b), Some(dp))
        }
        Err(e) if e.contains("HTTP 404") => (None, None),
        Err(e) => return Err(format!("read upstream locator: {e}")),
    };
    Ok(Some(Upstream {
        manifest,
        hdr,
        data_prefix,
    }))
}

/// Every `(repo id, epoch)` the fork will be reading, deduplicated.
///
/// Derived from the absolute keys in the copied manifest plus the data
/// prefix the locator resolves to, because those are exactly the objects
/// the fork's reads will reach for. Keys under the fork's own prefix are
/// skipped — a repository does not hold a reference against itself, and
/// `epoch_refs::register` refuses one anyway.
pub fn references_in(up: &Upstream, fork_repo_id: &str) -> Vec<(String, String)> {
    let m = &up.manifest;
    let mut keys: Vec<&str> = Vec::new();
    keys.extend(m.segments.iter().map(|s| s.key.as_str()));
    keys.extend(m.cold_segments.iter().map(|s| s.key.as_str()));
    keys.extend(m.hot_segments.iter().map(|s| s.key.as_str()));
    if let Some(s) = &m.snapshot {
        keys.push(&s.key);
    }
    if let Some(l) = &m.locator {
        keys.push(&l.key);
        keys.push(&l.chains_key);
    }
    keys.extend(m.ref_pages.iter().map(|p| p.key.as_str()));
    for w in &m.wal {
        keys.push(&w.key);
        keys.push(&w.oids_key);
    }

    let mut out: BTreeSet<(String, String)> = BTreeSet::new();
    for key in keys {
        if let Some(pair) = repo_and_epoch(key) {
            out.insert(pair);
        }
    }
    // The locator's own data prefix is a directory rather than an object
    // key, so it needs the trailing segment treated as the epoch.
    if let Some(dp) = &up.data_prefix {
        if let Some(pair) = repo_and_epoch(&format!("{dp}/x")) {
            out.insert(pair);
        }
    }
    out.into_iter()
        .filter(|(repo, _)| repo != fork_repo_id)
        .collect()
}

/// `o/{org}/r/{repo}/{layout}/{epoch}/…` → `(repo, epoch)`.
///
/// Anything that is not that shape is ignored rather than guessed at: a
/// key we cannot attribute is one we must not claim to have pinned.
fn repo_and_epoch(key: &str) -> Option<(String, String)> {
    let p: Vec<&str> = key.split('/').collect();
    if p.len() < 7 || p[0] != "o" || p[2] != "r" {
        return None;
    }
    Some((p[3].to_string(), p[5].to_string()))
}

/// Write the fork's two pointers: locator first, manifest last.
pub fn publish_fork(
    store: &ObjectStore,
    fork_prefix: &str,
    up: &Upstream,
    repo_name: &str,
    layout: &str,
) -> Result<ForkOutcome, String> {
    // The header, re-pointed at wherever upstream's bytes actually are.
    // Only when upstream has one — see `Upstream::hdr`.
    if let (Some(hdr), Some(dp)) = (&up.hdr, &up.data_prefix) {
        let rebased = plane::rebase_header(hdr, dp)?;
        store
            .put(
                &format!("{fork_prefix}/locator.hdr"),
                &rebased,
                PutCond::None,
            )
            .map_err(|e| format!("write fork locator: {e}"))?;
    }

    // The manifest is upstream's, with only its own identity changed.
    // Every segment key in it is absolute and stays absolute — that is
    // the entire zero-copy trick, and rewriting any of them would turn a
    // fork into a copy that reads keys nobody wrote.
    let mut manifest = up.manifest.clone();
    manifest.repo = repo_name.to_string();
    manifest.layout = layout.to_string();
    let body = serde_json::to_vec(&manifest).map_err(|e| format!("fork manifest: {e}"))?;
    match store.put(
        &format!("{fork_prefix}/manifest.json"),
        &body,
        PutCond::IfNoneMatchStar,
    ) {
        Ok(()) => Ok(ForkOutcome::Forked),
        // Somebody got there first — a retry of this same job, or the
        // other half of a race. The fork already has its manifest and
        // this one is not entitled to overwrite it: the manifest moves
        // only by CAS (I9), and "already published" is success, not a
        // failure to be retried into a clobber.
        Err(PutError::Conflict) => Ok(ForkOutcome::Forked),
        Err(e) => Err(format!("write fork manifest: {e:?}")),
    }
}

/// What promoting a fork did.
#[derive(Debug, PartialEq, Eq)]
pub enum PromoteOutcome {
    /// The fork now stands on storage of its own and references nobody.
    Promoted,
    /// Nothing to copy — an empty fork, or a layout with no head tip.
    /// It holds no references either, so there is nothing to release.
    NotNeeded,
    /// A concurrent push won the manifest CAS. Promotion is retried, and
    /// the push it lost to is the newer truth.
    LostRace,
}

/// Give a fork storage of its own, so it stops reading anybody else's.
///
/// **This is compaction, deliberately, rather than a second copy of the
/// same sequence.** Compaction materializes a pinned manifest into a real
/// repository, re-ingests it under a fresh epoch, and swaps the manifest
/// by CAS. Run against a fork, that re-ingest writes every object under
/// the *fork's own* prefix — which is exactly what promotion means. The
/// only difference is the trigger: compaction waits for a WAL threshold,
/// promotion cannot wait for anything, so the thresholds are zero.
///
/// `mirror/sync.rs` and this both wanting the ingest path is the reason
/// it is called rather than copied; a second implementation of the
/// materialize/re-ingest/CAS dance is a second thing to keep correct.
///
/// **Ordering, and it is I7's shape pointing the other way.** The caller
/// must promote the storage *first* and release the `epoch_refs` after.
/// A crash between them leaves a fork that owns its bytes and still
/// holds a claim on upstream's — storage pinned slightly too long, which
/// a sweeper reconciles. Releasing first would leave a fork reading data
/// nothing is protecting, which is the corruption this whole slice
/// exists to prevent.
pub fn promote(
    store: &ObjectStore,
    prefix: &str,
    cfg: &crate::ingest::IngestConfig,
    work_dir: &std::path::Path,
) -> Result<PromoteOutcome, String> {
    let thresholds = crate::compact::CompactionThresholds {
        wal_entries: 0,
        wal_bytes: 0,
    };
    match crate::compact::compact(store, prefix, cfg, &thresholds, work_dir)? {
        crate::compact::CompactOutcome::Compacted => Ok(PromoteOutcome::Promoted),
        crate::compact::CompactOutcome::NotNeeded => Ok(PromoteOutcome::NotNeeded),
        crate::compact::CompactOutcome::LostRace => Ok(PromoteOutcome::LostRace),
    }
}
