//! A hermetic OpenID Connect provider, and the signing it rests on.
//!
//! What the server's single sign-on talks to in every automated test:
//! discovery, the authorization endpoint, the token endpoint (code +
//! PKCE + client authentication), the signing keys and userinfo. Raw-TCP
//! HTTP like [`crate::fake_github`], so there is no framework between a
//! test and the bytes the server reads.
//!
//! **Everything this fake does is a belief about Okta, Entra ID, Google
//! and Keycloak**, and `scripts/manual-oidc.sh` is where those beliefs
//! meet the real thing. Where the providers are known to differ, the
//! fake can be told to behave like the awkward one — Entra's ID token
//! with no `email` in it, for instance — rather than like the easiest.
//!
//! The provider signs with RS256 under a fixed test-only key (the one the
//! fake GitHub App uses), so a token minted here is byte-for-byte
//! reproducible and nothing generates keys at test time.

use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::Pkcs1v15Sign;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The `kid` the fake's key is published under.
pub const TEST_KID: &str = "spool-test-1";

fn key() -> rsa::RsaPrivateKey {
    rsa::RsaPrivateKey::from_pkcs1_pem(crate::fake_github::TEST_APP_KEY_PEM)
        .expect("the test key parses")
}

/// Base64url without padding, as JOSE writes it.
pub fn b64url(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(A[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(A[n as usize & 63] as char);
        }
    }
    out
}

/// A compact JWS over `header` and `claims`, RS256 under the test key —
/// whatever `header` claims its algorithm is. A test that wants a token
/// whose header lies gets exactly that.
pub fn sign(header: &serde_json::Value, claims: &serde_json::Value) -> String {
    let input = format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(claims.to_string().as_bytes())
    );
    let sig = key()
        .sign(
            Pkcs1v15Sign::new::<Sha256>(),
            &Sha256::digest(input.as_bytes()),
        )
        .expect("sign");
    format!("{input}.{}", b64url(&sig))
}

/// The ordinary header: RS256 under [`TEST_KID`].
pub fn header() -> serde_json::Value {
    serde_json::json!({ "alg": "RS256", "typ": "JWT", "kid": TEST_KID })
}

/// The provider's published key set.
pub fn jwks() -> serde_json::Value {
    let public = key().to_public_key();
    serde_json::json!({ "keys": [{
        "kty": "RSA", "use": "sig", "alg": "RS256", "kid": TEST_KID,
        "n": b64url(&public.n().to_bytes_be()),
        "e": b64url(&public.e().to_bytes_be()),
    }]})
}

/// Who signs in at the fake.
#[derive(Debug, Clone, Default)]
pub struct Person {
    pub sub: String,
    pub email: Option<String>,
    /// `None` leaves the claim out, which is what Entra ID does.
    pub email_verified: Option<bool>,
    pub name: Option<String>,
    pub preferred_username: Option<String>,
    /// Google's hosted-domain claim.
    pub hd: Option<String>,
}

impl Person {
    /// An ordinary person at a provider that proves addresses.
    pub fn verified(sub: &str, email: &str) -> Person {
        Person {
            sub: sub.into(),
            email: Some(email.into()),
            email_verified: Some(true),
            name: Some(format!("Person {sub}")),
            ..Person::default()
        }
    }
}

/// How the next ID token is made wrong. Each is one of the checks the
/// server must make; a token wrong in exactly one way is how a test
/// proves that check is made.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Tamper {
    #[default]
    None,
    /// `alg: none` and no signature.
    AlgNone,
    /// `alg: HS256`, "signed" with the public key's bytes as a secret —
    /// the key-confusion attack.
    AlgHs256,
    /// Signed properly, under a `kid` the key set does not have.
    UnknownKid,
    /// A signature over different bytes.
    BadSignature,
    Issuer(String),
    Audience(String),
    /// A second audience, and no `azp` naming the client.
    ExtraAudienceNoAzp,
    Expired,
    IssuedInTheFuture,
    Nonce(String),
    NoSubject,
}

/// What `/userinfo` does.
#[derive(Debug, Clone, PartialEq)]
pub enum Userinfo {
    /// Answers for the person the access token was issued to.
    Answers,
    /// Answers for somebody else: a `sub` that is not the ID token's.
    OtherSubject,
    /// Answers 500.
    Down,
}

struct Grant {
    person: Person,
    client_id: String,
    redirect_uri: String,
    nonce: String,
    challenge: String,
}

