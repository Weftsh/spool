//! Where a repository came from, and who came from it.
//!
//! Forking is the mechanism by which open source is open: it is how
//! somebody with no push credential contributes at all. A fork here is
//! **zero-copy** — it shares upstream's immutable objects and writes
//! only its own manifest and pointer — so creating one costs
//! milliseconds and no storage until the histories diverge.
//!
//! This module owns the control-plane half of that: the parent and root
//! links, the job state, and the direct-fork count. The storage half is
//! `stratum_store::plane::rebase_header`, and the reference that keeps
//! upstream's bytes alive is [`crate::epoch_refs`].
//!
//! **Why this is a module rather than fields on `registry::Repo`.**
//! `registry.rs` is the busiest file in this crate and three people are
//! working in this tree; a fork read is one small indexed query and does
//! not justify widening the type every caller of `create_repo` depends
//! on. If the repository read model later wants these inline, moving
//! them is a mechanical change with these tests already standing.
//!
//! **The ordering the fork worker must follow**, of which this is step
//! two of five. Every step is idempotent, so a job that dies part-way
//! can simply run again:
//!
//! 1. `registry::create_repo` — the fork's own row.
//! 2. [`attach`] — link it to its parent, mark it `pending`.
//! 3. [`crate::epoch_refs::register`] — pin upstream's epoch.
//! 4. write the fork's `SLH4` `locator.hdr` into the store.
//! 5. [`set_state`] to `ready`.
//!
//! Step 3 before step 4 is not a preference — see the header of
//! `epoch_refs`. A crash between them pins storage slightly too long,
//! which is recoverable; the reverse leaves a live fork pointing at
//! collectable data, which is not.

use crate::ControlDb;

/// What kind of fork a repository is, if it is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkInfo {
    /// Who we forked directly. `None` means this is not a fork — or was
    /// one, and its upstream has since been deleted and it promoted.
    pub parent_id: Option<String>,
    /// The top of the chain. Denormalised so the fork network is one
    /// index scan rather than a recursive walk of unknown depth.
    pub root_id: Option<String>,
    pub state: Option<State>,
    /// How many repositories forked **this** one, directly. Not the
    /// network size: this is the number on the repository page.
    pub count: i32,
}

impl ForkInfo {
    /// Is this repository a fork right now? A promoted fork is not: it
    /// stands on its own storage and answers to nobody.
    pub fn is_fork(&self) -> bool {
        self.parent_id.is_some()
    }
}

/// How far along the fork job is.
///
/// A zero-copy fork is fast but not instant, and a repository that
/// exists but cannot yet be read has to be able to say so rather than
/// 404 at somebody who just watched us create it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Pending,
    Ready,
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Ready => "ready",
            State::Failed => "failed",
        }
    }

    /// Parse from the database. Unknown text is an error rather than a
    /// default: a row we cannot interpret must not silently read as
    /// `ready` and start serving.
    pub fn parse(s: &str) -> Result<State, String> {
        match s {
            "pending" => Ok(State::Pending),
            "ready" => Ok(State::Ready),
            "failed" => Ok(State::Failed),
            other => Err(format!("unknown fork state: {other}")),
        }
    }
}

/// One row of a fork listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkRow {
    pub id: String,
    pub org_id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub root_id: Option<String>,
}

/// Link a freshly created repository to the one it was forked from.
///
/// Idempotent, and the guard that makes it so is `fork_parent_id IS
/// NULL`: the count moves only when the row actually became a fork, so
/// a retried job — or a double-clicked button that got past
/// `enqueue_unique` — cannot inflate a number no rows justify. Same
/// shape as `stars`, same reason, and still no trigger.
///
/// The root is flattened here rather than walked later: a fork of a fork
/// takes its parent's root, so the chain is one hop deep no matter how
/// many times a project has been forked from a fork.
pub fn attach(db: &ControlDb, repo_id: &str, parent_id: &str) -> Result<(), String> {
    if repo_id == parent_id {
        return Err("forks: a repository cannot be its own parent".into());
    }
    let repo = repo_id.to_string();
    let parent = parent_id.to_string();
    let found = db
        .lock()
        .transaction(move |tx| {
            // The parent's own root, if it has one — otherwise the
            // parent *is* the root.
            let Some(row) =
                tx.query_opt("SELECT fork_root_id FROM repos WHERE id = $1", &[&parent])?
            else {
                // Carried out as a value rather than raised in here: the
                // closure can only fail with a `postgres::Error`, and
                // dressing "no such repository" up as a database fault
                // would put a misleading cause in front of whoever reads
                // the log.
                return Ok(false);
            };
            let root: String = row
                .get::<_, Option<String>>(0)
                .unwrap_or_else(|| parent.clone());

            let linked = tx.execute(
                "UPDATE repos SET fork_parent_id = $2, fork_root_id = $3, fork_state = 'pending' \
                 WHERE id = $1 AND fork_parent_id IS NULL",
                &[&repo, &parent, &root],
            )?;
            if linked == 1 {
                tx.execute(
                    "UPDATE repos SET fork_count = fork_count + 1 WHERE id = $1",
                    &[&parent],
                )?;
            }
            Ok(true)
        })
        .map_err(|e| format!("attach fork: {e}"))?;
    if !found {
        return Err(format!("attach fork: no repository {parent_id} to fork"));
    }
    Ok(())
}

