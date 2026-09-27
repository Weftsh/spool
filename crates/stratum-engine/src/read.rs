//! Layout-wide object reads for the REST API: WAL entries first (they are
//! newer truth, and a just-pushed object exists nowhere else), then the
//! locator plane. Verified on return — nothing hash-unchecked leaves this
//! module.
//!
//! Two costs shaped the second version of this file, both measured on
//! the production fleet against a mirror whose WAL had grown to 102
//! entries (see `docs/operations.md`, *Reads on a long WAL*):
//!
//! * A reader used to download **every** WAL pack, in full, before it
//!   could answer a single object — even one that lived in the plane —
//!   because membership was learned by opening the pack. Two GETs per
//!   entry, sequential, on every request: five to eleven seconds before
//!   the first byte of a three-byte file. Membership is now read from
//!   the push's oid sidecar alone, and a pack's payload is fetched only
//!   when the object being read is actually in it.
//! * Nothing survived the request. Every object, every sidecar, every
//!   pack was fetched again by the next reader, though all three are
//!   immutable: objects are content-addressed and a WAL key is minted
//!   once per push and never rewritten. [`ReadCache`] holds them across
//!   readers, bounded by bytes, and a hit costs no store round trip.

use crate::objwrite::{hash_object, hex, parse_hex};
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use stratum_store::manifest::{Manifest, WalEntry};
use stratum_store::pack::{apply_delta, resolve, scan_pack, Resolved};
use stratum_store::{ObjectStore, Plane};

const MAX_DELTA_DEPTH: usize = 64;

/// The error every store read past a [`LayoutReader::set_deadline`]
/// answers. Callers that walk history on a clock match on this to say
/// "truncated" rather than "failed".
pub const READ_BUDGET_EXHAUSTED: &str = "read budget exhausted";

pub fn is_read_budget_exhausted(err: &str) -> bool {
    err.contains(READ_BUDGET_EXHAUSTED)
}

/// A reader over one repo's layout at a manifest snapshot. WAL payloads
/// are fetched lazily and cached for the reader's lifetime (a request),
/// and across requests when the reader was given a [`ReadCache`].
pub struct LayoutReader<'a> {
    pub store: &'a ObjectStore,
    pub prefix: &'a str,
    pub manifest: &'a Manifest,
    plane: Option<Plane>,
    wal_cache: std::cell::RefCell<HashMap<String, WalPack>>,
    /// Which oids each WAL entry carries, by sidecar key. Learned
    /// without touching the pack, so a reader over a long WAL pays one
    /// small GET per entry it has not seen — not one pack download.
    members: std::cell::RefCell<HashMap<String, Arc<HashSet<String>>>>,
    shared: Option<Arc<ReadCache>>,
    /// After this instant a read that would go to the store answers
    /// [`READ_BUDGET_EXHAUSTED`] instead. Cache hits still answer: they
    /// are the reason a walk on a clock gets anywhere.
    deadline: Cell<Option<Instant>>,
}

/// One WAL pack, opened. Which objects it holds is read from the push's
/// oid sidecar, so membership is known before a byte of it is inflated;
/// *where* each one sits is learned entry by entry, in pack order, only
/// as far as a read needs. Identifying an entry means resolving it, and
/// a thin entry's base may live in the plane, in another pack, or later
/// in this one — every one of those lookups re-enters this index, which
/// is why it has to answer partially rather than all at once.
struct WalPack {
    payload: Arc<Vec<u8>>,
    /// Entry offsets in pack order.
    offsets: Vec<u64>,
    /// oid hex -> entry offset, for the entries identified so far.
    by_oid: HashMap<String, u64>,
    /// How many of `offsets` have been claimed for identification. An
    /// entry is claimed before it is resolved, so a lookup re-entered
    /// from inside that resolution moves on to the next one instead of
    /// resolving it again.
    claimed: usize,
}

/// What readers share across requests. Everything in it is immutable by
/// construction — an object's oid *is* its content, and a WAL key names
/// one push forever — so there is nothing to invalidate and a hit can
/// never be stale. The only policy is the byte budget: entries leave in
/// the order they arrived once it is spent.
///
/// Not an LRU on purpose. The reads this serves are a person walking a
/// tree — every object is asked for once or twice, close together — and
/// FIFO under a budget is the same answer with none of the bookkeeping
/// on the hot path.
pub struct ReadCache {
    inner: Mutex<CacheInner>,
    budget: usize,
}