/// What the fake has been told, and what it has seen.
pub struct State {
    /// Who `/authorize` signs in. `None` serves a sign-in form instead,
    /// for a person at a browser.
    pub person: Option<Person>,
    /// `/authorize` answers `error=access_denied`, as a Cancel does.
    pub deny: bool,
    pub tamper: Tamper,
    /// Leave `email` and `email_verified` out of the ID token and answer
    /// them only from userinfo — Entra ID's default shape.
    pub email_only_in_userinfo: bool,
    /// Accept client authentication only in the POST body, as some
    /// providers are configured to.
    pub post_auth_only: bool,
    /// How `/userinfo` misbehaves, if it does.
    pub userinfo: Userinfo,
    codes: HashMap<String, Grant>,
    access: HashMap<String, Person>,
}

pub struct FakeOidc {
    /// The issuer — and base URL — the fake answers as.
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub state: Arc<Mutex<State>>,
    pub token_calls: Arc<AtomicU64>,
    pub jwks_calls: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
}

impl Drop for FakeOidc {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.issuer.trim_start_matches("http://"));
    }
}

impl FakeOidc {
    /// Sign in as `p` from now on.
    pub fn sign_in_as(&self, p: Person) {
        self.state.lock().unwrap().person = Some(p);
    }

    pub fn set_tamper(&self, t: Tamper) {
        self.state.lock().unwrap().tamper = t;
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }
}

/// Spawn the fake on an ephemeral loopback port, for `client_id` with
/// `client_secret`.
pub fn spawn(client_id: &str, client_secret: &str) -> FakeOidc {
    spawn_on("127.0.0.1:0", client_id, client_secret)
}

/// As [`spawn`], on a given address — for the manual stack, where the
/// server is configured with the issuer before the fake starts.
pub fn spawn_on(addr: &str, client_id: &str, client_secret: &str) -> FakeOidc {
    let listener = TcpListener::bind(addr).expect("bind fake oidc");
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(State {
        person: None,
        deny: false,
        tamper: Tamper::None,
        email_only_in_userinfo: false,
        post_auth_only: false,
        userinfo: Userinfo::Answers,
        codes: HashMap::new(),
        access: HashMap::new(),
    }));
    let token_calls = Arc::new(AtomicU64::new(0));
    let jwks_calls = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let ctx = Arc::new(Ctx {
        issuer: issuer.clone(),
        client_id: client_id.into(),
        client_secret: client_secret.into(),
        state: state.clone(),
        token_calls: token_calls.clone(),
        jwks_calls: jwks_calls.clone(),
    });
    let stop = shutdown.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(mut stream) = stream else { continue };
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = handle(&mut stream, &ctx);
            });
        }
    });
    FakeOidc {
        issuer,
        client_id: client_id.into(),
        client_secret: client_secret.into(),
        state,
        token_calls,
        jwks_calls,
        shutdown,
    }
}

struct Ctx {
    issuer: String,
    client_id: String,
    client_secret: String,
    state: Arc<Mutex<State>>,
    token_calls: Arc<AtomicU64>,
    jwks_calls: Arc<AtomicU64>,
}

fn handle(stream: &mut TcpStream, ctx: &Ctx) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut content_length = 0usize;
    let mut authorization = None;
    for l in lines {
        let (k, v) = l.split_once(':').unwrap_or((l, ""));
        if k.trim().eq_ignore_ascii_case("content-length") {
            content_length = v.trim().parse().unwrap_or(0);
        }
        if k.trim().eq_ignore_ascii_case("authorization") {
            authorization = Some(v.trim().to_string());
        }
    }
    // Drain the body before answering, for the reason fake_github gives.
    let want = header_end + 4 + content_length;
    while buf.len() < want {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8_lossy(&buf[(header_end + 4).min(buf.len())..]).to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let q = parse_form(query);

    let reply = match (method, path) {
        ("GET", "/.well-known/openid-configuration") => json(
            200,
            &serde_json::json!({
                "issuer": ctx.issuer,
                "authorization_endpoint": format!("{}/authorize", ctx.issuer),
                "token_endpoint": format!("{}/token", ctx.issuer),
                "jwks_uri": format!("{}/jwks", ctx.issuer),
                "userinfo_endpoint": format!("{}/userinfo", ctx.issuer),
                "response_types_supported": ["code"],
                "subject_types_supported": ["public"],
                "id_token_signing_alg_values_supported": ["RS256"],
                "token_endpoint_auth_methods_supported":
                    if ctx.state.lock().unwrap().post_auth_only {
                        serde_json::json!(["client_secret_post"])
                    } else {
                        serde_json::json!(["client_secret_basic", "client_secret_post"])
                    },
                "code_challenge_methods_supported": ["S256"],
            }),
        ),
        ("GET", "/jwks") => {
            ctx.jwks_calls.fetch_add(1, Ordering::SeqCst);
            json(200, &jwks())
        }
        ("GET", "/authorize") => authorize(ctx, &q, None),
        ("POST", "/authorize/approve") => {
            let f = parse_form(&body);
            let email = f.get("email").cloned().unwrap_or_default();
            let person = Person {
                sub: format!("sub-{email}"),
                email: Some(email.clone()),
                email_verified: Some(true),
                name: f.get("name").cloned().filter(|n| !n.is_empty()),
                ..Person::default()
            };
            authorize(ctx, &f, Some(person))
        }
        ("POST", "/token") => token(ctx, &body, authorization.as_deref()),
        ("GET", "/userinfo") => userinfo(ctx, authorization.as_deref()),
        _ => json(404, &serde_json::json!({ "error": "not_found" })),
    };
    stream.write_all(&reply)
}

