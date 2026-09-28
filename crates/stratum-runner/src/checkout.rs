//! Getting the tree the job is about, without ever putting the credential
//! somewhere it can be read back.
//!
//! The obvious way to authenticate a fetch — `https://x:<token>@host/…` —
//! puts the token in the remote URL, which git then writes into
//! `.git/config`, echoes in its own progress output, and hands to every
//! step of the job through `git remote -v`. The next obvious way,
//! `-c http.extraHeader=…`, puts it in argv, where `ps` shows it to
//! anything else on the host and where any crash reporter picks it up.
//!
//! So the header travels in the environment instead, through git's own
//! `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n` protocol: it
//! is a config entry as far as git is concerned, but it never appears in a
//! command line, and the environment it lives in is not passed to any
//! step (see `spec::inherited_env`).

use crate::spec::ChangesetSpec;
use crate::steps::{supervise, Ctx, Ended, Outcome};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The one tree an ordinary job is about, under `workdir/repo`.
pub fn checkout(
    workdir: &Path,
    clone_url: &str,
    fetch_ref: &str,
    sha: &str,
    token: &str,
    inherited: &BTreeMap<String, String>,
    ctx: &Ctx,
) -> Outcome {
    into(
        &workdir.join("repo"),
        "Checkout",
        clone_url,
        fetch_ref,
        sha,
        token,
        inherited,
        ctx,
    )
}

/// Where a composed job's members are materialised.
pub fn workspace(workdir: &Path) -> PathBuf {
    workdir.join("workspace")
}

/// Every member of a composed job, in the changeset's order, each under
/// `workdir/workspace/<repo>`.
///
/// The job's own member is the tokenless one and is fetched with the job
/// token; every other member is fetched with the token minted for it, so
/// that this job's shell can reach exactly the repositories the changeset
/// named and no others. A member that cannot be materialised fails the
/// whole job by name: the tree the workflow was written against is not
/// there, and running the steps anyway would report a verdict about
/// something else.
pub fn members(
    workdir: &Path,
    cs: &ChangesetSpec,
    job_token: &str,
    inherited: &BTreeMap<String, String>,
    ctx: &Ctx,
) -> Outcome {
    let root = workspace(workdir);
    for m in &cs.members {
        // `m.repo` is a plain directory name — `spec::parse_changeset`
        // refuses anything else before it can reach this join.
        let outcome = into(
            &root.join(&m.repo),
            &format!("Checkout {}", m.repo),
            &m.clone_url,
            &m.fetch_ref,
            &m.commit_sha,
            token_for(m, job_token),
            inherited,
            ctx,
        );
        match outcome {
            Outcome::Passed => {}
            Outcome::Failed(e) => return Outcome::Failed(format!("member {}: {e}", m.repo)),
            stopped => return stopped,
        }
    }
    Outcome::Passed
}

/// Which credential fetches a member.
///
/// A sibling is read with the token minted for it and nothing else: the
/// job token is scoped to this job's own repository, so using it for a
/// sibling would simply fail, and using one org-wide token for all of
/// them would let a member's CI script — code its author wrote — read
/// repositories that author cannot see. Its own member is the tokenless
/// one, and the job token is exactly the credential for that.
///
/// Its own function because the local repositories the tests fetch from
/// never look at the header, so nothing else here could tell a wrong
/// choice from a right one.
fn token_for<'a>(m: &'a crate::spec::MemberSpec, job_token: &'a str) -> &'a str {
    m.token.as_deref().unwrap_or(job_token)
}

