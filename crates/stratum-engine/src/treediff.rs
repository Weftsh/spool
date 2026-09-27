//! Structural diff between two commits' trees: added/modified/deleted
//! paths with old/new oid+mode, recursing only into subtrees whose oids
//! differ. No rename detection, no textual diff (v1) — the callers that
//! need "which paths changed" (the read API's /diff, OWNERS evaluation,
//! change tracking) all consume this one walk.

use crate::objwrite::{self, hex, TreeEntry, OBJ_COMMIT, OBJ_TREE};
use crate::read::LayoutReader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    Added,
    Modified,
    Deleted,
}

impl DiffStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            DiffStatus::Added => "added",
            DiffStatus::Modified => "modified",
            DiffStatus::Deleted => "deleted",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub status: DiffStatus,
    pub path: String,
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
}

/// Diff `from` (None = the empty tree, for root commits) against `to`.
/// Both are commit oids/revs already resolved to 40-hex by the caller.
pub fn diff_commits(
    reader: &LayoutReader,
    from: Option<&str>,
    to: &str,
) -> Result<Vec<DiffEntry>, String> {
    let tree_of = |commit: &str| -> Result<String, String> {
        let (k, data) = reader.object(commit)?;
        if k != OBJ_COMMIT {
            return Err(format!("{commit} is not a commit"));
        }
        Ok(objwrite::parse_commit(&data)?.tree)
    };
    let ta = match from {
        Some(c) => Some(tree_of(c)?),
        None => None,
    };
    let tb = tree_of(to)?;
    let mut out = Vec::new();
    diff_trees(reader, ta.as_deref(), Some(&tb), "", &mut out)?;
    Ok(out)
}

fn diff_trees(
    reader: &LayoutReader,
    a: Option<&str>,
    b: Option<&str>,
    base: &str,
    out: &mut Vec<DiffEntry>,
) -> Result<(), String> {
    if a == b {
        return Ok(());
    }
    let load = |oid: Option<&str>| -> Result<Vec<TreeEntry>, String> {
        match oid {
            None => Ok(Vec::new()),
            Some(o) => {
                let (k, data) = reader.object(o)?;
                if k != OBJ_TREE {
                    return Err(format!("{o} is not a tree"));
                }
                objwrite::parse_tree(&data)
            }
        }
    };
    let ea = load(a)?;
    let eb = load(b)?;
    let names: std::collections::BTreeSet<&String> = ea
        .iter()
        .map(|e| &e.name)
        .chain(eb.iter().map(|e| &e.name))
        .collect();
    for name in names {
        let x = ea.iter().find(|e| &e.name == name);
        let y = eb.iter().find(|e| &e.name == name);
        let path = if base.is_empty() {
            name.clone()
        } else {
            format!("{base}/{name}")
        };
        let is_tree = |e: &TreeEntry| e.mode == "40000" || e.mode == "040000";
        match (x, y) {
            (Some(x), Some(y)) if x.oid == y.oid && x.mode == y.mode => {}
            (Some(x), Some(y)) if is_tree(x) && is_tree(y) => {
                diff_trees(reader, Some(&hex(&x.oid)), Some(&hex(&y.oid)), &path, out)?;
            }
            (Some(x), Some(y)) => {
                if is_tree(x) || is_tree(y) {
                    // Type change: report as delete + add subtrees/files.
                    if is_tree(x) {
                        diff_trees(reader, Some(&hex(&x.oid)), None, &path, out)?;
                    } else {
                        out.push(change(DiffStatus::Deleted, &path, Some(x), None));
                    }
                    if is_tree(y) {
                        diff_trees(reader, None, Some(&hex(&y.oid)), &path, out)?;
                    } else {
                        out.push(change(DiffStatus::Added, &path, None, Some(y)));
                    }
                } else {
                    out.push(change(DiffStatus::Modified, &path, Some(x), Some(y)));
                }
            }
            (Some(x), None) => {
                if is_tree(x) {
                    diff_trees(reader, Some(&hex(&x.oid)), None, &path, out)?;
                } else {
                    out.push(change(DiffStatus::Deleted, &path, Some(x), None));
                }
            }
            (None, Some(y)) => {
                if is_tree(y) {
                    diff_trees(reader, None, Some(&hex(&y.oid)), &path, out)?;
                } else {
                    out.push(change(DiffStatus::Added, &path, None, Some(y)));
                }
            }
            (None, None) => unreachable!(),
        }
    }
    Ok(())
}

fn change(
    status: DiffStatus,
    path: &str,
    old: Option<&TreeEntry>,
    new: Option<&TreeEntry>,
) -> DiffEntry {
    DiffEntry {
        status,
        path: path.to_string(),
        old_oid: old.map(|e| hex(&e.oid)),
        new_oid: new.map(|e| hex(&e.oid)),
        old_mode: old.map(|e| e.mode.clone()),
        new_mode: new.map(|e| e.mode.clone()),
    }
}
