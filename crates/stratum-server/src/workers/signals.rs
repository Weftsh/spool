//! The daily signals rollup: every repository's per-day counters kept
//! current, so Pulse and the org-wide insights page are a read of one
//! small table instead of a scan of the whole tenant.
//!
//! **A periodic sweep, not a queue.** The other shape was considered and
//! is wrong here for the reason [`jobs::try_lock`] gives: a queue worker
//! needs a row to claim, and there is no event that makes a rollup
//! stale. Nothing *happens* at 09:00 to make a row summed at 00:05 out
//! of date — only time passes. Manufacturing a job row every tick just
//! to throw it away would add a lease, which is a second clock to get
//! wrong on top of `repo_signals.rolled_at`, which already answers the
//! same question better. So: an advisory lock, exactly one node in the
//! fleet inside the sweep, and if that node dies the lock dies with its
//! session rather than waiting out a lease.
//!
//! **The sibling to copy here is [`super::billing`], not
//! [`super::contribs`]** — said explicitly because most of this
//! directory is queue workers, and "make it match its neighbours" is
//! precisely the change that would break it. A queue worker is right
//! when a push creates work; a sweep is right when the passage of time
//! does.
//!
//! **Two cadences, because there are two questions.** Keeping an open
//! day current is a read of `repo_signals` alone
//! ([`signals::due_open`]); *discovering* a day no row exists for is a
//! scan of every source table ([`signals::due_backfill`]). The first is
//! the steady state and runs on every tick. The second is what an import
//! or a contribution walk creates, plus one row per active repository
//! when the date rolls over, and it runs on its own much slower timer —
//! `STRATUM_SIGNALS_BACKFILL_SECS`, an hour by default.
//!
//! That interval is the whole cost of the arrangement and it is worth
//! stating plainly: a repository's **first** activity on a new day can
//! wait up to an hour before it has a row. From that moment the
//! five-minute sweep owns it and Pulse is current to within five
//! minutes, all day. Running the scan every five minutes instead is what
//! this replaced — ten full scans and HashAggregates of `issues`,
//! `changes`, `contributions`, `repo_stars`, `repos` and `repo_signals`
//! per tick in ordinary steady state, because every active repository's
//! today row was due on every tick and the batch never drained.
//!
//! **Bounded, and it drains at full speed.** A pass rolls at most
//! [`batch`] pairs so one enormous backlog cannot hold the lock for
//! minutes. Filling the batch means there is more to do, and the pass
//! immediately runs again rather than sleeping — otherwise a freshly
//! imported ten-year history would drain at one batch per tick, which at
//! the default interval is days. [`MAX_PASSES`] caps that so a bug that
//! made [`signals::due`] return the same pair forever is a slow loop
//! somebody notices, not a node pinned at 100% holding a fleet-wide
//! lock.
//!
//! **One bad repository does not stop the sweep.** A roll that fails is
//! logged and the pass moves on. The pair is still due next time — that
//! is what `rolled_at` not advancing means — so the retry is the rule
//! itself rather than a second mechanism.

use crate::app::SharedState;
use std::time::Duration;
use stratum_control::ids::now_ms;
use stratum_control::{jobs, signals, ControlDb};

/// Name of the fleet-wide lock this worker runs under. See
/// [`jobs::try_lock`].
const WORKER: &str = "signals-rollup";

/// How often the backlog scan runs, in seconds. Named as a constant so
/// the module header can link to it.
const STRATUM_SIGNALS_BACKFILL_SECS: &str = "STRATUM_SIGNALS_BACKFILL_SECS";

/// Pairs rolled per pass before the lock is checked again.
const DEFAULT_BATCH: i32 = 500;

/// How many full batches one tick may drain before it stands down.
///
/// The stop is not for the backlog's sake — a backlog is finite and
/// wants draining. It is for the case where it is *not* finite: a `due`
/// rule that stopped excluding what it had just rolled would loop here
/// forever, holding the lock, and the failure would look like a healthy
/// busy node. Ten batches is a large backlog's worth of progress per
/// tick and a bug's worth of evidence.
const MAX_PASSES: usize = 10;

/// The batch size a knob's value means.
///
/// Pure, and separated from the `env::var` that feeds it, because the
/// interesting part is the rule — a nonsense or non-positive value is
/// the default, never zero, because a batch of zero is a worker that
/// runs forever doing nothing — and testing a rule by mutating process
/// environment is how one test's `set_var` becomes another test's
/// inexplicable failure in the same binary.
fn parse_batch(v: Option<&str>) -> i32 {
    v.and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_BATCH)
}

