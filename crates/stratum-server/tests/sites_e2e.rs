//! The site and deploy tables, against a real PostgreSQL.
//!
//! These are the properties the serving path depends on and that a unit
//! test against a fake could not establish: that the unique index really
//! is what decides a host collision, that publishing is one atomic move,
//! that a deploy cannot be published onto a repository it does not
//! belong to, and that removing a repository takes its site with it
//! rather than leaving a row that answers a hostname forever.

use stratum_control::registry::{self, NewRepo, RepoKind};
use stratum_control::sites;
use stratum_control::ControlDb;
use stratum_testkit::pg::test_db_url;

fn db(hint: &str) -> ControlDb {
    ControlDb::open(&test_db_url(hint)).expect("open control db")
}

fn repo(db: &ControlDb, org: &str, name: &str) -> String {
    let o = registry::create_org(db, org).expect("create org");
    registry::create_repo(
        db,
        &o.id,
        &NewRepo {
            name,
            description: None,
            kind: RepoKind::Native,
            public: true,
            default_branch: "main",
            origin_url: None,
            origin_provider: None,
            origin_installation: None,
        },
    )
    .expect("create repo")
    .id
}

#[test]
fn a_site_is_found_by_the_host_it_is_served_at() {
    let db = db("sites_by_host");
    let r = repo(&db, "acme", "docs");

    assert_eq!(sites::by_host(&db, "docs--acme").unwrap(), None);
    sites::create(&db, &r, "docs--acme", None).expect("create site");

    let found = sites::by_host(&db, "docs--acme")
        .expect("lookup")
        .expect("some");
    assert_eq!(found.repo_id, r);
    assert_eq!(found.current, None, "nothing published yet");
}

/// The label derivation is lossy, so this index is the real arbiter.
/// Two repositories genuinely can want one label.
#[test]
fn the_unique_index_is_what_decides_a_host_collision() {
    let db = db("sites_collision");
    let a = repo(&db, "acme", "docs");
    let b = repo(&db, "other", "docs");

    sites::create(&db, &a, "docs--acme", None).expect("first");
    assert!(!sites::host_taken(&db, "free--label").unwrap());
    assert!(sites::host_taken(&db, "docs--acme").unwrap());

    let clash = sites::create(&db, &b, "docs--acme", None);
    assert!(clash.is_err(), "second repo must not take a taken host");

    // …and the loser can still have the next candidate.
    sites::create(&db, &b, "docs--acme-2", None).expect("second candidate");
    assert_eq!(
        sites::by_host(&db, "docs--acme-2")
            .unwrap()
            .unwrap()
            .repo_id,
        b
    );
}

/// The caller is a push, and a push happens again.
#[test]
fn creating_a_site_twice_returns_the_one_that_is_stored() {
    let db = db("sites_idempotent");
    let r = repo(&db, "acme", "docs");

    let first = sites::create(&db, &r, "docs--acme", Some("main")).expect("first");
    let again = sites::create(&db, &r, "some-other-label", None).expect("again");

    assert_eq!(
        again.host, first.host,
        "must not report a host we do not serve"
    );
    assert_eq!(again.branch.as_deref(), Some("main"));
    assert_eq!(sites::by_host(&db, "some-other-label").unwrap(), None);
}

#[test]
fn publishing_moves_one_column_and_a_recorded_deploy_is_not_yet_served() {
    let db = db("sites_publish");
    let r = repo(&db, "acme", "docs");
    sites::create(&db, &r, "docs--acme", None).expect("site");

    let one = sites::add_deploy(&db, &r, "c1", "t1", "dist", false, None).expect("deploy one");
    assert_eq!(
        sites::get(&db, &r).unwrap().unwrap().current,
        None,
        "recording a deploy must not serve it"
    );

    sites::publish(&db, &r, &one.id).expect("publish one");
    assert_eq!(
        sites::get(&db, &r).unwrap().unwrap().current.as_deref(),
        Some(one.id.as_str())
    );

    let two = sites::add_deploy(&db, &r, "c2", "t2", "dist", true, None).expect("deploy two");
    sites::publish(&db, &r, &two.id).expect("publish two");
    assert_eq!(
        sites::get(&db, &r).unwrap().unwrap().current.as_deref(),
        Some(two.id.as_str())
    );

    // A rollback is the same move backwards.
    sites::publish(&db, &r, &one.id).expect("roll back");
    assert_eq!(
        sites::get(&db, &r).unwrap().unwrap().current.as_deref(),
        Some(one.id.as_str())
    );
}

