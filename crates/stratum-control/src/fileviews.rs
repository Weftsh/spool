//! Per-file "viewed" marks on a change — one reviewer's own bookkeeping.
//!
//! A viewed mark is a private note to yourself: *I have read this file at
//! this revision*. Two properties make it worth storing at all, and both
//! are enforced here rather than in the handler.
//!
//! **It belongs to exactly one person.** Every function takes a
//! `user_id`, and there is no function that reads a mark without one, so
//! no caller can accidentally read or write somebody else's. The API
//! above uses the *authenticated* user for that argument and never a
//! value from the request, which is what makes "the viewer's own and
//! nobody else's" a shape rather than a rule to remember.
//!
//! **It is anchored to the patchset it was made against.** The row keeps
//! the patchset number, so a later revision can ask whether the file the
//! reviewer read is still the file that is there. A viewed flag that
//! survives a revision is worse than no flag at all: it tells a reviewer
//! they have seen code they have not.

use crate::db::ControlDb;
use crate::ids::now_ms;

/// One reviewer's mark on one path, and the patchset they read it at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileView {
    pub path: String,
    /// The patchset number the reviewer marked this path at. Compared
    /// against the change's latest by the caller — this module stores the
    /// fact, the review layer decides whether it still means anything.
    pub patchset: i32,
}

/// Every mark this person holds on this change, oldest patchset first
/// then path, so a listing is stable rather than in whatever order the
/// planner produced.
pub fn list(db: &ControlDb, change_id: &str, user_id: &str) -> Result<Vec<FileView>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT path, patchset FROM change_file_views \
             WHERE change_id = $1 AND user_id = $2 ORDER BY patchset, path",
            &[&change_id, &user_id],
        )
        .map_err(|e| format!("list file views: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| FileView {
            path: r.get::<_, String>("path"),
            patchset: r.get::<_, i32>("patchset"),
        })
        .collect())
}

/// The highest patchset number this person has marked anything at — the
/// revision their last pass was made against, and so the left-hand side
/// of "what changed since I last looked".
///
/// `None` when they have marked nothing, and that is the honest answer
/// rather than a stand-in: somebody who has ticked no box has not made a
/// pass, and defaulting them to patchset 1 would offer a reviewer a range
/// over code they never read, which is the same lie a viewed flag that
/// survives a revision tells.
///
/// Derived from a listing the caller already holds rather than asked of
/// the database separately, so the hint and the marks shown beside it
/// cannot describe two different moments — a mark landing between two
/// queries would produce a `since` that is not the maximum of the list
/// printed next to it, and nobody would ever see why.
pub fn last_marked(views: &[FileView]) -> Option<i32> {
    views.iter().map(|v| v.patchset).max()
}

/// Mark a path viewed at a patchset. Re-marking moves the row forward to
/// the newer patchset rather than adding a second one — the primary key
/// is (change, user, path), and the interesting question is always "at
/// which revision", never "how many times".
pub fn mark(
    db: &ControlDb,
    change_id: &str,
    user_id: &str,
    path: &str,
    patchset: i32,
) -> Result<(), String> {
    let at = now_ms();
    db.lock()
        .execute(
            "INSERT INTO change_file_views (change_id, user_id, path, patchset, viewed_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (change_id, user_id, path) \
             DO UPDATE SET patchset = EXCLUDED.patchset, viewed_at = EXCLUDED.viewed_at",
            &[&change_id, &user_id, &path, &patchset, &at],
        )
        .map(|_| ())
        .map_err(|e| format!("mark viewed: {e}"))
}

