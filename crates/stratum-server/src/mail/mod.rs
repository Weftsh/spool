//! Sending mail.
//!
//! Shaped like [`crate::mirror::origin::OriginProvider`] and
//! [`crate::workers::billing::BillingProvider`]: a narrow trait, several
//! transports, chosen by environment at boot. Four exist —
//!
//!  * [`Null`] drops everything, and is the default. A server that has
//!    not been told where to send mail should say so at boot, not
//!    discover it when the first invitation never arrives.
//!  * [`capture::Capture`] writes each message to a file. This is what
//!    the e2e suites and the manual browser pass read, so the way a
//!    person joins — invited, open the mail, follow the link — is
//!    exercised by machines and by people without a mail server.
//!  * [`smtp::Smtp`] speaks the protocol to a relay, for self-hosting.
//!  * [`ses::Ses`] posts to Amazon SES v2, signed with the SigV4 the
//!    object store already uses.
//!
//! **Every address and subject that reaches a transport is checked for
//! CR and LF first** ([`Message::validate`]). A newline in a `To:` is not
//! a formatting bug: in SMTP and in the SES simple-content API alike it
//! ends the header and starts another, so an attacker-chosen address
//! becomes an attacker-chosen `Bcc:`. Forgotten-password takes an address
//! from anybody at all, which is exactly the input this guards.

pub mod capture;
pub mod ses;
pub mod smtp;
pub mod templates;

use std::sync::Arc;

/// One message, already rendered.
///
/// Text only, deliberately: an HTML part doubles the templating surface
/// and every message this product sends is one sentence and one link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub to: String,
    pub subject: String,
    pub text: String,
}

impl Message {
    /// Refuse anything that could forge a header.
    ///
    /// Applied by every transport rather than at the call sites, because
    /// a call site added later would not know to.
    pub fn validate(&self) -> Result<(), String> {
        for (what, value) in [("recipient", &self.to), ("subject", &self.subject)] {
            if value.contains(['\r', '\n']) {
                return Err(format!("mail {what} contains a line break"));
            }
            if value.trim().is_empty() {
                return Err(format!("mail {what} is empty"));
            }
        }
        // Not a header, but an address with no `@` is a configuration
        // mistake worth catching before a relay rejects the envelope.
        if !self.to.contains('@') {
            return Err("mail recipient is not an address".into());
        }
        Ok(())
    }
}

pub trait Mailer: Send + Sync {
    fn send(&self, msg: &Message) -> Result<(), String>;
    /// What this transport is, for `/readyz` and the boot line. A person
    /// debugging "nobody got the email" should be able to see that the
    /// answer is `null` without reading the deployment's environment.
    fn kind(&self) -> &'static str;
}

/// Configured nowhere: messages are dropped.
pub struct Null;

impl Mailer for Null {
    fn send(&self, msg: &Message) -> Result<(), String> {
        msg.validate()
    }
    fn kind(&self) -> &'static str {
        "null"
    }
}

