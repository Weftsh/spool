//! Changesets: one review, one verdict and one landing across several
//! repositories of one organization.
//!
//! A changeset owns nothing a change does not already have. Its members
//! are existing [`crate::changes`] rows, one per repository, and its
//! edges say which member lands before which. What it adds is the
//! binding: while a change is a member of an open changeset it lands and
//! closes only through the changeset, so the "one landing" the word
//! promises is a database fact rather than an API habit.
//!
//! State machine: `open` → `landing` → `landed`, or `failed` once the
//! unwind has run; `open` → `abandoned`. Composition — members and edges
//! — is allowed only while `open`. Every transition is guarded by a
//! `WHERE state = …` and reports whether it won, like `changes`.
//!
//! The `landing` transition is the commit point of the landing protocol
//! ([`begin_landing`]): it writes the whole plan as a [`Landing`] in the
//! same transaction, and from then on the changeset leaves `landing`
//! only through [`finish_landing`], whichever worker gets there.
//!
//! Refusals are typed because the API answers them differently: an
//! [`Error::Invalid`] request never described a changeset (a cycle, a
//! stranger's change, too many members) and is a 400; an
//! [`Error::Conflict`] described one the world has since moved under (a
//! change that landed, a change already bound elsewhere, a changeset no
//! longer open) and is a 409.

use crate::audit::AuditCtx;
use crate::db::{detail, is_unique_violation, ControlDb};
use crate::ids::{now_ms, ulid};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The most repositories one landing spans. Small on purpose: every
/// member is one more manifest to CAS inside the apply window, and the
/// window is the honest part of the protocol.
pub const MAX_MEMBERS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The request does not describe a changeset.
    Invalid(String),
    /// It does, but the world has moved: the words say what is where.
    Conflict(String),
    Db(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(s) | Error::Conflict(s) | Error::Db(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Changeset {
    pub id: String,
    pub org_id: String,
    pub key: String,
    pub title: String,
    pub body: String,
    pub state: String,
    pub created_by: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// The id of the changeset this one reverts, when it was made by
    /// `POST …/changesets/{key}/revert`.
    pub reverts: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub change_id: String,
    pub position: i64,
}

/// `from` lands first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from_change_id: String,
    pub to_change_id: String,
}

/// A change offered for membership, with the name to refuse it by.
///
/// The module works on change ids; the API works on `repo/key`. A
/// refusal has to be in the caller's words — "web/Ib33f is landed", not
/// an id nobody typed — so the label travels with the id.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub change_id: &'a str,
    pub label: &'a str,
}

const CS_COLS: &str =
    "id, org_id, key, title, body, state, created_by, created_at, updated_at, reverts";

fn row_to_changeset(r: &postgres::Row) -> Changeset {
    Changeset {
        id: r.get("id"),
        org_id: r.get("org_id"),
        key: r.get("key"),
        title: r.get("title"),
        body: r.get("body"),
        state: r.get("state"),
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        reverts: r.get("reverts"),
    }
}

/// A transaction body's answer: the outer `Err` is the database failing,
/// the inner one is a refusal decided *before anything was written*.
///
/// That ordering is the invariant every writer here keeps: all the
/// reads that can refuse come first, all the writes after, so an
/// `Ok(Err(_))` commits a transaction that changed nothing.
type Decided<T> = Result<Result<T, Error>, postgres::Error>;

fn run<T: Send>(
    db: &ControlDb,
    what: &str,
    dup: &str,
    f: impl FnOnce(&mut postgres::Transaction) -> Decided<T> + Send,
) -> Result<T, Error> {
    match db.lock().transaction(f) {
        Ok(decided) => decided,
        Err(e) if is_unique_violation(&e) => Err(Error::Conflict(dup.to_string())),
        Err(e) => Err(Error::Db(format!("{what}: {}", detail(&e)))),
    }
}

/// Lock the changeset for the rest of the transaction and check that it
/// can still be composed.
fn open_for_update(tx: &mut postgres::Transaction, changeset_id: &str) -> Decided<Changeset> {
    let Some(row) = tx.query_opt(
        &format!("SELECT {CS_COLS} FROM changesets WHERE id = $1 FOR UPDATE"),
        &[&changeset_id],
    )?
    else {
        return Ok(Err(Error::Conflict("no such changeset".into())));
    };
    let cs = row_to_changeset(&row);
    if cs.state != "open" {
        return Ok(Err(Error::Conflict(format!("changeset is {}", cs.state))));
    }
    Ok(Ok(cs))
}

/// Check that a change may join a changeset of `org_id`, and lock its
/// row so the answer holds until the member is written. Answers the
/// change's repository, for the one-member-per-repository rule the
/// callers keep.
///
/// The lock is what makes the one-open-changeset rule race-free: two
/// composers binding the same change serialise here, and the second
/// reads the first's membership rather than tripping the unique index.
fn admit(tx: &mut postgres::Transaction, org_id: &str, c: Candidate) -> Decided<String> {
    let Some(row) = tx.query_opt(
        "SELECT org_id, repo_id, state FROM changes WHERE id = $1 FOR UPDATE",
        &[&c.change_id],
    )?
    else {
        return Ok(Err(Error::Conflict(format!("{} is not a change", c.label))));
    };
    if row.get::<_, String>("org_id") != org_id {
        return Ok(Err(Error::Invalid(format!(
            "{} is in another organization",
            c.label
        ))));
    }
    let state: String = row.get("state");
    if state != "open" {
        return Ok(Err(Error::Conflict(format!("{} is {state}", c.label))));
    }
    if let Some(bound) = tx.query_opt(
        "SELECT c.key FROM changeset_members m JOIN changesets c ON c.id = m.changeset_id \
         WHERE m.change_id = $1 AND m.active",
        &[&c.change_id],
    )? {
        let key: String = bound.get("key");
        return Ok(Err(Error::Conflict(format!(
            "{} is already in changeset {key}",
            c.label
        ))));
    }
    Ok(Ok(row.get("repo_id")))
}

/// The order members land in: a topological sort of the edges, ties
/// broken by member position so the answer is the same every time it is
/// asked. `Err` carries the ids left standing when no member was free
/// to go — a cycle, or every member downstream of one.
fn toposort(members: &[String], edges: &[(String, String)]) -> Result<Vec<String>, Vec<String>> {
    let mut indegree: BTreeMap<&str, usize> = members.iter().map(|m| (m.as_str(), 0)).collect();
    for (_, to) in edges {
        *indegree
            .get_mut(to.as_str())
            .expect("edge endpoint is a member") += 1;
    }
    let mut placed: BTreeSet<&str> = BTreeSet::new();
    let mut order = Vec::with_capacity(members.len());
    while order.len() < members.len() {
        let next = members
            .iter()
            .find(|m| !placed.contains(m.as_str()) && indegree[m.as_str()] == 0);
        let Some(m) = next else {
            return Err(members
                .iter()
                .filter(|m| !placed.contains(m.as_str()))
                .cloned()
                .collect());
        };
        placed.insert(m);
        order.push(m.clone());
        for (from, to) in edges {
            if from == m {
                *indegree
                    .get_mut(to.as_str())
                    .expect("edge endpoint is a member") -= 1;
            }
        }
    }
    Ok(order)
}

/// Edges as `(from, to)` change ids, deduplicated, with the landing
/// order they imply.
type CheckedEdges = (Vec<(String, String)>, Vec<String>);

/// Validate a set of edges against a member list and return them as
/// ids, deduplicated, with the landing order they imply.
fn check_edges(
    members: &[Candidate],
    edges: &[(Candidate, Candidate)],
) -> Result<CheckedEdges, Error> {
    let ids: Vec<String> = members.iter().map(|m| m.change_id.to_string()).collect();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (from, to) in edges {
        for end in [from, to] {
            if !ids.iter().any(|id| id == end.change_id) {
                return Err(Error::Invalid(format!(
                    "{} is not a member of this changeset",
                    end.label
                )));
            }
        }
        if from.change_id == to.change_id {
            return Err(Error::Invalid(format!(
                "{} cannot land before itself",
                from.label
            )));
        }
        if seen.insert((from.change_id, to.change_id)) {
            out.push((from.change_id.to_string(), to.change_id.to_string()));
        }
    }
    let order = toposort(&ids, &out).map_err(|stuck| {
        let names: Vec<&str> = stuck
            .iter()
            .filter_map(|id| members.iter().find(|m| m.change_id == id))
            .map(|m| m.label)
            .collect();
        Error::Invalid(format!("edges form a cycle through {}", names.join(", ")))
    })?;
    Ok((out, order))
}

fn valid_title(title: &str) -> bool {
    let t = title.trim();
    !t.is_empty() && t.len() <= 200
}

