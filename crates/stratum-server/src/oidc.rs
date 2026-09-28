//! Single sign-on with the company's OpenID Connect provider.
//!
//! Okta, Entra ID, Google Workspace, Keycloak — whichever the company
//! already runs. The provider decides who is admitted; this server only
//! checks, strictly, that the answer came from that provider and was
//! meant for it, and then signs the person in (see
//! [`stratum_control::sso`] for which account that is).
//!
//! # What is checked, every time
//!
//! The authorization-code flow with PKCE (S256), a `state` bound to the
//! browser by a cookie and a `nonce` bound into the ID token. The ID
//! token must be a compact JWS whose header says **exactly `RS256`** —
//! `none` and `HS256` are refused by name, because a verifier that lets
//! the token choose its algorithm can be handed a token "signed" with
//! the public key as an HMAC secret — under a `kid` in the provider's
//! published key set, with a valid signature; `iss` exactly the issuer
//! configured; `aud` naming this client, and `azp` naming it too when
//! there is more than one audience; `exp` in the future and `iat` not,
//! with two minutes' allowance for clocks; `nonce` the one this browser
//! was given; and a non-empty `sub`. Discovery must name the same issuer
//! and must offer RS256. Every one of those has a test that hands the
//! server a token wrong in only that way.
//!
//! # Which addresses are trusted
//!
//! An address in the ID token (or, when the token has none — Entra ID's
//! default — from userinfo) is trusted when the provider marks it
//! verified, or when the operator has named the domains this provider
//! speaks for and the address is in one of them. With a domain list set,
//! nothing outside it gets in. Google's issuer is shared by every Google
//! account on earth, so it needs the list, and there the `hd` (hosted
//! domain) claim decides. Microsoft's shared endpoints (`common`,
//! `organizations`, `consumers`) are refused at boot: their tokens name
//! the tenant they came from, so no fixed issuer could match them, and
//! accepting any tenant would admit any Microsoft account.

use rsa::{BigUint, Pkcs1v15Sign, RsaPublicKey};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use stratum_control::members::Role;

/// How long an SSO session lasts before the provider is asked again.
///
/// Much shorter than a password session's fourteen days, on purpose: the
/// provider is where people are switched off, and a session this server
/// minted keeps working until it expires. Twelve hours means somebody
/// disabled at the provider is out by the next working day.
pub const DEFAULT_SESSION_HOURS: i64 = 12;

/// Clock allowance for `exp` and `iat`.
const SKEW_SECS: i64 = 120;
/// How long discovery is trusted before it is fetched again.
const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
/// A key set is fetched again for an unknown `kid` at most this often, so
/// a stream of forged `kid`s cannot turn this server into a load on the
/// provider.
const JWKS_REFETCH_MIN: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// The operator's configuration, validated at boot.
#[derive(Debug, Clone)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// What the sign-in button calls the provider.
    pub name: String,
    /// The organization everybody arriving by SSO joins, by name, and at
    /// what role. Looked up at each sign-in rather than at boot: on a new
    /// install the organization does not exist until `admin bootstrap`
    /// makes it, and a server that refused to start until then could not
    /// be reached to make it.
    pub org: String,
    pub role: Role,
    /// Domains this provider speaks for, lowercased. Empty means "any
    /// address the provider marks verified".
    pub allowed_domains: Vec<String>,
    pub session_ttl_secs: i64,
}

/// Read and check the configuration. `get` is the environment in
/// production and a map in tests. `Ok(None)` when SSO is not configured.
///
/// Refused at boot, loudly, rather than at the first sign-in: a half
/// configuration, an issuer that is plain HTTP off this machine, one of
/// Microsoft's shared endpoints, Google without a domain list, an
/// organization name that could never exist.
pub fn config_from(get: impl Fn(&str) -> Option<String>) -> Result<Option<OidcConfig>, String> {
    let val = |k: &str| {
        get(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let (issuer, client_id, client_secret) = match (
        val("STRATUM_OIDC_ISSUER"),
        val("STRATUM_OIDC_CLIENT_ID"),
        val("STRATUM_OIDC_CLIENT_SECRET"),
    ) {
        (None, None, None) => return Ok(None),
        (Some(i), Some(c), Some(s)) => (i, c, s),
        _ => {
            return Err("SSO: STRATUM_OIDC_ISSUER, STRATUM_OIDC_CLIENT_ID and \
                        STRATUM_OIDC_CLIENT_SECRET are set together or not at all"
                .into())
        }
    };
    check_url("STRATUM_OIDC_ISSUER", &issuer)?;
    if microsoft_shared(&issuer) {
        return Err(format!(
            "SSO: {issuer} is one of Microsoft's shared endpoints, which sign in any \
             Microsoft account — use your tenant's own issuer, \
             https://login.microsoftonline.com/<tenant-id>/v2.0"
        ));
    }
    let allowed_domains: Vec<String> = val("STRATUM_OIDC_ALLOWED_DOMAINS")
        .map(|v| {
            v.split(',')
                .map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase())
                .filter(|d| !d.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for d in &allowed_domains {
        if !d.contains('.')
            || !d
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return Err(format!(
                "SSO: STRATUM_OIDC_ALLOWED_DOMAINS: {d:?} is not a domain"
            ));
        }
    }
    if is_google(&issuer) && allowed_domains.is_empty() {
        return Err("SSO: Google's issuer signs in every Google account — set \
             STRATUM_OIDC_ALLOWED_DOMAINS to your Workspace domain(s)"
            .into());
    }
    let org_name = val("STRATUM_OIDC_ORG").ok_or(
        "SSO: STRATUM_OIDC_ORG must name the organization people signing in with SSO join",
    )?;
    stratum_control::registry::valid_namespace_name("organization", &org_name)
        .map_err(|e| format!("SSO: STRATUM_OIDC_ORG: {e}"))?;
    let role = match val("STRATUM_OIDC_ROLE") {
        None => Role::Member,
        Some(r) => Role::parse(&r).ok_or_else(|| format!("SSO: STRATUM_OIDC_ROLE: {r:?}"))?,
    };
    let hours = match val("STRATUM_OIDC_SESSION_HOURS") {
        None => DEFAULT_SESSION_HOURS,
        Some(h) => h
            .parse::<i64>()
            .ok()
            .filter(|h| (1..=14 * 24).contains(h))
            .ok_or_else(|| format!("SSO: STRATUM_OIDC_SESSION_HOURS: {h:?} is not 1–336"))?,
    };
    let name = val("STRATUM_OIDC_NAME").unwrap_or_else(|| "SSO".into());
    if name.chars().count() > 40 {
        return Err("SSO: STRATUM_OIDC_NAME is at most 40 characters".into());
    }
    Ok(Some(OidcConfig {
        issuer,
        client_id,
        client_secret,
        name,
        org: org_name,
        role,
        allowed_domains,
        session_ttl_secs: hours * 3600,
    }))
}

/// Whether SSO is the only way in. On by default when SSO is configured:
/// a password or a linked GitHub account would otherwise let somebody the
/// company switched off at its provider keep signing in.
pub fn sso_only_from(
    get: impl Fn(&str) -> Option<String>,
    configured: bool,
) -> Result<bool, String> {
    match get("STRATUM_SSO_ONLY").map(|v| v.trim().to_ascii_lowercase()) {
        None => Ok(configured),
        Some(v) if v.is_empty() => Ok(configured),
        Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => {
            if configured {
                Ok(true)
            } else {
                Err("STRATUM_SSO_ONLY is on but SSO is not configured: nobody could sign in".into())
            }
        }
        Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => Ok(false),
        Some(v) => Err(format!("STRATUM_SSO_ONLY: {v:?} is not true or false")),
    }
}

/// HTTPS, or plain HTTP to this machine only — a provider on the network
/// must be reached over TLS, but a stand-in on loopback is how the tests
/// and the manual stack run.
fn check_url(what: &str, url: &str) -> Result<(), String> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let host = rest.split(['/', '?']).next().unwrap_or("");
        let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
        if matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
            return Ok(());
        }
    }
    Err(format!("SSO: {what} must be https:// (got {url:?})"))
}

