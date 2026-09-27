//! Which repository is reading whose bytes.
//!
//! A zero-copy fork shares upstream's immutable objects rather than
//! copying them: it writes its own manifest and an `SLH4` locator
//! pointing into upstream's data prefix. That is what makes forking cost
//! milliseconds, and it is also what makes epoch GC unsafe without this
//! table.
//!
//! GC deletes an epoch when no live pointer references it. Until forks
//! existed, "no live pointer" could be answered from one prefix's own
//! manifest and locator header. Now it cannot: upstream compacts, the
//! old epoch stops being referenced by anything *upstream* can see, one
//! grace window passes, and the objects a fork is still reading are
//! deleted. Nothing errors at the time. The first symptom is a clone
//! that fails `git fsck`, in a repository nobody touched.
//!
//! So this is the other half of the liveness question, and
//! `stratum-engine`'s `EpochRefs` resolver reads it on every sweep.
//!
//! **Ordering, which is the whole safety argument.** There is no
//! transaction spanning Postgres and an S3 conditional PUT, so the order
//! of the two writes *is* the correctness proof:
//!
//! 1. `register` the reference here, then
//! 2. publish the fork's `locator.hdr`.
//!
//! A crash between them leaves a reference pinning an epoch no pointer
//! uses — storage held slightly too long, found and released by a
//! sweeper, and **safe**. The reverse order leaves a live fork pointing
//! at collectable data, which is corruption and is not recoverable. This
//! is the same argument I7 makes as "data first, pointer last", and it
//! is why `register` is a separate call the fork worker has to make
//! before it writes anything to the store.

use crate::ids::now_ms;
use crate::ControlDb;

/// One repository's dependency on another's epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochRef {
    pub owner_repo_id: String,
    pub epoch: String,
    pub referencing_repo_id: String,
    pub created_at: i64,
}

/// Record that `referencing_repo_id` reads `owner_repo_id`'s `epoch`.
///
/// Idempotent: registering twice is the state the caller asked for, not
/// an error. A fork job that is retried after a crash — which is exactly
/// the crash this ordering is designed to survive — must be able to run
/// again without failing on its own earlier row.
///
/// Call this **before** publishing the fork's pointer. See the module
/// header for why that order is not a preference.
pub fn register(
    db: &ControlDb,
    owner_repo_id: &str,
    epoch: &str,
    referencing_repo_id: &str,
) -> Result<(), String> {
    // A repository referencing its own epochs is not a fork, and it
    // would make the RESTRICT on `owner_repo_id` refuse to ever delete
    // the repository — a row that exists only to block its own cleanup.
    if owner_repo_id == referencing_repo_id {
        return Err("epoch_refs: a repository cannot reference its own epoch".into());
    }
    let mut c = db.lock();
    c.execute(
        "INSERT INTO epoch_refs (owner_repo_id, epoch, referencing_repo_id, created_at) \
         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        &[&owner_repo_id, &epoch, &referencing_repo_id, &now_ms()],
    )
    .map_err(|e| format!("register epoch ref: {e}"))?;
    Ok(())
}

/// Drop one reference — the fork was re-pointed at a newer epoch, or
/// promoted away from upstream entirely.
///
/// Releasing a reference that is not there is not an error: it is a
/// request for a state we are already in.
pub fn release(
    db: &ControlDb,
    owner_repo_id: &str,
    epoch: &str,
    referencing_repo_id: &str,
) -> Result<(), String> {
    let mut c = db.lock();
    c.execute(
        "DELETE FROM epoch_refs \
         WHERE owner_repo_id = $1 AND epoch = $2 AND referencing_repo_id = $3",
        &[&owner_repo_id, &epoch, &referencing_repo_id],
    )
    .map_err(|e| format!("release epoch ref: {e}"))?;
    Ok(())
}

/// Every epoch of `owner_repo_id` that some other repository is reading.
///
/// This is the GC read, and it runs on every sweep of every repository —
/// served by the primary key's leading columns, so it is an index scan
/// over the rows for one repo and nothing more.
pub fn pinned_epochs(db: &ControlDb, owner_repo_id: &str) -> Result<Vec<String>, String> {
    let mut c = db.lock();
    let rows = c
        .query(
            "SELECT DISTINCT epoch FROM epoch_refs WHERE owner_repo_id = $1",
            &[&owner_repo_id],
        )
        .map_err(|e| format!("read epoch refs: {e}"))?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
}

