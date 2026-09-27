//! Build the locator plane for a staged epoch: the sorted 150-byte-record
//! oid table, the chains.bin hop plans, and the SLH3 header pointer.
//!
//! Faithful Rust port of the research repo's `bench/build_locator.py`.
//! Chain plans are complete at build time (I14): a record's plan covers the
//! entire delta resolution — span ranges include every intra-emission OFS
//! base, hops cover every REF root transitively — so the reader applies,
//! it never discovers.

use crate::gitcmd::{run_with_stdin, scrub};
use crate::ingest::IngestOutput;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::path::Path;
use stratum_store::gitobj;
use stratum_store::manifest::Locator;
use stratum_store::pack::{entry_header, hex, ofs_delta_distance};

const INLINE_HOPS: usize = 4;
const HOP_BYTES: usize = 24; // >IQIQ: seg u32, start u64, len u32, entry u64
const RECORD_BYTES: usize = 54 + INLINE_HOPS * HOP_BYTES; // 150

/// (segment, range start, range len, entry offset) — one hop of a chain
/// plan, root first.
#[derive(Clone, Copy)]
struct Hop {
    seg: u32,
    start: u64,
    len: u32,
    entry: u64,
}

struct Info {
    seg: u32,
    off: u64,
    len: u32,
    span: u64,
    root: Option<String>,
}

