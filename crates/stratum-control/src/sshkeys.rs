//! SSH public keys: registration, lookup, revocation.
//!
//! A key never carries its own permissions. It authenticates as one of
//! two things, and the row says which:
//!
//! * a **person** (`user_id`) — the normal case for a developer's laptop
//!   key. Their authority is resolved from their current role on every
//!   connection, so a demotion, a per-repo grant or an offboarding takes
//!   effect on the next `git push` with nothing to revoke by hand.
//! * a **token** (`token_id`) — a deploy key, whose authority is exactly
//!   that token's. This is what every key was before people existed.
//!
//! Either way the SSH transport proves possession of the private key,
//! computes the OpenSSH SHA-256 fingerprint of the presented public key,
//! and resolves it here on every connection.

use crate::db::ControlDb;
use crate::ids::{now_ms, ulid};
use sha2::{Digest, Sha256};

/// Key types the transport accepts. `ssh-rsa` here names the KEY format;
/// signature negotiation (rsa-sha2-*) is the transport's concern.
const ALLOWED_ALGOS: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
];

#[derive(Debug, Clone)]
pub struct SshKey {
    pub id: String,
    /// The namespace a *deploy* key belongs to. `None` for a personal
    /// key: that names a person, and a person belongs to as many
    /// namespaces as they have joined. Which one they are reaching is
    /// decided from the repository path, where membership is checked
    /// anyway.
    pub org_id: Option<String>,
    /// The token this key acts as — a deploy key. Mutually exclusive
    /// with `user_id`.
    pub token_id: Option<String>,
    /// The person this key acts as. Mutually exclusive with `token_id`.
    pub user_id: Option<String>,
    pub algo: String,
    /// Normalized `<algo> <base64-blob>` (comment stripped).
    pub pubkey: String,
    /// OpenSSH-format `SHA256:<base64-nopad>` over the raw key blob —
    /// byte-identical to `ssh-keygen -lf`.
    pub fingerprint_sha256: String,
    pub label: Option<String>,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
}

/// A syntactically valid public key line, decoded.
#[derive(Debug, Clone)]
pub struct ParsedKey {
    pub algo: String,
    pub blob: Vec<u8>,
}

impl ParsedKey {
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.blob)
    }

    /// The normalized stored form: `<algo> <base64-blob>`.
    pub fn line(&self) -> String {
        format!("{} {}", self.algo, b64_encode(&self.blob))
    }
}

/// Parse an authorized_keys-style line: `<algo> <base64> [comment…]`.
/// The base64 blob must decode and its embedded type string must match
/// the declared algorithm (an inconsistent line is an attack or a paste
/// accident — reject both).
pub fn parse_pubkey(line: &str) -> Result<ParsedKey, String> {
    let mut parts = line.split_whitespace();
    let algo = parts.next().ok_or("empty public key")?.to_string();
    let b64 = parts.next().ok_or("public key missing base64 data")?;
    if !ALLOWED_ALGOS.contains(&algo.as_str()) {
        return Err(format!(
            "unsupported key type {algo:?} (accepted: {})",
            ALLOWED_ALGOS.join(", ")
        ));
    }
    let blob = b64_decode(b64).ok_or("public key data is not valid base64")?;
    // Blob layout: u32 length + type string, then key material.
    let embedded = blob
        .len()
        .checked_sub(4)
        .and_then(|_| {
            let n = u32::from_be_bytes(blob.get(0..4)?.try_into().ok()?) as usize;
            blob.get(4..4 + n)
        })
        .ok_or("public key blob is truncated")?;
    if embedded != algo.as_bytes() {
        return Err("public key blob does not match its declared type".into());
    }
    Ok(ParsedKey { algo, blob })
}

/// OpenSSH SHA-256 fingerprint of a raw public-key blob.
pub fn fingerprint(blob: &[u8]) -> String {
    let digest = Sha256::digest(blob);
    format!("SHA256:{}", b64_encode_nopad(&digest))
}

