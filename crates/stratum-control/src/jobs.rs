//! Background jobs: export bundles now; compaction/GC sweeps next. Rows
//! are lease-shaped (claim with a lease_until) so the single-node loop
//! and a future multi-node fleet use the same discipline.

use crate::db::{detail, ControlDb};
use crate::ids::{now_ms, ulid};
use serde::Serialize;

/// How many times one row may be claimed before it is dead-lettered.
///
/// `claim` reclaims a `running` row whose lease has expired, which is the
/// whole point of a lease: a node that dies mid-job must not strand the
/// work. But a job that kills its node — an OOM on one enormous repo's
/// compaction — is *also* a row whose lease expires, and without a cap it
/// is a poison pill that every node in the fleet claims in turn and dies
/// on. Past the cap the row goes to the terminal `failed` state naming
/// the repo, so the queue drains and an operator has something to read.
const DEFAULT_MAX_ATTEMPTS: i64 = 5;

/// Advisory-lock namespace for the periodic workers. PostgreSQL keeps
/// two-argument advisory locks in a *different* space from one-argument
/// ones, so this cannot collide with `db::MIGRATE_LOCK_KEY` no matter
/// what the second key hashes to.
const WORKER_LOCK_NAMESPACE: i32 = 0x5354_5241; // "STRA"

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    pub org_id: String,
    pub repo_id: Option<String>,
    pub kind: String,
    pub state: String,
    pub payload: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub attempts: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// How many times a piece of work may be tried before the queue gives
