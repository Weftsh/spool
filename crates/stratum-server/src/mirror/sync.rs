//! Mirror sync: origin → seed clone → Stratum layout.
//!
//! Initial sync is a full ingest (cold/hot/snapshot/locator, published
//! create-or-replace). Incremental sync is deliberately push-shaped: a thin
//! pack of the new objects lands as a WAL entry plus a manifest CAS — the
//! same write path a `git push` takes — which is what makes
//! webhook-to-serving p50 < 10 s realistic (M1). The compactor folds WAL
//! growth later; serving is correct the moment the CAS lands.
//!
//! Duplicate discipline (I5): `rev-list new ^old` over-includes
//! re-introduced objects (the same quirk the research ingest fights), so
//! candidates already present in the layout (WAL sidecars ∪ locator) are
//! excluded with `^oid` before packing. A concat stream must never carry
//! an object twice.

use crate::mirror::forward;
use crate::mirror::origin::{GithubApp, OriginProvider};
use sha1::Digest;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use stratum_control::jobs;
use stratum_control::registry::{self, Repo};
use stratum_control::ControlDb;
use stratum_engine::gitcmd::{git, run, run_str, run_with_stdin};
use stratum_engine::ingest::{publish, PublishMode};
use stratum_proto::receive::Update;
use stratum_store::manifest::{Manifest, WalEntry};
use stratum_store::{LatencyModel, ObjectStore, Plane, PutCond, PutError};

const ZERO: &str = "0000000000000000000000000000000000000000";

const CAS_RETRIES: usize = 10;

#[derive(Debug, Clone, PartialEq)]
pub enum SyncOutcome {
    NoChange,
    /// Another node was syncing this mirror when this one arrived, and
    /// its result is the result. Nothing was fetched, nothing recorded.
    Elsewhere,
    Updated,
    InitialIngested,
    Reingested,
}

pub struct SyncManager {
    pub db: ControlDb,
    pub store_url: String,
    pub data_dir: PathBuf,
    providers: HashMap<String, Arc<dyn OriginProvider>>,
    /// Per-repo serialization + coalescing: webhook storms and freshness
    /// misses share one in-flight sync.
    locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The same GitHub App that is in `providers`, kept concretely as
    /// well. Syncing only ever needs the trait; *discovery* — listing
    /// installations and the repositories one can read — is not part of
    /// fetching a mirror and does not belong on `OriginProvider`, which
    /// every generic origin would then have to answer for.
    github: Option<Arc<GithubApp>>,
}

impl SyncManager {
    pub fn new(
        db: ControlDb,
        store_url: String,
        data_dir: PathBuf,
        providers: HashMap<String, Arc<dyn OriginProvider>>,
    ) -> SyncManager {
        SyncManager {
            db,
            store_url,
            data_dir,
            providers,
            locks: std::sync::Mutex::new(HashMap::new()),
            github: None,
        }
    }

    /// Register the GitHub App for discovery as well as fetching.
    pub fn with_github(mut self, app: Arc<GithubApp>) -> SyncManager {
        self.github = Some(app);
        self
    }

    /// The GitHub App, when this server has one configured.
    pub fn github_app(&self) -> Option<Arc<GithubApp>> {
        self.github.clone()
    }

    /// Whether fetching this origin stays on this machine.
    ///
    /// Creation checks a pasted origin before accepting it, and that
    /// check is a real `git ls-remote` to a real host — which is right
    /// for somebody pasting a GitHub URL into a form, and wrong for a
    /// hermetic suite or an operator-configured `file://` origin, where
    /// there is no network to ask and nothing a stranger supplied.
    ///
    /// This is the seam that tells the two apart, rather than the
    /// creation path guessing from the shape of a URL.
    pub fn origin_is_local(&self, provider: &str, origin: &str) -> bool {
        if origin.starts_with("file://") {
            return true;
        }
        match provider {
            "github" => self
                .github
                .as_ref()
                .is_some_and(|a| a.git_base.starts_with("file://")),
            _ => false,
        }
    }