/// `git init`, fetch the one ref, check out the exact commit. Returns
/// `Outcome::Passed` when `dir` holds that tree.
///
/// Only the one ref, and `--no-tags`: a runner needs the commit under
/// test, not the repository's history of everything, and on a large repo
/// the difference is minutes per job.
///
/// `label` is what the log calls this checkout — bare `Checkout` for an
/// ordinary job, `Checkout <repo>` for one member of a composed one, so
/// that a person reading a composed job's log can see which tree a
/// failure was about.
#[allow(clippy::too_many_arguments)]
fn into(
    dir: &Path,
    label: &str,
    clone_url: &str,
    fetch_ref: &str,
    sha: &str,
    token: &str,
    inherited: &BTreeMap<String, String>,
    ctx: &Ctx,
) -> Outcome {
    ctx.sink.line(&format!("▶ {label}"));
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Outcome::Failed(format!("cannot create {}: {e}", dir.display()));
    }

    let phases: [(&str, Vec<String>, String); 3] = [
        (
            "init",
            vec!["init".into(), "-q".into()],
            "git init failed".into(),
        ),
        (
            "fetch",
            vec![
                "fetch".into(),
                "-q".into(),
                "--no-tags".into(),
                clone_url.into(),
                fetch_ref.into(),
            ],
            format!("fetch of {fetch_ref} failed"),
        ),
        (
            "checkout",
            vec!["checkout".into(), "-q".into(), sha.into()],
            // The overwhelmingly common cause, and the one worth naming:
            // the branch moved between the server queueing the job and the
            // runner fetching it, so the commit under test is no longer
            // reachable from the ref we were told to fetch.
            format!("commit {sha} is no longer on {fetch_ref}"),
        ),
    ];

    for (phase, args, failure) in phases {
        match supervise(git(dir, &args, token, inherited), ctx) {
            Ok(Ended::Exited(0)) => {}
            Ok(Ended::Exited(code)) => {
                ctx.sink
                    .line(&format!("✗ {label}: git {phase} exited {code}"));
                return Outcome::Failed(failure);
            }
            Ok(Ended::TimedOut) => return Outcome::TimedOut,
            Ok(Ended::Cancelled) => return Outcome::Cancelled,
            Err(e) => {
                ctx.sink.line(&format!("✗ {label}: {e}"));
                return Outcome::Failed(format!("git {phase} could not start: {e}"));
            }
        }
    }
    ctx.sink.line(&format!("✓ {label} {sha}"));
    Outcome::Passed
}

/// A `git` invocation with a scrubbed environment and the auth header.
///
/// The environment is cleared down to the same allowlist a step gets —
/// `spec::inherited_env` — so that a variable in the operator's own
/// environment (`GIT_SSH_COMMAND`, a proxy, an `http.*` override) cannot
/// change where the fetch goes, and so that nothing but the job token
/// authenticates it. That arrives below, as a header, deliberately.
fn git(cwd: &Path, args: &[String], token: &str, inherited: &BTreeMap<String, String>) -> Command {
    let mut c = Command::new("git");
    c.current_dir(cwd).args(args).env_clear();
    for (k, v) in inherited {
        c.env(k, v);
    }
    // No global or system config: the fetch must behave the same in the
    // image, on a developer's machine, and under a test.
    c.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        // Never sit waiting for a username nobody is there to type.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env(
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: {}", basic(token)),
        );
    c
}

/// `Basic base64("x:<token>")` — the shape the git HTTP door accepts.
///
/// Hand-rolled because base64 is thirty lines and this crate's dependency
/// list is part of what makes the runner image defensible.
fn basic(token: &str) -> String {
    format!("Basic {}", b64(format!("x:{token}").as_bytes()))
}

