//! The point-read plane as a library: locator header/bucket lookups and
//! whole-object resolution against segment storage. Used by stratum-cat
//! (CLI reads) and by the receive path (thin-base fetches + existence
//! checks during push verification).

use crate::pack::{apply_delta, hex, resolve, Resolved};
use crate::store::ObjectStore;
use std::io::Read;

pub const RECORD: usize = 150; // 54-byte core (u32 seg) + four inline 24-byte hops
pub const HOP: usize = 24;
const COALESCE_GAP: u64 = 128 * 1024;

pub struct Plane {
    /// "<repo>/<layout>/<epoch>" — immutable data keys live here.
    pub data_prefix: String,
    pub n_cold: u64,
    /// Locator generation within the epoch (SLH3). None = legacy SLH2
    /// naming (locator.bin / chains.bin). Incremental compaction writes a
    /// new generation and swaps the hdr; older generations stay readable.
    pub generation: Option<u32>,
    buckets: Vec<u64>,
}

pub struct Record {
    pub seg: u64,
    pub off: u64,
    pub len: u64,
    pub span: u64,
    pub chain_off: u64,
    pub chain_cnt: u64,
    inline: Vec<u8>,
}

/// A parsed `locator.hdr`, independent of who is reading it.
///
/// STRATUM-CORE DIVERGENCE: the research repo parses this header inline
/// inside `Plane::load` and nowhere else. Zero-copy forks gave the format
/// a second and third reader — the fork writer, which re-points a copied
/// header at another repository's data, and epoch GC, which needs the
/// epoch and has to know whether the data prefix is even local. Three
/// hand-rolled decoders of one byte format is how the three drift apart,
/// so the decode lives here once and the callers share it.
#[derive(Debug, PartialEq, Eq)]
pub struct LocatorHeader {
    pub epoch: String,
    /// Locator generation within the epoch. `None` is the legacy SLH2
    /// naming (`locator.bin` / `chains.bin`).
    pub generation: Option<u32>,
    /// SLH4 only: an **absolute** data prefix, which is the whole point
    /// of the magic. `None` means the data lives under the reader's own
    /// prefix, as SLH2 and SLH3 have always assumed.
    pub data_prefix: Option<String>,
    pub records: u64,
    pub n_cold: u64,
    pub buckets: Vec<u64>,
}

/// Decode a `locator.hdr`. SLH2 and SLH3 decode exactly as they always
/// have; SLH4 adds a length-prefixed absolute data prefix after the
/// generation.
pub fn parse_header(hdr: &[u8]) -> Result<LocatorHeader, String> {
    let (generational, absolute) = match hdr.get(..4) {
        Some(b"SLH2") => (false, false),
        Some(b"SLH3") => (true, false),
        Some(b"SLH4") => (true, true),
        _ => return Err("locator.hdr: bad magic (stale format? re-run build_locator)".into()),
    };
    if hdr.len() < 6 {
        return Err("locator.hdr truncated".into());
    }
    let elen = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
    let mut at = 6 + elen;
    // SLH2 has no generation and SLH3 always has one, so for those two
    // the magic alone says whether the field is there. SLH4 has to carry
    // a flag instead: a fork of a *legacy* SLH2 repository is SLH4 —
    // absolute data prefix — with no generation, because upstream's data
    // files are still named locator.bin / chains.bin. Deriving presence
    // from the magic would misread that header by four bytes and send
    // every point read to a garbage key.
    let has_generation = if absolute {
        let flag = *hdr.get(at).ok_or("locator.hdr truncated")?;
        at += 1;
        match flag {
            0 => false,
            1 => true,
            _ => return Err("locator.hdr: SLH4 generation flag is not 0 or 1".into()),
        }
    } else {
        generational
    };
    let generation = if has_generation {
        let g = u32::from_be_bytes(
            hdr.get(at..at + 4)
                .ok_or("locator.hdr truncated")?
                .try_into()
                .unwrap(),
        );
        at += 4;
        Some(g)
    } else {
        None
    };
    let data_prefix = if absolute {
        let raw = hdr.get(at..at + 2).ok_or("locator.hdr truncated")?;
        let plen = u16::from_be_bytes([raw[0], raw[1]]) as usize;
        at += 2;
        let bytes = hdr.get(at..at + plen).ok_or("locator.hdr truncated")?;
        at += plen;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| "locator.hdr: data prefix is not UTF-8".to_string())?;
        // An empty prefix would silently resolve every data key to a
        // bare "/cold-0000.seg" at the bucket root. Refuse it: a reader
        // that sees a header it cannot honour must fail loudly.
        if text.is_empty() {
            return Err("locator.hdr: SLH4 with an empty data prefix".into());
        }
        Some(text.trim_end_matches('/').to_string())
    } else {
        None
    };
    let fixed = at + 16;
    if hdr.len() < fixed + 4097 * 8 {
        return Err("locator.hdr truncated".into());
    }
    Ok(LocatorHeader {
        epoch: String::from_utf8_lossy(&hdr[6..6 + elen]).to_string(),
        generation,
        data_prefix,
        records: u64::from_be_bytes(hdr[at..at + 8].try_into().unwrap()),
        n_cold: u64::from_be_bytes(hdr[at + 8..at + 16].try_into().unwrap()),
        // STRATUM-CORE DIVERGENCE: chunks_exact -> as_chunks (newer
        // clippy denies constant-size chunks_exact); behavior identical.
        buckets: hdr[fixed..fixed + 4097 * 8]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_be_bytes(*c))
            .collect(),
    })
}