/// Pick a transport from the environment.
///
/// `STRATUM_MAIL_TRANSPORT` is `null` (default), `capture`, `smtp` or
/// `ses`. Anything else is an error at boot rather than a silent
/// fallback to dropping mail — a typo in a deployment variable must not
/// look like a working system.
pub fn from_env() -> Result<Arc<dyn Mailer>, String> {
    let kind = std::env::var("STRATUM_MAIL_TRANSPORT").unwrap_or_else(|_| "null".into());
    let from = || {
        std::env::var("STRATUM_MAIL_FROM")
            .map_err(|_| format!("STRATUM_MAIL_TRANSPORT={kind} needs STRATUM_MAIL_FROM"))
    };
    match kind.as_str() {
        "null" => Ok(Arc::new(Null)),
        "capture" => Ok(Arc::new(capture::Capture::new(
            std::env::var("STRATUM_MAIL_DIR")
                .map_err(|_| "STRATUM_MAIL_TRANSPORT=capture needs STRATUM_MAIL_DIR".to_string())?,
        )?)),
        "smtp" => Ok(Arc::new(smtp::Smtp::from_env(from()?)?)),
        "ses" => Ok(Arc::new(ses::Ses::from_env(from()?)?)),
        other => Err(format!(
            "STRATUM_MAIL_TRANSPORT={other:?} is not one of null, capture, smtp, ses"
        )),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// The header-injection guard, through every field that reaches a
    /// header, in both line-ending spellings.
    #[test]
    fn a_line_break_in_an_address_or_subject_is_refused() {
        let ok = Message {
            to: "someone@example.com".into(),
            subject: "Verify your address".into(),
            text: "hello\nworld\n".into(),
        };
        ok.validate().unwrap();

        for bad in [
            Message {
                to: "a@example.com\r\nBcc: victim@example.com".into(),
                ..ok.clone()
            },
            Message {
                to: "a@example.com\nBcc: victim@example.com".into(),
                ..ok.clone()
            },
            Message {
                subject: "Hi\r\nBcc: victim@example.com".into(),
                ..ok.clone()
            },
            Message {
                subject: "Hi\nX-Header: no".into(),
                ..ok.clone()
            },
            // Empty and whitespace-only are refused too: an empty
            // envelope recipient is a bounce nobody sees.
            Message {
                to: "   ".into(),
                ..ok.clone()
            },
            Message {
                subject: "".into(),
                ..ok.clone()
            },
            Message {
                to: "not-an-address".into(),
                ..ok.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?} passed validation");
        }
        // The body may contain anything: it is not a header.
        Message {
            text: "line\r\nline\r\n.\r\nTo: nobody".into(),
            ..ok.clone()
        }
        .validate()
        .unwrap();
    }

    /// The Null transport still validates, so a deployment that has not
    /// configured mail yet does not hide an injection bug until the day
    /// it configures one.
    #[test]
    fn the_null_transport_drops_but_still_checks() {
        let null = Null;
        assert_eq!(null.kind(), "null");
        null.send(&Message {
            to: "a@example.com".into(),
            subject: "s".into(),
            text: "t".into(),
        })
        .unwrap();
        assert!(null
            .send(&Message {
                to: "a@example.com\nBcc: x@example.com".into(),
                subject: "s".into(),
                text: "t".into(),
            })
            .is_err());
    }

    /// Selection is by exact name, an unknown name is an error, and each
    /// transport that needs configuration says which variable is missing
    /// rather than failing later with a connection error.
    #[test]
    fn transport_selection_is_explicit_about_what_it_needs() {
        let _guard = crate::mail::tests::EnvLock::acquire();
        let vars = [
            "STRATUM_MAIL_TRANSPORT",
            "STRATUM_MAIL_FROM",
            "STRATUM_MAIL_DIR",
            "STRATUM_MAIL_SMTP_HOST",
            "STRATUM_MAIL_SMTP_USER",
            "STRATUM_MAIL_SMTP_PASSWORD",
        ];
        let clear = || {
            for v in vars {
                std::env::remove_var(v);
            }
        };
        clear();
        assert_eq!(from_env().unwrap().kind(), "null");

        std::env::set_var("STRATUM_MAIL_TRANSPORT", "nonsense");
        let e = from_env().err().unwrap();
        assert!(e.contains("null, capture, smtp, ses"), "{e}");

        std::env::set_var("STRATUM_MAIL_TRANSPORT", "capture");
        assert!(from_env().err().unwrap().contains("STRATUM_MAIL_DIR"));
        let dir = std::env::temp_dir().join(format!("stratum-mail-sel-{}", std::process::id()));
        std::env::set_var("STRATUM_MAIL_DIR", &dir);
        assert_eq!(from_env().unwrap().kind(), "capture");
        std::fs::remove_dir_all(&dir).ok();

        // A directory that cannot be created is a boot failure too,
        // reported here rather than on the first message.
        let not_a_dir = dir.join("file");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&not_a_dir, b"x").unwrap();
        std::env::set_var("STRATUM_MAIL_DIR", not_a_dir.join("under"));
        assert!(from_env().err().unwrap().contains("mail dir"));
        std::fs::remove_dir_all(&dir).ok();
        std::env::set_var("STRATUM_MAIL_DIR", &dir);

        for t in ["smtp", "ses"] {
            std::env::set_var("STRATUM_MAIL_TRANSPORT", t);
            let e = from_env().err().unwrap();
            assert!(e.contains("STRATUM_MAIL_FROM"), "{t}: {e}");
        }

        std::env::set_var("STRATUM_MAIL_FROM", "stratum@example.com");
        std::env::set_var("STRATUM_MAIL_TRANSPORT", "smtp");
        assert!(from_env().err().unwrap().contains("STRATUM_MAIL_SMTP_HOST"));
        std::env::set_var("STRATUM_MAIL_SMTP_HOST", "localhost:2525");
        assert_eq!(from_env().unwrap().kind(), "smtp");

        std::env::set_var("STRATUM_MAIL_TRANSPORT", "ses");
        assert_eq!(from_env().unwrap().kind(), "ses");

        clear();
    }

    /// Tests in this module mutate process-wide environment variables,
    /// which cargo runs concurrently by default. One lock, held for the
    /// duration, is cheaper than teaching every test to use a distinct
    /// variable name it would then not be testing.
    pub struct EnvLock(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl EnvLock {
        pub fn acquire() -> Self {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            EnvLock(LOCK.lock().unwrap_or_else(|e| e.into_inner()))
        }
    }
}
