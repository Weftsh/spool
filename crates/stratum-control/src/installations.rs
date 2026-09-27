//! An org connects GitHub once, and the installation id is never typed.
//!
//! Mirroring a private repository needs a GitHub App installation, and
//! until now the way to name one was to paste its id — an opaque number
//! a person digs out of a settings URL — into the create-mirror body,
//! once per repository. Nothing stored it, so they dug it out again
//! every time.
//!
//! Two tables, and the second exists entirely because of the first's
//! threat model.
//!
//! * [`bind`] records that an org may use an installation. The
//!   `(provider, installation_id)` index is unique across every org: an
//!   installation names one GitHub account, so a second org claiming it
//!   would be one org reading another's code. That is a database error
//!   here rather than a rule somebody has to remember.
//!
//! * [`start`] / [`spend`] are the anti-CSRF for the round trip through
//!   GitHub. GitHub hands the browser back with an `installation_id` and
//!   whatever `state` we sent, and *nothing else* says who began the
//!   flow. Without a state nobody could forge, a link could bind an
//!   attacker's installation to your org — giving them a mirror of
//!   whatever they install it on, inside your namespace — or bind yours
//!   to theirs. The secret is random, stored only as a hash, single-use
//!   and short-lived, exactly like every other credential in this
//!   schema.

use crate::db::ControlDb;
use crate::ids::{now_ms, token_secret, ulid};
use sha2::{Digest, Sha256};

/// Ten minutes. Long enough to read GitHub's install screen and pick
/// repositories; short enough that a state left in a browser's history
/// is not a live credential tomorrow.
pub const STATE_TTL_SECS: i64 = 600;

const PREFIX: &str = "stinst_";

/// One org's use of one installation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Installation {
    pub installation_id: String,
    pub provider: String,
    /// The GitHub account it is installed on, when the callback learned
    /// it. Shown to a person choosing between two installations, who
    /// cannot tell `48213904` from `48213905`.
    pub account: Option<String>,
    pub created_at: i64,
}

fn hash(secret: &str) -> String {
    stratum_store::pack::hex(&Sha256::digest(secret.as_bytes()))
}

/// Begin an install flow, returning the opaque `state` to hand GitHub.
pub fn start(
    db: &ControlDb,
    org_id: &str,
    provider: &str,
    user_id: Option<&str>,
) -> Result<String, String> {
    let id = ulid();
    let secret = token_secret();
    let now = now_ms();
    db.lock()
        .execute(
            "INSERT INTO install_states \
             (id, org_id, provider, secret_hash, created_by, created_at, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            &[
                &id,
                &org_id.to_string(),
                &provider.to_string(),
                &hash(&secret),
                &user_id.map(str::to_string),
                &now,
                &(now + STATE_TTL_SECS * 1000),
            ],
        )
        .map_err(|e| format!("start install: {e}"))?;
    Ok(format!("{PREFIX}{id}_{secret}"))
}

/// Spend a state, returning the org that began the flow.
///
/// One statement, for the same reason [`crate::usertokens::redeem`] is:
/// checking first and stamping after leaves a window where two requests
/// both see a live state, and here that means one state binding two
/// installations.
///
/// Every way a state can be wrong — malformed, unknown, spent, expired,
/// wrong secret — is the same `None`. A callback is a URL a stranger can
/// hit, and telling them *which* of those it was is telling them
/// something about a flow that is not theirs.
pub fn spend(db: &ControlDb, presented: &str, provider: &str) -> Result<Option<String>, String> {
    let Some(rest) = presented.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    let Some((id, secret)) = rest.split_once('_') else {
        return Ok(None);
    };
    let now = now_ms();
    let row = db
        .lock()
        .query_opt(
            "UPDATE install_states SET spent_at = $4 \
             WHERE id = $1 AND provider = $2 AND secret_hash = $3 \
               AND spent_at IS NULL AND expires_at > $4 \
             RETURNING org_id",
            &[&id, &provider.to_string(), &hash(secret), &now],
        )
        .map_err(|e| format!("spend install state: {e}"))?;
    Ok(row.map(|r| r.get("org_id")))
}

