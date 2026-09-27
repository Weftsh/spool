//! Proving an address, and getting back in without one.
//!
//! Both are the same machinery — a random secret, only its hash stored,
//! single-use, expiring — so both live here and are told apart by
//! [`Kind`]. The prefixes differ (`weftv_` / `weftrs_`) so that pasting
//! the wrong link into the wrong field gives a plain answer instead of a
//! puzzling one, but the *authority* is the `kind` column: a
//! verification token presented to the reset path is refused because the
//! row says what it is for, not because of how the string looked.
//!
//! Modelled on [`crate::invites`], deliberately, down to the failure
//! shape: every way a token can be wrong — malformed, unknown, spent,
//! expired, wrong kind, wrong secret — is the same `None`, so a token
//! cannot be used to learn which accounts exist.

use crate::db::ControlDb;
use crate::ids::{now_ms, token_secret, ulid};
use sha2::{Digest, Sha256};

/// A day. Long enough to survive a mail queue and a night's sleep, short
/// enough that a link in an old inbox is not a live credential.
pub const VERIFY_TTL_SECS: i64 = 24 * 3600;

/// An hour. A reset link is the strongest credential this system mails,
/// because redeeming it takes over the account — so it lives briefly.
pub const RESET_TTL_SECS: i64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Verify,
    Reset,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Verify => "verify",
            Kind::Reset => "reset",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Kind::Verify => "weftv_",
            Kind::Reset => "weftrs_",
        }
    }

    pub fn ttl_secs(self) -> i64 {
        match self {
            Kind::Verify => VERIFY_TTL_SECS,
            Kind::Reset => RESET_TTL_SECS,
        }
    }
}

fn hash(secret: &str) -> String {
    stratum_store::pack::hex(&Sha256::digest(secret.as_bytes()))
}

/// Mint a token for this person, returning the string to mail them.
///
/// Any live token of the same kind is spent first. Asking for a second
/// verification link must not leave the first one working: somebody who
/// clicks "resend" because they think the first went astray has told you
/// they no longer trust it, and two live credentials is one more than
/// the flow needs.
pub fn issue(db: &ControlDb, user_id: &str, kind: Kind) -> Result<String, String> {
    let secret = token_secret();
    let id = ulid();
    let now = now_ms();
    let expires = now + kind.ttl_secs() * 1000;
    let hashed = hash(&secret);
    let user_id = user_id.to_string();
    let row_id = id.clone();
    db.lock()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE user_tokens SET used_at = $3 \
                 WHERE user_id = $1 AND kind = $2 AND used_at IS NULL",
                &[&user_id, &kind.as_str(), &now],
            )?;
            tx.execute(
                "INSERT INTO user_tokens \
                 (id, user_id, kind, token_hash, created_at, expires_at) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[&row_id, &user_id, &kind.as_str(), &hashed, &now, &expires],
            )?;
            Ok(())
        })
        .map_err(|e| format!("issue {} token: {e}", kind.as_str()))?;
    Ok(format!("{}{id}_{secret}", kind.prefix()))
}

/// Spend a token, returning whose it was.
///
/// Verification and redemption are one statement on purpose. Checking
/// first and stamping after leaves a window in which two requests both
/// see a live token — and for a reset link that window is two people
/// taking over one account.
pub fn redeem(db: &ControlDb, presented: &str, kind: Kind) -> Result<Option<String>, String> {
    let Some(rest) = presented.strip_prefix(kind.prefix()) else {
        return Ok(None);
    };
    let Some((id, secret)) = rest.split_once('_') else {
        return Ok(None);
    };
    let now = now_ms();
    let row = db
        .lock()
        .query_opt(
            "UPDATE user_tokens SET used_at = $4 \
             WHERE id = $1 AND kind = $2 AND token_hash = $3 \
               AND used_at IS NULL AND expires_at > $4 \
             RETURNING user_id",
            &[&id, &kind.as_str(), &hash(secret), &now],
        )
        .map_err(|e| format!("redeem token: {e}"))?;
    Ok(row.map(|r| r.get("user_id")))
}

/// Mark an address proved. Idempotent: verifying twice is not an error,
/// it is somebody clicking a link twice.
/// The `user_emails` row for the same address is stamped alongside it,
/// in the same transaction. Proving the mailbox behind the account
/// credential proves that mailbox — and `user_emails` is what authorship
/// is resolved through (`profiles::user_for_author`), so leaving it
/// unproved would mean somebody's own commits, signed with the address
/// they signed up with, credited nobody.
pub fn mark_verified(db: &ControlDb, user_id: &str) -> Result<(), String> {
    let user_id = user_id.to_string();
    let now = now_ms();
    db.lock()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE users SET verified_at = $2 WHERE id = $1 AND verified_at IS NULL",
                &[&user_id, &now],
            )?;
            tx.execute(
                "UPDATE user_emails e SET verified_at = $2, \
                 verify_hash = NULL, verify_expires_at = NULL \
                 FROM users u \
                 WHERE u.id = e.user_id AND e.user_id = $1 AND e.address = u.email \
                   AND e.verified_at IS NULL",
                &[&user_id, &now],
            )?;
            Ok(())
        })
        .map_err(|e| format!("mark verified: {e}"))
}

