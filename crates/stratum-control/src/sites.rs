//! What a repository publishes as a static site, and what it has
//! published.
//!
//! A **deploy** is a pointer, not a copy. For a site that needs no
//! build it is the commit somebody pushed and the tree of the directory
//! their config named, and every blob under that tree is already in the
//! repository's store. Publishing therefore writes one row and moves
//! one column, which is why it is instant and why it costs no storage
//! anybody has to be billed for twice.
//!
//! A **site** is at most one per repository, keyed by `repo_id` for that
//! reason rather than carrying an id of its own. Its `host` is the DNS
//! label it is served at, derived once when the site is created and
//! thereafter authoritative: every request is an exact match against
//! this column, never a parse of a hostname back into an org and a
//! repository. The derivation is lossy — our names admit characters DNS
//! does not — so the unique index here is what actually decides between
//! two repositories that want the same label.
//!
//! `current` is the deploy being served, and it is a column rather than
//! "the newest row" so that a publish is a single atomic move and a
//! rollback is the same move backwards.

use crate::ids::{now_ms, ulid};
use crate::ControlDb;

/// One published deploy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deploy {
    pub id: String,
    pub repo_id: String,
    /// The commit this was published from.
    pub commit_oid: String,
    /// The tree of the published directory — the root of what is served.
    pub tree_oid: String,
    /// The directory the config named, kept for display. Serving uses
    /// `tree_oid`, which already points inside it.
    pub publish: String,
    /// Serve `index.html` for an unmatched path.
    pub spa: bool,
    /// The page for an unmatched path when `spa` is false.
    pub not_found: Option<String>,
    pub created_at: i64,
    /// Insertion order. History is ordered by this and never by
    /// `created_at`, which is a millisecond from whichever node handled
    /// the push and so is neither unique nor, across a fleet with
    /// skewed clocks, reliably ordered.
    pub seq: i64,
}

/// A repository's site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub repo_id: String,
    /// The DNS label, unique across the fleet.
    pub host: String,
    /// The ref whose pushes publish. `None` means the repository's
    /// default branch, resolved when a push arrives rather than stored,
    /// so that changing the default branch does not silently stop
    /// publishing.
    pub branch: Option<String>,
    /// The deploy being served. `None` when the site exists but nothing
    /// has published yet, which is the state between a config landing
    /// and the first successful publish.
    pub current: Option<String>,
    pub created_at: i64,
}

fn site_from(r: &postgres::Row) -> Site {
    Site {
        repo_id: r.get(0),
        host: r.get(1),
        branch: r.get(2),
        current: r.get(3),
        created_at: r.get(4),
    }
}

const SITE_COLS: &str = "repo_id, host, branch, current, created_at";

/// The site for `repo_id`, if it has one.
pub fn get(db: &ControlDb, repo_id: &str) -> Result<Option<Site>, String> {
    db.lock()
        .query_opt(
            &format!("SELECT {SITE_COLS} FROM sites WHERE repo_id = $1"),
            &[&repo_id],
        )
        .map(|row| row.as_ref().map(site_from))
        .map_err(|e| e.to_string())
}

/// The site served at `host`.
///
/// This is the request path, so it is one indexed equality and nothing
/// else. `host` is stored lowercase and the caller lowercases what it
/// read off the wire, because DNS is case-insensitive and a visitor who
/// typed capitals must not get a 404.
pub fn by_host(db: &ControlDb, host: &str) -> Result<Option<Site>, String> {
    db.lock()
        .query_opt(
            &format!("SELECT {SITE_COLS} FROM sites WHERE host = $1"),
            &[&host],
        )
        .map(|row| row.as_ref().map(site_from))
        .map_err(|e| e.to_string())
}

/// Is this label already taken?
///
/// Used by the caller that walks candidates. It is deliberately not the
/// only defence: the unique index is, because two repositories created
/// at once would both be told the label was free.
pub fn host_taken(db: &ControlDb, host: &str) -> Result<bool, String> {
    db.lock()
        .query_one(
            "SELECT COUNT(*)::BIGINT FROM sites WHERE host = $1",
            &[&host],
        )
        .map(|r| r.get::<_, i64>(0) > 0)
        .map_err(|e| e.to_string())
}

/// Create the site for `repo_id` at `host`, or return the one that is
/// already there.
///
/// Idempotent on the repository, because the caller is a push and a push
/// happens again. A conflict on `host` rather than on `repo_id` is the
/// collision case and is reported as an error so the caller can try its
/// next candidate.
pub fn create(
    db: &ControlDb,
    repo_id: &str,
    host: &str,
    branch: Option<&str>,
) -> Result<Site, String> {
    let site = Site {
        repo_id: repo_id.to_string(),
        host: host.to_string(),
        branch: branch.map(str::to_string),
        current: None,
        created_at: now_ms(),
    };
    let n = db
        .lock()
        .execute(
            "INSERT INTO sites (repo_id, host, branch, current, created_at) \
             VALUES ($1, $2, $3, NULL, $4) ON CONFLICT (repo_id) DO NOTHING",
            &[&site.repo_id, &site.host, &site.branch, &site.created_at],
        )
        .map_err(|e| e.to_string())?;
    if n == 0 {
        // Already existed. Return what is actually stored rather than
        // what we would have written, so the caller never acts on a
        // host that is not the one being served.
        return get(db, repo_id)?.ok_or_else(|| "site vanished during create".to_string());
    }
    Ok(site)
}

