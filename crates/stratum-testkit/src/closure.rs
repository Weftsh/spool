//! Store-closure oracle: given a live object store, prove that everything
//! the pointers reference actually exists and is internally consistent.
//!
//! `reference/invariants.md` states this as prose and nothing checked it.
//! A manifest is a promise: every key it names is already durable
//! (I7/I10), every declared length is exact (I1/I6), the epoch data it
//! points into is immutable and self-contained (I8), and `locator.hdr` is
//! a *second, independent* promise that may legally lag the manifest
//! (I15). After a chaos run — killed compactors, injected 500s, lost CAS
//! races — the store is either still closed under those promises or it is
//! not, and "the clone still worked" is a weaker statement than that.
//!
//! Deliberately **not** a `Drop` impl. A panicking `Drop` during unwind
//! aborts the process and takes the original test failure with it, so the
//! one run that would have taught you something prints nothing. Closure
//! is an explicit statement a test makes: `assert_closed(&bucket.base_url)`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sha1::{Digest, Sha1};
use stratum_store::manifest::{Manifest, TailEmission};
use stratum_store::pack::hex;
use stratum_store::plane::Plane;
use stratum_store::store::ObjectStore;
use stratum_store::{refpages, LatencyModel};

/// One broken promise, named by the invariant it breaks so a failure
/// message points at the paragraph that explains what it costs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The repo layout prefix ("<repo>/<layout>") the violation is under.
    pub prefix: String,
    /// The invariant id from `reference/invariants.md` ("I7", "I13", ...).
    pub invariant: &'static str,
    pub detail: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.invariant, self.prefix, self.detail)
    }
}

/// What the sweep actually looked at. A checker that silently examined
/// nothing passes; the counts are how a suite notices that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClosureReport {
    pub repos: usize,
    /// Distinct object keys whose existence was proven.
    pub keys_checked: usize,
    /// Epoch directory names reachable from the pointers.
    pub epochs: BTreeSet<String>,
    /// Declared bytes pinned exactly by a pair of ranged GETs.
    pub bytes_verified: u64,
    /// Legal-but-notable states — chiefly an I15 pointer skew. These are
    /// not failures; a healthy repo mid-compaction has them.
    pub observations: Vec<String>,
}

impl ClosureReport {
    fn absorb(&mut self, other: ClosureReport) {
        self.repos += other.repos;
        self.keys_checked += other.keys_checked;
        self.epochs.extend(other.epochs);
        self.bytes_verified += other.bytes_verified;
        self.observations.extend(other.observations);
    }
}

impl fmt::Display for ClosureReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} repo(s), {} keys, {} epoch(s), {} bytes pinned",
            self.repos,
            self.keys_checked,
            self.epochs.len(),
            self.bytes_verified
        )
    }
}

/// Check one repo layout, addressed by its pointer prefix
/// ("<repo>/<layout>" — the directory holding `manifest.json` and
/// `locator.hdr`).
pub fn check_repo(store: &ObjectStore, prefix: &str) -> Result<ClosureReport, Vec<Violation>> {
    let mut c = Checker::new(store, prefix);
    c.run();
    if c.violations.is_empty() {
        Ok(c.report)
    } else {
        Err(c.violations)
    }
}

/// Check every repo layout in the bucket. Repos are discovered the way a
/// sweep would: a `manifest.json` is a pointer, and its parent is a
/// prefix. Violations from all repos are returned together, because after
/// a fault injection "which repos broke" is the interesting shape.
pub fn check_bucket(store: &ObjectStore) -> Result<ClosureReport, Vec<Violation>> {
    let listing = match store.list("") {
        Ok(l) => l,
        Err(e) => {
            return Err(vec![Violation {
                prefix: String::new(),
                invariant: "I9",
                detail: format!("cannot list the bucket: {e}"),
            }])
        }
    };
    let prefixes: BTreeSet<String> = listing
        .into_iter()
        .filter_map(|(key, _)| key.strip_suffix("/manifest.json").map(str::to_string))
        .collect();
    let mut report = ClosureReport::default();
    let mut violations = Vec::new();
    for prefix in prefixes {
        match check_repo(store, &prefix) {
            Ok(r) => report.absorb(r),
            Err(v) => violations.extend(v),
        }
    }
    if violations.is_empty() {
        Ok(report)
    } else {
        Err(violations)
    }
}

