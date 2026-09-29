//! The Weft license key: what it says, whether this build trusts it, and
//! the daily check that tells the operator what Weft knows about it.
//!
//! **A license never stops the server.** No feature is behind a tier, no
//! request is refused, nothing slows down when a key expires, lapses or
//! is revoked, or when more people use the server than it covers. The
//! license is a term of access to releases and security patches, and
//! what this module produces is a sentence for the operator — in
//! `stratum-server admin license-status` and in the log — never an
//! instruction the server obeys.
//!
//! # The key
//!
//! `weft_lic_v1.<payload>.<signature>`: base64url JSON, signed with
//! Ed25519 by Weft's license service under a key id (`kid`) this build
//! trusts. [`verify_key`] refuses in the service's own order — the shape,
//! the `kid`, the signature, and only then the payload's contents — and
//! [`Payload::validate`] is rule for rule the service's
//! `license_core::Payload::validate`: the service verifies every key it
//! signs with those rules before it sends it, so a key it sent is a key
//! this reads. The license service's `spool_e2e` suite installs a key it
//! issued with this binary's own `admin license-install`, and is where
//! that agreement is checked rather than believed.
//!
//! For Spool, the key's `maxConcurrent` counts **people**: accounts that
//! are not switched off ([`stratum_control::license::people`]).
//!
//! # The daily check
//!
//! Once a day an online key sends exactly three fields — [`CheckBody`]:
//! the license id, this build's version, and the number of people — and
//! nothing else: no hostnames, no addresses, no repository or
//! organisation names, nothing about who the people are. An offline key
//! (enterprise, sold on annual prepay) never calls out at all. A refusal
//! (4xx) is not retried: it will say the same thing tomorrow.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use stratum_control::ControlDb;

pub const KEY_PREFIX: &str = "weft_lic_v1";

/// The longest key read. The service refuses to issue a longer one.
pub const MAX_KEY_LENGTH: usize = 8192;

/// Where the daily check goes unless `STRATUM_LICENSE_ENDPOINT` says
/// otherwise.
pub const DEFAULT_ENDPOINT: &str = "https://license.weft.sh/v1/spool/check";

/// The signing keys a release trusts, `(kid, PEM SPKI)`, compiled in.
///
/// Empty until Weft's production signing key for Spool exists: create it
/// in KMS, add its public half here and as the license service's
/// `LICENSE_SPOOL_PUBLIC_KEY_PEM` under the same kid. Until then only a
/// development key ([`trusted_keys`]) verifies, and a real key is refused
/// as signed by a key this build does not know — which is true.
pub const PRODUCTION_KEYS: &[(&str, &str)] = &[];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Community,
    Team,
    Business,
    Enterprise,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Community => "community",
            Tier::Team => "team",
            Tier::Business => "business",
            Tier::Enterprise => "enterprise",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Checks in once a day.
    Online,
    /// Never calls out. Enterprise only.
    Offline,
}

/// The signed payload, field for field the license service's
/// `license_core::Payload`. Unknown fields are refused, as there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Payload {
    pub v: u8,
    pub kid: String,
    pub lid: String,
    pub entity: String,
    pub tier: Tier,
    /// AWS account ids; Spool is not bound to AWS and its keys carry none.
    pub accounts: Vec<String>,
    /// How many people the license covers; `None` is unlimited.
    pub max_concurrent: Option<u32>,
    pub mode: Mode,
    pub iat: String,
    pub exp: String,
    /// `true` on a trial key, absent on every other. `null` is refused,
    /// as Sandy refuses it: absent and `null` are not the same key.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_bool"
    )]
    pub trial: Option<bool>,
}

/// A field that, when present, is a boolean — never `null`. (`default`
/// covers its absence; an `Option` alone would read `null` as absent.)
fn present_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    bool::deserialize(d).map(Some)
}