#[derive(Default)]
struct CacheInner {
    objects: HashMap<String, Arc<(u8, Vec<u8>)>>,
    packs: HashMap<String, Arc<Vec<u8>>>,
    members: HashMap<String, Arc<HashSet<String>>>,
    /// Insertion order, with each entry's accounted size.
    order: VecDeque<(Slot, String, usize)>,
    bytes: usize,
}

#[derive(Clone, Copy)]
enum Slot {
    Object,
    Pack,
    Members,
}

impl ReadCache {
    /// A cache that holds at most `budget` bytes of payload. `0` is a
    /// cache that remembers nothing, which is how a test proves a code
    /// path does not depend on one.
    pub fn new(budget: usize) -> ReadCache {
        ReadCache {
            inner: Mutex::new(CacheInner::default()),
            budget,
        }
    }

    /// Bytes currently held.
    pub fn bytes(&self) -> usize {
        self.lock().bytes
    }

    /// Entries currently held, of every kind.
    pub fn len(&self) -> usize {
        self.lock().order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheInner> {
        // A poisoned lock means a panic mid-insert on another thread; the
        // maps are still well-formed (every write is one insert or one
        // remove) and a cache that refuses forever is worse than one that
        // carries on.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn object(&self, oid: &str) -> Option<Arc<(u8, Vec<u8>)>> {
        self.lock().objects.get(oid).cloned()
    }

    fn pack(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        self.lock().packs.get(key).cloned()
    }

    fn members(&self, key: &str) -> Option<Arc<HashSet<String>>> {
        self.lock().members.get(key).cloned()
    }

    fn put_object(&self, oid: &str, obj: Arc<(u8, Vec<u8>)>) {
        let size = obj.1.len();
        let mut g = self.lock();
        if g.objects.insert(oid.to_string(), obj).is_none() {
            g.admit(Slot::Object, oid, size, self.budget);
        }
    }

    fn put_pack(&self, key: &str, payload: Arc<Vec<u8>>) {
        let size = payload.len();
        let mut g = self.lock();
        if g.packs.insert(key.to_string(), payload).is_none() {
            g.admit(Slot::Pack, key, size, self.budget);
        }
    }

    fn put_members(&self, key: &str, set: Arc<HashSet<String>>) {
        // Forty bytes of hex per oid is what the set costs to hold; the
        // sidecar on the wire is half that.
        let size = set.len() * 40;
        let mut g = self.lock();
        if g.members.insert(key.to_string(), set).is_none() {
            g.admit(Slot::Members, key, size, self.budget);
        }
    }
}

impl CacheInner {
    /// Account for a fresh insert and evict, oldest first, until the
    /// budget holds. An entry larger than the whole budget is admitted
    /// and evicted again on the spot — it was never going to fit, and
    /// refusing it up front would need the same accounting for a worse
    /// answer.
    fn admit(&mut self, slot: Slot, key: &str, size: usize, budget: usize) {
        self.order.push_back((slot, key.to_string(), size));
        self.bytes += size;
        // `bytes` is the sum of `order`'s sizes, so an overrun always has
        // a front to evict; the emptiness test is belt to that brace.
        while self.bytes > budget && !self.order.is_empty() {
            let (slot, key, size) = self.order.pop_front().expect("checked non-empty");
            match slot {
                Slot::Object => {
                    self.objects.remove(&key);
                }
                Slot::Pack => {
                    self.packs.remove(&key);
                }
                Slot::Members => {
                    self.members.remove(&key);
                }
            }
            self.bytes -= size;
        }
    }
}

impl<'a> LayoutReader<'a> {
    /// A reader that remembers nothing past its own lifetime.
    pub fn new(
        store: &'a ObjectStore,
        prefix: &'a str,
        manifest: &'a Manifest,
    ) -> Result<LayoutReader<'a>, String> {
        Self::open(store, prefix, manifest, None)
    }

