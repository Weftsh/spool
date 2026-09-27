//! Branch protection: which branches move only through the land queue.
//!
//! A protected branch cannot be pushed, reset, reverted or deleted by
//! anyone — including the admin who protected it. The one writer left is
//! the lander's ref transaction, which re-checks approval sufficiency at
//! claim time. That is the entire feature: review is only real when the
//! verdict is the *only* way trunk moves.
//!
//! Protecting and unprotecting are authority moves, so both are audited
//! in the same transaction that flips the row.

use crate::db::ControlDb;
use crate::ids::now_ms;

/// One protected branch, as the settings screen lists it.
#[derive(Debug, Clone)]
pub struct Protection {
    pub branch: String,
    pub created_by: Option<String>,
    pub created_at: i64,
}

/// The sentence every refusal uses, verbatim, at every door — wire push,
/// SSH push, the commits API, refops. One message, greppable in support
/// tickets and pinned by tests, instead of four paraphrases.
pub fn refusal(branch: &str) -> String {
    format!("branch '{branch}' is protected: land through review")
}

/// The same shape rule the changes API applies to a landing target: a
/// branch name is short, printable and free of git's revision metachars.
pub fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("..")
}

/// Protect a branch. Returns whether the row is new — protecting twice
/// is an ack, not an error, so a settings screen can be retried safely.
pub fn protect(
    db: &ControlDb,
    repo_id: &str,
    branch: &str,
    audit: &crate::audit::AuditCtx,
) -> Result<bool, String> {
    if !valid_branch(branch) {
        return Err(format!("invalid branch {branch:?}"));
    }
    let now = now_ms();
    let created_by = audit.user_id.clone();
    db.lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "INSERT INTO branch_protections (repo_id, branch, created_by, created_at) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
                &[&repo_id, &branch, &created_by, &now],
            )?;
            if n > 0 {
                let blob = serde_json::json!({ "branch": branch });
                crate::audit::record_tx(tx, audit, Some(repo_id), "repo.protect", Some(&blob))?;
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("protect: {e}"))
}

/// Remove a protection. Returns whether one existed.
pub fn unprotect(
    db: &ControlDb,
    repo_id: &str,
    branch: &str,
    audit: &crate::audit::AuditCtx,
) -> Result<bool, String> {
    if !valid_branch(branch) {
        return Ok(false);
    }
    db.lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "DELETE FROM branch_protections WHERE repo_id = $1 AND branch = $2",
                &[&repo_id, &branch],
            )?;
            if n > 0 {
                let blob = serde_json::json!({ "branch": branch });
                crate::audit::record_tx(tx, audit, Some(repo_id), "repo.unprotect", Some(&blob))?;
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("unprotect: {e}"))
}

/// Every protection on a repo, alphabetical — the settings screen.
pub fn list(db: &ControlDb, repo_id: &str) -> Result<Vec<Protection>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT branch, created_by, created_at FROM branch_protections \
             WHERE repo_id = $1 ORDER BY branch",
            &[&repo_id],
        )
        .map_err(|e| format!("protections: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| Protection {
            branch: r.get("branch"),
            created_by: r.get("created_by"),
            created_at: r.get("created_at"),
        })
        .collect())
}

/// Just the branch names — what the push doors check against. Kept as
/// one cheap query so both wire fronts can afford it on every push.
pub fn protected_branches(db: &ControlDb, repo_id: &str) -> Result<Vec<String>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT branch FROM branch_protections WHERE repo_id = $1 ORDER BY branch",
            &[&repo_id],
        )
        .map_err(|e| format!("protections: {e}"))?;
    Ok(rows.iter().map(|r| r.get("branch")).collect())
}

/// Is this one branch protected? The REST write seams ask before acting.
pub fn is_protected(db: &ControlDb, repo_id: &str, branch: &str) -> Result<bool, String> {
    if !valid_branch(branch) {
        return Ok(false);
    }
    db.lock()
        .query_opt(
            "SELECT 1 AS x FROM branch_protections WHERE repo_id = $1 AND branch = $2",
            &[&repo_id, &branch],
        )
        .map(|r| r.is_some())
        .map_err(|e| format!("protections: {e}"))
}

