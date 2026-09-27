//! OWNERS file grammar (v1) and the effective-rule computation.
//!
//! Pure string work, no I/O: the parser and the directory-chain walk are
//! exhaustively unit-testable here, and everything that needs a database
//! or an object store lives in `resolve` and `load`.
//!
//! Grammar, one directive per line:
//!
//! ```text
//! # comment (full-line, or trailing after the directive)
//! set noparent          # stop inheriting from parent directories
//! alice@example.com     # a person, by email
//! @payments             # a team, by name
//! *                     # anyone with write access to the repo
//! ```
//!
//! Inheritance is the default: the effective owners of `a/b/f.rs` are the
//! union of the entries in `a/b/OWNERS`, `a/OWNERS` and `OWNERS`, walking
//! from the deepest directory up and stopping at the first file that says
//! `set noparent`. A malformed line is a parse error, not a shrug — a
//! typo in an approval rule must block landing, never silently widen it.

/// One owner entry, as written. Resolution to people happens later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerEntry {
    /// An email address, lowercased.
    User(String),
    /// A team name, without the leading `@`, lowercased.
    Team(String),
    /// `*`: anyone with write access to the repo.
    Anyone,
}

impl OwnerEntry {
    /// The entry as it displays in requirements and explanations.
    pub fn display(&self) -> String {
        match self {
            OwnerEntry::User(e) => e.clone(),
            OwnerEntry::Team(t) => format!("@{t}"),
            OwnerEntry::Anyone => "*".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnersFile {
    pub entries: Vec<OwnerEntry>,
    pub noparent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line number of the offending line.
    pub line: usize,
    pub message: String,
}

/// Parse one OWNERS file. The first malformed line wins — reporting one
/// precise error beats a pile of cascading ones.
pub fn parse(text: &str) -> Result<OwnersFile, ParseError> {
    let mut entries: Vec<OwnerEntry> = Vec::new();
    let mut noparent = false;
    for (idx, raw) in text.lines().enumerate() {
        let line = idx + 1;
        // Strip a trailing comment: '#' ends the directive part. Emails
        // and team names never contain '#', so this cannot truncate a
        // legitimate entry.
        let body = raw.split('#').next().unwrap_or("").trim();
        if body.is_empty() {
            continue;
        }
        let mut tokens = body.split_whitespace();
        let first = tokens.next().expect("non-empty body has a token");
        let rest: Vec<&str> = tokens.collect();
        if first == "set" {
            if rest != ["noparent"] {
                return Err(ParseError {
                    line,
                    message: format!("expected \"set noparent\", got {body:?}"),
                });
            }
            noparent = true;
            continue;
        }
        if !rest.is_empty() {
            return Err(ParseError {
                line,
                message: format!("one directive per line, got {body:?}"),
            });
        }
        let entry = parse_entry(first).ok_or_else(|| ParseError {
            line,
            message: format!("unrecognized owner {first:?} (expected an email, @team, or *)"),
        })?;
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    Ok(OwnersFile { entries, noparent })
}

fn parse_entry(token: &str) -> Option<OwnerEntry> {
    if token == "*" {
        return Some(OwnerEntry::Anyone);
    }
    if let Some(team) = token.strip_prefix('@') {
        if team.is_empty() || team.contains('@') {
            return None;
        }
        return Some(OwnerEntry::Team(team.to_lowercase()));
    }
    // An email: exactly one '@', with something on both sides.
    let (local, domain) = token.split_once('@')?;
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return None;
    }
    Some(OwnerEntry::User(token.to_lowercase()))
}

/// One level of the effective-rule chain, for display: "these owners,
/// from this OWNERS file".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleLevel {
    /// Directory the OWNERS file sits in; "" is the repo root.
    pub dir: String,
    pub entries: Vec<OwnerEntry>,
    pub noparent: bool,
}

/// The effective rule for one path, or the parse error that poisons it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleOutcome {
    /// An OWNERS file on the governing chain does not parse. The path is
    /// blocked until someone fixes the file — failing closed is the only
    /// safe reading of a rule nobody can read.
    Error {
        dir: String,
        line: usize,
        message: String,
    },
    /// The assembled chain. `entries` is the deduplicated union in
    /// deepest-first order; empty means no OWNERS file governs the path.
    Rules {
        chain: Vec<RuleLevel>,
        entries: Vec<OwnerEntry>,
    },
}

/// Every ancestor directory of `path`, deepest first, ending with "".
/// `"a/b/f.rs"` → `["a/b", "a", ""]`.
pub fn ancestor_dirs(path: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut cur = path;
    while let Some((dir, _)) = cur.rsplit_once('/') {
        dirs.push(dir.to_string());
        cur = dir;
    }
    dirs.push(String::new());
    dirs
}

/// Assemble the effective rule for `path` from the parsed OWNERS files.
/// `files` maps a directory ("" = root) to that directory's parse result;
/// directories without an OWNERS file are simply absent.
pub fn effective_owners(
    path: &str,
    files: &std::collections::BTreeMap<String, Result<OwnersFile, ParseError>>,
) -> RuleOutcome {
    let mut chain = Vec::new();
    let mut entries: Vec<OwnerEntry> = Vec::new();
    for dir in ancestor_dirs(path) {
        let Some(parsed) = files.get(&dir) else {
            continue;
        };
        match parsed {
            Err(e) => {
                return RuleOutcome::Error {
                    dir,
                    line: e.line,
                    message: e.message.clone(),
                }
            }
            Ok(f) => {
                for e in &f.entries {
                    if !entries.contains(e) {
                        entries.push(e.clone());
                    }
                }
                chain.push(RuleLevel {
                    dir,
                    entries: f.entries.clone(),
                    noparent: f.noparent,
                });
                if f.noparent {
                    break;
                }
            }
        }
    }
    RuleOutcome::Rules { chain, entries }
}

/// A path is usable as a repo path in queries: relative, no empty or
/// dot-dot segments, no NUL or newline. Mirrors the commit API's path
/// discipline so hostile shapes never reach tree walks or SQL.
pub fn valid_repo_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 4096 {
        return false;
    }
    if path.contains('\0') || path.contains('\n') || path.starts_with('/') || path.ends_with('/') {
        return false;
    }
    path.split('/')
        .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn user(e: &str) -> OwnerEntry {
        OwnerEntry::User(e.to_string())
    }
    fn team(t: &str) -> OwnerEntry {
        OwnerEntry::Team(t.to_string())
    }