/// up on it (`STRATUM_JOB_MAX_ATTEMPTS`, default 5). `claim` applies it
/// to one row's claims; the landers apply the same number to the jobs
/// they mint for one landing, because a rescue that makes a *fresh* job
/// every time would otherwise never meet a cap at all.
pub fn max_attempts() -> i64 {
    std::env::var("STRATUM_JOB_MAX_ATTEMPTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
}

pub fn create(
    db: &ControlDb,
    org_id: &str,
    repo_id: Option<&str>,
    kind: &str,
    payload: Option<&str>,
) -> Result<Job, String> {
    let id = ulid();
    let now = now_ms();
    db.lock()
        .execute(
            "INSERT INTO jobs (id, org_id, repo_id, kind, payload, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $6)",
            &[&id, &org_id, &repo_id, &kind, &payload, &now],
        )
        .map_err(|e| detail(&e))?;
    get(db, org_id, &id)?.ok_or_else(|| "job vanished".into())
}

/// The kinds an `ON CONFLICT DO NOTHING` insert can actually collapse,
/// and the index that does it for each.
///
/// This list is the whole safety of [`enqueue_unique`], because the
/// statement it runs names **no conflict target**: it deduplicates
/// against whichever partial unique index happens to match the row, and
/// against nothing at all for a kind no index covers. That is a silent
/// failure by construction — the call compiles, the insert succeeds, and
/// the function's name is the only thing that says anything happened.
/// It has already gone wrong twice, and the second time reached users:
///
///   * migration 0037 found `import` and `checkspoll` outside the index
///     they were written to be inside;
///   * `notify-mail` was outside every index from the day it landed, so
///     a twelve-comment review sent twelve emails — the one failure this
///     queue exists to prevent, in the feature people judge a forge by.
///
/// So the kinds are enumerated here and an unlisted one is **refused**
/// rather than quietly inserted. A caller who means "many active rows
/// are fine" wants [`create`]; a caller who means "one" adds their kind
/// to a partial unique index in a new migration and to this list, in the
/// same change, or finds out from a test rather than from a mailbox.
///
///   * `compact`, `cdnpack`, `fork`, `promote`, `import`, `checkspoll`
///     — `jobs_active_per_repo` (0019, 0026, 0037), keyed on
///     `(kind, repo_id)` while a row is queued **or running**: the work
///     is a sweep of the repository's current state, so one run subsumes
///     a queued duplicate and two runs race.
///   * `notify-mail`, `notify-changeset` — `jobs_pending_notify` (0049),
///     keyed on the payload as well and only while a row is **queued**:
///     what makes two notifications the same is that they say the same
///     sentence about the same thing, which is what the payload is, and
///     a job already being sent cannot absorb an event that happened
///     after it was claimed.
const DEDUPLICATED_KINDS: &[&str] = &[
    "compact",
    "cdnpack",
    "fork",
    "promote",
    "import",
    "checkspoll",
    "notify-mail",
    "notify-changeset",
    // `sitepublish` — `jobs_active_per_repo` (0110). One publish per
    // repository at a time: the job reads the tip when it runs, so a
    // second push while one is queued is already covered by the one
    // that will run.
    "sitepublish",
];

/// Enqueue at most one active job of `kind` for `repo_id`, returning
/// `None` when the queue already holds the one this would duplicate.
///
/// The dedup is a partial unique index, not a preceding `SELECT`.
/// Read-then-insert is two statements with nothing holding between them,
/// so two nodes accepting two pushes in the same instant both read "no
/// active job" and both insert — and the claim side then correctly hands
/// the two rows to two different workers, which fold the same prefix at
/// the same time. The index makes the second insert a no-op inside the
/// one statement that decides.
///
/// Which index, and therefore what "the same job" means, is per kind:
/// see [`DEDUPLICATED_KINDS`], which also says why a kind no index covers
/// is an error here rather than a silent `create`.
pub fn enqueue_unique(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    kind: &str,
    payload: Option<&str>,
) -> Result<Option<Job>, String> {
    insert_unique(db, org_id, Some(repo_id), kind, payload, 0)
}

/// [`enqueue_unique`] for work that belongs to the organization rather
/// than to one repository — a changeset notification, whose subject
/// spans several repositories and so is none of them.
///
/// A separate entry point rather than an `Option<&str>` on the one
/// above, because every existing caller names a repository and reading
/// `enqueue_unique(db, org, None, …)` at a call site tells the next
/// person nothing about which of the two things they are doing.
/// Enqueue as [`enqueue_unique`], but not claimable until `not_before`.
///
/// For a worker a provider has told to slow down. Holding the *worker*
/// would be the easy way and it is the wrong one: one organization's
/// rate limit would park every other organization's work behind it. The
/// row waits instead, and the worker moves on to whatever else is ready.
///
/// `not_before` is epoch milliseconds; a value in the past is simply
/// ready, which is what makes 0 the right default everywhere else.
pub fn enqueue_unique_after(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    kind: &str,
    payload: Option<&str>,
    not_before: i64,
) -> Result<Option<Job>, String> {
    insert_unique(db, org_id, Some(repo_id), kind, payload, not_before)
}

pub fn enqueue_unique_org(
    db: &ControlDb,
    org_id: &str,
    kind: &str,
    payload: Option<&str>,
) -> Result<Option<Job>, String> {
    insert_unique(db, org_id, None, kind, payload, 0)
}

fn insert_unique(
    db: &ControlDb,
    org_id: &str,
    repo_id: Option<&str>,
    kind: &str,
    payload: Option<&str>,
    not_before: i64,
) -> Result<Option<Job>, String> {
    if !DEDUPLICATED_KINDS.contains(&kind) {
        // Loud, and at the call the mistake is in. The alternative is
        // the bug this list documents: an insert that succeeds, a name
        // that promises a dedup nothing performs, and a symptom that
        // only shows up as somebody's inbox.
        return Err(format!(
            "no unique index covers job kind {kind:?}, so enqueue_unique cannot \
             deduplicate it: use jobs::create, or add the kind to a partial unique \
             index in a new migration and to jobs::DEDUPLICATED_KINDS"
        ));
    }
    let id = ulid();
    let now = now_ms();
    let inserted = db
        .lock()
        .execute(
            "INSERT INTO jobs (id, org_id, repo_id, kind, payload, created_at, updated_at, \
             not_before) VALUES ($1, $2, $3, $4, $5, $6, $6, $7) ON CONFLICT DO NOTHING",
            &[&id, &org_id, &repo_id, &kind, &payload, &now, &not_before],
        )
        .map_err(|e| detail(&e))?;
    if inserted == 0 {
        return Ok(None);
    }
    get(db, org_id, &id)
}

pub fn get(db: &ControlDb, org_id: &str, id: &str) -> Result<Option<Job>, String> {
    db.lock()
        .query_opt(
            "SELECT id, org_id, repo_id, kind, state, payload, result, error, attempts, \
             created_at, updated_at FROM jobs WHERE org_id = $1 AND id = $2",
            &[&org_id, &id],
        )
        .map(|row| {
            row.map(|r| Job {
                id: r.get(0),
                org_id: r.get(1),
                repo_id: r.get(2),
                kind: r.get(3),
                state: r.get(4),
                payload: r.get(5),
                result: r.get(6),
                error: r.get(7),
                attempts: r.get(8),
                created_at: r.get(9),
                updated_at: r.get(10),
            })
        })
        .map_err(|e| detail(&e))
}

/// The most recent job of `kind` for one repository.
///
/// Exists so a surface can say **what happened**, not just how far a
/// worker got. An import that was refused and an import still walking
/// its first page leave identical phase cursors, and for as long as this
/// was unreadable the API could not tell them apart — which is precisely
/// the confusion `importer.rs` opens by saying must never happen: "an
/// import that quietly produces nothing looks exactly like a project
/// that never had issues".
///
/// Ordered by `created_at` and broken by `id`, which is a ULID and so
/// monotonic within a millisecond: two jobs enqueued in the same tick
/// would otherwise come back in an order the database chose.
pub fn latest_for_repo(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    kind: &str,
) -> Result<Option<Job>, String> {
    db.lock()
        .query_opt(
            "SELECT id, org_id, repo_id, kind, state, payload, result, error, attempts, \
             created_at, updated_at FROM jobs \
             WHERE org_id = $1 AND repo_id = $2 AND kind = $3 \
             ORDER BY created_at DESC, id DESC LIMIT 1",
            &[&org_id, &repo_id, &kind],
        )
        .map(|row| {
            row.map(|r| Job {
                id: r.get(0),
                org_id: r.get(1),
                repo_id: r.get(2),
                kind: r.get(3),
                state: r.get(4),
                payload: r.get(5),
                result: r.get(6),
                error: r.get(7),
                attempts: r.get(8),
                created_at: r.get(9),
                updated_at: r.get(10),
            })
        })
        .map_err(|e| detail(&e))
}

/// Claim one queued (or lease-expired running) job of `kind`, taking a
/// lease. Returns None when the queue is empty.
pub fn claim(db: &ControlDb, kind: &str, lease_ms: i64) -> Result<Option<Job>, String> {
    let now = now_ms();
    let cap = max_attempts();
    // Dead-letter first, so a poison row is out of the way before the
    // claim below looks: a job that kills its worker rather than
    // returning an error leaves an expired lease behind, which is
    // indistinguishable from a node that was deployed over — except in
    // how many times it has happened.
    db.lock()
        .execute(
            "UPDATE jobs SET state = 'failed', updated_at = $3, \
             error = 'dead-lettered after ' || attempts || ' attempts (repo ' \
                 || COALESCE(repo_id, '-') || ')' \
             WHERE kind = $1 AND state = 'running' AND lease_until < $3 AND attempts >= $2",
            &[&kind, &cap, &now],
        )
        .map_err(|e| detail(&e))?;
    // Single-statement claim: FOR UPDATE SKIP LOCKED makes concurrent
    // claimers (a multi-node fleet) skip rows another worker is taking
    // instead of double-claiming or queueing behind it.
    let row: Option<(String, String)> = db
        .lock()
        .query_opt(
            "UPDATE jobs SET state = 'running', lease_until = $2, \
             attempts = attempts + 1, updated_at = $3 \
             WHERE id = (SELECT id FROM jobs WHERE kind = $1 AND attempts < $4 AND \
                 not_before <= $3 AND \
                 (state = 'queued' OR (state = 'running' AND lease_until < $3)) \
                 ORDER BY created_at LIMIT 1 FOR UPDATE SKIP LOCKED) \
             RETURNING id, org_id",
            &[&kind, &(now + lease_ms), &now, &cap],
        )
        .map_err(|e| detail(&e))?
        .map(|r| (r.get(0), r.get(1)));
    let Some((id, org_id)) = row else {
        return Ok(None);
    };
    get(db, &org_id, &id)
}

pub fn complete(db: &ControlDb, id: &str, result: Option<&str>) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE jobs SET state = 'done', result = $2, error = NULL, updated_at = $3 \
             WHERE id = $1",
            &[&id, &result, &now_ms()],
        )
        .map(|_| ())
        .map_err(|e| detail(&e))
}

