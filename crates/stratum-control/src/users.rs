//! User accounts: the person behind a credential.
//!
//! Passwords get Argon2id, deliberately unlike the API tokens next door
//! in `auth.rs`. That contrast is the whole point: a token is 256 bits of
//! OS randomness verified on *every* request, so a plain SHA-256 of it is
//! both safe (nothing to guess) and necessary (a KDF per request would
//! blow the latency budget or force caching, which would break instant
//! revocation). A password is low-entropy, human-chosen, and verified
//! only at sign-in — the opposite trade in both directions.

use crate::db::ControlDb;
use crate::ids::{now_ms, ulid};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: String,
    pub created_at: i64,
    pub disabled_at: Option<i64>,
    /// Their personal namespace's name, once they have one. `None` for
    /// accounts created before namespaces existed, and for any created
    /// by an operator against an org rather than through signup.
    pub handle: Option<String>,
    /// When this address was proved. `None` means the account may sign
    /// in and look around but not create anything that costs us money.
    pub verified_at: Option<i64>,
}

impl User {
    /// Git author identity for work this person does through the API.
    /// Before users existed, REST commits were authored by the acting
    /// token — `token:01hx… <token:01hx…@stratum.local>` — which is
    /// unreadable in `git log` and untraceable to a human.
    pub fn git_ident(&self) -> String {
        format!("{} <{}>", self.name, self.email)
    }
}

fn row_to_user(row: &postgres::Row) -> User {
    User {
        id: row.get("id"),
        email: row.get("email"),
        name: row.get("name"),
        created_at: row.get("created_at"),
        disabled_at: row.get("disabled_at"),
        handle: row.get("handle"),
        verified_at: row.get("verified_at"),
    }
}

const COLS: &str = "id, email, name, created_at, disabled_at, handle, verified_at";

/// Normalized form for lookup and uniqueness. Addresses are compared
/// case-insensitively so `Alice@` and `alice@` cannot become two accounts
/// that each believe they own the mailbox.
pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// Rejects what cannot be an address at all. Deliberately permissive
/// about the exotic middle ground — the authority on whether a mailbox
/// exists is delivery, not a regex — but strict about the shapes that
/// would break storage or display.
pub fn valid_email(email: &str) -> bool {
    let e = email.trim();
    if e.len() < 3 || e.len() > 254 {
        return false;
    }
    if e.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    let Some((local, domain)) = e.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !e.contains("..")
        && e.matches('@').count() == 1
}

/// The floor, not advice. Long passphrases beat short complex ones, so
/// this checks length and nothing else; a composition rule would only
/// push people toward `Passw0rd!`.
pub const MIN_PASSWORD_LEN: usize = 12;

/// What makes a password acceptable, separately from hashing one.
///
/// A caller sometimes needs to know before it commits to something else:
/// the reset endpoint checks here first so that a too-short password
/// costs a typo rather than the single-use link, which would send
/// somebody back through their inbox for a mistake they can see on
/// screen.
pub fn check_password_strength(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        ));
    }
    // 4096 bytes is far past any reasonable passphrase and well short of
    // what would make hashing a denial-of-service vector.
    if password.len() > 4096 {
        return Err("password is too long".into());
    }
    Ok(())
}

