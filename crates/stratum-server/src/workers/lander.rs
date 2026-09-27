//! The lander: claims `land` jobs and promotes changes onto their
//! target branch, fast-forward only, through the same manifest CAS
//! every push uses — so a landing and a concurrent push serialize on
//! the storage layer's one truth, never on luck.
//!
//! The verdict discipline is the product surface: every outcome is a
//! short sentence stored on the change (`landed`, `ejected: not
//! fast-forward from <tip>`, …), because "the queue rejected your
//! change" without a why is what makes people stop trusting queues.
//!
//! That discipline is why a landing can *hold*. The gate has three
//! answers, not two — ready, refused, and not-yet — and the third one
//! ejects nothing: the change stays `landing`, `land_verdict` says which
//! checks it is waiting on, and the job re-enqueues. Bounded, because
//! silence is the case a hold cannot survive: past
//! `STRATUM_LAND_WAIT_SECS` it ejects naming the checks that never
//! reported.

use crate::api::changes_api;
use crate::app::SharedState;
use stratum_control::audit::AuditCtx;
use stratum_control::changes::LandGate;
use stratum_control::ids::now_ms;
use stratum_control::{changes, jobs, registry};
use stratum_engine::ancestry::{self, Ancestry};
use stratum_engine::read::LayoutReader;
use stratum_engine::refops::{transact, Expect, RefUpdate, TxnError};
use stratum_store::{LatencyModel, Manifest, ObjectStore};

/// How many times a landing re-reads the tip and retries the CAS when
/// pushes keep winning it, before ejecting honestly.
const CAS_RETRIES: usize = 3;

/// How long a landing may hold for a required check that has not
/// reported yet, before it gives up and ejects naming the check.
///
/// A bound rather than "forever" because the failure this guards is
/// *silence*: a workflow deleted, an App installation that lost
/// `actions: read`, a CI system that posted a `pending` and then went
/// away. None of those ever produce a verdict, and a change parked in
/// `landing` with nothing coming is indistinguishable to its author from
/// a queue that has broken. Thirty minutes is longer than the CI of the
/// projects this is for and short enough that somebody is still at their
/// desk when it ejects.
///
/// A knob for the same reason the leases are: a monorepo whose test
/// matrix takes two hours has to be able to say so without patching the
/// binary.
const WAIT_SECS_DEFAULT: u64 = 1800;

fn wait_budget_ms() -> i64 {
    budget_ms(super::env_secs("STRATUM_LAND_WAIT_SECS", WAIT_SECS_DEFAULT))
}

/// Seconds to milliseconds, saturating at "effectively forever".
///
/// Split out from [`wait_budget_ms`] so the arithmetic is testable
/// without touching the environment. `std::env::set_var` is
/// process-global and Rust runs a binary's tests in threads, so a test
/// that set this knob would be setting it for whatever else in the same
/// binary happened to read it next, in whatever order the scheduler
/// chose — the "passes here, fails there" shape, planted deliberately.
///
/// Saturating, like `workers::lease_ms` and for the same reason: an
/// operator who types nonsense gets a budget nothing can exceed, never a
/// negative one that ejects every held change on its first look. The
/// comparison this feeds is `elapsed >= budget` rather than `now +
/// budget`, so there is no deadline to overflow into the past either.
fn budget_ms(secs: u64) -> i64 {
    i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX)
}

/// A duration in the words a person would use, from milliseconds.
///
/// Minutes and seconds only: the thing being described is a wait for CI
/// on the order of the wait budget, and an hours field would be dead
/// weight in every message that ever prints.
fn human_duration(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        _ => format!("{}m{}s", secs / 60, secs % 60),
    }
}

/// What one land job did with its change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandOutcome {
    /// The job is finished with this change — landed, ejected, or there
    /// was nothing to do. The string is the job's result, verbatim.
    Settled(String),
    /// A required check has not reported yet. The change stays in
    /// `landing` and this job ends; [`reap`] is what looks again.
    ///
    /// It carries only the check names, and only so the job row and the
    /// waiting note can say them. Nothing here is a handle the caller
    /// acts on — the state is, which is the whole point of doing it this
    /// way.
    Waiting { on: Vec<String> },
}