    pub fn provider_by_name(&self, name: &str) -> Option<Arc<dyn OriginProvider>> {
        self.providers.get(name).cloned()
    }

    pub fn provider_for(&self, repo: &Repo) -> Result<Arc<dyn OriginProvider>, String> {
        let name = repo.origin_provider.as_deref().unwrap_or("generic");
        self.providers
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no origin provider {name:?} configured"))
    }

    /// Sync one mirror now. Serialized per repo; the sync body runs on the
    /// blocking pool. Sync state (last_sync_at / sync_error) is recorded
    /// win or lose — staleness headers are derived from it (M2).
    pub async fn sync(&self, repo: &Repo) -> Result<SyncOutcome, String> {
        let lock = {
            let mut map = self.locks.lock().unwrap();
            map.entry(repo.id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        let provider = self.provider_for(repo)?;
        let db = self.db.clone();
        let store_url = self.store_url.clone();
        let data_dir = self.data_dir.clone();
        let repo = repo.clone();
        tokio::task::spawn_blocking(move || {
            // One node at a time, fleet-wide. The mutex above serializes
            // this process; the advisory lock serializes the fleet. On
            // two nodes the poll ticks line up, and the second sync of
            // the same mirror used to fetch the origin again, rebuild
            // the same layout, lose the manifest swap and log "manifest
            // swap lost a race (concurrent writer)" — a failure for work
            // the other node had already done. Held elsewhere means the
            // sync is happening; this one has nothing to add.
            let _fleet = match jobs::try_lock_scoped(&db, "mirror-sync", &repo.id) {
                Ok(Some(guard)) => guard,
                Ok(None) => return Ok(SyncOutcome::Elsewhere),
                Err(e) => return Err(format!("sync lock: {e}")),
            };
            let out = sync_blocking(&db, provider.as_ref(), &store_url, &data_dir, &repo);
            record_outcome(&db, &store_url, &data_dir, &repo, &out);
            out
        })
        .await
        .unwrap_or_else(|e| Err(format!("sync task join: {e}")))
    }

    /// Whether a push to this mirror could reach its origin at all.
    ///
    /// Said at the advert, before the client packs anything: the answer
    /// is a property of how the mirror was registered, not of the push.
    pub fn push_credential(&self, repo: &Repo) -> Result<(), forward::Refusal> {
        let provider = self
            .provider_for(repo)
            .map_err(forward::Refusal::Unreachable)?;
        forward::credential_check(provider.as_ref(), repo)
    }

    /// Forward a push to the mirror's origin, then reflect the result.
    ///
    /// Under the same per-repo mutex and the same fleet lock as
    /// [`sync`](Self::sync), because the promise to the client is that
    /// the origin took it *and the mirror shows it*, and the second
    /// half needs the lock. Unlike a sync, a forward that finds the
    /// lock held waits for it: the holder is usually the webhook for a
    /// push that just landed, and walking away would answer `ok` for a
    /// ref the manifest does not carry yet.
    ///
    /// The steps, and why in this order: the storage cap and the local
    /// guards first, because the origin must never take a push we then
    /// refuse to store; the credential's permission next, cheaply and
    /// hermetically, before a pack is indexed; the seed fetched, so a
    /// thin pack has its bases; the push; and only then the seed
    /// re-fetched and reflected into the layout, exactly as a sync
    /// would. A stale lease means the mirror lagged its origin — the
    /// reflect still runs, so the mirror catches up, and the client is
    /// told to fetch and push again.
    pub async fn forward(
        &self,
        repo: &Repo,
        updates: Vec<Update>,
        pack: Option<Vec<u8>>,
        lock_wait: std::time::Duration,
    ) -> Result<SyncOutcome, forward::Refusal> {
        use forward::Refusal;
        let lock = {
            let mut map = self.locks.lock().unwrap();
            map.entry(repo.id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        let provider = self.provider_for(repo).map_err(Refusal::Unreachable)?;
        forward::credential_check(provider.as_ref(), repo)?;
        forward::local_guards(repo, &updates)?;
        let db = self.db.clone();
        let store_url = self.store_url.clone();
        let data_dir = self.data_dir.clone();
        let github = self.github.clone();
        let repo = repo.clone();
        tokio::task::spawn_blocking(move || {
            let _fleet = match jobs::lock_scoped_wait(&db, "mirror-sync", &repo.id, lock_wait) {
                Ok(Some(guard)) => guard,
                Ok(None) => {
                    return Err(Refusal::Busy(
                        "another node is syncing this mirror; try again".into(),
                    ))
                }
                Err(e) => return Err(Refusal::Unreachable(format!("sync lock: {e}"))),
            };
            // The permission, asked of the API rather than discovered
            // from a refused push: this is the leg a hermetic suite can
            // drive, and the one an operator meets — every installation
            // made before `Contents: write` was on the App lacks it.
            if let (Some(app), Some(inst), Some("github")) = (
                github.as_ref(),
                repo.origin_installation.as_deref(),
                repo.origin_provider.as_deref(),
            ) {
                match app.installation(inst) {
                    Ok(Some(detail)) if !detail.contents_write => {
                        return Err(Refusal::PermissionDenied(
                            forward::CONTENTS_WRITE_DENIED.into(),
                        ))
                    }
                    Ok(_) => {}
                    Err(e) => return Err(Refusal::Unreachable(e)),
                }
            }
            let seed =
                refresh_seed(provider.as_ref(), &data_dir, &repo).map_err(Refusal::Unreachable)?;
            let pushed = forward::push_to_origin(&seed, &updates, pack.as_deref());
            match pushed {
                Ok(()) | Err(Refusal::Behind(_)) => {}
                Err(other) => return Err(other),
            }
            // The origin moved (or had moved): the seed follows it and
            // the layout follows the seed, exactly as a sync would.
            let out = fetch_seed(&seed)
                .and_then(|_| reflect_seed(&db, &store_url, &data_dir, &repo, &seed));
            record_outcome(&db, &store_url, &data_dir, &repo, &out);
            match (pushed, out) {
                (Err(behind), _) => Err(behind),
                (Ok(()), Ok(outcome)) => Ok(outcome),
                // The origin took the push; only the mirror's own catch-up
                // failed. Say exactly that — "origin unreachable" would be
                // a lie about a commit the origin already holds, and would
                // send the person to re-push what landed.
                (Ok(()), Err(e)) => Err(Refusal::Unreachable(format!(
                    "the origin accepted the push, but the mirror could not reflect it yet: {e}"
                ))),
            }
        })
        .await
        .unwrap_or_else(|e| Err(Refusal::Unreachable(format!("forward task join: {e}"))))
    }
}

/// What a sync leaves behind, win or lose: the row's sync state, and
/// the work the moved layout now needs. Shared with a forwarded push,
/// which is a sync with a push in the middle.
fn record_outcome(
    db: &ControlDb,
    store_url: &str,
    data_dir: &Path,
    repo: &Repo,
    out: &Result<SyncOutcome, String>,
) {
    // A mirror's Actions runs come through the App installation
    // it syncs with, and nothing else ever asks for them: the
    // first real mirror on weft.sh sat on "first poll has not
    // finished" for hours, and a completed walk never re-armed,
    // so the runs of every later push would have stayed on
    // GitHub. Win or lose — a fetch that failed says nothing
    // about the API — and deduplicated while a poll is active.
    if repo.origin_provider.as_deref() == Some("github") && repo.origin_installation.is_some() {
        if let Err(e) = jobs::enqueue_unique(
            db,
            &repo.org_id,
            &repo.id,
            crate::workers::checks_poll::JOB_KIND,
            None,
        ) {
            eprintln!("weft: checks poll enqueue after sync: {e}");
        }
    }
    match out {
        Ok(outcome) => {
            let tip = seed_head_tip(&seed_dir(data_dir, &repo.id));
            let _ = registry::set_sync_state(db, &repo.id, tip.as_deref(), None);
            // The layout moved (or was confirmed): the row follows
            // the manifest, like every other write.
            if let Err(e) = crate::storage::refresh_repo_with(db, store_url, repo) {
                eprintln!("weft: storage refresh after sync {}: {e}", repo.id);
            }
            // The CDN pack was only ever built after a *push*,
            // so a mirror — where nobody pushes — never had one
            // and every clone took the inline path. Same rule as
            // a push: the layout moved, so the pack is stale.
            if !matches!(outcome, SyncOutcome::NoChange) {
                if let Err(e) = jobs::enqueue_unique(db, &repo.org_id, &repo.id, "cdnpack", None) {
                    eprintln!("weft: cdnpack enqueue after sync: {e}");
                }
                // And the site, for the same reason and with the
                // same history behind it: a mirrored repository
                // whose site never updated would be this exact
                // bug a third time. A push reaches this through
                // `workflow::trigger::on_push`; a sync has no
                // state handle to call a handler with, which is
                // why publishing is a queued job at all.
                if let Err(e) =
                    jobs::enqueue_unique(db, &repo.org_id, &repo.id, "sitepublish", None)
                {
                    eprintln!("weft: sitepublish enqueue after sync: {e}");
                }
                // A sync appends a WAL entry exactly as a push
                // does, and only a push ever asked for the fold.
                // The first real mirror on weft.sh reached 102
                // WAL entries against a threshold of 8, and every
                // read of it paid for all 102 before finding a
                // single object — 5 to 11 s per request where a
                // pushed repository answered in 0.4 s.
                if let Err(e) = jobs::enqueue_unique(db, &repo.org_id, &repo.id, "compact", None) {
                    eprintln!("weft: compact enqueue after sync: {e}");
                }
            }
            // The migration case, and the one the contribution
            // graph exists for: a decade of somebody's history
            // arrives here by mirroring, not by pushing. There
            // is no pusher — nobody here did this — so only a
            // *proved* address can claim any of it, which is
            // exactly the right answer for commits that came
            // from somewhere else.
            if let Err(e) = stratum_control::contribs::enqueue(db, &repo.org_id, &repo.id, None) {
                eprintln!("weft: contrib enqueue after sync: {e}");
            }
        }
        Err(e) => {
            let _ = registry::set_sync_state(db, &repo.id, None, Some(e));
        }
    }
}

fn seed_dir(data_dir: &Path, repo_id: &str) -> PathBuf {
    data_dir.join("mirrors").join(format!("{repo_id}.git"))
}

fn seed_head_tip(seed: &Path) -> Option<String> {
    run_str(git(seed).args(["rev-parse", "HEAD"])).ok()
}

fn sync_blocking(
    db: &ControlDb,
    provider: &dyn OriginProvider,
    store_url: &str,
    data_dir: &Path,
    repo: &Repo,
) -> Result<SyncOutcome, String> {
    let seed = refresh_seed(provider, data_dir, repo)?;
    reflect_seed(db, store_url, data_dir, repo, &seed)
}

/// The seed clone, fetched: origin → seed. The first half of a sync,
/// and the half a forwarded push needs *before* it pushes — the thin
/// pack the client sent is fixed against the seed's objects, and a
/// node that has never synced this mirror has no seed at all.
fn refresh_seed(
    provider: &dyn OriginProvider,
    data_dir: &Path,
    repo: &Repo,
) -> Result<PathBuf, String> {
    let fetch_url = provider.fetch_url(repo)?;
    let seed = seed_dir(data_dir, &repo.id);
    let fresh_seed = !seed.exists();
    if fresh_seed {
        std::fs::create_dir_all(&seed).map_err(|e| e.to_string())?;
        run(git(&seed).args(["init", "-q", "--bare", "."]))?;
        run(git(&seed).args(["remote", "add", "origin", &fetch_url]))?;
        // Heads and tags only — never refs/pull/* and friends; mirror ref
        // counts should track the origin's real surface.
        run(git(&seed).args([
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/heads/*",
        ]))?;
        run(git(&seed).args([
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/tags/*:refs/tags/*",
        ]))?;
    } else {
        // Credentials in the URL are short-lived; refresh every sync.
        run(git(&seed).args(["remote", "set-url", "origin", &fetch_url]))?;
    }
    // --update-shallow: a shallow origin's graft roots are accepted into
    // the seed (and its shallow file), so shallow upstreams mirror too.
    fetch_seed(&seed)?;
    Ok(seed)
}

fn fetch_seed(seed: &Path) -> Result<(), String> {
    run(git(seed).args([
        "fetch",
        "--prune",
        "--no-tags",
        "--update-shallow",
        "-q",
        "origin",
    ]))
    .map(|_| ())
    .map_err(|e| format!("origin unreachable: {e}"))
}

/// Seed → layout: the second half of a sync. Reads the origin's HEAD,
/// diffs the seed's refs against the manifest, and lands the
/// difference as a WAL entry or a full ingest.
fn reflect_seed(
    db: &ControlDb,
    store_url: &str,
    data_dir: &Path,
    repo: &Repo,
    seed: &Path,
) -> Result<SyncOutcome, String> {
    let seed = seed.to_path_buf();
    // Origin's default branch (mirror HEAD follows it).
    let head_branch = match run_str(git(&seed).args(["ls-remote", "--symref", "origin", "HEAD"])) {
        Ok(out) => out
            .lines()
            .find_map(|l| {
                l.strip_prefix("ref: refs/heads/")
                    .and_then(|r| r.split_whitespace().next())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| repo.default_branch.clone()),
        Err(_) => repo.default_branch.clone(),
    };
    run(git(&seed).args(["symbolic-ref", "HEAD", &format!("refs/heads/{head_branch}")]))?;
    if head_branch != repo.default_branch {
        registry::set_default_branch(db, &repo.id, &head_branch)?;
    }

    let store = ObjectStore::new(store_url, LatencyModel::None);
    let prefix = repo.prefix().as_str().to_string();
    let manifest_key = format!("{prefix}/manifest.json");

    let current: Option<(Manifest, String)> = match store.get_with_etag(&manifest_key) {
        Ok((bytes, etag)) => Some((
            serde_json::from_slice(&bytes).map_err(|e| format!("manifest: {e}"))?,
            etag,
        )),
        Err(e) if stratum_engine::errclass::is_absent(&e) => None,
        Err(e) => return Err(e),
    };

    // New ref state from the seed.
    let listed = run_str(git(&seed).args([
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        "refs/heads",
        "refs/tags",
    ]))?;
    let mut new_refs: Vec<(String, String)> = listed
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(n, o)| (n.to_string(), o.to_string()))
        .collect();
    new_refs.sort();
    if new_refs.is_empty() {
        return Err("origin has no refs to mirror".into());
    }
    if !new_refs
        .iter()
        .any(|(n, _)| n == &format!("refs/heads/{head_branch}"))
    {
        return Err(format!(
            "origin HEAD branch {head_branch:?} not among fetched refs"
        ));
    }

    let Some((manifest, etag)) = current else {
        return full_ingest(&store, &seed, &prefix, &head_branch, data_dir, repo, None)
            .map(|_| SyncOutcome::InitialIngested);
    };
    // A just-registered mirror has an empty placeholder manifest; the
    // first sync replaces it wholesale.
    if manifest.cold_segments.is_empty() && manifest.spine.is_empty() && manifest.wal.is_empty() {
        return full_ingest(
            &store,
            &seed,
            &prefix,
            &head_branch,
            data_dir,
            repo,
            Some(etag),
        )
        .map(|_| SyncOutcome::InitialIngested);
    }

    // Old ref state: the manifest (plus pages when sharded).
    let mut old_refs: Vec<(String, String)> = manifest.refs.clone();
    for page in &manifest.ref_pages {
        old_refs.extend(stratum_store::refpages::load_page(&store, page)?);
    }
    old_refs.sort();
    old_refs.dedup();

    if old_refs == new_refs {
        return Ok(SyncOutcome::NoChange);
    }

    // Paged ref stores don't support removals in v1; a deletion-bearing
    // sync on a paged mirror falls back to a full re-ingest (correct,
    // costlier, rare).
    let old_names: HashSet<&String> = old_refs.iter().map(|(n, _)| n).collect();
    let new_names: HashSet<&String> = new_refs.iter().map(|(n, _)| n).collect();
    let has_deletions = old_names.difference(&new_names).next().is_some();
    if has_deletions && !manifest.ref_pages.is_empty() {
        return full_ingest(
            &store,
            &seed,
            &prefix,
            &head_branch,
            data_dir,
            repo,
            Some(etag),
        )
        .map(|_| SyncOutcome::Reingested);
    }

    incremental_sync(
        &store,
        &seed,
        &prefix,
        &manifest_key,
        Some(head_branch.as_str()),
        old_refs,
        new_refs,
    )
    .map(|_| SyncOutcome::Updated)
}

fn full_ingest(
    store: &ObjectStore,
    seed: &Path,
    prefix: &str,
    head_branch: &str,
    data_dir: &Path,
    repo: &Repo,
    etag: Option<String>,
) -> Result<(), String> {
    let staging_root = data_dir.join("staging").join(&repo.id);
    let cfg = crate::app::ingest_config_from_env();
    let mut out = stratum_engine::ingest(seed, prefix, head_branch, &cfg, &staging_root)?;
    let hdr = stratum_engine::build_locator(seed, &mut out, prefix, 0)?;
    let mode = match etag {
        None => PublishMode::Create,
        Some(_) => PublishMode::Replace,
    };
    let result = publish(store, prefix, &out, &hdr, mode);
    let _ = std::fs::remove_dir_all(&staging_root);
    result.map_err(|e| e.to_string())
}

/// The push-shaped incremental path: thin pack of new objects → WAL append
/// → manifest CAS, refs rewritten to origin truth (mirror sync is
/// authoritative — force pushes and deletions at origin are mirrored, not
/// policed; the wire-push fast-forward policy is for tenants, not us).
/// `pub(crate)` so the review path can reuse it.
///
/// Landing a change whose commits live in a **fork** has to get those
/// objects into the target's plane before a ref can point at them, and
/// that is precisely this: thin pack of new objects, WAL append,
/// manifest CAS. Writing a second one next door is how the two would
/// drift on the first edge case fixed in only one of them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn incremental_sync(
    store: &ObjectStore,
    seed: &Path,
    prefix: &str,
    manifest_key: &str,
    // The origin's default branch, when the caller knows it — a sync
    // does, and points the layout's HEAD at it. `None` leaves HEAD
    // alone, which is what a caller staging one extra ref wants:
    // transplanting a fork's commits must not repoint trunk.
    head_branch: Option<&str>,
    old_refs: Vec<(String, String)>,
    new_refs: Vec<(String, String)>,
) -> Result<(), String> {
    let mut attempt = 0;
    loop {
        let (mbytes, etag) = store.get_with_etag(manifest_key)?;
        let mut manifest: Manifest =
            serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;

        // Old tips that still resolve in the seed bound the walk.
        let old_tips: Vec<&str> = {
            let mut v: Vec<&str> = old_refs
                .iter()
                .map(|(_, o)| o.as_str())
                .filter(|o| {
                    git(seed)
                        .args(["cat-file", "-e", o])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false)
                })
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        let changed_tips: Vec<&str> = {
            let old_map: HashMap<&str, &str> = old_refs
                .iter()
                .map(|(n, o)| (n.as_str(), o.as_str()))
                .collect();
            let mut v: Vec<&str> = new_refs
                .iter()
                .filter(|(n, o)| old_map.get(n.as_str()) != Some(&o.as_str()))
                .map(|(_, o)| o.as_str())
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        };

        // Candidate objects, then layout-dedup (WAL sidecars ∪ locator).
        let mut rl_input = String::new();
        for t in &changed_tips {
            rl_input.push_str(t);
            rl_input.push('\n');
        }
        for t in &old_tips {
            rl_input.push('^');
            rl_input.push_str(t);
            rl_input.push('\n');
        }
        let cand = run_with_stdin(
            git(seed).args(["rev-list", "--objects", "--stdin"]),
            rl_input.as_bytes(),
        )?;
        let cand = String::from_utf8_lossy(&cand);
        let mut wal_oids: HashSet<[u8; 20]> = HashSet::new();
        for w in &manifest.wal {
            let raw = store.get(&w.oids_key)?;
            for c in raw.as_chunks::<20>().0 {
                wal_oids.insert(*c);
            }
        }
        let plane = match Plane::load(store, prefix) {
            Ok(p) => Some(p),
            Err(e) if stratum_engine::errclass::is_absent(&e) => None,
            Err(e) => return Err(e),
        };
        let mut new_oids: Vec<String> = Vec::new();
        let mut revs = rl_input.clone();
        for line in cand.lines() {
            let oid = line.split(' ').next().unwrap_or("");
            if oid.len() != 40 {
                continue;
            }
            let bin = Plane::parse_oid(oid)?;
            let present = wal_oids.contains(&bin)
                || match &plane {
                    Some(p) => p.lookup(store, &bin)?.is_some(),
                    None => false,
                };
            if present {
                revs.push('^');
                revs.push_str(oid);
                revs.push('\n');
            } else {
                new_oids.push(oid.to_string());
            }
        }
        new_oids.sort();
        new_oids.dedup();

        // Ref-only change (deletion, tag move onto known objects).
        if new_oids.is_empty() {
            apply_refs(store, prefix, &mut manifest, &new_refs)?;
            point_head(&mut manifest, head_branch);
            point_head(&mut manifest, head_branch);
            let body = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
            match store.put(manifest_key, &body, PutCond::IfMatch(etag)) {
                Ok(()) => return Ok(()),
                Err(PutError::Conflict) => {
                    attempt += 1;
                    if attempt >= CAS_RETRIES {
                        return Err("mirror sync lost the manifest CAS race repeatedly".into());
                    }
                    continue;
                }
                Err(e) => return Err(e.to_string()),
            }
        }

        let out = run_with_stdin(
            git(seed).args([
                "pack-objects",
                "--revs",
                "--thin",
                "--no-sparse",
                "--no-use-bitmap-index",
                "--no-reuse-delta",
                "--delta-base-offset",
                "--stdout",
                "-q",
            ]),
            revs.as_bytes(),
        )?;
        if out.len() < 32 || &out[..4] != b"PACK" {
            return Err("pack-objects produced no pack".into());
        }
        let entries = u32::from_be_bytes(out[8..12].try_into().unwrap()) as u64;
        if entries != new_oids.len() as u64 {
            return Err(format!(
                "sync pack entry count {entries} != expected {} (walk divergence)",
                new_oids.len()
            ));
        }
        let payload = &out[12..out.len() - 20];

        let digest = stratum_store::pack::hex(&sha1::Sha1::digest(payload));
        let wal_key = format!("{prefix}/{}/wal/{digest}.seg", manifest.epoch);
        let oids_key = format!("{prefix}/{}/wal/{digest}.oids", manifest.epoch);
        let mut sorted_bin: Vec<[u8; 20]> = new_oids
            .iter()
            .map(|o| Plane::parse_oid(o))
            .collect::<Result<_, _>>()?;
        sorted_bin.sort();
        let oid_blob: Vec<u8> = sorted_bin.iter().flat_map(|o| o.iter().copied()).collect();
        store
            .put(&wal_key, payload, PutCond::None)
            .map_err(|e| e.to_string())?;
        store
            .put(&oids_key, &oid_blob, PutCond::None)
            .map_err(|e| e.to_string())?;

        let old_map: HashMap<&str, &str> = old_refs
            .iter()
            .map(|(n, o)| (n.as_str(), o.as_str()))
            .collect();
        let updates: Vec<(String, String, String)> = new_refs
            .iter()
            .filter(|(n, o)| old_map.get(n.as_str()) != Some(&o.as_str()))
            .map(|(n, o)| {
                (
                    n.clone(),
                    old_map.get(n.as_str()).unwrap_or(&ZERO).to_string(),
                    o.clone(),
                )
            })
            .collect();
        apply_refs(store, prefix, &mut manifest, &new_refs)?;
        manifest.wal.push(WalEntry {
            key: wal_key,
            oids_key,
            entries,
            bytes: payload.len() as u64,
            updates,
        });
        let body = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
        match store.put(manifest_key, &body, PutCond::IfMatch(etag)) {
            Ok(()) => return Ok(()),
            Err(PutError::Conflict) => {
                attempt += 1;
                if attempt >= CAS_RETRIES {
                    return Err("mirror sync lost the manifest CAS race repeatedly".into());
                }
                // The content-addressed WAL objects are reusable; re-read
                // the manifest and re-validate.
                continue;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Rewrite the manifest's ref view to origin truth. Unpaged manifests carry
/// the full list; paged ones (reached only on the no-deletions path — the
/// caller re-ingests otherwise) write each changed ref into its
/// content-addressed page, and the serving-critical tips in `refs` follow.
/// Point the layout's HEAD at the origin's default branch.
///
/// `manifest.head` was only ever set by `full_ingest`, so an origin that
/// **renamed** its default branch left the mirror advertising a HEAD that
/// no longer resolves — the incremental path updated the refs, pruned the
/// old branch with them, and left `head` naming it. Every fresh clone of
/// that mirror then landed on an unborn branch with no working tree, and
/// nothing anywhere reported an error. `master` to `main` is precisely
/// this event, and the whole ecosystem did it.
fn point_head(manifest: &mut Manifest, head_branch: Option<&str>) {
    let Some(b) = head_branch else { return };
    let want = format!("refs/heads/{b}");
    if manifest.head != want {
        manifest.head = want;
    }
}

fn apply_refs(
    store: &ObjectStore,
    prefix: &str,
    manifest: &mut Manifest,
    new_refs: &[(String, String)],
) -> Result<(), String> {
    if manifest.ref_pages.is_empty() {
        manifest.refs = new_refs.to_vec();
        return Ok(());
    }
    let mut current: Vec<(String, String)> = manifest.refs.clone();
    for page in &manifest.ref_pages {
        current.extend(stratum_store::refpages::load_page(store, page)?);
    }
    let cur_map: HashMap<&str, &str> = current
        .iter()
        .map(|(n, o)| (n.as_str(), o.as_str()))
        .collect();
    let data_prefix = format!("{prefix}/{}", manifest.epoch);
    for (n, o) in new_refs {
        if cur_map.get(n.as_str()) == Some(&o.as_str()) {
            continue;
        }
        stratum_store::refpages::update(store, manifest, &data_prefix, n, o)?;
        if let Some(r) = manifest.refs.iter_mut().find(|(rn, _)| rn == n) {
            r.1 = o.clone();
        }
    }
    Ok(())
}
