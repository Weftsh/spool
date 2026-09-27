//! Publishing a repository's static site after a write.
//!
//! A queued job rather than inline work in the push path, for two
//! reasons. A push must not wait on it, and — the one that decided it —
//! a **mirror sync** has no `SharedState` to call a handler with, but it
//! can enqueue. Publishing that only ever happened on a native push
//! would mean mirroring a repository here and watching its site never
//! update, which is the same shape as the bug where mirror sync armed
//! no CDN pack and no checks poll.
//!
//! The job reads the tip **when it runs**, not the one that enqueued it,
//! so a burst of pushes publishes the newest tree once. That is also why
//! it is safe for the queue to deduplicate it per repository.
//!
//! What it does not do is copy anything. A deploy for a site that needs
//! no build is the commit and the tree of the published directory, and
//! every blob under that tree is already in this repository's store.

use crate::app::SharedState;
use crate::site::host;
use crate::workflow::read::{self, SiteFile};
use crate::workflow::siteconfig::{self, SiteConfig};
use stratum_control::jobs;
use stratum_control::registry::Repo;
use stratum_control::sites;
use stratum_engine::objwrite::hex;

/// How many host labels to try before giving up.
///
/// The derivation is lossy, so `docs--acme` can genuinely be taken by
/// another repository; `-2`, `-3` and so on resolve that. Sixteen is far
/// past any real collision and bounds the work a hostile pair of names
/// could ask for.
const MAX_HOST_TRIES: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No `.weft/site.yml` at the tip. Any site already published keeps
    /// serving: deleting the config is not a request to take a site
    /// down, and treating it as one would make a mistaken revert an
    /// outage.
    NoConfig,
    /// The config is there but does not parse. Nothing is published, and
    /// what is already published keeps serving.
    Refused(String),
    /// The config names a directory that is not in the tree.
    NoSuchDirectory(String),
    /// Published, and now being served.
    Published { deploy_id: String, host: String },
    /// The repository is gone, or its branch is.
    NotNeeded,
}

pub fn enqueue(state: &SharedState, org_id: &str, repo_id: &str) {
    if let Err(e) = jobs::enqueue_unique(&state.db, org_id, repo_id, "sitepublish", None) {
        eprintln!("weft: sitepublish enqueue: {e}");
    }
}

/// What claiming one candidate label did.
///
/// Three answers rather than a bool because the middle one is the whole
/// reason this walks candidates at all, and it is not a failure.
pub enum Claim<T> {
    /// It is ours.
    Took(T),
    /// Somebody else has it — either we could see that before trying, or
    /// we lost the insert to another node. Try the next candidate.
    Taken,
}

/// Walk candidate labels until one is claimed.
///
/// Split from the database work, and from `ensure_site` below, because
/// the interesting behaviour here needs no database at all: that a
/// collision moves to the next candidate rather than failing, and that
/// exhausting the candidates reports rather than looping. Reaching
/// either through Postgres would need sixteen repositories whose names
/// happen to collide, or two nodes inserting in the same instant — the
/// seam the coverage gate pointed at, and a better design for being
/// moved.
pub fn pick_label<T>(
    org_name: &str,
    repo_name: &str,
    mut claim: impl FnMut(&str) -> Result<Claim<T>, String>,
) -> Result<T, String> {
    for n in 0..MAX_HOST_TRIES {
        match claim(&host::candidate(org_name, repo_name, n))? {
            Claim::Took(v) => return Ok(v),
            Claim::Taken => continue,
        }
    }
    Err(format!(
        "could not find a free host label for {org_name}/{repo_name} \
         after {MAX_HOST_TRIES} tries"
    ))
}

/// Pick and record the host label for a repository that does not have a
/// site yet.
///
/// `create` is what actually decides — the unique index is the arbiter,
/// not the `host_taken` check, which only keeps us from burning an
/// insert on a label we already know is gone.
fn ensure_site(
    state: &SharedState,
    repo: &Repo,
    org_name: &str,
    branch: Option<&str>,
) -> Result<sites::Site, String> {
    if let Some(existing) = sites::get(&state.db, &repo.id)? {
        return Ok(existing);
    }
    pick_label(org_name, &repo.name, |label| {
        if sites::host_taken(&state.db, label)? {
            return Ok(Claim::Taken);
        }
        match sites::create(&state.db, &repo.id, label, branch) {
            Ok(site) => Ok(Claim::Took(site)),
            // Lost the race to another node between the check and the
            // insert. Try the next candidate rather than failing: the
            // index did its job.
            Err(_) => Ok(Claim::Taken),
        }
    })
}