/// The one-liner a suite calls at teardown. Panics with every violation
/// listed, so one run tells you the whole shape of the damage rather than
/// the first symptom of it.
pub fn assert_closed(store_url: &str) -> ClosureReport {
    let store = ObjectStore::new(store_url, LatencyModel::None);
    match check_bucket(&store) {
        Ok(report) => report,
        Err(violations) => {
            let mut msg = format!(
                "store closure broken: {} violation(s) under {store_url}\n",
                violations.len()
            );
            for v in &violations {
                msg.push_str("  ");
                msg.push_str(&v.to_string());
                msg.push('\n');
            }
            panic!("{msg}");
        }
    }
}

// ---------------------------------------------------------------------

/// A key the pointers claim exists, with the length they claim for it.
struct Claim {
    key: String,
    /// Human label used in the violation text ("hot segment 1").
    what: String,
    invariant: &'static str,
    /// `None` when the pointer declares no length for this key (locator
    /// chains, WAL oid sidecars) — existence is all we can assert.
    bytes: Option<u64>,
}

struct Checker<'a> {
    store: &'a ObjectStore,
    prefix: String,
    violations: Vec<Violation>,
    report: ClosureReport,
    claims: Vec<Claim>,
}

impl<'a> Checker<'a> {
    fn new(store: &'a ObjectStore, prefix: &str) -> Self {
        Self {
            store,
            prefix: prefix.to_string(),
            violations: Vec::new(),
            report: ClosureReport {
                repos: 1,
                ..Default::default()
            },
            claims: Vec::new(),
        }
    }

    fn flag(&mut self, invariant: &'static str, detail: String) {
        self.violations.push(Violation {
            prefix: self.prefix.clone(),
            invariant,
            detail,
        });
    }

    fn observe(&mut self, note: String) {
        self.report.observations.push(note);
    }

    fn claim(&mut self, invariant: &'static str, what: &str, key: &str, bytes: Option<u64>) {
        // Serde-defaulted keys are absent fields, not dangling pointers:
        // a schema-3 manifest has no `chains_key`, an unpaged repo has no
        // pages. Reporting them would make the oracle fire on every repo
        // that simply does not use the feature.
        if key.is_empty() {
            return;
        }
        self.claims.push(Claim {
            key: key.to_string(),
            what: what.to_string(),
            invariant,
            bytes,
        });
    }

    fn run(&mut self) {
        let manifest_key = format!("{}/manifest.json", self.prefix);
        let raw = match self.store.get(&manifest_key) {
            Ok(b) => b,
            Err(e) => {
                self.flag("I9", format!("manifest unreadable: {e}"));
                return;
            }
        };
        let manifest: Manifest = match serde_json::from_slice(&raw) {
            Ok(m) => m,
            Err(e) => {
                self.flag("I9", format!("manifest does not parse: {e}"));
                return;
            }
        };

        self.collect_claims(&manifest);
        self.check_geometry(&manifest);
        self.check_ref_pages(&manifest);
        self.check_wal(&manifest);
        self.check_locator_header(&manifest);
        // Existence last: the structural checks above add claims of their
        // own (the header's own generation files), and one pass over a
        // deduplicated key set avoids re-GETting a segment named by a
        // dozen spine entries.
        self.check_claims();
    }

    /// I7/I9: every key the manifest names must already be durable.
    fn collect_claims(&mut self, m: &Manifest) {
        for (i, s) in m.segments.iter().enumerate() {
            self.claim("I7", &format!("flat segment {i}"), &s.key, Some(s.bytes));
        }
        for (i, s) in m.cold_segments.iter().enumerate() {
            self.claim("I7", &format!("cold segment {i}"), &s.key, Some(s.bytes));
        }
        for (i, s) in m.hot_segments.iter().enumerate() {
            self.claim("I7", &format!("hot segment {i}"), &s.key, Some(s.bytes));
        }
        if let Some(s) = &m.snapshot {
            self.claim("I7", "snapshot", &s.key, Some(s.bytes));
        }
        if let Some(l) = &m.locator {
            // The locator blob is exactly `records` fixed-width records,
            // so the manifest declares its length implicitly.
            let bytes = (l.records).checked_mul(l.record_bytes as u64);
            self.claim("I7", "locator table", &l.key, bytes);
            self.claim("I7", "locator hdr", &l.hdr_key, None);
            // chains.bin is legitimately empty when no record needs an
            // out-of-line hop plan, so it carries no declared length.
            self.claim("I7", "locator chains", &l.chains_key, None);
        }
        for (i, p) in m.ref_pages.iter().enumerate() {
            self.claim("I7", &format!("ref page {i}"), &p.key, Some(p.bytes));
        }
        for (i, w) in m.wal.iter().enumerate() {
            self.claim("I10", &format!("wal {i} payload"), &w.key, Some(w.bytes));
            self.claim("I10", &format!("wal {i} oids"), &w.oids_key, None);
        }
    }

