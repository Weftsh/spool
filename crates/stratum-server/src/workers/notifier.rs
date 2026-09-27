//! Telling people a change needs them.
//!
//! A review flow where nobody is told anything happened does not
//! function: a change sits in the land queue until somebody happens to
//! look at it. This worker is what closes that, and it is deliberately
//! the first social feature — ahead of anything with a visible surface,
//! because the visible surfaces are worth nothing if the person who has
//! to act never learns there is something to act on.
//!
//! It runs as a job rather than inline. A comment reaching five
//! reviewers must not make the person who wrote it wait on five SMTP
//! round trips, and a mail server being slow must not turn into a
//! request timing out on a write that already committed.

use crate::app::SharedState;
use std::collections::BTreeSet;
use stratum_control::{changes, jobs, profiles, users, watches};

/// What happened, in the words the subject line uses.
#[derive(Debug, Clone, Copy)]
pub enum Event {
    Opened,
    Commented,
    Approved,
    Landed,
    /// A whole review pass, submitted as one act — the reviewer's
    /// notes, however many comments they came in.
    ///
    /// This variant is why a twelve-comment review is one email. The
    /// per-comment `Commented` event is still right for a comment
    /// posted on its own, but a review's comments are drafted, notify
    /// nobody while they are, and are published together; one act, one
    /// sentence, one interruption.
    Reviewed,
    /// A review that said no. It reads differently from every other
    /// event here on purpose: it is the one that means the author has
    /// to do something before the change can move.
    ChangesRequested,
}

impl Event {
    fn as_str(self) -> &'static str {
        match self {
            Event::Opened => "opened",
            Event::Commented => "commented",
            Event::Approved => "approved",
            Event::Landed => "landed",
            Event::Reviewed => "reviewed",
            Event::ChangesRequested => "changes_requested",
        }
    }

    fn parse(s: &str) -> Option<Event> {
        Some(match s {
            "opened" => Event::Opened,
            "commented" => Event::Commented,
            "approved" => Event::Approved,
            "landed" => Event::Landed,
            "reviewed" => Event::Reviewed,
            "changes_requested" => Event::ChangesRequested,
            _ => return None,
        })
    }
}

/// Queue a notification. Called from the request path, where it is one
/// INSERT and nothing else.
///
/// Failures are logged and swallowed on purpose: a change that landed
/// has landed, and refusing the response because the *notification*
/// could not be queued would turn a mail problem into a review problem.
/// The cost of losing one is that somebody is not told, which is the
/// state we were in before this existed.
pub fn enqueue(state: &SharedState, change: &changes::Change, event: Event, actor: &str) {
    let payload = serde_json::json!({
        "change_id": change.id,
        "event": event.as_str(),
        "actor": actor,
    })
    .to_string();
    if let Err(e) = jobs::enqueue_unique(
        &state.db,
        &change.org_id,
        &change.repo_id,
        "notify-mail",
        Some(&payload),
    ) {
        eprintln!("weft: notification enqueue: {e}");
    }
}

pub fn spawn(state: SharedState) {
    let poll = super::env_period("STRATUM_NOTIFY_POLL_SECS", 5);
    if poll.is_zero() {
        return;
    }
    // Short, because the work is a handful of queries and some mail. A
    // long lease here would mean a node dying mid-send leaves people
    // unnotified for that long, and the whole point is timeliness.
    let lease_ms = super::lease_ms("STRATUM_NOTIFY_LEASE_SECS", 60);
    tokio::spawn(async move {
        loop {
            let claimed = {
                let db = state.db.clone();
                tokio::task::spawn_blocking(move || jobs::claim(&db, "notify-mail", lease_ms))
                    .await
                    .unwrap_or_else(|e| Err(format!("join: {e}")))
            };
            match claimed {
                Ok(Some(job)) => match run_one(&state, &job).await {
                    Ok(sent) => {
                        let _ = jobs::complete(&state.db, &job.id, Some(&format!("sent {sent}")));
                    }
                    Err(e) => {
                        eprintln!("weft: notification failed: {e}");
                        let _ = jobs::fail(&state.db, &job.id, &e);
                    }
                },
                Ok(None) => tokio::time::sleep(poll).await,
                Err(e) => {
                    eprintln!("weft: notify claim: {e}");
                    tokio::time::sleep(poll).await;
                }
            }
        }
    });
}

