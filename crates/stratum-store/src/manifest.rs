use serde::{Deserialize, Serialize};

/// Per-repo manifest: the "one small hot object per repo" of H2. One GET
/// fetches everything needed to plan a clone or a spine fetch.
///
/// Two layouts:
/// - schema 1 ("flat", experiment 003): `segments` only.
/// - schema 2 ("tiered", experiment 004): `cold_segments` (path-major,
///   self-contained) + `hot_segments` (concatenated per-spine-commit thin
///   emissions) + `spine` directory (the degenerate reachability index:
///   closure(spine[i]) = cold ∪ hot bytes up to spine[i+1]'s start).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub repo: String,
    pub layout: String,
    /// Git object format for every OID in this layout ("sha1" today).
    /// Declared explicitly so a SHA-256 layout is recognized and rejected
    /// loudly by builds that only speak SHA-1, instead of misparsing
    /// 32-byte OIDs — see docs/design-notes/sha256-plan.md.
    #[serde(default = "default_object_format")]
    pub object_format: String,
    pub refs: Vec<(String, String)>,
    pub head: String,
    #[serde(default)]
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub cold_segments: Vec<Segment>,
    #[serde(default)]
    pub hot_segments: Vec<HotSegment>,
    #[serde(default)]
    pub spine: Vec<SpineEntry>,
    #[serde(default)]
    pub locator: Option<Locator>,
    /// Graft commits of a shallow corpus; advertised via shallow-info.
    #[serde(default)]
    pub shallow: Vec<String>,
    /// Immutable epoch prefix all data keys live under (schema 3).
    #[serde(default)]
    pub epoch: String,
    /// Multi-ref trailer emission (secondary refs, thin vs the primary
    /// tip); physically the tail of the last hot segment at ingest time.
    /// Schema 3 form — schema 4 generalizes this to `tail_emissions`
    /// (incremental folds add more of them, mid-stream); `tails()`
    /// normalizes both forms.
    #[serde(default)]
    pub extra_emission: Option<ExtraEmission>,
    /// Positioned emissions that must reach every client regardless of
    /// ACK point: secondary-ref closures — the ingest trailer plus each
    /// incremental fold's non-primary part (schema 4). A full compaction
    /// resets this to at most one entry.
    #[serde(default)]
    pub tail_emissions: Vec<TailEmission>,
    /// Sharded ref store (schema 4): sorted, non-overlapping refname
    /// ranges, each an immutable content-addressed page object. Present
    /// when the repo's ref count outgrows the manifest; `refs` then only
    /// carries the tips the serving plans need (HEAD + spine primaries).
    #[serde(default)]
    pub ref_pages: Vec<RefPage>,
    /// depth-1 snapshot artifact: primary tip commit + tree closure,
    /// self-contained.
    #[serde(default)]
    pub snapshot: Option<Segment>,
    /// Per-push WAL entries (Continuity-shaped): thin pack payloads stored
    /// append-only under the epoch, referenced here in push order. The
    /// manifest itself is the CAS'd ref-transaction record.
    #[serde(default)]
    pub wal: Vec<WalEntry>,
}

fn default_object_format() -> String {
    "sha1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalEntry {
    /// Object key of the push's thin pack payload.
    pub key: String,
    /// Object key of its sorted binary oid list (dedup/connectivity aid).
    pub oids_key: String,
    pub entries: u64,
    pub bytes: u64,
    /// Ref transactions in this push: (ref name, old oid, new oid).
    pub updates: Vec<(String, String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraEmission {
    pub entries: u64,
    pub bytes: u64,
}

/// An emission at a fixed position in the hot tier that is not part of
/// any spine entry's count: secondary-ref closure bytes. Suffix plans
/// deliver it explicitly when the ACK byte-suffix starts after it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TailEmission {
    /// Index into `hot_segments`.
    pub seg: usize,
    /// Byte offset within that segment.
    pub off: u64,
    pub entries: u64,
    pub bytes: u64,
}

/// One shard of the ref store: all refs with `first <= name <= last`,
/// stored as sorted "oid refname" lines in an immutable page object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefPage {
    pub first: String,
    pub last: String,
    /// Object key of the page (content-addressed; a push that touches
    /// the page writes a new object and swaps this pointer via the
    /// manifest CAS).
    pub key: String,
    pub count: u64,
    pub bytes: u64,
}

