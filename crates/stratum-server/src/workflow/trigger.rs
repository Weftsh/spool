//! What starts a run: a push, a change, or a changeset.
//!
//! Three doors end here. The push doors (HTTP, SSH, the API commit) hand
//! over the refs they moved; the change route hands over the patchset it
//! just recorded; the changeset routes hand over a changeset whose
//! membership or whose members' tips have moved. This module reads
//! `.weft/` at the commit, decides what the files ask for, and writes
//! the runs — and it is the **only** writer of `running` runs, so the
//! rules below hold everywhere:
//!
//! - **One run per workflow file per commit.** A second push of the same
//!   sha, or a change re-posted with the same tip, finds the run that
//!   already exists and does not start another. CI that runs twice for
//!   one commit costs money and produces two verdicts that can disagree.
//! - **A new push to a branch cancels that branch's in-flight runs** —
//!   except on the default branch. A feature branch's old tip is
//!   history nobody will merge, so finishing its build is spent compute;
//!   the default branch is what people deploy from, and "was `main`
//!   green at 14:02" has to stay answerable.
//! - **A refused file is a failed run**, with the refusal as its error
//!   and a failing check on the commit, rather than nothing. A workflow
//!   that silently does not run looks exactly like one that has not
//!   started yet, and somebody waits for it.
//! - **A deployment with no runner fails the run at trigger time**, with
//!   an error naming the variables to set, for the same reason.
//! - **A change from a fork is `blocked`** rather than run: its workflow
//!   file was written by the contributor, and running it would hand a
//!   stranger a repo-read token and a machine. It shows as a queued
//!   check until a maintainer who may land the change approves it —
//!   `POST …/changes/:change/workflows/approve`, and the approval is
//!   for that tip only.
//! - **In a changeset the fork hold covers the whole composition.** A
//!   composed run materialises *every* member's tree under
//!   `$WEFT_WORKSPACE`, and a maintainer's own script may execute
//!   what it finds there — `make -C $WEFT_WORKSPACE/widget`, an
//!   `npm install` post-install. So one unapproved fork member blocks
//!   every member's run for that composition, not just its own, and the
//!   other members' rows name the member being waited on. See
//!   [`on_changeset`].
//! - **A composed run is superseded by the composition, not the
//!   commit.** A changeset's run is started for a *combination* of tips,
//!   so any member moving makes every one of them stale — including the
//!   members that did not move, whose build has not seen the new
//!   combination either. See [`on_changeset`].
//! - **A `timeout-minutes:` over the fleet's cap fails the run**,
//!   rather than being clamped down to the cap. A build told it may run
//!   for ten hours and stopped at six fails in a way its author cannot
//!   explain from anything they wrote.

use crate::api::reads::with_reader;
use crate::app::SharedState;
use crate::workflow::model::{Job, Pool, Trigger, Workflow};
use crate::workflow::read::{read_dir, stem, DIR};
use crate::workflow::{mirror, parse, plan};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use stratum_control::changes::Change;
use stratum_control::changeset_checks;
use stratum_control::changesets::{self, Changeset};
use stratum_control::registry::Repo;
use stratum_control::runners;
use stratum_control::workflows::{self, BlockedReason, NewJob, NewRun, Run};
use stratum_proto::receive::Update;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

/// What a run held for a maintainer says, when the change being held is
/// the one the run belongs to. One constant because the same sentence
/// is written by the `change` door and by the composed one, and a
/// second copy would drift out of step with the button it names.
const FORK_HELD: &str =
    "this change comes from a fork; a maintainer has to approve its workflows before they run";

/// The JSON a job carries into `workflow_jobs.spec` and out to the
/// runner: everything the runner needs to execute the job, frozen at
/// trigger time so a retry runs what the commit said.
pub fn job_spec(job: &Job, matrix: &BTreeMap<String, String>) -> serde_json::Value {
    // Workflow-level `env` is not a thing in this subset (the parser
    // refuses it), so the job's env is the whole environment.
    serde_json::json!({
        "image": job.image.as_deref().unwrap_or("default"),
        "timeout_minutes": job.timeout_minutes.unwrap_or(360),
        // The runner does not read these — it is handed a job, not a
        // choice of fleet — but the spec is the frozen record of what
        // the commit asked for, and "which fleet did this run on, and
        // why" is a question somebody asks about a build from last week.
        "pool": job.pool.as_str(),
        "labels": job.labels,
        "env": job.env,
        "matrix": matrix,
        "steps": job.steps.iter().map(|s| serde_json::json!({
            "name": s.name.clone().unwrap_or_else(|| s.run.clone()),
            "run": s.run,
            "env": s.env,
        })).collect::<Vec<_>>(),
    })
}

