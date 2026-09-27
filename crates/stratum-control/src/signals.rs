//! Per-day repository counters: the table Pulse and the org-wide
//! insights rollup both read, and the worker that keeps it true.
//!
//! `repo_signals` is a **cache with a recompute rule**, not a ledger.
//! Every number in it is derived from rows that are still there —
//! `issues`, `changes`, `contributions`, and the fork rows on `repos` —
//! and [`roll`] recomputes one (repository, day) from those
//! sources by assignment, never by increment. That is the whole reason
//! it can be safely re-run: a rollup that adds a delta cannot be run
//! twice, and a rollup that cannot be run twice cannot be run at all
//! after a crash.
//!
//! The reason to store it rather than derive it per request is the org
//! rollup. A tenant-wide "changes merged this month" over `changes`
//! scans every change row in the tenant; over this table it is a `SUM`
//! across a primary key range, once per repository. Per-repo Pulse then
//! reads the same rows, so there is one aggregation to keep correct
//! instead of two that can quietly disagree.
//!
//! ## Totals and deltas are not the same column
//!
//! **`forks` is a running total as of the end of that day.
//! Everything else is that day's own activity.** A chart that plots a
//! total as if it were a delta is a wrong graph that looks right —
//! smoothly rising instead of spiky — and nobody catches it by looking.
//! The distinction is stated here, on [`DaySignals`], and again at the
//! query that computes each one, because it is the single thing about
//! this table a reader can get wrong without noticing.
//!
//! ## Two honest limits, written down rather than papered over
//!
//! **There are no additions/deletions columns.** We do not store
//! per-commit diffstats, so any such number would be invented. GitHub's
//! own Pulse renders "0 files changed, 0 additions" over weeks with
//! hundreds of commits; a line that is not there beats a number nobody
//! can check.
//!
//! **A running total reconstructed for a past day is only as good as the
//! rows that survive.** The same
//! goes for a reopened issue, which clears its `closed_at` and so
//! vanishes from the day it was closed on. Neither is a defect here,
//! because [`due_open`] never recomputes a day that has closed and been
//! rolled since it closed — the number is fixed at the moment it stops
//! being able to move. It *is* a reason not to blindly re-roll history:
//! see the argument on [`due_open`].
//!
//! ## Visibility is not this module's business
//!
//! A private repository's rows are written exactly like a public one's.
//! Visibility is decided at read time from the live `repos.public`, for
//! the reason [`crate::contribs`]'s module header gives at length: a
//! denormalised flag and a live one that disagree is the shape of a leak
//! nobody notices for a year, and `public` can be flipped in both
//! directions. Filtering here would mean a repository made public shows
//! an empty Pulse for its whole private life, with no way to recover the
//! numbers.

use crate::db::ControlDb;
use crate::ids::now_ms;
use serde::Serialize;

/// Milliseconds in a day. The `day` numbering is
/// [`crate::contribs`]'s — days since the Unix epoch — and it is
/// deliberately the same one, because two day conventions in one schema
/// is a bug factory: a join between `contributions.day` and
/// `repo_signals.day` has to mean something, and it only does if they
/// count from the same place.
pub const DAY_MS: i64 = 86_400_000;

/// How long an *open* day's row may stand before it is rolled again.
///
/// Today's numbers move all day, so today's row is never final and is
/// always eligible; without a floor the sweep would re-sum every active
/// repository on every tick. Five minutes is chosen against what the
/// number is for — a Pulse page and a weekly digest — where "as of a few
/// minutes ago" is indistinguishable from live, and re-summing a day at
/// 5s intervals buys nothing anybody can see.
pub const OPEN_DAY_REFRESH_MS: i64 = 5 * 60 * 1000;

/// The largest window a single read may ask for, in days. Ten years —
/// the bound exists so a caller cannot ask for the Holocene (I13), not
/// because ten years is special.
pub const MAX_DAYS: i32 = 3653; // ten years, leap days included

/// The day number an instant falls in, UTC.
///
/// `div_euclid` rather than `/`: Rust's integer division truncates
/// toward zero, so a pre-epoch timestamp would land a day late. Nothing
/// in this product is dated before 1970, which is exactly why the bug
/// would survive to be found by something else.
///
/// **This is UTC, and `contributions.day` is not.** A commit's day is
/// the day its author would say they wrote it on, in their own
/// timezone — see [`crate::contribs`]. An issue's day is the UTC day it
/// was filed. Both are days since the epoch and both are the honest
/// answer for what they describe; a single row can therefore hold a
/// commit counted against one wall clock beside an issue counted against
/// another. Reconciling them would mean picking a timezone for the
/// repository, which is a fiction — repositories do not have timezones,
/// people do.
pub fn day_of_ms(ms: i64) -> i32 {
    ms.div_euclid(DAY_MS) as i32
}

/// The half-open millisecond range `[start, end)` a day covers, UTC.
pub fn day_bounds(day: i32) -> (i64, i64) {
    let start = day as i64 * DAY_MS;
    (start, start + DAY_MS)
}

/// One day of one repository, as the rollup stored it.
///
/// `forks` is a **running total** as of the end of `day`; every other
/// counter is that day's own activity. See the module
/// header — this is the field-level restatement of the one thing worth
/// restating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaySignals {
    /// Days since the Unix epoch, the same numbering `contributions`
    /// uses.
    pub day: i32,
    /// `YYYY-MM-DD`, so a client never has to agree with us about what
    /// `day` means.
    pub date: String,
    /// Total forks at the end of this day, not forks made on it.
    pub forks: i32,
    /// Changes opened on this day.
    pub changes: i32,
    /// Changes that reached `landed` on this day, by `landed_at` —
    /// when the merge happened, not when the row was last touched.
    /// "Merged" in the vocabulary of other forges; `landed` in ours,
    /// and the state column's own words are what the query matches on.
    pub changes_merged: i32,
    /// Issues filed on this day.
    pub issues_opened: i32,
    /// Issues closed on this day, by their current `closed_at`.
    pub issues_closed: i32,
    /// Commits authored on this day, across every contributor.
    pub commits: i32,
    /// Distinct people who authored a commit on this day.
    pub contributors: i32,
    /// When this row was last recomputed, in epoch milliseconds. It is
    /// the rollup's own bookkeeping — see [`due_open`] — but it is published
    /// because "as of when" is a fair question to ask of any cached
    /// number.
    pub rolled_at: i64,
}

/// One day of a whole namespace: every repository's counters, summed.
///
/// **No `contributors` field, deliberately.** Summing the per-repo
/// contributor counts double-counts anybody who touched two
/// repositories, and this table cannot tell you it happened — the
/// identities are gone by the time they are counted. A distinct count
/// across an org needs `contributions` itself, and inventing a number
/// here that is right only when nobody works on two repositories is the
/// failure this whole module is written to avoid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrgDay {
    pub day: i32,
    pub date: String,
    pub changes: i64,
    pub changes_merged: i64,
    pub issues_opened: i64,
    pub issues_closed: i64,
    pub commits: i64,
}

