//! `.weft/site.yml` — what this repository publishes as a static site.
//!
//! The same hand-written walk as [`super::parse`], over the same
//! [`Node`] scaffolding, for the same reason: a reader who mistypes a
//! key should be told the key, the line, and what the alternatives are,
//! rather than meeting a deserializer's "unknown field" with no position
//! in it.
//!
//! This is deliberately not `serde`. A site config is the first thing a
//! new user writes, and it is the file they write while their site is
//! not working yet — so every refusal here is a support conversation
//! that did not happen.
//!
//! **Scope.** Only the keys the no-build path actually uses are accepted
//! today. `build:`, `redirects:`, `headers:` and `previews:` arrive with
//! the slices that implement them, because a key this parser accepts and
//! nothing honours is worse than one it refuses: the refusal is a
//! message, the silent acceptance is a bug report.

use super::model::Refusal;
use super::parse::{as_map, as_scalar, load, unknown, Node};

/// The largest site config we will look at. A site config is smaller
/// than a workflow; the point is that there is a bound at all.
pub const MAX_BYTES: usize = 16 * 1024;

/// The keys this parser knows, named in refusals so a mistype is one
/// message rather than a documentation hunt.
const KEYS: [&str; 4] = ["publish", "branch", "spa", "not-found"];

/// The directory published when the file does not say. Matches what
/// every mainstream front-end toolchain writes by default.
pub const DEFAULT_PUBLISH: &str = "dist";

/// What a repository publishes, and how it is served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteConfig {
    /// Directory within the repository whose contents are the site.
    pub publish: String,
    /// The ref whose pushes publish. `None` means the repository's
    /// default branch, resolved when a push arrives rather than here —
    /// this parser has no repository to ask.
    pub branch: Option<String>,
    /// Serve `index.html` with a 200 for a path that matches no file,
    /// the way a client-side router needs.
    pub spa: bool,
    /// The page served for a miss when [`SiteConfig::spa`] is false.
    /// `None` means the built-in.
    pub not_found: Option<String>,
}

impl Default for SiteConfig {
    fn default() -> SiteConfig {
        SiteConfig {
            publish: DEFAULT_PUBLISH.to_string(),
            branch: None,
            spa: false,
            not_found: None,
        }
    }
}

/// Read a boolean the way a person writes one.
///
/// YAML's own boolean rules are famously wide (`yes`, `on`, `y`), and
/// this subset deliberately does not inherit them: `on` is a workflow
/// trigger three files away, and a config language where `on` sometimes
/// means `true` is one nobody can read confidently. Two spellings, and
/// anything else is a refusal that names them.
fn flag(node: &Node, key: &str) -> Result<bool, Refusal> {
    match as_scalar(node, key)? {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(Refusal::at(
            Some(node.line()),
            key,
            format!("{key} must be true or false, not {other:?}"),
        )
        .hint("write `true` or `false`")),
    }
}

/// A path inside the repository, checked for the things that would make
/// it mean something outside it.
///
/// This is a security boundary, not tidiness. `publish` and `not-found`
/// both become part of a store key and a tree lookup, and a value that
/// climbs out of the repository or names an absolute location is the
/// difference between publishing a directory and publishing whatever
/// the server can reach.
fn repo_path(value: &str, key: &str, line: usize) -> Result<String, Refusal> {
    let refuse = |msg: &str, hint: &str| {
        Err(Refusal::at(Some(line), key, format!("{key} {msg}")).hint(hint.to_string()))
    };
    if value.is_empty() {
        return refuse(
            "is empty",
            "name a directory in this repository, like `dist`",
        );
    }
    if value.starts_with('/') {
        return refuse(
            "must be relative to the repository, not absolute",
            "drop the leading `/`",
        );
    }
    if value.starts_with('~') {
        return refuse(
            "must be a path in this repository, not a home directory",
            "name a directory that is committed, like `dist`",
        );
    }
    if value.contains('\\') {
        return refuse(
            "must use `/` between segments",
            "a repository path is separated by `/` on every platform",
        );
    }
    if value.contains('\0') {
        return refuse("must not contain a NUL byte", "remove it");
    }
    // Normalise *before* checking segments, not after. A trailing slash
    // is what a person naturally writes for a directory and means the
    // same thing; a leading `./` likewise. Checking first would refuse
    // `./dist` for containing a `.` segment, which is a refusal with no
    // defensible reason behind it.
    let trimmed = value
        .strip_prefix("./")
        .unwrap_or(value)
        .trim_end_matches('/');
    if trimmed.is_empty() {
        return refuse(
            "names the repository root rather than a directory in it",
            "publish a directory, like `dist`",
        );
    }
    // Now the segment rules, on the path that will actually be used.
    // `..` is the one that matters: it is the difference between
    // publishing a directory and publishing whatever is above it.
    if trimmed.split('/').any(|p| p == "..") {
        return refuse(
            "must not climb out of the repository with `..`",
            "name a directory inside it",
        );
    }
    if trimmed.split('/').any(|p| p == ".") {
        return refuse(
            "must not contain a `.` segment",
            "write the path without it",
        );
    }
    if trimmed.split('/').any(|p| p.is_empty()) {
        return refuse(
            "must not contain an empty segment",
            "write one `/` between directories",
        );
    }
    Ok(trimmed.to_string())
}

