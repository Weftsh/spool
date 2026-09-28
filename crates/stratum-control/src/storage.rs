//! What each repository holds: the control-plane record behind the
//! stored-bytes number a repository page shows.
//!
//! `storage_usage` has one row per owner, distinguished by `owner_kind`,
//! carrying the **logical** bytes its manifest names and, when an
//! inventory has run, the **physical** bytes the bucket holds under its
//! prefix.
//!
//! Every write here is a **SET**, never an increment. The number a
//! repository is worth is derived from its manifest after each write
//! and by the sweep (`Manifest::stored_bytes`), so a process that died
//! between a manifest CAS and the row it should have written leaves a
//! stale value that the next write or sweep replaces, not a drift that
//! compounds.
//!
//! The table's `private` column and `storage_daily` belong to the hosted
//! edition, which billed private storage; every repository here is
//! private, so every row is written with it set and nothing reads it.

use crate::db::ControlDb;

/// The owner kind of a repository's row.
pub const OWNER_REPO: &str = "repo";

/// SET the logical bytes of one owner, creating its row if this is the
/// first write.
pub fn set_logical(
    db: &ControlDb,
    owner_kind: &str,
    owner_id: &str,
    org_id: &str,
    bytes: u64,
    now: i64,
) -> Result<(), String> {
    db.lock()
        .execute(
            "INSERT INTO storage_usage \
                 (owner_kind, owner_id, org_id, private, logical_bytes, sampled_at) \
             VALUES ($1, $2, $3, TRUE, $4, $5) \
             ON CONFLICT (owner_kind, owner_id) DO UPDATE SET \
                 org_id = $3, logical_bytes = $4, sampled_at = $5",
            &[&owner_kind, &owner_id, &org_id, &clamp(bytes), &now],
        )
        .map(|_| ())
        .map_err(|e| format!("set logical bytes: {e}"))
}

/// SET the physical bytes of an owner that already has a row. An owner
/// with no row has no logical bytes either, and an inventory of it
/// would be an inventory of nothing; the sweep refreshes logical bytes
/// before it inventories, so the row exists by then.
pub fn set_physical(
    db: &ControlDb,
    owner_kind: &str,
    owner_id: &str,
    bytes: u64,
    now: i64,
) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE storage_usage SET physical_bytes = $3, inventoried_at = $4 \
             WHERE owner_kind = $1 AND owner_id = $2",
            &[&owner_kind, &owner_id, &clamp(bytes), &now],
        )
        .map(|_| ())
        .map_err(|e| format!("set physical bytes: {e}"))
}

/// Forget an owner. Deleting a repository stops counting it at once, even
/// though its objects are swept from the bucket later.
pub fn remove(db: &ControlDb, owner_kind: &str, owner_id: &str) -> Result<(), String> {
    db.lock()
        .execute(
            "DELETE FROM storage_usage WHERE owner_kind = $1 AND owner_id = $2",
            &[&owner_kind, &owner_id],
        )
        .map(|_| ())
        .map_err(|e| format!("remove storage row: {e}"))
}

/// Forget every repository row whose repository is no longer active.
/// The delete handlers call [`remove`] as they tombstone; this is the
/// sweep's floor under them, so nothing deleted by another path is
/// counted forever. Returns how many rows went.
pub fn prune_missing_repos(db: &ControlDb) -> Result<u64, String> {
    db.lock()
        .execute(
            "DELETE FROM storage_usage WHERE owner_kind = 'repo' AND owner_id NOT IN \
                 (SELECT id FROM repos WHERE state = 'active')",
            &[],
        )
        .map_err(|e| format!("prune storage rows: {e}"))
}

/// One owner's `(logical, physical)` bytes; `None` for an owner never
/// written to. Physical is `None` until an inventory has run.
pub fn owner_bytes(
    db: &ControlDb,
    owner_kind: &str,
    owner_id: &str,
) -> Result<Option<(i64, Option<i64>)>, String> {
    db.lock()
        .query_opt(
            "SELECT logical_bytes, physical_bytes FROM storage_usage \
             WHERE owner_kind = $1 AND owner_id = $2",
            &[&owner_kind, &owner_id],
        )
        .map(|r| r.map(|r| (r.get(0), r.get(1))))
        .map_err(|e| format!("owner bytes: {e}"))
}

