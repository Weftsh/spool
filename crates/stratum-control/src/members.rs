//! Org membership, roles, and per-repo overrides.
//!
//! A role is not a new policy engine. It resolves to the same `Scope` set
//! `auth.rs` already enforces, so the ~40 inline authorization checks in
//! the server need no knowledge that users exist — which is what keeps
//! this from becoming a second, divergent way to say "may".
//!
//! Precedence is deliberately simple and total. A per-repo grant naming
//! this person, if one exists, *replaces* the org role for that repo. It
//! does not intersect or union with it. That means a grant can raise a
//! viewer to writer on one repo and equally hold an admin down to viewer
//! on a sensitive one, and there is exactly one row to read to know
//! which. Failing that, the answer is the highest of the org role and
//! any grant made to a team this person is in — teams raise only, and
//! [`crate::teams`] has the reasoning.

use crate::auth::{Principal, Scope};
use crate::db::ControlDb;
use crate::ids::{now_ms, valid_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Read anything in the org.
    Viewer,
    /// Read and write repo content.
    Member,
    /// Everything except transferring ownership: members, tokens, keys.
    Admin,
    /// Admin, plus the org itself.
    Owner,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
            Role::Viewer => "viewer",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "owner" => Some(Role::Owner),
            "admin" => Some(Role::Admin),
            "member" => Some(Role::Member),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }

    /// The scopes this role carries. Owner and admin both reach
    /// `OrgAdmin`; the difference between them is enforced where it
    /// matters (ownership transfer, removing the last owner) rather than
    /// by scope, because "may administer" really is the same answer.
    pub fn scopes(&self) -> Vec<Scope> {
        match self {
            Role::Owner | Role::Admin => vec![Scope::OrgAdmin],
            Role::Member => vec![Scope::OrgRead, Scope::RepoWrite],
            Role::Viewer => vec![Scope::OrgRead, Scope::RepoRead],
        }
    }

    /// Does this role carry `need`? Scope implication applies — an
    /// admin's `org:admin` carries everything under it — so this is the
    /// question to ask, never `scopes().contains()`.
    pub fn carries(&self, need: Scope) -> bool {
        self.scopes().iter().any(|s| crate::auth::grants(*s, need))
    }

    /// Roles that may be granted on a single repo. Org ownership is not
    /// a per-repo concept.
    pub fn valid_for_repo_grant(&self) -> bool {
        !matches!(self, Role::Owner)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub org_id: String,
    pub user_id: String,
    pub role: Role,
    pub created_at: i64,
}

/// A member with the user fields the UI needs, so listing members is one
/// query rather than N+1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    pub user_id: String,
    pub email: String,
    pub name: String,
    pub role: Role,
    pub disabled: bool,
    pub created_at: i64,
}

pub fn add(
    db: &ControlDb,
    org_id: &str,
    user_id: &str,
    role: Role,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<Membership, String> {
    // A personal namespace has exactly one member — its owner, added
    // when it was created. Letting a second person in would make
    // "somebody's own repositories" mean something else.
    if crate::registry::is_personal(db, org_id)? {
        return Err("a personal namespace cannot have members — create an organization".into());
    }
    let at = now_ms();
    let blob = serde_json::json!({ "user_id": user_id, "role": role.as_str() });
    db.lock()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO org_members (org_id, user_id, role, created_at) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (org_id, user_id) DO UPDATE SET role = EXCLUDED.role",
                &[&org_id, &user_id, &role.as_str(), &at],
            )?;
            if let Some(ctx) = audit {
                crate::audit::record_tx(tx, ctx, None, "member.add", Some(&blob))?;
            }
            Ok(())
        })
        .map_err(|e| format!("add member: {e}"))?;
    Ok(Membership {
        org_id: org_id.to_string(),
        user_id: user_id.to_string(),
        role,
        created_at: at,
    })
}

