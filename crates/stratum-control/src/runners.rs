//! Somebody else's machines: registration, grouping, policy, routing.
//!
//! A self-hosted runner is a laptop or a build box that an operator
//! installed a binary on — the only kind of runner this edition has —
//! and every design decision here comes from it being somebody else's
//! machine:
//!
//! * **The runner only ever calls out.** Nothing listens on the
//!   operator's network. Registration exchanges a short-lived token for
//!   a long-lived credential, and the runner then long-polls for work.
//! * **The credential is hashed at rest and rotatable.** It is shown
//!   once. Re-registering under the same name tombstones the old row in
//!   the same transaction, which is both how a credential is rotated and
//!   how a re-imaged machine comes back without an operator having to
//!   remove it first.
//! * **A repository has to be *admitted* to a machine, never the other
//!   way round.** A workflow file arrives from any repository anybody
//!   forked, so the question "may this repository's build run here" is
//!   answered by the runner's owner — through the group's repository
//!   access and through the organisation's policy — and `allow_public`
//!   defaults to false because a public repository's fork can carry its
//!   own `run:` lines.
//! * **State is derived, never stored.** `online`/`busy`/`offline` come
//!   from `last_seen_at` and from whether a `running` job points at the
//!   runner. There is no status column for a crashed process to leave
//!   lying, which is the same argument [`crate::workflows`] makes about
//!   readiness.
//!
//! Removal is a tombstone (`removed_at`), never a delete: a job row
//! names the runner that ran it, and a reader looking at last week's
//! build should still be told which machine that was.

use crate::audit::{self, AuditCtx};
use crate::db::ControlDb;
use crate::ids::{now_ms, token_secret, ulid, valid_id};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// The operator's short-lived secret, carried to the machine.
pub const REGISTRATION_PREFIX: &str = "weftg_";
/// The machine's own long-lived credential.
pub const CREDENTIAL_PREFIX: &str = "weftr_";

/// How long a registration token lives. An hour, GitHub's number and for
/// GitHub's reason: it is pasted into a shell and left in a scrollback,
/// so it has to stop being a credential well before the terminal is
/// closed.
pub const REGISTRATION_TTL_MS: i64 = 60 * 60 * 1000;

/// How recently a runner must have called for the list to call it
/// `online`. A minute: the claim long-poll is twenty seconds, so a
/// healthy idle runner touches its row three times inside the window and
/// one lost poll does not make it look dead.
pub const ONLINE_WINDOW_MS: i64 = 60 * 1000;

/// How long an unseen runner is kept before the sweep removes it.
/// Fourteen days for an ordinary machine — long enough to survive a
/// holiday — and one day for an ephemeral one, which by construction
/// should have exited after a single job.
pub const STALE_MS: i64 = 14 * 24 * 60 * 60 * 1000;
pub const STALE_EPHEMERAL_MS: i64 = 24 * 60 * 60 * 1000;

/// The name every organisation's first group has.
pub const DEFAULT_GROUP: &str = "default";

/// The error the sentence a removed runner's job is failed with. Named
/// because the API and its test must agree on it exactly.
pub const REMOVED_MID_JOB: &str = "runner removed while the job was running";

/// What went wrong, at the granularity the HTTP layer answers with.
///
/// Three variants rather than a `String` because the three have
/// different status codes and the mapping must not be a prose match in
/// a handler: a duplicate group name is a 409, a value outside the
/// enumeration is a 422, and a database failure is a 500.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The request cannot be honoured as written.
    Invalid(String),
    /// Something with that name is already there.
    Conflict(String),
    /// The database refused.
    Db(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(m) | Error::Conflict(m) | Error::Db(m) => f.write_str(m),
        }
    }
}

fn db_err(what: &str) -> impl Fn(postgres::Error) -> Error + '_ {
    move |e| Error::Db(format!("{what}: {e}"))
}

/// Only the hash is stored, for the registration token and for the
/// runner credential alike. The same construction `usertokens` uses.
fn hash(secret: &str) -> String {
    stratum_store::pack::hex(&Sha256::digest(secret.as_bytes()))
}

/// Split `<prefix><id>_<secret>` into its two halves.
fn split_secret<'a>(presented: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let rest = presented.strip_prefix(prefix)?;
    let (id, secret) = rest.split_once('_')?;
    valid_id(id).then_some((id, secret))
}

// ---------------------------------------------------------------------
// Names, labels, and the small vocabularies
// ---------------------------------------------------------------------

/// The operating systems a runner may claim to be.
pub const OSES: [&str; 3] = ["linux", "macos", "windows"];
/// The architectures a runner may claim to be.
pub const ARCHES: [&str; 2] = ["x64", "arm64"];

/// Is this the shape of a label?
///
/// Lowercase, because routing compares label sets and two spellings of
/// one label is a job that never runs and an operator who cannot see
/// why. The character class is deliberately narrower than a name's: a
/// label ends up in an array literal, a hint sentence and a UI chip.
pub fn valid_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// Is this the shape of a runner or group name? A machine's name comes
/// from its hostname by default, so this is a hostname's character set
/// plus the separators people use.
pub fn valid_runner_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !s.starts_with('.')
}