/// Move the job along. `ready` is what makes a fork readable.
pub fn set_state(db: &ControlDb, repo_id: &str, state: State) -> Result<(), String> {
    let mut c = db.lock();
    c.execute(
        "UPDATE repos SET fork_state = $2 WHERE id = $1",
        &[&repo_id, &state.as_str()],
    )
    .map_err(|e| format!("set fork state: {e}"))?;
    Ok(())
}

/// Cut a fork loose from its upstream — promotion.
///
/// Called after the fork's data has been materialised onto storage of
/// its own, which is what makes standing alone true rather than merely
/// asserted. Decrements the old parent's count in the same transaction,
/// and only when this row really was still attached.
pub fn detach(db: &ControlDb, repo_id: &str) -> Result<(), String> {
    let repo = repo_id.to_string();
    db.lock()
        .transaction(move |tx| {
            let row = tx.query_opt("SELECT fork_parent_id FROM repos WHERE id = $1", &[&repo])?;
            let parent: Option<String> = row.and_then(|r| r.get::<_, Option<String>>(0));
            let Some(parent) = parent else {
                return Ok(()); // already standing alone
            };
            let detached = tx.execute(
                // `fork_state` goes with the links: a promoted
                // repository is not a fork in any state, and leaving
                // "ready" behind would have the UI render a
                // "forked from" line pointing at nothing.
                "UPDATE repos SET fork_parent_id = NULL, fork_root_id = NULL, \
                 fork_state = NULL WHERE id = $1 AND fork_parent_id IS NOT NULL",
                &[&repo],
            )?;
            if detached == 1 {
                tx.execute(
                    "UPDATE repos SET fork_count = fork_count - 1 WHERE id = $1",
                    &[&parent],
                )?;
            }
            Ok(())
        })
        .map_err(|e| format!("detach fork: {e}"))
}

/// Read a repository's fork position.
pub fn info(db: &ControlDb, repo_id: &str) -> Result<Option<ForkInfo>, String> {
    let mut c = db.lock();
    let row = c
        .query_opt(
            "SELECT fork_parent_id, fork_root_id, fork_state, fork_count FROM repos WHERE id = $1",
            &[&repo_id],
        )
        .map_err(|e| format!("read fork info: {e}"))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let state = row
        .get::<_, Option<String>>(2)
        .map(|s| State::parse(&s))
        .transpose()?;
    Ok(Some(ForkInfo {
        parent_id: row.get(0),
        root_id: row.get(1),
        state,
        count: row.get(3),
    }))
}

/// The namespace a repository lives in.
///
/// Promotion needs it and cannot derive it: the dependents of a deleted
/// upstream are in *their own* namespaces, which is what forking is for,
/// so the deleted repository's org is the one org they are guaranteed
/// not to be in.
pub fn owner_org_of(db: &ControlDb, repo_id: &str) -> Result<Option<String>, String> {
    let mut c = db.lock();
    let row = c
        .query_opt("SELECT org_id FROM repos WHERE id = $1", &[&repo_id])
        .map_err(|e| format!("read repo org: {e}"))?;
    Ok(row.map(|r| r.get::<_, String>(0)))
}

/// The parent of a fork, as `(org id, repo id)`.
///
/// Both halves, because a fork's parent is usually in **somebody else's
/// namespace** — that is what forking is for — and every repository
/// lookup in `registry` is org-scoped. Asking for the parent under the
/// fork's own org finds nothing and reads as "the parent is gone", which
/// is a confusing way to say "I looked in the wrong place".
pub fn parent_of(db: &ControlDb, repo_id: &str) -> Result<Option<(String, String)>, String> {
    let mut c = db.lock();
    let row = c
        .query_opt(
            "SELECT parent.org_id, parent.id FROM repos self \
             JOIN repos parent ON parent.id = self.fork_parent_id \
             WHERE self.id = $1",
            &[&repo_id],
        )
        .map_err(|e| format!("read fork parent: {e}"))?;
    Ok(row.map(|r| (r.get(0), r.get(1))))
}

