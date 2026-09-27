//! The changeset lander: drives one recorded landing plan
//! (`changesets::Landing`) across several repositories, one manifest CAS
//! at a time, and finishes it — every member landed, or every member
//! that had landed reverted — no matter which node gets to finish it.
//!
//! What makes it all-or-nothing is not a transaction over the manifests,
//! because there is none; it is that the plan was written *before* the
//! first CAS and that every driver works from the plan and the store,
//! never from memory. A step the record says is `Done` is trusted. A
//! step it says is `Pending` is decided by reading the target ref: at
//! the new tip, it landed and the answer was lost; at the old tip, it is
//! still to do; anywhere else, the world moved under the plan and the
//! landing fails. That is what lets a job that died between two CASes
//! be picked up by the reaper and driven to the same end it would have
//! reached alone.
//!
//! The unwind is the revert route's own operation: a commit restoring
//! the pre-landing tree on top of the landed one, so trunk history in
//! every member repository stays append-only and a reader who cloned
//! inside the window sees a revert rather than a rewind.

use super::lander::LandOutcome;
use crate::api::refops_api::{restoring_commit, EMPTY_TREE};
use crate::app::SharedState;
use stratum_control::audit::AuditCtx;
use stratum_control::changesets::{self, Landing, MemberOutcome, Step, StepState};
use stratum_control::ids::now_ms;
use stratum_control::{changes, jobs, registry};
use stratum_engine::ancestry::{self, Descent};
use stratum_engine::objwrite::{self, ParsedCommit, OBJ_COMMIT};
use stratum_engine::read::LayoutReader;
use stratum_engine::refops::{transact, Expect, RefUpdate, TxnError};
use stratum_store::{LatencyModel, Manifest, ObjectStore};

/// How many stranded landings one sweep rescues; the same bound, for the
/// same reason, as the single-change reaper's.
const REAP_BATCH: i64 = 50;

/// The author line on a revert commit the unwind writes. Not a person:
/// the queue is undoing its own half-finished work.
const UNWIND_AUTHOR: &str = "weft-lander";

/// The first twelve hex digits, the way a person quotes a commit.
fn short(oid: &str) -> &str {
    &oid[..12.min(oid.len())]
}

/// One repository's manifest, fresh — every decision here is made
/// against the store as it is now, never against what a previous pass
/// remembered.
fn manifest(store: &ObjectStore, prefix: &str) -> Result<Manifest, String> {
    let mbytes = store.get(&format!("{prefix}/manifest.json"))?;
    let manifest: Manifest =
        serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;
    if manifest.object_format != "sha1" {
        return Err(format!(
            "object_format {:?} not supported by this build (sha1 only)",
            manifest.object_format
        ));
    }
    Ok(manifest)
}

/// A commit, parsed — or the reason the oid is not one. Every oid that
/// reaches here came out of a ref or a plan the engine wrote, so the
/// non-commit arm is a layout invariant being asserted, not a case.
fn commit_of(reader: &LayoutReader<'_>, oid: &str) -> Result<ParsedCommit, String> {
    let (kind, data) = reader.object(oid)?;
    if kind != OBJ_COMMIT {
        return Err(format!("{oid} is not a commit"));
    }
    objwrite::parse_commit(&data)
}

/// The tree a commit points at.
fn tree_of(reader: &LayoutReader<'_>, commit: &str) -> Result<String, String> {
    Ok(commit_of(reader, commit)?.tree)
}

/// Whether `candidate` is the revert this unwind would write for a step:
/// a commit whose only parent is the landed tip and whose tree is the
/// pre-landing tree. A revert CAS whose answer was lost — the store said
/// 503 after applying it — is recognised on the next pass this way,
/// rather than read as "somebody pushed since".
fn is_our_revert(
    reader: &LayoutReader<'_>,
    candidate: &str,
    landed: &str,
    restore_tree: &str,
) -> Result<bool, String> {
    let c = commit_of(reader, candidate)?;
    Ok(c.parents.len() == 1 && c.parents[0] == landed && c.tree == restore_tree)
}

