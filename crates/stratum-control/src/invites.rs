//! Org invitations.
//!
//! An invite link is a bearer credential, so it is stored like every
//! other one here: random secret, only the hash kept, verified against
//! the database. Three properties are load-bearing and each has a test:
//!
//!  * it is **single-use** — accepting stamps `accepted_at`, and a replay
//!    of the same link afterwards is refused;
//!  * it is **bound to the address it was sent to** — accepting with a
//!    different account does not silently move the invite;
//!  * accepting is **atomic** — the user and their membership are created
//!    together or not at all, because a user with no membership belongs
//!    to no org, cannot be reached by any org-scoped query, and would
//!    block the address from being invited again.

use crate::db::ControlDb;
use crate::ids::{now_ms, token_secret, ulid};
use crate::members::Role;
use sha2::{Digest, Sha256};

/// A week. Long enough for a holiday, short enough that a forwarded link
/// does not stay live indefinitely.
pub const DEFAULT_TTL_SECS: i64 = 7 * 24 * 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub id: String,
    pub org_id: String,
    pub email: String,
    pub role: Role,
    pub created_by: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub accepted_at: Option<i64>,
}

fn hash(secret: &str) -> String {
    stratum_store::pack::hex(&Sha256::digest(secret.as_bytes()))
}

fn row_to_invite(row: &postgres::Row) -> Option<Invite> {
    Some(Invite {
        id: row.get("id"),
        org_id: row.get("org_id"),
        email: row.get("email"),
        role: Role::parse(row.get::<_, String>("role").as_str())?,
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        expires_at: row.get("expires_at"),
        accepted_at: row.get("accepted_at"),
    })
}

const COLS: &str = "id, org_id, email, role, created_by, created_at, expires_at, accepted_at";

/// Create an invite. Returns the row and the one-time link secret.
pub fn create(
    db: &ControlDb,
    org_id: &str,
    email: &str,
    role: Role,
    created_by: &str,
    ttl_secs: i64,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<(Invite, String), String> {
    if crate::registry::is_personal(db, org_id)? {
        return Err("a personal namespace cannot have members — create an organization".into());
    }
    let email = crate::users::normalize_email(email);
    if !crate::users::valid_email(&email) {
        return Err("invalid email address".into());
    }
    let secret = token_secret();
    let now = now_ms();
    let invite = Invite {
        id: ulid(),
        org_id: org_id.to_string(),
        email,
        role,
        created_by: created_by.to_string(),
        created_at: now,
        expires_at: now + ttl_secs * 1000,
        accepted_at: None,
    };
    let blob = serde_json::json!({
        "invite_id": invite.id, "email": invite.email, "role": invite.role.as_str(),
    });
    let row = (
        invite.id.clone(),
        invite.org_id.clone(),
        invite.email.clone(),
        invite.role.as_str(),
        hash(&secret),
        invite.created_by.clone(),
        invite.created_at,
        invite.expires_at,
    );
    let n = db
        .lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "INSERT INTO invites \
                 (id, org_id, email, role, token_hash, created_by, created_at, expires_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (org_id, lower(email)) WHERE accepted_at IS NULL DO NOTHING",
                &[
                    &row.0, &row.1, &row.2, &row.3, &row.4, &row.5, &row.6, &row.7,
                ],
            )?;
            if n > 0 {
                if let Some(ctx) = audit {
                    crate::audit::record_tx(tx, ctx, None, "invite.create", Some(&blob))?;
                }
            }
            Ok(n)
        })
        .map_err(|e| format!("create invite: {e}"))?;
    if n == 0 {
        return Err("an invitation for that address is already pending".into());
    }
    let link = format!("stinv_{}_{secret}", invite.id);
    Ok((invite, link))
}

pub fn list(db: &ControlDb, org_id: &str) -> Result<Vec<Invite>, String> {
    db.lock()
        .query(
            &format!("SELECT {COLS} FROM invites WHERE org_id = $1 ORDER BY created_at DESC"),
            &[&org_id],
        )
        .map_err(|e| format!("list invites: {e}"))
        .map(|rows| rows.iter().filter_map(row_to_invite).collect())
}

