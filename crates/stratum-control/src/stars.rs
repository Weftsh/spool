//! Stars: how many people here liked this, and what somebody else's
//! number was.
//!
//! Two counts, never one. `stars` is what this forge knows first-hand —
//! one row in `repo_stars` per person who starred the repository here.
//! `origin_stars` is what the upstream said when the mirror last read
//! it, and it is nobody's achievement of ours.
//!
//! Keeping them apart is the entire point of the feature. A project
//! with 116k stars on GitHub, mirrored here on a Tuesday, honestly has
//! four of ours; rendering only our number tells every visitor the
//! project is dead, and adding the two together invents a figure that
//! matches nothing anybody can check and quietly borrows their
//! reputation. Two fields, two labels, never summed — and the imported
//! one carries a link to the origin so a reader can go and verify it.
//!
//! Starring is a **person's** act. There is no principal here that is
//! not somebody with an account: a repo-bound service token starring a
//! repository would be a number that means nothing, so the API layer
//! refuses one and this layer only ever sees a `user_id`.

use crate::ids::now_ms;
use crate::ControlDb;

/// Both counts and, when somebody is asking, whether they starred it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stars {
    /// Ours. Always known, always a number, and `0` is a true answer.
    pub stars: i32,
    /// The origin's, if we have ever read one. `None` is not zero: it
    /// means there is no imported number for this repository, and the
    /// UI must omit the row rather than render a nought somebody will
    /// read as "nobody upstream cared".
    pub origin_stars: Option<i32>,
    /// When the imported number was true. An imported count is a
    /// snapshot, and a stale snapshot shown as current is its own lie.
    pub origin_stars_at: Option<i64>,
    /// Whether the person asking has starred it. `false` for a stranger,
    /// which is also what an unauthenticated reader should see.
    pub starred: bool,
}

/// Read both counts, and the asker's own state if there is an asker.
pub fn state(db: &ControlDb, repo_id: &str, user_id: Option<&str>) -> Result<Stars, String> {
    let mut c = db.lock();
    let row = c
        .query_one(
            "SELECT stars, origin_stars, origin_stars_at FROM repos WHERE id = $1",
            &[&repo_id],
        )
        .map_err(|e| format!("read stars: {e}"))?;
    let starred = match user_id {
        Some(uid) => !c
            .query(
                "SELECT 1 FROM repo_stars WHERE repo_id = $1 AND user_id = $2",
                &[&repo_id, &uid],
            )
            .map_err(|e| format!("read star: {e}"))?
            .is_empty(),
        None => false,
    };
    Ok(Stars {
        stars: row.get("stars"),
        origin_stars: row.get("origin_stars"),
        origin_stars_at: row.get("origin_stars_at"),
        starred,
    })
}

/// Star it. Starring twice is the same as starring once.
///
/// The idempotence is the whole of the difficulty, and it is why the
/// count moves inside the same transaction as the row rather than after
/// it. `ON CONFLICT DO NOTHING` reports how many rows it actually
/// inserted, and the count is incremented only when that is one — so a
/// double-click, a retried request, or two tabs cannot make the number
/// disagree with the rows that justify it. Incrementing first and
/// inserting after would have exactly that bug, and it would show up as
/// a count nobody can explain months later.
pub fn star(db: &ControlDb, repo_id: &str, user_id: &str) -> Result<Stars, String> {
    let repo = repo_id.to_string();
    let user = user_id.to_string();
    db.lock()
        .transaction(move |tx| {
            let inserted = tx.execute(
                "INSERT INTO repo_stars (repo_id, user_id, created_at) \
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                &[&repo, &user, &now_ms()],
            )?;
            if inserted == 1 {
                tx.execute("UPDATE repos SET stars = stars + 1 WHERE id = $1", &[&repo])?;
            }
            Ok(())
        })
        .map_err(|e| format!("star: {e}"))?;
    state(db, repo_id, Some(user_id))
}