/// The `AND repo_id` in the publish statement, which is the difference
/// between a rollback and serving somebody else's site on your domain.
#[test]
fn a_deploy_cannot_be_published_onto_another_repository() {
    let db = db("sites_cross_repo");
    let mine = repo(&db, "acme", "docs");
    let theirs = repo(&db, "other", "secrets");
    sites::create(&db, &mine, "docs--acme", None).expect("mine");
    sites::create(&db, &theirs, "secrets--other", None).expect("theirs");

    let theirs_deploy =
        sites::add_deploy(&db, &theirs, "c1", "t1", "dist", false, None).expect("their deploy");

    let attempt = sites::publish(&db, &mine, &theirs_deploy.id);
    assert!(attempt.is_err(), "must refuse a deploy from another repo");
    assert_eq!(sites::get(&db, &mine).unwrap().unwrap().current, None);
}

#[test]
fn publishing_a_deploy_that_does_not_exist_is_refused() {
    let db = db("sites_missing_deploy");
    let r = repo(&db, "acme", "docs");
    sites::create(&db, &r, "docs--acme", None).expect("site");
    assert!(sites::publish(&db, &r, "no-such-deploy").is_err());
}

#[test]
fn the_config_is_snapshotted_onto_the_deploy() {
    let db = db("sites_snapshot");
    let r = repo(&db, "acme", "docs");
    sites::create(&db, &r, "docs--acme", None).expect("site");

    let d =
        sites::add_deploy(&db, &r, "c1", "t1", "public", true, Some("404.html")).expect("deploy");
    let read = sites::deploy(&db, &d.id).unwrap().unwrap();
    assert_eq!(read.publish, "public");
    assert_eq!(read.tree_oid, "t1");
    assert!(read.spa);
    assert_eq!(read.not_found.as_deref(), Some("404.html"));
}

#[test]
fn deploys_come_back_newest_first_and_respect_the_limit() {
    let db = db("sites_history");
    let r = repo(&db, "acme", "docs");
    sites::create(&db, &r, "docs--acme", None).expect("site");

    let mut ids = Vec::new();
    for n in 0..5 {
        let d = sites::add_deploy(
            &db,
            &r,
            &format!("c{n}"),
            &format!("t{n}"),
            "dist",
            false,
            None,
        )
        .expect("deploy");
        ids.push(d.id);
    }
    let got = sites::deploys(&db, &r, 3).expect("history");
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].id, *ids.last().unwrap(), "newest first");
    for w in got.windows(2) {
        assert!(w[0].seq > w[1].seq, "history is ordered by seq, strictly");
    }
    // The three most recent, in reverse creation order, and no others.
    let want: Vec<&String> = ids.iter().rev().take(3).collect();
    let saw: Vec<&String> = got.iter().map(|d| &d.id).collect();
    assert_eq!(saw, want);
}

