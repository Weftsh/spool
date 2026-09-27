//! Fault-injecting store proxy: per-request HTTP proxying in front of
//! MinIO with scripted failures — "the next PUT of manifest.json answers
//! 412", "every request answers 503", "the store is down". This is how
//! e2e tests reach the error and retry arms that a healthy store never
//! exercises. Unlike CountingProxy (transparent, keep-alive) this speaks
//! one request per connection and stamps Connection: close upstream.
//!
//! On top of the scripted rules sits a **seeded plan**: a set of fault
//! classes, each with a probability and an optional key/method scope,
//! drawn from a hash rather than a stream (see `draw`). Scripted rules
//! are consulted first and win outright, so every test written against
//! `inject`/`set_down`/`set_stalled` keeps exactly the determinism it
//! had; the plan only speaks where no rule matched. The two answer
//! different questions. A scripted fault tests the arm someone thought
//! of. A seeded plan tests the arms nobody did, and hands back a seed
//! that reproduces the failure.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The fault classes the proxy can inject. Each is here because it
/// threatens a specific invariant in a way a plain 503 does not; the
/// justification lives on the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fault {
    /// Relay the upstream head with `content-length` (and the end of a
    /// 206's `content-range`) rewritten down, then relay exactly that
    /// many body bytes. This is the *silent* truncation and it is the
    /// one worth testing: simply closing the socket early is already a
    /// loud path, because ureq's `LimitedRead` turns a short read into
    /// `UnexpectedEof` before the caller ever sees the bytes. A
    /// consistent short head is what a proxy, a range-serving CDN or a
    /// half-written object looks like, and nothing at the transport
    /// layer objects. Aimed at ref pages: `refpages.rs` verifies a page
    /// against the sha256 in its own key, so a truncated page must be
    /// *caught*, and this is what proves it.
    Truncate,
    /// Accept the connection, read the request, answer nothing. Worse
    /// than a slammed socket, which every caller notices instantly: a
    /// hang is indistinguishable from a slow store until the client's
    /// own read timeout fires, and it is the shape that exhausts
    /// request permits and wedges health checks. Parked responders
    /// wake within ~25 ms of a heal, and give up after `hang_for`, so a
    /// chaos run cannot leak threads or outlive the test.
    Hang,
    /// Dial upstream, send the request, read and discard the response,
    /// then answer 503. The mutation *landed*; the caller believes it
    /// failed and retries. Threatens I9 and I10 directly: the retry
    /// must not double-apply a ref update or re-append a WAL entry.
    /// This is the fault a store outage cannot simulate, because an
    /// outage stops the write and this one does not.
    ErrorAfter,
    /// Replay the last successful 200 seen for this key. With
    /// `stale_fresh_etag` it replays that old body carrying the
    /// *current* etag, which is what a read-through cache or a CDN in
    /// front of the store does on a revalidation it gets wrong. That
    /// combination is the sharp one: I9 says a writer reads
    /// `(manifest, etag)` once and validates against exactly that
    /// snapshot, and a stale body wearing a fresh etag lets it validate
    /// against old refs and still win the CAS.
    Stale,
    /// 412 on a conditional PUT — a request whose head carries
    /// `if-match` or `if-none-match`. A plan knob for chaos storms
    /// rather than a class needing its own test: `faults_e2e.rs`
    /// already covers scripted CAS rejection four ways.
    CasReject,
    /// Sleep before relaying. Nearly worthless alone; inside a chaos
    /// plan it is the race-window widener that lets the other classes
    /// interleave with concurrent pushers instead of completing before
    /// anyone else gets scheduled.
    Latency,
    /// GET and HEAD fail with 403; PUT and DELETE succeed. walgit's
    /// `black_hole`: writes are accepted and nothing can ever be read
    /// back, which is how a store with a broken read path or a revoked
    /// read grant behaves. It separates "the write failed" from "the
    /// write is invisible", and code that conflates them corrupts.
    ReadDenied,
}

impl Fault {
    pub const ALL: [Fault; 7] = [
        Fault::Truncate,
        Fault::Hang,
        Fault::ErrorAfter,
        Fault::Stale,
        Fault::CasReject,
        Fault::Latency,
        Fault::ReadDenied,
    ];

    fn index(self) -> usize {
        match self {
            Fault::Truncate => 0,
            Fault::Hang => 1,
            Fault::ErrorAfter => 2,
            Fault::Stale => 3,
            Fault::CasReject => 4,
            Fault::Latency => 5,
            Fault::ReadDenied => 6,
        }
    }

    /// The class's lane salt. Every class draws from its own hash lane,
    /// so adding a class — or reordering the enum, or changing another
    /// class's rate — cannot shift the verdicts an existing class
    /// produces for a given seed. The alternative (slicing one hash
    /// into bit fields by position) fails exactly there: the day the
    /// enum grows, every recorded seed quietly means something else and
    /// old failing seeds stop reproducing. The salts are ASCII
    /// mnemonics and must never be renumbered.
    fn lane(self) -> u64 {
        match self {
            Fault::Truncate => 0x5452_554e,   // TRUN
            Fault::Hang => 0x4841_4e47,       // HANG
            Fault::ErrorAfter => 0x4552_4146, // ERAF
            Fault::Stale => 0x5354_414c,      // STAL
            Fault::CasReject => 0x4341_5352,  // CASR
            Fault::Latency => 0x4c41_544e,    // LATN
            Fault::ReadDenied => 0x5244_444e, // RDDN
        }
    }
}

/// Precedence when more than one class fires on the same request. Fixed
/// so that a seed means one thing: the earliest entry that fired wins.
/// `Latency` is absent because it composes — it delays the request and
/// then whatever else fired still happens.
const PRECEDENCE: [Fault; 6] = [
    Fault::Hang,
    Fault::ErrorAfter,
    Fault::ReadDenied,
    Fault::CasReject,
    Fault::Stale,
    Fault::Truncate,
];

/// One class of fault, with its firing rate and its scope.
#[derive(Debug, Clone)]
pub struct FaultRule {
    pub fault: Fault,
    /// 0.0 = never, 1.0 = every matching request.
    pub rate: f64,
    only_keys: Vec<String>,
    only_methods: Vec<String>,
}

impl FaultRule {
    pub fn new(fault: Fault, rate: f64) -> Self {
        FaultRule {
            fault,
            rate,
            only_keys: Vec::new(),
            only_methods: Vec::new(),
        }
    }

    /// Restrict to request paths containing any of these substrings.
    /// Empty (the default) means every key.
    pub fn only_keys<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.only_keys = keys.into_iter().map(Into::into).collect();
        self
    }

    /// Restrict to these HTTP methods, case-insensitively. Empty (the
    /// default) means every method.
    pub fn only_methods<I, S>(mut self, methods: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.only_methods = methods
            .into_iter()
            .map(|m| m.into().to_ascii_uppercase())
            .collect();
        self
    }

    fn scopes(&self, method: &str, key: &str) -> bool {
        let key_ok = self.only_keys.is_empty() || self.only_keys.iter().any(|k| key.contains(k));
        let method_ok =
            self.only_methods.is_empty() || self.only_methods.iter().any(|m| m == method);
        key_ok && method_ok
    }
}

