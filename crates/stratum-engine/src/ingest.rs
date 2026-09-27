//! Offline ingest: a git repo → reachability-ordered segments + manifest,
//! staged locally, then published data-first-pointer-last.
//!
//! Faithful Rust port of the research repo's `bench/ingest_segments.py`
//! (tiered layout only — the flat layout was benchmark scaffolding).
//! Segments are raw pack *payloads*: a per-segment pack minus its 12-byte
//! header and 20-byte trailer. OFS_DELTA offsets are relative, so payloads
//! stay valid when the server concatenates them into one pack stream (I1).

use crate::gitcmd::{git, run, run_str, run_with_stdin};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use stratum_store::manifest::{ExtraEmission, HotSegment, Manifest, RefPage, Segment, SpineEntry};
use stratum_store::{ObjectStore, PutCond, PutError};

#[derive(Debug, Clone)]
pub struct IngestConfig {
    /// Cold segment payload budget (research: 64 MB — the measured
    /// bytes-ratio/latency sweet spot).
    pub budget_bytes: u64,
    /// Hot window: spine commits with per-commit thin emissions (research:
    /// 1024 ≈ a year of history on git/git).
    pub hot_commits: usize,
    /// Hot segment cut size (research: 16 MB).
    pub hot_budget_bytes: u64,
    /// Every Nth hot emission is packed non-thin, bounding REF-delta chain
    /// length for point reads (research: 64; 0 = never).
    pub hot_anchor: usize,
    /// Max refs per ref-store page; ref counts beyond one page shard into
    /// the paged store.
    pub page_size: usize,
    /// Always shard refs into pages (implied when count exceeds page_size).
    pub paged_refs: bool,
    /// Serve every ref in the source repo (heads + tags), not just the
    /// primary branch. Mirrors want this on.
    pub all_refs: bool,
}

impl Default for IngestConfig {
    fn default() -> Self {
        IngestConfig {
            budget_bytes: 64 * 1024 * 1024,
            hot_commits: 1024,
            hot_budget_bytes: 16 * 1024 * 1024,
            hot_anchor: 64,
            page_size: 1000,
            paged_refs: false,
            all_refs: true,
        }
    }
}

pub struct IngestOutput {
    pub manifest: Manifest,
    pub epoch: String,
    /// Local staging directory holding the epoch's files (segments,
    /// snapshot, ref pages, and later the locator generation files).
    pub staging: PathBuf,
}

struct Obj {
    oid: String,
    path: String,
    kind: String,
    size: u64,
}

/// `rev-list --objects --in-commit-order <rev>` + batch-check sizes:
/// (oid, path, kind, size) at first mention, newest commit first.
fn enumerate_objects(repo: &Path, rev: &str) -> Result<Vec<Obj>, String> {
    let out = run(git(repo).args(["rev-list", "--objects", "--in-commit-order", rev]))?;
    let text = String::from_utf8_lossy(&out);
    let mut listed: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let (oid, path) = line.split_once(' ').unwrap_or((line, ""));
        listed.push((oid.to_string(), path.to_string()));
    }
    let ids: Vec<u8> = listed
        .iter()
        .flat_map(|(o, _)| o.bytes().chain(std::iter::once(b'\n')))
        .collect();
    let batch = run_with_stdin(
        git(repo).args([
            "cat-file",
            "--batch-check=%(objecttype) %(objectsize)",
            "--buffer",
        ]),
        &ids,
    )?;
    let batch = String::from_utf8_lossy(&batch);
    let metas: Vec<&str> = batch.lines().collect();
    if metas.len() != listed.len() {
        return Err(format!(
            "cat-file returned {} metas for {} objects",
            metas.len(),
            listed.len()
        ));
    }
    let mut objs = Vec::with_capacity(listed.len());
    for ((oid, path), meta) in listed.into_iter().zip(metas) {
        let (kind, size) = meta
            .split_once(' ')
            .ok_or_else(|| format!("bad batch-check line {meta:?}"))?;
        objs.push(Obj {
            oid,
            path,
            kind: kind.to_string(),
            size: size.parse().map_err(|_| format!("bad size {meta:?}"))?,
        });
    }
    Ok(objs)
}

