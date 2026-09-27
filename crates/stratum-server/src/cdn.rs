//! CDN offload for clone traffic: resolving the URL a client is told to
//! fetch its bulk pack from (git's `packfile-uri`), and signing it for
//! CloudFront when the deployment enables edge auth.
//!
//! Why signed URLs rather than an authenticated route: git fetches the
//! advertised URL with **no credentials at all** (measured — the request
//! carries no Authorization header). Authorization therefore has to live
//! *in* the URL, which is exactly what a CloudFront signed URL is.
//!
//! Why resolution verifies the object exists first: if the advertised
//! pack 404s, an opted-in clone **aborts** rather than falling back —
//! the server has already excluded those objects from the inline stream.
//! Advertising is therefore only ever done for a pack we just confirmed.

use crate::app::SharedState;
use crate::workers::cdnpack;
use stratum_proto::serve::CdnPack;
use stratum_store::{LatencyModel, ObjectStore};

/// Config for handing out CDN pack URLs. `None` anywhere disables the
/// feature and the capability is never advertised.
#[derive(Clone, Debug)]
pub struct CdnConfig {
    /// Base the pack key is appended to, e.g. `https://d123.cloudfront.net`.
    pub base: String,
    /// CloudFront signing, when the CDN's origin is the object store and
    /// edge auth is enabled.
    pub signing: Option<CdnSigning>,
    /// HMAC secret for the *origin-route* mode, where the CDN's origin is
    /// this server rather than the bucket. Presence selects that mode.
    pub origin_secret: Option<String>,
    pub url_ttl_secs: u64,
}

#[derive(Clone, Debug)]
pub struct CdnSigning {
    pub key_pair_id: String,
    pub private_key_pem: String,
}

/// A pack the edge can serve, and how big it is.
///
/// The size rides beside the advertisement rather than inside it
/// because the wire never says it — git fetches the URL and the CDN
/// never reports back — and it is the only number the transfer meter
/// can record for an offloaded clone (`metering::pack_kind`).
#[derive(Clone, Debug)]
pub struct CdnOffer {
    pub pack: CdnPack,
    /// The stored pack's size in bytes, from its descriptor.
    pub size: u64,
}

/// Resolve the CDN pack to advertise for a repo, or `None` to serve the
/// normal inline clone. Every failure mode answers `None`: no descriptor,
/// an object that is not actually there, a store hiccup, the kill switch.
pub fn resolve(state: &SharedState, org: &str, repo: &str, prefix: &str) -> Option<CdnOffer> {
    let cfg = state.cdn.as_ref()?;
    let store = ObjectStore::new(&state.store_url, LatencyModel::None);
    let desc = cdnpack::load_descriptor(&store, prefix)?;
    // Verify-before-advertise: confirm the pack object exists right now.
    // A descriptor pointing at a deleted pack would break every opted-in
    // clone, so a missing object must degrade to the inline path.
    if !object_present(&store, &desc.pack_key) {
        return None;
    }
    // The wire path spells the repo `app.git`; the URL we hand out should
    // be the canonical name the REST route uses.
    let uri = url_for(
        cfg,
        org,
        repo.trim_end_matches(".git"),
        &desc.pack_key,
        now_secs() + cfg.url_ttl_secs,
    );
    if uri.is_empty() {
        // Signing failed; never advertise an unsigned URL for what may be
        // a private object.
        return None;
    }
    Some(CdnOffer {
        pack: CdnPack {
            uri,
            pack_hash: desc.pack_hash,
            tip: desc.tip,
            total_entries: desc.total_entries,
        },
        size: desc.size,
    })
}

fn object_present(store: &ObjectStore, key: &str) -> bool {
    // The store client has no HEAD; a 1-byte ranged read proves presence
    // without pulling the pack.
    store.get_stream(key, Some((0, 0))).is_ok()
}

