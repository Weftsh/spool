//! Counting TCP proxy in front of MinIO: forwards bytes untouched and
//! counts HTTP requests in the client→server direction. Latency budgets
//! in e2e tests are asserted as store round-trips ("create ≤ 2 ops"),
//! which is deterministic where wall-clock is not — the research repo's
//! RTT-count methodology carried into the product's regression tests.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub struct CountingProxy {
    /// `http://127.0.0.1:{port}` — point STRATUM_STORE_URL here.
    pub url: String,
    requests: Arc<AtomicUsize>,
}

const METHODS: [&[u8]; 5] = [b"GET ", b"PUT ", b"POST ", b"DELETE ", b"HEAD "];

/// Streaming request-line counter. Tracks the first bytes of the current
/// line across chunk boundaries, so a method split across reads is still
/// one count and body bytes mid-line are never counted. Bodies could in
/// principle contain a line that *looks* like a request (chunked uploads
/// of HTTP-shaped text) — S3 traffic here is packs, JSON and XML, and
/// budgets are upper bounds, so that residual risk is accepted.
struct LineScanner {
    line: Vec<u8>,
    counted: bool,
}

impl LineScanner {
    fn new() -> Self {
        LineScanner {
            line: Vec::new(),
            counted: false,
        }
    }

    fn feed(&mut self, chunk: &[u8], count: &AtomicUsize) {
        for &b in chunk {
            if b == b'\n' {
                self.line.clear();
                self.counted = false;
                continue;
            }
            // Only the first 8 bytes of a line matter for matching.
            if self.line.len() < 8 {
                self.line.push(b);
                if !self.counted && METHODS.iter().any(|m| self.line.starts_with(m)) {
                    count.fetch_add(1, Ordering::SeqCst);
                    self.counted = true;
                }
            }
        }
    }
}

impl CountingProxy {
    /// Spawn a proxy forwarding to `upstream` (a `host:port` addr).
    pub fn start(upstream: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let upstream = upstream.to_string();
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let Ok(server) = TcpStream::connect(&upstream) else {
                    continue;
                };
                let count = counter.clone();
                let (mut c_r, mut c_w) = (client.try_clone().unwrap(), client);
                let (mut s_r, mut s_w) = (server.try_clone().unwrap(), server);
                // client → server, counting request lines.
                std::thread::spawn(move || {
                    let mut buf = [0u8; 16 * 1024];
                    let mut scanner = LineScanner::new();
                    while let Ok(n) = c_r.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        scanner.feed(&buf[..n], &count);
                        if s_w.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    let _ = s_w.shutdown(std::net::Shutdown::Write);
                });
                // server → client, verbatim.
                std::thread::spawn(move || {
                    let mut buf = [0u8; 16 * 1024];
                    while let Ok(n) = s_r.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        if c_w.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    let _ = c_w.shutdown(std::net::Shutdown::Write);
                });
            }
        });
        CountingProxy {
            url: format!("http://127.0.0.1:{port}"),
            requests,
        }
    }

    pub fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// Requests seen since `before` — bracket an operation with two calls.
    pub fn since(&self, before: usize) -> usize {
        self.count() - before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> usize {
        let c = AtomicUsize::new(0);
        let mut s = LineScanner::new();
        for ch in chunks {
            s.feed(ch, &c);
        }
        c.load(Ordering::SeqCst)
    }

    #[test]
    fn counts_keepalive_requests() {
        assert_eq!(
            run(&[b"GET /a HTTP/1.1\r\nHost: x\r\n\r\nGET /b HTTP/1.1\r\n\r\n"]),
            2
        );
    }

    #[test]
    fn counts_split_method() {
        assert_eq!(run(&[b"GE", b"T /a HTTP/1.1\r\n\r\n"]), 1);
        assert_eq!(run(&[b"DELE", b"TE /a HTTP/1.1\r\n\r\n"]), 1);
        assert_eq!(run(&[b"PUT /a HTTP/1.1", b"\r\nGET /b HTTP/1.1\r\n"]), 2);
    }

    #[test]
    fn body_text_not_counted_mid_line() {
        // "GET" not at a line start is body content, not a request.
        assert_eq!(run(&[b"PUT /a HTTP/1.1\r\n\r\nxx GET yy"]), 1);
        // A long body line never re-matches after 8 bytes.
        assert_eq!(
            run(&[b"PUT /a HTTP/1.1\r\n\r\nbinary\x00stuff GET more"]),
            1
        );
    }
}
