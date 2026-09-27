//! SMTP, hand-rolled, for self-hosting.
//!
//! Same posture as the GitHub App JWT and the CloudFront signature: a
//! few hundred lines against a stable wire format, rather than a
//! dependency tree. The protocol here is deliberately the minimum that
//! delivers a one-part text message — `EHLO`, optional `AUTH PLAIN`,
//! `MAIL FROM`, `RCPT TO`, `DATA`, `QUIT`.
//!
//! ## Why there is no STARTTLS
//!
//! Nothing in this workspace links a TLS client library — `ureq` owns
//! its own and does not lend it out — so this transport speaks cleartext
//! SMTP. That is the normal self-hosting arrangement (a relay on
//! `localhost:25`, or a sidecar on a private network) and it is fine
//! *until credentials are involved*. So [`Smtp::from_env`] **refuses to
//! send `AUTH` to a non-loopback host** unless the operator states that
//! the link is already private
//! (`STRATUM_MAIL_SMTP_ALLOW_CLEARTEXT_AUTH=1`). Failing at boot with an
//! explanation beats mailing a relay password across a datacentre in the
//! clear, and beats a transport that silently drops the auth.
//!
//! For hosted deployments, use [`super::ses::Ses`], which is HTTPS.

use super::{Mailer, Message};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Long enough for a relay doing a synchronous spam check, short enough
/// that a wedged relay cannot pin a request thread indefinitely.
const TIMEOUT: Duration = Duration::from_secs(20);

pub struct Smtp {
    pub host: String,
    pub from: String,
    pub auth: Option<(String, String)>,
    /// Announced in `EHLO`. Some relays reject a bare IP literal.
    pub helo: String,
}

impl Smtp {
    pub fn from_env(from: String) -> Result<Self, String> {
        let host = std::env::var("STRATUM_MAIL_SMTP_HOST")
            .map_err(|_| "STRATUM_MAIL_TRANSPORT=smtp needs STRATUM_MAIL_SMTP_HOST".to_string())?;
        let host = if host.contains(':') {
            host
        } else {
            format!("{host}:25")
        };
        let auth =
            match (
                std::env::var("STRATUM_MAIL_SMTP_USER").ok(),
                std::env::var("STRATUM_MAIL_SMTP_PASSWORD").ok(),
            ) {
                (Some(u), Some(p)) => Some((u, p)),
                (None, None) => None,
                _ => return Err(
                    "STRATUM_MAIL_SMTP_USER and STRATUM_MAIL_SMTP_PASSWORD must be set together"
                        .into(),
                ),
            };
        if auth.is_some()
            && !is_loopback(&host)
            && std::env::var("STRATUM_MAIL_SMTP_ALLOW_CLEARTEXT_AUTH").as_deref() != Ok("1")
        {
            return Err(format!(
                "refusing to send SMTP credentials in the clear to {host}: this build has no \
                 STARTTLS. Use a loopback or sidecar relay, use STRATUM_MAIL_TRANSPORT=ses, or \
                 set STRATUM_MAIL_SMTP_ALLOW_CLEARTEXT_AUTH=1 if the link is already private"
            ));
        }
        Ok(Self {
            helo: std::env::var("STRATUM_MAIL_SMTP_HELO").unwrap_or_else(|_| "stratum".into()),
            host,
            from,
            auth,
        })
    }
}