    #[test]
    fn parses_the_full_grammar() {
        let f = parse(
            "# payments owners\n\
             alice@example.com\n\
             \n\
             @payments   # the team\n\
             *\n\
             set noparent\n",
        )
        .unwrap();
        assert_eq!(
            f.entries,
            vec![
                user("alice@example.com"),
                team("payments"),
                OwnerEntry::Anyone
            ]
        );
        assert!(f.noparent);
    }

    #[test]
    fn empty_and_comment_only_files_parse_to_nothing() {
        for text in ["", "\n\n", "# just a comment\n  # another\n"] {
            let f = parse(text).unwrap();
            assert!(f.entries.is_empty());
            assert!(!f.noparent);
        }
    }

    #[test]
    fn emails_and_teams_are_case_folded_and_deduplicated() {
        let f = parse("Alice@Example.COM\nalice@example.com\n@Payments\n@payments\n").unwrap();
        assert_eq!(f.entries, vec![user("alice@example.com"), team("payments")]);
    }

    #[test]
    fn malformed_lines_error_with_their_line_number() {
        let cases = [
            ("alice@example.com\nnot-an-email\n", 2, "unrecognized owner"),
            ("@\n", 1, "unrecognized owner"),
            ("@a@b\n", 1, "unrecognized owner"),
            ("a@\n", 1, "unrecognized owner"),
            ("@team extra\n", 1, "one directive per line"),
            ("set\n", 1, "set noparent"),
            ("set noparent extra\n", 1, "set noparent"),
            ("a@b@c\n", 1, "unrecognized owner"),
        ];
        for (text, line, msg) in cases {
            let e = parse(text).unwrap_err();
            assert_eq!(e.line, line, "line for {text:?}");
            assert!(e.message.contains(msg), "{text:?} gave {:?}", e.message);
        }
    }