/// The role this person holds in this org, or `None` if they hold none.
///
/// A **disabled account has no role anywhere**, which is enforced here
/// rather than at each caller. Bearer tokens and browser sessions each
/// check `disabled_at` in their own resolver, but an SSH key resolves
/// through this function alone — so before this join, disabling somebody
/// left their laptop key cloning. One seam is also the only way a future
/// credential inherits the answer for free.
///
/// Listing members is deliberately *not* built on this: an administrator
/// has to be able to see a disabled colleague in order to re-enable
/// them, and `list` carries a `disabled` flag for exactly that.
pub fn role_of(db: &ControlDb, org_id: &str, user_id: &str) -> Result<Option<Role>, String> {
    if !valid_id(user_id) {
        return Ok(None);
    }
    db.lock()
        .query_opt(
            "SELECT m.role FROM org_members m JOIN users u ON u.id = m.user_id \
             WHERE m.org_id = $1 AND m.user_id = $2 AND u.disabled_at IS NULL",
            &[&org_id, &user_id],
        )
        .map_err(|e| format!("member role: {e}"))
        .map(|r| r.and_then(|r| Role::parse(r.get::<_, String>("role").as_str())))
}

pub fn list(db: &ControlDb, org_id: &str) -> Result<Vec<MemberView>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT m.user_id, m.role, m.created_at, u.email, u.name, u.disabled_at \
             FROM org_members m JOIN users u ON u.id = m.user_id \
             WHERE m.org_id = $1 ORDER BY u.email",
            &[&org_id],
        )
        .map_err(|e| format!("list members: {e}"))?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(MemberView {
                user_id: r.get("user_id"),
                email: r.get("email"),
                name: r.get("name"),
                role: Role::parse(r.get::<_, String>("role").as_str())?,
                disabled: r.get::<_, Option<i64>>("disabled_at").is_some(),
                created_at: r.get("created_at"),
            })
        })
        .collect())
}

/// Orgs this user belongs to. The basis for "which repos may I see" and
/// for search visibility.
pub fn orgs_of(db: &ControlDb, user_id: &str) -> Result<Vec<String>, String> {
    if !valid_id(user_id) {
        return Ok(Vec::new());
    }
    db.lock()
        .query(
            "SELECT org_id FROM org_members WHERE user_id = $1 ORDER BY org_id",
            &[&user_id],
        )
        .map_err(|e| format!("member orgs: {e}"))
        .map(|rows| rows.iter().map(|r| r.get("org_id")).collect())
}

/// Remove a member. Refuses to remove the last owner: an org with no
/// owner cannot be administered by anyone, and there is no super-user to
/// repair it from outside.
pub fn remove(
    db: &ControlDb,
    org_id: &str,
    user_id: &str,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    if !valid_id(user_id) {
        return Ok(false);
    }
    let blob = serde_json::json!({ "user_id": user_id });
    db.lock()
        .transaction(move |tx| {
            let Some(role) = member_role_locked(tx, org_id, user_id)? else {
                return Ok(Ok(false));
            };
            if role == "owner" && count_owners_locked(tx, org_id)? <= 1 {
                return Ok(Err("cannot remove the last owner of an org".to_string()));
            }
            let n = tx.execute(
                "DELETE FROM org_members WHERE org_id = $1 AND user_id = $2",
                &[&org_id, &user_id],
            )?;
            if n > 0 {
                if let Some(ctx) = audit {
                    crate::audit::record_tx(tx, ctx, None, "member.remove", Some(&blob))?;
                }
            }
            Ok(Ok(n > 0))
        })
        .map_err(|e| format!("remove member: {e}"))?
}

/// This member's role, with the row locked for the rest of the
/// transaction.
///
/// The last-owner guard reads a count and then writes, so without the
/// lock two concurrent removals each see two owners, each decide they are
/// not the last, and both proceed — leaving an org nobody can administer
/// and no super-user to repair it. Locking the org's owner rows makes the
/// second one wait and then see the truth.
fn member_role_locked(
    tx: &mut postgres::Transaction,
    org_id: &str,
    user_id: &str,
) -> Result<Option<String>, postgres::Error> {
    Ok(tx
        .query_opt(
            "SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2 FOR UPDATE",
            &[&org_id, &user_id],
        )?
        .map(|r| r.get::<_, String>("role")))
}

