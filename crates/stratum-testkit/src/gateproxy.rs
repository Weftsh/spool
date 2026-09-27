//! A store proxy that can hold one response open mid-body, and that
//! remembers every key it forwarded.
//!
//! The two proxies that came before it cannot do this. `CountingProxy` is
//! transparent and keep-alive: it counts request lines and never gets in
//! the way. `FaultProxy` can stall, but it stalls *before* answering — the
//! client sees a store that accepted a request and went quiet, which is
//! the shape a timeout defence has to survive. Neither can put a reader
//! halfway through a segment and leave it there.
//!
//! That halfway point is where the epoch invariants live. I8 says epoch
//! data is immutable once a pointer references it, so a clone that loaded
//! a manifest may keep issuing ranged GETs against its keys while another
//! node compacts and swaps the pointer out from under it. Proving that
//! needs a reader parked mid-stream, a swap on a different node, and then
//! the reader let go — in that order, deterministically. `stall_get`
//! parks it, `wait_stalled` is the rendezvous that says the parking
//! actually happened (never a sleep: a sleep either flakes or lies), and
//! `release` lets it finish.
//!
//! `keys()` is the other half. I15 says a reader derives every data key
//! from `locator.hdr`'s epoch and never mixes it with the manifest's, and
//! the only way to see that from outside is to look at the keys one
//! in-flight request actually touched.
//!
//! Like `FaultProxy` this speaks one request per connection and stamps
//! `connection: close` upstream, so a response body relays until EOF and
//! a parked responder holds exactly one request.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Longest a parked responder will ever hold, whatever the test does.
/// `release` normally wakes it within a millisecond; this is the backstop
/// that keeps a forgotten (or panicked-past) release from leaving threads
/// parked after the test binary has moved on.
const MAX_PARK: Duration = Duration::from_secs(120);

pub struct GateProxy {
    /// `http://127.0.0.1:{port}` — point `STRATUM_STORE_URL` here.
    pub url: String,
    pub handle: GateHandle,
}

#[derive(Clone)]
pub struct GateHandle(Arc<Gate>);

struct Gate {
    /// Every `(method, key)` forwarded, in the order the proxy saw it.
    seen: Mutex<Vec<(String, String)>>,
    /// The one-shot stall rule, claimed by the first request that matches.
    rule: Mutex<Option<Rule>>,
    state: Mutex<State>,
    cv: Condvar,
}

struct Rule {
    needle: String,
    after_bytes: usize,
}

#[derive(Default)]
struct State {
    /// A responder has relayed its prefix and is now parked.
    engaged: bool,
    released: bool,
}

impl GateHandle {
    /// The next GET whose request line contains every whitespace-separated
    /// term of `needle` relays its headers and `after_bytes` of body, then
    /// parks until `release`. One-shot: later matching requests pass
    /// through untouched, so a clone that reads a segment twice stalls
    /// once.
    pub fn stall_get(&self, needle: &str, after_bytes: usize) {
        *self.0.rule.lock().unwrap() = Some(Rule {
            needle: needle.to_string(),
            after_bytes,
        });
    }

    /// Block until a request is parked at the stall point, or panic.
    ///
    /// This is the rendezvous the whole design exists for. A test that
    /// slept here would be asserting on a guess: too short and the swap
    /// races the reader instead of happening under it, too long and every
    /// run pays for the worst case. Panicking on timeout is deliberate —
    /// if nothing reached the stall, the needle missed or the body was
    /// shorter than `after_bytes`, and either way the test that follows
    /// would prove nothing.
    pub fn wait_stalled(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut st = self.0.state.lock().unwrap();
        while !st.engaged {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "no request reached the stall point within {timeout:?} \
                 (needle never matched, or the body was shorter than the prefix)"
            );
            st = self.0.cv.wait_timeout(st, left).unwrap().0;
        }
    }

    /// Let the parked responder finish its body.
    pub fn release(&self) {
        let mut st = self.0.state.lock().unwrap();
        st.released = true;
        self.0.cv.notify_all();
    }

    /// Every `(method, key)` forwarded so far, in order. Keys are the
    /// request path with any query string removed, so a bucket listing
    /// and a bucket GET read the same.
    pub fn keys(&self) -> Vec<(String, String)> {
        self.0.seen.lock().unwrap().clone()
    }

    /// Forget everything recorded so far — bracket one operation with
    /// this and `keys()` to see only its traffic.
    pub fn clear(&self) {
        self.0.seen.lock().unwrap().clear();
    }

    fn record(&self, method: &str, key: &str) {
        self.0
            .seen
            .lock()
            .unwrap()
            .push((method.to_string(), key.to_string()));
    }

    /// Claim the stall rule if this request line matches it.
    fn claim(&self, line: &str) -> Option<usize> {
        let mut rule = self.0.rule.lock().unwrap();
        let hit = rule.as_ref().is_some_and(|r| {
            line.starts_with("GET ") && r.needle.split_whitespace().all(|t| line.contains(t))
        });
        hit.then(|| rule.take().unwrap().after_bytes)
    }

    fn park(&self) {
        let mut st = self.0.state.lock().unwrap();
        st.engaged = true;
        self.0.cv.notify_all();
        let deadline = Instant::now() + MAX_PARK;
        while !st.released {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            st = self.0.cv.wait_timeout(st, left).unwrap().0;
        }
    }
}