/// Five deploys inside one millisecond used to order arbitrarily:
/// `created_at` is a millisecond and ULID suffixes are random within
/// one, so "newest first" was undefined. Across a fleet it was worse
/// than undefined — two nodes with skewed clocks could order a rollback
/// before the deploy it rolled back.
#[test]
fn deploys_in_the_same_millisecond_still_have_one_true_order() {
    let db = db("sites_same_ms");
    let r = repo(&db, "acme", "docs");
    sites::create(&db, &r, "docs--acme", None).expect("site");

    let mut ids = Vec::new();
    for n in 0..20 {
        ids.push(
            sites::add_deploy(
                &db,
                &r,
                &format!("c{n}"),
                &format!("t{n}"),
                "dist",
                false,
                None,
            )
            .expect("deploy")
            .id,
        );
    }
    let got = sites::deploys(&db, &r, 20).expect("history");
    let saw: Vec<&String> = got.iter().map(|d| &d.id).collect();
    let want: Vec<&String> = ids.iter().rev().collect();
    assert_eq!(saw, want, "insertion order, whatever the clock did");

    // The condition that made the old ordering ambiguous really did
    // hold, so this test is exercising the case it names.
    let stamps: std::collections::HashSet<i64> = got.iter().map(|d| d.created_at).collect();
    assert!(
        stamps.len() < got.len(),
        "expected a millisecond collision to exercise; saw {} distinct stamps for {} deploys",
        stamps.len(),
        got.len()
    );
}

#[test]
fn changing_the_branch_sticks_and_can_return_to_the_default() {
    let db = db("sites_branch");
    let r = repo(&db, "acme", "docs");
    sites::create(&db, &r, "docs--acme", Some("main")).expect("site");

    sites::set_branch(&db, &r, Some("release")).expect("set");
    assert_eq!(
        sites::get(&db, &r).unwrap().unwrap().branch.as_deref(),
        Some("release")
    );

    sites::set_branch(&db, &r, None).expect("clear");
    assert_eq!(sites::get(&db, &r).unwrap().unwrap().branch, None);
}

/// Deleting a repository frees its hostname.
///
/// The foreign key cannot do this: `delete_repo` is a **soft** delete —
/// the row stays with `state = 'deleted'` until the purge sweep — so
/// `ON DELETE CASCADE` never fires. Serving already refuses a
/// non-active repository, so nothing was ever exposed; what lingered was
/// the *name*, and it is unique across the fleet, so the next repository
/// that wanted it would silently have been counted up to `-2`.
#[test]
fn deleting_a_repository_frees_the_hostname_its_site_held() {
    let db = db("sites_repo_delete");
    let org = registry::create_org(&db, "acme").expect("create org");
    let make = |name: &str| {
        registry::create_repo(
            &db,
            &org.id,
            &NewRepo {
                name,
                description: None,
                kind: RepoKind::Native,
                public: true,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .expect("create repo")
        .id
    };

    let first = make("docs");
    sites::create(&db, &first, "docs--acme", None).expect("site");
    let d = sites::add_deploy(&db, &first, "c1", "t1", "dist", false, None).expect("deploy");
    sites::publish(&db, &first, &d.id).expect("publish");

    assert!(registry::delete_repo(&db, &org.id, &first).expect("delete"));

    assert_eq!(sites::get(&db, &first).unwrap(), None);
    assert_eq!(sites::by_host(&db, "docs--acme").unwrap(), None);
    assert_eq!(sites::deploy(&db, &d.id).unwrap(), None);
    assert!(!sites::host_taken(&db, "docs--acme").unwrap());

    // …and the name is genuinely reusable, which is the whole point.
    let second = make("docs2");
    sites::create(&db, &second, "docs--acme", None).expect("the host is free again");
}

/// A site row left behind would answer a hostname forever, for a
/// repository nobody can reach.
#[test]
fn removing_a_site_frees_its_host_and_takes_its_deploys() {
    let db = db("sites_remove");
    let r = repo(&db, "acme", "docs");
    sites::create(&db, &r, "docs--acme", None).expect("site");
    let d = sites::add_deploy(&db, &r, "c1", "t1", "dist", false, None).expect("deploy");
    sites::publish(&db, &r, &d.id).expect("publish");

    sites::remove(&db, &r).expect("remove");

    assert_eq!(sites::get(&db, &r).unwrap(), None);
    assert_eq!(sites::by_host(&db, "docs--acme").unwrap(), None);
    assert_eq!(sites::deploy(&db, &d.id).unwrap(), None);
    assert!(!sites::host_taken(&db, "docs--acme").unwrap());

    // The host is genuinely free again, for a different repository.
    let other = repo(&db, "other", "docs");
    sites::create(&db, &other, "docs--acme", None).expect("host is free");
}
