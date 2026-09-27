//! The CDN packer: maintains one self-contained packfile per repo in the
//! object store so clones can pull the bulk from a CDN (git's
//! `packfile-uri`) instead of from a task.
//!
//! Claims `cdnpack` jobs enqueued after accepted writes, exactly like the
//! compactor. Keeping the pack at the current tip is the whole job: the
//! serve path only offloads when the pack covers the layout exactly, so a
//! pack that lags simply is not advertised and clones fall back to the
//! inline path (correct, just not offloaded). Two things must never
//! happen, and both shape the order of operations here: advertising a
//! pack that is *absent* — git aborts the clone rather than falling back,
//! hence the descriptor is written only after the pack object lands — and
//! leaving a descriptor pointing at a superseded pack, hence the old pack
//! is deleted only after the pointer swap.

use crate::app::SharedState;
use serde::{Deserialize, Serialize};
use stratum_control::jobs;
use stratum_store::{LatencyModel, ObjectStore, PutCond};

/// Pointer to the repo's current CDN pack. One GET on the serving path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CdnPackDescriptor {
    /// Tip the pack was built at — the implicit `have` for the inline delta.
    pub tip: String,
    /// Object-store key of the pack.
    pub pack_key: String,
    /// git's pack hash, the id the protocol advertises alongside the URI.
    pub pack_hash: String,
    pub size: u64,
    /// `manifest.total_entries()` at build time. With `tip` this proves
    /// the pack still covers the whole layout (entries only move when the
    /// layout does), which is what the serve path needs to decide whether
    /// anything is left to stream inline.
    pub total_entries: u64,
    pub created_at: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PackOutcome {
    /// A fresh pack was built and published.
    Built,
    /// The existing pack already covers the current tip, or there is
    /// nothing to pack (empty repo, deleted repo).
    NotNeeded,
}

/// Where a repo's CDN descriptor lives.
pub fn descriptor_key(prefix: &str) -> String {
    format!("{prefix}/cdn/current.json")
}

/// Where a pack for `tip`/`hash` lives. Content-addressed by both so a
/// republish never collides with an in-flight download of the old pack.
pub fn pack_key(prefix: &str, tip: &str, pack_hash: &str) -> String {
    format!("{prefix}/cdn/{tip}-{pack_hash}.pack")
}