impl Drop for GateProxy {
    /// A test that panics between `stall_get` and `release` would
    /// otherwise leave a responder parked for `MAX_PARK`, holding a
    /// server thread with it. Dropping the proxy lets everyone go.
    fn drop(&mut self) {
        self.handle.release();
    }
}

impl GateProxy {
    pub fn start(upstream: &str) -> GateProxy {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind gate proxy");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = GateHandle(Arc::new(Gate {
            seen: Mutex::new(Vec::new()),
            rule: Mutex::new(None),
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
        }));
        let h = handle.clone();
        let upstream = upstream.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(client) = stream else { continue };
                let h = h.clone();
                let upstream = upstream.clone();
                std::thread::spawn(move || serve_one(client, &upstream, &h));
            }
        });
        GateProxy { url, handle }
    }
}

fn serve_one(mut client: TcpStream, upstream: &str, h: &GateHandle) {
    let Some(request) = read_head(&mut client, true) else {
        return;
    };
    let line = first_line(&request.head);
    let (method, key) = split_request_line(&line);
    h.record(&method, &key);
    let stall_at = h.claim(&line);

    let Ok(mut server) = TcpStream::connect(upstream) else {
        return;
    };
    let mut wire = force_close(&request.head);
    wire.extend_from_slice(&request.rest);
    if server.write_all(&wire).is_err() {
        return;
    }
    let Some(response) = read_head(&mut server, false) else {
        return;
    };
    if client.write_all(&response.head).is_err() {
        return;
    }

    let mut relay = Relay {
        client: &mut client,
        written: 0,
        engaged: false,
        stall_at,
        gate: h,
    };
    if !relay.feed(&response.rest) {
        return;
    }
    let mut buf = [0u8; 16 * 1024];
    while let Ok(n) = server.read(&mut buf) {
        if n == 0 {
            break;
        }
        if !relay.feed(&buf[..n]) {
            return;
        }
    }
    let _ = client.shutdown(std::net::Shutdown::Write);
}

struct Relay<'a> {
    client: &'a mut TcpStream,
    written: usize,
    engaged: bool,
    stall_at: Option<usize>,
    gate: &'a GateHandle,
}

impl Relay<'_> {
    /// Write body bytes through, parking exactly once when the running
    /// total first reaches the stall prefix. Returns false once the
    /// client has gone away.
    fn feed(&mut self, data: &[u8]) -> bool {
        let mut d = data;
        if let Some(limit) = self.stall_at {
            if !self.engaged && self.written + d.len() >= limit {
                let take = limit - self.written;
                if self.client.write_all(&d[..take]).is_err() {
                    return false;
                }
                self.written += take;
                self.engaged = true;
                self.gate.park();
                d = &d[take..];
            }
        }
        if d.is_empty() {
            return true;
        }
        if self.client.write_all(d).is_err() {
            return false;
        }
        self.written += d.len();
        true
    }
}

struct Head {
    head: Vec<u8>,
    /// Bytes read past the header terminator — body, already in hand.
    rest: Vec<u8>,
}

/// Read up to and including `\r\n\r\n`. When `with_body` is set, keep
/// reading until `content-length` bytes of body are in hand as well, so a
/// PUT is forwarded whole rather than in halves.
fn read_head(s: &mut TcpStream, with_body: bool) -> Option<Head> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 16 * 1024];
    let end = loop {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => return None,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = find(&buf, b"\r\n\r\n") {
                    break p + 4;
                }
            }
        }
    };
    let head = buf[..end].to_vec();
    let mut rest = buf[end..].to_vec();
    if with_body {
        let want = content_length(&head).unwrap_or(0);
        while rest.len() < want {
            match s.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => rest.extend_from_slice(&tmp[..n]),
            }
        }
    }
    Some(Head { head, rest })
}

fn content_length(head: &[u8]) -> Option<usize> {
    String::from_utf8_lossy(head).lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case("content-length")
            .then(|| v.trim().parse().ok())?
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn first_line(head: &[u8]) -> String {
    String::from_utf8_lossy(head)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// `("GET", "/bucket/o/…/cold-0000.seg")` — query string dropped.
fn split_request_line(line: &str) -> (String, String) {
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default();
    let key = path.split('?').next().unwrap_or_default().to_string();
    (method, key)
}

/// Force `connection: close` upstream so the response relays until EOF
/// and one connection carries exactly one request.
fn force_close(head: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(head);
    let mut out = String::new();
    for line in text.split("\r\n") {
        if line.is_empty() || line.to_ascii_lowercase().starts_with("connection:") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("connection: close\r\n\r\n");
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_lines_split_into_method_and_key() {
        let (m, k) = split_request_line("GET /b/o/1/r/2/prod/e/cold-0000.seg?x=1 HTTP/1.1");
        assert_eq!(m, "GET");
        assert_eq!(k, "/b/o/1/r/2/prod/e/cold-0000.seg");
    }

    #[test]
    fn upstream_head_is_rewritten_to_close() {
        let head = b"GET /a HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n";
        let out = String::from_utf8(force_close(head)).unwrap();
        assert!(out.ends_with("connection: close\r\n\r\n"), "{out}");
        assert_eq!(
            out.to_lowercase().matches("connection:").count(),
            1,
            "{out}"
        );
    }
}