/// The locator plane (H2): a sorted oid -> (segment, offset, len, span)
/// table in one object, with a 4096-bucket byte-offset directory so any
/// record is one range GET away. Built by bench/build_locator.py.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Locator {
    pub key: String,
    #[serde(default)]
    pub hdr_key: String,
    #[serde(default)]
    pub chains_key: String,
    pub record_bytes: usize,
    pub records: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub key: String,
    pub entries: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotSegment {
    pub key: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpineEntry {
    pub oid: String,
    /// Index into `hot_segments` where this commit's emission starts.
    pub seg: usize,
    /// Byte offset of the emission within that segment.
    pub off: u64,
    pub entries: u64,
    pub bytes: u64,
}

/// One piece of a pack stream: an object (or byte range of one) to fetch
/// and forward verbatim.
#[derive(Debug, Clone)]
pub struct StreamPart {
    pub key: String,
    /// Inclusive byte range within the object; None = whole object.
    pub range: Option<(u64, u64)>,
    pub expect_bytes: u64,
}

impl Manifest {
    pub fn tip(&self) -> Option<&str> {
        self.refs
            .iter()
            .find(|(name, _)| name == &self.head)
            .map(|(_, oid)| oid.as_str())
    }

    /// Tail emissions in canonical (schema 4) form. A schema-3 manifest
    /// expresses its single multi-ref trailer as `extra_emission` sitting
    /// at the end of the last hot segment; normalize it here so every
    /// consumer handles one shape.
    pub fn tails(&self) -> Vec<TailEmission> {
        if !self.tail_emissions.is_empty() {
            return self.tail_emissions.clone();
        }
        match (&self.extra_emission, self.hot_segments.last()) {
            (Some(e), Some(seg)) if e.entries > 0 => vec![TailEmission {
                seg: self.hot_segments.len() - 1,
                off: seg.bytes - e.bytes,
                entries: e.entries,
                bytes: e.bytes,
            }],
            _ => Vec::new(),
        }
    }

    pub fn total_entries(&self) -> u64 {
        let flat: u64 = self.segments.iter().map(|s| s.entries).sum();
        let cold: u64 = self.cold_segments.iter().map(|s| s.entries).sum();
        let hot: u64 = self.spine.iter().map(|s| s.entries).sum();
        let tails: u64 = self.tails().iter().map(|t| t.entries).sum();
        let wal: u64 = self.wal.iter().map(|w| w.entries).sum();
        flat + cold + hot + tails + wal
    }

    /// Bytes this manifest holds under its own prefix: every tier's
    /// objects whose key begins `"{own_prefix}/"`, by the byte counts the
    /// manifest itself carries.
    ///
    /// This is the **logical** size of a repository, and the number a
    /// customer is billed for. It is derived from the manifest rather
    /// than counted as writes happen because every durable write ends
    /// in a manifest CAS and the store crates hold no database handle:
    /// a control-plane row that is *set* to this value after each write
    /// is idempotent, so a process killed between the CAS and the row
    /// converges on the next write or the next sweep, where an
    /// increment would drift by exactly the writes that died.
    ///
    /// The prefix filter is what makes a zero-copy fork free. A fork's
    /// manifest names upstream's objects under upstream's prefix until
    /// promotion rewrites them under its own; those bytes are upstream's
    /// bill, and a fork-shaped manifest sums to zero here. Sidecars the
    /// manifest names only indirectly are counted where their size is
    /// exact — a WAL entry's sorted oid list is twenty bytes an object —
    /// and left out where it is not (the locator header and chains,
    /// which are kilobytes beside a plane of many megabytes). Exports,
    /// CDN packs and audit shards are not referenced here at all and
    /// never count: they are derived, and the sweep's *physical*
    /// inventory is where they show up.
    pub fn stored_bytes(&self, own_prefix: &str) -> u64 {
        let own = format!("{own_prefix}/");
        let mine = |key: &str| key.starts_with(&own);
        let flat: u64 = self
            .segments
            .iter()
            .chain(self.cold_segments.iter())
            .chain(self.snapshot.iter())
            .filter(|s| mine(&s.key))
            .map(|s| s.bytes)
            .sum();
        let hot: u64 = self
            .hot_segments
            .iter()
            .filter(|s| mine(&s.key))
            .map(|s| s.bytes)
            .sum();
        let wal: u64 = self
            .wal
            .iter()
            .filter(|w| mine(&w.key))
            .map(|w| w.bytes + w.entries * 20)
            .sum();
        let pages: u64 = self
            .ref_pages
            .iter()
            .filter(|p| mine(&p.key))
            .map(|p| p.bytes)
            .sum();
        let locator: u64 = self
            .locator
            .iter()
            .filter(|l| mine(&l.key))
            .map(|l| l.record_bytes as u64 * l.records)
            .sum();
        flat + hot + wal + pages + locator
    }

    fn wal_parts(&self, from: usize) -> (u64, Vec<StreamPart>) {
        let entries = self.wal[from..].iter().map(|w| w.entries).sum();
        let parts = self.wal[from..]
            .iter()
            .map(|w| StreamPart {
                key: w.key.clone(),
                range: None,
                expect_bytes: w.bytes,
            })
            .collect();
        (entries, parts)
    }

    /// Index of the WAL entry whose transaction produced `oid` as a new
    /// tip — such a client has everything up to and including that entry.
    pub fn find_wal_tip(&self, oid: &str) -> Option<usize> {
        self.wal
            .iter()
            .rposition(|w| w.updates.iter().any(|(_, _, new)| new == oid))
    }

    /// Fetch plan for a client at WAL entry `have`: the later WAL entries.
    pub fn wal_suffix_plan(&self, have: usize) -> (u64, Vec<StreamPart>) {
        self.wal_parts(have + 1)
    }

    /// All advertised ref tips (any of which is a valid want).
    pub fn tips(&self) -> Vec<&str> {
        self.refs.iter().map(|(_, oid)| oid.as_str()).collect()
    }

    /// Full-clone plan: every segment, in stream order (cold, hot, then WAL
    /// entries in push order — thin delta bases always precede their deltas).
    pub fn clone_plan(&self) -> Vec<StreamPart> {
        self.segments
            .iter()
            .chain(self.cold_segments.iter())
            .map(|s| StreamPart {
                key: s.key.clone(),
                range: None,
                expect_bytes: s.bytes,
            })
            .chain(self.hot_segments.iter().map(|s| StreamPart {
                key: s.key.clone(),
                range: None,
                expect_bytes: s.bytes,
            }))
            .chain(self.wal_parts(0).1)
            .collect()
    }

    pub fn find_spine(&self, oid: &str) -> Option<usize> {
        self.spine.iter().position(|s| s.oid == oid)
    }

    /// Fetch plan for a client at spine index `have`: the hot-tier byte
    /// suffix strictly after that commit's emission — exactly
    /// closure(tip) \ closure(spine[have]), thin against the client's
    /// objects (docs/experiments/004). Returns (entry count, parts).
    pub fn suffix_plan(&self, have: usize) -> (u64, Vec<StreamPart>) {
        let (wal_entries, wal_parts) = self.wal_parts(0);
        let (mut entries, mut parts) = self.spine_suffix(have);
        entries += wal_entries;
        parts.extend(wal_parts);
        (entries, parts)
    }

    fn spine_suffix(&self, have: usize) -> (u64, Vec<StreamPart>) {
        let mut entries: u64 = 0;
        let mut parts = Vec::new();
        // The contiguous byte suffix from the first un-had spine emission
        // to the end of the hot tier (None when the client is at the tip).
        let start = self.spine.get(have + 1).map(|f| (f.seg, f.off));
        if let Some((start_seg, start_off)) = start {
            entries += self.spine[have + 1..]
                .iter()
                .map(|s| s.entries)
                .sum::<u64>();
            for (i, seg) in self.hot_segments.iter().enumerate().skip(start_seg) {
                let from = if i == start_seg { start_off } else { 0 };
                if from >= seg.bytes {
                    continue;
                }
                parts.push(StreamPart {
                    key: seg.key.clone(),
                    range: if from == 0 {
                        None
                    } else {
                        Some((from, seg.bytes - 1))
                    },
                    expect_bytes: seg.bytes - from,
                });
            }
        }
        // Tail emissions (secondary-ref closures) reach every client. One
        // inside the byte suffix is already streaming — count its entries
        // (spine sums exclude them) but add no part. One before the suffix
        // start (a mid-stream trailer from before an incremental fold, or
        // any trailer when the client is at the tip) is delivered as an
        // explicit range: its REF_DELTA bases are ingest-time primary
        // objects, which any spine ACK's closure contains, and REF deltas
        // are position-independent for index-pack.
        for t in self.tails() {
            entries += t.entries;
            let inside = match start {
                Some((s_seg, s_off)) => t.seg > s_seg || (t.seg == s_seg && t.off >= s_off),
                None => false,
            };
            if !inside {
                let seg = &self.hot_segments[t.seg];
                parts.push(StreamPart {
                    key: seg.key.clone(),
                    range: if t.off == 0 && t.bytes == seg.bytes {
                        None
                    } else {
                        Some((t.off, t.off + t.bytes - 1))
                    },
                    expect_bytes: t.bytes,
                });
            }
        }
        (entries, parts)
    }

    /// Index of the ref page whose range covers (or would receive) `name`.
    /// Pages are sorted by `first`; names in a gap between pages belong to
    /// the preceding page. None only when there are no pages.
    pub fn page_index(&self, name: &str) -> Option<usize> {
        if self.ref_pages.is_empty() {
            return None;
        }
        let idx = self.ref_pages.partition_point(|p| p.first.as_str() <= name);
        Some(idx.saturating_sub(1))
    }

    /// Indices of pages that can contain refs starting with `prefix`.
    pub fn pages_overlapping(&self, prefix: &str) -> Vec<usize> {
        let mut upper = prefix.to_string();
        upper.push(char::MAX);
        self.ref_pages
            .iter()
            .enumerate()
            .filter(|(_, p)| p.last.as_str() >= prefix && p.first < upper)
            .map(|(i, _)| i)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"{
        "schema": 3, "repo": "r", "layout": "tiered-64",
        "refs": [["refs/heads/bench", "aa"]], "head": "refs/heads/bench",
        "epoch": "e1"
    }"#;

    #[test]
    fn object_format_defaults_to_sha1() {
        let m: Manifest = serde_json::from_str(MINIMAL).unwrap();
        assert_eq!(m.object_format, "sha1");
    }

    #[test]
    fn explicit_object_format_is_kept() {
        let j = MINIMAL.replace(
            "\"schema\": 3",
            "\"schema\": 3, \"object_format\": \"sha256\"",
        );
        let m: Manifest = serde_json::from_str(&j).unwrap();
        assert_eq!(m.object_format, "sha256");
    }

    /// Two hot segments; spine entries at (0,0), (0,100), (1,0); a v3
    /// extra trailer occupying the last 40 bytes of segment 1.
    fn tiered(extra_v3: bool, tails_v4: Vec<TailEmission>) -> Manifest {
        let mut m: Manifest = serde_json::from_str(MINIMAL).unwrap();
        m.hot_segments = vec![
            HotSegment {
                key: "h0".into(),
                bytes: 300,
            },
            HotSegment {
                key: "h1".into(),
                bytes: 200,
            },
        ];
        m.spine = vec![
            SpineEntry {
                oid: "a".into(),
                seg: 0,
                off: 0,
                entries: 5,
                bytes: 100,
            },
            SpineEntry {
                oid: "b".into(),
                seg: 0,
                off: 100,
                entries: 6,
                bytes: 200,
            },
            SpineEntry {
                oid: "c".into(),
                seg: 1,
                off: 0,
                entries: 7,
                bytes: 160,
            },
        ];
        if extra_v3 {
            m.extra_emission = Some(ExtraEmission {
                entries: 3,
                bytes: 40,
            });
        }
        m.tail_emissions = tails_v4;
        m
    }

    #[test]
    fn v3_extra_normalizes_to_tail() {
        let m = tiered(true, vec![]);
        let t = m.tails();
        assert_eq!(t.len(), 1);
        assert_eq!(
            (t[0].seg, t[0].off, t[0].entries, t[0].bytes),
            (1, 160, 3, 40)
        );
        assert_eq!(m.total_entries(), 5 + 6 + 7 + 3);
    }

    #[test]
    fn suffix_mid_spine_includes_tail_in_byte_range() {
        // Client at spine[0]: byte suffix starts at (0,100) and spans to
        // the end, which contains the trailer — entries counted, no
        // duplicate part.
        let m = tiered(true, vec![]);
        let (entries, parts) = m.suffix_plan(0);
        assert_eq!(entries, 6 + 7 + 3);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].range, Some((100, 299)));
        assert_eq!(parts[1].range, None); // whole h1
    }

    #[test]
    fn suffix_at_tip_delivers_tail_explicitly() {
        let m = tiered(true, vec![]);
        let (entries, parts) = m.suffix_plan(2);
        assert_eq!(entries, 3);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].key, "h1");
        assert_eq!(parts[0].range, Some((160, 199)));
    }

    #[test]
    fn mid_stream_tail_delivered_when_suffix_starts_after_it() {
        // v4 shape after a fold: the old trailer sits mid-stream in h0
        // ([250,300)), the fold's spine entry is in h1. A client at
        // spine[2] (in h1, off 0)... use spine[1] ACK: suffix starts at
        // (1,0) — the h0 trailer is before it and must ship explicitly.
        let mut m = tiered(
            false,
            vec![TailEmission {
                seg: 0,
                off: 250,
                entries: 3,
                bytes: 50,
            }],
        );
        m.spine[1] = SpineEntry {
            oid: "b".into(),
            seg: 0,
            off: 100,
            entries: 6,
            bytes: 150,
        };
        let (entries, parts) = m.suffix_plan(1);
        assert_eq!(entries, 7 + 3);
        // byte suffix = all of h1; plus the explicit h0 trailer range
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].key, "h1");
        assert_eq!(parts[1].key, "h0");
        assert_eq!(parts[1].range, Some((250, 299)));
    }

    fn paged() -> Manifest {
        let mut m: Manifest = serde_json::from_str(MINIMAL).unwrap();
        m.ref_pages = vec![
            RefPage {
                first: "refs/heads/a".into(),
                last: "refs/heads/m".into(),
                key: "p0".into(),
                count: 2,
                bytes: 10,
            },
            RefPage {
                first: "refs/heads/n".into(),
                last: "refs/tags/v1".into(),
                key: "p1".into(),
                count: 2,
                bytes: 10,
            },
            RefPage {
                first: "refs/tags/v2".into(),
                last: "refs/tags/v9".into(),
                key: "p2".into(),
                count: 2,
                bytes: 10,
            },
        ];
        m
    }

    #[test]
    fn page_index_covers_gaps_and_extremes() {
        let m = paged();
        assert_eq!(m.page_index("refs/heads/b"), Some(0));
        assert_eq!(m.page_index("refs/heads/zzz"), Some(1)); // gap -> preceding
        assert_eq!(m.page_index("refs/tags/v5"), Some(2));
        assert_eq!(m.page_index("refs/AAA"), Some(0)); // before first
        assert_eq!(m.page_index("refs/zz"), Some(2)); // after last
        assert!(tiered(false, vec![]).page_index("x").is_none());
    }

    #[test]
    fn pages_overlapping_prefix() {
        let m = paged();
        assert_eq!(m.pages_overlapping("refs/heads/"), vec![0, 1]);
        assert_eq!(m.pages_overlapping("refs/tags/"), vec![1, 2]);
        assert_eq!(m.pages_overlapping("refs/"), vec![0, 1, 2]);
        assert_eq!(m.pages_overlapping("refs/heads/c"), vec![0]);
        assert!(m.pages_overlapping("refs/zzz").is_empty());
    }

    /// Every tier under the repo's own prefix is summed by the byte
    /// counts the manifest carries; a WAL entry brings its oid list.
    #[test]
    fn stored_bytes_sums_every_tier_under_the_own_prefix() {
        let mut m: Manifest = serde_json::from_str(MINIMAL).unwrap();
        let p = "o/a/r/b/tiered-64";
        assert_eq!(m.stored_bytes(p), 0, "an empty repository holds nothing");
        m.segments.push(Segment {
            key: format!("{p}/e1/flat.seg"),
            entries: 1,
            bytes: 10,
        });
        m.cold_segments.push(Segment {
            key: format!("{p}/e1/cold.seg"),
            entries: 1,
            bytes: 100,
        });
        m.hot_segments.push(HotSegment {
            key: format!("{p}/e1/hot.seg"),
            bytes: 1_000,
        });
        m.snapshot = Some(Segment {
            key: format!("{p}/e1/snap.seg"),
            entries: 1,
            bytes: 10_000,
        });
        m.wal.push(WalEntry {
            key: format!("{p}/e1/wal/x.seg"),
            oids_key: format!("{p}/e1/wal/x.oids"),
            entries: 3,
            bytes: 100_000,
            updates: vec![],
        });
        m.ref_pages.push(RefPage {
            first: "a".into(),
            last: "z".into(),
            key: format!("{p}/e1/refs/p.page"),
            count: 1,
            bytes: 1_000_000,
        });
        m.locator = Some(Locator {
            key: format!("{p}/e1/locator.bin"),
            hdr_key: String::new(),
            chains_key: String::new(),
            record_bytes: 32,
            records: 4,
        });
        assert_eq!(
            m.stored_bytes(p),
            10 + 100 + 1_000 + 10_000 + 100_000 + 3 * 20 + 1_000_000 + 128
        );
    }

    /// A zero-copy fork's manifest names upstream's objects under
    /// upstream's prefix. Those are upstream's bytes: the fork sums to
    /// what it pushed itself, and a fresh fork to nothing at all — and a
    /// prefix that merely *starts* like the fork's own does not match.
    #[test]
    fn stored_bytes_leaves_out_a_forks_foreign_keys() {
        let mut m: Manifest = serde_json::from_str(MINIMAL).unwrap();
        let upstream = "o/a/r/up/tiered-64";
        let fork = "o/a/r/fork/tiered-64";
        m.cold_segments.push(Segment {
            key: format!("{upstream}/e1/cold.seg"),
            entries: 9,
            bytes: 5_000,
        });
        m.hot_segments.push(HotSegment {
            key: format!("{upstream}/e1/hot.seg"),
            bytes: 500,
        });
        m.locator = Some(Locator {
            key: format!("{upstream}/e1/locator.bin"),
            hdr_key: String::new(),
            chains_key: String::new(),
            record_bytes: 32,
            records: 9,
        });
        assert_eq!(
            m.stored_bytes(fork),
            0,
            "a fresh fork holds nothing of its own"
        );
        assert_eq!(m.stored_bytes(upstream), 5_000 + 500 + 9 * 32);
        m.wal.push(WalEntry {
            key: format!("{fork}/e1/wal/own.seg"),
            oids_key: format!("{fork}/e1/wal/own.oids"),
            entries: 2,
            bytes: 300,
            updates: vec![],
        });
        assert_eq!(m.stored_bytes(fork), 300 + 40);
        m.wal.push(WalEntry {
            key: format!("{fork}2/e1/wal/other.seg"),
            oids_key: String::new(),
            entries: 1,
            bytes: 7_000,
            updates: vec![],
        });
        assert_eq!(m.stored_bytes(fork), 340);
    }
}