/// The namespace somebody forks into when they do not name one.
///
/// Lives here rather than in `registry` because forking is the only
/// caller: everything else in the product is handed a namespace by the
/// route it arrived on.
pub fn personal_namespace(db: &ControlDb, user_id: &str) -> Result<Option<String>, String> {
    let mut c = db.lock();
    let row = c
        .query_opt(
            "SELECT id FROM orgs WHERE owner_user_id = $1 AND kind = 'personal'",
            &[&user_id],
        )
        .map_err(|e| format!("read personal namespace: {e}"))?;
    Ok(row.map(|r| r.get::<_, String>(0)))
}

/// Repositories forked directly from this one, wherever they live.
///
/// Every dependent, which is what the promotion and re-pointing jobs
/// need. A response must filter this to the forks its caller may read:
/// a fork lives in its owner's namespace, and its name is theirs.
pub fn forks_of(db: &ControlDb, repo_id: &str) -> Result<Vec<ForkRow>, String> {
    rows(
        db,
        "SELECT id, org_id, name, fork_parent_id, fork_root_id FROM repos \
         WHERE fork_parent_id = $1 AND state = 'active' ORDER BY name",
        repo_id,
    )
}

/// The whole fork network: everything sharing this root, the root
/// included. One index scan, which is what `fork_root_id` is
/// denormalised for.
pub fn network_of(db: &ControlDb, root_id: &str) -> Result<Vec<ForkRow>, String> {
    rows(
        db,
        "SELECT id, org_id, name, fork_parent_id, fork_root_id FROM repos \
         WHERE (fork_root_id = $1 OR id = $1) AND state = 'active' ORDER BY name",
        root_id,
    )
}