/// Parse `.weft/site.yml`.
pub fn parse(src: &str) -> Result<SiteConfig, Refusal> {
    if src.len() > MAX_BYTES {
        return Err(Refusal::at(
            None,
            "",
            format!("this file is {} bytes; the limit is {MAX_BYTES}", src.len()),
        )
        .hint("a site config is a few lines, not a payload"));
    }
    let root = load(src)?;
    let top = as_map(&root, "the site config")?;

    let mut cfg = SiteConfig::default();
    let mut saw_publish = false;

    for (key, line, value) in top {
        match key.as_str() {
            "publish" => {
                let raw = as_scalar(value, "publish")?;
                cfg.publish = repo_path(raw, "publish", value.line())?;
                saw_publish = true;
            }
            "branch" => {
                let raw = as_scalar(value, "branch")?;
                if raw.is_empty() {
                    return Err(Refusal::at(Some(value.line()), "branch", "branch is empty")
                        .hint("name a branch, or leave the key out for the default branch"));
                }
                cfg.branch = Some(raw.to_string());
            }
            "spa" => cfg.spa = flag(value, "spa")?,
            "not-found" => {
                let raw = as_scalar(value, "not-found")?;
                cfg.not_found = Some(repo_path(raw, "not-found", value.line())?);
            }
            other => return Err(unknown(other, "site", *line, &KEYS)),
        }
    }

    // `spa: true` answers every miss with `index.html`, so a 404 page
    // could never be reached. Refusing beats quietly ignoring one of the
    // two keys the author wrote.
    if cfg.spa && cfg.not_found.is_some() {
        return Err(Refusal::at(
            None,
            "not-found",
            "`spa: true` already answers every unmatched path with index.html, \
             so `not-found` could never be served",
        )
        .hint("drop one of them"));
    }
    if !saw_publish {
        // Not a refusal: `dist` is right for almost every toolchain, and
        // a config that is allowed to be empty is one people write.
        cfg.publish = DEFAULT_PUBLISH.to_string();
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> SiteConfig {
        parse(src).expect("should parse")
    }

    fn err(src: &str) -> Refusal {
        parse(src).expect_err("should be refused")
    }

    #[test]
    fn an_empty_config_publishes_dist_from_the_default_branch() {
        let c = ok("publish: dist\n");
        assert_eq!(c.publish, "dist");
        assert_eq!(c.branch, None);
        assert!(!c.spa);
        assert_eq!(c.not_found, None);
    }

    #[test]
    fn every_key_is_read() {
        let c = ok("publish: build\nbranch: release\nspa: true\n");
        assert_eq!(c.publish, "build");
        assert_eq!(c.branch.as_deref(), Some("release"));
        assert!(c.spa);
    }

    #[test]
    fn a_missing_publish_defaults_rather_than_refusing() {
        assert_eq!(ok("branch: main\n").publish, DEFAULT_PUBLISH);
    }

    #[test]
    fn a_directory_path_may_be_written_the_way_a_person_writes_it() {
        assert_eq!(ok("publish: dist/\n").publish, "dist");
        assert_eq!(ok("publish: ./dist\n").publish, "dist");
        assert_eq!(ok("publish: ./public/site/\n").publish, "public/site");
    }

    /// The security boundary. Each of these becomes a tree lookup and a
    /// store key, so each has to be refused before it is either.
    #[test]
    fn a_publish_path_may_not_leave_the_repository() {
        for bad in [
            "publish: /etc\n",
            "publish: ../../etc\n",
            "publish: dist/../../..\n",
            "publish: ~/secrets\n",
            "publish: dist\\win\n",
            "publish: \"\"\n",
            "publish: /\n",
            "publish: ./\n",
            "publish: a//b\n",
            "publish: dist/./x\n",
        ] {
            let r = err(bad);
            assert_eq!(r.key, "publish", "for {bad:?}");
            assert!(!r.hint.is_empty(), "every refusal needs a way out: {bad:?}");
        }
    }

    /// A NUL in a path is refused, and the guard is tested directly
    /// because nothing can reach it through `parse`: the YAML reader
    /// rejects a NUL in the document first, with a refusal about the
    /// document rather than about the key.
    ///
    /// Kept anyway, and tested here rather than deleted, because
    /// `repo_path` is a security boundary and the thing above it is a
    /// third-party parser. A path that carries a NUL is the shape of an
    /// attempt to truncate a name somewhere further down, and this guard
    /// should not depend on somebody else's tokeniser to catch it.
    #[test]
    fn a_nul_byte_in_a_path_is_refused_by_the_guard_itself() {
        let r = repo_path("di\0st", "publish", 1).expect_err("should be refused");
        assert_eq!(r.key, "publish");
        assert!(r.message.contains("NUL"), "{}", r.message);

        // And through `parse`, the document is refused before the key
        // is ever read — which is why the guard needs its own test.
        let doc = parse("publish: di\0st\n").expect_err("should be refused");
        assert_eq!(doc.key, "", "the reader refuses the document, not the key");
    }

    #[test]
    fn not_found_is_held_to_the_same_boundary() {
        let r = err("not-found: ../outside.html\n");
        assert_eq!(r.key, "not-found");
    }

    #[test]
    fn an_unknown_key_names_the_ones_that_exist() {
        let r = err("pubish: dist\n");
        assert_eq!(r.line, Some(1));
        for k in KEYS {
            assert!(r.hint.contains(k), "hint should name {k}: {}", r.hint);
        }
    }

    /// The keys deferred to later slices must refuse *now*, so that a
    /// config written today does not silently do nothing.
    #[test]
    fn keys_from_later_slices_are_refused_rather_than_ignored() {
        for src in [
            "build: site\n",
            "previews: true\n",
            "redirects:\n  - from: /a\n",
            "headers:\n  - for: /a\n",
        ] {
            let r = parse(src).expect_err("should be refused until implemented");
            assert!(!r.hint.is_empty());
        }
    }

    #[test]
    fn a_boolean_is_true_or_false_and_not_yamls_wider_set() {
        assert!(ok("spa: true\n").spa);
        assert!(!ok("spa: false\n").spa);
        for bad in ["spa: yes\n", "spa: on\n", "spa: 1\n", "spa: True\n"] {
            let r = err(bad);
            assert_eq!(r.key, "spa");
            assert!(r.hint.contains("true"));
        }
    }

    #[test]
    fn spa_and_not_found_together_are_refused_rather_than_one_ignored() {
        let r = err("spa: true\nnot-found: 404.html\n");
        assert_eq!(r.key, "not-found");
    }

    #[test]
    fn spa_with_not_found_is_fine_when_spa_is_off() {
        let c = ok("spa: false\nnot-found: 404.html\n");
        assert_eq!(c.not_found.as_deref(), Some("404.html"));
    }

    #[test]
    fn an_empty_branch_is_refused() {
        assert_eq!(err("branch: \"\"\n").key, "branch");
    }

    #[test]
    fn a_key_that_should_be_a_value_says_so() {
        let r = err("publish:\n  - dist\n");
        assert_eq!(r.key, "publish");
        assert!(r.message.contains("must be a value"));
    }

    #[test]
    fn an_oversized_file_is_refused_by_size_not_by_parsing_it() {
        let big = format!("publish: {}\n", "d".repeat(MAX_BYTES));
        let r = parse(&big).expect_err("should be refused");
        assert!(r.message.contains("the limit is"));
    }
}
