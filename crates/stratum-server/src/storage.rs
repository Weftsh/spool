//! Stored bytes: what a repository holds.
//!
//! How the number is kept. Every durable write ends in a manifest CAS,
//! and the manifest carries exact byte counts for every object it
//! names, so the control-plane row is **SET** from the manifest after
//! each write (`refresh_repo`) and again by the hourly sweep
//! (`workers::storage`) — never incremented. A process killed between
//! the CAS and the row leaves a stale value the next pass replaces;
//! an increment would have left a drift nothing could find. The row is
//! best-effort at every call site for the same reason: the write has
//! landed, the sweep will catch up, and a push must not fail because a
//! bookkeeping UPDATE did.
//!
//! A zero-copy fork counts nothing until it is promoted, because until
//! then its manifest names upstream's objects under upstream's prefix
//! (`Manifest::stored_bytes`). Nothing here caps anything: the number is
//! what the repository page and the metrics show.

use crate::app::{AppState, SharedState};
use stratum_control::registry::Repo;
use stratum_control::storage::{self as ctl, OWNER_REPO};
use stratum_control::ControlDb;
use stratum_store::{LatencyModel, Manifest, ObjectStore};

/// Re-derive one repository's logical bytes from its manifest and SET
/// the row, with `private = !repo.public`. A repository whose manifest
/// is not there yet holds nothing.
pub fn refresh_repo(state: &AppState, repo: &Repo) -> Result<u64, String> {
    refresh_repo_with(&state.db, &state.store_url, repo)
}

/// [`refresh_repo`] for callers that hold the pieces rather than the
/// state — the mirror sync, which runs where there is no `AppState`.
pub fn refresh_repo_with(db: &ControlDb, store_url: &str, repo: &Repo) -> Result<u64, String> {
    let store = ObjectStore::new(store_url, LatencyModel::None);
    let prefix = repo.prefix().as_str().to_string();
    let bytes = match store.get(&format!("{prefix}/manifest.json")) {
        Ok(raw) => {
            let m: Manifest = serde_json::from_slice(&raw).map_err(|e| format!("manifest: {e}"))?;
            m.stored_bytes(&prefix)
        }
        Err(e) if stratum_engine::errclass::is_absent(&e) => 0,
        Err(e) => return Err(e),
    };
    ctl::set_logical(
        db,
        OWNER_REPO,
        &repo.id,
        &repo.org_id,
        bytes,
        stratum_control::ids::now_ms(),
    )?;
    Ok(bytes)
}

/// [`refresh_repo`] as the post-write hooks call it: the write has
/// already landed, so a failure here is logged and the sweep's problem.
pub fn refresh_or_warn(state: &AppState, repo: &Repo) {
    if let Err(e) = refresh_repo(state, repo) {
        eprintln!("weft: storage refresh {}: {e}", repo.id);
    }
}

/// [`refresh_or_warn`] off the async runtime: the manifest GET is a
/// blocking HTTP call and must not run on a tokio worker thread.
pub async fn refresh_after_write(state: &SharedState, repo: &Repo) {
    let state = state.clone();
    let repo = repo.clone();
    let _ = tokio::task::spawn_blocking(move || refresh_or_warn(&state, &repo)).await;
}

/// [`refresh_after_write`] for a worker that holds a job's ids rather
/// than the row. A repository deleted since holds nothing to record.
pub async fn refresh_by_id(state: &SharedState, org_id: &str, repo_id: &str) {
    match stratum_control::registry::repo_by_id(&state.db, org_id, repo_id) {
        Ok(Some(repo)) => refresh_after_write(state, &repo).await,
        Ok(None) => {}
        Err(e) => eprintln!("weft: storage refresh {repo_id}: {e}"),
    }
}

/// [`refresh_by_id`] across organizations — a changeset's members may
/// live in several.
pub async fn refresh_any_by_id(state: &SharedState, repo_id: &str) {
    match stratum_control::registry::repo_by_id_any(&state.db, repo_id) {
        Ok(Some(repo)) => refresh_after_write(state, &repo).await,
        Ok(None) => {}
        Err(e) => eprintln!("weft: storage refresh {repo_id}: {e}"),
    }
}
