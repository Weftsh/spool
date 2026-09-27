//! How many commits a repository has, as GitHub counts them: everything
//! reachable from the default branch's tip, through every parent.
//!
//! The number is computed by a worker after a write and stored beside
//! the tip it was true for, never on a request. A request that walked
//! history to count it — the dashboard used to fetch the log with a
//! limit of a thousand, which the server clamped to five hundred
//! first-parent commits and walked one object at a time — spent nine
//! seconds on the production mirror to print a number that was wrong in
//! both directions: capped, and first-parent where GitHub's is not. A
//! stored count answers in the row read the page already makes, and the
//! tip it carries lets a client tell "current" from "as of the last
//! fold" without a second request.

use crate::ids::now_ms;
use crate::ControlDb;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitCount {
    /// The commit the count was taken from.
    pub tip: String,
    pub count: i64,
    /// `false` when the walk stopped at its cap: the true number is at
    /// least `count`. A client prints a `+`.
    pub exact: bool,
    pub computed_at: i64,
}

/// What is stored for `repo_id`, if a walk has ever finished.
pub fn get(db: &ControlDb, repo_id: &str) -> Result<Option<CommitCount>, String> {
    db.lock()
        .query_opt(
            "SELECT tip, count, exact, computed_at FROM repo_commit_counts WHERE repo_id = $1",
            &[&repo_id],
        )
        .map(|row| {
            row.map(|r| CommitCount {
                tip: r.get(0),
                count: r.get(1),
                exact: r.get(2),
                computed_at: r.get(3),
            })
        })
        .map_err(|e| e.to_string())
}

/// Record a finished walk. One row per repository: a newer walk
/// replaces the older, whatever tip either was taken from.
pub fn set(
    db: &ControlDb,
    repo_id: &str,
    tip: &str,
    count: i64,
    exact: bool,
) -> Result<(), String> {
    db.lock()
        .execute(
            "INSERT INTO repo_commit_counts (repo_id, tip, count, exact, computed_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (repo_id) DO UPDATE SET \
               tip = EXCLUDED.tip, count = EXCLUDED.count, \
               exact = EXCLUDED.exact, computed_at = EXCLUDED.computed_at",
            &[&repo_id, &tip, &count, &exact, &now_ms()],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, NewRepo, RepoKind};

    fn db(hint: &str) -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap()
    }

    fn repo(db: &ControlDb, hint: &str) -> registry::Repo {
        let org = registry::create_org(db, hint).unwrap();
        registry::create_repo(
            db,
            &org.id,
            &NewRepo {
                name: "app",
                description: None,
                kind: RepoKind::Native,
                public: false,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn a_count_is_absent_until_a_walk_records_one_and_then_replaced_whole() {
        let db = db("commit-counts");
        let r = repo(&db, "commit-counts");
        assert_eq!(get(&db, &r.id).unwrap(), None);

        set(&db, &r.id, "a".repeat(40).as_str(), 12, true).unwrap();
        let got = get(&db, &r.id).unwrap().unwrap();
        assert_eq!(
            (got.tip.as_str(), got.count, got.exact),
            ("a".repeat(40).as_str(), 12, true)
        );
        assert!(got.computed_at > 0);

        // A later walk from a newer tip, cut short at its cap.
        set(&db, &r.id, "b".repeat(40).as_str(), 100_000, false).unwrap();
        let got = get(&db, &r.id).unwrap().unwrap();
        assert_eq!(
            (got.tip.as_str(), got.count, got.exact),
            ("b".repeat(40).as_str(), 100_000, false)
        );
        // One row, not one per tip.
        let rows: i64 = db
            .lock()
            .query_one(
                "SELECT COUNT(*) FROM repo_commit_counts WHERE repo_id = $1",
                &[&r.id],
            )
            .unwrap()
            .get(0);
        assert_eq!(rows, 1);
    }

    #[test]
    fn a_deleted_repository_takes_its_count_with_it() {
        let db = db("commit-counts-gone");
        let r = repo(&db, "commit-counts-gone");
        set(&db, &r.id, "c".repeat(40).as_str(), 3, true).unwrap();
        db.lock()
            .execute("DELETE FROM repos WHERE id = $1", &[&r.id])
            .unwrap();
        assert_eq!(get(&db, &r.id).unwrap(), None);
    }
}