/// The labels a runner is actually registered with: `self-hosted`, its
/// OS and its architecture, then whatever custom labels it offered.
///
/// Order is kept because it is what the list shows, and the three
/// automatic ones lead so that every runner reads the same way. Deduped
/// so a runner that offers `linux` on a Linux box does not carry it
/// twice.
pub fn runner_labels(os: &str, arch: &str, custom: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(custom.len() + 3);
    for l in ["self-hosted", os, arch] {
        if !out.iter().any(|e| e == l) {
            out.push(l.to_string());
        }
    }
    for c in custom {
        let c = c.trim().to_ascii_lowercase();
        if !c.is_empty() && !out.iter().any(|e| e == &c) {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------
// The rows
// ---------------------------------------------------------------------

/// An organisation's runner switch, and the repositories it names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Policy {
    /// `all` | `selected` | `disabled`.
    pub self_hosted: String,
    /// Repository **names**, sorted. Only meaningful under `selected`,
    /// but returned always so a dashboard that switches back to
    /// `selected` still has the list somebody chose.
    pub self_hosted_repos: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Group {
    pub id: String,
    pub org_id: String,
    pub name: String,
    /// `all` | `selected`.
    pub repo_access: String,
    pub allow_public: bool,
    pub is_default: bool,
    /// Repository names, sorted. Empty under `all`.
    pub repos: Vec<String>,
    /// How many live runners are in it.
    pub runners: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Runner {
    pub id: String,
    pub org_id: String,
    pub group_id: String,
    pub group_name: String,
    pub name: String,
    pub labels: Vec<String>,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub ephemeral: bool,
    pub last_seen_at: i64,
    pub created_at: i64,
    pub removed_at: Option<i64>,
}

impl Runner {
    /// `online` if it has called recently, `offline` otherwise. `busy`
    /// is decided by the job that points at it and so is not knowable
    /// from the row alone — see [`list`].
    pub fn seen_state(&self, now: i64) -> &'static str {
        if now - self.last_seen_at <= ONLINE_WINDOW_MS {
            "online"
        } else {
            "offline"
        }
    }
}

/// What a runner is doing, for the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunningJob {
    pub run_id: String,
    /// The `workflow_jobs` row id — what a link into the run needs.
    pub job_id: String,
    /// The cell's human name, e.g. `test (linux)`.
    pub key: String,
    /// The repository's name — not its id. A run is addressed by
    /// `owner/repo`, so a page that has only the ids cannot build the
    /// link to the build this machine is busy with.
    pub repo: String,
}

/// A runner with the two things that are not in its row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunnerView {
    #[serde(flatten)]
    pub runner: Runner,
    /// `online` | `busy` | `offline`.
    pub state: String,
    pub job: Option<RunningJob>,
}

const GROUP_COLS: &str = "id, org_id, name, repo_access, allow_public, is_default, \
                          created_at, updated_at";

const RUNNER_COLS: &str = "r.id, r.org_id, r.group_id, r.name, r.labels, r.os, r.arch, \
                           r.version, r.ephemeral, r.last_seen_at, r.created_at, r.removed_at, \
                           g.name AS group_name";

fn row_to_runner(r: &postgres::Row) -> Runner {
    Runner {
        id: r.get("id"),
        org_id: r.get("org_id"),
        group_id: r.get("group_id"),
        group_name: r.get("group_name"),
        name: r.get("name"),
        labels: r.get("labels"),
        os: r.get("os"),
        arch: r.get("arch"),
        version: r.get("version"),
        ephemeral: r.get("ephemeral"),
        last_seen_at: r.get("last_seen_at"),
        created_at: r.get("created_at"),
        removed_at: r.get("removed_at"),
    }
}

// ---------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------

/// The organisation's `default` group, created if it is not there yet.
///
/// Lazy rather than written when the organisation is: an organisation
/// that never registers a runner should carry no runner rows at all, and
/// backfilling every existing org in the migration would have written
/// one for each of them to be sure of a row almost none of them need.
///
/// `ON CONFLICT DO NOTHING` and then a read, rather than a read and then
/// an insert: two registrations arriving at once on two nodes both find
/// nothing, and the partial unique index is what decides between them.
pub fn ensure_default_group(db: &ControlDb, org_id: &str) -> Result<String, Error> {
    let now = now_ms();
    let id = ulid();
    let mut conn = db.lock();
    conn.execute(
        "INSERT INTO runner_groups \
           (id, org_id, name, repo_access, allow_public, is_default, created_at, updated_at) \
         VALUES ($1, $2, $3, 'all', FALSE, TRUE, $4, $4) ON CONFLICT DO NOTHING",
        &[&id, &org_id, &DEFAULT_GROUP, &now],
    )
    .map_err(db_err("create default runner group"))?;
    let row = conn
        .query_opt(
            "SELECT id FROM runner_groups WHERE org_id = $1 AND is_default",
            &[&org_id],
        )
        .map_err(db_err("read default runner group"))?
        .ok_or_else(|| Error::Db("the default runner group is missing".into()))?;
    Ok(row.get("id"))
}

/// Repository names for a set of group ids, and the live runner counts.
fn decorate_groups(
    conn: &mut crate::db::Conn<'_>,
    mut groups: Vec<Group>,
) -> Result<Vec<Group>, Error> {
    let ids: Vec<String> = groups.iter().map(|g| g.id.clone()).collect();
    if ids.is_empty() {
        return Ok(groups);
    }
    let repos = conn
        .query(
            "SELECT gr.group_id, rp.name FROM runner_group_repos gr \
             JOIN repos rp ON rp.id = gr.repo_id \
             WHERE gr.group_id = ANY($1) ORDER BY rp.name",
            &[&ids],
        )
        .map_err(db_err("read runner group repositories"))?;
    for r in &repos {
        let gid: String = r.get("group_id");
        if let Some(g) = groups.iter_mut().find(|g| g.id == gid) {
            g.repos.push(r.get("name"));
        }
    }
    let counts = conn
        .query(
            "SELECT group_id, COUNT(*)::BIGINT AS n FROM runners \
             WHERE group_id = ANY($1) AND removed_at IS NULL GROUP BY group_id",
            &[&ids],
        )
        .map_err(db_err("count runners per group"))?;
    for r in &counts {
        let gid: String = r.get("group_id");
        if let Some(g) = groups.iter_mut().find(|g| g.id == gid) {
            g.runners = r.get("n");
        }
    }
    Ok(groups)
}

fn row_to_group(r: &postgres::Row) -> Group {
    Group {
        id: r.get("id"),
        org_id: r.get("org_id"),
        name: r.get("name"),
        repo_access: r.get("repo_access"),
        allow_public: r.get("allow_public"),
        is_default: r.get("is_default"),
        repos: Vec::new(),
        runners: 0,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

/// Every group in the organisation, default first then by name.
pub fn list_groups(db: &ControlDb, org_id: &str) -> Result<Vec<Group>, Error> {
    ensure_default_group(db, org_id)?;
    let mut conn = db.lock();
    let rows = conn
        .query(
            &format!(
                "SELECT {GROUP_COLS} FROM runner_groups WHERE org_id = $1 \
                 ORDER BY is_default DESC, name"
            ),
            &[&org_id],
        )
        .map_err(db_err("list runner groups"))?;
    let groups = rows.iter().map(row_to_group).collect();
    decorate_groups(&mut conn, groups)
}

fn check_repo_access(v: &str) -> Result<(), Error> {
    match v {
        "all" | "selected" => Ok(()),
        other => Err(Error::Invalid(format!(
            "repo_access must be \"all\" or \"selected\", not {other:?}"
        ))),
    }
}

/// Resolve repository names to ids inside one organisation.
///
/// A name that is not there is a refusal rather than a silent drop: a
/// group that quietly admits four of the five repositories somebody
/// listed is a build that does not run and a settings page that says it
/// should.
fn repo_ids(
    conn: &mut crate::db::Conn<'_>,
    org_id: &str,
    names: &[String],
) -> Result<Vec<String>, Error> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let row = conn
            .query_opt(
                "SELECT id FROM repos WHERE org_id = $1 AND name = $2 AND state = 'active'",
                &[&org_id, &name],
            )
            .map_err(db_err("resolve repository"))?
            .ok_or_else(|| Error::Invalid(format!("no repository named {name:?}")))?;
        let id: String = row.get("id");
        if !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
}

/// One group, scoped to its organisation.
pub fn group(db: &ControlDb, org_id: &str, id: &str) -> Result<Option<Group>, Error> {
    if !valid_id(id) {
        return Ok(None);
    }
    let mut conn = db.lock();
    let row = conn
        .query_opt(
            &format!("SELECT {GROUP_COLS} FROM runner_groups WHERE org_id = $1 AND id = $2"),
            &[&org_id, &id],
        )
        .map_err(db_err("read runner group"))?;
    let Some(row) = row else { return Ok(None) };
    let decorated = decorate_groups(&mut conn, vec![row_to_group(&row)])?;
    Ok(decorated.into_iter().next())
}

/// Create a group.
pub fn create_group(
    db: &ControlDb,
    org_id: &str,
    name: &str,
    repo_access: Option<&str>,
    allow_public: Option<bool>,
    repos: Option<&[String]>,
    actx: &AuditCtx,
) -> Result<Group, Error> {
    if !valid_runner_name(name) {
        return Err(Error::Invalid(format!(
            "{name:?} is not a group name (letters, digits, dot, dash, underscore; \
             at most 64 characters)"
        )));
    }
    let access = repo_access.unwrap_or("all");
    check_repo_access(access)?;
    let public = allow_public.unwrap_or(false);
    let id = ulid();
    let now = now_ms();
    let names = repos.unwrap_or(&[]).to_vec();
    let mut conn = db.lock();
    let repo_ids = repo_ids(&mut conn, org_id, &names)?;

    let org = org_id.to_string();
    let group_name = name.to_string();
    let gid = id.clone();
    let ctx = actx.clone();
    let existing = conn
        .query_opt(
            "SELECT id FROM runner_groups WHERE org_id = $1 AND name = $2",
            &[&org_id, &name],
        )
        .map_err(db_err("read runner group"))?;
    if existing.is_some() {
        return Err(Error::Conflict(format!(
            "a runner group named {name:?} already exists"
        )));
    }
    conn.transaction(move |tx| {
        tx.execute(
            "INSERT INTO runner_groups \
               (id, org_id, name, repo_access, allow_public, is_default, created_at, updated_at) \
             VALUES ($1,$2,$3,$4,$5,FALSE,$6,$6)",
            &[&gid, &org, &group_name, &access, &public, &now],
        )?;
        for rid in &repo_ids {
            tx.execute(
                "INSERT INTO runner_group_repos (group_id, repo_id) VALUES ($1,$2) \
                 ON CONFLICT DO NOTHING",
                &[&gid, rid],
            )?;
        }
        audit::record_tx(
            tx,
            &ctx,
            None,
            "runner_group.created",
            Some(&serde_json::json!({
                "group": group_name, "id": gid,
                "repo_access": access, "allow_public": public,
            })),
        )?;
        Ok(())
    })
    .map_err(db_err("create runner group"))?;
    drop(conn);
    group(db, org_id, &id)?.ok_or_else(|| Error::Db("the group just created is missing".into()))
}

/// Change a group. Every argument is optional; `None` leaves it alone.
///
/// The default group's **name** is fixed. It is the name printed in
/// every registration command we have ever handed out, and renaming it
/// would silently point those at nothing.
#[allow(clippy::too_many_arguments)]
pub fn update_group(
    db: &ControlDb,
    org_id: &str,
    id: &str,
    name: Option<&str>,
    repo_access: Option<&str>,
    allow_public: Option<bool>,
    repos: Option<&[String]>,
    actx: &AuditCtx,
) -> Result<Option<Group>, Error> {
    let Some(current) = group(db, org_id, id)? else {
        return Ok(None);
    };
    if let Some(n) = name {
        if current.is_default && n != current.name {
            return Err(Error::Invalid(
                "the default runner group cannot be renamed".into(),
            ));
        }
        if !valid_runner_name(n) {
            return Err(Error::Invalid(format!("{n:?} is not a group name")));
        }
    }
    if let Some(a) = repo_access {
        check_repo_access(a)?;
    }
    let now = now_ms();
    let new_name = name.unwrap_or(&current.name).to_string();
    let new_access = repo_access.unwrap_or(&current.repo_access).to_string();
    let new_public = allow_public.unwrap_or(current.allow_public);
    let mut conn = db.lock();
    let repo_ids = match repos {
        Some(names) => Some(repo_ids(&mut conn, org_id, names)?),
        None => None,
    };
    if name.is_some_and(|n| n != current.name) {
        let clash = conn
            .query_opt(
                "SELECT id FROM runner_groups WHERE org_id = $1 AND name = $2",
                &[&org_id, &new_name],
            )
            .map_err(db_err("read runner group"))?;
        if clash.is_some() {
            return Err(Error::Conflict(format!(
                "a runner group named {new_name:?} already exists"
            )));
        }
    }
    let gid = current.id.clone();
    let ctx = actx.clone();
    let audited = serde_json::json!({
        "group": new_name, "id": gid,
        "repo_access": new_access, "allow_public": new_public,
    });
    conn.transaction(move |tx| {
        tx.execute(
            "UPDATE runner_groups SET name = $2, repo_access = $3, allow_public = $4, \
               updated_at = $5 WHERE id = $1",
            &[&gid, &new_name, &new_access, &new_public, &now],
        )?;
        if let Some(ids) = &repo_ids {
            tx.execute(
                "DELETE FROM runner_group_repos WHERE group_id = $1",
                &[&gid],
            )?;
            for rid in ids {
                tx.execute(
                    "INSERT INTO runner_group_repos (group_id, repo_id) VALUES ($1,$2) \
                     ON CONFLICT DO NOTHING",
                    &[&gid, rid],
                )?;
            }
        }
        audit::record_tx(tx, &ctx, None, "runner_group.updated", Some(&audited))?;
        Ok(())
    })
    .map_err(db_err("update runner group"))?;
    drop(conn);
    group(db, org_id, id)
}

/// Delete a group, moving its runners to the default one.
///
/// Moved rather than removed: an operator tidying their groups has not
/// asked to take thirty machines offline, and a runner whose group
/// vanished would authenticate and then never be routed anything, which
/// is the least debuggable failure this feature has.
pub fn delete_group(
    db: &ControlDb,
    org_id: &str,
    id: &str,
    actx: &AuditCtx,
) -> Result<Option<()>, Error> {
    let Some(current) = group(db, org_id, id)? else {
        return Ok(None);
    };
    if current.is_default {
        return Err(Error::Invalid(
            "the default runner group cannot be deleted".into(),
        ));
    }
    let default_id = ensure_default_group(db, org_id)?;
    let gid = current.id.clone();
    let gname = current.name.clone();
    let ctx = actx.clone();
    let now = now_ms();
    db.lock()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runners SET group_id = $2 WHERE group_id = $1",
                &[&gid, &default_id],
            )?;
            // A registration token names a group. One outstanding for a
            // group that is going away has to go with it, rather than
            // registering a machine into a row that no longer exists.
            tx.execute(
                "DELETE FROM runner_registration_tokens WHERE group_id = $1",
                &[&gid],
            )?;
            tx.execute("DELETE FROM runner_groups WHERE id = $1", &[&gid])?;
            audit::record_tx(
                tx,
                &ctx,
                None,
                "runner_group.deleted",
                Some(&serde_json::json!({ "group": gname, "id": gid, "at": now })),
            )?;
            Ok(())
        })
        .map_err(db_err("delete runner group"))?;
    Ok(Some(()))
}

// ---------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------