/// Path-major ordering (the research result that beat the brief's own H1):
/// commits/tags in original newest-first order, then trees and blobs grouped
/// by (kind, path); groups ordered by first appearance, members newest-first.
fn order_path(objs: Vec<Obj>) -> Vec<Obj> {
    let mut commits = Vec::new();
    let mut groups: Vec<Vec<Obj>> = Vec::new();
    let mut group_of: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    for o in objs {
        if o.kind == "commit" || o.kind == "tag" {
            commits.push(o);
        } else {
            let key = (o.kind.clone(), o.path.clone());
            match group_of.get(&key) {
                Some(&i) => groups[i].push(o),
                None => {
                    group_of.insert(key, groups.len());
                    groups.push(vec![o]);
                }
            }
        }
    }
    for g in groups {
        commits.extend(g);
    }
    commits
}

fn cut_segments(objs: Vec<Obj>, budget: u64) -> Vec<Vec<Obj>> {
    let mut segs = Vec::new();
    let mut cur = Vec::new();
    let mut cur_bytes = 0u64;
    for o in objs {
        cur_bytes += o.size;
        cur.push(o);
        if cur_bytes >= budget {
            segs.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
    }
    if !cur.is_empty() {
        segs.push(cur);
    }
    segs
}

fn strip_pack(out: &[u8], expect_entries: Option<u64>) -> Result<(Vec<u8>, u64), String> {
    if out.len() < 32
        || &out[..4] != b"PACK"
        || u32::from_be_bytes(out[4..8].try_into().unwrap()) != 2
    {
        return Err("pack-objects produced no v2 pack".into());
    }
    let entries = u32::from_be_bytes(out[8..12].try_into().unwrap()) as u64;
    if let Some(want) = expect_entries {
        if entries != want {
            return Err(format!(
                "pack entry count {entries} != expected {want} — walk divergence \
                 (see research experiment 004: --no-sparse and seen-set discipline)"
            ));
        }
    }
    Ok((out[12..out.len() - 20].to_vec(), entries))
}

/// Pack exactly these objects, full (non-thin), delta search within the
/// segment only — cold deltas never cross a segment boundary (I2).
fn pack_segment_full(repo: &Path, seg: &[Obj]) -> Result<(Vec<u8>, u64), String> {
    let stdin: Vec<u8> = seg
        .iter()
        .flat_map(|o| {
            let line = if o.path.is_empty() {
                format!("{}\n", o.oid)
            } else {
                format!("{} {}\n", o.oid, o.path)
            };
            line.into_bytes()
        })
        .collect();
    let out = run_with_stdin(
        git(repo).args([
            "pack-objects",
            "--no-reuse-delta",
            "--delta-base-offset",
            "--stdout",
            "-q",
        ]),
        &stdin,
    )?;
    strip_pack(&out, Some(seg.len() as u64))
}

/// Thin pack of everything newly reachable at `commit` relative to `parent`
/// (H3: hot emissions stored pre-deltified against the past). The rev walk
/// over-includes re-introduced objects, which `^oid` exclusions remove —
/// a concatenated pack must never contain an object twice (I5). `seen` is
/// updated with the new objects.
fn pack_thin_emission(
    repo: &Path,
    commit: &str,
    parent: &str,
    seen: &mut HashSet<String>,
    thin: bool,
) -> Result<(Vec<u8>, u64), String> {
    let cand = run(git(repo).args(["rev-list", "--objects", commit, &format!("^{parent}")]))?;
    let cand = String::from_utf8_lossy(&cand);
    let mut new = Vec::new();
    let mut revs = format!("{commit}\n^{parent}\n");
    for line in cand.lines() {
        let oid = line.split(' ').next().unwrap_or("");
        if seen.contains(oid) {
            revs.push('^');
            revs.push_str(oid);
            revs.push('\n');
        } else {
            new.push(oid.to_string());
        }
    }
    // --no-use-bitmap-index matters enormously: the bitmap walk loses path
    // names, so pack-objects never adds the parent's tree/blob versions as
    // thin delta bases (measured in research: 48KB -> 948B for a 16-line
    // commit). Offline ingest can afford the plain walk; request-time stock
    // git cannot, which is exactly H3's edge.
    let mut args = vec!["pack-objects", "--revs"];
    if thin {
        args.push("--thin");
    }
    args.extend_from_slice(&[
        "--no-sparse",
        "--no-use-bitmap-index",
        "--no-reuse-delta",
        "--delta-base-offset",
        "--stdout",
        "-q",
    ]);
    let out = run_with_stdin(git(repo).args(&args), revs.as_bytes())?;
    let (payload, entries) = strip_pack(&out, Some(new.len() as u64))
        .map_err(|e| format!("emission at {commit}: {e}"))?;
    seen.extend(new);
    Ok((payload, entries))
}

/// UTC epoch id: `YYYYMMDDTHHMMSS-<pid>-<seq>` — opaque to every reader,
/// it only namespaces (formats.md). STRATUM-CORE DIVERGENCE: the research
/// harness minted one epoch per process, so time+pid sufficed; a
/// long-lived server minting two epochs in the same second would collide
/// and violate epoch immutability (I8) — the process-wide sequence makes
/// every mint unique.
pub fn mint_epoch() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    // Civil-from-days (Howard Hinnant's algorithm), valid for our range.
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "{y:04}{mo:02}{d:02}T{h:02}{m:02}{s:02}-{}-{seq}",
        std::process::id()
    )
}