fn b64(input: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for c in input.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            A[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::log::Sink;
    use crate::steps::MAX_PROCS;
    use crate::testdir::TestDir;
    use crate::watch::Abuse;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::Receiver;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn collector() -> (Sink, Receiver<Vec<u8>>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Sink::from_sender(tx), rx)
    }

    fn drain(rx: &Receiver<Vec<u8>>) -> String {
        let mut out = Vec::new();
        while let Ok(b) = rx.try_recv() {
            out.extend_from_slice(&b);
        }
        String::from_utf8_lossy(&out).to_string()
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    fn no() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn ctx<'a>(sink: &'a Sink, cancelled: &'a AtomicBool) -> Ctx<'a> {
        Ctx {
            sink,
            deadline: far(),
            cancelled,
            abuse: Arc::new(Abuse::default()),
            max_procs: MAX_PROCS,
        }
    }

    /// What `spec::inherited_env` would hand a real job.
    fn inherited() -> BTreeMap<String, String> {
        BTreeMap::from([("PATH".to_string(), std::env::var("PATH").expect("PATH"))])
    }

    /// A real repository with two commits on `main`, built with the real
    /// `git` CLI. `checkout` is exercised against a local path rather than
    /// an HTTP URL — git accepts a path as a remote, the auth header is
    /// simply unused, and the header-over-HTTP case is proven end to end by
    /// the server's own runner suite.
    pub(crate) fn origin(dir: &Path) -> (String, String) {
        origin_named(dir, "origin")
    }

    /// The same, under a name of the caller's choosing — a composed job
    /// needs more than one origin under the same scratch directory.
    pub(crate) fn origin_named(dir: &Path, name: &str) -> (String, String) {
        let src = dir.join(name);
        std::fs::create_dir_all(&src).expect("mkdir");
        run_git(&src, &["init", "-q", "-b", "main"]);
        std::fs::write(src.join("README.md"), "one\n").expect("write");
        run_git(&src, &["add", "-A"]);
        run_git(&src, &["commit", "-q", "-m", "one"]);
        let first = run_git(&src, &["rev-parse", "HEAD"]).trim().to_string();
        std::fs::write(src.join("README.md"), "two\n").expect("write");
        run_git(&src, &["add", "-A"]);
        run_git(&src, &["commit", "-q", "-m", "two"]);
        let second = run_git(&src, &["rev-parse", "HEAD"]).trim().to_string();
        (
            src.to_string_lossy().to_string(),
            format!("{first} {second}"),
        )
    }

    /// The same scrubbed-environment discipline the testkit uses, so a
    /// developer's own git config cannot change what these prove.
    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", "/nonexistent")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Runner Test")
            .env("GIT_AUTHOR_EMAIL", "runner@stratum.invalid")
            .env("GIT_COMMITTER_NAME", "Runner Test")
            .env("GIT_COMMITTER_EMAIL", "runner@stratum.invalid")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    #[test]
    fn a_checkout_lands_the_exact_commit_and_says_so() {
        let dir = TestDir::new("checkout-ok");
        let (url, shas) = origin(dir.path());
        let head = shas.split(' ').nth(1).expect("two shas").to_string();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("mkdir");
        let (sink, rx) = collector();
        assert_eq!(
            checkout(
                &work,
                &url,
                "refs/heads/main",
                &head,
                "tok",
                &inherited(),
                &ctx(&sink, &no()),
            ),
            Outcome::Passed
        );
        assert_eq!(
            std::fs::read_to_string(work.join("repo/README.md")).expect("worktree"),
            "two\n"
        );
        assert_eq!(
            run_git(&work.join("repo"), &["rev-parse", "HEAD"]).trim(),
            head
        );
        let out = drain(&rx);
        assert!(out.starts_with("▶ Checkout\n"), "{out}");
        assert!(out.contains(&format!("✓ Checkout {head}")), "{out}");
    }

    #[test]
    fn an_earlier_commit_on_the_ref_is_still_reachable() {
        // The job's sha is not always the tip: a push of two commits
        // can queue a job for the first one.
        let dir = TestDir::new("checkout-old");
        let (url, shas) = origin(dir.path());
        let first = shas.split(' ').next().expect("a sha").to_string();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("mkdir");
        let (sink, _rx) = collector();
        assert_eq!(
            checkout(
                &work,
                &url,
                "refs/heads/main",
                &first,
                "tok",
                &inherited(),
                &ctx(&sink, &no()),
            ),
            Outcome::Passed
        );
        assert_eq!(
            std::fs::read_to_string(work.join("repo/README.md")).expect("worktree"),
            "one\n"
        );
    }

    #[test]
    fn a_commit_that_is_not_on_the_fetched_ref_names_exactly_that() {
        let dir = TestDir::new("checkout-gone");
        let (url, _) = origin(dir.path());
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("mkdir");
        let (sink, rx) = collector();
        let missing = "0123456789012345678901234567890123456789";
        assert_eq!(
            checkout(
                &work,
                &url,
                "refs/heads/main",
                missing,
                "tok",
                &inherited(),
                &ctx(&sink, &no()),
            ),
            Outcome::Failed(format!("commit {missing} is no longer on refs/heads/main"))
        );
        assert!(drain(&rx).contains("✗ Checkout: git checkout exited"));
    }

    #[test]
    fn a_ref_that_does_not_exist_fails_at_the_fetch_and_says_which_ref() {
        let dir = TestDir::new("checkout-noref");
        let (url, _) = origin(dir.path());
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("mkdir");
        let (sink, rx) = collector();
        assert_eq!(
            checkout(
                &work,
                &url,
                "refs/heads/nope",
                "0000",
                "tok",
                &inherited(),
                &ctx(&sink, &no()),
            ),
            Outcome::Failed("fetch of refs/heads/nope failed".into())
        );
        assert!(drain(&rx).contains("✗ Checkout: git fetch exited"));
    }

    #[test]
    fn an_unwritable_workdir_is_reported_rather_than_panicked() {
        let dir = TestDir::new("checkout-blocked");
        // A *file* where the workdir's parent should be: ENOTDIR, whoever
        // the process happens to be running as. A path under `/` would
        // pass for root, which some CI images are.
        std::fs::write(dir.path().join("blocked"), "").expect("write");
        let blocked = dir.path().join("blocked/work");
        let (sink, _rx) = collector();
        let out = checkout(
            &blocked,
            "irrelevant",
            "refs/heads/main",
            "0000",
            "tok",
            &inherited(),
            &ctx(&sink, &no()),
        );
        // The errno differs by platform (EROFS under SIP, EACCES on a
        // Linux runner); what must not differ is that it is reported as
        // the job's error rather than a panic in the runner.
        // Matched through Debug rather than destructured: a `let … else`
        // would need an arm that a passing run can never take, and the
        // coverage gate would rightly ask what it is for.
        let shown = format!("{out:?}");
        assert!(
            shown.starts_with(&format!(
                "Failed(\"cannot create {}/repo: ",
                blocked.display()
            )),
            "{shown}"
        );
    }

    #[test]
    fn a_cancelled_or_timed_out_checkout_reports_itself_as_such() {
        let dir = TestDir::new("checkout-stop");
        let (url, _) = origin(dir.path());
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("mkdir");
        let (sink, _rx) = collector();
        // A deadline already in the past: the first git child is killed
        // before it can finish.
        assert_eq!(
            checkout(
                &work,
                &url,
                "refs/heads/main",
                "0000",
                "tok",
                &inherited(),
                &Ctx {
                    sink: &sink,
                    deadline: Instant::now() - Duration::from_secs(1),
                    cancelled: &no(),
                    abuse: Arc::new(Abuse::default()),
                    max_procs: MAX_PROCS,
                }
            ),
            Outcome::TimedOut
        );
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            checkout(
                &work,
                &url,
                "refs/heads/main",
                "0000",
                "tok",
                &inherited(),
                &ctx(&sink, &cancelled),
            ),
            Outcome::Cancelled
        );
    }

    #[test]
    fn a_git_that_cannot_be_spawned_is_a_failure_with_the_reason() {
        // `git` is resolved through the PATH the job runs with; pointed at
        // a directory with no git in it the child never starts, and the
        // job must say so rather than hang. (An empty environment would
        // not do — the exec falls back to a default PATH.)
        let dir = TestDir::new("checkout-nogit");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("mkdir");
        let (sink, _rx) = collector();
        let nowhere = BTreeMap::from([(
            "PATH".to_string(),
            "/nonexistent-weft-runner-bin".to_string(),
        )]);
        let out = checkout(
            &work,
            "u",
            "refs/heads/main",
            "0000",
            "tok",
            &nowhere,
            &ctx(&sink, &no()),
        );
        assert_eq!(
            out,
            Outcome::Failed(
                "git init could not start: cannot spawn: No such file or directory (os error 2)"
                    .into()
            )
        );
    }

    /// One member of a composed job, pointed at a real local repository.
    fn member(repo: &str, url: &str, sha: &str, token: Option<&str>) -> crate::spec::MemberSpec {
        crate::spec::MemberSpec {
            repo: repo.into(),
            change: format!("I{repo}"),
            clone_url: url.into(),
            fetch_ref: "refs/heads/main".into(),
            commit_sha: sha.into(),
            token: token.map(str::to_string),
        }
    }

    #[test]
    fn a_sibling_is_fetched_with_its_own_token_and_never_the_jobs() {
        let own = member("api", "u", "s", None);
        let sibling = member("web", "u", "s", Some("minted-for-web"));
        assert_eq!(token_for(&own, "job-token"), "job-token");
        assert_eq!(token_for(&sibling, "job-token"), "minted-for-web");
    }

    #[test]
    fn a_composed_job_gets_every_member_side_by_side_at_its_own_commit() {
        let dir = TestDir::new("workspace-ok");
        let (api, api_shas) = origin_named(dir.path(), "api");
        let (web, web_shas) = origin_named(dir.path(), "web");
        // Different commits on purpose: the members are independent, and
        // a wrong one would otherwise be invisible.
        let api_head = api_shas.split(' ').nth(1).expect("two shas").to_string();
        let web_first = web_shas.split(' ').next().expect("a sha").to_string();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("mkdir");
        let cs = ChangesetSpec {
            key: "Ic5000001".into(),
            members: vec![
                member("api", &api, &api_head, None),
                member("web", &web, &web_first, Some("member-s3cret")),
            ],
        };
        let (sink, rx) = collector();
        assert_eq!(
            members(&work, &cs, "job-token", &inherited(), &ctx(&sink, &no())),
            Outcome::Passed
        );
        let root = workspace(&work);
        assert_eq!(root, work.join("workspace"));
        assert_eq!(
            std::fs::read_to_string(root.join("api/README.md")).expect("api worktree"),
            "two\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("web/README.md")).expect("web worktree"),
            "one\n"
        );
        assert_eq!(
            run_git(&root.join("api"), &["rev-parse", "HEAD"]).trim(),
            api_head
        );
        assert_eq!(
            run_git(&root.join("web"), &["rev-parse", "HEAD"]).trim(),
            web_first
        );
        // Named per member, in the changeset's order, so a person reading
        // the log can tell which tree a failure was about.
        let out = drain(&rx);
        assert!(out.starts_with("▶ Checkout api\n"), "{out}");
        assert!(out.contains(&format!("✓ Checkout api {api_head}")), "{out}");
        assert!(out.contains("▶ Checkout web\n"), "{out}");
        assert!(
            out.contains(&format!("✓ Checkout web {web_first}")),
            "{out}"
        );
        assert!(!out.contains("member-s3cret"), "{out}");
    }

    #[test]
    fn a_member_that_cannot_be_checked_out_fails_the_job_by_name() {
        let dir = TestDir::new("workspace-bad");
        let (api, api_shas) = origin_named(dir.path(), "api");
        let api_head = api_shas.split(' ').nth(1).expect("two shas").to_string();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("mkdir");
        let cs = ChangesetSpec {
            key: "Ic5000001".into(),
            members: vec![
                member("api", &api, &api_head, None),
                // A sibling whose patchset ref is gone: recomposed, or
                // the change was abandoned between spec and fetch.
                member(
                    "web",
                    &dir.path().join("nowhere").to_string_lossy(),
                    "0000",
                    Some("t"),
                ),
            ],
        };
        let (sink, rx) = collector();
        assert_eq!(
            members(&work, &cs, "job-token", &inherited(), &ctx(&sink, &no())),
            Outcome::Failed("member web: fetch of refs/heads/main failed".into())
        );
        assert!(drain(&rx).contains("✗ Checkout web: git fetch exited"));
        // The member before it was still materialised; the job stops at
        // the first one it cannot get, rather than running steps against
        // half a workspace.
        assert!(workspace(&work).join("api/README.md").exists());
    }

    #[test]
    fn a_workspace_checkout_that_is_stopped_reports_itself_as_such() {
        // Cancellation and the deadline mean the same thing to a member
        // as to an ordinary checkout: nobody wants a verdict, so it is
        // not turned into a failure that names a repository.
        let dir = TestDir::new("workspace-stop");
        let (api, _) = origin_named(dir.path(), "api");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("mkdir");
        let cs = ChangesetSpec {
            key: "Ic5".into(),
            members: vec![member("api", &api, "0000", None)],
        };
        let (sink, _rx) = collector();
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            members(&work, &cs, "t", &inherited(), &ctx(&sink, &cancelled)),
            Outcome::Cancelled
        );
    }

    #[test]
    fn the_credential_is_a_header_in_the_environment_and_nowhere_else() {
        let c = git(
            Path::new("/tmp"),
            &["init".to_string()],
            "s3cret",
            &inherited(),
        );
        let env: Vec<(String, String)> = c
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.unwrap_or_default().to_string_lossy().to_string(),
                )
            })
            .collect();
        assert!(env.contains(&("GIT_CONFIG_COUNT".into(), "1".into())));
        assert!(env.contains(&("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into())));
        assert!(env.contains(&(
            "GIT_CONFIG_VALUE_0".into(),
            "Authorization: Basic eDpzM2NyZXQ=".into()
        )));
        // Not in argv — the property the whole module exists for.
        let argv: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(!argv.iter().any(|a| a.contains("s3cret")), "{argv:?}");
        assert!(!c.get_program().to_string_lossy().contains("s3cret"));
    }

    #[test]
    fn the_basic_encoding_matches_what_the_git_door_decodes() {
        // Every padding case, since the tail is where a hand-rolled base64
        // goes wrong.
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foob"), "Zm9vYg==");
        assert_eq!(b64(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
        // The two characters that separate base64 from base64url, which a
        // wrong table would silently swap.
        assert_eq!(b64(&[251, 255]), "+/8=");
        assert_eq!(basic("t"), "Basic eDp0");
    }
}
