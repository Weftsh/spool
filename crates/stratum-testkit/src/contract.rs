//! The store contract: the semantics every invariant above the object
//! store actually leans on, written down as executable cases that run
//! against any S3-compatible backend.
//!
//! It lives in the testkit rather than in `stratum-store` on purpose.
//! These cases assert things about a *backend*; nothing in the server
//! ever calls them, and most of each case is the failure branch that a
//! healthy store never takes. Putting that in a product crate would hand
//! the 100%-coverage gate dozens of lines that can only be reached by a
//! misbehaving store — i.e. a pile of ledger entries — for code that is,
//! by the coverage gate's own words, test infrastructure and not product
//! code.
//!
//! Why this exists. I9 says the manifest is the only ref truth and it
//! changes only by compare-and-swap: read `(manifest, etag)`, validate
//! against exactly that snapshot, write with `If-Match`, re-run
//! everything on a conflict. That is the whole consistency story, and it
//! is not a property of our code — it is a property of the *store*. If a
//! backend's conditional-PUT semantics differ from what the engine
//! assumes in any respect, the CAS is not a CAS and the invariant is
//! decoration.
//!
//! Until this module existed, those semantics had been verified against
//! exactly one implementation — MinIO, in tests — while production runs a
//! fleet of nodes against real S3. `reference/formats.md` called that the
//! top open item, and it was right to: the first thing this suite pinned
//! was that real S3 answers **409 ConditionalRequestConflict** when two
//! conditional writes to one key overlap, where MinIO only ever answers
//! 412. The store client mapped 409 to a generic error, so on real S3 the
//! loser of a manifest CAS fell out of the retry loop and failed a push
//! that should simply have re-run. No amount of MinIO testing could have
//! found that.
//!
//! Each case is derived from a real call site, not from the S3 docs. The
//! rule for adding one: name the code that would break if the backend did
//! otherwise. A case nobody depends on is a case that will be "fixed" by
//! relaxing it the first time some backend disagrees.
//!
//! The suite is backend-agnostic and takes a key prefix, so it is safe to
//! point at a production-shaped bucket. It creates only keys under that
//! prefix and deletes them as it goes.

use stratum_store::{ObjectStore, PutCond, PutError};

/// One semantic the engine relies on.
pub struct ContractCase {
    pub name: &'static str,
    /// What breaks if the backend does otherwise. Printed on failure so a
    /// red case explains itself without a trip back to this file.
    pub relied_on_by: &'static str,
    pub run: fn(&ObjectStore, &str) -> Result<(), String>,
}