pub fn is_verified(db: &ControlDb, user_id: &str) -> Result<bool, String> {
    db.lock()
        .query_opt(
            "SELECT verified_at FROM users WHERE id = $1",
            &[&user_id.to_string()],
        )
        .map_err(|e| format!("read verified: {e}"))
        .map(|row| {
            row.and_then(|r| r.get::<_, Option<i64>>("verified_at"))
                .is_some()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users;

    fn setup(hint: &str) -> (ControlDb, String) {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap();
        let u =
            users::create(&db, "person@example.com", "Person", Some("a long password")).unwrap();
        (db, u.id)
    }

    #[test]
    fn a_token_works_once_and_only_for_its_own_kind() {
        let (db, user) = setup("usertokens");
        let verify = issue(&db, &user, Kind::Verify).unwrap();
        assert!(verify.starts_with("weftv_"));

        // A verification link is not a password reset, even though both
        // name the same person and both are live.
        let reset = issue(&db, &user, Kind::Reset).unwrap();
        assert!(reset.starts_with("weftrs_"));
        assert!(redeem(&db, &verify, Kind::Reset).unwrap().is_none());
        assert!(redeem(&db, &reset, Kind::Verify).unwrap().is_none());

        // …and each works exactly once for its own kind.
        assert_eq!(
            redeem(&db, &verify, Kind::Verify).unwrap(),
            Some(user.clone())
        );
        assert!(redeem(&db, &verify, Kind::Verify).unwrap().is_none());
        assert_eq!(
            redeem(&db, &reset, Kind::Reset).unwrap(),
            Some(user.clone())
        );
        assert!(redeem(&db, &reset, Kind::Reset).unwrap().is_none());
    }

    /// Issuing again spends what came before. Somebody who asks for a
    /// second link has told you they do not trust the first.
    #[test]
    fn issuing_again_kills_the_previous_link() {
        let (db, user) = setup("usertokens-reissue");
        let first = issue(&db, &user, Kind::Verify).unwrap();
        let second = issue(&db, &user, Kind::Verify).unwrap();
        assert_ne!(first, second);
        assert!(redeem(&db, &first, Kind::Verify).unwrap().is_none());
        assert_eq!(redeem(&db, &second, Kind::Verify).unwrap(), Some(user));
    }

    /// Every wrong shape answers the same `None`, so a token cannot be
    /// used to ask which accounts or which links exist.
    #[test]
    fn every_malformed_or_forged_token_is_refused_identically() {
        let (db, user) = setup("usertokens-forged");
        let good = issue(&db, &user, Kind::Verify).unwrap();
        let id = good
            .strip_prefix("weftv_")
            .and_then(|r| r.split_once('_'))
            .map(|(i, _)| i.to_string())
            .unwrap();
        for bad in [
            String::new(),
            "nonsense".into(),
            "weftv_".into(),
            "weftv_onlyid".into(),
            // The right id with the wrong secret: the id is not the
            // credential.
            format!("weftv_{id}_{}", "x".repeat(52)),
            // A well-formed token for a row that does not exist.
            format!("weftv_01zzzzzzzzzzzzzzzzzzzzzzzz_{}", "x".repeat(52)),
            format!("{good}x"),
            // The right token under the wrong prefix.
            good.replace("weftv_", "weftrs_"),
        ] {
            assert!(
                redeem(&db, &bad, Kind::Verify).unwrap().is_none(),
                "{bad:?} was redeemed"
            );
        }
        // The genuine one still works after all that.
        assert_eq!(redeem(&db, &good, Kind::Verify).unwrap(), Some(user));
    }

    /// An expired token is dead even though nothing has spent it.
    #[test]
    fn an_expired_token_is_refused() {
        let (db, user) = setup("usertokens-expiry");
        let token = issue(&db, &user, Kind::Reset).unwrap();
        // Reach past the API to age it: the issuing path deliberately
        // has no way to mint one that is already dead.
        db.lock()
            .execute(
                "UPDATE user_tokens SET expires_at = $2 WHERE user_id = $1",
                &[&user, &(now_ms() - 1)],
            )
            .unwrap();
        assert!(redeem(&db, &token, Kind::Reset).unwrap().is_none());
    }

    /// Existing accounts were marked verified by the migration; a new
    /// one is not, until it is.
    #[test]
    fn verification_is_recorded_once_and_is_idempotent() {
        let (db, user) = setup("usertokens-verified");
        // `create` does not verify: only redeeming a link does.
        db.lock()
            .execute(
                "UPDATE users SET verified_at = NULL WHERE id = $1",
                &[&user],
            )
            .unwrap();
        assert!(!is_verified(&db, &user).unwrap());
        mark_verified(&db, &user).unwrap();
        assert!(is_verified(&db, &user).unwrap());
        let first: Option<i64> = db
            .lock()
            .query_opt("SELECT verified_at FROM users WHERE id = $1", &[&user])
            .unwrap()
            .unwrap()
            .get("verified_at");
        // Clicking the link twice does not move the timestamp.
        mark_verified(&db, &user).unwrap();
        let again: Option<i64> = db
            .lock()
            .query_opt("SELECT verified_at FROM users WHERE id = $1", &[&user])
            .unwrap()
            .unwrap()
            .get("verified_at");
        assert_eq!(first, again);
        // An account that does not exist is simply not verified.
        assert!(!is_verified(&db, "01zzzzzzzzzzzzzzzzzzzzzzzz").unwrap());
    }
}