/// How long a change may sit in `landing` with nothing driving it before
/// the reaper picks it up.
///
/// This is two intervals that turned out to be one. It is the recheck
/// interval of a *hold* — a change waiting on CI is a change with no
/// live job, by construction — and it is the grace before a *stranded*
/// change is rescued. Twenty seconds because the input moves on the
/// scale of minutes: a build does not finish faster for being asked
/// about more often, and every ask is a job row.
///
/// It must stay comfortably above the time between creating a land job
/// and pointing the change at it, or the reaper would race the thing it
/// is rescuing. Two statements against the same database is microseconds
/// and this is twenty seconds, so the margin is six orders of magnitude
/// — but the CAS in [`reap`] is what makes it *correct*, not this.
const RECHECK_SECS_DEFAULT: u64 = 20;

fn recheck_ms() -> i64 {
    budget_ms(super::env_secs(
        "STRATUM_LAND_RECHECK_SECS",
        RECHECK_SECS_DEFAULT,
    ))
}

/// How many stranded changes one sweep rescues. A bound because this
/// runs on every idle tick and a fleet-wide incident could leave
/// thousands; they will be picked up on the ticks after this one.
const REAP_BATCH: i64 = 50;

/// Re-enqueue land jobs for changes stuck in `landing` with nothing
/// driving them.
///
/// **This is the only resume path, and that is deliberate.** A held
/// change and a stranded change are the same row: `state = 'landing'`,
/// no live job. So rather than have a hold remember to re-enqueue itself
/// — an intention that a node dying between two statements simply loses,
/// wedging the change forever — the hold completes its job and says
/// nothing, and this reads the state back and acts on it. An intention
/// can be dropped; a state cannot.
///
/// It closes a pre-existing door on the way past. A land job that `Err`s
/// goes to `jobs::fail`, which `claim` never re-claims, and one that
/// kills its node enough times is dead-lettered at `attempts >=
/// max_attempts`. Both left a change in `landing` that no writer would
/// accept — `set_landing` wants `open`, `abandon` wants `open`, and
/// `set_ejected` only ever runs from inside a land job — so recovery was
/// a manual `UPDATE`. Now it is a tick.
async fn reap(state: &SharedState) {
    let idle_before = now_ms().saturating_sub(recheck_ms());
    let db = state.db.clone();
    let stranded = tokio::task::spawn_blocking(move || {
        changes::stranded_landings(&db, idle_before, REAP_BATCH)
    })
    .await
    .unwrap_or_else(|e| Err(format!("join: {e}")));
    let stranded = match stranded {
        Ok(s) => s,
        Err(e) => {
            eprintln!("weft: land reap: {e}");
            return;
        }
    };
    for change in stranded {
        if let Err(e) = readopt(state, &change) {
            eprintln!("weft: land reap {}: {e}", change.change_key);
        }
    }
    super::changeset_lander::reap(state, idle_before).await;
}

