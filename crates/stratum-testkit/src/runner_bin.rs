//! The real `weft-runner` binary, for suites that drive a whole job
//! through it: server, claim, checkout, steps, log, verdict.
//!
//! A suite that stands in for the runner with its own HTTP calls proves
//! what we believe the runner sends; this is how the other half gets
//! proved — the agent an operator actually installs, registering and
//! claiming against the server a customer actually runs.

use crate::Server;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The runner binary next to a server binary, built if it is not there
/// or is older than the runner's source.
///
/// `cargo test --workspace` builds every package's binaries before it
/// runs anything, so the sibling exists; a single-suite run
/// (`--test self_hosted_e2e`) does not build other packages, and building
/// it here — same profile, same target directory — keeps that run
/// honest instead of skipping the runner. The staleness check is for
/// the same single-suite run after an edit to the runner: an existing
/// binary would be taken as-is and the suite would pass or fail against
/// the previous runner, which reads as the edit having had no effect.
/// The runner's cargo package and the binary it produces are not the
/// same name: the package kept `stratum-runner` when the public contract
/// was renamed to Weft, and only the `[[bin]]` became `weft-runner`.
/// `cargo build -p` takes the package. Building the binary name here
/// failed with "package ID specification did not match any packages",
/// and a workspace run never noticed because the sibling already
/// existed — only a single-suite run on a clean target met it.
pub const RUNNER_PACKAGE: &str = "stratum-runner";
pub const RUNNER_BIN: &str = "weft-runner";

pub fn runner_bin_next_to(server_bin: &str) -> PathBuf {
    let server = PathBuf::from(server_bin);
    let dir = server.parent().expect("server binary has a directory");
    let bin = dir.join(RUNNER_BIN);
    if bin.exists() && !older_than_runner_source(&bin) {
        return bin;
    }
    let profile = dir.file_name().and_then(|n| n.to_str()).unwrap_or("debug");
    let target = dir.parent().expect("target dir");
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.args(["build", "-p", RUNNER_PACKAGE, "--target-dir"])
        .arg(target)
        .stdout(Stdio::null());
    if profile == "release" {
        cmd.arg("--release");
    }
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("run cargo build for {RUNNER_PACKAGE}: {e}"));
    assert!(status.success(), "cargo build -p {RUNNER_PACKAGE} failed");
    assert!(bin.exists(), "built, but {} is not there", bin.display());
    bin
}

