//! Polling GitHub Actions for a repository's verdicts.
//!
//! Stratum never runs anybody's code. This is the half of `check_runs`
//! that goes and *asks*: for a repository mirrored from GitHub, through
//! the App installation it already syncs commits with, it walks
//! `/actions/runs` and records each run as a check. The other half —
//! `api::checks_intake` — waits to be told. One table, so the Checks tab
//! never has to know which of the two put a row there.
//!
//! Modelled on [`super::importer`] deliberately, down to the shape of
//! the loop, because the constraints are the same ones: a rate budget of
//! five thousand requests an hour, a process that may be killed by a
//! deploy at any moment, and an upstream that answers three different
//! ways to one question. A run does a bounded amount of work, writes
//! where it got to, and re-enqueues.
//!
//! ## The three answers, and why they must stay apart
//!
//! * **data** — write the runs, follow `Link` to the next page.
//! * **rate limited** — a *normal state*, not a failure. Record when we
//!   may ask again, re-enqueue, and come back. Failing the job here
//!   would make a large repository look broken every time it hit the
//!   budget it was always going to hit.
//! * **refused** — stop, and record why where a person can read it. An
//!   installation without `actions: read` answers 403, and reporting
//!   that as an empty list is the specific lie this module exists to
//!   avoid: "this project has no CI" and "this installation may not see
//!   the CI this project has" are different facts, and a Checks tab that
//!   renders both as an empty page sends a maintainer to look for a
//!   problem in their workflow files when the problem is a permission
//!   on the App. [`origin::is_actions_read_denied`] is how that case is
//!   recognised — by the predicate, never by a substring of the message,
//!   which is prose meant for a person and will be rewritten.
//!
//! ## Incremental, and why it is a high-water mark rather than a cursor
//!
//! The runs endpoint is newest-first over a list that *changes*: a run
//! enters `queued`, becomes `in_progress`, becomes `completed`, and new
//! runs are inserted at the front while we walk. A cursor that
//! remembered "I got as far as page 4" would therefore be pointing at a
//! different page an hour later, and one that remembered the oldest run
//! it had seen would never notice the run it already had going green.
//!
//! So the memory is a high-water mark on `updated_at`: page forward from
//! the front until a page arrives on which *every* run is at or below
//! the newest `updated_at` of the last completed walk. Below that line
//! nothing has changed since we last looked, and — because the order is
//! by recency of change — nothing below it can have either. A first pass
//! has no mark and therefore walks the whole history, bounded by
//! [`page_budget`] and resumed from the `Link` URL GitHub named, exactly
//! as the importer resumes.
//!
//! What this saves is *requests to GitHub*, which is the scarce thing.
//! It deliberately does not skip writing the runs on a page it has
//! decided is the last one: they cost us nothing, `upsert` is idempotent
//! on `(repo_id, provider, external_id)`, and a run whose `updated_at`
//! GitHub did not bump is still re-stated correctly rather than left at
//! whatever we recorded first.
//!
//! The mark is only promoted when a walk *completes*. Promoting it from
//! the first page of an interrupted first pass would declare the whole
//! history seen and the backfill would never happen.
//!
//! ## Where the state lives
//!
//! Four facts per repository — the in-flight page URL, the high-water
//! mark, a rate-limit "not before", and the last hard failure — and no
//! column of their own to put them in. They live in `import_cursor`
//! under the `checks:github:*` phases.
//!
//! That table is generically shaped (`repo_id`, `phase`, `cursor`), has
//! no CHECK on `phase`, cascades with the repository, and already has a
//! public reader in `imports::cursor` — which is what lets the API
//! surface the refusal without a line of new control-plane code. Its
//! *name* is wrong for this, and that is reported rather than hidden.
//!
//! `repos.sync_error` was the other candidate and is the wrong one.
//! `registry::set_sync` rewrites that column on every mirror fetch, so a
//! permission refusal recorded there would be erased by the next
//! successful `git fetch` — and until it was, the repository page would
//! say the *mirror* was broken, which is a different and untrue thing.
//! An authorization fact about the App's Actions permission survives
//! until somebody re-approves the installation, and it needs somewhere
//! that a git sync does not touch.