/// Give one stranded change a fresh land job and point it at it.
///
/// **Create, then adopt, and the adopt is a CAS.** Two nodes can sweep
/// the same change in the same instant; `adopt_land_job` only succeeds
/// against the `land_job_id` we read, so exactly one of them wins and
/// the loser tidies away the job it made. Doing it the other way round
/// — claim the change, then create the job — would leave a change
/// pointing at a job that does not exist if the node died between, which
/// is a worse stranding than the one being fixed.
///
/// **The surviving window, stated honestly.** A node that dies after
/// creating and before adopting leaves a land job nobody is pointed at,
/// and the change still looks stranded — so the next sweep makes a
/// second one. Two land jobs then run for one change: the first drives
/// it, and the second finds the change is no longer `landing` and
/// returns `no-op`. So the cost is one wasted job row, once, and it is
/// self-limiting because the second attempt does adopt.
///
/// The order cannot be reversed to close it: `jobs::create` mints the id
/// it returns, so there is no id to write into `land_job_id` before the
/// row exists. Writing the column first would need a caller-supplied
/// job id, and the failure it would trade for is worse — a change
/// pointing at a job that was never created is stranded behind a
/// liveness signal that says it is fine.
///
/// **Bounded, at the job queue's own cap.** A land job that `Err`s goes
/// to `jobs::fail`, and the rescue is a fresh job with a fresh count, so
/// a change whose driver fails the same way every time — a store that
/// refuses every write to one repository — was rescued every recheck,
/// forever, and its author saw `landing` for as long as the store stayed
/// broken. The failures are counted in the payload, beside the clock,
/// and at `STRATUM_JOB_MAX_ATTEMPTS` of them the change is ejected with
/// the last error in its verdict. A hold is not a failure: a job that
/// completed saying `waiting on …` leaves the count where it was.
fn readopt(state: &SharedState, change: &changes::Change) -> Result<(), String> {
    let last = change
        .land_job_id
        .as_deref()
        .and_then(|id| jobs::get(&state.db, &change.org_id, id).ok().flatten());
    let last_payload = last
        .as_ref()
        .and_then(|j| j.payload.as_deref())
        .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
        .unwrap_or(serde_json::Value::Null);
    // The clock on a hold survives the job that was holding it. The
    // previous job's payload is where it lives, so a change that has
    // been waiting twenty-nine minutes does not get a fresh half hour
    // from a rescue — which would make the budget unreachable by exactly
    // the changes it exists to catch.
    let since = last_payload["waiting_since"]
        .as_i64()
        .or(last.as_ref().map(|j| j.created_at))
        .unwrap_or_else(now_ms);
    let failed_before = last_payload["failed_attempts"].as_i64().unwrap_or(0)
        + i64::from(last.as_ref().is_some_and(|j| j.state == "failed"));
    if failed_before >= jobs::max_attempts() {
        let error = last
            .and_then(|j| j.error)
            .unwrap_or_else(|| "no reason recorded".into());
        eject(
            state,
            change,
            format!(
                "ejected: the landing was given up after {failed_before} attempts; \
                 the last failed with: {error}"
            ),
        );
        return Ok(());
    }
    let payload = serde_json::json!({
        "change_id": change.id,
        "waiting_since": since,
        "failed_attempts": failed_before,
    })
    .to_string();
    let job = jobs::create(
        &state.db,
        &change.org_id,
        Some(&change.repo_id),
        "land",
        Some(&payload),
    )?;
    match changes::adopt_land_job(
        &state.db,
        &change.id,
        change.land_job_id.as_deref(),
        &job.id,
    ) {
        Ok(true) => Ok(()),
        // Another node got there first, or the change left `landing`
        // while we were making the row. Either way this job must not
        // run: a second lander on one change is the thing the CAS is
        // for, and leaving it queued would defeat it.
        Ok(false) => jobs::complete(&state.db, &job.id, Some("no-op: lost the reap race")),
        Err(e) => Err(e),
    }
}