/// Point the site's `branch` at a new value, or at the default branch
/// when the config stopped naming one.
pub fn set_branch(db: &ControlDb, repo_id: &str, branch: Option<&str>) -> Result<(), String> {
    let b = branch.map(str::to_string);
    db.lock()
        .execute(
            "UPDATE sites SET branch = $2 WHERE repo_id = $1",
            &[&repo_id, &b],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Record a deploy. Does not publish it — see [`publish`].
///
/// Separating the two is what makes a failed publish harmless: a deploy
/// row nobody points at is invisible, and the site keeps serving what it
/// was serving.
#[allow(clippy::too_many_arguments)]
pub fn add_deploy(
    db: &ControlDb,
    repo_id: &str,
    commit_oid: &str,
    tree_oid: &str,
    publish: &str,
    spa: bool,
    not_found: Option<&str>,
) -> Result<Deploy, String> {
    let mut d = Deploy {
        id: ulid(),
        repo_id: repo_id.to_string(),
        commit_oid: commit_oid.to_string(),
        tree_oid: tree_oid.to_string(),
        publish: publish.to_string(),
        spa,
        not_found: not_found.map(str::to_string),
        created_at: now_ms(),
        // Replaced by what the sequence actually issued. Guessing it
        // here would be a number that disagrees with the row.
        seq: 0,
    };
    let row = db
        .lock()
        .query_one(
            "INSERT INTO site_deploys \
             (id, repo_id, commit_oid, tree_oid, publish, spa, not_found, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING seq",
            &[
                &d.id,
                &d.repo_id,
                &d.commit_oid,
                &d.tree_oid,
                &d.publish,
                &d.spa,
                &d.not_found,
                &d.created_at,
            ],
        )
        .map_err(|e| e.to_string())?;
    d.seq = row.get(0);
    Ok(d)
}

/// Serve `deploy_id` from now on.
///
/// One column move, so a visitor mid-request either gets the whole old
/// deploy or the whole new one. The `AND` on `repo_id` is not
/// decoration: it is what stops a deploy id from one repository being
/// published on another.
pub fn publish(db: &ControlDb, repo_id: &str, deploy_id: &str) -> Result<(), String> {
    let n = db
        .lock()
        .execute(
            "UPDATE sites SET current = $2 WHERE repo_id = $1 \
             AND EXISTS (SELECT 1 FROM site_deploys WHERE id = $2 AND repo_id = $1)",
            &[&repo_id, &deploy_id],
        )
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err(format!("no deploy {deploy_id} for repo {repo_id}"));
    }
    Ok(())
}

const DEPLOY_COLS: &str =
    "id, repo_id, commit_oid, tree_oid, publish, spa, not_found, created_at, seq";

fn deploy_from(r: &postgres::Row) -> Deploy {
    Deploy {
        id: r.get(0),
        repo_id: r.get(1),
        commit_oid: r.get(2),
        tree_oid: r.get(3),
        publish: r.get(4),
        spa: r.get(5),
        not_found: r.get(6),
        created_at: r.get(7),
        seq: r.get(8),
    }
}

/// One deploy by id.
pub fn deploy(db: &ControlDb, deploy_id: &str) -> Result<Option<Deploy>, String> {
    db.lock()
        .query_opt(
            &format!("SELECT {DEPLOY_COLS} FROM site_deploys WHERE id = $1"),
            &[&deploy_id],
        )
        .map(|row| row.as_ref().map(deploy_from))
        .map_err(|e| e.to_string())
}

/// A repository's deploys, newest first.
pub fn deploys(db: &ControlDb, repo_id: &str, limit: i64) -> Result<Vec<Deploy>, String> {
    db.lock()
        .query(
            &format!(
                "SELECT {DEPLOY_COLS} FROM site_deploys WHERE repo_id = $1 \
                 ORDER BY seq DESC LIMIT $2"
            ),
            &[&repo_id, &limit],
        )
        .map(|rows| rows.iter().map(deploy_from).collect())
        .map_err(|e| e.to_string())
}

/// Forget this repository's site entirely. The deploys go with it by
/// cascade, and `current` is dropped first so the cascade has nothing
/// pointing back at it.
pub fn remove(db: &ControlDb, repo_id: &str) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE sites SET current = NULL WHERE repo_id = $1",
            &[&repo_id],
        )
        .map_err(|e| e.to_string())?;
    db.lock()
        .execute("DELETE FROM site_deploys WHERE repo_id = $1", &[&repo_id])
        .map_err(|e| e.to_string())?;
    db.lock()
        .execute("DELETE FROM sites WHERE repo_id = $1", &[&repo_id])
        .map(|_| ())
        .map_err(|e| e.to_string())
}