/// BIGINT columns: a byte count past `i64::MAX` is not a repository
/// anybody has, but a cast that wraps would make it a negative one.
fn clamp(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry;

    fn org(db: &ControlDb, name: &str) -> String {
        registry::create_org(db, name).unwrap().id
    }

    #[test]
    fn writes_are_sets_and_a_removed_owner_is_forgotten() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("storage-set")).unwrap();
        let acme = org(&db, "acme");
        let other = org(&db, "other");
        assert_eq!(owner_bytes(&db, OWNER_REPO, "r1").unwrap(), None);

        set_logical(&db, OWNER_REPO, "r1", &acme, 1_000, 10).unwrap();
        set_logical(&db, OWNER_REPO, "r1", &acme, 700, 11).unwrap();
        set_logical(&db, OWNER_REPO, "r2", &acme, 50, 12).unwrap();
        set_logical(&db, OWNER_REPO, "r3", &other, 9_999, 13).unwrap();
        assert_eq!(
            owner_bytes(&db, OWNER_REPO, "r1").unwrap(),
            Some((700, None)),
            "the second write replaced the first, not added to it"
        );
        // No row, no error: a never-written repository holds nothing.
        set_physical(&db, OWNER_REPO, "never", 5, 1).unwrap();
        assert_eq!(owner_bytes(&db, OWNER_REPO, "never").unwrap(), None);

        set_physical(&db, OWNER_REPO, "r1", 1_400, 20).unwrap();
        assert_eq!(
            owner_bytes(&db, OWNER_REPO, "r1").unwrap(),
            Some((700, Some(1_400)))
        );
        // Keyed by kind as well as id: another kind's row with the same
        // id is a different row. (`package` is the one other kind the
        // table's CHECK admits — the hosted edition's registry.)
        set_logical(&db, "package", "r1", &acme, 3, 21).unwrap();
        assert_eq!(owner_bytes(&db, "package", "r1").unwrap(), Some((3, None)));

        remove(&db, OWNER_REPO, "r1").unwrap();
        assert_eq!(owner_bytes(&db, OWNER_REPO, "r1").unwrap(), None);
        assert_eq!(
            owner_bytes(&db, "package", "r1").unwrap(),
            Some((3, None)),
            "removing a repository took another kind's row with it"
        );
        remove(&db, OWNER_REPO, "r1").unwrap();
        // The floor under the delete handlers: a row whose repository
        // is gone, or never was, goes with the next prune; a live one
        // and another kind's row stay.
        let live = registry::create_repo(
            &db,
            &acme,
            &registry::NewRepo {
                description: None,
                name: "live",
                kind: registry::RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        set_logical(&db, OWNER_REPO, &live.id, &acme, 9, 40).unwrap();
        assert_eq!(
            prune_missing_repos(&db).unwrap(),
            2,
            "r2 and r3 had no repository"
        );
        assert_eq!(
            owner_bytes(&db, OWNER_REPO, &live.id).unwrap(),
            Some((9, None))
        );
        assert_eq!(owner_bytes(&db, "package", "r1").unwrap(), Some((3, None)));
        assert!(registry::delete_repo(&db, &acme, &live.id).unwrap());
        assert_eq!(prune_missing_repos(&db).unwrap(), 1);
        assert_eq!(owner_bytes(&db, OWNER_REPO, &live.id).unwrap(), None);
        // Bytes past BIGINT clamp rather than wrap negative.
        set_logical(&db, OWNER_REPO, "huge", &acme, u64::MAX, 30).unwrap();
        assert_eq!(
            owner_bytes(&db, OWNER_REPO, "huge").unwrap(),
            Some((i64::MAX, None))
        );
    }
}
