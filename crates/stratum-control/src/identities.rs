//! Linked sign-in providers: one person, arriving through somebody
//! else's front door.
//!
//! The `identities` table has been in the schema since migration 0001
//! and nothing has ever read or written it. It was put there then so
//! that adding OAuth would be a handler rather than a migration on a
//! live `users` table — this module is that handler's half of the work.
//!
//! # The key is the provider's id, never its login
//!
//! `provider_user_id` is the provider's own immutable account id — for
//! GitHub, the integer in `GET /user`. A login is renameable, and a
//! released login becomes claimable by somebody else, so an identity
//! keyed on the *name* hands whoever takes that name next the account
//! it used to mean. The numeric id never moves and is never reissued.
//!
//! # One provider account reaches at most one user
//!
//! `UNIQUE (provider, provider_user_id)` makes that a property of the
//! database rather than a rule a handler has to remember. [`link`]
//! leans on it: an identity already held by somebody else is a refusal,
//! never a silent re-point, because re-pointing it is exactly the move
//! an account takeover would need.

use crate::db::ControlDb;
use crate::ids::{now_ms, ulid};

/// GitHub, as this table spells it. The only provider today; named as a
/// constant so the two call sites cannot disagree about the spelling.
pub const GITHUB: &str = "github";

/// The user a provider account belongs to, if it has been linked.
pub fn user_for(
    db: &ControlDb,
    provider: &str,
    provider_user_id: &str,
) -> Result<Option<String>, String> {
    db.lock()
        .query_opt(
            "SELECT user_id FROM identities WHERE provider = $1 AND provider_user_id = $2",
            &[&provider.to_string(), &provider_user_id.to_string()],
        )
        .map(|r| r.as_ref().map(|r| r.get::<_, String>("user_id")))
        .map_err(|e| format!("lookup identity: {e}"))
}

/// Record that `user_id` signs in through this provider account.
///
/// Idempotent for the owner, because the sign-in handler calls it on
/// every arrival: an account that was created before it had a link
/// acquires one the first time its owner comes through this door, and
/// every trip after that is a no-op rather than a duplicate row.
///
/// Refused when another user already holds the identity. That cannot
/// happen through the sign-in handler — it looks the identity up first
/// — but it is the one outcome that would be an account takeover, so it
/// is refused here as well, where the unique index can enforce it
/// rather than an ordering somebody might rearrange.
pub fn link(
    db: &ControlDb,
    user_id: &str,
    provider: &str,
    provider_user_id: &str,
) -> Result<(), String> {
    let n = db
        .lock()
        .execute(
            "INSERT INTO identities (id, user_id, provider, provider_user_id, created_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (provider, provider_user_id) DO NOTHING",
            &[
                &ulid(),
                &user_id.to_string(),
                &provider.to_string(),
                &provider_user_id.to_string(),
                &now_ms(),
            ],
        )
        .map_err(|e| format!("link identity: {e}"))?;
    if n == 0 {
        return match user_for(db, provider, provider_user_id)? {
            Some(held) if held == user_id => Ok(()),
            // Somebody else's, or gone between the insert and the read.
            // Both are "not yours", and neither is worth telling apart
            // to a caller that must refuse either way.
            _ => Err("that account is already linked to another user".into()),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users;

    fn setup(hint: &str) -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap()
    }

    fn user(db: &ControlDb, email: &str) -> String {
        users::create(db, email, "Person", None).unwrap().id
    }

    #[test]
    fn an_unlinked_provider_account_belongs_to_nobody() {
        let db = setup("identities_unlinked");
        assert_eq!(user_for(&db, GITHUB, "42").unwrap(), None);
    }

    #[test]
    fn a_link_is_found_by_the_provider_id() {
        let db = setup("identities_link");
        let uid = user(&db, "ada@example.com");
        link(&db, &uid, GITHUB, "42").unwrap();
        assert_eq!(user_for(&db, GITHUB, "42").unwrap(), Some(uid));
        // A different provider account is a different row, even for the
        // same number.
        assert_eq!(user_for(&db, "gitlab", "42").unwrap(), None);
    }

    #[test]
    fn linking_the_same_identity_again_is_a_no_op() {
        let db = setup("identities_idempotent");
        let uid = user(&db, "ada@example.com");
        link(&db, &uid, GITHUB, "42").unwrap();
        link(&db, &uid, GITHUB, "42").unwrap();
        assert_eq!(user_for(&db, GITHUB, "42").unwrap(), Some(uid));
    }

    /// The takeover this table's unique index exists to refuse. A second
    /// account claiming a linked provider id must not re-point it.
    #[test]
    fn a_provider_account_cannot_be_moved_to_another_user() {
        let db = setup("identities_stolen");
        let ada = user(&db, "ada@example.com");
        let mallory = user(&db, "mallory@example.com");
        link(&db, &ada, GITHUB, "42").unwrap();
        let err = link(&db, &mallory, GITHUB, "42").unwrap_err();
        assert!(err.contains("already linked"), "{err}");
        assert_eq!(user_for(&db, GITHUB, "42").unwrap(), Some(ada));
    }

    /// A hostile string reaches the query as a parameter, never as SQL,
    /// and names nothing.
    #[test]
    fn a_hostile_provider_id_names_nothing() {
        let db = setup("identities_hostile");
        assert_eq!(
            user_for(&db, GITHUB, "42'; DROP TABLE users--").unwrap(),
            None
        );
    }
}
