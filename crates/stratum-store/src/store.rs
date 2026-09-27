use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::time::{Duration, Instant};

/// Injected latency/throughput model (docs/storage-model.md). Applied
/// client-side so local MinIO can stand in for S3 Standard / Express One
/// Zone. `None` = raw backend (correctness tests only).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LatencyModel {
    None,
    /// S3 Standard: 35ms TTFB p50, 150ms on 1% of requests, 90 MB/s cap.
    Standard,
    /// S3 Express One Zone: 4ms TTFB p50, 15ms on 1%, 200 MB/s cap.
    Express,
    /// DynamoDB-class KV (H2's manifest/locator tier): 3ms p50, 15ms on 1%.
    Kv,
}

impl LatencyModel {
    // STRATUM-CORE DIVERGENCE: latency models are benchmark scaffolding; the
    // product build compiles the env hook out unless `bench-models` is on.
    #[cfg(feature = "bench-models")]
    pub fn from_env() -> Self {
        match std::env::var("STRATUM_LATENCY_MODEL").as_deref() {
            Ok("standard") => Self::Standard,
            Ok("express") => Self::Express,
            _ => Self::None,
        }
    }

    #[cfg(not(feature = "bench-models"))]
    pub fn from_env() -> Self {
        Self::None
    }

    fn ttfb(&self, key: &str, seq: u64) -> Duration {
        // Deterministic 1% long tail: hash of (key, per-process request seq).
        let mut h = DefaultHasher::new();
        (key, seq).hash(&mut h);
        let tail = h.finish() % 100 == 0;
        match self {
            Self::None => Duration::ZERO,
            Self::Standard => Duration::from_millis(if tail { 150 } else { 35 }),
            Self::Express => Duration::from_millis(if tail { 15 } else { 4 }),
            Self::Kv => Duration::from_millis(if tail { 15 } else { 3 }),
        }
    }

    fn bytes_per_sec(&self) -> Option<f64> {
        match self {
            Self::None => None,
            Self::Standard => Some(90e6),
            Self::Express => Some(200e6),
            Self::Kv => None,
        }
    }
}

/// Minimal S3-compatible client: GET / GET-with-Range / conditional PUT
/// against `base_url` (e.g. http://127.0.0.1:9000/stratum). Requests are
/// SigV4-signed when AWS credentials are in the environment
/// (AWS_ACCESS_KEY_ID etc. — see sig.rs); anonymous otherwise.
pub struct ObjectStore {
    base_url: String,
    /// Host header value ("host[:port]") — must match what the HTTP layer
    /// sends, since SigV4 signs it.
    host: String,
    /// URL path prefix including the bucket ("/stratum"), part of the
    /// canonical URI.
    path_prefix: String,
    signer: Option<crate::sig::SigV4>,
    model: LatencyModel,
    agent: ureq::Agent,
    requests: std::sync::atomic::AtomicU64,
}

/// Read timeout for ordinary traffic. Clones stream whole segments, so
/// this is generous by design.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

impl ObjectStore {
    pub fn new(base_url: &str, model: LatencyModel) -> Self {
        Self::with_timeout(base_url, model, DEFAULT_TIMEOUT)
    }

    /// STRATUM-CORE DIVERGENCE: a constructor that bounds the read
    /// timeout. Health probing needs a *short* one. `readyz` proves the
    /// store is reachable, and with the 300 s default a store that
    /// accepts connections but never answers makes the health check
    /// itself hang for five minutes instead of reporting 503 — so no load
    /// balancer ever evicts the instance, and the fleet degrades silently
    /// rather than shedding the bad task. A probe that cannot fail fast
    /// is not a probe.
    pub fn with_timeout(base_url: &str, model: LatencyModel, timeout: Duration) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        let rest = base_url
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(&base_url);
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        Self {
            host: host.to_string(),
            path_prefix: if path.is_empty() {
                String::new()
            } else {
                format!("/{path}")
            },
            signer: crate::sig::SigV4::from_env(),
            base_url,
            model,
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(5))
                .timeout(timeout)
                .build(),
            requests: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// SigV4-sign `req` for `key` if credentials are configured.
    fn signed(
        &self,
        mut req: ureq::Request,
        method: &str,
        key: &str,
        payload_sha256: &str,
    ) -> Result<ureq::Request, String> {
        if let Some(signer) = &self.signer {
            let path = format!("{}/{key}", self.path_prefix);
            let hdrs = signer.sign(method, &self.host, &path, payload_sha256)?;
            for (name, value) in hdrs.headers {
                req = req.set(name, &value);
            }
        }
        Ok(req)
    }