    /// Prove each claimed key exists, and — where a length was declared —
    /// that the length is exact, with two ranged GETs and no transfer.
    fn check_claims(&mut self) {
        let mut seen: BTreeMap<String, Option<u64>> = BTreeMap::new();
        let claims = std::mem::take(&mut self.claims);
        for c in claims {
            match seen.get(&c.key) {
                Some(&prior) => {
                    // Two pointers naming one key with two lengths cannot
                    // both be right, and the reader that loses believes a
                    // range that runs off the end of the object.
                    if let (Some(a), Some(b)) = (prior, c.bytes) {
                        if a != b {
                            self.flag(
                                "I6",
                                format!(
                                    "{}: {} declares {b} bytes, another pointer says {a}",
                                    c.key, c.what
                                ),
                            );
                        }
                    }
                    continue;
                }
                None => seen.insert(c.key.clone(), c.bytes),
            };
            if let Some(epoch) = self.epoch_of(&c.key) {
                self.report.epochs.insert(epoch);
            }
            self.report.keys_checked += 1;
            match c.bytes {
                Some(bytes) => self.check_exact_len(c.invariant, &c.what, &c.key, bytes),
                None => self.check_exists(c.invariant, &c.what, &c.key),
            }
        }
    }

    /// I8: data keys live under an immutable epoch directory. Derived the
    /// same way `stratum_engine::gc::live_epochs` derives it, so the
    /// oracle and the sweeper can never disagree about what is live.
    fn epoch_of(&self, key: &str) -> Option<String> {
        key.strip_prefix(&format!("{}/", self.prefix))?
            .split_once('/')
            .map(|(epoch, _)| epoch.to_string())
    }

    fn check_exists(&mut self, invariant: &'static str, what: &str, key: &str) {
        // A zero-length object cannot be probed with a range at all (any
        // range is unsatisfiable), so try the cheap probe first and only
        // fall back to a whole-object GET, which is what distinguishes
        // "empty" from "gone".
        if self.store.get_stream(key, Some((0, 0))).is_ok() {
            return;
        }
        if let Err(e) = self.store.get(key) {
            self.flag(invariant, format!("{what} {key}: {e}"));
        }
    }

    /// Two ranged GETs pin a declared length in both directions and
    /// exercise I13's "ranged GETs must return 206" at the same time:
    /// the last declared byte must be servable, and the byte after it
    /// must not be. `get_stream` already refuses a 200 answer to a
    /// ranged request, so a proxy that ignores Range fails here too.
    fn check_exact_len(&mut self, invariant: &'static str, what: &str, key: &str, bytes: u64) {
        if bytes == 0 {
            match self.store.get(key) {
                Ok(body) if body.is_empty() => {}
                Ok(body) => self.flag(
                    "I6",
                    format!("{what} {key}: declared 0 bytes, store has {}", body.len()),
                ),
                Err(e) => self.flag(invariant, format!("{what} {key}: {e}")),
            }
            return;
        }
        let last = bytes - 1;
        let mut buf = Vec::new();
        match self.store.get_stream(key, Some((last, last))) {
            Ok(mut r) => {
                if let Err(e) = std::io::Read::read_to_end(&mut r, &mut buf) {
                    self.flag(invariant, format!("{what} {key}: read byte {last}: {e}"));
                    return;
                }
            }
            // One violation, not two: a key that is missing (or short)
            // would also "pass" the past-the-end probe, and reporting the
            // same broken pointer twice buries the next one.
            Err(e) => {
                self.flag(
                    invariant,
                    format!("{what} {key}: declared {bytes} bytes, byte {last} unreadable: {e}"),
                );
                return;
            }
        }
        if buf.len() != 1 {
            self.flag(
                "I13",
                format!(
                    "{what} {key}: ranged GET of one byte returned {} bytes",
                    buf.len()
                ),
            );
            return;
        }
        if self.store.get_stream(key, Some((bytes, bytes))).is_ok() {
            self.flag(
                "I6",
                format!("{what} {key}: declared {bytes} bytes but byte {bytes} is servable"),
            );
            return;
        }
        self.report.bytes_verified += bytes;
    }