fn is_google(issuer: &str) -> bool {
    issuer.trim_end_matches('/') == "https://accounts.google.com"
}

fn microsoft_shared(issuer: &str) -> bool {
    let Some(rest) = issuer.strip_prefix("https://login.microsoftonline.com/") else {
        return false;
    };
    let tenant = rest.split('/').next().unwrap_or("").to_ascii_lowercase();
    matches!(tenant.as_str(), "common" | "organizations" | "consumers")
}

/// What discovery told us, checked.
#[derive(Debug, Clone)]
pub struct Discovery {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    pub userinfo_endpoint: Option<String>,
    /// Authenticate to the token endpoint with HTTP Basic (the spec's
    /// default) rather than in the form body.
    pub basic_auth: bool,
}

/// Read a discovery document. The issuer in it must be the configured
/// one, character for character — the OIDC Discovery rule, and what
/// stops a document from elsewhere steering us to somebody else's keys.
pub fn discovery_from(doc: &serde_json::Value, issuer: &str) -> Result<Discovery, String> {
    if doc["issuer"].as_str() != Some(issuer) {
        return Err(format!(
            "discovery names issuer {:?}, not {issuer:?}",
            doc["issuer"]
        ));
    }
    let endpoint = |k: &str| -> Result<String, String> {
        let u = doc[k]
            .as_str()
            .ok_or_else(|| format!("discovery has no {k}"))?;
        check_url(k, u)?;
        Ok(u.to_string())
    };
    let algs = doc["id_token_signing_alg_values_supported"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !algs.iter().any(|a| a.as_str() == Some("RS256")) {
        return Err("the provider does not offer RS256-signed ID tokens".into());
    }
    let methods: Vec<String> = doc["token_endpoint_auth_methods_supported"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| vec!["client_secret_basic".into()]);
    let basic_auth = methods.iter().any(|m| m == "client_secret_basic");
    if !basic_auth && !methods.iter().any(|m| m == "client_secret_post") {
        return Err("the provider takes neither client_secret_basic nor client_secret_post".into());
    }
    Ok(Discovery {
        authorization_endpoint: endpoint("authorization_endpoint")?,
        token_endpoint: endpoint("token_endpoint")?,
        jwks_uri: endpoint("jwks_uri")?,
        userinfo_endpoint: doc["userinfo_endpoint"]
            .as_str()
            .map(|u| check_url("userinfo_endpoint", u).map(|()| u.to_string()))
            .transpose()?,
        basic_auth,
    })
}

/// A provider's signing keys, by `kid` (`""` for a key published without
/// one). Only RSA signing keys of at least 2048 bits are kept.
pub fn keys_from(jwks: &serde_json::Value) -> HashMap<String, RsaPublicKey> {
    let mut out = HashMap::new();
    for k in jwks["keys"].as_array().into_iter().flatten() {
        if k["kty"].as_str() != Some("RSA") {
            continue;
        }
        if !matches!(k["use"].as_str(), None | Some("sig")) {
            continue;
        }
        if !matches!(k["alg"].as_str(), None | Some("RS256")) {
            continue;
        }
        let (Some(n), Some(e)) = (
            k["n"].as_str().and_then(b64url_decode),
            k["e"].as_str().and_then(b64url_decode),
        ) else {
            continue;
        };
        let Ok(key) = RsaPublicKey::new(BigUint::from_bytes_be(&n), BigUint::from_bytes_be(&e))
        else {
            continue;
        };
        if rsa::traits::PublicKeyParts::size(&key) * 8 < 2048 {
            continue;
        }
        out.insert(k["kid"].as_str().unwrap_or("").to_string(), key);
    }
    out
}

/// What an ID token said, once it has been checked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IdClaims {
    pub sub: String,
    pub email: Option<String>,
    pub email_verified: Option<bool>,
    pub name: Option<String>,
    pub preferred_username: Option<String>,
    pub hd: Option<String>,
}

