//! Tokens: `weft_<id>_<secret>`, SHA-256 of the secret at rest.
//!
//! SHA-256 rather than a password KDF is deliberate: secrets are 256-bit
//! random strings, not human passwords — brute force is information-
//! theoretically hopeless, and a per-request KDF would wreck the <100 ms
//! repo-create budget or force token caching, which would break instant
//! revocation. Every request hits the database; revocation is one UPDATE.

use crate::db::ControlDb;
use crate::ids::{now_ms, token_secret, ulid};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    OrgAdmin,
    OrgRead,
    RepoRead,
    RepoWrite,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::OrgAdmin => "org:admin",
            Scope::OrgRead => "org:read",
            Scope::RepoRead => "repo:read",
            Scope::RepoWrite => "repo:write",
        }
    }

    fn parse(s: &str) -> Option<Scope> {
        match s {
            "org:admin" => Some(Scope::OrgAdmin),
            "org:read" => Some(Scope::OrgRead),
            "repo:read" => Some(Scope::RepoRead),
            "repo:write" => Some(Scope::RepoWrite),
            _ => None,
        }
    }
}

/// The verified identity attached to a request.
///
/// Reached three ways, all of which land here so the ~40 inline
/// authorization checks in the server need only one shape: an API token,
/// an SSH key resolving to one, or a browser session resolving to a
/// person. `token_id` is empty for a session — there is no token — which
/// is why `audit_id` prefers the user.
#[derive(Debug, Clone)]
pub struct Principal {
    pub token_id: String,
    pub org_id: String,
    pub scopes: Vec<Scope>,
    /// Some(_) = token restricted to that single repo.
    pub repo_id: Option<String>,
    /// The person this acts as, when there is one. None = an org-level
    /// service token, which is every token minted before users existed.
    pub user_id: Option<String>,
    /// For a personal access token: the scopes it was minted with.
    ///
    /// `scopes` above is the *effective* answer for the context resolved
    /// so far — the person's role narrowed by this ceiling. Narrowing is
    /// lossy, so the ceiling is kept: a per-repo grant replaces the org
    /// role on that repo, and computing the replacement needs the
    /// original ceiling rather than the already-narrowed result. None for
    /// a session or an SSH key, which have no ceiling beyond the role.
    pub ceiling: Option<Vec<Scope>>,
}

impl Principal {
    /// A principal derived from a person's role rather than a token.
    /// Repo-scoping is expressed by the caller resolving the effective
    /// role for that repo first, so the scopes here are already correct
    /// for the resource being reached.
    pub fn for_user(org_id: &str, user_id: &str, scopes: Vec<Scope>) -> Principal {
        Principal {
            token_id: String::new(),
            org_id: org_id.to_string(),
            scopes,
            repo_id: None,
            user_id: Some(user_id.to_string()),
            ceiling: None,
        }
    }
}

impl Principal {
    /// May this principal act with `need` on `repo_id` (None = org-level op)?
    pub fn allows(&self, need: Scope, repo_id: Option<&str>) -> bool {
        if let (Some(bound), Some(target)) = (self.repo_id.as_deref(), repo_id) {
            if bound != target {
                return false;
            }
        }
        // A repo-bound token never grants org-level operations.
        if self.repo_id.is_some() && repo_id.is_none() && need != Scope::OrgRead {
            return false;
        }
        self.scopes.iter().any(|s| grants(*s, need))
    }

    /// Audit identity. A person when there is one, so the trail reads
    /// `user:01hx…` rather than naming whichever credential they happened
    /// to use; the token only when it acts for no one.
    pub fn audit_id(&self) -> String {
        match &self.user_id {
            Some(u) => format!("user:{u}"),
            None => format!("token:{}", self.token_id),
        }
    }
}

/// Every scope, for computing intersections capability by capability.
const ALL_SCOPES: [Scope; 4] = [
    Scope::OrgAdmin,
    Scope::OrgRead,
    Scope::RepoRead,
    Scope::RepoWrite,
];

/// The authority two scope sets both allow.
///
/// Computed capability by capability rather than by matching names,
/// because scopes imply one another: `repo:write` carries `repo:read`.
/// Intersecting the names alone would leave a `repo:write` token held by
/// a viewer granting nothing at all, when it plainly should still read.
pub fn intersect(a: &[Scope], b: &[Scope]) -> Vec<Scope> {
    ALL_SCOPES
        .into_iter()
        .filter(|need| a.iter().any(|s| grants(*s, *need)) && b.iter().any(|s| grants(*s, *need)))
        .collect()
}