/// A namespace's totals over a window.
///
/// The activity counters sum over the window, because a sum of daily
/// activity is that window's activity. `forks` does not: it is a running
/// total, so its value for the window is the value on the **last day at
/// or before `to_day`** for each repository, summed across repositories.
/// Summing a running total over 30 days would report a repository with
/// 10 forks as having 300, which is the delta/total
/// confusion the module header warns about, in its most expensive form.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct OrgTotals {
    pub changes: i64,
    pub changes_merged: i64,
    pub issues_opened: i64,
    pub issues_closed: i64,
    pub commits: i64,
    /// Forks across the namespace as of `to_day`, not forks made.
    pub forks: i64,
}

// ---------------------------------------------------------------------
// Writing.
// ---------------------------------------------------------------------

/// Recompute one repository's counters for one day and write the row.
///
/// Every counter is computed in **one** statement, so all nine of them
/// see one snapshot of the database. Nine separate queries would let a
/// change land between the third and the fourth, and produce a row whose
/// `changes` and `changes_merged` describe two different instants — an
/// inconsistency that shows up as a merged count exceeding an opened
/// count and is unreproducible by the time anybody looks.
///
/// The write is an upsert that **assigns**, so rolling the same
/// (repository, day) twice produces the same row rather than doubled
/// counts. That is not a nicety: the sweep re-rolls today's row every
/// few minutes by design, and a crash between the read and the write
/// leaves the next pass to do it again.
pub fn roll(db: &ControlDb, repo_id: &str, day: i32) -> Result<(), String> {
    let (start, end) = day_bounds(day);
    let now = now_ms();
    let repo = repo_id.to_string();
    db.lock()
        .execute(
            "INSERT INTO repo_signals \
               (repo_id, day, stars, forks, changes, changes_merged, \
                issues_opened, issues_closed, commits, contributors, rolled_at) \
             SELECT $1, $2, \
               -- `stars` is a column the hosted edition fills; nothing
               -- here stars anything.
               0,
               -- A running total, as of the end of the day. Forks have
               -- `deleted_at`, so a fork that existed then and is gone
               -- now is still counted then.
               (SELECT count(*) FROM repos f
                 WHERE f.fork_parent_id = $1 AND f.created_at < $4
                   AND (f.deleted_at IS NULL OR f.deleted_at >= $4)),
               -- This day's own activity, from here down.
               (SELECT count(*) FROM changes c
                 WHERE c.repo_id = $1 AND c.created_at >= $3 AND c.created_at < $4),
               -- `landed` is this forge's word for merged, and
               -- `landed_at` is the moment it happened. Reading
               -- `updated_at` instead is correct the instant a change
               -- lands and stays correct only while nothing ever
               -- touches the row again — and a title edit on a landed
               -- change is an ordinary thing to do. It would move that
               -- merge into a later day's counters silently, in a
               -- number nobody can check against anything.
               --
               -- `COALESCE` because the column was added later and
               -- backfilled from `updated_at`: a row that predates the
               -- migration and was somehow missed still belongs to its
               -- best available day rather than to no day at all.
               (SELECT count(*) FROM changes c
                 WHERE c.repo_id = $1 AND c.state = 'landed'
                   AND coalesce(c.landed_at, c.updated_at) >= $3
                   AND coalesce(c.landed_at, c.updated_at) < $4),
               (SELECT count(*) FROM issues i
                 WHERE i.repo_id = $1 AND i.created_at >= $3 AND i.created_at < $4),
               -- `closed_at` is cleared on reopen, so a reopened issue
               -- correctly stops counting against the day it was closed
               -- on. The state check is belt and braces against a row
               -- that somehow kept a stale timestamp.
               (SELECT count(*) FROM issues i
                 WHERE i.repo_id = $1 AND i.state = 'closed'
                   AND i.closed_at >= $3 AND i.closed_at < $4),
               (SELECT coalesce(sum(k.count), 0) FROM contributions k
                 WHERE k.repo_id = $1 AND k.day = $2),
               -- One row per (person, repo, day), so a plain count of
               -- the rows with work on them is a distinct count of
               -- people.
               (SELECT count(*) FROM contributions k
                 WHERE k.repo_id = $1 AND k.day = $2 AND k.count > 0),
               $5 \
             ON CONFLICT (repo_id, day) DO UPDATE SET \
               stars = excluded.stars, forks = excluded.forks, \
               changes = excluded.changes, changes_merged = excluded.changes_merged, \
               issues_opened = excluded.issues_opened, \
               issues_closed = excluded.issues_closed, \
               commits = excluded.commits, contributors = excluded.contributors, \
               rolled_at = excluded.rolled_at",
            &[&repo, &day, &start, &end, &now],
        )
        .map_err(|e| format!("roll signals for {repo_id} day {day}: {e}"))?;
    Ok(())
}