/// Un-mark a path. `false` means there was nothing to remove, which is
/// not an error: unticking a box nobody ticked is the same outcome.
pub fn unmark(db: &ControlDb, change_id: &str, user_id: &str, path: &str) -> Result<bool, String> {
    db.lock()
        .execute(
            "DELETE FROM change_file_views WHERE change_id = $1 AND user_id = $2 AND path = $3",
            &[&change_id, &user_id, &path],
        )
        .map(|n| n > 0)
        .map_err(|e| format!("unmark viewed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, NewRepo, RepoKind};

    fn setup() -> (ControlDb, String, String, String) {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("fileviews")).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let repo = registry::create_repo(
            &db,
            &org.id,
            &NewRepo {
                description: None,
                name: "app",
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        let user = crate::users::create(&db, "a@example.com", "A", Some("a long enough password"))
            .unwrap();
        let change = crate::changes::create_or_update(
            &db,
            &org.id,
            &repo.id,
            "Iaaaa1111",
            "work",
            "main",
            &"a".repeat(40),
            None,
            "work\n\nChange-Id: Iaaaa1111\n",
            Some(&user.id),
            None,
            None,
        )
        .unwrap()
        .unwrap()
        .0;
        (db, org.id, user.id, change.id)
    }

    /// The mark is a per-person fact. Two reviewers on one change hold
    /// separate rows, and neither listing can show the other's — the
    /// query has no shape that could.
    #[test]
    fn a_mark_belongs_to_one_person() {
        let (db, _org, alice, change) = setup();
        let bob = crate::users::create(&db, "b@example.com", "B", Some("a long enough password"))
            .unwrap()
            .id;

        mark(&db, &change, &alice, "src/a.rs", 1).unwrap();
        assert_eq!(
            list(&db, &change, &alice).unwrap(),
            vec![FileView {
                path: "src/a.rs".into(),
                patchset: 1
            }]
        );
        assert!(list(&db, &change, &bob).unwrap().is_empty());

        // Bob's own mark on the same path is his own row, at his own
        // patchset, and does not disturb Alice's.
        mark(&db, &change, &bob, "src/a.rs", 2).unwrap();
        assert_eq!(list(&db, &change, &bob).unwrap()[0].patchset, 2);
        assert_eq!(list(&db, &change, &alice).unwrap()[0].patchset, 1);
    }

    /// Re-marking moves the row forward rather than duplicating it, and
    /// un-marking removes it. The second unmark reports "nothing there"
    /// instead of failing.
    #[test]
    fn re_marking_moves_the_patchset_and_unmarking_is_idempotent() {
        let (db, _org, user, change) = setup();
        mark(&db, &change, &user, "src/a.rs", 1).unwrap();
        mark(&db, &change, &user, "src/a.rs", 4).unwrap();
        let rows = list(&db, &change, &user).unwrap();
        assert_eq!(rows.len(), 1, "one row per path, not one per marking");
        assert_eq!(rows[0].patchset, 4);

        assert!(unmark(&db, &change, &user, "src/a.rs").unwrap());
        assert!(list(&db, &change, &user).unwrap().is_empty());
        assert!(!unmark(&db, &change, &user, "src/a.rs").unwrap());
    }

    /// The "since" hint is the newest patchset the person marked
    /// anything at, not the oldest and not the count — a reviewer who
    /// read most of patchset 1 and then one file of patchset 3 last
    /// looked at 3, and offering them 1 → latest would re-show work they
    /// have already done. Nothing marked means no last pass at all.
    #[test]
    fn the_since_hint_is_the_newest_patchset_marked_and_nothing_when_none_are() {
        let (db, _org, user, change) = setup();
        assert_eq!(
            last_marked(&list(&db, &change, &user).unwrap()),
            None,
            "somebody who has ticked no box has no last pass"
        );

        mark(&db, &change, &user, "a.rs", 1).unwrap();
        mark(&db, &change, &user, "b.rs", 3).unwrap();
        mark(&db, &change, &user, "c.rs", 2).unwrap();
        assert_eq!(last_marked(&list(&db, &change, &user).unwrap()), Some(3));

        // Un-marking the newest one moves the hint back: the fact is
        // "what I have read", so withdrawing a tick withdraws it.
        unmark(&db, &change, &user, "b.rs").unwrap();
        assert_eq!(last_marked(&list(&db, &change, &user).unwrap()), Some(2));
    }

    /// Listings are ordered, so a reviewer's rail does not reshuffle
    /// between loads.
    #[test]
    fn listings_are_ordered_by_patchset_then_path() {
        let (db, _org, user, change) = setup();
        mark(&db, &change, &user, "z.rs", 1).unwrap();
        mark(&db, &change, &user, "a.rs", 1).unwrap();
        mark(&db, &change, &user, "b.rs", 2).unwrap();
        let paths: Vec<String> = list(&db, &change, &user)
            .unwrap()
            .into_iter()
            .map(|v| v.path)
            .collect();
        assert_eq!(paths, vec!["a.rs", "z.rs", "b.rs"]);
    }
}