    /// A reader that reads through `cache` and leaves what it fetched
    /// there for the next one.
    pub fn with_cache(
        store: &'a ObjectStore,
        prefix: &'a str,
        manifest: &'a Manifest,
        cache: Arc<ReadCache>,
    ) -> Result<LayoutReader<'a>, String> {
        Self::open(store, prefix, manifest, Some(cache))
    }

    fn open(
        store: &'a ObjectStore,
        prefix: &'a str,
        manifest: &'a Manifest,
        shared: Option<Arc<ReadCache>>,
    ) -> Result<LayoutReader<'a>, String> {
        let plane = match Plane::load(store, prefix) {
            Ok(p) => Some(p),
            Err(e) if crate::errclass::is_absent(&e) => None,
            Err(e) => return Err(crate::errclass::diagnose_store(e)),
        };
        Ok(LayoutReader {
            store,
            prefix,
            manifest,
            plane,
            wal_cache: std::cell::RefCell::new(HashMap::new()),
            members: std::cell::RefCell::new(HashMap::new()),
            shared,
            deadline: Cell::new(None),
        })
    }

    /// Refuse store reads after `at`. `None` lifts the limit. Reads that
    /// the cache can answer are unaffected.
    pub fn set_deadline(&self, at: Option<Instant>) {
        self.deadline.set(at);
    }

    fn check_deadline(&self) -> Result<(), String> {
        match self.deadline.get() {
            Some(at) if Instant::now() >= at => Err(READ_BUDGET_EXHAUSTED.to_string()),
            _ => Ok(()),
        }
    }

    /// Resolve a ref name to its oid (manifest list, then pages).
    pub fn ref_oid(&self, name: &str) -> Result<Option<String>, String> {
        if let Some((_, oid)) = self.manifest.refs.iter().find(|(n, _)| n == name) {
            return Ok(Some(oid.clone()));
        }
        if !self.manifest.ref_pages.is_empty() {
            return stratum_store::refpages::lookup(self.store, self.manifest, name);
        }
        Ok(None)
    }

    /// All refs (manifest ∪ pages), sorted, deduped.
    pub fn all_refs(&self) -> Result<Vec<(String, String)>, String> {
        let mut listed = self.manifest.refs.clone();
        for page in &self.manifest.ref_pages {
            listed.extend(stratum_store::refpages::load_page(self.store, page)?);
        }
        listed.sort();
        listed.dedup_by(|a, b| a.0 == b.0);
        Ok(listed)
    }

    /// Resolve a user-supplied revision: 40-hex oid, full ref name, or a
    /// short name tried as refs/heads/ then refs/tags/.
    pub fn resolve_rev(&self, rev: &str) -> Result<Option<String>, String> {
        if rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(Some(rev.to_lowercase()));
        }
        if rev == "HEAD" {
            return Ok(self.manifest.tip().map(str::to_string));
        }
        if rev.starts_with("refs/") {
            return self.ref_oid(rev);
        }
        if let Some(oid) = self.ref_oid(&format!("refs/heads/{rev}"))? {
            return Ok(Some(oid));
        }
        self.ref_oid(&format!("refs/tags/{rev}"))
    }

    /// Read one object: (kind, bytes), hash-verified.
    pub fn object(&self, oid: &str) -> Result<(u8, Vec<u8>), String> {
        let oid = oid.to_lowercase();
        if let Some(hit) = self.shared.as_ref().and_then(|c| c.object(&oid)) {
            // Verified when it went in; an oid is its content.
            return Ok((hit.0, hit.1.clone()));
        }
        let (kind, data) = self.object_inner(&oid, 0)?;
        let got = hex(&hash_object(kind, &data));
        if got != oid {
            return Err(format!("object {oid}: content hashes to {got}"));
        }
        if let Some(c) = &self.shared {
            c.put_object(&oid, Arc::new((kind, data.clone())));
        }
        Ok((kind, data))
    }

    fn object_inner(&self, oid: &str, depth: usize) -> Result<(u8, Vec<u8>), String> {
        if depth > MAX_DELTA_DEPTH {
            return Err(format!("delta chain deeper than {MAX_DELTA_DEPTH}"));
        }
        // WAL first: entries there are newer truth than the locator, and
        // just-pushed objects exist nowhere else. Membership comes from
        // the sidecar; the pack itself is opened only on a hit.
        for w in &self.manifest.wal {
            if !self.members_of(w)?.contains(oid) {
                continue;
            }
            self.open_wal(w)?;
            let off = self.locate(&w.key, oid, depth)?;
            return self.resolve_at(&w.key, off, depth);
        }
        // Locator plane.
        if let Some(plane) = &self.plane {
            self.check_deadline()?;
            let (t, bytes, _gets) = plane.read_object(self.store, self.store, oid)?;
            return Ok((t, bytes));
        }
        Err(format!("{oid}: not in this layout"))
    }

    /// Every oid one push carried, from its sidecar: one small GET the
    /// first time any reader asks, none after.
    fn members_of(&self, w: &WalEntry) -> Result<Arc<HashSet<String>>, String> {
        if let Some(m) = self.members.borrow().get(&w.oids_key) {
            return Ok(m.clone());
        }
        let set = match self.shared.as_ref().and_then(|c| c.members(&w.oids_key)) {
            Some(m) => m,
            None => {
                self.check_deadline()?;
                let set: HashSet<String> = self
                    .store
                    .get(&w.oids_key)?
                    .as_chunks::<20>()
                    .0
                    .iter()
                    .map(hex)
                    .collect();
                let set = Arc::new(set);
                if let Some(c) = &self.shared {
                    c.put_members(&w.oids_key, set.clone());
                }
                set
            }
        };
        self.members
            .borrow_mut()
            .insert(w.oids_key.clone(), set.clone());
        Ok(set)
    }

    /// Fetch a pack and scan its entry boundaries. Nothing is inflated
    /// here; a pack whose objects a read never touches is never fetched
    /// at all (see `members_of`).
    fn open_wal(&self, w: &WalEntry) -> Result<(), String> {
        if self.wal_cache.borrow().contains_key(&w.key) {
            return Ok(());
        }
        let payload = match self.shared.as_ref().and_then(|c| c.pack(&w.key)) {
            Some(p) => p,
            None => {
                self.check_deadline()?;
                let p = Arc::new(self.store.get(&w.key)?);
                if let Some(c) = &self.shared {
                    c.put_pack(&w.key, p.clone());
                }
                p
            }
        };
        let offsets = scan_pack(&payload, w.entries)?
            .iter()
            .map(|e| e.offset)
            .collect();
        self.wal_cache.borrow_mut().insert(
            w.key.clone(),
            WalPack {
                payload,
                offsets,
                by_oid: HashMap::new(),
                claimed: 0,
            },
        );
        Ok(())
    }

    /// The offset of `oid` in an opened pack whose sidecar lists it,
    /// identifying entries in pack order until it turns up.
    fn locate(&self, key: &str, oid: &str, depth: usize) -> Result<u64, String> {
        loop {
            let (known, next) = {
                let cache = self.wal_cache.borrow();
                let pack = &cache[key];
                (
                    pack.by_oid.get(oid).copied(),
                    pack.offsets.get(pack.claimed).copied(),
                )
            };
            if let Some(off) = known {
                return Ok(off);
            }
            let Some(off) = next else {
                // The sidecar promised an object the entries do not hash
                // to — or resolving it needed itself, which no valid
                // pack does.
                return Err(format!("{oid}: listed by {key} but not among its entries"));
            };
            self.wal_cache.borrow_mut().get_mut(key).unwrap().claimed += 1;
            let (t, bytes) = self.resolve_at(key, off, depth)?;
            self.wal_cache
                .borrow_mut()
                .get_mut(key)
                .unwrap()
                .by_oid
                .insert(hex(&hash_object(t, &bytes)), off);
        }
    }

    /// Resolve one entry of an opened pack to full bytes. An in-pack
    /// delta base already identified is taken by offset; any other base
    /// goes back through the layout, which finds it wherever it is —
    /// including further along this same pack.
    fn resolve_at(&self, key: &str, off: u64, depth: usize) -> Result<(u8, Vec<u8>), String> {
        if depth > MAX_DELTA_DEPTH {
            return Err(format!("delta chain deeper than {MAX_DELTA_DEPTH}"));
        }
        let payload = self.wal_cache.borrow()[key].payload.clone();
        match resolve(&payload, 0, off)? {
            Resolved::Object(t, bytes) => Ok((t, bytes)),
            Resolved::External(base, stack) => {
                let known = self.wal_cache.borrow()[key].by_oid.get(&base).copied();
                let (t, mut obj) = match known {
                    Some(boff) => self.resolve_at(key, boff, depth + 1)?,
                    None => self.object_inner(&base, depth + 1)?,
                };
                for delta in stack {
                    obj = apply_delta(&obj, &delta)?;
                }
                Ok((t, obj))
            }
        }
    }

    /// Walk a path ("a/b/c.txt") from a commit's root tree.
    /// Ok(None) = path absent.
    pub fn entry_at(
        &self,
        commit_oid: &str,
        path: &str,
    ) -> Result<Option<crate::objwrite::TreeEntry>, String> {
        let (kind, data) = self.object(commit_oid)?;
        if kind != crate::objwrite::OBJ_COMMIT {
            return Err(format!("{commit_oid} is not a commit"));
        }
        let commit = crate::objwrite::parse_commit(&data)?;
        let mut tree_oid = commit.tree;
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Ok(Some(crate::objwrite::TreeEntry {
                mode: "40000".into(),
                name: String::new(),
                oid: parse_hex(&tree_oid)?,
            }));
        }
        for (i, part) in parts.iter().enumerate() {
            let (k, tdata) = self.object(&tree_oid)?;
            if k != crate::objwrite::OBJ_TREE {
                return Ok(None);
            }
            let entries = crate::objwrite::parse_tree(&tdata)?;
            let Some(entry) = entries.into_iter().find(|e| e.name == *part) else {
                return Ok(None);
            };
            if i == parts.len() - 1 {
                return Ok(Some(entry));
            }
            if entry.mode != "40000" && entry.mode != "040000" {
                return Ok(None);
            }
            tree_oid = hex(&entry.oid);
        }
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(n: usize) -> Arc<(u8, Vec<u8>)> {
        Arc::new((3, vec![0u8; n]))
    }

    #[test]
    fn the_budget_evicts_oldest_first_and_counts_every_kind() {
        let c = ReadCache::new(100);
        c.put_object("a", obj(40));
        c.put_pack("p", Arc::new(vec![0u8; 40]));
        // One oid: forty bytes of hex.
        c.put_members("m", Arc::new(HashSet::from(["x".repeat(40)])));
        // 40 + 40 + 40 is over the line; the oldest forty went.
        assert_eq!(c.bytes(), 80);
        // `a` left first; the pack and the members stayed.
        assert!(c.object("a").is_none());
        assert!(c.pack("p").is_some());
        assert!(c.members("m").is_some());
        assert_eq!(c.len(), 2);
        assert!(!c.is_empty());
    }

    #[test]
    fn a_repeat_insert_is_not_counted_twice() {
        let c = ReadCache::new(1000);
        c.put_object("a", obj(10));
        c.put_object("a", obj(10));
        c.put_pack("p", Arc::new(vec![1]));
        c.put_pack("p", Arc::new(vec![1]));
        c.put_members("m", Arc::new(HashSet::new()));
        c.put_members("m", Arc::new(HashSet::new()));
        assert_eq!(c.bytes(), 11);
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn an_entry_past_the_whole_budget_does_not_linger() {
        let c = ReadCache::new(10);
        c.put_object("big", obj(50));
        assert_eq!(c.bytes(), 0);
        assert!(c.object("big").is_none());
        assert!(c.is_empty());
    }

    #[test]
    fn a_zero_budget_remembers_nothing() {
        let c = ReadCache::new(0);
        c.put_object("a", obj(1));
        assert!(c.object("a").is_none());
        assert_eq!(c.bytes(), 0);
    }

    #[test]
    fn a_poisoned_lock_still_answers() {
        let c = Arc::new(ReadCache::new(100));
        let c2 = c.clone();
        let _ = std::thread::spawn(move || {
            let _g = c2.inner.lock().unwrap();
            panic!("poison");
        })
        .join();
        c.put_object("a", obj(1));
        assert!(c.object("a").is_some());
    }

    #[test]
    fn the_budget_error_is_recognised_wherever_it_lands() {
        assert!(is_read_budget_exhausted(&format!(
            "GET x: {READ_BUDGET_EXHAUSTED}"
        )));
        assert!(!is_read_budget_exhausted("GET x: HTTP 404"));
    }
}
