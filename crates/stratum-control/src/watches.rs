//! Who hears about a change, and who is deliberately left alone.
//!
//! The subscription levels are GitHub's, because a maintainer already
//! knows what they mean. What "participating" *resolves to* is not:
//! there it means you commented or were mentioned, and here it also
//! means the change needs you, because the repository's OWNERS file
//! says which reviewers a change actually requires.
//!
//! That distinction is the product. A forge that mails everybody about
//! everything trains people to filter it out, and then the one change
//! that needed a human sits in the queue behind the noise.

use crate::ids::now_ms;
use crate::ControlDb;
use std::collections::BTreeSet;

/// How much of a repository somebody wants to hear about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Everything that happens here.
    All,
    /// Only changes they are involved in — the default, and the one
    /// nobody has to choose.
    Participating,
    /// Nothing at all.
    Ignore,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::All => "all",
            Level::Participating => "participating",
            Level::Ignore => "ignore",
        }
    }

    /// Parse a level from the wire. Unknown text is rejected rather than
    /// silently read as the default: a caller who sent `"none"` meaning
    /// `"ignore"` should be told, not quietly subscribed.
    pub fn parse(s: &str) -> Result<Level, String> {
        match s {
            "all" => Ok(Level::All),
            "participating" => Ok(Level::Participating),
            "ignore" => Ok(Level::Ignore),
            other => Err(format!(
                "watch level {other:?} is not one of all, participating, ignore"
            )),
        }
    }
}

/// What this person has chosen for this repository.
///
/// The absence of a row is `Participating`, and no row is written to
/// establish that. A new member is already subscribed to the right
/// amount without anybody enrolling them, and the table holds only the
/// people who wanted something other than the sensible thing.
pub fn level_for(db: &ControlDb, repo_id: &str, user_id: &str) -> Result<Level, String> {
    let rows = db
        .lock()
        .query(
            "SELECT level FROM repo_watches WHERE repo_id = $1 AND user_id = $2",
            &[&repo_id, &user_id],
        )
        .map_err(|e| e.to_string())?;
    match rows.first() {
        Some(r) => Level::parse(r.get::<_, &str>("level")),
        None => Ok(Level::Participating),
    }
}

/// Record a deliberate choice. Choosing the default removes the row
/// rather than storing it, so "I never decided" and "I decided on the
/// default" stay the same state — there is no third thing to reason
/// about later.
pub fn set_level(db: &ControlDb, repo_id: &str, user_id: &str, level: Level) -> Result<(), String> {
    let mut c = db.lock();
    if level == Level::Participating {
        c.execute(
            "DELETE FROM repo_watches WHERE repo_id = $1 AND user_id = $2",
            &[&repo_id, &user_id],
        )
        .map_err(|e| e.to_string())?;
        return Ok(());
    }
    c.execute(
        "INSERT INTO repo_watches (repo_id, user_id, level, updated_at) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (repo_id, user_id) DO UPDATE SET level = $3, updated_at = $4",
        &[&repo_id, &user_id, &level.as_str(), &now_ms()],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Everybody who asked to hear about everything here.
pub fn watching_all(db: &ControlDb, repo_id: &str) -> Result<BTreeSet<String>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT user_id FROM repo_watches WHERE repo_id = $1 AND level = 'all'",
            &[&repo_id],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|r| r.get::<_, String>("user_id")).collect())
}

/// How many people are watching this repository, for the masthead.
///
/// `level = 'all'` and nothing else, which is the only count this
/// schema can state honestly. The default is stored as **no row**, so
/// `count(*)` over the table would be "people who changed their mind
/// about the default in either direction" — a number that goes *up*
/// when somebody chooses Ignore. Counting the rows that mean "tell me
/// everything" is the same thing GitHub's "N watching" means, and it is
/// the one a reader can check against the menu they just used.
///
/// Public information: it rides on the repository view beside the fork
/// count rather than on `…/watch`, which is a person's own setting and
/// refuses a stranger. A count that only signed-in people could see
/// would make the masthead reflow when they signed in.
pub fn watching_count(db: &ControlDb, repo_id: &str) -> Result<i64, String> {
    let row = db
        .lock()
        .query_one(
            "SELECT count(*) AS n FROM repo_watches WHERE repo_id = $1 AND level = 'all'",
            &[&repo_id],
        )
        .map_err(|e| e.to_string())?;
    Ok(row.get::<_, i64>("n"))
}

/// Everybody who asked to hear nothing here.
///
/// Read as a set and subtracted last, so it beats every other reason a
/// person might have been on the list. Somebody who said "never" and is
/// then named in an OWNERS file has still said never.
pub fn ignoring(db: &ControlDb, repo_id: &str) -> Result<BTreeSet<String>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT user_id FROM repo_watches WHERE repo_id = $1 AND level = 'ignore'",
            &[&repo_id],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|r| r.get::<_, String>("user_id")).collect())
}

