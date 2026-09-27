//! Loading OWNERS files out of a repo snapshot at a commit.
//!
//! One tree walk per distinct ancestor directory of the paths under
//! evaluation — the reader's `entry_at` is an index-plane lookup, so
//! this stays cheap even for wide diffs.

use crate::review::owners::{self, OwnersFile, ParseError};
use std::collections::{BTreeMap, BTreeSet};
use stratum_engine::objwrite::{hex, OBJ_BLOB};
use stratum_engine::read::LayoutReader;

/// Read and parse every OWNERS file that could govern any of `paths`,
/// at `commit`. Directories without one are absent from the map.
pub fn owners_files_for_paths(
    reader: &LayoutReader,
    commit: &str,
    paths: &[String],
) -> Result<BTreeMap<String, Result<OwnersFile, ParseError>>, String> {
    let dirs: BTreeSet<String> = paths
        .iter()
        .flat_map(|p| owners::ancestor_dirs(p))
        .collect();
    let mut out = BTreeMap::new();
    for dir in dirs {
        let file_path = if dir.is_empty() {
            "OWNERS".to_string()
        } else {
            format!("{dir}/OWNERS")
        };
        let Some(entry) = reader.entry_at(commit, &file_path)? else {
            continue;
        };
        let (kind, data) = reader.object(&hex(&entry.oid))?;
        if kind != OBJ_BLOB {
            // A directory named OWNERS is not a rule file; it governs
            // nothing rather than poisoning the path.
            continue;
        }
        let text = String::from_utf8_lossy(&data);
        out.insert(dir, owners::parse(&text));
    }
    Ok(out)
}