/// The URL a client is told to fetch the bulk pack from.
///
/// Two deployment shapes, both real:
///   * **store origin** — the CDN fronts the bucket, so the URL path is
///     the pack key, CloudFront-signed when edge auth is configured;
///   * **origin route** — the CDN fronts *this server*, so the URL is the
///     pack route below, authorized by a short-lived HMAC token because
///     git sends no credentials.
pub fn url_for(cfg: &CdnConfig, org: &str, repo: &str, pack_key: &str, expires_at: u64) -> String {
    let base = cfg.base.trim_end_matches('/');
    if let Some(secret) = &cfg.origin_secret {
        let file = pack_key.rsplit('/').next().unwrap_or_default();
        let sig = origin_token(secret, pack_key, expires_at);
        return format!("{base}/v1/orgs/{org}/repos/{repo}/cdn/{file}?exp={expires_at}&sig={sig}");
    }
    let unsigned = format!("{base}/{}", pack_key.trim_start_matches('/'));
    match &cfg.signing {
        None => unsigned,
        Some(sign) => sign_canned_url(
            &unsigned,
            &sign.key_pair_id,
            &sign.private_key_pem,
            expires_at,
        )
        // A signing failure must never yield an *unsigned* URL for a
        // private object; callers treat the empty string as "no CDN".
        .unwrap_or_default(),
    }
}

/// A CloudFront **canned policy** signed URL, per AWS's specification:
/// RSA-SHA1 over the policy document, base64'd with CloudFront's
/// URL-safe alphabet (`+/=` → `-~_`).
pub fn sign_canned_url(
    url: &str,
    key_pair_id: &str,
    private_key_pem: &str,
    expires_at: u64,
) -> Result<String, String> {
    let policy = canned_policy(url, expires_at);
    let sig = rsa_sha1_sign(private_key_pem, policy.as_bytes())?;
    let sep = if url.contains('?') { '&' } else { '?' };
    Ok(format!(
        "{url}{sep}Expires={expires_at}&Signature={}&Key-Pair-Id={key_pair_id}",
        cf_base64(&sig)
    ))
}

/// The canned policy document. AWS matches this byte-for-byte, so the
/// field order and the absence of whitespace are load-bearing.
pub fn canned_policy(url: &str, expires_at: u64) -> String {
    format!(
        "{{\"Statement\":[{{\"Resource\":\"{url}\",\"Condition\":{{\"DateLessThan\":{{\"AWS:EpochTime\":{expires_at}}}}}}}]}}"
    )
}

/// PKCS#1 v1.5 RSA-**SHA1** signature (what CloudFront verifies; note the
/// GitHub-App path next door uses SHA-256 for JWTs). Accepts PKCS#1 and
/// PKCS#8 PEM, like `mirror::origin::rs256_sign`.
pub fn rsa_sha1_sign(pem: &str, data: &[u8]) -> Result<Vec<u8>, String> {
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::Pkcs1v15Sign;
    use sha1::{Digest, Sha1};
    let key = rsa::RsaPrivateKey::from_pkcs1_pem(pem)
        .or_else(|_| rsa::RsaPrivateKey::from_pkcs8_pem(pem))
        .map_err(|e| format!("cdn signing key: {e}"))?;
    let digest = Sha1::digest(data);
    key.sign(Pkcs1v15Sign::new::<Sha1>(), &digest)
        .map_err(|e| format!("cdn sign: {e}"))
}

/// Standard base64 with CloudFront's substitutions: `+`→`-`, `/`→`~`,
/// `=`→`_` (its URL-safe alphabet is not RFC 4648's).
pub fn cf_base64(data: &[u8]) -> String {
    stratum_store::b64::encode(data)
        .replace('+', "-")
        .replace('/', "~")
        .replace('=', "_")
}

/// The origin-route bearer: HMAC-SHA256 over the **full pack key** and the
/// expiry. Binding the key (not just the file name) is what stops a token
/// minted for one repo from fetching another's pack.
pub fn origin_token(secret: &str, pack_key: &str, expires_at: u64) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key");
    mac.update(pack_key.as_bytes());
    mac.update(b"\0");
    mac.update(expires_at.to_string().as_bytes());
    stratum_store::pack::hex(&mac.finalize().into_bytes())
}