    #[test]
    fn trailing_comments_do_not_reach_the_directive() {
        let f = parse("alice@example.com # primary\nset noparent # sealed\n").unwrap();
        assert_eq!(f.entries, vec![user("alice@example.com")]);
        assert!(f.noparent);
    }

    #[test]
    fn ancestor_dirs_walk_deepest_first_to_root() {
        assert_eq!(ancestor_dirs("a/b/f.rs"), vec!["a/b", "a", ""]);
        assert_eq!(ancestor_dirs("f.rs"), vec![""]);
    }

    fn files(
        pairs: &[(&str, Result<OwnersFile, ParseError>)],
    ) -> BTreeMap<String, Result<OwnersFile, ParseError>> {
        pairs
            .iter()
            .map(|(d, r)| (d.to_string(), r.clone()))
            .collect()
    }

    #[test]
    fn inheritance_unions_the_chain_deepest_first() {
        let map = files(&[
            ("", Ok(parse("root@example.com").unwrap())),
            ("a", Ok(parse("mid@example.com").unwrap())),
            (
                "a/b",
                Ok(parse("deep@example.com\nroot@example.com").unwrap()),
            ),
        ]);
        let RuleOutcome::Rules { chain, entries } = effective_owners("a/b/f.rs", &map) else {
            panic!("expected rules");
        };
        assert_eq!(
            entries,
            vec![
                user("deep@example.com"),
                user("root@example.com"),
                user("mid@example.com"),
            ]
        );
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].dir, "a/b");
        assert_eq!(chain[2].dir, "");
    }

    #[test]
    fn noparent_stops_the_walk_at_its_level() {
        let map = files(&[
            ("", Ok(parse("root@example.com").unwrap())),
            ("a", Ok(parse("mid@example.com\nset noparent").unwrap())),
        ]);
        let RuleOutcome::Rules { chain, entries } = effective_owners("a/b/f.rs", &map) else {
            panic!("expected rules");
        };
        assert_eq!(entries, vec![user("mid@example.com")]);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].dir, "a");
        assert!(chain[0].noparent);
    }

    #[test]
    fn a_parse_error_on_the_chain_poisons_the_path() {
        let map = files(&[
            ("", Ok(parse("root@example.com").unwrap())),
            (
                "a",
                Err(ParseError {
                    line: 3,
                    message: "unrecognized owner \"???\"".into(),
                }),
            ),
        ]);
        match effective_owners("a/f.rs", &map) {
            RuleOutcome::Error { dir, line, .. } => {
                assert_eq!(dir, "a");
                assert_eq!(line, 3);
            }
            other => panic!("expected error, got {other:?}"),
        }
        // A sibling outside the poisoned directory is unaffected.
        match effective_owners("b/f.rs", &map) {
            RuleOutcome::Rules { entries, .. } => {
                assert_eq!(entries, vec![user("root@example.com")]);
            }
            other => panic!("expected rules, got {other:?}"),
        }
    }

    #[test]
    fn ungoverned_paths_produce_an_empty_rule() {
        let map = files(&[]);
        let RuleOutcome::Rules { chain, entries } = effective_owners("x/y.rs", &map) else {
            panic!("expected rules");
        };
        assert!(chain.is_empty());
        assert!(entries.is_empty());
    }

    #[test]
    fn valid_repo_path_rejects_hostile_shapes() {
        for good in ["a", "a/b/c.rs", "OWNERS", "a-b_c.d/e"] {
            assert!(valid_repo_path(good), "{good:?}");
        }
        let long = "a/".repeat(3000);
        for bad in [
            "",
            "/abs",
            "trail/",
            "a//b",
            "a/../b",
            "./a",
            "a/.",
            "..",
            "nul\0byte",
            "new\nline",
            long.as_str(),
        ] {
            assert!(!valid_repo_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn entry_display_round_trips_the_source_form() {
        assert_eq!(user("a@b.c").display(), "a@b.c");
        assert_eq!(team("payments").display(), "@payments");
        assert_eq!(OwnerEntry::Anyone.display(), "*");
    }
}