/// Encode a `locator.hdr`. The magic follows the content: an absolute
/// data prefix means SLH4, a generation alone means SLH3, neither means
/// SLH2 — so a header decoded and re-encoded is byte-identical.
pub fn write_header(h: &LocatorHeader) -> Vec<u8> {
    let eb = h.epoch.as_bytes();
    let mut out = Vec::with_capacity(4 + 2 + eb.len() + 6 + 16 + 4097 * 8);
    out.extend_from_slice(match (&h.data_prefix, h.generation) {
        (Some(_), _) => b"SLH4",
        (None, Some(_)) => b"SLH3",
        (None, None) => b"SLH2",
    });
    out.extend_from_slice(&(eb.len() as u16).to_be_bytes());
    out.extend_from_slice(eb);
    // SLH4 states generation presence explicitly; see parse_header.
    if h.data_prefix.is_some() {
        out.push(u8::from(h.generation.is_some()));
    }
    if let Some(g) = h.generation {
        out.extend_from_slice(&g.to_be_bytes());
    }
    if let Some(p) = &h.data_prefix {
        out.extend_from_slice(&(p.len() as u16).to_be_bytes());
        out.extend_from_slice(p.as_bytes());
    }
    out.extend_from_slice(&h.records.to_be_bytes());
    out.extend_from_slice(&h.n_cold.to_be_bytes());
    for b in &h.buckets {
        out.extend_from_slice(&b.to_be_bytes());
    }
    out
}

/// Re-point a copied `locator.hdr` at somebody else's data.
///
/// This is the whole of a zero-copy fork's read path: upstream's header,
/// byte-copied into the fork's prefix with the data prefix made
/// absolute, so the fork's point reads resolve to upstream's immutable
/// objects instead of to keys under the fork that do not exist. Every
/// manifest segment key is already absolute, which is why only this one
/// pointer had to change.
///
/// The caller must have registered its `epoch_refs` reference **before**
/// publishing the header this returns — see the write-ordering argument
/// in the fork worker.
pub fn rebase_header(hdr: &[u8], data_prefix: &str) -> Result<Vec<u8>, String> {
    let mut parsed = parse_header(hdr)?;
    let trimmed = data_prefix.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("rebase_header: empty data prefix".into());
    }
    parsed.data_prefix = Some(trimmed.to_string());
    Ok(write_header(&parsed))
}

