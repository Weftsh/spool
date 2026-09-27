//! The commit count, refreshed after every write.
//!
//! Runs inside the compaction job — `compactor::run_one` calls
//! [`refresh`] once the fold question is settled — because that job is
//! already queued by every accepted write (push, REST commit, mirror
//! sync) and already reads the repository's objects. A count job of its
//! own would need the same four enqueue sites and the same lease
//! discipline for one more row.
//!
//! The walk is the full reachable set from the default branch's tip,
//! every parent of every commit, which is what GitHub's number means.
//! Not the first-parent line the log walks: on a repository merged from
//! branches the two differ by a factor of four, and the smaller number
//! reads as "history is missing". It is bounded by
//! `STRATUM_COMMIT_COUNT_CAP` (I13: every walk on the server is), and a
//! walk that hits the cap is stored as inexact so the page prints a `+`
//! instead of a wrong number.
//!
//! Cost: one object read per commit, through the process read cache. The
//! first walk of a repository pays for every commit; every walk after it
//! pays for the commits pushed since, because a commit's oid is its
//! content and the cache keeps what it has read.

use crate::app::SharedState;
use std::collections::HashSet;
use stratum_control::commit_counts::{self, CommitCount};
use stratum_control::registry::Repo;
use stratum_engine::objwrite::{parse_commit, OBJ_COMMIT};
use stratum_engine::read::LayoutReader;

/// How many commits one walk may visit before it stops and says so.
fn cap() -> usize {
    std::env::var("STRATUM_COMMIT_COUNT_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100_000)
        .max(1)
}

/// Every commit reachable from `tip`, counted; `false` when the walk
/// stopped at `cap` with history still to go.
pub fn count_reachable(
    reader: &LayoutReader,
    tip: &str,
    cap: usize,
) -> Result<(u64, bool), String> {
    let mut pending: Vec<String> = vec![tip.to_string()];
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(oid) = pending.pop() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        if seen.len() > cap {
            return Ok((cap as u64, false));
        }
        let (kind, data) = reader.object(&oid)?;
        if kind != OBJ_COMMIT {
            return Err(format!("{oid} is not a commit"));
        }
        pending.extend(parse_commit(&data)?.parents);
    }
    Ok((seen.len() as u64, true))
}

/// Bring the stored count up to the repository's current tip. Answers
/// what is stored afterwards: `None` for a repository with no commits.
///
/// Skips the walk when the stored row is already for this tip, which is
/// the common case for a compaction job queued by a write that moved
/// some other branch.
pub async fn refresh(state: &SharedState, repo: &Repo) -> Result<Option<CommitCount>, String> {
    let stored = commit_counts::get(&state.db, &repo.id)?;
    let known_tip = stored.as_ref().map(|c| c.tip.clone());
    let prefix = repo.prefix().as_str().to_string();
    let cap = cap();
    let walked = crate::api::reads::with_reader(state, prefix, move |reader| {
        let Some(tip) = reader.resolve_rev("HEAD")? else {
            return Ok(None);
        };
        if known_tip.as_deref() == Some(tip.as_str()) {
            return Ok(Some((tip, None)));
        }
        let (count, exact) = count_reachable(reader, &tip, cap)?;
        Ok(Some((tip, Some((count, exact)))))
    })
    .await?;
    match walked {
        None => Ok(None),
        Some((_, None)) => Ok(stored),
        Some((tip, Some((count, exact)))) => {
            commit_counts::set(&state.db, &repo.id, &tip, count as i64, exact)?;
            commit_counts::get(&state.db, &repo.id)
        }
    }
}