/// What one push door saw move.
pub async fn on_push(state: &SharedState, repo: &Repo, updates: &[Update]) {
    for u in updates {
        let Some(branch) = u.name.strip_prefix("refs/heads/") else {
            // Tags and everything else: workflows trigger on branches.
            continue;
        };
        if u.new == ZERO_OID {
            // The branch is gone; so is any reason to finish its build.
            supersede(state, repo, branch, "push", None, "branch deleted").await;
            continue;
        }
        if branch != repo.default_branch {
            supersede(
                state,
                repo,
                branch,
                "push",
                Some(&u.new),
                &format!("superseded by {}", &u.new[..12.min(u.new.len())]),
            )
            .await;
        }
        // The run records the branch by its short name, as every other
        // check row does; the runner API turns it back into the ref to
        // fetch.
        start(
            state,
            repo,
            &Cause {
                event: "push",
                sha: &u.new,
                ref_name: Some(branch),
                change_key: None,
                fork_hold: None,
                composed: None,
            },
        )
        .await;
    }
}

/// A change was opened or got a new patchset at `tip`. `target` is the
/// branch it will land on — what the run is recorded against, since a
/// patchset is not on a branch of its own.
pub async fn on_change(
    state: &SharedState,
    repo: &Repo,
    change_key: &str,
    target: &str,
    tip: &str,
    from_fork: bool,
) {
    let live = match workflows::live_runs_for_change(&state.db, &repo.id, change_key) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("weft: workflow trigger: live runs for change {change_key}: {e}");
            Vec::new()
        }
    };
    for run in live.iter().filter(|r| r.commit_sha != tip) {
        cancel_and_stop(
            state,
            &run.id,
            &format!("superseded by {}", &tip[..12.min(tip.len())]),
        )
        .await;
    }
    start(
        state,
        repo,
        &Cause {
            event: "change",
            sha: tip,
            ref_name: Some(target),
            change_key: Some(change_key),
            fork_hold: from_fork.then_some(FORK_HELD),
            composed: None,
        },
    )
    .await;
}

/// The changeset a composed run belongs to, and what that changeset was
/// when the run started.
///
/// Both, together, and never one without the other: the id says which
/// review the verdict belongs to and the hash says which *version* of it
/// produced the verdict, and a row carrying only the first is a verdict
/// about a combination nobody can name.
#[derive(Debug, Clone)]
pub struct Composed {
    pub changeset_id: String,
    pub composition: String,
}

/// One member of a changeset, resolved to everything a composed run
/// needs to be started, dispatched and checked out.
pub struct ComposedMember {
    pub repo: Repo,
    pub change: Change,
    /// The latest patchset's commit — this member's tip in the
    /// composition.
    pub commit_sha: String,
    /// When that patchset was uploaded, ms. Carried so the changeset
    /// workspace can date its commit by the newest proposal rather than
    /// by the moment somebody cloned it: a synthetic commit stamped with
    /// `now()` would have a different id on every request, and `git
    /// pull` would report a change every time.
    pub patchset_at: i64,
}

/// What the changeset is, right now: its members at their current tips,
/// and the hash that names that combination.
///
/// The hash is `sha256` over `repo_id:commit_sha` for every member,
/// sorted and newline-joined. Sorted rather than in landing order,
/// because the identity of a combination is the *set* of tips — adding
/// an edge reorders the landing and does not change what a build would
/// see, so it must not supersede a run that is still going.
///
/// A member whose repository has been deleted, or whose change is gone,
/// is left out — the same members the changeset's own view
/// (`changesets_api::load_members`) leaves out, so what is built is what
/// the person is shown and what landing would land. It used to make the
/// whole changeset uncomposable, which switched composed CI off for the
/// rest of that changeset's life while the body went on showing the
/// members it would have built.
///
/// `None` means the changeset cannot be composed at all: no member is
/// left, or one has no patchset to build. There is no combination to
/// build and — this is the part worth stating — no combination to
/// supersede an existing run *with* either, so a caller that gets `None`
/// leaves live composed runs alone rather than cancelling them into a
/// state where nothing will ever replace them.
pub fn compose(state: &SharedState, cs: &Changeset) -> Option<(String, Vec<ComposedMember>)> {
    let members = match changesets::members(&state.db, &cs.id) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "weft: workflow trigger: members of changeset {}: {e}",
                cs.key
            );
            return None;
        }
    };
    let mut out = Vec::with_capacity(members.len());
    for m in &members {
        let change = match stratum_control::changes::by_id(&state.db, &m.change_id) {
            Ok(Some(c)) => c,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("weft: workflow trigger: member change {}: {e}", m.change_id);
                return None;
            }
        };
        let repo =
            match stratum_control::registry::repo_by_id(&state.db, &cs.org_id, &change.repo_id) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(e) => {
                    eprintln!(
                        "weft: workflow trigger: member repo {}: {e}",
                        change.repo_id
                    );
                    return None;
                }
            };
        let (commit_sha, patchset_at) =
            match stratum_control::changes::latest_patchset(&state.db, &change.id) {
                Ok(Some(p)) => (p.commit_oid, p.created_at),
                Ok(None) => return None,
                Err(e) => {
                    eprintln!("weft: workflow trigger: member patchset {}: {e}", change.id);
                    return None;
                }
            };
        out.push(ComposedMember {
            repo,
            change,
            commit_sha,
            patchset_at,
        });
    }
    if out.is_empty() {
        return None;
    }
    Some((composition_of(&out), out))
}