#[derive(Debug, Default)]
pub struct ContractReport {
    pub passed: Vec<&'static str>,
    pub failed: Vec<(&'static str, String)>,
    /// Facts observed rather than asserted — the ETag shapes a backend
    /// actually produced, say. The manual real-S3 run leaves this behind
    /// as evidence of what was seen, which is the point of running it.
    pub observed: Vec<(&'static str, String)>,
}

impl ContractReport {
    pub fn ok(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Run every case against `store`, isolated under `prefix`.
pub fn run_all(store: &ObjectStore, prefix: &str) -> ContractReport {
    let mut report = ContractReport::default();
    for case in cases() {
        match (case.run)(store, prefix) {
            Ok(()) => report.passed.push(case.name),
            Err(e) => report.failed.push((case.name, e)),
        }
    }
    if let Ok(shape) = observe_etag_shape(store, prefix) {
        report.observed.push(("etag_shape", shape));
    }
    report
}

/// A key unique to this case, under the caller's prefix.
fn k(prefix: &str, name: &str) -> String {
    format!("{prefix}/contract-{name}")
}

fn cleanup(store: &ObjectStore, key: &str) {
    let _ = store.delete(key);
}

/// Record — never assert — what an ETag looks like here. Nothing in the
/// codebase parses ETag format (`get_with_etag` keeps it as an opaque
/// String and echoes it back verbatim), and nothing should start: S3
/// multipart ETags are not MD5 and carry a `-N` suffix, and SSE-KMS
/// changes the shape again. This exists so the manual run leaves a record
/// of the shape, not so anything can depend on it.
fn observe_etag_shape(store: &ObjectStore, prefix: &str) -> Result<String, String> {
    let key = k(prefix, "etag-shape");
    store
        .put(&key, b"shape", PutCond::None)
        .map_err(|e| e.to_string())?;
    let (_, etag) = store.get_with_etag(&key)?;
    cleanup(store, &key);
    Ok(etag)
}

pub fn cases() -> &'static [ContractCase] {
    &[
        ContractCase {
            name: "if_none_match_star_creates_an_absent_key",
            relied_on_by: "engine/repoinit.rs — the empty-manifest PUT that \
                           makes repo creation atomic",
            run: |store, prefix| {
                let key = k(prefix, "inm-create");
                cleanup(store, &key);
                store
                    .put(&key, b"first", PutCond::IfNoneMatchStar)
                    .map_err(|e| format!("create on absent key must succeed: {e}"))?;
                let got = store.get(&key)?;
                cleanup(store, &key);
                if got != b"first" {
                    return Err(format!("read back {got:?}, wanted b\"first\""));
                }
                Ok(())
            },
        },
        ContractCase {
            name: "if_none_match_star_conflicts_on_a_present_key",
            relied_on_by: "engine/repoinit.rs — 'repo storage already \
                           initialized'; two nodes creating one repo",
            run: |store, prefix| {
                let key = k(prefix, "inm-conflict");
                cleanup(store, &key);
                store
                    .put(&key, b"first", PutCond::IfNoneMatchStar)
                    .map_err(|e| e.to_string())?;
                let second = store.put(&key, b"second", PutCond::IfNoneMatchStar);
                let body = store.get(&key)?;
                cleanup(store, &key);
                match second {
                    Err(PutError::Conflict) => {}
                    Ok(()) => return Err("create over a present key succeeded".into()),
                    Err(e) => return Err(format!("wanted Conflict, got {e}")),
                }
                if body != b"first" {
                    return Err("a refused create still changed the bytes".into());
                }
                Ok(())
            },
        },
        ContractCase {
            name: "if_match_current_etag_succeeds",
            relied_on_by: "proto/receive.rs and engine/refops.rs — the \
                           manifest swap on every push",
            run: |store, prefix| {
                let key = k(prefix, "ifmatch-ok");
                cleanup(store, &key);
                store
                    .put(&key, b"v1", PutCond::None)
                    .map_err(|e| e.to_string())?;
                let (_, etag) = store.get_with_etag(&key)?;
                let r = store.put(&key, b"v2", PutCond::IfMatch(etag));
                let body = store.get(&key)?;
                cleanup(store, &key);
                r.map_err(|e| format!("swap on the current etag must succeed: {e}"))?;
                if body != b"v2" {
                    return Err("the swap did not take".into());
                }
                Ok(())
            },
        },
        ContractCase {
            name: "if_match_stale_etag_conflicts",
            relied_on_by: "I9 — 'never patch a manifest you didn't validate \
                           against'; the CAS retry arm in receive.rs",
            run: |store, prefix| {
                let key = k(prefix, "ifmatch-stale");
                cleanup(store, &key);
                store
                    .put(&key, b"v1", PutCond::None)
                    .map_err(|e| e.to_string())?;
                let (_, stale) = store.get_with_etag(&key)?;
                // Someone else wins the race.
                store
                    .put(&key, b"v2", PutCond::None)
                    .map_err(|e| e.to_string())?;
                let r = store.put(&key, b"v3", PutCond::IfMatch(stale));
                let body = store.get(&key)?;
                cleanup(store, &key);
                match r {
                    Err(PutError::Conflict) => {}
                    Ok(()) => {
                        return Err("a stale If-Match overwrote a newer object — \
                                    the CAS is not a CAS on this backend"
                            .into())
                    }
                    Err(e) => return Err(format!("wanted Conflict, got {e}")),
                }
                if body != b"v2" {
                    return Err("a refused swap still changed the bytes".into());
                }
                Ok(())
            },
        },
        ContractCase {
            name: "a_losing_concurrent_conditional_write_reports_conflict",
            relied_on_by: "the whole multi-node story: every racing pusher \
                           must retry, not fail. This is the case that \
                           catches S3's 409 vs MinIO's 412.",
            run: |store, prefix| {
                let key = k(prefix, "cas-race");
                cleanup(store, &key);
                store
                    .put(&key, b"base", PutCond::None)
                    .map_err(|e| e.to_string())?;
                let (_, etag) = store.get_with_etag(&key)?;

                // Every writer validated against the same snapshot, so
                // exactly one may win and every loser must be told to
                // re-read and retry — not handed an opaque error that
                // falls out of the retry loop.
                let results: Vec<Result<(), PutError>> = std::thread::scope(|s| {
                    let handles: Vec<_> = (0..4)
                        .map(|i| {
                            let etag = etag.clone();
                            let key = key.clone();
                            s.spawn(move || {
                                store.put(&key, format!("w{i}").as_bytes(), PutCond::IfMatch(etag))
                            })
                        })
                        .collect();
                    handles.into_iter().map(|h| h.join().unwrap()).collect()
                });
                cleanup(store, &key);

                let winners = results.iter().filter(|r| r.is_ok()).count();
                if winners != 1 {
                    return Err(format!(
                        "{winners} of 4 concurrent conditional writes won; exactly 1 must"
                    ));
                }
                for r in &results {
                    if let Err(e) = r {
                        if !matches!(e, PutError::Conflict) {
                            return Err(format!(
                                "a loser got {e} instead of Conflict. On this backend a \
                                 racing writer is not told to retry, so the loser of a \
                                 manifest CAS fails the user's push."
                            ));
                        }
                    }
                }
                Ok(())
            },
        },
        ContractCase {
            name: "an_etag_is_accepted_verbatim_and_is_strong",
            relied_on_by: "store.rs — the etag is echoed into If-Match with \
                           no normalisation",
            run: |store, prefix| {
                let key = k(prefix, "etag-verbatim");
                cleanup(store, &key);
                store
                    .put(&key, b"v1", PutCond::None)
                    .map_err(|e| e.to_string())?;
                let (_, etag) = store.get_with_etag(&key)?;
                cleanup(store, &key);
                if etag.is_empty() {
                    return Err("empty ETag: If-Match would be sent with no value, \
                                and the answer is undefined"
                        .into());
                }
                if etag.starts_with("W/") {
                    return Err(format!(
                        "weak ETag {etag}: weak validators are not legal in If-Match, \
                         so every CAS on this path would be silently unconditional. \
                         An interposed proxy or CDN is the usual cause."
                    ));
                }
                Ok(())
            },
        },
        ContractCase {
            name: "an_etag_changes_when_the_bytes_change",
            relied_on_by: "I9 — the etag is the CAS token; a stable etag \
                           across a rewrite would make every swap succeed",
            run: |store, prefix| {
                let key = k(prefix, "etag-changes");
                cleanup(store, &key);
                store
                    .put(&key, b"v1", PutCond::None)
                    .map_err(|e| e.to_string())?;
                let (_, first) = store.get_with_etag(&key)?;
                store
                    .put(&key, b"v2-different", PutCond::None)
                    .map_err(|e| e.to_string())?;
                let (_, second) = store.get_with_etag(&key)?;
                cleanup(store, &key);
                if first == second {
                    return Err("the etag did not change when the object did".into());
                }
                Ok(())
            },
        },
        ContractCase {
            name: "a_missing_key_reports_404_not_403",
            relied_on_by: "read.rs, receive.rs, cdnpack.rs, sync.rs, \
                           freshness.rs — ~11 sites branch on the literal \
                           string \"HTTP 404\" to mean 'absent'",
            run: |store, prefix| {
                let key = k(prefix, "definitely-absent");
                cleanup(store, &key);
                match store.get(&key) {
                    Ok(_) => Err("a key we just deleted still reads".into()),
                    Err(e) if e.contains("HTTP 404") => Ok(()),
                    Err(e) if e.contains("HTTP 403") => Err(format!(
                        "absence reports 403, not 404: {e}. Every 'is it there?' \
                         check in the engine matches on \"HTTP 404\", so on this \
                         backend the first push into a fresh repo fails hard. The \
                         usual cause is an IAM policy without s3:ListBucket."
                    )),
                    Err(e) => Err(format!("absence reports neither 404 nor 403: {e}")),
                }
            },
        },
        ContractCase {
            name: "a_fresh_put_is_readable_immediately",
            relied_on_by: "I7 data-first-pointer-last, and every 'push \
                           acknowledged ⇒ the next request anywhere sees \
                           it' claim we make",
            run: |store, prefix| {
                let key = k(prefix, "raw-visibility");
                cleanup(store, &key);
                store
                    .put(&key, b"immediately", PutCond::None)
                    .map_err(|e| e.to_string())?;
                let got = store.get(&key);
                cleanup(store, &key);
                match got {
                    Ok(b) if b == b"immediately" => Ok(()),
                    Ok(b) => Err(format!("read back {} bytes, wanted 11", b.len())),
                    Err(e) => Err(format!(
                        "a just-written object was not readable: {e}. This backend is \
                         eventually consistent for reads-after-write, which the design \
                         does not tolerate."
                    )),
                }
            },
        },
        ContractCase {
            name: "an_overwrite_reads_back_the_new_bytes",
            relied_on_by: "the manifest pointer swap — a reader that gets \
                           the old body after a CAS serves stale refs",
            run: |store, prefix| {
                let key = k(prefix, "overwrite-visibility");
                cleanup(store, &key);
                store
                    .put(&key, b"old", PutCond::None)
                    .map_err(|e| e.to_string())?;
                store
                    .put(&key, b"new", PutCond::None)
                    .map_err(|e| e.to_string())?;
                let got = store.get(&key);
                cleanup(store, &key);
                match got {
                    Ok(b) if b == b"new" => Ok(()),
                    Ok(_) => Err("an overwrite read back the old bytes".into()),
                    Err(e) => Err(e),
                }
            },
        },
        ContractCase {
            name: "a_ranged_get_returns_206_with_exactly_the_requested_bytes",
            relied_on_by: "I1 — a served stream is header + verbatim byte \
                           ranges + trailer; I13 requires 206",
            run: |store, prefix| {
                use std::io::Read;
                let key = k(prefix, "ranges");
                cleanup(store, &key);
                let body: Vec<u8> = (0u8..64).collect();
                store
                    .put(&key, &body, PutCond::None)
                    .map_err(|e| e.to_string())?;
                // A middle range, a single byte, and a range ending on the
                // final byte — the shapes Manifest::suffix_plan emits.
                let spans: [(u64, u64); 3] = [(8, 15), (0, 0), (32, 63)];
                let mut fail = None;
                for (start, end) in spans {
                    let mut buf = Vec::new();
                    match store
                        .get_stream(&key, Some((start, end)))
                        .and_then(|mut r| r.read_to_end(&mut buf).map_err(|e| e.to_string()))
                    {
                        Ok(_) => {
                            let want = &body[start as usize..=end as usize];
                            if buf != want {
                                fail = Some(format!(
                                    "range {start}-{end}: got {} bytes, wanted {}",
                                    buf.len(),
                                    want.len()
                                ));
                                break;
                            }
                        }
                        Err(e) => {
                            fail = Some(format!("range {start}-{end}: {e}"));
                            break;
                        }
                    }
                }
                cleanup(store, &key);
                match fail {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            },
        },
        ContractCase {
            name: "list_returns_keys_and_parseable_timestamps",
            relied_on_by: "engine/gc.rs — epoch sweeps compare \
                           LastModified against a grace window",
            run: |store, prefix| {
                let base = format!("{prefix}/contract-listing");
                for i in 0..3 {
                    store
                        .put(&format!("{base}/{i}.txt"), b"x", PutCond::None)
                        .map_err(|e| e.to_string())?;
                }
                let listing = store.list(&format!("{base}/"))?;
                for i in 0..3 {
                    let _ = store.delete(&format!("{base}/{i}.txt"));
                }
                if listing.len() != 3 {
                    return Err(format!("listed {} keys, wanted 3", listing.len()));
                }
                for (key, last_modified) in &listing {
                    if !key.starts_with(&base) {
                        return Err(format!("key {key} is not returned as a full key"));
                    }
                    // gc::parse_rfc3339_secs wants "YYYY-MM-DDTHH:MM:SS…".
                    let looks_rfc3339 = last_modified.len() >= 19
                        && last_modified.as_bytes()[4] == b'-'
                        && last_modified.as_bytes()[10] == b'T';
                    if !looks_rfc3339 {
                        return Err(format!(
                            "LastModified {last_modified:?} is not RFC3339; epoch GC \
                             treats an unparseable timestamp as infinitely new and \
                             would never reclaim anything"
                        ));
                    }
                }
                Ok(())
            },
        },
        ContractCase {
            name: "deleting_an_absent_key_is_not_an_error",
            relied_on_by: "engine/gc.rs — sweeps re-run over partially \
                           deleted epochs and must be idempotent",
            run: |store, prefix| {
                let key = k(prefix, "delete-idempotent");
                cleanup(store, &key);
                store
                    .delete(&key)
                    .map_err(|e| format!("deleting an absent key must be Ok: {e}"))
            },
        },
    ]
}