/// Create a changeset over `members` (one existing open change each),
/// with `edges` saying who lands first. Everything is checked before
/// anything is written: a refusal leaves no row behind. `reverts` names
/// the changeset this one undoes, for the one caller that makes such a
/// thing.
#[allow(clippy::too_many_arguments)]
pub fn create(
    db: &ControlDb,
    org_id: &str,
    key: &str,
    title: &str,
    body: &str,
    created_by: Option<&str>,
    members: &[Candidate],
    edges: &[(Candidate, Candidate)],
    reverts: Option<&str>,
    audit: &AuditCtx,
) -> Result<Changeset, Error> {
    if !crate::changes::valid_change_key(key) {
        return Err(Error::Invalid(format!("invalid changeset key {key:?}")));
    }
    if !valid_title(title) {
        return Err(Error::Invalid(
            "a title is required (at most 200 bytes)".into(),
        ));
    }
    if members.is_empty() || members.len() > MAX_MEMBERS {
        return Err(Error::Invalid(format!(
            "a changeset has between 1 and {MAX_MEMBERS} members"
        )));
    }
    let mut distinct = BTreeSet::new();
    if let Some(dup) = members.iter().find(|m| !distinct.insert(m.change_id)) {
        return Err(Error::Invalid(format!("{} is listed twice", dup.label)));
    }
    let (edge_ids, _) = check_edges(members, edges)?;
    let org_id = org_id.to_string();
    let key = key.to_string();
    let title = title.trim().to_string();
    let body = body.to_string();
    let created_by = created_by.map(str::to_string);
    let reverts = reverts.map(str::to_string);
    let members: Vec<Candidate> = members.to_vec();
    let dup = format!("changeset {key} already exists");
    run(db, "create changeset", &dup, move |tx| {
        // One member per repository: two changes to one manifest would
        // be two CASes whose second depends on the first's outcome, and
        // the protocol has one plan per repository.
        let mut repos: BTreeMap<String, &str> = BTreeMap::new();
        for m in &members {
            let repo_id = match admit(tx, &org_id, *m)? {
                Ok(r) => r,
                Err(e) => return Ok(Err(e)),
            };
            if let Some(other) = repos.insert(repo_id, m.label) {
                return Ok(Err(Error::Invalid(format!(
                    "{other} and {} are in the same repository",
                    m.label
                ))));
            }
        }
        let now = now_ms();
        let cs = Changeset {
            id: ulid(),
            org_id,
            key,
            title,
            body,
            state: "open".into(),
            created_by,
            created_at: now,
            updated_at: now,
            reverts,
        };
        tx.execute(
            &format!(
                "INSERT INTO changesets ({CS_COLS}) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"
            ),
            &[
                &cs.id,
                &cs.org_id,
                &cs.key,
                &cs.title,
                &cs.body,
                &cs.state,
                &cs.created_by,
                &cs.created_at,
                &cs.updated_at,
                &cs.reverts,
            ],
        )?;
        for (i, m) in members.iter().enumerate() {
            tx.execute(
                "INSERT INTO changeset_members (changeset_id, change_id, position) \
                 VALUES ($1, $2, $3)",
                &[&cs.id, &m.change_id, &(i as i64 + 1)],
            )?;
        }
        for (from, to) in &edge_ids {
            tx.execute(
                "INSERT INTO changeset_edges (changeset_id, from_change_id, to_change_id) \
                 VALUES ($1, $2, $3)",
                &[&cs.id, from, to],
            )?;
        }
        let blob = serde_json::json!({
            "changeset_id": cs.id,
            "key": cs.key,
            "members": members.iter().map(|m| m.change_id).collect::<Vec<_>>(),
            "edges": edge_ids,
            "reverts": cs.reverts,
        });
        crate::audit::record_tx(tx, audit, None, "changeset.create", Some(&blob))?;
        Ok(Ok(cs))
    })
}

/// Add one open change to an open changeset. Its position is after every
/// current member; edges are added separately.
pub fn add_member(
    db: &ControlDb,
    changeset_id: &str,
    member: Candidate,
    audit: &AuditCtx,
) -> Result<(), Error> {
    let changeset_id = changeset_id.to_string();
    run(db, "add member", "already a member", move |tx| {
        let cs = match open_for_update(tx, &changeset_id)? {
            Ok(cs) => cs,
            Err(e) => return Ok(Err(e)),
        };
        let count: i64 = tx
            .query_one(
                "SELECT count(*) FROM changeset_members WHERE changeset_id = $1",
                &[&changeset_id],
            )?
            .get(0);
        if count as usize >= MAX_MEMBERS {
            return Ok(Err(Error::Invalid(format!(
                "a changeset holds at most {MAX_MEMBERS} members"
            ))));
        }
        let repo_id = match admit(tx, &cs.org_id, member)? {
            Ok(r) => r,
            Err(e) => return Ok(Err(e)),
        };
        if tx
            .query_opt(
                "SELECT 1 FROM changeset_members m JOIN changes c ON c.id = m.change_id \
                 WHERE m.changeset_id = $1 AND c.repo_id = $2",
                &[&changeset_id, &repo_id],
            )?
            .is_some()
        {
            return Ok(Err(Error::Conflict(format!(
                "{}: another change from that repository is already a member",
                member.label
            ))));
        }
        tx.execute(
            "INSERT INTO changeset_members (changeset_id, change_id, position) \
             SELECT $1, $2, COALESCE(MAX(position), 0) + 1 \
             FROM changeset_members WHERE changeset_id = $1",
            &[&changeset_id, &member.change_id],
        )?;
        touch(tx, &changeset_id)?;
        let blob =
            serde_json::json!({ "changeset_id": changeset_id, "change_id": member.change_id });
        crate::audit::record_tx(tx, audit, None, "changeset.member.add", Some(&blob))?;
        Ok(Ok(()))
    })
}

/// Remove a member and every edge that named it. `Ok(false)` when the
/// change was not a member. The last member cannot be removed — a
/// changeset with nothing in it is not a thing; abandon it instead.
pub fn remove_member(
    db: &ControlDb,
    changeset_id: &str,
    change_id: &str,
    audit: &AuditCtx,
) -> Result<bool, Error> {
    let changeset_id = changeset_id.to_string();
    let change_id = change_id.to_string();
    run(db, "remove member", "remove member", move |tx| {
        if let Err(e) = open_for_update(tx, &changeset_id)? {
            return Ok(Err(e));
        }
        let members: Vec<String> = tx
            .query(
                "SELECT change_id FROM changeset_members WHERE changeset_id = $1",
                &[&changeset_id],
            )?
            .iter()
            .map(|r| r.get("change_id"))
            .collect();
        if !members.contains(&change_id) {
            return Ok(Ok(false));
        }
        if members.len() == 1 {
            return Ok(Err(Error::Conflict(
                "a changeset needs at least one member — abandon it instead".into(),
            )));
        }
        tx.execute(
            "DELETE FROM changeset_edges WHERE changeset_id = $1 \
             AND (from_change_id = $2 OR to_change_id = $2)",
            &[&changeset_id, &change_id],
        )?;
        tx.execute(
            "DELETE FROM changeset_members WHERE changeset_id = $1 AND change_id = $2",
            &[&changeset_id, &change_id],
        )?;
        touch(tx, &changeset_id)?;
        let blob = serde_json::json!({ "changeset_id": changeset_id, "change_id": change_id });
        crate::audit::record_tx(tx, audit, None, "changeset.member.remove", Some(&blob))?;
        Ok(Ok(true))
    })
}

/// Replace the edges wholesale. Returns the landing order they imply.
///
/// Wholesale rather than one at a time because acyclicity is a property
/// of the set: adding edges singly would let a client walk through an
/// intermediate cycle, and each step would have to be refused or the
/// invariant abandoned.
pub fn set_edges(
    db: &ControlDb,
    changeset_id: &str,
    edges: &[(Candidate, Candidate)],
    // Every member's id paired with the `repo/change` the caller would
    // recognise it by. The membership check does not need these; the
    // cycle refusal does, and it is the one that names members rather
    // than edge ends. See the comment at the call below.
    member_labels: &[(String, String)],
    audit: &AuditCtx,
) -> Result<Vec<String>, Error> {
    let changeset_id = changeset_id.to_string();
    let edges: Vec<(Candidate, Candidate)> = edges.to_vec();
    let member_labels: Vec<(String, String)> = member_labels.to_vec();
    run(db, "set edges", "set edges", move |tx| {
        if let Err(e) = open_for_update(tx, &changeset_id)? {
            return Ok(Err(e));
        }
        let current = members_tx(tx, &changeset_id)?;
        // Members carry the caller's `repo/change` labels, not their ids.
        //
        // It is tempting to label them by id and rely on the edges' own
        // labels, since the edge ends are what "is not a member" and
        // "cannot land before itself" name. But the **cycle** refusal is
        // different: it names the members a topological sort got stuck
        // on, resolved back through *this* list. Labelled by id, that
        // sentence read `edges form a cycle through 01H…, 01J…` — while
        // `create` answered `edges form a cycle through api/c-api,
        // web/c-web` for the identical mistake. One machine, two
        // sentences for one error, and the harder-to-reach one names
        // nothing the person can see on their screen.
        //
        // Reachable whenever two people edit one changeset at once, which
        // is exactly when a confusing refusal costs the most.
        let cands: Vec<Candidate> = current
            .iter()
            .map(|m| Candidate {
                change_id: &m.change_id,
                label: member_labels
                    .iter()
                    .find(|(id, _)| id == &m.change_id)
                    .map_or(m.change_id.as_str(), |(_, label)| label.as_str()),
            })
            .collect();
        let (edge_ids, order) = match check_edges(&cands, &edges) {
            Ok(x) => x,
            Err(e) => return Ok(Err(e)),
        };
        tx.execute(
            "DELETE FROM changeset_edges WHERE changeset_id = $1",
            &[&changeset_id],
        )?;
        for (from, to) in &edge_ids {
            tx.execute(
                "INSERT INTO changeset_edges (changeset_id, from_change_id, to_change_id) \
                 VALUES ($1, $2, $3)",
                &[&changeset_id, from, to],
            )?;
        }
        touch(tx, &changeset_id)?;
        let blob = serde_json::json!({ "changeset_id": changeset_id, "edges": edge_ids });
        crate::audit::record_tx(tx, audit, None, "changeset.edges", Some(&blob))?;
        Ok(Ok(order))
    })
}

/// Close without landing. Members are released — each change stays open
/// on its own and may be composed again. `Ok(false)` when the changeset
/// was not open.
pub fn abandon(db: &ControlDb, changeset_id: &str, audit: &AuditCtx) -> Result<bool, Error> {
    let changeset_id = changeset_id.to_string();
    run(db, "abandon changeset", "abandon changeset", move |tx| {
        let n = tx.execute(
            "UPDATE changesets SET state = 'abandoned', updated_at = $2 \
             WHERE id = $1 AND state = 'open'",
            &[&changeset_id, &now_ms()],
        )?;
        if n == 0 {
            return Ok(Ok(false));
        }
        release_members(tx, &changeset_id)?;
        let blob = serde_json::json!({ "changeset_id": changeset_id });
        crate::audit::record_tx(tx, audit, None, "changeset.abandon", Some(&blob))?;
        Ok(Ok(true))
    })
}