fn authorize(ctx: &Ctx, q: &HashMap<String, String>, chosen: Option<Person>) -> Vec<u8> {
    let redirect_uri = q.get("redirect_uri").cloned().unwrap_or_default();
    let state_param = q.get("state").cloned().unwrap_or_default();
    if q.get("client_id").map(String::as_str) != Some(ctx.client_id.as_str())
        || q.get("response_type").map(String::as_str) != Some("code")
    {
        return json(400, &serde_json::json!({ "error": "invalid_request" }));
    }
    let mut st = ctx.state.lock().unwrap();
    if st.deny {
        return redirect(&format!(
            "{redirect_uri}?error=access_denied&state={}",
            enc(&state_param)
        ));
    }
    let Some(person) = chosen.or_else(|| st.person.clone()) else {
        drop(st);
        return form(q);
    };
    if q.get("code_challenge_method").map(String::as_str) != Some("S256") {
        return json(400, &serde_json::json!({ "error": "invalid_request" }));
    }
    let code = format!("code-{}", st.codes.len() + st.access.len() + 1);
    st.codes.insert(
        code.clone(),
        Grant {
            person,
            client_id: ctx.client_id.clone(),
            redirect_uri: redirect_uri.clone(),
            nonce: q.get("nonce").cloned().unwrap_or_default(),
            challenge: q.get("code_challenge").cloned().unwrap_or_default(),
        },
    );
    redirect(&format!(
        "{redirect_uri}?code={}&state={}",
        enc(&code),
        enc(&state_param)
    ))
}

/// A sign-in page for a person at a browser: the walkthrough fills it
/// in the way somebody signing in at their company's provider would.
fn form(q: &HashMap<String, String>) -> Vec<u8> {
    let hidden: String = q
        .iter()
        .map(|(k, v)| {
            format!(
                "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                html(k),
                html(v)
            )
        })
        .collect();
    let page = format!(
        "<!doctype html><html><head><title>Stand-in identity provider</title></head>\
         <body><h1>Sign in to your company</h1>\
         <form method=\"post\" action=\"/authorize/approve\">{hidden}\
         <label>Email <input name=\"email\" type=\"email\" required></label>\
         <label>Name <input name=\"name\"></label>\
         <button type=\"submit\">Sign in</button></form></body></html>"
    );
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        page.len()
    )
    .into_bytes();
    out.extend_from_slice(page.as_bytes());
    out
}