/// Which App a live state was minted for, without spending it.
///
/// GitHub keeps an App's callback *path* and drops its query string, so
/// the Runners App's `?app=runners` never reaches the callback when the
/// install requested user authorization (seen on production,
/// 2026-09-15: the code was exchanged with the mirror App's client and
/// refused as `bad_verification_code`). The state was minted by a
/// request that named its App, so it is the one thing on the redirect
/// that still says which App this is.
pub fn provider_of(db: &ControlDb, presented: &str) -> Result<Option<String>, String> {
    let Some(rest) = presented.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    let Some((id, secret)) = rest.split_once('_') else {
        return Ok(None);
    };
    let row = db
        .lock()
        .query_opt(
            "SELECT provider FROM install_states \
             WHERE id = $1 AND secret_hash = $2 AND spent_at IS NULL AND expires_at > $3",
            &[&id, &hash(secret), &now_ms()],
        )
        .map_err(|e| format!("read install state: {e}"))?;
    Ok(row.map(|r| r.get("provider")))
}

/// The flows this person has started and not finished, one per org,
/// newest first — live states only.
///
/// For the callback that arrives *without* a state: GitHub's redirect
/// after a person edits an installation carries no `state`, and an
/// install begun on GitHub's side never had one. What it still has is
/// the person's own session, and if that person began exactly one
/// connect recently, that is the org they meant.
pub fn pending_for(
    db: &ControlDb,
    user_id: &str,
    provider: &str,
) -> Result<Vec<(String, String)>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT DISTINCT ON (org_id) id, org_id FROM install_states \
             WHERE created_by = $1 AND provider = $2 \
               AND spent_at IS NULL AND expires_at > $3 \
             ORDER BY org_id, created_at DESC",
            &[&user_id.to_string(), &provider.to_string(), &now_ms()],
        )
        .map_err(|e| format!("pending installs: {e}"))?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("id"), r.get("org_id")))
        .collect())
}

/// Spend a state by its id rather than its secret — for a flow finished
/// by the person who started it, proved by their session rather than
/// by the state GitHub did not carry back. Same single statement, same
/// `None` for anything already spent or expired.
pub fn spend_id(db: &ControlDb, state_id: &str, provider: &str) -> Result<Option<String>, String> {
    let now = now_ms();
    let row = db
        .lock()
        .query_opt(
            "UPDATE install_states SET spent_at = $3 \
             WHERE id = $1 AND provider = $2 AND spent_at IS NULL AND expires_at > $3 \
             RETURNING org_id",
            &[&state_id.to_string(), &provider.to_string(), &now],
        )
        .map_err(|e| format!("spend install state: {e}"))?;
    Ok(row.map(|r| r.get("org_id")))
}

/// Thirty minutes: long enough to create an account, confirm an
/// address and pick an organization; short enough that a claim left in
/// a browser is not a live credential tomorrow.
pub const CLAIM_TTL_SECS: i64 = 1800;

const CLAIM_PREFIX: &str = "stclaim_";

/// An installation parked by the callback, waiting for an org.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Pending {
    pub installation_id: String,
    pub account: Option<String>,
    pub expires_at: i64,
    /// Which App parked it — the mirror App's provider name or the
    /// Runners App's — so the claim binds under the right one.
    pub provider: String,
}

/// Park an installation the callback has proved is the person's but
/// has no org for yet. Returns the claim secret to hand the browser.
pub fn park(
    db: &ControlDb,
    provider: &str,
    installation_id: &str,
    account: Option<&str>,
) -> Result<String, String> {
    let id = ulid();
    let secret = token_secret();
    let now = now_ms();
    db.lock()
        .execute(
            "INSERT INTO install_claims \
             (id, provider, installation_id, account, secret_hash, created_at, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            &[
                &id,
                &provider.to_string(),
                &installation_id.to_string(),
                &account.map(str::to_string),
                &hash(&secret),
                &now,
                &(now + CLAIM_TTL_SECS * 1000),
            ],
        )
        .map_err(|e| format!("park installation: {e}"))?;
    Ok(format!("{CLAIM_PREFIX}{id}_{secret}"))
}