/// A closed changeset's members are no longer bound: `active` is what
/// the one-open-changeset index reads.
fn release_members(
    tx: &mut postgres::Transaction,
    changeset_id: &str,
) -> Result<(), postgres::Error> {
    tx.execute(
        "UPDATE changeset_members SET active = FALSE WHERE changeset_id = $1",
        &[&changeset_id],
    )?;
    Ok(())
}

fn touch(tx: &mut postgres::Transaction, changeset_id: &str) -> Result<(), postgres::Error> {
    tx.execute(
        "UPDATE changesets SET updated_at = $2 WHERE id = $1",
        &[&changeset_id, &now_ms()],
    )?;
    Ok(())
}

fn members_tx(
    tx: &mut postgres::Transaction,
    changeset_id: &str,
) -> Result<Vec<Member>, postgres::Error> {
    Ok(tx
        .query(
            "SELECT change_id, position FROM changeset_members \
             WHERE changeset_id = $1 ORDER BY position",
            &[&changeset_id],
        )?
        .iter()
        .map(|r| Member {
            change_id: r.get("change_id"),
            position: r.get("position"),
        })
        .collect())
}

pub fn get(db: &ControlDb, org_id: &str, key: &str) -> Result<Option<Changeset>, Error> {
    // A key that could never have been created is "no such changeset",
    // not a database error: a NUL in the path used to reach Postgres,
    // which refused the bind and turned a hostile URL into a 500.
    if !crate::changes::valid_change_key(key) {
        return Ok(None);
    }
    db.lock()
        .query_opt(
            &format!("SELECT {CS_COLS} FROM changesets WHERE org_id = $1 AND key = $2"),
            &[&org_id, &key],
        )
        .map(|r| r.as_ref().map(row_to_changeset))
        .map_err(|e| Error::Db(format!("get changeset: {}", detail(&e))))
}

pub fn by_id(db: &ControlDb, changeset_id: &str) -> Result<Option<Changeset>, Error> {
    db.lock()
        .query_opt(
            &format!("SELECT {CS_COLS} FROM changesets WHERE id = $1"),
            &[&changeset_id],
        )
        .map(|r| r.as_ref().map(row_to_changeset))
        .map_err(|e| Error::Db(format!("changeset by id: {}", detail(&e))))
}

/// Every changeset made to revert `changeset_id`, oldest first. Usually
/// none or one; two when the first revert was abandoned and somebody
/// tried again.
pub fn reverted_by(db: &ControlDb, changeset_id: &str) -> Result<Vec<Changeset>, Error> {
    db.lock()
        .query(
            &format!("SELECT {CS_COLS} FROM changesets WHERE reverts = $1 ORDER BY id"),
            &[&changeset_id],
        )
        .map(|rows| rows.iter().map(row_to_changeset).collect())
        .map_err(|e| Error::Db(format!("changeset reverted by: {}", detail(&e))))
}

/// Newest first, optionally one state only.
pub fn list(
    db: &ControlDb,
    org_id: &str,
    state: Option<&str>,
    limit: i64,
) -> Result<Vec<Changeset>, Error> {
    let limit = limit.clamp(1, 500);
    db.lock()
        .query(
            &format!(
                "SELECT {CS_COLS} FROM changesets WHERE org_id = $1 \
                 AND ($2::TEXT IS NULL OR state = $2) ORDER BY id DESC LIMIT $3"
            ),
            &[&org_id, &state, &limit],
        )
        .map(|rows| rows.iter().map(row_to_changeset).collect())
        .map_err(|e| Error::Db(format!("list changesets: {}", detail(&e))))
}

/// Members in position order.
pub fn members(db: &ControlDb, changeset_id: &str) -> Result<Vec<Member>, Error> {
    db.lock()
        .query(
            "SELECT change_id, position FROM changeset_members \
             WHERE changeset_id = $1 ORDER BY position",
            &[&changeset_id],
        )
        .map(|rows| {
            rows.iter()
                .map(|r| Member {
                    change_id: r.get("change_id"),
                    position: r.get("position"),
                })
                .collect()
        })
        .map_err(|e| Error::Db(format!("changeset members: {}", detail(&e))))
}

pub fn edges(db: &ControlDb, changeset_id: &str) -> Result<Vec<Edge>, Error> {
    db.lock()
        .query(
            "SELECT from_change_id, to_change_id FROM changeset_edges \
             WHERE changeset_id = $1 ORDER BY from_change_id, to_change_id",
            &[&changeset_id],
        )
        .map(|rows| {
            rows.iter()
                .map(|r| Edge {
                    from_change_id: r.get("from_change_id"),
                    to_change_id: r.get("to_change_id"),
                })
                .collect()
        })
        .map_err(|e| Error::Db(format!("changeset edges: {}", detail(&e))))
}

/// The order the lander walks: dependencies first, position breaking
/// ties. Stored edges were checked acyclic when written and a removed
/// member takes its edges with it, so the sort cannot get stuck here.
pub fn landing_order(members: &[Member], edges: &[Edge]) -> Vec<String> {
    let ids: Vec<String> = members.iter().map(|m| m.change_id.clone()).collect();
    let pairs: Vec<(String, String)> = edges
        .iter()
        .map(|e| (e.from_change_id.clone(), e.to_change_id.clone()))
        .collect();
    toposort(&ids, &pairs).unwrap_or_default()
}

/// The open (or landing) changeset a change belongs to, if any — the
/// binding that makes the change land and close only through it.
pub fn binding(db: &ControlDb, change_id: &str) -> Result<Option<Changeset>, Error> {
    db.lock()
        .query_opt(
            "SELECT c.id, c.org_id, c.key, c.title, c.body, c.state, c.created_by, \
             c.created_at, c.updated_at, c.reverts \
             FROM changesets c JOIN changeset_members m ON m.changeset_id = c.id \
             WHERE m.change_id = $1 AND m.active",
            &[&change_id],
        )
        .map(|r| r.as_ref().map(row_to_changeset))
        .map_err(|e| Error::Db(format!("changeset binding: {}", detail(&e))))
}

/// For each of these changes, the changeset that currently holds it.
///
/// This is the *same* question `admit` asks before it refuses a member
/// with 409 — an **active** membership row, whatever state its changeset
/// is in — and it is deliberately one query rather than one per change:
/// the changeset picker renders a page of changes and needs to grey out
/// the ones already spoken for, and a lookup per row is how that page
/// becomes a hundred round trips.
///
/// The rule has to be `active`, not "the changeset is open". Abandoning
/// or landing a changeset releases its members (`release_members`), so a
/// change from a closed changeset may be composed again and must read
/// back as free here — otherwise the picker would grey out a change the
/// API would happily accept.
///
/// A change with no active membership is simply absent from the map, and
/// absent is exactly `changeset: null`.
pub fn holding_changeset_keys(
    db: &ControlDb,
    change_ids: &[String],
) -> Result<std::collections::HashMap<String, String>, Error> {
    if change_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let ids = change_ids.to_vec();
    db.lock()
        .query(
            "SELECT m.change_id, c.key FROM changeset_members m \
             JOIN changesets c ON c.id = m.changeset_id \
             WHERE m.change_id = ANY($1) AND m.active",
            &[&ids],
        )
        .map(|rows| {
            rows.iter()
                .map(|r| (r.get("change_id"), r.get("key")))
                .collect()
        })
        .map_err(|e| Error::Db(format!("holding changesets: {}", detail(&e))))
}

/// How far one member of a landing has got.
///
/// The plan is a sequence of these, in landing order, and the sequence
/// is also the record: a driver that picks the landing up after a crash
/// reads them back and trusts the ones that say `Done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    /// Not attempted, or attempted without a recorded answer — the driver
    /// re-reads the store to find out which.
    Pending,
    /// The target ref is at `new`, by this landing's CAS.
    Done,
    /// It was `Done`, and the unwind has put a commit restoring the old
    /// tree on top of it. `note` names the revert commit.
    Reverted,
    /// The CAS could not be made, and `note` says why in the words the
    /// member's author reads.
    Failed,
}

/// One member's part of a landing plan: the ref it moves, the tip it was
/// expected at when the plan was made, the tip it moves to, and how far
/// that has got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub change_id: String,
    pub repo_id: String,
    /// `repo/change`, so a note can name the member without a lookup.
    pub label: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// `None` when the target did not exist at pre-flight.
    pub old: Option<String>,
    pub new: String,
    pub state: StepState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The commit point of one landing attempt on a changeset, and its
/// progress. See the module doc and migration 0046.
#[derive(Debug, Clone)]
pub struct Landing {
    pub id: String,
    pub changeset_id: String,
    /// The `land` job driving it. Re-pointed by [`adopt_landing_job`]
    /// when a reaper hands a stranded landing to a fresh job.
    pub job_id: Option<String>,
    /// How many jobs have driven it so far.
    pub attempt: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// `landed` or `failed` once `finished_at` is set; the two move
    /// together, by constraint.
    pub outcome: Option<String>,
    pub progress: Vec<Step>,
}

const LANDING_COLS: &str =
    "id, changeset_id, job_id, attempt, started_at, finished_at, outcome, progress";

fn row_to_landing(r: &postgres::Row) -> Result<Landing, Error> {
    let raw: String = r.get("progress");
    let progress = serde_json::from_str(&raw)
        .map_err(|e| Error::Db(format!("changeset landing progress: {e}")))?;
    Ok(Landing {
        id: r.get("id"),
        changeset_id: r.get("changeset_id"),
        job_id: r.get("job_id"),
        attempt: r.get("attempt"),
        started_at: r.get("started_at"),
        finished_at: r.get("finished_at"),
        outcome: r.get("outcome"),
        progress,
    })
}

fn encode_progress(steps: &[Step]) -> Result<String, Error> {
    serde_json::to_string(steps).map_err(|e| Error::Db(format!("encode progress: {e}")))
}