pub(crate) fn grants(have: Scope, need: Scope) -> bool {
    match have {
        Scope::OrgAdmin => true,
        Scope::OrgRead => matches!(need, Scope::OrgRead | Scope::RepoRead),
        Scope::RepoWrite => matches!(need, Scope::RepoWrite | Scope::RepoRead),
        Scope::RepoRead => need == Scope::RepoRead,
    }
}

pub struct MintedToken {
    pub id: String,
    /// The full plaintext token — shown once, never stored.
    pub plaintext: String,
}

fn hash_secret(secret: &str) -> String {
    stratum_hex(&Sha256::digest(secret.as_bytes()))
}

fn stratum_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// What a token is being minted *as*.
///
/// A struct rather than five more positional parameters, and not only to
/// quiet a lint: `repo_id`, `label` and `user_id` are three adjacent
/// `Option<&str>` that the compiler cannot tell apart, so a
/// transposition between them type-checks and mints a credential bound
/// to the wrong thing. Naming them at the call site is what makes that a
/// compile error instead of an incident.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mint<'a> {
    /// Bind to a single repository. `None` is org-wide.
    pub repo_id: Option<&'a str>,
    pub label: Option<&'a str>,
    /// The person who owns it. `None` is an org service token — nobody's
    /// personal credential, and it does not die with an offboarding.
    pub user_id: Option<&'a str>,
    /// When it stops working by itself, epoch millis. `None` is the
    /// personal-token shape: it lives until somebody revokes it. `Some`
    /// is the machine shape — a credential handed to something that may
    /// not outlive its own cleanup.
    pub expires_at: Option<i64>,
}

pub fn mint(
    db: &ControlDb,
    org_id: &str,
    scopes: &[Scope],
    repo_id: Option<&str>,
    label: Option<&str>,
) -> Result<MintedToken, String> {
    mint_for(
        db,
        org_id,
        scopes,
        Mint {
            repo_id,
            label,
            ..Default::default()
        },
        None,
    )
}