/// What this organisation allows.
pub fn policy(db: &ControlDb, org_id: &str) -> Result<Policy, Error> {
    let mut conn = db.lock();
    let row = conn
        .query_opt(
            "SELECT runner_self_hosted FROM orgs WHERE id = $1",
            &[&org_id],
        )
        .map_err(db_err("read runner policy"))?
        .ok_or_else(|| Error::Invalid(format!("no such organisation: {org_id}")))?;
    let repos = conn
        .query(
            "SELECT rp.name FROM org_self_hosted_repos s JOIN repos rp ON rp.id = s.repo_id \
             WHERE s.org_id = $1 ORDER BY rp.name",
            &[&org_id],
        )
        .map_err(db_err("read self-hosted repositories"))?;
    Ok(Policy {
        self_hosted: row.get("runner_self_hosted"),
        self_hosted_repos: repos.iter().map(|r| r.get("name")).collect(),
    })
}

/// Change the policy. Any subset; `None` leaves a field alone.
pub fn set_policy(
    db: &ControlDb,
    org_id: &str,
    self_hosted: Option<&str>,
    repos: Option<&[String]>,
    actx: &AuditCtx,
) -> Result<Policy, Error> {
    if let Some(s) = self_hosted {
        if !matches!(s, "all" | "selected" | "disabled") {
            return Err(Error::Invalid(format!(
                "self_hosted must be \"all\", \"selected\" or \"disabled\", not {s:?}"
            )));
        }
    }
    let current = policy(db, org_id)?;
    let new_self = self_hosted.unwrap_or(&current.self_hosted).to_string();
    let mut conn = db.lock();
    let repo_ids = match repos {
        Some(names) => Some(repo_ids(&mut conn, org_id, names)?),
        None => None,
    };
    let org = org_id.to_string();
    let ctx = actx.clone();
    let audited = serde_json::json!({
        "self_hosted": new_self,
        "self_hosted_repos": repos.map(|r| r.to_vec()),
    });
    conn.transaction(move |tx| {
        tx.execute(
            "UPDATE orgs SET runner_self_hosted = $2 WHERE id = $1",
            &[&org, &new_self],
        )?;
        if let Some(ids) = &repo_ids {
            tx.execute(
                "DELETE FROM org_self_hosted_repos WHERE org_id = $1",
                &[&org],
            )?;
            for rid in ids {
                tx.execute(
                    "INSERT INTO org_self_hosted_repos (org_id, repo_id) VALUES ($1,$2) \
                     ON CONFLICT DO NOTHING",
                    &[&org, rid],
                )?;
            }
        }
        audit::record_tx(tx, &ctx, None, "runner_policy.updated", Some(&audited))?;
        Ok(())
    })
    .map_err(db_err("set runner policy"))?;
    drop(conn);
    policy(db, org_id)
}

/// Whether the organisation's policy admits self-hosted runners for this
/// repository.
pub fn self_hosted_allowed(db: &ControlDb, org_id: &str, repo_id: &str) -> Result<bool, Error> {
    let mut conn = db.lock();
    let row = conn
        .query_opt(
            "SELECT runner_self_hosted FROM orgs WHERE id = $1",
            &[&org_id],
        )
        .map_err(db_err("read self-hosted runner policy"))?;
    let Some(row) = row else { return Ok(false) };
    match row.get::<_, String>("runner_self_hosted").as_str() {
        "all" => Ok(true),
        "disabled" => Ok(false),
        _ => {
            let hit = conn
                .query_opt(
                    "SELECT 1 FROM org_self_hosted_repos WHERE org_id = $1 AND repo_id = $2",
                    &[&org_id, &repo_id],
                )
                .map_err(db_err("read selected self-hosted repositories"))?;
            Ok(hit.is_some())
        }
    }
}

/// Whether **any** group in the organisation admits this repository.
///
/// The trigger's third refusal: a repository nobody has let near a
/// machine can never be routed, whatever labels it asks for, and saying
/// so at trigger time is the difference between a settings page to visit
/// and a build that sits queued forever.
pub fn any_group_admits(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    public: bool,
) -> Result<bool, Error> {
    ensure_default_group(db, org_id)?;
    let hit = db
        .lock()
        .query_opt(
            "SELECT 1 FROM runner_groups g WHERE g.org_id = $1 \
               AND (NOT $3 OR g.allow_public) \
               AND (g.repo_access = 'all' \
                    OR EXISTS (SELECT 1 FROM runner_group_repos gr \
                               WHERE gr.group_id = g.id AND gr.repo_id = $2)) \
             LIMIT 1",
            &[&org_id, &repo_id, &public],
        )
        .map_err(db_err("read runner group admission"))?;
    Ok(hit.is_some())
}

/// Whether a live runner exists that could ever take a job with these
/// labels in this repository — the fourth refusal.
pub fn any_runner_for(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    public: bool,
    labels: &[String],
) -> Result<bool, Error> {
    let labels = labels.to_vec();
    let hit = db
        .lock()
        .query_opt(
            "SELECT 1 FROM runners r JOIN runner_groups g ON g.id = r.group_id \
             WHERE r.org_id = $1 AND r.removed_at IS NULL \
               AND r.labels @> $4::TEXT[] \
               AND (NOT $3 OR g.allow_public) \
               AND (g.repo_access = 'all' \
                    OR EXISTS (SELECT 1 FROM runner_group_repos gr \
                               WHERE gr.group_id = g.id AND gr.repo_id = $2)) \
             LIMIT 1",
            &[&org_id, &repo_id, &public, &labels],
        )
        .map_err(db_err("read runners for labels"))?;
    Ok(hit.is_some())
}

// ---------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------

/// What a freshly minted registration token is.
#[derive(Debug, Clone)]
pub struct RegistrationToken {
    /// The only time the plaintext exists.
    pub token: String,
    pub group: String,
    pub expires_at: i64,
}

/// Mint a single-use, one-hour registration token for a group.
pub fn mint_registration_token(
    db: &ControlDb,
    org_id: &str,
    group_name: Option<&str>,
    actx: &AuditCtx,
) -> Result<RegistrationToken, Error> {
    let default_id = ensure_default_group(db, org_id)?;
    let (group_id, group) = match group_name.filter(|g| !g.is_empty()) {
        None => (default_id, DEFAULT_GROUP.to_string()),
        Some(name) => {
            let row = db
                .lock()
                .query_opt(
                    "SELECT id, name FROM runner_groups WHERE org_id = $1 AND name = $2",
                    &[&org_id, &name],
                )
                .map_err(db_err("read runner group"))?
                .ok_or_else(|| Error::Invalid(format!("no runner group named {name:?}")))?;
            (row.get("id"), row.get("name"))
        }
    };
    let id = ulid();
    let secret = token_secret();
    let now = now_ms();
    let expires_at = now + REGISTRATION_TTL_MS;
    let hashed = hash(&secret);
    let org = org_id.to_string();
    let row_id = id.clone();
    let gid = group_id.clone();
    let ctx = actx.clone();
    let who = actx.principal.clone();
    let audited = serde_json::json!({ "group": group, "expires_at": expires_at });
    db.lock()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runner_registration_tokens \
                   (id, org_id, group_id, token_hash, created_by, expires_at, created_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7)",
                &[&row_id, &org, &gid, &hashed, &who, &expires_at, &now],
            )?;
            audit::record_tx(
                tx,
                &ctx,
                None,
                "runner.registration_token.created",
                Some(&audited),
            )?;
            Ok(())
        })
        .map_err(db_err("mint runner registration token"))?;
    Ok(RegistrationToken {
        token: format!("{REGISTRATION_PREFIX}{id}_{secret}"),
        group,
        expires_at,
    })
}

/// Spend a registration token, returning the organisation and group it
/// was for.
///
/// One statement, for the same reason `usertokens::redeem` is: checking
/// and then stamping leaves a window in which two machines register on
/// one token, and every way the token can be wrong — malformed,
/// unknown, spent, expired — is the same `None`, so it cannot be used to
/// learn which organisations exist.
pub fn consume_registration_token(
    db: &ControlDb,
    presented: &str,
) -> Result<Option<(String, String)>, Error> {
    let Some((id, secret)) = split_secret(presented, REGISTRATION_PREFIX) else {
        return Ok(None);
    };
    let now = now_ms();
    let row = db
        .lock()
        .query_opt(
            "UPDATE runner_registration_tokens SET used_at = $3 \
             WHERE id = $1 AND token_hash = $2 AND used_at IS NULL AND expires_at > $3 \
             RETURNING org_id, group_id",
            &[&id, &hash(secret), &now],
        )
        .map_err(db_err("redeem runner registration token"))?;
    Ok(row.map(|r| (r.get("org_id"), r.get("group_id"))))
}

/// What a registration produced.
#[derive(Debug, Clone)]
pub struct Registered {
    pub runner: Runner,
    /// Shown once. Never recoverable.
    pub credential: String,
    /// True when this replaced a live runner of the same name.
    pub rotated: bool,
}

/// Register a machine, or re-register one under a name that is already
/// live — which is the same act and is how a credential is rotated.
///
/// The tombstone and the insert are one transaction. Two rows with one
/// live name is what the partial unique index refuses, and doing it in
/// two statements would leave a window where a machine that had just
/// rotated could not authenticate with either credential.
#[allow(clippy::too_many_arguments)]
pub fn register(
    db: &ControlDb,
    org_id: &str,
    group_id: &str,
    name: &str,
    custom_labels: &[String],
    os: &str,
    arch: &str,
    version: &str,
    ephemeral: bool,
) -> Result<Registered, Error> {
    if !valid_runner_name(name) {
        return Err(Error::Invalid(format!(
            "{name:?} is not a runner name (letters, digits, dot, dash, underscore; \
             at most 64 characters)"
        )));
    }
    if !OSES.contains(&os) {
        return Err(Error::Invalid(format!(
            "os must be one of {}, not {os:?}",
            OSES.join(", ")
        )));
    }
    if !ARCHES.contains(&arch) {
        return Err(Error::Invalid(format!(
            "arch must be one of {}, not {arch:?}",
            ARCHES.join(", ")
        )));
    }
    for l in custom_labels {
        let lower = l.trim().to_ascii_lowercase();
        if !valid_label(&lower) {
            return Err(Error::Invalid(format!(
                "{l:?} is not a label (lowercase letters, digits, dot, dash, underscore; \
                 at most 64 characters)"
            )));
        }
    }
    if version.len() > 64 {
        return Err(Error::Invalid("version is too long".into()));
    }
    let labels = runner_labels(os, arch, custom_labels);
    let id = ulid();
    let secret = token_secret();
    let now = now_ms();
    let credential_hash = hash(&secret);

    let (org, gid) = (org_id.to_string(), group_id.to_string());
    let (rname, ros, rarch, rver) = (
        name.to_string(),
        os.to_string(),
        arch.to_string(),
        version.to_string(),
    );
    let row_id = id.clone();
    let insert_labels = labels.clone();
    let rotated = db
        .lock()
        .transaction(move |tx| {
            let replaced = tx.execute(
                "UPDATE runners SET removed_at = $3 \
                 WHERE org_id = $1 AND name = $2 AND removed_at IS NULL",
                &[&org, &rname, &now],
            )?;
            tx.execute(
                "INSERT INTO runners \
                   (id, org_id, group_id, name, labels, os, arch, version, ephemeral, \
                    credential_hash, last_seen_at, created_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$11)",
                &[
                    &row_id,
                    &org,
                    &gid,
                    &rname,
                    &insert_labels,
                    &ros,
                    &rarch,
                    &rver,
                    &ephemeral,
                    &credential_hash,
                    &now,
                ],
            )?;
            let ctx = AuditCtx {
                principal: format!("runner:{row_id}"),
                user_id: None,
                org_id: org.clone(),
            };
            audit::record_tx(
                tx,
                &ctx,
                None,
                "runner.registered",
                Some(&serde_json::json!({
                    "runner": rname, "id": row_id, "labels": insert_labels,
                    "ephemeral": ephemeral, "rotated": replaced > 0,
                })),
            )?;
            Ok(replaced > 0)
        })
        .map_err(db_err("register runner"))?;

    let runner =
        by_id(db, &id)?.ok_or_else(|| Error::Db("the runner just created is missing".into()))?;
    Ok(Registered {
        runner,
        credential: format!("{CREDENTIAL_PREFIX}{id}_{secret}"),
        rotated,
    })
}