pub fn hash_password(password: &str) -> Result<String, String> {
    check_password_strength(password)?;
    // Salt from the same OS CSPRNG the rest of the control plane uses,
    // rather than pulling a second randomness stack into the build.
    let salt = SaltString::encode_b64(&crate::ids::random(16)).map_err(|e| format!("salt: {e}"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("hash password: {e}"))
}

/// Constant-time within Argon2's own verification. A stored hash that
/// fails to parse verifies as `false` rather than erroring, so a corrupt
/// row cannot be told apart from a wrong password.
pub fn verify_password(hash: &str, password: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Create a user. `password` may be `None` for an account that will only
/// ever sign in through a linked identity.
pub fn create(
    db: &ControlDb,
    email: &str,
    name: &str,
    password: Option<&str>,
) -> Result<User, String> {
    let email = normalize_email(email);
    if !valid_email(&email) {
        return Err("invalid email address".into());
    }
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err("name must be 1-200 characters".into());
    }
    let hash = match password {
        Some(p) => Some(hash_password(p)?),
        None => None,
    };
    let name_owned = name.to_string();
    let user = User {
        id: ulid(),
        email,
        name: name.to_string(),
        created_at: now_ms(),
        disabled_at: None,
        handle: None,
        // Proved by redeeming a mailed link, never by asking. The admin
        // CLI verifies explicitly after creating an operator account.
        verified_at: None,
    };
    // The credential address is also an *owned* address, so it lands in
    // `user_emails` in the same transaction. Migration 0020 backfilled
    // every account that already existed; without this, every account
    // created after it would be missing from the table authorship is
    // resolved through — which fails twice over. Their own commits would
    // credit nobody, and the string would still be unclaimed, so a
    // stranger could add somebody else's login address to their account
    // and hold it. Its proof tracks `users.verified_at`, which starts
    // NULL and is stamped by [`crate::usertokens::mark_verified`].
    let (row_id, row_email, row_created) = (user.id.clone(), user.email.clone(), user.created_at);
    let n = db
        .lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "INSERT INTO users (id, email, name, password_hash, created_at) \
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT (email) DO NOTHING",
                &[&row_id, &row_email, &name_owned, &hash, &row_created],
            )?;
            if n > 0 {
                tx.execute(
                    "INSERT INTO user_emails (address, user_id, verified_at, private, created_at) \
                     VALUES ($1, $2, NULL, true, $3)",
                    &[&row_email, &row_id, &row_created],
                )?;
            }
            Ok(n)
        })
        .map_err(|e| format!("create user: {e}"))?;
    if n == 0 {
        return Err("a user with that email already exists".into());
    }
    Ok(user)
}

pub fn by_email(db: &ControlDb, email: &str) -> Result<Option<User>, String> {
    let email = normalize_email(email);
    if !valid_email(&email) {
        // An unstorable address is definitionally absent — never let a
        // hostile string reach the query.
        return Ok(None);
    }
    db.lock()
        .query_opt(
            &format!("SELECT {COLS} FROM users WHERE email = $1"),
            &[&email],
        )
        .map(|r| r.as_ref().map(row_to_user))
        .map_err(|e| format!("lookup user: {e}"))
}

/// Accounts with no handle, oldest first.
///
/// A handle-less account is a **half-made account**, and both halves it
/// is missing are silent. `users.handle` is how a person is attributed,
/// so their issues render with no author at all; the personal namespace
/// that is claimed in the same transaction is where a fork with no
/// target goes, so forking answers "no personal namespace to fork
/// into". Neither errors at the point the account is made.
///
/// `admin user-create` produced exactly this state until it was fixed,
/// and the fix healed the door without healing the accounts already
/// through it. This is what the repair command enumerates.
pub fn without_handle(db: &ControlDb) -> Result<Vec<User>, String> {
    db.lock()
        .query(
            &format!("SELECT {COLS} FROM users WHERE handle IS NULL ORDER BY created_at"),
            &[],
        )
        .map(|rows| rows.iter().map(row_to_user).collect())
        .map_err(|e| format!("list accounts without a handle: {e}"))
}

pub fn by_id(db: &ControlDb, id: &str) -> Result<Option<User>, String> {
    if !crate::ids::valid_id(id) {
        return Ok(None);
    }
    db.lock()
        .query_opt(&format!("SELECT {COLS} FROM users WHERE id = $1"), &[&id])
        .map(|r| r.as_ref().map(row_to_user))
        .map_err(|e| format!("lookup user: {e}"))
}

