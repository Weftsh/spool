//! The server's Weft license key, and what the license service last said
//! about it.
//!
//! One row: a server has one license or none. The key is stored as the
//! operator installed it — the server verifies it again whenever it
//! reads it, so a key is only ever as trusted as the build reading it.
//! Nothing here decides anything: a license informs the operator and
//! never stops the server (see `stratum-server`'s `license` module).

use crate::db::{detail, ControlDb};
use crate::ids::now_ms;

/// The installed key, and the last daily check's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub key: String,
    /// The license's id (`lid`), which the daily check names.
    pub lid: String,
    pub installed_at: i64,
    /// When the last check finished, answered or not.
    pub checked_at: Option<i64>,
    /// `active`, `lapsed` or `revoked`, when the service answered.
    pub status: Option<String>,
    /// A sentence for the operator, when the service had one.
    pub notice: Option<String>,
    /// Why the last check got no answer, when it got none.
    pub error: Option<String>,
}

pub const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS server_license (
    -- One row, ever: the server's license.
    one           BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (one),
    key           TEXT NOT NULL,
    lid           TEXT NOT NULL,
    installed_at  BIGINT NOT NULL,
    checked_at    BIGINT,
    status        TEXT,
    notice        TEXT,
    error         TEXT
);
"#;

pub fn get(db: &ControlDb) -> Result<Option<Installed>, String> {
    let row = db
        .lock()
        .query_opt(
            "SELECT key, lid, installed_at, checked_at, status, notice, error
             FROM server_license",
            &[],
        )
        .map_err(|e| detail(&e))?;
    Ok(row.map(|r| Installed {
        key: r.get("key"),
        lid: r.get("lid"),
        installed_at: r.get("installed_at"),
        checked_at: r.get("checked_at"),
        status: r.get("status"),
        notice: r.get("notice"),
        error: r.get("error"),
    }))
}

/// Install a key the caller has verified, replacing any other. What the
/// service said about the previous key is not about this one, so it goes.
pub fn install(db: &ControlDb, key: &str, lid: &str) -> Result<(), String> {
    db.lock()
        .execute(
            "INSERT INTO server_license (one, key, lid, installed_at)
             VALUES (TRUE, $1, $2, $3)
             ON CONFLICT (one) DO UPDATE
               SET key = EXCLUDED.key, lid = EXCLUDED.lid,
                   installed_at = EXCLUDED.installed_at,
                   checked_at = NULL, status = NULL, notice = NULL, error = NULL",
            &[&key, &lid, &now_ms()],
        )
        .map(|_| ())
        .map_err(|e| detail(&e))
}

/// Remove the key. `false` when there was none.
pub fn remove(db: &ControlDb) -> Result<bool, String> {
    db.lock()
        .execute("DELETE FROM server_license", &[])
        .map(|n| n > 0)
        .map_err(|e| detail(&e))
}

/// Record a check's outcome against the key it was made for — a key
/// replaced while the check was in flight keeps its own, empty, record.
pub fn record_answer(
    db: &ControlDb,
    lid: &str,
    status: &str,
    notice: Option<&str>,
) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE server_license
             SET checked_at = $2, status = $3, notice = $4, error = NULL
             WHERE lid = $1",
            &[&lid, &now_ms(), &status, &notice],
        )
        .map(|_| ())
        .map_err(|e| detail(&e))
}

/// Record a check that got no answer. What the service last said stays:
/// an unreachable service has not changed its mind.
pub fn record_error(db: &ControlDb, lid: &str, error: &str) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE server_license SET checked_at = $2, error = $3 WHERE lid = $1",
            &[&lid, &now_ms(), &error],
        )
        .map(|_| ())
        .map_err(|e| detail(&e))
}

/// The people a license counts: accounts that are not switched off.
/// Disabled accounts keep their history but cannot sign in, push or be
/// given anything, so they are nobody the server is being used by.
pub fn people(db: &ControlDb) -> Result<u32, String> {
    let n: i64 = db
        .lock()
        .query_one("SELECT count(*) FROM users WHERE disabled_at IS NULL", &[])
        .map_err(|e| detail(&e))?
        .get(0);
    u32::try_from(n).map_err(|_| format!("{n} accounts is out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users;

    fn db(hint: &str) -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap()
    }

    #[test]
    fn one_key_at_a_time_and_a_new_key_starts_with_no_answer() {
        let db = db("license_one");
        assert_eq!(get(&db).unwrap(), None);
        assert!(!remove(&db).unwrap());

        install(&db, "weft_lic_v1.a.b", "lic_a").unwrap();
        record_answer(&db, "lic_a", "active", Some("a sentence")).unwrap();
        let got = get(&db).unwrap().unwrap();
        assert_eq!(
            (
                got.lid.as_str(),
                got.status.as_deref(),
                got.notice.as_deref()
            ),
            ("lic_a", Some("active"), Some("a sentence"))
        );
        assert!(got.checked_at.is_some());

        // Unreachable: the last answer stays, and the error is beside it.
        record_error(&db, "lic_a", "connection refused").unwrap();
        let got = get(&db).unwrap().unwrap();
        assert_eq!(got.status.as_deref(), Some("active"));
        assert_eq!(got.error.as_deref(), Some("connection refused"));

        // A replacement is a different license: nothing carries over, and
        // an answer that arrives late for the old one lands nowhere.
        install(&db, "weft_lic_v1.c.d", "lic_c").unwrap();
        record_answer(&db, "lic_a", "revoked", None).unwrap();
        let got = get(&db).unwrap().unwrap();
        assert_eq!(got.lid, "lic_c");
        assert_eq!((got.checked_at, got.status, got.error), (None, None, None));

        assert!(remove(&db).unwrap());
        assert_eq!(get(&db).unwrap(), None);
    }

    #[test]
    fn people_are_the_accounts_that_are_not_switched_off() {
        let db = db("license_people");
        let before = people(&db).unwrap();
        let a = users::create(&db, "a@acme.test", "A", None).unwrap();
        users::create(&db, "b@acme.test", "B", None).unwrap();
        assert_eq!(people(&db).unwrap(), before + 2);
        users::set_disabled(&db, &a.id, true).unwrap();
        assert_eq!(people(&db).unwrap(), before + 1);
    }
}
