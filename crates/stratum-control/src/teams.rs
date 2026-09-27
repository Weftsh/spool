//! Teams, and the repo access they carry.
//!
//! Per-repo access used to be one row per (repo, person), granted one
//! person at a time. "The payments squad can write here" is a single
//! statement about the org, and saying it person by person means it
//! drifts the moment somebody joins or leaves.
//!
//! The two kinds of grant behave differently on purpose, and the
//! difference is the whole design:
//!
//! * A **per-user** grant ([`crate::members::grant_repo`]) *replaces* the
//!   org role on that repo, in either direction. It can raise a viewer to
//!   writer, and equally hold an admin down to viewer on something
//!   sensitive.
//! * A **team** grant only ever *raises*. Being added to a team is how
//!   people get more access; it must never be how they quietly lose some,
//!   because nobody reads a team's grant list before adding a colleague
//!   to it. Lowering stays a deliberate, per-person act.
//!
//! So the resolution order is: a per-user grant wins outright if one
//! exists; otherwise the answer is the highest of the org role and every
//! team grant on that repo. Two teams disagreeing takes the higher —
//! [`crate::members::Role`] is `Ord` for exactly this.

use crate::db::{is_unique_violation, ControlDb};
use crate::ids::{now_ms, ulid, valid_id};
use crate::members::Role;
use crate::registry::valid_name;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Team {
    pub id: String,
    pub org_id: String,
    pub name: String,
    pub description: Option<String>,
    pub created_at: i64,
    /// How many people are in it. Listing teams without this means the
    /// UI does N+1 queries to draw one screen.
    pub member_count: i64,
}

fn row_to_team(r: &postgres::Row) -> Team {
    Team {
        id: r.get("id"),
        org_id: r.get("org_id"),
        name: r.get("name"),
        description: r.get("description"),
        created_at: r.get("created_at"),
        member_count: r.try_get("member_count").unwrap_or(0),
    }
}

const LIST_SQL: &str = "SELECT t.id, t.org_id, t.name, t.description, t.created_at, \
                        COUNT(m.user_id) AS member_count \
                        FROM teams t LEFT JOIN team_members m ON m.team_id = t.id ";

pub fn create(
    db: &ControlDb,
    org_id: &str,
    name: &str,
    description: Option<&str>,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<Team, String> {
    // Same shape rules as an org or repo name: a team name goes in URLs
    // and in the audit trail, so it gets the same alphabet.
    if !valid_name(name) {
        return Err(format!("invalid team name {name:?}"));
    }
    if description.is_some_and(|d| d.len() > 500) {
        return Err("description is too long".into());
    }
    // A team is a way to talk about several people at once, and a
    // personal namespace has one.
    if crate::registry::is_personal(db, org_id)? {
        return Err("a personal namespace cannot have teams — create an organization".into());
    }
    let team = Team {
        id: ulid(),
        org_id: org_id.to_string(),
        name: name.to_string(),
        description: description.map(str::to_string),
        created_at: now_ms(),
        member_count: 0,
    };
    let blob = serde_json::json!({ "team_id": team.id, "name": name });
    let t = team.clone();
    db.lock()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO teams (id, org_id, name, description, created_at) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[&t.id, &t.org_id, &t.name, &t.description, &t.created_at],
            )?;
            if let Some(ctx) = audit {
                crate::audit::record_tx(tx, ctx, None, "team.create", Some(&blob))?;
            }
            Ok(())
        })
        .map_err(|e| {
            if is_unique_violation(&e) {
                format!("team {name:?} already exists")
            } else {
                format!("create team: {e}")
            }
        })?;
    Ok(team)
}

pub fn list(db: &ControlDb, org_id: &str) -> Result<Vec<Team>, String> {
    let rows = db
        .lock()
        .query(
            &format!("{LIST_SQL} WHERE t.org_id = $1 GROUP BY t.id ORDER BY lower(t.name)"),
            &[&org_id],
        )
        .map_err(|e| format!("list teams: {e}"))?;
    Ok(rows.iter().map(row_to_team).collect())
}