    /// I1/I6: the stream is composed of byte ranges, so every range the
    /// manifest describes has to lie inside the object it names, and the
    /// pieces must not overlap — spine entry counts deliberately exclude
    /// tail emissions, so a tail sitting inside a spine range would be
    /// streamed once and counted twice.
    fn check_geometry(&mut self, m: &Manifest) {
        // `tails()` normalizes the schema-3 trailer by subtracting its
        // length from the last hot segment's. Check that first: an
        // oversized trailer underflows there rather than reporting.
        if let (Some(e), Some(seg)) = (&m.extra_emission, m.hot_segments.last()) {
            if e.entries > 0 && e.bytes > seg.bytes {
                self.flag(
                    "I1",
                    format!(
                        "extra_emission is {} bytes but the last hot segment is only {}",
                        e.bytes, seg.bytes
                    ),
                );
                return;
            }
        }

        // (seg, off, end, label) for every piece that must tile the hot
        // tier without overlapping. Pieces already out of bounds are left
        // out: they are reported once, and cascading them into overlap
        // reports would bury the cause under its consequences.
        let mut pieces: Vec<(usize, u64, u64, String)> = Vec::new();
        let mut push = |c: &mut Self, seg: usize, off: u64, bytes: u64, what: String| {
            let Some(hot) = m.hot_segments.get(seg) else {
                c.flag(
                    "I1",
                    format!(
                        "{what}: segment index {seg} but there are {} hot segments",
                        m.hot_segments.len()
                    ),
                );
                return;
            };
            let end = match off.checked_add(bytes) {
                Some(e) => e,
                None => {
                    c.flag("I1", format!("{what}: offset {off} + {bytes} overflows"));
                    return;
                }
            };
            if end > hot.bytes {
                c.flag(
                    "I1",
                    format!(
                        "{what}: [{off},{end}) runs past hot segment {seg}, which is {} bytes",
                        hot.bytes
                    ),
                );
                return;
            }
            pieces.push((seg, off, end, what));
        };
        for (i, s) in m.spine.iter().enumerate() {
            push(
                self,
                s.seg,
                s.off,
                s.bytes,
                format!("spine {i} ({})", s.oid),
            );
        }
        for (i, t) in m.tails().iter().enumerate() {
            push(self, t.seg, t.off, t.bytes, format!("tail emission {i}"));
        }
        pieces.sort_by_key(|(seg, off, _, _)| (*seg, *off));
        for w in pieces.windows(2) {
            let (aseg, _, aend, awhat) = &w[0];
            let (bseg, boff, _, bwhat) = &w[1];
            if aseg == bseg && boff < aend {
                self.flag(
                    "I1",
                    format!("{awhat} ends at {aend} but {bwhat} starts at {boff}"),
                );
            }
        }

        // Spine order is the stream order clients cut at (I3/I4): a spine
        // that walks backwards means an ACK resolves to a suffix that is
        // not a suffix.
        for w in m.spine.windows(2) {
            if (w[1].seg, w[1].off) < (w[0].seg, w[0].off) {
                self.flag(
                    "I1",
                    format!(
                        "spine is out of stream order: {} at ({},{}) precedes {} at ({},{})",
                        w[0].oid, w[0].seg, w[0].off, w[1].oid, w[1].seg, w[1].off
                    ),
                );
            }
        }

        // I6: the header count a client's index-pack verifies is the sum
        // of the declared parts. Recomputed here from the parts rather
        // than read from `total_entries()`, so an edit that changes what
        // that method sums shows up as a disagreement instead of
        // silently redefining the invariant.
        let parts: u64 = m
            .segments
            .iter()
            .chain(m.cold_segments.iter())
            .map(|s| s.entries)
            .sum::<u64>()
            + m.spine.iter().map(|s| s.entries).sum::<u64>()
            + m.tails()
                .iter()
                .map(|t: &TailEmission| t.entries)
                .sum::<u64>()
            + m.wal.iter().map(|w| w.entries).sum::<u64>();
        if parts != m.total_entries() {
            self.flag(
                "I6",
                format!(
                    "entry count {} disagrees with the sum of the declared parts, {parts}",
                    m.total_entries()
                ),
            );
        }
    }