/// One runner by id, whatever its state.
pub fn by_id(db: &ControlDb, id: &str) -> Result<Option<Runner>, Error> {
    if !valid_id(id) {
        return Ok(None);
    }
    let row = db
        .lock()
        .query_opt(
            &format!(
                "SELECT {RUNNER_COLS} FROM runners r \
                 JOIN runner_groups g ON g.id = r.group_id WHERE r.id = $1"
            ),
            &[&id],
        )
        .map_err(db_err("read runner"))?;
    Ok(row.as_ref().map(row_to_runner))
}

/// Resolve a runner credential.
///
/// A removed runner is `None`, which the claim route turns into a 401
/// and the runner turns into "this runner has been removed; register it
/// again" and an exit — the whole revocation mechanism, since there is
/// nothing listening on the operator's machine to tell.
pub fn authenticate(db: &ControlDb, presented: &str) -> Result<Option<Runner>, Error> {
    let Some((id, secret)) = split_secret(presented, CREDENTIAL_PREFIX) else {
        return Ok(None);
    };
    let row = db
        .lock()
        .query_opt(
            &format!(
                "SELECT {RUNNER_COLS} FROM runners r \
                 JOIN runner_groups g ON g.id = r.group_id \
                 WHERE r.id = $1 AND r.credential_hash = $2 AND r.removed_at IS NULL"
            ),
            &[&id, &hash(secret)],
        )
        .map_err(db_err("authenticate runner"))?;
    Ok(row.as_ref().map(row_to_runner))
}

/// Note that a runner called. Cheap, and on every request it makes.
pub fn touch(db: &ControlDb, runner_id: &str, now: i64) -> Result<(), Error> {
    db.lock()
        .execute(
            "UPDATE runners SET last_seen_at = $2 WHERE id = $1 AND removed_at IS NULL",
            &[&runner_id, &now],
        )
        .map(|_| ())
        .map_err(db_err("touch runner"))
}

/// Every live runner in the organisation, with its derived state and
/// whatever it is running.
pub fn list(db: &ControlDb, org_id: &str, now: i64) -> Result<Vec<RunnerView>, Error> {
    let mut conn = db.lock();
    let rows = conn
        .query(
            &format!(
                "SELECT {RUNNER_COLS} FROM runners r \
                 JOIN runner_groups g ON g.id = r.group_id \
                 WHERE r.org_id = $1 AND r.removed_at IS NULL \
                 ORDER BY r.name, r.id"
            ),
            &[&org_id],
        )
        .map_err(db_err("list runners"))?;
    let runners: Vec<Runner> = rows.iter().map(row_to_runner).collect();
    let ids: Vec<String> = runners.iter().map(|r| r.id.clone()).collect();
    let jobs = if ids.is_empty() {
        Vec::new()
    } else {
        conn.query(
            "SELECT j.runner_id, j.id, j.run_id, j.key, rp.name AS repo \
             FROM workflow_jobs j JOIN repos rp ON rp.id = j.repo_id \
             WHERE j.runner_id = ANY($1) AND j.state = 'running'",
            &[&ids],
        )
        .map_err(db_err("read runner jobs"))?
    };
    Ok(runners
        .into_iter()
        .map(|runner| {
            let job = jobs
                .iter()
                .find(|j| j.get::<_, Option<String>>("runner_id").as_deref() == Some(&runner.id))
                .map(|j| RunningJob {
                    run_id: j.get("run_id"),
                    job_id: j.get("id"),
                    key: j.get("key"),
                    repo: j.get("repo"),
                });
            let state = if job.is_some() {
                "busy".to_string()
            } else {
                runner.seen_state(now).to_string()
            };
            RunnerView { runner, state, job }
        })
        .collect())
}

/// What a removal has to be followed by, when the runner was mid-job.
#[derive(Debug, Clone)]
pub struct Removed {
    pub runner: Runner,
    /// The `workflow_jobs` row that was running on it, which the caller
    /// fails with [`REMOVED_MID_JOB`]. Not failed here: recording a
    /// verdict cascades to a run and to mirrored check rows, which is
    /// the server's business and not this table's.
    pub running_job: Option<String>,
}

/// Remove a runner. Its credential is dead from the next call.
pub fn remove(
    db: &ControlDb,
    org_id: &str,
    id: &str,
    actx: &AuditCtx,
) -> Result<Option<Removed>, Error> {
    let Some(runner) = by_id(db, id)? else {
        return Ok(None);
    };
    if runner.org_id != org_id || runner.removed_at.is_some() {
        return Ok(None);
    }
    let now = now_ms();
    let rid = runner.id.clone();
    let rname = runner.name.clone();
    let ctx = actx.clone();
    let running = db
        .lock()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runners SET removed_at = $2 WHERE id = $1 AND removed_at IS NULL",
                &[&rid, &now],
            )?;
            let job = tx.query_opt(
                "SELECT id FROM workflow_jobs WHERE runner_id = $1 AND state = 'running'",
                &[&rid],
            )?;
            audit::record_tx(
                tx,
                &ctx,
                None,
                "runner.removed",
                Some(&serde_json::json!({ "runner": rname, "id": rid })),
            )?;
            Ok(job.map(|r| r.get::<_, String>("id")))
        })
        .map_err(db_err("remove runner"))?;
    Ok(Some(Removed {
        runner,
        running_job: running,
    }))
}

/// Retire an ephemeral runner once its one job is over. `false` when the
/// runner is not ephemeral, is already gone, or does not exist.
pub fn retire_ephemeral(db: &ControlDb, runner_id: &str, now: i64) -> Result<bool, Error> {
    db.lock()
        .execute(
            "UPDATE runners SET removed_at = $2 \
             WHERE id = $1 AND ephemeral AND removed_at IS NULL",
            &[&runner_id, &now],
        )
        .map(|n| n > 0)
        .map_err(db_err("retire ephemeral runner"))
}

