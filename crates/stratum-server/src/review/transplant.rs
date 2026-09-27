//! Move a fork's objects into the repository a change targets, so that
//! landing can point a ref at them.
//!
//! **The problem, stated plainly.** A fork is zero-copy: it shares
//! upstream's data prefix for everything that existed when it was made,
//! and its own new commits land as new segments in its *own* prefix.
//! `refops::transact` writes refs in the target's prefix. So a change
//! opened from a fork is a ref update whose commit is not in the plane
//! the ref lives in — the CAS would succeed and produce a repository
//! that fails `git fsck` on the next clone. I11 is not negotiable, so
//! the objects move first.
//!
//! **Why this reuses `mirror::sync::incremental_sync` rather than
//! reimplementing it.** That function is already exactly this operation:
//! a thin pack of new objects, a WAL append, a manifest CAS. The plan
//! for this work said to factor it and share it rather than copy it, and
//! a second copy is how two paths drift on the first edge case somebody
//! fixes in only one of them.
//!
//! **Why the objects do not need re-verifying (I12).** They are already
//! in our store because somebody pushed them to the fork, and that push
//! went through `receive_pack`'s quarantine, hash verification,
//! `index-pack --fix-thin`, and the connectivity and fast-forward BFS.
//! This is not a wire push from a stranger; it is a move between two
//! planes we already own. What is *not* skipped is the fast-forward
//! proof against the target's tip — the lander still does that, after
//! this, and refuses exactly as it always did.
//!
//! **Cost.** Materialising both planes is O(repo), paid once per land
//! rather than per push. That is the same trade the fork-promotion path
//! took, and for the same reason: correct and expensive beats clever and
//! conditionally wrong when being wrong means a repository that does not
//! `fsck`.

use crate::app::SharedState;
use std::path::Path;
use stratum_control::registry::Repo;
use stratum_engine::gitcmd::{git, run};
use stratum_store::manifest::Manifest;
use stratum_store::{LatencyModel, ObjectStore};

/// The ref a transplanted commit is parked on inside the target.
///
/// It exists so the objects are **reachable**, and therefore not
/// collectable, between the transplant and the land — and it stays
/// afterwards so that a change which is approved, transplanted, and then
/// ejected does not leave unreferenced objects for the sweeper to take
/// while the contributor is still working. `refs/staged/*` is outside
/// `refs/heads/*`, so it is not a branch, does not appear in the branch
/// list, and no protection rule applies to it.
pub fn staged_ref(change_key: &str) -> String {
    format!("refs/staged/{change_key}")
}

/// Bring `commit` and its ancestors from `source` into `target`'s plane.
///
/// Idempotent: if the commit already resolves in the target — because a
/// previous attempt got this far, or because the contributor also has
/// push access and pushed it directly — this does nothing and says so.
pub fn transplant_blocking(
    store_url: &str,
    data_dir: &Path,
    source: &Repo,
    target: &Repo,
    commit: &str,
    change_key: &str,
) -> Result<bool, String> {
    let store = ObjectStore::new(store_url, LatencyModel::None);
    let target_prefix = target.prefix().as_str().to_string();
    let source_prefix = source.prefix().as_str().to_string();

    let work = data_dir.join("staging").join("transplant").join(change_key);
    // A previous attempt that died mid-way leaves a directory behind;
    // starting from whatever it contained would be starting from an
    // unknown state.
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("transplant workdir: {e}"))?;

    let result = (|| -> Result<bool, String> {
        let target_dir = work.join("target");
        let source_dir = work.join("source");
        stratum_engine::materialize::materialize(&store, &target_prefix, &target_dir)?;

        // Already here? Then there is nothing to move, and saying so is
        // better than doing the work again.
        if run(git(&target_dir).args(["cat-file", "-e", commit])).is_ok() {
            return Ok(false);
        }

        stratum_engine::materialize::materialize(&store, &source_prefix, &source_dir)?;
        // Fetch by oid rather than by ref: the contributor's branch name
        // is theirs to move, and the change is pinned to a commit.
        run(git(&target_dir).args([
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            &source_dir.to_string_lossy(),
            commit,
        ]))
        .map_err(|e| format!("fetch {commit} from fork: {e}"))?;
        run(git(&target_dir).args(["cat-file", "-e", commit]))
            .map_err(|_| format!("fork does not contain {commit} after fetch"))?;

        let manifest_key = format!("{target_prefix}/manifest.json");
        let mbytes = store.get(&manifest_key)?;
        let manifest: Manifest =
            serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;
        let mut old_refs: Vec<(String, String)> = manifest.refs.clone();
        for page in &manifest.ref_pages {
            old_refs.extend(stratum_store::refpages::load_page(&store, page)?);
        }
        old_refs.sort();
        old_refs.dedup();

        let staged = staged_ref(change_key);
        let mut new_refs = old_refs.clone();
        new_refs.retain(|(n, _)| n != &staged);
        new_refs.push((staged.clone(), commit.to_string()));
        new_refs.sort();
        new_refs.dedup();
        // The ref has to exist in the materialised repo too, or the
        // pack-building walk has nothing telling it these objects are
        // wanted.
        run(git(&target_dir).args(["update-ref", &staged, commit]))
            .map_err(|e| format!("stage {staged}: {e}"))?;

        crate::mirror::sync::incremental_sync(
            &store,
            &target_dir,
            &target_prefix,
            &manifest_key,
            // Staging a fork's commits must not repoint the target's
            // HEAD; only a mirror sync knows the origin's default.
            None,
            old_refs,
            new_refs,
        )?;
        Ok(true)
    })();

    let _ = std::fs::remove_dir_all(&work);
    result
}

/// The async wrapper the lander calls. `None` source is not a fork
/// change and is a no-op, so the caller does not branch.
pub async fn ensure_present(
    state: &SharedState,
    source: Option<Repo>,
    target: &Repo,
    commit: &str,
    change_key: &str,
) -> Result<bool, String> {
    let Some(source) = source else {
        return Ok(false);
    };
    let store_url = state.store_url.clone();
    let data_dir = state.data_dir.clone();
    let target = target.clone();
    let commit = commit.to_string();
    let change_key = change_key.to_string();
    tokio::task::spawn_blocking(move || {
        transplant_blocking(
            &store_url,
            &data_dir,
            &source,
            &target,
            &commit,
            &change_key,
        )
    })
    .await
    .map_err(|e| format!("join: {e}"))?
}