fn composition_of(members: &[ComposedMember]) -> String {
    composition_hash(
        members
            .iter()
            .map(|m| format!("{}:{}", m.repo.id, m.commit_sha)),
    )
}

/// The hash itself, over `repo:commit` strings.
///
/// Split from [`composition_of`] so the rules the identity rests on —
/// order does not matter, every tip does — can be tested with no
/// database behind them. A hash a test cannot reach is a hash whose
/// ordering bug is found by a build that ran twice.
fn composition_hash(parts: impl IntoIterator<Item = String>) -> String {
    let mut parts: Vec<String> = parts.into_iter().collect();
    parts.sort();
    let mut hasher = Sha256::new();
    hasher.update(parts.join("\n").as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A changeset was composed, recomposed, or a member got a new patchset:
/// run every member's `on: changeset` files against the combination as
/// it now stands.
///
/// The supersession rule is the same one a new patchset gets, moved up a
/// level: a run is stale when the *combination* it was started for is no
/// longer the combination, whichever member moved. Runs already at the
/// current composition are left exactly alone — `start` finds them by
/// `run_for_commit` and does not start a second. A *cancelled* run at
/// the current composition does not count as one: remove a member and
/// add it back and the tips are what they were, but the build for them
/// was cancelled when they stopped being current and nobody has built
/// them since — `run_for_commit` says why it is narrow to composed runs.
///
/// The one exception is a `blocked` placeholder left by the fork hold
/// after the hold has lifted: it is at the current composition, so
/// nothing supersedes it, and `run_for_commit` matches a `blocked` run,
/// so leaving it would mean the real run is never created. Those are
/// deleted here — the same two rows `changes_api::approve_workflows`
/// removes at the approved change's own tip, for the members whose tips
/// that route never sees.
pub async fn on_changeset(state: &SharedState, cs: &Changeset) {
    let Some((composition, members)) = compose(state, cs) else {
        // Nothing to build and nothing to replace what is running with.
        // Leaving the live runs alone is deliberate: cancelling them
        // here would leave a changeset whose checks are cancelled and
        // whose next patchset is the only thing that could restart them.
        return;
    };
    let live = match workflows::live_runs_for_changeset(&state.db, &cs.id) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "weft: workflow trigger: live runs for changeset {}: {e}",
                cs.key
            );
            Vec::new()
        }
    };
    for run in live
        .iter()
        .filter(|r| r.composition.as_deref() != Some(composition.as_str()))
    {
        cancel_and_stop(
            state,
            &run.id,
            &format!("superseded by a new composition of changeset {}", cs.key),
        )
        .await;
    }
    // Which members are unapproved forks, decided once for the whole
    // composition and before anything is started. The hold is a
    // property of the *combination*, not of the member carrying the
    // fork: a composed run materialises every member's tree under
    // `$WEFT_WORKSPACE` and the maintainer's own script may execute
    // what it finds there, so one stranger's patch reaches every
    // member's runner whichever repository the run is recorded against.
    let unapproved: Vec<bool> = members.iter().map(|m| from_fork(state, m)).collect();
    let waiting_on = unapproved.iter().position(|&h| h);
    if waiting_on.is_none() {
        // The hold has lifted. `approve_workflows` deleted the
        // placeholders at the approved change's own tip, but the other
        // members' placeholders are in their own repositories at tips
        // that have not moved — the composition is unchanged, so the
        // supersession above left them alone and `start`'s idempotency
        // guard, which counts a `blocked` run, would find them and never
        // create the real run. So they go here, both rows,
        // exactly as the approve route removes its own.
        for run in live.iter().filter(|r| {
            r.composition.as_deref() == Some(composition.as_str())
                && r.state == "blocked"
                && r.blocked_reason.as_deref() == Some(BlockedReason::Fork.as_str())
        }) {
            if let Err(e) = changeset_checks::delete_external(&state.db, &run.id) {
                eprintln!("weft: workflow trigger: composed check for {}: {e}", run.id);
            }
            if let Err(e) = workflows::delete_blocked_run(&state.db, &run.id) {
                eprintln!("weft: workflow trigger: delete blocked run {}: {e}", run.id);
            }
        }
    }
    let composed = Composed {
        changeset_id: cs.id.clone(),
        composition,
    };
    for (i, m) in members.iter().enumerate() {
        // A member that is itself the unapproved fork says so in the
        // words its own change would use — the approve button is on
        // *its* change and the sentence has to lead there. Every other
        // member names the member being waited on instead, because
        // "this change comes from a fork" on a maintainer's own repo,
        // about a change that does not, is a sentence nobody can act on.
        let held = waiting_on.map(|w| {
            fork_hold(
                unapproved[i],
                &members[w].repo.name,
                &members[w].change.change_key,
            )
        });
        start(
            state,
            &m.repo,
            &Cause {
                event: "changeset",
                sha: &m.commit_sha,
                ref_name: Some(&m.change.target_branch),
                change_key: Some(&m.change.change_key),
                fork_hold: held.as_deref(),
                composed: Some(&composed),
            },
        )
        .await;
    }
}

