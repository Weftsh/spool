//! The sufficiency engine: given the changed paths' requirements and a
//! set of approvers, is the change landable, and — either way — exactly
//! why? Pure data-in data-out so every verdict branch is unit-tested
//! here; resolution of OWNERS entries to people happens in `resolve`.
//!
//! The rule: a change is landable when, for every touched path, at least
//! one approver satisfies that path's requirement. The verdict's
//! explanation strings are product surface — the dashboard and API show
//! them verbatim — so their exact shapes are pinned by tests.

use std::collections::BTreeSet;

/// Someone whose approval is on the table, with the email the
/// explanation strings display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approver {
    pub user_id: String,
    pub email: String,
}

#[derive(Debug, Clone, Default)]
pub struct ApproverSet {
    pub approvers: Vec<Approver>,
    /// Everyone with effective write access to the repo, by user id.
    /// Satisfies `*` entries and paths no OWNERS file governs.
    pub writers: BTreeSet<String>,
}

/// What one path demands, after OWNERS resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requirement {
    /// An OWNERS file on the governing chain does not parse: blocked
    /// until fixed. A rule nobody can read must fail closed.
    OwnersError {
        dir: String,
        line: usize,
        message: String,
    },
    /// Governed by OWNERS entries. `owner_ids` is every user id that
    /// counts as an owner (direct or via a team); `display` is the
    /// entries as written; `anyone_with_write` is a `*` entry.
    Owned {
        owner_ids: BTreeSet<String>,
        display: Vec<String>,
        anyone_with_write: bool,
    },
    /// No OWNERS file governs the path: any approval with write access.
    Ungoverned,
}