/// Everything one fork depends on, oldest first.
///
/// Read when a fork is deleted, re-pointed at a newer epoch, or promoted
/// because its upstream is going away. Oldest first because the job that
/// bounds storage drift wants the references most worth moving.
pub fn references_held_by(
    db: &ControlDb,
    referencing_repo_id: &str,
) -> Result<Vec<EpochRef>, String> {
    let mut c = db.lock();
    let rows = c
        .query(
            "SELECT owner_repo_id, epoch, referencing_repo_id, created_at FROM epoch_refs \
             WHERE referencing_repo_id = $1 ORDER BY created_at",
            &[&referencing_repo_id],
        )
        .map_err(|e| format!("read epoch refs: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| EpochRef {
            owner_repo_id: r.get(0),
            epoch: r.get(1),
            referencing_repo_id: r.get(2),
            created_at: r.get(3),
        })
        .collect())
}

/// Who is reading this repository's data, if anybody.
///
/// The question `sweep_prefix` and the repository-deletion path have to
/// ask before removing anything. The database also enforces this — the
/// `owner_repo_id` foreign key RESTRICTs — but a caller that can find
/// out cheaply should say something better than a constraint violation.
pub fn dependents(db: &ControlDb, owner_repo_id: &str) -> Result<Vec<String>, String> {
    let mut c = db.lock();
    let rows = c
        .query(
            "SELECT DISTINCT referencing_repo_id FROM epoch_refs WHERE owner_repo_id = $1",
            &[&owner_repo_id],
        )
        .map_err(|e| format!("read dependents: {e}"))?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
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
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap()
    }

    /// Upstream, and a repository that forked it.
    fn pair(db: &ControlDb, hint: &str) -> (registry::Repo, registry::Repo, String) {
        let (_, org) = person(db, hint, &format!("{hint}@example.com"));
        let up = repo(db, &org, "upstream");
        let fork = repo(db, &org, "fork");
        (up, fork, org)
    }

    #[test]
    fn a_registered_reference_is_visible_from_both_directions() {
        let db = db("epochrefs_both");
        let (up, fork, _) = pair(&db, "ada");

        assert_eq!(pinned_epochs(&db, &up.id).unwrap(), Vec::<String>::new());
        register(&db, &up.id, "e1", &fork.id).unwrap();

        assert_eq!(pinned_epochs(&db, &up.id).unwrap(), vec!["e1".to_string()]);
        assert_eq!(dependents(&db, &up.id).unwrap(), vec![fork.id.clone()]);
        let held = references_held_by(&db, &fork.id).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].owner_repo_id, up.id);
        assert_eq!(held[0].epoch, "e1");
    }

    #[test]
    fn registering_twice_is_the_state_the_caller_asked_for() {
        // A fork job that crashed between registering and publishing its
        // pointer is retried, and the retry must not fail on its own
        // earlier row — that crash is precisely the one this write
        // ordering exists to survive.
        let db = db("epochrefs_idempotent");
        let (up, fork, _) = pair(&db, "ada");

        register(&db, &up.id, "e1", &fork.id).unwrap();
        register(&db, &up.id, "e1", &fork.id).unwrap();
        assert_eq!(pinned_epochs(&db, &up.id).unwrap(), vec!["e1".to_string()]);
        assert_eq!(references_held_by(&db, &fork.id).unwrap().len(), 1);
    }

    #[test]
    fn releasing_something_never_registered_is_not_an_error() {
        let db = db("epochrefs_release_absent");
        let (up, fork, _) = pair(&db, "ada");
        release(&db, &up.id, "e1", &fork.id).unwrap();

        register(&db, &up.id, "e1", &fork.id).unwrap();
        release(&db, &up.id, "e1", &fork.id).unwrap();
        assert_eq!(pinned_epochs(&db, &up.id).unwrap(), Vec::<String>::new());
        // Releasing one epoch leaves the others alone.
        register(&db, &up.id, "e1", &fork.id).unwrap();
        register(&db, &up.id, "e2", &fork.id).unwrap();
        release(&db, &up.id, "e1", &fork.id).unwrap();
        assert_eq!(pinned_epochs(&db, &up.id).unwrap(), vec!["e2".to_string()]);
    }

    #[test]
    fn a_repository_cannot_reference_its_own_epoch() {
        // It is not a fork, and the row would exist only to make the
        // RESTRICT below refuse to ever delete the repository — a
        // reference blocking its own cleanup.
        let db = db("epochrefs_self");
        let (up, _, _) = pair(&db, "ada");
        let err = register(&db, &up.id, "e1", &up.id).unwrap_err();
        assert!(err.contains("its own epoch"), "{err}");
    }

    #[test]
    fn upstream_cannot_be_hard_deleted_while_a_fork_reads_it() {
        // The invariant that matters, and the reason it is a foreign key
        // rather than a check in code. Dropping the upstream row would
        // take the very references that were keeping the fork's data
        // alive: the delete succeeds, GC finds nothing referenced, and
        // one grace window later the fork is hollow. The database
        // refuses instead.
        let db = db("epochrefs_restrict");
        let (up, fork, org) = pair(&db, "ada");
        register(&db, &up.id, "e1", &fork.id).unwrap();

        // Asserted as behaviour, not as text: `registry::purge_repo`
        // maps the postgres error with `e.to_string()`, which for a
        // constraint violation is the entirely uninformative "db error".
        // Matching on that string would pin a message rather than the
        // property, and would pass just as happily if the delete had
        // failed for some unrelated reason.
        assert!(registry::purge_repo(&db, &org, &up.id).is_err());

        // Nothing was removed: the row is there and so is its reference.
        assert_eq!(pinned_epochs(&db, &up.id).unwrap(), vec!["e1".to_string()]);
        assert_eq!(dependents(&db, &up.id).unwrap(), vec![fork.id.clone()]);

        // And this is what discriminates: with the reference released —
        // which is what the promotion job does after re-ingesting the
        // dependents — the very same delete succeeds. So the refusal
        // above was the reference, and nothing else.
        release(&db, &up.id, "e1", &fork.id).unwrap();
        registry::purge_repo(&db, &org, &up.id).unwrap();
    }

    #[test]
    fn deleting_a_fork_frees_the_epochs_it_was_holding() {
        // The common case, and it should need no cleanup job: the fork
        // goes, its claim on upstream goes with it, and upstream's
        // storage becomes collectable on the next pass.
        let db = db("epochrefs_cascade");
        let (up, fork, org) = pair(&db, "ada");
        register(&db, &up.id, "e1", &fork.id).unwrap();
        assert_eq!(dependents(&db, &up.id).unwrap().len(), 1);

        registry::purge_repo(&db, &org, &fork.id).unwrap();
        assert_eq!(pinned_epochs(&db, &up.id).unwrap(), Vec::<String>::new());
        assert_eq!(dependents(&db, &up.id).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn two_forks_of_one_epoch_are_two_references() {
        // The count is what decides whether upstream's epoch may go, so
        // one fork leaving must not release another's hold on it.
        let db = db("epochrefs_two");
        let (_, org) = person(&db, "ada", "ada@example.com");
        let up = repo(&db, &org, "upstream");
        let a = repo(&db, &org, "fork-a");
        let b = repo(&db, &org, "fork-b");

        register(&db, &up.id, "e1", &a.id).unwrap();
        register(&db, &up.id, "e1", &b.id).unwrap();
        // One epoch, deduplicated, but two dependents.
        assert_eq!(pinned_epochs(&db, &up.id).unwrap(), vec!["e1".to_string()]);
        let mut deps = dependents(&db, &up.id).unwrap();
        deps.sort();
        let mut want = vec![a.id.clone(), b.id.clone()];
        want.sort();
        assert_eq!(deps, want);

        release(&db, &up.id, "e1", &a.id).unwrap();
        assert_eq!(
            pinned_epochs(&db, &up.id).unwrap(),
            vec!["e1".to_string()],
            "the last fork's hold was released by another fork leaving"
        );
        release(&db, &up.id, "e1", &b.id).unwrap();
        assert_eq!(pinned_epochs(&db, &up.id).unwrap(), Vec::<String>::new());
    }
}