    pub fn from_env() -> Self {
        let base = std::env::var("STRATUM_STORE_URL")
            .expect("STRATUM_STORE_URL must be set (e.g. http://127.0.0.1:9000/stratum)");
        Self::new(&base, LatencyModel::from_env())
    }

    /// Number of GET requests issued by this process so far.
    pub fn request_count(&self) -> u64 {
        self.requests.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// GET an object (optionally a byte range), returning a streaming reader
    /// with the latency model applied.
    pub fn get_stream(
        &self,
        key: &str,
        range: Option<(u64, u64)>, // inclusive start..end byte offsets
    ) -> Result<Box<dyn Read + Send>, String> {
        let seq = self
            .requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ttfb = self.model.ttfb(key, seq);
        if !ttfb.is_zero() {
            std::thread::sleep(ttfb);
        }
        let url = format!("{}/{}", self.base_url, key);
        // Transient failures (throttling, connection resets) get two retries
        // with backoff; 4xx never retries.
        let mut attempt = 0;
        let resp = loop {
            let mut req = self
                .signed(self.agent.get(&url), "GET", key, crate::sig::EMPTY_SHA256)
                .map_err(|e| format!("GET {key}: {e}"))?;
            if let Some((start, end)) = range {
                req = req.set("Range", &format!("bytes={start}-{end}"));
            }
            match req.call() {
                Ok(r) => break r,
                Err(ureq::Error::Status(code, _)) if code < 500 => {
                    return Err(format!("GET {key}: HTTP {code}"));
                }
                Err(e) if attempt < 2 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(100 << attempt));
                    let _ = e;
                }
                Err(e) => return Err(format!("GET {key}: {e}")),
            }
        };
        let status = resp.status();
        // A store or proxy that ignores Range and returns 200 would silently
        // shift every offset we resolve against — fail loudly instead.
        match (range.is_some(), status) {
            (true, 206) | (false, 200) => {}
            (true, other) => return Err(format!("GET {key}: expected 206 for range, got {other}")),
            (false, other) => return Err(format!("GET {key}: HTTP {other}")),
        }
        let reader = resp.into_reader();
        Ok(match self.model.bytes_per_sec() {
            None => Box::new(reader),
            Some(rate) => Box::new(Throttled::new(reader, rate)),
        })
    }

    /// GET a whole object into memory.
    pub fn get(&self, key: &str) -> Result<Vec<u8>, String> {
        let mut buf = Vec::new();
        self.get_stream(key, None)?
            .read_to_end(&mut buf)
            .map_err(|e| format!("read {key}: {e}"))?;
        Ok(buf)
    }

    /// GET a whole object plus its ETag (the CAS token for a later put).
    pub fn get_with_etag(&self, key: &str) -> Result<(Vec<u8>, String), String> {
        let url = format!("{}/{}", self.base_url, key);
        let resp = self
            .signed(self.agent.get(&url), "GET", key, crate::sig::EMPTY_SHA256)
            .map_err(|e| format!("GET {key}: {e}"))?
            .call()
            // STRATUM-CORE DIVERGENCE: status errors formatted like
            // get_stream's ("HTTP <code>") so 404 detection is uniform.
            .map_err(|e| match e {
                ureq::Error::Status(code, _) => format!("GET {key}: HTTP {code}"),
                e => format!("GET {key}: {e}"),
            })?;
        let etag = resp.header("ETag").unwrap_or_default().to_string();
        let mut buf = Vec::new();
        resp.into_reader()
            .read_to_end(&mut buf)
            .map_err(|e| format!("read {key}: {e}"))?;
        Ok((buf, etag))
    }

    /// PUT an object, optionally guarded by a write condition. The store
    /// must enforce the condition (verified against MinIO: 412 on
    /// violated If-None-Match:* and stale If-Match).
    pub fn put(&self, key: &str, body: &[u8], cond: PutCond) -> Result<(), PutError> {
        let url = format!("{}/{}", self.base_url, key);
        let mut req = self
            .signed(
                self.agent.put(&url),
                "PUT",
                key,
                &crate::sig::sha256_hex(body),
            )
            .map_err(PutError::Other)?;
        match &cond {
            PutCond::None => {}
            PutCond::IfNoneMatchStar => req = req.set("If-None-Match", "*"),
            PutCond::IfMatch(etag) => req = req.set("If-Match", etag),
        }
        match req.send_bytes(body) {
            Ok(_) => Ok(()),
            // STRATUM-CORE DIVERGENCE: 409 alongside 412. Real S3 answers
            // 409 ConditionalRequestConflict when two conditional writes
            // to one key overlap, and 412 only when the precondition
            // genuinely failed; a racing pair can see 409 then 412 on the
            // retry. Both mean "someone else won, re-read and retry",
            // which is exactly `Conflict`. MinIO never emits 409, so the
            // research harness never saw it — but the product runs a
            // multi-node fleet against real S3, where mapping it to
            // `Other` drops the loser of a manifest CAS out of the retry
            // loop and fails a push that should have re-run.
            Err(ureq::Error::Status(412, _)) | Err(ureq::Error::Status(409, _)) => {
                Err(PutError::Conflict)
            }
            Err(e) => Err(PutError::Other(format!("PUT {key}: {e}"))),
        }
    }
}