/// The commit point. In one transaction: the changeset moves `open` →
/// `landing`, every member change moves `open` → `landing` pointed at
/// `job_id`, and the plan is written as a [`Landing`] with id
/// `landing_id`. From this row on the landing will finish.
///
/// The id is the caller's because the job that drives the landing has to
/// carry it in its payload, and `jobs::create` mints the job's id first;
/// the two rows name each other, so one of them has to be named before
/// it exists. Refused — `Ok(Err(Conflict))`, nothing written — when the
/// changeset is no longer open or any member has moved since the plan
/// was made: an approval revoked in the window is the caller's to
/// re-check, but a member that landed by inclusion or was abandoned is a
/// plan that no longer describes the world.
pub fn begin_landing(
    db: &ControlDb,
    changeset_id: &str,
    landing_id: &str,
    job_id: &str,
    plan: &[Step],
    audit: &AuditCtx,
) -> Result<Landing, Error> {
    if plan.is_empty() {
        return Err(Error::Invalid("changeset has no members".into()));
    }
    let progress = encode_progress(plan)?;
    let changeset_id = changeset_id.to_string();
    let landing_id = landing_id.to_string();
    let job_id = job_id.to_string();
    let plan: Vec<Step> = plan.to_vec();
    run(
        db,
        "begin changeset landing",
        "changeset is already landing",
        move |tx| {
            let cs = match open_for_update(tx, &changeset_id)? {
                Ok(cs) => cs,
                Err(e) => return Ok(Err(e)),
            };
            // Every read that can refuse, before any write.
            for step in &plan {
                let Some(row) = tx.query_opt(
                    "SELECT state FROM changes WHERE id = $1 FOR UPDATE",
                    &[&step.change_id],
                )?
                else {
                    return Ok(Err(Error::Conflict(format!(
                        "{} is not a change",
                        step.label
                    ))));
                };
                let state: String = row.get("state");
                if state != "open" {
                    return Ok(Err(Error::Conflict(format!("{} is {state}", step.label))));
                }
            }
            let now = now_ms();
            tx.execute(
                "UPDATE changesets SET state = 'landing', updated_at = $2 WHERE id = $1",
                &[&cs.id, &now],
            )?;
            for step in &plan {
                tx.execute(
                    "UPDATE changes SET state = 'landing', land_job_id = $2, \
                     land_verdict = NULL, updated_at = $3 WHERE id = $1",
                    &[&step.change_id, &job_id, &now],
                )?;
            }
            tx.execute(
                &format!(
                    "INSERT INTO changeset_landings ({LANDING_COLS}) \
                     VALUES ($1, $2, $3, 1, $4, NULL, NULL, $5)"
                ),
                &[&landing_id, &cs.id, &job_id, &now, &progress],
            )?;
            let blob = serde_json::json!({
                "changeset_id": cs.id,
                "key": cs.key,
                "landing_id": landing_id,
                "job": job_id,
                "plan": plan.iter().map(|s| serde_json::json!({
                    "change_id": s.change_id,
                    "ref": s.ref_name,
                    "old": s.old,
                    "new": s.new,
                })).collect::<Vec<_>>(),
            });
            crate::audit::record_tx(tx, audit, None, "changeset.land", Some(&blob))?;
            Ok(Ok(Landing {
                id: landing_id,
                changeset_id: cs.id,
                job_id: Some(job_id),
                attempt: 1,
                started_at: now,
                finished_at: None,
                outcome: None,
                progress: plan,
            }))
        },
    )
}

pub fn landing(db: &ControlDb, landing_id: &str) -> Result<Option<Landing>, Error> {
    db.lock()
        .query_opt(
            &format!("SELECT {LANDING_COLS} FROM changeset_landings WHERE id = $1"),
            &[&landing_id],
        )
        .map_err(|e| Error::Db(format!("changeset landing: {}", detail(&e))))?
        .as_ref()
        .map(row_to_landing)
        .transpose()
}

/// The most recent landing attempt on a changeset, finished or not —
/// what a reader of the changeset sees as "what happened when it landed".
pub fn latest_landing(db: &ControlDb, changeset_id: &str) -> Result<Option<Landing>, Error> {
    db.lock()
        .query_opt(
            &format!(
                "SELECT {LANDING_COLS} FROM changeset_landings WHERE changeset_id = $1 \
                 ORDER BY started_at DESC, id DESC LIMIT 1"
            ),
            &[&changeset_id],
        )
        .map_err(|e| Error::Db(format!("latest changeset landing: {}", detail(&e))))?
        .as_ref()
        .map(row_to_landing)
        .transpose()
}

/// Write the plan back with its steps' states as they now stand.
/// Guarded on the landing being unfinished: a finished landing's record
/// is final, and a late writer — a job that lost its lease and kept
/// running — must not rewrite it.
pub fn record_progress(db: &ControlDb, landing_id: &str, progress: &[Step]) -> Result<bool, Error> {
    let encoded = encode_progress(progress)?;
    db.lock()
        .execute(
            "UPDATE changeset_landings SET progress = $2 \
             WHERE id = $1 AND finished_at IS NULL",
            &[&landing_id, &encoded],
        )
        .map(|n| n > 0)
        .map_err(|e| Error::Db(format!("record landing progress: {}", detail(&e))))
}

/// Point an unfinished landing at a fresh job, counting the attempt.
/// A CAS on `job_id`, like `changes::adopt_land_job` and for the same
/// reason: two reapers can find the same stranded landing in the same
/// tick, and exactly one of them may drive it.
///
/// The member changes follow: each still pointed at the old job is
/// pointed at the new one, so `changes.land_job_id` keeps naming the job
/// that is really driving the change.
pub fn adopt_landing_job(
    db: &ControlDb,
    landing_id: &str,
    expect: Option<&str>,
    new_job_id: &str,
) -> Result<bool, Error> {
    let landing_id = landing_id.to_string();
    let expect = expect.map(str::to_string);
    let new_job_id = new_job_id.to_string();
    run(db, "adopt landing job", "adopt landing job", move |tx| {
        let n = tx.execute(
            "UPDATE changeset_landings SET job_id = $3, attempt = attempt + 1 \
             WHERE id = $1 AND finished_at IS NULL AND job_id IS NOT DISTINCT FROM $2",
            &[&landing_id, &expect, &new_job_id],
        )?;
        if n == 0 {
            return Ok(Ok(false));
        }
        tx.execute(
            "UPDATE changes SET land_job_id = $3 \
             WHERE state = 'landing' AND land_job_id IS NOT DISTINCT FROM $2 \
               AND id IN (SELECT m.change_id FROM changeset_members m \
                          JOIN changeset_landings l ON l.changeset_id = m.changeset_id \
                          WHERE l.id = $1 AND m.active)",
            &[&landing_id, &expect, &new_job_id],
        )?;
        Ok(Ok(true))
    })
}

