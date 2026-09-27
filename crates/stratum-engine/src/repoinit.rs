//! Empty-repo creation (Repos R1): one control-row insert plus exactly one
//! conditional PUT — the whole reason create-repo lands in <100 ms.

use stratum_store::manifest::Manifest;
use stratum_store::{ObjectStore, PutCond, PutError};

/// Create the empty manifest for a brand-new repo at `prefix`
/// (create-only: a second attempt on the same prefix fails loudly).
pub fn create_empty(store: &ObjectStore, prefix: &str, default_branch: &str) -> Result<(), String> {
    let layout = prefix.rsplit('/').next().unwrap_or(prefix).to_string();
    let manifest = Manifest {
        schema: 3,
        repo: prefix.to_string(),
        layout,
        object_format: "sha1".into(),
        refs: Vec::new(),
        head: format!("refs/heads/{default_branch}"),
        segments: Vec::new(),
        cold_segments: Vec::new(),
        hot_segments: Vec::new(),
        spine: Vec::new(),
        locator: None,
        shallow: Vec::new(),
        epoch: crate::ingest::mint_epoch(),
        extra_emission: None,
        tail_emissions: Vec::new(),
        ref_pages: Vec::new(),
        snapshot: None,
        wal: Vec::new(),
    };
    let body = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    match store.put(
        &format!("{prefix}/manifest.json"),
        &body,
        PutCond::IfNoneMatchStar,
    ) {
        Ok(()) => Ok(()),
        Err(PutError::Conflict) => Err("repo storage already initialized".into()),
        Err(e) => Err(e.to_string()),
    }
}
