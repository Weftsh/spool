//! Resolution of OWNERS entries to people, at evaluation time.
//!
//! An OWNERS file names emails and teams; sufficiency needs user ids.
//! Resolving late — against the control plane, on every evaluation — is
//! what makes ownership follow org structure: someone joining a team, or
//! leaving the org, changes every verdict on the next request with no
//! file edit.

use crate::review::owners::{OwnerEntry, RuleOutcome};
use crate::review::sufficiency::{Approver, Requirement};
use std::collections::BTreeSet;
use stratum_control::members::Role;
use stratum_control::{members, teams, users, ControlDb};

/// A resolved view of one path's entries, for display and evaluation.
#[derive(Debug, Clone, Default)]
pub struct ResolvedOwners {
    /// Every user id an approval can come from to count as an owner.
    pub owner_ids: BTreeSet<String>,
    /// The entries as written, for explanations.
    pub display: Vec<String>,
    /// A `*` entry: anyone with write access.
    pub anyone_with_write: bool,
    /// Known people, as (email, name).
    pub users: Vec<(String, String)>,
    /// Known teams, as (name, member_count).
    pub teams: Vec<(String, i64)>,
    /// Entries that resolved to nobody in this org (unknown email,
    /// unknown team, or a person who is not an org member). Displayed so
    /// a stale OWNERS file is visible instead of silently unsatisfiable.
    pub unknown: Vec<String>,
}

/// Resolve entries against the org's members and teams.
pub fn resolve_entries(
    db: &ControlDb,
    org_id: &str,
    entries: &[OwnerEntry],
) -> Result<ResolvedOwners, String> {
    let mut out = ResolvedOwners::default();
    for entry in entries {
        out.display.push(entry.display());
        match entry {
            OwnerEntry::Anyone => out.anyone_with_write = true,
            OwnerEntry::User(email) => match users::by_email(db, email)? {
                // Membership is the gate: an OWNERS entry cannot grant a
                // stranger standing in the org's approvals.
                Some(u) if members::role_of(db, org_id, &u.id)?.is_some() => {
                    out.owner_ids.insert(u.id.clone());
                    out.users.push((u.email, u.name));
                }
                _ => out.unknown.push(entry.display()),
            },
            OwnerEntry::Team(name) => {
                let team = teams::list(db, org_id)?
                    .into_iter()
                    .find(|t| t.name.eq_ignore_ascii_case(name));
                match team {
                    Some(t) => {
                        let members = teams::members(db, &t.id)?;
                        out.teams.push((t.name, members.len() as i64));
                        for m in members {
                            out.owner_ids.insert(m.user_id);
                        }
                    }
                    None => out.unknown.push(entry.display()),
                }
            }
        }
    }
    Ok(out)
}

/// Everyone with effective write access to the repo, by user id.
pub fn writer_ids(db: &ControlDb, org_id: &str, repo_id: &str) -> Result<BTreeSet<String>, String> {
    let mut out = BTreeSet::new();
    for m in members::list(db, org_id)? {
        if m.disabled {
            continue;
        }
        let role = members::effective_role(db, org_id, Some(repo_id), &m.user_id)?;
        if role.is_some_and(|r| r >= Role::Member) {
            out.insert(m.user_id);
        }
    }
    Ok(out)
}

/// Turn one path's rule outcome into the requirement sufficiency judges,
/// plus the resolved view the API displays.
pub fn requirement_for(
    db: &ControlDb,
    org_id: &str,
    outcome: &RuleOutcome,
) -> Result<(Requirement, ResolvedOwners), String> {
    match outcome {
        RuleOutcome::Error { dir, line, message } => Ok((
            Requirement::OwnersError {
                dir: dir.clone(),
                line: *line,
                message: message.clone(),
            },
            ResolvedOwners::default(),
        )),
        RuleOutcome::Rules { entries, .. } if entries.is_empty() => {
            Ok((Requirement::Ungoverned, ResolvedOwners::default()))
        }
        RuleOutcome::Rules { entries, .. } => {
            let resolved = resolve_entries(db, org_id, entries)?;
            Ok((
                Requirement::Owned {
                    owner_ids: resolved.owner_ids.clone(),
                    display: resolved.display.clone(),
                    anyone_with_write: resolved.anyone_with_write,
                },
                resolved,
            ))
        }
    }
}