/// Mint a token owned by a person. Their tokens die when they are
/// disabled, and audit rows for its use name them rather than it.
pub fn mint_for(
    db: &ControlDb,
    org_id: &str,
    scopes: &[Scope],
    as_: Mint<'_>,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<MintedToken, String> {
    let Mint {
        repo_id,
        label,
        user_id,
        expires_at,
    } = as_;
    if scopes.is_empty() {
        return Err("token needs at least one scope".into());
    }
    let id = ulid();
    let secret = token_secret();
    let scope_str = scopes
        .iter()
        .map(Scope::as_str)
        .collect::<Vec<_>>()
        .join(",");
    let hash = hash_secret(&secret);
    let now = now_ms();
    // `expires_at` is in the audit blob because a credential's lifetime
    // is part of what was granted. "who minted a token with these
    // scopes" and "and it was good for four hours" are the same
    // question during an incident.
    let blob = audit.map(|_| {
        serde_json::json!({
            "token_id": id,
            "scopes": scope_str,
            "user_id": user_id,
            "expires_at": expires_at,
        })
    });
    // The token and the record of who minted it commit together: a
    // credential nobody is recorded as having created must not exist.
    let tx_id = id.clone();
    db.lock()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO tokens \
                   (id, org_id, hash, scopes, repo_id, label, created_at, user_id, expires_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                &[
                    &tx_id,
                    &org_id,
                    &hash,
                    &scope_str,
                    &repo_id,
                    &label,
                    &now,
                    &user_id,
                    &expires_at,
                ],
            )?;
            if let Some(ctx) = audit {
                // Deliberately org-level even for a repo-bound token: a
                // credential event is not a repo event, and filing it
                // under the repo would show the credential inventory to
                // anything holding a token for that repo. The binding is
                // in the blob, where only an org-level reader sees it.
                crate::audit::record_tx(tx, ctx, None, "token.mint", blob.as_ref())?;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    Ok(MintedToken {
        plaintext: format!("weft_{id}_{secret}"),
        id,
    })
}

/// The authority a token owned by a person actually carries.
///
/// The scope list frozen at mint time is a *ceiling*, not the answer:
/// their current org role is intersected with it on every request. So a
/// demotion shrinks every token they hold, and removal from the org kills
/// them outright — which is what "offboarding is total" has to mean for a
/// personal access token, not only for a browser session. `None` means
/// the person is no longer a member here and the credential is dead.
fn personal_scopes(
    db: &ControlDb,
    org_id: &str,
    user_id: &str,
    frozen: Vec<Scope>,
) -> Result<Option<Vec<Scope>>, String> {
    let Some(role) = crate::members::role_of(db, org_id, user_id)? else {
        return Ok(None);
    };
    Ok(Some(intersect(&frozen, &role.scopes())))
}

/// Whether a token's deadline has passed. `None` is "never expires",
/// which is every token minted before the column existed and every
/// personal access token since.
///
/// Compared against **our** clock at the moment of the request, on both
/// authentication paths, so a credential stops working at its deadline
/// rather than when something gets round to deleting the row. Pure, and
/// separately tested, because it is the whole of the new refusal and it
/// needs no database to exercise.
///
/// The comparison is `now >= at`, not `now > at`: a token whose deadline
/// is this millisecond is finished. Off by one in the other direction is
/// a credential that works for one tick longer than it was sold as, and
/// on a boundary the safe direction is the shorter life.
pub(crate) fn expired(expires_at: Option<i64>) -> bool {
    matches!(expires_at, Some(at) if crate::ids::now_ms() >= at)
}

/// Verify a presented token string. Constant-time hash comparison; every
/// call hits the database so revocation — and expiry — is instant (M5).
pub fn verify(db: &ControlDb, presented: &str) -> Result<Option<Principal>, String> {
    let rest = match presented.strip_prefix("weft_") {
        Some(r) => r,
        None => return Ok(None),
    };
    let (id, secret) = match rest.split_once('_') {
        Some(x) => x,
        None => return Ok(None),
    };
    let row = db
        .lock()
        .query_opt(
            "SELECT t.org_id, t.hash, t.scopes, t.repo_id, t.revoked_at, t.expires_at, \
                    t.user_id, u.disabled_at \
             FROM tokens t LEFT JOIN users u ON u.id = t.user_id WHERE t.id = $1",
            &[&id],
        )
        .map_err(|e| e.to_string())?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.get::<_, Option<i64>>("revoked_at").is_some() {
        return Ok(None);
    }
    if expired(row.get("expires_at")) {
        return Ok(None);
    }
    // A token owned by a disabled person is dead with them. Without this
    // a personal access token would outlive the offboarding that was
    // supposed to end their access.
    if row.get::<_, Option<i64>>("disabled_at").is_some() {
        return Ok(None);
    }
    let stored_hash: String = row.get("hash");
    let presented_hash = hash_secret(secret);
    if !constant_time_eq(presented_hash.as_bytes(), stored_hash.as_bytes()) {
        return Ok(None);
    }
    let org_id: String = row.get("org_id");
    let user_id: Option<String> = row.get("user_id");
    let frozen: Vec<Scope> = row
        .get::<_, String>("scopes")
        .split(',')
        .filter_map(Scope::parse)
        .collect();
    let scopes = match &user_id {
        Some(u) => match personal_scopes(db, &org_id, u, frozen.clone())? {
            Some(s) => s,
            None => return Ok(None),
        },
        None => frozen.clone(),
    };
    Ok(Some(Principal {
        token_id: id.to_string(),
        org_id,
        scopes,
        repo_id: row.get("repo_id"),
        ceiling: user_id.is_some().then_some(frozen),
        user_id,
    }))
}

/// Which token this string *is*, alive or not.
///
/// [`verify`] answers `None` for a token that is revoked, expired, or
/// owned by a disabled person, and that is the right answer for every
/// authorization decision — this function must never be used to make
/// one. It exists for the one question that outlives the credential:
/// *whose* token was that.
///
/// The hosted runner is the caller that needs it. Cancelling a job
/// revokes its token and then asks the platform to stop the container;
/// a container that survives the stop is supposed to learn it is
/// finished from the 410 on its next call, and it never did, because
/// the revoked token was refused at the door with a 401 the runner
/// reads as "retry later". Identifying the dead token is what lets that
/// call answer 410 to the runner that owns the job, and 401 to
/// everybody else.
///
/// The secret is still checked, constant-time, against the stored hash.
/// A token id alone proves nothing: ids travel in job rows and audit
/// entries, and answering on one would turn a job's state into
/// something anybody who has seen an id can read.
pub fn identify(db: &ControlDb, presented: &str) -> Result<Option<String>, String> {
    let Some(rest) = presented.strip_prefix("weft_") else {
        return Ok(None);
    };
    let Some((id, secret)) = rest.split_once('_') else {
        return Ok(None);
    };
    let row = db
        .lock()
        .query_opt("SELECT hash FROM tokens WHERE id = $1", &[&id])
        .map_err(|e| e.to_string())?;
    let Some(row) = row else {
        return Ok(None);
    };
    let stored_hash: String = row.get("hash");
    if !constant_time_eq(hash_secret(secret).as_bytes(), stored_hash.as_bytes()) {
        return Ok(None);
    }
    Ok(Some(id.to_string()))
}

/// Principal from a token row alone — for credentials that stand in for
/// the secret, i.e. an SSH key registered against the token (possession
/// of the private key IS the credential; the SSH transport verified it).
/// Same instant-revocation property as [`verify`]: every call hits the
/// database, so a revoked token cuts off its keys on the next connection.
pub fn principal_for_token_id(db: &ControlDb, token_id: &str) -> Result<Option<Principal>, String> {
    let row = db
        .lock()
        .query_opt(
            "SELECT t.org_id, t.scopes, t.repo_id, t.revoked_at, t.expires_at, t.user_id, \
                    u.disabled_at \
             FROM tokens t LEFT JOIN users u ON u.id = t.user_id WHERE t.id = $1",
            &[&token_id],
        )
        .map_err(|e| e.to_string())?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.get::<_, Option<i64>>("revoked_at").is_some()
        || row.get::<_, Option<i64>>("disabled_at").is_some()
        || expired(row.get("expires_at"))
    {
        return Ok(None);
    }
    let org_id: String = row.get("org_id");
    let user_id: Option<String> = row.get("user_id");
    let frozen: Vec<Scope> = row
        .get::<_, String>("scopes")
        .split(',')
        .filter_map(Scope::parse)
        .collect();
    let scopes = match &user_id {
        Some(u) => match personal_scopes(db, &org_id, u, frozen.clone())? {
            Some(s) => s,
            None => return Ok(None),
        },
        None => frozen.clone(),
    };
    Ok(Some(Principal {
        token_id: token_id.to_string(),
        org_id,
        scopes,
        repo_id: row.get("repo_id"),
        ceiling: user_id.is_some().then_some(frozen),
        user_id,
    }))
}

/// A token as the dashboard shows it: everything except the secret,
/// which is unrecoverable by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenView {
    pub id: String,
    pub label: Option<String>,
    pub scopes: Vec<String>,
    pub repo_id: Option<String>,
    pub user_id: Option<String>,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
    /// When it stops working by itself. `None` is "never".
    pub expires_at: Option<i64>,
}