/// One team, scoped to its org — so a team id from another org reads as
/// absent rather than as somebody else's team.
pub fn by_id(db: &ControlDb, org_id: &str, team_id: &str) -> Result<Option<Team>, String> {
    if !valid_id(team_id) {
        return Ok(None);
    }
    let row = db
        .lock()
        .query_opt(
            &format!("{LIST_SQL} WHERE t.org_id = $1 AND t.id = $2 GROUP BY t.id"),
            &[&org_id, &team_id],
        )
        .map_err(|e| format!("team: {e}"))?;
    Ok(row.as_ref().map(row_to_team))
}

pub fn update(
    db: &ControlDb,
    org_id: &str,
    team_id: &str,
    name: Option<&str>,
    description: Option<&str>,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    if !valid_id(team_id) {
        return Ok(false);
    }
    if name.is_some_and(|n| !valid_name(n)) {
        return Err(format!("invalid team name {:?}", name.unwrap_or_default()));
    }
    if description.is_some_and(|d| d.len() > 500) {
        return Err("description is too long".into());
    }
    let blob = serde_json::json!({ "team_id": team_id, "name": name });
    db.lock()
        .transaction(move |tx| {
            // COALESCE so a PATCH may change either field alone.
            let n = tx.execute(
                "UPDATE teams SET name = COALESCE($3, name), \
                                  description = COALESCE($4, description) \
                 WHERE org_id = $1 AND id = $2",
                &[&org_id, &team_id, &name, &description],
            )?;
            if n > 0 {
                if let Some(ctx) = audit {
                    crate::audit::record_tx(tx, ctx, None, "team.update", Some(&blob))?;
                }
            }
            Ok(n > 0)
        })
        .map_err(|e| {
            if is_unique_violation(&e) {
                format!("team {:?} already exists", name.unwrap_or_default())
            } else {
                format!("update team: {e}")
            }
        })
}

/// Delete a team. Its membership and its grants go with it — that is the
/// point of deleting it, and the schema's ON DELETE CASCADE means the
/// access is gone on the very next request rather than at some later
/// sweep.
pub fn delete(
    db: &ControlDb,
    org_id: &str,
    team_id: &str,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    if !valid_id(team_id) {
        return Ok(false);
    }
    let blob = serde_json::json!({ "team_id": team_id });
    db.lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "DELETE FROM teams WHERE org_id = $1 AND id = $2",
                &[&org_id, &team_id],
            )?;
            if n > 0 {
                if let Some(ctx) = audit {
                    crate::audit::record_tx(tx, ctx, None, "team.delete", Some(&blob))?;
                }
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("delete team: {e}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamMember {
    pub user_id: String,
    pub email: String,
    pub name: String,
    pub created_at: i64,
}

pub fn members(db: &ControlDb, team_id: &str) -> Result<Vec<TeamMember>, String> {
    if !valid_id(team_id) {
        return Ok(Vec::new());
    }
    let rows = db
        .lock()
        .query(
            "SELECT tm.user_id, tm.created_at, u.email, u.name \
             FROM team_members tm JOIN users u ON u.id = tm.user_id \
             WHERE tm.team_id = $1 ORDER BY lower(u.email)",
            &[&team_id],
        )
        .map_err(|e| format!("team members: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| TeamMember {
            user_id: r.get("user_id"),
            email: r.get("email"),
            name: r.get("name"),
            created_at: r.get("created_at"),
        })
        .collect())
}