/// The steady-state half: (repository, day) pairs that already have a
/// row and are not final yet, newest day first.
///
/// # Why there are two of these and not one
///
/// A (repository, day) is due when the repository is active and
///
/// ```text
/// coalesce(rolled_at, 0) < (day + 1) * 86400000        -- not final
/// coalesce(rolled_at, 0) + OPEN_DAY_REFRESH_MS <= now  -- past the floor
/// ```
///
/// The `coalesce` is what makes that decompose, and the decomposition is
/// the whole design here:
///
/// * a day that **has** a `repo_signals` row takes the first branch, so
///   the predicate reads only that row and the source tables are
///   irrelevant to it. That is this function, and because [`roll`]
///   upserts, every day the rollup has ever touched is in it. This is
///   the entire steady state.
/// * a day that has **no** row takes `0`, which is below the end of any
///   day at or after the epoch, so such a day is *always* due. That is
///   [`due_backfill`], and it is the only part that has to look at the
///   source tables at all.
///
/// Their union is exactly the set one query over both would return —
/// [`tests::the_two_halves_are_exactly_the_old_single_query`] holds that
/// against the query this pair replaced, run as a reference
/// implementation inside the test.
///
/// # The rule, and why it is this one
///
/// **A row is final once `rolled_at` is at or past the end of its day.**
/// Nothing else. Everything that feeds a day's counters is timestamped
/// inside that day, so once the day has closed no new row can land in
/// it; a sum taken after the close is therefore the last sum that will
/// ever differ, and recomputing it can only *change* it — for the worse,
/// per the module header's note on unstars and reopens. A final row is
/// never returned here again, at any age.
///
/// Anything not final is due, subject to one floor:
/// [`OPEN_DAY_REFRESH_MS`] since its last roll. That covers both of the
/// cases that matter, with one predicate:
///
/// * **today's row**, rolled at 00:05, has `rolled_at` far below the end
///   of today and is due again as soon as the floor passes — which is
///   the point. Freezing today's numbers at whatever they were just
///   after midnight is the failure mode a naive "already rolled, skip
///   it" rule produces, and it is invisible: the page renders, the
///   numbers are plausible, they are simply yesterday's;
/// * **yesterday's row**, if the last roll of it happened before
///   midnight, is not final either, and gets exactly one more roll —
///   after which it is, forever. Without this, the tail of every day
///   (everything between the final tick and midnight) is silently
///   missing from history.
///
/// The rule the alternatives get wrong: "re-roll the last N days every
/// pass" re-sums closed days forever and grows with history; "roll a day
/// once" freezes today. `rolled_at` against the day's own end is the
/// only cheap predicate that separates *a day I summed while it was
/// running* from *a day I summed after it stopped*, and that distinction
/// is the entire reason the column exists.
///
/// # What this costs, which is the reason it exists
///
/// One index range over `repo_signals` and nothing else. **No source
/// table is touched**, which is the entire point: this is the query that
/// runs every five minutes forever, and its cost has to be proportional
/// to the open days, not to the history.
///
/// It is held to that by the partial index
///
/// ```sql
/// CREATE INDEX repo_signals_open ON repo_signals (rolled_at)
///     WHERE rolled_at < (day::BIGINT + 1) * 86400000;
/// ```
///
/// whose predicate is exactly the "not final" half of the rule above, so
/// the planner can use it here. Its interesting property is that a row
/// leaves it **permanently** the moment its day finalises: it holds
/// roughly two entries per active repository — today's and possibly
/// yesterday's — however many years of history the table has behind it.
/// The `::BIGINT` is load-bearing in both places. `day` is `INT`, so
/// `(day + 1) * 86400000` is int4 arithmetic and overflows somewhere in
/// 2022; the cast is also what lets the index predicate and this
/// `WHERE` be recognised as the same expression.
///
/// The predicate that used to be here was a `UNION` over every source
/// table, and the comment above it claimed the `rolled_at` filter made
/// that "nearly free in steady state". **It was the exact opposite.**
/// The filter reads `s.rolled_at` from a `LEFT JOIN` outside the CTE, so
/// it cannot be pushed into the union's branches: the whole candidate
/// set had to be built and de-duplicated before a single row could be
/// discarded, and the `LIMIT` saved nothing. Every active repository's
/// *today* row is due on essentially every tick, so past a few thousand
/// repositories the sweep ran all ten of its passes and still did not
/// drain — ten full scans and HashAggregates of `issues`, `changes`,
/// `contributions`, `repo_stars`, `repos` and `repo_signals` every five
/// minutes, in ordinary steady state, spilling to disk once the
/// aggregate outgrew `work_mem`. An index would have changed the
/// constant; the split changes the complexity.
///
/// Deleted repositories are excluded. Their rows are still there
/// (soft-delete keeps them until the sweep hard-deletes and CASCADE
/// takes the signals with it), and re-summing a repository nobody can
/// read is work done for no reader.
pub fn due_open(db: &ControlDb, now_ms: i64, limit: i32) -> Result<Vec<(String, i32)>, String> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let limit = limit as i64;
    db.lock()
        .query(OPEN_SQL, &[&OPEN_DAY_REFRESH_MS, &now_ms, &limit])
        .map_err(|e| format!("signals due: {e}"))
        .map(|rows| rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

/// The text of [`due_open`]'s query, named so a test can `EXPLAIN` it
/// and assert that no source table appears in the plan. That assertion
/// is the only thing standing between this and a well-meaning edit that
/// reintroduces the union — the correctness tests would all still pass.
const OPEN_SQL: &str = "SELECT s.repo_id, s.day \
       FROM repo_signals s \
       JOIN repos r ON r.id = s.repo_id AND r.state = 'active' \
      WHERE s.rolled_at < (s.day::BIGINT + 1) * 86400000 \
        AND s.rolled_at + $1 <= $2 \
      ORDER BY s.day DESC, s.repo_id \
      LIMIT $3";

/// The backfill half: (repository, day) pairs a source table has work
/// on that the rollup has **never** summed, newest day first.
///
/// These are the days [`due_open`] structurally cannot see, because
/// there is no `repo_signals` row for it to read. Every such day is due
/// — `coalesce(rolled_at, 0)` is `0`, which is below the end of any day
/// at or after the epoch — so the only filters left are the repository's
/// state and the two exactness conditions noted at the `WHERE`.
///
/// # This is the expensive one, and that is why it is separate
///
/// It scans every source table and de-duplicates the result; there is no
/// index that makes "a day some table has a row on" cheap, because the
/// question is about the absence of a row somewhere else. What makes it
/// affordable is that it is **not the steady state**. New work happens
/// on days the rollup is already tracking; a genuinely unsummed day
/// arrives when history does — an import lands ten years of issues, a
/// contribution walk backfills a decade of commits — and when the day
/// rolls over and a repository's first row for the new day has to be
/// created.
///
/// So the caller runs it on its own, much slower cadence: see
/// [`crate`]'s consumer, `workers::signals`, which drives it once an
/// hour against [`due_open`]'s five minutes. The cost of that choice is
/// stated where it lands: a repository's first activity on a new day can
/// wait up to that interval before it appears on Pulse, after which
/// [`due_open`] has the row and keeps it current every five minutes.
pub fn due_backfill(db: &ControlDb, now_ms: i64, limit: i32) -> Result<Vec<(String, i32)>, String> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let limit = limit as i64;
    db.lock()
        .query(
            "WITH cand AS ( \
                 SELECT repo_id, (created_at / 86400000)::INT AS day FROM issues \
               UNION \
                 SELECT repo_id, (closed_at / 86400000)::INT FROM issues \
                   WHERE closed_at IS NOT NULL \
               UNION \
                 SELECT repo_id, (created_at / 86400000)::INT FROM changes \
               UNION \
                 SELECT repo_id, (coalesce(landed_at, updated_at) / 86400000)::INT \
                   FROM changes WHERE state = 'landed' \
               UNION \
                 SELECT repo_id, day FROM contributions \
               UNION \
                 SELECT fork_parent_id, (created_at / 86400000)::INT FROM repos \
                   WHERE fork_parent_id IS NOT NULL \
             ) \
             SELECT c.repo_id, c.day \
               FROM cand c \
               JOIN repos r ON r.id = c.repo_id AND r.state = 'active' \
               LEFT JOIN repo_signals s ON s.repo_id = c.repo_id AND s.day = c.day \
              WHERE s.repo_id IS NULL \
                -- The two conditions the `coalesce(rolled_at, 0)` rule
                -- degenerates to when there is no row. `day >= 0` is
                -- `0 < (day + 1) * 86400000` written out, and it is here
                -- so that this half plus `due_open` is *exactly* the set
                -- the single query returned rather than approximately
                -- it — a pre-epoch day was never due and still is not.
                AND c.day >= 0 \
                AND $1::BIGINT <= $2::BIGINT \
              ORDER BY c.day DESC, c.repo_id \
              LIMIT $3",
            &[&OPEN_DAY_REFRESH_MS, &now_ms, &limit],
        )
        .map_err(|e| format!("signals backfill due: {e}"))
        .map(|rows| rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

// ---------------------------------------------------------------------
// Reading.
// ---------------------------------------------------------------------

/// Reject a window that is backwards or unbounded. Shared by every read
/// so a caller cannot get a different answer to "is this a legal range"
/// depending on which one it asked.
fn check_range(from_day: i32, to_day: i32) -> Result<(), String> {
    if to_day < from_day {
        return Err(format!("day range {from_day}..{to_day} runs backwards"));
    }
    // `i64` on purpose: `to - from` over the full `i32` range overflows,
    // and an overflow check that itself overflows is decoration.
    if to_day as i64 - from_day as i64 >= MAX_DAYS as i64 {
        return Err(format!("day range is longer than {MAX_DAYS} days"));
    }
    Ok(())
}

/// One repository's rows over an **inclusive** window, oldest first.
///
/// Only days that have a row. A client renders the gaps; sending a
/// zeroed row for each of 365 days would be sending nothing 365 times,
/// which is the same argument [`crate::contribs::graph`] makes for the
/// same reason.
pub fn range(
    db: &ControlDb,
    repo_id: &str,
    from_day: i32,
    to_day: i32,
) -> Result<Vec<DaySignals>, String> {
    check_range(from_day, to_day)?;
    let repo = repo_id.to_string();
    db.lock()
        .query(
            "SELECT day, forks, changes, changes_merged, issues_opened, \
                    issues_closed, commits, contributors, rolled_at \
               FROM repo_signals \
              WHERE repo_id = $1 AND day >= $2 AND day <= $3 \
              ORDER BY day",
            &[&repo, &from_day, &to_day],
        )
        .map_err(|e| format!("read signals: {e}"))
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    let day: i32 = r.get(0);
                    DaySignals {
                        day,
                        date: crate::contribs::iso_of_day(day),
                        forks: r.get(1),
                        changes: r.get(2),
                        changes_merged: r.get(3),
                        issues_opened: r.get(4),
                        issues_closed: r.get(5),
                        commits: r.get(6),
                        contributors: r.get(7),
                        rolled_at: r.get(8),
                    }
                })
                .collect()
        })
}