/// Resolve a list of emails to approvers; unknown emails and non-members
/// are returned separately rather than silently dropped.
pub fn approvers_from_emails(
    db: &ControlDb,
    org_id: &str,
    emails: &[String],
) -> Result<(Vec<Approver>, Vec<String>), String> {
    let mut approvers = Vec::new();
    let mut unknown = Vec::new();
    for email in emails {
        let normalized = users::normalize_email(email);
        match users::by_email(db, &normalized)? {
            Some(u) if members::role_of(db, org_id, &u.id)?.is_some() => {
                if !approvers.iter().any(|a: &Approver| a.user_id == u.id) {
                    approvers.push(Approver {
                        user_id: u.id,
                        email: u.email,
                    });
                }
            }
            _ => unknown.push(normalized),
        }
    }
    Ok((approvers, unknown))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::owners;
    use stratum_control::registry::{self, NewRepo, RepoKind};

    fn org_with_people(hint: &str) -> (ControlDb, String, String, String, String) {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let alice = users::create(&db, "alice@example.com", "Alice", None).unwrap();
        let bob = users::create(&db, "bob@example.com", "Bob", None).unwrap();
        members::add(&db, &org.id, &alice.id, Role::Member, None).unwrap();
        members::add(&db, &org.id, &bob.id, Role::Viewer, None).unwrap();
        (db, org.id, alice.id, bob.id, alice.email)
    }

    fn make_repo(db: &ControlDb, org_id: &str) -> registry::Repo {
        registry::create_repo(
            db,
            org_id,
            &NewRepo {
                name: "app",
                kind: RepoKind::Native,
                description: None,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn users_resolve_only_when_they_are_org_members() {
        let (db, org_id, alice_id, _, _) = org_with_people("resolve-users");
        // A user who exists but belongs to no org must not resolve.
        users::create(&db, "stranger@example.com", "Stranger", None).unwrap();
        let entries = vec![
            OwnerEntry::User("alice@example.com".into()),
            OwnerEntry::User("stranger@example.com".into()),
            OwnerEntry::User("ghost@example.com".into()),
        ];
        let r = resolve_entries(&db, &org_id, &entries).unwrap();
        assert_eq!(r.owner_ids, [alice_id].into_iter().collect());
        assert_eq!(r.users, vec![("alice@example.com".into(), "Alice".into())]);
        assert_eq!(
            r.unknown,
            vec![
                "stranger@example.com".to_string(),
                "ghost@example.com".to_string()
            ]
        );
        assert!(!r.anyone_with_write);
    }

    #[test]
    fn teams_resolve_to_their_current_membership() {
        let (db, org_id, alice_id, bob_id, _) = org_with_people("resolve-teams");
        let team = teams::create(&db, &org_id, "payments", None, None).unwrap();
        teams::add_member(&db, &org_id, &team.id, &alice_id, None)
            .unwrap()
            .unwrap();
        let entries = vec![
            OwnerEntry::Team("payments".into()),
            OwnerEntry::Team("nonesuch".into()),
        ];
        let r = resolve_entries(&db, &org_id, &entries).unwrap();
        assert_eq!(r.owner_ids, [alice_id.clone()].into_iter().collect());
        assert_eq!(r.teams, vec![("payments".into(), 1)]);
        assert_eq!(r.unknown, vec!["@nonesuch".to_string()]);
        assert_eq!(r.display, vec!["@payments", "@nonesuch"]);
        // Membership is read at evaluation time: adding Bob changes the
        // next resolution with no file edit.
        teams::add_member(&db, &org_id, &team.id, &bob_id, None)
            .unwrap()
            .unwrap();
        let r2 = resolve_entries(&db, &org_id, &entries).unwrap();
        assert!(r2.owner_ids.contains(&bob_id));
    }

    #[test]
    fn writer_ids_follow_effective_role_not_org_role() {
        let (db, org_id, alice_id, bob_id, _) = org_with_people("resolve-writers");
        let repo = make_repo(&db, &org_id);
        // Alice is a member (writer); Bob is a viewer (not).
        let w = writer_ids(&db, &org_id, &repo.id).unwrap();
        assert!(w.contains(&alice_id));
        assert!(!w.contains(&bob_id));
        // A direct grant raises Bob on this repo only.
        members::grant_repo(&db, &repo.id, &bob_id, Role::Member, None).unwrap();
        let w2 = writer_ids(&db, &org_id, &repo.id).unwrap();
        assert!(w2.contains(&bob_id));
        // A disabled account is not a writer, whatever its role says: a
        // departed employee must fall out of `*` and ungoverned rules on
        // the next evaluation, not at the next file edit.
        users::set_disabled(&db, &alice_id, true).unwrap();
        let w3 = writer_ids(&db, &org_id, &repo.id).unwrap();
        assert!(!w3.contains(&alice_id));
    }

    #[test]
    fn requirement_for_maps_each_outcome_shape() {
        let (db, org_id, alice_id, _, _) = org_with_people("resolve-req");
        // Parse error passes through.
        let err = RuleOutcome::Error {
            dir: "payments".into(),
            line: 2,
            message: "m".into(),
        };
        let (req, _) = requirement_for(&db, &org_id, &err).unwrap();
        assert!(matches!(req, Requirement::OwnersError { .. }));
        // Empty rules mean ungoverned.
        let empty = owners::effective_owners("x.rs", &Default::default());
        let (req, _) = requirement_for(&db, &org_id, &empty).unwrap();
        assert!(matches!(req, Requirement::Ungoverned));
        // Entries resolve to owner ids.
        let mut files = std::collections::BTreeMap::new();
        files.insert(
            String::new(),
            Ok(owners::parse("alice@example.com\n*").unwrap()),
        );
        let rules = owners::effective_owners("x.rs", &files);
        let (req, resolved) = requirement_for(&db, &org_id, &rules).unwrap();
        match req {
            Requirement::Owned {
                owner_ids,
                display,
                anyone_with_write,
            } => {
                assert!(owner_ids.contains(&alice_id));
                assert_eq!(display, vec!["alice@example.com", "*"]);
                assert!(anyone_with_write);
            }
            other => panic!("expected owned, got {other:?}"),
        }
        assert!(resolved.anyone_with_write);
    }

    #[test]
    fn approver_emails_resolve_case_insensitively_and_dedupe() {
        let (db, org_id, alice_id, _, _) = org_with_people("resolve-approvers");
        let (approvers, unknown) = approvers_from_emails(
            &db,
            &org_id,
            &[
                "Alice@Example.COM".to_string(),
                "alice@example.com".to_string(),
                "ghost@example.com".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(approvers.len(), 1);
        assert_eq!(approvers[0].user_id, alice_id);
        assert_eq!(unknown, vec!["ghost@example.com".to_string()]);
    }
}