pub fn spawn(state: SharedState) {
    let poll = super::env_period("STRATUM_LAND_POLL_SECS", 2);
    if poll.is_zero() {
        return;
    }
    // See the compactor's lease for why this is a knob and not a
    // constant: a crashed lander holds the change hostage until it lapses.
    let lease_ms = super::lease_ms("STRATUM_LAND_LEASE_SECS", 120);
    tokio::spawn(async move {
        loop {
            let claimed = {
                let db = state.db.clone();
                tokio::task::spawn_blocking(move || jobs::claim(&db, "land", lease_ms))
                    .await
                    .unwrap_or_else(|e| Err(format!("join: {e}")))
            };
            match claimed {
                Ok(Some(job)) => match run_one(&state, &job).await {
                    Ok(LandOutcome::Settled(outcome)) => {
                        let _ = jobs::complete(&state.db, &job.id, Some(&outcome));
                    }
                    // A required check is still running. The change is
                    // not landable and it is not refused either, so the
                    // job simply says what it is waiting for and ends.
                    //
                    // **Nothing is enqueued here, on purpose.** The
                    // obvious move is to re-enqueue a successor, and it
                    // is wrong in two ways at once. It is an intention
                    // held across two statements, so a node dying
                    // between them wedges the change in `landing`
                    // forever — and completing after enqueueing rather
                    // than before only trades that for the conflict that
                    // cost the importer and the checks poller their
                    // resume paths, the day `jobs_active_per_repo` grows
                    // to cover `land` (it covers `compact`, `cdnpack`,
                    // `fork`, `promote`, `import` and `checkspoll`
                    // today, and it has grown twice already).
                    //
                    // Ending the job *is* the request to be looked at
                    // again: it leaves the change in `landing` with no
                    // live job, which is exactly what [`reap`] reads.
                    // A state cannot be dropped the way an intention
                    // can, and the same tick that resumes this hold
                    // rescues a change whose job failed or was
                    // dead-lettered.
                    Ok(LandOutcome::Waiting { on, .. }) => {
                        let result = format!("{}{}", changes::LAND_WAITING_PREFIX, on.join(", "));
                        let _ = jobs::complete(&state.db, &job.id, Some(&result));
                    }
                    Err(e) => {
                        eprintln!("weft: land failed: {e}");
                        let _ = jobs::fail(&state.db, &job.id, &e);
                    }
                },
                // Nothing to claim is when there is time to look for
                // changes nothing is claiming *for*. On the idle tick
                // rather than on a timer of its own: a busy queue is one
                // that is draining, and a stranded change is by
                // definition not being worked on.
                Ok(None) => {
                    reap(&state).await;
                    tokio::time::sleep(poll).await;
                }
                Err(e) => {
                    eprintln!("weft: land claim: {e}");
                    tokio::time::sleep(poll).await;
                }
            }
        }
    });
}

enum RefusalOr<T> {
    Done(T),
    Ejected(String),
}