/// One required check, as the branch's settings row lists it.
#[derive(Debug, Clone)]
pub struct RequiredCheck {
    pub branch: String,
    pub name: String,
    pub created_by: Option<String>,
    pub created_at: i64,
}

/// The longest check name that can be required.
///
/// Deliberately [`crate::changes::MAX_CHECK_NAME`] and not a second
/// number: a required name has to be *satisfiable*, and the intake
/// refuses to store a longer one, so a longer requirement could only
/// ever be a gate nothing can open.
pub const MAX_REQUIRED_CHECK_NAME: usize = crate::changes::MAX_CHECK_NAME;

/// Is this a name an admin can require?
///
/// **Not** `changes::valid_check_name`, and that is the whole decision.
/// That rule is `[A-Za-z0-9-_./:]`, which is right for the intake —
/// those names are minted by whoever holds the service token. The other
/// half of the gate is `check_runs`, whose names are GitHub *workflow*
/// names, and those are prose: `Build and test`, `CI / lint
/// (ubuntu-latest)`. Reusing the intake rule here would make every
/// mirrored Actions workflow unrequirable, which is precisely the
/// repository this feature exists for.
///
/// So the rule is the widest one that still keeps a name a name:
///
/// - non-empty, and at most [`MAX_REQUIRED_CHECK_NAME`] bytes;
/// - **no control characters** — a newline or a NUL in a required name
///   is either a mistake or an attempt to make one row print as two in
///   a settings list or a land-gate reason, both of which a person
///   reads and acts on;
/// - no leading or trailing whitespace, because `"ci "` and `"ci"` are
///   two rows under the primary key and one name to a reader, so an
///   admin could require a check that looks exactly like the one that
///   is passing and never be able to see why it never goes green.
pub fn valid_required_check_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_REQUIRED_CHECK_NAME
        && !name.chars().any(char::is_control)
        && name.trim() == name
}

/// Require a named check on a branch. Returns whether the row is new —
/// requiring twice acks, like [`protect`], so a settings screen retries
/// safely.
pub fn require_check(
    db: &ControlDb,
    repo_id: &str,
    branch: &str,
    name: &str,
    audit: &crate::audit::AuditCtx,
) -> Result<bool, String> {
    if !valid_branch(branch) {
        return Err(format!("invalid branch {branch:?}"));
    }
    if !valid_required_check_name(name) {
        return Err(format!("invalid check name {name:?}"));
    }
    let now = now_ms();
    let created_by = audit.user_id.clone();
    db.lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "INSERT INTO required_checks (repo_id, branch, name, created_by, created_at) \
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
                &[&repo_id, &branch, &name, &created_by, &now],
            )?;
            if n > 0 {
                let blob = serde_json::json!({ "branch": branch, "check": name });
                crate::audit::record_tx(
                    tx,
                    audit,
                    Some(repo_id),
                    "repo.require_check",
                    Some(&blob),
                )?;
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("require check: {e}"))
}

/// Stop requiring a check. Returns whether one was required.
pub fn unrequire_check(
    db: &ControlDb,
    repo_id: &str,
    branch: &str,
    name: &str,
    audit: &crate::audit::AuditCtx,
) -> Result<bool, String> {
    // A shape that cannot be stored cannot be present, so removing one
    // is an honest `false` rather than an error — same reasoning as
    // `unprotect`, and it keeps a settings screen idempotent.
    if !valid_branch(branch) || !valid_required_check_name(name) {
        return Ok(false);
    }
    db.lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "DELETE FROM required_checks WHERE repo_id = $1 AND branch = $2 AND name = $3",
                &[&repo_id, &branch, &name],
            )?;
            if n > 0 {
                let blob = serde_json::json!({ "branch": branch, "check": name });
                crate::audit::record_tx(
                    tx,
                    audit,
                    Some(repo_id),
                    "repo.unrequire_check",
                    Some(&blob),
                )?;
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("unrequire check: {e}"))
}

