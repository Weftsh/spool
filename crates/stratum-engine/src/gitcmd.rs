//! Scrubbed `git` subprocess helpers for ingest and sync work.
//!
//! Same posture as the receive-path quarantine: environment cleared, no
//! user/system config, no prompts. Ingest inputs are repos we cloned
//! ourselves (not hostile packs), so the receive path's rlimits are not
//! applied here — ingest of a large repo legitimately needs more than the
//! quarantine budget.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// A `git` command running against `repo` (a bare or non-bare git dir),
/// with a scrubbed environment.
pub fn git(repo: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(repo);
    scrub(&mut c);
    c
}

pub fn scrub(c: &mut Command) {
    c.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
}

/// Run to completion; return stdout bytes; stderr becomes the error.
pub fn run(cmd: &mut Command) -> Result<Vec<u8>, String> {
    let out = cmd
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(out.stdout)
}

pub fn run_str(cmd: &mut Command) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&run(cmd)?).trim().to_string())
}

/// Run with stdin piped in from `input`, reading stdout concurrently so
/// large packs can flow both ways without deadlocking on pipe buffers.
pub fn run_with_stdin(cmd: &mut Command, input: &[u8]) -> Result<Vec<u8>, String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn git: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();

    let res = std::thread::scope(|s| {
        let writer = s.spawn(move || {
            // A closed pipe (child exited early) surfaces as the child's
            // stderr, which is the useful error; ignore the write error.
            let _ = stdin.write_all(input);
        });
        let err_reader = s.spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        });
        let mut out = Vec::new();
        let read_res = stdout.read_to_end(&mut out);
        writer.join().ok();
        let errbuf = err_reader.join().unwrap_or_default();
        read_res
            .map(|_| (out, errbuf))
            .map_err(|e| format!("read git stdout: {e}"))
    })?;
    let (out, errbuf) = res;
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(String::from_utf8_lossy(&errbuf).to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `git` that exits non-zero is an error carrying **its stderr**,
    /// not an `Ok` carrying whatever it managed to print.
    ///
    /// This is the arm every caller's error message actually comes from,
    /// and nothing here reached it on purpose: it was covered on a Linux
    /// runner, where some ingest fixture happens to make git fail, and
    /// uncovered on a development machine, so the coverage gate reported
    /// the ledger entry as stale on one machine and correct on the
    /// other. Making it deterministic is one directory and one command.
    #[test]
    fn a_git_that_exits_non_zero_is_an_error_carrying_its_stderr() {
        let dir = std::env::temp_dir().join(format!("stratum-gitcmd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Not a repository, so `git -C` refuses before reading a byte of
        // stdin — which also exercises the write half's closed pipe.
        let err = run_with_stdin(git(&dir).args(["cat-file", "--batch"]), b"HEAD\n")
            .expect_err("git cannot read objects outside a repository");
        assert!(
            err.contains("repository"),
            "the error is git's own words: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
