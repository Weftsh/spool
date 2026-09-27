//! The freshness contract (M2) — the Mirror product's hard part.
//!
//! Invariant: **never a silent stale miss.**
//! - A fetch asking for a commit the mirror doesn't have triggers a
//!   synchronous, bounded sync from origin; success serves it fresh.
//! - Origin unreachable → last-known state is served, and the response
//!   carries `X-Weft-Staleness` (seconds) + `X-Weft-Origin-Error`.
//! - The want still absent after a successful sync, or the sync timing
//!   out → 404 with an explanation naming the origin. Never a 200 that
//!   quietly lacks the commit.

use super::sync::SyncOutcome;
use crate::app::SharedState;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use stratum_control::registry::Repo;
use stratum_store::{LatencyModel, Manifest, ObjectStore, Plane};

/// Headers to attach to a mirror response served from possibly-stale state.
#[derive(Debug, Default, Clone)]
pub struct Staleness {
    pub headers: Vec<(&'static str, String)>,
}

/// Staleness headers from the repo row's recorded sync state: present
/// whenever the last sync attempt failed.
pub fn staleness_headers(repo: &Repo) -> Staleness {
    let mut s = Staleness::default();
    if let Some(err) = &repo.sync_error {
        let age_secs = repo
            .last_sync_at
            .map(|at| (stratum_control::ids::now_ms() - at).max(0) / 1000)
            .unwrap_or(-1);
        s.headers.push(("x-weft-staleness", age_secs.to_string()));
        s.headers.push((
            "x-weft-origin-error",
            err.chars().take(200).collect::<String>().replace('\n', " "),
        ));
    }
    s
}

/// Why a fetch cannot be served — transport-neutral so every front door
/// renders the same contract: HTTP as status codes + headers, SSH as an
/// in-band pkt-line ERR.
#[derive(Debug)]
pub enum Denied {
    /// Infrastructure failure while classifying wants (store unreachable,
    /// malformed manifest). Not the contract's explicit answer.
    Internal(String),
    /// The contract's explicit answer: the want is not servable and here
    /// is why, naming the origin. `stale` carries the honest-staleness
    /// signal when the origin is down.
    NotFound { msg: String, stale: Staleness },
}

/// Ensure `wants` are servable, syncing from origin when they are not.
/// HTTP wrapper over [`ensure_wants_core`]: Ok(Staleness) = proceed with
/// serving (headers attached as computed); Err(response) = the contract's
/// explicit failure answer.
pub async fn ensure_wants(
    state: &SharedState,
    repo: &Repo,
    wants: &[String],
) -> Result<Staleness, Response> {
    ensure_wants_core(state, repo, wants)
        .await
        .map_err(|d| match d {
            Denied::Internal(e) => crate::git_http::err_response(e),
            Denied::NotFound { msg, stale } => explain_404_with(msg, stale),
        })
}

/// The freshness contract itself, independent of transport.
pub async fn ensure_wants_core(
    state: &SharedState,
    repo: &Repo,
    wants: &[String],
) -> Result<Staleness, Denied> {
    let known = wants_known(state, repo, wants)
        .await
        .map_err(Denied::Internal)?;
    if known {
        // Fresh enough for this request; still surface a failed-origin
        // condition honestly.
        let repo = refreshed(state, repo);
        return Ok(staleness_headers(&repo));
    }

    let origin = repo.origin_url.clone().unwrap_or_else(|| "origin".into());
    // Coalescing is a promise to wait, not to give up. When another node
    // holds this mirror's sync, `sync` answers `Elsewhere` at once, and
    // this used to fall straight through to `wants_known` — which the
    // other node's fetch had not finished filling in — and refuse the
    // want as "did not surface it". Two jobs of one workflow fetching a
    // just-pushed commit at the same moment: one got its pack, the other
    // got a 404 naming a sync that was, at that instant, still running.
    // Retry until the lock is ours (a no-op sync once the winner is
    // done) or the budget is spent; the timeout below bounds the wait.
    let sync_result = tokio::time::timeout(state.freshness_timeout, async {
        loop {
            match state.sync.sync(repo).await {
                Ok(SyncOutcome::Elsewhere) => {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                other => return other,
            }
        }
    })
    .await;
    match sync_result {
        Ok(Ok(_)) => {
            if wants_known(state, repo, wants)
                .await
                .map_err(Denied::Internal)?
            {
                Ok(Staleness::default())
            } else {
                Err(Denied::NotFound {
                    msg: format!(
                        "commit not found on this mirror, and a synchronous sync of the \
                         origin ({origin}) did not surface it — it may not exist upstream"
                    ),
                    stale: Staleness::default(),
                })
            }
        }
        Ok(Err(sync_err)) => {
            let repo = refreshed(state, repo);
            let stale = staleness_headers(&repo);
            Err(Denied::NotFound {
                msg: format!(
                    "commit not found on this mirror and the origin ({origin}) is \
                     unreachable: {sync_err}. The mirror is serving last-known state; \
                     already-mirrored commits remain available."
                ),
                stale,
            })
        }
        Err(_elapsed) => Err(Denied::NotFound {
            msg: format!(
                "commit not yet on this mirror; syncing from the origin ({origin}) \
                 exceeded the {}s freshness budget — retry shortly",
                state.freshness_timeout.as_secs()
            ),
            stale: Staleness::default(),
        }),
    }
}

fn refreshed(state: &SharedState, repo: &Repo) -> Repo {
    stratum_control::registry::repo_by_id(&state.db, &repo.org_id, &repo.id)
        .ok()
        .flatten()
        .unwrap_or_else(|| repo.clone())
}

fn explain_404(msg: String) -> Response {
    (StatusCode::NOT_FOUND, format!("stratum-mirror: {msg}\n")).into_response()
}

fn explain_404_with(msg: String, stale: Staleness) -> Response {
    let mut resp = explain_404(msg);
    attach(&mut resp, &stale);
    resp
}

pub fn attach(resp: &mut Response, stale: &Staleness) {
    for (name, value) in &stale.headers {
        if let Ok(v) = value.parse() {
            resp.headers_mut()
                .insert(axum::http::HeaderName::from_static(name), v);
        }
    }
}

/// Mirror of the serving path's want-classification: manifest tips, WAL
/// tips, spine commits, and (for paged repos) locator membership.
async fn wants_known(state: &SharedState, repo: &Repo, wants: &[String]) -> Result<bool, String> {
    let store_url = state.store_url.clone();
    let prefix = repo.prefix().as_str().to_string();
    let wants = wants.to_vec();
    tokio::task::spawn_blocking(move || -> Result<bool, String> {
        let store = ObjectStore::new(&store_url, LatencyModel::None);
        let manifest: Manifest = match store.get(&format!("{prefix}/manifest.json")) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("manifest: {e}"))?,
            // No manifest yet (mirror registered, first sync pending):
            // nothing is known.
            Err(e) if stratum_engine::errclass::is_absent(&e) => return Ok(false),
            Err(e) => return Err(e),
        };
        let tips = manifest.tips();
        let mut plane: Option<Option<Plane>> = None; // lazy-loaded
        for w in &wants {
            if tips.contains(&w.as_str())
                || manifest.find_wal_tip(w).is_some()
                || manifest.find_spine(w).is_some()
            {
                continue;
            }
            if !manifest.ref_pages.is_empty() {
                let p = match &plane {
                    Some(p) => p,
                    None => {
                        let loaded = match Plane::load(&store, &prefix) {
                            Ok(p) => Some(p),
                            Err(e) if stratum_engine::errclass::is_absent(&e) => None,
                            Err(e) => return Err(e),
                        };
                        plane = Some(loaded);
                        plane.as_ref().unwrap()
                    }
                };
                if let Some(p) = p {
                    if p.lookup(&store, &Plane::parse_oid(w)?)?.is_some() {
                        continue;
                    }
                }
            }
            return Ok(false);
        }
        Ok(true)
    })
    .await
    .unwrap_or_else(|e| Err(format!("task join: {e}")))
}