/// A seeded set of fault rules. Handed to `FaultHandle::set_plan`; a
/// failing run reports its seed and re-running with that seed replays
/// the same verdicts, request for request.
#[derive(Debug, Clone)]
pub struct FaultPlan {
    seed: u64,
    rules: Vec<FaultRule>,
    /// Fraction of the real body a `Truncate` relays.
    pub truncate_fraction: f64,
    /// How long a `Latency` fault sleeps.
    pub latency: Duration,
    /// How long a `Hang` parks before giving up and closing.
    pub hang_for: Duration,
    /// Replay the stale body with the *current* etag (see `Fault::Stale`).
    pub stale_fresh_etag: bool,
}

impl FaultPlan {
    pub fn new(seed: u64) -> Self {
        FaultPlan {
            seed,
            rules: Vec::new(),
            truncate_fraction: 0.5,
            latency: Duration::from_millis(40),
            hang_for: Duration::from_secs(3),
            stale_fresh_etag: false,
        }
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn with(mut self, rule: FaultRule) -> Self {
        self.rules.push(rule);
        self
    }

    pub fn truncate_fraction(mut self, f: f64) -> Self {
        self.truncate_fraction = f;
        self
    }

    pub fn latency(mut self, d: Duration) -> Self {
        self.latency = d;
        self
    }

    pub fn hang_for(mut self, d: Duration) -> Self {
        self.hang_for = d;
        self
    }

    pub fn stale_fresh_etag(mut self, yes: bool) -> Self {
        self.stale_fresh_etag = yes;
        self
    }

    /// Everything at once, at `rate`. Truncation is scoped to ref-page
    /// reads because that is where a silent short body is a *content*
    /// bug rather than a transport one — the page names its own sha256,
    /// so a truncated page has to be caught by verification or not at
    /// all. The rest is unscoped on purpose: the point of a storm is the
    /// combinations nobody enumerated.
    pub fn chaos(seed: u64, rate: f64) -> Self {
        FaultPlan::new(seed)
            .with(
                FaultRule::new(Fault::Truncate, rate)
                    .only_keys(["/refs/page-"])
                    .only_methods(["GET"]),
            )
            .with(FaultRule::new(Fault::Hang, rate))
            .with(FaultRule::new(Fault::ErrorAfter, rate))
            .with(FaultRule::new(Fault::CasReject, rate))
            .with(FaultRule::new(Fault::Latency, rate))
            .hang_for(Duration::from_millis(750))
    }

    /// Writes land, reads are refused.
    pub fn black_hole() -> Self {
        FaultPlan::new(0).with(FaultRule::new(Fault::ReadDenied, 1.0))
    }

    /// Every read answers the last body seen for that key, forever. Pass
    /// `fresh_etag` to make it the adversarial variant that also carries
    /// the current etag.
    pub fn stale_forever(fresh_etag: bool) -> Self {
        FaultPlan::new(0)
            .with(FaultRule::new(Fault::Stale, 1.0).only_methods(["GET"]))
            .stale_fresh_etag(fresh_etag)
    }

    /// Every request parks. `hang_for` bounds how long, so this cannot
    /// wedge a suite the way an unbounded `set_stalled` can.
    pub fn stalled(hang_for: Duration) -> Self {
        FaultPlan::new(0)
            .with(FaultRule::new(Fault::Hang, 1.0))
            .hang_for(hang_for)
    }

    /// Whether `fault` fires for the `seq`-th `method` request on `key`.
    /// Public so a test can assert determinism and lane independence
    /// without standing up a proxy.
    pub fn would_fire(&self, fault: Fault, method: &str, key: &str, seq: u64) -> bool {
        self.rules
            .iter()
            .filter(|r| r.fault == fault && r.scopes(method, key))
            .any(|r| fires(self.seed, fault, method, key, seq, r.rate))
    }

    fn verdict(&self, method: &str, key: &str, seq: u64, conditional: bool) -> Verdict {
        let latency = self
            .would_fire(Fault::Latency, method, key, seq)
            .then_some(self.latency);
        let fault = PRECEDENCE.iter().copied().find(|&f| {
            if !self.would_fire(f, method, key, seq) {
                return false;
            }
            // Two classes only make sense on part of the traffic, and
            // scoping that in every caller's plan would be a trap. A
            // read cannot be denied to a writer, and a PUT that carries
            // no precondition has no CAS to reject.
            match f {
                Fault::ReadDenied => matches!(method, "GET" | "HEAD"),
                Fault::CasReject => method == "PUT" && conditional,
                _ => true,
            }
        });
        Verdict { latency, fault }
    }
}

struct Verdict {
    latency: Option<Duration>,
    fault: Option<Fault>,
}

/// The draw for one (class, request) pair.
///
/// Reproducibility under concurrency is the whole design constraint
/// here. walgit steps a single xorshift stream per request, which is
/// deterministic only while requests are serialized: with two pushers in
/// flight, the *ordinal* a given request draws depends on scheduling, so
/// a seed that failed yesterday injects its faults somewhere else today
/// and the seed stops reproducing — which is the only reason to record
/// one.
///
/// So there is no stream. Each verdict is a pure function of the seed,
/// the class's lane, the method, the key and a per-(method, key)
/// counter. The Nth GET of a given key gets the same verdict no matter
/// what else the process is doing, and no matter how the threads
/// interleaved to get there. It is the same idiom `LatencyModel::ttfb`
/// uses in `stratum-store/src/store.rs` to place its 1% long tail.
///
/// The counter is per (method, key) rather than per key on purpose: a
/// shared counter would let a concurrent PUT and GET of the same key
/// swap ordinals and reintroduce exactly the nondeterminism this exists
/// to remove.
fn draw(seed: u64, fault: Fault, method: &str, key: &str, seq: u64) -> u64 {
    let mut h = DefaultHasher::new();
    (seed, fault.lane(), method, key, seq).hash(&mut h);
    h.finish()
}

fn fires(seed: u64, fault: Fault, method: &str, key: &str, seq: u64, rate: f64) -> bool {
    if rate <= 0.0 {
        return false;
    }
    if rate >= 1.0 {
        return true;
    }
    let scale = 1_000_000u64;
    draw(seed, fault, method, key, seq) % scale < (rate * scale as f64) as u64
}

/// Per-class fire counts, snapshotted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats([u64; 7]);

impl Stats {
    pub fn count(&self, fault: Fault) -> u64 {
        self.0[fault.index()]
    }