/// Why an ID token was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum Reject {
    Malformed(&'static str),
    /// The header named an algorithm other than RS256.
    Algorithm(String),
    /// The `kid` is not in the key set we hold — worth one fetch of a
    /// fresh set, since providers rotate keys.
    UnknownKey,
    Signature,
    Issuer,
    Audience,
    Expired,
    NotYetValid,
    Nonce,
    Subject,
}

/// Check an ID token. Pure: the keys, the expectations and the clock are
/// all passed in, so every refusal has a test with no network at all.
pub fn verify_id_token(
    token: &str,
    keys: &HashMap<String, RsaPublicKey>,
    issuers: &[String],
    client_id: &str,
    nonce: &str,
    now: i64,
) -> Result<IdClaims, Reject> {
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Reject::Malformed("not three parts"));
    };
    let header: serde_json::Value = b64url_decode(h)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or(Reject::Malformed("header"))?;
    match header["alg"].as_str() {
        Some("RS256") => {}
        other => return Err(Reject::Algorithm(other.unwrap_or("").to_string())),
    }
    let key = match header["kid"].as_str() {
        Some(kid) => keys.get(kid).ok_or(Reject::UnknownKey)?,
        // No `kid`: allowed only when there is exactly one key to mean.
        None if keys.len() == 1 => keys.values().next().expect("one key"),
        None => return Err(Reject::UnknownKey),
    };
    let sig = b64url_decode(s).ok_or(Reject::Malformed("signature"))?;
    let digest = Sha256::digest(format!("{h}.{p}").as_bytes());
    key.verify(Pkcs1v15Sign::new::<Sha256>(), &digest, &sig)
        .map_err(|_| Reject::Signature)?;

    let c: serde_json::Value = b64url_decode(p)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or(Reject::Malformed("claims"))?;
    check_claims(&c, issuers, client_id, nonce, now)
}

/// Everything about an ID token but its signature: who issued it, for
/// whom, when, for which round trip, and about whom. Apart so the
/// recorded claims of a real provider's token (`fixtures/oidc`, whose
/// signatures a scrubbed recording cannot keep) are held to exactly the
/// rules a live sign-in is.
pub fn check_claims(
    c: &serde_json::Value,
    issuers: &[String],
    client_id: &str,
    nonce: &str,
    now: i64,
) -> Result<IdClaims, Reject> {
    if !c["iss"]
        .as_str()
        .is_some_and(|i| issuers.iter().any(|x| x == i))
    {
        return Err(Reject::Issuer);
    }
    let auds: Vec<&str> = match &c["aud"] {
        serde_json::Value::String(a) => vec![a.as_str()],
        serde_json::Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
        _ => vec![],
    };
    if !auds.contains(&client_id) {
        return Err(Reject::Audience);
    }
    match c["azp"].as_str() {
        Some(azp) if azp != client_id => return Err(Reject::Audience),
        None if auds.len() > 1 => return Err(Reject::Audience),
        _ => {}
    }
    let exp = number(&c["exp"]).ok_or(Reject::Malformed("exp"))?;
    if now > exp + SKEW_SECS {
        return Err(Reject::Expired);
    }
    let iat = number(&c["iat"]).ok_or(Reject::Malformed("iat"))?;
    if iat > now + SKEW_SECS {
        return Err(Reject::NotYetValid);
    }
    if nonce.is_empty() || c["nonce"].as_str() != Some(nonce) {
        return Err(Reject::Nonce);
    }
    let sub = c["sub"].as_str().unwrap_or("");
    if sub.is_empty() || sub.len() > 255 {
        return Err(Reject::Subject);
    }
    Ok(IdClaims {
        sub: sub.to_string(),
        email: c["email"].as_str().map(str::to_string),
        email_verified: flag(&c["email_verified"]),
        name: c["name"].as_str().map(str::to_string),
        preferred_username: c["preferred_username"].as_str().map(str::to_string),
        hd: c["hd"].as_str().map(str::to_string),
    })
}

fn number(v: &serde_json::Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

/// `true`, or the string `"true"` — some providers send the claim as a
/// string, and reading that as "not verified" would lock their people out.
fn flag(v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::String(s) if s == "true" => Some(true),
        serde_json::Value::String(s) if s == "false" => Some(false),
        _ => None,
    }
}

/// Whether the server trusts the address the provider gave, and so may
/// link or make an account by it.
#[derive(Debug, Clone, PartialEq)]
pub enum Trust {
    Email(String),
    /// No address, or one the provider did not vouch for.
    NoEmail,
    /// An address outside the domains this provider speaks for.
    Domain,
}

pub fn trusted_email(cfg: &OidcConfig, c: &IdClaims) -> Trust {
    let Some(email) = c
        .email
        .as_deref()
        .map(stratum_control::users::normalize_email)
    else {
        return Trust::NoEmail;
    };
    if !stratum_control::users::valid_email(&email) {
        return Trust::NoEmail;
    }
    // A provider that says the address is *not* verified is believed,
    // whatever its domain.
    if c.email_verified == Some(false) {
        return Trust::NoEmail;
    }
    let domain = email.rsplit_once('@').map(|(_, d)| d).unwrap_or("");
    if is_google(&cfg.issuer) {
        // Google says which Workspace an account belongs to in `hd`; an
        // address at the right domain on an ordinary Google account is
        // not the company's.
        return match c.hd.as_deref().map(str::to_ascii_lowercase) {
            Some(hd) if cfg.allowed_domains.contains(&hd) && domain == hd => Trust::Email(email),
            _ => Trust::Domain,
        };
    }
    if !cfg.allowed_domains.is_empty() {
        return if cfg.allowed_domains.iter().any(|d| d == domain) {
            Trust::Email(email)
        } else {
            Trust::Domain
        };
    }
    if c.email_verified == Some(true) {
        Trust::Email(email)
    } else {
        Trust::NoEmail
    }
}

