//! Telling people a *changeset* needs them.
//!
//! The single-change notifier ([`super::notifier`]) has been the answer
//! to "a review nobody is told about sits in the queue" since the day it
//! landed. A changeset had no such worker at all: compose one review
//! across four repositories and not one person was told it existed. That
//! is the same defect, in the feature that is harder to notice — the
//! per-repository path still worked, so every mailbox looked alive while
//! the cross-repository one was silent.
//!
//! Two things make this worker more than the other one with a different
//! subject line, and both are the reason it is not a fifth `Event` on it:
//!
//!   * **The list is a union over members.** A changeset's reviewers are
//!     every member's participants and every member's OWNERS-required
//!     reviewers, together, because the thing being reviewed is the
//!     combination. Neither `changes_api::required_reviewers` nor
//!     `watches::recipients` is reimplemented here — one wrong second
//!     resolution of an OWNERS file is silent in the worst direction,
//!     and the person the change needs is the one who hears nothing.
//!   * **Reading a changeset needs read on *every* member.** That is
//!     what `changesets_api::load` enforces on the wire, and a mail is
//!     the same read by another route: it names the member repositories,
//!     so sending one to somebody who may not see one of them publishes
//!     the existence of a private repository. An outside contributor who
//!     commented on a change in a public repository is exactly that
//!     person, and they are a participant by every other rule here.

use crate::app::SharedState;
use std::collections::BTreeSet;
use stratum_control::changesets::Changeset;
use stratum_control::{changes, changesets, jobs, members, profiles, registry, users, watches};

/// The job kind. Deduplicated by payload while queued — see
/// `jobs::DEDUPLICATED_KINDS` and migration 0049.
const JOB_KIND: &str = "notify-changeset";

/// What happened, in the words the subject line uses.
#[derive(Debug, Clone, Copy)]
pub enum Event {
    /// The changeset now exists: the review has been asked for.
    Composed,
    Landed,
    /// A landing that was attempted and put back. Its own event rather
    /// than a flavour of `Landed`, because what the reader does next is
    /// different and they decide it from the subject line.
    Failed,
}

impl Event {
    fn as_str(self) -> &'static str {
        match self {
            Event::Composed => "composed",
            Event::Landed => "landed",
            Event::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Option<Event> {
        Some(match s {
            "composed" => Event::Composed,
            "landed" => Event::Landed,
            "failed" => Event::Failed,
            _ => return None,
        })
    }
}

/// Queue a notification. Called from the request path and from the
/// lander, where it is one INSERT and nothing else.
///
/// Failures are logged and swallowed, exactly as the single-change
/// enqueue does and for the same reason: a changeset that landed has
/// landed, and refusing the response because the *notification* could
/// not be queued would turn a mail problem into a review problem.
pub fn enqueue(state: &SharedState, cs: &Changeset, event: Event, actor: &str) {
    let payload = serde_json::json!({
        "changeset_id": cs.id,
        "event": event.as_str(),
        "actor": actor,
    })
    .to_string();
    if let Err(e) = jobs::enqueue_unique_org(&state.db, &cs.org_id, JOB_KIND, Some(&payload)) {
        eprintln!("weft: changeset notification enqueue: {e}");
    }
}

pub fn spawn(state: SharedState) {
    let poll = super::env_period("STRATUM_CHANGESET_NOTIFY_POLL_SECS", 5);
    if poll.is_zero() {
        return;
    }
    // Short, for the reason the single-change notifier's is short: a
    // node dying mid-send must not leave people unnotified for the
    // length of the lease, and timeliness is the whole feature.
    let lease_ms = super::lease_ms("STRATUM_CHANGESET_NOTIFY_LEASE_SECS", 60);
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
                    Ok(sent) => {
                        let _ = jobs::complete(&state.db, &job.id, Some(&format!("sent {sent}")));
                    }
                    Err(e) => {
                        eprintln!("weft: changeset notification failed: {e}");
                        let _ = jobs::fail(&state.db, &job.id, &e);
                    }
                },
                Ok(None) => tokio::time::sleep(poll).await,
                Err(e) => {
                    eprintln!("weft: changeset notify claim: {e}");
                    tokio::time::sleep(poll).await;
                }
            }
        }
    });
}

/// One member of the changeset, as this worker needs it: the repository
/// it is in, and everybody that member alone would have told.
struct MemberAudience {
    repo: registry::Repo,
    /// Participants and OWNERS-required reviewers, before the changeset's
    /// own rules are applied.
    involved: BTreeSet<String>,
}