fn split_claim(presented: &str) -> Option<(&str, &str)> {
    presented.strip_prefix(CLAIM_PREFIX)?.split_once('_')
}

/// What a claim secret is parked for, if it is live. Read-only: the
/// dashboard asks this to say "connect <account> to which org?".
pub fn peek(db: &ControlDb, presented: &str) -> Result<Option<Pending>, String> {
    let Some((id, secret)) = split_claim(presented) else {
        return Ok(None);
    };
    let row = db
        .lock()
        .query_opt(
            "SELECT installation_id, account, expires_at, provider FROM install_claims \
             WHERE id = $1 AND secret_hash = $2 \
               AND claimed_at IS NULL AND expires_at > $3",
            &[&id, &hash(secret), &now_ms()],
        )
        .map_err(|e| format!("peek install claim: {e}"))?;
    Ok(row.map(|r| Pending {
        installation_id: r.get("installation_id"),
        account: r.get("account"),
        expires_at: r.get("expires_at"),
        provider: r.get("provider"),
    }))
}

/// Spend a claim, returning what it was parked for. One statement, for
/// the reason [`spend`] is: two requests must not both see it live.
/// Every way it can be wrong is the same `None`.
pub fn claim(db: &ControlDb, presented: &str) -> Result<Option<Pending>, String> {
    let Some((id, secret)) = split_claim(presented) else {
        return Ok(None);
    };
    let now = now_ms();
    let row = db
        .lock()
        .query_opt(
            "UPDATE install_claims SET claimed_at = $3 \
             WHERE id = $1 AND secret_hash = $2 \
               AND claimed_at IS NULL AND expires_at > $3 \
             RETURNING installation_id, account, expires_at, provider",
            &[&id, &hash(secret), &now],
        )
        .map_err(|e| format!("claim installation: {e}"))?;
    Ok(row.map(|r| Pending {
        installation_id: r.get("installation_id"),
        account: r.get("account"),
        expires_at: r.get("expires_at"),
        provider: r.get("provider"),
    }))
}

/// Record that this org may use this installation.
///
/// Re-installing on the same account produces the same id, so binding
/// twice is somebody clicking through the flow again, not an error —
/// the account is refreshed and the row kept. Binding an installation
/// another org already holds is refused by the unique index, and the
/// caller gets that as an error rather than a silent no-op.
pub fn bind(
    db: &ControlDb,
    org_id: &str,
    provider: &str,
    installation_id: &str,
    account: Option<&str>,
    user_id: Option<&str>,
) -> Result<(), String> {
    db.lock()
        .execute(
            "INSERT INTO org_installations \
             (org_id, provider, installation_id, account, created_by, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (org_id, provider, installation_id) DO UPDATE \
               SET account = COALESCE(EXCLUDED.account, org_installations.account)",
            &[
                &org_id.to_string(),
                &provider.to_string(),
                &installation_id.to_string(),
                &account.map(str::to_string),
                &user_id.map(str::to_string),
                &now_ms(),
            ],
        )
        .map(|_| ())
        .map_err(|e| format!("bind installation: {e}"))
}

/// Every installation this org may use, oldest first.
pub fn list(db: &ControlDb, org_id: &str, provider: &str) -> Result<Vec<Installation>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT installation_id, provider, account, created_at \
             FROM org_installations WHERE org_id = $1 AND provider = $2 \
             ORDER BY created_at, installation_id",
            &[&org_id.to_string(), &provider.to_string()],
        )
        .map_err(|e| format!("list installations: {e}"))?;
    Ok(rows
        .into_iter()
        .map(|r| Installation {
            installation_id: r.get("installation_id"),
            provider: r.get("provider"),
            account: r.get("account"),
            created_at: r.get("created_at"),
        })
        .collect())
}

