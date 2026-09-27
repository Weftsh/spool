//! Check runs: the verdicts other people's build systems reached.
//!
//! Stratum does not run anybody's code — `api::checks_intake` says why at
//! length, and it is worth repeating here because this table is the place
//! somebody will eventually be tempted to grow a scheduler. We accept a
//! verdict; we do not produce one. Every row in `check_runs` was reported
//! by a provider that already knows whether the build passed, and the
//! only thing this module decides is *which row a report belongs to*.
//!
//! That question is the whole of the difficulty. A provider polls, or
//! retries a webhook, or sends "queued" and then "running" and then
//! "passing" for one build. Those are four reports of one run, and a
//! forge that stores four rows shows a commit with four contradictory
//! verdicts beside it — which is worse than showing none, because a
//! reader cannot tell which one is current. So writes are upserts on the
//! run's identity, and the identity is the interesting part:
//!
//! - A provider that gives us its own id (`external_id`) is trusted with
//!   it. `(repo_id, provider, external_id)` is a real unique index, the
//!   upsert is a real `ON CONFLICT`, and it is atomic. `provider` is in
//!   the key because "42" from GitHub Actions and "42" from Buildkite are
//!   two builds that happen to have picked the same number.
//! - A provider that gives us nothing to key on gets
//!   `(repo_id, provider, commit_sha, name)` — see [`upsert`], which has
//!   to work harder for it and says how.
//!
//! Reads are shaped by what the pages actually ask for: a repository's
//! history newest-first with filters down the left rail
//! ([`list`], [`workflow_names`]), and one verdict per workflow beside a
//! single commit ([`latest_for_commit`]).

use crate::db::ControlDb;
use crate::ids::{now_ms, ulid};
use postgres::types::ToSql;
use serde::Serialize;

/// The most rows one page will return. A caller asking for a million is
/// asking for the biggest page we have, not making an error: `limit` is a
/// hint about how much the *renderer* wants, and it usually comes from a
/// query string somebody typed or a client's default rather than from a
/// deliberate choice. Refusing it with a 400 turns a harmless
/// over-request into a broken page, and the caller's only recovery is to
/// guess our maximum — which they cannot see. Clamping answers the
/// request as closely as we can and hands back a cursor, which is what
/// they needed either way.
pub const MAX_LIMIT: i64 = 100;

/// Where a run got to.
///
/// The six the `check_runs` CHECK constraint permits, and no seventh:
/// this enum and that constraint have to be edited together, and the
/// tests here fail if they drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Accepted by the provider, not started.
    Queued,
    /// In progress. Not yet a verdict.
    Running,
    /// Green.
    Passing,
    /// Red.
    Failing,
    /// Stopped by a person or a newer push, with no verdict reached.
    Cancelled,
    /// The provider decided this run did not apply — a path filter, a
    /// docs-only change. Distinct from `Passing`: nothing was checked.
    Skipped,
}

impl RunState {
    /// Every state, in the order a refusal lists them.
    ///
    /// Exported so an error message can be *built* from the vocabulary
    /// rather than restating it. A hand-written list in a 400 is the one
    /// copy no test reads and every caller does, so it is the copy that
    /// goes stale — and a refusal naming five of six valid words sends a
    /// reporter to change something that was already right.
    pub const ALL: [RunState; 6] = [
        RunState::Queued,
        RunState::Running,
        RunState::Passing,
        RunState::Failing,
        RunState::Cancelled,
        RunState::Skipped,
    ];

    /// The vocabulary as a reader sees it: `"queued, running, …"`.
    pub fn names() -> String {
        Self::ALL
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Queued => "queued",
            RunState::Running => "running",
            RunState::Passing => "passing",
            RunState::Failing => "failing",
            RunState::Cancelled => "cancelled",
            RunState::Skipped => "skipped",
        }
    }

    /// Parse a state from the wire. Unknown text is rejected by name
    /// rather than read as a default, for the same reason
    /// `watches::Level::parse` refuses one: a provider that sent
    /// `"success"` meaning `"passing"` has to be told. Defaulting an
    /// unknown verdict to anything at all is the worst available answer
    /// — quietly green is a lie that ships broken code, and quietly red
    /// is a lie that blocks good code, and neither leaves a trace of
    /// having guessed.
    pub fn parse(s: &str) -> Result<RunState, String> {
        match s {
            "queued" => Ok(RunState::Queued),
            "running" => Ok(RunState::Running),
            "passing" => Ok(RunState::Passing),
            "failing" => Ok(RunState::Failing),
            "cancelled" => Ok(RunState::Cancelled),
            "skipped" => Ok(RunState::Skipped),
            other => Err(format!(
                "check state {other:?} is not one of queued, running, passing, \
                 failing, cancelled, skipped"
            )),
        }
    }
}

/// One check run as stored.
///
/// `state` is a `String` rather than a [`RunState`], and deliberately so.
/// The column carries a CHECK constraint naming the same six values, so
/// the database is the authority on what can be in there; re-parsing it
/// on the way out would add a failure branch that no test can reach and
/// no operator can trigger, and an unreachable branch on a read path is
/// how a page learns to answer 500 for a row that is perfectly fine.
/// Writes are typed — see [`NewCheckRun`] — which is the end that needs
/// it, because that is where caller text arrives.
#[derive(Debug, Clone, Serialize)]
pub struct CheckRun {
    pub id: String,
    pub repo_id: String,
    /// The commit the provider says it built. Not validated as a
    /// reachable object: a run can be reported for a commit that has
    /// since been rewritten away, and dropping the report would lose
    /// the only record that it happened.
    pub commit_sha: String,
    /// The branch or tag it ran for, when the provider names one.
    pub ref_name: Option<String>,
    pub provider: String,
    /// The provider's own id for the run, when it has one.
    pub external_id: Option<String>,
    /// The workflow's name, which is what a reader recognises.
    pub name: String,
    pub run_number: Option<i64>,
    /// What triggered it — `push`, `pull_request`, and so on.
    pub event: Option<String>,
    pub state: String,
    /// Where to go to read the log. Ours is a summary; theirs is the
    /// truth, and every check row should be one click from it.
    pub detail_url: Option<String>,
    pub actor: Option<String>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    /// When we first heard about this run — the paging key, and stable
    /// across every later report about the same run.
    pub created_at: i64,
    /// When we last heard about it.
    pub updated_at: i64,
}