    pub fn total(&self) -> u64 {
        self.0.iter().sum()
    }
}

type Observer = Arc<dyn Fn(&str, u64) + Send + Sync>;
/// Last successful 200 per key: the head to replay and the body to
/// replay with it.
type StaleCache = Arc<Mutex<HashMap<String, (Head, Vec<u8>)>>>;

#[derive(Clone)]
pub struct FaultHandle {
    rules: Arc<Mutex<Vec<Rule>>>,
    down: Arc<AtomicBool>,
    stalled: Arc<AtomicBool>,
    plan: Arc<Mutex<Option<FaultPlan>>>,
    /// Bumped whenever the plan changes, so parked `Hang` responders
    /// notice a heal instead of waiting out their deadline.
    generation: Arc<AtomicU64>,
    seqs: Arc<Mutex<HashMap<(String, String), u64>>>,
    stale: StaleCache,
    counters: Arc<[AtomicU64; 7]>,
    trace: Arc<Mutex<Vec<(Fault, String)>>>,
    observer: Arc<Mutex<Option<Observer>>>,
}

struct Rule {
    needle: String,
    remaining: usize,
    status: u16,
}

impl FaultHandle {
    /// The next `times` requests whose request line contains every
    /// whitespace-separated term of `needle` (e.g. "PUT manifest.json")
    /// answer `status` with an empty body instead of reaching the store.
    pub fn inject(&self, needle: &str, times: usize, status: u16) {
        self.rules.lock().unwrap().push(Rule {
            needle: needle.into(),
            remaining: times,
            status,
        });
    }

    /// While down, every connection is slammed shut (transport errors).
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// While stalled, a connection is accepted and its request read — and
    /// then nothing is ever written back. This is a different failure from
    /// `set_down`, and a worse one: a slammed connection is an instant
    /// transport error that every caller notices, whereas a stall is
    /// indistinguishable from a slow store until the client's own read
    /// timeout fires. It is the shape that quietly exhausts request
    /// permits and hangs health checks, so it is the shape those defences
    /// have to be tested against.
    ///
    /// Parked responders wake within ~50 ms of `set_stalled(false)`, so a
    /// test can release them without leaking threads past its own end.
    pub fn set_stalled(&self, stalled: bool) {
        self.stalled.store(stalled, Ordering::SeqCst);
    }

    /// Drop all pending rules.
    pub fn clear(&self) {
        self.rules.lock().unwrap().clear();
    }

    /// Install (or replace) the seeded plan. Replacing it releases any
    /// responder currently parked on a `Hang`.
    pub fn set_plan(&self, plan: FaultPlan) {
        *self.plan.lock().unwrap() = Some(plan);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn clear_plan(&self) {
        *self.plan.lock().unwrap() = None;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Everything off: scripted rules, plan, down, stalled. Parked
    /// responders wake and relay normally. Call this before the
    /// convalescence half of a fault test, so what follows is measuring
    /// recovery rather than the tail of the injury.
    pub fn heal(&self) {
        self.clear();
        self.set_down(false);
        self.set_stalled(false);
        self.clear_plan();
    }

    pub fn stats(&self) -> Stats {
        let mut s = Stats::default();
        for f in Fault::ALL {
            s.0[f.index()] = self.counters[f.index()].load(Ordering::SeqCst);
        }
        s
    }

    /// Every fault that fired, with the key it hit, in fire order. This
    /// is what makes a failing seed debuggable: the seed reproduces the
    /// run, the trace says what the run did.
    pub fn trace(&self) -> Vec<(Fault, String)> {
        self.trace.lock().unwrap().clone()
    }

    pub fn reset_stats(&self) {
        for c in self.counters.iter() {
            c.store(0, Ordering::SeqCst);
        }
        self.trace.lock().unwrap().clear();
    }

    /// Called with the request line and the per-(method, key) sequence
    /// before the request is relayed.
    ///
    /// walgit injects one-shot panics at named protocol steps; a proxy
    /// cannot panic our server, which runs as a child process. This is
    /// the stronger tool anyway — the callback can `SIGKILL` the child
    /// at a chosen store operation, so a resumability test faces a real
    /// process death with whatever partial state it left behind, not a
    /// synthetic unwind.
    pub fn observe<F: Fn(&str, u64) + Send + Sync + 'static>(&self, f: F) {
        *self.observer.lock().unwrap() = Some(Arc::new(f));
    }

    pub fn clear_observer(&self) {
        *self.observer.lock().unwrap() = None;
    }

    fn decide(&self, request_line: &str) -> Option<u16> {
        let mut rules = self.rules.lock().unwrap();
        for r in rules.iter_mut() {
            let hit = r.remaining > 0
                && r.needle
                    .split_whitespace()
                    .all(|term| request_line.contains(term));
            if hit {
                r.remaining -= 1;
                return Some(r.status);
            }
        }
        None
    }

    fn next_seq(&self, method: &str, key: &str) -> u64 {
        let mut seqs = self.seqs.lock().unwrap();
        let n = seqs
            .entry((method.to_string(), key.to_string()))
            .or_insert(0);
        let seq = *n;
        *n += 1;
        seq
    }

    fn record(&self, fault: Fault, key: &str) {
        self.counters[fault.index()].fetch_add(1, Ordering::SeqCst);
        self.trace.lock().unwrap().push((fault, key.to_string()));
    }
}

pub struct FaultProxy {
    pub url: String,
    pub handle: FaultHandle,
}

/// A parsed HTTP head — status/request line plus headers, order kept.
/// Faults have to rewrite headers (`content-length` for a truncation,
/// `etag` for the adversarial stale replay), and doing that on a raw
/// byte blob is where proxies grow their own bugs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Head {
    first_line: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn parse(head: &str) -> Head {
        let mut lines = head.split("\r\n").filter(|l| !l.is_empty());
        let first_line = lines.next().unwrap_or_default().to_string();
        let headers = lines
            .filter_map(|l| {
                let (k, v) = l.split_once(':')?;
                Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
            })
            .collect();
        Head {
            first_line,
            headers,
        }
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    fn set(&mut self, name: &str, value: &str) {
        match self.headers.iter_mut().find(|(k, _)| k == name) {
            Some((_, v)) => *v = value.to_string(),
            None => self.headers.push((name.to_string(), value.to_string())),
        }
    }

    fn status(&self) -> Option<u16> {
        self.first_line.split_whitespace().nth(1)?.parse().ok()
    }

    fn render(&self) -> Vec<u8> {
        let mut out = self.first_line.clone();
        out.push_str("\r\n");
        for (k, v) in &self.headers {
            out.push_str(k);
            out.push_str(": ");
            out.push_str(v);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        out.into_bytes()
    }
}

/// `("PUT", "/stratum/data/refs/page-ab.txt")` from a request line. The
/// query string is dropped: it carries presigning noise that would make
/// otherwise-identical requests draw different verdicts.
fn method_and_key(request_line: &str) -> (String, String) {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or_default();
    let key = target.split('?').next().unwrap_or(target).to_string();
    (method, key)
}

fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn read_request(s: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 16 * 1024];
    let head_end = loop {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => return None,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = head_end(&buf) {
                    break p;
                }
            }
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let cl: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buf.len() - head_end < cl {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    Some(buf)
}

/// Rewrite the head to force upstream connection close (so the response
/// relay can stream until EOF).
fn force_close(request: &[u8]) -> Vec<u8> {
    let end = head_end(request).unwrap_or(request.len());
    let head = String::from_utf8_lossy(&request[..end]);
    let mut out = String::new();
    for line in head.split("\r\n") {
        if line.is_empty() || line.to_ascii_lowercase().starts_with("connection:") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("connection: close\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&request[end..]);
    bytes
}

fn canned(status: u16, reason: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
        .into_bytes()
}

/// Read the upstream response head, leaving whatever body bytes arrived
/// with it in the returned tail.
fn read_response_head(server: &mut TcpStream) -> Option<(Head, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 16 * 1024];
    loop {
        match server.read(&mut tmp) {
            Ok(0) | Err(_) => return None,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = head_end(&buf) {
                    let head = Head::parse(&String::from_utf8_lossy(&buf[..p]));
                    return Some((head, buf[p..].to_vec()));
                }
            }
        }
    }
}

/// `bytes 0-99/1000` truncated to `n` bytes → `bytes 0-{start+n-1}/1000`.
/// The total is left alone: a range server that lies about the whole
/// object's size is a different (and louder) fault than one whose range
/// simply comes up short.
fn truncate_content_range(value: &str, n: u64) -> Option<String> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, _end) = range.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    if n == 0 {
        return None;
    }
    Some(format!("bytes {start}-{}/{total}", start + n - 1))
}

/// Stale bodies are held in memory, so a pack-sized object is recorded
/// only up to this and otherwise skipped. Ref pages, manifests and WAL
/// entries — the objects whose staleness attacks I9 — are far below it.
const STALE_MAX_BODY: usize = 8 * 1024 * 1024;

impl FaultProxy {
    pub fn start(upstream: &str) -> FaultProxy {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fault proxy");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = FaultHandle {
            rules: Arc::new(Mutex::new(Vec::new())),
            down: Arc::new(AtomicBool::new(false)),
            stalled: Arc::new(AtomicBool::new(false)),
            plan: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
            seqs: Arc::new(Mutex::new(HashMap::new())),
            stale: Arc::new(Mutex::new(HashMap::new())),
            counters: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
            trace: Arc::new(Mutex::new(Vec::new())),
            observer: Arc::new(Mutex::new(None)),
        };
        let h = handle.clone();
        let upstream = upstream.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(client) = stream else { continue };
                if h.down.load(Ordering::SeqCst) {
                    continue; // drop: connection reset for the client
                }
                let h = h.clone();
                let upstream = upstream.clone();
                std::thread::spawn(move || serve(client, &upstream, &h));
            }
        });
        FaultProxy { url, handle }
    }
}