/// Whether this member is an unapproved fork change.
///
/// It is the caller that turns this into a hold, and the hold covers the
/// whole composition rather than this member's run — see
/// [`on_changeset`].
///
/// Being a fork change is not enough on its own, and that is the whole
/// subtlety. A maintainer who has already approved this exact tip has a
/// *started* run at it — the approval is for the tip, and it deleted the
/// placeholder to make one — so re-blocking the composed run would put
/// an approval they have already given back behind the button, and every
/// recomposition would ask them again for the same commit.
///
/// So: a fork member is blocked only while nothing at this tip has got
/// past the gate. `blocked` runs do not count as past it — they *are*
/// the gate — and a run in any other state is one a maintainer let
/// through, or one that never needed approval at all.
///
/// A database error reads as "not approved", which is the safe direction
/// here and the opposite of the rule [`pool_refusal`] follows: this gate
/// protects somebody's machine from a stranger's code, and the failure
/// mode of guessing wrong is running it.
fn from_fork(state: &SharedState, m: &ComposedMember) -> bool {
    if m.change.source_repo_id.is_none() {
        return false;
    }
    match workflows::runs_for_change_commit(
        &state.db,
        &m.repo.id,
        &m.change.change_key,
        &m.commit_sha,
    ) {
        Ok(runs) => !runs.iter().any(|r| r.state != "blocked"),
        Err(e) => {
            eprintln!(
                "weft: workflow trigger: runs at {} for {}: {e}",
                m.commit_sha, m.change.change_key
            );
            true
        }
    }
}

/// What a composed run held for a maintainer says.
///
/// Two sentences, because the member the hold is *about* is not usually
/// the member being held. `held_self` is the member whose own change is
/// the unapproved fork: it says what the `change` door says, because the
/// approve button is on that change and the sentence has to lead there.
/// Every other member names the member being waited on instead — "this
/// change comes from a fork", on a maintainer's own repository about a
/// change that did not come from one, is a sentence nobody can act on.
fn fork_hold(held_self: bool, repo: &str, change_key: &str) -> String {
    if held_self {
        return FORK_HELD.to_string();
    }
    format!(
        "member {repo}/{change_key} comes from a fork; a maintainer has to \
         approve its workflows before the changeset's runs"
    )
}

/// Stop every composed run of a changeset — what abandoning it does.
pub async fn cancel_changeset(state: &SharedState, cs: &Changeset, reason: &str) {
    let live = match workflows::live_runs_for_changeset(&state.db, &cs.id) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "weft: workflow trigger: live runs for changeset {}: {e}",
                cs.key
            );
            return;
        }
    };
    for run in &live {
        cancel_and_stop(state, &run.id, reason).await;
    }
}

/// Cancel the live runs for a ref whose tip is not `keep`.
async fn supersede(
    state: &SharedState,
    repo: &Repo,
    ref_name: &str,
    event: &str,
    keep: Option<&str>,
    reason: &str,
) {
    let live = match workflows::live_runs_for_ref(&state.db, &repo.id, ref_name, event) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("weft: workflow trigger: live runs for {ref_name}: {e}");
            return;
        }
    };
    for run in live.iter().filter(|r| Some(r.commit_sha.as_str()) != keep) {
        cancel_and_stop(state, &run.id, reason).await;
    }
}

/// Cancel a run, stop whatever was running for it, and bring the
/// mirrored checks along. The one door for cancellation, so the API
/// route and the trigger agree on what "cancelled" leaves behind.
pub async fn cancel_and_stop(state: &SharedState, run_id: &str, reason: &str) -> Option<Run> {
    let running = match workflows::cancel_run(&state.db, run_id, reason) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("weft: cancel run {run_id}: {e}");
            return None;
        }
    };
    crate::workflow::credentials::stop_jobs(state, &running, reason).await;
    let run = workflows::run_by_id(&state.db, run_id).ok().flatten()?;
    let page = mirror::run_page_by_id(&state.db, &state.public_url, &run.repo_id, &run.id);
    if let Ok(jobs) = workflows::jobs_of(&state.db, run_id) {
        for job in &jobs {
            if let Err(e) = mirror::mirror_job(&state.db, &run.repo_id, &run, job, page.as_deref())
            {
                eprintln!("weft: mirror job {}: {e}", job.id);
            }
        }
    }
    Some(run)
}