/// Resolve one job's recipients and mail them. Returns how many were
/// sent, which is what the job result records — "sent 0" is a real and
/// common answer (a change nobody but its author is involved in yet).
pub async fn run_one(state: &SharedState, job: &jobs::Job) -> Result<usize, String> {
    let payload: serde_json::Value = serde_json::from_str(job.payload.as_deref().unwrap_or("{}"))
        .map_err(|e| format!("notification payload: {e}"))?;
    let change_id = payload["change_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let actor = payload["actor"].as_str().unwrap_or_default().to_string();
    let event = Event::parse(payload["event"].as_str().unwrap_or_default())
        .ok_or_else(|| format!("unknown notification event in job {}", job.id))?;

    let Some(change) = changes::by_id(&state.db, &change_id)? else {
        // The change was deleted between the enqueue and the claim.
        // Nothing to say about it, and nothing wrong.
        return Ok(0);
    };
    let Some(repo_row) =
        stratum_control::registry::repo_by_id(&state.db, &change.org_id, &change.repo_id)?
    else {
        return Ok(0);
    };

    // Everybody who had done something on this change **when this
    // happened** — not when the job got round to running.
    //
    // Those are different sets, and the difference is visible: the job
    // polls, so a comment posted in the second between an event and its
    // send would make its author a participant retroactively. That is
    // how somebody was told "Ada opened a change" for no better reason
    // than that they had since commented on it — a notification about
    // an event that predates their involvement, which reads as the
    // system being confused about who they are.
    //
    // `job.created_at` is the moment the event was recorded, so it is
    // the cutoff. Anybody who joined after it will be on the list for
    // whatever they do next, which is soon enough.
    let at = job.created_at;
    let mut participants: BTreeSet<String> = BTreeSet::new();
    participants.extend(change.created_by.clone());
    for c in changes::comments_for(&state.db, &change.id)?
        .into_iter()
        .filter(|c| c.created_at <= at)
    {
        // Comments carry the acting principal rather than a user id,
        // because CI comments too — "the perf suite regressed" is
        // review. Only people are notified: a service principal has no
        // mailbox, and telling a token that somebody replied to it
        // would be mail nobody reads.
        if let Some(id) = c.author_principal.strip_prefix("user:") {
            participants.insert(id.to_string());
        }
    }
    let patchsets = changes::patchsets(&state.db, &change.id)?;
    if let Some(latest) = patchsets.last() {
        for a in changes::approvals_for(&state.db, &latest.id)?
            .into_iter()
            .filter(|a| a.created_at <= at)
        {
            participants.insert(a.user_id);
        }
    }

    // ...and everybody the change *needs*, which is the half GitHub
    // cannot compute. A failure here is logged and treated as "nobody":
    // an unreadable OWNERS file must not stop the author's own
    // participants being told.
    let reviewers = match patchsets.last() {
        Some(latest) => {
            crate::api::changes_api::required_reviewers(state, &change, &repo_row, latest)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("weft: owners for notification: {e}");
                    BTreeSet::new()
                })
        }
        None => BTreeSet::new(),
    };

    let to = watches::recipients(
        &participants,
        &reviewers,
        &watches::watching_all(&state.db, &change.repo_id)?,
        &watches::ignoring(&state.db, &change.repo_id)?,
        &actor,
    );

    let actor_name = users::by_id(&state.db, &actor)?
        .map(|u| u.name)
        .unwrap_or_else(|| "somebody".into());
    let org = stratum_control::registry::org_by_id(&state.db, &change.org_id)?
        .map(|o| o.name)
        .unwrap_or_default();

    let mut sent = 0;
    for user_id in to {
        // Only a *proved* address. An unverified one is a claim, and
        // mailing a claim is how a forge becomes the thing that sends
        // strangers' review traffic to an address they never confirmed.
        // The address the account signs in with, and only if it is
        // proved. `list_emails` returns the primary first, so this is
        // the first verified row.
        let Some(addr) = profiles::list_emails(&state.db, &user_id)?
            .into_iter()
            .find(|e| e.verified_at.is_some())
            .map(|e| e.address)
        else {
            continue;
        };
        let msg =
            crate::mail::templates::change_activity(&crate::mail::templates::ChangeActivity {
                to: &addr,
                public_url: &state.public_url,
                org: &org,
                repo: &repo_row.name,
                change_key: &change.change_key,
                title: &change.title,
                actor: &actor_name,
                event: event.as_str(),
            });
        if let Err(e) = state.mailer.send(&msg) {
            // One bad address must not cost the rest of the list their
            // notification, so this is logged rather than returned.
            eprintln!("weft: notification to {addr}: {e}");
            continue;
        }
        sent += 1;
    }
    Ok(sent)
}
