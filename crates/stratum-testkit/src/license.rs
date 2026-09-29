//! A stand-in for Weft's license service, and a signer for the keys it
//! issues.
//!
//! **This is a belief.** It answers Spool's daily check the way we
//! believe the real service does — exactly three fields or a 400, an
//! unknown license a 404, otherwise `{status, notice?}` — and it signs
//! keys the way we believe the service signs them. The license service's
//! own `spool_e2e` suite runs this server's `admin license-install` and
//! `license-check` against the real thing; that is where the belief is
//! checked, and where it wins if the two disagree.

use crate::oidc::b64url;
use ed25519_dalek::{Signer, SigningKey};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The kid the test signing key is trusted under.
pub const KID: &str = "spool-test";

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

/// The test key's public half, as PEM SPKI — what the server under test
/// is told to trust in `STRATUM_DEV_LICENSE_PUBLIC_KEYS`.
pub fn public_pem() -> String {
    let mut der = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    der.extend_from_slice(signing_key().verifying_key().as_bytes());
    format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        stratum_store::b64::encode(&der)
    )
}

/// The environment a server needs to trust the test key.
pub fn trust_env() -> Vec<(&'static str, String)> {
    vec![
        ("STRATUM_DEV_MODE", "1".into()),
        (
            "STRATUM_DEV_LICENSE_PUBLIC_KEYS",
            serde_json::json!({ KID: public_pem() }).to_string(),
        ),
    ]
}

/// `YYYY-MM-DDTHH:MM:SSZ`, `days` from now.
pub fn stamp(days: i64) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + days * 86_400;
    let (z, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil-from-days (Hinnant).
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// A year's online team key for `lid`, covering twenty people — the
/// payload the service writes, field for field.
pub fn payload(lid: &str) -> serde_json::Value {
    serde_json::json!({
        "v": 1, "kid": KID, "lid": lid, "entity": "Acme GmbH", "tier": "team",
        "accounts": [], "maxConcurrent": 20, "mode": "online",
        "iat": stamp(0), "exp": stamp(365),
    })
}

/// `weft_lic_v1.<payload>.<signature>`, signed with the test key.
pub fn sign(payload: &serde_json::Value) -> String {
    sign_with(payload, &signing_key())
}

/// The same, with another key — one the server does not trust.
pub fn sign_with_seed(payload: &serde_json::Value, seed: u8) -> String {
    sign_with(payload, &SigningKey::from_bytes(&[seed; 32]))
}

fn sign_with(payload: &serde_json::Value, key: &SigningKey) -> String {
    let part = b64url(payload.to_string().as_bytes());
    let sig = key.sign(part.as_bytes());
    format!("weft_lic_v1.{part}.{}", b64url(&sig.to_bytes()))
}

/// How the fake answers a license it knows.
#[derive(Debug, Clone)]
pub enum Answer {
    Status {
        status: String,
        notice: Option<String>,
    },
    /// An HTTP status with no answer in it — a service having a bad day.
    Fails(u16),
}

impl Answer {
    pub fn status(status: &str, notice: Option<&str>) -> Answer {
        Answer::Status {
            status: status.into(),
            notice: notice.map(str::to_string),
        }
    }
}

pub struct FakeLicense {
    /// `http://127.0.0.1:<port>/v1/spool/check`.
    pub endpoint: String,
    answers: Arc<Mutex<HashMap<String, Answer>>>,
    received: Arc<Mutex<Vec<serde_json::Value>>>,
    shutdown: Arc<AtomicBool>,
    addr: String,
}

impl Drop for FakeLicense {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(&self.addr);
    }
}

impl FakeLicense {
    /// Answer checks for `lid` with `answer` from now on.
    pub fn answer(&self, lid: &str, answer: Answer) {
        self.answers.lock().unwrap().insert(lid.into(), answer);
    }

    /// Every body that reached the check, in order, as JSON — including
    /// ones it refused.
    pub fn received(&self) -> Vec<serde_json::Value> {
        self.received.lock().unwrap().clone()
    }
}

pub fn spawn() -> FakeLicense {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake license service");
    let addr = listener.local_addr().unwrap().to_string();
    let answers: Arc<Mutex<HashMap<String, Answer>>> = Arc::default();
    let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let shutdown = Arc::new(AtomicBool::new(false));
    let (a, r, stop) = (answers.clone(), received.clone(), shutdown.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(mut stream) = stream else { continue };
            let (a, r) = (a.clone(), r.clone());
            std::thread::spawn(move || {
                let _ = serve(&mut stream, &a, &r);
            });
        }
    });
    FakeLicense {
        endpoint: format!("http://{addr}/v1/spool/check"),
        answers,
        received,
        shutdown,
        addr,
    }
}

fn serve(
    stream: &mut TcpStream,
    answers: &Mutex<HashMap<String, Answer>>,
    received: &Mutex<Vec<serde_json::Value>>,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let length: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buf.len() < header_end + 4 + length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = &buf[header_end + 4..(header_end + 4 + length).min(buf.len())];
    let request_line = head.lines().next().unwrap_or_default();
    let reply = if !request_line.starts_with("POST /v1/spool/check ") {
        (404, serde_json::json!({ "error": "not found" }))
    } else {
        let raw: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        received.lock().unwrap().push(raw.clone());
        answer(&raw, answers)
    };
    let text = reply.1.to_string();
    write!(
        stream,
        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        reply.0,
        text.len()
    )
}

/// Exactly `keyId`, `version` and `people`, of the right types, or 400;
/// a license it was told about, or 404; otherwise its answer.
fn answer(
    raw: &serde_json::Value,
    answers: &Mutex<HashMap<String, Answer>>,
) -> (u16, serde_json::Value) {
    let refuse = |why: &str| (400, serde_json::json!({ "error": why }));
    let Some(obj) = raw.as_object() else {
        return refuse("the check takes exactly keyId, version and people");
    };
    let mut names: Vec<&str> = obj.keys().map(String::as_str).collect();
    names.sort_unstable();
    if names != ["keyId", "people", "version"] {
        return refuse("the check takes exactly keyId, version and people");
    }
    let (Some(lid), Some(_), Some(_)) = (
        obj["keyId"].as_str(),
        obj["version"].as_str().filter(|v| !v.is_empty()),
        obj["people"].as_u64(),
    ) else {
        return refuse("keyId and version are strings, people a whole number");
    };
    match answers.lock().unwrap().get(lid) {
        None => (404, serde_json::json!({ "error": "no such license" })),
        Some(Answer::Fails(code)) => (*code, serde_json::json!({ "error": "unavailable" })),
        Some(Answer::Status { status, notice }) => {
            let mut out = serde_json::json!({ "status": status });
            if let Some(n) = notice {
                out["notice"] = n.clone().into();
            }
            (200, out)
        }
    }
}
