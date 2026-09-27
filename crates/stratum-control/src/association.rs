//! How a reader should weigh an opinion: the author's association with
//! the repository they are talking about.
//!
//! In a thread full of strangers, "this is fine" from the person who owns
//! the org and "this is fine" from somebody who has never landed anything
//! are different sentences. GitHub answers this with an author
//! association on every comment, and the answer is derived, never stored:
//! it is a *current* fact about a person, and a stored copy would be a
//! second truth that goes stale the moment somebody is promoted or
//! leaves.
//!
//! So nothing here writes. The membership half is
//! [`crate::members::effective_role`] — the one resolver for "what may
//! this person do on this repo", per-repo grants and team grants
//! included — rather than a second copy of that rule. The contribution
//! half is one question of the changes table: has this person landed
//! anything here?

use crate::db::ControlDb;
use crate::ids::valid_id;
use crate::members::Role;

/// Where the author stands relative to this repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Association {
    /// Owns the organization the repository lives in.
    Owner,
    /// Has a role here — org member, per-repo grant, or through a team.
    /// Read-only members included: a viewer is inside the tent.
    Member,
    /// Not a member, but has landed a change on this repository before.
    Contributor,
    /// Not a member and has landed nothing here yet. Rendered so that
    /// somebody's first contribution is *visible* to reviewers, which is
    /// the whole point of distinguishing it.
    FirstTime,
}

impl Association {
    pub fn as_str(&self) -> &'static str {
        match self {
            Association::Owner => "owner",
            Association::Member => "member",
            Association::Contributor => "contributor",
            Association::FirstTime => "first-time",
        }
    }
}

/// The association of `user_id` with `repo_id` in `org_id`.
///
/// Role first, because it is the stronger claim: an org owner who has
/// never landed a commit is still the owner. Only somebody with no role
/// at all is asked the contribution question.
pub fn of(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    user_id: &str,
) -> Result<Association, String> {
    match crate::members::effective_role(db, org_id, Some(repo_id), user_id)? {
        Some(Role::Owner) => Ok(Association::Owner),
        Some(_) => Ok(Association::Member),
        None if has_landed(db, repo_id, user_id)? => Ok(Association::Contributor),
        None => Ok(Association::FirstTime),
    }
}

/// Has this person landed a change on this repository?
///
/// Landed, not opened: the question a badge answers is whether their work
/// has been accepted here before, and an open change is the thing being
/// weighed rather than evidence about it.
fn has_landed(db: &ControlDb, repo_id: &str, user_id: &str) -> Result<bool, String> {
    // An identifier that cannot be a real id is definitionally absent —
    // the same short-circuit the registry lookups take, so hostile bytes
    // never reach a query.
    if !valid_id(user_id) {
        return Ok(false);
    }
    db.lock()
        .query_opt(
            "SELECT 1 FROM changes WHERE repo_id = $1 AND created_by = $2 \
             AND state = 'landed' LIMIT 1",
            &[&repo_id, &user_id],
        )
        .map(|r| r.is_some())
        .map_err(|e| format!("landed changes: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, NewRepo, RepoKind};

    fn make_user(db: &ControlDb, email: &str) -> String {
        crate::users::create(db, email, "Person", Some("a long enough password"))
            .unwrap()
            .id
    }

    fn setup() -> (ControlDb, String, String) {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("association")).unwrap();
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
        (db, org.id, repo.id)
    }

    fn open_change(
        db: &ControlDb,
        org: &str,
        repo: &str,
        key: &str,
        commit: &str,
        user: &str,
    ) -> crate::changes::Change {
        crate::changes::create_or_update(
            db,
            org,
            repo,
            key,
            "work",
            "main",
            commit,
            None,
            &format!("work\n\nChange-Id: {key}\n"),
            Some(user),
            None,
            None,
        )
        .unwrap()
        .unwrap()
        .0
    }

    /// Role decides first, and it is the *effective* role — a per-repo
    /// grant is exactly the case a second, org-only resolver would get
    /// wrong.
    #[test]
    fn role_decides_first_and_a_repo_grant_counts() {
        let (db, org, repo) = setup();
        let owner = make_user(&db, "owner@acme.test");
        let viewer = make_user(&db, "viewer@acme.test");
        let outsider = make_user(&db, "outsider@acme.test");

        crate::members::add(&db, &org, &owner, Role::Owner, None).unwrap();
        crate::members::add(&db, &org, &viewer, Role::Viewer, None).unwrap();
        assert_eq!(of(&db, &org, &repo, &owner).unwrap(), Association::Owner);
        assert_eq!(of(&db, &org, &repo, &viewer).unwrap(), Association::Member);
        assert_eq!(
            of(&db, &org, &repo, &outsider).unwrap(),
            Association::FirstTime
        );

        // An org owner held down to member on this one repository is
        // still the owner: the badge answers "who is this", and a repo
        // grant cannot take the org away.
        crate::members::grant_repo(&db, &repo, &owner, Role::Member, None).unwrap();
        assert_eq!(of(&db, &org, &repo, &owner).unwrap(), Association::Member);
    }

    /// A non-member is a contributor once something of theirs has landed
    /// here, and only then. An open change is not evidence.
    #[test]
    fn landing_is_what_makes_a_contributor() {
        let (db, org, repo) = setup();
        let outsider = make_user(&db, "outsider@acme.test");

        let change = open_change(&db, &org, &repo, "Ibbbb2222", &"a".repeat(40), &outsider);
        assert_eq!(
            of(&db, &org, &repo, &outsider).unwrap(),
            Association::FirstTime,
            "an open change is the thing being weighed, not evidence about it"
        );

        crate::changes::set_landed(&db, &change.id, &"c".repeat(40), "landed").unwrap();
        assert_eq!(
            of(&db, &org, &repo, &outsider).unwrap(),
            Association::Contributor
        );
    }

    /// A landing on a *different* repository says nothing about this one.
    #[test]
    fn a_landing_elsewhere_does_not_travel() {
        let (db, org, repo) = setup();
        let other = registry::create_repo(
            &db,
            &org,
            &NewRepo {
                description: None,
                name: "other",
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        let outsider = make_user(&db, "outsider@acme.test");
        let elsewhere = open_change(
            &db,
            &org,
            &other.id,
            "Icccc3333",
            &"b".repeat(40),
            &outsider,
        );
        crate::changes::set_landed(&db, &elsewhere.id, &"c".repeat(40), "landed").unwrap();

        assert_eq!(
            of(&db, &org, &other.id, &outsider).unwrap(),
            Association::Contributor
        );
        assert_eq!(
            of(&db, &org, &repo, &outsider).unwrap(),
            Association::FirstTime
        );
    }

    /// The names are the wire format; they must not drift silently.
    #[test]
    fn the_names_are_stable() {
        assert_eq!(Association::Owner.as_str(), "owner");
        assert_eq!(Association::Member.as_str(), "member");
        assert_eq!(Association::Contributor.as_str(), "contributor");
        assert_eq!(Association::FirstTime.as_str(), "first-time");
    }

    /// A hostile identifier is absent, not a database error.
    #[test]
    fn an_impossible_identifier_is_simply_absent() {
        let (db, _org, repo) = setup();
        assert!(!has_landed(&db, &repo, "'; DROP TABLE changes; --").unwrap());
        assert!(!has_landed(&db, &repo, "").unwrap());
    }
}
