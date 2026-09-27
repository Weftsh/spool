//! Scripted HTTP responder for unit-testing client error paths: serves a
//! fixed queue of responses (status, headers, body) one per connection,
//! recording each request head. No TLS, no keep-alive — the store client
//! reconnects per request when the server closes.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

pub struct FakeHttp {
    pub url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

/// One request as it arrived: the full head (request line and headers)
/// and the body bytes. Signed requests are only testable if the headers
/// survive, which is why this is more than the request line.
#[derive(Clone)]
pub struct Recorded {
    pub head: String,
    pub body: Vec<u8>,
}

impl Recorded {
    /// Case-insensitive header lookup, trimmed.
    pub fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    }
}

pub struct Scripted {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Scripted {
    pub fn new(status: u16, body: &[u8]) -> Self {
        Scripted {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

/// A connection-level fault instead of an HTTP response.
pub enum Reply {
    Http(Scripted),
    /// Accept then immediately close (transport error on the client).
    Slam,
}

impl FakeHttp {
    /// Serve `replies` in order, one per connection; further connections
    /// are slammed shut.
    pub fn start(replies: Vec<Reply>) -> FakeHttp {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake http");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        std::thread::spawn(move || {
            let mut queue = replies.into_iter();
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let reply = queue.next();
                // Read the request head (and drain a content-length body so
                // the client never sees a reset mid-upload).
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let head_end = loop {
                    match s.read(&mut tmp) {
                        Ok(0) => break None,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break Some(p + 4);
                            }
                        }
                        Err(_) => break None,
                    }
                };
                let Some(head_end) = head_end else { continue };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let cl: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())?
                    })
                    .unwrap_or(0);
                let mut body = buf[head_end..].to_vec();
                while body.len() < cl {
                    match s.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => body.extend_from_slice(&tmp[..n]),
                    }
                }
                log.lock().unwrap().push(Recorded { head, body });
                match reply {
                    Some(Reply::Http(r)) => {
                        let mut resp = format!(
                            "HTTP/1.1 {} X\r\ncontent-length: {}\r\nconnection: close\r\n",
                            r.status,
                            r.body.len()
                        );
                        for (k, v) in &r.headers {
                            resp.push_str(&format!("{k}: {v}\r\n"));
                        }
                        resp.push_str("\r\n");
                        let _ = s.write_all(resp.as_bytes());
                        let _ = s.write_all(&r.body);
                    }
                    Some(Reply::Slam) | None => { /* drop = RST/EOF */ }
                }
            }
        });
        FakeHttp { url, requests }
    }

    /// Just the request lines, which is all most callers assert on.
    pub fn requests(&self) -> Vec<String> {
        self.recorded()
            .iter()
            .map(|r| r.head.lines().next().unwrap_or("").to_string())
            .collect()
    }

    /// Every request in full — head and body.
    pub fn recorded(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}