fn serve(mut client: TcpStream, upstream: &str, h: &FaultHandle) {
    let Some(request) = read_request(&mut client) else {
        return;
    };
    // Hold the connection open, answering nothing, until the test
    // releases the stall. The client sees a store that accepted its
    // request and went quiet.
    while h.stalled.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let line =
        String::from_utf8_lossy(&request[..request.iter().position(|&b| b == b'\r').unwrap_or(0)])
            .to_string();

    // Scripted rules first, and they win outright. Thirteen e2e tests
    // depend on this exact determinism; the plan speaks only where no
    // rule matched.
    if let Some(status) = h.decide(&line) {
        let _ = client.write_all(&canned(status, "Injected"));
        return;
    }

    let (method, key) = method_and_key(&line);
    let seq = h.next_seq(&method, &key);
    if let Some(obs) = h.observer.lock().unwrap().clone() {
        obs(&line, seq);
    }

    let request_head = head_end(&request)
        .map(|p| Head::parse(&String::from_utf8_lossy(&request[..p])))
        .unwrap_or_else(|| Head::parse(&line));
    let conditional = request_head.has("if-match") || request_head.has("if-none-match");

    let (plan, generation) = (
        h.plan.lock().unwrap().clone(),
        h.generation.load(Ordering::SeqCst),
    );
    let Some(plan) = plan else {
        relay(&mut client, upstream, &request, None, h, &key, &method);
        return;
    };
    let verdict = plan.verdict(&method, &key, seq, conditional);

    if let Some(d) = verdict.latency {
        h.record(Fault::Latency, &key);
        std::thread::sleep(d);
    }

    match verdict.fault {
        Some(Fault::Hang) => {
            h.record(Fault::Hang, &key);
            // Park, but never past the plan's deadline and never past a
            // heal: a leaked responder outlives the test, holds the
            // child server's connection open, and turns one seed's
            // failure into the next test's mystery.
            let deadline = Instant::now() + plan.hang_for;
            while Instant::now() < deadline
                && h.generation.load(Ordering::SeqCst) == generation
                && !h.down.load(Ordering::SeqCst)
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            if h.generation.load(Ordering::SeqCst) != generation {
                relay(&mut client, upstream, &request, None, h, &key, &method);
            }
            // Otherwise: close with nothing written, which is what a
            // store that gave up mid-request looks like.
        }
        Some(Fault::ErrorAfter) => {
            h.record(Fault::ErrorAfter, &key);
            // Let the mutation land, discard the answer, then lie.
            if let Ok(mut server) = TcpStream::connect(upstream) {
                if server.write_all(&force_close(&request)).is_ok() {
                    let mut sink = Vec::new();
                    let _ = server.read_to_end(&mut sink);
                }
            }
            let _ = client.write_all(&canned(503, "Injected After Apply"));
        }
        Some(Fault::ReadDenied) => {
            h.record(Fault::ReadDenied, &key);
            let _ = client.write_all(&canned(403, "Injected Read Denied"));
        }
        Some(Fault::CasReject) => {
            h.record(Fault::CasReject, &key);
            let _ = client.write_all(&canned(412, "Injected Precondition Failed"));
        }
        Some(Fault::Stale) => {
            let cached = h.stale.lock().unwrap().get(&key).cloned();
            match cached {
                Some((head, body)) => {
                    h.record(Fault::Stale, &key);
                    if plan.stale_fresh_etag {
                        // Harvest the etag the store would answer with
                        // right now and staple it to the old body. A
                        // reader that trusts the etag as a snapshot
                        // identity now validates against refs that no
                        // longer exist.
                        let fresh = current_etag(upstream, &request);
                        replay_stale(&mut client, head, &body, fresh);
                    } else {
                        replay_stale(&mut client, head, &body, None);
                    }
                }
                // Nothing recorded yet — relay, and record it so the
                // next request has something to go stale with.
                None => relay(&mut client, upstream, &request, None, h, &key, &method),
            }
        }
        Some(Fault::Truncate) => {
            relay(
                &mut client,
                upstream,
                &request,
                Some(plan.truncate_fraction),
                h,
                &key,
                &method,
            );
        }
        Some(Fault::Latency) | None => {
            relay(&mut client, upstream, &request, None, h, &key, &method)
        }
    }
}