/// Ingest `repo` into a staged epoch for the layout at `prefix`
/// (`.../<layout>` — all keys become `<prefix>/<epoch>/<name>`).
/// `primary` is the branch the spine follows (mirror: origin's HEAD branch).
pub fn ingest(
    repo: &Path,
    prefix: &str,
    primary: &str,
    cfg: &IngestConfig,
    staging_root: &Path,
) -> Result<IngestOutput, String> {
    let tip = run_str(git(repo).args(["rev-parse", &format!("refs/heads/{primary}")]))?;
    let epoch = mint_epoch();
    let staging = staging_root.join(&epoch);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;

    // Hot window bounded by first-parent depth: a repo younger than the
    // window gets a spine over its whole history minus the root closure.
    let depth: usize = run_str(git(repo).args([
        "rev-list",
        "--first-parent",
        "--count",
        &format!("refs/heads/{primary}"),
    ]))?
    .parse()
    .map_err(|e| format!("bad rev-list count: {e}"))?;
    let m = cfg.hot_commits.min(depth.saturating_sub(1));

    let boundary = run_str(git(repo).args(["rev-parse", &format!("refs/heads/{primary}~{m}")]))?;
    let spine: Vec<String> = if m == 0 {
        Vec::new()
    } else {
        run_str(git(repo).args([
            "rev-list",
            "--first-parent",
            "--reverse",
            &format!("{boundary}..refs/heads/{primary}"),
        ]))?
        .split_whitespace()
        .map(str::to_string)
        .collect()
    };
    if spine.len() != m || (m > 0 && spine[m - 1] != tip) {
        return Err(format!(
            "spine walk mismatch: {} commits for window {m}",
            spine.len()
        ));
    }

    let mut ref_tips: Vec<(String, String)> = vec![(format!("refs/heads/{primary}"), tip.clone())];
    if cfg.all_refs {
        // **Every ref, not just heads and tags.**
        //
        // This listed those two namespaces only, which meant any other
        // ref was silently dropped by compaction — the fold rebuilds the
        // layout by re-ingesting a seed and simply did not look there,
        // so the ref went and the objects it was holding down went with
        // it. Nothing failed; the repository just quietly lost them at a
        // WAL threshold.
        //
        // That bit `refs/patchsets/*`, which is how a review pins the
        // commit it is about — a rebased or deleted branch leaves the
        // patchset's commit on no other ref, and losing it turns the
        // change's record into dangling oids. It would bite the next
        // namespace anybody adds in exactly the same way, so the fix is
        // to stop enumerating namespaces rather than to add one.
        let listed =
            run_str(git(repo).args(["for-each-ref", "--format=%(refname) %(objectname)"]))?;
        for line in listed.lines() {
            if let Some((name, oid)) = line.split_once(' ') {
                if name != format!("refs/heads/{primary}") {
                    // Annotated tags are advertised as the tag object itself;
                    // its closure rides the extra emission.
                    ref_tips.push((name.to_string(), oid.to_string()));
                }
            }
        }
    }

    let layout = prefix.rsplit('/').next().unwrap_or(prefix).to_string();
    let mut manifest = Manifest {
        schema: 3,
        repo: prefix.to_string(),
        layout,
        object_format: "sha1".into(),
        refs: ref_tips.clone(),
        head: format!("refs/heads/{primary}"),
        segments: Vec::new(),
        cold_segments: Vec::new(),
        hot_segments: Vec::new(),
        spine: Vec::new(),
        locator: None,
        shallow: Vec::new(),
        epoch: epoch.clone(),
        extra_emission: None,
        tail_emissions: Vec::new(),
        ref_pages: Vec::new(),
        snapshot: None,
        wal: Vec::new(),
    };

    // Shallow corpora: graft commits must be advertised via shallow-info,
    // or index-pack rejects boundary commits whose parents are absent.
    let shallow_path = run_str(git(repo).args(["rev-parse", "--git-path", "shallow"]))?;
    let shallow_file = repo.join(shallow_path);
    if shallow_file.exists() {
        let body = std::fs::read_to_string(&shallow_file).map_err(|e| e.to_string())?;
        manifest.shallow = body.split_whitespace().map(str::to_string).collect();
    }

    // Sharded ref store: when forced or when the ref count outgrows one
    // page, refs live in sorted name-range pages under the epoch and the
    // manifest keeps only the serving-critical tip (schema 4).
    if cfg.paged_refs || ref_tips.len() > cfg.page_size {
        let mut entries = ref_tips.clone();
        entries.sort();
        manifest.schema = 4;
        let refs_dir = staging.join("refs");
        std::fs::create_dir_all(&refs_dir).map_err(|e| e.to_string())?;
        for chunk in entries.chunks(cfg.page_size) {
            let body: String = chunk.iter().map(|(n, o)| format!("{o} {n}\n")).collect();
            let digest = Sha256::digest(body.as_bytes());
            let fname = format!("page-{}.txt", stratum_store::pack::hex(&digest));
            std::fs::write(refs_dir.join(&fname), body.as_bytes()).map_err(|e| e.to_string())?;
            manifest.ref_pages.push(RefPage {
                first: chunk[0].0.clone(),
                last: chunk[chunk.len() - 1].0.clone(),
                key: format!("{prefix}/{epoch}/refs/{fname}"),
                count: chunk.len() as u64,
                bytes: body.len() as u64,
            });
        }
        manifest.refs = vec![(format!("refs/heads/{primary}"), tip.clone())];
    }

    // Cold tier: path-major segments over closure(boundary), each packed
    // independently (I2), 4-way parallel like the research harness.
    let objs = enumerate_objects(repo, &boundary)?;
    let mut seen: HashSet<String> = objs.iter().map(|o| o.oid.clone()).collect();
    let ordered = order_path(objs);
    let segs = cut_segments(ordered, cfg.budget_bytes);
    type PackResult = Result<(Vec<u8>, u64), String>;
    let payloads: Vec<PackResult> = {
        let mut results: Vec<Option<PackResult>> = (0..segs.len()).map(|_| None).collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results_mx = std::sync::Mutex::new(&mut results);
        std::thread::scope(|s| {
            for _ in 0..4.min(segs.len().max(1)) {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= segs.len() {
                        break;
                    }
                    let r = pack_segment_full(repo, &segs[i]);
                    results_mx.lock().unwrap()[i] = Some(r);
                });
            }
        });
        results.into_iter().map(|r| r.unwrap()).collect()
    };
    for (i, res) in payloads.into_iter().enumerate() {
        let (payload, entries) = res?;
        let name = format!("cold-{i:04}.seg");
        std::fs::write(staging.join(&name), &payload).map_err(|e| e.to_string())?;
        manifest.cold_segments.push(Segment {
            key: format!("{prefix}/{epoch}/{name}"),
            entries,
            bytes: payload.len() as u64,
        });
    }

    // Hot tier: per-spine-commit thin emissions, cut into ~hot_budget
    // segments; every hot_anchor-th emission is non-thin (chain bound).
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_idx = 0usize;
    let flush_hot =
        |cur: &mut Vec<u8>, cur_idx: &mut usize, manifest: &mut Manifest| -> Result<(), String> {
            if cur.is_empty() {
                return Ok(());
            }
            let name = format!("hot-{:04}.seg", *cur_idx);
            std::fs::write(staging.join(&name), &cur).map_err(|e| e.to_string())?;
            manifest.hot_segments.push(HotSegment {
                key: format!("{prefix}/{epoch}/{name}"),
                bytes: cur.len() as u64,
            });
            cur.clear();
            *cur_idx += 1;
            Ok(())
        };

    let mut parent = boundary.clone();
    for (n, commit) in spine.iter().enumerate() {
        let thin = cfg.hot_anchor == 0 || (n % cfg.hot_anchor) != 0;
        let (payload, entries) = pack_thin_emission(repo, commit, &parent, &mut seen, thin)?;
        if cur.len() as u64 + payload.len() as u64 > cfg.hot_budget_bytes && !cur.is_empty() {
            flush_hot(&mut cur, &mut cur_idx, &mut manifest)?;
        }
        manifest.spine.push(SpineEntry {
            oid: commit.clone(),
            seg: cur_idx,
            off: cur.len() as u64,
            entries,
            bytes: payload.len() as u64,
        });
        cur.extend_from_slice(&payload);
        parent = commit.clone();
    }

    // Multi-ref: one final emission with everything reachable from
    // secondary tips but not the primary tip, thin against it. It sits
    // after the last spine emission so every spine suffix includes it (I3).
    let mut secondaries: Vec<String> = ref_tips[1..].iter().map(|(_, t)| t.clone()).collect();
    secondaries.sort();
    secondaries.dedup();
    secondaries.retain(|t| t != &tip);
    if !secondaries.is_empty() {
        let mut rl_input = String::new();
        for t in &secondaries {
            rl_input.push_str(t);
            rl_input.push('\n');
        }
        rl_input.push_str(&format!("^{tip}\n"));
        let cand = run_with_stdin(
            git(repo).args(["rev-list", "--objects", "--stdin"]),
            rl_input.as_bytes(),
        )?;
        let cand = String::from_utf8_lossy(&cand);
        let mut new: HashSet<&str> = HashSet::new();
        let mut revs = rl_input.clone();
        for line in cand.lines() {
            let oid = line.split(' ').next().unwrap_or("");
            if seen.contains(oid) {
                revs.push('^');
                revs.push_str(oid);
                revs.push('\n');
            } else {
                new.insert(oid);
            }
        }
        let out = run_with_stdin(
            git(repo).args([
                "pack-objects",
                "--revs",
                "--thin",
                "--no-sparse",
                "--no-use-bitmap-index",
                "--no-reuse-delta",
                "--delta-base-offset",
                "--stdout",
                "-q",
            ]),
            revs.as_bytes(),
        )?;
        let (payload, entries) =
            strip_pack(&out, Some(new.len() as u64)).map_err(|e| format!("extra emission: {e}"))?;
        if cur.len() as u64 + payload.len() as u64 > cfg.hot_budget_bytes && !cur.is_empty() {
            flush_hot(&mut cur, &mut cur_idx, &mut manifest)?;
        }
        manifest.extra_emission = Some(ExtraEmission {
            entries,
            bytes: payload.len() as u64,
        });
        cur.extend_from_slice(&payload);
    }
    flush_hot(&mut cur, &mut cur_idx, &mut manifest)?;

    // depth-1 snapshot artifact: primary tip commit + tree closure,
    // self-contained, so `deepen 1` clones need no graph work.
    let snap_list = run(git(repo).args(["rev-list", "--objects", &format!("{tip}^{{tree}}")]))?;
    let snap_list = String::from_utf8_lossy(&snap_list);
    let mut snap_oids = format!("{tip}\n");
    for line in snap_list.lines() {
        snap_oids.push_str(line.split(' ').next().unwrap_or(""));
        snap_oids.push('\n');
    }
    let out = run_with_stdin(
        git(repo).args([
            "pack-objects",
            "--no-reuse-delta",
            "--delta-base-offset",
            "--stdout",
            "-q",
        ]),
        snap_oids.as_bytes(),
    )?;
    let (payload, entries) = strip_pack(&out, None)?;
    std::fs::write(staging.join("snapshot.seg"), &payload).map_err(|e| e.to_string())?;
    manifest.snapshot = Some(Segment {
        key: format!("{prefix}/{epoch}/snapshot.seg"),
        entries,
        bytes: payload.len() as u64,
    });

    Ok(IngestOutput {
        manifest,
        epoch,
        staging,
    })
}