/// The issuers a token may name: the configured one — and for Google,
/// its documented bare-host spelling too.
pub fn accepted_issuers(issuer: &str) -> Vec<String> {
    let mut v = vec![issuer.to_string()];
    if is_google(issuer) {
        v.push("accounts.google.com".into());
    }
    v
}

/// PKCE: the S256 challenge for a verifier.
pub fn pkce_challenge(verifier: &str) -> String {
    crate::mirror::origin::b64url(&Sha256::digest(verifier.as_bytes()))
}

/// Base64url, padded or not.
pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    if s.contains(['+', '/']) {
        return None;
    }
    crate::authx::base64_decode(&s.replace('-', "+").replace('_', "/"))
}

/// Tokens the provider answered a code with.
#[derive(Debug)]
pub struct Tokens {
    pub id_token: String,
    pub access_token: Option<String>,
}

/// Why a code exchange failed.
#[derive(Debug)]
pub enum Exchange {
    /// The provider said no to the code — spent, stale or foreign.
    Refused(String),
    /// The provider said no to *this server*: `invalid_client` or
    /// `unauthorized_client` (RFC 6749 §5.2) — a wrong secret, a client
    /// that was deleted, or credentials sent in a way it does not take.
    /// Kept apart from [`Exchange::Refused`] because "start again" is the
    /// right answer to a stale code and a useless one here: every
    /// sign-in would fail the same way, forever, and the person would be
    /// told it was their round trip.
    Client(String),
    /// The provider did not answer, or answered nonsense.
    Unanswered(String),
}

/// Which kind of refusal a token endpoint's `error` code is.
fn refusal(error: Option<&str>, detail: String) -> Exchange {
    match error {
        Some("invalid_client" | "unauthorized_client") => Exchange::Client(detail),
        _ => Exchange::Refused(detail),
    }
}

/// The configured provider, with what has been fetched from it.
pub struct Oidc {
    pub cfg: OidcConfig,
    agent: ureq::Agent,
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    discovery: Option<(Discovery, Instant)>,
    keys: HashMap<String, RsaPublicKey>,
    keys_at: Option<Instant>,
}

impl Oidc {
    /// The organization newcomers join, if it exists and is an
    /// organization rather than somebody's personal namespace.
    pub fn org_id(&self, db: &stratum_control::ControlDb) -> Result<String, String> {
        let org = stratum_control::registry::org_by_name(db, &self.cfg.org)?.ok_or_else(|| {
            format!(
                "STRATUM_OIDC_ORG names {:?}, and there is no such organization — \
                 make it with `admin bootstrap --org {}`",
                self.cfg.org, self.cfg.org
            )
        })?;
        if stratum_control::registry::is_personal(db, &org.id)? {
            return Err(format!(
                "STRATUM_OIDC_ORG names {:?}, which is a person's namespace, not an organization",
                self.cfg.org
            ));
        }
        Ok(org.id)
    }

    pub fn new(cfg: OidcConfig) -> Oidc {
        Oidc {
            cfg,
            agent: ureq::AgentBuilder::new()
                .timeout(HTTP_TIMEOUT)
                .redirects(0)
                .build(),
            cache: Mutex::new(Cache::default()),
        }
    }