/// Whether the configured host is the local machine, judged on the name
/// as written. A hostname that *resolves* to loopback is not enough:
/// resolution can change under us, and the point is that the operator
/// wrote down something that cannot leave the box.
fn is_loopback(host: &str) -> bool {
    let name = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    let name = name.trim_start_matches('[').trim_end_matches(']');
    name.eq_ignore_ascii_case("localhost")
        || name == "::1"
        || name
            .parse::<std::net::Ipv4Addr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

impl Mailer for Smtp {
    fn send(&self, msg: &Message) -> Result<(), String> {
        msg.validate()?;
        let stream =
            TcpStream::connect(&self.host).map_err(|e| format!("smtp {}: {e}", self.host))?;
        stream
            .set_read_timeout(Some(TIMEOUT))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(TIMEOUT))
            .map_err(|e| e.to_string())?;
        let mut w = stream.try_clone().map_err(|e| e.to_string())?;
        let mut r = BufReader::new(stream);
        converse(
            &mut r,
            &mut w,
            &self.helo,
            &self.from,
            self.auth.as_ref().map(|(u, p)| (u.as_str(), p.as_str())),
            msg,
            now_secs(),
        )
    }

    fn kind(&self) -> &'static str {
        "smtp"
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The whole protocol, over anything readable and writable.
///
/// Split out from [`Smtp::send`] so the conversation — including every
/// way a relay can refuse — is testable without a socket, and so the
/// only line that needs a real network is the `connect` above.
#[allow(clippy::too_many_arguments)]
pub fn converse<R: BufRead, W: Write>(
    r: &mut R,
    w: &mut W,
    helo: &str,
    from: &str,
    auth: Option<(&str, &str)>,
    msg: &Message,
    now: u64,
) -> Result<(), String> {
    expect(r, 220, "greeting")?;
    say(w, &format!("EHLO {helo}"))?;
    expect(r, 250, "EHLO")?;
    if let Some((user, pass)) = auth {
        // AUTH PLAIN's payload is NUL-separated, which is why it cannot
        // simply be sent as a header-safe string.
        let mut blob = Vec::new();
        blob.push(0u8);
        blob.extend_from_slice(user.as_bytes());
        blob.push(0u8);
        blob.extend_from_slice(pass.as_bytes());
        let auth_line = format!("AUTH PLAIN {}", stratum_store::b64::encode(&blob));
        say(w, &auth_line)?;
        expect(r, 235, "AUTH")?;
    }
    say(w, &format!("MAIL FROM:<{from}>"))?;
    expect(r, 250, "MAIL FROM")?;
    say(w, &format!("RCPT TO:<{}>", msg.to))?;
    expect(r, 250, "RCPT TO")?;
    say(w, "DATA")?;
    expect(r, 354, "DATA")?;
    w.write_all(rfc5322(from, msg, now).as_bytes())
        .map_err(|e| format!("smtp body: {e}"))?;
    say(w, ".")?;
    expect(r, 250, "end of data")?;
    // A relay that refuses QUIT has still accepted the message, so the
    // send succeeded; closing the socket is enough.
    say(w, "QUIT").ok();
    Ok(())
}

fn say<W: Write>(w: &mut W, line: &str) -> Result<(), String> {
    write!(w, "{line}\r\n").map_err(|e| format!("smtp write: {e}"))?;
    w.flush().map_err(|e| format!("smtp flush: {e}"))
}

/// Read one reply, which may be several lines (`250-EXT` … `250 OK`),
/// and check its code.
fn expect<R: BufRead>(r: &mut R, code: u16, what: &str) -> Result<(), String> {
    loop {
        let mut line = String::new();
        let n = r
            .read_line(&mut line)
            .map_err(|e| format!("smtp read after {what}: {e}"))?;
        if n == 0 {
            return Err(format!("smtp: connection closed waiting for {what}"));
        }
        let line = line.trim_end();
        let got: u16 = line
            .get(..3)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| format!("smtp: unparseable reply to {what}: {line:?}"))?;
        if got != code {
            return Err(format!("smtp: {what} refused: {line}"));
        }
        if line.as_bytes().get(3) != Some(&b'-') {
            return Ok(());
        }
    }
}