/// Read the current descriptor, if any. A missing or unparseable pointer
/// is simply "no CDN pack" — never an error that breaks serving.
pub fn load_descriptor(store: &ObjectStore, prefix: &str) -> Option<CdnPackDescriptor> {
    let bytes = store.get(&descriptor_key(prefix)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Enqueue a repack after a write (deduplicated exactly like the
/// compactor: the partial unique index decides, inside the insert).
pub fn enqueue(state: &SharedState, org_id: &str, repo_id: &str) {
    if let Err(e) = jobs::enqueue_unique(&state.db, org_id, repo_id, "cdnpack", None) {
        eprintln!("weft: cdnpack enqueue: {e}");
    }
}

pub fn spawn(state: SharedState) {
    let poll = super::env_period("STRATUM_CDNPACK_POLL_SECS", 60);
    if poll.is_zero() {
        return;
    }
    // The lease is the recovery time, not a tuning knob.
    //
    // A claim is held until it expires, so if the node packing a
    // repository dies, this is exactly how long that repository's
    // packing stays stuck before another node may take it. Hard-coded
    // at 900s it was fifteen minutes with no way to shorten it, and no
    // way for a test to observe recovery at all — a constant that reads
    // as arbitrary right up until something dies mid-pack.
    //
    // 900 stays the default because packing genuinely is the long job
    // here; what changes is that an operator can now say otherwise.
    // Every other worker already took its lease from the environment;
    // this one was the last that did not.
    let lease_ms = super::lease_ms("STRATUM_CDNPACK_LEASE_SECS", 900);
    tokio::spawn(async move {
        loop {
            let claimed = {
                let db = state.db.clone();
                tokio::task::spawn_blocking(move || jobs::claim(&db, "cdnpack", lease_ms))
                    .await
                    .unwrap_or_else(|e| Err(format!("join: {e}")))
            };
            match claimed {
                Ok(Some(job)) => match run_one(&state, &job).await {
                    Ok(o) => {
                        let _ = jobs::complete(&state.db, &job.id, Some(&format!("{o:?}")));
                    }
                    Err(e) => {
                        eprintln!("weft: cdn pack failed: {e}");
                        let _ = jobs::fail(&state.db, &job.id, &e);
                    }
                },
                Ok(None) => tokio::time::sleep(poll).await,
                Err(e) => {
                    eprintln!("weft: cdnpack claim: {e}");
                    tokio::time::sleep(poll).await;
                }
            }
        }
    });
}

pub async fn run_one(
    state: &SharedState,
    job: &stratum_control::jobs::Job,
) -> Result<PackOutcome, String> {
    let Some(repo_id) = job.repo_id.clone() else {
        return Err("cdnpack job without repo".into());
    };
    let Some(repo) = stratum_control::registry::repo_by_id(&state.db, &job.org_id, &repo_id)?
    else {
        return Ok(PackOutcome::NotNeeded);
    };
    let store_url = state.store_url.clone();
    let prefix = repo.prefix().as_str().to_string();
    let work = super::job_work_dir(&state.data_dir, "cdnpack", &repo_id, &job.id);
    tokio::task::spawn_blocking(move || {
        let store = ObjectStore::new(&store_url, LatencyModel::None);
        let out = build(&store, &prefix, &work);
        // This run's own directory; see `workers::job_work_dir`.
        let _ = std::fs::remove_dir_all(&work);
        out
    })
    .await
    .unwrap_or_else(|e| Err(format!("join: {e}")))
}

/// The blocking half: skip when current, else materialize → pack →
/// publish. Split out so integration tests can drive it directly.
pub fn build(
    store: &ObjectStore,
    prefix: &str,
    work: &std::path::Path,
) -> Result<PackOutcome, String> {
    let manifest_bytes = match store.get(&format!("{prefix}/manifest.json")) {
        Ok(b) => b,
        // No manifest yet (repo registered, nothing pushed): nothing to do.
        Err(e) if e.contains("HTTP 404") => return Ok(PackOutcome::NotNeeded),
        Err(e) => return Err(e),
    };
    let manifest: stratum_store::Manifest =
        serde_json::from_slice(&manifest_bytes).map_err(|e| format!("manifest: {e}"))?;
    let Some(tip) = manifest.tip().map(str::to_string) else {
        // Empty repo (no HEAD ref yet) — not a failure, just nothing to pack.
        return Ok(PackOutcome::NotNeeded);
    };
    if manifest.total_entries() == 0 {
        return Ok(PackOutcome::NotNeeded);
    }
    let previous = load_descriptor(store, prefix);
    if previous.as_ref().is_some_and(|d| d.tip == tip) {
        return Ok(PackOutcome::NotNeeded);
    }

    std::fs::create_dir_all(work).map_err(|e| e.to_string())?;
    let (pack_hash, pack_path) = stratum_engine::materialize::export_pack(store, prefix, work)?;
    let bytes = std::fs::read(&pack_path).map_err(|e| e.to_string())?;
    let key = pack_key(prefix, &tip, &pack_hash);
    store
        .put(&key, &bytes, PutCond::None)
        .map_err(|e| format!("publish cdn pack: {e:?}"))?;

    // Pointer last, and only after the pack itself landed: a descriptor
    // naming an absent pack would make opted-in clones fail outright.
    let descriptor = CdnPackDescriptor {
        tip,
        pack_key: key,
        pack_hash,
        size: bytes.len() as u64,
        total_entries: manifest.total_entries(),
        created_at: stratum_control::ids::now_ms(),
    };
    let body = serde_json::to_vec(&descriptor).map_err(|e| e.to_string())?;
    store
        .put(&descriptor_key(prefix), &body, PutCond::None)
        .map_err(|e| format!("publish cdn descriptor: {e:?}"))?;

    // Superseded pack is unreferenced now; a failed delete is harmless
    // (the GC sweep and lifecycle rules also cover it).
    if let Some(old) = previous {
        if old.pack_key != descriptor.pack_key {
            let _ = store.delete(&old.pack_key);
        }
    }
    let _ = std::fs::remove_file(&pack_path);
    Ok(PackOutcome::Built)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_repo_scoped_and_content_addressed() {
        let p = "o/ORG/r/REPO/prod";
        assert_eq!(descriptor_key(p), "o/ORG/r/REPO/prod/cdn/current.json");
        // Tip AND pack hash both in the name: republishing never collides
        // with an in-flight download of the superseded pack.
        assert_eq!(
            pack_key(p, "aaaa", "bbbb"),
            "o/ORG/r/REPO/prod/cdn/aaaa-bbbb.pack"
        );
        // Keys stay under the repo prefix — the isolation boundary.
        assert!(pack_key(p, "t", "h").starts_with(p));
        assert!(descriptor_key(p).starts_with(p));
    }

    #[test]
    fn descriptor_round_trips() {
        let d = CdnPackDescriptor {
            tip: "0123456789abcdef0123456789abcdef01234567".into(),
            pack_key: "o/a/r/b/prod/cdn/tip-hash.pack".into(),
            pack_hash: "hash".into(),
            size: 4096,
            total_entries: 12,
            created_at: 1_700_000_000_000,
        };
        let bytes = serde_json::to_vec(&d).unwrap();
        let back: CdnPackDescriptor = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn a_descriptor_is_only_current_for_its_own_tip() {
        // The staleness rule the worker applies: same tip = skip, any
        // other tip = rebuild. (A stale pack is still SERVED — the serve
        // path tops up inline — this only decides when to repack.)
        let d = CdnPackDescriptor {
            tip: "tip-1".into(),
            pack_key: "k".into(),
            pack_hash: "h".into(),
            size: 1,
            total_entries: 1,
            created_at: 0,
        };
        assert!(Some(&d).is_some_and(|x| x.tip == "tip-1"));
        assert!(Some(&d).is_none_or(|x| x.tip != "tip-2"));
    }
}