/// Whether this could be a GitHub installation id at all.
///
/// Shared by every route that takes one from a caller, so the create
/// route and the picker cannot drift on what they let through.
///
/// They are integers, always. Checking that before the id reaches a
/// query is not decoration: an id containing a NUL byte is text
/// PostgreSQL refuses outright, and the first version of this answered
/// a 500 with `db error` in the body — a client's malformed input
/// reported as the server breaking, with a fragment of the database
/// layer attached. The negative suite found it with the shared
/// injection corpus.
///
/// The bound is generous — GitHub's ids are nine digits today — and
/// exists so an "id" of a megabyte of digits is refused before it is
/// stored or sent anywhere.
pub fn plausible_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_digit())
}

/// Whether this org may use this installation.
///
/// The authorization seam for every route that takes an installation id
/// from a caller. Without it, an id is a bearer token to somebody else's
/// source: the App will happily mint a token for any installation it is
/// asked about, so "which org is allowed to ask" has to be decided here.
pub fn org_may_use(
    db: &ControlDb,
    org_id: &str,
    provider: &str,
    installation_id: &str,
) -> Result<bool, String> {
    db.lock()
        .query_opt(
            "SELECT 1 FROM org_installations \
             WHERE org_id = $1 AND provider = $2 AND installation_id = $3",
            &[
                &org_id.to_string(),
                &provider.to_string(),
                &installation_id.to_string(),
            ],
        )
        .map(|r| r.is_some())
        .map_err(|e| format!("check installation: {e}"))
}

/// The organisation an installation is bound to, if any.
///
/// The tenant seam for anything GitHub *sends* us rather than anything a
/// caller asks for: a webhook names an installation, and the unique
/// index on `(provider, installation_id)` is what makes the answer at
/// most one organisation. It is deliberately the only thing consulted —
/// the payload's `repository.owner` is GitHub's word about GitHub, not
/// about who here may spend minutes on it.
pub fn org_for(
    db: &ControlDb,
    provider: &str,
    installation_id: &str,
) -> Result<Option<String>, String> {
    db.lock()
        .query_opt(
            "SELECT org_id FROM org_installations WHERE provider = $1 AND installation_id = $2",
            &[&provider.to_string(), &installation_id.to_string()],
        )
        .map(|r| r.map(|r| r.get("org_id")))
        .map_err(|e| format!("installation's org: {e}"))
}

/// Forget an installation. The App stays installed on GitHub — only
/// GitHub can uninstall it — so this is "stop offering it here", and the
/// repos already mirrored through it keep their own `origin_installation`
/// until somebody removes them.
pub fn unbind(
    db: &ControlDb,
    org_id: &str,
    provider: &str,
    installation_id: &str,
) -> Result<bool, String> {
    db.lock()
        .execute(
            "DELETE FROM org_installations \
             WHERE org_id = $1 AND provider = $2 AND installation_id = $3",
            &[
                &org_id.to_string(),
                &provider.to_string(),
                &installation_id.to_string(),
            ],
        )
        .map(|n| n > 0)
        .map_err(|e| format!("unbind installation: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry;

    /// The webhook's tenant seam: an installation names at most one
    /// organisation, and only under the provider it was bound for.
    #[test]
    fn an_installation_names_its_org_and_nobody_elses() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("inst-org-for")).unwrap();
        let acme = registry::create_org(&db, "acme").unwrap();
        bind(&db, &acme.id, "github", "4001", Some("acme-inc"), None).unwrap();
        assert_eq!(
            org_for(&db, "github", "4001").unwrap().as_deref(),
            Some(acme.id.as_str())
        );
        assert_eq!(org_for(&db, "github", "4002").unwrap(), None);
        assert_eq!(org_for(&db, "gitlab", "4001").unwrap(), None);
        // A second organisation cannot take it: the unique index is what
        // makes the answer above trustworthy.
        let globex = registry::create_org(&db, "globex").unwrap();
        assert!(bind(&db, &globex.id, "github", "4001", None, None).is_err());
        assert_eq!(
            org_for(&db, "github", "4001").unwrap().as_deref(),
            Some(acme.id.as_str())
        );
    }
}