pub async fn run_one(state: &SharedState, job: &jobs::Job) -> Result<LandOutcome, String> {
    let payload: serde_json::Value = job
        .payload
        .as_deref()
        .and_then(|p| serde_json::from_str(p).ok())
        .unwrap_or(serde_json::Value::Null);
    // One queue, two kinds of plan. A changeset's landing is several
    // manifests driven from one recorded plan, and shares the claim
    // loop, the lease and the reaper's tick with the single-change
    // landing so that there is one answer to "what drives a landing".
    if let Some(landing_id) = payload["landing_id"].as_str() {
        return super::changeset_lander::run_one(state, job, landing_id).await;
    }
    let change_id = payload["change_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "land job without a change_id".to_string())?;
    // When this landing started waiting, not when this job was made.
    // A hold re-enqueues, so measuring the budget from the job's own
    // `created_at` would restart the clock every recheck and the
    // timeout would never arrive — the bound would be decoration. The
    // first job of a landing has no mark, and for it the two are the
    // same thing.
    let waiting_since = payload["waiting_since"].as_i64().unwrap_or(job.created_at);
    let Some(change) = changes::by_id(&state.db, &change_id)? else {
        // There is nothing left to eject.
        //
        // This used to say the repo and its changes vanish "by cascade"
        // between enqueue and claim. `changes.repo_id` does carry
        // `ON DELETE CASCADE`, but nothing in the product ever deletes a
        // `repos` row: `delete_repo` writes a tombstone (`state =
        // 'deleted'`), and the GC sweep purges the *storage* and marks
        // the row, never removes it. So the cascade cannot fire, and a
        // job whose repository was deleted settles on the repo guard
        // below instead — which is what
        // `a_land_job_whose_change_was_swept_away…` actually exercises.
        //
        // The guard stays, because a `change_id` that resolves to
        // nothing must never be landed and the row is read from a job
        // payload rather than from a live query. But it is now labelled
        // for what it is: a fail-closed check on an input, not a case
        // the schema produces.
        return Ok(LandOutcome::Settled("change vanished".into()));
    };
    if change.state != "landing" {
        // A re-leased job after a crash, or an operator moved it first:
        // the state row is the truth, the job just goes away.
        return Ok(LandOutcome::Settled(format!(
            "no-op: change is {}",
            change.state
        )));
    }
    let Some(repo) = registry::repo_by_id(&state.db, &change.org_id, &change.repo_id)? else {
        return Ok(LandOutcome::Settled("repo vanished".into()));
    };
    let Some(ps) = changes::latest_patchset(&state.db, &change.id)? else {
        eject(state, &change, "ejected: change has no patchsets".into());
        return Ok(LandOutcome::Settled("ejected: no patchsets".into()));
    };

    // 1. Sufficiency, re-evaluated at claim time. Enqueueing checked it
    //    too, but an approval revoked in between must count: the queue's
    //    authority is only as good as its last look.
    let verdict = match changes_api::compute_verdict(state, &change, &repo, &ps).await {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("ejected: land error — {e}");
            eject(state, &change, msg.clone());
            return Ok(LandOutcome::Settled(msg));
        }
    };
    if !verdict.landable {
        let msg = format!("ejected: sufficiency lost — {}", verdict.explanation);
        eject(state, &change, msg.clone());
        return Ok(LandOutcome::Settled(msg));
    }

    // 1b. The machine's verdict, re-read at claim time for the same
    //     reason: CI that turned red between enqueue and claim counts.
    //     Three answers, and the middle one is the whole point: a check
    //     that has not reported yet is not a pass and it is not a
    //     refusal, and the gate that collapsed those two let a change
    //     land before its build had started.
    match changes::land_gate(&state.db, &change.id)? {
        LandGate::Ready => {}
        LandGate::Blocked { reason } => {
            let msg = format!("ejected: {reason}");
            eject(state, &change, msg.clone());
            return Ok(LandOutcome::Settled(msg));
        }
        LandGate::Waiting { on } => {
            // Silence is the failure mode a hold cannot survive on its
            // own, so the hold is bounded and the ejection names the
            // checks that never arrived. "waited 30m0s for ci/tests,
            // which never reported" tells an author where to look; a
            // change that simply reappears as `open` does not.
            let waited = now_ms().saturating_sub(waiting_since);
            if waited >= wait_budget_ms() {
                let msg = format!(
                    "ejected: waited {} for {}, which never reported",
                    human_duration(waited),
                    on.join(", ")
                );
                eject(state, &change, msg.clone());
                return Ok(LandOutcome::Settled(msg));
            }
            // The change stays `landing` — the queue has not given up —
            // so `land_verdict` is the only place a person can read why
            // nothing is happening. Re-stated whenever the list of
            // checks changes, because the set shrinks as they report and
            // a stale list reads as a stuck queue; skipped when it has
            // not, because this runs once per poll interval for the
            // whole hold and an unconditional write would move
            // `changes.updated_at` every couple of seconds, which is a
            // row that looks busy while precisely nothing is happening.
            // Guarded on `landing` in the control plane, so a concurrent
            // eject or land wins and this cannot resurrect a note on a
            // change that has already left the queue.
            //
            // The list only, never the prefix: `set_land_waiting` owns
            // `LAND_WAITING_PREFIX` so that it is a discriminator rather
            // than a convention two crates have to remember separately.
            // The comparison below is against the stored form, which is
            // the list *with* the prefix, so it is built from the
            // constant rather than spelled out here — a literal would go
            // stale the moment that constant is reworded, silently, as a
            // write on every poll instead of a wrong string anybody
            // could see.
            let on_list = on.join(", ");
            let note = format!("{}{on_list}", changes::LAND_WAITING_PREFIX);
            if change.land_verdict.as_deref() != Some(note.as_str()) {
                if let Err(e) = changes::set_land_waiting(&state.db, &change.id, &on_list) {
                    eprintln!("weft: land waiting note {}: {e}", change.change_key);
                }
            }
            return Ok(LandOutcome::Waiting { on });
        }
    }

    // 2. Fast-forward proof + CAS promotion, retried while pushes win.
    let store_url = state.store_url.clone();
    let prefix = repo.prefix().as_str().to_string();
    let target_ref = format!("refs/heads/{}", change.target_branch);
    let commit = ps.commit_oid.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let store = ObjectStore::new(&store_url, LatencyModel::None);
        for _ in 0..=CAS_RETRIES {
            let tip = read_tip(&store, &prefix, &target_ref, &commit)?;
            let tip = match tip {
                RefusalOr::Done(t) => t,
                RefusalOr::Ejected(msg) => return Ok(RefusalOr::Ejected(msg)),
            };
            let expect = match &tip {
                Some(t) => Expect::Equals(t.clone()),
                None => Expect::Absent,
            };
            match transact(
                &store,
                &prefix,
                &[RefUpdate {
                    name: target_ref.clone(),
                    expect,
                    new: Some(commit.clone()),
                }],
                None,
            ) {
                Ok(_) => return Ok(RefusalOr::Done(commit.clone())),
                // A push won the CAS: re-read and re-prove against the
                // new tip rather than forcing anything.
                Err(TxnError::Conflict(_, _)) => continue,
                Err(TxnError::Other(e)) => return Err(e),
            }
        }
        Ok(RefusalOr::Ejected(
            "ejected: not fast-forward (trunk moved)".into(),
        ))
    })
    .await
    .unwrap_or_else(|e| Err(format!("join: {e}")))?;

    let landed_commit = match outcome {
        RefusalOr::Ejected(msg) => {
            eject(state, &change, msg.clone());
            return Ok(LandOutcome::Settled(msg));
        }
        RefusalOr::Done(c) => c,
    };

    // 3. Record it, tell the world, and reconcile the stack under it.
    changes::set_landed(&state.db, &change.id, &landed_commit, "landed")?;
    // Landing is never refused for want of room — the bytes were
    // accepted when they were pushed — but the row follows the manifest.
    crate::storage::refresh_after_write(state, &repo).await;
    announce_landed(state, &change, &landed_commit, ps.number).await;
    Ok(LandOutcome::Settled(format!("landed {landed_commit}")))
}