impl Payload {
    /// The first rule the payload breaks — the service's rules, in the
    /// service's order, in its words.
    pub fn validate(&self) -> Result<(), String> {
        if self.v != 1 {
            return Err("unsupported payload version".into());
        }
        for (name, value) in [
            ("kid", &self.kid),
            ("lid", &self.lid),
            ("entity", &self.entity),
            ("iat", &self.iat),
            ("exp", &self.exp),
        ] {
            if value.is_empty() {
                return Err(format!("{name} must be a non-empty string"));
            }
        }
        if self.mode == Mode::Offline && self.tier != Tier::Enterprise {
            return Err("offline keys are issued for the enterprise tier only".into());
        }
        if !self
            .accounts
            .iter()
            .all(|a| a.len() == 12 && a.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err("accounts must be a list of 12-digit AWS account IDs".into());
        }
        if self.max_concurrent == Some(0) {
            return Err("maxConcurrent must be a positive integer or null".into());
        }
        let (Some(iat), Some(exp)) = (parse_rfc3339(&self.iat), parse_rfc3339(&self.exp)) else {
            return Err("iat and exp must be ISO 8601 timestamps".into());
        };
        if exp <= iat {
            return Err("exp must be after iat".into());
        }
        Ok(())
    }

    /// When the key stops covering releases, in unix seconds.
    pub fn expires_at(&self) -> i64 {
        parse_rfc3339(&self.exp).unwrap_or(0)
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` and nothing looser: the only spelling the
/// service writes, parsed the way it parses it.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = &s[r];
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let month_len = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if d == 0 || d > month_len || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    let days = i64::from(stratum_control::contribs::days_from_civil(
        y as i32, mo as u32, d as u32,
    ));
    Some(days * 86_400 + h * 3600 + mi * 60 + se)
}

/// `YYYY-MM-DD`, the UTC day an instant falls on.
pub fn format_date(secs: i64) -> String {
    let (y, m, d) = stratum_control::contribs::civil_from_days(secs.div_euclid(86_400) as i32);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The DER prefix of an Ed25519 `SubjectPublicKeyInfo` (RFC 8410): the
/// 32 key bytes follow it, and nothing else may.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// An Ed25519 public key from PEM SPKI — the shape the license service
/// is configured with, and the shape `aws kms get-public-key` gives.
pub fn public_key_from_pem(pem: &str) -> Result<VerifyingKey, String> {
    let body: String = pem
        .trim()
        .strip_prefix("-----BEGIN PUBLIC KEY-----")
        .and_then(|s| s.strip_suffix("-----END PUBLIC KEY-----"))
        .ok_or("not a PEM public key (-----BEGIN PUBLIC KEY-----)")?
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let der = crate::authx::base64_decode(&body).ok_or("the PEM body is not base64")?;
    let key = der
        .strip_prefix(&ED25519_SPKI_PREFIX)
        .filter(|k| k.len() == 32)
        .ok_or("not an Ed25519 public key")?;
    VerifyingKey::from_bytes(key.try_into().expect("32 bytes"))
        .map_err(|e| format!("not an Ed25519 public key: {e}"))
}

/// Why a key was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    Malformed(String),
    UnknownSigningKey(String),
    BadSignature,
    InvalidPayload(String),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Malformed(d) => write!(f, "malformed key: {d}"),
            VerifyError::UnknownSigningKey(k) => {
                write!(f, "signed by {k}, a key this build of Spool does not trust")
            }
            VerifyError::BadSignature => write!(f, "the signature does not match"),
            VerifyError::InvalidPayload(d) => write!(f, "invalid payload: {d}"),
        }
    }
}

fn is_base64url(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Verify a key: shape, `kid`, signature, and only then the payload.
pub fn verify_key(
    key: &str,
    trusted: &HashMap<String, VerifyingKey>,
) -> Result<Payload, VerifyError> {
    let key = key.trim();
    if key.len() > MAX_KEY_LENGTH {
        return Err(VerifyError::Malformed("key is too long".into()));
    }
    let parts: Vec<&str> = key.split('.').collect();
    if parts.len() != 3 || parts[0] != KEY_PREFIX {
        return Err(VerifyError::Malformed(format!(
            "key must look like {KEY_PREFIX}.<payload>.<signature>"
        )));
    }
    let (payload_part, sig_part) = (parts[1], parts[2]);
    if !is_base64url(payload_part) || !is_base64url(sig_part) {
        return Err(VerifyError::Malformed(
            "key segments must be base64url".into(),
        ));
    }
    let bytes = crate::oidc::b64url_decode(payload_part)
        .ok_or_else(|| VerifyError::Malformed("payload is not base64url".into()))?;
    let sig = crate::oidc::b64url_decode(sig_part)
        .ok_or_else(|| VerifyError::Malformed("signature is not base64url".into()))?;
    let raw: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| VerifyError::Malformed("payload is not JSON".into()))?;
    let Some(kid) = raw.get("kid").and_then(|k| k.as_str()) else {
        return Err(VerifyError::Malformed("payload has no kid".into()));
    };
    let Some(public) = trusted.get(kid) else {
        return Err(VerifyError::UnknownSigningKey(kid.to_string()));
    };
    let sig: [u8; 64] = sig.try_into().map_err(|_| VerifyError::BadSignature)?;
    public
        .verify(payload_part.as_bytes(), &Signature::from_bytes(&sig))
        .map_err(|_| VerifyError::BadSignature)?;
    let payload: Payload = serde_json::from_value(raw)
        .map_err(|e| VerifyError::InvalidPayload(format!("payload does not parse: {e}")))?;
    payload.validate().map_err(VerifyError::InvalidPayload)?;
    Ok(payload)
}

/// How this server reads and checks its license.
#[derive(Debug, Clone)]
pub struct Config {
    pub trusted: HashMap<String, VerifyingKey>,
    pub endpoint: String,
    /// Between the daily check's attempts, when the service did not answer.
    pub retry_pause: Duration,
}

/// The keys this build trusts: [`PRODUCTION_KEYS`], and — only with
/// `STRATUM_DEV_MODE=1` — `STRATUM_DEV_LICENSE_PUBLIC_KEYS`, a JSON map
/// of kid to PEM, for a license service in development. Without dev
/// mode that variable is refused rather than ignored: a server quietly
/// trusting a key somebody configured is exactly what it must not do.
pub fn config_from(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
    let mut trusted = HashMap::new();
    for (kid, pem) in PRODUCTION_KEYS {
        trusted.insert(
            kid.to_string(),
            public_key_from_pem(pem).map_err(|e| format!("license: built-in key {kid}: {e}"))?,
        );
    }
    let dev_mode = get("STRATUM_DEV_MODE").is_some_and(|v| v.trim() == "1");
    if let Some(dev) = get("STRATUM_DEV_LICENSE_PUBLIC_KEYS").filter(|v| !v.trim().is_empty()) {
        if !dev_mode {
            return Err(
                "STRATUM_DEV_LICENSE_PUBLIC_KEYS is for a license service in \
                        development, and is read only with STRATUM_DEV_MODE=1"
                    .into(),
            );
        }
        let map: HashMap<String, String> = serde_json::from_str(&dev).map_err(|e| {
            format!("STRATUM_DEV_LICENSE_PUBLIC_KEYS must be a JSON map of kid to PEM: {e}")
        })?;
        for (kid, pem) in map {
            let key = public_key_from_pem(&pem)
                .map_err(|e| format!("STRATUM_DEV_LICENSE_PUBLIC_KEYS: {kid}: {e}"))?;
            trusted.insert(kid, key);
        }
    }
    let endpoint = get("STRATUM_LICENSE_ENDPOINT")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
    if !crate::oidc::https_or_loopback(&endpoint) {
        return Err(format!(
            "STRATUM_LICENSE_ENDPOINT must be https:// (got {endpoint:?})"
        ));
    }
    let retry_ms = match get("STRATUM_LICENSE_RETRY_MS") {
        None => 30_000,
        Some(v) => v
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("STRATUM_LICENSE_RETRY_MS: {v:?} is not a number"))?,
    };
    Ok(Config {
        trusted,
        endpoint,
        retry_pause: Duration::from_millis(retry_ms),
    })
}

/// What a verified key means today, for the operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Standing {
    /// `active`, `trial` or `expired`.
    pub state: &'static str,
    /// The last day the key covers releases, `YYYY-MM-DD`.
    pub expires: String,
    /// People the license covers; `None` is unlimited.
    pub limit: Option<u32>,
    pub people: u32,
    pub over_limit: bool,
}

pub fn evaluate(p: &Payload, people: u32, now: i64) -> Standing {
    let exp = p.expires_at();
    let state = if exp <= now {
        "expired"
    } else if p.trial == Some(true) {
        "trial"
    } else {
        "active"
    };
    Standing {
        state,
        expires: format_date(exp),
        limit: p.max_concurrent,
        people,
        over_limit: p.max_concurrent.is_some_and(|max| people > max),
    }
}

/// The daily check's body: these three fields and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckBody {
    /// The license id, `lid`.
    pub key_id: String,
    /// This build: `0.1.0`.
    pub version: String,
    /// Accounts that are not switched off, right now.
    pub people: u32,
}

/// What the service said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// `active`, `lapsed` or `revoked`, and perhaps a sentence.
    Answered {
        status: String,
        notice: Option<String>,
    },
    /// A 4xx: the service will say the same tomorrow, so it is not asked
    /// again today.
    Refused(String),
    /// No answer, or none that reads, after every attempt.
    Unanswered(String),
}

const ATTEMPTS: u32 = 3;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// A notice longer than this is not a sentence for an operator.
const MAX_NOTICE: usize = 2000;

/// Send the check, up to three times while the service does not answer,
/// never again once it has. Returns the outcome and how many requests
/// were made.
pub fn send_check(cfg: &Config, body: &CheckBody) -> (CheckOutcome, u32) {
    let agent = ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .redirects(0)
        .build();
    let text = serde_json::to_string(body).expect("a check body serializes");
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        if attempt > 1 {
            std::thread::sleep(cfg.retry_pause);
        }
        let reply = agent
            .post(&cfg.endpoint)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .send_string(&text);
        match reply {
            Ok(r) => {
                let answer = r.into_string().map_err(|e| e.to_string());
                match answer.and_then(|a| read_answer(&a)) {
                    Ok(outcome) => return (outcome, attempt),
                    // An answer that does not read is the service
                    // misbehaving, not refusing: it may read tomorrow.
                    Err(e) => last = format!("the license service's answer did not read: {e}"),
                }
            }
            Err(ureq::Error::Status(code, r)) if (400..500).contains(&code) => {
                let said = r
                    .into_string()
                    .ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                    .and_then(|v| v["error"].as_str().map(str::to_string))
                    .unwrap_or_default();
                return (
                    CheckOutcome::Refused(format!("{code}: {said}").trim_end_matches(": ").into()),
                    attempt,
                );
            }
            Err(ureq::Error::Status(code, _)) => {
                last = format!("the license service answered {code}");
            }
            Err(e) => last = format!("the license service did not answer: {e}"),
        }
    }
    (CheckOutcome::Unanswered(last), ATTEMPTS)
}

fn read_answer(text: &str) -> Result<CheckOutcome, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let status = v["status"]
        .as_str()
        .filter(|s| matches!(*s, "active" | "lapsed" | "revoked"))
        .ok_or_else(|| format!("no status this build knows in {text}"))?;
    let notice = match &v["notice"] {
        serde_json::Value::Null => None,
        serde_json::Value::String(n) if n.chars().count() <= MAX_NOTICE => Some(n.clone()),
        other => return Err(format!("notice is not a sentence: {other}")),
    };
    Ok(CheckOutcome::Answered {
        status: status.to_string(),
        notice,
    })
}

/// What one check did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// `answered`, `refused`, `unanswered`, `offline` (never calls out),
    /// `none` (no key installed) or `untrusted` (the stored key does not
    /// verify under this build).
    pub outcome: &'static str,
    pub status: Option<String>,
    pub notice: Option<String>,
    pub error: Option<String>,
    /// Requests made.
    pub calls: u32,
    /// What was sent, when anything was.
    pub sent: Option<CheckBody>,
}

impl Report {
    fn quiet(outcome: &'static str, error: Option<String>) -> Report {
        Report {
            outcome,
            status: None,
            notice: None,
            error,
            calls: 0,
            sent: None,
        }
    }
}

/// Check the installed key with the service now, and record the outcome.
pub fn run_check(db: &ControlDb, cfg: &Config) -> Result<Report, String> {
    let Some(installed) = stratum_control::license::get(db)? else {
        return Ok(Report::quiet("none", None));
    };
    let payload = match verify_key(&installed.key, &cfg.trusted) {
        Ok(p) => p,
        Err(e) => {
            // Not sent: a key this build does not trust names a license
            // this build cannot vouch for.
            let why = e.to_string();
            stratum_control::license::record_error(db, &installed.lid, &why)?;
            return Ok(Report::quiet("untrusted", Some(why)));
        }
    };
    if payload.mode == Mode::Offline {
        return Ok(Report::quiet("offline", None));
    }
    let body = CheckBody {
        key_id: payload.lid.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        people: stratum_control::license::people(db)?,
    };
    let (outcome, calls) = send_check(cfg, &body);
    let mut report = Report {
        calls,
        sent: Some(body),
        ..Report::quiet("answered", None)
    };
    match outcome {
        CheckOutcome::Answered { status, notice } => {
            stratum_control::license::record_answer(db, &payload.lid, &status, notice.as_deref())?;
            report.status = Some(status);
            report.notice = notice;
        }
        CheckOutcome::Refused(e) => {
            let why = format!("the license service refused the check: {e}");
            stratum_control::license::record_error(db, &payload.lid, &why)?;
            report.outcome = "refused";
            report.error = Some(why);
        }
        CheckOutcome::Unanswered(e) => {
            stratum_control::license::record_error(db, &payload.lid, &e)?;
            report.outcome = "unanswered";
            report.error = Some(e);
        }
    }
    Ok(report)
}

/// `admin license-status`: the key, what it means today, and what the
/// service last said — one JSON object.
pub fn status(db: &ControlDb, cfg: &Config, now: i64) -> Result<serde_json::Value, String> {
    let people = stratum_control::license::people(db)?;
    let Some(installed) = stratum_control::license::get(db)? else {
        return Ok(serde_json::json!({ "installed": false, "people": people }));
    };
    let check = serde_json::json!({
        "at": installed.checked_at,
        "status": installed.status,
        "notice": installed.notice,
        "error": installed.error,
    });
    let payload = match verify_key(&installed.key, &cfg.trusted) {
        Ok(p) => p,
        Err(e) => {
            return Ok(serde_json::json!({
                "installed": true, "trusted": false, "lid": installed.lid,
                "error": e.to_string(), "people": people, "check": check,
            }))
        }
    };
    let standing = evaluate(&payload, people, now);
    Ok(serde_json::json!({
        "installed": true,
        "trusted": true,
        "lid": payload.lid,
        "entity": payload.entity,
        "tier": payload.tier.as_str(),
        "mode": if payload.mode == Mode::Online { "online" } else { "offline" },
        "trial": payload.trial == Some(true),
        "state": standing.state,
        "expires": standing.expires,
        "limit": standing.limit,
        "people": standing.people,
        "over_limit": standing.over_limit,
        "check": check,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_testkit::license as fake;

    fn trusted() -> HashMap<String, VerifyingKey> {
        HashMap::from([(
            fake::KID.to_string(),
            public_key_from_pem(&fake::public_pem()).unwrap(),
        )])
    }

    fn with(edit: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut p = fake::payload("lic_0123456789abcdefghjkmnpqrs");
        edit(&mut p);
        fake::sign(&p)
    }

    /// The JSON the service writes — its own test's payload, field for
    /// field — reads here as the same thing, `null` limit and all.
    #[test]
    fn the_services_payload_reads_as_written() {
        let key = with(|p| {
            *p = serde_json::json!({
                "v": 1, "kid": fake::KID, "lid": "lic_0123456789abcdefghjkmnpqrs",
                "entity": "Acme GmbH", "tier": "team", "accounts": ["111122223333"],
                "maxConcurrent": null, "mode": "online",
                "iat": "2026-09-28T00:00:00Z", "exp": "2027-09-28T00:00:00Z"
            })
        });
        let p = verify_key(&key, &trusted()).unwrap();
        assert_eq!(p.max_concurrent, None);
        assert_eq!(p.trial, None);
        assert_eq!(p.accounts, ["111122223333"]);
        assert_eq!(
            p.expires_at(),
            parse_rfc3339("2027-09-28T00:00:00Z").unwrap()
        );
        let trial = verify_key(&with(|p| p["trial"] = true.into()), &trusted()).unwrap();
        assert_eq!(trial.trial, Some(true));
    }

    /// Every way a key is refused, in the order the service refuses them.
    #[test]
    fn a_key_wrong_in_any_one_way_is_refused_for_that_reason() {
        let good = with(|_| {});
        assert!(verify_key(&good, &trusted()).is_ok());
        let (_, rest) = good.split_once('.').unwrap();
        let (payload_part, _) = rest.split_once('.').unwrap();
        let malformed =
            |k: &str| matches!(verify_key(k, &trusted()), Err(VerifyError::Malformed(_)));

        assert!(malformed(&"a".repeat(MAX_KEY_LENGTH + 1)));
        assert!(malformed(&good.replacen("weft_lic_v1", "weft_lic_v2", 1)));
        assert!(malformed(&format!("{good}.more")));
        assert!(malformed(&format!("weft_lic_v1.{payload_part}.a+b/")));
        assert!(malformed(&format!(
            "weft_lic_v1.{}.{}",
            fake_b64("not json"),
            fake_b64("s")
        )));
        assert!(malformed(&format!(
            "weft_lic_v1.{}.{}",
            fake_b64("{\"v\":1}"),
            fake_b64("s")
        )));
        assert_eq!(
            verify_key(&with(|p| p["kid"] = "spool-2031-01".into()), &trusted()),
            Err(VerifyError::UnknownSigningKey("spool-2031-01".into()))
        );
        let other = fake::sign_with_seed(&fake::payload("lic_x"), 9);
        assert_eq!(
            verify_key(&other, &trusted()),
            Err(VerifyError::BadSignature)
        );
        // A signature over a different payload.
        let (_, sig) = other.rsplit_once('.').unwrap();
        assert_eq!(
            verify_key(&format!("weft_lic_v1.{payload_part}.{sig}"), &trusted()),
            Err(VerifyError::BadSignature)
        );
        // Whitespace around a pasted key is not part of it.
        assert!(verify_key(&format!("  {good}\n"), &trusted()).is_ok());
    }

    fn fake_b64(s: &str) -> String {
        stratum_testkit::oidc::b64url(s.as_bytes())
    }

    /// The payload rules, each broken alone.
    #[test]
    fn a_payload_breaking_any_rule_is_refused_by_it() {
        let invalid = |edit: &dyn Fn(&mut serde_json::Value), why: &str| {
            let mut p = fake::payload("lic_0123456789abcdefghjkmnpqrs");
            edit(&mut p);
            match verify_key(&fake::sign(&p), &trusted()) {
                Err(VerifyError::InvalidPayload(d)) => assert!(d.contains(why), "{d}"),
                other => panic!("{why}: {other:?}"),
            }
        };
        invalid(&|p| p["v"] = 2.into(), "unsupported payload version");
        for f in ["lid", "entity", "iat", "exp"] {
            invalid(&|p| p[f] = "".into(), "must be a non-empty string");
        }
        invalid(&|p| p["mode"] = "offline".into(), "enterprise tier only");
        invalid(
            &|p| p["accounts"] = serde_json::json!(["12345"]),
            "12-digit",
        );
        invalid(
            &|p| p["maxConcurrent"] = 0.into(),
            "positive integer or null",
        );
        invalid(&|p| p["maxConcurrent"] = (-1).into(), "does not parse");
        invalid(&|p| p["tier"] = "gold".into(), "does not parse");
        invalid(&|p| p["extra"] = true.into(), "does not parse");
        invalid(&|p| p["trial"] = serde_json::Value::Null, "does not parse");
        invalid(&|p| p["exp"] = p["iat"].clone(), "exp must be after iat");
        for loose in [
            "2027-09-28T00:00:00.000Z",
            "2027-09-28T00:00:00+00:00",
            "2027-09-28 00:00:00Z",
            "2027-02-30T00:00:00Z",
            "2027-09-28T24:00:00Z",
        ] {
            invalid(&|p| p["exp"] = loose.into(), "ISO 8601");
        }
        // Offline is fine on enterprise.
        let mut p = fake::payload("lic_0123456789abcdefghjkmnpqrs");
        p["mode"] = "offline".into();
        p["tier"] = "enterprise".into();
        assert_eq!(
            verify_key(&fake::sign(&p), &trusted()).unwrap().mode,
            Mode::Offline
        );
    }

    #[test]
    fn only_an_ed25519_pem_is_a_public_key() {
        assert!(public_key_from_pem(&fake::public_pem()).is_ok());
        let rsa = "-----BEGIN PUBLIC KEY-----\nMFwwDQYJKoZIhvcNAQEBBQADSwAwSAJBAKj34GkxFhD90vcNLYLInFEX6Ppy1tPf\n9Cnzj4p4WGeKLs1Pt8QuKUpRKfFLfRYC9AIKjbJTWit+CqvjWYzvQwECAwEAAQ==\n-----END PUBLIC KEY-----";
        assert!(public_key_from_pem(rsa).is_err());
        assert!(public_key_from_pem("not a pem").is_err());
        assert!(
            public_key_from_pem("-----BEGIN PUBLIC KEY-----\n!!\n-----END PUBLIC KEY-----")
                .is_err()
        );
    }

    #[test]
    fn development_keys_are_trusted_only_in_development() {
        let env = |pairs: Vec<(&'static str, String)>| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
        };
        let dev = fake::trust_env();
        let cfg = config_from(env(dev.clone())).unwrap();
        assert!(cfg.trusted.contains_key(fake::KID));
        assert_eq!(cfg.endpoint, DEFAULT_ENDPOINT);

        let without_mode: Vec<_> = dev
            .iter()
            .filter(|(k, _)| *k != "STRATUM_DEV_MODE")
            .cloned()
            .collect();
        let err = config_from(env(without_mode)).unwrap_err();
        assert!(err.contains("STRATUM_DEV_MODE=1"), "{err}");

        // Nothing configured: only what the build trusts.
        assert_eq!(
            config_from(env(vec![])).unwrap().trusted.len(),
            PRODUCTION_KEYS.len()
        );
        let mut bad = dev.clone();
        bad.push((
            "STRATUM_LICENSE_ENDPOINT",
            "http://license.example/v1".into(),
        ));
        assert!(config_from(env(bad)).is_err());
        let mut local = dev;
        local.push(("STRATUM_LICENSE_ENDPOINT", "http://127.0.0.1:9/v1".into()));
        assert!(config_from(env(local)).is_ok());
    }

    #[test]
    fn a_standing_says_what_the_key_means_today() {
        let p = verify_key(&with(|_| {}), &trusted()).unwrap();
        let now = parse_rfc3339(&fake::stamp(0)).unwrap();
        let s = evaluate(&p, 3, now);
        assert_eq!(
            (s.state, s.limit, s.over_limit),
            ("active", Some(20), false)
        );
        assert!(evaluate(&p, 21, now).over_limit);
        assert!(!evaluate(&p, 20, now).over_limit);
        assert_eq!(evaluate(&p, 1, p.expires_at()).state, "expired");
        let trial = verify_key(&with(|p| p["trial"] = true.into()), &trusted()).unwrap();
        assert_eq!(evaluate(&trial, 1, now).state, "trial");
        let unlimited = verify_key(
            &with(|p| p["maxConcurrent"] = serde_json::Value::Null),
            &trusted(),
        )
        .unwrap();
        assert!(!evaluate(&unlimited, 100_000, now).over_limit);
    }

    #[test]
    fn only_an_answer_this_build_can_read_is_one() {
        assert_eq!(
            read_answer(r#"{"status":"revoked","notice":"Weft revoked it."}"#),
            Ok(CheckOutcome::Answered {
                status: "revoked".into(),
                notice: Some("Weft revoked it.".into())
            })
        );
        assert!(matches!(
            read_answer(r#"{"status":"active"}"#),
            Ok(CheckOutcome::Answered { notice: None, .. })
        ));
        for bad in [
            r#"{"status":"suspended"}"#,
            r#"{"notice":"x"}"#,
            r#"{"status":"active","notice":7}"#,
            "not json",
        ] {
            assert!(read_answer(bad).is_err(), "{bad}");
        }
        let long = format!(r#"{{"status":"active","notice":"{}"}}"#, "x".repeat(2001));
        assert!(read_answer(&long).is_err());
    }
}