fn batch() -> i32 {
    parse_batch(std::env::var("STRATUM_SIGNALS_BATCH").ok().as_deref())
}

/// How many ticks apart the backlog scan runs.
///
/// Pure and separated from the two `env_secs` that feed it, for the
/// reason [`parse_batch`] gives. **Never zero**: a modulus of zero
/// panics, and "every tick" is the honest reading of a backfill
/// interval shorter than the tick itself — an operator who sets it to
/// 60 with a 300-second tick is asking for it as often as possible, not
/// for a division by zero. A backfill interval of 0 means the same
/// thing; it does not disable the scan, because a rollup that never
/// discovers a new day is a rollup that stops working at midnight.
fn backfill_ticks(backfill_secs: u64, tick_secs: u64) -> u64 {
    (backfill_secs / tick_secs.max(1)).max(1)
}

pub fn spawn(state: SharedState) {
    // From the environment, never a literal, for the same reason a job
    // lease is: how often a Pulse page is allowed to be wrong is a
    // deployment's decision, not this file's. 0 disables the worker.
    let every = super::env_secs("STRATUM_SIGNALS_ROLLUP_SECS", 300);
    if every == 0 {
        return;
    }
    let backfill_every =
        backfill_ticks(super::env_secs(STRATUM_SIGNALS_BACKFILL_SECS, 3600), every);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(every));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Counts ticks, not wall clock: the first tick fires
        // immediately, so a node that has just started scans for a
        // backlog before it does anything else. That is the tick a
        // freshly restored database needs.
        let mut ticks: u64 = 0;
        loop {
            tick.tick().await;
            let backfill = ticks.is_multiple_of(backfill_every);
            ticks = ticks.wrapping_add(1);
            let db = state.db.clone();
            let out = tokio::task::spawn_blocking(move || sweep(&db, backfill)).await;
            if let Ok(Err(e)) = out {
                eprintln!("weft: signals rollup: {e}");
            }
        }
    });
}

/// One sweep — at most one in the fleet at a time. Returns how many
/// (repository, day) pairs were rolled, which is what the tests assert
/// on and what a future metric would report.
///
/// `backfill` decides whether this pass also runs the source-table scan
/// that finds days with no row yet. See the module header: it is the
/// expensive half and it runs on its own cadence.
pub fn sweep(db: &ControlDb, backfill: bool) -> Result<usize, String> {
    sweep_limited(db, batch(), backfill)
}

/// [`sweep`] with the batch size given rather than read, so a test can
/// drive a multi-pass drain without a process-wide `set_var`.
///
/// The lock is taken once and covers both halves. Two locks, or a lock
/// per half, would let a node run the backlog scan while another node
/// rolled open days out from under it — and the point of the fleet-wide
/// lock is that exactly one node is summing at a time.
pub fn sweep_limited(db: &ControlDb, limit: i32, backfill: bool) -> Result<usize, String> {
    let Some(_lock) = jobs::try_lock(db, WORKER)? else {
        return Ok(0);
    };
    // Open days first, always. They are the ones somebody is looking at.
    let mut rolled = drain(db, limit, signals::due_open)?;
    if backfill {
        rolled += drain(db, limit, signals::due_backfill)?;
    }
    Ok(rolled)
}

/// One of the two rules in [`signals`] that name work to do. Taken by
/// value so [`drain`] can be written once; see its note.
type DueRule = fn(&ControlDb, i64, i32) -> Result<Vec<(String, i32)>, String>;