/// Constant-time check of a presented token, including expiry. Anything
/// unparseable, stale, or mismatched is a refusal.
pub fn verify_origin_token(
    secret: &str,
    pack_key: &str,
    expires_at: u64,
    presented: &str,
    now: u64,
) -> bool {
    if expires_at <= now {
        return false;
    }
    let want = origin_token(secret, pack_key, expires_at);
    constant_time_eq(want.as_bytes(), presented.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> String {
        use rsa::pkcs8::EncodePrivateKey;
        let mut rng = rand::thread_rng();
        let key = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
        key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap()
            .to_string()
    }

    #[test]
    fn cf_base64_uses_cloudfronts_alphabet_not_rfc4648() {
        // CloudFront swaps +/= for -~_ . Bytes chosen to force both a '+'
        // and a '/' in standard base64, plus a padding case.
        // std "+/8=" → both substitutions plus padding, in one vector.
        assert_eq!(cf_base64(&[0xfb, 0xff]), "-~8_");
        assert_eq!(cf_base64(&[0xff, 0xff, 0xff]), "~~~~"); // '/'→'~'
        assert_eq!(cf_base64(b""), "");
        assert_eq!(cf_base64(b"f"), "Zg__");
        assert_eq!(cf_base64(b"fo"), "Zm8_");
        assert_eq!(cf_base64(b"foo"), "Zm9v");
        // Never emits a character that would need escaping in a query.
        let s = cf_base64(&[0u8, 16, 131, 16, 81, 135, 32, 146, 139]);
        assert!(!s.contains('+') && !s.contains('/') && !s.contains('='));
    }

    #[test]
    fn canned_policy_matches_the_documented_shape() {
        // AWS matches this byte-for-byte; field order and the absence of
        // whitespace are part of the contract.
        assert_eq!(
            canned_policy("https://d1.cloudfront.net/a.pack", 1700000000),
            "{\"Statement\":[{\"Resource\":\"https://d1.cloudfront.net/a.pack\",\
             \"Condition\":{\"DateLessThan\":{\"AWS:EpochTime\":1700000000}}}]}"
        );
    }

    /// The signature is verified with the public half in-process: this
    /// proves the digest (SHA-1) and padding (PKCS#1 v1.5) are what
    /// CloudFront will check, without needing a live distribution.
    #[test]
    fn signature_verifies_against_the_public_key() {
        use rsa::pkcs1v15::{Signature, VerifyingKey};
        use rsa::pkcs8::DecodePrivateKey;
        use rsa::signature::Verifier;
        use sha1::Sha1;

        let pem = test_key();
        let url = "https://d1.cloudfront.net/o/org/r/repo/prod/cdn/tip-hash.pack";
        let expires = 2_000_000_000u64;
        let policy = canned_policy(url, expires);
        let sig = rsa_sha1_sign(&pem, policy.as_bytes()).unwrap();

        let priv_key = rsa::RsaPrivateKey::from_pkcs8_pem(&pem).unwrap();
        let vk = VerifyingKey::<Sha1>::new(priv_key.to_public_key());
        vk.verify(
            policy.as_bytes(),
            &Signature::try_from(sig.as_slice()).unwrap(),
        )
        .expect("CloudFront must be able to verify this signature");

        // A tampered policy (e.g. a client extending the expiry) fails.
        let forged = canned_policy(url, expires + 86_400);
        assert!(vk
            .verify(
                forged.as_bytes(),
                &Signature::try_from(rsa_sha1_sign(&pem, policy.as_bytes()).unwrap().as_slice())
                    .unwrap()
            )
            .is_err());
    }

    #[test]
    fn signed_url_carries_the_three_required_query_params() {
        let pem = test_key();
        let cfg = CdnConfig {
            base: "https://d1.cloudfront.net".into(),
            signing: Some(CdnSigning {
                key_pair_id: "KEYPAIRID1".into(),
                private_key_pem: pem,
            }),
            origin_secret: None,
            url_ttl_secs: 3600,
        };
        let url = url_for(
            &cfg,
            "org",
            "repo",
            "o/org/r/repo/prod/cdn/t-h.pack",
            2_000_000_000,
        );
        assert!(url.starts_with("https://d1.cloudfront.net/o/org/r/repo/prod/cdn/t-h.pack?"));
        assert!(url.contains("Expires=2000000000"));
        assert!(url.contains("&Key-Pair-Id=KEYPAIRID1"));
        assert!(url.contains("&Signature="));
        // The signature must be query-safe.
        let sig = url
            .split("Signature=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        assert!(!sig.contains('+') && !sig.contains('/') && !sig.contains('='));
    }

    fn origin_cfg() -> CdnConfig {
        CdnConfig {
            base: "https://cdn.example.com".into(),
            signing: None,
            origin_secret: Some("origin-secret".into()),
            url_ttl_secs: 600,
        }
    }

    #[test]
    fn origin_route_urls_address_the_pack_route_with_a_bearer_token() {
        let cfg = origin_cfg();
        let key = "o/org1/r/repo1/prod/cdn/abc123-def456.pack";
        let url = url_for(&cfg, "acme", "app", key, 2_000_000_000);
        assert!(
            url.starts_with(
                "https://cdn.example.com/v1/orgs/acme/repos/app/cdn/abc123-def456.pack?"
            ),
            "{url}"
        );
        assert!(url.contains("exp=2000000000"));
        let sig = url.split("sig=").nth(1).unwrap();
        // Hex, so nothing in it needs escaping in a query string.
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(verify_origin_token(
            "origin-secret",
            key,
            2_000_000_000,
            sig,
            1_000
        ));
    }

    /// The token grants exactly one object in exactly one tenant, and only
    /// for as long as it says. Each of these is a way a bad actor would
    /// try to widen it.
    #[test]
    fn origin_tokens_do_not_travel_between_repos_expiries_or_secrets() {
        let a = "o/orgA/r/repoA/prod/cdn/aa-bb.pack";
        let b = "o/orgB/r/repoB/prod/cdn/aa-bb.pack";
        let exp = 2_000_000_000u64;
        let tok = origin_token("s", a, exp);

        assert!(verify_origin_token("s", a, exp, &tok, exp - 1));
        // Another tenant's pack with the same file name: refused.
        assert!(!verify_origin_token("s", b, exp, &tok, exp - 1));
        // Extending the expiry invalidates the signature over it.
        assert!(!verify_origin_token("s", a, exp + 86_400, &tok, exp));
        // A different signing secret never validates.
        assert!(!verify_origin_token("other", a, exp, &tok, exp - 1));
        // Flipping one hex digit.
        let mut tampered: Vec<char> = tok.chars().collect();
        tampered[0] = if tampered[0] == 'a' { 'b' } else { 'a' };
        let tampered: String = tampered.into_iter().collect();
        assert!(!verify_origin_token("s", a, exp, &tampered, exp - 1));
        // Truncation and emptiness are refusals, not panics.
        assert!(!verify_origin_token("s", a, exp, &tok[..10], exp - 1));
        assert!(!verify_origin_token("s", a, exp, "", exp - 1));
        // Expired exactly at the boundary, and past it.
        assert!(!verify_origin_token("s", a, exp, &tok, exp));
        assert!(!verify_origin_token("s", a, exp, &tok, exp + 1));
    }

    #[test]
    fn unsigned_deployments_get_a_plain_url_and_bad_keys_yield_nothing() {
        let plain = CdnConfig {
            base: "https://cdn.example.com/".into(),
            signing: None,
            origin_secret: None,
            url_ttl_secs: 60,
        };
        // Trailing slash on the base and leading slash on the key must not
        // produce a doubled separator.
        assert_eq!(
            url_for(&plain, "a", "b", "/o/a/r/b/prod/cdn/x.pack", 1),
            "https://cdn.example.com/o/a/r/b/prod/cdn/x.pack"
        );
        // A broken signing key must NEVER degrade to an unsigned URL for
        // what may be a private object — it yields nothing instead.
        let broken = CdnConfig {
            base: "https://d1.cloudfront.net".into(),
            signing: Some(CdnSigning {
                key_pair_id: "K".into(),
                private_key_pem:
                    "-----BEGIN PRIVATE KEY-----\nnot a key\n-----END PRIVATE KEY-----".into(),
            }),
            origin_secret: None,
            url_ttl_secs: 60,
        };
        assert_eq!(url_for(&broken, "a", "b", "k.pack", 1), "");
        assert!(rsa_sha1_sign("garbage", b"x").is_err());
    }
}