/// Why this file may not run at all, whoever pushed it — or `None` if it
/// may.
///
/// Five refusals, in an order that is deliberate: each one is a
/// different person's decision, and a reader has to be told the *first*
/// thing that is wrong rather than the last. A `ubuntu-latest` file is
/// not going to run on a server with no hosted fleet however many
/// machines are registered, and a repository no group admits will not
/// be routed however many labels match.
///
/// All five settle the run as **failed with no blocked reason**, and
/// that is the difference between them and the fork gate: an approval
/// lifts that one, so a `blocked` run is a run that might still happen.
/// These do not. Somebody has to edit the file or edit the settings, and
/// a check row that says "waiting" for a thing that will never happen is
/// the failure mode this whole family exists to avoid.
///
/// **A database error here is not a refusal**: the failure mode of
/// treating an unreadable `orgs` row as "forbidden" is every
/// organisation's CI stopping the moment Postgres hiccups. An unrefused
/// job that cannot be routed simply sits in the queue, which is
/// recoverable; a refused run is not.
fn pool_refusal(state: &SharedState, repo: &Repo, wf: &Workflow) -> Option<String> {
    let warn = |what: &str, e: stratum_control::runners::Error| {
        eprintln!("weft: workflow trigger: {what} for {}: {e}", repo.id);
    };
    // Every job runs on the organisation's own machines here: there is
    // no hosted fleet behind this server. A job whose `runs-on` does not
    // name `self-hosted` is refused by the file rather than queued for a
    // runner that will never exist, and the sentence says what to write.
    if let Some(job) = wf.jobs.iter().find(|j| j.pool == Pool::Hosted) {
        return Some(format!(
            "job {:?} asks for runs-on: {}, but this server runs workflows only on \
             self-hosted runners; use runs-on: [self-hosted, …]",
            job.id,
            job.labels.join(", ")
        ));
    }
    let self_hosted: Vec<&Job> = wf
        .jobs
        .iter()
        .filter(|j| j.pool == Pool::SelfHosted)
        .collect();
    if self_hosted.is_empty() {
        return None;
    }
    // Before any policy question, because this one is a property of the
    // file alone and the answer does not change with who is asking. A
    // self-hosted job runs its steps directly on the machine somebody
    // owns — there is no container to put an image in — so a file that
    // names one is asking for something that cannot happen, and saying
    // so is more useful than silently ignoring the line.
    if let Some(reason) = image_refusal(&self_hosted) {
        return Some(reason);
    }
    match runners::self_hosted_allowed(&state.db, &repo.org_id, &repo.id) {
        Ok(false) => {
            return Some(
                "self-hosted runners are not allowed for this repository \
                 (organisation policy)"
                    .to_string(),
            )
        }
        Ok(true) => {}
        Err(e) => warn("self-hosted runner policy", e),
    }
    match runners::any_group_admits(&state.db, &repo.org_id, &repo.id) {
        Ok(false) => {
            return Some(
                "no runner group admits this repository; add it to a group \
                 under Settings → Runners"
                    .to_string(),
            )
        }
        Ok(true) => {}
        Err(e) => warn("runner group admission", e),
    }
    // Per job, because a file may ask for two different machines and
    // only one of them may be missing. The labels are printed in the
    // order the file wrote them so the sentence is recognisable as the
    // `runs-on:` line its author typed.
    for job in self_hosted {
        match runners::any_runner_for(&state.db, &repo.org_id, &repo.id, &job.labels) {
            Ok(false) => {
                return Some(format!(
                    "no runner with labels [{}] is registered for this repository",
                    job.labels.join(", ")
                ))
            }
            Ok(true) => {}
            Err(e) => warn("runners for labels", e),
        }
    }
    None
}

/// The one refusal a self-hosted job earns from its own text, with no
/// question asked of the database: it named an image. A self-hosted job
/// runs its steps directly on the machine, as the person who registered
/// it, in whatever the machine has — there is no container to put an
/// image in. Ignoring the line would be worse than refusing it: the
/// build would run, and run against the wrong toolchain.
fn image_refusal(self_hosted: &[&Job]) -> Option<String> {
    self_hosted
        .iter()
        .find_map(|job| match job.image.as_deref() {
            None | Some("default") => None,
            Some(image) => Some(format!(
                "image {image:?} is not available on self-hosted runners; \
             steps run directly on the machine"
            )),
        })
}