const COLS: &str = "id, org_id, token_id, user_id, algo, pubkey, fingerprint_sha256, \
                    label, created_at, revoked_at";

/// The key and the record of who registered it commit together: a
/// credential nobody is recorded as having added must not exist.
fn insert(
    db: &ControlDb,
    key: &SshKey,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<(), String> {
    let blob = serde_json::json!({
        "key_id": key.id,
        "fingerprint": key.fingerprint_sha256,
        "token_id": key.token_id,
        "user_id": key.user_id,
    });
    let k = key.clone();
    db.lock()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO ssh_keys \
                 (id, org_id, token_id, user_id, algo, pubkey, fingerprint_sha256, \
                  label, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                &[
                    &k.id,
                    &k.org_id,
                    &k.token_id,
                    &k.user_id,
                    &k.algo,
                    &k.pubkey,
                    &k.fingerprint_sha256,
                    &k.label,
                    &k.created_at,
                ],
            )?;
            if let Some(ctx) = audit {
                crate::audit::record_tx(tx, ctx, None, "sshkey.add", Some(&blob))?;
            }
            Ok(())
        })
        .map_err(|e| {
            if crate::db::is_unique_violation(&e) {
                "this key is already registered".to_string()
            } else {
                e.to_string()
            }
        })?;
    Ok(())
}

