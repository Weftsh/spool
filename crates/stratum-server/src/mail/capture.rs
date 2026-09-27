//! A transport that writes mail to a directory instead of sending it.
//!
//! This is product code rather than a test double, and deliberately: the
//! manual browser pass has to be able to sign up, open the verification
//! mail and click the link, on a machine with no relay and no AWS
//! account. A fake that only existed inside `cargo test` would leave the
//! most important funnel in the product exercised only by unit tests.
//!
//! One JSON file per message, named by arrival order and recipient, so a
//! reader can find the newest mail for an address without parsing all of
//! them.

use super::{Mailer, Message};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Capture {
    dir: PathBuf,
    seq: AtomicU64,
}

impl Capture {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, String> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| format!("mail dir {}: {e}", dir.display()))?;
        // Continue the numbering across restarts rather than overwriting
        // message 1: a walkthrough that restarts the server mid-run must
        // not lose the mail it is about to read.
        let seq = std::fs::read_dir(&dir)
            .map_err(|e| format!("mail dir {}: {e}", dir.display()))?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.split('-').next())
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0);
        Ok(Self {
            dir,
            seq: AtomicU64::new(seq),
        })
    }
}

/// Everything outside `[A-Za-z0-9._-]` becomes `_`, so a recipient
/// cannot steer the write out of the directory or collide with another
/// message's file.
fn safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl Mailer for Capture {
    fn send(&self, msg: &Message) -> Result<(), String> {
        msg.validate()?;
        let n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let path = self.dir.join(format!("{n:06}-{}.json", safe(&msg.to)));
        let blob = serde_json::json!({
            "seq": n,
            "to": msg.to,
            "subject": msg.subject,
            "text": msg.text,
        });
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&blob).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("write {}: {e}", path.display()))
    }

    fn kind(&self) -> &'static str {
        "capture"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("stratum-mail-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&d).ok();
        d
    }

    #[test]
    fn each_message_lands_in_its_own_file_and_survives_a_restart() {
        let dir = tmp("capture");
        let cap = Capture::new(&dir).unwrap();
        assert_eq!(cap.kind(), "capture");
        cap.send(&Message {
            to: "a@example.com".into(),
            subject: "First".into(),
            text: "one".into(),
        })
        .unwrap();
        cap.send(&Message {
            to: "b@example.com".into(),
            subject: "Second".into(),
            text: "two".into(),
        })
        .unwrap();

        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["000001-a_example.com.json", "000002-b_example.com.json"]
        );

        // A second Capture over the same directory continues the
        // numbering rather than overwriting message 1.
        let again = Capture::new(&dir).unwrap();
        again
            .send(&Message {
                to: "c@example.com".into(),
                subject: "Third".into(),
                text: "three".into(),
            })
            .unwrap();
        let blob: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("000003-c_example.com.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(blob["subject"], "Third");
        assert_eq!(blob["to"], "c@example.com");
        assert_eq!(blob["text"], "three");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The filename is derived from a recipient, and a recipient is
    /// attacker-supplied at signup. Traversal and separators must not
    /// survive into the path.
    #[test]
    fn a_hostile_recipient_cannot_steer_the_write() {
        let dir = tmp("capture-hostile");
        let cap = Capture::new(&dir).unwrap();
        cap.send(&Message {
            to: "../../etc/passwd@x".into(),
            subject: "s".into(),
            text: "t".into(),
        })
        .unwrap();
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["000001-.._.._etc_passwd_x.json"]);
        assert!(!dir.join("../../etc/passwd@x").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An unwritable directory is a configuration error, and reported as
    /// one at construction — not on the first signup.
    #[test]
    fn an_unusable_directory_fails_at_construction() {
        let file = tmp("capture-file");
        std::fs::create_dir_all(file.parent().unwrap()).ok();
        std::fs::write(&file, b"not a directory").unwrap();
        let e = Capture::new(&file).err().unwrap();
        assert!(e.contains("mail dir"), "{e}");
        std::fs::remove_file(&file).ok();
    }

    /// Validation runs before anything is written: a forged header does
    /// not leave a file behind.
    #[test]
    fn an_invalid_message_writes_nothing() {
        let dir = tmp("capture-invalid");
        let cap = Capture::new(&dir).unwrap();
        assert!(cap
            .send(&Message {
                to: "a@example.com\nBcc: x@example.com".into(),
                subject: "s".into(),
                text: "t".into(),
            })
            .is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