pub fn fail(db: &ControlDb, id: &str, error: &str) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE jobs SET state = 'failed', error = $2, updated_at = $3 WHERE id = $1",
            &[&id, &error, &now_ms()],
        )
        .map(|_| ())
        .map_err(|e| detail(&e))
}

/// Move a worker cursor forward, never backward, returning the value now
/// stored.
///
/// A cursor is the one piece of worker state a *periodic* sweep carries
/// between runs, and an unconditional upsert makes it the fleet's weakest
/// link: two nodes that read the same cursor and finish out of order
/// leave the slower one's smaller value behind, and the next run re-does
/// work that was already published. `GREATEST` makes the write express
/// what the caller actually means — "this much is done" — so an
/// out-of-order writer is a no-op rather than a rewind.
///
/// It lives here rather than beside `meta_set` because it is the same
/// concern as the leases above: what a fleet is allowed to do to shared
/// worker state. `meta_set` remains correct for values that are simply
/// the latest word.
pub fn advance_cursor(db: &ControlDb, key: &str, to: i64) -> Result<i64, String> {
    let value = to.to_string();
    let row = db
        .lock()
        .query_one(
            "INSERT INTO meta (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE \
             SET value = GREATEST(meta.value::BIGINT, EXCLUDED.value::BIGINT)::TEXT \
             RETURNING value::BIGINT",
            &[&key, &value],
        )
        .map_err(|e| detail(&e))?;
    Ok(row.get(0))
}

