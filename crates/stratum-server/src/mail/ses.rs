//! Amazon SES v2, signed with the same SigV4 the object store uses.
//!
//! One POST to `/v2/email/outbound-emails` with the "simple" content
//! shape. Credentials come from the standard environment variables via
//! [`stratum_store::sig::SigV4::from_env`], so a task with an instance
//! role needs no mail-specific secret at all.
//!
//! The request is *built* by [`Ses::request`] and *sent* by
//! [`Ses::send`], split so the signature, the endpoint and the JSON body
//! are all testable without a network — and so the hermetic test can
//! point [`Ses::endpoint`] at a local responder and cover the send path
//! too.

use super::{Mailer, Message};
use std::time::Duration;

/// A request ready to send: URL, body, and headers.
type Request = (String, Vec<u8>, Vec<(String, String)>);
use stratum_store::sig::SigV4;

const TIMEOUT: Duration = Duration::from_secs(20);
const PATH: &str = "/v2/email/outbound-emails";

pub struct Ses {
    pub from: String,
    /// Base URL, no trailing slash. Defaults to the regional SES
    /// endpoint; overridable so tests (and a VPC endpoint) can point
    /// somewhere else.
    pub endpoint: String,
    pub region: String,
    /// Optional configuration set — SES's own dashboarding.
    pub configuration_set: Option<String>,
}

impl Ses {
    pub fn from_env(from: String) -> Result<Self, String> {
        let region = std::env::var("STRATUM_MAIL_SES_REGION")
            .or_else(|_| std::env::var("AWS_REGION"))
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".into());
        let endpoint = std::env::var("STRATUM_MAIL_SES_ENDPOINT")
            .unwrap_or_else(|_| format!("https://email.{region}.amazonaws.com"));
        Ok(Self {
            from,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            region,
            configuration_set: std::env::var("STRATUM_MAIL_SES_CONFIGURATION_SET")
                .ok()
                .filter(|s| !s.is_empty()),
        })
    }

    /// The SES v2 request body. Charset is stated explicitly on both
    /// parts: SES defaults to 7-bit ASCII and would mangle a name with
    /// an accent in it.
    pub fn body(&self, msg: &Message) -> serde_json::Value {
        let mut v = serde_json::json!({
            "FromEmailAddress": self.from,
            "Destination": { "ToAddresses": [msg.to] },
            "Content": { "Simple": {
                "Subject": { "Data": msg.subject, "Charset": "UTF-8" },
                "Body": { "Text": { "Data": msg.text, "Charset": "UTF-8" } },
            }},
        });
        if let Some(set) = &self.configuration_set {
            v["ConfigurationSetName"] = serde_json::Value::String(set.clone());
        }
        v
    }

    /// `(url, body, signed headers)`.
    ///
    /// Unsigned when no credentials are in the environment, which is how
    /// the hermetic test reaches this path — and how a VPC endpoint
    /// behind its own authorization would be used.
    pub fn request(&self, msg: &Message) -> Result<Request, String> {
        msg.validate()?;
        let body = serde_json::to_vec(&self.body(msg)).map_err(|e| e.to_string())?;
        let url = format!("{}{PATH}", self.endpoint);
        let host = url
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&url)
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string();
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        if let Some(sig) = SigV4::from_env().map(|s| s.with_region(&self.region)) {
            let payload = stratum_store::sig::sha256_hex(&body);
            let signed = sig.sign_service("ses", "POST", &host, PATH, &payload)?;
            headers.extend(signed.headers.into_iter().map(|(k, v)| (k.to_string(), v)));
        }
        Ok((url, body, headers))
    }
}

impl Mailer for Ses {
    fn send(&self, msg: &Message) -> Result<(), String> {
        let (url, body, headers) = self.request(msg)?;
        let mut req = ureq::post(&url).timeout(TIMEOUT);
        for (k, v) in &headers {
            req = req.set(k, v);
        }
        req.send_bytes(&body)
            .map(|_| ())
            .map_err(|e| format!("ses send: {e}"))
    }

    fn kind(&self) -> &'static str {
        "ses"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_testkit::httpfake::{FakeHttp, Reply, Scripted};

    fn msg() -> Message {
        Message {
            to: "someone@example.com".into(),
            subject: "Vérifiez".into(),
            text: "hello".into(),
        }
    }

    #[test]
    fn the_body_is_ses_v2_simple_content_with_an_explicit_charset() {
        let ses = Ses {
            from: "no-reply@example.com".into(),
            endpoint: "https://email.eu-west-1.amazonaws.com".into(),
            region: "eu-west-1".into(),
            configuration_set: None,
        };
        let b = ses.body(&msg());
        assert_eq!(b["FromEmailAddress"], "no-reply@example.com");
        assert_eq!(b["Destination"]["ToAddresses"][0], "someone@example.com");
        assert_eq!(b["Content"]["Simple"]["Subject"]["Data"], "Vérifiez");
        assert_eq!(b["Content"]["Simple"]["Subject"]["Charset"], "UTF-8");
        assert_eq!(b["Content"]["Simple"]["Body"]["Text"]["Data"], "hello");
        assert!(b.get("ConfigurationSetName").is_none());

        let ses = Ses {
            configuration_set: Some("stratum-prod".into()),
            ..ses
        };
        assert_eq!(ses.body(&msg())["ConfigurationSetName"], "stratum-prod");
    }