impl Plane {
    /// Load the plane from its atomic pointer object `<prefix>/locator.hdr`.
    /// SLH3 adds a u32 generation after the epoch (generation-named data
    /// files); SLH2 is the legacy generation-less form. SLH4 additionally
    /// carries an absolute data prefix, so the plane can point out of the
    /// prefix it was loaded from — a zero-copy fork reading upstream.
    pub fn load(locator_store: &ObjectStore, prefix: &str) -> Result<Plane, String> {
        let hdr = locator_store.get(&format!("{prefix}/locator.hdr"))?;
        let parsed = parse_header(&hdr)?;
        Ok(Plane {
            // The caller's prefix is the fallback, not the truth: SLH4
            // says where the bytes are, and for a fork that is not here.
            data_prefix: match parsed.data_prefix {
                Some(p) => p,
                None => format!("{prefix}/{}", parsed.epoch),
            },
            n_cold: parsed.n_cold,
            generation: parsed.generation,
            buckets: parsed.buckets,
        })
    }

    fn locator_key(&self) -> String {
        match self.generation {
            Some(g) => format!("{}/locator-g{g:04}.bin", self.data_prefix),
            None => format!("{}/locator.bin", self.data_prefix),
        }
    }

    pub fn chains_key(&self) -> String {
        match self.generation {
            Some(g) => format!("{}/chains-g{g:04}.bin", self.data_prefix),
            None => format!("{}/chains.bin", self.data_prefix),
        }
    }

    pub fn seg_key(&self, seg: u64) -> String {
        if seg < self.n_cold {
            format!("{}/cold-{seg:04}.seg", self.data_prefix)
        } else {
            format!("{}/hot-{:04}.seg", self.data_prefix, seg - self.n_cold)
        }
    }

    /// Locator lookup: one bucket range GET, scan for the record.
    pub fn lookup(
        &self,
        locator_store: &ObjectStore,
        oid_bytes: &[u8; 20],
    ) -> Result<Option<Record>, String> {
        let bucket = ((oid_bytes[0] as usize) << 4) | ((oid_bytes[1] as usize) >> 4);
        let (from, to) = (self.buckets[bucket], self.buckets[bucket + 1]);
        if from == to {
            return Ok(None);
        }
        let key = self.locator_key();
        let mut slice = Vec::new();
        read_into(locator_store, &key, Some((from, to - 1)), &mut slice)?;
        // STRATUM-CORE DIVERGENCE: chunks_exact -> as_chunks, as above.
        let rec = match slice
            .as_chunks::<RECORD>()
            .0
            .iter()
            .find(|r| r[..20] == oid_bytes[..])
        {
            Some(r) => r,
            None => return Ok(None),
        };
        Ok(Some(Record {
            seg: u32::from_be_bytes(rec[20..24].try_into().unwrap()) as u64,
            off: u64::from_be_bytes(rec[24..32].try_into().unwrap()),
            len: u32::from_be_bytes(rec[32..36].try_into().unwrap()) as u64,
            span: u64::from_be_bytes(rec[36..44].try_into().unwrap()),
            chain_off: u64::from_be_bytes(rec[44..52].try_into().unwrap()),
            chain_cnt: u16::from_be_bytes([rec[52], rec[53]]) as u64,
            inline: rec[54..].to_vec(),
        }))
    }

    pub fn parse_oid(oid: &str) -> Result<[u8; 20], String> {
        if oid.len() != 40 {
            return Err("bad oid".into());
        }
        let mut out = [0u8; 20];
        for i in 0..20 {
            out[i] = u8::from_str_radix(&oid[i * 2..i * 2 + 2], 16)
                .map_err(|_| "bad oid".to_string())?;
        }
        Ok(out)
    }

