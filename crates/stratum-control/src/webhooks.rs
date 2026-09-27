//! Repos push webhooks: subscriptions + delivery records.

use crate::db::ControlDb;
use crate::ids::{now_ms, token_secret, ulid};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Subscription {
    pub id: String,
    pub org_id: String,
    pub repo_id: String,
    pub url: String,
    /// HMAC secret; returned once at creation.
    #[serde(skip_serializing)]
    pub secret: String,
    pub created_at: i64,
}

pub fn create(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    url: &str,
) -> Result<Subscription, String> {
    let sub = Subscription {
        id: ulid(),
        org_id: org_id.into(),
        repo_id: repo_id.into(),
        url: url.into(),
        secret: token_secret(),
        created_at: now_ms(),
    };
    db.lock()
        .execute(
            "INSERT INTO webhook_subscriptions (id, org_id, repo_id, url, secret, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &sub.id,
                &sub.org_id,
                &sub.repo_id,
                &sub.url,
                &sub.secret,
                &sub.created_at,
            ],
        )
        .map_err(|e| e.to_string())?;
    Ok(sub)
}

pub fn for_repo(db: &ControlDb, repo_id: &str) -> Result<Vec<Subscription>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT id, org_id, repo_id, url, secret, created_at \
             FROM webhook_subscriptions WHERE repo_id = $1",
            &[&repo_id],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|r| Subscription {
            id: r.get(0),
            org_id: r.get(1),
            repo_id: r.get(2),
            url: r.get(3),
            secret: r.get(4),
            created_at: r.get(5),
        })
        .collect())
}

pub fn delete(db: &ControlDb, org_id: &str, id: &str) -> Result<bool, String> {
    let n = db
        .lock()
        .execute(
            "DELETE FROM webhook_subscriptions WHERE org_id = $1 AND id = $2",
            &[&org_id, &id],
        )
        .map_err(|e| e.to_string())?;
    Ok(n > 0)
}

pub fn record_delivery(
    db: &ControlDb,
    subscription_id: &str,
    event: &str,
    state: &str,
    attempts: i64,
    last_error: Option<&str>,
) -> Result<(), String> {
    db.lock()
        .execute(
            "INSERT INTO webhook_deliveries \
             (id, subscription_id, event, state, attempts, last_error, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $7)",
            &[
                &ulid(),
                &subscription_id,
                &event,
                &state,
                &attempts,
                &last_error,
                &now_ms(),
            ],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Worker-cursor storage (audit shipper etc.).
pub fn meta_get(db: &ControlDb, key: &str) -> Result<Option<String>, String> {
    db.lock()
        .query_opt("SELECT value FROM meta WHERE key = $1", &[&key])
        .map(|row| row.map(|r| r.get(0)))
        .map_err(|e| e.to_string())
}

pub fn meta_set(db: &ControlDb, key: &str, value: &str) -> Result<(), String> {
    db.lock()
        .execute(
            "INSERT INTO meta (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = $2",
            &[&key, &value],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_roundtrip() {
        let db = crate::ControlDb::open(&stratum_testkit::pg::test_db_url("control-meta")).unwrap();
        assert_eq!(meta_get(&db, "cursor").unwrap(), None);
        meta_set(&db, "cursor", "42").unwrap();
        assert_eq!(meta_get(&db, "cursor").unwrap(), Some("42".into()));
        meta_set(&db, "cursor", "43").unwrap();
        assert_eq!(meta_get(&db, "cursor").unwrap(), Some("43".into()));
    }
}
