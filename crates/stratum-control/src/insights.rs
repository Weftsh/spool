//! Pulse: what happened here in the last day, week, month or year — and
//! the same question asked of a whole namespace.
//!
//! This is the rail a maintainer opens on a Monday to answer "is this
//! project alive, and who moved it". Everything in it is a count over
//! rows we already hold: changes, issues, and the authorship rows the
//! contribution walker writes. Nothing here is a stored aggregate, so a
//! repository going public or private, a change landing late, or a
//! backfilled walk all show up the next time somebody looks.
//!
//! **What is deliberately not here: files changed, additions, deletions.**
//! We do not store per-commit diffstats, and this module does not derive
//! them from anything. That absence is the feature. GitHub's own Pulse
//! will render "0 files changed, 0 additions and 0 deletions" above a
//! week with 222 commits in it, because the number it wants comes from a
//! source that did not answer, and a reader has no way to tell that from
//! a genuinely quiet week. A line that is not there is honest; a number
//! nobody can check is not. If we ever want these, they arrive as a
//! stored diffstat written by the same walker that writes authorship —
//! not as an aggregate invented at read time.
//!
//! **The window is half-open, `[from_ms, to_ms)`.** One convention,
//! stated once, applied to every measure in the file. The reason is that
//! the UI's period selector renders adjacent periods side by side: if a
//! timestamp exactly on a boundary landed in both, the two bars would
//! sum to more than the year they are supposed to partition, and nobody
//! would ever find out why. `contributions` is stored per *day* rather
//! than per millisecond, so its window is the same convention in day
//! space — days `[floor(from_ms / 86_400_000), floor(to_ms / 86_400_000))`
//! — which partitions cleanly for the day-aligned boundaries the period
//! selector produces. A caller who hands us a boundary mid-day gets that
//! day counted in the earlier period, once.
//!
//! **Authorization is the caller's, and this module makes no visibility
//! decision.** [`pulse`] takes a `repo_id` that has already been resolved
//! through the same check as every other repo-scoped read, exactly like
//! [`crate::contribs::contributors`]; there is no `public` filter in it,
//! because one would silently empty a member's view of their own private
//! repository. [`org_insights`] spans repositories, so it says in its own
//! doc comment precisely which ones it counts and why that is safe.
//!
//! **The plan gate is not here.** `orgs.plan` decides who may read the
//! org rollup at all, and that check lives in the API layer with every
//! other authorization decision. Do not add a second opinion about it
//! inside these functions: two places that both believe they own a gate
//! is how one of them ends up wrong for a year.

use crate::db::ControlDb;
use serde::Serialize;

/// Milliseconds in a day. `contributions.day` counts these.
const DAY_MS: i64 = 86_400_000;

/// The widest window a caller may ask for (I13). The UI's longest period
/// is a year; the slack covers a client that computes "a year ago" in a
/// timezone we do not share and a leap day.
pub const MAX_WINDOW_MS: i64 = 400 * DAY_MS;

/// How many committers a Pulse chart ever names. GitHub's own tops out
/// around here, and the bar chart stops being readable well before it.
pub const MAX_TOP_COMMITTERS: i32 = 25;

/// How many repositories the org drill-through table returns. The count
/// of *active* repositories in [`OrgInsights::repos_active`] is computed
/// over all of them, so a namespace with more than this still gets a
/// truthful total above a truncated table.
pub const MAX_BREAKDOWN_REPOS: i32 = 100;

/// One person's commits inside the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Committer {
    pub user_id: String,
    /// The handle from `orgs`, not `users.handle` — see
    /// [`crate::contribs::contributors`] for why that is the one that is
    /// unique and case-folded.
    pub handle: String,
    pub commits: i64,
}

/// One repository's window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pulse {
    pub from_ms: i64,
    pub to_ms: i64,
    /// Changes touched in the window — opened in it, or updated in it.
    /// This is the denominator of the "active pull requests" bar; the
    /// two numbers below are the parts of it the bar splits.
    pub changes_active: i64,
    pub changes_opened: i64,
    /// Changes whose state is `landed` and whose `landed_at` falls in
    /// the window — see this module's `MERGED_AT` for why that column
    /// and not `updated_at`.
    pub changes_merged: i64,
    /// Issues touched in the window — opened in it, updated in it, or
    /// closed in it.
    ///
    /// The third clause is not redundant. `issues_closed` is read from
    /// `closed_at`, and an issue whose `closed_at` falls in the window
    /// while its `updated_at` does not is possible — an import writes
    /// both from the origin, and nothing constrains them to agree. Left
    /// out, the two segments of the proportion bar could add up to more
    /// than the bar. A part exceeding its whole is the one arithmetic a
    /// reader is guaranteed to spot.
    pub issues_active: i64,
    pub issues_opened: i64,
    pub issues_closed: i64,
    /// Distinct people with at least one commit in the window. The
    /// Summary paragraph's "N authors".
    pub authors: i64,
    /// Commits in the window, across all branches the walker has read.
    ///
    /// There is no main-only figure beside it, for the same reason there
    /// are no diffstats: `contributions` does not record which ref a
    /// commit was reached through, so a "commits to main" number would
    /// have to be guessed. The prose reads "N authors have pushed M
    /// commits" and stops there.
    pub commits: i64,
    /// Ranked, bounded by [`MAX_TOP_COMMITTERS`].
    pub top_committers: Vec<Committer>,
}

/// One row of the org drill-through table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoInsights {
    pub repo_id: String,
    pub name: String,
    /// The repository's *live* `public` flag, so a caller rendering this
    /// table can mark private rows without a second query.
    pub public: bool,
    pub commits: i64,
    pub authors: i64,
    pub changes_opened: i64,
    pub changes_merged: i64,
    pub issues_opened: i64,
    pub issues_closed: i64,
}