/// A namespace's activity per day over an inclusive window, oldest
/// first — one `SUM` over an index, not a loop over repositories.
///
/// The point of the table is that this is a query. Fetching each
/// repository's rows and adding them in Rust would be N round trips that
/// grow with the tenant, for an answer PostgreSQL produces in one.
///
/// Running totals are absent here on purpose: a per-day chart of org
/// stars would need the last value at or before each day for every
/// repository, which is a different (and much heavier) query than a sum,
/// and [`org_totals`] answers the question people actually ask.
pub fn org_daily(
    db: &ControlDb,
    org_id: &str,
    from_day: i32,
    to_day: i32,
) -> Result<Vec<OrgDay>, String> {
    check_range(from_day, to_day)?;
    let org = org_id.to_string();
    db.lock()
        .query(
            "SELECT s.day, sum(s.changes), sum(s.changes_merged), \
                    sum(s.issues_opened), sum(s.issues_closed), sum(s.commits) \
               FROM repo_signals s \
               JOIN repos r ON r.id = s.repo_id \
              WHERE r.org_id = $1 AND r.state = 'active' \
                AND s.day >= $2 AND s.day <= $3 \
              GROUP BY s.day \
              ORDER BY s.day",
            &[&org, &from_day, &to_day],
        )
        .map_err(|e| format!("org signals: {e}"))
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    let day: i32 = r.get(0);
                    OrgDay {
                        day,
                        date: crate::contribs::iso_of_day(day),
                        changes: r.get(1),
                        changes_merged: r.get(2),
                        issues_opened: r.get(3),
                        issues_closed: r.get(4),
                        commits: r.get(5),
                    }
                })
                .collect()
        })
}