/// A held fleet-wide worker lock. Released on drop, and by PostgreSQL
/// itself if this node dies while holding it.
pub struct WorkerLock {
    db: ControlDb,
    key: i32,
    name: &'static str,
}

impl Drop for WorkerLock {
    fn drop(&mut self) {
        if let Err(e) = self.db.lock().execute(
            "SELECT pg_advisory_unlock($1, $2)",
            &[&WORKER_LOCK_NAMESPACE, &self.key],
        ) {
            eprintln!("weft: releasing worker lock {}: {e}", self.name);
        }
    }
}

/// Try to take the fleet-wide lock for a periodic worker, returning
/// `None` when another node holds it.
///
/// The *queue* workers (compact, cdnpack, land) are deduplicated by their
/// rows: there is a thing to claim, and `FOR UPDATE SKIP LOCKED` decides
/// who claims it. The periodic ones have no row — the tick itself is the
/// work — so a job row would have to be manufactured every tick by every
/// node just to be thrown away, and the row's *lease* would then be a
/// second clock to get wrong. A session advisory lock says the same thing
/// with no rows and no timeout: exactly one node in the fleet is inside
/// the sweep, and if that node dies the lock dies with its session rather
/// than waiting out a lease.
///
/// The one way it can be lost early is a mid-sweep reconnect (see
/// `db::ClientBox::reconnect`), which drops session state. That costs a
/// duplicated sweep, not a corrupted one — which is why the audit
/// shipper's cursor is monotonic (`advance_cursor`) as well as locked.
pub fn try_lock(db: &ControlDb, name: &'static str) -> Result<Option<WorkerLock>, String> {
    let key = lock_key(name);
    let taken: bool = db
        .lock()
        .query_one(
            "SELECT pg_try_advisory_lock($1, $2)",
            &[&WORKER_LOCK_NAMESPACE, &key],
        )
        .map(|r| r.get(0))
        .map_err(|e| detail(&e))?;
    if !taken {
        return Ok(None);
    }
    Ok(Some(WorkerLock {
        db: db.clone(),
        key,
        name,
    }))
}

/// The same lock, scoped to one thing — a repository — rather than to a
/// worker. A mirror is synced by whichever node's webhook, poll tick or
/// sync request gets there first, and on a two-node fleet two of those
/// arrive within the same second: both fetch the origin, both build the
/// same layout, and one of them loses the manifest swap and logs a
/// failure for work that was not needed. Taken around the sync, the
/// second node sees the lock held and does nothing — the first node's
/// result is the result.
///
/// `name` is the `&'static str` the lock is reported under; `scope` is
/// hashed with it, so two repositories never contend and a repository's
/// sync never contends with a periodic worker. Held by the session for
/// as long as the guard lives; the same session may take it again.
pub fn try_lock_scoped(
    db: &ControlDb,
    name: &'static str,
    scope: &str,
) -> Result<Option<WorkerLock>, String> {
    let key = lock_key(&format!("{name}:{scope}"));
    let taken: bool = db
        .lock()
        .query_one(
            "SELECT pg_try_advisory_lock($1, $2)",
            &[&WORKER_LOCK_NAMESPACE, &key],
        )
        .map(|r| r.get(0))
        .map_err(|e| detail(&e))?;
    if !taken {
        return Ok(None);
    }
    Ok(Some(WorkerLock {
        db: db.clone(),
        key,
        name,
    }))
}

