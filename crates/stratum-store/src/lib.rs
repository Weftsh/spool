//! Stratum's read-side storage plane: the segment manifest and the
//! object-store client (with the S3 latency model from docs/storage-model.md).
//!
//! The store surface is deliberately tiny (brief §8.6): GET and GET-with-Range
//! over any S3-compatible HTTP endpoint. Writes happen offline in the harness
//! (bench/ingest_segments.py), never here.

// STRATUM-CORE DIVERGENCE: vendored code is kept byte-close to the research
// repo; suppress style-only lints newer clippy raises against it.
#![allow(clippy::manual_is_multiple_of)]

pub mod b64;
pub mod gitobj;
pub mod manifest;
pub mod pack;
pub mod plane;
pub mod refpages;
pub mod sig;
pub mod store;

pub use manifest::Manifest;
pub use plane::Plane;
pub use store::{LatencyModel, ObjectStore, PutCond, PutError};

/// Load a repo's manifest per env config. STRATUM_MANIFEST_TIER=kv reads it
/// at KV latency (H2's low-latency tier) when a latency model is active.
pub fn load_manifest(store: &ObjectStore, repo: &str, layout: &str) -> Result<Manifest, String> {
    let key = format!("{repo}/{layout}/manifest.json");
    // STRATUM-CORE DIVERGENCE: the KV-tier read (bench latency modeling) is
    // compiled out of the product build.
    #[cfg(feature = "bench-models")]
    let bytes = if std::env::var("STRATUM_MANIFEST_TIER").as_deref() == Ok("kv")
        && LatencyModel::from_env() != LatencyModel::None
    {
        let base = std::env::var("STRATUM_STORE_URL").map_err(|e| e.to_string())?;
        ObjectStore::new(&base, LatencyModel::Kv).get(&key)?
    } else {
        store.get(&key)?
    };
    #[cfg(not(feature = "bench-models"))]
    let bytes = store.get(&key)?;
    let m: Manifest = serde_json::from_slice(&bytes).map_err(|e| format!("manifest {key}: {e}"))?;
    // This build's OID parsing, locator records, and pack hashing are all
    // SHA-1-width; serving a SHA-256 layout would misread it. Fail loudly.
    if m.object_format != "sha1" {
        return Err(format!(
            "manifest {key}: object_format {:?} not supported by this build (sha1 only — \
             see docs/design-notes/sha256-plan.md)",
            m.object_format
        ));
    }
    Ok(m)
}