/// Apply the plan's pending steps in order, stopping at the first that
/// cannot be made. Runs on the blocking pool; records every step's
/// answer before moving to the next, so a crash after the record leaves
/// nothing to rediscover and a crash before it leaves something the
/// store can answer.
///
/// `Err` is a store or database failure with the plan mid-flight; the
/// job fails and the reaper resumes from the record.
fn apply(
    db: &stratum_control::ControlDb,
    store: &ObjectStore,
    landing_id: &str,
    steps: &mut [Step],
    prefixes: &std::collections::HashMap<String, String>,
) -> Result<(), String> {
    for i in 0..steps.len() {
        match steps[i].state {
            StepState::Pending => {}
            // A failure the record already holds is the plan's end,
            // however this driver came to be looking at it. A driver
            // resumed after the *unwind* died used to walk past it and
            // land the members behind it — onto a changeset that had
            // already failed — and then revert them again a moment later.
            StepState::Failed => return Ok(()),
            StepState::Done | StepState::Reverted => continue,
        }
        let label = steps[i].label.clone();
        let prefix = &prefixes[&steps[i].repo_id];
        let m = manifest(store, prefix).map_err(|e| format!("{label}: {e}"))?;
        let reader = LayoutReader::new(store, prefix, &m).map_err(|e| format!("{label}: {e}"))?;
        let tip = reader
            .ref_oid(&steps[i].ref_name)
            .map_err(|e| format!("{label}: {e}"))?;
        let step = &mut steps[i];
        if tip.as_deref() == Some(step.new.as_str()) {
            // Landed by a driver that did not live to record it.
            step.state = StepState::Done;
        } else if tip != step.old {
            // Somewhere else. Either a push won the window, or the last
            // driver landed this and the trunk has taken a push since —
            // and only the history can tell the two apart. Reading it as
            // "moved before it could land" would eject a change whose
            // commit is on the trunk, and a person would be told to land
            // something that is already there.
            let at = tip.as_deref().map(short).unwrap_or("nothing");
            let since = tip
                .as_deref()
                .map(|t| ancestry::descent(&reader, t, &step.new))
                .transpose()?;
            match since {
                Some(Descent::Contains { .. }) => {
                    step.state = StepState::Done;
                    step.note = Some(format!(
                        "{} moved to {at} after {} landed",
                        step.ref_name, step.label
                    ));
                }
                Some(Descent::CapExceeded) => {
                    step.state = StepState::Failed;
                    step.note = Some(format!(
                        "{} moved to {at}; whether {} landed first is beyond the history walk's bound",
                        step.ref_name, step.label
                    ));
                }
                _ => {
                    step.state = StepState::Failed;
                    step.note = Some(format!(
                        "{} moved to {at} before {} could land",
                        step.ref_name, step.label
                    ));
                }
            }
        } else {
            let expect = match &step.old {
                Some(o) => Expect::Equals(o.clone()),
                None => Expect::Absent,
            };
            match transact(
                store,
                prefix,
                &[RefUpdate {
                    name: step.ref_name.clone(),
                    expect,
                    new: Some(step.new.clone()),
                }],
                None,
            ) {
                Ok(_) => step.state = StepState::Done,
                // A push won the ref between our read and our write. The
                // plan named a tip and it is gone; unlike a single change,
                // which re-proves fast-forward against whatever is there
                // now, a changeset's members were judged together against
                // the tips they were judged against, and re-proving one
                // alone would land a combination nobody reviewed.
                Err(TxnError::Conflict(_, cur)) => {
                    step.state = StepState::Failed;
                    step.note = Some(format!(
                        "{} moved to {} before {} could land",
                        step.ref_name,
                        cur.as_deref().map(short).unwrap_or("nothing"),
                        step.label
                    ));
                }
                Err(TxnError::Other(e)) => return Err(format!("{}: {e}", step.label)),
            }
        }
        let failed = step.state == StepState::Failed;
        changesets::record_progress(db, landing_id, steps).map_err(|e| e.to_string())?;
        if failed {
            return Ok(());
        }
    }
    Ok(())
}