fn token(ctx: &Ctx, body: &str, authorization: Option<&str>) -> Vec<u8> {
    ctx.token_calls.fetch_add(1, Ordering::SeqCst);
    let f = parse_form(body);
    let mut st = ctx.state.lock().unwrap();
    // Client authentication: HTTP Basic (the spec's default) or in the
    // body, and the fake refuses a client that sent neither — or, when
    // told to, one that used Basic.
    let basic = authorization
        .and_then(|a| a.strip_prefix("Basic "))
        .and_then(crate::oidc::b64std_decode)
        .and_then(|raw| String::from_utf8(raw).ok())
        .and_then(|s| {
            let (id, secret) = s.split_once(':')?;
            Some((parse_component(id), parse_component(secret)))
        });
    let posted = match (f.get("client_id"), f.get("client_secret")) {
        (Some(id), Some(secret)) => Some((id.clone(), secret.clone())),
        _ => None,
    };
    let creds = if st.post_auth_only {
        posted
    } else {
        basic.or(posted)
    };
    if creds != Some((ctx.client_id.clone(), ctx.client_secret.clone())) {
        return json(401, &serde_json::json!({ "error": "invalid_client" }));
    }
    if f.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return json(
            400,
            &serde_json::json!({ "error": "unsupported_grant_type" }),
        );
    }
    let code = f.get("code").cloned().unwrap_or_default();
    // Single use: a code is spent by being presented, right or wrong.
    let Some(grant) = st.codes.remove(&code) else {
        return json(400, &serde_json::json!({ "error": "invalid_grant" }));
    };
    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
    if grant.client_id != ctx.client_id
        || f.get("redirect_uri") != Some(&grant.redirect_uri)
        || b64url(&Sha256::digest(verifier.as_bytes())) != grant.challenge
    {
        return json(400, &serde_json::json!({ "error": "invalid_grant" }));
    }
    let access = format!("at-{code}");
    st.access.insert(access.clone(), grant.person.clone());
    let id_token = id_token(ctx, &st, &grant);
    json(
        200,
        &serde_json::json!({
            "access_token": access, "token_type": "Bearer", "expires_in": 3600,
            "id_token": id_token,
        }),
    )
}

fn id_token(ctx: &Ctx, st: &State, grant: &Grant) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let p = &grant.person;
    let mut c = serde_json::json!({
        "iss": ctx.issuer, "sub": p.sub, "aud": ctx.client_id,
        "iat": now, "exp": now + 3600, "nonce": grant.nonce,
    });
    if !st.email_only_in_userinfo {
        if let Some(e) = &p.email {
            c["email"] = e.clone().into();
        }
        if let Some(v) = p.email_verified {
            c["email_verified"] = v.into();
        }
    }
    if let Some(n) = &p.name {
        c["name"] = n.clone().into();
    }
    if let Some(u) = &p.preferred_username {
        c["preferred_username"] = u.clone().into();
    }
    if let Some(h) = &p.hd {
        c["hd"] = h.clone().into();
    }
    let mut h = header();
    match &st.tamper {
        Tamper::None => {}
        Tamper::AlgNone => {
            let head = b64url(
                serde_json::json!({ "alg": "none", "typ": "JWT" })
                    .to_string()
                    .as_bytes(),
            );
            return format!("{head}.{}.", b64url(c.to_string().as_bytes()));
        }
        Tamper::AlgHs256 => {
            use hmac::{Hmac, Mac};
            let secret = jwks()["keys"][0]["n"].as_str().unwrap().to_string();
            let head = b64url(
                serde_json::json!({ "alg": "HS256", "typ": "JWT", "kid": TEST_KID })
                    .to_string()
                    .as_bytes(),
            );
            let input = format!("{head}.{}", b64url(c.to_string().as_bytes()));
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
            mac.update(input.as_bytes());
            return format!("{input}.{}", b64url(&mac.finalize().into_bytes()));
        }
        Tamper::UnknownKid => h["kid"] = "somebody-elses".into(),
        Tamper::BadSignature => {
            // A real signature — over different claims.
            let good = sign(&h, &c);
            let (input, _) = good.rsplit_once('.').unwrap();
            let other = sign(&h, &serde_json::json!({ "sub": "not this" }));
            let other_sig = other.rsplit('.').next().unwrap();
            return format!("{input}.{other_sig}");
        }
        Tamper::Issuer(i) => c["iss"] = i.clone().into(),
        Tamper::Audience(a) => c["aud"] = a.clone().into(),
        Tamper::ExtraAudienceNoAzp => {
            c["aud"] = serde_json::json!([ctx.client_id, "another-client"]);
        }
        Tamper::Expired => {
            c["iat"] = (now - 7200).into();
            c["exp"] = (now - 3600).into();
        }
        Tamper::IssuedInTheFuture => c["iat"] = (now + 3600).into(),
        Tamper::Nonce(n) => c["nonce"] = n.clone().into(),
        Tamper::NoSubject => c["sub"] = "".into(),
    }
    sign(&h, &c)
}

fn userinfo(ctx: &Ctx, authorization: Option<&str>) -> Vec<u8> {
    let st = ctx.state.lock().unwrap();
    let Some(p) = authorization
        .and_then(|a| a.strip_prefix("Bearer "))
        .and_then(|t| st.access.get(t))
    else {
        return json(401, &serde_json::json!({ "error": "invalid_token" }));
    };
    let sub = match st.userinfo {
        Userinfo::Answers => p.sub.clone(),
        Userinfo::OtherSubject => format!("{}-somebody-else", p.sub),
        Userinfo::Down => return json(500, &serde_json::json!({ "error": "server_error" })),
    };
    let mut c = serde_json::json!({ "sub": sub });
    if let Some(e) = &p.email {
        c["email"] = e.clone().into();
    }
    if let Some(v) = p.email_verified {
        c["email_verified"] = v.into();
    }
    if let Some(n) = &p.name {
        c["name"] = n.clone().into();
    }
    json(200, &c)
}

