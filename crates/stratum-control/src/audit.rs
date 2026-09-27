//! Append-only audit log (R7). No UPDATE or DELETE statement for this
//! table exists anywhere in the codebase — immutability by construction at
//! the application layer; the hourly S3 batch shipper (worker) is the
//! durable copy.

use crate::auth::Principal;
use crate::db::ControlDb;
use crate::ids::now_ms;
use serde::Serialize;

/// What an unauthenticated action is recorded as. Anonymous reads are not
/// audited; this is for the writes a public repo permits.
pub const ANONYMOUS: &str = "anonymous";

/// Passed by value into every mutating operation so recording can't be
/// forgotten at call sites.
#[derive(Debug, Clone)]
pub struct AuditCtx {
    /// Acting principal, e.g. "user:<id>", "token:<id>" or
    /// "system:<worker>". Always produced by [`Principal::audit_id`] for
    /// a request, so no call site gets to spell it differently.
    pub principal: String,
    /// The person, when one acted. Stored beside `principal` so "what did
    /// Alice do?" is an indexed query that can join to her name, rather
    /// than a prefix match on text.
    pub user_id: Option<String>,
    pub org_id: String,
}

impl AuditCtx {
    /// The context for a request, from whoever it authenticated as.
    ///
    /// The single producer: given the principal, there is exactly one way
    /// to spell who acted, and forgetting the person is not one of them.
    pub fn of(org_id: &str, principal: Option<&Principal>) -> AuditCtx {
        AuditCtx {
            principal: principal
                .map(|p| p.audit_id())
                .unwrap_or_else(|| ANONYMOUS.to_string()),
            user_id: principal.and_then(|p| p.user_id.clone()),
            org_id: org_id.to_string(),
        }
    }

    /// The context for work the platform does on its own behalf —
    /// compaction, gc, mirror polling. `who` names the worker.
    pub fn system(org_id: &str, who: &str) -> AuditCtx {
        AuditCtx {
            principal: format!("system:{who}"),
            user_id: None,
            org_id: org_id.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub seq: i64,
    pub at: i64,
    pub org_id: String,
    pub repo_id: Option<String>,
    pub principal: String,
    /// The person who acted, when one did.
    pub user_id: Option<String>,
    /// Their address at read time — resolved by join, never copied into
    /// the row: an audit entry records what happened, and a person's
    /// address is not part of what happened.
    pub user_email: Option<String>,
    pub user_name: Option<String>,
    pub action: String,
    pub context: Option<serde_json::Value>,
}

pub fn record(
    db: &ControlDb,
    ctx: &AuditCtx,
    repo_id: Option<&str>,
    action: &str,
    context: Option<&serde_json::Value>,
) -> Result<(), String> {
    db.lock()
        .execute(
            "INSERT INTO audit_log \
             (at, org_id, repo_id, principal, user_id, action, context) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            &[
                &now_ms(),
                &ctx.org_id,
                &repo_id,
                &ctx.principal,
                &ctx.user_id,
                &action,
                &context.map(|c| c.to_string()),
            ],
        )
        .map(|_| ())
        .map_err(|e| format!("record audit entry: {e}"))
}

/// Record inside a caller's transaction, so the change and its record
/// commit together or not at all.
///
/// The trail is not a log kept alongside the change; for a
/// security-relevant change it is part of it. A credential that exists
/// with no record of who created it is worse than no credential, and
/// "the audit write failed, ignore it" is how that happens. Callers that
/// mint, revoke, or move authority use this rather than [`record`].
pub fn record_tx(
    tx: &mut postgres::Transaction,
    ctx: &AuditCtx,
    repo_id: Option<&str>,
    action: &str,
    context: Option<&serde_json::Value>,
) -> Result<(), postgres::Error> {
    tx.execute(
        "INSERT INTO audit_log \
         (at, org_id, repo_id, principal, user_id, action, context) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        &[
            &now_ms(),
            &ctx.org_id,
            &repo_id,
            &ctx.principal,
            &ctx.user_id,
            &action,
            &context.map(|c| c.to_string()),
        ],
    )?;
    Ok(())
}

#[derive(Default)]
pub struct AuditQuery<'a> {
    pub repo_id: Option<&'a str>,
    pub principal: Option<&'a str>,
    /// Everything one person did, without the caller having to know how
    /// `principal` is spelled.
    pub user_id: Option<&'a str>,
    pub action: Option<&'a str>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub after_seq: Option<i64>,
    /// Page *backwards* — rows strictly older than this seq. The cursor
    /// that goes with `newest_first`.
    pub before_seq: Option<i64>,
    /// Newest first. The shipper walks the table forward from a
    /// watermark and wants the oldest unshipped row next; a person
    /// reading an activity feed wants what just happened, and would
    /// otherwise be handed the hundred oldest events the org ever
    /// recorded. Both orders are the same rows, read from opposite ends.
    pub newest_first: bool,
    pub limit: usize,
}