    /// The paged ref store is the only ref truth once it exists (I9), so
    /// a page that does not parse, or a range that overlaps its
    /// neighbour, means some ref is unreachable or ambiguous.
    fn check_ref_pages(&mut self, m: &Manifest) {
        if m.ref_pages.is_empty() {
            return;
        }
        for (i, p) in m.ref_pages.iter().enumerate() {
            if p.first > p.last {
                self.flag(
                    "I9",
                    format!("ref page {i}: range [{}, {}] is inverted", p.first, p.last),
                );
            }
        }
        for (i, w) in m.ref_pages.windows(2).enumerate() {
            if w[1].first <= w[0].last {
                self.flag(
                    "I9",
                    format!(
                        "ref pages {i}/{}: [{}, {}] overlaps [{}, {}]",
                        i + 1,
                        w[0].first,
                        w[0].last,
                        w[1].first,
                        w[1].last
                    ),
                );
            }
        }

        // Page bodies: length, content address and count are all checked
        // by `refpages::load_page` — the store's own verifier, reused so
        // there is exactly one definition of "this page is intact".
        let mut names: BTreeMap<String, String> = BTreeMap::new();
        for (i, p) in m.ref_pages.iter().enumerate() {
            let entries = match refpages::load_page(self.store, p) {
                Ok(e) => e,
                Err(e) => {
                    self.flag("I9", format!("ref page {i}: {e}"));
                    continue;
                }
            };
            if let (Some((first, _)), Some((last, _))) = (entries.first(), entries.last()) {
                if first != &p.first || last != &p.last {
                    self.flag(
                        "I9",
                        format!(
                            "ref page {i}: body spans [{first}, {last}], manifest says [{}, {}]",
                            p.first, p.last
                        ),
                    );
                }
            }
            if entries.windows(2).any(|w| w[0].0 >= w[1].0) {
                self.flag("I9", format!("ref page {i}: names are not sorted"));
            }
            for (name, oid) in entries {
                names.insert(name, oid);
            }
        }

        // Where a name is in both stores they must agree — the flat list
        // is a cache of the tips the serving plans need, not a second
        // source of truth.
        for (name, oid) in &m.refs {
            if let Some(paged) = names.get(name) {
                if paged != oid {
                    self.flag(
                        "I9",
                        format!("{name}: manifest.refs says {oid}, the page says {paged}"),
                    );
                }
            }
        }
        // An empty repo has no refs at all and a HEAD pointing at an
        // unborn branch, which is legal; only a repo that has refs is
        // promising that HEAD resolves.
        let known = !names.is_empty() || !m.refs.is_empty();
        let resolves = names.contains_key(&m.head) || m.refs.iter().any(|(n, _)| n == &m.head);
        if known && !resolves {
            self.flag(
                "I9",
                format!("HEAD is {} but no ref by that name exists", m.head),
            );
        }
    }

    /// I10: WAL objects are content-addressed, so their keys are
    /// self-verifying — the payload proves its own name. A replay that
    /// wrote different bytes under an existing key would be invisible to
    /// every other check.
    fn check_wal(&mut self, m: &Manifest) {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for (i, w) in m.wal.iter().enumerate() {
            if !seen.insert(&w.key) {
                self.flag("I10", format!("wal {i}: key {} appears twice", w.key));
            }
            let Some(want) = basename_stem(&w.key, ".seg") else {
                // No producer in this tree mints another shape; treat it
                // as unverifiable rather than wrong, exactly as the ref
                // page verifier does for a non-`page-` key.
                continue;
            };
            let payload = match self.store.get(&w.key) {
                Ok(p) => p,
                // Absence is already reported by the existence pass; a
                // second report of the same missing object adds nothing.
                Err(_) => continue,
            };
            let actual = hex(&Sha1::digest(&payload));
            if actual != want {
                self.flag(
                    "I10",
                    format!("wal {i}: {} holds a payload hashing to {actual}", w.key),
                );
            }
            if payload.len() as u64 != w.bytes {
                self.flag(
                    "I6",
                    format!(
                        "wal {i}: payload is {} bytes, manifest says {}",
                        payload.len(),
                        w.bytes
                    ),
                );
            }
            match basename_stem(&w.oids_key, ".oids") {
                Some(o) if o != want => self.flag(
                    "I10",
                    format!(
                        "wal {i}: oid sidecar {} is not the payload's sidecar",
                        w.oids_key
                    ),
                ),
                _ => {}
            }
            if let Ok(oids) = self.store.get(&w.oids_key) {
                if oids.len() % 20 != 0 {
                    self.flag(
                        "I10",
                        format!(
                            "wal {i}: oid sidecar is {} bytes, not a whole number of oids",
                            oids.len()
                        ),
                    );
                }
            }
        }
    }