fn json(status: u16, v: &serde_json::Value) -> Vec<u8> {
    let body = v.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        500 => "Internal Server Error",
        _ => "Not Found",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn redirect(location: &str) -> Vec<u8> {
    format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .into_bytes()
}

/// `application/x-www-form-urlencoded`, decoded.
pub fn parse_form(s: &str) -> HashMap<String, String> {
    s.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (parse_component(k), parse_component(v))
        })
        .collect()
}

fn parse_component(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Percent-encode a query component.
pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Standard-alphabet base64, for a Basic header.
pub fn b64std_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut nbits = 0;
    for &c in s.trim_end_matches('=').as_bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------
// Driving the server's side of the round trip, one leg at a time, so a
// test can break any leg on purpose.
// ---------------------------------------------------------------------

fn no_redirects() -> ureq::Agent {
    ureq::AgentBuilder::new().redirects(0).build()
}

fn call(req: ureq::Request) -> ureq::Response {
    match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("transport: {e}"),
    }
}

/// What `GET /v1/auth/sso/start` answered.
#[derive(Debug, Clone)]
pub struct Started {
    pub status: u16,
    /// Where the browser is sent — the provider's authorization URL.
    pub location: String,
    /// The round-trip cookie, as `name=value`, if one was set.
    pub cookie: Option<String>,
}

pub fn start(server_base: &str) -> Started {
    let r = call(no_redirects().get(&format!("{server_base}/v1/auth/sso/start")));
    let cookie = r
        .all("set-cookie")
        .into_iter()
        .find(|c| c.starts_with("weft_sso="))
        .and_then(|c| c.split(';').next().map(str::to_string));
    Started {
        status: r.status(),
        location: r.header("location").unwrap_or_default().to_string(),
        cookie,
    }
}

/// Visit the provider's authorization URL as the browser would, and
/// return where it sends the browser back to.
pub fn visit_provider(url: &str) -> String {
    let r = call(no_redirects().get(url));
    assert_eq!(
        r.status(),
        302,
        "the provider did not redirect back (is anybody signed in at the fake?)"
    );
    r.header("location").unwrap_or_default().to_string()
}

/// How the round trip ended.
#[derive(Debug, Clone)]
pub struct Landing {
    pub status: u16,
    /// The `?sso=` outcome the dashboard was sent.
    pub outcome: String,
    /// The session the server issued, as the bare cookie value.
    pub session: Option<String>,
    /// Every `Set-Cookie` the callback answered with.
    pub set_cookies: Vec<String>,
}

/// Arrive at the server's callback, carrying `cookie` (`name=value`) or
/// none.
pub fn finish(callback: &str, cookie: Option<&str>) -> Landing {
    let mut req = no_redirects().get(callback);
    if let Some(c) = cookie {
        req = req.set("Cookie", c);
    }
    let r = call(req);
    let location = r.header("location").unwrap_or_default().to_string();
    let outcome = location
        .split_once("?sso=")
        .map(|(_, o)| o.split('&').next().unwrap_or("").to_string())
        .unwrap_or_default();
    let set_cookies: Vec<String> = r
        .all("set-cookie")
        .into_iter()
        .map(str::to_string)
        .collect();
    let session = set_cookies
        .iter()
        .find_map(|c| c.strip_prefix("stratum_session="))
        .and_then(|c| c.split(';').next())
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    Landing {
        status: r.status(),
        outcome,
        session,
        set_cookies,
    }
}

/// The whole round trip, as a browser that follows every redirect.
pub fn round_trip(server_base: &str) -> Landing {
    let s = start(server_base);
    assert_eq!(s.status, 303, "start did not redirect: {s:?}");
    if !s.location.contains("/authorize") {
        // Refused before reaching the provider: land where start sent us.
        let outcome = s
            .location
            .split_once("?sso=")
            .map(|(_, o)| o.to_string())
            .unwrap_or_default();
        return Landing {
            status: s.status,
            outcome,
            session: None,
            set_cookies: vec![],
        };
    }
    let back = visit_provider(&s.location);
    finish(&back, s.cookie.as_deref())
}