    /// Configuration, including the region precedence that decides which
    /// endpoint — and therefore which signature — is used.
    #[test]
    fn configuration_follows_the_region() {
        let _guard = crate::mail::tests::EnvLock::acquire();
        for v in [
            "STRATUM_MAIL_SES_REGION",
            "STRATUM_MAIL_SES_ENDPOINT",
            "STRATUM_MAIL_SES_CONFIGURATION_SET",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
        ] {
            std::env::remove_var(v);
        }
        let ses = Ses::from_env("a@example.com".into()).unwrap();
        assert_eq!(ses.region, "us-east-1");
        assert_eq!(ses.endpoint, "https://email.us-east-1.amazonaws.com");
        assert_eq!(ses.kind(), "ses");

        std::env::set_var("AWS_DEFAULT_REGION", "ap-south-1");
        assert_eq!(Ses::from_env("a@b.c".into()).unwrap().region, "ap-south-1");
        std::env::set_var("AWS_REGION", "eu-west-1");
        assert_eq!(Ses::from_env("a@b.c".into()).unwrap().region, "eu-west-1");
        std::env::set_var("STRATUM_MAIL_SES_REGION", "us-west-2");
        let ses = Ses::from_env("a@b.c".into()).unwrap();
        assert_eq!(ses.region, "us-west-2");
        assert_eq!(ses.endpoint, "https://email.us-west-2.amazonaws.com");

        // An explicit endpoint wins, and a trailing slash does not
        // produce a doubled one in the signed path.
        std::env::set_var("STRATUM_MAIL_SES_ENDPOINT", "https://ses.internal:8443/");
        std::env::set_var("STRATUM_MAIL_SES_CONFIGURATION_SET", "");
        let ses = Ses::from_env("a@b.c".into()).unwrap();
        assert_eq!(ses.endpoint, "https://ses.internal:8443");
        assert!(ses.configuration_set.is_none());
        let (url, _, _) = ses.request(&msg()).unwrap();
        assert_eq!(url, "https://ses.internal:8443/v2/email/outbound-emails");

        for v in [
            "STRATUM_MAIL_SES_REGION",
            "STRATUM_MAIL_SES_ENDPOINT",
            "STRATUM_MAIL_SES_CONFIGURATION_SET",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
        ] {
            std::env::remove_var(v);
        }
    }

    /// With credentials present the request carries a SigV4 signature
    /// scoped to `ses` and the configured region — not `s3`, and not the
    /// store's region.
    #[test]
    fn the_request_is_signed_for_ses_in_its_own_region() {
        let _guard = crate::mail::tests::EnvLock::acquire();
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret");
        std::env::set_var("AWS_REGION", "us-east-1");
        let ses = Ses {
            from: "no-reply@example.com".into(),
            endpoint: "https://email.eu-west-1.amazonaws.com".into(),
            region: "eu-west-1".into(),
            configuration_set: None,
        };
        let (_, body, headers) = ses.request(&msg()).unwrap();
        let find = |n: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(n))
                .map(|(_, v)| v.clone())
        };
        let auth = find("Authorization").expect("signed");
        assert!(auth.contains("/eu-west-1/ses/aws4_request"), "{auth}");
        // The payload hash covers the body actually sent.
        assert_eq!(
            find("x-amz-content-sha256"),
            Some(stratum_store::sig::sha256_hex(&body))
        );
        assert_eq!(find("Content-Type").as_deref(), Some("application/json"));

        // No credentials: still a well-formed request, just unsigned.
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        std::env::remove_var("AWS_REGION");
        let (_, _, headers) = ses.request(&msg()).unwrap();
        assert_eq!(headers.len(), 1, "only Content-Type: {headers:?}");
    }

    /// The send path end to end against a local responder: the accepted
    /// case, and a rejection surfaced as an error rather than swallowed.
    #[test]
    fn a_rejected_send_is_an_error_and_an_accepted_one_is_not() {
        let _guard = crate::mail::tests::EnvLock::acquire();
        for v in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"] {
            std::env::remove_var(v);
        }
        let fake = FakeHttp::start(vec![
            Reply::Http(Scripted::new(200, br#"{"MessageId":"0100"}"#)),
            Reply::Http(Scripted::new(
                400,
                br#"{"message":"Email address is not verified"}"#,
            )),
        ]);
        let ses = Ses {
            from: "no-reply@example.com".into(),
            endpoint: fake.url.clone(),
            region: "us-east-1".into(),
            configuration_set: None,
        };
        ses.send(&msg()).unwrap();
        let e = ses.send(&msg()).unwrap_err();
        assert!(e.starts_with("ses send:"), "{e}");

        let rec = fake.recorded();
        assert!(rec[0].head.starts_with("POST /v2/email/outbound-emails "));
        assert_eq!(
            rec[0].header("content-type").as_deref(),
            Some("application/json")
        );
        let sent: serde_json::Value = serde_json::from_slice(&rec[0].body).unwrap();
        assert_eq!(sent["Destination"]["ToAddresses"][0], "someone@example.com");

        // A forged header never reaches the wire.
        assert!(ses
            .send(&Message {
                to: "a@example.com\nBcc: x@example.com".into(),
                ..msg()
            })
            .unwrap_err()
            .contains("line break"));
        assert_eq!(fake.recorded().len(), 2);
    }
}