/// How many owners this org has, with every owner row locked. `COUNT(*)`
/// cannot take `FOR UPDATE`, so the rows are selected and counted here.
fn count_owners_locked(
    tx: &mut postgres::Transaction,
    org_id: &str,
) -> Result<usize, postgres::Error> {
    Ok(tx
        .query(
            "SELECT user_id FROM org_members WHERE org_id = $1 AND role = 'owner' FOR UPDATE",
            &[&org_id],
        )?
        .len())
}

/// Change a member's role, with the same last-owner guard as removal:
/// demoting the only owner leaves the org unadministrable.
pub fn set_role(
    db: &ControlDb,
    org_id: &str,
    user_id: &str,
    role: Role,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    if !valid_id(user_id) {
        return Ok(false);
    }
    let blob = serde_json::json!({ "user_id": user_id, "role": role.as_str() });
    db.lock()
        .transaction(move |tx| {
            let Some(current) = member_role_locked(tx, org_id, user_id)? else {
                return Ok(Ok(false));
            };
            if current == "owner" && role != Role::Owner && count_owners_locked(tx, org_id)? <= 1 {
                return Ok(Err("cannot demote the last owner of an org".to_string()));
            }
            let n = tx.execute(
                "UPDATE org_members SET role = $3 WHERE org_id = $1 AND user_id = $2",
                &[&org_id, &user_id, &role.as_str()],
            )?;
            if n > 0 {
                if let Some(ctx) = audit {
                    crate::audit::record_tx(tx, ctx, None, "member.role", Some(&blob))?;
                }
            }
            Ok(Ok(n > 0))
        })
        .map_err(|e| format!("set role: {e}"))?
}

/// The most this person can do anywhere in this org: their org role, or
/// a per-repo grant if one gives them more.
///
/// This is the ceiling a personal token may be minted with. Capping at
/// the org role alone would be too strict — a contractor who is a viewer
/// org-wide but a member on one repo genuinely can push somewhere, and
/// must be able to hold a credential that says so. What the token
/// actually does on a given repo is still `effective_role` there.
pub fn max_role(db: &ControlDb, org_id: &str, user_id: &str) -> Result<Option<Role>, String> {
    let Some(org_role) = role_of(db, org_id, user_id)? else {
        return Ok(None);
    };
    let rows = db
        .lock()
        .query(
            "SELECT g.role FROM repo_grants g JOIN repos r ON r.id = g.repo_id \
             WHERE r.org_id = $1 AND g.user_id = $2",
            &[&org_id, &user_id],
        )
        .map_err(|e| format!("max role: {e}"))?;
    let best = rows
        .iter()
        .filter_map(|r| Role::parse(r.get::<_, String>("role").as_str()))
        .fold(org_role, |acc, r| acc.max(r));
    // Team grants count towards the ceiling too, or someone whose only
    // write access comes from a team could not mint a token that writes.
    let team = crate::teams::best_grant_in_org(db, org_id, user_id)?;
    Ok(Some(team.map_or(best, |t| best.max(t))))
}

/// Narrow a principal to one repo.
///
/// A personal credential's authority on a given repo is the *effective*
/// role there — per-repo grants exist precisely to differ from the org
/// role — and which repo is being reached is only known after
/// authentication. So every seam that has resolved a repo passes the
/// principal through here before asking `allows`. A service token has no
/// person whose grants could apply and comes back unchanged; `None` means
/// this person has no access to this repo at all.
pub fn refine_for_repo(
    db: &ControlDb,
    p: &Principal,
    repo_id: &str,
) -> Result<Option<Principal>, String> {
    let Some(user_id) = p.user_id.as_deref() else {
        return Ok(Some(p.clone()));
    };
    let Some(role) = effective_role(db, &p.org_id, Some(repo_id), user_id)? else {
        return Ok(None);
    };
    // The effective role *replaces* the org role here — that is what a
    // grant is — so this is computed from the role, not by narrowing what
    // the org role already produced. Narrowing would make a grant a
    // one-way ratchet: it could take write away and never give it back.
    //
    // A personal token's mint scopes still cap the result. A grant can
    // raise you to what your token was issued for, never past it.
    let scopes = match &p.ceiling {
        Some(ceiling) => crate::auth::intersect(&role.scopes(), ceiling),
        None => role.scopes(),
    };
    Ok(Some(Principal {
        scopes,
        ..p.clone()
    }))
}