/// A filter value that could never match anything stored.
///
/// Every value in this table was written by us: ids are ULIDs, actions
/// come from a fixed set, principals are `user:`/`token:`/`system:` plus
/// a ULID. A filter carrying a NUL or a control character matches none of
/// them — but handed to Postgres it is a `db error` and a 500, where the
/// contract promises "no results". Same reasoning as `ids::valid_id`.
fn unmatchable(v: &str) -> bool {
    v.bytes().any(|b| b < 0x20 || b == 0x7f)
}

pub fn query(db: &ControlDb, org_id: &str, q: &AuditQuery) -> Result<Vec<AuditEntry>, String> {
    // A user id that cannot exist, or any filter that cannot match, names
    // nothing — answer that rather than round-tripping hostile bytes.
    if q.user_id.is_some_and(|u| !crate::ids::valid_id(u))
        || [q.action, q.principal, q.repo_id]
            .into_iter()
            .flatten()
            .any(unmatchable)
    {
        return Ok(Vec::new());
    }
    // `ORDER BY` cannot be a bind parameter, so the direction is the one
    // thing interpolated — from a bool, not from anything a caller typed.
    let sql = format!(
        "SELECT a.seq, a.at, a.org_id, a.repo_id, a.principal, a.user_id, \
                    a.action, a.context, u.email, u.name \
             FROM audit_log a LEFT JOIN users u ON u.id = a.user_id \
             WHERE a.org_id = $1 \
               AND ($2::text IS NULL OR a.repo_id = $2) \
               AND ($3::text IS NULL OR a.principal = $3) \
               AND ($4::int8 IS NULL OR a.at >= $4) \
               AND ($5::int8 IS NULL OR a.seq > $5) \
               AND ($6::text IS NULL OR a.user_id = $6) \
               AND ($7::text IS NULL OR a.action = $7) \
               AND ($8::int8 IS NULL OR a.at <= $8) \
               AND ($9::int8 IS NULL OR a.seq < $9) \
             ORDER BY a.seq {} LIMIT $10",
        if q.newest_first { "DESC" } else { "ASC" }
    );
    let rows = db
        .lock()
        .query(
            sql.as_str(),
            &[
                &org_id,
                &q.repo_id,
                &q.principal,
                &q.since_ms,
                &q.after_seq,
                &q.user_id,
                &q.action,
                &q.until_ms,
                &q.before_seq,
                &(q.limit.clamp(1, 1000) as i64),
            ],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|r| AuditEntry {
            seq: r.get("seq"),
            at: r.get("at"),
            org_id: r.get("org_id"),
            repo_id: r.get("repo_id"),
            principal: r.get("principal"),
            user_id: r.get("user_id"),
            user_email: r.get("email"),
            user_name: r.get("name"),
            action: r.get("action"),
            context: r
                .get::<_, Option<String>>("context")
                .and_then(|s| serde_json::from_str(&s).ok()),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry;

    #[test]
    fn record_and_filtered_query() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("control-audit")).unwrap();
        let org = registry::create_org(&db, "org").unwrap();
        let ctx = AuditCtx {
            principal: "token:t1".into(),
            user_id: None,
            org_id: org.id.clone(),
        };
        let blob = serde_json::json!({"agent": "run-42"});
        record(&db, &ctx, Some("r1"), "repo.commit", Some(&blob)).unwrap();
        record(&db, &ctx, Some("r2"), "repo.delete", None).unwrap();

        let all = query(
            &db,
            &org.id,
            &AuditQuery {
                repo_id: None,
                principal: None,
                since_ms: None,
                after_seq: None,
                limit: 100,
                ..AuditQuery::default()
            },
        )
        .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].action, "repo.commit");
        assert_eq!(all[0].context.as_ref().unwrap()["agent"], "run-42");

        let only_r1 = query(
            &db,
            &org.id,
            &AuditQuery {
                repo_id: Some("r1"),
                principal: None,
                since_ms: None,
                after_seq: None,
                limit: 100,
                ..AuditQuery::default()
            },
        )
        .unwrap();
        assert_eq!(only_r1.len(), 1);

        // A wrong-org query sees nothing.
        let other = query(
            &db,
            "no-such-org",
            &AuditQuery {
                repo_id: None,
                principal: None,
                since_ms: None,
                after_seq: None,
                limit: 100,
                ..AuditQuery::default()
            },
        )
        .unwrap();
        assert!(other.is_empty());

        // Both ends of the same rows. Ascending is what the shipper
        // walks; descending is what a person reading a feed wants, and
        // it pages with `before_seq` rather than `after_seq`.
        let newest = query(
            &db,
            &org.id,
            &AuditQuery {
                newest_first: true,
                limit: 1,
                ..AuditQuery::default()
            },
        )
        .unwrap();
        assert_eq!(newest.len(), 1);
        assert_eq!(newest[0].action, "repo.delete");

        let older = query(
            &db,
            &org.id,
            &AuditQuery {
                newest_first: true,
                before_seq: Some(newest[0].seq),
                limit: 10,
                ..AuditQuery::default()
            },
        )
        .unwrap();
        assert_eq!(older.len(), 1, "the page before the newest is the oldest");
        assert_eq!(older[0].action, "repo.commit");
        assert!(older[0].seq < newest[0].seq);
    }
}