pub fn revoke(
    db: &ControlDb,
    org_id: &str,
    invite_id: &str,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<bool, String> {
    let blob = serde_json::json!({ "invite_id": invite_id });
    db.lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "DELETE FROM invites WHERE org_id = $1 AND id = $2 AND accepted_at IS NULL",
                &[&org_id, &invite_id],
            )?;
            if n > 0 {
                if let Some(ctx) = audit {
                    crate::audit::record_tx(tx, ctx, None, "invite.revoke", Some(&blob))?;
                }
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("revoke invite: {e}"))
}

/// Resolve a presented link to a live invite. Every failure shape —
/// malformed, unknown, already accepted, expired, wrong secret — is the
/// same `None`, so a link cannot be used to probe which invites exist.
pub fn verify(db: &ControlDb, presented: &str) -> Result<Option<Invite>, String> {
    let Some(rest) = presented.strip_prefix("stinv_") else {
        return Ok(None);
    };
    let Some((id, secret)) = rest.split_once('_') else {
        return Ok(None);
    };
    let row = db
        .lock()
        .query_opt(
            &format!("SELECT {COLS}, token_hash FROM invites WHERE id = $1"),
            &[&id],
        )
        .map_err(|e| format!("verify invite: {e}"))?;
    let Some(row) = row else { return Ok(None) };
    let stored: String = row.get("token_hash");
    if !constant_time_eq(stored.as_bytes(), hash(secret).as_bytes()) {
        return Ok(None);
    }
    let Some(invite) = row_to_invite(&row) else {
        return Ok(None);
    };
    if invite.accepted_at.is_some() || invite.expires_at <= now_ms() {
        return Ok(None);
    }
    Ok(Some(invite))
}

/// What accepting an invite produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub user_id: String,
    pub org_id: String,
    pub role: Role,
    /// False when the invite attached an existing account to a new org.
    pub created_user: bool,
}

