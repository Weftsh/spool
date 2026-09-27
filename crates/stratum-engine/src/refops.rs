//! Server-side ref transactions: the REST API's authority over refs,
//! implemented directly as manifest CAS (I9 — the manifest is the only ref
//! truth and changes only by CAS). Unlike the wire receive path's
//! fast-forward-and-create-only policy, the API may move refs backwards
//! (reset), delete branches, and write tags: the asymmetry is deliberate —
//! git-push keeps forge semantics, the API owns undo (R4).
//!
//! Reset never destroys objects: segments and WAL entries stay; orphaned
//! commits remain reachable by SHA until epoch GC's grace window (R4).

use stratum_store::manifest::{Manifest, WalEntry};
use stratum_store::{ObjectStore, PutCond, PutError};

const CAS_RETRIES: usize = 10;

/// Precondition on a ref's current value (optimistic concurrency; R2's
/// compare-and-swap on parent).
#[derive(Debug, Clone, PartialEq)]
pub enum Expect {
    Any,
    Absent,
    Equals(String),
}

#[derive(Debug, Clone)]
pub struct RefUpdate {
    pub name: String,
    pub expect: Expect,
    /// None = delete the ref.
    pub new: Option<String>,
}

/// A pack of new objects to land with the transaction (commit API):
/// content-addressed WAL entry + oids sidecar.
pub struct NewPack {
    pub payload: Vec<u8>,
    /// Sorted binary oids of the payload's objects.
    pub oids: Vec<[u8; 20]>,
    pub entries: u64,
}

/// Conflict outcomes carry the current tip so clients can rebase (409).
pub enum TxnError {
    /// (ref name, current value) — precondition failed.
    Conflict(String, Option<String>),
    Other(String),
}

impl From<String> for TxnError {
    fn from(e: String) -> TxnError {
        TxnError::Other(e)
    }
}

/// Run one ref transaction: validate preconditions against the current
/// manifest, optionally append a WAL entry, swap the manifest by CAS.
/// Returns the committed manifest.
pub fn transact(
    store: &ObjectStore,
    prefix: &str,
    updates: &[RefUpdate],
    pack: Option<&NewPack>,
) -> Result<Manifest, TxnError> {
    let manifest_key = format!("{prefix}/manifest.json");
    let mut attempt = 0;
    loop {
        let (mbytes, etag) = store
            .get_with_etag(&manifest_key)
            .map_err(TxnError::Other)?;
        let mut manifest: Manifest = serde_json::from_slice(&mbytes)
            .map_err(|e| TxnError::Other(format!("manifest: {e}")))?;
        if manifest.epoch.is_empty() {
            return Err(TxnError::Other("layout predates epochs".into()));
        }

        // Preconditions against this snapshot.
        for u in updates {
            let current = current_ref(store, &manifest, &u.name).map_err(TxnError::Other)?;
            match (&u.expect, &current) {
                (Expect::Any, _) => {}
                (Expect::Absent, None) => {}
                (Expect::Absent, Some(cur)) => {
                    return Err(TxnError::Conflict(u.name.clone(), Some(cur.clone())))
                }
                (Expect::Equals(want), Some(cur)) if want == cur => {}
                (Expect::Equals(_), cur) => {
                    return Err(TxnError::Conflict(u.name.clone(), cur.clone()))
                }
            }
            if u.new.is_none() && !manifest.ref_pages.is_empty() {
                return Err(TxnError::Other(
                    "ref deletion on paged ref stores is not supported yet".into(),
                ));
            }
        }

        // WAL objects land before the manifest referencing them (I10).
        if let Some(p) = pack {
            let digest = stratum_store::pack::hex(&sha1_digest(&p.payload));
            let wal_key = format!("{prefix}/{}/wal/{digest}.seg", manifest.epoch);
            let oids_key = format!("{prefix}/{}/wal/{digest}.oids", manifest.epoch);
            let oid_blob: Vec<u8> = p.oids.iter().flat_map(|o| o.iter().copied()).collect();
            store
                .put(&wal_key, &p.payload, PutCond::None)
                .map_err(|e| TxnError::Other(e.to_string()))?;
            store
                .put(&oids_key, &oid_blob, PutCond::None)
                .map_err(|e| TxnError::Other(e.to_string()))?;
            manifest.wal.push(WalEntry {
                key: wal_key,
                oids_key,
                entries: p.entries,
                bytes: p.payload.len() as u64,
                updates: updates
                    .iter()
                    .filter_map(|u| {
                        u.new.as_ref().map(|n| {
                            (
                                u.name.clone(),
                                match &u.expect {
                                    Expect::Equals(old) => old.clone(),
                                    _ => "0".repeat(40),
                                },
                                n.clone(),
                            )
                        })
                    })
                    .collect(),
            });
        }

        // Apply ref changes.
        let paged = !manifest.ref_pages.is_empty();
        let data_prefix = format!("{prefix}/{}", manifest.epoch);
        for u in updates {
            match &u.new {
                Some(new) => {
                    if paged {
                        stratum_store::refpages::update(
                            store,
                            &mut manifest,
                            &data_prefix,
                            &u.name,
                            new,
                        )
                        .map_err(TxnError::Other)?;
                        if let Some(r) = manifest.refs.iter_mut().find(|(n, _)| n == &u.name) {
                            r.1 = new.clone();
                        }
                    } else {
                        match manifest.refs.iter_mut().find(|(n, _)| n == &u.name) {
                            Some(r) => r.1 = new.clone(),
                            None => manifest.refs.push((u.name.clone(), new.clone())),
                        }
                        manifest.refs.sort();
                    }
                }
                None => {
                    manifest.refs.retain(|(n, _)| n != &u.name);
                }
            }
        }

        let body = serde_json::to_vec(&manifest).map_err(|e| TxnError::Other(e.to_string()))?;
        match store.put(&manifest_key, &body, PutCond::IfMatch(etag)) {
            Ok(()) => return Ok(manifest),
            Err(PutError::Conflict) => {
                attempt += 1;
                if attempt >= CAS_RETRIES {
                    return Err(TxnError::Other(
                        "concurrent writers; transaction retried out".into(),
                    ));
                }
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as u64)
                    .unwrap_or(12345);
                std::thread::sleep(std::time::Duration::from_millis(
                    10 * attempt as u64 + nanos % 50,
                ));
                continue;
            }
            Err(e) => return Err(TxnError::Other(e.to_string())),
        }
    }
}

pub fn current_ref(
    store: &ObjectStore,
    manifest: &Manifest,
    name: &str,
) -> Result<Option<String>, String> {
    if let Some((_, oid)) = manifest.refs.iter().find(|(n, _)| n == name) {
        return Ok(Some(oid.clone()));
    }
    if !manifest.ref_pages.is_empty() {
        return stratum_store::refpages::lookup(store, manifest, name);
    }
    Ok(None)
}

fn sha1_digest(data: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    Sha1::digest(data).into()
}