    fn get_json(&self, url: &str, bearer: Option<&str>) -> Result<serde_json::Value, String> {
        let mut req = self.agent.get(url).set("Accept", "application/json");
        if let Some(t) = bearer {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let resp = req.call().map_err(|e| format!("GET {url}: {e}"))?;
        let text = resp.into_string().map_err(|e| format!("read {url}: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("{url}: {e}"))
    }

    /// Discovery, fetched on first use and again after an hour — never at
    /// boot, because a provider that is down must not stop this server
    /// starting.
    pub fn discovery(&self) -> Result<Discovery, String> {
        if let Some((d, at)) = &self.cache.lock().unwrap().discovery {
            if at.elapsed() < DISCOVERY_TTL {
                return Ok(d.clone());
            }
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.cfg.issuer.trim_end_matches('/')
        );
        let doc = self.get_json(&url, None)?;
        let d = discovery_from(&doc, &self.cfg.issuer)?;
        self.cache.lock().unwrap().discovery = Some((d.clone(), Instant::now()));
        Ok(d)
    }

    /// The URL the browser is sent to.
    pub fn authorize_url(
        &self,
        d: &Discovery,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
        challenge: &str,
    ) -> String {
        let enc = crate::mail::templates::urlencode;
        let sep = if d.authorization_endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        format!(
            "{}{sep}response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}\
             &code_challenge={}&code_challenge_method=S256",
            d.authorization_endpoint,
            enc(&self.cfg.client_id),
            enc(redirect_uri),
            enc("openid email profile"),
            enc(state),
            enc(nonce),
            enc(challenge),
        )
    }

    /// Trade the code for tokens.
    pub fn exchange(
        &self,
        d: &Discovery,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<Tokens, Exchange> {
        let enc = crate::mail::templates::urlencode;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ];
        let mut req = self
            .agent
            .post(&d.token_endpoint)
            .set("Accept", "application/json");
        if d.basic_auth {
            // RFC 6749 §2.3.1: each half form-encoded before the Basic
            // encoding, which matters for a secret with a `:` or `%`.
            let pair = format!(
                "{}:{}",
                enc(&self.cfg.client_id),
                enc(&self.cfg.client_secret)
            );
            req = req.set(
                "Authorization",
                &format!("Basic {}", stratum_store::b64::encode(pair.as_bytes())),
            );
        } else {
            form.push(("client_id", &self.cfg.client_id));
            form.push(("client_secret", &self.cfg.client_secret));
        }
        let url = d.token_endpoint.clone();
        let text = match req.send_form(&form) {
            Ok(r) => r
                .into_string()
                .map_err(|e| Exchange::Unanswered(format!("read {url}: {e}")))?,
            // A refusal is a 4xx with an `error` in the body.
            Err(ureq::Error::Status(code, r)) if (400..500).contains(&code) => {
                let body = r.into_string().unwrap_or_default();
                let error = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["error"].as_str().map(str::to_string));
                return Err(refusal(error.as_deref(), format!("{code}: {body}")));
            }
            Err(e) => return Err(Exchange::Unanswered(format!("POST {url}: {e}"))),
        };
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| Exchange::Unanswered(format!("token response: {e}")))?;
        if let Some(err) = v["error"].as_str() {
            return Err(refusal(Some(err), err.to_string()));
        }
        let id_token = v["id_token"]
            .as_str()
            .ok_or_else(|| Exchange::Unanswered("the token response has no id_token".into()))?;
        Ok(Tokens {
            id_token: id_token.to_string(),
            access_token: v["access_token"].as_str().map(str::to_string),
        })
    }

    /// Check an ID token against the provider's keys, fetching them the
    /// first time and again — at most once a minute — for a `kid` we do
    /// not hold, since providers rotate.
    pub fn verify(&self, d: &Discovery, id_token: &str, nonce: &str) -> Result<IdClaims, Reject> {
        let issuers = accepted_issuers(&self.cfg.issuer);
        let now = crate::cdn::now_secs() as i64;
        let check = |keys: &HashMap<String, RsaPublicKey>| {
            verify_id_token(id_token, keys, &issuers, &self.cfg.client_id, nonce, now)
        };
        let (keys, fresh_enough) = {
            let c = self.cache.lock().unwrap();
            let fresh_enough = c.keys_at.is_some_and(|at| at.elapsed() < JWKS_REFETCH_MIN);
            (c.keys.clone(), fresh_enough)
        };
        match check(&keys) {
            Err(Reject::UnknownKey) if !fresh_enough => {}
            other => return other,
        }
        let fetched = self
            .get_json(&d.jwks_uri, None)
            .map(|v| keys_from(&v))
            .unwrap_or_default();
        {
            let mut c = self.cache.lock().unwrap();
            c.keys = fetched.clone();
            c.keys_at = Some(Instant::now());
        }
        check(&fetched)
    }

    /// How many usable signing keys the provider publishes right now —
    /// for `admin sso-check`, which asks before anybody signs in.
    pub fn key_count(&self, d: &Discovery) -> Result<usize, String> {
        let n = keys_from(&self.get_json(&d.jwks_uri, None)?).len();
        if n == 0 {
            return Err(format!(
                "{} holds no RSA signing key of at least 2048 bits",
                d.jwks_uri
            ));
        }
        Ok(n)
    }

    /// Ask userinfo for the address when the ID token carried none —
    /// Entra ID's default shape, unless its admin configures the claim.
    /// The answer must be about the same `sub`, per the spec.
    ///
    /// `Err` when userinfo was needed and could not be believed: it did
    /// not answer, or answered for a different `sub`. The caller decides
    /// what that costs — nothing for somebody already linked, who needs
    /// no address, and an `error` rather than `noemail` for a newcomer,
    /// because "the provider gave no address" would send them to look at
    /// their own account for a fault that is the provider's.
    pub fn complete(
        &self,
        d: &Discovery,
        access_token: Option<&str>,
        c: &mut IdClaims,
    ) -> Result<(), String> {
        if c.email.is_some() {
            return Ok(());
        }
        let (Some(url), Some(token)) = (&d.userinfo_endpoint, access_token) else {
            return Ok(());
        };
        let v = self
            .get_json(url, Some(token))
            .map_err(|e| format!("userinfo: {e}"))?;
        merge_userinfo(c, &v)
    }
}

/// Take the address from a userinfo answer — only if it is about the
/// person the ID token named. Entra ID's `sub` is pairwise, per
/// application; a userinfo answer keyed differently is somebody else's
/// as far as this server can tell.
pub fn merge_userinfo(c: &mut IdClaims, v: &serde_json::Value) -> Result<(), String> {
    if v["sub"].as_str() != Some(c.sub.as_str()) {
        return Err(format!(
            "userinfo answered for sub {:?}, not the ID token's {:?}",
            v["sub"], c.sub
        ));
    }
    c.email = v["email"].as_str().map(str::to_string);
    c.email_verified = flag(&v["email_verified"]);
    if c.name.is_none() {
        c.name = v["name"].as_str().map(str::to_string);
    }
    if c.hd.is_none() {
        c.hd = v["hd"].as_str().map(str::to_string);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_testkit::oidc as fake;

    const ISS: &str = "https://idp.acme.test";
    const CLIENT: &str = "spool-client";

    fn keys() -> HashMap<String, RsaPublicKey> {
        keys_from(&fake::jwks())
    }

    fn now() -> i64 {
        crate::cdn::now_secs() as i64
    }

    fn claims() -> serde_json::Value {
        serde_json::json!({
            "iss": ISS, "sub": "s-1", "aud": CLIENT, "iat": now(), "exp": now() + 600,
            "nonce": "n-1", "email": "dana@acme.test", "email_verified": true,
        })
    }

    fn verify(token: &str) -> Result<IdClaims, Reject> {
        verify_id_token(token, &keys(), &[ISS.to_string()], CLIENT, "n-1", now())
    }

    #[test]
    fn a_good_token_is_read() {
        let got = verify(&fake::sign(&fake::header(), &claims())).unwrap();
        assert_eq!(got.sub, "s-1");
        assert_eq!(got.email.as_deref(), Some("dana@acme.test"));
        assert_eq!(got.email_verified, Some(true));
    }

    /// Every check, each with a token wrong in only that way.
    #[test]
    fn a_token_wrong_in_any_one_way_is_refused_for_that_reason() {
        let h = fake::header();
        let with = |k: &str, v: serde_json::Value| {
            let mut c = claims();
            c[k] = v;
            fake::sign(&h, &c)
        };
        let unsigned = format!(
            "{}.{}.",
            fake::b64url(br#"{"alg":"none"}"#),
            fake::b64url(claims().to_string().as_bytes())
        );
        let mut hs = h.clone();
        hs["alg"] = "HS256".into();
        let mut stranger = h.clone();
        stranger["kid"] = "not-ours".into();
        let good = fake::sign(&h, &claims());
        let (input, _) = good.rsplit_once('.').unwrap();
        let other = fake::sign(&h, &serde_json::json!({ "x": 1 }));
        let forged = format!("{input}.{}", other.rsplit('.').next().unwrap());
        let mut two_auds = claims();
        two_auds["aud"] = serde_json::json!([CLIENT, "somebody-else"]);
        let mut wrong_azp = claims();
        wrong_azp["azp"] = "somebody-else".into();
        for (why, token, want) in [
            ("alg none", unsigned, Reject::Algorithm("none".into())),
            (
                "alg HS256",
                fake::sign(&hs, &claims()),
                Reject::Algorithm("HS256".into()),
            ),
            (
                "unknown kid",
                fake::sign(&stranger, &claims()),
                Reject::UnknownKey,
            ),
            ("forged signature", forged, Reject::Signature),
            (
                "issuer",
                with("iss", "https://evil.test".into()),
                Reject::Issuer,
            ),
            (
                "audience",
                with("aud", "another-app".into()),
                Reject::Audience,
            ),
            (
                "two audiences, no azp",
                fake::sign(&h, &two_auds),
                Reject::Audience,
            ),
            ("azp not us", fake::sign(&h, &wrong_azp), Reject::Audience),
            (
                "expired",
                with("exp", (now() - 600).into()),
                Reject::Expired,
            ),
            (
                "issued later",
                with("iat", (now() + 600).into()),
                Reject::NotYetValid,
            ),
            ("nonce", with("nonce", "n-2".into()), Reject::Nonce),
            (
                "no nonce",
                with("nonce", serde_json::Value::Null),
                Reject::Nonce,
            ),
            ("empty sub", with("sub", "".into()), Reject::Subject),
            (
                "garbage",
                "a.b".into(),
                Reject::Malformed("not three parts"),
            ),
        ] {
            assert_eq!(verify(&token).unwrap_err(), want, "{why}");
        }
        // An empty expected nonce matches nothing, not a token without one.
        let mut no_nonce = claims();
        no_nonce["nonce"] = "".into();
        assert_eq!(
            verify_id_token(
                &fake::sign(&h, &no_nonce),
                &keys(),
                &[ISS.into()],
                CLIENT,
                "",
                now()
            ),
            Err(Reject::Nonce)
        );
        // Within the clock allowance is fine, both ways.
        assert!(verify(&with("exp", (now() - 60).into())).is_ok());
        assert!(verify(&with("iat", (now() + 60).into())).is_ok());
        // Two audiences are fine when `azp` names us.
        let mut ok_azp = two_auds.clone();
        ok_azp["azp"] = CLIENT.into();
        assert!(verify(&fake::sign(&h, &ok_azp)).is_ok());
    }

    /// Discovery must name the configured issuer and offer RS256; the
    /// token endpoint's client authentication follows what it offers.
    #[test]
    fn discovery_is_held_to_the_issuer_and_to_rs256() {
        let doc = |iss: &str, algs: serde_json::Value, auth: serde_json::Value| {
            serde_json::json!({
                "issuer": iss,
                "authorization_endpoint": format!("{ISS}/authorize"),
                "token_endpoint": format!("{ISS}/token"),
                "jwks_uri": format!("{ISS}/jwks"),
                "id_token_signing_alg_values_supported": algs,
                "token_endpoint_auth_methods_supported": auth,
            })
        };
        let rs = serde_json::json!(["RS256"]);
        let basic = serde_json::json!(["client_secret_basic"]);
        assert!(
            discovery_from(&doc(ISS, rs.clone(), basic.clone()), ISS)
                .unwrap()
                .basic_auth
        );
        assert!(discovery_from(
            &doc("https://elsewhere.test", rs.clone(), basic.clone()),
            ISS
        )
        .is_err());
        assert!(discovery_from(&doc(&format!("{ISS}/"), rs.clone(), basic.clone()), ISS).is_err());
        assert!(discovery_from(&doc(ISS, serde_json::json!(["ES256"]), basic), ISS).is_err());
        let post = discovery_from(
            &doc(ISS, rs.clone(), serde_json::json!(["client_secret_post"])),
            ISS,
        )
        .unwrap();
        assert!(!post.basic_auth);
        assert!(
            discovery_from(&doc(ISS, rs, serde_json::json!(["private_key_jwt"])), ISS).is_err()
        );
        // An endpoint that is plain HTTP off this machine is refused.
        let mut http = doc(
            ISS,
            serde_json::json!(["RS256"]),
            serde_json::json!(["client_secret_basic"]),
        );
        http["token_endpoint"] = "http://idp.acme.test/token".into();
        assert!(discovery_from(&http, ISS).is_err());
    }

    /// Only RSA signing keys of a real size are kept.
    #[test]
    fn only_rsa_signing_keys_are_kept() {
        let mut set = fake::jwks();
        let good = set["keys"][0].clone();
        let mut enc_key = good.clone();
        enc_key["use"] = "enc".into();
        enc_key["kid"] = "enc".into();
        let mut ec = good.clone();
        ec["kty"] = "EC".into();
        ec["kid"] = "ec".into();
        let mut small = good.clone();
        small["n"] = fake::b64url(&[0xff; 64]).into();
        small["kid"] = "small".into();
        set["keys"] = serde_json::json!([good, enc_key, ec, small]);
        let got = keys_from(&set);
        assert_eq!(got.keys().collect::<Vec<_>>(), vec![fake::TEST_KID]);
    }

    fn cfg(issuer: &str, domains: &[&str]) -> OidcConfig {
        OidcConfig {
            issuer: issuer.into(),
            client_id: CLIENT.into(),
            client_secret: "s".into(),
            name: "SSO".into(),
            org: "acme".into(),
            role: Role::Member,
            allowed_domains: domains.iter().map(|d| d.to_string()).collect(),
            session_ttl_secs: 3600,
        }
    }

    #[test]
    fn which_addresses_are_trusted() {
        let c = |email: Option<&str>, verified: Option<bool>, hd: Option<&str>| IdClaims {
            sub: "s".into(),
            email: email.map(str::to_string),
            email_verified: verified,
            hd: hd.map(str::to_string),
            ..IdClaims::default()
        };
        let open = cfg(ISS, &[]);
        let listed = cfg(ISS, &["acme.test"]);
        let google = cfg("https://accounts.google.com", &["acme.test"]);
        let t = |cfg: &OidcConfig, c: IdClaims| trusted_email(cfg, &c);
        // Without a domain list: only what the provider marked verified.
        assert_eq!(
            t(&open, c(Some("A@Acme.test"), Some(true), None)),
            Trust::Email("a@acme.test".into())
        );
        assert_eq!(t(&open, c(Some("a@acme.test"), None, None)), Trust::NoEmail);
        assert_eq!(t(&open, c(None, Some(true), None)), Trust::NoEmail);
        // With one: the domain decides, and nothing outside it gets in.
        assert_eq!(
            t(&listed, c(Some("a@acme.test"), None, None)),
            Trust::Email("a@acme.test".into())
        );
        assert_eq!(
            t(&listed, c(Some("a@evil.test"), Some(true), None)),
            Trust::Domain
        );
        assert_eq!(
            t(&listed, c(Some("a@sub.acme.test"), Some(true), None)),
            Trust::Domain
        );
        // An explicit "not verified" is believed whatever the domain.
        assert_eq!(
            t(&listed, c(Some("a@acme.test"), Some(false), None)),
            Trust::NoEmail
        );
        // Google: the hosted-domain claim decides, not the address.
        assert_eq!(
            t(
                &google,
                c(Some("a@acme.test"), Some(true), Some("acme.test"))
            ),
            Trust::Email("a@acme.test".into())
        );
        assert_eq!(
            t(&google, c(Some("a@acme.test"), Some(true), None)),
            Trust::Domain
        );
        assert_eq!(
            t(
                &google,
                c(Some("a@gmail.com"), Some(true), Some("acme.test"))
            ),
            Trust::Domain
        );
    }

    #[test]
    fn a_string_true_is_verified() {
        let mut c = claims();
        c["email_verified"] = "true".into();
        assert_eq!(
            verify(&fake::sign(&fake::header(), &c))
                .unwrap()
                .email_verified,
            Some(true)
        );
    }

    #[test]
    fn microsofts_shared_endpoints_and_plain_http_are_refused() {
        assert!(microsoft_shared(
            "https://login.microsoftonline.com/common/v2.0"
        ));
        assert!(microsoft_shared(
            "https://login.microsoftonline.com/Organizations/v2.0"
        ));
        assert!(microsoft_shared(
            "https://login.microsoftonline.com/consumers/v2.0"
        ));
        assert!(!microsoft_shared(
            "https://login.microsoftonline.com/0b1c2d3e-0000-4000-8000-000000000000/v2.0"
        ));
        assert!(check_url("x", "http://127.0.0.1:9000").is_ok());
        assert!(check_url("x", "http://localhost/realms/acme").is_ok());
        assert!(check_url("x", "http://idp.acme.test").is_err());
        assert!(check_url("x", "http://127.0.0.1.evil.test").is_err());
        assert!(check_url("x", "https://idp.acme.test").is_ok());
    }

    #[test]
    fn sso_only_defaults_to_on_when_configured_and_cannot_lock_everybody_out() {
        let env = |v: Option<&'static str>| {
            move |k: &str| {
                (k == "STRATUM_SSO_ONLY")
                    .then_some(v)
                    .flatten()
                    .map(str::to_string)
            }
        };
        assert_eq!(sso_only_from(env(None), true), Ok(true));
        assert_eq!(sso_only_from(env(None), false), Ok(false));
        assert_eq!(sso_only_from(env(Some("false")), true), Ok(false));
        assert!(sso_only_from(env(Some("true")), false).is_err());
        assert!(sso_only_from(env(Some("maybe")), true).is_err());
    }

    /// `docker-compose.yml` passes every SSO setting through as
    /// `${SPOOL_…:-}`, so a stack nobody configured SSO on boots with
    /// every one of them set to the empty string. That must be SSO off —
    /// not half a configuration refusing to boot, and not SSO-only
    /// refusing because nothing is configured. `deploy-validation` boots
    /// exactly that, and this is its assumption, checked without docker.
    #[test]
    fn the_compose_files_empty_settings_mean_no_sso() {
        let compose = include_str!("../../../docker-compose.yml");
        let names: Vec<&str> = compose
            .lines()
            .filter_map(|l| l.trim().split_once(':'))
            .map(|(k, _)| k)
            .filter(|k| k.starts_with("STRATUM_OIDC_") || *k == "STRATUM_SSO_ONLY")
            .collect();
        assert!(
            names.len() >= 4,
            "compose passes no SSO settings: {names:?}"
        );
        let empty = |k: &str| names.contains(&k).then(String::new);
        assert!(matches!(config_from(empty), Ok(None)));
        assert_eq!(sso_only_from(empty, false), Ok(false));
        // And an empty SSO_ONLY beside a configured provider is the
        // default, on — not "false" by being blank.
        assert_eq!(sso_only_from(empty, true), Ok(true));
    }

    #[test]
    fn base64url_decodes_both_ways_and_refuses_the_other_alphabet() {
        assert_eq!(b64url_decode("-_8").unwrap(), vec![0xfb, 0xff]);
        assert_eq!(b64url_decode(&fake::b64url(b"hello")).unwrap(), b"hello");
        assert!(b64url_decode("+/8").is_none());
    }

    /// Every provider `scripts/manual-oidc.sh fixtures` has recorded, and
    /// the fake's own answers beside them, run through the rules a live
    /// sign-in applies: discovery, the key set, the ID token's claims,
    /// userinfo, the trust rule, and what a refused code is. A provider
    /// whose real answers these rules cannot read is found here, not by
    /// the first person at a customer who tries to sign in.
    #[test]
    fn oidc_fixtures_parse_like_the_fake() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../stratum-testkit/fixtures/oidc");
        let read = |dir: &std::path::Path, f: &str| -> Option<serde_json::Value> {
            let text = std::fs::read_to_string(dir.join(f)).ok()?;
            Some(serde_json::from_str(&text).unwrap_or_else(|e| panic!("{dir:?}/{f}: {e}")))
        };
        let mut dirs: Vec<_> = std::fs::read_dir(&root)
            .expect("fixtures/oidc")
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        assert!(
            dirs.iter().any(|d| d.ends_with("belief")),
            "the fake's own answers are missing from {root:?}"
        );
        let mut observed = 0;
        for dir in &dirs {
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            let prov = read(dir, "provenance.json").expect("provenance.json");
            if prov["observed"] == true {
                observed += 1;
            }
            let issuer = prov["issuer"].as_str().unwrap();
            let client = prov["client_id"].as_str().unwrap();

            // Discovery, as the server reads it, and the client
            // authentication it would choose.
            let d = discovery_from(&read(dir, "discovery.json").unwrap(), issuer)
                .unwrap_or_else(|e| panic!("{name}: discovery: {e}"));
            assert_eq!(
                d.basic_auth,
                prov["client_auth"] == "basic",
                "{name}: client authentication"
            );

            // The key set, and the recorded token's key among it.
            let keys = keys_from(&read(dir, "jwks.json").unwrap());
            assert!(!keys.is_empty(), "{name}: no usable signing key");
            let header = read(dir, "id-token-header.json").unwrap();
            assert_eq!(header["alg"], "RS256", "{name}");
            match header["kid"].as_str() {
                Some(kid) => assert!(keys.contains_key(kid), "{name}: kid {kid} unpublished"),
                None => assert_eq!(keys.len(), 1, "{name}: no kid, and several keys"),
            }

            // The claims, held to every rule but the signature (which a
            // scrubbed recording cannot keep; the script checked it).
            let raw = read(dir, "id-token-claims.json").unwrap();
            let at = raw["iat"].as_f64().unwrap() as i64 + 1;
            let mut claims = check_claims(
                &raw,
                &accepted_issuers(issuer),
                client,
                prov["nonce"].as_str().unwrap(),
                at,
            )
            .unwrap_or_else(|r| panic!("{name}: the recorded claims are refused: {r:?}"));

            // Userinfo, when the token had no address to give.
            if claims.email.is_none() {
                if let Some(info) = read(dir, "userinfo.json") {
                    merge_userinfo(&mut claims, &info)
                        .unwrap_or_else(|e| panic!("{name}: userinfo: {e}"));
                }
            }

            // The trust rule, under the domains that run was made with,
            // lands where the run saw it land.
            let domains: Vec<&str> = prov["allowed_domains"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d.as_str().unwrap())
                .collect();
            let trust = match trusted_email(&cfg(issuer, &domains), &claims) {
                Trust::Email(_) => "email",
                Trust::NoEmail => "noemail",
                Trust::Domain => "domain",
            };
            assert_eq!(Some(trust), prov["trust"].as_str(), "{name}: trust");

            // A code the provider never issued is a refusal of the code,
            // not of the client — or the server would tell every person
            // their round trip was stale when its own secret was wrong.
            let refused = read(dir, "token-error.json").unwrap();
            let status = refused["status"].as_u64().unwrap();
            assert!(
                (400..500).contains(&status) || status == 200,
                "{name}: {status}"
            );
            assert!(
                matches!(
                    refusal(refused["error"].as_str(), String::new()),
                    Exchange::Refused(_)
                ),
                "{name}: a made-up code read as a client refusal"
            );
        }

        // The belief is the fake: its key and header are what the suite
        // signs with, so the two cannot drift apart unnoticed.
        let belief = root.join("belief");
        assert_eq!(read(&belief, "jwks.json").unwrap(), fake::jwks());
        assert_eq!(
            read(&belief, "id-token-header.json").unwrap(),
            fake::header()
        );
        if observed == 0 {
            eprintln!(
                "oidc fixtures: no real provider recorded yet — the suite is held to the \
                 fake's belief only. scripts/manual-oidc.sh all, then fixtures."
            );
        }
    }
}