/// Verify an email/password pair. Returns `None` for every failure shape
/// — unknown address, wrong password, disabled account, or an account
/// with no password at all — so sign-in cannot be used to enumerate who
/// has an account here.
pub fn authenticate(db: &ControlDb, email: &str, password: &str) -> Result<Option<User>, String> {
    let email = normalize_email(email);
    if !valid_email(&email) {
        return Ok(None);
    }
    let row = db
        .lock()
        .query_opt(
            &format!("SELECT {COLS}, password_hash FROM users WHERE email = $1"),
            &[&email],
        )
        .map_err(|e| format!("authenticate: {e}"))?;
    let Some(row) = row else {
        // Spend the same work as a real verification would, so response
        // time does not reveal whether the address is registered.
        let _ = verify_password(DUMMY_HASH, password);
        return Ok(None);
    };
    let hash: Option<String> = row.get("password_hash");
    let user = row_to_user(&row);
    match hash {
        Some(h) if verify_password(&h, password) && user.disabled_at.is_none() => Ok(Some(user)),
        Some(h) => {
            // Verified already; nothing more to do. Kept explicit so the
            // disabled-account case is visibly a refusal, not a fallthrough.
            let _ = h;
            Ok(None)
        }
        None => {
            let _ = verify_password(DUMMY_HASH, password);
            Ok(None)
        }
    }
}

/// A real Argon2id hash of a value nobody knows, used to equalize the
/// cost of authenticating a nonexistent account.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c3RyYXR1bWR1bW15c2FsdA$\
                          RdescudvJCsgt3ub+b+dWRWJTmaaJObG";

pub fn set_password(db: &ControlDb, user_id: &str, password: &str) -> Result<(), String> {
    let hash = hash_password(password)?;
    let n = db
        .lock()
        .execute(
            "UPDATE users SET password_hash = $2 WHERE id = $1",
            &[&user_id.to_string(), &hash],
        )
        .map_err(|e| format!("set password: {e}"))?;
    if n == 0 {
        return Err("no such user".into());
    }
    Ok(())
}

/// Take the password off an account, leaving it reachable only through
/// a linked identity.
///
/// This exists for one case, and it is a security one. Somebody can sign
/// up with an address they do not own and never confirm it — that
/// account can do nothing, which is the point of the confirmation gate,
/// but it *is* sitting on the address with a password its maker knows.
/// If the real owner of that mailbox then signs in through a provider
/// that has proved the address, adopting the waiting account as-is would
/// hand them an account the first person can still open. That is account
/// pre-hijacking, and the fix is to make the waiting credential
/// worthless at the moment the address is finally proved by somebody
/// else.
///
/// Safe to call on an account that has no password: `NULL = NULL` is
/// still `NULL`, and the row is simply written with what it held.
pub fn clear_password(db: &ControlDb, user_id: &str) -> Result<(), String> {
    let n = db
        .lock()
        .execute(
            "UPDATE users SET password_hash = NULL WHERE id = $1",
            &[&user_id.to_string()],
        )
        .map_err(|e| format!("clear password: {e}"))?;
    if n == 0 {
        return Err("no such user".into());
    }
    Ok(())
}