/// Unstar it. Unstarring something you never starred is not an error;
/// it is a request for a state you are already in.
///
/// Same shape as `star`, for the same reason: the delete reports
/// whether there was anything to delete, and only then does the count
/// move. Without that, unstarring twice would drive the number below
/// the rows — and the `CHECK (stars >= 0)` on the column would turn
/// that into a failed write, which is the backstop working, but the
/// caller would still have hit an error doing something harmless.
pub fn unstar(db: &ControlDb, repo_id: &str, user_id: &str) -> Result<Stars, String> {
    let repo = repo_id.to_string();
    let user = user_id.to_string();
    db.lock()
        .transaction(move |tx| {
            let removed = tx.execute(
                "DELETE FROM repo_stars WHERE repo_id = $1 AND user_id = $2",
                &[&repo, &user],
            )?;
            if removed == 1 {
                tx.execute("UPDATE repos SET stars = stars - 1 WHERE id = $1", &[&repo])?;
            }
            Ok(())
        })
        .map_err(|e| format!("unstar: {e}"))?;
    state(db, repo_id, Some(user_id))
}

/// Record what the origin said, and when.
///
/// Called by the mirror path. It never touches `stars`: an imported
/// number is not evidence about anybody here, and the one way this
/// feature can become dishonest is for these two values to meet.
pub fn set_origin_stars(db: &ControlDb, repo_id: &str, count: i32) -> Result<(), String> {
    if count < 0 {
        // An upstream that reports a negative count is broken, and
        // writing it would trip the column's CHECK with a message about
        // our schema rather than about their answer.
        return Err(format!("an origin star count cannot be negative: {count}"));
    }
    db.lock()
        .execute(
            "UPDATE repos SET origin_stars = $2, origin_stars_at = $3 WHERE id = $1",
            &[&repo_id, &count, &now_ms()],
        )
        .map_err(|e| format!("set origin stars: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, NewRepo, RepoKind};

    fn db(hint: &str) -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap()
    }

    fn person(db: &ControlDb, handle: &str, email: &str) -> (String, String) {
        let u = crate::users::create(db, email, handle, Some("a long enough password")).unwrap();
        crate::usertokens::mark_verified(db, &u.id).unwrap();
        let ns = registry::create_personal_namespace(db, &u.id, handle, None).unwrap();
        (u.id, ns.id)
    }

    fn repo(db: &ControlDb, org_id: &str, name: &str) -> registry::Repo {
        registry::create_repo(
            db,
            org_id,
            &NewRepo {
                description: Some("a repository"),
                name,
                kind: RepoKind::Native,
                public: true,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn a_new_repository_has_our_zero_and_nobody_elses_number() {
        let db = db("stars_new");
        let (_, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        let s = state(&db, &r.id, None).unwrap();
        assert_eq!(s.stars, 0);
        // Not `Some(0)`. There is no imported number, which is a
        // different fact from an origin that reported none, and the UI
        // renders the two differently on purpose.
        assert_eq!(s.origin_stars, None);
        assert_eq!(s.origin_stars_at, None);
        assert!(!s.starred);
    }

    #[test]
    fn starring_twice_counts_once() {
        // The bug this is here for: increment-then-insert counts a
        // double-click twice and leaves a number no row justifies.
        let db = db("stars_idempotent");
        let (uid, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        let first = star(&db, &r.id, &uid).unwrap();
        assert_eq!(first.stars, 1);
        assert!(first.starred);
        let second = star(&db, &r.id, &uid).unwrap();
        assert_eq!(second.stars, 1);
        assert!(second.starred);
        assert_eq!(rows(&db, &r.id), 1);
    }

    #[test]
    fn unstarring_something_you_never_starred_is_not_an_error() {
        // And in particular it must not drive the count negative — the
        // column's CHECK would refuse the write, so a caller doing
        // something harmless would get a 500 out of it.
        let db = db("stars_unstar_absent");
        let (uid, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        let s = unstar(&db, &r.id, &uid).unwrap();
        assert_eq!(s.stars, 0);
        assert!(!s.starred);
        let again = unstar(&db, &r.id, &uid).unwrap();
        assert_eq!(again.stars, 0);
    }

    #[test]
    fn the_count_follows_the_rows_through_a_full_cycle() {
        let db = db("stars_cycle");
        let (ada, org) = person(&db, "ada", "ada@example.com");
        let (bob, _) = person(&db, "bob", "bob@example.com");
        let r = repo(&db, &org, "widget");

        assert_eq!(star(&db, &r.id, &ada).unwrap().stars, 1);
        assert_eq!(star(&db, &r.id, &bob).unwrap().stars, 2);
        assert_eq!(rows(&db, &r.id), 2);

        // Bob's own view says starred; Ada's still does too, and a
        // stranger's says nothing about either of them.
        assert!(state(&db, &r.id, Some(&bob)).unwrap().starred);
        assert!(!state(&db, &r.id, None).unwrap().starred);

        assert_eq!(unstar(&db, &r.id, &ada).unwrap().stars, 1);
        assert!(!state(&db, &r.id, Some(&ada)).unwrap().starred);
        assert!(state(&db, &r.id, Some(&bob)).unwrap().starred);
        assert_eq!(rows(&db, &r.id), 1);

        assert_eq!(unstar(&db, &r.id, &bob).unwrap().stars, 0);
        assert_eq!(rows(&db, &r.id), 0);
    }

    #[test]
    fn one_persons_star_is_not_anothers() {
        let db = db("stars_per_person");
        let (ada, org) = person(&db, "ada", "ada@example.com");
        let (bob, _) = person(&db, "bob", "bob@example.com");
        let r = repo(&db, &org, "widget");
        star(&db, &r.id, &ada).unwrap();
        assert!(state(&db, &r.id, Some(&ada)).unwrap().starred);
        assert!(!state(&db, &r.id, Some(&bob)).unwrap().starred);
    }

    #[test]
    fn an_imported_count_never_touches_ours() {
        // The rule the whole feature rests on. After importing 60300,
        // our own count is still the one star we actually have.
        let db = db("stars_origin");
        let (uid, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        star(&db, &r.id, &uid).unwrap();
        set_origin_stars(&db, &r.id, 60_300).unwrap();

        let s = state(&db, &r.id, Some(&uid)).unwrap();
        assert_eq!(s.stars, 1, "an import must not move our own count");
        assert_eq!(s.origin_stars, Some(60_300));
        assert!(s.origin_stars_at.is_some(), "an import is a dated snapshot");
        // And the two are never the same field: nothing anywhere adds
        // these, and a test that accepted 60301 would be endorsing the
        // one outcome the feature exists to prevent.
        assert_ne!(s.stars, s.origin_stars.unwrap());
    }

    #[test]
    fn an_origin_reporting_zero_is_not_an_origin_we_never_asked() {
        let db = db("stars_origin_zero");
        let (_, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        assert_eq!(state(&db, &r.id, None).unwrap().origin_stars, None);
        set_origin_stars(&db, &r.id, 0).unwrap();
        assert_eq!(state(&db, &r.id, None).unwrap().origin_stars, Some(0));
    }

    #[test]
    fn a_negative_import_is_refused_in_our_words() {
        // The column's CHECK would also stop this, but it would report
        // our schema at somebody debugging their upstream.
        let db = db("stars_origin_negative");
        let (_, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        let e = set_origin_stars(&db, &r.id, -1).unwrap_err();
        assert!(e.contains("cannot be negative"), "{e}");
        assert_eq!(state(&db, &r.id, None).unwrap().origin_stars, None);
    }

    #[test]
    fn deleting_a_repository_takes_its_stars_with_it() {
        // `ON DELETE CASCADE`, asserted rather than assumed: an orphaned
        // star row would be counted by nothing and block nothing, and
        // would surface much later as a foreign-key failure.
        let db = db("stars_cascade");
        let (uid, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        star(&db, &r.id, &uid).unwrap();
        db.lock()
            .execute("DELETE FROM repos WHERE id = $1", &[&r.id])
            .unwrap();
        assert_eq!(rows(&db, &r.id), 0);
    }

    /// The rows that justify the count, counted independently of it.
    fn rows(db: &ControlDb, repo_id: &str) -> i64 {
        db.lock()
            .query_one(
                "SELECT COUNT(*) FROM repo_stars WHERE repo_id = $1",
                &[&repo_id],
            )
            .unwrap()
            .get(0)
    }
}