/// Add someone to a team.
///
/// Team membership requires org membership, checked here rather than
/// left to resolution: a team is a subset of the org, and a team row for
/// a stranger would be a grant with no role to raise from. Both reads
/// happen inside the transaction so a concurrent org removal cannot slip
/// between the check and the insert.
pub fn add_member(
    db: &ControlDb,
    org_id: &str,
    team_id: &str,
    user_id: &str,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<Result<bool, String>, String> {
    if !valid_id(team_id) || !valid_id(user_id) {
        return Ok(Err("no such team or user".into()));
    }
    let at = now_ms();
    let blob = serde_json::json!({ "team_id": team_id, "user_id": user_id });
    db.lock()
        .transaction(move |tx| {
            if tx
                .query_opt(
                    "SELECT id FROM teams WHERE org_id = $1 AND id = $2",
                    &[&org_id, &team_id],
                )?
                .is_none()
            {
                return Ok(Err("no such team".to_string()));
            }
            if tx
                .query_opt(
                    "SELECT user_id FROM org_members WHERE org_id = $1 AND user_id = $2 FOR UPDATE",
                    &[&org_id, &user_id],
                )?
                .is_none()
            {
                return Ok(Err("not a member of this org".to_string()));
            }
            let n = tx.execute(
                "INSERT INTO team_members (team_id, user_id, created_at) \
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                &[&team_id, &user_id, &at],
            )?;
            if n > 0 {
                if let Some(ctx) = audit {
                    crate::audit::record_tx(tx, ctx, None, "team.member.add", Some(&blob))?;
                }
            }
            Ok(Ok(n > 0))
        })
        .map_err(|e| format!("add team member: {e}"))
}

pub fn remove_member(
    db: &ControlDb,
    org_id: &str,
    team_id: &str,
    user_id: &str,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    if !valid_id(team_id) || !valid_id(user_id) {
        return Ok(false);
    }
    let blob = serde_json::json!({ "team_id": team_id, "user_id": user_id });
    db.lock()
        .transaction(move |tx| {
            // The join to `teams` keeps this org-scoped: a team id from
            // another org deletes nothing.
            let n = tx.execute(
                "DELETE FROM team_members tm USING teams t \
                 WHERE tm.team_id = t.id AND t.org_id = $1 \
                   AND tm.team_id = $2 AND tm.user_id = $3",
                &[&org_id, &team_id, &user_id],
            )?;
            if n > 0 {
                if let Some(ctx) = audit {
                    crate::audit::record_tx(tx, ctx, None, "team.member.remove", Some(&blob))?;
                }
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("remove team member: {e}"))
}

/// Grant a team a role on a repo.
///
/// The repo is checked against the org here, and so is the team: a grant
/// joining one org's team to another org's repo is the one mistake this
/// table makes possible, and the foreign keys alone would not catch it.
pub fn grant_repo(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    team_id: &str,
    role: Role,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<Result<(), String>, String> {
    if !valid_id(team_id) {
        return Ok(Err("no such team".into()));
    }
    if !role.valid_for_repo_grant() {
        return Ok(Err("owner is an org role, not a repo role".into()));
    }
    let at = now_ms();
    let blob = serde_json::json!({ "team_id": team_id, "role": role.as_str() });
    db.lock()
        .transaction(move |tx| {
            if tx
                .query_opt(
                    "SELECT id FROM teams WHERE org_id = $1 AND id = $2",
                    &[&org_id, &team_id],
                )?
                .is_none()
            {
                return Ok(Err("no such team".to_string()));
            }
            if tx
                .query_opt(
                    "SELECT id FROM repos WHERE org_id = $1 AND id = $2",
                    &[&org_id, &repo_id],
                )?
                .is_none()
            {
                return Ok(Err("no such repo".to_string()));
            }
            tx.execute(
                "INSERT INTO repo_team_grants (repo_id, team_id, role, created_at) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (repo_id, team_id) DO UPDATE SET role = EXCLUDED.role",
                &[&repo_id, &team_id, &role.as_str(), &at],
            )?;
            if let Some(ctx) = audit {
                crate::audit::record_tx(tx, ctx, Some(repo_id), "repo.team_grant", Some(&blob))?;
            }
            Ok(Ok(()))
        })
        .map_err(|e| format!("grant repo to team: {e}"))
}

pub fn revoke_repo_grant(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    team_id: &str,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    if !valid_id(team_id) {
        return Ok(false);
    }
    let blob = serde_json::json!({ "team_id": team_id });
    db.lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "DELETE FROM repo_team_grants g USING teams t \
                 WHERE g.team_id = t.id AND t.org_id = $1 \
                   AND g.repo_id = $2 AND g.team_id = $3",
                &[&org_id, &repo_id, &team_id],
            )?;
            if n > 0 {
                if let Some(ctx) = audit {
                    crate::audit::record_tx(
                        tx,
                        ctx,
                        Some(repo_id),
                        "repo.team_grant.revoke",
                        Some(&blob),
                    )?;
                }
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("revoke team grant: {e}"))
}

/// The highest role this person's teams grant them on this repo.
///
/// `None` means no team of theirs has a grant here — which is different
/// from a grant of `viewer`, and the caller must not confuse the two:
/// only `Some` participates in the max, so a person in three teams with
/// no grants keeps their org role untouched.
pub fn best_grant_for(
    db: &ControlDb,
    repo_id: &str,
    user_id: &str,
) -> Result<Option<Role>, String> {
    if !valid_id(user_id) {
        return Ok(None);
    }
    let rows = db
        .lock()
        .query(
            "SELECT g.role FROM repo_team_grants g \
             JOIN team_members tm ON tm.team_id = g.team_id \
             WHERE g.repo_id = $1 AND tm.user_id = $2",
            &[&repo_id, &user_id],
        )
        .map_err(|e| format!("team grants for repo: {e}"))?;
    Ok(rows
        .iter()
        .filter_map(|r| Role::parse(r.get::<_, String>("role").as_str()))
        .max())
}

/// The highest role any team grant anywhere in this org gives this
/// person. The ceiling on what a personal token may be minted with has to
/// account for team-granted access, or someone whose only write access
/// comes from a team cannot mint a token that writes.
pub fn best_grant_in_org(
    db: &ControlDb,
    org_id: &str,
    user_id: &str,
) -> Result<Option<Role>, String> {
    if !valid_id(user_id) {
        return Ok(None);
    }
    let rows = db
        .lock()
        .query(
            "SELECT g.role FROM repo_team_grants g \
             JOIN team_members tm ON tm.team_id = g.team_id \
             JOIN repos r ON r.id = g.repo_id \
             WHERE r.org_id = $1 AND tm.user_id = $2",
            &[&org_id, &user_id],
        )
        .map_err(|e| format!("team grants in org: {e}"))?;
    Ok(rows
        .iter()
        .filter_map(|r| Role::parse(r.get::<_, String>("role").as_str()))
        .max())
}

/// Where a person's or team's access to a repo came from.
///
/// This is the answer to "why can Alice write here?", which is the
/// question that makes teams usable at all. Without it, three rules
/// interact invisibly and the only way to find out is to try.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Their role in the org, with nothing narrowing it.
    OrgRole,
    /// A grant naming them personally. Wins outright, either direction.
    DirectGrant,
    /// A grant to a team they are in. Raises only.
    Team,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::OrgRole => "org_role",
            Source::DirectGrant => "direct_grant",
            Source::Team => "team",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRow {
    pub user_id: String,
    pub email: String,
    pub name: String,
    pub role: Role,
    pub source: Source,
    /// Set when `source` is `Team`: which team it came from.
    pub team_id: Option<String>,
    pub team_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamAccessRow {
    pub team_id: String,
    pub team_name: String,
    pub role: Role,
    pub member_count: i64,
}

/// Everyone who can reach this repo, and the team grants that put some of
/// them there.
///
/// Resolved in Rust rather than SQL on purpose: the precedence rule lives
/// in exactly one place ([`crate::members::effective_role`]), and a
/// second copy of it expressed as a CASE expression is a second copy that
/// can drift.
pub fn repo_access(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
) -> Result<(Vec<AccessRow>, Vec<TeamAccessRow>), String> {
    let people = crate::members::list(db, org_id)?;
    let team_rows = db
        .lock()
        .query(
            "SELECT g.team_id, g.role, t.name, COUNT(tm.user_id) AS member_count \
             FROM repo_team_grants g JOIN teams t ON t.id = g.team_id \
             LEFT JOIN team_members tm ON tm.team_id = t.id \
             WHERE g.repo_id = $1 AND t.org_id = $2 \
             GROUP BY g.team_id, g.role, t.name ORDER BY lower(t.name)",
            &[&repo_id, &org_id],
        )
        .map_err(|e| format!("repo team access: {e}"))?;
    let teams: Vec<TeamAccessRow> = team_rows
        .iter()
        .filter_map(|r| {
            Role::parse(r.get::<_, String>("role").as_str()).map(|role| TeamAccessRow {
                team_id: r.get("team_id"),
                team_name: r.get("name"),
                role,
                member_count: r.get("member_count"),
            })
        })
        .collect();

    let mut out = Vec::with_capacity(people.len());
    for p in people {
        let Some(role) = crate::members::effective_role(db, org_id, Some(repo_id), &p.user_id)?
        else {
            continue;
        };
        // Which rule produced that role. Asked in precedence order, so a
        // person who is both directly granted and in a granted team is
        // reported as directly granted — which is what actually decided
        // it.
        let (source, team) = if crate::members::repo_grant(db, repo_id, &p.user_id)?.is_some() {
            (Source::DirectGrant, None)
        } else {
            match best_team_row(db, repo_id, &p.user_id, &teams)? {
                Some(t) if t.role > p.role => (Source::Team, Some(t)),
                _ => (Source::OrgRole, None),
            }
        };
        out.push(AccessRow {
            user_id: p.user_id,
            email: p.email,
            name: p.name,
            role,
            source,
            team_id: team.as_ref().map(|t| t.team_id.clone()),
            team_name: team.map(|t| t.team_name),
        });
    }
    Ok((out, teams))
}

/// Which of this repo's granted teams gives this person the most, if any.
fn best_team_row(
    db: &ControlDb,
    repo_id: &str,
    user_id: &str,
    teams: &[TeamAccessRow],
) -> Result<Option<TeamAccessRow>, String> {
    if teams.is_empty() {
        return Ok(None);
    }
    let rows = db
        .lock()
        .query(
            "SELECT g.team_id FROM repo_team_grants g \
             JOIN team_members tm ON tm.team_id = g.team_id \
             WHERE g.repo_id = $1 AND tm.user_id = $2",
            &[&repo_id, &user_id],
        )
        .map_err(|e| format!("team grant source: {e}"))?;
    let mine: Vec<String> = rows.iter().map(|r| r.get("team_id")).collect();
    Ok(teams
        .iter()
        .filter(|t| mine.contains(&t.team_id))
        .max_by_key(|t| t.role)
        .cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::members;
    use crate::registry::{self, NewRepo, RepoKind};

    struct World {
        db: ControlDb,
        org: String,
        repo: String,
        /// An org *viewer* — the interesting subject, because raising and
        /// lowering are both visible from there.
        vic: String,
    }

    fn world(hint: &str) -> World {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap();
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
        let vic = crate::users::create(&db, "vic@acme.test", "Vic", Some("a long enough password"))
            .unwrap();
        members::add(&db, &org.id, &vic.id, Role::Viewer, None).unwrap();
        World {
            db,
            org: org.id,
            repo: repo.id,
            vic: vic.id,
        }
    }

    #[test]
    fn a_team_grant_raises_and_never_lowers() {
        let w = world("teams-raise");
        let t = create(&w.db, &w.org, "payments", Some("the squad"), None).unwrap();
        assert!(add_member(&w.db, &w.org, &t.id, &w.vic, None)
            .unwrap()
            .unwrap());

        // Before any grant, the org role is the whole answer.
        assert_eq!(
            members::effective_role(&w.db, &w.org, Some(&w.repo), &w.vic).unwrap(),
            Some(Role::Viewer)
        );

        // A team grant above the org role raises.
        grant_repo(&w.db, &w.org, &w.repo, &t.id, Role::Member, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            members::effective_role(&w.db, &w.org, Some(&w.repo), &w.vic).unwrap(),
            Some(Role::Member)
        );

        // A second team granting *less* must not take that away: nobody
        // reads a team's grants before adding a colleague to it.
        let low = create(&w.db, &w.org, "readers", None, None).unwrap();
        add_member(&w.db, &w.org, &low.id, &w.vic, None)
            .unwrap()
            .unwrap();
        grant_repo(&w.db, &w.org, &w.repo, &low.id, Role::Viewer, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            members::effective_role(&w.db, &w.org, Some(&w.repo), &w.vic).unwrap(),
            Some(Role::Member),
            "two teams disagreeing must take the higher"
        );

        // A team grant cannot lower someone below their org role either.
        let admin = crate::users::create(&w.db, "ada@acme.test", "Ada", Some("a long password 12"))
            .unwrap();
        members::add(&w.db, &w.org, &admin.id, Role::Admin, None).unwrap();
        add_member(&w.db, &w.org, &low.id, &admin.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            members::effective_role(&w.db, &w.org, Some(&w.repo), &admin.id).unwrap(),
            Some(Role::Admin),
            "a viewer-level team grant must not demote an admin"
        );
    }

    #[test]
    fn a_grant_naming_the_person_beats_the_team_in_both_directions() {
        let w = world("teams-direct");
        let t = create(&w.db, &w.org, "payments", None, None).unwrap();
        add_member(&w.db, &w.org, &t.id, &w.vic, None)
            .unwrap()
            .unwrap();
        grant_repo(&w.db, &w.org, &w.repo, &t.id, Role::Admin, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            members::effective_role(&w.db, &w.org, Some(&w.repo), &w.vic).unwrap(),
            Some(Role::Admin)
        );

        // Naming the person holds them down, even though their team says
        // admin. This is the whole reason the two tables are separate.
        members::grant_repo(&w.db, &w.repo, &w.vic, Role::Viewer, None).unwrap();
        assert_eq!(
            members::effective_role(&w.db, &w.org, Some(&w.repo), &w.vic).unwrap(),
            Some(Role::Viewer)
        );

        // And raises, on a repo where the team grants nothing at all.
        let other = registry::create_repo(
            &w.db,
            &w.org,
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
        members::grant_repo(&w.db, &other.id, &w.vic, Role::Member, None).unwrap();
        assert_eq!(
            members::effective_role(&w.db, &w.org, Some(&other.id), &w.vic).unwrap(),
            Some(Role::Member)
        );
    }

    #[test]
    fn deleting_a_team_withdraws_its_access_at_once() {
        let w = world("teams-delete");
        let t = create(&w.db, &w.org, "payments", None, None).unwrap();
        add_member(&w.db, &w.org, &t.id, &w.vic, None)
            .unwrap()
            .unwrap();
        grant_repo(&w.db, &w.org, &w.repo, &t.id, Role::Member, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            members::effective_role(&w.db, &w.org, Some(&w.repo), &w.vic).unwrap(),
            Some(Role::Member)
        );

        assert!(delete(&w.db, &w.org, &t.id, None).unwrap());
        assert_eq!(
            members::effective_role(&w.db, &w.org, Some(&w.repo), &w.vic).unwrap(),
            Some(Role::Viewer),
            "access must go with the team, not linger"
        );
        assert!(list(&w.db, &w.org).unwrap().is_empty());
        // Deleting it twice is not an error, it is the same state.
        assert!(!delete(&w.db, &w.org, &t.id, None).unwrap());
    }

    #[test]
    fn a_team_is_bounded_by_its_org() {
        let w = world("teams-orgs");
        let other = registry::create_org(&w.db, "other").unwrap();
        let t = create(&w.db, &w.org, "payments", None, None).unwrap();

        // The other org cannot see it, rename it, delete it, or grant it.
        assert!(by_id(&w.db, &other.id, &t.id).unwrap().is_none());
        assert!(list(&w.db, &other.id).unwrap().is_empty());
        assert!(!update(&w.db, &other.id, &t.id, Some("theirs"), None, None).unwrap());
        assert!(!delete(&w.db, &other.id, &t.id, None).unwrap());
        assert_eq!(
            grant_repo(&w.db, &other.id, &w.repo, &t.id, Role::Member, None).unwrap(),
            Err("no such team".to_string())
        );

        // A stranger cannot be put in it, and neither can a made-up id.
        let stranger =
            crate::users::create(&w.db, "s@nowhere.test", "S", Some("a long enough password"))
                .unwrap();
        assert_eq!(
            add_member(&w.db, &w.org, &t.id, &stranger.id, None).unwrap(),
            Err("not a member of this org".to_string())
        );
        assert!(add_member(&w.db, &w.org, &t.id, "not-an-id", None)
            .unwrap()
            .is_err());
        assert!(add_member(&w.db, &w.org, "not-an-id", &w.vic, None)
            .unwrap()
            .is_err());
        // Well-formed, and still nobody's team.
        assert_eq!(
            add_member(&w.db, &w.org, "01zzzzzzzzzzzzzzzzzzzzzzzz", &w.vic, None).unwrap(),
            Err("no such team".to_string())
        );

        // Hostile bytes in an id name nothing rather than erroring.
        for bad in ["\0", "'; DROP TABLE teams;--", "../../etc/passwd"] {
            assert!(by_id(&w.db, &w.org, bad).unwrap().is_none());
            assert!(members(&w.db, bad).unwrap().is_empty());
            assert!(best_grant_for(&w.db, &w.repo, bad).unwrap().is_none());
            assert!(best_grant_in_org(&w.db, &w.org, bad).unwrap().is_none());
            assert!(!remove_member(&w.db, &w.org, bad, &w.vic, None).unwrap());
            assert!(!revoke_repo_grant(&w.db, &w.org, &w.repo, bad, None).unwrap());
        }
    }

    #[test]
    fn names_are_shaped_case_folded_and_unique_per_org() {
        let w = world("teams-names");
        create(&w.db, &w.org, "payments", None, None).unwrap();
        assert!(create(&w.db, &w.org, "payments", None, None).is_err());
        assert!(
            create(&w.db, &w.org, "Payments", None, None).is_err(),
            "case must not make a second team of the same name"
        );

        for bad in ["", "has space", "quote\"", ".", ".hidden", "semi;colon"] {
            assert!(create(&w.db, &w.org, bad, None, None).is_err(), "{bad:?}");
        }
        assert!(create(&w.db, &w.org, "ops", Some(&"x".repeat(501)), None).is_err());

        // Another org may use the same name — uniqueness is per org.
        let other = registry::create_org(&w.db, "other").unwrap();
        assert!(create(&w.db, &other.id, "payments", None, None).is_ok());

        // Renaming into a taken name is refused; renaming to a free one
        // works and leaves the description alone.
        let t = create(&w.db, &w.org, "infra", Some("keeps the lights on"), None).unwrap();
        assert!(update(&w.db, &w.org, &t.id, Some("payments"), None, None).is_err());
        assert!(update(&w.db, &w.org, &t.id, Some("platform"), None, None).unwrap());
        let after = by_id(&w.db, &w.org, &t.id).unwrap().unwrap();
        assert_eq!(after.name, "platform");
        assert_eq!(after.description.as_deref(), Some("keeps the lights on"));
        assert!(update(&w.db, &w.org, &t.id, Some("has space"), None, None).is_err());
        assert!(update(&w.db, &w.org, &t.id, None, Some(&"x".repeat(501)), None).is_err());
        // An id that could never exist names nothing rather than erroring.
        assert!(!update(&w.db, &w.org, "not-an-id", Some("x"), None, None).unwrap());
    }

    #[test]
    fn a_team_grant_lifts_the_ceiling_a_personal_token_is_minted_against() {
        let w = world("teams-ceiling");
        // A viewer may only mint read.
        assert_eq!(
            members::max_role(&w.db, &w.org, &w.vic).unwrap(),
            Some(Role::Viewer)
        );
        let t = create(&w.db, &w.org, "payments", None, None).unwrap();
        add_member(&w.db, &w.org, &t.id, &w.vic, None)
            .unwrap()
            .unwrap();
        grant_repo(&w.db, &w.org, &w.repo, &t.id, Role::Member, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            members::max_role(&w.db, &w.org, &w.vic).unwrap(),
            Some(Role::Member),
            "someone whose only write access is a team's must still be \
             able to mint a token that writes"
        );

        // Leaving the team takes it back.
        assert!(remove_member(&w.db, &w.org, &t.id, &w.vic, None).unwrap());
        assert_eq!(
            members::max_role(&w.db, &w.org, &w.vic).unwrap(),
            Some(Role::Viewer)
        );
    }

    #[test]
    fn the_access_map_says_where_each_persons_access_came_from() {
        let w = world("teams-access");
        let ada = crate::users::create(&w.db, "ada@acme.test", "Ada", Some("a long password 12"))
            .unwrap();
        members::add(&w.db, &w.org, &ada.id, Role::Admin, None).unwrap();
        let t = create(&w.db, &w.org, "payments", None, None).unwrap();
        add_member(&w.db, &w.org, &t.id, &w.vic, None)
            .unwrap()
            .unwrap();
        grant_repo(&w.db, &w.org, &w.repo, &t.id, Role::Member, None)
            .unwrap()
            .unwrap();

        let (people, granted) = repo_access(&w.db, &w.org, &w.repo).unwrap();
        let row = |id: &str| people.iter().find(|p| p.user_id == id).unwrap().clone();
        let v = row(&w.vic);
        assert_eq!((v.role, v.source), (Role::Member, Source::Team));
        assert_eq!(v.team_name.as_deref(), Some("payments"));
        let a = row(&ada.id);
        assert_eq!((a.role, a.source), (Role::Admin, Source::OrgRole));
        assert_eq!(a.team_id, None);
        assert_eq!(granted.len(), 1);
        assert_eq!(
            (granted[0].role, granted[0].member_count),
            (Role::Member, 1)
        );

        // A direct grant is reported as what actually decided it, even
        // though the team grant is still there.
        members::grant_repo(&w.db, &w.repo, &w.vic, Role::Viewer, None).unwrap();
        let (people, _) = repo_access(&w.db, &w.org, &w.repo).unwrap();
        let v = people.iter().find(|p| p.user_id == w.vic).unwrap();
        assert_eq!((v.role, v.source), (Role::Viewer, Source::DirectGrant));

        // A repo nobody's team was granted on: every row is an org role,
        // and the team list comes back empty rather than absent.
        let untouched = registry::create_repo(
            &w.db,
            &w.org,
            &NewRepo {
                description: None,
                name: "untouched",
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        let (people, granted) = repo_access(&w.db, &w.org, &untouched.id).unwrap();
        assert!(granted.is_empty());
        assert!(people.iter().all(|p| p.source == Source::OrgRole));
        assert!(people.iter().any(|p| p.user_id == w.vic));

        for s in [Source::OrgRole, Source::DirectGrant, Source::Team] {
            assert!(!s.as_str().is_empty());
        }
    }

    #[test]
    fn a_team_grant_is_replaced_not_stacked_and_can_be_revoked() {
        let w = world("teams-regrant");
        let t = create(&w.db, &w.org, "payments", None, None).unwrap();
        add_member(&w.db, &w.org, &t.id, &w.vic, None)
            .unwrap()
            .unwrap();
        grant_repo(&w.db, &w.org, &w.repo, &t.id, Role::Admin, None)
            .unwrap()
            .unwrap();
        grant_repo(&w.db, &w.org, &w.repo, &t.id, Role::Member, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            best_grant_for(&w.db, &w.repo, &w.vic).unwrap(),
            Some(Role::Member),
            "re-granting must replace the row, not leave the old one to win"
        );

        // Owner is not a per-repo role.
        assert_eq!(
            grant_repo(&w.db, &w.org, &w.repo, &t.id, Role::Owner, None).unwrap(),
            Err("owner is an org role, not a repo role".to_string())
        );
        // Nor is a repo from another org reachable.
        assert_eq!(
            grant_repo(
                &w.db,
                &w.org,
                "01vvvvvvvvvvvvvvvvvvvvvvvv",
                &t.id,
                Role::Member,
                None
            )
            .unwrap(),
            Err("no such repo".to_string())
        );

        assert!(revoke_repo_grant(&w.db, &w.org, &w.repo, &t.id, None).unwrap());
        assert!(best_grant_for(&w.db, &w.repo, &w.vic).unwrap().is_none());
        assert!(!revoke_repo_grant(&w.db, &w.org, &w.repo, &t.id, None).unwrap());

        // Adding the same person twice is idempotent, and removing
        // someone who was never there is not an error.
        assert!(!add_member(&w.db, &w.org, &t.id, &w.vic, None)
            .unwrap()
            .unwrap());
        assert_eq!(members(&w.db, &t.id).unwrap().len(), 1);
        assert!(remove_member(&w.db, &w.org, &t.id, &w.vic, None).unwrap());
        assert!(!remove_member(&w.db, &w.org, &t.id, &w.vic, None).unwrap());
    }
}