fn rows(db: &ControlDb, sql: &str, arg: &str) -> Result<Vec<ForkRow>, String> {
    let mut c = db.lock();
    let rows = c
        .query(sql, &[&arg])
        .map_err(|e| format!("read forks: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| ForkRow {
            id: r.get(0),
            org_id: r.get(1),
            name: r.get(2),
            parent_id: r.get(3),
            root_id: r.get(4),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, NewRepo, RepoKind};

    fn db(hint: &str) -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap()
    }

    fn person(db: &ControlDb, handle: &str) -> (String, String) {
        let u = crate::users::create(
            db,
            &format!("{handle}@example.com"),
            handle,
            Some("a long enough password"),
        )
        .unwrap();
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

    #[test]
    fn a_new_repository_is_not_a_fork_of_anything() {
        let db = db("forks_plain");
        let (_, org) = person(&db, "ada");
        let r = repo(&db, &org, "widget");
        let i = info(&db, &r.id).unwrap().unwrap();
        assert!(!i.is_fork());
        assert_eq!(i.parent_id, None);
        assert_eq!(i.root_id, None);
        assert_eq!(i.state, None);
        assert_eq!(i.count, 0);
    }

    #[test]
    fn attaching_links_the_child_and_counts_it_on_the_parent() {
        let db = db("forks_attach");
        let (_, org) = person(&db, "ada");
        let up = repo(&db, &org, "upstream");
        let fk = repo(&db, &org, "fork");

        attach(&db, &fk.id, &up.id).unwrap();

        let child = info(&db, &fk.id).unwrap().unwrap();
        assert!(child.is_fork());
        assert_eq!(child.parent_id.as_deref(), Some(up.id.as_str()));
        // The parent is the root, because the parent had none.
        assert_eq!(child.root_id.as_deref(), Some(up.id.as_str()));
        assert_eq!(child.state, Some(State::Pending));

        assert_eq!(info(&db, &up.id).unwrap().unwrap().count, 1);
        assert_eq!(forks_of(&db, &up.id).unwrap().len(), 1);
    }

    #[test]
    fn attaching_twice_counts_once() {
        // The fork worker registers its reference before publishing its
        // pointer, so a crash between those two writes is retried from
        // the top — and the retry must not inflate a count that no rows
        // justify. Increment-then-link would have exactly that bug.
        let db = db("forks_idempotent");
        let (_, org) = person(&db, "ada");
        let up = repo(&db, &org, "upstream");
        let fk = repo(&db, &org, "fork");

        attach(&db, &fk.id, &up.id).unwrap();
        attach(&db, &fk.id, &up.id).unwrap();
        attach(&db, &fk.id, &up.id).unwrap();
        assert_eq!(info(&db, &up.id).unwrap().unwrap().count, 1);
        assert_eq!(forks_of(&db, &up.id).unwrap().len(), 1);
    }

    #[test]
    fn a_fork_of_a_fork_keeps_the_original_root() {
        // Flattened at write time so the network listing stays one index
        // scan. A chain walked at read time would be a recursive CTE of
        // unknown depth on a page view, and a fork of a fork of a fork is
        // not rare among people trying to land one patch.
        let db = db("forks_chain");
        let (_, org) = person(&db, "ada");
        let root = repo(&db, &org, "root");
        let mid = repo(&db, &org, "mid");
        let leaf = repo(&db, &org, "leaf");

        attach(&db, &mid.id, &root.id).unwrap();
        attach(&db, &leaf.id, &mid.id).unwrap();

        let l = info(&db, &leaf.id).unwrap().unwrap();
        assert_eq!(l.parent_id.as_deref(), Some(mid.id.as_str()));
        assert_eq!(
            l.root_id.as_deref(),
            Some(root.id.as_str()),
            "root not flattened"
        );

        // Direct counts, not network counts.
        assert_eq!(info(&db, &root.id).unwrap().unwrap().count, 1);
        assert_eq!(info(&db, &mid.id).unwrap().unwrap().count, 1);

        // The network is everything sharing the root, root included.
        let mut net: Vec<String> = network_of(&db, &root.id)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        net.sort();
        assert_eq!(net, vec!["leaf", "mid", "root"]);
    }

    #[test]
    fn a_repository_cannot_fork_itself() {
        let db = db("forks_self");
        let (_, org) = person(&db, "ada");
        let r = repo(&db, &org, "widget");
        assert!(attach(&db, &r.id, &r.id).is_err());
    }

    #[test]
    fn forking_something_that_does_not_exist_says_so() {
        // Rather than a database fault, which is what raising it from
        // inside the transaction would have produced in the log.
        let db = db("forks_missing_parent");
        let (_, org) = person(&db, "ada");
        let r = repo(&db, &org, "widget");
        let err = attach(&db, &r.id, "repo_that_never_existed").unwrap_err();
        assert!(err.contains("to fork"), "{err}");
        assert!(!info(&db, &r.id).unwrap().unwrap().is_fork());
    }

    #[test]
    fn promotion_cuts_the_link_and_gives_the_count_back() {
        let db = db("forks_detach");
        let (_, org) = person(&db, "ada");
        let up = repo(&db, &org, "upstream");
        let fk = repo(&db, &org, "fork");
        attach(&db, &fk.id, &up.id).unwrap();

        detach(&db, &fk.id).unwrap();
        let child = info(&db, &fk.id).unwrap().unwrap();
        assert!(!child.is_fork());
        assert_eq!(child.root_id, None);
        assert_eq!(info(&db, &up.id).unwrap().unwrap().count, 0);

        // And detaching again is the state we are already in, not an
        // error and not a second decrement into the negative.
        detach(&db, &fk.id).unwrap();
        assert_eq!(info(&db, &up.id).unwrap().unwrap().count, 0);
    }

    #[test]
    fn deleting_an_upstream_promotes_its_forks_rather_than_destroying_them() {
        // ON DELETE SET NULL, not CASCADE, and this is the test that
        // says why: deleting your own repository must never destroy
        // everybody else's work derived from it.
        let db = db("forks_upstream_gone");
        let (_, org) = person(&db, "ada");
        let up = repo(&db, &org, "upstream");
        let fk = repo(&db, &org, "fork");
        attach(&db, &fk.id, &up.id).unwrap();

        registry::purge_repo(&db, &org, &up.id).unwrap();

        // The fork is still here, and is now nobody's fork.
        let child = info(&db, &fk.id).unwrap().unwrap();
        assert_eq!(child.parent_id, None);
        assert_eq!(child.root_id, None);
    }

    #[test]
    fn fork_state_moves_and_unknown_state_is_refused() {
        let db = db("forks_state");
        let (_, org) = person(&db, "ada");
        let up = repo(&db, &org, "upstream");
        let fk = repo(&db, &org, "fork");
        attach(&db, &fk.id, &up.id).unwrap();

        assert_eq!(
            info(&db, &fk.id).unwrap().unwrap().state,
            Some(State::Pending)
        );
        set_state(&db, &fk.id, State::Ready).unwrap();
        assert_eq!(
            info(&db, &fk.id).unwrap().unwrap().state,
            Some(State::Ready)
        );
        set_state(&db, &fk.id, State::Failed).unwrap();
        assert_eq!(
            info(&db, &fk.id).unwrap().unwrap().state,
            Some(State::Failed)
        );

        // A row we cannot interpret must not read as `ready` and start
        // serving a repository whose storage may not be there.
        assert!(State::parse("nearly").is_err());
        assert_eq!(State::parse("ready").unwrap(), State::Ready);
    }

    #[test]
    fn a_missing_repository_has_no_fork_info_at_all() {
        let db = db("forks_absent");
        assert_eq!(info(&db, "no_such_repo").unwrap(), None);
    }
}