/// The `provider` of every check-run row the server writes for its own
/// hosted job. External reporters go through the intake, which stamps
/// its own provider, so this value is the server saying *I ran this*.
pub const HOSTED_PROVIDER: &str = "weft";

/// A report about a run, on the way in.
///
/// Borrowed because it is built from a decoded request body that outlives
/// the write, and typed on `state` because that is the one field whose
/// value came from somebody else's JSON.
#[derive(Debug, Clone)]
pub struct NewCheckRun<'a> {
    pub commit_sha: &'a str,
    pub ref_name: Option<&'a str>,
    pub provider: &'a str,
    pub external_id: Option<&'a str>,
    pub name: &'a str,
    pub run_number: Option<i64>,
    pub event: Option<&'a str>,
    pub state: RunState,
    pub detail_url: Option<&'a str>,
    pub actor: Option<&'a str>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
}

const RUN_COLS: &str = "id, repo_id, commit_sha, ref_name, provider, external_id, name, \
                        run_number, event, state, detail_url, actor, started_at, \
                        completed_at, created_at, updated_at";

fn row_to_run(r: &postgres::Row) -> CheckRun {
    CheckRun {
        id: r.get("id"),
        repo_id: r.get("repo_id"),
        commit_sha: r.get("commit_sha"),
        ref_name: r.get("ref_name"),
        provider: r.get("provider"),
        external_id: r.get("external_id"),
        name: r.get("name"),
        run_number: r.get("run_number"),
        event: r.get("event"),
        state: r.get("state"),
        detail_url: r.get("detail_url"),
        actor: r.get("actor"),
        started_at: r.get("started_at"),
        completed_at: r.get("completed_at"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

// --- writes ---------------------------------------------------------

/// The insert both upsert paths share.
///
/// `created_at` is set from `$15` on insert and is **absent from the
/// `DO UPDATE SET` list**, which is the line that matters: a run keeps
/// the moment we first heard about it forever. It is the paging key, so
/// a run that moved it would jump to the top of the list every time the
/// provider polled, and a reader watching a busy repository would see
/// the same build shuffle past them all afternoon.
const INSERT_RUN: &str = "\
    INSERT INTO check_runs \
      (id, repo_id, commit_sha, ref_name, provider, external_id, name, run_number, \
       event, state, detail_url, actor, started_at, completed_at, created_at, updated_at) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $15) \
    ON CONFLICT (repo_id, provider, external_id) WHERE external_id IS NOT NULL \
    DO UPDATE SET \
      commit_sha = EXCLUDED.commit_sha, ref_name = EXCLUDED.ref_name, \
      name = EXCLUDED.name, run_number = EXCLUDED.run_number, \
      event = EXCLUDED.event, state = EXCLUDED.state, \
      detail_url = EXCLUDED.detail_url, actor = EXCLUDED.actor, \
      started_at = EXCLUDED.started_at, completed_at = EXCLUDED.completed_at, \
      updated_at = EXCLUDED.updated_at \
    RETURNING ";

/// The same move, for a run with no provider id, keyed on the identity
/// argued for in [`upsert`].
const UPDATE_BY_IDENTITY: &str = "\
    UPDATE check_runs SET \
      ref_name = $4, run_number = $6, event = $7, state = $8, detail_url = $9, \
      actor = $10, started_at = $11, completed_at = $12, updated_at = $13 \
    WHERE repo_id = $1 AND provider = $2 AND commit_sha = $3 AND name = $5 \
      AND external_id IS NULL \
    RETURNING ";

/// A 64-bit key for the advisory lock that serialises an id-less upsert.
///
/// Pure and separately tested, because it is the thing the correctness of
/// the id-less path rests on and it needs no database to exercise. FNV-1a
/// over the identity tuple with the separator included, so
/// `("a", "bc")` and `("ab", "c")` are different keys — an unseparated
/// concatenation would let two distinct runs share a lock, which is
/// merely slow, but it would also be the kind of subtlety nobody
/// re-derives correctly during an incident.
fn identity_lock_key(repo_id: &str, provider: &str, commit_sha: &str, name: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in [repo_id, provider, commit_sha, name] {
        for b in part.bytes().chain(std::iter::once(0u8)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
    }
    h as i64
}

/// Record a report, updating the run it is about rather than adding a
/// second row for it.
///
/// Two paths, because the schema gives us two different amounts of help.
///
/// **With an `external_id`** the partial unique index applies and this is
/// a plain `ON CONFLICT` upsert: atomic, and correct no matter how many
/// nodes take the report at once.
///
/// **Without one** the index does not apply, and the honest options were
/// to insert every time or to invent an identity. Inserting every time is
/// not defensible for the surface this exists to serve: the id-less
/// caller is a script POSTing `{"name": "lint", "state": "passing"}` from
/// a CI job, it will re-post on every retry and on the state change from
/// `running` to `passing`, and a commit page showing "lint: running" and
/// "lint: passing" side by side is a bug report. So the identity is
/// `(repo_id, provider, commit_sha, name)` — the tuple such a caller can
/// actually reproduce across two requests, and the same tuple a person
/// means when they say "the lint run for that commit". The cost is real
/// and worth naming: a provider that genuinely re-runs one workflow on
/// one commit twice gets one row, the second run overwriting the first.
/// That is the right trade only because such a provider almost always
/// *has* an id, and giving us one moves it to the other path.
///
/// The identity has no unique index behind it, which makes the
/// update-then-insert a read-modify-write with a window: two nodes could
/// both find nothing and both insert. `ControlDb` serialises this
/// process's queries through one connection, so a single node cannot lose
/// that race — but we run a fleet, and "it cannot happen on one node" is
/// exactly the reasoning that has already cost this repository a
/// production incident (see the S3 conditional-PUT story in CLAUDE.md).
/// A transaction-scoped advisory lock on the identity closes it without a
/// migration: contending writers queue, the loser finds the row the
/// winner wrote, and the lock is released by the commit whether or not
/// the statement succeeded.
///
/// **This wants a migration.** `CREATE UNIQUE INDEX check_runs_identity ON
/// check_runs(repo_id, provider, commit_sha, name) WHERE external_id IS
/// NULL` would make this path a second `ON CONFLICT` and delete the lock,
/// the transaction, and this paragraph. `db.rs` is not this module's to
/// edit; the index is reported rather than assumed.
///
/// An `external_id` of `""` is read as no id at all. It arrives from a
/// client that serialised a missing field as an empty string, and taking
/// it literally would be the worst of both worlds: every id-less run in
/// the repository would collide on one index key and overwrite each
/// other, so a repository's whole check history would collapse to a
/// single row per provider.
pub fn upsert(db: &ControlDb, repo_id: &str, run: &NewCheckRun) -> Result<CheckRun, String> {
    let external_id = run.external_id.filter(|e| !e.is_empty());
    let now = now_ms();
    let id = ulid();
    let state = run.state.as_str();
    let insert_sql = format!("{INSERT_RUN}{RUN_COLS}");
    let update_sql = format!("{UPDATE_BY_IDENTITY}{RUN_COLS}");

    let row = db
        .lock()
        .transaction(|tx| {
            let insert: [&(dyn ToSql + Sync); 15] = [
                &id,
                &repo_id,
                &run.commit_sha,
                &run.ref_name,
                &run.provider,
                &external_id,
                &run.name,
                &run.run_number,
                &run.event,
                &state,
                &run.detail_url,
                &run.actor,
                &run.started_at,
                &run.completed_at,
                &now,
            ];
            if external_id.is_some() {
                return tx.query_one(insert_sql.as_str(), &insert);
            }
            let key = identity_lock_key(repo_id, run.provider, run.commit_sha, run.name);
            tx.execute("SELECT pg_advisory_xact_lock($1)", &[&key])?;
            let existing = tx.query_opt(
                update_sql.as_str(),
                &[
                    &repo_id,
                    &run.provider,
                    &run.commit_sha,
                    &run.ref_name,
                    &run.name,
                    &run.run_number,
                    &run.event,
                    &state,
                    &run.detail_url,
                    &run.actor,
                    &run.started_at,
                    &run.completed_at,
                    &now,
                ],
            )?;
            match existing {
                Some(r) => Ok(r),
                None => tx.query_one(insert_sql.as_str(), &insert),
            }
        })
        .map_err(|e| format!("record check run: {e}"))?;
    Ok(row_to_run(&row))
}

// --- reads ----------------------------------------------------------

/// A filter value that could never match anything stored.
///
/// Same reasoning as `audit::unmatchable` and `ids::valid_id`: every
/// value in this table came in through a validated intake, so none of
/// them contains a control character. Handed to Postgres, a NUL is a
/// database error and a 500, where the contract promises "no results".
/// Answering "nothing matched" is both true and the thing the caller can
/// act on.
fn unmatchable(v: &str) -> bool {
    v.bytes().any(|b| b < 0x20 || b == 0x7f)
}

/// One run, read through the repository it belongs to.
///
/// `repo_id` is in the `WHERE` clause rather than checked against the row
/// afterwards, and that is the whole of the security of this function. A
/// run belonging to another repository has to be indistinguishable from
/// an id that was never issued — same 404, no timing tell, nothing that
/// confirms the id exists — and a fetch-then-compare has the row in
/// memory before it decides. That shape survives review once; the edit
/// that logs the mismatch, or reports "wrong repository" to help the
/// caller, or returns the row's `state` in a metric, is one careless
/// commit away and turns the guard into an oracle. Scoping the query
/// leaves nothing to leak.
///
/// `None` is the only "no" this returns. An id of the wrong shape, an id
/// from another tenant, and an id nobody ever minted are one answer, and
/// the caller cannot tell them apart because it must not be able to.
pub fn get(db: &ControlDb, repo_id: &str, id: &str) -> Result<Option<CheckRun>, String> {
    // Same guard as `latest_for_commit`: a control byte in a path
    // segment is "nothing matched", not a database error and a 500.
    if unmatchable(id) {
        return Ok(None);
    }
    let row = db
        .lock()
        .query_opt(
            &format!("SELECT {RUN_COLS} FROM check_runs WHERE repo_id = $1 AND id = $2"),
            &[&repo_id, &id],
        )
        .map_err(|e| format!("read check run: {e}"))?;
    Ok(row.as_ref().map(row_to_run))
}

/// Remove the row a provider wrote for `external_id`, if there is one.
///
/// The narrowest delete in this module and the only one: it exists
/// because approving a fork's workflows replaces a `blocked` placeholder
/// run with the real run for the same commit, and the placeholder's
/// mirrored row is keyed on the **run** id while the real jobs each
/// mirror a row keyed on a **job** id. Leaving it behind is not
/// cosmetic — the land gate reads these rows, so the change would sit
/// unlandable behind a check for a run that was deliberately replaced,
/// with nothing on the page to explain it.
///
/// Scoped to `repo_id` for the same reason [`get`] is: an id belonging
/// to another tenant has to be indistinguishable from one nobody minted.
/// `false` means nothing matched, which is not an error — the caller is
/// tidying up after a row that may never have been written.
pub fn delete_external(
    db: &ControlDb,
    repo_id: &str,
    provider: &str,
    external_id: &str,
) -> Result<bool, String> {
    if unmatchable(external_id) || external_id.is_empty() {
        return Ok(false);
    }
    db.lock()
        .execute(
            "DELETE FROM check_runs \
             WHERE repo_id = $1 AND provider = $2 AND external_id = $3",
            &[&repo_id, &provider, &external_id],
        )
        .map(|n| n > 0)
        .map_err(|e| format!("delete check run: {e}"))
}

/// One bound parameter, owned. Same shape as `issues::Arg`, and for the
/// same reason: the filter shaping is pure so it can be reasoned about
/// without a database, which means it cannot hand back borrows into a
/// temporary.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Arg {
    Text(String),
    Int(i64),
}

/// A `WHERE` fragment and the arguments it expects, in order. `$1` is
/// always the repo id and is not in `args`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shaped {
    sql: String,
    args: Vec<Arg>,
}

impl Shaped {
    fn params(&self) -> Vec<&(dyn ToSql + Sync)> {
        self.args
            .iter()
            .map(|a| match a {
                Arg::Text(s) => s as &(dyn ToSql + Sync),
                Arg::Int(i) => i as &(dyn ToSql + Sync),
            })
            .collect()
    }
}

/// What the checks page is asking for. All optional but `limit`.
#[derive(Debug, Clone, Default)]
pub struct RunQuery<'a> {
    /// Matched against `ref_name`. Called `branch` because that is what
    /// the filter is called on the page and in the query string.
    pub branch: Option<&'a str>,
    pub state: Option<RunState>,
    pub event: Option<&'a str>,
    pub actor: Option<&'a str>,
    /// Matched against `name` — one of [`workflow_names`]'s answers.
    pub workflow: Option<&'a str>,
    /// Clamped to [`MAX_LIMIT`], never refused.
    pub limit: i64,
    /// The cursor: rows strictly older than this `created_at`.
    pub before: Option<i64>,
}

/// Shape a filter into SQL with numbered placeholders.
///
/// Every caller-supplied value becomes a bind parameter and nothing else
/// is ever pushed into the string — only placeholder *numbers*, which are
/// derived from the count of arguments and not from anything a caller
/// typed. That is what makes a workflow named `'; DROP TABLE --` a
/// workflow named `'; DROP TABLE --`, matched literally against `name`.
fn shape(q: &RunQuery) -> Shaped {
    let mut sql = String::from("repo_id = $1");
    let mut args: Vec<Arg> = Vec::new();
    let push = |args: &mut Vec<Arg>, sql: &mut String, col: &str, op: &str, a: Arg| {
        args.push(a);
        let n = args.len() + 1;
        sql.push_str(&format!(" AND {col} {op} ${n}"));
    };

    if let Some(b) = q.branch {
        push(
            &mut args,
            &mut sql,
            "ref_name",
            "=",
            Arg::Text(b.to_string()),
        );
    }
    if let Some(s) = q.state {
        push(
            &mut args,
            &mut sql,
            "state",
            "=",
            Arg::Text(s.as_str().to_string()),
        );
    }
    if let Some(e) = q.event {
        push(&mut args, &mut sql, "event", "=", Arg::Text(e.to_string()));
    }
    if let Some(a) = q.actor {
        push(&mut args, &mut sql, "actor", "=", Arg::Text(a.to_string()));
    }
    if let Some(w) = q.workflow {
        push(&mut args, &mut sql, "name", "=", Arg::Text(w.to_string()));
    }
    if let Some(b) = q.before {
        push(&mut args, &mut sql, "created_at", "<", Arg::Int(b));
    }
    Shaped { sql, args }
}

/// A repository's check runs, newest first, and the cursor for the next
/// page.
///
/// The cursor is a `created_at` and the page is `created_at < before`,
/// which has one hazard worth spelling out because it is invisible in
/// testing until a machine is fast: `created_at` is a **millisecond**,
/// and a provider reporting a matrix of twenty jobs writes twenty rows
/// inside one. A page that ended in the middle of such a group would
/// hand back that millisecond as the cursor, and the next page — strictly
/// older — would skip every sibling that shared it. Rows dropped from a
/// paged list are the worst class of paging bug: nothing errors, the
/// counts nearly add up, and the run somebody is looking for is simply
/// not there.
///
/// So the page is taken in two steps in one statement. The inner query
/// takes `limit` rows to find the boundary millisecond; the outer one
/// returns *everything* at or after it. A page can therefore come back
/// slightly longer than `limit` — bounded by how many runs one repository
/// started in the same millisecond — and in exchange the boundary always
/// falls between two millisecond groups, so no row is ever skipped or
/// returned twice. `id DESC` breaks the remaining ties for a stable
/// order; ULIDs are time-ordered, so that reads as newest-first too.
///
/// The cursor is `Some` whenever the page came back full, which can mean
/// one wasted request at the end of a list that divides exactly. That is
/// the right way round: a spurious empty page is visible and harmless,
/// where a `None` computed optimistically truncates the history.
pub fn list(
    db: &ControlDb,
    repo_id: &str,
    q: &RunQuery,
) -> Result<(Vec<CheckRun>, Option<i64>), String> {
    if [q.branch, q.event, q.actor, q.workflow]
        .into_iter()
        .flatten()
        .any(unmatchable)
    {
        return Ok((Vec::new(), None));
    }
    let w = shape(q);
    let limit = q.limit.clamp(1, MAX_LIMIT);
    let slot = w.args.len() + 2;
    let sql = format!(
        "WITH page AS (\
           SELECT created_at FROM check_runs WHERE {where_sql} \
           ORDER BY created_at DESC, id DESC LIMIT ${slot}) \
         SELECT {RUN_COLS} FROM check_runs \
         WHERE {where_sql} AND created_at >= (SELECT min(created_at) FROM page) \
         ORDER BY created_at DESC, id DESC",
        where_sql = w.sql,
    );
    let bound = w.params();
    let mut params: Vec<&(dyn ToSql + Sync)> = vec![&repo_id];
    params.extend(bound.iter().copied());
    params.push(&limit);

    let rows = db
        .lock()
        .query(sql.as_str(), &params)
        .map_err(|e| format!("list check runs: {e}"))?;
    let runs: Vec<CheckRun> = rows.iter().map(row_to_run).collect();
    let next = match runs.last() {
        Some(last) if runs.len() as i64 >= limit => Some(last.created_at),
        _ => None,
    };
    Ok((runs, next))
}

/// Every workflow name this repository has ever reported, in order.
///
/// The left rail's filter list. Ordered by name rather than by recency
/// on purpose: a rail whose entries move around between page loads is one
/// people stop using, and alphabetical is the only order a reader can
/// predict well enough to click without reading.
pub fn workflow_names(db: &ControlDb, repo_id: &str) -> Result<Vec<String>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT DISTINCT name FROM check_runs WHERE repo_id = $1 ORDER BY name",
            &[&repo_id],
        )
        .map_err(|e| format!("list workflow names: {e}"))?;
    Ok(rows.iter().map(|r| r.get::<_, String>("name")).collect())
}