/// Which ref publishes: what the config says, else the repository's
/// default branch. Resolved here rather than stored so that changing the
/// default branch does not silently stop publishing.
fn publish_ref(cfg: &SiteConfig, repo: &Repo) -> String {
    let branch = cfg
        .branch
        .clone()
        .unwrap_or_else(|| repo.default_branch.clone());
    format!("refs/heads/{branch}")
}

pub async fn run_one(state: &SharedState, job: &jobs::Job) -> Result<Outcome, String> {
    let Some(repo_id) = job.repo_id.clone() else {
        return Err("sitepublish job without repo".into());
    };
    let Some(repo) = stratum_control::registry::repo_by_id(&state.db, &job.org_id, &repo_id)?
    else {
        return Ok(Outcome::NotNeeded);
    };
    let Some(org) = stratum_control::registry::org_by_id(&state.db, &job.org_id)? else {
        return Ok(Outcome::NotNeeded);
    };
    let prefix = repo.prefix().as_str().to_string();

    // One reader for the whole job: the config and the published tree
    // are two lookups in the same layout, and the read cache makes the
    // second nearly free.
    let repo_for_task = repo.clone();
    let read: Result<ReadOut, String> = crate::api::reads::with_reader(state, prefix, move |r| {
        // The config first, from the default branch — it is what says
        // which branch publishes, so it cannot be read from the branch
        // it names.
        let head = format!("refs/heads/{}", repo_for_task.default_branch);
        let cfg = match read::read_config(r, &head)? {
            // A rev that does not resolve and a repository with no
            // config are one answer here, not two: either way there is
            // nothing to publish and nothing has gone wrong. The route
            // that reports to a person keeps them apart; this does not
            // need to.
            SiteFile::NoRev | SiteFile::Absent => return Ok(ReadOut::NoConfig),
            SiteFile::Present(_, src) => match siteconfig::parse(&src) {
                Ok(c) => c,
                Err(refusal) => {
                    return Ok(ReadOut::Refused(
                        refusal.render(&format!("{}/site.yml", crate::workflow::read::DIR)),
                    ))
                }
            },
        };
        let want_ref = publish_ref(&cfg, &repo_for_task);
        let Some(commit) = r.resolve_rev(&want_ref)? else {
            return Ok(ReadOut::NoBranch);
        };
        let Some(entry) = r.entry_at(&commit, &cfg.publish)? else {
            return Ok(ReadOut::NoDir(cfg.publish.clone()));
        };
        let is_dir = entry.mode == "40000" || entry.mode == "040000";
        if !is_dir {
            return Ok(ReadOut::NoDir(cfg.publish.clone()));
        }
        Ok(ReadOut::Ready {
            cfg,
            commit,
            tree: hex(&entry.oid),
        })
    })
    .await;

    match read? {
        ReadOut::NoConfig => Ok(Outcome::NoConfig),
        ReadOut::Refused(msg) => Ok(Outcome::Refused(msg)),
        ReadOut::NoBranch => Ok(Outcome::NotNeeded),
        ReadOut::NoDir(d) => Ok(Outcome::NoSuchDirectory(d)),
        ReadOut::Ready { cfg, commit, tree } => {
            let site = ensure_site(state, &repo, &org.name, cfg.branch.as_deref())?;
            // Keep the stored branch in step with the config, so the
            // settings panel shows what is actually publishing.
            if site.branch.as_deref() != cfg.branch.as_deref() {
                sites::set_branch(&state.db, &repo.id, cfg.branch.as_deref())?;
            }
            // Nothing to do if this exact tree is already what is being
            // served: republishing would add a history row for a deploy
            // that changes nothing.
            if let Some(cur) = &site.current {
                if let Some(d) = sites::deploy(&state.db, cur)? {
                    if d.tree_oid == tree {
                        return Ok(Outcome::Published {
                            deploy_id: d.id,
                            host: site.host,
                        });
                    }
                }
            }
            let deploy = sites::add_deploy(
                &state.db,
                &repo.id,
                &commit,
                &tree,
                &cfg.publish,
                cfg.spa,
                cfg.not_found.as_deref(),
            )?;
            sites::publish(&state.db, &repo.id, &deploy.id)?;
            Ok(Outcome::Published {
                deploy_id: deploy.id,
                host: site.host,
            })
        }
    }
}

/// What the one store read came back with. A separate type so the read
/// closure stays `Send` and the database work happens outside it.
enum ReadOut {
    NoConfig,
    Refused(String),
    NoBranch,
    NoDir(String),
    Ready {
        cfg: SiteConfig,
        commit: String,
        tree: String,
    },
}