fn replay_stale(client: &mut TcpStream, mut head: Head, body: &[u8], fresh_etag: Option<String>) {
    head.set("content-length", &body.len().to_string());
    head.set("connection", "close");
    if let Some(etag) = fresh_etag {
        head.set("etag", &etag);
    }
    let mut out = head.render();
    out.extend_from_slice(body);
    let _ = client.write_all(&out);
}

/// The etag the store would answer *now* for this request's key, fetched
/// by replaying the request head as a HEAD. Best-effort: a store that
/// refuses the probe simply leaves the stale etag in place.
fn current_etag(upstream: &str, request: &[u8]) -> Option<String> {
    let end = head_end(request)?;
    let head = String::from_utf8_lossy(&request[..end]);
    let mut out = String::new();
    for (i, line) in head.split("\r\n").enumerate() {
        if line.is_empty() || line.to_ascii_lowercase().starts_with("connection:") {
            continue;
        }
        if i == 0 {
            // Same target, HEAD instead. SigV4 signs the method, so an
            // unsigned probe may well be refused — hence best-effort.
            let rest = line.split_once(' ').map(|(_, r)| r).unwrap_or(line);
            out.push_str("HEAD ");
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
        out.push_str("\r\n");
    }
    out.push_str("connection: close\r\n\r\n");
    let mut server = TcpStream::connect(upstream).ok()?;
    server.write_all(out.as_bytes()).ok()?;
    let (head, _) = read_response_head(&mut server)?;
    (head.status() == Some(200)).then(|| head.get("etag").map(str::to_string))?
}

/// Relay one request upstream and its response back. `truncate_to` is
/// the fraction of the declared body to actually deliver, with the head
/// rewritten to match so nothing at the transport layer notices.
fn relay(
    client: &mut TcpStream,
    upstream: &str,
    request: &[u8],
    truncate_to: Option<f64>,
    h: &FaultHandle,
    key: &str,
    method: &str,
) {
    let Ok(mut server) = TcpStream::connect(upstream) else {
        return;
    };
    if server.write_all(&force_close(request)).is_err() {
        return;
    }
    let Some((mut head, tail)) = read_response_head(&mut server) else {
        return;
    };

    let declared: Option<u64> = head
        .get("content-length")
        .and_then(|v| v.trim().parse().ok());
    // Truncation needs a declared length to lie about. Without one the
    // body ends at EOF, so cutting it short is just a closed socket —
    // the loud path ureq already turns into UnexpectedEof, and not the
    // fault this class exists to test. Skip rather than pretend.
    let limit = match (truncate_to, declared) {
        (Some(frac), Some(len)) if len > 0 => {
            let n = ((len as f64) * frac.clamp(0.0, 1.0)) as u64;
            let n = n.min(len);
            head.set("content-length", &n.to_string());
            if head.status() == Some(206) {
                let rewritten = head
                    .get("content-range")
                    .and_then(|v| truncate_content_range(v, n));
                if let Some(cr) = rewritten {
                    head.set("content-range", &cr);
                }
            }
            h.record(Fault::Truncate, key);
            Some(n)
        }
        _ => None,
    };

    // Only worth buffering when something might replay it later.
    let recording = method == "GET"
        && head.status() == Some(200)
        && limit.is_none()
        && declared.is_some_and(|l| (l as usize) <= STALE_MAX_BODY)
        && h.plan
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.rules.iter().any(|r| r.fault == Fault::Stale));
    let mut recorded = recording.then(Vec::new);

    if client.write_all(&head.render()).is_err() {
        return;
    }

    let mut written = 0u64;
    let mut chunk = tail;
    let mut buf = [0u8; 16 * 1024];
    loop {
        if !chunk.is_empty() {
            let take = match limit {
                Some(n) => chunk.len().min((n - written) as usize),
                None => chunk.len(),
            };
            if let Some(rec) = recorded.as_mut() {
                rec.extend_from_slice(&chunk[..take]);
            }
            if client.write_all(&chunk[..take]).is_err() {
                return;
            }
            written += take as u64;
            if limit.is_some_and(|n| written >= n) {
                return; // close: the client got exactly what we promised
            }
        }
        match server.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => chunk = buf[..n].to_vec(),
        }
    }

    if let Some(body) = recorded {
        h.stale
            .lock()
            .unwrap()
            .insert(key.to_string(), (head, body));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_with(fault: Fault, rate: f64, seed: u64) -> FaultPlan {
        FaultPlan::new(seed).with(FaultRule::new(fault, rate))
    }

    fn verdicts(plan: &FaultPlan, fault: Fault, key: &str) -> Vec<bool> {
        (0..64)
            .map(|i| plan.would_fire(fault, "GET", key, i))
            .collect()
    }

    #[test]
    fn a_seed_and_a_sequence_number_decide_everything() {
        // The property the whole design exists for: the Nth request on a
        // key gets the same verdict every time, so a seed reproduces
        // under any interleaving. Two independently built plans with the
        // same seed must agree, and asking twice must not advance
        // anything — there is no stream to advance.
        let a = plan_with(Fault::Truncate, 0.5, 99);
        let b = plan_with(Fault::Truncate, 0.5, 99);
        let first = verdicts(&a, Fault::Truncate, "/d/refs/page-aa.txt");
        assert_eq!(first, verdicts(&b, Fault::Truncate, "/d/refs/page-aa.txt"));
        assert_eq!(first, verdicts(&a, Fault::Truncate, "/d/refs/page-aa.txt"));
        // A rate of 0.5 that produced all-or-nothing would pass the
        // equality checks above while testing nothing.
        assert!(first.iter().any(|&v| v) && first.iter().any(|&v| !v));
    }

    #[test]
    fn different_seeds_diverge() {
        let a = verdicts(
            &plan_with(Fault::Hang, 0.5, 1),
            Fault::Hang,
            "/d/manifest.json",
        );
        let b = verdicts(
            &plan_with(Fault::Hang, 0.5, 2),
            Fault::Hang,
            "/d/manifest.json",
        );
        assert_ne!(a, b);
    }

    #[test]
    fn different_keys_and_methods_draw_separately() {
        let p = plan_with(Fault::ErrorAfter, 0.5, 7);
        assert_ne!(
            verdicts(&p, Fault::ErrorAfter, "/d/a.txt"),
            verdicts(&p, Fault::ErrorAfter, "/d/b.txt")
        );
        // Per-(method, key) sequencing: a concurrent PUT must not be
        // able to steal a GET's ordinal.
        let gets: Vec<bool> = (0..32)
            .map(|i| p.would_fire(Fault::ErrorAfter, "GET", "/d/a.txt", i))
            .collect();
        let puts: Vec<bool> = (0..32)
            .map(|i| p.would_fire(Fault::ErrorAfter, "PUT", "/d/a.txt", i))
            .collect();
        assert_ne!(gets, puts);
    }

    #[test]
    fn lanes_are_independent_of_one_another() {
        // The reason for per-class lane salts: a recorded seed has to
        // keep meaning the same thing when the plan grows. Adding other
        // classes — at any rate, in any order — must not move a single
        // one of Truncate's verdicts.
        let alone = plan_with(Fault::Truncate, 0.5, 4242);
        let crowded = plan_with(Fault::Truncate, 0.5, 4242)
            .with(FaultRule::new(Fault::Hang, 0.9))
            .with(FaultRule::new(Fault::ErrorAfter, 0.1))
            .with(FaultRule::new(Fault::Latency, 1.0));
        assert_eq!(
            verdicts(&alone, Fault::Truncate, "/d/refs/page-aa.txt"),
            verdicts(&crowded, Fault::Truncate, "/d/refs/page-aa.txt")
        );
        // That first assertion holds by construction — `would_fire` is a
        // pure function of one class's lane — so on its own it is
        // decoration. This is the half with teeth: every class must draw
        // a *different* sequence from the same seed, key and ordinal.
        // Collapse the lanes to a shared salt and this fails
        // immediately, which is exactly the failure a seed recorded
        // before the enum grew would otherwise suffer in silence.
        let all: Vec<Vec<bool>> = Fault::ALL
            .iter()
            .map(|&f| {
                (0..64)
                    .map(|i| fires(4242, f, "GET", "/d/x", i, 0.5))
                    .collect()
            })
            .collect();
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(
                    all[i],
                    all[j],
                    "{:?} and {:?} share a lane",
                    Fault::ALL[i],
                    Fault::ALL[j]
                );
            }
        }
    }

    #[test]
    fn only_keys_and_only_methods_scope_a_rule() {
        let p = FaultPlan::new(3).with(
            FaultRule::new(Fault::Truncate, 1.0)
                .only_keys(["/refs/page-"])
                .only_methods(["get"]),
        );
        assert!(p.would_fire(Fault::Truncate, "GET", "/d/refs/page-aa.txt", 0));
        assert!(!p.would_fire(Fault::Truncate, "PUT", "/d/refs/page-aa.txt", 0));
        assert!(!p.would_fire(Fault::Truncate, "GET", "/d/manifest.json", 0));
        // No scope at all means everything.
        let open = plan_with(Fault::Truncate, 1.0, 3);
        assert!(open.would_fire(Fault::Truncate, "DELETE", "/anything", 0));
    }

    #[test]
    fn rates_at_the_extremes_are_exact() {
        let never = plan_with(Fault::CasReject, 0.0, 5);
        let always = plan_with(Fault::CasReject, 1.0, 5);
        for i in 0..16 {
            assert!(!never.would_fire(Fault::CasReject, "PUT", "/d/m.json", i));
            assert!(always.would_fire(Fault::CasReject, "PUT", "/d/m.json", i));
        }
    }

    #[test]
    fn precedence_is_fixed_and_method_gated() {
        let p = FaultPlan::new(1)
            .with(FaultRule::new(Fault::Hang, 1.0))
            .with(FaultRule::new(Fault::ErrorAfter, 1.0));
        assert_eq!(
            p.verdict("GET", "/d/m.json", 0, false).fault,
            Some(Fault::Hang)
        );
        // ReadDenied is a read fault; a PUT passes through it.
        let rd = plan_with(Fault::ReadDenied, 1.0, 1);
        assert_eq!(
            rd.verdict("GET", "/d/m.json", 0, false).fault,
            Some(Fault::ReadDenied)
        );
        assert_eq!(rd.verdict("PUT", "/d/m.json", 0, false).fault, None);
        // CasReject needs an actual precondition to reject.
        let cas = plan_with(Fault::CasReject, 1.0, 1);
        assert_eq!(
            cas.verdict("PUT", "/d/m.json", 0, true).fault,
            Some(Fault::CasReject)
        );
        assert_eq!(cas.verdict("PUT", "/d/m.json", 0, false).fault, None);
    }

    #[test]
    fn latency_composes_rather_than_masking() {
        let p = FaultPlan::new(1)
            .with(FaultRule::new(Fault::Latency, 1.0))
            .with(FaultRule::new(Fault::ErrorAfter, 1.0))
            .latency(Duration::from_millis(7));
        let v = p.verdict("PUT", "/d/m.json", 0, false);
        assert_eq!(v.latency, Some(Duration::from_millis(7)));
        assert_eq!(v.fault, Some(Fault::ErrorAfter));
    }

    #[test]
    fn presets_say_what_they_mean() {
        assert!(FaultPlan::black_hole().would_fire(Fault::ReadDenied, "GET", "/k", 0));
        assert!(!FaultPlan::black_hole().would_fire(Fault::Truncate, "GET", "/k", 0));
        let stale = FaultPlan::stale_forever(true);
        assert!(stale.stale_fresh_etag);
        assert!(stale.would_fire(Fault::Stale, "GET", "/k", 0));
        assert!(!stale.would_fire(Fault::Stale, "PUT", "/k", 0));
        let chaos = FaultPlan::chaos(11, 1.0);
        assert_eq!(chaos.seed(), 11);
        assert!(chaos.would_fire(Fault::Truncate, "GET", "/d/refs/page-a.txt", 0));
        assert!(!chaos.would_fire(Fault::Truncate, "GET", "/d/manifest.json", 0));
        assert!(FaultPlan::stalled(Duration::from_millis(5)).would_fire(
            Fault::Hang,
            "GET",
            "/k",
            0
        ));
    }

    #[test]
    fn request_lines_split_into_method_and_key() {
        assert_eq!(
            method_and_key("PUT /stratum/d/refs/page-aa.txt?x=1 HTTP/1.1"),
            ("PUT".into(), "/stratum/d/refs/page-aa.txt".into())
        );
        assert_eq!(
            method_and_key("get /a HTTP/1.1"),
            ("GET".into(), "/a".into())
        );
        assert_eq!(method_and_key(""), (String::new(), String::new()));
    }

    #[test]
    fn heads_round_trip_and_rewrite() {
        let mut h = Head::parse("HTTP/1.1 200 OK\r\nETag: \"abc\"\r\nContent-Length: 5\r\n\r\n");
        assert_eq!(h.status(), Some(200));
        assert_eq!(h.get("etag"), Some("\"abc\""));
        assert!(h.has("content-length"));
        assert!(!h.has("if-match"));
        h.set("etag", "\"zzz\"");
        h.set("connection", "close");
        let rendered = String::from_utf8(h.render()).unwrap();
        assert!(rendered.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(rendered.contains("etag: \"zzz\"\r\n"));
        assert!(rendered.contains("connection: close\r\n"));
        assert!(rendered.ends_with("\r\n\r\n"));
        // Only one etag survives a rewrite.
        assert_eq!(rendered.matches("etag:").count(), 1);
    }

    #[test]
    fn content_range_ends_move_with_the_truncation() {
        assert_eq!(
            truncate_content_range("bytes 0-999/1000", 100).as_deref(),
            Some("bytes 0-99/1000")
        );
        assert_eq!(
            truncate_content_range("bytes 500-999/1000", 10).as_deref(),
            Some("bytes 500-509/1000")
        );
        assert_eq!(truncate_content_range("bytes 0-9/10", 0), None);
        assert_eq!(truncate_content_range("chunks 0-9", 5), None);
    }

    #[test]
    fn scripted_rules_are_consulted_before_the_plan() {
        // The compatibility promise, asserted directly: `decide` is what
        // the request path calls first, and it is untouched by any plan.
        let proxy_handle = handle_for_test();
        proxy_handle.set_plan(FaultPlan::chaos(1, 1.0));
        proxy_handle.inject("PUT manifest.json", 2, 412);
        assert_eq!(
            proxy_handle.decide("PUT /d/manifest.json HTTP/1.1"),
            Some(412)
        );
        assert_eq!(
            proxy_handle.decide("PUT /d/manifest.json HTTP/1.1"),
            Some(412)
        );
        assert_eq!(proxy_handle.decide("PUT /d/manifest.json HTTP/1.1"), None);
        proxy_handle.inject("GET wal", 1, 503);
        proxy_handle.clear();
        assert_eq!(proxy_handle.decide("GET /d/wal/1 HTTP/1.1"), None);
    }

    #[test]
    fn sequences_are_per_method_and_key() {
        let h = handle_for_test();
        assert_eq!(h.next_seq("GET", "/a"), 0);
        assert_eq!(h.next_seq("GET", "/a"), 1);
        assert_eq!(h.next_seq("PUT", "/a"), 0);
        assert_eq!(h.next_seq("GET", "/b"), 0);
        assert_eq!(h.next_seq("GET", "/a"), 2);
    }

    #[test]
    fn stats_and_trace_follow_the_fires() {
        let h = handle_for_test();
        assert_eq!(h.stats().total(), 0);
        h.record(Fault::Truncate, "/d/refs/page-aa.txt");
        h.record(Fault::Hang, "/d/m.json");
        h.record(Fault::Truncate, "/d/refs/page-bb.txt");
        assert_eq!(h.stats().count(Fault::Truncate), 2);
        assert_eq!(h.stats().count(Fault::Hang), 1);
        assert_eq!(h.stats().count(Fault::Stale), 0);
        assert_eq!(h.stats().total(), 3);
        assert_eq!(
            h.trace(),
            vec![
                (Fault::Truncate, "/d/refs/page-aa.txt".to_string()),
                (Fault::Hang, "/d/m.json".to_string()),
                (Fault::Truncate, "/d/refs/page-bb.txt".to_string()),
            ]
        );
        h.reset_stats();
        assert_eq!(h.stats().total(), 0);
        assert!(h.trace().is_empty());
    }

    #[test]
    fn healing_bumps_the_generation_that_wakes_parked_responders() {
        let h = handle_for_test();
        let before = h.generation.load(Ordering::SeqCst);
        h.set_plan(FaultPlan::stalled(Duration::from_secs(30)));
        let parked = h.generation.load(Ordering::SeqCst);
        assert_ne!(before, parked);
        h.heal();
        assert_ne!(parked, h.generation.load(Ordering::SeqCst));
        assert!(h.plan.lock().unwrap().is_none());
        assert!(!h.stalled.load(Ordering::SeqCst));
        assert!(!h.down.load(Ordering::SeqCst));
    }

    #[test]
    fn observers_see_the_request_line_and_its_sequence() {
        let h = handle_for_test();
        let seen: Arc<Mutex<Vec<(String, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        h.observe(move |line, seq| sink.lock().unwrap().push((line.to_string(), seq)));
        let obs = h.observer.lock().unwrap().clone().unwrap();
        obs("GET /a HTTP/1.1", 0);
        obs("GET /a HTTP/1.1", 1);
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert_eq!(seen.lock().unwrap()[1].1, 1);
        h.clear_observer();
        assert!(h.observer.lock().unwrap().is_none());
    }

    /// A minimal upstream that answers a fixed body with a versioned
    /// etag, so a socket-level test can tell a replayed body from a
    /// fresh one. Bumping `version` is how a test says "the store moved
    /// on". Every fault class below is asserted through a real
    /// connection, because the interesting half of a proxy fault is the
    /// bytes on the wire and no amount of verdict-level testing sees
    /// those.
    struct Upstream {
        addr: String,
        version: Arc<AtomicU64>,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Upstream {
        fn start() -> Upstream {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let version = Arc::new(AtomicU64::new(1));
            let seen = Arc::new(Mutex::new(Vec::new()));
            let (v, s) = (version.clone(), seen.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut c) = stream else { continue };
                    let Some(req) = read_request(&mut c) else {
                        continue;
                    };
                    let line = String::from_utf8_lossy(
                        &req[..req.iter().position(|&b| b == b'\r').unwrap_or(0)],
                    )
                    .to_string();
                    s.lock().unwrap().push(line.clone());
                    let n = v.load(Ordering::SeqCst);
                    let (method, _) = method_and_key(&line);
                    let body = format!("body-v{n}").repeat(4);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\netag: \"v{n}\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = c.write_all(head.as_bytes());
                    if method != "HEAD" {
                        let _ = c.write_all(body.as_bytes());
                    }
                }
            });
            Upstream {
                addr,
                version,
                seen,
            }
        }
    }

    /// One request through the proxy; returns the raw response bytes.
    fn request(url: &str, line: &str, extra: &str) -> Vec<u8> {
        let addr = url.trim_start_matches("http://");
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(format!("{line} HTTP/1.1\r\nhost: x\r\n{extra}\r\n").as_bytes())
            .unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        out
    }

    fn split(response: &[u8]) -> (Head, Vec<u8>) {
        let p = head_end(response).expect("a response head");
        (
            Head::parse(&String::from_utf8_lossy(&response[..p])),
            response[p..].to_vec(),
        )
    }

    #[test]
    fn a_truncated_body_is_short_and_says_so() {
        // The whole point of this class: the head agrees with the short
        // body, so nothing at the transport layer objects and the
        // content check has to be the thing that catches it.
        let up = Upstream::start();
        let proxy = FaultProxy::start(&up.addr);
        proxy.handle.set_plan(
            FaultPlan::new(1)
                .with(FaultRule::new(Fault::Truncate, 1.0).only_keys(["/refs/page-"]))
                .truncate_fraction(0.25),
        );
        let (head, body) = split(&request(&proxy.url, "GET /d/refs/page-aa.txt", ""));
        let full = "body-v1".repeat(4).len();
        assert_eq!(body.len(), full / 4);
        assert_eq!(
            head.get("content-length")
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            body.len()
        );
        assert!(body.starts_with(b"body-v1"));
        assert_eq!(proxy.handle.stats().count(Fault::Truncate), 1);

        // Out of scope: whole body, untouched.
        let (head, body) = split(&request(&proxy.url, "GET /d/manifest.json", ""));
        assert_eq!(body.len(), full);
        assert_eq!(
            head.get("content-length")
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            full
        );
        assert_eq!(proxy.handle.stats().count(Fault::Truncate), 1);
    }

    #[test]
    fn a_read_denied_store_still_takes_writes() {
        let up = Upstream::start();
        let proxy = FaultProxy::start(&up.addr);
        proxy.handle.set_plan(FaultPlan::black_hole());
        let (head, _) = split(&request(&proxy.url, "GET /d/manifest.json", ""));
        assert_eq!(head.status(), Some(403));
        assert!(
            up.seen.lock().unwrap().is_empty(),
            "no read reached upstream"
        );
        let (head, _) = split(&request(&proxy.url, "PUT /d/manifest.json", ""));
        assert_eq!(head.status(), Some(200));
        assert_eq!(up.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn error_after_lets_the_write_land_before_it_lies() {
        // The dangerous shape: upstream applied the mutation, the caller
        // was told 503 and will retry.
        let up = Upstream::start();
        let proxy = FaultProxy::start(&up.addr);
        proxy
            .handle
            .set_plan(FaultPlan::new(1).with(FaultRule::new(Fault::ErrorAfter, 1.0)));
        let (head, _) = split(&request(&proxy.url, "PUT /d/manifest.json", ""));
        assert_eq!(head.status(), Some(503));
        assert_eq!(
            up.seen.lock().unwrap().len(),
            1,
            "the PUT reached the store"
        );
        assert_eq!(proxy.handle.stats().count(Fault::ErrorAfter), 1);
    }

    #[test]
    fn a_stale_body_can_arrive_wearing_the_current_etag() {
        // I9's nightmare in one test: the body is yesterday's, the etag
        // is today's, and a reader that treats the etag as the snapshot
        // identity will validate against refs that no longer exist.
        let up = Upstream::start();
        let proxy = FaultProxy::start(&up.addr);
        proxy.handle.set_plan(FaultPlan::stale_forever(true));

        // First read has nothing to go stale with: it relays and records.
        let (head, body) = split(&request(&proxy.url, "GET /d/manifest.json", ""));
        assert_eq!(head.get("etag"), Some("\"v1\""));
        assert_eq!(body, "body-v1".repeat(4).as_bytes());

        up.version.store(2, Ordering::SeqCst);
        let (head, body) = split(&request(&proxy.url, "GET /d/manifest.json", ""));
        assert_eq!(body, "body-v1".repeat(4).as_bytes(), "the old body");
        assert_eq!(head.get("etag"), Some("\"v2\""), "the current etag");
        assert_eq!(
            head.get("content-length")
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            body.len()
        );
        assert_eq!(proxy.handle.stats().count(Fault::Stale), 1);

        // Without the adversarial flag the etag is stale too, which a
        // conditional writer would at least notice.
        proxy.handle.set_plan(FaultPlan::stale_forever(false));
        let (head, body) = split(&request(&proxy.url, "GET /d/manifest.json", ""));
        assert_eq!(body, "body-v1".repeat(4).as_bytes());
        assert_eq!(head.get("etag"), Some("\"v1\""));
    }

    #[test]
    fn a_hang_answers_nothing_and_then_releases() {
        let up = Upstream::start();
        let proxy = FaultProxy::start(&up.addr);
        proxy
            .handle
            .set_plan(FaultPlan::stalled(Duration::from_millis(300)));
        let started = Instant::now();
        let response = request(&proxy.url, "GET /d/manifest.json", "");
        assert!(response.is_empty(), "nothing was ever written back");
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert_eq!(proxy.handle.stats().count(Fault::Hang), 1);

        // Healing releases a parked responder rather than making it wait
        // out its deadline — a leaked one outlives the test.
        proxy
            .handle
            .set_plan(FaultPlan::stalled(Duration::from_secs(30)));
        let url = proxy.url.clone();
        let waiter = std::thread::spawn(move || request(&url, "GET /d/manifest.json", ""));
        std::thread::sleep(Duration::from_millis(150));
        let healed = Instant::now();
        proxy.handle.heal();
        let (head, _) = split(&waiter.join().unwrap());
        assert_eq!(head.status(), Some(200));
        assert!(healed.elapsed() < Duration::from_secs(5), "woke promptly");
    }

    #[test]
    fn a_conditional_put_is_the_only_thing_cas_reject_touches() {
        let up = Upstream::start();
        let proxy = FaultProxy::start(&up.addr);
        proxy
            .handle
            .set_plan(FaultPlan::new(1).with(FaultRule::new(Fault::CasReject, 1.0)));
        let (head, _) = split(&request(
            &proxy.url,
            "PUT /d/manifest.json",
            "if-match: \"v1\"\r\n",
        ));
        assert_eq!(head.status(), Some(412));
        let (head, _) = split(&request(&proxy.url, "PUT /d/manifest.json", ""));
        assert_eq!(
            head.status(),
            Some(200),
            "no precondition, nothing to reject"
        );
    }

    #[test]
    fn an_observer_sees_every_relayed_request_in_order() {
        let up = Upstream::start();
        let proxy = FaultProxy::start(&up.addr);
        let seen: Arc<Mutex<Vec<(String, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        proxy
            .handle
            .observe(move |line, seq| sink.lock().unwrap().push((line.to_string(), seq)));
        request(&proxy.url, "GET /d/manifest.json", "");
        request(&proxy.url, "GET /d/manifest.json", "");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].0.starts_with("GET /d/manifest.json"));
        assert_eq!((seen[0].1, seen[1].1), (0, 1));
    }

    #[test]
    fn scripted_rules_beat_the_plan_on_the_wire() {
        let up = Upstream::start();
        let proxy = FaultProxy::start(&up.addr);
        proxy.handle.set_plan(FaultPlan::black_hole());
        proxy.handle.inject("GET manifest.json", 1, 503);
        let (head, _) = split(&request(&proxy.url, "GET /d/manifest.json", ""));
        assert_eq!(head.status(), Some(503), "the script, not the plan's 403");
        let (head, _) = split(&request(&proxy.url, "GET /d/manifest.json", ""));
        assert_eq!(head.status(), Some(403), "rule spent, plan speaks");
    }

    /// A handle with no listener behind it: everything asserted above is
    /// a decision, and decisions do not need a socket.
    fn handle_for_test() -> FaultHandle {
        FaultHandle {
            rules: Arc::new(Mutex::new(Vec::new())),
            down: Arc::new(AtomicBool::new(false)),
            stalled: Arc::new(AtomicBool::new(false)),
            plan: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
            seqs: Arc::new(Mutex::new(HashMap::new())),
            stale: Arc::new(Mutex::new(HashMap::new())),
            counters: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
            trace: Arc::new(Mutex::new(Vec::new())),
            observer: Arc::new(Mutex::new(None)),
        }
    }
}
