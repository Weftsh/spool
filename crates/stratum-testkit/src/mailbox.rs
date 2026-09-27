//! Reading the mail the server captured.
//!
//! Pairs with the server's `capture` transport: point
//! `STRATUM_MAIL_TRANSPORT=capture` and `STRATUM_MAIL_DIR` at a
//! directory, then read what landed. Tests assert on it; the manual
//! browser walkthrough opens the link out of it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct CapturedMail {
    pub seq: u64,
    pub to: String,
    pub subject: String,
    pub text: String,
}

impl CapturedMail {
    /// The first `http(s)` URL in the body — every message this product
    /// sends has exactly one, and it is the whole point of the message.
    pub fn link(&self) -> Option<String> {
        let start = self
            .text
            .find("http://")
            .or_else(|| self.text.find("https://"))?;
        let rest = &self.text[start..];
        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

pub struct Mailbox {
    dir: PathBuf,
}

impl Mailbox {
    /// An empty mailbox in a fresh directory.
    pub fn new(dir: impl Into<PathBuf>) -> Mailbox {
        let dir = dir.into();
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("create mailbox dir");
        Mailbox { dir }
    }

    /// A mailbox in a per-process temp directory, named for the suite.
    pub fn temp(hint: &str) -> Mailbox {
        Mailbox::new(
            std::env::temp_dir().join(format!("stratum-mail-{hint}-{}", std::process::id())),
        )
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// What the server needs in its environment to send here.
    pub fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("STRATUM_MAIL_TRANSPORT", "capture".to_string()),
            ("STRATUM_MAIL_DIR", self.dir.display().to_string()),
            ("STRATUM_MAIL_FROM", "no-reply@stratum.test".to_string()),
        ]
    }

    /// Everything captured, oldest first.
    pub fn all(&self) -> Vec<CapturedMail> {
        let mut out: Vec<CapturedMail> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let raw = std::fs::read_to_string(e.path()).ok()?;
                let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
                Some(CapturedMail {
                    seq: v["seq"].as_u64()?,
                    to: v["to"].as_str()?.to_string(),
                    subject: v["subject"].as_str()?.to_string(),
                    text: v["text"].as_str()?.to_string(),
                })
            })
            .collect();
        out.sort_by_key(|m| m.seq);
        out
    }

    pub fn to(&self, address: &str) -> Vec<CapturedMail> {
        self.all()
            .into_iter()
            .filter(|m| m.to.eq_ignore_ascii_case(address))
            .collect()
    }

    /// Block until a message arrives for `address`, or panic. The
    /// transport writes synchronously inside the request, so this is a
    /// short wait for a file to appear, not a poll for a background job.
    pub fn wait_for(&self, address: &str, within: Duration) -> CapturedMail {
        let deadline = Instant::now() + within;
        loop {
            if let Some(m) = self.to(address).pop() {
                return m;
            }
            assert!(
                Instant::now() < deadline,
                "no mail for {address} within {within:?}; mailbox holds {:?}",
                self.all().iter().map(|m| m.to.clone()).collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