    /// I15: `locator.hdr` carries the epoch its data lives in, and a
    /// reader derives every data key from that one header. So the header
    /// is checked *against itself*, never against the manifest — the two
    /// pointers lag each other during ingest by design, and a checker
    /// that treated the skew as damage would fire on healthy repos in the
    /// middle of every compaction.
    fn check_locator_header(&mut self, m: &Manifest) {
        let hdr_key = format!("{}/locator.hdr", self.prefix);
        let raw = match self.store.get(&hdr_key) {
            Ok(r) => r,
            Err(e) => {
                // A repo with no locator has nothing to point at yet.
                if m.locator.is_some() {
                    self.flag("I15", format!("locator.hdr unreadable: {e}"));
                }
                return;
            }
        };
        let plane = match Plane::load(self.store, &self.prefix) {
            Ok(p) => p,
            Err(e) => {
                self.flag("I15", format!("locator.hdr does not parse: {e}"));
                return;
            }
        };
        let epoch = plane
            .data_prefix
            .strip_prefix(&format!("{}/", self.prefix))
            .unwrap_or_default()
            .to_string();
        self.report.epochs.insert(epoch.clone());
        if epoch != m.epoch {
            self.observe(format!(
                "{}: locator.hdr is on epoch {epoch}, manifest.json on {} — legal skew (I15)",
                self.prefix, m.epoch
            ));
        }
        if plane.n_cold != m.cold_segments.len() as u64 {
            self.observe(format!(
                "{}: locator.hdr declares {} cold segments, manifest has {} — legal skew (I15)",
                self.prefix,
                plane.n_cold,
                m.cold_segments.len()
            ));
        }

        // The header's own generation files must exist in the header's
        // own epoch. This is the half of I8 that the manifest cannot
        // speak for: a compaction that swapped the hdr and then died owes
        // these objects regardless of what the manifest points at.
        let (loc_key, chains_key) = match plane.generation {
            Some(g) => (
                format!("{}/locator-g{g:04}.bin", plane.data_prefix),
                plane.chains_key(),
            ),
            None => (
                format!("{}/locator.bin", plane.data_prefix),
                plane.chains_key(),
            ),
        };
        // The bucket directory's terminal offset is the blob length, so
        // the header declares its table's size and can be held to it.
        let (records, buckets) = match parse_hdr_tail(&raw) {
            Some(v) => v,
            None => {
                self.flag("I15", "locator.hdr truncated past the bucket table".into());
                return;
            }
        };
        if buckets.windows(2).any(|w| w[1] < w[0]) {
            self.flag("I15", "locator.hdr bucket offsets are not monotonic".into());
        }
        let declared = buckets[4096];
        if records * stratum_store::plane::RECORD as u64 != declared {
            self.flag(
                "I15",
                format!(
                    "locator.hdr says {records} records but its bucket table ends at {declared}"
                ),
            );
        }
        self.claim("I8", "locator table (per hdr)", &loc_key, Some(declared));
        self.claim("I8", "locator chains (per hdr)", &chains_key, None);
    }
}

/// "<dir>/<stem><suffix>" -> "<stem>", when the stem is a lowercase hex
/// digest of the expected width. Anything else is a key shape no producer
/// in this tree mints, so it is unverifiable rather than wrong.
fn basename_stem(key: &str, suffix: &str) -> Option<String> {
    let stem = key.rsplit('/').next()?.strip_suffix(suffix)?;
    if stem.len() == 40 && stem.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(stem.to_string())
    } else {
        None
    }
}

