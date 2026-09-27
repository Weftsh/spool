//! Reading `.weft/` out of a repository at a rev.
//!
//! Two callers want exactly this walk: the `GET …/workflows` route,
//! which shows a reader what would run, and the trigger, which decides
//! what *does* run when a push lands. They must agree — a file the route
//! says is fine and the trigger refuses, or the other way round, is the
//! kind of inconsistency that gets reported as "CI is flaky" — so the
//! walk lives here once and both call it.

use stratum_engine::read::LayoutReader;

/// Where workflows live. One directory, not a glob anybody configures:
/// a repository whose CI could be hiding anywhere is one nobody can
/// audit by looking.
pub const DIR: &str = ".weft";

/// The most files we will read out of it.
///
/// A repository is untrusted input, and `.weft/` with ten thousand
/// files in it is a way to make one request do a lot of reading. Real
/// projects have one to five.
pub const MAX_FILES: usize = 32;

/// The names under [`DIR`] that configure a repository's static site on
/// the hosted edition. This one does not host sites, and ignores them.
///
/// Still reserved rather than walked as a workflow, and reserved *here*
/// rather than at either call site, because the route that lists
/// workflows and the trigger that runs them must agree about what a file
/// is. A site config has no `jobs:`, so leaving it in the walk would
/// refuse every one of them as a malformed workflow — and the trigger
/// would write that refusal into the checks of every repository moved
/// here from the hosted edition.
///
/// Both extensions are reserved because both are valid YAML names.
pub const SITE_CONFIG: [&str; 2] = ["site.yml", "site.yaml"];

/// Is this name configuration rather than a workflow?
pub fn is_reserved(name: &str) -> bool {
    SITE_CONFIG.contains(&name)
}

pub fn is_workflow(name: &str) -> bool {
    !is_reserved(name) && (name.ends_with(".yml") || name.ends_with(".yaml"))
}

/// The file's name without its extension — what an unnamed workflow is
/// called.
pub fn stem(name: &str) -> &str {
    name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name)
}

fn is_dir(mode: &str) -> bool {
    mode == "40000" || mode == "040000"
}

/// The entries of [`DIR`] at `rev`.
///
/// `None` means the rev does not resolve. An empty vec means the
/// directory is absent, is a file rather than a directory, or is not a
/// tree — none of which is an error, they are all just repositories with
/// nothing in `.weft/`.
///
/// Both readers below go through here so there is exactly one piece of
/// code that knows where `.weft/` is and what it means for it to be
/// missing.
fn entries(
    reader: &LayoutReader,
    rev: &str,
) -> Result<Option<Vec<stratum_engine::objwrite::TreeEntry>>, String> {
    let Some(commit) = reader.resolve_rev(rev)? else {
        return Ok(None);
    };
    let Some(entry) = reader.entry_at(&commit, DIR)? else {
        return Ok(Some(Vec::new()));
    };
    if !is_dir(&entry.mode) {
        // `.weft` exists and is a file. Not an error — it is a
        // repository that happens to have that name — but there are no
        // workflows in it.
        return Ok(Some(Vec::new()));
    }
    let (kind, data) = reader.object(&stratum_engine::objwrite::hex(&entry.oid))?;
    if kind != stratum_engine::objwrite::OBJ_TREE {
        return Ok(Some(Vec::new()));
    }
    Ok(Some(stratum_engine::objwrite::parse_tree(&data)?))
}

/// Read one entry's blob. `Ok(None)` means the entry is a directory or
/// the object is not a blob — both of which mean "there is no file here"
/// rather than "something went wrong".
///
/// A store failure stays an `Err` and is propagated. Swallowing it would
/// turn a transient read error into a workflow that silently does not
/// exist, which is a push that quietly runs no CI and an author with
/// nothing to look at.
///
/// Decoding is lossy rather than a refusal: a file that is not UTF-8 is
/// going to be refused by its parser anyway, and it should be refused
/// with a line number rather than with a decoding error nobody can act
/// on.
fn blob(
    reader: &LayoutReader,
    e: &stratum_engine::objwrite::TreeEntry,
) -> Result<Option<String>, String> {
    if is_dir(&e.mode) {
        return Ok(None);
    }
    let (kind, data) = reader.object(&stratum_engine::objwrite::hex(&e.oid))?;
    if kind != stratum_engine::objwrite::OBJ_BLOB {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&data).into_owned()))
}

/// Every workflow file under [`DIR`] at `rev`, as `(name, source)` in
/// tree order. `None` means the rev does not resolve; an empty vec means
/// the directory is absent, is a file, or holds no `.yml`/`.yaml`.
///
/// The reserved configuration names are not workflows and never appear
/// here — see [`SITE_CONFIG`].
pub fn read_dir(reader: &LayoutReader, rev: &str) -> Result<Option<Vec<(String, String)>>, String> {
    let Some(es) = entries(reader, rev)? else {
        return Ok(None);
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for e in es {
        if out.len() >= MAX_FILES {
            break;
        }
        if !is_workflow(&e.name) {
            continue;
        }
        let Some(src) = blob(reader, &e)? else {
            continue;
        };
        out.push((e.name.clone(), src));
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_files_are_yaml_by_extension_only() {
        assert!(is_workflow("ci.yml"));
        assert!(is_workflow("ci.yaml"));
        assert!(!is_workflow("ci.yml.bak"));
        assert!(!is_workflow("README"));
    }

    #[test]
    fn stem_drops_only_the_last_extension() {
        assert_eq!(stem("ci.yml"), "ci");
        assert_eq!(stem("release.v2.yaml"), "release.v2");
        assert_eq!(stem("noext"), "noext");
    }

    /// A site config is YAML in `.weft/`, so without the reservation it
    /// is a workflow with no `jobs:` and the trigger writes that refusal
    /// into the repository's checks.
    #[test]
    fn the_site_config_is_not_a_workflow() {
        for name in SITE_CONFIG {
            assert!(is_reserved(name), "{name} should be reserved");
            assert!(!is_workflow(name), "{name} should not be a workflow");
        }
    }

    /// Reserving `site.yml` must not reserve everything that merely
    /// looks like it. A workflow an author calls `site-deploy.yml` is
    /// still a workflow, and so is one in a directory named `site.yml`
    /// somewhere else — this predicate only ever sees a bare name.
    #[test]
    fn only_the_exact_reserved_names_are_reserved() {
        for name in [
            "site.yml.bak",
            "site-deploy.yml",
            "sites.yml",
            "my-site.yaml",
            "site.json",
            "site",
        ] {
            assert!(!is_reserved(name), "{name} should not be reserved");
        }
        assert!(is_workflow("site-deploy.yml"));
        assert!(is_workflow("sites.yml"));
    }
}