/// The people to tell, given who is involved and who is watching.
///
/// Pure, and separately tested, because this is the function whose bugs
/// are invisible: mailing one person too few is silence nobody notices,
/// and mailing one too many is the noise that makes people filter the
/// whole channel. It takes sets rather than a database so the rules can
/// be exercised without one.
///
/// The order matters. `ignore` is subtracted after everything, and the
/// actor is subtracted at all — nobody needs an email telling them what
/// they just did, and that single line is the difference between a
/// notification system people keep on and one they turn off.
pub fn recipients(
    participants: &BTreeSet<String>,
    reviewers: &BTreeSet<String>,
    watching_all: &BTreeSet<String>,
    ignoring: &BTreeSet<String>,
    actor: &str,
) -> BTreeSet<String> {
    participants
        .union(reviewers)
        .cloned()
        .collect::<BTreeSet<_>>()
        .union(watching_all)
        .filter(|u| u.as_str() != actor && !ignoring.contains(*u))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_actor_is_never_told_what_they_just_did() {
        // The line that decides whether people keep notifications on.
        let out = recipients(&set(&["ada", "bo"]), &set(&[]), &set(&[]), &set(&[]), "ada");
        assert_eq!(out, set(&["bo"]));
    }

    #[test]
    fn a_reviewer_the_change_needs_is_told_without_watching() {
        // "Participating" here means the OWNERS file says this change
        // requires you — the thing GitHub cannot compute.
        let out = recipients(&set(&[]), &set(&["cy"]), &set(&[]), &set(&[]), "ada");
        assert_eq!(out, set(&["cy"]));
    }

    #[test]
    fn ignore_beats_every_other_reason_to_be_on_the_list() {
        // Named in OWNERS, watching everything, and commented — and
        // still said never. Never wins.
        let out = recipients(
            &set(&["di"]),
            &set(&["di"]),
            &set(&["di"]),
            &set(&["di"]),
            "ada",
        );
        assert!(
            out.is_empty(),
            "somebody who said never was mailed: {out:?}"
        );
    }

    #[test]
    fn nobody_is_told_twice_for_being_involved_twice() {
        let out = recipients(
            &set(&["eve"]),
            &set(&["eve"]),
            &set(&["eve"]),
            &set(&[]),
            "ada",
        );
        assert_eq!(out, set(&["eve"]));
    }

    #[test]
    fn the_default_is_not_stored_so_it_cannot_drift() {
        assert_eq!(Level::parse("participating").unwrap(), Level::Participating);
        assert!(Level::parse("none").is_err(), "a typo was read as a level");
    }

    /// Choosing a level, changing it, and going back to the default.
    ///
    /// The round trip is the point. `set_level(Participating)` **deletes**
    /// the row rather than storing it, so that "I never decided" and "I
    /// decided on the default" stay one state — and that delete had no
    /// test, which means a person who chose All Activity and then went
    /// back to the default could have been left subscribed to
    /// everything with the UI showing them unsubscribed. The three
    /// levels are offered in the watch menu, so all three are product
    /// paths, not just the two that write.
    #[test]
    fn a_level_round_trips_and_the_default_is_stored_as_no_row() {
        let db =
            crate::ControlDb::open(&stratum_testkit::pg::test_db_url("watches_levels")).unwrap();
        let u = crate::users::create(
            &db,
            "ada@example.com",
            "ada",
            Some("a long enough password"),
        )
        .unwrap();
        let ns = crate::registry::create_personal_namespace(&db, &u.id, "ada", None).unwrap();
        let r = crate::registry::create_repo(
            &db,
            &ns.id,
            &crate::registry::NewRepo {
                description: None,
                name: "widget",
                kind: crate::registry::RepoKind::Native,
                public: true,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();

        // Nobody has decided anything yet.
        assert_eq!(level_for(&db, &r.id, &u.id).unwrap(), Level::Participating);
        assert_eq!(watching_count(&db, &r.id).unwrap(), 0);

        set_level(&db, &r.id, &u.id, Level::All).unwrap();
        assert_eq!(level_for(&db, &r.id, &u.id).unwrap(), Level::All);
        assert!(watching_all(&db, &r.id).unwrap().contains(&u.id));
        assert_eq!(watching_count(&db, &r.id).unwrap(), 1);

        // Changing it updates in place rather than adding a second row —
        // the ON CONFLICT arm.
        set_level(&db, &r.id, &u.id, Level::Ignore).unwrap();
        assert_eq!(level_for(&db, &r.id, &u.id).unwrap(), Level::Ignore);
        assert!(ignoring(&db, &r.id).unwrap().contains(&u.id));
        assert!(!watching_all(&db, &r.id).unwrap().contains(&u.id));
        // The count this masthead publishes must go *down* when somebody
        // asks to be left alone. A `count(*)` over the table would have
        // it go up, because Ignore is a stored row and the default is
        // not — the whole reason `watching_count` filters on level.
        assert_eq!(
            watching_count(&db, &r.id).unwrap(),
            0,
            "somebody who chose Ignore was counted as watching"
        );

        // And back to the default, which must actually remove the row.
        set_level(&db, &r.id, &u.id, Level::Participating).unwrap();
        assert_eq!(level_for(&db, &r.id, &u.id).unwrap(), Level::Participating);
        assert!(!ignoring(&db, &r.id).unwrap().contains(&u.id));
        assert!(!watching_all(&db, &r.id).unwrap().contains(&u.id));
        assert_eq!(watching_count(&db, &r.id).unwrap(), 0);
    }
}