// ----------------------------------------------------------- repo grants

pub fn grant_repo(
    db: &ControlDb,
    repo_id: &str,
    user_id: &str,
    role: Role,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<(), String> {
    if !valid_id(user_id) {
        return Err("no such user".into());
    }
    if !role.valid_for_repo_grant() {
        return Err("owner is an org role, not a repo role".into());
    }
    let at = now_ms();
    let blob = serde_json::json!({ "user_id": user_id, "role": role.as_str() });
    db.lock()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO repo_grants (repo_id, user_id, role, created_at) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (repo_id, user_id) DO UPDATE SET role = EXCLUDED.role",
                &[&repo_id, &user_id, &role.as_str(), &at],
            )?;
            if let Some(ctx) = audit {
                crate::audit::record_tx(tx, ctx, Some(repo_id), "repo.grant", Some(&blob))?;
            }
            Ok(())
        })
        .map_err(|e| format!("grant repo: {e}"))
}

pub fn revoke_repo_grant(
    db: &ControlDb,
    repo_id: &str,
    user_id: &str,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    if !valid_id(user_id) {
        return Ok(false);
    }
    let blob = serde_json::json!({ "user_id": user_id });
    db.lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "DELETE FROM repo_grants WHERE repo_id = $1 AND user_id = $2",
                &[&repo_id, &user_id],
            )?;
            if n > 0 {
                if let Some(ctx) = audit {
                    crate::audit::record_tx(
                        tx,
                        ctx,
                        Some(repo_id),
                        "repo.grant.revoke",
                        Some(&blob),
                    )?;
                }
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("revoke grant: {e}"))
}

pub fn repo_grant(db: &ControlDb, repo_id: &str, user_id: &str) -> Result<Option<Role>, String> {
    if !valid_id(user_id) {
        return Ok(None);
    }
    db.lock()
        .query_opt(
            "SELECT role FROM repo_grants WHERE repo_id = $1 AND user_id = $2",
            &[&repo_id, &user_id],
        )
        .map_err(|e| format!("repo grant: {e}"))
        .map(|r| r.and_then(|r| Role::parse(r.get::<_, String>("role").as_str())))
}