/// The parts of `locator.hdr` that `Plane` does not expose: the record
/// count and the 4097-entry bucket directory. Offsets mirror
/// `Plane::load` exactly — SLH3 inserts a u32 generation after the epoch.
fn parse_hdr_tail(hdr: &[u8]) -> Option<(u64, Vec<u64>)> {
    let extra = match hdr.get(..4)? {
        b"SLH3" => 4,
        b"SLH2" => 0,
        _ => return None,
    };
    let elen = u16::from_be_bytes([*hdr.get(4)?, *hdr.get(5)?]) as usize;
    let at = 6 + elen + extra;
    let records = u64::from_be_bytes(hdr.get(at..at + 8)?.try_into().ok()?);
    let table = hdr.get(at + 16..at + 16 + 4097 * 8)?;
    let buckets = table
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_be_bytes(*c))
        .collect();
    Some((records, buckets))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minio::Minio;
    use stratum_store::manifest::{
        ExtraEmission, HotSegment, Locator, RefPage, Segment, SpineEntry, WalEntry,
    };
    use stratum_store::store::PutCond;

    const PREFIX: &str = "r/tiered-64";
    const EPOCH: &str = "e1";
    const MAIN: &str = "1111111111111111111111111111111111111111";

    fn data(name: &str) -> String {
        format!("{PREFIX}/{EPOCH}/{name}")
    }

    fn put(store: &ObjectStore, key: &str, body: &[u8]) {
        store.put(key, body, PutCond::None).expect("put");
    }

    /// The header exactly as `stratum_engine::locator` mints it: SLH3,
    /// epoch, generation, record count, cold count, 4097 byte offsets.
    fn locator_hdr(epoch: &str, generation: u32, records: u64, n_cold: u64) -> Vec<u8> {
        let eb = epoch.as_bytes();
        let mut hdr = Vec::new();
        hdr.extend_from_slice(b"SLH3");
        hdr.extend_from_slice(&(eb.len() as u16).to_be_bytes());
        hdr.extend_from_slice(eb);
        hdr.extend_from_slice(&generation.to_be_bytes());
        hdr.extend_from_slice(&records.to_be_bytes());
        hdr.extend_from_slice(&n_cold.to_be_bytes());
        // Every record in bucket 0: offsets step to the blob length at
        // bucket 1 and stay there, which is the shape a real table has
        // for any oid distribution — monotonic, terminating at the length.
        let end = records * stratum_store::plane::RECORD as u64;
        for b in 0..4097u64 {
            hdr.extend_from_slice(&(if b == 0 { 0 } else { end }).to_be_bytes());
        }
        hdr
    }

    fn page(store: &ObjectStore, entries: &[(&str, &str)]) -> RefPage {
        let owned: Vec<(String, String)> = entries
            .iter()
            .map(|(n, o)| (n.to_string(), o.to_string()))
            .collect();
        let body = refpages::encode_page(&owned);
        let key = data(&format!("refs/page-{}.txt", refpages::page_digest(&body)));
        put(store, &key, &body);
        RefPage {
            first: owned[0].0.clone(),
            last: owned[owned.len() - 1].0.clone(),
            key,
            count: owned.len() as u64,
            bytes: body.len() as u64,
        }
    }

    /// A small but complete repo of the shape the serving path expects:
    /// one cold segment, two hot segments with a spine and a schema-3
    /// trailer, a snapshot, a paged ref store, a locator generation with
    /// its header, and one WAL push.
    fn healthy(store: &ObjectStore) -> Manifest {
        put(store, &data("cold-0000.seg"), &[b'c'; 100]);
        put(store, &data("hot-0000.seg"), &vec![b'h'; 300]);
        put(store, &data("hot-0001.seg"), &[b'H'; 200]);
        put(store, &data("snapshot.seg"), &[b's'; 50]);
        put(store, &data("locator-g0001.bin"), &vec![0u8; 2 * 150]);
        put(store, &data("chains-g0001.bin"), &[]);
        put(
            store,
            &format!("{PREFIX}/locator.hdr"),
            &locator_hdr(EPOCH, 1, 2, 1),
        );

        let payload = b"a thin pack payload".to_vec();
        let digest = hex(&Sha1::digest(&payload));
        let wal_key = data(&format!("wal/{digest}.seg"));
        let oids_key = data(&format!("wal/{digest}.oids"));
        put(store, &wal_key, &payload);
        put(store, &oids_key, &[0u8; 40]);

        let p0 = page(
            store,
            &[("refs/heads/a", &"a".repeat(40)), ("refs/heads/main", MAIN)],
        );
        let p1 = page(
            store,
            &[
                ("refs/tags/v1", &"b".repeat(40)),
                ("refs/tags/v2", &"c".repeat(40)),
            ],
        );

        Manifest {
            schema: 4,
            repo: "r".into(),
            layout: "tiered-64".into(),
            object_format: "sha1".into(),
            refs: vec![("refs/heads/main".into(), MAIN.into())],
            head: "refs/heads/main".into(),
            segments: Vec::new(),
            cold_segments: vec![Segment {
                key: data("cold-0000.seg"),
                entries: 3,
                bytes: 100,
            }],
            hot_segments: vec![
                HotSegment {
                    key: data("hot-0000.seg"),
                    bytes: 300,
                },
                HotSegment {
                    key: data("hot-0001.seg"),
                    bytes: 200,
                },
            ],
            spine: vec![
                SpineEntry {
                    oid: MAIN.into(),
                    seg: 0,
                    off: 0,
                    entries: 5,
                    bytes: 100,
                },
                SpineEntry {
                    oid: "b".repeat(40),
                    seg: 0,
                    off: 100,
                    entries: 6,
                    bytes: 200,
                },
                SpineEntry {
                    oid: "c".repeat(40),
                    seg: 1,
                    off: 0,
                    entries: 7,
                    bytes: 160,
                },
            ],
            locator: Some(Locator {
                key: data("locator-g0001.bin"),
                hdr_key: format!("{PREFIX}/locator.hdr"),
                chains_key: data("chains-g0001.bin"),
                record_bytes: 150,
                records: 2,
            }),
            shallow: Vec::new(),
            epoch: EPOCH.into(),
            extra_emission: Some(ExtraEmission {
                entries: 3,
                bytes: 40,
            }),
            tail_emissions: Vec::new(),
            ref_pages: vec![p0, p1],
            snapshot: Some(Segment {
                key: data("snapshot.seg"),
                entries: 4,
                bytes: 50,
            }),
            wal: vec![WalEntry {
                key: wal_key,
                oids_key,
                entries: 2,
                bytes: payload.len() as u64,
                updates: vec![("refs/heads/main".into(), "0".repeat(40), MAIN.into())],
            }],
        }
    }

    fn commit(store: &ObjectStore, m: &Manifest) {
        put(
            store,
            &format!("{PREFIX}/manifest.json"),
            &serde_json::to_vec(m).unwrap(),
        );
    }

    /// The oracle must be silent on a repo that is actually intact —
    /// otherwise nothing it says later means anything.
    #[test]
    fn a_healthy_repo_is_closed() {
        let bucket = Minio::shared().bucket("closure-healthy");
        let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
        let m = healthy(&store);
        commit(&store, &m);

        let report = check_repo(&store, PREFIX).expect("healthy repo must be closed");
        assert_eq!(report.repos, 1);
        assert_eq!(report.epochs, BTreeSet::from([EPOCH.to_string()]));
        assert!(report.observations.is_empty(), "{:?}", report.observations);
        // cold + 2 hot + snapshot + locator table + 2 pages + wal
        // payload, each pinned by a pair of ranged GETs; locator.hdr,
        // chains.bin and the oid sidecar declare no length, so they are
        // proven present rather than measured.
        assert_eq!(
            report.bytes_verified,
            100 + 300 + 200 + 50 + 300 + 111 + 108 + 19
        );
        assert_eq!(report.keys_checked, 11);

        // And the bucket-wide entry point discovers it by its pointer.
        assert_eq!(assert_closed(&bucket.base_url).repos, 1);
    }

    /// Three independent breakages, one of each kind the oracle exists
    /// for: a pointer to an object that is not there (I7), a page whose
    /// manifest metadata disagrees with its body (I9), and a byte range
    /// that runs off the end of the segment it claims to be in (I1).
    #[test]
    fn it_reports_exactly_the_three_things_that_are_broken() {
        let bucket = Minio::shared().bucket("closure-broken");
        let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
        let mut m = healthy(&store);

        m.hot_segments[0].key = data("hot-9999.seg");
        m.ref_pages[1].count += 1;
        m.spine[2].bytes = 250; // hot segment 1 is 200 bytes
        commit(&store, &m);

        let v = check_repo(&store, PREFIX).expect_err("must fire");
        let rendered: Vec<String> = v.iter().map(Violation::to_string).collect();
        assert_eq!(v.len(), 3, "{rendered:#?}");

        let missing = &v.iter().find(|v| v.invariant == "I7").expect("I7").detail;
        assert!(missing.contains("hot-9999.seg"), "{missing}");
        let page = &v.iter().find(|v| v.invariant == "I9").expect("I9").detail;
        assert!(page.contains("2 refs, manifest says 3"), "{page}");
        let spine = &v.iter().find(|v| v.invariant == "I1").expect("I1").detail;
        assert!(spine.contains("runs past hot segment 1"), "{spine}");
    }

    /// I15 in the direction that matters: after a lost compaction race
    /// the two pointers legally disagree, and each is self-consistent on
    /// its own. An oracle that failed here would fire on healthy repos in
    /// the middle of every compaction.
    #[test]
    fn a_pointer_skew_is_an_observation_not_a_violation() {
        let bucket = Minio::shared().bucket("closure-skew");
        let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
        let m = healthy(&store);
        commit(&store, &m);
        // The compactor swapped locator.hdr to epoch e2 and died before
        // the manifest CAS. e2's locator files are already durable (I7).
        put(
            &store,
            &format!("{PREFIX}/e2/locator-g0002.bin"),
            &[0u8; 150],
        );
        put(&store, &format!("{PREFIX}/e2/chains-g0002.bin"), &[]);
        put(
            &store,
            &format!("{PREFIX}/locator.hdr"),
            &locator_hdr("e2", 2, 1, 1),
        );

        let report = check_repo(&store, PREFIX).expect("skew is legal");
        assert_eq!(
            report.epochs,
            BTreeSet::from(["e1".to_string(), "e2".to_string()])
        );
        assert!(
            report.observations.iter().any(|o| o.contains("legal skew")),
            "{:?}",
            report.observations
        );

        // ...but the epoch the header points at still owes its objects.
        store
            .delete(&format!("{PREFIX}/e2/locator-g0002.bin"))
            .unwrap();
        let v = check_repo(&store, PREFIX).expect_err("a dangling hdr is not legal");
        assert_eq!(v.len(), 1, "{v:#?}");
        assert_eq!(v[0].invariant, "I8");
        assert!(v[0].detail.contains("locator-g0002.bin"), "{}", v[0].detail);
    }
}