/// Everything that follows a change landing, once its row says so: the
/// audit line, the meter, the contribution walk, the notifications, and
/// the stack reconciliation under the new tip. Shared with the changeset
/// lander, whose members land through `finish_landing` rather than
/// `set_landed` and then owe the world exactly the same announcements.
pub(super) async fn announce_landed(
    state: &SharedState,
    change: &changes::Change,
    landed_commit: &str,
    patchset: i64,
) {
    let actx = AuditCtx::system(&change.org_id, "lander");
    crate::api::record_or_warn(
        &state.db,
        &actx,
        Some(&change.repo_id),
        "change.landed",
        Some(&serde_json::json!({
            "change_key": change.change_key,
            "commit": landed_commit,
            "branch": change.target_branch,
        })),
    );
    state.meter.record(&change.repo_id, "land", 0, None);
    // **Count the work.**
    //
    // A landing moves trunk, and moving trunk is the only way a commit
    // becomes part of a project here — but nothing enqueued a
    // contribution walk for it. The walk was enqueued on push (SSH,
    // HTTP, the REST commit route) and on a mirror sync, which covers
    // every path *except* the one this product tells people to use: on a
    // protected trunk nobody pushes, everything lands through review,
    // and so the walker never looked at the repository again. An outside
    // contributor's square appeared only if some maintainer later pushed
    // directly, and on a properly fenced project that never happens.
    //
    // It was invisible because it was masked by a second defect: a fork
    // holds its parent's history, forks were being walked, and the
    // contributor was picked up in *their own fork* instead — credited,
    // by the wrong repository, alongside every author of the history
    // they had forked. Fixing that one is what made this one show.
    //
    // `None` as the pusher: the lander is not a person, and the walk
    // attributes by the commit's own author line, which is the
    // contributor.
    crate::workers::contribs::enqueue(state, &change.org_id, &change.repo_id, None);
    // Everybody involved, told that it landed. The actor is the lander
    // itself rather than a person — a queue moved trunk, and there is
    // nobody to leave off the list, so the author hears about their own
    // change landing, which is exactly the one they want.
    crate::workers::notifier::enqueue(
        state,
        change,
        crate::workers::notifier::Event::Landed,
        "system:lander",
    );
    crate::workers::notify::notify(
        state,
        &change.repo_id,
        "change.landed",
        serde_json::json!({
            "change": change.change_key,
            "commit": landed_commit,
            "branch": change.target_branch,
            "patchset": patchset,
        }),
    );
    reconcile_stack(state, change, landed_commit).await;
}