fn token_view(r: &postgres::Row) -> TokenView {
    TokenView {
        id: r.get("id"),
        label: r.get("label"),
        scopes: r
            .get::<_, String>("scopes")
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        repo_id: r.get("repo_id"),
        user_id: r.get("user_id"),
        created_at: r.get("created_at"),
        revoked_at: r.get("revoked_at"),
        expires_at: r.get("expires_at"),
    }
}

const TOKEN_COLS: &str = "id, label, scopes, repo_id, user_id, created_at, revoked_at, expires_at";

/// The label a token was minted with, if it has one. This is the name a
/// service token signs its commits with: `token:release-bot` is a fact a
/// reader can act on where `token:01hx…` was twenty-six characters that
/// only said "not a person".
pub fn label_of(db: &ControlDb, token_id: &str) -> Result<Option<String>, String> {
    db.lock()
        .query_opt("SELECT label FROM tokens WHERE id = $1", &[&token_id])
        .map(|r| r.and_then(|r| r.get::<_, Option<String>>("label")))
        .map_err(|e| format!("token label: {e}"))
}

/// Every token in the org — the administrator's view.
pub fn list(db: &ControlDb, org_id: &str) -> Result<Vec<TokenView>, String> {
    let rows = db
        .lock()
        .query(
            &format!("SELECT {TOKEN_COLS} FROM tokens WHERE org_id = $1 ORDER BY created_at, id"),
            &[&org_id],
        )
        .map_err(|e| format!("list tokens: {e}"))?;
    Ok(rows.iter().map(token_view).collect())
}

/// Only this person's own tokens — what a member may see. Service tokens
/// (`user_id IS NULL`) are nobody's, so they never appear here.
pub fn list_for_user(
    db: &ControlDb,
    org_id: &str,
    user_id: &str,
) -> Result<Vec<TokenView>, String> {
    if !crate::ids::valid_id(user_id) {
        return Ok(Vec::new());
    }
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {TOKEN_COLS} FROM tokens WHERE org_id = $1 AND user_id = $2 \
                 ORDER BY created_at, id"
            ),
            &[&org_id, &user_id],
        )
        .map_err(|e| format!("list tokens: {e}"))?;
    Ok(rows.iter().map(token_view).collect())
}

