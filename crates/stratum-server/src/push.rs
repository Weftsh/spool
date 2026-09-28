//! The push door both wires share: HTTP's `git-receive-pack` RPC and
//! the SSH session hand the request body here and get the report back.
//!
//! A native repository's push is landed by the wire engine — quarantine,
//! connectivity, WAL, manifest CAS. A mirror's is **forwarded**: parsed
//! here, pushed to the origin by [`crate::mirror::forward`], reflected
//! into the layout by the sync machinery, and reported in the same pkt
//! format, so `git push` cannot tell which kind of repository it spoke
//! to except by what the refusal says.

use crate::app::SharedState;
use crate::git_http::{Accepted, RepoCtx};
use axum::body::Bytes;
use stratum_control::audit::AuditCtx;
use stratum_control::registry::{Repo, RepoKind};
use stratum_proto::receive;

/// Land or forward one push. Answers the report bytes and what was
/// accepted; an `Err` is a transport-level failure with no report.
pub async fn receive(
    state: &SharedState,
    repo: &Repo,
    ctx: &RepoCtx,
    body: Bytes,
    protected: Vec<String>,
) -> Result<(Vec<u8>, Accepted), String> {
    let _p = state.permits.receive.acquire().await;
    if repo.kind != RepoKind::Mirror {
        let ctx = ctx.clone();
        return tokio::task::spawn_blocking(move || {
            let store = ctx.store();
            let mut buf = Vec::new();
            // `None`: nothing caps what a repository may hold here.
            let accepted = receive::receive(&store, &ctx.prefix, &body, &protected, &mut buf)?;
            Ok((buf, accepted))
        })
        .await
        .unwrap_or_else(|e| Err(format!("task join: {e}")));
    }
    let Some(req) = receive::parse_request(&body)? else {
        let mut buf = Vec::new();
        stratum_proto::pktline::write_flush(&mut buf).map_err(|e| e.to_string())?;
        return Ok((buf, None));
    };
    let pack = (!req.pack.is_empty()).then(|| req.pack.clone());
    let n = req.updates.len();
    let verdicts = match state
        .sync
        .forward(repo, req.updates.clone(), pack, state.freshness_timeout)
        .await
    {
        Ok(_) => vec![Ok(()); n],
        Err(refusal) => refusal.verdicts(n),
    };
    let mut buf = Vec::new();
    let accepted = receive::report_each(&mut buf, &req, &verdicts)?;
    Ok((buf, accepted))
}

/// What both doors do once a push is in: the audit row, the meter, the
/// outbound webhook, and — for a native repository — the fold, the
/// CDN pack, authorship and the workflow trigger. A forwarded push
/// already queued the first three through the sync it ended with, and
/// runs no workflow here: a mirror's CI is its origin's, which the
/// checks poll the same sync armed will read.
pub async fn after_accept(
    state: &SharedState,
    repo: &Repo,
    actx: &AuditCtx,
    updates: &[receive::Update],
    via: &str,
    pushed_bytes: u64,
    started: std::time::Instant,
) {
    let forwarded = repo.kind == RepoKind::Mirror;
    if !forwarded {
        crate::storage::refresh_after_write(state, repo).await;
    }
    let blob = forwarded.then(|| serde_json::json!({ "forwarded_to": repo.origin_url }));
    crate::api::record_or_warn(&state.db, actx, Some(&repo.id), "repo.push", blob.as_ref());
    state.meter.record(
        &repo.id,
        "push",
        pushed_bytes,
        Some(started.elapsed().as_millis() as u64),
    );
    crate::workers::notify::notify(
        state,
        &repo.id,
        "push",
        serde_json::json!({ "via": via, "forwarded": forwarded }),
    );
    if forwarded {
        return;
    }
    crate::workers::compactor::enqueue(state, &repo.org_id, &repo.id);
    crate::workers::cdnpack::enqueue(state, &repo.org_id, &repo.id);
    // Authorship, in the background. The person who just pushed ten
    // thousand commits must not also pay to have them counted, and
    // who they are is knowable only here — `pushed-by` is one of the
    // two grounds on which a commit counts at all.
    crate::workers::contribs::enqueue(state, &repo.org_id, &repo.id, actx.user_id.as_deref());
    crate::workflow::trigger::on_push(state, repo, updates).await;
}