/// The current verdict from each workflow for one commit.
///
/// One row per workflow name, the newest — which is what a commit page
/// needs and a history is not. Rendering every run for a commit puts
/// "build: failing" from twenty minutes ago above "build: passing" from
/// two, and a reader who scans the first line concludes the commit is
/// broken.
///
/// Keyed on `name` alone rather than `(provider, name)`, because the name
/// is what the reader recognises and a commit showing two rows both
/// labelled "build" is the ambiguity this function exists to remove. The
/// cost: if two providers genuinely report a workflow of the same name
/// for one commit, the newer one wins and the other is not shown. Worth
/// revisiting the day a repository is dual-reported; until then, one
/// unambiguous line per name is the better answer.
pub fn latest_for_commit(
    db: &ControlDb,
    repo_id: &str,
    commit_sha: &str,
) -> Result<Vec<CheckRun>, String> {
    if unmatchable(commit_sha) {
        return Ok(Vec::new());
    }
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT DISTINCT ON (name) {RUN_COLS} FROM check_runs \
                 WHERE repo_id = $1 AND commit_sha = $2 \
                 ORDER BY name, created_at DESC, id DESC"
            ),
            &[&repo_id, &commit_sha],
        )
        .map_err(|e| format!("read checks for commit: {e}"))?;
    Ok(rows.iter().map(row_to_run).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(tag: &str) -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url(tag)).unwrap()
    }

    /// A person, their namespace, and a repository to hang runs off.
    fn fixture(db: &ControlDb) -> (String, String) {
        let u =
            crate::users::create(db, "ada@example.com", "ada", Some("a long password")).unwrap();
        let ns = crate::registry::create_personal_namespace(db, &u.id, "ada", None).unwrap();
        let r = repo(db, &ns.id, "widget");
        (ns.id, r)
    }

    /// Another repository in the same namespace, for the scoping tests.
    /// Public, like the first: the isolation being asserted has to be the
    /// row scoping and not a visibility check that would have hidden the
    /// row anyway.
    fn repo(db: &ControlDb, ns: &str, name: &str) -> String {
        crate::registry::create_repo(
            db,
            ns,
            &crate::registry::NewRepo {
                description: None,
                name,
                kind: crate::registry::RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap()
        .id
    }

    fn run<'a>(name: &'a str, state: RunState) -> NewCheckRun<'a> {
        NewCheckRun {
            commit_sha: "abc123",
            ref_name: Some("refs/heads/main"),
            provider: "github",
            external_id: None,
            name,
            run_number: Some(1),
            event: Some("push"),
            state,
            detail_url: Some("https://ci.example/1"),
            actor: Some("ada"),
            started_at: Some(10),
            completed_at: None,
        }
    }

    fn count(db: &ControlDb, repo_id: &str) -> i64 {
        db.lock()
            .query_one(
                "SELECT COUNT(*) FROM check_runs WHERE repo_id = $1",
                &[&repo_id],
            )
            .unwrap()
            .get(0)
    }

    /// Pin a run's `created_at` so ordering assertions do not depend on
    /// how fast the machine got through the inserts.
    fn stamp(db: &ControlDb, id: &str, at: i64) {
        db.lock()
            .execute(
                "UPDATE check_runs SET created_at = $2 WHERE id = $1",
                &[&id, &at],
            )
            .unwrap();
    }

    fn ids(runs: &[CheckRun]) -> Vec<&str> {
        runs.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn a_repolled_run_updates_in_place_and_keeps_the_moment_we_first_saw_it() {
        // The bug this prevents is a commit page showing one build
        // twice, with two different verdicts and nothing to say which
        // is current.
        let db = db("checks_repoll");
        let (_, repo) = fixture(&db);

        let first = upsert(
            &db,
            &repo,
            &NewCheckRun {
                external_id: Some("gh-42"),
                ..run("build", RunState::Running)
            },
        )
        .unwrap();
        assert_eq!(first.state, "running");
        stamp(&db, &first.id, 1_000);

        let second = upsert(
            &db,
            &repo,
            &NewCheckRun {
                external_id: Some("gh-42"),
                completed_at: Some(99),
                ..run("build", RunState::Passing)
            },
        )
        .unwrap();

        assert_eq!(count(&db, &repo), 1, "a re-poll inserted a second row");
        assert_eq!(second.id, first.id, "the update minted a new id");
        assert_eq!(second.state, "passing");
        assert_eq!(second.completed_at, Some(99));
        assert_eq!(
            second.created_at, 1_000,
            "a re-poll moved created_at, so the run would jump to the \
             top of the list every time the provider polled"
        );
        assert!(second.updated_at >= 1_000);
    }

    #[test]
    fn two_providers_may_use_the_same_id_for_different_runs() {
        // "42" from GitHub Actions and "42" from Buildkite are two
        // builds. The unique index includes `provider`; this fails if
        // somebody ever narrows it to (repo_id, external_id).
        let db = db("checks_two_providers");
        let (_, repo) = fixture(&db);

        let a = upsert(
            &db,
            &repo,
            &NewCheckRun {
                provider: "github",
                external_id: Some("42"),
                ..run("build", RunState::Passing)
            },
        )
        .unwrap();
        let b = upsert(
            &db,
            &repo,
            &NewCheckRun {
                provider: "buildkite",
                external_id: Some("42"),
                ..run("build", RunState::Failing)
            },
        )
        .unwrap();

        assert_ne!(a.id, b.id);
        assert_eq!(count(&db, &repo), 2);
        assert_eq!(a.state, "passing");
        assert_eq!(b.state, "failing");
    }

    #[test]
    fn an_idless_caller_reporting_the_same_logical_run_updates_it() {
        // The identity for a caller with nothing to key on:
        // (repo_id, provider, commit_sha, name). A CI script POSTing
        // "running" and then "passing" for one job means one run.
        let db = db("checks_idless");
        let (_, repo) = fixture(&db);

        let first = upsert(&db, &repo, &run("lint", RunState::Running)).unwrap();
        stamp(&db, &first.id, 2_000);
        let second = upsert(&db, &repo, &run("lint", RunState::Passing)).unwrap();

        assert_eq!(
            count(&db, &repo),
            1,
            "the same logical run was stored twice"
        );
        assert_eq!(second.id, first.id);
        assert_eq!(second.state, "passing");
        assert_eq!(second.created_at, 2_000);

        // A different name on the same commit is a different run, and a
        // different commit under the same name is too — the identity is
        // the whole tuple, not any part of it.
        upsert(&db, &repo, &run("test", RunState::Passing)).unwrap();
        upsert(
            &db,
            &repo,
            &NewCheckRun {
                commit_sha: "def456",
                ..run("lint", RunState::Passing)
            },
        )
        .unwrap();
        assert_eq!(count(&db, &repo), 3);
    }

    #[test]
    fn an_empty_external_id_is_no_external_id() {
        // A client that serialised a missing field as "" would
        // otherwise put every id-less run in the repository on one
        // index key, collapsing the whole history to a row per
        // provider.
        let db = db("checks_empty_extid");
        let (_, repo) = fixture(&db);

        let a = upsert(
            &db,
            &repo,
            &NewCheckRun {
                external_id: Some(""),
                ..run("build", RunState::Passing)
            },
        )
        .unwrap();
        let b = upsert(
            &db,
            &repo,
            &NewCheckRun {
                external_id: Some(""),
                ..run("test", RunState::Passing)
            },
        )
        .unwrap();

        assert_eq!(a.external_id, None, "\"\" was stored as an id");
        assert_ne!(a.id, b.id, "two different workflows collapsed into one row");
        assert_eq!(count(&db, &repo), 2);
    }

    #[test]
    fn the_identity_lock_key_separates_its_parts() {
        // ("a","b",…) and ("ab","",…) must not share a lock. Pure, so
        // it needs no database.
        assert_ne!(
            identity_lock_key("a", "b", "c", "d"),
            identity_lock_key("ab", "", "c", "d")
        );
        assert_eq!(
            identity_lock_key("r", "github", "abc", "build"),
            identity_lock_key("r", "github", "abc", "build"),
            "the key is not stable, so the lock protects nothing"
        );
    }

    #[test]
    fn listing_is_newest_first_and_the_cursor_pages_without_skipping() {
        let db = db("checks_paging");
        let (_, repo) = fixture(&db);

        // Seven runs, one per millisecond, oldest first.
        let mut made = Vec::new();
        for i in 0..7i64 {
            let name = format!("job-{i}");
            let r = upsert(&db, &repo, &run(&name, RunState::Passing)).unwrap();
            stamp(&db, &r.id, 1_000 + i);
            made.push(r.id);
        }
        made.reverse(); // newest first, which is what `list` promises

        let mut seen: Vec<String> = Vec::new();
        let mut before = None;
        let mut pages = 0;
        loop {
            let (runs, next) = list(
                &db,
                &repo,
                &RunQuery {
                    limit: 3,
                    before,
                    ..RunQuery::default()
                },
            )
            .unwrap();
            pages += 1;
            seen.extend(runs.iter().map(|r| r.id.clone()));
            match next {
                Some(c) => before = Some(c),
                None => break,
            }
            assert!(pages < 10, "paging did not terminate");
        }

        assert!(pages >= 3, "seven rows at three a page took {pages} pages");
        assert_eq!(seen, made, "a page skipped, repeated or reordered a row");
    }

    #[test]
    fn a_page_boundary_never_falls_inside_one_millisecond() {
        // A matrix of jobs reported together shares a `created_at`. A
        // cursor of `created_at < before` taken mid-group would skip
        // every sibling that shared the millisecond — rows silently
        // missing from a list, which nothing else in the system would
        // report.
        let db = db("checks_tied_ms");
        let (_, repo) = fixture(&db);

        let mut all = Vec::new();
        for i in 0..5 {
            let name = format!("matrix-{i}");
            let r = upsert(&db, &repo, &run(&name, RunState::Passing)).unwrap();
            stamp(&db, &r.id, 5_000); // every one of them, same instant
            all.push(r.id);
        }
        // One older run, so there is a second page to reach at all.
        let older = upsert(&db, &repo, &run("older", RunState::Passing)).unwrap();
        stamp(&db, &older.id, 4_000);
        all.push(older.id);

        let (first, next) = list(
            &db,
            &repo,
            &RunQuery {
                limit: 2,
                ..RunQuery::default()
            },
        )
        .unwrap();
        assert_eq!(
            first.len(),
            5,
            "the page stopped inside the tied millisecond, so the next \
             page would skip its siblings"
        );
        let (second, _) = list(
            &db,
            &repo,
            &RunQuery {
                limit: 2,
                before: next,
                ..RunQuery::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&second), vec![all[5].as_str()]);

        let mut seen: Vec<String> = first
            .iter()
            .chain(second.iter())
            .map(|r| r.id.clone())
            .collect();
        seen.sort();
        all.sort();
        assert_eq!(seen, all, "a row was lost or repeated across the boundary");
    }

    #[test]
    fn every_filter_narrows_and_a_sql_metacharacter_is_just_text() {
        let db = db("checks_filters");
        let (_, repo) = fixture(&db);

        upsert(
            &db,
            &repo,
            &NewCheckRun {
                ref_name: Some("refs/heads/release"),
                event: Some("tag"),
                actor: Some("bo"),
                ..run("deploy", RunState::Failing)
            },
        )
        .unwrap();
        let plain = upsert(&db, &repo, &run("build", RunState::Passing)).unwrap();

        // A workflow whose name is a SQL injection attempt is a workflow
        // with a silly name. It matches itself and nothing else, and
        // asking for it leaves every other row exactly where it was.
        let hostile = "'; DROP TABLE check_runs; -- 100%_";
        let injected = upsert(&db, &repo, &run(hostile, RunState::Queued)).unwrap();

        let one = |q: RunQuery| list(&db, &repo, &q).unwrap().0;
        let all = one(RunQuery {
            limit: 50,
            ..RunQuery::default()
        });
        assert_eq!(all.len(), 3);

        assert_eq!(
            ids(&one(RunQuery {
                branch: Some("refs/heads/release"),
                limit: 50,
                ..RunQuery::default()
            }))
            .len(),
            1
        );
        assert_eq!(
            ids(&one(RunQuery {
                state: Some(RunState::Passing),
                limit: 50,
                ..RunQuery::default()
            })),
            vec![plain.id.as_str()]
        );
        assert_eq!(
            one(RunQuery {
                event: Some("tag"),
                limit: 50,
                ..RunQuery::default()
            })
            .len(),
            1
        );
        assert_eq!(
            one(RunQuery {
                actor: Some("bo"),
                limit: 50,
                ..RunQuery::default()
            })
            .len(),
            1
        );

        let hit = one(RunQuery {
            workflow: Some(hostile),
            limit: 50,
            ..RunQuery::default()
        });
        assert_eq!(
            ids(&hit),
            vec![injected.id.as_str()],
            "not matched literally"
        );
        // The `%` and `_` are not wildcards either — this is `=`, not
        // `LIKE`, and a filter of "100%_" must not match "build".
        assert!(one(RunQuery {
            workflow: Some("100%_"),
            limit: 50,
            ..RunQuery::default()
        })
        .is_empty());
        // And the table is still there with every row in it.
        assert_eq!(count(&db, &repo), 3);
        assert_eq!(
            one(RunQuery {
                limit: 50,
                ..RunQuery::default()
            })
            .len(),
            3
        );

        // A filter carrying bytes no stored value can contain answers
        // "nothing matched" rather than a database error and a 500.
        let (none, cursor) = list(
            &db,
            &repo,
            &RunQuery {
                workflow: Some("build\0"),
                limit: 50,
                ..RunQuery::default()
            },
        )
        .unwrap();
        assert!(none.is_empty() && cursor.is_none());
    }

    #[test]
    fn an_over_large_limit_is_clamped_rather_than_refused() {
        // `limit` is a rendering hint, not something the caller typed
        // with intent; a 400 would break the page and give them nothing
        // to correct it with.
        let db = db("checks_limit");
        let (_, repo) = fixture(&db);
        // More than MAX_LIMIT rows, on distinct milliseconds — the only
        // way the clamp is observable at all. With three rows in the
        // table, `LIMIT 1000000` and `LIMIT 100` return the same page
        // and the assertion below would hold against no clamp
        // whatsoever.
        let over = MAX_LIMIT + 1;
        for i in 0..over {
            let name = format!("job-{i}");
            let r = upsert(&db, &repo, &run(&name, RunState::Passing)).unwrap();
            stamp(&db, &r.id, 1_000 + i);
        }

        let (runs, next) = list(
            &db,
            &repo,
            &RunQuery {
                limit: 1_000_000,
                ..RunQuery::default()
            },
        )
        .unwrap();
        assert_eq!(
            runs.len() as i64,
            MAX_LIMIT,
            "a huge limit was answered in full, so one request can ask \
             for the whole table"
        );
        // Clamping is not truncation: the rest is still reachable, which
        // is what makes answering a smaller page than asked for honest.
        assert!(next.is_some(), "a clamped page handed back no cursor");
        let (rest, _) = list(
            &db,
            &repo,
            &RunQuery {
                limit: 1_000_000,
                before: next,
                ..RunQuery::default()
            },
        )
        .unwrap();
        assert_eq!(rest.len() as i64, over - MAX_LIMIT);

        // And the floor: a zero or negative limit still returns a page.
        let (one, _) = list(
            &db,
            &repo,
            &RunQuery {
                limit: 0,
                ..RunQuery::default()
            },
        )
        .unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn a_commit_shows_one_verdict_per_workflow_and_it_is_the_newest() {
        // Rendering the history instead would put a stale "failing"
        // above a fresh "passing", and a reader who scans the first
        // line concludes the commit is broken.
        let db = db("checks_latest");
        let (_, repo) = fixture(&db);

        let stale = upsert(
            &db,
            &repo,
            &NewCheckRun {
                external_id: Some("gh-1"),
                ..run("build", RunState::Failing)
            },
        )
        .unwrap();
        stamp(&db, &stale.id, 1_000);
        let fresh = upsert(
            &db,
            &repo,
            &NewCheckRun {
                external_id: Some("gh-2"),
                ..run("build", RunState::Passing)
            },
        )
        .unwrap();
        stamp(&db, &fresh.id, 2_000);
        let lint = upsert(
            &db,
            &repo,
            &NewCheckRun {
                external_id: Some("gh-3"),
                ..run("lint", RunState::Passing)
            },
        )
        .unwrap();
        stamp(&db, &lint.id, 1_500);
        // Another commit's run must not appear beside this one.
        upsert(
            &db,
            &repo,
            &NewCheckRun {
                commit_sha: "def456",
                external_id: Some("gh-4"),
                ..run("build", RunState::Failing)
            },
        )
        .unwrap();

        let latest = latest_for_commit(&db, &repo, "abc123").unwrap();
        assert_eq!(ids(&latest), vec![fresh.id.as_str(), lint.id.as_str()]);
        assert!(latest.iter().all(|r| r.state == "passing"));

        // The rail lists each name once, in an order a reader can
        // predict.
        assert_eq!(
            workflow_names(&db, &repo).unwrap(),
            vec!["build".to_string(), "lint".to_string()]
        );

        // A commit sha carrying bytes nothing stored can contain names
        // no commit.
        assert!(latest_for_commit(&db, &repo, "abc\x00123")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn an_unknown_state_is_refused_by_name() {
        // Defaulting would be the worst answer available: quietly green
        // ships broken code, quietly red blocks good code, and neither
        // leaves a trace of having guessed.
        assert_eq!(RunState::parse("passing").unwrap(), RunState::Passing);
        for s in ["queued", "running", "failing", "cancelled", "skipped"] {
            assert_eq!(RunState::parse(s).unwrap().as_str(), s);
        }
        let e = RunState::parse("success").unwrap_err();
        assert!(e.contains("success"), "{e}");
        assert!(
            e.contains("passing"),
            "the refusal did not say what is valid: {e}"
        );
    }

    #[test]
    fn a_run_is_readable_only_through_the_repository_that_owns_it() {
        // The one that matters is the middle assertion. Both
        // repositories are public and both belong to the same person, so
        // nothing but the `repo_id` in the WHERE clause is standing
        // between a caller and somebody else's run — which is exactly
        // the condition a fetch-then-compare would also pass today and
        // fail after the next well-meaning edit.
        let db = db("checks_get");
        let (ns, mine) = fixture(&db);
        let theirs = repo(&db, &ns, "gadget");

        let run_of_mine = upsert(&db, &mine, &run("build", RunState::Passing)).unwrap();
        let run_of_theirs = upsert(&db, &theirs, &run("build", RunState::Failing)).unwrap();

        let got = get(&db, &mine, &run_of_mine.id).unwrap().unwrap();
        assert_eq!(got.id, run_of_mine.id);
        assert_eq!(got.state, "passing");

        assert!(
            get(&db, &mine, &run_of_theirs.id).unwrap().is_none(),
            "another repository's run was readable through this one's path"
        );

        // And the three ways of saying no are one answer: an id that was
        // never minted, and an id of a shape that could never be minted.
        assert!(get(&db, &mine, &ulid()).unwrap().is_none());
        assert!(
            get(&db, &mine, "01m12p1wxk\0ce413pcqnk7wm")
                .unwrap()
                .is_none(),
            "a control byte in the path reached the database"
        );
    }

    #[test]
    fn a_deleted_repository_takes_its_check_runs_with_it() {
        // `ON DELETE CASCADE`, asserted rather than assumed: an orphaned
        // run row would be shown by nothing and would surface much later
        // as a foreign-key failure on an unrelated write.
        let db = db("checks_cascade");
        let (_, repo) = fixture(&db);
        upsert(&db, &repo, &run("build", RunState::Passing)).unwrap();
        assert_eq!(count(&db, &repo), 1);

        db.lock()
            .execute("DELETE FROM repos WHERE id = $1", &[&repo])
            .unwrap();
        assert_eq!(count(&db, &repo), 0);
    }

    /// A provider's row can be removed by the id that provider gave it —
    /// in that repository, from that provider, and nowhere else.
    ///
    /// The one delete in this module, and the scoping is the whole test:
    /// approving a fork's workflows removes a placeholder row by run id,
    /// and an unscoped delete would let one repository's tidy-up reach
    /// another repository's checks, or one provider's id collide with
    /// another's.
    #[test]
    fn a_row_is_deleted_by_its_providers_own_id_and_only_in_its_own_repository() {
        let db = db("checks-delete-external");
        let (ns, repo_a) = fixture(&db);
        let repo_b = repo(&db, &ns, "other");

        let mine = NewCheckRun {
            provider: "weft",
            external_id: Some("run-1"),
            ..run("ci / test", RunState::Queued)
        };
        upsert(&db, &repo_a, &mine).unwrap();
        // The same external id in another repository, and the same id
        // from another provider in this one. Neither may be touched.
        upsert(&db, &repo_b, &mine).unwrap();
        upsert(
            &db,
            &repo_a,
            &NewCheckRun {
                provider: "github",
                external_id: Some("run-1"),
                ..run("build", RunState::Queued)
            },
        )
        .unwrap();

        assert!(delete_external(&db, &repo_a, "weft", "run-1").unwrap());
        assert_eq!(
            count(&db, &repo_a),
            1,
            "the github row is not ours to delete"
        );
        assert_eq!(count(&db, &repo_b), 1, "another repository's row survived");
        assert!(
            list(
                &db,
                &repo_a,
                &RunQuery {
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap()
            .0
            .iter()
            .all(|r| r.provider == "github"),
            "the stratum row is gone and the github one is not"
        );

        // Nothing matched is not an error: the caller is tidying up
        // after a row that may never have been written.
        assert!(!delete_external(&db, &repo_a, "weft", "run-1").unwrap());
        assert!(!delete_external(&db, &repo_a, "weft", "never-minted").unwrap());
        // Hostile and empty ids are "nothing matched" rather than a
        // database error and a 500 — same guard as `get`.
        assert!(!delete_external(&db, &repo_a, "weft", "").unwrap());
        assert!(!delete_external(&db, &repo_a, "weft", "run\u{0}1").unwrap());
    }
}