/// Disable an account. Deliberately not a delete: the audit trail refers
/// to this user by id, and rows that point at a vanished person are worse
/// than rows that point at a disabled one.
pub fn set_disabled(db: &ControlDb, user_id: &str, disabled: bool) -> Result<bool, String> {
    let at = disabled.then(now_ms);
    db.lock()
        .execute(
            "UPDATE users SET disabled_at = $2 WHERE id = $1",
            &[&user_id.to_string(), &at],
        )
        .map(|n| n > 0)
        .map_err(|e| format!("set disabled: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url("users")).unwrap()
    }

    /// Clearing a password must actually close the door, not just blank
    /// a column: `authenticate` has to refuse a NULL hash rather than
    /// treat it as "no password required".
    #[test]
    fn clearing_a_password_closes_the_door_it_opened() {
        let db = db();
        let u = create(
            &db,
            "clearme@example.com",
            "Clear Me",
            Some("a long password"),
        )
        .unwrap();
        assert!(authenticate(&db, "clearme@example.com", "a long password")
            .unwrap()
            .is_some());

        clear_password(&db, &u.id).unwrap();
        assert!(authenticate(&db, "clearme@example.com", "a long password")
            .unwrap()
            .is_none());
        // And an empty string is not a way back in either.
        assert!(authenticate(&db, "clearme@example.com", "")
            .unwrap()
            .is_none());

        // Idempotent: an account that already had no password is left
        // exactly as it was rather than erroring.
        clear_password(&db, &u.id).unwrap();

        // A user that is not there is an error, not a silent no-op —
        // this runs on a security path and "nothing happened" must never
        // read as "the credential is gone".
        let err = clear_password(&db, "01hxxxxxxxxxxxxxxxxxxxxxxx").unwrap_err();
        assert!(err.contains("no such user"), "{err}");
    }

    #[test]
    fn email_validation_and_normalization() {
        assert_eq!(normalize_email("  Alice@Example.COM "), "alice@example.com");
        for good in ["a@b.co", "alice.smith+tag@sub.example.com"] {
            assert!(valid_email(good), "{good} should be valid");
        }
        for bad in [
            "",
            "a",
            "no-at-sign",
            "@example.com",
            "alice@",
            "alice@nodot",
            "alice@.example.com",
            "alice@example.com.",
            "a..b@example.com",
            "two@at@example.com",
            "has space@example.com",
            "nul\0@example.com",
            "new\nline@example.com",
        ] {
            assert!(!valid_email(bad), "{bad:?} should be invalid");
        }
        // Length bound, so an absurd address cannot reach storage.
        assert!(!valid_email(&format!("{}@example.com", "a".repeat(250))));
    }

    #[test]
    fn passwords_are_hashed_not_stored_and_verify_only_against_themselves() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(hash.starts_with("$argon2id$"), "{hash}");
        assert!(
            !hash.contains("correct horse"),
            "the password is in the hash"
        );
        assert!(verify_password(&hash, "correct horse battery staple"));
        assert!(!verify_password(&hash, "correct horse battery stapl"));
        assert!(!verify_password(&hash, ""));

        // Two hashes of the same password differ: the salt is per-hash,
        // so a stolen table cannot be attacked by grouping equal rows.
        let again = hash_password("correct horse battery staple").unwrap();
        assert_ne!(hash, again);
        assert!(verify_password(&again, "correct horse battery staple"));

        // A corrupt or empty stored hash is a failed verification, not a
        // panic and not an accidental success.
        assert!(!verify_password("", "anything"));
        assert!(!verify_password("$argon2id$garbage", "anything"));
        assert!(!verify_password("plaintext-password", "plaintext-password"));

        // The floor is enforced at hashing time, so a short password can
        // never reach storage.
        assert!(hash_password("short").is_err());
        assert!(hash_password(&"a".repeat(MIN_PASSWORD_LEN - 1)).is_err());
        assert!(hash_password(&"a".repeat(MIN_PASSWORD_LEN)).is_ok());
        assert!(hash_password(&"a".repeat(5000)).is_err());
    }

    #[test]
    fn create_lookup_and_authenticate() {
        let db = db();
        let u = create(
            &db,
            "  Alice@Example.com ",
            "Alice",
            Some("a long enough password"),
        )
        .unwrap();
        assert_eq!(u.email, "alice@example.com");
        assert_eq!(u.git_ident(), "Alice <alice@example.com>");

        // Case-insensitive lookup, by either spelling.
        assert_eq!(
            by_email(&db, "ALICE@example.com").unwrap().unwrap().id,
            u.id
        );
        assert_eq!(by_id(&db, &u.id).unwrap().unwrap(), u);
        assert!(by_id(&db, "nope").unwrap().is_none());
        // An address that could never have been stored is definitionally
        // absent — the hostile string never reaches the query.
        assert!(by_email(&db, "not-an-email").unwrap().is_none());
        assert!(by_email(&db, "nul\0@example.com").unwrap().is_none());

        // Same mailbox in different case is the same account, not a second.
        assert!(create(
            &db,
            "alice@EXAMPLE.com",
            "Impostor",
            Some("another password!")
        )
        .is_err());

        assert_eq!(
            authenticate(&db, "alice@example.com", "a long enough password")
                .unwrap()
                .unwrap()
                .id,
            u.id
        );
        // Every failure shape is the same answer: no account enumeration.
        assert!(authenticate(&db, "alice@example.com", "wrong")
            .unwrap()
            .is_none());
        assert!(
            authenticate(&db, "ghost@example.com", "a long enough password")
                .unwrap()
                .is_none()
        );
        assert!(authenticate(&db, "not-an-email", "a long enough password")
            .unwrap()
            .is_none());

        // A disabled account cannot sign in, and re-enabling restores it.
        assert!(set_disabled(&db, &u.id, true).unwrap());
        assert!(
            authenticate(&db, "alice@example.com", "a long enough password")
                .unwrap()
                .is_none()
        );
        assert!(set_disabled(&db, &u.id, false).unwrap());
        assert!(
            authenticate(&db, "alice@example.com", "a long enough password")
                .unwrap()
                .is_some()
        );

        // Changing the password invalidates the old one.
        set_password(&db, &u.id, "a different long password").unwrap();
        assert!(
            authenticate(&db, "alice@example.com", "a long enough password")
                .unwrap()
                .is_none()
        );
        assert!(
            authenticate(&db, "alice@example.com", "a different long password")
                .unwrap()
                .is_some()
        );
        assert!(set_password(&db, "ghost", "a different long password").is_err());
        assert!(!set_disabled(&db, "ghost", true).unwrap());
    }

    /// Sign-in must cost the same whether or not the address exists.
    ///
    /// Argon2 takes tens of milliseconds; a database miss takes under
    /// one. Without deliberately spending the same work on an unknown
    /// address, "is alice@ a customer?" would be answerable with a
    /// stopwatch. The band here is wide on purpose — this is a check
    /// that the equalising work happens at all, not a benchmark.
    #[test]
    fn authenticating_an_unknown_address_costs_what_a_real_one_does() {
        let db = db();
        create(&db, "real@example.com", "R", Some("a long enough password")).unwrap();

        let once = |email: &str| {
            let t = std::time::Instant::now();
            let _ = authenticate(&db, email, "the wrong password");
            t.elapsed()
        };
        // Warm both paths, then take the best of five for each, the two
        // addresses alternating. The minimum is the run least disturbed
        // by whatever else the machine is doing, and alternating is what
        // makes that fair: timed as two back-to-back blocks, a burst of
        // load that covered every sample of the first and none of the
        // second read as the equalising work being missing.
        let _ = once("real@example.com");
        let _ = once("ghost@example.com");
        let (mut known, mut unknown) = (std::time::Duration::MAX, std::time::Duration::MAX);
        for _ in 0..5 {
            known = known.min(once("real@example.com"));
            unknown = unknown.min(once("ghost@example.com"));
        }
        assert!(
            unknown.as_secs_f64() > known.as_secs_f64() * 0.5,
            "unknown address answered in {unknown:?} vs {known:?} for a real one \
             — the equalising verification is not happening"
        );
    }

    /// Repeated failures neither lock the account nor change the answer.
    ///
    /// There is deliberately no lockout: an attacker who knows an address
    /// could otherwise lock its owner out at will, which trades a
    /// password attack for a denial-of-service one. The defence against
    /// guessing is the KDF's cost per attempt plus the 12-character
    /// floor, not a counter.
    #[test]
    fn repeated_wrong_passwords_neither_lock_out_nor_leak() {
        let db = db();
        create(
            &db,
            "target@example.com",
            "T",
            Some("a long enough password"),
        )
        .unwrap();
        for i in 0..12 {
            assert!(
                authenticate(&db, "target@example.com", &format!("guess-{i}"))
                    .unwrap()
                    .is_none(),
                "guess {i} was accepted"
            );
        }
        assert!(
            authenticate(&db, "target@example.com", "a long enough password")
                .unwrap()
                .is_some(),
            "the owner must not be locked out by someone else's guessing"
        );
    }

    #[test]
    fn creation_rejects_input_that_cannot_be_stored() {
        let db = db();
        assert!(create(&db, "not-an-email", "X", Some("a long enough password")).is_err());
        assert!(create(&db, "ok@example.com", "  ", Some("a long enough password")).is_err());
        assert!(create(
            &db,
            "ok@example.com",
            &"n".repeat(201),
            Some("a long enough password")
        )
        .is_err());
        assert!(create(&db, "ok@example.com", "X", Some("short")).is_err());
        // A passwordless account is legitimate but cannot sign in with one.
        let u = create(&db, "oauth@example.com", "OAuth Only", None).unwrap();
        assert!(authenticate(&db, "oauth@example.com", "")
            .unwrap()
            .is_none());
        assert!(authenticate(&db, "oauth@example.com", "anything at all")
            .unwrap()
            .is_none());
        assert!(by_id(&db, &u.id).unwrap().is_some());
    }
}