use crate::app::SharedState;
use crate::mirror::origin::{is_actions_read_denied, ActionsRun, ActionsRuns};
use stratum_control::checks::{self, NewCheckRun, RunState};
use stratum_control::ids::now_ms;
use stratum_control::imports;
use stratum_control::{jobs, registry};

/// The job kind. One word, no separator, matching `compact`, `cdnpack`
/// and `promote` rather than inventing a punctuation convention for the
/// sixth queue.
pub const JOB_KIND: &str = "checkspoll";

/// The provider these rows are written under. Not "github-actions": a
/// reader filtering by provider is asking which system reported, and
/// the workflow's own name is already in `name`.
pub const PROVIDER: &str = "github";

/// The name a run with no workflow name is filed under.
///
/// GitHub sends the workflow file's path when the file has no `name:`,
/// so a null here is a wire shape we tolerate rather than one we expect.
/// It is a fixed string, and the cost of that is stated plainly: two
/// genuinely unnamed workflows reporting on one commit collapse to one
/// line on the commit page, because `checks::latest_for_commit` keys on
/// the name a reader recognises. A per-run name — the run number — would
/// avoid the collapse and replace it with a Checks tab whose every row
/// has a different heading, which is worse.
const UNNAMED: &str = "(unnamed workflow)";

// --- the per-repository state, in `import_cursor` -------------------

/// The next page's URL while a walk is in flight. Empty means none.
const PHASE_CURSOR: &str = "checks:github:cursor";
/// The newest `updated_at` at the end of the last **completed** walk.
/// Present at all means at least one walk has finished, which is what
/// tells "never polled" from "polled, and this project has no CI".
const PHASE_HIGH_WATER: &str = "checks:github:high-water";
/// The newest `updated_at` seen by the walk in flight, not yet promoted.
const PHASE_PENDING: &str = "checks:github:pending";
/// Epoch milliseconds before which GitHub asked us not to come back.
const PHASE_AFTER: &str = "checks:github:after";
/// The last hard failure, verbatim, or empty once one has cleared it.
const PHASE_ERROR: &str = "checks:github:error";

/// How many pages one run walks before writing its cursor and
/// re-enqueueing.
///
/// Smaller than the importer's twenty because a page here is a hundred
/// runs of a *repeating* sweep rather than a one-off migration: the
/// steady state after the first pass is one page, and the budget only
/// binds while backfilling history nobody is waiting on.
fn page_budget() -> usize {
    super::env_secs("STRATUM_CHECKS_PAGES_PER_RUN", 5) as usize
}

/// What one run of the poller did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// The walk reached the end, or the line below which nothing has
    /// changed. The high-water mark has moved.
    Done { runs: usize },
    /// Budget spent or rate limited. The walk's resume point is
    /// written; the **caller** re-enqueues, once this job's row is no
    /// longer active — see [`spawn`] for why that order is not a
    /// preference.
    More { runs: usize },
    /// GitHub asked us not to come back yet. Nothing was requested, and
    /// the job is re-enqueued the same way `More` is.
    Backoff,
    /// The repository is gone, or is not a GitHub mirror.
    NotNeeded,
}

/// Ask for a poll of this repository's Actions runs.
///
/// `enqueue_unique` rather than `create`, because a poll is a sweep of
/// the repository's current CI state: running it twice is waste and
/// running it concurrently is two nodes walking one history. **The
/// dedup is not in force yet** — `jobs_active_per_repo` (migrations 0019
/// and 0026) lists four kinds and this is not one of them, so today the
/// insert never conflicts. This is the call that becomes correct the
/// moment the index covers `checkspoll`, and the alternative — `create`
/// — would still be wrong then. Reported to the owner of `db.rs` with
/// the migration rather than worked around here; nothing below depends
/// on the dedup for correctness.
pub fn enqueue(state: &SharedState, org_id: &str, repo_id: &str) -> Result<(), String> {
    jobs::enqueue_unique(&state.db, org_id, repo_id, JOB_KIND, None)?;
    Ok(())
}