/// STRATUM-CORE DIVERGENCE: LIST and DELETE, needed by epoch GC and
/// deleted-repo sweeps (the research brief's §8.6 surface allows LIST;
/// the harness never needed it at runtime).
impl ObjectStore {
    /// List keys under `prefix` via ListObjectsV2, fully paginated.
    /// Returns (key, last_modified_rfc3339) pairs.
    pub fn list(&self, prefix: &str) -> Result<Vec<(String, String)>, String> {
        Ok(self
            .list_entries(prefix)?
            .into_iter()
            .map(|e| (e.key, e.last_modified))
            .collect())
    }

    /// LIST every key under `prefix` with its size in bytes, as the
    /// store reports it.
    ///
    /// This is the *physical* view of a repository: what the bucket
    /// actually holds, including exports, CDN packs and audit shards
    /// that no manifest references. It is never what a customer is
    /// billed for — the manifest's byte counts are — but it is the
    /// number that catches a manifest that has quietly lost track of
    /// what it wrote, or a sweep that has stopped deleting. See
    /// `stratum-server`'s storage inventory.
    pub fn list_sized(&self, prefix: &str) -> Result<Vec<(String, u64)>, String> {
        Ok(self
            .list_entries(prefix)?
            .into_iter()
            .map(|e| (e.key, e.size))
            .collect())
    }