/// What is asking for a run, and everything about it a workflow file's
/// verdict is recorded against.
///
/// A struct rather than six positional parameters, for the reason
/// `auth::Mint` gives at length: `sha`, `ref_name` and `change_key` are
/// three adjacent strings the compiler cannot tell apart, so a
/// transposition between them type-checks and files a build's verdict
/// against the wrong thing. Naming them at the call site makes that a
/// compile error instead of a run nobody can find.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cause<'a> {
    /// `push`, `change` or `changeset` — the literal in
    /// `workflow_runs.event`, and what a file's `on:` is matched against.
    pub event: &'a str,
    pub sha: &'a str,
    /// The branch the run is recorded against: the one pushed to, or the
    /// one a change or member will land on.
    pub ref_name: Option<&'a str>,
    pub change_key: Option<&'a str>,
    /// Hold the run for a maintainer rather than running a stranger's
    /// workflow file — the sentence to leave where the verdict would
    /// go, since the member a composed hold is *waiting on* is not
    /// always the member being held. See the fork rules in the module
    /// doc.
    pub fork_hold: Option<&'a str>,
    /// Set for a composed run, and for nothing else.
    pub composed: Option<&'a Composed>,
}

/// Read `.weft/` at `sha` and write a run per file that asks for
/// `event`. Idempotent per (file, commit, change, composition).
pub(crate) async fn start(state: &SharedState, repo: &Repo, cause: &Cause<'_>) {
    let Cause {
        event,
        sha,
        ref_name,
        change_key,
        fork_hold,
        composed,
    } = *cause;
    // Carried into every `NewRun` below and into the idempotency guard
    // beside it: one run per workflow file per commit *per composition*,
    // so a member whose tip has not moved still gets a fresh run when
    // the rest of the changeset does — it is a different combination and
    // a different build.
    let (changeset_id, composition) = match composed {
        Some(c) => (Some(c.changeset_id.as_str()), Some(c.composition.as_str())),
        None => (None, None),
    };
    let prefix = repo.prefix().as_str().to_string();
    let rev = sha.to_string();
    let files = match with_reader(state, prefix, move |reader| read_dir(reader, &rev)).await {
        Ok(Some(f)) => f,
        Ok(None) => return,
        Err(e) => {
            eprintln!("weft: workflow trigger: read {DIR} at {sha}: {e}");
            // From the outside, a push whose workflow directory could not
            // be read looks exactly like a push whose CI has not started
            // — and this one has already happened: a fold between two
            // pushes left the second one's `.weft/` unreadable for a
            // whole afternoon, with nothing on the commit but this line
            // in a log nobody was watching. Say so where the verdict
            // would have gone. The row is named for the directory, since
            // which files it held is precisely what could not be learned.
            let already = workflows::run_for_commit(
                &state.db,
                &repo.id,
                event,
                sha,
                change_key,
                DIR,
                composition,
            )
            .ok()
            .flatten()
            .is_some();
            if !already {
                let new = NewRun {
                    file: DIR,
                    name: "weft",
                    commit_sha: sha,
                    ref_name,
                    event,
                    change_key,
                    changeset_id,
                    composition,
                    from_fork: fork_hold.is_some(),
                };
                let why = format!(
                    "{DIR}/ at {sha} could not be read: {e}. Nothing ran. This is a \
                     problem on Stratum's side, not with the workflow files."
                );
                settle(state, repo, &new, "failed", &why, None);
            }
            return;
        }
    };
    let want = Trigger::parse(event);
    for (name, src) in files {
        let path = format!("{DIR}/{name}");
        match workflows::run_for_commit(
            &state.db,
            &repo.id,
            event,
            sha,
            change_key,
            &path,
            composition,
        ) {
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(e) => {
                eprintln!("weft: workflow trigger: lookup {path}: {e}");
                continue;
            }
        }
        let parsed = parse::parse(stem(&name), &src).and_then(|wf| match plan::plan(&wf) {
            Ok(p) => Ok((wf, p)),
            Err(mut rs) => Err(rs.remove(0)),
        });
        let (wf, planned) = match parsed {
            Ok(x) => x,
            Err(r) => {
                // The file is wrong. Say so on the commit, in the words
                // the `workflows` route would use, whatever the event —
                // the person who pushed it needs to know now.
                let new = NewRun {
                    file: &path,
                    name: stem(&name),
                    commit_sha: sha,
                    ref_name,
                    event,
                    change_key,
                    changeset_id,
                    composition,
                    from_fork: fork_hold.is_some(),
                };
                settle(state, repo, &new, "failed", &r.render(&path), None);
                continue;
            }
        };
        // A `timeout-minutes:` over the fleet's ceiling is a property of
        // the file, so it is refused here beside the parse refusals and
        // for the same reason: whatever the event, the person who wrote
        // it needs to be told, and the file will not run on this
        // deployment until they change it.
        if let Some(asked) = wf
            .jobs
            .iter()
            .filter_map(|j| j.timeout_minutes)
            .map(i64::from)
            .find(|&m| m > state.max_timeout_minutes)
        {
            let new = NewRun {
                file: &path,
                name: stem(&name),
                commit_sha: sha,
                ref_name,
                event,
                change_key,
                changeset_id,
                composition,
                from_fork: fork_hold.is_some(),
            };
            settle(
                state,
                repo,
                &new,
                "failed",
                &format!(
                    "timeout-minutes: {asked} exceeds this fleet's limit of {}",
                    state.max_timeout_minutes
                ),
                None,
            );
            continue;
        }
        if !want.is_some_and(|t| wf.on.contains(&t)) {
            continue;
        }
        let new = NewRun {
            file: &path,
            name: &wf.name,
            commit_sha: sha,
            ref_name,
            event,
            change_key,
            changeset_id,
            composition,
            from_fork: fork_hold.is_some(),
        };
        // First, because these are the refusals nothing lifts: a file
        // that asks for a machine this server cannot give it, a
        // repository no group admits, labels no machine has.
        if let Some(reason) = pool_refusal(state, repo, &wf) {
            settle(state, repo, &new, "failed", &reason, None);
            continue;
        }
        // The fork gate applies to **both** pools, and it matters more
        // for the self-hosted one: the thing being protected there is
        // somebody's own machine, and the workflow file was written by
        // a stranger.
        if let Some(why) = fork_hold {
            settle(state, repo, &new, "blocked", why, Some(BlockedReason::Fork));
            continue;
        }
        // Owned strings first, then the borrowed slice `create_run`
        // wants: `NewJob` borrows and the planner's cells are ours.
        // Every cell of a matrix inherits its job's pool and labels:
        // `runs-on` is written once for the job, and a matrix expands
        // what it runs, not where.
        let cells: Vec<(String, String, String, &'static str, Vec<String>)> = planned
            .order
            .iter()
            .map(|&i| {
                let pj = &planned.jobs[i];
                let job = wf
                    .job(&pj.job_id)
                    .expect("planned job exists in its workflow");
                (
                    serde_json::to_string(&pj.matrix).expect("matrix json"),
                    job_spec(job, &pj.matrix).to_string(),
                    pj.key.clone(),
                    job.pool.as_str(),
                    job.labels.clone(),
                )
            })
            .collect();
        // `needs` indexes the planner's `jobs`; the rows go in `order`,
        // so remap each edge to its position in the created slice.
        let position: BTreeMap<usize, usize> = planned
            .order
            .iter()
            .enumerate()
            .map(|(pos, &i)| (i, pos))
            .collect();
        let needs: Vec<Vec<usize>> = planned
            .order
            .iter()
            .map(|&i| planned.jobs[i].needs.iter().map(|n| position[n]).collect())
            .collect();
        let jobs: Vec<NewJob> = planned
            .order
            .iter()
            .enumerate()
            .map(|(pos, &i)| NewJob {
                job_id: &planned.jobs[i].job_id,
                key: &cells[pos].2,
                matrix: &cells[pos].0,
                needs: &needs[pos],
                spec: &cells[pos].1,
                pool: cells[pos].3,
                labels: &cells[pos].4,
            })
            .collect();
        match workflows::create_run(&state.db, &repo.org_id, &repo.id, &new, &jobs) {
            Ok(run) => {
                let page = mirror::run_page_by_id(&state.db, &state.public_url, &repo.id, &run.id);
                if let Ok(rows) = workflows::jobs_of(&state.db, &run.id) {
                    for job in &rows {
                        if let Err(e) =
                            mirror::mirror_job(&state.db, &repo.id, &run, job, page.as_deref())
                        {
                            eprintln!("weft: mirror job {}: {e}", job.id);
                        }
                    }
                }
            }
            Err(e) => eprintln!("weft: workflow trigger: create run for {path}: {e}"),
        }
    }
}

fn settle(
    state: &SharedState,
    repo: &Repo,
    new: &NewRun,
    run_state: &str,
    error: &str,
    blocked: Option<BlockedReason>,
) {
    match workflows::create_settled_run(
        &state.db,
        &repo.org_id,
        &repo.id,
        new,
        run_state,
        Some(error),
        blocked,
    ) {
        Ok(run) => {
            let page = mirror::run_page_by_id(&state.db, &state.public_url, &repo.id, &run.id);
            if let Err(e) = mirror::mirror_refusal(&state.db, &repo.id, &run, page.as_deref()) {
                eprintln!("weft: mirror refusal {}: {e}", run.id);
            }
        }
        Err(e) => eprintln!("weft: workflow trigger: settle {}: {e}", new.file),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spec_defaults_the_image_and_timeout_and_names_unnamed_steps() {
        let wf = parse::parse(
            "ci",
            "on: push\njobs:\n  test:\n    steps:\n      - run: cargo test\n      - name: Lint\n        run: cargo clippy\n        env:\n          RUSTFLAGS: -D warnings\n",
        )
        .unwrap();
        let spec = job_spec(&wf.jobs[0], &BTreeMap::new());
        assert_eq!(spec["image"], "default");
        assert_eq!(spec["timeout_minutes"], 360);
        assert_eq!(spec["steps"][0]["name"], "cargo test");
        assert_eq!(spec["steps"][1]["name"], "Lint");
        assert_eq!(spec["steps"][1]["env"]["RUSTFLAGS"], "-D warnings");
        assert_eq!(spec["matrix"], serde_json::json!({}));
    }

    /// A self-hosted job that names an image is refused by its own text,
    /// naming the image back — and `default`, which is what every job
    /// that says nothing gets, is not an image anybody is naming.
    #[test]
    fn a_self_hosted_job_may_not_name_an_image() {
        let wf = parse::parse(
            "ci",
            "on: push\njobs:\n  a:\n    runs-on: [self-hosted, gpu]\n    image: rust:1.83\n    steps:\n      - run: cargo test\n",
        )
        .unwrap();
        let jobs: Vec<&Job> = wf.jobs.iter().collect();
        assert_eq!(
            image_refusal(&jobs).as_deref(),
            Some(
                "image \"rust:1.83\" is not available on self-hosted runners; \
                 steps run directly on the machine"
            )
        );

        for text in [
            "on: push\njobs:\n  a:\n    runs-on: [self-hosted]\n    steps:\n      - run: x\n",
            "on: push\njobs:\n  a:\n    runs-on: [self-hosted]\n    image: default\n    steps:\n      - run: x\n",
        ] {
            let wf = parse::parse("ci", text).unwrap();
            let jobs: Vec<&Job> = wf.jobs.iter().collect();
            assert_eq!(image_refusal(&jobs), None, "{text}");
        }

        // A file whose *second* self-hosted job names one is refused
        // too: the refusal is the file's, not the first job's.
        let wf = parse::parse(
            "ci",
            "on: push\njobs:\n  a:\n    runs-on: [self-hosted]\n    steps:\n      - run: x\n  b:\n    runs-on: [self-hosted]\n    container: node:18\n    steps:\n      - run: y\n",
        )
        .unwrap();
        let jobs: Vec<&Job> = wf.jobs.iter().collect();
        assert!(
            image_refusal(&jobs)
                .unwrap()
                .starts_with("image \"node:18\""),
            "the second job's image is the one named"
        );
    }

    /// The composition names a *set* of tips: the same members in a
    /// different order are the same combination, one different tip is a
    /// different one, and a member joining or leaving changes it.
    ///
    /// The order rule is the load-bearing one. Members come back in
    /// `position` order, and adding an edge reorders the *landing*
    /// without changing what a build would see — so an
    /// order-sensitive hash would supersede every live composed run
    /// every time somebody drew a dependency arrow.
    /// The member being held reads its own change's sentence — the
    /// approve button is on that change — and everybody else reads who
    /// they are waiting for, by repository and change key. A composed
    /// hold that told a maintainer their own change came from a fork
    /// would send them looking for a button that is not on their screen.
    #[test]
    fn a_composed_hold_names_the_member_it_is_waiting_for() {
        assert_eq!(fork_hold(true, "widget", "I9c0ffee"), FORK_HELD);
        assert_eq!(
            fork_hold(false, "widget", "I9c0ffee"),
            "member widget/I9c0ffee comes from a fork; a maintainer has to \
             approve its workflows before the changeset's runs"
        );
    }

    #[test]
    fn the_composition_is_the_set_of_tips_and_not_their_order() {
        let a = "repo_a:1111111111111111111111111111111111111111".to_string();
        let b = "repo_b:2222222222222222222222222222222222222222".to_string();
        let b2 = "repo_b:3333333333333333333333333333333333333333".to_string();

        let forward = composition_hash([a.clone(), b.clone()]);
        assert_eq!(
            forward,
            composition_hash([b.clone(), a.clone()]),
            "reordering the members is not a recomposition"
        );
        assert_eq!(
            forward,
            composition_hash([a.clone(), b.clone()]),
            "and it is stable across calls"
        );
        assert_ne!(
            forward,
            composition_hash([a.clone(), b2]),
            "a member at a new tip is a different combination"
        );
        assert_ne!(
            forward,
            composition_hash([a.clone()]),
            "a member leaving is a different combination"
        );
        // Hex, and the whole digest: a truncated hash would collide
        // across changesets that share a database column.
        assert_eq!(forward.len(), 64, "{forward}");
        assert!(forward.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