/// The target's current tip, or the ejection that spares the CAS a
/// pointless round-trip. Runs on the blocking pool.
fn read_tip(
    store: &ObjectStore,
    prefix: &str,
    target_ref: &str,
    commit: &str,
) -> Result<RefusalOr<Option<String>>, String> {
    let mbytes = store.get(&format!("{prefix}/manifest.json"))?;
    let manifest: Manifest =
        serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;
    if manifest.object_format != "sha1" {
        return Err(format!(
            "object_format {:?} not supported by this build (sha1 only)",
            manifest.object_format
        ));
    }
    let reader = LayoutReader::new(store, prefix, &manifest)?;
    let tip = reader.ref_oid(target_ref)?;
    match ancestry::is_fast_forward(&reader, tip.as_deref(), commit)? {
        Ancestry::FastForward => Ok(RefusalOr::Done(tip)),
        Ancestry::NotAncestor => Ok(RefusalOr::Ejected(format!(
            "ejected: not fast-forward from {}",
            tip.as_deref().map(|t| &t[..12.min(t.len())]).unwrap_or("?")
        ))),
        Ancestry::CapExceeded => Ok(RefusalOr::Ejected(
            "ejected: history walk exceeded bound".into(),
        )),
    }
}

fn eject(state: &SharedState, change: &changes::Change, verdict: String) {
    match changes::set_ejected(&state.db, &change.id, &verdict) {
        Ok(true) => {}
        Ok(false) => return, // someone else moved the change; their truth wins
        Err(e) => {
            eprintln!("weft: eject {}: {e}", change.change_key);
            return;
        }
    }
    announce_ejected(state, change, &verdict);
}

/// The audit line and the webhook for a change the queue gave back,
/// once its row says `open` again. Shared with the changeset lander for
/// the same reason as [`announce_landed`].
pub(super) fn announce_ejected(state: &SharedState, change: &changes::Change, verdict: &str) {
    let actx = AuditCtx::system(&change.org_id, "lander");
    crate::api::record_or_warn(
        &state.db,
        &actx,
        Some(&change.repo_id),
        "change.ejected",
        Some(&serde_json::json!({
            "change_key": change.change_key,
            "verdict": verdict,
        })),
    );
    crate::workers::notify::notify(
        state,
        &change.repo_id,
        "change.ejected",
        serde_json::json!({ "change": change.change_key, "verdict": verdict }),
    );
}