/// Resolve one job's recipients and mail them, returning how many were
/// sent — "sent 0" is a real answer for a changeset nobody but its
/// composer is involved in yet.
pub async fn run_one(state: &SharedState, job: &jobs::Job) -> Result<usize, String> {
    let payload: serde_json::Value = serde_json::from_str(job.payload.as_deref().unwrap_or("{}"))
        .map_err(|e| format!("changeset notification payload: {e}"))?;
    let changeset_id = payload["changeset_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let actor = payload["actor"].as_str().unwrap_or_default().to_string();
    let event = Event::parse(payload["event"].as_str().unwrap_or_default())
        .ok_or_else(|| format!("unknown changeset notification event in job {}", job.id))?;

    let Some(cs) = changesets::by_id(&state.db, &changeset_id).map_err(|e| e.to_string())? else {
        // Deleted between the enqueue and the claim. Nothing to say
        // about it, and nothing wrong.
        return Ok(0);
    };

    let mut audiences = Vec::new();
    for m in changesets::members(&state.db, &cs.id).map_err(|e| e.to_string())? {
        // A member whose change or repository is gone is skipped, the
        // same way `changesets_api::load_members` skips it: the set is
        // still worth telling people about, and it is not this worker's
        // place to report a dangling row.
        let Some(change) = changes::by_id(&state.db, &m.change_id)? else {
            continue;
        };
        let Some(repo) = registry::repo_by_id_any(&state.db, &change.repo_id)? else {
            continue;
        };
        let mut involved: BTreeSet<String> = BTreeSet::new();
        involved.extend(change.created_by.clone());
        // Everybody who had done something on this member **when the
        // event happened**, not when the job got round to running — the
        // cutoff the single-change notifier documents: a comment posted
        // in the second between the two would otherwise make its author
        // a participant retroactively and be told about an event that
        // predates their involvement.
        for c in changes::comments_for(&state.db, &change.id)?
            .into_iter()
            .filter(|c| c.created_at <= job.created_at)
        {
            // Only people. A service principal — CI comments too — has
            // no mailbox.
            if let Some(id) = c.author_principal.strip_prefix("user:") {
                involved.insert(id.to_string());
            }
        }
        let patchsets = changes::patchsets(&state.db, &change.id)?;
        if let Some(latest) = patchsets.last() {
            for a in changes::approvals_for(&state.db, &latest.id)?
                .into_iter()
                .filter(|a| a.created_at <= job.created_at)
            {
                involved.insert(a.user_id);
            }
            // ...and everybody this member *needs*. A failure is logged
            // and read as "nobody": an unreadable OWNERS file in one
            // repository must not cost the other members' people their
            // notification. `required_reviewers` is also where the
            // `anyone_with_write` rule lives — a path that names no
            // owner in particular requires nobody, so nobody is mailed
            // on that basis. Mailing everyone with commit access about
            // every changeset is the notification people filter.
            match crate::api::changes_api::required_reviewers(state, &change, &repo, latest).await {
                Ok(ids) => involved.extend(ids),
                Err(e) => eprintln!("weft: owners for changeset notification: {e}"),
            }
        }
        // "Never" wins here as it does everywhere: somebody who asked to
        // hear nothing about this repository is not put on the list by
        // being named in its OWNERS file. `watching_all` is deliberately
        // empty — a person watching one member repository asked to hear
        // about *that repository*, and each member's own landing tells
        // them through the single-change notifier; the changeset mail is
        // for the people the review is addressed to, and adding every
        // watcher of every member is how one review over four
        // repositories becomes four organizations' worth of mail.
        let involved = watches::recipients(
            &involved,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &watches::ignoring(&state.db, &repo.id)?,
            &actor,
        );
        audiences.push(MemberAudience { repo, involved });
    }
    if audiences.is_empty() {
        return Ok(0);
    }

    let candidates: BTreeSet<String> = audiences
        .iter()
        .flat_map(|a| a.involved.iter().cloned())
        .collect();
    // A changeset read requires read on **every** member, so a mail
    // about it does too. This is not a refinement of the list, it is the
    // same masking `changesets_api::load` applies: the mail names the
    // member repositories, so sending it to somebody who may not see one
    // of them tells them a private repository exists. The case is
    // ordinary rather than exotic — an outsider who commented on a
    // change in a public member is a participant by every rule above.
    let mut to: Vec<String> = Vec::new();
    for user_id in candidates {
        let mut may_see_all = true;
        for a in &audiences {
            if !can_read(state, &cs.org_id, &a.repo, &user_id)? {
                may_see_all = false;
                break;
            }
        }
        if may_see_all {
            to.push(user_id);
        }
    }

    let actor_name = users::by_id(&state.db, &actor)?
        .map(|u| u.name)
        .unwrap_or_else(|| "somebody".into());
    let org = registry::org_by_id(&state.db, &cs.org_id)?
        .map(|o| o.name)
        .unwrap_or_default();
    // Named once, in member order, so two mails about one changeset
    // list its repositories the same way.
    let repos: Vec<String> = audiences.iter().map(|a| a.repo.name.clone()).collect();

    let mut sent = 0;
    for user_id in to {
        // Only a *proved* address, for the reason `notifier` gives: an
        // unverified one is a claim, and mailing a claim is how a forge
        // becomes the thing that sends strangers' review traffic to an
        // address nobody confirmed.
        let Some(addr) = profiles::list_emails(&state.db, &user_id)?
            .into_iter()
            .find(|e| e.verified_at.is_some())
            .map(|e| e.address)
        else {
            continue;
        };
        let msg = crate::mail::templates::changeset_activity(
            &crate::mail::templates::ChangesetActivity {
                to: &addr,
                public_url: &state.public_url,
                org: &org,
                key: &cs.key,
                title: &cs.title,
                actor: &actor_name,
                event: event.as_str(),
                repos: &repos,
            },
        );
        if let Err(e) = state.mailer.send(&msg) {
            // One bad address must not cost the rest of the list their
            // notification.
            eprintln!("weft: changeset notification to {addr}: {e}");
            continue;
        }
        sent += 1;
    }
    Ok(sent)
}

/// May this person read this repository at all?
///
/// A public repository is readable by anybody with an account, and a
/// private one by whoever holds a role on it — which is
/// `members::effective_role`, the same answer `rest_repo_auth` reaches
/// through the principal, rather than a second opinion about access.
/// Asked per (user, repo) and memoized nowhere, because a changeset has
/// at most `MAX_MEMBERS` members and a handful of recipients.
fn can_read(
    state: &SharedState,
    org_id: &str,
    repo: &registry::Repo,
    user_id: &str,
) -> Result<bool, String> {
    if repo.public {
        return Ok(true);
    }
    Ok(members::effective_role(&state.db, org_id, Some(&repo.id), user_id)?.is_some())
}