/// A namespace's window: the same measures, summed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrgInsights {
    pub from_ms: i64,
    pub to_ms: i64,
    /// Repositories in the namespace with at least one measured thing in
    /// the window. Counted over every active repository, not only the
    /// ones that fit in [`repos`](OrgInsights::repos).
    pub repos_active: i64,
    /// Distinct people with at least one commit in the window, across
    /// the namespace. Not the sum of the per-repo `authors` — somebody
    /// who worked in three repositories is one contributor.
    pub contributors: i64,
    pub commits: i64,
    pub changes_active: i64,
    pub changes_opened: i64,
    pub changes_merged: i64,
    pub issues_active: i64,
    pub issues_opened: i64,
    pub issues_closed: i64,
    /// Median wall-clock time from a change being opened to it landing —
    /// `landed_at - created_at` — over the changes that landed **in this
    /// window** (not the ones opened in it: a period's throughput is what
    /// came out of it).
    ///
    /// `None` when nothing landed. Zero merges and a median of zero are
    /// different facts and must not render the same.
    ///
    /// For an even number of merges this is the mean of the two middle
    /// values, rounded to the nearest millisecond — Postgres'
    /// `percentile_cont`, which interpolates. For an odd number it is
    /// the middle value exactly.
    pub median_time_to_merge_ms: Option<i64>,
    /// The drill-through table: repositories with something in the
    /// window, busiest first, bounded by [`MAX_BREAKDOWN_REPOS`].
    pub repos: Vec<RepoInsights>,
    /// Ranked across the namespace, bounded by [`MAX_TOP_COMMITTERS`].
    pub top_committers: Vec<Committer>,
}

/// A validated window: the millisecond bounds as given, and the day
/// bounds the `contributions` reads use.
///
/// Refusing rather than clamping is deliberate. A reversed window is a
/// caller asking a question with no answer, and a decade-wide one is a
/// tenant-wide scan wearing a period selector's clothes; both are bugs
/// in the caller, and naming them is more use than quietly returning
/// something that looks like data.
fn window(from_ms: i64, to_ms: i64) -> Result<(i64, i64, i32, i32), String> {
    if to_ms < from_ms {
        return Err(format!(
            "insights window ends before it starts: {from_ms}..{to_ms}"
        ));
    }
    // Checked, not bare subtraction: `from_ms` is caller input, and
    // i64::MIN..i64::MAX would panic here in release-mode wrapping and
    // pass the width check.
    let span = to_ms
        .checked_sub(from_ms)
        .ok_or_else(|| format!("insights window is not representable: {from_ms}..{to_ms}"))?;
    if span > MAX_WINDOW_MS {
        return Err(format!(
            "insights window of {span}ms is wider than the {MAX_WINDOW_MS}ms maximum"
        ));
    }
    // `div_euclid`, not `/`: a pre-epoch timestamp must floor toward the
    // earlier day, and truncating division rounds it toward zero, which
    // would put a commit from 1969 in the wrong day and — worse — make
    // the two halves of a boundary disagree about which day it is.
    let from_day = from_ms.div_euclid(DAY_MS);
    let to_day = to_ms.div_euclid(DAY_MS);
    // The span check above bounds these to ±400 days of each other, and
    // both derive from an i64 of milliseconds, so the cast is lossy only
    // for dates ~5.8 million years out. Saturating rather than `as`
    // keeps that from wrapping into a window that reads as valid.
    let clamp = |d: i64| d.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    Ok((from_ms, to_ms, clamp(from_day), clamp(to_day)))
}

/// When a change landed, as SQL, for the `ch` alias.
///
/// `changes.landed_at` is stamped by [`crate::changes::set_landed`] and
/// never moves again. `updated_at` moves whenever anything about the
/// change is edited, so reading a merge time from it would let a title
/// edit six months later drag the change into a later period and inflate
/// its time-to-merge — quietly, in a number nobody could check against
/// anything.
///
/// The `COALESCE` is not belt-and-braces. The migration backfills
/// `landed_at` for everything already landed, but a change landed by an
/// older binary partway through a rolling deploy carries a NULL, and a
/// NULL here would drop it out of the rollup entirely. An approximate
/// timestamp is worth more than a change that silently is not there.
///
/// Written once because it appears in four places — `changes_merged` in
/// each of the three query shapes below, and the time-to-merge
/// percentile — and four copies of a predicate is three chances to fix
/// only some of them.
const MERGED_AT: &str = "COALESCE(ch.landed_at, ch.updated_at)";

/// The scalar measures of one repository's window, as SQL. Shared by
/// [`pulse`] so the numbers and their names are written once.
///
/// `$1` repo id, `$2` from ms, `$3` to ms, `$4` from day, `$5` to day.
fn repo_measures() -> String {
    format!(
        "\
    (SELECT COUNT(*) FROM changes ch WHERE ch.repo_id = $1 \
       AND ((ch.created_at >= $2 AND ch.created_at < $3) \
         OR (ch.updated_at >= $2 AND ch.updated_at < $3)))::BIGINT AS changes_active, \
    (SELECT COUNT(*) FROM changes ch WHERE ch.repo_id = $1 \
       AND ch.created_at >= $2 AND ch.created_at < $3)::BIGINT AS changes_opened, \
    (SELECT COUNT(*) FROM changes ch WHERE ch.repo_id = $1 AND ch.state = 'landed' \
       AND {MERGED_AT} >= $2 AND {MERGED_AT} < $3)::BIGINT AS changes_merged, \
    (SELECT COUNT(*) FROM issues i WHERE i.repo_id = $1 \
       AND ((i.created_at >= $2 AND i.created_at < $3) \
         OR (i.updated_at >= $2 AND i.updated_at < $3) \
         OR (i.closed_at >= $2 AND i.closed_at < $3)))::BIGINT AS issues_active, \
    (SELECT COUNT(*) FROM issues i WHERE i.repo_id = $1 \
       AND i.created_at >= $2 AND i.created_at < $3)::BIGINT AS issues_opened, \
    (SELECT COUNT(*) FROM issues i WHERE i.repo_id = $1 AND i.state = 'closed' \
       AND i.closed_at >= $2 AND i.closed_at < $3)::BIGINT AS issues_closed, \
    (SELECT COUNT(DISTINCT c.user_id) FROM contributions c WHERE c.repo_id = $1 \
       AND c.day >= $4 AND c.day < $5 AND c.count > 0)::BIGINT AS authors, \
    COALESCE((SELECT SUM(c.count) FROM contributions c WHERE c.repo_id = $1 \
       AND c.day >= $4 AND c.day < $5), 0)::BIGINT AS commits"
    )
}