/// Revoke a token this person owns. A member may end their own
/// credentials without needing an administrator, and cannot touch anyone
/// else's — including the org's service tokens.
/// Revoke a token this person owns, recording it in the same
/// transaction. `Ok(false)` = nothing of theirs by that id was live.
pub fn revoke_own(
    db: &ControlDb,
    org_id: &str,
    token_id: &str,
    user_id: &str,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    if !crate::ids::valid_id(user_id) {
        return Ok(false);
    }
    revoke_where(
        db,
        "UPDATE tokens SET revoked_at = $4 \
         WHERE org_id = $1 AND id = $2 AND user_id = $3 AND revoked_at IS NULL",
        org_id,
        token_id,
        Some(user_id),
        audit,
    )
}

pub fn revoke(
    db: &ControlDb,
    org_id: &str,
    token_id: &str,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    revoke_where(
        db,
        "UPDATE tokens SET revoked_at = $3 \
         WHERE org_id = $1 AND id = $2 AND revoked_at IS NULL",
        org_id,
        token_id,
        None,
        audit,
    )
}

/// The revocation and its record commit together, for the same reason
/// minting does: "revoked, but nobody knows by whom" is the shape of an
/// attacker covering their tracks.
fn revoke_where(
    db: &ControlDb,
    sql: &'static str,
    org_id: &str,
    token_id: &str,
    user_id: Option<&str>,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    let now = now_ms();
    let blob = serde_json::json!({ "token_id": token_id });
    db.lock()
        .transaction(move |tx| {
            let n = match user_id {
                Some(u) => tx.execute(sql, &[&org_id, &token_id, &u, &now])?,
                None => tx.execute(sql, &[&org_id, &token_id, &now])?,
            };
            if n > 0 {
                if let Some(ctx) = audit {
                    crate::audit::record_tx(tx, ctx, None, "token.revoke", Some(&blob))?;
                }
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("revoke token: {e}"))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::registry;

    /// The whole of the new refusal, without a database.
    /// An expired token is refused on **both** doors, and is refused the
    /// same way a revoked one is.
    ///
    /// Two authentication paths read a token row — `verify` for a
    /// presented secret and `principal_for_token_id` for a credential
    /// that stands in for one, which is how an SSH key authenticates.
    /// A deadline honoured on one and not the other is not a shorter
    /// life, it is a *different* life depending on which door you knock
    /// on, and the one that would have been missed here is the door that
    /// does not carry the secret.
    #[test]
    fn an_expired_token_is_refused_on_every_door() {
        let (db, org) = setup();
        let live = mint_for(
            &db,
            &org,
            &[Scope::OrgRead],
            Mint {
                label: Some("runner"),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert!(verify(&db, &live.plaintext).unwrap().is_some());
        assert!(principal_for_token_id(&db, &live.id).unwrap().is_some());

        // Same token, a deadline already behind us.
        let dead = mint_for(
            &db,
            &org,
            &[Scope::OrgRead],
            Mint {
                label: Some("runner"),
                expires_at: Some(now_ms() - 1),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert!(
            verify(&db, &dead.plaintext).unwrap().is_none(),
            "an expired secret must not authenticate"
        );
        assert!(
            principal_for_token_id(&db, &dead.id).unwrap().is_none(),
            "nor may a credential that stands in for it — this is the SSH door"
        );

        // The row is still there. Expiry is enforced by the reader, not
        // by a sweep, so nothing has to have run for the refusal to be
        // correct — which is the property the runner is relying on.
        let listed = list(&db, &org).unwrap();
        let row = listed
            .iter()
            .find(|t| t.id == dead.id)
            .expect("still listed");
        assert!(row.expires_at.is_some(), "and the deadline is readable");
        assert!(row.revoked_at.is_none(), "it expired; nobody revoked it");
    }

    /// `identify` answers for a token that no longer authenticates, and
    /// only ever on proof of the secret.
    ///
    /// It exists so a cancelled job's runner can be told 410 instead of
    /// 401, and the thing that keeps that from being a disclosure is the
    /// hash comparison: token ids travel in job rows and audit entries,
    /// so answering on an id alone would let anyone who has seen one
    /// read a job's state.
    #[test]
    fn a_dead_token_is_still_identifiable_but_only_to_whoever_holds_it() {
        let (db, org) = setup();
        let t = mint_for(
            &db,
            &org,
            &[Scope::RepoRead],
            Mint {
                label: Some("job"),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            identify(&db, &t.plaintext).unwrap().as_deref(),
            Some(t.id.as_str())
        );

        // Revoked, and then expired: `verify` is done with it on both
        // counts, and `identify` still knows whose it was.
        assert!(revoke(&db, &org, &t.id, None).unwrap());
        assert!(verify(&db, &t.plaintext).unwrap().is_none());
        assert_eq!(
            identify(&db, &t.plaintext).unwrap().as_deref(),
            Some(t.id.as_str())
        );
        let expired_tok = mint_for(
            &db,
            &org,
            &[Scope::RepoRead],
            Mint {
                label: Some("job"),
                expires_at: Some(now_ms() - 1),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            identify(&db, &expired_tok.plaintext).unwrap().as_deref(),
            Some(expired_tok.id.as_str())
        );

        // The id alone proves nothing, and neither does a shape that is
        // not one of ours.
        let (id, _secret) = t
            .plaintext
            .strip_prefix("weft_")
            .unwrap()
            .split_once('_')
            .unwrap();
        assert_eq!(
            identify(&db, &format!("weft_{id}_wrongsecret")).unwrap(),
            None
        );
        assert_eq!(identify(&db, &format!("weft_{id}")).unwrap(), None);
        assert_eq!(identify(&db, "weft_nosuchid_secret").unwrap(), None);
        assert_eq!(identify(&db, "not-a-stratum-token").unwrap(), None);
    }

    #[test]
    fn expiry_is_a_deadline_and_absence_means_never() {
        assert!(!expired(None), "every token minted before the column");
        assert!(expired(Some(0)), "the epoch is long past");
        assert!(!expired(Some(now_ms() + 60_000)), "a minute of life left");
        // On the boundary the shorter life wins: a token whose deadline
        // is *now* is finished. The other rounding is a credential that
        // works one tick longer than it was sold as.
        assert!(expired(Some(now_ms())));
    }

    fn setup() -> (ControlDb, String) {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-auth")).unwrap();
        let org = registry::create_org(&db, "org").unwrap();
        (db, org.id)
    }

    #[test]
    fn scope_and_secret_edges() {
        let (db, org) = setup();
        // At least one scope is required.
        assert!(mint(&db, &org, &[], None, None).is_err());
        // A presented token with the right id but a wrong-length secret
        // fails the constant-time comparison, not an earlier length check.
        let t = mint(&db, &org, &[Scope::OrgRead], None, None).unwrap();
        let stem = t.plaintext.rsplit_once('_').unwrap().0;
        let short = format!("{stem}_tooshort");
        assert!(verify(&db, &short).unwrap().is_none());
        // Garbage without the weft_ shape resolves to None fast.
        assert!(verify(&db, "not-a-token").unwrap().is_none());
    }

    #[test]
    fn mint_verify_revoke() {
        let (db, org) = setup();
        let t = mint(&db, &org, &[Scope::RepoWrite], None, Some("ci")).unwrap();
        let p = verify(&db, &t.plaintext).unwrap().unwrap();
        assert_eq!(p.org_id, org);
        assert!(p.allows(Scope::RepoRead, Some("any")));
        assert!(p.allows(Scope::RepoWrite, Some("any")));
        assert!(!p.allows(Scope::OrgAdmin, None));

        assert!(revoke(&db, &org, &t.id, None).unwrap());
        assert!(
            verify(&db, &t.plaintext).unwrap().is_none(),
            "instant revocation"
        );
    }

    #[test]
    fn wrong_secret_and_garbage_fail_closed() {
        let (db, org) = setup();
        let t = mint(&db, &org, &[Scope::OrgAdmin], None, None).unwrap();
        let tampered = format!("{}x", t.plaintext);
        assert!(verify(&db, &tampered).unwrap().is_none());
        assert!(verify(&db, "weft_nonexistent_secret").unwrap().is_none());
        assert!(verify(&db, "Bearer nope").unwrap().is_none());
        assert!(verify(&db, "").unwrap().is_none());
    }

    #[test]
    fn repo_bound_tokens_cannot_cross_repos_or_run_org_ops() {
        let (db, org) = setup();
        let t = mint(&db, &org, &[Scope::RepoWrite], Some("repo-1"), None).unwrap();
        let p = verify(&db, &t.plaintext).unwrap().unwrap();
        assert!(p.allows(Scope::RepoWrite, Some("repo-1")));
        assert!(!p.allows(Scope::RepoRead, Some("repo-2")));
        assert!(!p.allows(Scope::OrgAdmin, None));
        assert!(!p.allows(Scope::RepoWrite, None));
    }

    /// A personal access token must die with the person. Without this,
    /// offboarding someone would end their sign-in while leaving every
    /// token they ever minted live.
    #[test]
    fn a_token_owned_by_a_disabled_user_stops_verifying() {
        let (db, org) = setup();
        let user = crate::users::create(&db, "t@example.com", "T", Some("a long enough password"))
            .unwrap();
        crate::members::add(&db, &org, &user.id, crate::members::Role::Member, None).unwrap();
        let minted = mint_for(
            &db,
            &org,
            &[Scope::OrgRead],
            Mint {
                label: Some("laptop"),
                user_id: Some(&user.id),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let p = verify(&db, &minted.plaintext).unwrap().unwrap();
        assert_eq!(p.user_id.as_deref(), Some(user.id.as_str()));
        // The audit trail names the person, not whichever credential they
        // happened to reach for.
        assert_eq!(p.audit_id(), format!("user:{}", user.id));
        assert!(principal_for_token_id(&db, &minted.id).unwrap().is_some());

        crate::users::set_disabled(&db, &user.id, true).unwrap();
        assert!(verify(&db, &minted.plaintext).unwrap().is_none());
        assert!(principal_for_token_id(&db, &minted.id).unwrap().is_none());

        crate::users::set_disabled(&db, &user.id, false).unwrap();
        assert!(verify(&db, &minted.plaintext).unwrap().is_some());
    }

    /// A personal token carries the authority its owner has *now*. This
    /// is what makes offboarding total: removing someone from an org ends
    /// every credential they hold there, not only their browser session,
    /// with nothing to hunt down and revoke by hand.
    #[test]
    fn a_personal_token_follows_its_owner_role_and_dies_with_the_membership() {
        let (db, org) = setup();
        let user = crate::users::create(&db, "p@example.com", "P", Some("a long enough password"))
            .unwrap();
        crate::members::add(&db, &org, &user.id, crate::members::Role::Member, None).unwrap();
        let minted = mint_for(
            &db,
            &org,
            &[Scope::RepoWrite, Scope::OrgRead],
            Mint {
                label: Some("laptop"),
                user_id: Some(&user.id),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let p = verify(&db, &minted.plaintext).unwrap().unwrap();
        assert!(p.allows(Scope::RepoWrite, Some("repo")));

        // Demoted to viewer: the same token now reads and no longer
        // writes, without being reissued.
        crate::members::set_role(&db, &org, &user.id, crate::members::Role::Viewer, None).unwrap();
        let p = verify(&db, &minted.plaintext).unwrap().unwrap();
        assert!(p.allows(Scope::RepoRead, Some("repo")));
        assert!(!p.allows(Scope::RepoWrite, Some("repo")));

        // Promotion cannot widen it past what it was minted with: the
        // frozen scope list is a ceiling, the role is the floor.
        crate::members::set_role(&db, &org, &user.id, crate::members::Role::Admin, None).unwrap();
        let p = verify(&db, &minted.plaintext).unwrap().unwrap();
        assert!(!p.allows(Scope::OrgAdmin, None), "{:?}", p.scopes);

        // Removed from the org: dead, by both resolution paths.
        crate::members::remove(&db, &org, &user.id, None).unwrap();
        assert!(verify(&db, &minted.plaintext).unwrap().is_none());
        assert!(principal_for_token_id(&db, &minted.id).unwrap().is_none());
    }

    /// A per-repo grant *replaces* the org role on that repo, so it has
    /// to work in both directions for a personal token too — and the
    /// token's mint scopes stay the ceiling either way.
    #[test]
    fn a_repo_grant_moves_a_personal_token_in_both_directions() {
        let (db, org) = setup();
        let repo = registry::create_repo(
            &db,
            &org,
            &registry::NewRepo {
                description: None,
                name: "app",
                kind: registry::RepoKind::Native,
                public: false,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        let user = crate::users::create(&db, "g@example.com", "G", Some("a long enough password"))
            .unwrap();
        crate::members::add(&db, &org, &user.id, crate::members::Role::Viewer, None).unwrap();
        let minted = mint_for(
            &db,
            &org,
            &[Scope::RepoWrite],
            Mint {
                label: Some("laptop"),
                user_id: Some(&user.id),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // A viewer's token reads and does not write, anywhere.
        let p = verify(&db, &minted.plaintext).unwrap().unwrap();
        let here = crate::members::refine_for_repo(&db, &p, &repo.id)
            .unwrap()
            .unwrap();
        assert!(here.allows(Scope::RepoRead, Some(&repo.id)));
        assert!(!here.allows(Scope::RepoWrite, Some(&repo.id)));

        // Granted member on this repo, the same token now writes here —
        // nothing reissued. Narrowing rather than replacing would make a
        // grant a one-way ratchet and this would still refuse.
        crate::members::grant_repo(&db, &repo.id, &user.id, crate::members::Role::Member, None)
            .unwrap();
        let p = verify(&db, &minted.plaintext).unwrap().unwrap();
        let here = crate::members::refine_for_repo(&db, &p, &repo.id)
            .unwrap()
            .unwrap();
        assert!(here.allows(Scope::RepoWrite, Some(&repo.id)));

        // …but never past what the token was minted for: an admin grant
        // does not turn a repo:write token into an org:admin one.
        crate::members::grant_repo(&db, &repo.id, &user.id, crate::members::Role::Admin, None)
            .unwrap();
        let p = verify(&db, &minted.plaintext).unwrap().unwrap();
        let here = crate::members::refine_for_repo(&db, &p, &repo.id)
            .unwrap()
            .unwrap();
        assert!(here.allows(Scope::RepoWrite, Some(&repo.id)));
        assert!(!here.allows(Scope::OrgAdmin, None), "{:?}", here.scopes);

        // A service token has no person, so no grant applies to it.
        let svc = mint(&db, &org, &[Scope::RepoRead], None, Some("ci")).unwrap();
        let p = verify(&db, &svc.plaintext).unwrap().unwrap();
        let same = crate::members::refine_for_repo(&db, &p, &repo.id)
            .unwrap()
            .unwrap();
        assert_eq!(same.scopes, p.scopes);
    }

    /// A person's own credentials, keyed by an id that cannot exist,
    /// name nothing — the same contract every other user-id lookup keeps,
    /// so hostile bytes never reach a query.
    #[test]
    fn credential_lookups_are_inert_for_malformed_user_ids() {
        let (db, org) = setup();
        for bad in [
            "",
            "ghost",
            "not an id",
            "\0",
            "01hx'; DROP TABLE tokens;--",
        ] {
            assert!(list_for_user(&db, &org, bad).unwrap().is_empty());
            assert!(!revoke_own(&db, &org, "01hxaaaaaaaaaaaaaaaaaaaaaa", bad, None).unwrap());
        }
    }

    /// Intersection is about capabilities, not names. The case that
    /// matters: a `repo:write` token held by a viewer must still read.
    /// Matching scope names would leave it granting nothing.
    #[test]
    fn intersecting_two_authorities_keeps_what_both_allow() {
        use Scope::*;
        assert_eq!(
            intersect(&[RepoWrite], &[OrgRead, RepoRead]),
            vec![RepoRead]
        );
        assert_eq!(
            intersect(&[RepoWrite, OrgRead], &[OrgRead, RepoRead]),
            vec![OrgRead, RepoRead]
        );
        // An admin role cannot widen a token past what it was minted with.
        assert_eq!(intersect(&[RepoRead], &[OrgAdmin]), vec![RepoRead]);
        // …and an admin token cannot widen a viewer past their role.
        assert_eq!(
            intersect(&[OrgAdmin], &[OrgRead, RepoRead]),
            vec![OrgRead, RepoRead]
        );
        assert_eq!(
            intersect(&[RepoWrite], &[OrgAdmin]),
            vec![RepoRead, RepoWrite]
        );
        // Two admins stay admin; a disjoint pair grants nothing.
        assert_eq!(intersect(&[OrgAdmin], &[OrgAdmin]), ALL_SCOPES.to_vec());
        assert!(intersect(&[RepoRead], &[]).is_empty());
        assert!(intersect(&[], &[OrgAdmin]).is_empty());
    }

    /// Tokens minted before users existed have no owner and must keep
    /// working exactly as they did — that is what CI and git depend on.
    #[test]
    fn an_ownerless_service_token_is_unaffected_by_users() {
        let (db, org) = setup();
        let minted = mint(&db, &org, &[Scope::OrgAdmin], None, Some("ci")).unwrap();
        let p = verify(&db, &minted.plaintext).unwrap().unwrap();
        assert_eq!(p.user_id, None);
        assert_eq!(p.audit_id(), format!("token:{}", minted.id));
        assert!(p.allows(Scope::OrgAdmin, None));
    }

    #[test]
    fn a_user_principal_carries_its_role_scopes() {
        let p = Principal::for_user("org1", "user1", vec![Scope::OrgRead, Scope::RepoRead]);
        assert_eq!(p.audit_id(), "user:user1");
        assert!(p.token_id.is_empty(), "a session has no token");
        assert!(p.allows(Scope::RepoRead, Some("repo1")));
        assert!(!p.allows(Scope::RepoWrite, Some("repo1")));
        assert!(!p.allows(Scope::OrgAdmin, None));
    }
}
