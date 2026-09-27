//! Real-git-CLI helpers: build fixture repos, clone through a server under
//! test, and run the non-negotiable `fsck --full --strict` gate (invariant
//! I11 in reference/invariants.md).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Run `git` with a scrubbed environment plus a fixed identity, in `cwd`.
/// Panics with the full stderr on failure — test helpers should fail loudly.
pub fn git(cwd: &Path, args: &[&str]) -> String {
    let out = git_cmd(cwd, args)
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    if !out.status.success() {
        panic!(
            "git {args:?} in {} failed:\n{}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Like `git` but returns Err instead of panicking (for asserting failures).
pub fn git_expect_err(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = git_cmd(cwd, args)
        .output()
        .map_err(|e| format!("spawn git {args:?}: {e}"))?;
    if out.status.success() {
        Err(format!(
            "git {args:?} unexpectedly succeeded:\n{}",
            String::from_utf8_lossy(&out.stdout)
        ))
    } else {
        Ok(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

fn git_cmd(cwd: &Path, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    c.current_dir(cwd)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Testkit")
        .env("GIT_AUTHOR_EMAIL", "testkit@stratum.invalid")
        .env("GIT_COMMITTER_NAME", "Testkit")
        .env("GIT_COMMITTER_EMAIL", "testkit@stratum.invalid")
        // Deterministic commit ids across runs where callers want them.
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z");
    c
}

/// Build a small working repo at `dir` with `commits` commits on the default
/// branch (named `main`) touching a handful of paths, plus one side branch.
/// Returns the tip commit id of `main`.
pub fn fixture_repo(dir: &Path, commits: usize) -> String {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    for i in 0..commits {
        std::fs::write(dir.join("README.md"), format!("fixture rev {i}\n")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            format!("// rev {i}\npub fn v() -> usize {{ {i} }}\n"),
        )
        .unwrap();
        std::fs::write(dir.join(format!("file-{}.txt", i % 5)), format!("{i}\n")).unwrap();
        git(dir, &["add", "-A"]);
        git(
            dir,
            &[
                "commit",
                "-q",
                "--date",
                &format!("2026-01-01T00:{:02}:00Z", i % 60),
                "-m",
                &format!("commit {i}"),
            ],
        );
    }
    git(dir, &["checkout", "-q", "-b", "side"]);
    std::fs::write(dir.join("side.txt"), "side branch\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "side branch commit"]);
    git(dir, &["checkout", "-q", "main"]);
    git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

/// Clone `url` into `dest` and run the I11 gate: `fsck --full --strict`
/// must be clean. Panics otherwise. Returns the clone's HEAD commit.
pub fn clone_and_fsck(url: &str, dest: &Path) -> String {
    let parent = dest.parent().unwrap();
    std::fs::create_dir_all(parent).unwrap();
    git(parent, &["clone", "-q", url, dest.to_str().unwrap()]);
    fsck(dest);
    git(dest, &["rev-parse", "HEAD"]).trim().to_string()
}

/// The non-negotiable correctness gate on any produced clone.
pub fn fsck(repo: &Path) {
    git(repo, &["fsck", "--full", "--strict"]);
}

/// A scratch directory under the target dir, removed on drop.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(hint: &str) -> Scratch {
        // Claimed, not adopted: a leftover under a reused pid is somebody
        // else's checkout, and `git clone` into it refuses — see `tempdir`.
        let dir = crate::tempdir::TempDir::new(&format!("stratum-test-{hint}"))
            .unwrap_or_else(|e| panic!("scratch directory: {e}"));
        Scratch(dir.into_path())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
