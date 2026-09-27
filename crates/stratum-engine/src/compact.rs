//! Full compaction: fold the WAL back into a fresh, optimally-ordered
//! epoch. Product-native port of the research repo's `bench/compact.py`
//! full mode: materialize the layout (a verified clone of exactly what a
//! wire client would get), re-ingest under a fresh epoch, and swap the
//! manifest with a CAS against the etag read *before* materializing —
//! concurrent pushes always win, compaction retries.
//!
//! After compaction: WAL empty, depth-1 snapshot fresh again, point reads
//! of formerly-WAL objects go through the locator, and the old epoch is
//! GC fodder once its grace window passes.

use crate::ingest::{publish, IngestConfig, PublishError, PublishMode};
use crate::materialize::materialize_manifest;
use std::path::Path;
use stratum_store::manifest::Manifest;
use stratum_store::ObjectStore;

pub struct CompactionThresholds {
    /// Compact when the WAL holds at least this many entries…
    pub wal_entries: usize,
    /// …or this many payload bytes.
    pub wal_bytes: u64,
}

impl Default for CompactionThresholds {
    fn default() -> Self {
        CompactionThresholds {
            wal_entries: 8,
            wal_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum CompactOutcome {
    /// Below thresholds; nothing done.
    NotNeeded,
    Compacted,
    /// A concurrent writer landed between snapshot and swap; caller may
    /// re-enqueue.
    LostRace,
}

/// The branch a fold builds its spine against.
///
/// Normally HEAD's. The fallback exists because of a bug that cost a
/// customer-visible 4x slowdown and was invisible from the inside.
///
/// This used to refuse outright when HEAD named a branch the manifest
/// does not hold — a repository that was only ever pushed to on a
/// feature branch, or whose default branch was deleted — and it refused
/// by returning `NotNeeded`, the same answer as "the WAL is short, there
/// is nothing to do". So compaction switched itself off for that
/// repository, permanently and silently. Every later push and every
/// later read then materialized the whole WAL, which grows without
/// bound: measured at 61 entries, a push took **79x** what it took into
/// the same repository with an empty WAL, and one fold put it back.
///
/// The choice must be the same on every node, or two folds of one
/// repository build different spines and fight: hence the sort and the
/// fixed preference order rather than "whichever ref came back first".
fn spine_branch(m: &Manifest) -> Option<String> {
    if m.tip().is_some() {
        return m.head.strip_prefix("refs/heads/").map(str::to_string);
    }
    let mut branches: Vec<&str> = m
        .refs
        .iter()
        .filter_map(|(n, _)| n.strip_prefix("refs/heads/"))
        .collect();
    branches.sort_unstable();
    for conventional in ["main", "master", "trunk"] {
        if branches.contains(&conventional) {
            return Some(conventional.to_string());
        }
    }
    branches.first().map(|b| b.to_string())
}

pub fn compact(
    store: &ObjectStore,
    prefix: &str,
    cfg: &IngestConfig,
    thresholds: &CompactionThresholds,
    work_dir: &Path,
) -> Result<CompactOutcome, String> {
    let manifest_key = format!("{prefix}/manifest.json");
    let (mbytes, etag) = store.get_with_etag(&manifest_key)?;
    let manifest: Manifest =
        serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;
    let wal_bytes: u64 = manifest.wal.iter().map(|w| w.bytes).sum();
    if manifest.wal.len() < thresholds.wal_entries && wal_bytes < thresholds.wal_bytes {
        return Ok(CompactOutcome::NotNeeded);
    }
    let Some(primary) = spine_branch(&manifest) else {
        // Genuinely nothing to spine against: no branches at all. A
        // repository with only tags, or none, has no history to fold
        // around.
        return Ok(CompactOutcome::NotNeeded);
    };

    let seed = work_dir.join("compact-seed.git");
    let staging = work_dir.join("compact-staging");
    let _ = std::fs::remove_dir_all(&seed);
    let _ = std::fs::remove_dir_all(&staging);
    materialize_manifest(store, &manifest, &seed)?;

    let result = (|| -> Result<CompactOutcome, String> {
        let mut out = crate::ingest(&seed, prefix, &primary, cfg, &staging)?;
        let hdr = crate::build_locator(&seed, &mut out, prefix, 0)?;
        match publish(store, prefix, &out, &hdr, PublishMode::ReplaceIfMatch(etag)) {
            Ok(()) => Ok(CompactOutcome::Compacted),
            Err(PublishError::LostRace(_)) => Ok(CompactOutcome::LostRace),
            Err(e) => Err(e.to_string()),
        }
    })();
    let _ = std::fs::remove_dir_all(&seed);
    let _ = std::fs::remove_dir_all(&staging);
    result
}

#[cfg(test)]
mod spine_tests {
    use super::*;

    fn manifest(head: &str, branches: &[&str]) -> Manifest {
        let mut m: Manifest = serde_json::from_str(
            r#"{"schema":1,"repo":"r","layout":"L","refs":[],"head":"refs/heads/main","epoch":"e1"}"#,
        )
        .unwrap();
        m.head = head.to_string();
        m.refs = branches
            .iter()
            .map(|b| (format!("refs/heads/{b}"), "0".repeat(40)))
            .collect();
        m
    }

    /// The ordinary case: HEAD resolves, and it is what the spine is
    /// built against.
    #[test]
    fn head_is_used_when_it_resolves() {
        let m = manifest("refs/heads/main", &["main", "topic"]);
        assert_eq!(spine_branch(&m).as_deref(), Some("main"));
    }

    /// The bug. HEAD names a branch that is not there — pushed only a
    /// feature branch, or the default was deleted — and the fold must
    /// still happen, because refusing here disables compaction for that
    /// repository permanently and silently.
    #[test]
    fn a_dangling_head_falls_back_rather_than_refusing() {
        let m = manifest("refs/heads/main", &["feature/setup"]);
        assert_eq!(spine_branch(&m).as_deref(), Some("feature/setup"));
    }

    /// Two nodes folding one repository must choose the same spine, or
    /// they build different layouts and fight over the swap. The answer
    /// cannot depend on which ref the store happened to return first.
    #[test]
    fn the_fallback_is_the_same_on_every_node() {
        let one = manifest("refs/heads/gone", &["zeta", "alpha", "mid"]);
        let other = manifest("refs/heads/gone", &["mid", "zeta", "alpha"]);
        assert_eq!(spine_branch(&one), spine_branch(&other));
        assert_eq!(spine_branch(&one).as_deref(), Some("alpha"));
    }

    /// A conventional default is a better spine than whatever sorts
    /// first: it is the branch the history is actually shaped around.
    #[test]
    fn a_conventional_default_wins_over_alphabetical_order() {
        let m = manifest("refs/heads/gone", &["aaa", "main", "zzz"]);
        assert_eq!(spine_branch(&m).as_deref(), Some("main"));
        let m = manifest("refs/heads/gone", &["aaa", "master", "zzz"]);
        assert_eq!(spine_branch(&m).as_deref(), Some("master"));
    }

    /// No branches at all is the one case where refusing is right: there
    /// is no history to fold around. Tags do not make a spine.
    #[test]
    fn no_branches_at_all_is_still_nothing_to_do() {
        let mut m = manifest("refs/heads/main", &[]);
        m.refs = vec![("refs/tags/v1".into(), "0".repeat(40))];
        assert_eq!(spine_branch(&m), None);
    }
}