pub fn spawn(state: SharedState) {
    let poll = super::env_period("STRATUM_CHECKS_POLL_SECS", 30);
    if poll.is_zero() {
        return;
    }
    // How long a crashed node's poll stays stuck. Modest, because a run
    // is bounded by design: nothing here legitimately takes minutes.
    let lease_ms = super::lease_ms("STRATUM_CHECKS_LEASE_SECS", 120);
    tokio::spawn(async move {
        loop {
            let claimed = {
                let db = state.db.clone();
                tokio::task::spawn_blocking(move || jobs::claim(&db, JOB_KIND, lease_ms))
                    .await
                    .unwrap_or_else(|e| Err(format!("join: {e}")))
            };
            match claimed {
                Ok(Some(job)) => match run_one(&state, &job).await {
                    Ok(o) => {
                        // **Complete first, then re-enqueue, in that
                        // order.** `jobs_active_per_repo` covers this
                        // kind, and its predicate is `state IN
                        // ('queued','running')` — so a follow-up
                        // enqueued from inside `run_one` conflicts with
                        // the very row that is running it, `ON CONFLICT
                        // DO NOTHING` no-ops, and the walk silently
                        // never resumes. That is what happened the day
                        // the index grew to cover `checkspoll`: three
                        // tests that had been green went red together,
                        // all of them the ones that depend on a bounded
                        // run continuing.
                        //
                        // The cost of this order is a window: a node
                        // dying between the two statements leaves no
                        // follow-up row. Nothing is *lost* — the cursor
                        // and the high-water mark are already written,
                        // so the next enqueue from anywhere resumes
                        // exactly where this run stopped — but that
                        // repository's backfill waits for it. The
                        // alternative loses the whole re-enqueue to a
                        // conflict every single time, which is not a
                        // window but a wall.
                        let _ = jobs::complete(&state.db, &job.id, Some(&format!("{o:?}")));
                        if matches!(o, PollOutcome::More { .. } | PollOutcome::Backoff) {
                            if let Some(repo_id) = &job.repo_id {
                                if let Err(e) = enqueue(&state, &job.org_id, repo_id) {
                                    eprintln!("weft: checks poll re-enqueue: {e}");
                                }
                            }
                        }
                        // A repository we have been asked not to call
                        // yet re-enqueues immediately, so without this
                        // the loop would spin claim-backoff-enqueue at
                        // the speed of the database. GitHub is not
                        // being dialled either way; the sleep is what
                        // keeps that true of Postgres too.
                        if o == PollOutcome::Backoff {
                            tokio::time::sleep(poll).await;
                        }
                    }
                    Err(e) => {
                        eprintln!("weft: checks poll failed: {e}");
                        let _ = jobs::fail(&state.db, &job.id, &e);
                    }
                },
                Ok(None) => tokio::time::sleep(poll).await,
                Err(e) => {
                    eprintln!("weft: checks poll claim: {e}");
                    tokio::time::sleep(poll).await;
                }
            }
        }
    });
}