/// The role that actually applies to this user on this repo: the grant if
/// there is one, otherwise the org role. `None` means no access at all.
pub fn effective_role(
    db: &ControlDb,
    org_id: &str,
    repo_id: Option<&str>,
    user_id: &str,
) -> Result<Option<Role>, String> {
    let org_role = role_of(db, org_id, user_id)?;
    // A grant is worthless without membership: removing someone from the
    // org must remove their access everywhere, not leave per-repo holes.
    let Some(org_role) = org_role else {
        return Ok(None);
    };
    let Some(repo) = repo_id else {
        return Ok(Some(org_role));
    };
    // A grant naming this person wins outright — that is what makes it
    // the way to *lower* someone on one repo.
    if let Some(direct) = repo_grant(db, repo, user_id)? {
        return Ok(Some(direct));
    }
    // Otherwise the highest of the org role and any team grant. Teams
    // raise only: nobody reads a team's grant list before adding a
    // colleague to it, so joining one must never take access away.
    let team = crate::teams::best_grant_for(db, repo, user_id)?;
    Ok(Some(team.map_or(org_role, |t| org_role.max(t))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, NewRepo, RepoKind};

    fn make_repo(db: &ControlDb, org: &str, name: &str) -> registry::Repo {
        registry::create_repo(
            db,
            org,
            &NewRepo {
                description: None,
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

    fn setup() -> (ControlDb, String, String) {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("members")).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let user = crate::users::create(&db, "a@example.com", "A", Some("a long enough password"))
            .unwrap();
        (db, org.id, user.id)
    }

    /// A disabled account holds no role anywhere.
    ///
    /// Enforced in `role_of` rather than at each caller: tokens and
    /// sessions check `disabled_at` in their own resolvers, but an SSH
    /// key resolves through this function alone — so before the join,
    /// disabling somebody left their laptop key cloning.
    #[test]
    fn a_disabled_account_holds_no_role() {
        let (db, org, user) = setup();
        add(&db, &org, &user, Role::Admin, None).unwrap();
        assert_eq!(role_of(&db, &org, &user).unwrap(), Some(Role::Admin));
        assert_eq!(max_role(&db, &org, &user).unwrap(), Some(Role::Admin));
        assert!(effective_role(&db, &org, None, &user).unwrap().is_some());

        crate::users::set_disabled(&db, &user, true).unwrap();
        assert_eq!(role_of(&db, &org, &user).unwrap(), None);
        assert_eq!(max_role(&db, &org, &user).unwrap(), None);
        assert_eq!(effective_role(&db, &org, None, &user).unwrap(), None);

        // Still visible to an administrator, flagged — somebody has to be
        // able to see them in order to put them back.
        let listed = list(&db, &org).unwrap();
        let row = listed.iter().find(|m| m.user_id == user).expect("listed");
        assert!(row.disabled);
        assert_eq!(row.role, Role::Admin, "the role is kept, not erased");

        // And re-enabling restores exactly what they had.
        crate::users::set_disabled(&db, &user, false).unwrap();
        assert_eq!(role_of(&db, &org, &user).unwrap(), Some(Role::Admin));
    }

    #[test]
    fn roles_round_trip_and_map_to_scopes() {
        for r in [Role::Owner, Role::Admin, Role::Member, Role::Viewer] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
            assert!(!r.scopes().is_empty());
        }
        assert_eq!(Role::parse("root"), None);
        assert_eq!(Role::parse(""), None);

        // A viewer must never carry write.
        assert!(!Role::Viewer.scopes().contains(&Scope::RepoWrite));
        assert!(Role::Member.scopes().contains(&Scope::RepoWrite));
        assert!(!Role::Member.scopes().contains(&Scope::OrgAdmin));
        assert!(Role::Admin.scopes().contains(&Scope::OrgAdmin));

        // Ordering is meaningful: it is what "at least admin" compares.
        assert!(Role::Owner > Role::Admin);
        assert!(Role::Admin > Role::Member);
        assert!(Role::Member > Role::Viewer);

        // Owner is an org role only.
        assert!(!Role::Owner.valid_for_repo_grant());
        assert!(Role::Admin.valid_for_repo_grant());
    }

    #[test]
    fn membership_lifecycle() {
        let (db, org, user) = setup();
        assert!(role_of(&db, &org, &user).unwrap().is_none());
        assert!(orgs_of(&db, &user).unwrap().is_empty());

        add(&db, &org, &user, Role::Member, None).unwrap();
        assert_eq!(role_of(&db, &org, &user).unwrap(), Some(Role::Member));
        assert_eq!(orgs_of(&db, &user).unwrap(), vec![org.clone()]);

        // Adding again updates rather than duplicating.
        add(&db, &org, &user, Role::Admin, None).unwrap();
        assert_eq!(role_of(&db, &org, &user).unwrap(), Some(Role::Admin));

        let members = list(&db, &org).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].email, "a@example.com");
        assert_eq!(members[0].role, Role::Admin);
        assert!(!members[0].disabled);

        assert!(set_role(&db, &org, &user, Role::Viewer, None).unwrap());
        assert_eq!(role_of(&db, &org, &user).unwrap(), Some(Role::Viewer));
        assert!(!set_role(&db, &org, "ghost", Role::Viewer, None).unwrap());

        assert!(remove(&db, &org, &user, None).unwrap());
        assert!(role_of(&db, &org, &user).unwrap().is_none());
        assert!(!remove(&db, &org, &user, None).unwrap());
    }

    /// An org with no owner cannot be administered by anyone, and there
    /// is no super-user to repair it from outside — so the last owner is
    /// neither removable nor demotable.
    #[test]
    fn the_last_owner_cannot_be_removed_or_demoted() {
        let (db, org, owner) = setup();
        add(&db, &org, &owner, Role::Owner, None).unwrap();

        assert!(remove(&db, &org, &owner, None).is_err());
        assert!(set_role(&db, &org, &owner, Role::Admin, None).is_err());
        assert_eq!(role_of(&db, &org, &owner).unwrap(), Some(Role::Owner));

        // With a second owner, both operations are allowed again.
        let other = crate::users::create(&db, "b@example.com", "B", Some("a long enough password"))
            .unwrap();
        add(&db, &org, &other.id, Role::Owner, None).unwrap();
        assert!(set_role(&db, &org, &owner, Role::Admin, None).unwrap());

        // Removal is likewise allowed while a second owner still exists —
        // the guard is about the last one, not about owners generally.
        add(&db, &org, &owner, Role::Owner, None).unwrap();
        assert!(remove(&db, &org, &owner, None).unwrap());
        assert!(
            remove(&db, &org, &other.id, None).is_err(),
            "now the last owner"
        );
    }

    /// Two owners, two concurrent removals, each removing the other.
    ///
    /// The guard reads a count and then deletes. Without the row lock
    /// both transactions see two owners, both decide they are not
    /// removing the last one, and both commit — leaving an org nobody can
    /// administer and no super-user to repair it. Two real connections,
    /// because a single `ControlDb` serialises on its own mutex and could
    /// never expose this.
    #[test]
    fn concurrent_removals_cannot_strand_an_org_without_an_owner() {
        let url = stratum_testkit::pg::test_db_url("members-race");
        let db = ControlDb::open(&url).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let a = crate::users::create(&db, "a@example.com", "A", Some("a long enough password"))
            .unwrap();
        let b = crate::users::create(&db, "b@example.com", "B", Some("a long enough password"))
            .unwrap();
        add(&db, &org.id, &a.id, Role::Owner, None).unwrap();
        add(&db, &org.id, &b.id, Role::Owner, None).unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for (me, them) in [(a.id.clone(), b.id.clone()), (b.id.clone(), a.id.clone())] {
            let (url, org_id, barrier) = (url.clone(), org.id.clone(), barrier.clone());
            let _ = me;
            handles.push(std::thread::spawn(move || {
                let db = ControlDb::open(&url).unwrap();
                barrier.wait();
                remove(&db, &org_id, &them, None)
            }));
        }
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let left = list(&db, &org.id).unwrap();
        let owners = left.iter().filter(|m| m.role == Role::Owner).count();
        assert_eq!(
            owners, 1,
            "both removals committed and left {owners} owners: {outcomes:?}"
        );
        // One removal succeeded; the other was refused or removed nobody.
        assert_eq!(
            outcomes.iter().filter(|o| matches!(o, Ok(true))).count(),
            1,
            "{outcomes:?}"
        );
    }

    /// An id that cannot exist names nothing. Every lookup keyed by a
    /// user id answers "absent" for a malformed one rather than letting
    /// hostile bytes reach a query, where a NUL would surface as a
    /// database error instead of the "not found" the contract promises.
    #[test]
    fn malformed_user_ids_are_inert_on_every_path() {
        let (db, org, _user) = setup();
        let repo = make_repo(&db, &org, "app");
        for bad in ["", "ghost", "not an id", "\0", "01hx'; DROP TABLE users;--"] {
            assert!(orgs_of(&db, bad).unwrap().is_empty(), "orgs_of {bad:?}");
            assert!(
                role_of(&db, &org, bad).unwrap().is_none(),
                "role_of {bad:?}"
            );
            assert!(!remove(&db, &org, bad, None).unwrap(), "remove {bad:?}");
            assert!(!set_role(&db, &org, bad, Role::Viewer, None).unwrap());
            assert!(max_role(&db, &org, bad).unwrap().is_none());
            assert!(grant_repo(&db, &repo.id, bad, Role::Member, None).is_err());
            assert!(!revoke_repo_grant(&db, &repo.id, bad, None).unwrap());
            assert!(repo_grant(&db, &repo.id, bad).unwrap().is_none());
        }
    }

    /// A well-formed id for someone who is simply not here is absent, not
    /// an error — the distinction the API turns into 404 rather than 500.
    #[test]
    fn a_well_formed_stranger_is_absent_rather_than_an_error() {
        let (db, org, _user) = setup();
        let stranger = crate::ids::ulid();
        assert!(role_of(&db, &org, &stranger).unwrap().is_none());
        assert!(max_role(&db, &org, &stranger).unwrap().is_none());
        assert!(!set_role(&db, &org, &stranger, Role::Viewer, None).unwrap());
        assert!(!remove(&db, &org, &stranger, None).unwrap());
    }

    #[test]
    fn a_repo_grant_replaces_the_org_role_for_that_repo_only() {
        let (db, org, user) = setup();
        let repo = make_repo(&db, &org, "app");
        let other = make_repo(&db, &org, "other");
        add(&db, &org, &user, Role::Viewer, None).unwrap();

        // Without a grant, the org role applies everywhere.
        assert_eq!(
            effective_role(&db, &org, Some(&repo.id), &user).unwrap(),
            Some(Role::Viewer)
        );

        // A grant raises this user on one repo and leaves the other alone.
        grant_repo(&db, &repo.id, &user, Role::Member, None).unwrap();
        assert_eq!(
            effective_role(&db, &org, Some(&repo.id), &user).unwrap(),
            Some(Role::Member)
        );
        assert_eq!(
            effective_role(&db, &org, Some(&other.id), &user).unwrap(),
            Some(Role::Viewer)
        );
        // …and the org-level answer is untouched by a per-repo grant.
        assert_eq!(
            effective_role(&db, &org, None, &user).unwrap(),
            Some(Role::Viewer)
        );

        // It cuts both ways: a grant can hold an admin down on one repo.
        add(&db, &org, &user, Role::Admin, None).unwrap();
        grant_repo(&db, &repo.id, &user, Role::Viewer, None).unwrap();
        assert_eq!(
            effective_role(&db, &org, Some(&repo.id), &user).unwrap(),
            Some(Role::Viewer)
        );
        assert_eq!(
            effective_role(&db, &org, Some(&other.id), &user).unwrap(),
            Some(Role::Admin)
        );

        assert!(revoke_repo_grant(&db, &repo.id, &user, None).unwrap());
        assert!(!revoke_repo_grant(&db, &repo.id, &user, None).unwrap());
        assert_eq!(
            effective_role(&db, &org, Some(&repo.id), &user).unwrap(),
            Some(Role::Admin)
        );

        // Owner is not grantable per repo.
        assert!(grant_repo(&db, &repo.id, &user, Role::Owner, None).is_err());
    }

    /// Removing someone from the org must remove their access everywhere.
    /// A stale per-repo grant surviving a removal would be a back door
    /// into exactly the repo they were most trusted with.
    #[test]
    fn a_repo_grant_is_worthless_without_membership() {
        let (db, org, user) = setup();
        let repo = make_repo(&db, &org, "app");
        add(&db, &org, &user, Role::Member, None).unwrap();
        grant_repo(&db, &repo.id, &user, Role::Admin, None).unwrap();
        assert_eq!(
            effective_role(&db, &org, Some(&repo.id), &user).unwrap(),
            Some(Role::Admin)
        );

        remove(&db, &org, &user, None).unwrap();
        assert_eq!(
            effective_role(&db, &org, Some(&repo.id), &user).unwrap(),
            None
        );
        assert_eq!(effective_role(&db, &org, None, &user).unwrap(), None);
        // The grant row still exists; it simply grants nothing.
        assert_eq!(repo_grant(&db, &repo.id, &user).unwrap(), Some(Role::Admin));
    }

    #[test]
    fn a_non_member_has_no_role_anywhere() {
        let (db, org, _user) = setup();
        let stranger =
            crate::users::create(&db, "z@example.com", "Z", Some("a long enough password"))
                .unwrap();
        assert_eq!(effective_role(&db, &org, None, &stranger.id).unwrap(), None);
        assert!(list(&db, &org).unwrap().is_empty());
        assert!(orgs_of(&db, &stranger.id).unwrap().is_empty());
    }
}