/// Put back every member that landed, newest first, by appending a
/// commit that restores its pre-landing tree. A member whose trunk has
/// moved on since it landed is left landed and says so: rewinding over
/// somebody's push to undo our own is the one thing worse than a
/// half-landed changeset.
fn unwind(
    db: &stratum_control::ControlDb,
    store: &ObjectStore,
    landing_id: &str,
    key: &str,
    steps: &mut [Step],
    prefixes: &std::collections::HashMap<String, String>,
) -> Result<(), String> {
    let failed = steps
        .iter()
        .find(|s| s.state == StepState::Failed)
        .map(|s| {
            format!(
                "{} — {}",
                s.label,
                s.note.as_deref().unwrap_or("did not land")
            )
        })
        .unwrap_or_else(|| "a member did not land".to_string());
    for i in (0..steps.len()).rev() {
        if steps[i].state != StepState::Done || steps[i].note.is_some() {
            continue;
        }
        let label = steps[i].label.clone();
        let prefix = &prefixes[&steps[i].repo_id];
        let m = manifest(store, prefix).map_err(|e| format!("revert {label}: {e}"))?;
        let reader =
            LayoutReader::new(store, prefix, &m).map_err(|e| format!("revert {label}: {e}"))?;
        let step = &mut steps[i];
        let restore_tree = match &step.old {
            Some(o) => tree_of(&reader, o)?,
            None => EMPTY_TREE.to_string(),
        };
        let tip = reader.ref_oid(&step.ref_name)?;
        // The tip is where we left it, or it is not. If not, the commit
        // sitting directly on the landed one is either the revert a
        // previous pass wrote and never got to record — the store said
        // 503 after applying it — or somebody's push, and a push on top
        // of ours is not ours to rewind over. A trunk that no longer has
        // the landed commit in its history at all was reset by somebody
        // who knew what they were doing.
        if tip.as_deref() != Some(step.new.as_str()) {
            let above = match tip
                .as_deref()
                .map(|t| ancestry::descent(&reader, t, &step.new))
                .transpose()?
            {
                Some(Descent::Contains { child }) => child,
                _ => None,
            };
            match above {
                Some(c) if is_our_revert(&reader, &c, &step.new, &restore_tree)? => {
                    step.state = StepState::Reverted;
                    step.note = Some(c);
                }
                _ => {
                    step.note = Some(format!(
                        "not reverted: {} moved to {} after it landed",
                        step.ref_name,
                        tip.as_deref().map(short).unwrap_or("nothing")
                    ));
                }
            }
            changesets::record_progress(db, landing_id, steps).map_err(|e| e.to_string())?;
            continue;
        }
        let message = format!(
            "Revert {label}: changeset {key} did not land\n\n\
             Restores {ref_name} to {old} because {failed}.\n\n\
             Reverts commit {new}.",
            label = step.label,
            ref_name = step.ref_name,
            old = step
                .old
                .as_deref()
                .map(|o| format!("the tree of {}", short(o)))
                .unwrap_or_else(|| "the empty tree".to_string()),
            new = step.new,
        );
        let (pack, revert) =
            restoring_commit(&reader, &step.new, &restore_tree, UNWIND_AUTHOR, &message)?;
        match transact(
            store,
            prefix,
            &[RefUpdate {
                name: step.ref_name.clone(),
                expect: Expect::Equals(step.new.clone()),
                new: Some(revert.clone()),
            }],
            Some(&pack),
        ) {
            Ok(_) => {
                step.state = StepState::Reverted;
                step.note = Some(revert);
            }
            Err(TxnError::Conflict(_, cur)) => {
                step.note = Some(format!(
                    "not reverted: {} moved to {} after it landed",
                    step.ref_name,
                    cur.as_deref().map(short).unwrap_or("nothing")
                ));
            }
            Err(TxnError::Other(e)) => return Err(format!("revert {}: {e}", step.label)),
        }
        changesets::record_progress(db, landing_id, steps).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// What each member is left as, from the plan's final state.
fn outcomes(key: &str, steps: &[Step]) -> (bool, Vec<(String, MemberOutcome)>) {
    let failed = steps
        .iter()
        .find(|s| s.state == StepState::Failed)
        .map(|s| {
            format!(
                "{} — {}",
                s.label,
                s.note.as_deref().unwrap_or("did not land")
            )
        });
    let all_landed = failed.is_none();
    let members = steps
        .iter()
        .map(|s| {
            let outcome = match s.state {
                StepState::Done => MemberOutcome::Landed {
                    commit: s.new.clone(),
                    verdict: match &s.note {
                        Some(note) => format!("landed with changeset {key}; {note}"),
                        None => format!("landed with changeset {key}"),
                    },
                },
                StepState::Reverted => MemberOutcome::Reopened {
                    verdict: format!(
                        "landed, then reverted in {}: {} — push a new patchset to land again",
                        s.note.as_deref().map(short).unwrap_or("?"),
                        failed.as_deref().unwrap_or("the changeset did not land")
                    ),
                },
                StepState::Failed => MemberOutcome::Reopened {
                    verdict: format!("ejected: {}", s.note.as_deref().unwrap_or("did not land")),
                },
                StepState::Pending => MemberOutcome::Reopened {
                    verdict: format!(
                        "ejected: not attempted, {}",
                        failed.as_deref().unwrap_or("the changeset did not land")
                    ),
                },
            };
            (s.change_id.clone(), outcome)
        })
        .collect();
    (all_landed, members)
}

/// Drive one landing from wherever its record says it is to its end.
pub async fn run_one(
    state: &SharedState,
    job: &jobs::Job,
    landing_id: &str,
) -> Result<LandOutcome, String> {
    let Some(landing) = changesets::landing(&state.db, landing_id).map_err(|e| e.to_string())?
    else {
        // The job was made before the commit point and the commit point
        // refused, or the node died between the two. Nothing began.
        return Ok(LandOutcome::Settled("no-op: landing did not begin".into()));
    };
    if let Some(outcome) = &landing.outcome {
        return Ok(LandOutcome::Settled(format!(
            "no-op: landing already {outcome}"
        )));
    }
    if landing.job_id.as_deref() != Some(job.id.as_str()) {
        // A reaper has handed this landing to a newer job; two drivers
        // on one plan is what the adopt CAS exists to prevent.
        return Ok(LandOutcome::Settled(
            "no-op: another job is driving this landing".into(),
        ));
    }
    let Some(cs) =
        changesets::by_id(&state.db, &landing.changeset_id).map_err(|e| e.to_string())?
    else {
        return Ok(LandOutcome::Settled("changeset vanished".into()));
    };
    if cs.state != "landing" {
        return Ok(LandOutcome::Settled(format!(
            "no-op: changeset is {}",
            cs.state
        )));
    }
    let mut prefixes = std::collections::HashMap::new();
    for step in &landing.progress {
        let Some(repo) = registry::repo_by_id_any(&state.db, &step.repo_id)? else {
            return Err(format!("{}: repository vanished", step.label));
        };
        prefixes.insert(step.repo_id.clone(), repo.prefix().as_str().to_string());
    }

    let member_ids: Vec<String> = prefixes.keys().cloned().collect();
    let db = state.db.clone();
    let store_url = state.store_url.clone();
    let id = landing.id.clone();
    let key = cs.key.clone();
    let mut steps = landing.progress.clone();
    let steps = tokio::task::spawn_blocking(move || {
        let store = ObjectStore::new(&store_url, LatencyModel::None);
        apply(&db, &store, &id, &mut steps, &prefixes)?;
        if steps.iter().any(|s| s.state == StepState::Failed) {
            unwind(&db, &store, &id, &key, &mut steps, &prefixes)?;
        }
        Ok::<Vec<Step>, String>(steps)
    })
    .await
    .unwrap_or_else(|e| Err(format!("join: {e}")))?;

    // Every member's manifest may have moved — landed, or landed and
    // unwound; either way its row follows the manifest.
    for repo_id in &member_ids {
        crate::storage::refresh_any_by_id(state, repo_id).await;
    }

    Ok(LandOutcome::Settled(
        finish(state, &cs, &landing.id, &steps).await?,
    ))
}

/// Close a landing from its plan's final state: the record, the
/// changeset, every member's row, and then what the world is owed for
/// each — the same announcements a change landing alone would make.
/// Shared by the driver and by the reaper's give-up, which closes a
/// landing no driver could finish.
async fn finish(
    state: &SharedState,
    cs: &changesets::Changeset,
    landing_id: &str,
    steps: &[Step],
) -> Result<String, String> {
    let (all_landed, members) = outcomes(&cs.key, steps);
    let outcome = if all_landed { "landed" } else { "failed" };
    let actx = AuditCtx::system(&cs.org_id, "lander");
    let finished =
        changesets::finish_landing(&state.db, landing_id, outcome, steps, &members, &actx)
            .map_err(|e| e.to_string())?;
    if !finished {
        return Ok("no-op: another driver finished this landing".into());
    }
    for (change_id, m) in &members {
        let Ok(Some(change)) = changes::by_id(&state.db, change_id) else {
            continue;
        };
        match m {
            MemberOutcome::Landed { commit, .. } => {
                let patchset = changes::latest_patchset(&state.db, change_id)
                    .ok()
                    .flatten()
                    .map(|p| p.number)
                    .unwrap_or(0);
                super::lander::announce_landed(state, &change, commit, patchset).await;
            }
            MemberOutcome::Reopened { verdict } => {
                super::lander::announce_ejected(state, &change, verdict);
            }
        }
    }
    // The whole set's own outcome, once, to everybody the review is
    // addressed to. The per-member announcements above are the changes'
    // — "your change landed" — and they cannot say the thing a reader of
    // a changeset needs first: whether the *unit* landed. A failed
    // landing especially, where each member is separately told it was
    // reopened and nobody is told why.
    //
    // The actor is the queue rather than a person, the same convention
    // `lander::announce_landed` uses: a job finished this, and there is
    // nobody to leave off the list — so whoever pressed Land is told the
    // outcome, which is the notification they were waiting for.
    crate::workers::changeset_notifier::enqueue(
        state,
        cs,
        if all_landed {
            crate::workers::changeset_notifier::Event::Landed
        } else {
            crate::workers::changeset_notifier::Event::Failed
        },
        "system:lander",
    );
    let n = steps.len();
    Ok(match outcome {
        "landed" => format!("landed changeset {} ({n} members)", cs.key),
        _ => format!(
            "failed changeset {}: {}",
            cs.key,
            steps
                .iter()
                .find(|s| s.state == StepState::Failed)
                .and_then(|s| s.note.clone())
                .unwrap_or_default()
        ),
    })
}

/// Hand every unfinished landing with no live job to a fresh one. Runs
/// on the lander's idle tick, after the single-change sweep, with the
/// same grace.
pub async fn reap(state: &SharedState, idle_before: i64) {
    let db = state.db.clone();
    let stranded = tokio::task::spawn_blocking(move || {
        changesets::stranded_landings(&db, idle_before, REAP_BATCH)
    })
    .await
    .unwrap_or_else(|e| Err(changesets::Error::Db(format!("join: {e}"))));
    let stranded = match stranded {
        Ok(s) => s,
        Err(e) => {
            eprintln!("weft: changeset land reap: {e}");
            return;
        }
    };
    for landing in stranded {
        if let Err(e) = readopt(state, &landing).await {
            eprintln!("weft: changeset land reap {}: {e}", landing.id);
        }
    }
}

/// Create, then adopt by CAS — see `lander::readopt` for why in that
/// order, and for the one wasted job row a death between the two costs.
///
/// **Bounded, at the job queue's own cap.** A landing whose driver fails
/// the same way every time — a store answering 500 for one repository,
/// a commit the reader cannot parse — would otherwise be handed to a
/// fresh job every recheck, forever: `jobs::claim` dead-letters a job
/// after `STRATUM_JOB_MAX_ATTEMPTS` claims, but every rescue here is a
/// new job with a count of its own, so nothing ever met it. The landing's
/// `attempt` is the count that survives rescues, and it is held to the
/// same number.
///
/// At the cap the landing is not simply dropped, because members may
/// have landed. If the plan has not failed yet — the drivers kept dying
/// in the *apply* — it is failed where the last one stood and given one
/// more driver, whose apply stops at that step and whose unwind puts
/// back what had landed. If it has — the drivers kept dying in the
/// *unwind*, or that last driver did — the landing is closed as it
/// stands: each landed member is left landed and its note says it was
/// not reverted and why, so a person is told rather than a queue kept
/// warm.
async fn readopt(state: &SharedState, landing: &Landing) -> Result<(), String> {
    let Some(cs) =
        changesets::by_id(&state.db, &landing.changeset_id).map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    let last_error = landing
        .job_id
        .as_deref()
        .and_then(|id| jobs::get(&state.db, &cs.org_id, id).ok().flatten())
        .filter(|j| j.state == "failed")
        .map(|j| j.error.unwrap_or_else(|| "no reason recorded".into()));
    if let Some(error) = last_error.filter(|_| landing.attempt >= jobs::max_attempts()) {
        let reason = format!(
            "the landing was given up after {} attempts; the last failed with: {error}",
            landing.attempt
        );
        let mut steps = landing.progress.clone();
        let failed_already = steps.iter().any(|s| s.state == StepState::Failed);
        match steps.iter_mut().find(|s| s.state == StepState::Pending) {
            Some(step) if !failed_already => {
                // The apply walks the plan in order and stops at the
                // first error, so the first pending step is the one the
                // drivers kept dying on.
                step.state = StepState::Failed;
                step.note = Some(reason);
                changesets::record_progress(&state.db, &landing.id, &steps)
                    .map_err(|e| e.to_string())?;
            }
            _ => {
                if failed_already {
                    for s in &mut steps {
                        if s.state == StepState::Done && s.note.is_none() {
                            s.note = Some(format!("not reverted: {reason}"));
                        }
                    }
                }
                let result = finish(state, &cs, &landing.id, &steps).await?;
                eprintln!("weft: changeset land reap {}: {result}", landing.id);
                return Ok(());
            }
        }
    }
    let payload = serde_json::json!({
        "landing_id": landing.id,
        "changeset_id": landing.changeset_id,
        "resumed_at": now_ms(),
    })
    .to_string();
    let job = jobs::create(&state.db, &cs.org_id, None, "land", Some(&payload))?;
    match changesets::adopt_landing_job(&state.db, &landing.id, landing.job_id.as_deref(), &job.id)
    {
        Ok(true) => Ok(()),
        Ok(false) => jobs::complete(&state.db, &job.id, Some("no-op: lost the reap race")),
        Err(e) => Err(e.to_string()),
    }
}