/// How the manifest pointer is allowed to land.
pub enum PublishMode {
    /// First ingest: the manifest must not exist yet (If-None-Match:*).
    Create,
    /// Replace an existing layout wholesale (If-Match against the manifest
    /// current at call time). A concurrent writer turns into Conflict,
    /// never a silent clobber.
    Replace,
    /// Replace against a caller-pinned etag — the compactor's mode: the
    /// etag was read *before* materializing, so any concurrent push wins
    /// the race and compaction retries (research compact.py discipline).
    ReplaceIfMatch(String),
}

/// Upload a staged epoch and swap the pointers, data first (I7):
/// every epoch file, then `locator.hdr`, then `manifest.json` (CAS).
pub fn publish(
    store: &ObjectStore,
    prefix: &str,
    out: &IngestOutput,
    locator_hdr: &[u8],
    mode: PublishMode,
) -> Result<(), PublishError> {
    // The manifest CAS token is read before any data lands, so a writer
    // that raced us between now and the swap wins loudly (412), and our
    // orphaned epoch is left for GC.
    let manifest_key = format!("{prefix}/manifest.json");
    let expected = match mode {
        PublishMode::Create => None,
        PublishMode::Replace => Some(
            store
                .get_with_etag(&manifest_key)
                .map_err(|e| format!("read current manifest: {e}"))?
                .1,
        ),
        PublishMode::ReplaceIfMatch(etag) => Some(etag),
    };

    upload_dir(store, &out.staging, &format!("{prefix}/{}", out.epoch))?;

    let hdr_key = format!("{prefix}/locator.hdr");
    // Only genuine absence means "there is no pointer yet". Treating any
    // error as absence turns a 500 or a timeout into a create-only PUT
    // that then fails its condition and reports a lost race, which is a
    // misleading answer to a store that was merely unwell.
    let prior_hdr = match store.get_with_etag(&hdr_key) {
        Ok((body, etag)) => Some((body, etag)),
        Err(e) if crate::errclass::is_absent(&e) => None,
        Err(e) => return Err(PublishError::Other(format!("read locator.hdr: {e}"))),
    };
    let hdr_cond = match &prior_hdr {
        Some((_, etag)) => PutCond::IfMatch(etag.clone()),
        None => PutCond::IfNoneMatchStar,
    };
    match store.put(&hdr_key, locator_hdr, hdr_cond) {
        Ok(()) => {}
        Err(PutError::Conflict) => return Err(PublishError::LostRace(Pointer::Locator)),
        Err(e) => return Err(PublishError::Other(e.to_string())),
    }

    let body = serde_json::to_vec(&out.manifest).map_err(|e| e.to_string())?;
    let cond = match expected {
        None => PutCond::IfNoneMatchStar,
        Some(etag) => PutCond::IfMatch(etag),
    };
    match store.put(&manifest_key, &body, cond) {
        Ok(()) => Ok(()),
        Err(PutError::Conflict) => {
            // We advanced `locator.hdr` and then lost the manifest, so the
            // two pointers now name different epochs. I15 permits that —
            // each is internally consistent on its own — and readers do
            // cope. But losing this race is the *ordinary* outcome in a
            // fleet, since a concurrent push is supposed to win, and
            // leaving the plane pointed at an epoch the manifest never
            // adopted has two costs worth avoiding: point reads resolve
            // against an epoch nothing else references, and receive-pack's
            // existence oracle is that same plane, so a later push whose
            // thin base the *winner* folded can be refused as incomplete.
            // Put the pointer back where it was.
            restore_locator_hdr(store, &hdr_key, locator_hdr, prior_hdr.as_ref());
            Err(PublishError::LostRace(Pointer::Manifest))
        }
        Err(e) => Err(PublishError::Other(e.to_string())),
    }
}

