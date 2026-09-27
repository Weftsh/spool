//! Fast-forward verification: is `old_tip` reachable from `new_tip`?
//!
//! The land queue's one graph question. A bounded BFS over commit
//! parents (all of them — a merge commit fast-forwards past either
//! parent), with the bound reported as its own answer rather than
//! guessed around: hostile or degenerate history exhausts the budget
//! loudly (I13), and the caller turns that into an ejection verdict a
//! person can read.

use crate::objwrite::{self, OBJ_COMMIT};
use crate::read::LayoutReader;
use std::collections::{HashSet, VecDeque};

/// Commits visited before the walk gives up. Trunk moves between a
/// change's creation and its landing by hours of work, not by histories
/// this deep; anything past the cap is a case for a human.
pub const ANCESTOR_WALK_CAP: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ancestry {
    /// `old_tip` is an ancestor of (or equal to) `new_tip`: moving the
    /// ref forward loses nothing.
    FastForward,
    /// `new_tip`'s history does not contain `old_tip`.
    NotAncestor,
    /// The walk hit its budget before deciding.
    CapExceeded,
}

/// Would moving a ref from `old_tip` to `new_tip` be a fast-forward?
/// `old_tip == None` (an unborn branch) always is.
pub fn is_fast_forward(
    reader: &LayoutReader,
    old_tip: Option<&str>,
    new_tip: &str,
) -> Result<Ancestry, String> {
    is_fast_forward_capped(reader, old_tip, new_tip, ANCESTOR_WALK_CAP)
}

/// The walk with an explicit budget, so tests exercise the cap without
/// manufacturing four thousand commits.
pub fn is_fast_forward_capped(
    reader: &LayoutReader,
    old_tip: Option<&str>,
    new_tip: &str,
    cap: usize,
) -> Result<Ancestry, String> {
    let Some(old) = old_tip else {
        return Ok(Ancestry::FastForward);
    };
    Ok(match descent_capped(reader, new_tip, old, cap)? {
        Descent::Contains { .. } => Ancestry::FastForward,
        Descent::NotFound => Ancestry::NotAncestor,
        Descent::CapExceeded => Ancestry::CapExceeded,
    })
}

/// Where `target` stands in `tip`'s history — the question a driver asks
/// of a trunk it wrote to when it cannot remember whether the write
/// took: the store said 503 *after* applying a CAS, or the node died
/// between the CAS and recording it, and by the time anyone looks again
/// the trunk may have taken other people's pushes on top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Descent {
    /// `target` is `tip` itself or in its history. `child` is the first
    /// commit met with `target` as a parent — the one sitting directly on
    /// it — and `None` when `target` is `tip`.
    Contains { child: Option<String> },
    /// `tip`'s history does not contain `target`.
    NotFound,
    /// The walk hit its budget before deciding.
    CapExceeded,
}

/// Is `target` in `tip`'s history, and what sits directly on it?
pub fn descent(reader: &LayoutReader, tip: &str, target: &str) -> Result<Descent, String> {
    descent_capped(reader, tip, target, ANCESTOR_WALK_CAP)
}

/// [`descent`] with an explicit budget.
pub fn descent_capped(
    reader: &LayoutReader,
    tip: &str,
    target: &str,
    cap: usize,
) -> Result<Descent, String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut frontier: VecDeque<String> = VecDeque::new();
    frontier.push_back(tip.to_string());
    let mut child = None;
    while let Some(oid) = frontier.pop_front() {
        if oid == target {
            return Ok(Descent::Contains { child });
        }
        if !seen.insert(oid.clone()) {
            continue;
        }
        if seen.len() > cap {
            return Ok(Descent::CapExceeded);
        }
        let (k, data) = reader.object(&oid)?;
        if k != OBJ_COMMIT {
            return Err(format!("{oid} is not a commit"));
        }
        for parent in objwrite::parse_commit(&data)?.parents {
            if parent == target && child.is_none() {
                child = Some(oid.clone());
            }
            frontier.push_back(parent);
        }
    }
    Ok(Descent::NotFound)
}