    /// Resolve one object to (type, bytes, data-GETs-used). Chained hot-tier
    /// entries fetch their precomputed hop plan and read all ranges
    /// coalesced, ≤ 8 in parallel.
    pub fn read_object(
        &self,
        store: &ObjectStore,
        locator_store: &ObjectStore,
        oid: &str,
    ) -> Result<(u8, Vec<u8>, u64), String> {
        let oid_bytes = Self::parse_oid(oid)?;
        let rec = self
            .lookup(locator_store, &oid_bytes)?
            .ok_or_else(|| format!("{oid}: not in locator"))?;

        if rec.chain_cnt == 0 {
            let mut data = Vec::with_capacity((rec.off + rec.len - rec.span) as usize);
            read_into(
                store,
                &self.seg_key(rec.seg),
                Some((rec.span, rec.off + rec.len - 1)),
                &mut data,
            )?;
            return match resolve(&data, rec.span, rec.off)? {
                Resolved::Object(t, bytes) => Ok((t, bytes, 2)),
                Resolved::External(base, _) => {
                    Err(format!("{oid}: unchained entry hit external base {base}"))
                }
            };
        }

        let mut extra_get = 0;
        let plan_bytes: Vec<u8> = if rec.chain_cnt <= 4 {
            rec.inline[..(rec.chain_cnt as usize) * HOP].to_vec()
        } else {
            let mut p = Vec::new();
            read_into(
                locator_store,
                &self.chains_key(),
                Some((
                    rec.chain_off,
                    rec.chain_off + rec.chain_cnt * HOP as u64 - 1,
                )),
                &mut p,
            )?;
            extra_get = 1;
            p
        };
        // STRATUM-CORE DIVERGENCE: chunks_exact -> as_chunks, as above.
        let hops: Vec<(u64, u64, u64, u64)> = plan_bytes
            .as_chunks::<HOP>()
            .0
            .iter()
            .map(|h| {
                (
                    u32::from_be_bytes(h[0..4].try_into().unwrap()) as u64,
                    u64::from_be_bytes(h[4..12].try_into().unwrap()),
                    u32::from_be_bytes(h[12..16].try_into().unwrap()) as u64,
                    u64::from_be_bytes(h[16..24].try_into().unwrap()),
                )
            })
            .collect();

        let mut want_ranges: Vec<(u64, u64, u64)> =
            hops.iter().map(|&(s, a, l, _)| (s, a, a + l)).collect();
        want_ranges.sort();
        let mut merged: Vec<(u64, u64, u64)> = Vec::new();
        for (s, a, b) in want_ranges {
            match merged.last_mut() {
                Some((ms, _, mb)) if *ms == s && a <= *mb + COALESCE_GAP => *mb = (*mb).max(b),
                _ => merged.push((s, a, b)),
            }
        }

        let mut fetched: Vec<(u64, u64, Vec<u8>)> = Vec::with_capacity(merged.len());
        for batch in merged.chunks(8) {
            let part: Vec<(u64, u64, Vec<u8>)> = std::thread::scope(|scope| {
                let handles: Vec<_> = batch
                    .iter()
                    .map(|&(s, a, b)| {
                        let store = &store;
                        let me = &self;
                        scope.spawn(move || -> Result<(u64, u64, Vec<u8>), String> {
                            let mut buf = Vec::with_capacity((b - a) as usize);
                            read_into(store, &me.seg_key(s), Some((a, b - 1)), &mut buf)?;
                            Ok((s, a, buf))
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().map_err(|_| "fetch thread panicked".to_string())?)
                    .collect::<Result<Vec<_>, String>>()
            })?;
            fetched.extend(part);
        }
        let gets = 1 + extra_get + fetched.len() as u64;

        let find = |s: u64, at: u64| -> Result<&(u64, u64, Vec<u8>), String> {
            fetched
                .iter()
                .find(|(fs, fa, d)| *fs == s && *fa <= at && at < fa + d.len() as u64)
                .ok_or_else(|| format!("no fetched range covers seg {s} @ {at}"))
        };

        let mut cur: Option<(u8, Vec<u8>)> = None;
        for &(s, _, _, entry) in &hops {
            let (_, fa, data) = find(s, entry)?;
            match resolve(data, *fa, entry)? {
                Resolved::Object(t, bytes) => cur = Some((t, bytes)),
                Resolved::External(_, stack) => {
                    let (t, mut obj) = cur.take().ok_or("chain starts at external base")?;
                    for delta in stack {
                        obj = apply_delta(&obj, &delta)?;
                    }
                    cur = Some((t, obj));
                }
            }
        }
        let (t, obj) = cur.ok_or("empty chain")?;
        let _ = hex(&oid_bytes); // (kept for symmetry with callers' verification)
        Ok((t, obj, gets))
    }
}

fn read_into(
    store: &ObjectStore,
    key: &str,
    range: Option<(u64, u64)>,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    store
        .get_stream(key, range)?
        .read_to_end(out)
        .map_err(|e| format!("{key}: {e}"))?;
    Ok(())
}