/// Unfinished landings with no live job driving them, oldest first.
///
/// The same rule as `changes::stranded_landings`: a job that is `done`
/// or `failed` is not driving anything, a job state nobody has thought
/// of yet reads as "still driving" and is left alone, and
/// `idle_before_ms` is the grace that keeps a job between two statements
/// from being double-driven.
pub fn stranded_landings(
    db: &ControlDb,
    idle_before_ms: i64,
    limit: i64,
) -> Result<Vec<Landing>, Error> {
    let cols = LANDING_COLS
        .split(", ")
        .map(|c| format!("l.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let limit = limit.clamp(1, 1000);
    db.lock()
        .query(
            &format!(
                "SELECT {cols} FROM changeset_landings l LEFT JOIN jobs j ON j.id = l.job_id \
                 WHERE l.finished_at IS NULL \
                   AND (j.id IS NULL OR j.state IN ('done', 'failed')) \
                   AND COALESCE(j.updated_at, l.started_at) < $1 \
                 ORDER BY COALESCE(j.updated_at, l.started_at), l.id LIMIT $2"
            ),
            &[&idle_before_ms, &limit],
        )
        .map_err(|e| Error::Db(format!("stranded changeset landings: {}", detail(&e))))?
        .iter()
        .map(row_to_landing)
        .collect()
}

/// What one member is left as when its landing finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberOutcome {
    /// The change landed: `landed`, at this commit, with this verdict.
    Landed { commit: String, verdict: String },
    /// The change goes back to `open` with this verdict for its author.
    Reopened { verdict: String },
}

/// The end of a landing, in one transaction: the landing row is closed
/// with `outcome` and its final progress, the changeset moves `landing`
/// → `landed` or `failed`, its members are released from the binding,
/// and each member change is left as `members` says — `landed` with its
/// commit, or `open` again with the sentence that says what happened.
///
/// `Ok(false)` when the landing was already finished or the changeset
/// had already left `landing`: another driver got there first and its
/// record stands. Nothing is written in that case.
pub fn finish_landing(
    db: &ControlDb,
    landing_id: &str,
    outcome: &str,
    progress: &[Step],
    members: &[(String, MemberOutcome)],
    audit: &AuditCtx,
) -> Result<bool, Error> {
    debug_assert!(outcome == "landed" || outcome == "failed");
    let encoded = encode_progress(progress)?;
    let landing_id = landing_id.to_string();
    let outcome = outcome.to_string();
    let members: Vec<(String, MemberOutcome)> = members.to_vec();
    run(
        db,
        "finish changeset landing",
        "finish changeset landing",
        move |tx| {
            let now = now_ms();
            let Some(row) = tx.query_opt(
                "SELECT changeset_id FROM changeset_landings WHERE id = $1 \
             AND finished_at IS NULL FOR UPDATE",
                &[&landing_id],
            )?
            else {
                return Ok(Ok(false));
            };
            let changeset_id: String = row.get("changeset_id");
            let moved = tx.execute(
                "UPDATE changesets SET state = $2, updated_at = $3 \
             WHERE id = $1 AND state = 'landing'",
                &[&changeset_id, &outcome, &now],
            )?;
            if moved == 0 {
                return Ok(Ok(false));
            }
            tx.execute(
                "UPDATE changeset_landings SET finished_at = $2, outcome = $3, progress = $4 \
             WHERE id = $1",
                &[&landing_id, &now, &outcome, &encoded],
            )?;
            release_members(tx, &changeset_id)?;
            for (change_id, m) in &members {
                match m {
                    MemberOutcome::Landed { commit, verdict } => {
                        tx.execute(
                            "UPDATE changes SET state = 'landed', land_verdict = $2, \
                         landed_commit = $3, updated_at = $4, landed_at = $4 \
                         WHERE id = $1 AND state = 'landing'",
                            &[change_id, verdict, commit, &now],
                        )?;
                    }
                    MemberOutcome::Reopened { verdict } => {
                        tx.execute(
                            "UPDATE changes SET state = 'open', land_verdict = $2, \
                         land_job_id = NULL, updated_at = $3 \
                         WHERE id = $1 AND state = 'landing'",
                            &[change_id, verdict, &now],
                        )?;
                    }
                }
            }
            let blob = serde_json::json!({
                "changeset_id": changeset_id,
                "landing_id": landing_id,
                "outcome": outcome,
                "members": members.iter().map(|(id, m)| serde_json::json!({
                    "change_id": id,
                    "state": match m {
                        MemberOutcome::Landed { .. } => "landed",
                        MemberOutcome::Reopened { .. } => "open",
                    },
                })).collect::<Vec<_>>(),
            });
            let action = if outcome == "landed" {
                "changeset.landed"
            } else {
                "changeset.failed"
            };
            crate::audit::record_tx(tx, audit, None, action, Some(&blob))?;
            Ok(Ok(true))
        },
    )
}

/// The changeset's own check gate: what its composed runs say about the
/// composition it is at right now.
///
/// [`crate::changes::LandGate`] rather than a fourth vocabulary, because
/// it is the same question with the same three answers and the changeset
/// verdict folds the two together — see `review::changeset::compose`.
///
/// Three rules, and the order between them is the same one `land_gate`
/// uses for a single change: a refusal beats a wait, because waiting on
/// a build cannot rescue something that has already been told no.
///
/// * any `failing`, `cancelled` or `skipped` → `Blocked`. `cancelled`
///   and `skipped` block here where a *non-required* check on a single
///   change would not, and deliberately: a composed run exists only
///   because the file asked for one, so there is no "somebody's optional
///   build" case to be gentle about. Nothing checked the combination,
///   and the combination is the only thing this gate is for.
/// * else any `queued` or `running` → `Waiting`, naming them.
/// * else `Ready` — including a changeset with **no rows at all**, which
///   is the ordinary case: most changesets have no `on: changeset` file
///   anywhere in their members, and a gate that waited for a verdict
///   nothing was ever going to produce would make composed CI mandatory
///   by accident.
///
/// The repository is named in every refusal because a changeset spans
/// several and "which one do I go and look at" is the reader's first
/// question. A repository that has since been deleted names its id,
/// which is worse than a name and much better than the sentence losing
/// the member.
pub fn composed_gate(
    db: &ControlDb,
    changeset_id: &str,
    composition: &str,
) -> Result<crate::changes::LandGate, Error> {
    use crate::changes::LandGate;
    let rows = crate::changeset_checks::for_composition(db, changeset_id, composition)
        .map_err(Error::Db)?;
    let mut blocked: Vec<String> = Vec::new();
    let mut waiting: Vec<String> = Vec::new();
    for c in &rows {
        if c.state == "passing" {
            continue;
        }
        let repo = crate::registry::repo_by_id_any(db, &c.repo_id)
            .map_err(Error::Db)?
            .map_or_else(|| c.repo_id.clone(), |r| r.name);
        match c.state.as_str() {
            "failing" | "cancelled" | "skipped" => {
                blocked.push(format!(
                    "composed check {} in {repo} is {}",
                    c.name, c.state
                ));
            }
            // `queued`, `running`, and anything the column ever grows:
            // a verdict is still coming, which is a wait and not a pass.
            //
            // Named `repo: check`, the spelling the dashboard gives a
            // composed row, and not the bare job name: two members whose
            // workflows share a job name — `ci / test` is most of them —
            // used to wait on two identical strings, and a reader could
            // not tell which repository's build they were waiting for.
            _ => waiting.push(format!("{repo}: {}", c.name)),
        }
    }
    if !blocked.is_empty() {
        blocked.sort();
        return Ok(LandGate::Blocked {
            reason: blocked.join("; "),
        });
    }
    if !waiting.is_empty() {
        waiting.sort();
        return Ok(LandGate::Waiting { on: waiting });
    }
    Ok(LandGate::Ready)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes;
    use crate::registry::{self, NewRepo, RepoKind};
    use crate::users;

    struct World {
        db: ControlDb,
        org: String,
        ctx: AuditCtx,
    }

    fn world(hint: &str) -> World {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let alice = users::create(&db, "alice@example.com", "Alice", None).unwrap();
        let ctx = AuditCtx {
            principal: format!("user:{}", alice.id),
            user_id: Some(alice.id.clone()),
            org_id: org.id.clone(),
        };
        World {
            db,
            org: org.id,
            ctx,
        }
    }

    fn repo(w: &World, org: &str, name: &str) -> String {
        registry::create_repo(
            &w.db,
            org,
            &NewRepo {
                name,
                kind: RepoKind::Native,
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

    /// One open change in a fresh repository named `name`; the change key
    /// is `I<name>`.
    fn change(w: &World, name: &str) -> String {
        change_in(w, &w.org, name)
    }

    fn change_in(w: &World, org: &str, name: &str) -> String {
        let repo_id = repo(w, org, name);
        let key = format!("I{name}");
        changes::create_or_update(
            &w.db,
            org,
            &repo_id,
            &key,
            name,
            "main",
            &format!("{:0>40}", name.len()),
            None,
            &format!("{name}\n\nChange-Id: {key}\n"),
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap()
        .0
        .id
    }

    fn cand<'a>(id: &'a str, label: &'a str) -> Candidate<'a> {
        Candidate {
            change_id: id,
            label,
        }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn the_landing_order_follows_edges_and_breaks_ties_by_position() {
        // No edges: position order.
        assert_eq!(
            toposort(&ids(&["a", "b", "c"]), &[]).unwrap(),
            ids(&["a", "b", "c"])
        );
        // c must land before a; b keeps its place among the free ones.
        assert_eq!(
            toposort(&ids(&["a", "b", "c"]), &pairs(&[("c", "a")])).unwrap(),
            ids(&["b", "c", "a"])
        );
        // A chain.
        assert_eq!(
            toposort(&ids(&["a", "b", "c"]), &pairs(&[("c", "b"), ("b", "a")])).unwrap(),
            ids(&["c", "b", "a"])
        );
        // A cycle leaves its members, and everything downstream of them,
        // standing — that is what the refusal names.
        assert_eq!(
            toposort(
                &ids(&["a", "b", "c", "d"]),
                &pairs(&[("a", "b"), ("b", "a"), ("b", "c")])
            )
            .unwrap_err(),
            ids(&["a", "b", "c"])
        );
    }

    #[test]
    fn edges_are_checked_against_the_member_list() {
        let m = [
            cand("1", "api/I1"),
            cand("2", "web/I2"),
            cand("3", "cli/I3"),
        ];
        let err = check_edges(&m, &[(cand("1", "api/I1"), cand("9", "docs/I9"))]).unwrap_err();
        assert_eq!(
            err,
            Error::Invalid("docs/I9 is not a member of this changeset".into())
        );
        let err = check_edges(&m, &[(cand("1", "api/I1"), cand("1", "api/I1"))]).unwrap_err();
        assert_eq!(
            err,
            Error::Invalid("api/I1 cannot land before itself".into())
        );
        let err = check_edges(
            &m,
            &[
                (cand("1", "api/I1"), cand("2", "web/I2")),
                (cand("2", "web/I2"), cand("3", "cli/I3")),
                (cand("3", "cli/I3"), cand("1", "api/I1")),
            ],
        )
        .unwrap_err();
        assert_eq!(
            err,
            Error::Invalid("edges form a cycle through api/I1, web/I2, cli/I3".into())
        );
        // A repeated edge is one edge.
        let (edges, order) = check_edges(
            &m,
            &[
                (cand("2", "web/I2"), cand("1", "api/I1")),
                (cand("2", "web/I2"), cand("1", "api/I1")),
            ],
        )
        .unwrap();
        assert_eq!(edges, pairs(&[("2", "1")]));
        assert_eq!(order, ids(&["2", "1", "3"]));
        // The same answer from the stored shape.
        let members = [
            Member {
                change_id: "1".into(),
                position: 1,
            },
            Member {
                change_id: "2".into(),
                position: 2,
            },
            Member {
                change_id: "3".into(),
                position: 3,
            },
        ];
        let stored = [Edge {
            from_change_id: "2".into(),
            to_change_id: "1".into(),
        }];
        assert_eq!(landing_order(&members, &stored), ids(&["2", "1", "3"]));
    }

    #[test]
    fn a_changeset_is_refused_before_anything_is_written() {
        let w = world("changesets_refused");
        let api = change(&w, "api");
        let web = change(&w, "web");
        let members = [cand(&api, "api/Iapi"), cand(&web, "web/Iweb")];
        let mk = |key: &str, title: &str, m: &[Candidate], e: &[(Candidate, Candidate)]| {
            create(&w.db, &w.org, key, title, "", None, m, e, None, &w.ctx)
        };
        let invalid = |e: Error, what: &str| {
            assert!(
                matches!(e, Error::Invalid(ref s) if s.contains(what)),
                "{e:?} is not Invalid mentioning {what:?}"
            )
        };
        let conflict = |e: Error, what: &str| {
            assert!(
                matches!(e, Error::Conflict(ref s) if s.contains(what)),
                "{e:?} is not Conflict mentioning {what:?}"
            )
        };

        invalid(
            mk("has space", "t", &members, &[]).unwrap_err(),
            "invalid changeset key",
        );
        invalid(
            mk("Ics", "   ", &members, &[]).unwrap_err(),
            "title is required",
        );
        invalid(mk("Ics", "t", &[], &[]).unwrap_err(), "between 1 and 16");
        let many: Vec<Candidate> = (0..17).map(|_| members[0]).collect();
        invalid(mk("Ics", "t", &many, &[]).unwrap_err(), "between 1 and 16");
        invalid(
            mk("Ics", "t", &[members[0], members[0]], &[]).unwrap_err(),
            "api/Iapi is listed twice",
        );
        invalid(
            mk(
                "Ics",
                "t",
                &members,
                &[(members[0], cand("nope", "cli/Icli"))],
            )
            .unwrap_err(),
            "cli/Icli is not a member",
        );
        invalid(
            mk(
                "Ics",
                "t",
                &members,
                &[(members[0], members[1]), (members[1], members[0])],
            )
            .unwrap_err(),
            "cycle through api/Iapi, web/Iweb",
        );
        // Refused inside the transaction, by what the database says.
        conflict(
            mk(
                "Ics",
                "t",
                &[members[0], cand("01H0000000000000000000000X", "web/Ighost")],
                &[],
            )
            .unwrap_err(),
            "web/Ighost is not a change",
        );
        let other_org = registry::create_org(&w.db, "rival").unwrap().id;
        let theirs = change_in(&w, &other_org, "theirs");
        invalid(
            mk(
                "Ics",
                "t",
                &[members[0], cand(&theirs, "theirs/Itheirs")],
                &[],
            )
            .unwrap_err(),
            "theirs/Itheirs is in another organization",
        );
        let api2_repo = registry::repo_by_id_any(
            &w.db,
            &changes::by_id(&w.db, &api).unwrap().unwrap().repo_id,
        )
        .unwrap()
        .unwrap();
        let (api2, _, _) = changes::create_or_update(
            &w.db,
            &w.org,
            &api2_repo.id,
            "Iapi2",
            "second api change",
            "main",
            &"b".repeat(40),
            None,
            "second api change\n\nChange-Id: Iapi2\n",
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        invalid(
            mk("Ics", "t", &[members[0], cand(&api2.id, "api/Iapi2")], &[]).unwrap_err(),
            "api/Iapi and api/Iapi2 are in the same repository",
        );
        changes::abandon(&w.db, &api2.id, &w.ctx).unwrap();
        conflict(
            mk("Ics", "t", &[members[1], cand(&api2.id, "api/Iapi2")], &[]).unwrap_err(),
            "api/Iapi2 is abandoned",
        );

        // Nothing above left a row behind.
        assert!(list(&w.db, &w.org, None, 50).unwrap().is_empty());
        assert!(binding(&w.db, &api).unwrap().is_none());

        // Now it exists, and the key is taken.
        let cs = mk(
            "Ics",
            "  Split the config  ",
            &members,
            &[(members[1], members[0])],
        )
        .unwrap();
        assert_eq!(cs.title, "Split the config");
        assert_eq!(cs.state, "open");
        let docs = change(&w, "docs");
        conflict(
            mk("Ics", "again", &[cand(&docs, "docs/Idocs")], &[]).unwrap_err(),
            "changeset Ics already exists",
        );
        conflict(
            mk("Iother", "t", &[members[0]], &[]).unwrap_err(),
            "api/Iapi is already in changeset Ics",
        );
        assert_eq!(binding(&w.db, &api).unwrap().unwrap().id, cs.id);
        let ms = super::members(&w.db, &cs.id).unwrap();
        assert_eq!(
            ms.iter()
                .map(|m| (m.change_id.as_str(), m.position))
                .collect::<Vec<_>>(),
            vec![(api.as_str(), 1), (web.as_str(), 2)]
        );
        let es = edges(&w.db, &cs.id).unwrap();
        assert_eq!(es.len(), 1);
        assert_eq!(landing_order(&ms, &es), vec![web.clone(), api.clone()]);
        assert_eq!(get(&w.db, &w.org, "Ics").unwrap().unwrap().id, cs.id);
        assert!(get(&w.db, &w.org, "Inope").unwrap().is_none());
        assert_eq!(list(&w.db, &w.org, Some("open"), 50).unwrap().len(), 1);
        assert!(list(&w.db, &w.org, Some("landed"), 50).unwrap().is_empty());
    }

    /// The picker's "already spoken for" field has to be the same
    /// question `admit` refuses on, or the screen and the API disagree:
    /// a change greyed out that could be composed, or offered and then
    /// refused with a 409.
    #[test]
    fn the_holding_changeset_is_the_one_a_second_composition_would_collide_with() {
        let w = world("changesets_holding");
        let api = change(&w, "api");
        let web = change(&w, "web");
        let cli = change(&w, "cli");
        let all = ids(&[&api, &web, &cli]);

        // Nothing composed: every change is free.
        assert!(holding_changeset_keys(&w.db, &all).unwrap().is_empty());
        // And an empty ask is an empty answer, not every row in the table.
        assert!(holding_changeset_keys(&w.db, &[]).unwrap().is_empty());

        let cs = create(
            &w.db,
            &w.org,
            "Ics",
            "t",
            "",
            w.ctx.user_id.as_deref(),
            &[cand(&api, "api/Iapi"), cand(&web, "web/Iweb")],
            &[],
            None,
            &w.ctx,
        )
        .unwrap();
        let held = holding_changeset_keys(&w.db, &all).unwrap();
        assert_eq!(held.get(&api).map(String::as_str), Some("Ics"));
        assert_eq!(held.get(&web).map(String::as_str), Some("Ics"));
        assert_eq!(held.get(&cli), None, "cli is in nothing");
        // Exactly the changes asked about, and no others.
        assert_eq!(
            holding_changeset_keys(&w.db, &ids(&[&cli])).unwrap().len(),
            0
        );

        // Removed from the changeset: free again, and it is the *active*
        // row that says so — the membership row survives the removal.
        remove_member(&w.db, &cs.id, &web, &w.ctx).unwrap();
        assert_eq!(holding_changeset_keys(&w.db, &all).unwrap().get(&web), None);

        // Abandoning releases the rest, exactly as `admit` would then
        // let them be composed again.
        abandon(&w.db, &cs.id, &w.ctx).unwrap();
        assert!(holding_changeset_keys(&w.db, &all).unwrap().is_empty());
        assert!(binding(&w.db, &api).unwrap().is_none());
    }

    #[test]
    fn membership_and_edges_move_only_while_open() {
        let w = world("changesets_compose");
        let api = change(&w, "api");
        let web = change(&w, "web");
        let cli = change(&w, "cli");
        let cs = create(
            &w.db,
            &w.org,
            "Ics",
            "t",
            "",
            w.ctx.user_id.as_deref(),
            &[cand(&api, "api/Iapi")],
            &[],
            None,
            &w.ctx,
        )
        .unwrap();
        assert_eq!(cs.created_by, w.ctx.user_id);

        // The only member cannot leave; a stranger is not a member.
        assert_eq!(
            remove_member(&w.db, &cs.id, &api, &w.ctx).unwrap_err(),
            Error::Conflict("a changeset needs at least one member — abandon it instead".into())
        );
        assert!(!remove_member(&w.db, &cs.id, &web, &w.ctx).unwrap());

        add_member(&w.db, &cs.id, cand(&web, "web/Iweb"), &w.ctx).unwrap();
        add_member(&w.db, &cs.id, cand(&cli, "cli/Icli"), &w.ctx).unwrap();
        assert_eq!(
            add_member(&w.db, &cs.id, cand(&cli, "cli/Icli"), &w.ctx).unwrap_err(),
            Error::Conflict("cli/Icli is already in changeset Ics".into())
        );
        // A second change to a member's repository is refused.
        let api_repo = changes::by_id(&w.db, &api).unwrap().unwrap().repo_id;
        let (api2, _, _) = changes::create_or_update(
            &w.db,
            &w.org,
            &api_repo,
            "Iapi2",
            "t",
            "main",
            &"c".repeat(40),
            None,
            "t",
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            add_member(&w.db, &cs.id, cand(&api2.id, "api/Iapi2"), &w.ctx).unwrap_err(),
            Error::Conflict(
                "api/Iapi2: another change from that repository is already a member".into()
            )
        );
        let ms = members(&w.db, &cs.id).unwrap();
        assert_eq!(
            ms.iter().map(|m| m.position).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        // Edges: cli before web before api.
        let labels: Vec<(String, String)> = vec![
            (api.clone(), "api/Iapi".into()),
            (web.clone(), "web/Iweb".into()),
            (cli.clone(), "cli/Icli".into()),
        ];
        let order = set_edges(
            &w.db,
            &cs.id,
            &[
                (cand(&cli, "cli/Icli"), cand(&web, "web/Iweb")),
                (cand(&web, "web/Iweb"), cand(&api, "api/Iapi")),
            ],
            &labels,
            &w.ctx,
        )
        .unwrap();
        assert_eq!(order, vec![cli.clone(), web.clone(), api.clone()]);
        assert_eq!(
            set_edges(
                &w.db,
                &cs.id,
                &[(cand(&api, "api/Iapi"), cand("zzz", "docs/Idocs"))],
                &labels,
                &w.ctx
            )
            .unwrap_err(),
            Error::Invalid("docs/Idocs is not a member of this changeset".into())
        );

        // A cycle through `set_edges` must name members the way `create`
        // does. It used to answer with raw change ids here and
        // `repo/change` there — one machine, two sentences for the same
        // mistake, and the id form names nothing on the person's screen.
        // Two people editing one changeset at once is how you reach it.
        assert_eq!(
            set_edges(
                &w.db,
                &cs.id,
                &[
                    (cand(&api, "api/Iapi"), cand(&web, "web/Iweb")),
                    (cand(&web, "web/Iweb"), cand(&api, "api/Iapi")),
                ],
                &labels,
                &w.ctx
            )
            .unwrap_err(),
            Error::Invalid("edges form a cycle through api/Iapi, web/Iweb".into())
        );
        // A refused replacement leaves the old edges standing.
        assert_eq!(edges(&w.db, &cs.id).unwrap().len(), 2);

        // Removing the middle member takes both of its edges with it.
        assert!(remove_member(&w.db, &cs.id, &web, &w.ctx).unwrap());
        assert!(edges(&w.db, &cs.id).unwrap().is_empty());
        assert!(binding(&w.db, &web).unwrap().is_none());
        assert_eq!(
            landing_order(&members(&w.db, &cs.id).unwrap(), &[]),
            vec![api.clone(), cli.clone()]
        );

        // Sixteen is the ceiling.
        for i in 3..=16 {
            let c = change(&w, &format!("r{i}"));
            add_member(&w.db, &cs.id, cand(&c, "r/I"), &w.ctx).unwrap();
        }
        let one_more = change(&w, "r17");
        assert_eq!(
            add_member(&w.db, &cs.id, cand(&one_more, "r17/Ir17"), &w.ctx).unwrap_err(),
            Error::Invalid("a changeset holds at most 16 members".into())
        );

        // Abandon releases every member; the second abandon lost.
        assert!(abandon(&w.db, &cs.id, &w.ctx).unwrap());
        assert!(!abandon(&w.db, &cs.id, &w.ctx).unwrap());
        assert_eq!(
            get(&w.db, &w.org, "Ics").unwrap().unwrap().state,
            "abandoned"
        );
        assert!(binding(&w.db, &api).unwrap().is_none());
        let closed = Error::Conflict("changeset is abandoned".into());
        assert_eq!(
            add_member(&w.db, &cs.id, cand(&web, "web/Iweb"), &w.ctx).unwrap_err(),
            closed
        );
        assert_eq!(
            remove_member(&w.db, &cs.id, &api, &w.ctx).unwrap_err(),
            closed
        );
        assert_eq!(
            set_edges(&w.db, &cs.id, &[], &[], &w.ctx).unwrap_err(),
            closed
        );
        assert_eq!(
            add_member(
                &w.db,
                "01H000000000000000000000NO",
                cand(&web, "web/Iweb"),
                &w.ctx
            )
            .unwrap_err(),
            Error::Conflict("no such changeset".into())
        );
        // …and the released change composes again.
        let again = create(
            &w.db,
            &w.org,
            "Ics2",
            "t",
            "",
            None,
            &[cand(&api, "api/Iapi")],
            &[],
            None,
            &w.ctx,
        )
        .unwrap();
        assert_eq!(binding(&w.db, &api).unwrap().unwrap().id, again.id);
        assert_eq!(list(&w.db, &w.org, None, 50).unwrap()[0].id, again.id);

        // The trail names every step.
        let trail = crate::audit::query(
            &w.db,
            &w.org,
            &crate::audit::AuditQuery {
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
        for action in [
            "changeset.create",
            "changeset.member.add",
            "changeset.member.remove",
            "changeset.edges",
            "changeset.abandon",
        ] {
            assert!(trail.iter().any(|e| e.action == action), "{action} missing");
        }
    }

    /// The API renders a `Db` error through `Display` and the other two
    /// through their own arms; the words must be the same either way.
    fn step(change_id: &str, repo_id: &str, label: &str, old: Option<&str>) -> Step {
        Step {
            change_id: change_id.to_string(),
            repo_id: repo_id.to_string(),
            label: label.to_string(),
            ref_name: "refs/heads/main".into(),
            old: old.map(str::to_string),
            new: "f".repeat(40),
            state: StepState::Pending,
            note: None,
        }
    }

    /// The commit point and everything after it, as the lander and the
    /// reaper use them: one transaction turns the changeset and every
    /// member `landing`; the record is written back step by step but
    /// never after it is final; a stranded landing is one whose job has
    /// stopped, and adopting it is a CAS that also re-points the members;
    /// finishing leaves each member as told and refuses to happen twice.
    #[test]
    fn a_landing_is_recorded_before_it_begins_and_finished_exactly_once() {
        let w = world("changesets_landing");
        let api = change(&w, "api");
        let web = change(&w, "web");
        let api_repo = changes::by_id(&w.db, &api).unwrap().unwrap().repo_id;
        let web_repo = changes::by_id(&w.db, &web).unwrap().unwrap().repo_id;
        let cs = create(
            &w.db,
            &w.org,
            "Iland",
            "t",
            "",
            None,
            &[cand(&api, "api/Iapi"), cand(&web, "web/Iweb")],
            &[],
            None,
            &w.ctx,
        )
        .unwrap();
        let plan = vec![
            step(&api, &api_repo, "api/Iapi", Some(&"a".repeat(40))),
            step(&web, &web_repo, "web/Iweb", None),
        ];

        // Refused before anything is written: an empty plan, and a member
        // that is no longer open.
        assert_eq!(
            begin_landing(&w.db, &cs.id, "L0", "J0", &[], &w.ctx).unwrap_err(),
            Error::Invalid("changeset has no members".into())
        );
        let stranger = change(&w, "cli");
        assert!(changes::set_landing(&w.db, &stranger, "Jx").unwrap());
        assert_eq!(
            begin_landing(
                &w.db,
                &cs.id,
                "L0",
                "J0",
                &[step(&stranger, "r", "cli/Icli", None)],
                &w.ctx
            )
            .unwrap_err(),
            Error::Conflict("cli/Icli is landing".into())
        );
        assert_eq!(
            begin_landing(
                &w.db,
                &cs.id,
                "L0",
                "J0",
                &[step("nope", "r", "x/Ix", None)],
                &w.ctx
            )
            .unwrap_err(),
            Error::Conflict("x/Ix is not a change".into())
        );
        assert_eq!(by_id(&w.db, &cs.id).unwrap().unwrap().state, "open");
        assert_eq!(changes::by_id(&w.db, &api).unwrap().unwrap().state, "open");
        assert!(landing(&w.db, "L0").unwrap().is_none());

        // The commit point.
        let j1 = crate::jobs::create(&w.db, &w.org, None, "land", None).unwrap();
        let l = begin_landing(&w.db, &cs.id, "L1", &j1.id, &plan, &w.ctx).unwrap();
        assert_eq!(l.attempt, 1);
        assert_eq!(l.progress, plan);
        assert_eq!(by_id(&w.db, &cs.id).unwrap().unwrap().state, "landing");
        for id in [&api, &web] {
            let c = changes::by_id(&w.db, id).unwrap().unwrap();
            assert_eq!(c.state, "landing");
            assert_eq!(c.land_job_id.as_deref(), Some(j1.id.as_str()));
        }
        assert_eq!(
            begin_landing(&w.db, &cs.id, "L2", "J2", &plan, &w.ctx).unwrap_err(),
            Error::Conflict("changeset is landing".into())
        );
        assert_eq!(latest_landing(&w.db, &cs.id).unwrap().unwrap().id, "L1");
        // A hostile id is an absence.
        assert!(landing(&w.db, "'; DROP TABLE changeset_landings; --")
            .unwrap()
            .is_none());

        // Progress is written back, and read back as written.
        let mut progress = plan.clone();
        progress[0].state = StepState::Done;
        assert!(record_progress(&w.db, "L1", &progress).unwrap());
        assert_eq!(landing(&w.db, "L1").unwrap().unwrap().progress, progress);

        // Stranded: not while the job is queued, however old; yes once it
        // has failed; and a member of a landing changeset is never handed
        // to the single-change reaper, whose job would land it alone.
        let far = now_ms() + 60_000;
        assert!(stranded_landings(&w.db, far, 10).unwrap().is_empty());
        crate::jobs::fail(&w.db, &j1.id, "node died").unwrap();
        assert!(stranded_landings(&w.db, 0, 10).unwrap().is_empty(), "grace");
        let stranded = stranded_landings(&w.db, far, 10).unwrap();
        assert_eq!(stranded.len(), 1);
        assert_eq!(stranded[0].id, "L1");
        assert!(
            changes::stranded_landings(&w.db, far, 100)
                .unwrap()
                .iter()
                .all(|c| c.id != api && c.id != web),
            "a changeset member was handed to the single-change reaper"
        );

        // Adopting is a CAS on the job, and the members follow.
        let j2 = crate::jobs::create(&w.db, &w.org, None, "land", None).unwrap();
        assert!(!adopt_landing_job(&w.db, "L1", None, &j2.id).unwrap());
        assert!(adopt_landing_job(&w.db, "L1", Some(&j1.id), &j2.id).unwrap());
        assert!(!adopt_landing_job(&w.db, "L1", Some(&j1.id), "J3").unwrap());
        let l = landing(&w.db, "L1").unwrap().unwrap();
        assert_eq!(l.job_id.as_deref(), Some(j2.id.as_str()));
        assert_eq!(l.attempt, 2);
        for id in [&api, &web] {
            assert_eq!(
                changes::by_id(&w.db, id)
                    .unwrap()
                    .unwrap()
                    .land_job_id
                    .as_deref(),
                Some(j2.id.as_str())
            );
        }
        assert!(stranded_landings(&w.db, far, 10).unwrap().is_empty());

        // Finish: api landed, web put back open with its sentence.
        progress[1].state = StepState::Failed;
        progress[1].note = Some("moved".into());
        let members = vec![
            (
                api.clone(),
                MemberOutcome::Landed {
                    commit: "f".repeat(40),
                    verdict: "landed with changeset Iland".into(),
                },
            ),
            (
                web.clone(),
                MemberOutcome::Reopened {
                    verdict: "ejected: moved".into(),
                },
            ),
        ];
        assert!(finish_landing(&w.db, "L1", "failed", &progress, &members, &w.ctx).unwrap());
        let l = landing(&w.db, "L1").unwrap().unwrap();
        assert_eq!(l.outcome.as_deref(), Some("failed"));
        assert!(l.finished_at.is_some());
        assert_eq!(l.progress, progress);
        assert_eq!(by_id(&w.db, &cs.id).unwrap().unwrap().state, "failed");
        let a = changes::by_id(&w.db, &api).unwrap().unwrap();
        assert_eq!(a.state, "landed");
        assert_eq!(a.landed_commit.as_deref(), Some("f".repeat(40).as_str()));
        assert_eq!(
            a.land_verdict.as_deref(),
            Some("landed with changeset Iland")
        );
        let b = changes::by_id(&w.db, &web).unwrap().unwrap();
        assert_eq!(b.state, "open");
        assert_eq!(b.land_verdict.as_deref(), Some("ejected: moved"));
        assert!(b.land_job_id.is_none());
        assert!(
            binding(&w.db, &web).unwrap().is_none(),
            "members are released"
        );
        assert_eq!(members_of(&w.db, &cs.id), vec![api.clone(), web.clone()]);

        // Final means final.
        assert!(!record_progress(&w.db, "L1", &plan).unwrap());
        assert!(!adopt_landing_job(&w.db, "L1", Some(&j2.id), "J4").unwrap());
        assert!(!finish_landing(&w.db, "L1", "landed", &progress, &members, &w.ctx).unwrap());
        assert_eq!(landing(&w.db, "L1").unwrap().unwrap().progress, progress);
        assert!(stranded_landings(&w.db, far, 10).unwrap().is_empty());

        // A second landing on a changeset that already finished is not a
        // thing; and a landing whose changeset has been moved from under
        // it (by an operator) refuses to finish rather than overwrite.
        let cs2 = create(
            &w.db,
            &w.org,
            "Iland2",
            "t",
            "",
            None,
            &[cand(&web, "web/Iweb")],
            &[],
            None,
            &w.ctx,
        )
        .unwrap();
        let plan2 = vec![step(&web, &web_repo, "web/Iweb", None)];
        begin_landing(&w.db, &cs2.id, "L5", "J5", &plan2, &w.ctx).unwrap();
        w.db.lock()
            .execute(
                "UPDATE changesets SET state = 'abandoned' WHERE id = $1",
                &[&cs2.id],
            )
            .unwrap();
        assert!(!finish_landing(&w.db, "L5", "landed", &plan2, &[], &w.ctx).unwrap());
        assert!(landing(&w.db, "L5").unwrap().unwrap().outcome.is_none());
        // And the latest landing is the newest one, not the first.
        w.db.lock()
            .execute(
                "UPDATE changeset_landings SET progress = 'not json' WHERE id = $1",
                &[&"L5"],
            )
            .unwrap();
        assert!(matches!(landing(&w.db, "L5").unwrap_err(), Error::Db(_)));
    }

    fn members_of(db: &ControlDb, changeset_id: &str) -> Vec<String> {
        members(db, changeset_id)
            .unwrap()
            .into_iter()
            .map(|m| m.change_id)
            .collect()
    }

    /// A composition can be built twice, and only the newest build
    /// speaks for it.
    ///
    /// Remove a member and the composition is superseded and its checks
    /// are cancelled; add the member back and the same combination is
    /// current again, with a new run reporting against it. Both runs'
    /// rows are filed under that composition — they are keyed on the
    /// job, and it is a different job — so a read that returned both
    /// would hold the gate on a cancelled build that has already been
    /// replaced, forever, with nothing able to clear it.
    #[test]
    fn a_rebuilt_composition_reads_its_newest_verdict_and_not_the_cancelled_one() {
        use crate::changes::LandGate;
        let w = world("cs-rebuilt");
        let a = change(&w, "api");
        let repo_id = changes::by_id(&w.db, &a).unwrap().unwrap().repo_id;
        let cs = create(
            &w.db,
            &w.org,
            "Irebuilt1",
            "one repo",
            "",
            None,
            &[cand(&a, "api/Iapi")],
            &[],
            None,
            &w.ctx,
        )
        .unwrap();
        let run = |file: &str| {
            crate::workflows::create_settled_run(
                &w.db,
                &w.org,
                &repo_id,
                &crate::workflows::NewRun {
                    file,
                    name: "ci",
                    commit_sha: &"b".repeat(40),
                    ref_name: Some("main"),
                    event: "changeset",
                    change_key: Some("Iapi"),
                    changeset_id: Some(&cs.id),
                    composition: Some("c1"),
                    from_fork: false,
                },
                "failed",
                Some("boom"),
                None,
            )
            .unwrap()
            .id
        };
        let report = |run_id: &str, external: &str, state: &str| {
            crate::changeset_checks::upsert(
                &w.db,
                &crate::changeset_checks::NewChangesetCheck {
                    changeset_id: &cs.id,
                    composition: "c1",
                    repo_id: &repo_id,
                    run_id,
                    external_id: external,
                    name: "ci / test",
                    state,
                    detail_url: None,
                },
            )
            .unwrap();
        };
        // The first build of the composition, cancelled when a member
        // moved…
        let first = run(".weft/ci.yml");
        report(&first, "job-first", "cancelled");
        assert_eq!(
            composed_gate(&w.db, &cs.id, "c1").unwrap(),
            LandGate::Blocked {
                reason: "composed check ci / test in api is cancelled".into()
            },
            "on its own the cancelled build is still the verdict"
        );
        // …and the build of the same combination after the member came
        // back. Same check name, a different job.
        let second = run(".weft/ci2.yml");
        report(&second, "job-second", "passing");
        let rows = crate::changeset_checks::for_composition(&w.db, &cs.id, "c1").unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r.external_id.as_str())
                .collect::<Vec<_>>(),
            vec!["job-second"],
            "one row per check name, the newest"
        );
        assert_eq!(composed_gate(&w.db, &cs.id, "c1").unwrap(), LandGate::Ready);
    }

    /// The composed gate answers about **one** composition, refuses on
    /// anything that is not passing, and treats "nothing reported" as
    /// ready rather than as a wait.
    ///
    /// The last of those is what keeps composed CI opt-in: most
    /// changesets have no `on: changeset` file in any member, and a gate
    /// that waited for a verdict nothing would ever produce would make
    /// every one of them unlandable.
    #[test]
    fn the_composed_gate_reads_one_composition_and_refuses_anything_unchecked() {
        use crate::changes::LandGate;
        let w = world("cs-composed-gate");
        let a = change(&w, "api");
        let repo_id = changes::by_id(&w.db, &a).unwrap().unwrap().repo_id;
        let cs = create(
            &w.db,
            &w.org,
            "Icomposed1",
            "two repos",
            "",
            None,
            &[cand(&a, "api/Iapi")],
            &[],
            None,
            &w.ctx,
        )
        .unwrap();

        // A changeset nothing has reported on is ready, not waiting.
        assert_eq!(
            composed_gate(&w.db, &cs.id, "c1").unwrap(),
            LandGate::Ready,
            "no composed runs must not mean an unlandable changeset"
        );

        let run = crate::workflows::create_settled_run(
            &w.db,
            &w.org,
            &repo_id,
            &crate::workflows::NewRun {
                file: ".weft/ci.yml",
                name: "ci",
                commit_sha: &"a".repeat(40),
                ref_name: Some("main"),
                event: "changeset",
                change_key: Some("Iapi"),
                changeset_id: Some(&cs.id),
                composition: Some("c1"),
                from_fork: false,
            },
            "failed",
            Some("boom"),
            None,
        )
        .unwrap();
        let mut row = crate::changeset_checks::NewChangesetCheck {
            changeset_id: &cs.id,
            composition: "c1",
            repo_id: &repo_id,
            run_id: &run.id,
            external_id: "job-1",
            name: "ci / test",
            state: "running",
            detail_url: None,
        };
        crate::changeset_checks::upsert(&w.db, &row).unwrap();
        assert_eq!(
            composed_gate(&w.db, &cs.id, "c1").unwrap(),
            LandGate::Waiting {
                on: vec!["api: ci / test".into()]
            },
            "a wait names the repository whose build it is"
        );
        // …and says nothing about a composition it is not for. The
        // trigger has already superseded those runs; reporting them
        // would be a verdict about a combination nobody is looking at.
        assert_eq!(composed_gate(&w.db, &cs.id, "c2").unwrap(), LandGate::Ready);

        // Cancelled and skipped block, where a non-required check on a
        // single change would not: a composed run exists only because a
        // file asked for one, and nothing checked the combination.
        for state in ["failing", "cancelled", "skipped"] {
            row.state = state;
            crate::changeset_checks::upsert(&w.db, &row).unwrap();
            assert_eq!(
                composed_gate(&w.db, &cs.id, "c1").unwrap(),
                LandGate::Blocked {
                    reason: format!("composed check ci / test in api is {state}")
                },
                "{state} must not pass for a combination"
            );
        }

        row.state = "passing";
        crate::changeset_checks::upsert(&w.db, &row).unwrap();
        assert_eq!(composed_gate(&w.db, &cs.id, "c1").unwrap(), LandGate::Ready);
        // One row per job, however many times it reported.
        assert_eq!(
            crate::changeset_checks::for_composition(&w.db, &cs.id, "c1")
                .unwrap()
                .len(),
            1
        );
        // A changeset id that is not one — the shape a caller could only
        // have made up — reads as no checks, without asking the table.
        assert!(
            crate::changeset_checks::for_composition(&w.db, "not-a-changeset", "c1")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            composed_gate(&w.db, "not-a-changeset", "c1").unwrap(),
            LandGate::Ready
        );

        // A refusal beats a wait: a build that has said no cannot be
        // rescued by another that has not finished.
        crate::changeset_checks::upsert(
            &w.db,
            &crate::changeset_checks::NewChangesetCheck {
                external_id: "job-2",
                name: "ci / lint",
                state: "failing",
                ..row.clone()
            },
        )
        .unwrap();
        crate::changeset_checks::upsert(
            &w.db,
            &crate::changeset_checks::NewChangesetCheck {
                external_id: "job-3",
                name: "ci / slow",
                state: "queued",
                ..row.clone()
            },
        )
        .unwrap();
        assert_eq!(
            composed_gate(&w.db, &cs.id, "c1").unwrap(),
            LandGate::Blocked {
                reason: "composed check ci / lint in api is failing".into()
            }
        );
    }

    #[test]
    fn every_error_displays_its_own_words() {
        for e in [
            Error::Invalid("bad".into()),
            Error::Conflict("taken".into()),
            Error::Db("down".into()),
        ] {
            let words = match &e {
                Error::Invalid(s) | Error::Conflict(s) | Error::Db(s) => s.clone(),
            };
            assert_eq!(e.to_string(), words);
        }
    }
}