/// Build the locator for `out`'s staged epoch, writing
/// `locator-g{generation:04}.bin` / `chains-g{generation:04}.bin` into the
/// staging dir, updating `manifest.locator`, and returning the bytes of the
/// `locator.hdr` pointer (published last, after the data — I7/I15).
///
/// `repo` provides the odb that resolves thin REF bases during indexing —
/// for a fresh ingest, the ingested repo itself.
pub fn build_locator(
    repo: &Path,
    out: &mut IngestOutput,
    prefix: &str,
    generation: u32,
) -> Result<Vec<u8>, String> {
    let manifest = &out.manifest;
    let epoch = &out.epoch;
    let tails = manifest.tails();

    // Global segment order: cold then hot (matches plane.rs seg_key).
    let mut seg_specs: Vec<(String, u64)> = Vec::new(); // (staged file name, entries)
    for s in &manifest.cold_segments {
        let name = s.key.rsplit('/').next().unwrap().to_string();
        seg_specs.push((name, s.entries));
    }
    for (hot_idx, s) in manifest.hot_segments.iter().enumerate() {
        let name = s.key.rsplit('/').next().unwrap().to_string();
        let entries: u64 = manifest
            .spine
            .iter()
            .filter(|sp| sp.seg == hot_idx)
            .map(|sp| sp.entries)
            .sum::<u64>()
            + tails
                .iter()
                .filter(|t| t.seg == hot_idx)
                .map(|t| t.entries)
                .sum::<u64>();
        seg_specs.push((name, entries));
    }

    let mut info: HashMap<String, Info> = HashMap::new();
    let tmp = tempdir()?;
    for (seg_idx, (name, entries)) in seg_specs.iter().enumerate() {
        let payload = std::fs::read(out.staging.join(name)).map_err(|e| e.to_string())?;
        let by_off = index_segment(repo, &payload, *entries, &tmp, name)?;
        let (spans, roots) = compute_spans(&payload, &by_off)?;
        let mut ordered: Vec<u64> = by_off.keys().copied().collect();
        ordered.sort_unstable();
        for (j, &off) in ordered.iter().enumerate() {
            let end = ordered.get(j + 1).copied().unwrap_or(payload.len() as u64);
            info.insert(
                by_off[&off].clone(),
                Info {
                    seg: seg_idx as u32,
                    off,
                    len: (end - off) as u32,
                    span: spans[&off],
                    root: roots[&off].clone(),
                },
            );
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);

    // Transitive chain plans (root first), memoized, iteratively resolved —
    // deep chains blew Python's recursion limit; Rust must not blow the
    // stack either.
    let mut hop_memo: HashMap<String, Vec<Hop>> = HashMap::new();
    let own_hop = |i: &Info| Hop {
        seg: i.seg,
        start: i.span,
        len: (i.off + i.len as u64 - i.span) as u32,
        entry: i.off,
    };
    let mut chains: Vec<u8> = Vec::new();
    let mut records: Vec<[u8; RECORD_BYTES]> = Vec::with_capacity(info.len());

    for (oid, i) in &info {
        let mut chain_off = 0u64;
        let mut chain_cnt = 0u16;
        let mut inline = [0u8; INLINE_HOPS * HOP_BYTES];
        let chained = i
            .root
            .as_ref()
            .map(|r| info.contains_key(r))
            .unwrap_or(false);
        if chained {
            let chain = hops_of(oid, &info, &mut hop_memo, &own_hop);
            chain_cnt = chain.len() as u16;
            if chain.len() <= INLINE_HOPS {
                for (k, h) in chain.iter().enumerate() {
                    pack_hop(&mut inline[k * HOP_BYTES..(k + 1) * HOP_BYTES], h);
                }
            } else {
                chain_off = chains.len() as u64;
                for h in &chain {
                    let mut buf = [0u8; HOP_BYTES];
                    pack_hop(&mut buf, h);
                    chains.extend_from_slice(&buf);
                }
            }
        }
        let mut rec = [0u8; RECORD_BYTES];
        let oid_bin = parse_hex(oid)?;
        rec[..20].copy_from_slice(&oid_bin);
        rec[20..24].copy_from_slice(&i.seg.to_be_bytes());
        rec[24..32].copy_from_slice(&i.off.to_be_bytes());
        rec[32..36].copy_from_slice(&i.len.to_be_bytes());
        rec[36..44].copy_from_slice(&i.span.to_be_bytes());
        rec[44..52].copy_from_slice(&chain_off.to_be_bytes());
        rec[52..54].copy_from_slice(&chain_cnt.to_be_bytes());
        rec[54..].copy_from_slice(&inline);
        records.push(rec);
    }
    records.sort_unstable();

    let loc_name = format!("locator-g{generation:04}.bin");
    let chains_name = format!("chains-g{generation:04}.bin");
    let blob: Vec<u8> = records.iter().flat_map(|r| r.iter().copied()).collect();
    std::fs::write(out.staging.join(&loc_name), &blob).map_err(|e| e.to_string())?;
    std::fs::write(out.staging.join(&chains_name), &chains).map_err(|e| e.to_string())?;

    // 4096-bucket directory: top 12 oid bits -> byte offsets into the
    // locator blob; bucket b covers records [offset[b], offset[b+1]).
    let mut counts = [0u64; 4097];
    for r in &records {
        let bucket = ((r[0] as usize) << 4) | ((r[1] as usize) >> 4);
        counts[bucket + 1] += 1;
    }
    for b in 1..4097 {
        counts[b] += counts[b - 1];
    }
    let n_cold = manifest.cold_segments.len() as u64;
    let eb = epoch.as_bytes();
    let mut hdr = Vec::with_capacity(4 + 2 + eb.len() + 4 + 16 + 4097 * 8);
    hdr.extend_from_slice(b"SLH3");
    hdr.extend_from_slice(&(eb.len() as u16).to_be_bytes());
    hdr.extend_from_slice(eb);
    hdr.extend_from_slice(&generation.to_be_bytes());
    hdr.extend_from_slice(&(records.len() as u64).to_be_bytes());
    hdr.extend_from_slice(&n_cold.to_be_bytes());
    for c in counts {
        hdr.extend_from_slice(&(c * RECORD_BYTES as u64).to_be_bytes());
    }

    out.manifest.locator = Some(Locator {
        key: format!("{prefix}/{epoch}/{loc_name}"),
        hdr_key: format!("{prefix}/locator.hdr"),
        chains_key: format!("{prefix}/{epoch}/{chains_name}"),
        record_bytes: RECORD_BYTES,
        records: records.len() as u64,
    });
    Ok(hdr)
}

fn pack_hop(buf: &mut [u8], h: &Hop) {
    buf[..4].copy_from_slice(&h.seg.to_be_bytes());
    buf[4..12].copy_from_slice(&h.start.to_be_bytes());
    buf[12..16].copy_from_slice(&h.len.to_be_bytes());
    buf[16..24].copy_from_slice(&h.entry.to_be_bytes());
}

/// Chain plan for `oid`, root first, memoized. Iterative: walk the root
/// pointers down, then build plans back up.
fn hops_of(
    oid: &str,
    info: &HashMap<String, Info>,
    memo: &mut HashMap<String, Vec<Hop>>,
    own_hop: &impl Fn(&Info) -> Hop,
) -> Vec<Hop> {
    let mut stack: Vec<String> = Vec::new();
    let mut cur = oid.to_string();
    loop {
        if memo.contains_key(&cur) {
            break;
        }
        stack.push(cur.clone());
        let i = &info[&cur];
        match i.root.as_ref().filter(|r| info.contains_key(*r)) {
            Some(r) => cur = r.clone(),
            None => break,
        }
    }
    while let Some(o) = stack.pop() {
        let i = &info[&o];
        let mut chain = match i.root.as_ref().filter(|r| info.contains_key(*r)) {
            Some(r) => memo[r].clone(),
            None => Vec::new(),
        };
        chain.push(own_hop(i));
        memo.insert(o, chain);
    }
    memo[oid].clone()
}

/// Reconstruct a full pack from a payload and index it against `repo`'s odb
/// (fixing thin refs). Returns payload offset -> oid hex for entries inside
/// the payload (thin-fix appends external bases past the payload; skipped).
fn index_segment(
    repo: &Path,
    payload: &[u8],
    entries: u64,
    tmp: &Path,
    name: &str,
) -> Result<HashMap<u64, String>, String> {
    let mut pack = Vec::with_capacity(payload.len() + 32);
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&(entries as u32).to_be_bytes());
    pack.extend_from_slice(payload);
    let digest = Sha1::digest(&pack);
    pack.extend_from_slice(&digest);

    let idx_path = tmp.join(format!("{name}.idx"));
    let mut c = std::process::Command::new("git");
    c.arg("-C").arg(repo);
    scrub(&mut c);
    c.args(["index-pack", "--stdin", "--fix-thin", "-o"])
        .arg(&idx_path);
    run_with_stdin(&mut c, &pack).map_err(|e| format!("index {name}: {e}"))?;
    let idx = std::fs::read(&idx_path).map_err(|e| format!("read idx for {name}: {e}"))?;

    let pairs = gitobj::parse_idx(&idx)?;
    let mut by_off = HashMap::new();
    for (oid, full_off) in pairs {
        if full_off < 12 {
            return Err(format!("{name}: idx offset {full_off} inside header"));
        }
        let off = full_off - 12;
        if off < payload.len() as u64 {
            by_off.insert(off, hex(&oid));
        }
    }
    Ok(by_off)
}