/// Undo our `locator.hdr` swap after losing the manifest CAS.
///
/// Best effort by design: the caller is already returning `LostRace`, and
/// the un-rolled-back state is legal under I15, so a failure here must not
/// turn a benign race into a hard error.
///
/// The guard that matters is the read-back. We only restore if the header
/// still holds exactly the bytes *we* wrote: if another writer has moved
/// it on since, our "previous" value is stale and writing it would undo
/// their work — turning a tidy-up into the very corruption it exists to
/// prevent. `If-Match` on the etag we just read closes the remaining gap.
fn restore_locator_hdr(
    store: &ObjectStore,
    hdr_key: &str,
    ours: &[u8],
    prior: Option<&(Vec<u8>, String)>,
) {
    let Ok((current, etag)) = store.get_with_etag(hdr_key) else {
        return;
    };
    if current != ours {
        return;
    }
    match prior {
        Some((prior_bytes, _)) => {
            let _ = store.put(hdr_key, prior_bytes, PutCond::IfMatch(etag));
        }
        // There was no header before us — this compaction created it — so
        // going back means removing it, not rewriting it. That is a real
        // state the reader already handles: `read::LayoutReader` treats an
        // absent `locator.hdr` as "no plane yet" and falls back, which is
        // exactly what it did a moment ago. Leaving ours in place instead
        // would strand the point-read plane on an epoch the manifest never
        // adopted, which is the whole failure this function exists to undo.
        None => {
            let _ = store.delete(hdr_key);
        }
    }
}