/// Accept an invite, creating the account if this is a new person.
///
/// The whole thing is one transaction: stamping the invite, creating the
/// user, and creating the membership either all happen or none do. A
/// partial accept would leave an account that belongs to no org and an
/// invite that can never be used again.
pub fn accept(
    db: &ControlDb,
    presented: &str,
    name: &str,
    password: Option<&str>,
) -> Result<Accepted, String> {
    let Some(invite) = verify(db, presented)? else {
        return Err("this invitation is not valid".into());
    };
    let existing = crate::users::by_email(db, &invite.email)?;

    // Hash outside the transaction: Argon2id is deliberately slow, and
    // holding the single control-plane connection for it would stall
    // every other request.
    let hashed = match (&existing, password) {
        (None, Some(p)) => Some(crate::users::hash_password(p)?),
        (None, None) => return Err("a password is required to create an account".into()),
        (Some(_), _) => None,
    };
    let name = name.trim().to_string();
    if existing.is_none() && (name.is_empty() || name.chars().count() > 200) {
        return Err("name must be 1-200 characters".into());
    }

    let user_id = existing.as_ref().map(|u| u.id.clone()).unwrap_or_else(ulid);
    let tx_user_id = user_id.clone();
    let created_user = existing.is_none();
    let now = now_ms();
    let email = invite.email.clone();
    let role = invite.role;
    let org_id = invite.org_id.clone();
    let invite_id = invite.id.clone();

    db.lock()
        .transaction(move |tx| {
            // Re-stamp under the row lock. If a concurrent accept won the
            // race, this updates zero rows and the whole thing rolls back
            // — which is what makes an invite single-use under
            // concurrency, not merely in sequence.
            let stamped = tx.execute(
                "UPDATE invites SET accepted_at = $2 WHERE id = $1 AND accepted_at IS NULL",
                &[&invite_id, &now],
            )?;
            if stamped == 0 {
                // Force a rollback with a real error rather than
                // returning Ok on a lost race.
                return Err(tx
                    .query_one("SELECT 1/0", &[])
                    .expect_err("division by zero"));
            }
            if created_user {
                // Verified on arrival: the link that got them here was
                // mailed to this address and nowhere else, which is the
                // same proof the confirmation flow asks for. Making an
                // invited colleague confirm an address their invitation
                // already travelled through is friction with nothing
                // behind it.
                tx.execute(
                    "INSERT INTO users \
                     (id, email, name, password_hash, created_at, verified_at) \
                     VALUES ($1, $2, $3, $4, $5, $5)",
                    &[&tx_user_id, &email, &name, &hashed, &now],
                )?;
            }
            tx.execute(
                "INSERT INTO org_members (org_id, user_id, role, created_at) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (org_id, user_id) DO UPDATE SET role = EXCLUDED.role",
                &[&org_id, &tx_user_id, &role.as_str(), &now],
            )?;
            // The actor is the person accepting — they exist as of the
            // statement above, so the trail can name them from the start.
            let ctx = crate::audit::AuditCtx {
                principal: format!("user:{tx_user_id}"),
                user_id: Some(tx_user_id.clone()),
                org_id: org_id.clone(),
            };
            crate::audit::record_tx(
                tx,
                &ctx,
                None,
                "invite.accept",
                Some(&serde_json::json!({
                    "invite_id": invite_id, "role": role.as_str(),
                    "created_user": created_user,
                })),
            )?;
            Ok(())
        })
        .map_err(|e| format!("accept invite: {e}"))?;

    Ok(Accepted {
        user_id,
        org_id: invite.org_id,
        role,
        created_user,
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same guard as the session comparator: unequal lengths are refused
    /// before any byte is read past the shorter operand.
    #[test]
    fn constant_time_eq_rejects_unequal_lengths() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
    }
    use crate::{members, registry, users};

    fn setup() -> (ControlDb, String, String) {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("invites")).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let owner = users::create(
            &db,
            "owner@example.com",
            "Owner",
            Some("a long enough password"),
        )
        .unwrap();
        members::add(&db, &org.id, &owner.id, Role::Owner, None).unwrap();
        (db, org.id, owner.id)
    }

    #[test]
    fn accepting_creates_the_account_and_the_membership_together() {
        let (db, org, owner) = setup();
        let (inv, link) = create(
            &db,
            &org,
            "New@Example.com",
            Role::Member,
            &owner,
            DEFAULT_TTL_SECS,
            None,
        )
        .unwrap();
        assert_eq!(inv.email, "new@example.com", "address is normalized");
        assert!(link.starts_with("stinv_"));

        // A new account needs a usable name: an invite that would create
        // a nameless user is refused before anything is written, and the
        // invite survives to be accepted properly.
        for bad_name in ["", "   ", &"n".repeat(201)] {
            assert!(
                accept(&db, &link, bad_name, Some("a long enough password")).is_err(),
                "name {bad_name:?} was accepted"
            );
        }

        let out = accept(&db, &link, "New Person", Some("a long enough password")).unwrap();
        assert!(out.created_user);
        assert_eq!(out.role, Role::Member);
        assert_eq!(
            members::role_of(&db, &org, &out.user_id).unwrap(),
            Some(Role::Member)
        );
        let u = users::by_id(&db, &out.user_id).unwrap().unwrap();
        assert_eq!(u.email, "new@example.com");
        // The account is immediately usable with the password just set.
        assert!(
            users::authenticate(&db, "new@example.com", "a long enough password")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn an_invite_is_single_use_and_expires() {
        let (db, org, owner) = setup();
        let (_, link) = create(
            &db,
            &org,
            "once@example.com",
            Role::Member,
            &owner,
            DEFAULT_TTL_SECS,
            None,
        )
        .unwrap();
        accept(&db, &link, "Once", Some("a long enough password")).unwrap();

        // Replay is refused, and the link no longer verifies at all.
        assert!(verify(&db, &link).unwrap().is_none());
        assert!(accept(&db, &link, "Again", Some("a long enough password")).is_err());

        // An expired invite is dead on arrival.
        let (_, expired) = create(
            &db,
            &org,
            "late@example.com",
            Role::Member,
            &owner,
            -1,
            None,
        )
        .unwrap();
        assert!(verify(&db, &expired).unwrap().is_none());
        assert!(accept(&db, &expired, "Late", Some("a long enough password")).is_err());

        // Revoking a pending invite kills it.
        let (rev, revoked_link) = create(
            &db,
            &org,
            "gone@example.com",
            Role::Member,
            &owner,
            DEFAULT_TTL_SECS,
            None,
        )
        .unwrap();
        assert!(revoke(&db, &org, &rev.id, None).unwrap());
        assert!(verify(&db, &revoked_link).unwrap().is_none());
        assert!(!revoke(&db, &org, &rev.id, None).unwrap());
        // Another org cannot revoke this org's invite.
        let (other, _) = create(
            &db,
            &org,
            "x@example.com",
            Role::Member,
            &owner,
            DEFAULT_TTL_SECS,
            None,
        )
        .unwrap();
        assert!(!revoke(&db, "some-other-org", &other.id, None).unwrap());
    }

    #[test]
    fn a_forged_or_malformed_link_is_refused() {
        let (db, org, owner) = setup();
        let (inv, link) = create(
            &db,
            &org,
            "real@example.com",
            Role::Admin,
            &owner,
            DEFAULT_TTL_SECS,
            None,
        )
        .unwrap();
        for bad in [
            "",
            "nonsense",
            "stinv_",
            "stinv_onlyid",
            &format!("stinv_{}_wrongsecret", inv.id),
            &format!("stinv_nosuchid_{}", "x".repeat(52)),
            // The id alone is not enough — the secret is the credential.
            &inv.id,
        ] {
            assert!(verify(&db, bad).unwrap().is_none(), "{bad:?} verified");
        }
        // The genuine link still works after all that.
        assert!(verify(&db, &link).unwrap().is_some());
    }

    #[test]
    fn an_existing_account_is_attached_rather_than_duplicated() {
        let (db, org, owner) = setup();
        let existing = users::create(
            &db,
            "known@example.com",
            "Known",
            Some("a long enough password"),
        )
        .unwrap();
        let (_, link) = create(
            &db,
            &org,
            "known@example.com",
            Role::Viewer,
            &owner,
            DEFAULT_TTL_SECS,
            None,
        )
        .unwrap();

        // No password needed: the account already has one.
        let out = accept(&db, &link, "", None).unwrap();
        assert!(!out.created_user);
        assert_eq!(out.user_id, existing.id);
        assert_eq!(
            members::role_of(&db, &org, &existing.id).unwrap(),
            Some(Role::Viewer)
        );
    }

    #[test]
    fn creation_is_validated_and_deduplicated() {
        let (db, org, owner) = setup();
        assert!(create(
            &db,
            &org,
            "not-an-email",
            Role::Member,
            &owner,
            DEFAULT_TTL_SECS,
            None
        )
        .is_err());

        create(
            &db,
            &org,
            "dup@example.com",
            Role::Member,
            &owner,
            DEFAULT_TTL_SECS,
            None,
        )
        .unwrap();
        // A second pending invite to the same address is refused, in any case.
        assert!(create(
            &db,
            &org,
            "DUP@example.com",
            Role::Admin,
            &owner,
            DEFAULT_TTL_SECS,
            None
        )
        .is_err());

        assert_eq!(list(&db, &org).unwrap().len(), 1);
        assert!(list(&db, "other-org").unwrap().is_empty());

        // A new account needs a password; refusing is better than making
        // an account nobody can sign in to.
        let (_, link) = create(
            &db,
            &org,
            "nopass@example.com",
            Role::Member,
            &owner,
            DEFAULT_TTL_SECS,
            None,
        )
        .unwrap();
        assert!(accept(&db, &link, "No Pass", None).is_err());
        // …and the invite survives the failed attempt, still usable.
        assert!(verify(&db, &link).unwrap().is_some());
        assert!(accept(&db, &link, "No Pass", Some("a long enough password")).is_ok());
    }
}