/// Whether `bin` predates any file under `crates/stratum-runner/src`.
/// Unknown (no source tree beside this crate, no mtime) counts as
/// fresh: a build is the expensive answer and the sibling that is there
/// is the one cargo just produced.
fn older_than_runner_source(bin: &Path) -> bool {
    let Ok(built) = bin.metadata().and_then(|m| m.modified()) else {
        return false;
    };
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../stratum-runner/src");
    let mut stack = vec![src];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t > built)
            {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------
// A real agent, attached to a test server
// ---------------------------------------------------------------------

/// Mint a registration token for `org` — the call the dashboard's "add a
/// runner" page makes, and what an operator pastes into `register`.
/// `admin` is any credential that may administer the organisation: an
/// API token for [`Server::post`].
pub fn registration_token(server: &Server, admin: &str, org: &str) -> String {
    let (st, out) = server.post(
        &format!("/v1/orgs/{org}/runners/registration-token"),
        admin,
        None,
    );
    assert_eq!(st, 201, "mint a registration token for {org}: {out}");
    out["token"]
        .as_str()
        .unwrap_or_else(|| panic!("no token in {out}"))
        .to_string()
}

/// What `weft-runner register` left behind in its directory, before
/// anything has been asked to run.
///
/// Split from [`Agent`] because the registration is itself a contract —
/// what it prints, the `.runner` it writes and the mode it writes it with
/// — and a suite that only ever sees a running agent cannot look at it.
pub struct Registered {
    bin: PathBuf,
    dir: PathBuf,
    name: String,
    /// What `register` printed on stdout: the line an operator reads.
    pub printed: String,
}

impl Registered {
    /// `weft-runner register --url <base> --token <token> --name <name>
    /// --labels <labels> --dir <dir>`, as a separate process. Panics with
    /// both streams if it does not exit 0.
    pub fn register(
        bin: &Path,
        base: &str,
        token: &str,
        name: &str,
        labels: &[&str],
        dir: &Path,
    ) -> Registered {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
        let mut cmd = Command::new(bin);
        cmd.args(["register", "--url", base, "--token", token, "--name", name])
            .arg("--dir")
            .arg(dir);
        if !labels.is_empty() {
            cmd.args(["--labels", &labels.join(",")]);
        }
        let out = cmd
            .output()
            .unwrap_or_else(|e| panic!("run {} register: {e}", bin.display()));
        assert!(
            out.status.success(),
            "register {name} exited {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        Registered {
            bin: bin.to_path_buf(),
            dir: dir.to_path_buf(),
            name: name.to_string(),
            printed: String::from_utf8_lossy(&out.stdout).into_owned(),
        }
    }

    /// The `.runner` file, as the binary wrote it. It carries the
    /// runner's long-lived credential, so a suite reads it only to prove
    /// something about that file or to act as this machine itself.
    pub fn state(&self) -> serde_json::Value {
        let path = self.dir.join(".runner");
        let text = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_slice(&text)
            .unwrap_or_else(|e| panic!("{} is not JSON: {e}", path.display()))
    }

    /// `weft-runner run --dir <dir>`: start asking for work.
    pub fn run(self) -> Agent {
        let mut agent = Agent {
            child: None,
            reg: self,
            exited: None,
        };
        agent.spawn();
        agent
    }
}

/// A registered `weft-runner run` process, polling a test server for
/// work the way it would on somebody's build box.
///
/// What it prints goes to `agent.out` and `agent.err` in its directory,
/// and is all a suite may know about what it did: the binary keeps its
/// job tokens in memory and puts them nowhere a test could read, which
/// is the design, not an obstacle.
///
/// Dropping it stops it with **SIGTERM**, never SIGKILL: an agent's
/// contract is to end the job it is in and exit 0 on TERM, and a killed
/// child writes no coverage profile. A paused agent is continued first,
/// since a stopped process does not act on TERM until it runs again.
pub struct Agent {
    child: Option<Child>,
    reg: Registered,
    exited: Option<ExitStatus>,
}

/// How long a stopping agent gets before the harness gives up on being
/// polite. Ending a job is a group kill and a log flush; ten seconds is
/// far past that, and the SIGKILL after it is there so a wedged agent
/// fails the suite rather than hanging it.
const STOP_GRACE: Duration = Duration::from_secs(10);

impl Agent {
    /// Register as `name` with `labels` and start running — the whole of
    /// what an operator does to add a machine.
    pub fn attach(
        bin: &Path,
        base: &str,
        token: &str,
        name: &str,
        labels: &[&str],
        dir: &Path,
    ) -> Agent {
        Registered::register(bin, base, token, name, labels, dir).run()
    }

    fn spawn(&mut self) {
        // Appended, so a restarted agent's output follows its first
        // life's rather than replacing it.
        let open = |file: &str| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.reg.dir.join(file))
                .unwrap_or_else(|e| panic!("open {file}: {e}"))
        };
        let child = Command::new(&self.reg.bin)
            .arg("run")
            .arg("--dir")
            .arg(&self.reg.dir)
            .stdout(open("agent.out"))
            .stderr(open("agent.err"))
            .spawn()
            .unwrap_or_else(|e| panic!("run {} run: {e}", self.reg.bin.display()));
        self.child = Some(child);
        self.exited = None;
    }

    pub fn name(&self) -> &str {
        &self.reg.name
    }

    pub fn registration(&self) -> &Registered {
        &self.reg
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().map(Child::id).unwrap_or_default()
    }

    pub fn stdout(&self) -> String {
        std::fs::read_to_string(self.reg.dir.join("agent.out")).unwrap_or_default()
    }

    pub fn stderr(&self) -> String {
        std::fs::read_to_string(self.reg.dir.join("agent.err")).unwrap_or_default()
    }

    /// Both streams, labelled — for a failure message, where what the
    /// agent said is usually the whole diagnosis.
    pub fn said(&self) -> String {
        format!(
            "--- {} stdout ---\n{}--- {} stderr ---\n{}",
            self.reg.name,
            self.stdout(),
            self.reg.name,
            self.stderr()
        )
    }

    /// The job ids it announced taking (`took job <id>`), in order.
    pub fn took(&self) -> Vec<String> {
        self.announced("took job ")
    }

    /// The job ids it announced being done with (`finished job <id>`),
    /// however they ended.
    pub fn finished(&self) -> Vec<String> {
        self.announced("finished job ")
    }

    fn announced(&self, prefix: &str) -> Vec<String> {
        self.stdout()
            .lines()
            .filter_map(|l| l.strip_prefix(prefix))
            .filter_map(|rest| rest.split_whitespace().next())
            .map(str::to_string)
            .collect()
    }

    /// Block until `pred` holds of this agent, or panic with what it said.
    pub fn wait_until(&self, what: &str, within: Duration, pred: impl Fn(&Agent) -> bool) {
        let deadline = Instant::now() + within;
        while !pred(self) {
            assert!(
                Instant::now() < deadline,
                "waited {within:?} for {what}\n{}",
                self.said()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn signal(&self, sig: libc::c_int) {
        if let (Some(_), None) = (&self.child, self.exited) {
            // SAFETY: kill(2) on a pid this struct spawned and has not
            // reaped, so it cannot name somebody else's process.
            unsafe { libc::kill(self.pid() as libc::pid_t, sig) };
        }
    }

    /// SIGSTOP: the machine is still there, and says nothing — no claim,
    /// no log, no heartbeat. Its step keeps running in its own process
    /// group, exactly as it would behind a network partition.
    pub fn pause(&self) {
        self.signal(libc::SIGSTOP);
    }

    /// SIGCONT: it picks up where it was, and learns from its next call
    /// whatever happened while it was quiet.
    pub fn resume(&self) {
        self.signal(libc::SIGCONT);
    }

    /// Wait for the process to end on its own; its status, or `None` if
    /// it was still running at the deadline.
    pub fn wait_exit(&mut self, within: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(s) = self.exited {
                return Some(s);
            }
            let child = self.child.as_mut()?;
            match child.try_wait() {
                Ok(Some(s)) => self.exited = Some(s),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                _ => return None,
            }
        }
    }

    /// SIGTERM, and wait for it to go — what `systemctl stop` does.
    /// Idempotent; the status it exited with.
    pub fn stop(&mut self) -> Option<ExitStatus> {
        if let Some(s) = self.wait_exit(Duration::ZERO) {
            return Some(s);
        }
        self.resume();
        self.signal(libc::SIGTERM);
        if let Some(s) = self.wait_exit(STOP_GRACE) {
            return Some(s);
        }
        let child = self.child.as_mut()?;
        let _ = child.kill();
        let status = child.wait().ok();
        self.exited = status;
        status
    }

    /// Stop it and start it again from the same directory — the same
    /// `.runner`, so the same machine as far as the server knows. What a
    /// service manager does on `restart`, or a box does when it reboots.
    pub fn restart(&mut self) {
        self.stop();
        self.spawn();
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One value in a manifest under `[package]`, one under `[[bin]]`.
    fn manifest_names() -> (String, String) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../stratum-runner/Cargo.toml");
        let text = std::fs::read_to_string(&path).expect("the runner's Cargo.toml");
        let mut section = String::new();
        let mut package = None;
        let mut bin = None;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                section = line.to_string();
            } else if let Some(rest) = line.strip_prefix("name") {
                let value = rest
                    .trim_start()
                    .trim_start_matches('=')
                    .trim()
                    .trim_matches('"');
                match section.as_str() {
                    "[package]" => package = Some(value.to_string()),
                    "[[bin]]" => bin = Some(value.to_string()),
                    _ => {}
                }
            }
        }
        let package = package.expect("[package] name");
        let bin = bin.unwrap_or_else(|| package.clone());
        (package, bin)
    }

    /// The names the harness builds and looks for are the ones the
    /// runner's manifest declares. The rename that made them differ is
    /// exactly what this catches: `cargo build -p <binary name>` is a
    /// refusal, and a single-suite run on a clean target dir is the
    /// only place it shows.
    #[test]
    fn the_runner_package_and_binary_names_match_its_manifest() {
        let (package, bin) = manifest_names();
        assert_eq!(
            package, RUNNER_PACKAGE,
            "cargo build -p takes the package name"
        );
        assert_eq!(bin, RUNNER_BIN, "the sibling binary is the [[bin]] name");
    }
}