/// Register a personal key for a person.
///
/// It names no token and no namespace: the person's authority is
/// resolved fresh on every connection, so nothing has to be re-issued
/// when their role changes — and one key reaches every namespace they
/// belong to, which is the whole point of it naming a person.
///
/// `org_id` is where they were standing when they added it, used only to
/// check they are a member somewhere before accepting a key. It is not
/// stored.
pub fn add_for_user(
    db: &ControlDb,
    org_id: &str,
    user_id: &str,
    pubkey_line: &str,
    label: Option<&str>,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<SshKey, String> {
    let parsed = parse_pubkey(pubkey_line)?;
    if crate::members::role_of(db, org_id, user_id)?.is_none() {
        return Err("no such member in this org".into());
    }
    let key = SshKey {
        id: ulid(),
        org_id: None,
        token_id: None,
        user_id: Some(user_id.into()),
        algo: parsed.algo.clone(),
        pubkey: parsed.line(),
        fingerprint_sha256: parsed.fingerprint(),
        label: label.map(str::to_string),
        created_at: now_ms(),
        revoked_at: None,
    };
    insert(db, &key, audit)?;
    Ok(key)
}

/// Register a deploy key against `token_id` (which must be an active
/// token in `org_id` — the key inherits exactly its authority).
pub fn add(
    db: &ControlDb,
    org_id: &str,
    token_id: &str,
    pubkey_line: &str,
    label: Option<&str>,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<SshKey, String> {
    let parsed = parse_pubkey(pubkey_line)?;
    let token_active: Option<bool> = db
        .lock()
        .query_opt(
            "SELECT revoked_at IS NULL FROM tokens WHERE id = $1 AND org_id = $2",
            &[&token_id, &org_id],
        )
        .map_err(|e| e.to_string())?
        .map(|r| r.get(0));
    match token_active {
        Some(true) => {}
        Some(false) => return Err("token is revoked".into()),
        None => return Err("no such token in this org".into()),
    }
    let key = SshKey {
        id: ulid(),
        org_id: Some(org_id.into()),
        token_id: Some(token_id.into()),
        user_id: None,
        algo: parsed.algo.clone(),
        pubkey: parsed.line(),
        fingerprint_sha256: parsed.fingerprint(),
        label: label.map(str::to_string),
        created_at: now_ms(),
        revoked_at: None,
    };
    insert(db, &key, audit)?;
    Ok(key)
}

/// Every key that can reach this org: its deploy keys, plus the personal
/// keys of the people who are members of it.
///
/// The join is what makes a personal key visible here at all, now that
/// such a key names a person rather than a namespace. Filtering on
/// `org_id` alone — which is what this did before — silently hid every
/// member's key from the administrator accountable for them. Found by
/// the manual browser pass, where the key just added stopped appearing.
pub fn list(db: &ControlDb, org_id: &str) -> Result<Vec<SshKey>, String> {
    let cols = COLS
        .split(", ")
        .map(|c| format!("k.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ");
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {cols} FROM ssh_keys k \
                 LEFT JOIN org_members m ON m.user_id = k.user_id AND m.org_id = $1 \
                 WHERE k.org_id = $1 OR m.user_id IS NOT NULL \
                 ORDER BY k.created_at, k.id"
            ),
            &[&org_id],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(from_row).collect())
}

/// Only this person's own keys — what a member may see and manage.
///
/// Not filtered by namespace: these are one person's keys, and they work
/// everywhere that person does. Showing a different list depending on
/// which org page they happened to open would be a lie about what the
/// key can reach.
pub fn list_for_user(db: &ControlDb, user_id: &str) -> Result<Vec<SshKey>, String> {
    if !crate::ids::valid_id(user_id) {
        return Ok(Vec::new());
    }
    let rows = db
        .lock()
        .query(
            &format!("SELECT {COLS} FROM ssh_keys WHERE user_id = $1 ORDER BY created_at, id"),
            &[&user_id],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(from_row).collect())
}

/// Resolve an ACTIVE key by fingerprint — the SSH auth path. Returns the
/// key row; the caller then resolves the token to a principal (and gets
/// token-revocation checking for free).
///
/// A personal key whose owner is **disabled** is not active. The
/// disabled check used to live one step later, in `members::role_of`,
/// which answered "no role anywhere" and so masked every namespace as
/// not found. That held only while a person with no role could read
/// nothing over SSH; once a key with no role reads public repositories
/// — a maintainer fetching a contributor's fork — a disabled account's
/// key would have gone on cloning them. A disabled account's *token* is
/// refused as a credential before any resource is named; its key now is
/// too, in the one query both auth steps go through.
pub fn lookup_active(db: &ControlDb, fingerprint: &str) -> Result<Option<SshKey>, String> {
    let row = db
        .lock()
        .query_opt(
            &format!(
                "SELECT {COLS} FROM ssh_keys \
                 WHERE fingerprint_sha256 = $1 AND revoked_at IS NULL \
                 AND NOT EXISTS (SELECT 1 FROM users u \
                                 WHERE u.id = ssh_keys.user_id \
                                 AND u.disabled_at IS NOT NULL)"
            ),
            &[&fingerprint],
        )
        .map_err(|e| e.to_string())?;
    Ok(row.as_ref().map(from_row))
}

/// Revoke a key this person owns. A member manages their own keys
/// without an administrator, and cannot touch anyone else's — including
/// the org's deploy keys, which belong to no one.
/// Revoke a key this person owns.
///
/// Keyed on the person, not on a namespace, for the same reason listing
/// is: the key is theirs and reaches everywhere they do, so revoking it
/// from one org page and leaving it live elsewhere would be the defect
/// this replaced.
pub fn revoke_own(
    db: &ControlDb,
    key_id: &str,
    user_id: &str,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    if !crate::ids::valid_id(user_id) {
        return Ok(false);
    }
    revoke_where(
        db,
        "UPDATE ssh_keys SET revoked_at = $3 \
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
        &[&key_id, &user_id],
        key_id,
        audit,
    )
}

/// Revoke any key that can reach this org — an administrator's reach.
///
/// That is this org's deploy keys, *and* the personal keys of its
/// members: offboarding somebody has to be able to cut the laptop key
/// they used here. Matching on `org_id` alone — which is what this did
/// before personal keys stopped carrying one — left an administrator
/// unable to revoke exactly the keys they are accountable for, while
/// still showing them the row.
///
/// It does not reach a personal key belonging to somebody who is not a
/// member here: that key is theirs and reaches other namespaces, and one
/// org's admin does not get to end their access everywhere.
pub fn revoke(
    db: &ControlDb,
    org_id: &str,
    key_id: &str,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    revoke_where(
        db,
        "UPDATE ssh_keys k SET revoked_at = $3 \
         WHERE k.id = $2 AND k.revoked_at IS NULL AND ( \
             k.org_id = $1 \
             OR EXISTS (SELECT 1 FROM org_members m \
                        WHERE m.user_id = k.user_id AND m.org_id = $1))",
        &[&org_id, &key_id],
        key_id,
        audit,
    )
}

/// The revocation and its record commit together, for the same reason
/// registration does.
fn revoke_where(
    db: &ControlDb,
    sql: &'static str,
    args: &[&(dyn postgres::types::ToSql + Sync)],
    key_id: &str,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    let now = now_ms();
    let blob = serde_json::json!({ "key_id": key_id });
    // Both callers pass two identifying arguments and the timestamp
    // last, so one binding shape covers them.
    let a0 = args[0];
    let a1 = args[1];
    db.lock()
        .transaction(move |tx| {
            let n = tx.execute(sql, &[a0, a1, &now])?;
            if n > 0 {
                if let Some(ctx) = audit {
                    crate::audit::record_tx(tx, ctx, None, "sshkey.revoke", Some(&blob))?;
                }
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("revoke ssh key: {e}"))
}

fn from_row(r: &postgres::Row) -> SshKey {
    SshKey {
        id: r.get("id"),
        org_id: r.get("org_id"),
        token_id: r.get("token_id"),
        user_id: r.get("user_id"),
        algo: r.get("algo"),
        pubkey: r.get("pubkey"),
        fingerprint_sha256: r.get("fingerprint_sha256"),
        label: r.get("label"),
        created_at: r.get("created_at"),
        revoked_at: r.get("revoked_at"),
    }
}

use stratum_store::b64::{
    decode as b64_decode, encode as b64_encode, encode_nopad as b64_encode_nopad,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{self, Scope};
    use crate::registry;

    // Constructed ed25519-shaped blob; fingerprint cross-checked against
    // python hashlib and the OpenSSH format definition. The e2e suite
    // additionally proves agreement with a real `ssh-keygen -lf`.
    const VECTOR_LINE: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGRlYWRiZWVmZGVhZGJlZWZkZWFkYmVlZmRlYWRiZWVm test@host";
    const VECTOR_FP: &str = "SHA256:xyrwjwNKqTIivpsCwlGBJoCJCtNo5voQKyNc3jGN9iI";

    fn setup() -> (ControlDb, String, String) {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-sshkeys")).unwrap();
        let org = registry::create_org(&db, "org").unwrap();
        let tok = auth::mint(&db, &org.id, &[Scope::RepoWrite], None, Some("ssh")).unwrap();
        (db, org.id, tok.id)
    }

    #[test]
    fn principal_resolution_fails_closed_for_unknown_token() {
        let (db, _, _) = setup();
        // A key row pointing at a token id that does not exist (tokens are
        // never deleted, but the transport must not assume that).
        assert!(auth::principal_for_token_id(&db, "no-such-token")
            .unwrap()
            .is_none());
    }

    #[test]
    fn parse_and_fingerprint_vector() {
        let k = parse_pubkey(VECTOR_LINE).unwrap();
        assert_eq!(k.algo, "ssh-ed25519");
        assert_eq!(k.fingerprint(), VECTOR_FP);
        // Normalization strips the comment and keeps algo + blob.
        assert_eq!(k.line(), VECTOR_LINE.rsplit_once(' ').unwrap().0);
        // Round-trip: the normalized line re-parses to the same blob.
        assert_eq!(parse_pubkey(&k.line()).unwrap().blob, k.blob);
    }

    #[test]
    fn parse_rejects_malformed_lines() {
        assert!(parse_pubkey("").is_err());
        assert!(parse_pubkey("ssh-ed25519").is_err());
        assert!(
            parse_pubkey("ssh-dss AAAA0000").is_err(),
            "unsupported type"
        );
        assert!(parse_pubkey("ssh-ed25519 !!!not-base64!!!").is_err());
        // Valid base64, but the embedded type says ssh-rsa: mismatch.
        let mismatched = VECTOR_LINE.replace("ssh-ed25519 ", "ssh-rsa ");
        assert!(parse_pubkey(&mismatched).is_err());
        // Truncated blob (length prefix runs past the data).
        assert!(parse_pubkey("ssh-ed25519 AAAAC3NzaC1l").is_err());
    }

    /// A person's own keys, keyed by an id that cannot exist, name
    /// nothing — the same contract every other user-id lookup keeps.
    #[test]
    fn personal_key_lookups_are_inert_for_malformed_user_ids() {
        let (db, org, _tok) = setup();
        for bad in [
            "",
            "ghost",
            "not an id",
            "\0",
            "01hx'; DROP TABLE ssh_keys;--",
        ] {
            assert!(list_for_user(&db, bad).unwrap().is_empty());
            assert!(!revoke_own(&db, "01hxaaaaaaaaaaaaaaaaaaaaaa", bad, None).unwrap());
            assert!(add_for_user(&db, &org, bad, VECTOR_LINE, None, None).is_err());
        }
    }

    #[test]
    fn add_lookup_revoke_lifecycle() {
        let (db, org, tok) = setup();
        let key = add(&db, &org, &tok, VECTOR_LINE, Some("laptop"), None).unwrap();
        assert_eq!(key.fingerprint_sha256, VECTOR_FP);
        assert_eq!(key.label.as_deref(), Some("laptop"));

        // Auth path: fingerprint → key → token principal.
        let found = lookup_active(&db, VECTOR_FP).unwrap().unwrap();
        assert_eq!(found.token_id.as_deref(), Some(tok.as_str()));
        assert!(found.user_id.is_none(), "a deploy key names no person");
        let p = auth::principal_for_token_id(&db, found.token_id.as_deref().unwrap())
            .unwrap()
            .unwrap();
        assert!(p.allows(Scope::RepoWrite, Some("any")));

        // Duplicate active fingerprint is refused…
        assert!(add(&db, &org, &tok, VECTOR_LINE, None, None)
            .unwrap_err()
            .contains("already registered"));

        // …revocation frees it and kills the auth path instantly.
        assert!(revoke(&db, &org, &key.id, None).unwrap());
        assert!(
            !revoke(&db, &org, &key.id, None).unwrap(),
            "second revoke is a no-op"
        );
        assert!(lookup_active(&db, VECTOR_FP).unwrap().is_none());
        let listed = list(&db, &org).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].revoked_at.is_some());

        // A revoked fingerprint may be deliberately re-registered.
        add(&db, &org, &tok, VECTOR_LINE, Some("again"), None).unwrap();
        assert!(lookup_active(&db, VECTOR_FP).unwrap().is_some());
    }

    #[test]
    fn key_dies_with_its_token_and_binds_to_real_tokens_only() {
        let (db, org, tok) = setup();
        add(&db, &org, &tok, VECTOR_LINE, None, None).unwrap();
        auth::revoke(&db, &org, &tok, None).unwrap();
        // The key row is still active, but the principal resolution the
        // transport performs comes back empty — instant cutoff.
        let key = lookup_active(&db, VECTOR_FP).unwrap().unwrap();
        assert!(
            auth::principal_for_token_id(&db, key.token_id.as_deref().unwrap())
                .unwrap()
                .is_none()
        );
        // New keys cannot bind to the dead token, nor to a foreign org's.
        assert!(add(&db, &org, &tok, VECTOR_LINE, None, None)
            .unwrap_err()
            .contains("revoked"));
        let other = registry::create_org(&db, "other").unwrap();
        assert!(add(&db, &other.id, &tok, VECTOR_LINE, None, None)
            .unwrap_err()
            .contains("no such token"));
    }
}