/// A namespace's totals over an inclusive window.
///
/// Activity sums; running totals do not. The stars and forks halves come
/// from a `DISTINCT ON` picking each repository's newest row at or
/// before `to_day` — the last thing the rollup said about it — and a
/// repository with no row in the window contributes nothing, which is
/// correct: a repository the rollup has never seen has no total to
/// report, and guessing zero would be a claim we cannot support.
///
/// It is one statement rather than two because the two halves must
/// describe one snapshot; a sum from before a roll and a total from
/// after it is a report of a state that never existed.
pub fn org_totals(
    db: &ControlDb,
    org_id: &str,
    from_day: i32,
    to_day: i32,
) -> Result<OrgTotals, String> {
    check_range(from_day, to_day)?;
    let org = org_id.to_string();
    let row = db
        .lock()
        .query_one(
            "WITH mine AS ( \
                 SELECT s.* FROM repo_signals s \
                   JOIN repos r ON r.id = s.repo_id \
                  WHERE r.org_id = $1 AND r.state = 'active' \
             ), \
             activity AS ( \
                 SELECT coalesce(sum(changes), 0) AS changes, \
                        coalesce(sum(changes_merged), 0) AS changes_merged, \
                        coalesce(sum(issues_opened), 0) AS issues_opened, \
                        coalesce(sum(issues_closed), 0) AS issues_closed, \
                        coalesce(sum(commits), 0) AS commits \
                   FROM mine WHERE day >= $2 AND day <= $3 \
             ), \
             latest AS ( \
                 SELECT DISTINCT ON (repo_id) forks \
                   FROM mine WHERE day <= $3 \
                  ORDER BY repo_id, day DESC \
             ) \
             SELECT a.changes, a.changes_merged, a.issues_opened, a.issues_closed, \
                    a.commits, \
                    coalesce((SELECT sum(forks) FROM latest), 0) \
               FROM activity a",
            &[&org, &from_day, &to_day],
        )
        .map_err(|e| format!("org signal totals: {e}"))?;
    Ok(OrgTotals {
        changes: row.get(0),
        changes_merged: row.get(1),
        issues_opened: row.get(2),
        issues_closed: row.get(3),
        commits: row.get(4),
        forks: row.get(5),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, NewRepo, RepoKind};
    use crate::users;

    /// A day well clear of "now", so a test's fixtures cannot collide
    /// with today's row and so every day under test is one the rollup
    /// considers closed.
    const D: i32 = 19_000; // 2022-01-08

    fn open(name: &str) -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url(name)).unwrap()
    }

    fn user(db: &ControlDb, handle: &str) -> String {
        users::create(
            db,
            &format!("{handle}@example.com"),
            handle,
            Some("a long enough password"),
        )
        .unwrap()
        .id
    }

    fn repo(db: &ControlDb, org_id: &str, name: &str) -> String {
        registry::create_repo(
            db,
            org_id,
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
        .id
    }

    /// A user, their personal namespace and one repository in it.
    fn fixture(db: &ControlDb, handle: &str, name: &str) -> (String, String, String) {
        let uid = user(db, handle);
        let ns = registry::create_personal_namespace(db, &uid, handle, None)
            .unwrap()
            .id;
        let rid = repo(db, &ns, name);
        (uid, ns, rid)
    }

    /// Source rows are seeded directly rather than through the issue and
    /// change APIs, because those stamp `now_ms()` and the whole point
    /// of these tests is to place work on a named day.
    fn seed_issue(db: &ControlDb, repo_id: &str, number: i32, created: i64, closed: Option<i64>) {
        let state = if closed.is_some() { "closed" } else { "open" };
        db.lock()
            .execute(
                "INSERT INTO issues (id, repo_id, number, title, state, created_at, \
                 updated_at, closed_at) VALUES ($1, $2, $3, 'i', $4, $5, $5, $6)",
                &[
                    &crate::ids::ulid(),
                    &repo_id.to_string(),
                    &number,
                    &state.to_string(),
                    &created,
                    &closed,
                ],
            )
            .unwrap();
    }

    fn seed_change(
        db: &ControlDb,
        org_id: &str,
        repo_id: &str,
        key: &str,
        created: i64,
        landed: Option<i64>,
    ) {
        let state = if landed.is_some() { "landed" } else { "open" };
        db.lock()
            .execute(
                "INSERT INTO changes (id, org_id, repo_id, change_key, title, \
                 target_branch, state, created_at, updated_at, landed_at) \
                 VALUES ($1, $2, $3, $4, 't', 'main', $5, $6, $7, $8)",
                &[
                    &crate::ids::ulid(),
                    &org_id.to_string(),
                    &repo_id.to_string(),
                    &key.to_string(),
                    &state.to_string(),
                    &created,
                    &landed.unwrap_or(created),
                    &landed,
                ],
            )
            .unwrap();
    }

    /// Touch a change the way any later edit does — `updated_at` moves,
    /// `landed_at` does not. This is `changes::set_title`'s write, which
    /// is what makes the distinction between the two columns observable.
    fn touch_change(db: &ControlDb, key: &str, at: i64) {
        let n = db
            .lock()
            .execute(
                "UPDATE changes SET title = 'edited', updated_at = $2 WHERE change_key = $1",
                &[&key.to_string(), &at],
            )
            .unwrap();
        assert_eq!(n, 1, "the change to touch was not there");
    }

    fn seed_commits(db: &ControlDb, repo_id: &str, user_id: &str, day: i32, count: i32) {
        db.lock()
            .execute(
                "INSERT INTO contributions (user_id, repo_id, day, count, public) \
                 VALUES ($1, $2, $3, $4, true)",
                &[&user_id.to_string(), &repo_id.to_string(), &day, &count],
            )
            .unwrap();
    }

    fn one(db: &ControlDb, repo_id: &str, day: i32) -> DaySignals {
        range(db, repo_id, day, day)
            .unwrap()
            .pop()
            .expect("a row for the day that was rolled")
    }

    /// The headline: every counter equals the rows that produced it, and
    /// nothing bleeds in from the days either side.
    #[test]
    fn a_days_counters_match_the_rows_behind_them() {
        let db = open("signals-counters");
        let (uid, ns, rid) = fixture(&db, "ada", "widget");
        let (start, _) = day_bounds(D);

        // Three issues filed today, one of them closed today; one filed
        // yesterday and closed today; one filed tomorrow.
        seed_issue(&db, &rid, 1, start + 1, None);
        seed_issue(&db, &rid, 2, start + 2, None);
        seed_issue(&db, &rid, 3, start + 3, Some(start + 4));
        seed_issue(&db, &rid, 4, start - DAY_MS, Some(start + 5));
        seed_issue(&db, &rid, 5, start + DAY_MS, None);

        // Two changes opened today, one of which landed today; one
        // opened yesterday that lands today.
        seed_change(&db, &ns, &rid, "c1", start + 10, None);
        seed_change(&db, &ns, &rid, "c2", start + 11, Some(start + 12));
        seed_change(&db, &ns, &rid, "c3", start - DAY_MS, Some(start + 13));
        seed_change(&db, &ns, &rid, "c4", start + DAY_MS, None);

        // Two people, seven commits, and a third person with a row
        // carrying no work — a contributor count must not count them.
        let bob = user(&db, "bob");
        let eve = user(&db, "eve");
        seed_commits(&db, &rid, &uid, D, 4);
        seed_commits(&db, &rid, &bob, D, 3);
        seed_commits(&db, &rid, &eve, D, 0);
        seed_commits(&db, &rid, &uid, D + 1, 9);

        // A fork made before the day, and one made after it.
        let old_fork = repo(&db, &ns, "fork-old");
        let new_fork = repo(&db, &ns, "fork-new");
        for (fork, at) in [(&old_fork, start - 1), (&new_fork, start + DAY_MS)] {
            db.lock()
                .execute(
                    "UPDATE repos SET fork_parent_id = $2, created_at = $3 WHERE id = $1",
                    &[&fork.clone(), &rid.clone(), &at],
                )
                .unwrap();
        }

        roll(&db, &rid, D).unwrap();
        let s = one(&db, &rid, D);
        assert_eq!(s.date, "2022-01-08", "{s:?}");
        assert_eq!(s.issues_opened, 3, "{s:?}");
        assert_eq!(s.issues_closed, 2, "{s:?}");
        assert_eq!(s.changes, 2, "{s:?}");
        assert_eq!(s.changes_merged, 2, "{s:?}");
        assert_eq!(s.commits, 7, "{s:?}");
        assert_eq!(s.contributors, 2, "a row with no work counted as a person");
        assert_eq!(s.forks, 1, "the fork total is as of the end of the day");
        assert!(s.rolled_at > 0, "{s:?}");
    }

    /// A fork that existed on the day and has since been deleted is
    /// still part of that day's total, because `repos.deleted_at` says
    /// when it stopped existing and the day is before that.
    #[test]
    fn a_since_deleted_fork_still_counts_on_the_day_it_existed() {
        let db = open("signals-deleted-fork");
        let (_uid, ns, rid) = fixture(&db, "ada", "widget");
        let (start, _) = day_bounds(D);
        let fork = repo(&db, &ns, "fork");
        db.lock()
            .execute(
                "UPDATE repos SET fork_parent_id = $2, created_at = $3, \
                 state = 'deleted', deleted_at = $4 WHERE id = $1",
                &[&fork, &rid.clone(), &(start - 1), &(start + 2 * DAY_MS)],
            )
            .unwrap();

        roll(&db, &rid, D).unwrap();
        assert_eq!(one(&db, &rid, D).forks, 1);

        // And it is gone from a day after the deletion.
        roll(&db, &rid, D + 3).unwrap();
        assert_eq!(one(&db, &rid, D + 3).forks, 0);
    }

    /// A merge belongs to the day it merged, not the day somebody last
    /// edited the row.
    ///
    /// `changes.updated_at` moves on any write — a title edit on an
    /// already-landed change is an ordinary thing to do — so reading it
    /// silently relocates that merge into a later day's counters, and
    /// the relocated number is still perfectly plausible. Nobody catches
    /// it by looking at a chart. `landed_at` is the moment itself and
    /// never moves.
    ///
    /// Both halves are pinned, because [`roll`] and the due rules each
    /// had
    /// their own copy of the expression: the counter must land on the
    /// merge day, *and* the sweep must propose the merge day rather than
    /// the edit day — a rollup that never visits a day is as wrong as
    /// one that sums it incorrectly, and quieter.
    #[test]
    fn a_merge_counts_on_the_day_it_landed_not_the_day_it_was_edited() {
        let db = open("signals-landed-at");
        let (_uid, ns, rid) = fixture(&db, "ada", "widget");
        // Opened five days before it landed, and edited thirty days
        // after — three distinct days, so nothing can pass by accident.
        seed_change(
            &db,
            &ns,
            &rid,
            "c1",
            day_bounds(D - 5).0 + 1,
            Some(day_bounds(D).0 + 2),
        );
        touch_change(&db, "c1", day_bounds(D + 30).0 + 3);

        // The sweep must see the landing day, and must not invent the
        // edit day — which has nothing on it at all. Nothing has been
        // rolled yet, so this is the backfill half's question.
        let pairs = due_backfill(&db, now_ms(), 50).unwrap();
        assert!(
            pairs.contains(&(rid.clone(), D)),
            "the sweep never visits the day the change landed: {pairs:?}"
        );
        assert!(
            !pairs.contains(&(rid.clone(), D + 30)),
            "the sweep proposed the day the change was edited: {pairs:?}"
        );

        for d in [D - 5, D, D + 30] {
            roll(&db, &rid, d).unwrap();
        }
        assert_eq!(one(&db, &rid, D).changes_merged, 1, "the merge day");
        assert_eq!(
            one(&db, &rid, D + 30).changes_merged,
            0,
            "the edit day was credited with a merge"
        );
        // And the change is still opened on the day it was opened.
        assert_eq!(one(&db, &rid, D - 5).changes, 1);
        assert_eq!(one(&db, &rid, D).changes, 0);
    }

    /// The property the whole rollup rests on: it may be run again. A
    /// rollup that added deltas would double every counter here.
    #[test]
    fn rolling_the_same_day_twice_is_idempotent() {
        let db = open("signals-idempotent");
        let (uid, ns, rid) = fixture(&db, "ada", "widget");
        let (start, _) = day_bounds(D);
        seed_issue(&db, &rid, 1, start + 1, Some(start + 2));
        seed_change(&db, &ns, &rid, "c1", start + 3, Some(start + 4));
        seed_commits(&db, &rid, &uid, D, 5);

        roll(&db, &rid, D).unwrap();
        let first = one(&db, &rid, D);
        roll(&db, &rid, D).unwrap();
        let second = one(&db, &rid, D);

        assert_eq!(first.issues_opened, second.issues_opened);
        assert_eq!(first.issues_closed, second.issues_closed);
        assert_eq!(first.changes, second.changes);
        assert_eq!(first.changes_merged, second.changes_merged);
        assert_eq!(first.commits, second.commits);
        assert_eq!(first.contributors, second.contributors);
        assert_eq!(first.commits, 5, "{first:?}");
        // One row, not two — the primary key and the upsert together.
        assert_eq!(range(&db, &rid, D, D).unwrap().len(), 1);
    }

    /// The due rule, in both directions and across both halves. A day
    /// with work and no row is the backfill's; once it has a row the
    /// open half owns it, comes back after the refresh floor, and drops
    /// it forever once it is final.
    #[test]
    fn due_returns_an_open_day_again_and_a_closed_one_never() {
        let db = open("signals-due");
        let (_uid, ns, rid) = fixture(&db, "ada", "widget");
        let today = day_of_ms(now_ms());
        let (today_start, today_end) = day_bounds(today);
        seed_change(&db, &ns, &rid, "c1", today_start + 1, None);
        seed_change(&db, &ns, &rid, "c2", day_bounds(D).0 + 1, None);

        // Nothing has a row, so the open half cannot see either day and
        // the backfill half sees both. That split is the fix: the
        // expensive query answers only the question that needs it.
        assert!(
            due_open(&db, now_ms(), 10).unwrap().is_empty(),
            "the steady-state query invented a day with no row"
        );
        let pairs = due_backfill(&db, now_ms(), 10).unwrap();
        assert!(
            pairs.contains(&(rid.clone(), today)) && pairs.contains(&(rid.clone(), D)),
            "a day with activity and no row must be due: {pairs:?}"
        );
        // Newest first, so a backlog does not starve the live day.
        assert_eq!(pairs.first().map(|p| p.1), Some(today), "{pairs:?}");

        roll(&db, &rid, today).unwrap();
        roll(&db, &rid, D).unwrap();

        // Both have rows now, so the backfill half is done with them
        // forever — this is why it can run on a slow cadence.
        assert!(
            due_backfill(&db, now_ms(), 10).unwrap().is_empty(),
            "the backfill half kept proposing days that now have rows"
        );

        // Just rolled: the floor holds both back.
        let pairs = due_open(&db, now_ms(), 10).unwrap();
        assert!(!pairs.iter().any(|p| p.0 == rid), "{pairs:?}");

        // Past the floor, today is due again — it is still moving — and
        // the closed day is not, because it was rolled after it closed.
        let later = now_ms() + OPEN_DAY_REFRESH_MS;
        let pairs = due_open(&db, later, 10).unwrap();
        assert!(
            pairs.contains(&(rid.clone(), today)),
            "today's row was frozen: {pairs:?}"
        );
        assert!(
            !pairs.contains(&(rid.clone(), D)),
            "a day rolled after it closed was re-summed: {pairs:?}"
        );

        // And a day rolled *while it was still running* is due exactly
        // once more, however long ago that was: here, a row for today
        // backdated to a roll at 00:05 stays due even a year later.
        db.lock()
            .execute(
                "UPDATE repo_signals SET rolled_at = $2 WHERE repo_id = $1 AND day = $3",
                &[&rid.clone(), &(today_start + 300_000), &today],
            )
            .unwrap();
        let pairs = due_open(&db, today_end + 365 * DAY_MS, 10).unwrap();
        assert!(
            pairs.contains(&(rid.clone(), today)),
            "a day summed while it was still running never got its final roll: {pairs:?}"
        );
    }

    /// `limit` bounds a pass, and a non-positive one asks for nothing
    /// rather than for everything.
    #[test]
    fn due_respects_its_bound() {
        let db = open("signals-due-limit");
        let (_uid, ns, rid) = fixture(&db, "ada", "widget");
        for d in 0..5 {
            seed_change(
                &db,
                &ns,
                &rid,
                &format!("c{d}"),
                day_bounds(D + d).0 + 1,
                None,
            );
        }
        assert_eq!(due_backfill(&db, now_ms(), 2).unwrap().len(), 2);
        assert_eq!(due_backfill(&db, now_ms(), 0).unwrap().len(), 0);
        assert_eq!(due_backfill(&db, now_ms(), -1).unwrap().len(), 0);

        // The same bound on the other half. The days have to be rolled
        // to have rows at all, and rolling a closed day makes it final —
        // so `rolled_at` is put back to a roll that happened before the
        // day ended, which is exactly the state the open half is for.
        for d in 0..5 {
            roll(&db, &rid, D + d).unwrap();
        }
        db.lock()
            .execute(
                "UPDATE repo_signals SET rolled_at = 1 WHERE repo_id = $1",
                &[&rid.clone()],
            )
            .unwrap();
        assert_eq!(due_open(&db, now_ms(), 2).unwrap().len(), 2);
        assert_eq!(due_open(&db, now_ms(), 0).unwrap().len(), 0);
        assert_eq!(due_open(&db, now_ms(), -1).unwrap().len(), 0);
    }

    /// The exact query this pair replaced, kept as a reference
    /// implementation so the decomposition can be checked against it
    /// rather than against an argument about SQL.
    ///
    /// It is a single `UNION` over every source table plus
    /// `repo_signals`, with the `coalesce(rolled_at, 0)` predicate
    /// applied afterwards over a `LEFT JOIN`. That "afterwards" is the
    /// defect: the filter cannot be pushed into the branches, so the
    /// whole candidate set is built and de-duplicated on every call.
    fn old_due(db: &ControlDb, now: i64, limit: i64) -> Vec<(String, i32)> {
        db.lock()
            .query(
                "WITH cand AS ( \
                     SELECT repo_id, (created_at / 86400000)::INT AS day FROM issues \
                   UNION \
                     SELECT repo_id, (closed_at / 86400000)::INT FROM issues \
                       WHERE closed_at IS NOT NULL \
                   UNION \
                     SELECT repo_id, (created_at / 86400000)::INT FROM changes \
                   UNION \
                     SELECT repo_id, (coalesce(landed_at, updated_at) / 86400000)::INT \
                       FROM changes WHERE state = 'landed' \
                   UNION \
                     SELECT repo_id, day FROM contributions \
                   UNION \
                     SELECT fork_parent_id, (created_at / 86400000)::INT FROM repos \
                       WHERE fork_parent_id IS NOT NULL \
                   UNION \
                     SELECT repo_id, day FROM repo_signals \
                 ) \
                 SELECT c.repo_id, c.day \
                   FROM cand c \
                   JOIN repos r ON r.id = c.repo_id AND r.state = 'active' \
                   LEFT JOIN repo_signals s ON s.repo_id = c.repo_id AND s.day = c.day \
                  WHERE coalesce(s.rolled_at, 0) < (c.day::BIGINT + 1) * 86400000 \
                    AND coalesce(s.rolled_at, 0) + $1 <= $2 \
                  ORDER BY c.day DESC, c.repo_id \
                  LIMIT $3",
                &[&OPEN_DAY_REFRESH_MS, &now, &limit],
            )
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect()
    }

    fn sorted(mut v: Vec<(String, i32)>) -> Vec<(String, i32)> {
        v.sort();
        v
    }

    /// The claim the whole change rests on: **`due_open` ∪ `due_backfill`
    /// is the set the one query returned**, not an approximation of it.
    ///
    /// Split against a repository holding every case at once — days that
    /// are final, days rolled while still running, days with work and no
    /// row at all, a day known only to `repo_signals` because the row
    /// that produced it is gone, and a deleted repository that must be
    /// in neither answer. Checked at three different "now"s, because the
    /// refresh floor moves the boundary between the two halves and a
    /// decomposition that only holds at one instant is not one.
    #[test]
    fn the_two_halves_are_exactly_the_old_single_query() {
        let db = open("signals-due-partition");
        let (uid, ns, rid) = fixture(&db, "ada", "widget");
        let today = day_of_ms(now_ms());

        // Work spread over days that are long closed, and some today.
        seed_issue(
            &db,
            &rid,
            1,
            day_bounds(D).0 + 1,
            Some(day_bounds(D + 2).0 + 1),
        );
        seed_change(&db, &ns, &rid, "c1", day_bounds(D + 1).0 + 1, None);
        seed_change(
            &db,
            &ns,
            &rid,
            "c2",
            day_bounds(D + 3).0 + 1,
            Some(day_bounds(D + 4).0 + 1),
        );
        seed_commits(&db, &rid, &uid, D + 5, 3);
        seed_change(&db, &ns, &rid, "c3", day_bounds(today).0 + 1, None);

        // A second repository, so the answer is not one repo's by luck.
        let other = repo(&db, &ns, "gadget");
        seed_commits(&db, &other, &uid, D + 1, 1);

        // A deleted one with work on it: in neither half, in both
        // implementations.
        let (_u2, ns2, gone) = fixture(&db, "zoe", "theirs");
        seed_commits(&db, &gone, &uid, D, 1);
        assert!(registry::delete_repo(&db, &ns2, &gone).unwrap());

        let both = |now: i64| {
            let mut v = due_open(&db, now, 1000).unwrap();
            v.extend(due_backfill(&db, now, 1000).unwrap());
            sorted(v)
        };

        // Before anything is rolled: everything is the backfill's.
        assert_eq!(both(now_ms()), sorted(old_due(&db, now_ms(), 1000)));
        assert!(!both(now_ms()).iter().any(|p| p.0 == gone));

        // Roll a mix: two closed days (now final), and today (not).
        for d in [D, D + 2, today] {
            roll(&db, &rid, d).unwrap();
        }
        // A day `repo_signals` knows about and no source table does —
        // only the open half can ever see this one.
        db.lock()
            .execute(
                "INSERT INTO repo_signals (repo_id, day, rolled_at) VALUES ($1, $2, 0)",
                &[&rid.clone(), &(D + 40)],
            )
            .unwrap();

        for now in [
            now_ms(),
            now_ms() + OPEN_DAY_REFRESH_MS,
            day_bounds(today + 400).0,
        ] {
            assert_eq!(
                both(now),
                sorted(old_due(&db, now, 1000)),
                "the two halves stopped adding up to the old query at {now}"
            );
        }

        // And the split is not trivial — both halves are carrying rows.
        let later = now_ms() + OPEN_DAY_REFRESH_MS;
        assert!(!due_open(&db, later, 1000).unwrap().is_empty());
        assert!(!due_backfill(&db, later, 1000).unwrap().is_empty());
    }

    /// The steady-state query touches **no source table**, which is the
    /// property the fix is, and the one every other test here would go
    /// on passing without.
    ///
    /// Asserted against the plan rather than the SQL text, because what
    /// matters is what PostgreSQL reads. A `UNION` reintroduced anywhere
    /// under this query — in a CTE, a subquery, a view — puts the table
    /// in the plan, and this fails.
    #[test]
    fn the_steady_state_query_reads_no_source_table() {
        let db = open("signals-due-plan");
        let plan: String = db
            .lock()
            .query(
                &format!("EXPLAIN {OPEN_SQL}"),
                &[&OPEN_DAY_REFRESH_MS, &now_ms(), &1000i64],
            )
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>()
            .join("\n");

        for table in ["issues", "changes", "contributions"] {
            assert!(
                !plan.contains(table),
                "the five-minute sweep reads {table}:\n{plan}"
            );
        }
        assert!(plan.contains("repo_signals"), "{plan}");
    }

    /// A deleted repository is neither swept nor read. The soft-delete
    /// keeps the rows; the sweep must not spend passes on them.
    ///
    /// The day under test is **today**, deliberately. An old day is
    /// final and would drop out of `due` for that reason alone, which
    /// makes the assertion pass whether or not the state filter is
    /// there — a green test proving nothing. Today's row is due right up
    /// until the delete, so its disappearance is attributable.
    #[test]
    fn a_deleted_repository_takes_its_signals_with_it() {
        let db = open("signals-delete");
        let (uid, ns, rid) = fixture(&db, "ada", "widget");
        let today = day_of_ms(now_ms());
        seed_commits(&db, &rid, &uid, today, 2);
        roll(&db, &rid, today).unwrap();
        assert_eq!(range(&db, &rid, today, today).unwrap().len(), 1);

        // Still moving, so still due once the refresh floor passes.
        let later = now_ms() + OPEN_DAY_REFRESH_MS;
        assert!(
            due_open(&db, later, 10)
                .unwrap()
                .contains(&(rid.clone(), today)),
            "today's row should be due before the repository is deleted"
        );

        // Soft delete: the rows survive, but nothing proposes them.
        assert!(registry::delete_repo(&db, &ns, &rid).unwrap());
        let pairs = due_open(&db, later, 10).unwrap();
        assert!(
            !pairs.iter().any(|p| p.0 == rid),
            "a deleted repository was still being rolled: {pairs:?}"
        );

        // Hard delete: `ON DELETE CASCADE` takes the signals.
        db.lock()
            .execute("DELETE FROM repos WHERE id = $1", &[&rid.clone()])
            .unwrap();
        assert!(range(&db, &rid, today, today).unwrap().is_empty());
    }

    /// The window is inclusive at both ends and ordered oldest first —
    /// an off-by-one here silently drops a column from every chart.
    #[test]
    fn the_range_read_is_inclusive_and_ordered() {
        let db = open("signals-range");
        let (uid, _ns, rid) = fixture(&db, "ada", "widget");
        for d in 0..5 {
            seed_commits(&db, &rid, &uid, D + d, d + 1);
            roll(&db, &rid, D + d).unwrap();
        }

        let days = range(&db, &rid, D + 1, D + 3).unwrap();
        assert_eq!(
            days.iter().map(|d| d.day).collect::<Vec<_>>(),
            vec![D + 1, D + 2, D + 3],
            "both ends are inclusive and the order is oldest first"
        );
        assert_eq!(
            days.iter().map(|d| d.commits).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert_eq!(days[0].date, crate::contribs::iso_of_day(D + 1));

        // A single day is a legal window.
        assert_eq!(range(&db, &rid, D, D).unwrap().len(), 1);
        // A backwards or unbounded one is not.
        assert!(range(&db, &rid, D, D - 1).is_err());
        assert!(range(&db, &rid, 0, MAX_DAYS).is_err());
        assert!(org_daily(&db, "ns", D, D - 1).is_err());
        assert!(org_totals(&db, "ns", D, D - 1).is_err());
    }

    /// The reason the table exists: an org-wide number is a sum over its
    /// repositories' rows, and it must equal what those rows say.
    #[test]
    fn the_org_sum_equals_the_sum_of_its_repositories() {
        let db = open("signals-org");
        let (uid, ns, a) = fixture(&db, "ada", "widget");
        let b = repo(&db, &ns, "gadget");
        let bob = user(&db, "bob");

        // Two repositories, two days, and one person active in both —
        // which is why there is no org contributor count to check.
        seed_commits(&db, &a, &uid, D, 3);
        seed_commits(&db, &b, &uid, D, 4);
        seed_commits(&db, &b, &bob, D + 1, 5);
        seed_change(&db, &ns, &a, "c1", day_bounds(D).0 + 1, None);
        seed_change(
            &db,
            &ns,
            &b,
            "c2",
            day_bounds(D + 1).0 + 1,
            Some(day_bounds(D + 1).0 + 2),
        );
        seed_issue(&db, &a, 1, day_bounds(D).0 + 1, None);
        // A fork of `a` from before the window: a running total.
        let fork = repo(&db, &ns, "widget-fork");
        db.lock()
            .execute(
                "UPDATE repos SET fork_parent_id = $2, created_at = $3 WHERE id = $1",
                &[&fork, &a.clone(), &(day_bounds(D).0 - 1)],
            )
            .unwrap();

        for d in [D, D + 1] {
            roll(&db, &a, d).unwrap();
            roll(&db, &b, d).unwrap();
        }

        // Per day, against the two repositories' own rows.
        let daily = org_daily(&db, &ns, D, D + 1).unwrap();
        assert_eq!(daily.len(), 2, "{daily:?}");
        for od in &daily {
            let ra = one(&db, &a, od.day);
            let rb = one(&db, &b, od.day);
            assert_eq!(od.commits, (ra.commits + rb.commits) as i64, "{od:?}");
            assert_eq!(od.changes, (ra.changes + rb.changes) as i64, "{od:?}");
            assert_eq!(
                od.changes_merged,
                (ra.changes_merged + rb.changes_merged) as i64,
                "{od:?}"
            );
            assert_eq!(
                od.issues_opened,
                (ra.issues_opened + rb.issues_opened) as i64,
                "{od:?}"
            );
        }
        assert_eq!(daily[0].date, crate::contribs::iso_of_day(D));

        // Over the window: activity sums, running totals do not.
        let t = org_totals(&db, &ns, D, D + 1).unwrap();
        assert_eq!(t.commits, 12, "{t:?}");
        assert_eq!(t.changes, 2, "{t:?}");
        assert_eq!(t.changes_merged, 1, "{t:?}");
        assert_eq!(t.issues_opened, 1, "{t:?}");
        assert_eq!(
            t.forks, 1,
            "a running total summed over two days would say 2: {t:?}"
        );

        // A repository outside the namespace is outside the sum.
        let (_o, other_ns, other) = fixture(&db, "zoe", "theirs");
        seed_commits(&db, &other, &uid, D, 99);
        roll(&db, &other, D).unwrap();
        assert_eq!(org_totals(&db, &ns, D, D + 1).unwrap().commits, 12);
        assert_eq!(org_totals(&db, &other_ns, D, D + 1).unwrap().commits, 99);

        // An empty namespace is zero, not an error and not a missing row.
        assert_eq!(
            org_totals(&db, "no-such-org", D, D + 1).unwrap(),
            OrgTotals::default()
        );
        assert!(org_daily(&db, "no-such-org", D, D + 1).unwrap().is_empty());
    }

    /// A private repository is rolled exactly like a public one.
    /// Visibility is a read-time decision from the live `repos.public`,
    /// and a rollup that skipped private repositories would leave a
    /// repository made public with an unrecoverable hole in its history.
    #[test]
    fn a_private_repositorys_rows_are_still_written() {
        let db = open("signals-private");
        let uid = user(&db, "ada");
        let ns = registry::create_personal_namespace(&db, &uid, "ada", None)
            .unwrap()
            .id;
        let rid = registry::create_repo(
            &db,
            &ns,
            &NewRepo {
                description: None,
                name: "secret",
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap()
        .id;
        seed_commits(&db, &rid, &uid, D, 6);

        assert!(
            due_backfill(&db, now_ms(), 10)
                .unwrap()
                .contains(&(rid.clone(), D)),
            "a private repository was skipped by the sweep"
        );
        roll(&db, &rid, D).unwrap();
        assert_eq!(one(&db, &rid, D).commits, 6);
    }

    /// Day arithmetic, at the boundaries a date bug actually lands on.
    #[test]
    fn day_numbers_agree_with_the_contribution_graphs() {
        assert_eq!(day_of_ms(0), 0);
        assert_eq!(day_of_ms(DAY_MS - 1), 0);
        assert_eq!(day_of_ms(DAY_MS), 1);
        // Truncating division would answer 0 here, putting the last
        // millisecond of 1969 into 1970.
        assert_eq!(day_of_ms(-1), -1);
        assert_eq!(day_bounds(0), (0, DAY_MS));
        assert_eq!(day_bounds(-1), (-DAY_MS, 0));
        // The same epoch `contributions` counts from.
        assert_eq!(crate::contribs::iso_of_day(day_of_ms(0)), "1970-01-01");
        assert_eq!(crate::contribs::iso_of_day(D), "2022-01-08");
    }
}