/// One repository's Pulse.
///
/// **Makes no visibility decision, and must not start.** `repo_id` has
/// already been resolved by the caller through the authorization every
/// other repo-scoped read uses, so by the time we are here the answer to
/// "may this reader see this repository" is yes. A `public` filter added
/// here would silently empty a member's Pulse for their own private
/// repository — the numbers would be zero and the page would look like a
/// dead project rather than a refused one.
///
/// The window is half-open, `[from_ms, to_ms)`; see the module header.
pub fn pulse(db: &ControlDb, repo_id: &str, from_ms: i64, to_ms: i64) -> Result<Pulse, String> {
    let (from_ms, to_ms, from_day, to_day) = window(from_ms, to_ms)?;
    let repo = repo_id.to_string();

    let row = db
        .lock()
        .query_one(
            &format!("SELECT {}", repo_measures()),
            &[&repo, &from_ms, &to_ms, &from_day, &to_day],
        )
        .map_err(|e| format!("read pulse: {e}"))?;

    let top_committers = db
        .lock()
        .query(
            // Same joins, and the same reasoning, as
            // `contribs::contributors`: the handle comes from the
            // personal namespace in `orgs`, disabled accounts are
            // dropped rather than rendered as an unfollowable face, and
            // `lower(o.name)` is the tiebreak so that two people on the
            // same count come back in the same order every time.
            "SELECT c.user_id, o.name AS handle, SUM(c.count)::BIGINT AS commits \
             FROM contributions c \
             JOIN users u ON u.id = c.user_id AND u.disabled_at IS NULL \
             JOIN orgs o ON o.owner_user_id = u.id AND o.kind = 'personal' \
             WHERE c.repo_id = $1 AND c.day >= $2 AND c.day < $3 AND c.count > 0 \
             GROUP BY c.user_id, o.name \
             ORDER BY commits DESC, lower(o.name) ASC \
             LIMIT $4",
            &[&repo, &from_day, &to_day, &i64::from(MAX_TOP_COMMITTERS)],
        )
        .map_err(|e| format!("read pulse committers: {e}"))?
        .iter()
        .map(|r| Committer {
            user_id: r.get("user_id"),
            handle: r.get("handle"),
            commits: r.get("commits"),
        })
        .collect();

    Ok(Pulse {
        from_ms,
        to_ms,
        changes_active: row.get("changes_active"),
        changes_opened: row.get("changes_opened"),
        changes_merged: row.get("changes_merged"),
        issues_active: row.get("issues_active"),
        issues_opened: row.get("issues_opened"),
        issues_closed: row.get("issues_closed"),
        authors: row.get("authors"),
        commits: row.get("commits"),
        top_committers,
    })
}

/// Per-repository measures over one namespace, one row per repository.
///
/// `$1` org id, `$2` from ms, `$3` to ms, `$4` from day, `$5` to day.
/// Written once and used twice — for the bounded drill-through table and
/// for the count of active repositories above it — so the two can never
/// disagree about what "active" means.
fn org_repo_rollup() -> String {
    format!(
        "\
    SELECT r.id AS repo_id, r.name, r.public, \
      COALESCE((SELECT SUM(c.count) FROM contributions c WHERE c.repo_id = r.id \
         AND c.day >= $4 AND c.day < $5), 0)::BIGINT AS commits, \
      (SELECT COUNT(DISTINCT c.user_id) FROM contributions c WHERE c.repo_id = r.id \
         AND c.day >= $4 AND c.day < $5 AND c.count > 0)::BIGINT AS authors, \
      (SELECT COUNT(*) FROM changes ch WHERE ch.repo_id = r.id \
         AND ch.created_at >= $2 AND ch.created_at < $3)::BIGINT AS changes_opened, \
      (SELECT COUNT(*) FROM changes ch WHERE ch.repo_id = r.id AND ch.state = 'landed' \
         AND {MERGED_AT} >= $2 AND {MERGED_AT} < $3)::BIGINT AS changes_merged, \
      (SELECT COUNT(*) FROM issues i WHERE i.repo_id = r.id \
         AND i.created_at >= $2 AND i.created_at < $3)::BIGINT AS issues_opened, \
      (SELECT COUNT(*) FROM issues i WHERE i.repo_id = r.id AND i.state = 'closed' \
         AND i.closed_at >= $2 AND i.closed_at < $3)::BIGINT AS issues_closed \
    FROM repos r WHERE r.org_id = $1 AND r.state = 'active'"
    )
}

/// What makes a repository count as active in the window.
const ORG_REPO_ACTIVE: &str = "commits > 0 OR authors > 0 OR changes_opened > 0 \
    OR changes_merged > 0 OR issues_opened > 0 OR issues_closed > 0";