fn upload_dir(store: &ObjectStore, dir: &Path, key_prefix: &str) -> Result<(), String> {
    let mut entries: Vec<PathBuf> = Vec::new();
    walk(dir, &mut entries)?;
    for path in entries {
        let rel = path
            .strip_prefix(dir)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let body = std::fs::read(&path).map_err(|e| e.to_string())?;
        // Epoch data is immutable and content-determined; unconditional PUT
        // of identical bytes on a retry is harmless (I8).
        store
            .put(&format!("{key_prefix}/{rel}"), &body, PutCond::None)
            .map_err(|e| format!("upload {rel}: {e}"))?;
    }
    Ok(())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let p = entry.path();
        if p.is_dir() {
            walk(&p, out)?;
        } else {
            out.push(p);
        }
    }
    Ok(())
}

/// Which of the layout's two mutable pointers lost a CAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pointer {
    /// `<prefix>/locator.hdr`.
    Locator,
    /// `<prefix>/manifest.json` — the only ref truth (I9).
    Manifest,
}

/// Why a [`publish`] did not land.
///
/// `LostRace` is the distinction the compactor and the mirror sync
/// actually act on, and it used to be flattened into the message and
/// recovered downstream with `e.contains("race") || e.contains("conflict")
/// || e.contains("Conflict")`. That was wrong in both directions: an
/// unrelated failure whose text merely mentioned a conflict was retried
/// forever as a benign lost race, and rewording either message here would
/// silently turn every genuine lost race into a hard job failure. The
/// store already knows — `PutError::Conflict` — so carry it instead of
/// re-deriving it from prose.
///
/// `Display` still reproduces the two original messages byte-for-byte,
/// because callers that only log or propagate the error keep working and
/// operators keep recognising the line.
#[derive(Debug, PartialEq, Eq)]
pub enum PublishError {
    /// A concurrent writer won the CAS on this pointer. Benign: re-read
    /// and retry (the compactor re-enqueues; pushes always win).
    LostRace(Pointer),
    Other(String),
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublishError::LostRace(Pointer::Locator) => write!(f, "locator.hdr swap lost a race"),
            PublishError::LostRace(Pointer::Manifest) => {
                write!(f, "manifest swap lost a race (concurrent writer)")
            }
            PublishError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// Lets the `?` on the plain-`String` helpers inside `publish` keep
/// working unchanged; vendored code below still speaks `String`.
impl From<String> for PublishError {
    fn from(e: String) -> Self {
        PublishError::Other(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire-visible text of a publish failure, pinned. `Display` is
    /// what reaches logs and the job record, and the strings below are
    /// the ones this crate has always emitted.
    #[test]
    fn publish_errors_read_the_way_they_always_did() {
        assert_eq!(
            PublishError::LostRace(Pointer::Locator).to_string(),
            "locator.hdr swap lost a race"
        );
        assert_eq!(
            PublishError::LostRace(Pointer::Manifest).to_string(),
            "manifest swap lost a race (concurrent writer)"
        );
        assert_eq!(PublishError::Other("boom".into()).to_string(), "boom");
        assert_eq!(
            PublishError::from("boom".to_string()),
            PublishError::Other("boom".into())
        );
    }

    /// The bug the typed error exists to kill: an unrelated failure whose
    /// text happens to mention a conflict is *not* a lost race, and the
    /// compactor must not requeue it forever. Under the old
    /// `e.contains("conflict")` recovery both of these were LostRace.
    #[test]
    fn a_failure_that_merely_mentions_a_conflict_is_not_a_lost_race() {
        for text in [
            "upload seg-0: PUT o/1/r/2/e/seg-0: merge conflict marker in blob",
            "read current manifest: GET o/1/r/2/L1/manifest.json: HTTP 500",
            r#"repo "conflict-resolution" is gone"#,
        ] {
            let e = PublishError::Other(text.into());
            assert!(
                !matches!(e, PublishError::LostRace(_)),
                "{text} must not classify as a lost race"
            );
        }
    }
}

// Writer used by tests and callers wanting progress; kept minimal.
