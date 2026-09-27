//! A relay that accepts mail and remembers it.
//!
//! Enough SMTP to be a plausible counterparty — the greeting, a
//! multi-line `EHLO` capability list, `AUTH`, the envelope, `DATA`
//! terminated by a lone dot, and `QUIT` — so a client can be tested over
//! a real socket rather than over a scripted buffer. Optionally refuses
//! at a named stage, which is how the delivery-failure path is exercised
//! without an unreachable host.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// One accepted message, as the relay saw it.
#[derive(Clone, Debug, Default)]
pub struct Delivered {
    pub helo: String,
    pub auth: Option<String>,
    pub mail_from: String,
    pub rcpt_to: Vec<String>,
    /// The `DATA` payload with dot-stuffing undone and CRLF normalised.
    pub data: String,
}

impl Delivered {
    /// A header's value, or `None`. Headers only — the search stops at
    /// the blank line, so a body line that looks like a header is not
    /// mistaken for one.
    pub fn header(&self, name: &str) -> Option<String> {
        self.data.split("\n\n").next()?.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }

    /// Everything after the header block.
    pub fn body(&self) -> String {
        self.data
            .split_once("\n\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default()
    }
}

pub struct FakeSmtp {
    pub host: String,
    delivered: Arc<Mutex<Vec<Delivered>>>,
}

impl FakeSmtp {
    pub fn start() -> FakeSmtp {
        Self::start_refusing(None)
    }

    /// Refuse at a stage: one of `MAIL`, `RCPT`, `DATA`, `AUTH`, `EOD`.
    pub fn start_refusing(refuse: Option<&'static str>) -> FakeSmtp {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake smtp");
        let host = listener.local_addr().unwrap().to_string();
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let log = delivered.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let log = log.clone();
                std::thread::spawn(move || serve(stream, refuse, log));
            }
        });
        FakeSmtp { host, delivered }
    }

    pub fn delivered(&self) -> Vec<Delivered> {
        self.delivered.lock().unwrap().clone()
    }
}

fn serve(stream: std::net::TcpStream, refuse: Option<&str>, log: Arc<Mutex<Vec<Delivered>>>) {
    let Ok(mut w) = stream.try_clone() else {
        return;
    };
    let mut r = BufReader::new(stream);
    let say = |w: &mut std::net::TcpStream, s: &str| {
        let _ = write!(w, "{s}\r\n");
        let _ = w.flush();
    };
    say(&mut w, "220 fake.stratum.test ESMTP");
    let mut msg = Delivered::default();
    loop {
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let line = line.trim_end().to_string();
        let upper = line.to_ascii_uppercase();
        let refused = |stage: &str| refuse == Some(stage);
        if let Some(rest) = upper.strip_prefix("EHLO ") {
            msg.helo = line[5..].to_string();
            let _ = rest;
            say(&mut w, "250-fake.stratum.test");
            say(&mut w, "250-AUTH PLAIN LOGIN");
            say(&mut w, "250 SIZE 10485760");
        } else if let Some(rest) = line.strip_prefix("AUTH PLAIN ") {
            msg.auth = Some(rest.to_string());
            say(
                &mut w,
                if refused("AUTH") {
                    "535 authentication failed"
                } else {
                    "235 2.7.0 authenticated"
                },
            );
        } else if upper.starts_with("MAIL FROM:") {
            msg.mail_from = angled(&line);
            say(
                &mut w,
                if refused("MAIL") {
                    "550 sender rejected"
                } else {
                    "250 2.1.0 ok"
                },
            );
        } else if upper.starts_with("RCPT TO:") {
            msg.rcpt_to.push(angled(&line));
            say(
                &mut w,
                if refused("RCPT") {
                    "550 no such user here"
                } else {
                    "250 2.1.5 ok"
                },
            );
        } else if upper == "DATA" {
            if refused("DATA") {
                say(&mut w, "451 4.3.0 try again later");
                continue;
            }
            say(&mut w, "354 end with <CRLF>.<CRLF>");
            let mut data = String::new();
            loop {
                let mut l = String::new();
                match r.read_line(&mut l) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let l = l.trim_end_matches(['\r', '\n']);
                if l == "." {
                    break;
                }
                data.push_str(l.strip_prefix('.').unwrap_or(l));
                data.push('\n');
            }
            msg.data = data;
            if refused("EOD") {
                say(&mut w, "552 5.3.4 message too big");
                continue;
            }
            say(&mut w, "250 2.0.0 queued as FAKE1");
            log.lock().unwrap().push(std::mem::take(&mut msg));
        } else if upper == "QUIT" {
            say(&mut w, "221 2.0.0 bye");
            return;
        } else {
            say(&mut w, "502 5.5.2 not implemented");
        }
    }
}

fn angled(line: &str) -> String {
    line.split_once('<')
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(addr, _)| addr.to_string())
        .unwrap_or_default()
}