pub async fn run_one(state: &SharedState, job: &jobs::Job) -> Result<PollOutcome, String> {
    let Some(repo_id) = job.repo_id.clone() else {
        return Err("checks poll job without repo".into());
    };
    let Some(repo) = registry::repo_by_id(&state.db, &job.org_id, &repo_id)? else {
        return Ok(PollOutcome::NotNeeded);
    };
    // A poll needs an upstream to poll, and it is the mirror's own —
    // the same `owner/name` and the same installation the commits come
    // through. A native repository has no Actions and asking for one is
    // not an error, it is simply nothing to do.
    let (Some(full_name), Some(installation)) = (
        repo.origin_url.clone().filter(|_| is_github(&repo)),
        repo.origin_installation.clone(),
    ) else {
        return Ok(PollOutcome::NotNeeded);
    };
    let Some(app) = state.sync.github_app() else {
        return Err("no GitHub App is configured on this deployment".into());
    };

    // GitHub told us when to come back. Honour it without asking: a
    // claimed job that dials anyway is how a rate limit becomes a ban.
    if let Some(after) = read_num(state, &repo.id, PHASE_AFTER)? {
        if now_ms() < after {
            return Ok(PollOutcome::Backoff);
        }
    }

    let seen = read_num(state, &repo.id, PHASE_HIGH_WATER)?;
    let mut pending = read_num(state, &repo.id, PHASE_PENDING)?.unwrap_or(0);
    let mut cursor = read_str(state, &repo.id, PHASE_CURSOR)?;
    let budget = page_budget();
    let mut walked = 0usize;
    let mut written = 0usize;

    loop {
        if walked >= budget {
            save_walk(state, &repo.id, cursor.as_deref(), pending)?;
            return Ok(PollOutcome::More { runs: written });
        }
        let got = match cursor.as_deref() {
            // A URL GitHub named, dialled through `actions_runs_next` so
            // that the host check applies: following it means handing an
            // installation token to whatever it points at.
            Some(url) => app.actions_runs_next(&installation, url),
            None => app.actions_runs(&installation, &full_name, 1),
        };
        let got = match got {
            Ok(g) => g,
            // Every hard failure is recorded where the API can read it,
            // not just on the job row. A refusal that only exists in a
            // worker's log is a Checks tab that shows nothing and says
            // nothing, which is the outcome this module exists to make
            // impossible. The message is stored verbatim; whether it is
            // *the* permission refusal is decided on the way out by the
            // predicate that owns that wording.
            Err(e) => {
                set(state, &repo.id, PHASE_ERROR, &e)?;
                return Err(e);
            }
        };
        let (runs, next) = match got {
            ActionsRuns::Data { runs, next } => (runs, next),
            // Not a failure. The verdicts are still there; the budget
            // is not. Keep the walk exactly where it is and say when we
            // may resume it.
            ActionsRuns::RateLimited { retry_after_secs } => {
                let until = now_ms() + (retry_after_secs as i64) * 1000;
                set(state, &repo.id, PHASE_AFTER, &until.to_string())?;
                save_walk(state, &repo.id, cursor.as_deref(), pending)?;
                return Ok(PollOutcome::More { runs: written });
            }
        };
        // A page arrived, so whatever was wrong before is not wrong now.
        // Cleared on success rather than on a timer: a stale refusal on
        // the screen is the same defect as a missing one.
        set(state, &repo.id, PHASE_ERROR, "")?;

        let fresh = runs
            .iter()
            .any(|r| r.updated_at.unwrap_or(0) > seen.unwrap_or(i64::MIN));
        for run in &runs {
            let Some(report) = as_report(run) else {
                continue;
            };
            checks::upsert(&state.db, &repo.id, &report)?;
            written += 1;
            pending = pending.max(run.updated_at.unwrap_or(0));
        }
        walked += 1;

        // Everything on this page was already at the mark, and the list
        // is ordered by recency of change, so everything below it is
        // too. Nothing further back can have moved.
        if seen.is_some() && !fresh {
            finish(state, &repo.id, pending, seen)?;
            return Ok(PollOutcome::Done { runs: written });
        }
        match next {
            Some(url) => {
                save_walk(state, &repo.id, Some(&url), pending)?;
                cursor = Some(url);
            }
            None => {
                finish(state, &repo.id, pending, seen)?;
                return Ok(PollOutcome::Done { runs: written });
            }
        }
    }
}