/// Roll everything one due rule proposes, up to [`MAX_PASSES`] batches.
///
/// Taking the rule as a parameter rather than writing the loop twice is
/// not tidiness: the two halves must agree about batching, about the
/// per-pass clock, and about what a failed roll does, and two copies of
/// this loop is how they would come to disagree.
fn drain(db: &ControlDb, limit: i32, due: DueRule) -> Result<usize, String> {
    let mut rolled = 0;
    for _ in 0..MAX_PASSES {
        // `now_ms()` per pass, not once: draining a large backlog takes
        // real time, and a stale clock would keep proposing days whose
        // refresh floor has since passed while the pass itself made them
        // current.
        let pairs = due(db, now_ms(), limit)?;
        let filled = pairs.len() == limit as usize;
        for (repo_id, day) in pairs {
            match signals::roll(db, &repo_id, day) {
                Ok(()) => rolled += 1,
                // Logged and skipped. The pair stays due — `rolled_at`
                // did not move — so the next tick retries it without any
                // retry bookkeeping of its own.
                Err(e) => eprintln!("weft: signals rollup: {e}"),
            }
        }
        if !filled {
            break;
        }
    }
    Ok(rolled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_control::registry::{self, NewRepo, RepoKind};
    use stratum_control::signals::{day_bounds, day_of_ms, DAY_MS};

    fn repo(db: &ControlDb, org_id: &str, name: &str) -> String {
        registry::create_repo(
            db,
            org_id,
            &NewRepo {
                description: None,
                name,
                kind: RepoKind::Native,
                public: true,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap()
        .id
    }

    /// A change on a named day. Seeded over a direct connection because
    /// `ControlDb`'s handle is private to its own crate and
    /// `changes::create_or_update` stamps `now_ms()`, while the whole
    /// point here is to put work on a day that is not today.
    fn seed_change(url: &str, org_id: &str, repo_id: &str, key: &str, at: i64) {
        postgres::Client::connect(url, postgres::NoTls)
            .unwrap()
            .execute(
                "INSERT INTO changes (id, org_id, repo_id, change_key, title, \
                 target_branch, state, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, 't', 'main', 'open', $5, $5)",
                &[&stratum_control::ids::ulid(), &org_id, &repo_id, &key, &at],
            )
            .unwrap();
    }

    /// The sweep finds the work, does it, and then has nothing left to
    /// do — the last part being the one that matters, because a rollup
    /// that never settles burns a node forever.
    #[test]
    fn a_sweep_rolls_what_is_due_and_then_settles() {
        let url = stratum_testkit::pg::test_db_url("signals-sweep");
        let db = ControlDb::open(&url).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let r = repo(&db, &org.id, "app");
        let day = day_of_ms(now_ms()) - 3;
        seed_change(&url, &org.id, &r, "c1", day_bounds(day).0 + 1);

        assert!(sweep(&db, true).unwrap() >= 1, "the sweep rolled nothing");
        let rows = stratum_control::signals::range(&db, &r, day, day).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].changes, 1, "{rows:?}");

        // Everything is now either final or inside its refresh floor.
        assert_eq!(
            sweep(&db, true).unwrap(),
            0,
            "the sweep re-rolled days it had just finished"
        );
    }

    /// One node folds; the rest of the fleet stands down. Without the
    /// lock every node re-sums every due pair on every tick, and the
    /// cost of the rollup grows with the size of the fleet rather than
    /// the size of the data.
    #[test]
    fn a_node_that_loses_the_lock_does_not_sweep() {
        let url = stratum_testkit::pg::test_db_url("signals-sweep-lock");
        let a = ControlDb::open(&url).unwrap();
        let b = ControlDb::open(&url).unwrap();
        let org = registry::create_org(&a, "acme").unwrap();
        let r = repo(&a, &org.id, "app");
        let day = day_of_ms(now_ms()) - 3;
        seed_change(&url, &org.id, &r, "c1", day_bounds(day).0 + 1);

        let held = jobs::try_lock(&b, WORKER).unwrap().expect("b is sweeping");
        assert_eq!(
            sweep(&a, true).unwrap(),
            0,
            "a locked-out node swept anyway"
        );
        assert!(stratum_control::signals::range(&a, &r, day, day)
            .unwrap()
            .is_empty());

        // A pass, not a failure: with the lock free the same call folds.
        drop(held);
        assert!(sweep(&a, true).unwrap() >= 1);
    }

    /// A backlog larger than one batch drains inside a single tick. With
    /// the pass loop reduced to one iteration this leaves days unrolled,
    /// and at the default interval a freshly imported history would take
    /// days to appear on its own Pulse page.
    #[test]
    fn a_backlog_bigger_than_one_batch_drains_in_one_tick() {
        let url = stratum_testkit::pg::test_db_url("signals-sweep-batch");
        let db = ControlDb::open(&url).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let r = repo(&db, &org.id, "app");
        let first = day_of_ms(now_ms()) - 10;
        for d in 0..6 {
            seed_change(
                &url,
                &org.id,
                &r,
                &format!("c{d}"),
                day_bounds(first + d).0 + 1,
            );
        }

        // A batch of two against six days of backlog: three passes.
        let rolled = sweep_limited(&db, 2, true).unwrap();

        assert_eq!(rolled, 6, "the backlog did not drain in one tick");
        assert_eq!(
            stratum_control::signals::range(&db, &r, first, first + 5)
                .unwrap()
                .len(),
            6
        );
    }

    /// The knob's rule, without touching the environment. A batch of
    /// zero is a worker that holds the fleet lock forever doing nothing,
    /// so no value a person can type may produce one.
    #[test]
    fn a_nonsense_batch_size_is_the_default_never_zero() {
        assert_eq!(parse_batch(Some("7")), 7);
        assert_eq!(parse_batch(None), DEFAULT_BATCH);
        assert_eq!(parse_batch(Some("0")), DEFAULT_BATCH);
        assert_eq!(parse_batch(Some("-3")), DEFAULT_BATCH);
        assert_eq!(parse_batch(Some("banana")), DEFAULT_BATCH);
        assert_eq!(parse_batch(Some("")), DEFAULT_BATCH);
        assert_eq!(DAY_MS, 86_400_000);
    }

    /// The backlog scan's cadence, in ticks. Zero is the value that
    /// matters: `ticks % 0` panics, and a backfill interval at or below
    /// the tick means "every tick", not "crash the worker".
    #[test]
    fn the_backfill_cadence_is_never_zero_ticks() {
        assert_eq!(backfill_ticks(3600, 300), 12);
        assert_eq!(backfill_ticks(300, 300), 1);
        assert_eq!(backfill_ticks(60, 300), 1, "shorter than a tick");
        assert_eq!(backfill_ticks(0, 300), 1, "0 is every tick, not never");
        assert_eq!(backfill_ticks(3600, 0), 3600, "a tick of 0 cannot divide");
        assert_eq!(backfill_ticks(3600, 7200), 1);
    }

    /// The split that makes the whole change safe: **the five-minute
    /// pass alone keeps an open day current**, and the expensive
    /// source-table scan is only needed to discover a day for the first
    /// time.
    ///
    /// Both directions are asserted, because each one alone would pass
    /// against a broken split. A pass with no backfill must not find the
    /// unrolled day — that is what makes the hourly cadence a real
    /// saving rather than a rename. And once the day has a row, the
    /// no-backfill pass must keep rolling it — that is what makes the
    /// hourly cadence safe rather than a Pulse page frozen at whatever
    /// it said on the hour.
    #[test]
    fn an_open_day_stays_current_without_the_backlog_scan() {
        let url = stratum_testkit::pg::test_db_url("signals-sweep-cadence");
        let db = ControlDb::open(&url).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let r = repo(&db, &org.id, "app");
        let today = day_of_ms(now_ms());
        seed_change(&url, &org.id, &r, "c1", day_bounds(today).0 + 1);

        // No row yet, so only the scan can find it.
        assert_eq!(
            sweep(&db, false).unwrap(),
            0,
            "the cheap pass discovered a day with no row — the scan is \
             then running on every tick after all"
        );
        assert!(stratum_control::signals::range(&db, &r, today, today)
            .unwrap()
            .is_empty());

        assert!(sweep(&db, true).unwrap() >= 1, "the scan rolled nothing");
        let before = stratum_control::signals::range(&db, &r, today, today).unwrap();
        assert_eq!(before.len(), 1, "{before:?}");

        // Now the cheap pass owns it. Backdate the roll past the refresh
        // floor — the sweep's own clock is `now_ms()`, so moving the row
        // is the only way to cross the floor without sleeping.
        postgres::Client::connect(&url, postgres::NoTls)
            .unwrap()
            .execute(
                "UPDATE repo_signals SET rolled_at = rolled_at - $1 WHERE repo_id = $2",
                &[
                    &(stratum_control::signals::OPEN_DAY_REFRESH_MS + 1),
                    &r.as_str(),
                ],
            )
            .unwrap();
        assert_eq!(
            sweep(&db, false).unwrap(),
            1,
            "today's row froze once the backlog scan stopped running"
        );
        let after = stratum_control::signals::range(&db, &r, today, today).unwrap();
        assert!(
            after[0].rolled_at
                > before[0].rolled_at - stratum_control::signals::OPEN_DAY_REFRESH_MS,
            "the row was not re-rolled: {after:?}"
        );
    }
}