/// Every check required on one branch, alphabetical.
///
/// The land gate reads this on every evaluation, so it is one indexed
/// point query on the primary key's prefix, and an empty answer — the
/// overwhelmingly common one — costs exactly that.
pub fn required_checks(
    db: &ControlDb,
    repo_id: &str,
    branch: &str,
) -> Result<Vec<RequiredCheck>, String> {
    if !valid_branch(branch) {
        return Ok(Vec::new());
    }
    let rows = db
        .lock()
        .query(
            "SELECT branch, name, created_by, created_at FROM required_checks \
             WHERE repo_id = $1 AND branch = $2 ORDER BY name",
            &[&repo_id, &branch],
        )
        .map_err(|e| format!("required checks: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| RequiredCheck {
            branch: r.get("branch"),
            name: r.get("name"),
            created_by: r.get("created_by"),
            created_at: r.get("created_at"),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditCtx;
    use crate::registry::{self, NewRepo, RepoKind};

    fn world(hint: &str) -> (ControlDb, String, AuditCtx) {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let repo = registry::create_repo(
            &db,
            &org.id,
            &NewRepo {
                name: "app",
                kind: RepoKind::Native,
                public: false,
                description: None,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        let ctx = AuditCtx {
            principal: "user:test".into(),
            user_id: None,
            org_id: org.id,
        };
        (db, repo.id, ctx)
    }

    #[test]
    fn protect_is_idempotent_listed_and_audited() {
        let (db, repo, ctx) = world("prot-basic");
        assert!(protect(&db, &repo, "main", &ctx).unwrap());
        // Second protect acks without a second row or a second audit line.
        assert!(!protect(&db, &repo, "main", &ctx).unwrap());
        assert!(protect(&db, &repo, "release/1.0", &ctx).unwrap());

        let listed = list(&db, &repo).unwrap();
        assert_eq!(
            listed.iter().map(|p| p.branch.as_str()).collect::<Vec<_>>(),
            vec!["main", "release/1.0"],
        );
        assert!(is_protected(&db, &repo, "main").unwrap());
        assert!(!is_protected(&db, &repo, "dev").unwrap());
        assert_eq!(
            protected_branches(&db, &repo).unwrap(),
            vec!["main".to_string(), "release/1.0".to_string()],
        );

        let audit = crate::audit::query(
            &db,
            &ctx.org_id,
            &crate::audit::AuditQuery {
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
        let protects = audit.iter().filter(|e| e.action == "repo.protect").count();
        assert_eq!(protects, 2, "one audit line per protection, not per call");
    }

    #[test]
    fn unprotect_removes_and_audits_only_real_rows() {
        let (db, repo, ctx) = world("prot-remove");
        protect(&db, &repo, "main", &ctx).unwrap();
        assert!(unprotect(&db, &repo, "main", &ctx).unwrap());
        assert!(!unprotect(&db, &repo, "main", &ctx).unwrap());
        assert!(!is_protected(&db, &repo, "main").unwrap());
        let audit = crate::audit::query(
            &db,
            &ctx.org_id,
            &crate::audit::AuditQuery {
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
        let unprotects = audit
            .iter()
            .filter(|e| e.action == "repo.unprotect")
            .count();
        assert_eq!(unprotects, 1);
    }

    #[test]
    fn hostile_branch_shapes_are_refused_at_the_door() {
        let (db, repo, ctx) = world("prot-shape");
        for bad in ["", "a..b", "/lead", "trail/", "sp ace", "tab\tname", "b~1"] {
            assert!(protect(&db, &repo, bad, &ctx).is_err(), "{bad:?}");
            assert!(!is_protected(&db, &repo, bad).unwrap(), "{bad:?}");
            assert!(!unprotect(&db, &repo, bad, &ctx).unwrap(), "{bad:?}");
        }
        let long = "b".repeat(201);
        assert!(protect(&db, &repo, &long, &ctx).is_err());
        assert!(list(&db, &repo).unwrap().is_empty());
    }

    /// Requiring, acking, listing and removing — and the audit line per
    /// real change rather than per call, like protections.
    #[test]
    fn required_checks_round_trip_and_are_audited_once_per_change() {
        let (db, repo, ctx) = world("prot-required");
        assert!(required_checks(&db, &repo, "main").unwrap().is_empty());

        assert!(require_check(&db, &repo, "main", "ci/tests", &ctx).unwrap());
        assert!(!require_check(&db, &repo, "main", "ci/tests", &ctx).unwrap());
        // A workflow name with spaces and parentheses — the mirrored
        // Actions case the intake's name rule would have refused.
        assert!(require_check(&db, &repo, "main", "Build and test (ubuntu-latest)", &ctx).unwrap());
        // A different branch is a different gate.
        assert!(require_check(&db, &repo, "release/1.0", "ci/tests", &ctx).unwrap());

        let listed = required_checks(&db, &repo, "main").unwrap();
        assert_eq!(
            listed.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["Build and test (ubuntu-latest)", "ci/tests"],
        );
        assert!(listed.iter().all(|c| c.branch == "main"));
        assert_eq!(
            required_checks(&db, &repo, "release/1.0")
                .unwrap()
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["ci/tests"],
        );

        assert!(unrequire_check(&db, &repo, "main", "ci/tests", &ctx).unwrap());
        assert!(!unrequire_check(&db, &repo, "main", "ci/tests", &ctx).unwrap());
        assert_eq!(
            required_checks(&db, &repo, "main").unwrap().len(),
            1,
            "removing one name took the other with it",
        );
        assert_eq!(
            required_checks(&db, &repo, "release/1.0").unwrap().len(),
            1,
            "removing on one branch removed the same name on another",
        );

        let audit = crate::audit::query(
            &db,
            &ctx.org_id,
            &crate::audit::AuditQuery {
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            audit
                .iter()
                .filter(|e| e.action == "repo.require_check")
                .count(),
            3,
        );
        assert_eq!(
            audit
                .iter()
                .filter(|e| e.action == "repo.unrequire_check")
                .count(),
            1,
        );
    }

    /// A name a person reads has to be one line and one name.
    #[test]
    fn hostile_check_names_are_refused_at_the_door() {
        let (db, repo, ctx) = world("prot-required-shape");
        let long = "n".repeat(MAX_REQUIRED_CHECK_NAME + 1);
        for bad in [
            "",
            "ci\nfake: passing",
            "ci\0",
            "ci\ttests",
            " ci",
            "ci ",
            "ci\u{7f}",
            &long,
        ] {
            assert!(!valid_required_check_name(bad), "{bad:?}");
            let e = require_check(&db, &repo, "main", bad, &ctx).unwrap_err();
            assert!(e.contains("invalid check name"), "{bad:?}: {e}");
            assert!(
                !unrequire_check(&db, &repo, "main", bad, &ctx).unwrap(),
                "{bad:?}"
            );
        }
        // The boundary itself is storable.
        assert!(valid_required_check_name(
            &"n".repeat(MAX_REQUIRED_CHECK_NAME)
        ));
        // And a bad branch is refused with the branch's own words.
        let e = require_check(&db, &repo, "a..b", "ci/tests", &ctx).unwrap_err();
        assert!(e.contains("invalid branch"), "{e}");
        assert!(required_checks(&db, &repo, "a..b").unwrap().is_empty());
        assert!(required_checks(&db, &repo, "main").unwrap().is_empty());
    }

    /// Purging the repository takes its requirements with it — the
    /// alternative is a row referencing nothing, which is also what the
    /// GC worker's `DELETE FROM repos` would trip over. Deletion is a
    /// tombstone first (`delete_repo`) and a row removal later
    /// (`purge_repo`), and it is the second one the cascade is for.
    #[test]
    fn required_checks_cascade_when_the_repo_is_purged() {
        let (db, repo, ctx) = world("prot-required-cascade");
        require_check(&db, &repo, "main", "ci/tests", &ctx).unwrap();
        // Deliberately no `protect` here: `branch_protections` predates
        // the cascade habit and references `repos` with no ON DELETE, so
        // a protected repo cannot be purged at all. That is a finding
        // about that table, not about this one, and it is reported
        // rather than fixed here — widening it would change what a purge
        // does to a repository that has data.
        assert!(registry::delete_repo(&db, &ctx.org_id, &repo).unwrap());
        // A tombstoned repo keeps its rows: the sweep has not run.
        assert_eq!(required_checks(&db, &repo, "main").unwrap().len(), 1);
        registry::purge_repo(&db, &ctx.org_id, &repo).unwrap();
        assert!(required_checks(&db, &repo, "main").unwrap().is_empty());
    }

    #[test]
    fn refusal_sentence_is_pinned() {
        // Every door quotes this string; a reword here breaks the wire,
        // SSH, REST and dashboard tests together — deliberately.
        assert_eq!(
            refusal("main"),
            "branch 'main' is protected: land through review"
        );
    }
}