pub fn spawn(state: SharedState) {
    let poll = super::env_period("STRATUM_SITEPUBLISH_POLL_SECS", 5);
    if poll.is_zero() {
        return;
    }
    // Short: publishing is a couple of tree reads and two small writes,
    // so a stuck node should hand the repository on quickly.
    let lease_ms = super::lease_ms("STRATUM_SITEPUBLISH_LEASE_SECS", 120);
    tokio::spawn(async move {
        loop {
            let claimed = {
                let db = state.db.clone();
                tokio::task::spawn_blocking(move || jobs::claim(&db, "sitepublish", lease_ms))
                    .await
                    .unwrap_or_else(|e| Err(format!("join: {e}")))
            };
            match claimed {
                Ok(Some(job)) => match run_one(&state, &job).await {
                    Ok(o) => {
                        let _ = jobs::complete(&state.db, &job.id, Some(&format!("{o:?}")));
                    }
                    Err(e) => {
                        eprintln!("weft: site publish failed: {e}");
                        let _ = jobs::fail(&state.db, &job.id, &e);
                    }
                },
                Ok(None) => tokio::time::sleep(poll).await,
                Err(e) => {
                    eprintln!("weft: sitepublish claim: {e}");
                    tokio::time::sleep(poll).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_control::registry::RepoKind;

    fn repo(default_branch: &str) -> Repo {
        Repo {
            id: "r1".into(),
            org_id: "o1".into(),
            name: "docs".into(),
            description: None,
            homepage: None,
            kind: RepoKind::Native,
            public: true,
            default_branch: default_branch.into(),
            origin_url: None,
            origin_provider: None,
            origin_installation: None,
            last_sync_at: None,
            last_synced_commit: None,
            sync_error: None,
            created_at: 0,
        }
    }

    #[test]
    fn the_first_free_label_is_the_one_taken() {
        let mut seen = Vec::new();
        let got = pick_label("acme", "docs", |l| {
            seen.push(l.to_string());
            Ok(Claim::Took(l.to_string()))
        })
        .expect("a label");
        assert_eq!(got, "docs--acme");
        assert_eq!(seen, ["docs--acme"], "no candidate is tried needlessly");
    }

    /// The lossy derivation means two repositories genuinely can want
    /// one label. Reaching this through Postgres would need two
    /// repositories whose names collide; the behaviour is the same and
    /// belongs here.
    #[test]
    fn a_taken_label_moves_to_the_next_candidate() {
        let mut seen = Vec::new();
        let got = pick_label("acme", "docs", |l| {
            seen.push(l.to_string());
            if seen.len() < 3 {
                return Ok(Claim::Taken);
            }
            Ok(Claim::Took(l.to_string()))
        })
        .expect("a label");
        assert_eq!(got, "docs--acme-3");
        assert_eq!(seen, ["docs--acme", "docs--acme-2", "docs--acme-3"]);
    }

    /// Exhaustion reports rather than looping, and says which repository
    /// it gave up on — the operator reading it has nothing else to go on.
    #[test]
    fn exhausting_the_candidates_is_an_error_that_names_the_repository() {
        let mut tries = 0;
        let err = pick_label("acme", "docs", |_| {
            tries += 1;
            Ok::<Claim<()>, String>(Claim::Taken)
        })
        .expect_err("should give up");
        assert_eq!(tries, MAX_HOST_TRIES, "bounded, and it spends the bound");
        assert!(err.contains("acme/docs"), "{err}");
        assert!(err.contains(&MAX_HOST_TRIES.to_string()), "{err}");
    }

    /// A database failure is not a collision, and must not be retried as
    /// one — sixteen failing queries and then a misleading "no free
    /// label" would hide the real error.
    #[test]
    fn a_failure_looking_for_a_label_is_reported_not_retried() {
        let mut tries = 0;
        let err = pick_label("acme", "docs", |_| {
            tries += 1;
            Err::<Claim<()>, String>("db is on fire".into())
        })
        .expect_err("should propagate");
        assert_eq!(tries, 1, "it must not keep asking a broken database");
        assert_eq!(err, "db is on fire");
    }

    #[test]
    fn the_config_decides_the_branch_and_the_repository_is_the_default() {
        let mut cfg = SiteConfig::default();
        assert_eq!(publish_ref(&cfg, &repo("main")), "refs/heads/main");
        assert_eq!(publish_ref(&cfg, &repo("trunk")), "refs/heads/trunk");
        cfg.branch = Some("release".into());
        assert_eq!(publish_ref(&cfg, &repo("main")), "refs/heads/release");
    }
}