/// Whether this repository's origin is a GitHub one.
///
/// The provider column decides, not the shape of the URL: a repository
/// mirrored from somewhere else can perfectly well have an
/// `origin_installation` left over from a previous connection, and
/// polling GitHub for its runs would attribute another forge's history
/// to it.
fn is_github(repo: &registry::Repo) -> bool {
    repo.origin_provider.as_deref() == Some("github")
}

// --- state, read and written ----------------------------------------

/// A stored phase value, with the empty string read as absence.
///
/// `imports::set_cursor` is an upsert and there is no delete on that
/// surface, so "" is how a value is cleared — the same move
/// `checks_intake::load_secret` makes for a revoked secret, and for the
/// same reason: a cleared value must never be mistaken for a real one.
fn read_str(state: &SharedState, repo_id: &str, phase: &str) -> Result<Option<String>, String> {
    Ok(imports::cursor(&state.db, repo_id, phase)?.filter(|v| !v.is_empty()))
}

fn read_num(state: &SharedState, repo_id: &str, phase: &str) -> Result<Option<i64>, String> {
    // A value we cannot parse is a value we wrote, so it can only be
    // absent or a number; `ok()` here means a corrupted row degrades to
    // "not set" and the next completed walk repairs it, rather than
    // wedging the poller on a string nobody can fix from the UI.
    Ok(read_str(state, repo_id, phase)?.and_then(|v| v.parse().ok()))
}

fn set(state: &SharedState, repo_id: &str, phase: &str, value: &str) -> Result<(), String> {
    imports::set_cursor(&state.db, repo_id, phase, value)
}

/// Remember where an unfinished walk is, so the next run resumes rather
/// than starting from the front and spending the budget again.
fn save_walk(
    state: &SharedState,
    repo_id: &str,
    cursor: Option<&str>,
    pending: i64,
) -> Result<(), String> {
    set(state, repo_id, PHASE_CURSOR, cursor.unwrap_or(""))?;
    set(state, repo_id, PHASE_PENDING, &pending.to_string())
}

/// Promote the walk's high-water mark and clear the resume point.
///
/// `max` against what was already there, never a plain assignment: two
/// nodes that finish out of order must not move the mark backwards, and
/// a repository whose newest run predates the mark (every run deleted,
/// say) must not un-see everything above it.
fn finish(
    state: &SharedState,
    repo_id: &str,
    pending: i64,
    seen: Option<i64>,
) -> Result<(), String> {
    let mark = pending.max(seen.unwrap_or(0));
    set(state, repo_id, PHASE_HIGH_WATER, &mark.to_string())?;
    save_walk(state, repo_id, None, 0)
}

// --- what the API surfaces ------------------------------------------

/// What the poller has to say about one repository.
///
/// Four distinguishable answers, and the point of the type is that they
/// stay four: never polled, polled and refused, polled and there is no
/// CI, polled and here are the runs. Collapsing the middle two is the
/// bug; `denied` exists so the UI can say "this GitHub App installation
/// cannot read Actions" beside a re-approve link instead of drawing an
/// empty table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PollState {
    /// Has a walk ever completed? False here means "we have not looked",
    /// which is not the same as "there is nothing to see".
    pub polled: bool,
    /// The App installation lacks `actions: read`.
    pub denied: bool,
    /// The last hard failure, verbatim, for an operator to read.
    pub error: Option<String>,
    /// The newest `updated_at` we have seen, if any.
    pub high_water: Option<i64>,
    /// The page an interrupted walk will resume from. Deliberately
    /// visible: somebody watching a large backfill should see it move.
    pub resuming_from: Option<String>,
    /// Milliseconds until GitHub is willing to hear from us again.
    pub retry_in_ms: Option<i64>,
}