type Spans = (HashMap<u64, u64>, HashMap<u64, Option<String>>);

/// For each entry offset: the smallest in-segment offset its OFS-delta
/// chain reaches (fetch [span, off+len) and resolve locally), and the REF
/// base oid at the chain's root (None for full objects). Iterative — deep
/// chains must not recurse.
fn compute_spans(payload: &[u8], by_off: &HashMap<u64, String>) -> Result<Spans, String> {
    let mut spans: HashMap<u64, u64> = HashMap::new();
    let mut roots: HashMap<u64, Option<String>> = HashMap::new();
    for &start in by_off.keys() {
        if spans.contains_key(&start) {
            continue;
        }
        let mut stack = vec![start];
        while let Some(off) = stack.last().copied() {
            if spans.contains_key(&off) {
                stack.pop();
                continue;
            }
            let (ofs_base, ref_oid) = entry_meta(payload, off)?;
            match ofs_base {
                None => {
                    spans.insert(off, off);
                    roots.insert(off, ref_oid);
                    stack.pop();
                }
                Some(base) => {
                    if let (Some(&bs), Some(br)) = (spans.get(&base), roots.get(&base)) {
                        spans.insert(off, off.min(bs));
                        roots.insert(off, br.clone());
                        stack.pop();
                    } else {
                        stack.push(base);
                    }
                }
            }
        }
    }
    Ok((spans, roots))
}

/// Entry header at `off` -> (OFS base offset | None, REF base oid | None).
fn entry_meta(payload: &[u8], off: u64) -> Result<(Option<u64>, Option<String>), String> {
    let pos = off as usize;
    let (typ, _size, used) = entry_header(payload, pos)?;
    let after = pos + used;
    match typ {
        7 => {
            let oid = payload
                .get(after..after + 20)
                .ok_or("REF base out of range")?;
            Ok((None, Some(hex(oid))))
        }
        6 => {
            let (dist, _) = ofs_delta_distance(payload, after)?;
            let base = off
                .checked_sub(dist)
                .ok_or_else(|| format!("OFS distance {dist} underflows at {off}"))?;
            Ok((Some(base), None))
        }
        _ => Ok((None, None)),
    }
}

fn parse_hex(oid: &str) -> Result<[u8; 20], String> {
    stratum_store::Plane::parse_oid(oid)
}

fn tempdir() -> Result<std::path::PathBuf, String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("stratum-locator-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&p).map_err(|e| e.to_string())?;
    Ok(p)
}