/// After a landing, any open change whose latest patchset is now an
/// ancestor of the new tip has landed by inclusion — that is what makes
/// landing a stack's top land the stack, and what closes changes whose
/// commits arrived on trunk by an ordinary push.
pub(super) async fn reconcile_stack(state: &SharedState, landed: &changes::Change, new_tip: &str) {
    let open = match changes::list(&state.db, &landed.repo_id, Some("open"), 200) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("weft: stack reconcile list: {e}");
            return;
        }
    };
    if open.is_empty() {
        return;
    }
    let Ok(Some(repo)) = registry::repo_by_id(&state.db, &landed.org_id, &landed.repo_id) else {
        return;
    };
    // One reader pass answers every membership question.
    let mut included: Vec<(changes::Change, changes::Patchset)> = Vec::new();
    for change in open {
        if change.target_branch != landed.target_branch {
            continue;
        }
        let ps = match changes::latest_patchset(&state.db, &change.id) {
            Ok(Some(p)) => p,
            _ => continue,
        };
        included.push((change, ps));
    }
    if included.is_empty() {
        return;
    }
    let store_url = state.store_url.clone();
    let prefix = repo.prefix().as_str().to_string();
    let tip = new_tip.to_string();
    let commits: Vec<String> = included.iter().map(|(_, p)| p.commit_oid.clone()).collect();
    let verdicts = tokio::task::spawn_blocking(move || {
        let store = ObjectStore::new(&store_url, LatencyModel::None);
        let mbytes = store.get(&format!("{prefix}/manifest.json"))?;
        let manifest: Manifest =
            serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;
        let reader = LayoutReader::new(&store, &prefix, &manifest)?;
        let mut out = Vec::with_capacity(commits.len());
        for c in &commits {
            out.push(matches!(
                ancestry::is_fast_forward(&reader, Some(c), &tip)?,
                Ancestry::FastForward
            ));
        }
        Ok::<Vec<bool>, String>(out)
    })
    .await
    .unwrap_or_else(|e| Err(format!("join: {e}")));
    let verdicts = match verdicts {
        Ok(v) => v,
        Err(e) => {
            eprintln!("weft: stack reconcile walk: {e}");
            return;
        }
    };
    let tip12 = &new_tip[..12.min(new_tip.len())];
    for ((change, ps), on_trunk) in included.into_iter().zip(verdicts) {
        if !on_trunk {
            continue;
        }
        let verdict = format!("landed: included in {tip12}");
        match changes::set_landed(&state.db, &change.id, &ps.commit_oid, &verdict) {
            Ok(true) => {
                let actx = AuditCtx::system(&change.org_id, "lander");
                crate::api::record_or_warn(
                    &state.db,
                    &actx,
                    Some(&change.repo_id),
                    "change.landed",
                    Some(&serde_json::json!({
                        "change_key": change.change_key,
                        "commit": ps.commit_oid,
                        "branch": change.target_branch,
                        "included_in": new_tip,
                    })),
                );
                crate::workers::notify::notify(
                    state,
                    &change.repo_id,
                    "change.landed",
                    serde_json::json!({
                        "change": change.change_key,
                        "commit": ps.commit_oid,
                        "branch": change.target_branch,
                        "included_in": new_tip,
                    }),
                );
            }
            Ok(false) => {}
            Err(e) => eprintln!("weft: stack reconcile {}: {e}", change.change_key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wait budget is a knob, and an operator who sets it to
    /// something absurd gets "effectively forever" rather than a
    /// negative budget that ejects every held change on its first look.
    ///
    /// Asserted against the arithmetic, not against the environment: see
    /// [`budget_ms`] for why a test that wrote the knob would be a race
    /// with every other test in this binary.
    #[test]
    fn the_wait_budget_saturates_rather_than_wrapping() {
        assert_eq!(budget_ms(WAIT_SECS_DEFAULT), 1_800_000, "thirty minutes");
        assert_eq!(budget_ms(0), 0, "a zero budget ejects on the first look");
        assert_eq!(budget_ms(90), 90_000);
        assert_eq!(budget_ms(u64::MAX), i64::MAX, "not a negative budget");
        // The value a saturating multiply would wrap on if it were not
        // saturating: seconds enough to overflow i64 milliseconds.
        assert_eq!(budget_ms(u64::MAX / 999), i64::MAX);
        assert!(
            budget_ms(u64::MAX) > 0,
            "an absurd budget must mean forever, never a deadline in the past"
        );
    }

    /// The eject message an author reads is built out of this, and the
    /// long side of it is the side that prints in production — a wait
    /// that ran the full budget is minutes, not seconds. A test that
    /// only exercised the short arm would leave the arm that actually
    /// ships unproven.
    #[test]
    fn a_wait_is_reported_in_minutes_and_seconds() {
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(1_500), "1s");
        assert_eq!(human_duration(59_000), "59s");
        assert_eq!(human_duration(60_000), "1m0s");
        assert_eq!(human_duration(1_800_000), "30m0s");
        assert_eq!(human_duration(3_661_000), "61m1s");
        // A clock that stepped backwards between enqueue and claim must
        // not print a negative wait.
        assert_eq!(human_duration(-5_000), "0s");
    }
}