#[derive(Debug, Clone)]
pub struct PathRequirement {
    pub path: String,
    pub requirement: Requirement,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathVerdict {
    pub path: String,
    pub satisfied: bool,
    /// The governing entries as written ("alice@…", "@payments", "*");
    /// empty for ungoverned paths.
    pub owners: Vec<String>,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub landable: bool,
    pub explanation: String,
    pub per_path: Vec<PathVerdict>,
}

fn owners_file_path(dir: &str) -> String {
    if dir.is_empty() {
        "OWNERS".to_string()
    } else {
        format!("{dir}/OWNERS")
    }
}

pub fn evaluate(paths: &[PathRequirement], approvers: &ApproverSet) -> Verdict {
    if paths.is_empty() {
        return Verdict {
            landable: true,
            explanation: "ok: no changed paths".to_string(),
            per_path: Vec::new(),
        };
    }
    let per_path: Vec<PathVerdict> = paths.iter().map(|p| judge(p, approvers)).collect();
    let landable = per_path.iter().all(|v| v.satisfied);
    let explanation = if landable {
        format!("ok: all {} changed path(s) approved", per_path.len())
    } else {
        per_path
            .iter()
            .find(|v| !v.satisfied)
            .map(|v| v.explanation.clone())
            .expect("not landable means some path is unsatisfied")
    };
    Verdict {
        landable,
        explanation,
        per_path,
    }
}

fn judge(p: &PathRequirement, set: &ApproverSet) -> PathVerdict {
    let writer_approver = || {
        set.approvers
            .iter()
            .find(|a| set.writers.contains(&a.user_id))
    };
    match &p.requirement {
        Requirement::OwnersError { dir, line, message } => PathVerdict {
            path: p.path.clone(),
            satisfied: false,
            owners: Vec::new(),
            explanation: format!(
                "blocked: OWNERS parse error at {}:{line} ({message})",
                owners_file_path(dir)
            ),
        },
        Requirement::Owned {
            owner_ids,
            display,
            anyone_with_write,
        } => {
            let by_owner = set
                .approvers
                .iter()
                .find(|a| owner_ids.contains(&a.user_id));
            let (satisfied, explanation) = if let Some(a) = by_owner {
                (true, format!("ok: /{} approved by {}", p.path, a.email))
            } else if let Some(a) = anyone_with_write.then(writer_approver).flatten() {
                (
                    true,
                    format!("ok: /{} approved by {} (write access)", p.path, a.email),
                )
            } else {
                (
                    false,
                    format!(
                        "blocked: needs an owner of /{} (owners: {})",
                        p.path,
                        display.join(", ")
                    ),
                )
            };
            PathVerdict {
                path: p.path.clone(),
                satisfied,
                owners: display.clone(),
                explanation,
            }
        }
        Requirement::Ungoverned => {
            let (satisfied, explanation) = match writer_approver() {
                Some(a) => (
                    true,
                    format!(
                        "ok: /{} approved by {} (write access; no OWNERS rule governs it)",
                        p.path, a.email
                    ),
                ),
                None => (
                    false,
                    format!(
                        "blocked: /{} needs any approval with write access (no OWNERS rule governs it)",
                        p.path
                    ),
                ),
            };
            PathVerdict {
                path: p.path.clone(),
                satisfied,
                owners: Vec::new(),
                explanation,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approver(id: &str, email: &str) -> Approver {
        Approver {
            user_id: id.to_string(),
            email: email.to_string(),
        }
    }

    fn owned(path: &str, owner_ids: &[&str], display: &[&str], anyone: bool) -> PathRequirement {
        PathRequirement {
            path: path.to_string(),
            requirement: Requirement::Owned {
                owner_ids: owner_ids.iter().map(|s| s.to_string()).collect(),
                display: display.iter().map(|s| s.to_string()).collect(),
                anyone_with_write: anyone,
            },
        }
    }

    fn set(approvers: Vec<Approver>, writers: &[&str]) -> ApproverSet {
        ApproverSet {
            approvers,
            writers: writers.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn no_changed_paths_is_trivially_landable() {
        let v = evaluate(&[], &ApproverSet::default());
        assert!(v.landable);
        assert_eq!(v.explanation, "ok: no changed paths");
        assert!(v.per_path.is_empty());
    }

    #[test]
    fn an_owner_approval_satisfies_the_path() {
        let paths = [owned(
            "payments/gateway.rs",
            &["u1"],
            &["alice@example.com", "@payments"],
            false,
        )];
        let v = evaluate(&paths, &set(vec![approver("u1", "alice@example.com")], &[]));
        assert!(v.landable);
        assert_eq!(v.explanation, "ok: all 1 changed path(s) approved");
        assert_eq!(
            v.per_path[0].explanation,
            "ok: /payments/gateway.rs approved by alice@example.com"
        );
        assert_eq!(v.per_path[0].owners, vec!["alice@example.com", "@payments"]);
    }

    #[test]
    fn a_non_owner_approval_blocks_with_the_owner_list() {
        let paths = [owned(
            "payments/gateway.rs",
            &["u1"],
            &["alice@example.com", "@payments"],
            false,
        )];
        let v = evaluate(
            &paths,
            &set(vec![approver("u2", "bob@example.com")], &["u2"]),
        );
        assert!(!v.landable);
        assert_eq!(
            v.explanation,
            "blocked: needs an owner of /payments/gateway.rs (owners: alice@example.com, @payments)"
        );
    }

    #[test]
    fn star_lets_any_writer_approve_but_never_a_non_writer() {
        let paths = [owned("docs/readme.md", &["u1"], &["*"], true)];
        let with_writer = evaluate(
            &paths,
            &set(vec![approver("u9", "casey@example.com")], &["u9"]),
        );
        assert!(with_writer.landable);
        assert_eq!(
            with_writer.per_path[0].explanation,
            "ok: /docs/readme.md approved by casey@example.com (write access)"
        );
        // The same approval without write access does not count.
        let without = evaluate(&paths, &set(vec![approver("u9", "casey@example.com")], &[]));
        assert!(!without.landable);
    }

    #[test]
    fn an_owner_match_is_preferred_over_a_writer_match_in_the_explanation() {
        let paths = [owned("a.rs", &["u2"], &["bob@example.com", "*"], true)];
        let v = evaluate(
            &paths,
            &set(
                vec![
                    approver("u1", "writer@example.com"),
                    approver("u2", "bob@example.com"),
                ],
                &["u1", "u2"],
            ),
        );
        assert!(v.landable);
        assert_eq!(
            v.per_path[0].explanation,
            "ok: /a.rs approved by bob@example.com"
        );
    }

    #[test]
    fn ungoverned_paths_need_any_writer_approval() {
        let paths = [PathRequirement {
            path: "scratch/notes.txt".to_string(),
            requirement: Requirement::Ungoverned,
        }];
        let blocked = evaluate(&paths, &set(vec![], &["u1"]));
        assert!(!blocked.landable);
        assert_eq!(
            blocked.explanation,
            "blocked: /scratch/notes.txt needs any approval with write access (no OWNERS rule governs it)"
        );
        let viewer_only = evaluate(&paths, &set(vec![approver("u3", "view@example.com")], &[]));
        assert!(
            !viewer_only.landable,
            "a non-writer approval must not count"
        );
        let ok = evaluate(
            &paths,
            &set(vec![approver("u1", "dev@example.com")], &["u1"]),
        );
        assert!(ok.landable);
        assert_eq!(
            ok.per_path[0].explanation,
            "ok: /scratch/notes.txt approved by dev@example.com (write access; no OWNERS rule governs it)"
        );
    }

    #[test]
    fn a_parse_error_blocks_and_names_the_file_and_line() {
        let paths = [PathRequirement {
            path: "payments/x.rs".to_string(),
            requirement: Requirement::OwnersError {
                dir: "payments".to_string(),
                line: 3,
                message: "unrecognized owner \"???\"".to_string(),
            },
        }];
        // Even an owner-of-everything approval cannot get past it.
        let v = evaluate(&paths, &set(vec![approver("u1", "a@b.c")], &["u1"]));
        assert!(!v.landable);
        assert_eq!(
            v.explanation,
            "blocked: OWNERS parse error at payments/OWNERS:3 (unrecognized owner \"???\")"
        );
        // Root-level errors name the bare OWNERS file.
        let root = [PathRequirement {
            path: "x.rs".to_string(),
            requirement: Requirement::OwnersError {
                dir: String::new(),
                line: 1,
                message: "m".to_string(),
            },
        }];
        let rv = evaluate(&root, &ApproverSet::default());
        assert!(rv.explanation.contains("at OWNERS:1"));
    }

    #[test]
    fn every_path_must_be_satisfied_and_the_first_block_leads() {
        let paths = [
            owned("a/x.rs", &["u1"], &["alice@example.com"], false),
            owned("b/y.rs", &["u2"], &["bob@example.com"], false),
        ];
        let v = evaluate(&paths, &set(vec![approver("u1", "alice@example.com")], &[]));
        assert!(!v.landable);
        assert!(v.per_path[0].satisfied);
        assert!(!v.per_path[1].satisfied);
        assert_eq!(
            v.explanation,
            "blocked: needs an owner of /b/y.rs (owners: bob@example.com)"
        );
        let both = evaluate(
            &paths,
            &set(
                vec![
                    approver("u1", "alice@example.com"),
                    approver("u2", "bob@example.com"),
                ],
                &[],
            ),
        );
        assert!(both.landable);
        assert_eq!(both.explanation, "ok: all 2 changed path(s) approved");
    }

    #[test]
    fn entries_that_resolve_to_nobody_still_block_and_display() {
        // A ghost email in OWNERS resolves to no user id: the path is
        // unsatisfiable until the file is fixed, and the display says
        // exactly what the file says.
        let paths = [owned("a.rs", &[], &["ghost@example.com"], false)];
        let v = evaluate(&paths, &set(vec![approver("u1", "a@b.c")], &["u1"]));
        assert!(!v.landable);
        assert_eq!(
            v.explanation,
            "blocked: needs an owner of /a.rs (owners: ghost@example.com)"
        );
    }
}