/// Render the message as RFC 5322, dot-stuffed and CRLF-terminated.
///
/// Addresses and subject have already been checked for line breaks by
/// [`Message::validate`]; the body has not, and does not need to be —
/// dot-stuffing is what keeps a line of `.` in the body from ending the
/// message early.
pub fn rfc5322(from: &str, msg: &Message, now: u64) -> String {
    let mut out = String::new();
    out.push_str(&format!("From: {from}\r\n"));
    out.push_str(&format!("To: {}\r\n", msg.to));
    out.push_str(&format!("Subject: {}\r\n", encode_header(&msg.subject)));
    out.push_str(&format!("Date: {}\r\n", rfc5322_date(now)));
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    out.push_str("Content-Transfer-Encoding: 8bit\r\n");
    out.push_str("\r\n");
    for line in msg.text.replace("\r\n", "\n").split('\n') {
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out
}

/// RFC 2047 encoded-word, but only when the subject needs it. ASCII
/// subjects stay readable in the raw message, which matters when the
/// raw message is what a test or an operator is reading.
pub fn encode_header(s: &str) -> String {
    if s.is_ascii() {
        return s.to_string();
    }
    format!("=?UTF-8?B?{}?=", stratum_store::b64::encode(s.as_bytes()))
}

/// `Tue, 22 Aug 2026 19:55:39 +0000`, from an epoch second.
pub fn rfc5322_date(secs: u64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // The civil-date conversion lives once, in the signer; this reads
    // its fixed-width output rather than growing a second copy.
    let (_, stamp) = stratum_store::sig::format_utc(secs);
    let num = |a: usize, b: usize| stamp[a..b].parse::<usize>().unwrap_or(0);
    // 1970-01-01 was a Thursday, index 4 in a Sunday-first week.
    let dow = DAYS[((secs / 86_400 + 4) % 7) as usize];
    let month = MONTHS[num(4, 6).clamp(1, 12) - 1];
    format!(
        "{dow}, {:02} {month} {} {}:{}:{} +0000",
        num(6, 8),
        &stamp[0..4],
        &stamp[9..11],
        &stamp[11..13],
        &stamp[13..15],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg() -> Message {
        Message {
            to: "someone@example.com".into(),
            subject: "Verify your address".into(),
            text: "Click here:\nhttps://example.com/x\n".into(),
        }
    }

    /// A scripted relay: hands back the replies in order and records
    /// every command it was sent.
    fn run(replies: &[&str], auth: Option<(&str, &str)>) -> (Result<(), String>, String) {
        let script = replies
            .iter()
            .map(|r| format!("{r}\r\n"))
            .collect::<String>();
        let mut r = std::io::BufReader::new(std::io::Cursor::new(script.into_bytes()));
        let mut w: Vec<u8> = Vec::new();
        let out = converse(
            &mut r,
            &mut w,
            "stratum",
            "no-reply@example.com",
            auth,
            &msg(),
            1_787_428_539,
        );
        (out, String::from_utf8(w).unwrap())
    }

    #[test]
    fn a_successful_delivery_sends_the_commands_in_order() {
        let (out, sent) = run(
            &[
                "220 relay ready",
                "250-relay greets you\r\n250-AUTH PLAIN\r\n250 SIZE 1000000",
                "250 sender ok",
                "250 recipient ok",
                "354 go ahead",
                "250 queued as ABC",
            ],
            None,
        );
        out.unwrap();
        let lines: Vec<&str> = sent.split("\r\n").collect();
        assert_eq!(lines[0], "EHLO stratum");
        assert_eq!(lines[1], "MAIL FROM:<no-reply@example.com>");
        assert_eq!(lines[2], "RCPT TO:<someone@example.com>");
        assert_eq!(lines[3], "DATA");
        assert!(sent.contains("Subject: Verify your address\r\n"));
        assert!(sent.contains("Date: Sat, 22 Aug 2026 19:55:39 +0000\r\n"));
        assert!(sent.ends_with("\r\n.\r\nQUIT\r\n"));
    }

    #[test]
    fn auth_plain_carries_a_nul_separated_blob() {
        let (out, sent) = run(
            &[
                "220 relay ready",
                "250 relay greets you",
                "235 authenticated",
                "250 sender ok",
                "250 recipient ok",
                "354 go ahead",
                "250 queued",
            ],
            Some(("user", "pa55")),
        );
        out.unwrap();
        let line = sent
            .split("\r\n")
            .find(|l| l.starts_with("AUTH PLAIN "))
            .expect("AUTH sent");
        let decoded = stratum_store::b64::decode(&line["AUTH PLAIN ".len()..]).unwrap();
        assert_eq!(decoded, b"\0user\0pa55");
    }

    /// Every stage a relay can refuse at, and the closed-connection and
    /// garbage-reply cases. Each must be an error naming the stage, not
    /// a silent success.
    #[test]
    fn every_refusal_is_reported_with_the_stage_that_failed() {
        for (replies, auth, needle) in [
            (vec!["554 no service"], None, "greeting"),
            (vec!["220 ok", "502 command not implemented"], None, "EHLO"),
            (
                vec!["220 ok", "250 ok", "535 bad credentials"],
                Some(("u", "p")),
                "AUTH",
            ),
            (
                vec!["220 ok", "250 ok", "550 sender rejected"],
                None,
                "MAIL FROM",
            ),
            (
                vec!["220 ok", "250 ok", "250 ok", "550 no such user"],
                None,
                "RCPT TO",
            ),
            (
                vec!["220 ok", "250 ok", "250 ok", "250 ok", "451 try later"],
                None,
                "DATA",
            ),
            (
                vec![
                    "220 ok",
                    "250 ok",
                    "250 ok",
                    "250 ok",
                    "354 go",
                    "552 too big",
                ],
                None,
                "end of data",
            ),
            // The relay hangs up mid-conversation.
            (vec!["220 ok"], None, "connection closed"),
            // …and answers something that is not a reply at all.
            (vec!["hello?"], None, "unparseable"),
        ] {
            let (out, _) = run(&replies, auth);
            let e = out.expect_err("expected a refusal");
            assert!(e.contains(needle), "{e:?} should mention {needle:?}");
        }
    }

    /// Dot-stuffing: a body line of `.` would otherwise end the message,
    /// truncating it and leaving the rest to be read as commands.
    #[test]
    fn a_leading_dot_in_the_body_is_stuffed() {
        let body = rfc5322(
            "a@example.com",
            &Message {
                to: "b@example.com".into(),
                subject: "s".into(),
                // CRLF in the source normalises rather than doubling.
                text: ".\r\n..hidden\r\nnormal".into(),
            },
            0,
        );
        let (_, data) = body.split_once("\r\n\r\n").unwrap();
        assert_eq!(data, "..\r\n...hidden\r\nnormal\r\n");
    }

    /// A non-ASCII subject is encoded; an ASCII one is left legible.
    #[test]
    fn subjects_are_encoded_only_when_they_need_to_be() {
        assert_eq!(encode_header("Verify your address"), "Verify your address");
        assert_eq!(
            encode_header("Vérifiez votre adresse"),
            "=?UTF-8?B?VsOpcmlmaWV6IHZvdHJlIGFkcmVzc2U=?="
        );
    }

    /// Fixed epoch seconds against hand-checked civil dates, including
    /// the epoch itself and a leap day.
    #[test]
    fn dates_render_in_rfc5322_form() {
        assert_eq!(rfc5322_date(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(
            rfc5322_date(1_709_164_800),
            "Thu, 29 Feb 2024 00:00:00 +0000"
        );
        assert_eq!(
            rfc5322_date(1_787_428_539),
            "Sat, 22 Aug 2026 19:55:39 +0000"
        );
    }

    /// The cleartext-credential guard, which is the security-relevant
    /// part of this transport's configuration.
    #[test]
    fn credentials_are_refused_to_a_remote_relay_without_an_explicit_opt_in() {
        let _guard = crate::mail::tests::EnvLock::acquire();
        let vars = [
            "STRATUM_MAIL_SMTP_HOST",
            "STRATUM_MAIL_SMTP_USER",
            "STRATUM_MAIL_SMTP_PASSWORD",
            "STRATUM_MAIL_SMTP_ALLOW_CLEARTEXT_AUTH",
            "STRATUM_MAIL_SMTP_HELO",
        ];
        let clear = || {
            for v in vars {
                std::env::remove_var(v);
            }
        };
        clear();
        let from = || "no-reply@example.com".to_string();

        assert!(Smtp::from_env(from())
            .err()
            .unwrap()
            .contains("STRATUM_MAIL_SMTP_HOST"));

        // No credentials: any host is fine, and a bare host gets :25.
        std::env::set_var("STRATUM_MAIL_SMTP_HOST", "relay.example.com");
        let s = Smtp::from_env(from()).unwrap();
        assert_eq!(s.host, "relay.example.com:25");
        assert_eq!(s.helo, "stratum");
        assert!(s.auth.is_none());
        assert_eq!(s.kind(), "smtp");

        // Half-configured credentials are a mistake, not "no auth".
        std::env::set_var("STRATUM_MAIL_SMTP_USER", "u");
        assert!(Smtp::from_env(from())
            .err()
            .unwrap()
            .contains("must be set together"));
        std::env::set_var("STRATUM_MAIL_SMTP_PASSWORD", "p");

        // Credentials to a remote relay: refused, with the way out named.
        let e = Smtp::from_env(from()).err().unwrap();
        assert!(e.contains("in the clear"), "{e}");
        assert!(e.contains("STRATUM_MAIL_SMTP_ALLOW_CLEARTEXT_AUTH"), "{e}");

        // …allowed to a loopback relay, in every spelling of loopback.
        for host in ["localhost:2525", "127.0.0.1:25", "127.9.9.9", "[::1]:25"] {
            std::env::set_var("STRATUM_MAIL_SMTP_HOST", host);
            assert!(
                Smtp::from_env(from()).is_ok(),
                "{host} should be treated as local"
            );
        }
        // A hostname that merely looks local is not local.
        std::env::set_var("STRATUM_MAIL_SMTP_HOST", "localhost.evil.example");
        assert!(Smtp::from_env(from()).is_err());

        // …and allowed anywhere once the operator says the link is private.
        std::env::set_var("STRATUM_MAIL_SMTP_HOST", "relay.example.com:587");
        std::env::set_var("STRATUM_MAIL_SMTP_ALLOW_CLEARTEXT_AUTH", "1");
        std::env::set_var("STRATUM_MAIL_SMTP_HELO", "stratum.example.com");
        let s = Smtp::from_env(from()).unwrap();
        assert_eq!(s.auth, Some(("u".into(), "p".into())));
        assert_eq!(s.helo, "stratum.example.com");
        clear();
    }

    /// A message that would forge a header never reaches the socket, and
    /// an unreachable relay is an error rather than a panic.
    #[test]
    fn send_validates_before_it_connects() {
        let smtp = Smtp {
            // Port 0 is not connectable, so reaching the connect at all
            // is itself an error — which is the point of the second case.
            host: "127.0.0.1:0".into(),
            from: "no-reply@example.com".into(),
            auth: None,
            helo: "stratum".into(),
        };
        let e = smtp
            .send(&Message {
                to: "a@example.com\nBcc: x@example.com".into(),
                subject: "s".into(),
                text: "t".into(),
            })
            .unwrap_err();
        assert!(e.contains("line break"), "{e}");

        let e = smtp.send(&msg()).unwrap_err();
        assert!(e.starts_with("smtp 127.0.0.1:0"), "{e}");
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use stratum_testkit::smtpfake::FakeSmtp;

    fn smtp(host: String, auth: Option<(String, String)>) -> Smtp {
        Smtp {
            host,
            from: "no-reply@stratum.test".into(),
            auth,
            helo: "stratum.test".into(),
        }
    }

    /// The whole transport over a real socket against a real-ish relay:
    /// what the relay receives is what the recipient would read. The
    /// scripted-buffer tests above prove the client's half of the
    /// conversation; this proves the two halves fit together.
    #[test]
    fn a_message_arrives_at_the_relay_intact() {
        let relay = FakeSmtp::start();
        let out = smtp(relay.host.clone(), Some(("user".into(), "pa55".into())));
        out.send(&Message {
            to: "someone@example.com".into(),
            subject: "Vérifiez votre adresse".into(),
            text: "Hello.\n.\nA line that begins with a dot survives.\n".into(),
        })
        .unwrap();

        let mail = &relay.delivered()[0];
        assert_eq!(mail.helo, "stratum.test");
        assert_eq!(mail.mail_from, "no-reply@stratum.test");
        assert_eq!(mail.rcpt_to, ["someone@example.com"]);
        assert_eq!(
            stratum_store::b64::decode(mail.auth.as_ref().unwrap()).unwrap(),
            b"\0user\0pa55"
        );
        assert_eq!(mail.header("To").as_deref(), Some("someone@example.com"));
        assert_eq!(
            mail.header("From").as_deref(),
            Some("no-reply@stratum.test")
        );
        assert_eq!(
            mail.header("Content-Type").as_deref(),
            Some("text/plain; charset=utf-8")
        );
        // A non-ASCII subject arrives encoded, and decodes back.
        let subject = mail.header("Subject").unwrap();
        let inner = subject
            .strip_prefix("=?UTF-8?B?")
            .and_then(|s| s.strip_suffix("?="))
            .expect("encoded word");
        assert_eq!(
            String::from_utf8(stratum_store::b64::decode(inner).unwrap()).unwrap(),
            "Vérifiez votre adresse"
        );
        // Dot-stuffing survives the round trip: the body the relay
        // reconstructs is the body that was written.
        assert_eq!(
            mail.body(),
            "Hello.\n.\nA line that begins with a dot survives.\n\n"
        );
    }

    /// A relay that refuses at each stage is an error naming the stage,
    /// and — the part that matters — nothing is recorded as delivered.
    #[test]
    fn a_refusal_at_any_stage_delivers_nothing() {
        for (stage, needle) in [
            ("AUTH", "AUTH"),
            ("MAIL", "MAIL FROM"),
            ("RCPT", "RCPT TO"),
            ("DATA", "DATA"),
            ("EOD", "end of data"),
        ] {
            let relay = FakeSmtp::start_refusing(Some(stage));
            let e = smtp(relay.host.clone(), Some(("u".into(), "p".into())))
                .send(&Message {
                    to: "someone@example.com".into(),
                    subject: "s".into(),
                    text: "t".into(),
                })
                .expect_err("relay refused at {stage}");
            assert!(e.contains(needle), "{stage}: {e}");
            assert!(relay.delivered().is_empty(), "{stage} recorded a delivery");
        }
    }
}