    fn list_entries(&self, prefix: &str) -> Result<Vec<ListEntry>, String> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query: Vec<(&str, &str)> =
                vec![("list-type", "2"), ("prefix", prefix), ("max-keys", "1000")];
            let tok = token.clone();
            if let Some(t) = tok.as_deref() {
                query.push(("continuation-token", t));
            }
            let qs = crate::sig::canonical_query(&query);
            let url = format!("{}://{}{}?{qs}", self.scheme(), self.host, self.path_prefix);
            let mut req = self.agent.get(&url);
            if let Some(signer) = &self.signer {
                let path = if self.path_prefix.is_empty() {
                    "/".to_string()
                } else {
                    self.path_prefix.clone()
                };
                let hdrs = signer.sign_with_query(
                    "GET",
                    &self.host,
                    &path,
                    &query,
                    crate::sig::EMPTY_SHA256,
                )?;
                for (name, value) in hdrs.headers {
                    req = req.set(name, &value);
                }
            }
            let resp = req.call().map_err(|e| match e {
                ureq::Error::Status(code, _) => format!("LIST {prefix}: HTTP {code}"),
                e => format!("LIST {prefix}: {e}"),
            })?;
            let body = resp
                .into_string()
                .map_err(|e| format!("LIST {prefix}: {e}"))?;
            out.extend(parse_list_xml(&body));
            match (
                xml_tag(&body, "IsTruncated").as_deref(),
                xml_tag(&body, "NextContinuationToken"),
            ) {
                (Some("true"), Some(t)) => token = Some(t),
                _ => break,
            }
        }
        Ok(out)
    }

    /// DELETE one object (idempotent — S3 204s for absent keys).
    pub fn delete(&self, key: &str) -> Result<(), String> {
        let url = format!("{}/{}", self.base_url, key);
        let req = self
            .signed(
                self.agent.delete(&url),
                "DELETE",
                key,
                crate::sig::EMPTY_SHA256,
            )
            .map_err(|e| format!("DELETE {key}: {e}"))?;
        match req.call() {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(404, _)) => Ok(()),
            Err(ureq::Error::Status(code, _)) => Err(format!("DELETE {key}: HTTP {code}")),
            Err(e) => Err(format!("DELETE {key}: {e}")),
        }
    }

    fn scheme(&self) -> &str {
        if self.base_url.starts_with("https://") {
            "https"
        } else {
            "http"
        }
    }
}

/// Extract every <Key>/<LastModified> pair from a ListObjectsV2 response.
/// Keys in our stores are URI-unreserved, so no XML entities appear.
/// One `<Contents>` element of a ListObjectsV2 page: the three fields
/// anything here reads. `<Size>` is what S3 and MinIO both emit for the
/// object's byte length; an absent or unparsable one reads as zero
/// rather than failing the whole listing, because a GC sweep that
/// cannot list is worse than an inventory that undercounts one key.
struct ListEntry {
    key: String,
    last_modified: String,
    size: u64,
}

fn parse_list_xml(body: &str) -> Vec<ListEntry> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("<Contents>") {
        let Some(end) = rest[start..].find("</Contents>") else {
            break;
        };
        let chunk = &rest[start..start + end];
        if let Some(key) = xml_tag(chunk, "Key") {
            out.push(ListEntry {
                key,
                last_modified: xml_tag(chunk, "LastModified").unwrap_or_default(),
                size: xml_tag(chunk, "Size")
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0),
            });
        }
        rest = &rest[start + end + 11..];
    }
    out
}

fn xml_tag(chunk: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let s = chunk.find(&open)? + open.len();
    let e = chunk[s..].find(&close)? + s;
    Some(chunk[s..e].to_string())
}

/// Write condition for ObjectStore::put.
pub enum PutCond {
    None,
    /// Create-only: fail with Conflict if the key already exists.
    IfNoneMatchStar,
    /// Compare-and-swap: fail with Conflict unless the stored ETag matches.
    IfMatch(String),
}

#[derive(Debug)]
pub enum PutError {
    /// The write condition failed (concurrent writer won).
    Conflict,
    Other(String),
}

impl std::fmt::Display for PutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PutError::Conflict => write!(f, "conditional write conflict"),
            PutError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// Caps sustained read throughput at `rate` bytes/sec: after n bytes, if
/// wall-clock is ahead of n/rate, sleep the difference.
struct Throttled<R> {
    inner: R,
    rate: f64,
    started: Option<Instant>,
    consumed: u64,
}

impl<R> Throttled<R> {
    fn new(inner: R, rate: f64) -> Self {
        Self {
            inner,
            rate,
            started: None,
            consumed: 0,
        }
    }
}

impl<R: Read> Read for Throttled<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let start = *self.started.get_or_insert_with(Instant::now);
        self.consumed += n as u64;
        let due = Duration::from_secs_f64(self.consumed as f64 / self.rate);
        let elapsed = start.elapsed();
        if due > elapsed {
            std::thread::sleep(due - elapsed);
        }
        Ok(n)
    }
}