/// Tombstone runners nobody has heard from for too long.
///
/// A machine that was decommissioned, or a laptop that was reimaged,
/// otherwise sits in the list forever looking like something an operator
/// should investigate. Ephemeral runners go a day after their last call
/// because one that is still there a day later did not exit after its
/// job and is not coming back.
pub fn sweep(db: &ControlDb, now: i64) -> Result<u64, Error> {
    db.lock()
        .execute(
            "UPDATE runners SET removed_at = $1::BIGINT WHERE removed_at IS NULL AND \
               last_seen_at < CASE WHEN ephemeral THEN $2::BIGINT ELSE $3::BIGINT END",
            &[&now, &(now - STALE_EPHEMERAL_MS), &(now - STALE_MS)],
        )
        .map_err(db_err("sweep runners"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three automatic labels lead, custom ones follow in the order
    /// they were offered, and nothing is repeated. Order is load-bearing
    /// — it is what the runner list shows — and a duplicate would make
    /// two identical chips.
    #[test]
    fn a_runners_labels_are_its_own_plus_the_three_we_always_add() {
        assert_eq!(
            runner_labels("linux", "x64", &["gpu".into(), "big".into()]),
            vec!["self-hosted", "linux", "x64", "gpu", "big"]
        );
        // A runner that offers what we add anyway gets it once.
        assert_eq!(
            runner_labels("linux", "arm64", &["Linux".into(), "self-hosted".into()]),
            vec!["self-hosted", "linux", "arm64"]
        );
        // Case and surrounding space are the operator's, not the
        // routing's: `--labels GPU` and `--labels gpu` must be one label
        // or a job asking for `gpu` silently never runs.
        assert_eq!(
            runner_labels("macos", "arm64", &["  GPU  ".into()]),
            vec!["self-hosted", "macos", "arm64", "gpu"]
        );
        // An empty entry is dropped rather than becoming a label no job
        // can ever name (`--labels a,,b`).
        assert_eq!(
            runner_labels("linux", "x64", &["".into(), " ".into(), "a".into()]),
            vec!["self-hosted", "linux", "x64", "a"]
        );
    }

    #[test]
    fn labels_and_names_have_the_shapes_the_contract_says() {
        assert!(valid_label("gpu"));
        assert!(valid_label("cuda-12.1"));
        assert!(valid_label("a_b"));
        assert!(valid_label(&"a".repeat(64)));
        assert!(!valid_label(&"a".repeat(65)));
        assert!(!valid_label(""));
        // Uppercase is refused rather than folded, because the folding
        // happens before this is asked — see `runner_labels`.
        assert!(!valid_label("GPU"));
        assert!(!valid_label("has space"));
        assert!(!valid_label("semi;colon"));

        assert!(valid_runner_name("build-box-01"));
        assert!(valid_runner_name("Mac.mini"));
        assert!(!valid_runner_name(""));
        assert!(!valid_runner_name(".hidden"));
        assert!(!valid_runner_name("has space"));
        assert!(!valid_runner_name(&"a".repeat(65)));
    }

    /// Every malformed credential shape is refused before it reaches a
    /// query, so hostile bytes never become a database error where a
    /// `None` was promised.
    #[test]
    fn a_credential_must_look_like_one_before_it_is_looked_up() {
        assert!(split_secret("weftr_", CREDENTIAL_PREFIX).is_none());
        assert!(split_secret("weftr_onlyid", CREDENTIAL_PREFIX).is_none());
        assert!(split_secret("nonsense", CREDENTIAL_PREFIX).is_none());
        // The right shape under the wrong prefix.
        assert!(split_secret("weftg_01zzzzzzzzzzzzzzzzzzzzzzzz_s", CREDENTIAL_PREFIX).is_none());
        // An id that is not an id we mint — a NUL or a quote here would
        // reach the query otherwise.
        assert!(split_secret("weftr_not-a-ulid_s", CREDENTIAL_PREFIX).is_none());
        assert_eq!(
            split_secret("weftr_01zzzzzzzzzzzzzzzzzzzzzzzz_sec", CREDENTIAL_PREFIX),
            Some(("01zzzzzzzzzzzzzzzzzzzzzzzz", "sec"))
        );
    }

    // -----------------------------------------------------------------
    // Against a real database
    // -----------------------------------------------------------------

    use crate::registry::{self, NewRepo, RepoKind};
    use crate::workflows::{self, NewJob, NewRun, RunnerRoute};

    struct World {
        db: ControlDb,
        org: String,
        repo: String,
        ctx: AuditCtx,
    }

    fn world(hint: &str) -> World {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let repo = repo_in(&db, &org.id, "app", false);
        let ctx = AuditCtx::system(&org.id, "test");
        World {
            db,
            org: org.id,
            repo,
            ctx,
        }
    }

    fn repo_in(db: &ControlDb, org_id: &str, name: &str, public: bool) -> String {
        registry::create_repo(
            db,
            org_id,
            &NewRepo {
                name,
                kind: RepoKind::Native,
                public,
                description: None,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap()
        .id
    }

    /// Register a machine the way the HTTP route does: mint a token,
    /// spend it, register with what came back.
    fn enrol(w: &World, name: &str, labels: &[&str], group: Option<&str>) -> Registered {
        let token = mint_registration_token(&w.db, &w.org, group, &w.ctx).unwrap();
        let (org_id, group_id) = consume_registration_token(&w.db, &token.token)
            .unwrap()
            .expect("a fresh token is live");
        let custom: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
        register(
            &w.db, &org_id, &group_id, name, &custom, "linux", "x64", "0.1.0", false,
        )
        .unwrap()
    }

    /// Queue one self-hosted job asking for `labels`.
    fn queue(w: &World, repo_id: &str, labels: &[&str], key: &str) -> String {
        let want: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
        let sha = format!("{:0>40}", key.len());
        let run = workflows::create_run(
            &w.db,
            &w.org,
            repo_id,
            &NewRun {
                file: ".weft/ci.yml",
                name: "ci",
                commit_sha: &sha,
                ref_name: Some("main"),
                event: "push",
                change_key: None,
                changeset_id: None,
                composition: None,
                from_fork: false,
            },
            &[NewJob {
                job_id: key,
                key,
                pool: workflows::POOL_SELF_HOSTED,
                labels: &want,
                ..Default::default()
            }],
        )
        .unwrap();
        workflows::jobs_of(&w.db, &run.id).unwrap()[0].id.clone()
    }

    fn route<'a>(runner: &'a Runner, group: &'a Group) -> RunnerRoute<'a> {
        RunnerRoute {
            runner_id: &runner.id,
            org_id: &runner.org_id,
            group_id: &group.id,
            labels: &runner.labels,
            all_repos: group.repo_access == "all",
            allow_public: group.allow_public,
        }
    }

    fn default_group(w: &World) -> Group {
        list_groups(&w.db, &w.org)
            .unwrap()
            .into_iter()
            .find(|g| g.is_default)
            .expect("every org has a default group")
    }

    /// The default group is made once, however many callers race for it,
    /// and the partial unique index is what decides — not a read
    /// followed by a write, which two nodes both win.
    #[test]
    fn the_default_group_is_created_once_and_only_once() {
        let w = world("runners-default-group");
        let a = ensure_default_group(&w.db, &w.org).unwrap();
        let b = ensure_default_group(&w.db, &w.org).unwrap();
        assert_eq!(a, b);
        let groups = list_groups(&w.db, &w.org).unwrap();
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].name, DEFAULT_GROUP);
        assert!(groups[0].is_default);
        // Open to every repository, closed to public ones — the two
        // defaults this feature's security rests on.
        assert_eq!(groups[0].repo_access, "all");
        assert!(!groups[0].allow_public);
    }

    /// A registration token works once, and every other way of
    /// presenting one is the same `None` — so it cannot be used to ask
    /// which organisations or groups exist.
    #[test]
    fn a_registration_token_is_single_use_and_fails_identically_otherwise() {
        let w = world("runners-regtoken");
        let t = mint_registration_token(&w.db, &w.org, None, &w.ctx).unwrap();
        assert!(t.token.starts_with(REGISTRATION_PREFIX));
        assert_eq!(t.group, DEFAULT_GROUP);
        assert!(t.expires_at > now_ms());

        let id = t
            .token
            .strip_prefix(REGISTRATION_PREFIX)
            .and_then(|r| r.split_once('_'))
            .map(|(i, _)| i.to_string())
            .unwrap();
        for bad in [
            String::new(),
            "nonsense".into(),
            "weftg_".into(),
            "weftg_onlyid".into(),
            // The right id with the wrong secret: the id is not the
            // credential.
            format!("weftg_{id}_{}", "x".repeat(52)),
            // Well formed, for a row nobody minted.
            format!("weftg_01zzzzzzzzzzzzzzzzzzzzzzzz_{}", "x".repeat(52)),
            // The genuine secret under the runner-credential prefix.
            t.token.replace(REGISTRATION_PREFIX, CREDENTIAL_PREFIX),
        ] {
            assert!(
                consume_registration_token(&w.db, &bad).unwrap().is_none(),
                "{bad:?} was spent"
            );
        }
        assert!(consume_registration_token(&w.db, &t.token)
            .unwrap()
            .is_some());
        // …and only once. Two machines must not register on one token.
        assert!(consume_registration_token(&w.db, &t.token)
            .unwrap()
            .is_none());
    }

    /// An expired token is dead even though nothing spent it. Reached
    /// past the API to age it, because the minting path deliberately has
    /// no way to make one that is already dead.
    #[test]
    fn an_expired_registration_token_is_refused() {
        let w = world("runners-regtoken-expiry");
        let t = mint_registration_token(&w.db, &w.org, None, &w.ctx).unwrap();
        w.db.lock()
            .execute(
                "UPDATE runner_registration_tokens SET expires_at = $1 WHERE org_id = $2",
                &[&(now_ms() - 1), &w.org],
            )
            .unwrap();
        assert!(consume_registration_token(&w.db, &t.token)
            .unwrap()
            .is_none());
    }

    /// Registration mints a credential that authenticates, and the
    /// credential is not recoverable from anything stored.
    #[test]
    fn a_registered_runner_authenticates_with_what_it_was_handed() {
        let w = world("runners-register");
        let r = enrol(&w, "build-box", &["gpu"], None);
        assert!(r.credential.starts_with(CREDENTIAL_PREFIX));
        assert!(!r.rotated);
        assert_eq!(
            r.runner.labels,
            vec!["self-hosted", "linux", "x64", "gpu"],
            "the three automatic labels lead, custom ones follow"
        );

        let back = authenticate(&w.db, &r.credential).unwrap().unwrap();
        assert_eq!(back.id, r.runner.id);
        assert_eq!(back.group_name, DEFAULT_GROUP);

        // Only the hash is stored: the plaintext appears nowhere.
        let stored: String =
            w.db.lock()
                .query_one(
                    "SELECT credential_hash FROM runners WHERE id = $1",
                    &[&r.runner.id],
                )
                .unwrap()
                .get("credential_hash");
        assert_ne!(stored, r.credential);
        assert!(!r.credential.contains(&stored));

        // A near-miss credential is nobody.
        assert!(authenticate(&w.db, &format!("{}x", r.credential))
            .unwrap()
            .is_none());
        assert!(authenticate(&w.db, "weftr_nonsense").unwrap().is_none());
    }

    /// Re-registering a live name replaces the machine: that is how a
    /// credential is rotated, and the old one must be dead the instant
    /// the new one exists.
    #[test]
    fn re_registering_the_same_name_rotates_the_credential() {
        let w = world("runners-rotate");
        let first = enrol(&w, "build-box", &["gpu"], None);
        let second = enrol(&w, "build-box", &["gpu"], None);
        assert!(second.rotated);
        assert_ne!(first.runner.id, second.runner.id);
        assert_ne!(first.credential, second.credential);

        assert!(authenticate(&w.db, &first.credential).unwrap().is_none());
        assert_eq!(
            authenticate(&w.db, &second.credential).unwrap().unwrap().id,
            second.runner.id
        );
        // One machine in the list, not two: the old row is a tombstone,
        // kept so last week's build can still name what ran it.
        let live = list(&w.db, &w.org, now_ms()).unwrap();
        assert_eq!(live.len(), 1, "{live:?}");
        assert_eq!(live[0].runner.id, second.runner.id);
        assert!(by_id(&w.db, &first.runner.id)
            .unwrap()
            .unwrap()
            .removed_at
            .is_some());
    }

    /// Everything about a machine that is not the shape it must be is
    /// refused as `Invalid`, which the API answers 422 to.
    #[test]
    fn registration_refuses_a_machine_it_could_not_route_to() {
        let w = world("runners-register-invalid");
        let gid = ensure_default_group(&w.db, &w.org).unwrap();
        let bad = |name: &str, labels: &[&str], os: &str, arch: &str| {
            let custom: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
            register(&w.db, &w.org, &gid, name, &custom, os, arch, "0.1.0", false)
                .expect_err("should be refused")
        };
        assert!(matches!(bad("", &[], "linux", "x64"), Error::Invalid(_)));
        assert!(matches!(
            bad("has space", &[], "linux", "x64"),
            Error::Invalid(_)
        ));
        assert!(matches!(bad("box", &[], "plan9", "x64"), Error::Invalid(_)));
        assert!(matches!(
            bad("box", &[], "linux", "i386"),
            Error::Invalid(_)
        ));
        // A label that could never be written in a `runs-on:` list is
        // refused at registration rather than becoming a label no job
        // can ever name.
        assert!(matches!(
            bad("box", &["has space"], "linux", "x64"),
            Error::Invalid(_)
        ));
        // Nothing was written by any of them.
        assert!(list(&w.db, &w.org, now_ms()).unwrap().is_empty());
    }

    /// `busy` beats the clock; the clock is a window; and the job a
    /// machine is running is named so a reader can click through to it.
    #[test]
    fn the_runner_list_derives_state_and_names_the_job() {
        let w = world("runners-list-state");
        let r = enrol(&w, "box", &[], None);
        let now = now_ms();

        let view = &list(&w.db, &w.org, now).unwrap()[0];
        assert_eq!(view.state, "online");
        assert!(view.job.is_none());

        // Nothing heard from it for two minutes.
        assert_eq!(
            list(&w.db, &w.org, now + 2 * ONLINE_WINDOW_MS).unwrap()[0].state,
            "offline"
        );

        // Now give it a job. `busy` is derived from the job pointing at
        // it, so it wins even over a `last_seen_at` that has gone stale.
        // In a *second* repository, so that the name the list reports
        // is the job's own and not whichever repository happened to be
        // first: the page links to `owner/repo/…` and a name from the
        // wrong row sends somebody to a build that is not this one.
        let tools = repo_in(&w.db, &w.org, "tools", false);
        let job_id = queue(&w, &tools, &["self-hosted"], "test");
        let group = default_group(&w);
        let claimed = workflows::claim_self_hosted(&w.db, &route(&r.runner, &group), 60_000)
            .unwrap()
            .expect("the job is routable");
        assert_eq!(claimed.id, job_id);

        let view = &list(&w.db, &w.org, now + 2 * ONLINE_WINDOW_MS).unwrap()[0];
        assert_eq!(view.state, "busy");
        let job = view.job.as_ref().expect("busy names its job");
        assert_eq!(job.job_id, job_id);
        assert_eq!(job.key, "test");
        assert_eq!(job.run_id, claimed.run_id);
        assert_eq!(job.repo, "tools");
    }

    /// Routing is a subset test, in both directions.
    #[test]
    fn a_job_reaches_only_a_runner_that_has_every_label_it_asked_for() {
        let w = world("runners-routing-labels");
        let plain = enrol(&w, "plain", &[], None);
        let gpu = enrol(&w, "gpu-box", &["gpu"], None);
        let group = default_group(&w);

        let wanted = queue(&w, &w.repo, &["self-hosted", "gpu"], "gpu-job");
        // The machine without `gpu` cannot see it, however long it asks.
        assert!(
            workflows::claim_self_hosted(&w.db, &route(&plain.runner, &group), 60_000)
                .unwrap()
                .is_none()
        );
        let took = workflows::claim_self_hosted(&w.db, &route(&gpu.runner, &group), 60_000)
            .unwrap()
            .expect("the gpu machine has every label asked for");
        assert_eq!(took.id, wanted);

        // The other direction: extra labels on the runner are fine. A
        // job asking only for `self-hosted` runs anywhere.
        let any = queue(&w, &w.repo, &["self-hosted"], "any-job");
        let took = workflows::claim_self_hosted(&w.db, &route(&plain.runner, &group), 60_000)
            .unwrap()
            .expect("a bare self-hosted job runs on any machine");
        assert_eq!(took.id, any);
    }

    /// A runner never takes a job outside the self-hosted pool. This
    /// edition never writes one — a file asking for a hosted runner is
    /// refused at trigger — but the column admits `hosted`, and a row
    /// with it (restored from another deployment's backup, say) must sit
    /// in the queue rather than be run on somebody's machine.
    #[test]
    fn a_runner_never_takes_a_job_outside_its_pool() {
        let w = world("runners-pool-isolation");
        let r = enrol(&w, "box", &[], None);
        let group = default_group(&w);

        let run = workflows::create_run(
            &w.db,
            &w.org,
            &w.repo,
            &NewRun {
                file: ".weft/ci.yml",
                name: "ci",
                commit_sha: &"a".repeat(40),
                ref_name: Some("main"),
                event: "push",
                change_key: None,
                changeset_id: None,
                composition: None,
                from_fork: false,
            },
            &[
                NewJob {
                    job_id: "hosted",
                    key: "hosted",
                    pool: "hosted",
                    labels: &[],
                    ..Default::default()
                },
                NewJob {
                    job_id: "mine",
                    key: "mine",
                    pool: workflows::POOL_SELF_HOSTED,
                    labels: &["self-hosted".to_string()],
                    ..Default::default()
                },
            ],
        )
        .unwrap();
        let jobs = workflows::jobs_of(&w.db, &run.id).unwrap();
        let mine = jobs.iter().find(|j| j.key == "mine").unwrap();

        // Both queued and ready, which is the arrangement that can go
        // wrong: the hosted one is older, and has no labels a runner
        // could fail to hold.
        let took = workflows::claim_self_hosted(&w.db, &route(&r.runner, &group), 60_000)
            .unwrap()
            .expect("the self-hosted job is routable");
        assert_eq!(took.id, mine.id, "a runner took the hosted job");
        assert!(
            workflows::claim_self_hosted(&w.db, &route(&r.runner, &group), 60_000)
                .unwrap()
                .is_none(),
            "a runner took a job outside its pool"
        );
    }

    /// A public repository is not admitted by default, and that default
    /// is the lock: a public repository can be forked, and a fork's
    /// change carries its own `run:` lines.
    #[test]
    fn a_public_repository_needs_the_group_to_say_so() {
        let w = world("runners-public");
        let public = repo_in(&w.db, &w.org, "open", true);
        let r = enrol(&w, "box", &[], None);
        let group = default_group(&w);
        queue(&w, &public, &["self-hosted"], "test");

        assert!(!any_group_admits(&w.db, &w.org, &public, true).unwrap());
        assert!(
            workflows::claim_self_hosted(&w.db, &route(&r.runner, &group), 60_000)
                .unwrap()
                .is_none(),
            "a public repository reached a machine that never allowed one"
        );

        update_group(
            &w.db,
            &w.org,
            &group.id,
            None,
            None,
            Some(true),
            None,
            &w.ctx,
        )
        .unwrap();
        let group = default_group(&w);
        assert!(any_group_admits(&w.db, &w.org, &public, true).unwrap());
        assert!(
            workflows::claim_self_hosted(&w.db, &route(&r.runner, &group), 60_000)
                .unwrap()
                .is_some()
        );
    }

    /// A `selected` group admits exactly the repositories it names.
    #[test]
    fn a_selected_group_admits_only_the_repositories_it_names() {
        let w = world("runners-selected-group");
        let other = repo_in(&w.db, &w.org, "other", false);
        let g = create_group(
            &w.db,
            &w.org,
            "builders",
            Some("selected"),
            None,
            Some(&["app".to_string()]),
            &w.ctx,
        )
        .unwrap();
        assert_eq!(g.repos, vec!["app"]);
        let r = enrol(&w, "box", &[], Some("builders"));
        assert_eq!(r.runner.group_name, "builders");

        queue(&w, &other, &["self-hosted"], "elsewhere");
        assert!(
            workflows::claim_self_hosted(&w.db, &route(&r.runner, &g), 60_000)
                .unwrap()
                .is_none(),
            "a repository the group does not name reached the machine"
        );
        let mine = queue(&w, &w.repo, &["self-hosted"], "here");
        assert_eq!(
            workflows::claim_self_hosted(&w.db, &route(&r.runner, &g), 60_000)
                .unwrap()
                .unwrap()
                .id,
            mine
        );
        // A repository name that is not there is a refusal, not a
        // silent drop: a group that quietly admits four of the five
        // somebody listed is a build that does not run and a settings
        // page that says it should.
        assert!(matches!(
            create_group(
                &w.db,
                &w.org,
                "typo",
                Some("selected"),
                None,
                Some(&["ap".to_string()]),
                &w.ctx,
            ),
            Err(Error::Invalid(_))
        ));
    }

    /// The organisation's policy is re-read at claim time, not only at
    /// trigger time: a job can sit in the queue across a policy change,
    /// and the answer that matters is the one at the moment it starts.
    #[test]
    fn the_org_policy_is_applied_again_at_claim_time() {
        let w = world("runners-policy-claim");
        let r = enrol(&w, "box", &[], None);
        let group = default_group(&w);
        queue(&w, &w.repo, &["self-hosted"], "test");

        set_policy(&w.db, &w.org, Some("disabled"), None, &w.ctx).unwrap();
        assert!(
            workflows::claim_self_hosted(&w.db, &route(&r.runner, &group), 60_000)
                .unwrap()
                .is_none()
        );

        // `selected`, naming a different repository: still refused.
        let other = repo_in(&w.db, &w.org, "other", false);
        let _ = other;
        set_policy(
            &w.db,
            &w.org,
            Some("selected"),
            Some(&["other".to_string()]),
            &w.ctx,
        )
        .unwrap();
        assert!(!self_hosted_allowed(&w.db, &w.org, &w.repo).unwrap());
        assert!(
            workflows::claim_self_hosted(&w.db, &route(&r.runner, &group), 60_000)
                .unwrap()
                .is_none()
        );

        // Naming this one: admitted.
        let p = set_policy(&w.db, &w.org, None, Some(&["app".to_string()]), &w.ctx).unwrap();
        assert_eq!(p.self_hosted_repos, vec!["app"]);
        assert!(self_hosted_allowed(&w.db, &w.org, &w.repo).unwrap());
        assert!(
            workflows::claim_self_hosted(&w.db, &route(&r.runner, &group), 60_000)
                .unwrap()
                .is_some()
        );
    }

    /// The policy's values are an enumeration, and anything outside it
    /// is refused rather than stored — a stored `"disabled "` would
    /// silently admit everything.
    #[test]
    fn the_policy_refuses_a_value_outside_its_enumeration() {
        let w = world("runners-policy-values");
        assert!(matches!(
            set_policy(&w.db, &w.org, Some("some"), None, &w.ctx),
            Err(Error::Invalid(_))
        ));
        // …and nothing moved.
        let p = policy(&w.db, &w.org).unwrap();
        assert_eq!(p.self_hosted, "all");
        assert!(p.self_hosted_repos.is_empty());
    }

    /// A lease that lapsed hands the job to the next machine, and the
    /// attempt count is what tells the claimer it is a second try.
    #[test]
    fn a_lapsed_lease_hands_the_job_to_the_next_machine() {
        let w = world("runners-lease");
        let first = enrol(&w, "first", &[], None);
        let second = enrol(&w, "second", &[], None);
        let group = default_group(&w);
        let job_id = queue(&w, &w.repo, &["self-hosted"], "test");

        let took = workflows::claim_self_hosted(&w.db, &route(&first.runner, &group), 60_000)
            .unwrap()
            .unwrap();
        assert_eq!(took.attempts, 1);
        assert_eq!(took.runner_id.as_deref(), Some(first.runner.id.as_str()));
        // While the lease is live nobody else may have it.
        assert!(
            workflows::claim_self_hosted(&w.db, &route(&second.runner, &group), 60_000)
                .unwrap()
                .is_none()
        );

        w.db.lock()
            .execute(
                "UPDATE workflow_jobs SET lease_until = $2 WHERE id = $1",
                &[&job_id, &(now_ms() - 1)],
            )
            .unwrap();
        let again = workflows::claim_self_hosted(&w.db, &route(&second.runner, &group), 60_000)
            .unwrap()
            .expect("a dead lease is free work");
        assert_eq!(again.id, job_id);
        assert_eq!(again.attempts, 2);
        assert_eq!(again.runner_id.as_deref(), Some(second.runner.id.as_str()));
    }

    /// Removing a machine kills its credential and hands back the job it
    /// was in the middle of, for the caller to fail.
    #[test]
    fn removing_a_runner_kills_its_credential_and_names_its_job() {
        let w = world("runners-remove");
        let r = enrol(&w, "box", &[], None);
        let group = default_group(&w);
        let job_id = queue(&w, &w.repo, &["self-hosted"], "test");
        workflows::claim_self_hosted(&w.db, &route(&r.runner, &group), 60_000).unwrap();

        let removed = remove(&w.db, &w.org, &r.runner.id, &w.ctx)
            .unwrap()
            .expect("it was there");
        assert_eq!(removed.running_job.as_deref(), Some(job_id.as_str()));
        assert!(authenticate(&w.db, &r.credential).unwrap().is_none());
        assert!(list(&w.db, &w.org, now_ms()).unwrap().is_empty());
        // Removing it twice is a 404, not a second removal.
        assert!(remove(&w.db, &w.org, &r.runner.id, &w.ctx)
            .unwrap()
            .is_none());
        // And a machine in another organisation is simply not there.
        let other = registry::create_org(&w.db, "globex").unwrap();
        assert!(remove(&w.db, &other.id, &r.runner.id, &w.ctx)
            .unwrap()
            .is_none());
    }

    /// An ephemeral machine is retired the moment its one job is over;
    /// an ordinary one is not.
    #[test]
    fn only_an_ephemeral_runner_is_retired_after_its_job() {
        let w = world("runners-ephemeral");
        let gid = ensure_default_group(&w.db, &w.org).unwrap();
        let ordinary = register(
            &w.db,
            &w.org,
            &gid,
            "steady",
            &[],
            "linux",
            "x64",
            "0.1.0",
            false,
        )
        .unwrap();
        let once = register(
            &w.db,
            &w.org,
            &gid,
            "once",
            &[],
            "linux",
            "x64",
            "0.1.0",
            true,
        )
        .unwrap();
        assert!(!retire_ephemeral(&w.db, &ordinary.runner.id, now_ms()).unwrap());
        assert!(retire_ephemeral(&w.db, &once.runner.id, now_ms()).unwrap());
        // Idempotent: a verdict that arrives twice must not be an error.
        assert!(!retire_ephemeral(&w.db, &once.runner.id, now_ms()).unwrap());
        assert!(authenticate(&w.db, &once.credential).unwrap().is_none());
        assert!(authenticate(&w.db, &ordinary.credential).unwrap().is_some());
    }

    /// The sweep uses two deadlines, and the ephemeral one is much
    /// shorter: a machine that should have exited after one job and is
    /// still there a day later is not coming back.
    #[test]
    fn the_sweep_has_a_shorter_fuse_for_an_ephemeral_runner() {
        let w = world("runners-sweep");
        let gid = ensure_default_group(&w.db, &w.org).unwrap();
        let steady = register(
            &w.db,
            &w.org,
            &gid,
            "steady",
            &[],
            "linux",
            "x64",
            "0.1.0",
            false,
        )
        .unwrap();
        let once = register(
            &w.db,
            &w.org,
            &gid,
            "once",
            &[],
            "linux",
            "x64",
            "0.1.0",
            true,
        )
        .unwrap();
        let now = now_ms();

        assert_eq!(sweep(&w.db, now).unwrap(), 0, "nothing is stale yet");
        // Two days on: the ephemeral one goes, the ordinary one stays.
        assert_eq!(sweep(&w.db, now + 2 * STALE_EPHEMERAL_MS).unwrap(), 1);
        assert!(authenticate(&w.db, &once.credential).unwrap().is_none());
        assert!(authenticate(&w.db, &steady.credential).unwrap().is_some());
        // Fifteen days on: so does the ordinary one.
        assert_eq!(sweep(&w.db, now + STALE_MS + 1).unwrap(), 1);
        assert!(authenticate(&w.db, &steady.credential).unwrap().is_none());
        // And the sweep is idempotent — a tombstone is not re-swept.
        assert_eq!(sweep(&w.db, now + STALE_MS + 1).unwrap(), 0);
    }

    /// Touching is what makes `online` mean anything, and it must never
    /// resurrect a machine somebody removed.
    #[test]
    fn touching_moves_the_clock_but_not_a_removed_runner() {
        let w = world("runners-touch");
        let r = enrol(&w, "box", &[], None);
        let later = now_ms() + 5_000;
        touch(&w.db, &r.runner.id, later).unwrap();
        assert_eq!(
            by_id(&w.db, &r.runner.id).unwrap().unwrap().last_seen_at,
            later
        );

        remove(&w.db, &w.org, &r.runner.id, &w.ctx).unwrap();
        touch(&w.db, &r.runner.id, later + 5_000).unwrap();
        let back = by_id(&w.db, &r.runner.id).unwrap().unwrap();
        assert_eq!(back.last_seen_at, later, "a removed runner was touched");
        assert!(back.removed_at.is_some());
    }

    /// Groups: a duplicate name is a conflict, the default is immovable,
    /// and deleting one moves its machines rather than stranding them.
    #[test]
    fn groups_refuse_a_duplicate_name_and_protect_the_default() {
        let w = world("runners-groups");
        let g = create_group(&w.db, &w.org, "builders", None, None, None, &w.ctx).unwrap();
        assert!(!g.is_default);
        assert_eq!(g.runners, 0);
        assert!(matches!(
            create_group(&w.db, &w.org, "builders", None, None, None, &w.ctx),
            Err(Error::Conflict(_))
        ));
        // The default group's name is the one printed in every
        // registration command ever handed out.
        let default = default_group(&w);
        assert!(matches!(
            update_group(
                &w.db,
                &w.org,
                &default.id,
                Some("renamed"),
                None,
                None,
                None,
                &w.ctx
            ),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            delete_group(&w.db, &w.org, &default.id, &w.ctx),
            Err(Error::Invalid(_))
        ));
        // Renaming a *non*-default group onto a taken name is a conflict
        // rather than a silent no-op.
        create_group(&w.db, &w.org, "other", None, None, None, &w.ctx).unwrap();
        assert!(matches!(
            update_group(
                &w.db,
                &w.org,
                &g.id,
                Some("other"),
                None,
                None,
                None,
                &w.ctx
            ),
            Err(Error::Conflict(_))
        ));

        // A machine in a deleted group moves to the default rather than
        // being stranded somewhere nothing can route to it.
        let r = enrol(&w, "box", &[], Some("builders"));
        assert_eq!(
            list_groups(&w.db, &w.org)
                .unwrap()
                .iter()
                .find(|x| x.id == g.id)
                .unwrap()
                .runners,
            1
        );
        delete_group(&w.db, &w.org, &g.id, &w.ctx).unwrap();
        let moved = by_id(&w.db, &r.runner.id).unwrap().unwrap();
        assert_eq!(moved.group_id, default.id);
        assert_eq!(moved.group_name, DEFAULT_GROUP);
        // A group that is not there, and one in another organisation,
        // are both simply absent.
        assert!(delete_group(&w.db, &w.org, &g.id, &w.ctx)
            .unwrap()
            .is_none());
        assert!(group(&w.db, &w.org, "not-an-id").unwrap().is_none());
    }

    /// `any_runner_for` is what the trigger refuses on, so it has to
    /// agree with the claim about every one of the same three rules.
    #[test]
    fn the_triggers_question_and_the_claims_answer_agree() {
        let w = world("runners-trigger-agrees");
        let group = default_group(&w);
        let labels = vec!["self-hosted".to_string(), "gpu".to_string()];

        // No machines at all.
        assert!(!any_runner_for(&w.db, &w.org, &w.repo, false, &labels).unwrap());
        // A machine without the label.
        let plain = enrol(&w, "plain", &[], None);
        assert!(!any_runner_for(&w.db, &w.org, &w.repo, false, &labels).unwrap());
        // One with it.
        let gpu = enrol(&w, "gpu-box", &["gpu"], None);
        assert!(any_runner_for(&w.db, &w.org, &w.repo, false, &labels).unwrap());
        // …and the claim agrees, which is the point: a trigger that says
        // "yes" over a claim that says "no" is a job queued forever.
        queue(&w, &w.repo, &["self-hosted", "gpu"], "gpu-job");
        assert!(
            workflows::claim_self_hosted(&w.db, &route(&plain.runner, &group), 60_000)
                .unwrap()
                .is_none()
        );
        assert!(
            workflows::claim_self_hosted(&w.db, &route(&gpu.runner, &group), 60_000)
                .unwrap()
                .is_some()
        );

        // Removing the only machine that could serve it takes the
        // trigger's answer back with it.
        remove(&w.db, &w.org, &gpu.runner.id, &w.ctx).unwrap();
        assert!(!any_runner_for(&w.db, &w.org, &w.repo, false, &labels).unwrap());
    }

    /// `busy` beats the clock, and the clock is a window rather than an
    /// instant.
    #[test]
    fn a_runners_seen_state_is_a_window_not_an_instant() {
        let r = Runner {
            id: "r".into(),
            org_id: "o".into(),
            group_id: "g".into(),
            group_name: "default".into(),
            name: "box".into(),
            labels: vec![],
            os: "linux".into(),
            arch: "x64".into(),
            version: "0.1.0".into(),
            ephemeral: false,
            last_seen_at: 1_000_000,
            created_at: 0,
            removed_at: None,
        };
        assert_eq!(r.seen_state(1_000_000), "online");
        assert_eq!(r.seen_state(1_000_000 + ONLINE_WINDOW_MS), "online");
        assert_eq!(r.seen_state(1_000_000 + ONLINE_WINDOW_MS + 1), "offline");
        // A clock that went backwards reads as online rather than as a
        // machine from the future: the window is what "recently" means,
        // and the failure mode of the other sign is a whole fleet
        // reported offline after an NTP step.
        assert_eq!(r.seen_state(999_000), "online");
    }

    /// The three variants print the sentence they carry and nothing
    /// else. The status code is the variant's job; the words are what a
    /// person reads, and wrapping them in `Invalid(...)` debug noise is
    /// what a `Display` written as a derive would do.
    #[test]
    fn every_refusal_prints_the_sentence_it_carries() {
        assert_eq!(
            Error::Invalid("no such os".into()).to_string(),
            "no such os"
        );
        assert_eq!(Error::Conflict("taken".into()).to_string(), "taken");
        assert_eq!(
            Error::Db("postgres said no".into()).to_string(),
            "postgres said no"
        );
    }

    /// Decorating nothing asks the database nothing. Both callers hold a
    /// group in their hands — `list_groups` has just ensured the default
    /// one exists — so this is the guard rather than a case the API can
    /// reach, and it is here to keep an empty `= ANY($1)` out of the
    /// query log rather than to answer a request.
    #[test]
    fn decorating_an_empty_list_of_groups_asks_nothing() {
        let w = world("runners-decorate-empty");
        let mut conn = w.db.lock();
        assert!(decorate_groups(&mut conn, vec![]).unwrap().is_empty());
    }

    /// The two small vocabularies are enforced where they are written,
    /// not by the column: a value outside them is a 422 with the value
    /// quoted back, so an operator who typed `private` is told what they
    /// typed rather than being handed a database error.
    #[test]
    fn a_group_refuses_a_repo_access_outside_its_enumeration() {
        let w = world("runners-group-access-enum");
        let err = create_group(
            &w.db,
            &w.org,
            "builders",
            Some("private"),
            None,
            None,
            &w.ctx,
        )
        .expect_err("\"private\" is not a repo_access");
        assert!(matches!(err, Error::Invalid(_)), "{err:?}");
        assert!(err.to_string().contains("\"private\""), "{err}");

        // …and again on the way in through an update, where the same
        // string arrives from the same settings form.
        let g = create_group(&w.db, &w.org, "builders", None, None, None, &w.ctx).unwrap();
        assert!(matches!(
            update_group(&w.db, &w.org, &g.id, None, Some("some"), None, None, &w.ctx),
            Err(Error::Invalid(_))
        ));
        // The refused update changed nothing.
        assert_eq!(
            group(&w.db, &w.org, &g.id).unwrap().unwrap().repo_access,
            "all"
        );
    }

    /// A group name has the same shape a runner name does, and it is
    /// checked on the way in and on a rename. The name reaches a shell —
    /// it is what a registration command names — so a space or a
    /// semicolon is refused rather than stored.
    #[test]
    fn a_group_name_has_a_shape_on_creation_and_on_rename() {
        let w = world("runners-group-name-shape");
        for bad in ["", "has space", "semi;colon", &"a".repeat(65)] {
            let err = create_group(&w.db, &w.org, bad, None, None, None, &w.ctx)
                .expect_err("the group name was accepted");
            assert!(matches!(err, Error::Invalid(_)), "{bad:?}: {err:?}");
        }

        let g = create_group(&w.db, &w.org, "builders", None, None, None, &w.ctx).unwrap();
        assert!(matches!(
            update_group(
                &w.db,
                &w.org,
                &g.id,
                Some("has space"),
                None,
                None,
                None,
                &w.ctx
            ),
            Err(Error::Invalid(_))
        ));
        // A name that does have the shape, and is nobody else's, lands.
        let renamed = update_group(
            &w.db,
            &w.org,
            &g.id,
            Some("build-boxes"),
            None,
            None,
            None,
            &w.ctx,
        )
        .unwrap()
        .expect("the group is still there");
        assert_eq!(renamed.name, "build-boxes");
        assert_eq!(renamed.id, g.id, "a rename must not mint a new group");
    }

    /// A group that is not there — a stale settings tab, an id from
    /// another organisation, or bytes that were never an id — is absent
    /// rather than an error, so the route answers 404 and the caller
    /// cannot use the difference to learn which groups exist.
    #[test]
    fn updating_a_group_that_is_not_there_is_simply_absent() {
        let w = world("runners-update-missing-group");
        assert!(update_group(
            &w.db,
            &w.org,
            "not-an-id",
            Some("x"),
            None,
            None,
            None,
            &w.ctx
        )
        .unwrap()
        .is_none());
        assert!(update_group(
            &w.db,
            &w.org,
            "01zzzzzzzzzzzzzzzzzzzzzzzz",
            Some("x"),
            None,
            None,
            None,
            &w.ctx
        )
        .unwrap()
        .is_none());
    }

    /// Narrowing a group's repository list is not a decoration on a
    /// settings page: the set is rewritten, and what the machine may
    /// claim changes with it. Asserted through a claim rather than
    /// through the rows, because the rows are not what an operator is
    /// promising when they edit that field.
    #[test]
    fn rewriting_a_groups_repositories_changes_what_it_admits() {
        let w = world("runners-group-repos-rewrite");
        let other = repo_in(&w.db, &w.org, "other", false);
        let g = create_group(
            &w.db,
            &w.org,
            "builders",
            Some("selected"),
            None,
            Some(&["app".to_string()]),
            &w.ctx,
        )
        .unwrap();
        let r = enrol(&w, "box", &[], Some("builders"));

        // Hand it the other repository's list, and the job it refused a
        // moment ago is the one it now takes.
        let elsewhere = queue(&w, &other, &["self-hosted"], "elsewhere");
        assert!(
            workflows::claim_self_hosted(&w.db, &route(&r.runner, &g), 60_000)
                .unwrap()
                .is_none(),
            "a repository the group does not name reached the machine"
        );
        let g = update_group(
            &w.db,
            &w.org,
            &g.id,
            None,
            Some("selected"),
            None,
            Some(&["other".to_string()]),
            &w.ctx,
        )
        .unwrap()
        .expect("the group is still there");
        assert_eq!(g.repos, vec!["other"], "the set is rewritten, not added to");
        assert_eq!(
            workflows::claim_self_hosted(&w.db, &route(&r.runner, &g), 60_000)
                .unwrap()
                .expect("the newly named repository did not reach the machine")
                .id,
            elsewhere
        );

        // And the repository it used to name is now the one refused.
        queue(&w, &w.repo, &["self-hosted"], "was-mine");
        assert!(
            workflows::claim_self_hosted(&w.db, &route(&r.runner, &g), 60_000)
                .unwrap()
                .is_none(),
            "a repository the group stopped naming still reached the machine"
        );

        // A name that is not there refuses the whole edit, so a typo
        // cannot silently empty the list a fleet is routing on.
        assert!(matches!(
            update_group(
                &w.db,
                &w.org,
                &g.id,
                None,
                None,
                None,
                Some(&["othe".to_string()]),
                &w.ctx,
            ),
            Err(Error::Invalid(_))
        ));
        assert_eq!(
            group(&w.db, &w.org, &g.id).unwrap().unwrap().repos,
            vec!["other"]
        );
    }

    /// The version string is the runner's own claim about itself and is
    /// shown in the list. It is bounded on the way in so a machine
    /// cannot post a kilobyte into every operator's table.
    #[test]
    fn a_runner_cannot_claim_an_unbounded_version() {
        let w = world("runners-version-length");
        let gid = ensure_default_group(&w.db, &w.org).unwrap();
        let err = register(
            &w.db,
            &w.org,
            &gid,
            "box",
            &[],
            "linux",
            "x64",
            &"9".repeat(65),
            false,
        )
        .expect_err("a 65-character version was accepted");
        assert!(matches!(err, Error::Invalid(_)), "{err:?}");
        // Sixty-four is the boundary and is allowed.
        assert!(register(
            &w.db,
            &w.org,
            &gid,
            "box",
            &[],
            "linux",
            "x64",
            &"9".repeat(64),
            false,
        )
        .is_ok());
    }

    /// Bytes that were never an id are absent rather than a query. The
    /// id reaches `WHERE id = $1`, so the shape is checked before the
    /// lookup and both readers of a runner answer the same `None`.
    #[test]
    fn a_runner_id_that_is_not_one_is_absent_rather_than_looked_up() {
        let w = world("runners-bad-id");
        for bad in ["", "not-an-id", "'; DROP TABLE runners; --", "01ZZ ZZ"] {
            assert!(by_id(&w.db, bad).unwrap().is_none(), "{bad:?}");
            assert!(
                remove(&w.db, &w.org, bad, &w.ctx).unwrap().is_none(),
                "{bad:?}"
            );
        }
        // Well formed, for a machine nobody registered.
        assert!(by_id(&w.db, "01zzzzzzzzzzzzzzzzzzzzzzzz")
            .unwrap()
            .is_none());
        assert!(remove(&w.db, &w.org, "01zzzzzzzzzzzzzzzzzzzzzzzz", &w.ctx)
            .unwrap()
            .is_none());
    }

    /// A registration that cannot be written must not take the live
    /// machine down with it. Re-registering tombstones the old row and
    /// inserts the new one in one transaction precisely so that a
    /// failure at the insert leaves the credential the machine is still
    /// long-polling with alive — and the caller is told the database
    /// refused, in the database's own words, rather than being handed a
    /// `None` that reads as "there was no such runner".
    #[test]
    fn a_registration_that_the_database_refuses_leaves_the_live_one_alone() {
        let w = world("runners-register-refused");
        let live = enrol(&w, "build-box", &["gpu"], None);

        // A group id that is not a group: the foreign key refuses the
        // insert after the tombstone has been written.
        let err = register(
            &w.db,
            &w.org,
            "01zzzzzzzzzzzzzzzzzzzzzzzz",
            "build-box",
            &[],
            "linux",
            "x64",
            "0.1.0",
            false,
        )
        .expect_err("a runner was registered into a group that is not there");
        assert!(matches!(err, Error::Db(_)), "{err:?}");
        assert!(
            err.to_string().starts_with("register runner: "),
            "the refusal must name the statement that failed: {err}"
        );

        // The transaction rolled back, so the machine that was already
        // there is still there and its credential still authenticates.
        let back = by_id(&w.db, &live.runner.id).unwrap().unwrap();
        assert!(
            back.removed_at.is_none(),
            "a failed registration tombstoned the live runner"
        );
        assert_eq!(
            authenticate(&w.db, &live.credential).unwrap().unwrap().id,
            live.runner.id
        );
    }
}