/// [`try_lock_scoped`], but waiting for the holder to finish.
///
/// A sync that finds the lock held can walk away — the holder's result
/// is the result. A push being forwarded to a mirror's origin cannot:
/// its answer to the client is "the origin took it and the mirror
/// shows it", and the mirror shows it only once *this* node's sync has
/// landed, which needs the lock. Walking away would answer `ok` for a
/// ref the manifest does not yet carry — the silent stale miss the
/// freshness contract forbids. So this polls, and past `timeout` says
/// so with `Ok(None)`, having changed nothing.
pub fn lock_scoped_wait(
    db: &ControlDb,
    name: &'static str,
    scope: &str,
    timeout: std::time::Duration,
) -> Result<Option<WorkerLock>, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(guard) = try_lock_scoped(db, name, scope)? {
            return Ok(Some(guard));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// FNV-1a over the worker name. Any stable mapping would do — the names
/// are a closed set fixed in this repository, and the namespace above
/// keeps the whole space away from the migration lock.
fn lock_key(name: &str) -> i32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in name.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database error out of this module says what went wrong.
    ///
    /// `postgres::Error` renders through `Display` as, literally, `db
    /// error`. The SQLSTATE, the server's message and the constraint name
    /// all live in `as_db_error()`, and eleven call sites in this file
    /// threw them away — so the single most common CI failure in this
    /// repository read, in full:
    ///
    /// ```text
    /// enqueue a poll: "db error"
    /// ```
    ///
    /// Nine of fourteen runs were red when this was found, `main`
    /// included, and there was no way to tell why. `db::detail` already
    /// existed for exactly this — `migrate_locked` uses it, with a
    /// comment making the same argument about deploys — and five other
    /// modules had each worked around the bare string locally rather than
    /// fixing it at the source (`registry.rs:637` records an operator
    /// meeting it in production).
    #[test]
    fn a_database_error_names_itself_instead_of_saying_db_error() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-detail")).unwrap();
        // Take the table away, in this test's own database, so the next
        // call fails for a reason the message must be able to carry.
        db.lock().execute("DROP TABLE jobs", &[]).unwrap();

        let e = create(&db, "org1", Some("r1"), "export", None)
            .expect_err("the table is gone; this cannot succeed");

        assert!(
            e.contains("42P01"),
            "the SQLSTATE is what makes a failure searchable: {e}"
        );
        assert!(e.contains("jobs"), "the message does not say what: {e}");
        assert_ne!(e, "db error", "the cause was thrown away again");
    }

    /// And it stays that way.
    ///
    /// A guard on the source rather than on behaviour, because the
    /// failure mode is a *new* call site copying the old pattern — which
    /// is exactly how eleven of them accumulated. `detail` falls back to
    /// `to_string` itself when there is no `DbError` (a dropped
    /// connection), so nothing is lost by never reaching for it directly.
    #[test]
    fn no_database_call_in_this_module_stringifies_its_error_bare() {
        const SOURCE: &str = include_str!("jobs.rs");
        // Assembled rather than written out, or this line matches itself
        // and the guard reports the guard. It did, on the first run.
        let needle = format!("map_err(|e| e.{}())", "to_string");
        let offenders: Vec<(usize, &str)> = SOURCE
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains(&needle))
            .map(|(i, l)| (i + 1, l.trim()))
            .collect();
        assert!(
            offenders.is_empty(),
            "these throw the cause away; use `detail(&e)`: {offenders:?}"
        );
    }

    #[test]
    fn lifecycle_and_lease_claims() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-jobs")).unwrap();
        let j = create(&db, "org1", Some("r1"), "export", Some("{}")).unwrap();
        assert_eq!(j.state, "queued");

        let claimed = claim(&db, "export", 60_000).unwrap().unwrap();
        assert_eq!(claimed.id, j.id);
        assert_eq!(claimed.state, "running");
        // A live lease blocks re-claim.
        assert!(claim(&db, "export", 60_000).unwrap().is_none());

        complete(&db, &j.id, Some("key")).unwrap();
        let done = get(&db, "org1", &j.id).unwrap().unwrap();
        assert_eq!(done.state, "done");
        assert_eq!(done.result.as_deref(), Some("key"));

        // Expired leases are reclaimable.
        let j2 = create(&db, "org1", None, "export", None).unwrap();
        claim(&db, "export", -1).unwrap().unwrap();
        let re = claim(&db, "export", 60_000).unwrap().unwrap();
        assert_eq!(re.id, j2.id);
        assert_eq!(re.attempts, 2);
    }

    /// A job that *kills* its worker leaves an expired lease, exactly
    /// like a node that was deployed over — so without a cap it is
    /// reclaimed forever, and on a fleet it takes every node down in
    /// turn. Past the cap the row is terminal and names its repo.
    /// A deferred row is invisible to `claim` until its moment, and the
    /// queue keeps moving in the meantime.
    ///
    /// The second half is the point. Honouring a provider's `Retry-After`
    /// by parking the worker would be easy and wrong: one organization's
    /// rate limit would hold every other organization's work behind it.
    /// The row waits; the worker does not.
    #[test]
    fn a_deferred_job_waits_its_turn_without_holding_up_the_queue() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-defer")).unwrap();
        let future = now_ms() + 60_000;

        let held = enqueue_unique_after(&db, "org1", "repo-held", "import", None, future)
            .unwrap()
            .expect("a deferred row is still a row");
        assert_eq!(held.state, "queued", "deferred is queued, not a new state");

        // Not claimable, however many times anyone asks.
        for _ in 0..3 {
            assert!(
                claim(&db, "import", 60_000).unwrap().is_none(),
                "a deferred job was claimed before its time"
            );
        }

        // And it is not blocking the queue: a ready row behind it goes.
        let ready = enqueue_unique_after(&db, "org1", "repo-ready", "import", None, 0)
            .unwrap()
            .expect("a ready row");
        let got = claim(&db, "import", 60_000)
            .unwrap()
            .expect("the ready row was not claimed");
        assert_eq!(
            got.id, ready.id,
            "the deferred row was claimed ahead of the ready one"
        );

        // Once its moment has passed it is ordinary again. Moving the row
        // rather than sleeping keeps the test honest about what is being
        // checked — that `claim` reads the column — without spending a
        // minute proving it.
        db.lock()
            .execute(
                "UPDATE jobs SET not_before = $2 WHERE id = $1",
                &[&held.id, &(now_ms() - 1)],
            )
            .unwrap();
        let got = claim(&db, "import", 60_000)
            .unwrap()
            .expect("a job past its not_before was still invisible");
        assert_eq!(got.id, held.id);
    }

    /// The ordinary enqueue is unchanged: no caller that does not care
    /// about deferral has to say so, and every row before this migration
    /// reads as ready.
    #[test]
    fn an_undeferred_job_is_claimable_at_once() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-nodefer")).unwrap();
        let j = enqueue_unique(&db, "org1", "repo-now", "import", None)
            .unwrap()
            .expect("a row");
        let got = claim(&db, "import", 60_000).unwrap().expect("claimable");
        assert_eq!(got.id, j.id);
    }

    #[test]
    fn a_poison_job_is_dead_lettered_instead_of_reclaimed_forever() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-poison")).unwrap();
        let cap = max_attempts();
        let j = create(&db, "org1", Some("repo-poison"), "compact", None).unwrap();

        // Every claim is followed by the worker dying: nothing completes
        // the row, the lease simply expires (a negative lease is "already
        // expired" without sleeping).
        for n in 1..=cap {
            let got = claim(&db, "compact", -1)
                .unwrap()
                .unwrap_or_else(|| panic!("attempt {n} should still be claimable"));
            assert_eq!(got.id, j.id);
            assert_eq!(got.attempts, n);
        }

        // Cap reached: the next claim finds nothing, and the row is
        // terminal rather than queued behind a lease that never ends.
        assert!(claim(&db, "compact", -1).unwrap().is_none());
        let dead = get(&db, "org1", &j.id).unwrap().unwrap();
        assert_eq!(dead.state, "failed", "{dead:?}");
        assert_eq!(dead.attempts, cap);
        let error = dead.error.unwrap();
        assert!(error.contains("dead-lettered"), "{error}");
        assert!(error.contains("repo-poison"), "{error}");

        // And it stays dead: further polls never resurrect it.
        assert!(claim(&db, "compact", -1).unwrap().is_none());
        assert_eq!(
            get(&db, "org1", &j.id).unwrap().unwrap().state,
            "failed",
            "a dead-lettered row must stay terminal"
        );
    }

    /// The dedup that used to be `exists_active` + `create`: two nodes
    /// enqueueing in the same instant must produce one active row, and
    /// the *next* one only after the first is out of the way.
    #[test]
    fn only_one_sweep_job_per_repo_is_active_at_a_time() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-dedup")).unwrap();
        let first = enqueue_unique(&db, "org1", "r1", "compact", None)
            .unwrap()
            .expect("first enqueue wins");
        assert!(enqueue_unique(&db, "org1", "r1", "compact", None)
            .unwrap()
            .is_none());

        // A different kind, and a different repo, are different work.
        assert!(enqueue_unique(&db, "org1", "r1", "cdnpack", None)
            .unwrap()
            .is_some());
        assert!(enqueue_unique(&db, "org1", "r2", "compact", None)
            .unwrap()
            .is_some());

        // Claiming does not free the slot — the job is still active.
        claim(&db, "compact", 60_000).unwrap().unwrap();
        assert!(enqueue_unique(&db, "org1", "r1", "compact", None)
            .unwrap()
            .is_none());

        // Finishing it does.
        complete(&db, &first.id, None).unwrap();
        assert!(enqueue_unique(&db, "org1", "r1", "compact", None)
            .unwrap()
            .is_some());
    }

    /// Kinds a person triggers, or that name a unit of their own, are
    /// deliberately outside the index: two exports of one repo, two
    /// changes landing on one repo, and two operators hitting
    /// `POST /compact` are all legitimate.
    #[test]
    fn kinds_that_are_not_sweeps_may_have_many_active_rows() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-multi")).unwrap();
        for kind in ["export", "land", "compact-now", "cdnpack-now"] {
            create(&db, "org1", Some("r1"), kind, None).unwrap();
            create(&db, "org1", Some("r1"), kind, None)
                .unwrap_or_else(|e| panic!("{kind} must allow a second active row: {e}"));
        }
        // Org-scoped rows carry no repo and are never deduplicated by it.
        create(&db, "org1", None, "compact", None).unwrap();
        create(&db, "org1", None, "compact", None).unwrap();
    }

    /// The boundary of `enqueue_unique`, on purpose and in both
    /// directions: what it deduplicates, how, and what it does with a
    /// kind nothing can deduplicate.
    ///
    /// The middle case is the one that shipped broken. `notify-mail` was
    /// covered by no unique index at all, so the `ON CONFLICT DO
    /// NOTHING` behind this function matched nothing and every enqueue
    /// inserted: twelve comments on one change, twelve emails. Nothing
    /// failed, because a name is the only thing that promised otherwise.
    #[test]
    fn a_notification_deduplicates_by_what_it_would_say_and_an_uncovered_kind_is_refused() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-notifydedup")).unwrap();
        let comment = r#"{"change_id":"c1","event":"commented","actor":"u1"}"#;

        // Covered by `jobs_active_per_repo`: one sweep per repository.
        assert!(enqueue_unique(&db, "org1", "r1", "compact", None)
            .unwrap()
            .is_some());
        assert!(enqueue_unique(&db, "org1", "r1", "compact", None)
            .unwrap()
            .is_none());

        // Covered by `jobs_pending_notify`: one *pending sentence*.
        assert!(
            enqueue_unique(&db, "org1", "r1", "notify-mail", Some(comment))
                .unwrap()
                .is_some()
        );
        assert!(
            enqueue_unique(&db, "org1", "r1", "notify-mail", Some(comment))
                .unwrap()
                .is_none(),
            "a second identical notification was queued: the review that \
             gets twelve comments sends twelve emails"
        );

        // A different event about the same change is a different
        // sentence and must still be sent — this is what a per-repo key
        // would have swallowed.
        let opened = r#"{"change_id":"c1","event":"opened","actor":"u2"}"#;
        assert!(
            enqueue_unique(&db, "org1", "r1", "notify-mail", Some(opened))
                .unwrap()
                .is_some(),
            "a different event about the same change was dropped"
        );

        // Claiming frees the slot, unlike a sweep: the job is being sent
        // and cannot carry something that happened after it was claimed.
        let claimed = claim(&db, "notify-mail", 60_000).unwrap().unwrap();
        assert_eq!(claimed.payload.as_deref(), Some(comment));
        assert!(
            enqueue_unique(&db, "org1", "r1", "notify-mail", Some(comment))
                .unwrap()
                .is_some(),
            "an event that happened while a send was in flight was folded \
             into that send and lost"
        );

        // Org-scoped notifications — a changeset belongs to no one
        // repository — are keyed the same way.
        let composed = r#"{"changeset_id":"cs1","event":"composed","actor":"u1"}"#;
        assert!(
            enqueue_unique_org(&db, "org1", "notify-changeset", Some(composed))
                .unwrap()
                .is_some()
        );
        assert!(
            enqueue_unique_org(&db, "org1", "notify-changeset", Some(composed))
                .unwrap()
                .is_none()
        );
        assert!(
            enqueue_unique_org(&db, "org2", "notify-changeset", Some(composed))
                .unwrap()
                .is_some(),
            "one org's changeset notification suppressed another org's"
        );

        // And a kind no index covers is refused rather than inserted
        // under a name that promises a dedup it cannot perform.
        let e = enqueue_unique(&db, "org1", "r1", "export", None).unwrap_err();
        assert!(e.contains("export") && e.contains("jobs::create"), "{e}");
    }

    #[test]
    fn a_cursor_only_ever_moves_forward() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-cursor")).unwrap();
        assert_eq!(advance_cursor(&db, "c", 100).unwrap(), 100);
        assert_eq!(advance_cursor(&db, "c", 250).unwrap(), 250);
        // The slow node's write is a no-op, not a rewind.
        assert_eq!(advance_cursor(&db, "c", 120).unwrap(), 250);
        assert_eq!(advance_cursor(&db, "c", 250).unwrap(), 250);
        // Cursors are per key.
        assert_eq!(advance_cursor(&db, "d", 7).unwrap(), 7);
        assert_eq!(advance_cursor(&db, "c", 251).unwrap(), 251);
    }

    #[test]
    fn a_worker_lock_admits_one_node_and_is_released_on_drop() {
        let url = stratum_testkit::pg::test_db_url("control-worklock");
        // Two handles = two sessions, which is what two nodes are.
        let a = ControlDb::open(&url).unwrap();
        let b = ControlDb::open(&url).unwrap();

        let held = try_lock(&a, "audit-shipper").unwrap().expect("a wins");
        assert!(try_lock(&b, "audit-shipper").unwrap().is_none());
        // A different worker is a different lock — one node must not
        // shut the whole fleet's sweeps out.
        let other = try_lock(&b, "gc-sweep").unwrap().expect("different key");
        drop(other);

        drop(held);
        let now_b = try_lock(&b, "audit-shipper")
            .unwrap()
            .expect("released on drop");
        assert!(try_lock(&a, "audit-shipper").unwrap().is_none());
        drop(now_b);
    }

    /// The fleet-wide lock is released in `Drop`, and `Drop` runs during
    /// unwinding: a node whose database has gone away — the case the lock
    /// was designed around, since PostgreSQL releases it for us — must log
    /// the failed unlock and carry on, never panic and abort the process.
    /// Needs its own cluster, because the point is to stop it.
    #[test]
    fn losing_the_database_does_not_make_releasing_a_lock_panic() {
        let pg = stratum_testkit::pg::Pg::start().expect("start a private cluster");
        let db = ControlDb::open(&pg.database("control-worklock-lost")).unwrap();
        let held = try_lock(&db, "audit-shipper").unwrap().expect("taken");
        drop(pg);
        // The unlock fails, the reconnect fails, and the lock is dropped
        // all the same.
        drop(held);
    }

    #[test]
    fn lock_keys_are_stable_and_distinct_per_worker() {
        assert_eq!(lock_key("audit-shipper"), lock_key("audit-shipper"));
        assert_ne!(lock_key("audit-shipper"), lock_key("gc-sweep"));
        assert_ne!(lock_key("gc-sweep"), lock_key("billing-rollup"));
    }
}