/// A whole namespace's window.
///
/// **Which repositories this counts: every repository whose `org_id` is
/// this namespace and whose `state` is `active` — public and private
/// alike, deleted ones never.** That is the honest answer to "how did
/// this org do this month", and it is safe because of who the API layer
/// lets through: the rollup is an org-scoped read, authorised against
/// membership of *this* namespace before it is called, so the reader can
/// already see every one of these repositories by name. A public-only
/// filter here would not make it safer, it would make it wrong — a team
/// whose work is private would open Insights and see a dead month.
///
/// It follows that this must never widen beyond the namespace: nothing
/// in it reaches a repository in another org, and no measure is computed
/// from a table without joining back through `repos` to `$1`. A
/// contribution row, a change and an issue all carry a `repo_id` and
/// nothing else about tenancy, so the join *is* the tenancy check.
///
/// **The plan gate is the caller's.** `orgs.plan` being `paid` is what
/// decides whether this may be read at all, and that lives in the API
/// layer beside every other authorization decision. There is deliberately
/// no plan check in here.
///
/// The window is half-open, `[from_ms, to_ms)`; see the module header.
pub fn org_insights(
    db: &ControlDb,
    org_id: &str,
    from_ms: i64,
    to_ms: i64,
) -> Result<OrgInsights, String> {
    let (from_ms, to_ms, from_day, to_day) = window(from_ms, to_ms)?;
    let org = org_id.to_string();
    let args: [&(dyn postgres::types::ToSql + Sync); 5] =
        [&org, &from_ms, &to_ms, &from_day, &to_day];

    let totals = db
        .lock()
        .query_one(
            &format!(
                "SELECT \
               COALESCE((SELECT SUM(c.count) FROM contributions c \
                  JOIN repos r ON r.id = c.repo_id \
                  WHERE r.org_id = $1 AND r.state = 'active' \
                    AND c.day >= $4 AND c.day < $5), 0)::BIGINT AS commits, \
               (SELECT COUNT(DISTINCT c.user_id) FROM contributions c \
                  JOIN repos r ON r.id = c.repo_id \
                  WHERE r.org_id = $1 AND r.state = 'active' \
                    AND c.day >= $4 AND c.day < $5 AND c.count > 0)::BIGINT AS contributors, \
               (SELECT COUNT(*) FROM changes ch JOIN repos r ON r.id = ch.repo_id \
                  WHERE r.org_id = $1 AND r.state = 'active' \
                    AND ((ch.created_at >= $2 AND ch.created_at < $3) \
                      OR (ch.updated_at >= $2 AND ch.updated_at < $3)))::BIGINT AS changes_active, \
               (SELECT COUNT(*) FROM changes ch JOIN repos r ON r.id = ch.repo_id \
                  WHERE r.org_id = $1 AND r.state = 'active' \
                    AND ch.created_at >= $2 AND ch.created_at < $3)::BIGINT AS changes_opened, \
               (SELECT COUNT(*) FROM changes ch JOIN repos r ON r.id = ch.repo_id \
                  WHERE r.org_id = $1 AND r.state = 'active' AND ch.state = 'landed' \
                    AND {MERGED_AT} >= $2 AND {MERGED_AT} < $3)::BIGINT AS changes_merged, \
               (SELECT COUNT(*) FROM issues i JOIN repos r ON r.id = i.repo_id \
                  WHERE r.org_id = $1 AND r.state = 'active' \
                    AND ((i.created_at >= $2 AND i.created_at < $3) \
                      OR (i.updated_at >= $2 AND i.updated_at < $3) \
                      OR (i.closed_at >= $2 AND i.closed_at < $3)))::BIGINT AS issues_active, \
               (SELECT COUNT(*) FROM issues i JOIN repos r ON r.id = i.repo_id \
                  WHERE r.org_id = $1 AND r.state = 'active' \
                    AND i.created_at >= $2 AND i.created_at < $3)::BIGINT AS issues_opened, \
               (SELECT COUNT(*) FROM issues i JOIN repos r ON r.id = i.repo_id \
                  WHERE r.org_id = $1 AND r.state = 'active' AND i.state = 'closed' \
                    AND i.closed_at >= $2 AND i.closed_at < $3)::BIGINT AS issues_closed, \
               (SELECT PERCENTILE_CONT(0.5) WITHIN GROUP \
                    (ORDER BY ({MERGED_AT} - ch.created_at)::DOUBLE PRECISION) \
                  FROM changes ch JOIN repos r ON r.id = ch.repo_id \
                  WHERE r.org_id = $1 AND r.state = 'active' AND ch.state = 'landed' \
                    AND {MERGED_AT} >= $2 AND {MERGED_AT} < $3) AS median_ttm"
            ),
            &args,
        )
        .map_err(|e| format!("read org insights: {e}"))?;

    let repos_active: i64 = db
        .lock()
        .query_one(
            &format!(
                "SELECT COUNT(*)::BIGINT AS n FROM ({}) t WHERE {ORG_REPO_ACTIVE}",
                org_repo_rollup()
            ),
            &args,
        )
        .map_err(|e| format!("count active repos: {e}"))?
        .get("n");

    let limit = i64::from(MAX_BREAKDOWN_REPOS);
    let repos = db
        .lock()
        .query(
            &format!(
                "SELECT * FROM ({}) t WHERE {ORG_REPO_ACTIVE} \
                 ORDER BY commits DESC, changes_merged DESC, lower(name) ASC LIMIT $6",
                org_repo_rollup()
            ),
            &[&org, &from_ms, &to_ms, &from_day, &to_day, &limit],
        )
        .map_err(|e| format!("read org repo breakdown: {e}"))?
        .iter()
        .map(|r| RepoInsights {
            repo_id: r.get("repo_id"),
            name: r.get("name"),
            public: r.get("public"),
            commits: r.get("commits"),
            authors: r.get("authors"),
            changes_opened: r.get("changes_opened"),
            changes_merged: r.get("changes_merged"),
            issues_opened: r.get("issues_opened"),
            issues_closed: r.get("issues_closed"),
        })
        .collect();

    let top_committers = db
        .lock()
        .query(
            "SELECT c.user_id, o.name AS handle, SUM(c.count)::BIGINT AS commits \
             FROM contributions c \
             JOIN repos rp ON rp.id = c.repo_id AND rp.org_id = $1 AND rp.state = 'active' \
             JOIN users u ON u.id = c.user_id AND u.disabled_at IS NULL \
             JOIN orgs o ON o.owner_user_id = u.id AND o.kind = 'personal' \
             WHERE c.day >= $2 AND c.day < $3 AND c.count > 0 \
             GROUP BY c.user_id, o.name \
             ORDER BY commits DESC, lower(o.name) ASC \
             LIMIT $4",
            &[&org, &from_day, &to_day, &i64::from(MAX_TOP_COMMITTERS)],
        )
        .map_err(|e| format!("read org committers: {e}"))?
        .iter()
        .map(|r| Committer {
            user_id: r.get("user_id"),
            handle: r.get("handle"),
            commits: r.get("commits"),
        })
        .collect();

    Ok(OrgInsights {
        from_ms,
        to_ms,
        repos_active,
        contributors: totals.get("contributors"),
        commits: totals.get("commits"),
        changes_active: totals.get("changes_active"),
        changes_opened: totals.get("changes_opened"),
        changes_merged: totals.get("changes_merged"),
        issues_active: totals.get("issues_active"),
        issues_opened: totals.get("issues_opened"),
        issues_closed: totals.get("issues_closed"),
        // `percentile_cont` answers in floating point because it
        // interpolates; the rest of this file speaks milliseconds, so it
        // is rounded here rather than left for a caller to truncate
        // differently from the next caller.
        median_time_to_merge_ms: totals
            .get::<_, Option<f64>>("median_ttm")
            .map(|v| v.round() as i64),
        repos,
        top_committers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, NewRepo, RepoKind};

    fn db(hint: &str) -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap()
    }

    /// A person and their personal namespace — the handle top-committers
    /// joins through.
    fn person(db: &ControlDb, handle: &str) -> (String, String) {
        let u = crate::users::create(
            db,
            &format!("{handle}@example.com"),
            handle,
            Some("a long enough password"),
        )
        .unwrap();
        let ns = registry::create_personal_namespace(db, &u.id, handle, None).unwrap();
        (u.id, ns.id)
    }

    fn repo(db: &ControlDb, org_id: &str, name: &str, public: bool) -> String {
        registry::create_repo(
            db,
            org_id,
            &NewRepo {
                description: None,
                name,
                kind: RepoKind::Native,
                public,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap()
        .id
    }

    /// Rows are inserted directly rather than through `changes::create`
    /// and friends, because every assertion in this file is about a
    /// timestamp landing on one side of a boundary or the other, and the
    /// constructors stamp `now_ms()`. A test that cannot say when a row
    /// happened cannot test a window at all.
    fn change(
        db: &ControlDb,
        org_id: &str,
        repo_id: &str,
        key: &str,
        created: i64,
        state: &str,
        updated: i64,
    ) {
        // A landed change gets `landed_at` stamped equal to `updated_at`,
        // which is what `changes::set_landed` does in one statement. A
        // change in any other state has none, which is also what the
        // product produces — and is the NULL the `COALESCE` covers.
        let landed_at = (state == "landed").then_some(updated);
        db.lock()
            .execute(
                "INSERT INTO changes (id, org_id, repo_id, change_key, title, target_branch, \
                 state, created_at, updated_at, landed_at) \
                 VALUES ($1, $2, $3, $4, $4, 'main', $5, $6, $7, $8)",
                &[
                    &crate::ids::ulid(),
                    &org_id.to_string(),
                    &repo_id.to_string(),
                    &key.to_string(),
                    &state.to_string(),
                    &created,
                    &updated,
                    &landed_at,
                ],
            )
            .unwrap();
    }

    fn issue(
        db: &ControlDb,
        repo_id: &str,
        number: i32,
        created: i64,
        state: &str,
        closed: Option<i64>,
    ) {
        // `updated_at` is deliberately left at `created_at` even for a
        // closed issue: that is the shape an import produces, and it is
        // the case where `closed_at` is the *only* thing that puts the
        // issue in the window.
        db.lock()
            .execute(
                "INSERT INTO issues (id, repo_id, number, title, state, created_at, updated_at, closed_at) \
                 VALUES ($1, $2, $3, 'an issue', $4, $5, $5, $6)",
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

    fn commits(db: &ControlDb, user_id: &str, repo_id: &str, day: i32, count: i32, public: bool) {
        db.lock()
            .execute(
                "INSERT INTO contributions (user_id, repo_id, day, count, public) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &user_id.to_string(),
                    &repo_id.to_string(),
                    &day,
                    &count,
                    &public,
                ],
            )
            .unwrap();
    }

    const DAY: i64 = DAY_MS;

    #[test]
    fn every_pulse_count_matches_the_rows_that_produced_it() {
        let db = db("insights_pulse_counts");
        let (uid, ns) = person(&db, "ada");
        let r = repo(&db, &ns, "widget", true);
        let (from, to) = (10 * DAY, 20 * DAY);

        // Two opened in the window, one of which landed in it; one older
        // change that landed inside the window, so `opened` and `merged`
        // cannot be the same set.
        change(&db, &ns, &r, "c1", 11 * DAY, "open", 11 * DAY);
        change(&db, &ns, &r, "c2", 12 * DAY, "landed", 13 * DAY);
        change(&db, &ns, &r, "c3", 2 * DAY, "landed", 14 * DAY);
        // Untouched in the window at all.
        change(&db, &ns, &r, "c4", 2 * DAY, "open", 3 * DAY);

        issue(&db, &r, 1, 11 * DAY, "open", None);
        issue(&db, &r, 2, 12 * DAY, "closed", Some(13 * DAY));
        issue(&db, &r, 3, 2 * DAY, "closed", Some(15 * DAY));
        issue(&db, &r, 4, 2 * DAY, "open", None);

        commits(&db, &uid, &r, 11, 5, true);
        commits(&db, &uid, &r, 12, 7, true);

        let p = pulse(&db, &r, from, to).unwrap();
        assert_eq!(p.changes_opened, 2);
        assert_eq!(
            p.changes_merged, 2,
            "an old change that landed in the window is a merge of this window"
        );
        assert_eq!(
            p.changes_active, 3,
            "c4 was neither opened nor touched here"
        );
        assert_eq!(p.issues_opened, 2);
        assert_eq!(p.issues_closed, 2);
        // Issue 3 was opened long before the window and its `updated_at`
        // is still back there; only `closed_at` puts it in this period.
        // Without that clause in `issues_active` it would be counted as
        // closed here and not as active — the parts of the proportion
        // bar adding up to more than the bar.
        assert_eq!(p.issues_active, 3);
        assert!(
            p.issues_active >= p.issues_opened.max(p.issues_closed),
            "a segment of the proportion bar exceeded the bar: {p:?}"
        );
        assert!(
            p.changes_active >= p.changes_opened.max(p.changes_merged),
            "a segment of the proportion bar exceeded the bar: {p:?}"
        );
        assert_eq!(p.commits, 12);
        assert_eq!(p.authors, 1);
        assert_eq!(p.top_committers.len(), 1);
        assert_eq!(p.top_committers[0].handle, "ada");
        assert_eq!(p.top_committers[0].commits, 12);
    }

    /// The boundary, in both directions and in both time bases.
    ///
    /// This is the test the whole half-open convention exists for: a row
    /// exactly on `from` is inside, a row exactly on `to` is outside, so
    /// two adjacent periods partition the timeline instead of
    /// double-counting the instant between them. Get it wrong and the
    /// bars in the UI sum to more than the year they divide up, which
    /// nobody notices until somebody adds the numbers.
    #[test]
    fn the_window_is_half_open_at_both_ends() {
        let db = db("insights_window_edges");
        let (uid, ns) = person(&db, "bo");
        let r = repo(&db, &ns, "widget", true);
        let (from, to) = (10 * DAY, 20 * DAY);

        change(&db, &ns, &r, "at-from", from, "landed", from);
        change(&db, &ns, &r, "at-to", to, "landed", to);
        issue(&db, &r, 1, from, "closed", Some(from));
        issue(&db, &r, 2, to, "closed", Some(to));
        commits(&db, &uid, &r, 10, 3, true); // the day `from` falls on
        commits(&db, &uid, &r, 20, 9, true); // the day `to` falls on

        let p = pulse(&db, &r, from, to).unwrap();
        assert_eq!(p.changes_opened, 1, "the change at `to` was counted");
        assert_eq!(p.changes_merged, 1);
        assert_eq!(p.issues_opened, 1);
        assert_eq!(p.issues_closed, 1);
        assert_eq!(
            p.commits, 3,
            "the day `to` falls on belongs to the next period"
        );

        // And the adjacent period picks up exactly what this one left.
        let next = pulse(&db, &r, to, to + 10 * DAY).unwrap();
        assert_eq!(next.changes_opened, 1);
        assert_eq!(next.changes_merged, 1);
        assert_eq!(next.issues_opened, 1);
        assert_eq!(next.issues_closed, 1);
        assert_eq!(next.commits, 9);
    }

    #[test]
    fn top_committers_are_ranked_with_a_deterministic_tiebreak() {
        let db = db("insights_top_committers");
        let (ada, ns) = person(&db, "ada");
        let (zeb, _) = person(&db, "Zeb");
        let (cy, _) = person(&db, "cy");
        let r = repo(&db, &ns, "widget", true);
        let (from, to) = (10 * DAY, 20 * DAY);

        commits(&db, &cy, &r, 11, 9, true);
        // Tied on 4, and named so that a raw-byte sort would put `Zeb`
        // ahead of `ada` while the case-folded one does not.
        commits(&db, &ada, &r, 11, 4, true);
        commits(&db, &zeb, &r, 11, 4, true);
        // Outside the window, and must not lift anybody's rank.
        commits(&db, &ada, &r, 21, 100, true);

        let p = pulse(&db, &r, from, to).unwrap();
        let got: Vec<_> = p
            .top_committers
            .iter()
            .map(|c| (c.handle.as_str(), c.commits))
            .collect();
        assert_eq!(got, vec![("cy", 9), ("ada", 4), ("Zeb", 4)]);
        assert_eq!(p.authors, 3);
    }

    #[test]
    fn an_empty_window_is_zeros_and_not_an_error() {
        let db = db("insights_empty");
        let (_, ns) = person(&db, "ada");
        let r = repo(&db, &ns, "widget", true);
        change(&db, &ns, &r, "c1", 2 * DAY, "landed", 3 * DAY);

        let p = pulse(&db, &r, 10 * DAY, 20 * DAY).unwrap();
        assert_eq!(p.commits, 0);
        assert_eq!(p.authors, 0);
        assert_eq!(p.changes_active, 0);
        assert_eq!(p.issues_active, 0);
        assert!(p.top_committers.is_empty());

        let o = org_insights(&db, &ns, 10 * DAY, 20 * DAY).unwrap();
        assert_eq!(o.repos_active, 0);
        assert_eq!(o.contributors, 0);
        assert!(o.repos.is_empty());
        assert_eq!(
            o.median_time_to_merge_ms, None,
            "nothing merged must read as None, not as a median of zero"
        );
    }

    #[test]
    fn a_reversed_or_oversized_window_is_refused_rather_than_clamped() {
        let db = db("insights_bad_window");
        let (_, ns) = person(&db, "ada");
        let r = repo(&db, &ns, "widget", true);

        let e = pulse(&db, &r, 20 * DAY, 10 * DAY).unwrap_err();
        assert!(e.contains("ends before it starts"), "{e}");
        let e = org_insights(&db, &ns, 0, MAX_WINDOW_MS + 1).unwrap_err();
        assert!(e.contains("wider than"), "{e}");
        // The cap itself is allowed — an off-by-one here is a year-long
        // period the UI offers and the API refuses.
        org_insights(&db, &ns, 0, MAX_WINDOW_MS).unwrap();
        let e = pulse(&db, &r, i64::MIN, i64::MAX).unwrap_err();
        assert!(e.contains("not representable"), "{e}");
    }

    /// The tenancy property, from both sides.
    ///
    /// A member's own private repository is part of how their namespace
    /// did this month and must be counted; a repository in a *different*
    /// namespace must never leak into it, however busy it is. The second
    /// half is the leak-shaped one: every measure reaches its table
    /// through a `repo_id` that carries no tenancy of its own, so the
    /// join back to `repos.org_id` is the only thing standing between
    /// this rollup and somebody else's numbers.
    #[test]
    fn the_org_rollup_counts_private_repos_of_this_org_and_nothing_of_another() {
        let db = db("insights_org_tenancy");
        let (ada, mine) = person(&db, "ada");
        let (bo, theirs) = person(&db, "bo");
        let (from, to) = (10 * DAY, 20 * DAY);

        let open = repo(&db, &mine, "open-source", true);
        let secret = repo(&db, &mine, "secret", false);
        let elsewhere = repo(&db, &theirs, "elsewhere", true);

        commits(&db, &ada, &open, 11, 3, true);
        commits(&db, &ada, &secret, 11, 4, false);
        commits(&db, &bo, &elsewhere, 11, 1000, true);
        change(&db, &mine, &secret, "c1", 11 * DAY, "landed", 12 * DAY);
        change(&db, &theirs, &elsewhere, "c1", 11 * DAY, "landed", 12 * DAY);
        issue(&db, &secret, 1, 11 * DAY, "open", None);
        issue(&db, &elsewhere, 1, 11 * DAY, "open", None);

        let o = org_insights(&db, &mine, from, to).unwrap();
        assert_eq!(
            o.commits, 7,
            "the private repo's own work is this org's work"
        );
        assert_eq!(
            o.contributors, 1,
            "one person across two repositories is one contributor"
        );
        assert_eq!(o.changes_merged, 1);
        assert_eq!(o.issues_opened, 1);
        assert_eq!(o.repos_active, 2);

        let names: Vec<_> = o.repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["secret", "open-source"], "busiest first");
        assert!(
            !o.repos[0].public,
            "the breakdown carries the live public flag"
        );
        assert!(
            !names.contains(&"elsewhere"),
            "another namespace's repository reached this rollup"
        );
        assert_eq!(
            o.top_committers
                .iter()
                .map(|c| c.handle.as_str())
                .collect::<Vec<_>>(),
            vec!["ada"],
            "another namespace's committer reached this rollup"
        );
    }

    #[test]
    fn a_deleted_repository_leaves_the_rollup() {
        let db = db("insights_deleted_repo");
        let (ada, ns) = person(&db, "ada");
        let r = repo(&db, &ns, "widget", true);
        commits(&db, &ada, &r, 11, 5, true);

        let before = org_insights(&db, &ns, 10 * DAY, 20 * DAY).unwrap();
        assert_eq!(before.commits, 5);
        assert_eq!(before.repos_active, 1);

        db.lock()
            .execute(
                "UPDATE repos SET state = 'deleted', deleted_at = 1 WHERE id = $1",
                &[&r],
            )
            .unwrap();

        let after = org_insights(&db, &ns, 10 * DAY, 20 * DAY).unwrap();
        assert_eq!(after.commits, 0, "a deleted repository still counted");
        assert_eq!(after.repos_active, 0);
        assert!(after.repos.is_empty());
        assert!(after.top_committers.is_empty());
    }

    /// The median, for both parities and for the interpolating case.
    ///
    /// An even count is the mean of the two middle values, which is the
    /// only definition that does not silently prefer one arbitrary side
    /// of the split — and it is the one the doc comment promises, so it
    /// is pinned here rather than left to whichever aggregate somebody
    /// swaps in later.
    #[test]
    fn the_median_time_to_merge_is_right_for_an_odd_and_an_even_count() {
        let db = db("insights_median_ttm");
        let (_, ns) = person(&db, "ada");
        let r = repo(&db, &ns, "widget", true);
        let (from, to) = (10 * DAY, 20 * DAY);
        let land = 15 * DAY;

        // Three merges: 1h, 2h, 6h. Landed inside the window, opened
        // before it — which is exactly the shape the "landed in the
        // window" rule is chosen for.
        for (i, hours) in [1i64, 2, 6].iter().enumerate() {
            let ms = hours * 3_600_000;
            change(&db, &ns, &r, &format!("odd{i}"), land - ms, "landed", land);
        }
        let o = org_insights(&db, &ns, from, to).unwrap();
        assert_eq!(o.changes_merged, 3);
        assert_eq!(o.median_time_to_merge_ms, Some(2 * 3_600_000));

        // A fourth at 10h makes it even: the middle two are 2h and 6h.
        change(&db, &ns, &r, "even", land - 10 * 3_600_000, "landed", land);
        let o = org_insights(&db, &ns, from, to).unwrap();
        assert_eq!(o.changes_merged, 4);
        assert_eq!(o.median_time_to_merge_ms, Some(4 * 3_600_000));

        // A change that is still open contributes nothing, however long
        // it has been sitting there.
        change(&db, &ns, &r, "still-open", 0, "open", land);
        let o = org_insights(&db, &ns, from, to).unwrap();
        assert_eq!(o.changes_merged, 4);
        assert_eq!(o.median_time_to_merge_ms, Some(4 * 3_600_000));
    }

    /// A landed change stays in the period it landed in, whatever
    /// happens to it afterwards.
    ///
    /// This is the property `changes.landed_at` exists to give, and
    /// without a test it is documentation rather than a guarantee. Every
    /// merge measure used to read `updated_at`, which moves whenever
    /// anything about the change is edited — so a title change or an
    /// import backfill months later would silently move the merge into a
    /// later period *and* inflate its time-to-merge by the whole gap.
    /// Both numbers would have been wrong in a way nobody could check
    /// against anything.
    ///
    /// The `UPDATE` below is the thing to notice: it is one line, it is
    /// the kind of line an unrelated feature adds without thinking, and
    /// this test is the signal the person who adds it gets.
    #[test]
    fn a_landed_change_stays_in_its_own_period_when_updated_at_moves() {
        let db = db("insights_landed_at_is_stable");
        let (_, ns) = person(&db, "ada");
        let r = repo(&db, &ns, "widget", true);
        let (from, to) = (10 * DAY, 20 * DAY);
        let land = 15 * DAY;

        change(&db, &ns, &r, "c1", land - 3_600_000, "landed", land);

        let before = pulse(&db, &r, from, to).unwrap();
        let org_before = org_insights(&db, &ns, from, to).unwrap();
        assert_eq!(before.changes_merged, 1);
        assert_eq!(org_before.median_time_to_merge_ms, Some(3_600_000));

        // Somebody edits the change a year later — a title, a comment
        // counter, a backfill. Exactly the one line that used to be
        // enough to move the number.
        db.lock()
            .execute(
                "UPDATE changes SET updated_at = $2 WHERE repo_id = $1",
                &[&r, &(land + 365 * DAY)],
            )
            .unwrap();

        let after = pulse(&db, &r, from, to).unwrap();
        assert_eq!(
            after.changes_merged, 1,
            "an edit long after the fact moved the merge out of its period"
        );
        let org_after = org_insights(&db, &ns, from, to).unwrap();
        assert_eq!(org_after.changes_merged, 1);
        assert_eq!(
            org_after.median_time_to_merge_ms,
            Some(3_600_000),
            "an edit long after the fact inflated the time-to-merge"
        );
        assert_eq!(
            org_after.repos[0].changes_merged, 1,
            "the drill-through row disagreed with the total above it"
        );

        // And it does not appear in the later period either. Counted
        // twice is as wrong as counted in the wrong place, and only one
        // of the two shows up as a missing row.
        let later = pulse(&db, &r, land + 360 * DAY, land + 370 * DAY).unwrap();
        assert_eq!(later.changes_merged, 0);
    }

    /// A change landed by a binary that predates `landed_at` still
    /// counts, from `updated_at`.
    ///
    /// The migration backfills every row that exists when it runs, so
    /// this is the rolling-deploy window: an old binary lands a change
    /// against the new schema and writes no `landed_at`. Without the
    /// `COALESCE` that change is NULL on every merge measure and drops
    /// out of the rollup silently — a merge that happened and is
    /// nowhere, which is worse than a merge timed approximately.
    #[test]
    fn a_change_landed_without_a_landed_at_still_counts_from_updated_at() {
        let db = db("insights_landed_at_null");
        let (_, ns) = person(&db, "ada");
        let r = repo(&db, &ns, "widget", true);
        let (from, to) = (10 * DAY, 20 * DAY);
        let land = 15 * DAY;

        change(&db, &ns, &r, "c1", land - 3_600_000, "landed", land);
        db.lock()
            .execute(
                "UPDATE changes SET landed_at = NULL WHERE repo_id = $1",
                &[&r],
            )
            .unwrap();

        let p = pulse(&db, &r, from, to).unwrap();
        assert_eq!(p.changes_merged, 1, "a merge with no landed_at vanished");
        let o = org_insights(&db, &ns, from, to).unwrap();
        assert_eq!(o.changes_merged, 1);
        assert_eq!(o.repos[0].changes_merged, 1);
        assert_eq!(o.median_time_to_merge_ms, Some(3_600_000));
    }

    /// A repository with nothing in the window is not a row of zeroes.
    ///
    /// The drill-through table is bounded, so an org with a long tail of
    /// dormant repositories would otherwise spend the whole budget on
    /// rows saying nothing and push the busy ones off the end.
    #[test]
    fn the_breakdown_omits_repositories_with_nothing_in_the_window() {
        let db = db("insights_breakdown_quiet");
        let (ada, ns) = person(&db, "ada");
        let busy = repo(&db, &ns, "busy", true);
        let _quiet = repo(&db, &ns, "quiet", true);
        // Active only because an issue was closed here — no commits, no
        // changes. It still belongs in the table.
        let closed_only = repo(&db, &ns, "closer", true);

        commits(&db, &ada, &busy, 11, 2, true);
        issue(&db, &closed_only, 1, 2 * DAY, "closed", Some(11 * DAY));

        let o = org_insights(&db, &ns, 10 * DAY, 20 * DAY).unwrap();
        let names: Vec<_> = o.repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["busy", "closer"]);
        assert_eq!(o.repos_active, 2);
        assert_eq!(o.repos[1].issues_closed, 1);
    }
}