/// Read the poller's state for one repository.
pub fn state_of(state: &SharedState, repo_id: &str) -> Result<PollState, String> {
    let error = read_str(state, repo_id, PHASE_ERROR)?;
    let after = read_num(state, repo_id, PHASE_AFTER)?;
    Ok(PollState {
        polled: read_str(state, repo_id, PHASE_HIGH_WATER)?.is_some(),
        // By the predicate, never by a substring: the wording is a
        // message to a person and will be improved one day, and a
        // caller matching a fragment of it silently stops recognising
        // the case that afternoon.
        denied: error.as_deref().is_some_and(is_actions_read_denied),
        error,
        high_water: read_num(state, repo_id, PHASE_HIGH_WATER)?,
        resuming_from: read_str(state, repo_id, PHASE_CURSOR)?,
        retry_in_ms: after.map(|a| a - now_ms()).filter(|d| *d > 0),
    })
}

// --- one run, as a report -------------------------------------------

/// `Some(&str)` for text that carries information, `None` for the empty
/// string. A column holding `""` renders as a present-but-blank branch
/// name or a link to nowhere; absence renders as absence.
fn nonempty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// One GitHub Actions run as a report about a commit, or `None` when
/// there is nothing to key it on.
///
/// A run with no id cannot be upserted idempotently — it would land on
/// the id-less identity path and collide with every other unkeyed run of
/// the same workflow on the same commit — and a run with no `head_sha`
/// is a verdict about nothing. Neither is a shape GitHub sends; both are
/// dropped rather than stored wrong, because a plausible bad row in a
/// verdict table is worse than a missing one.
fn as_report(run: &ActionsRun) -> Option<NewCheckRun<'_>> {
    let commit_sha = nonempty(&run.head_sha)?;
    let external_id = nonempty(&run.id)?;
    // `run_state` is total over every (status, conclusion) pair — an
    // unrecognised conclusion degrades to `queued`, never to green — so
    // the six it can answer are exactly the six `RunState::parse`
    // accepts. `unwrap_or` states the same default rather than adding a
    // failure branch nothing can reach.
    let state = RunState::parse(run.state()).unwrap_or(RunState::Queued);
    Some(NewCheckRun {
        commit_sha,
        ref_name: run.head_branch.as_deref().and_then(nonempty),
        provider: PROVIDER,
        external_id: Some(external_id),
        name: run.name.as_deref().and_then(nonempty).unwrap_or(UNNAMED),
        run_number: Some(run.run_number),
        event: nonempty(&run.event),
        state,
        detail_url: nonempty(&run.html_url),
        actor: run.actor.as_deref(),
        started_at: run.run_started_at,
        // GitHub has no `completed_at` on a run; `updated_at` is when it
        // last moved, which for a finished run is when it finished.
        // Only claimed for a terminal state: on a run still in flight
        // that same timestamp is the last heartbeat, and recording a
        // completion time for something that has not completed puts a
        // duration on the screen that is a measurement of nothing.
        completed_at: match state {
            RunState::Queued | RunState::Running => None,
            _ => run.updated_at,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_run() -> ActionsRun {
        ActionsRun {
            id: "90000001".into(),
            name: Some("CI".into()),
            run_number: 41,
            head_sha: "3f2a1b4c5d6e7f8091a2b3c4d5e6f708192a3b4c".into(),
            head_branch: Some("main".into()),
            event: "push".into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
            html_url: "https://github.com/acme/widget/actions/runs/90000001".into(),
            actor: Some("octocat".into()),
            run_started_at: Some(1_700_000_000_000),
            updated_at: Some(1_700_000_060_000),
        }
    }

    #[test]
    fn every_field_a_checks_tab_shows_is_carried_across() {
        let run = a_run();
        let r = as_report(&run).expect("a whole run is reportable");
        assert_eq!(r.commit_sha, run.head_sha);
        assert_eq!(r.external_id, Some("90000001"));
        assert_eq!(r.provider, "github");
        assert_eq!(r.name, "CI");
        assert_eq!(r.ref_name, Some("main"));
        assert_eq!(r.event, Some("push"));
        assert_eq!(r.actor, Some("octocat"));
        assert_eq!(r.run_number, Some(41));
        assert_eq!(r.state, RunState::Passing);
        assert_eq!(r.detail_url.unwrap(), run.html_url);
        assert_eq!(r.started_at, Some(1_700_000_000_000));
        assert_eq!(r.completed_at, Some(1_700_000_060_000));
    }

    /// The optional fields are optional *on the wire*, and every one of
    /// them is null or absent on some real run. A default in any of
    /// these positions is a confident wrong answer: `""` renders as a
    /// branch with no name and a link to nowhere, and a zero timestamp
    /// renders as January 1970 at the top of a list sorted by date.
    #[test]
    fn absent_fields_stay_absent_rather_than_becoming_empty_strings() {
        let mut run = a_run();
        run.name = None;
        run.head_branch = None;
        run.actor = None;
        run.event = String::new();
        run.html_url = String::new();
        run.run_started_at = None;
        run.updated_at = None;
        let r = as_report(&run).expect("still reportable");
        assert_eq!(r.ref_name, None, "a tag build has no branch");
        assert_eq!(r.event, None);
        assert_eq!(r.actor, None, "a deleted account");
        assert_eq!(r.detail_url, None);
        assert_eq!(r.started_at, None);
        assert_eq!(r.completed_at, None);
        assert_eq!(r.name, UNNAMED, "named, not blank");

        // An empty string in the wire field is the same as absence, and
        // must not become a `Some("")` that renders as a blank row.
        let mut run = a_run();
        run.name = Some(String::new());
        run.head_branch = Some(String::new());
        assert_eq!(as_report(&run).unwrap().name, UNNAMED);
        assert_eq!(as_report(&run).unwrap().ref_name, None);
    }

    #[test]
    fn a_run_with_nothing_to_key_it_on_is_dropped_rather_than_stored_wrong() {
        let mut run = a_run();
        run.id = String::new();
        assert!(as_report(&run).is_none(), "no id to upsert on");
        let mut run = a_run();
        run.head_sha = String::new();
        assert!(as_report(&run).is_none(), "a verdict about no commit");
    }

    /// The whole (status, conclusion) space, and the two properties that
    /// matter about the edges: a completion time is claimed only for a
    /// run that completed, and a conclusion GitHub invents tomorrow
    /// reads as "we do not know yet" rather than as a green tick.
    #[test]
    fn a_completion_time_is_only_claimed_for_a_run_that_completed() {
        let cases: &[(&str, Option<&str>, RunState, bool)] = &[
            ("completed", Some("success"), RunState::Passing, true),
            ("completed", Some("failure"), RunState::Failing, true),
            ("completed", Some("timed_out"), RunState::Failing, true),
            ("completed", Some("cancelled"), RunState::Cancelled, true),
            ("completed", Some("skipped"), RunState::Skipped, true),
            ("completed", Some("neutral"), RunState::Skipped, true),
            // The one this codebase has never heard of.
            (
                "completed",
                Some("action_required"),
                RunState::Queued,
                false,
            ),
            ("in_progress", None, RunState::Running, false),
            ("queued", None, RunState::Queued, false),
            ("waiting", None, RunState::Queued, false),
        ];
        for (status, conclusion, want, terminal) in cases {
            let mut run = a_run();
            run.status = (*status).into();
            run.conclusion = conclusion.map(str::to_string);
            let r = as_report(&run).expect("reportable");
            assert_eq!(r.state, *want, "{status}/{conclusion:?}");
            assert_eq!(
                r.completed_at.is_some(),
                *terminal,
                "{status}/{conclusion:?} claimed the wrong completion"
            );
            assert_ne!(
                (r.state, *status),
                (RunState::Passing, "queued"),
                "an unmapped verdict must never read as a pass"
            );
        }
    }

    #[test]
    fn empty_text_is_absence() {
        assert_eq!(nonempty(""), None);
        assert_eq!(nonempty("main"), Some("main"));
    }
}
