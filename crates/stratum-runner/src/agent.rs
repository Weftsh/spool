//! The agent: register once, then ask for work until stopped.
//!
//! A runner is somebody else's machine, which the server cannot reach and
//! must never need to: every call here is **outbound**, nothing listens,
//! and the only credential on the machine is one this runner exchanged
//! for itself and that its operator can revoke from the runner list.
//!
//! Two commands, and the seam between them is a file:
//!
//! - `register` exchanges a one-hour, single-use registration token for
//!   the runner's own long-lived credential and writes `DIR/.runner`, mode
//!   0600. That is the only time a registration token is on disk, and it
//!   never is: it arrives as an argument and leaves as a request body.
//! - `run` reads `DIR/.runner` and long-polls `POST /v1/runners/claim`. A
//!   claimed job comes with a per-job token scoped to that job, and
//!   everything after the claim — the spec fetch, the checkout, the
//!   steps, the miner watch, the verdict — is [`crate::run_with`]. The
//!   agent adds a loop and a credential around it; it does not add a
//!   second way to run a job.
//!
//! **Nothing here holds the claim thread's socket open past a stop.** The
//! claim blocks for up to 25 seconds by design — the server long-polls, so
//! an idle fleet is not a fleet of pollers — but an operator who presses
//! Ctrl-C expects the process to end now, not in twenty seconds. The call
//! therefore runs on a thread the loop watches with a 50 ms granularity,
//! and a stop returns immediately and lets the process exit take the
//! socket with it.

use crate::cli::{self, RegisterOpts, RunOpts};
use crate::client::Tuning;
use crate::log::LogConfig;
use crate::signals;
use crate::steps;
use crate::Config;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

// ------------------------------------------------------------ the .runner

/// What `register` wrote and `run` reads.
///
/// No `Debug`: it holds the runner's long-lived credential.
pub struct Registration {
    pub url: String,
    pub runner_id: String,
    pub name: String,
    pub credential: String,
    pub ephemeral: bool,
    /// The labels the *server* settled on — `self-hosted`, the OS, the
    /// architecture and whatever custom ones were offered — not the ones
    /// that were asked for. What `run` prints has to be what a job would
    /// have to match.
    pub labels: Vec<String>,
}

impl Registration {
    fn to_json(&self) -> String {
        serde_json::json!({
            "url": self.url,
            "runner_id": self.runner_id,
            "name": self.name,
            "credential": self.credential,
            "ephemeral": self.ephemeral,
            "labels": self.labels,
        })
        .to_string()
    }

    fn parse(text: &str) -> Result<Registration, String> {
        let v: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!(".runner is not JSON: {e}"))?;
        Ok(Registration {
            url: string_field(&v, "url", ".runner")?,
            runner_id: string_field(&v, "runner_id", ".runner")?,
            name: string_field(&v, "name", ".runner")?,
            credential: string_field(&v, "credential", ".runner")?,
            ephemeral: v["ephemeral"].as_bool().unwrap_or(false),
            labels: string_list(&v, "labels"),
        })
    }

    fn read(dir: &Path) -> Result<Registration, String> {
        let path = dir.join(".runner");
        let text = std::fs::read_to_string(&path).map_err(|e| {
            format!(
                "cannot read {}: {e}; run `weft-runner register` first",
                path.display()
            )
        })?;
        Registration::parse(&text)
    }

    /// Written 0600 and *set* 0600. The mode on `OpenOptions` applies to a
    /// file being created; a `.runner` left world-readable by an earlier
    /// version, or by an operator's `cp`, would otherwise keep whatever
    /// mode it already had and the credential would stay readable by every
    /// account on the machine.
    fn write(&self, dir: &Path) -> Result<PathBuf, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = dir.join(".runner");
        write_private(&path, &self.to_json())
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(path)
    }
}

fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(body.as_bytes())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

fn string_field(v: &serde_json::Value, key: &str, what: &str) -> Result<String, String> {
    match v[key].as_str() {
        Some(s) => Ok(s.to_string()),
        None => Err(format!("{what}: field {key:?} is missing or not a string")),
    }
}

fn string_list(v: &serde_json::Value, key: &str) -> Vec<String> {
    match v[key].as_array() {
        Some(a) => a
            .iter()
            .filter_map(|e| e.as_str().map(str::to_string))
            .collect(),
        None => Vec::new(),
    }
}

// ------------------------------------------------------------------ HTTP

/// One answer to one POST. The status is kept rather than mapped, because
/// `claim` acts on four of them differently and a client that folded them
/// into "ok / not ok" would have to guess which.
enum Answer {
    Http(u16, String),
    /// The server was not reached, or the answer was cut off mid-body —
    /// the same fact from the caller's side, and the same response: say
    /// so, wait, ask again.
    Transport(String),
}

fn post(agent: &ureq::Agent, url: &str, bearer: &str, body: &str) -> Answer {
    match agent
        .post(url)
        .set("Authorization", &format!("Bearer {bearer}"))
        .set("Content-Type", "application/json")
        .send_string(body)
    {
        Ok(r) => read_body(r),
        // A refusal is an answer, not a failure: `claim` needs the 401 and
        // the 409, and `register` needs to repeat the server's sentence.
        Err(ureq::Error::Status(_, r)) => read_body(r),
        Err(ureq::Error::Transport(t)) => Answer::Transport(t.to_string()),
    }
}

fn read_body(r: ureq::Response) -> Answer {
    let status = r.status();
    match r.into_string() {
        Ok(t) => Answer::Http(status, t),
        Err(e) => Answer::Transport(format!("{status}: the answer was cut off: {e}")),
    }
}

/// The server's own sentence if it sent one, and something an operator can
/// act on if it did not — a proxy's HTML error page reaches here as
/// readily as our JSON, and "401" alone tells nobody anything.
fn sentence(status: u16, body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => match v["error"].as_str() {
            Some(s) => s.to_string(),
            None => format!("{status}: {}", snippet(body)),
        },
        Err(_) => format!("{status}: {}", snippet(body)),
    }
}

fn snippet(body: &str) -> String {
    let flat = body.trim().replace('\n', " ");
    flat.chars().take(200).collect()
}

// -------------------------------------------------------------- register

pub fn register(o: &RegisterOpts) -> i32 {
    match do_register(o, &Tuning::default()) {
        Ok(line) => {
            println!("{line}");
            0
        }
        // 1, not 2: the arguments were fine, the exchange was not. An
        // operator scripting a fleet reads 2 as "I typed it wrong" and 1 as
        // "the server said no", and a token that expired while a machine
        // was booting is the second.
        Err(e) => {
            eprintln!("weft-runner: {e}");
            1
        }
    }
}

fn do_register(o: &RegisterOpts, t: &Tuning) -> Result<String, String> {
    let (os, arch) = cli::platform()?;
    let name = match &o.name {
        Some(n) => n.clone(),
        None => cli::default_name(),
    };
    let base = o.url.trim_end_matches('/').to_string();
    let body = serde_json::json!({
        "name": name,
        "labels": o.labels,
        "os": os,
        "arch": arch,
        "version": env!("CARGO_PKG_VERSION"),
        "ephemeral": o.ephemeral,
    })
    .to_string();
    let agent = ureq::builder()
        .timeout_connect(t.connect_timeout)
        .timeout_read(t.read_timeout)
        .build();
    let url = format!("{base}/v1/runners/register");
    let text = match post(&agent, &url, &o.token, &body) {
        Answer::Http(status, body) if (200..300).contains(&status) => body,
        Answer::Http(status, body) => return Err(sentence(status, &body)),
        Answer::Transport(m) => return Err(format!("cannot reach {base}: {m}")),
    };
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("the registration answer is not JSON: {e}"))?;
    let group = string_field(&v, "group", "the registration answer")?;
    let reg = Registration {
        url: base,
        runner_id: string_field(&v, "runner_id", "the registration answer")?,
        name,
        credential: string_field(&v, "credential", "the registration answer")?,
        ephemeral: o.ephemeral,
        labels: string_list(&v, "labels"),
    };
    reg.write(&o.dir)?;
    Ok(format!(
        "registered {} as {} in group {group} with labels [{}]",
        reg.name,
        reg.runner_id,
        reg.labels.join(", ")
    ))
}

// ------------------------------------------------------------- the agent

/// Cadences. Fields rather than constants for the same reason
/// [`LogConfig`]'s are: proving that an idle runner keeps listening must
/// not take twenty-five seconds per poll.
pub struct Params {
    /// Longer than the server's own claim wait, so the long poll ends
    /// because the server answered rather than because we gave up — a
    /// client timeout mid-claim is indistinguishable here from a broken
    /// network and would be reported as one.
    pub claim_timeout: Duration,
    pub connect_timeout: Duration,
    /// After a claim we could not make sense of.
    pub backoff: Duration,
    /// The floor on one lap of the loop. The server long-polls, so an
    /// empty answer normally costs twenty seconds; against a server that
    /// answers 204 at once — an old build, a proxy with its own idea of
    /// timeouts — this is what stops the runner spinning on the CPU and
    /// the network.
    pub idle_floor: Duration,
    /// How often a sleep or a claim looks at the stop flag.
    pub poll: Duration,
    pub log: LogConfig,
    pub max_procs: u64,
}

impl Default for Params {
    fn default() -> Params {
        Params {
            claim_timeout: Duration::from_secs(25),
            connect_timeout: Duration::from_secs(10),
            backoff: Duration::from_secs(5),
            idle_floor: Duration::from_secs(1),
            poll: Duration::from_millis(50),
            log: LogConfig::default(),
            max_procs: steps::MAX_PROCS,
        }
    }
}

pub fn run(o: &RunOpts) -> i32 {
    start(
        o,
        |key| std::env::var_os(key).map(|v| v.to_string_lossy().into_owned()),
        signals::stop_flag(),
    )
}

/// [`run`], with the environment it reads passed in, so that a test can
/// hand it a malformed knob without writing to the process-wide
/// environment every other test in the suite is reading.
///
/// The knobs are read before `.runner` is, so a service unit with a typo
/// in it fails the same way whether or not the machine has registered.
fn start(o: &RunOpts, env: impl Fn(&str) -> Option<String>, stop: &'static AtomicBool) -> i32 {
    match params_from(env) {
        Ok(p) => run_with(o, &p, stop),
        Err(e) => {
            eprintln!("weft-runner: {e}");
            2
        }
    }
}

/// The defaults, with the two knobs an operator may turn from the
/// environment `run` is started in:
///
/// - `STRATUM_RUNNER_MAX_PROCS`, the ceiling on a step's processes (see
///   [`steps::MAX_PROCS`]) — lowered where a job should have less of the
///   machine, and by the end-to-end suite to prove the bound without
///   forking the machine it runs on into the ground;
/// - `STRATUM_RUNNER_FLUSH_MS`, how often a running job's log is sent.
///
/// Unset and empty mean the same thing, so a service unit can clear one
/// with `Environment=NAME=`. Anything else that is not a whole number
/// above zero is refused, by name, rather than ignored: an operator who
/// meant to lower the ceiling and typoed it would otherwise run with the
/// default and believe they had not. Zero is refused with the rest,
/// because it reads as "no limit" and means the opposite — every fork a
/// step makes, `git`'s own included, would fail — and a flush every 0 ms
/// is a log thread spinning a core.
fn params_from(env: impl Fn(&str) -> Option<String>) -> Result<Params, String> {
    let mut p = Params::default();
    if let Some(n) = knob(&env, "STRATUM_RUNNER_MAX_PROCS")? {
        p.max_procs = n;
    }
    if let Some(ms) = knob(&env, "STRATUM_RUNNER_FLUSH_MS")? {
        p.log.flush = Duration::from_millis(ms);
    }
    Ok(p)
}

fn knob(env: &impl Fn(&str) -> Option<String>, key: &str) -> Result<Option<u64>, String> {
    match env(key).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) => match v.parse::<u64>() {
            Ok(n) if n > 0 => Ok(Some(n)),
            _ => Err(format!(
                "{key} must be a whole number above zero, not {v:?}"
            )),
        },
    }
}

/// The loop, with the flag a signal raises passed in — the same injection,
/// and for the same reason, as [`crate::run_with`]: a real signal in the
/// test process is process-global and would stop every other test's job.
pub fn run_with(o: &RunOpts, p: &Params, stop: &'static AtomicBool) -> i32 {
    let reg = match Registration::read(&o.dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("weft-runner: {e}");
            return 2;
        }
    };
    let agent = ureq::builder()
        .timeout_connect(p.connect_timeout)
        .timeout_read(p.claim_timeout)
        .build();
    // SAFETY: geteuid has no preconditions and cannot fail.
    if let Some(warning) = root_warning(unsafe { libc::geteuid() }) {
        eprintln!("{warning}");
    }
    println!("listening as {} ({})", reg.name, reg.labels.join(", "));
    loop {
        if stop.load(Ordering::SeqCst) {
            return 0;
        }
        let lap = Instant::now();
        match claim(&agent, &reg, p, stop) {
            Claim::Job(job) => {
                println!("took job {}", job.job_id);
                let code = run_job(&reg.url, &o.dir, &job, p, stop);
                match code {
                    0 => println!("finished job {}", job.job_id),
                    _ => println!("finished job {} (the runner exited {code})", job.job_id),
                }
                if stop.load(Ordering::SeqCst) {
                    return 0;
                }
                // One job, then gone: the operator's autoscaler starts the
                // next machine, and nothing from this job can reach it.
                if reg.ephemeral {
                    return 0;
                }
            }
            Claim::Idle => {
                if stop.load(Ordering::SeqCst) {
                    return 0;
                }
                nap(stop, p.idle_floor.saturating_sub(lap.elapsed()), p.poll);
            }
            // The credential is dead and re-asking cannot revive it. 2,
            // the same code a job that never started uses, so a systemd
            // unit with `Restart=on-failure` keeps restarting a runner
            // that lost its network and stops restarting one that was
            // removed.
            Claim::Removed => {
                eprintln!("this runner has been removed; register it again");
                return 2;
            }
            Claim::Trouble(m) => {
                eprintln!("weft-runner: {m}");
                nap(stop, p.backoff, p.poll);
            }
        }
    }
}

/// What an agent started as root says before it takes any work.
///
/// A step runs as whoever started `weft-runner`, and the process ceiling
/// that stops a fork bomb ([`steps::MAX_PROCS`]) is `RLIMIT_NPROC`, which
/// the kernel does not apply to root. So an agent run as root — the
/// default for a service unit that names no `User=` — runs every
/// workflow step with the whole machine and no bound on what it forks.
/// Said at startup, where the person installing it is looking, rather
/// than refused: a disposable build VM is a reasonable place to run as
/// root, and it is the operator's machine to decide about.
fn root_warning(euid: u32) -> Option<&'static str> {
    (euid == 0).then_some(
        "weft-runner: warning: running as root — every workflow step runs as root on \
         this machine, and the process ceiling that stops a fork bomb does not apply \
         to root. Run the agent under an account of its own.",
    )
}

/// A job this runner has been given, and the credentials to do it with.
///
/// No `Debug`: `token` is the per-job credential.
struct Job {
    job_id: String,
    token: String,
}

enum Claim {
    Job(Job),
    /// Nothing to do — 204, or a 409 saying this runner already holds a
    /// job, which is the same instruction: wait and ask again. A 409 is
    /// what a runner that was restarted while a job was in flight sees,
    /// and treating it as an error would put a machine into a printing
    /// loop over a condition that clears when the old lease expires.
    Idle,
    Removed,
    Trouble(String),
}

/// One claim, abandoned the moment a stop arrives.
fn claim(agent: &ureq::Agent, reg: &Registration, p: &Params, stop: &'static AtomicBool) -> Claim {
    let (tx, rx) = std::sync::mpsc::channel();
    let (a, url, credential) = (
        agent.clone(),
        format!("{}/v1/runners/claim", reg.url),
        reg.credential.clone(),
    );
    // The receiver is dropped if we stop first, so the send fails rather
    // than blocking: the thread ends with its answer unread and the
    // process exit closes the socket.
    std::thread::spawn(move || {
        let _ = tx.send(post(&a, &url, &credential, "{}"));
    });
    let answer = loop {
        match rx.recv_timeout(p.poll) {
            Ok(a) => break a,
            Err(RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::SeqCst) {
                    return Claim::Idle;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Claim::Trouble("the claim did not answer".into())
            }
        }
    };
    match answer {
        Answer::Http(200, body) => job_from(&body),
        Answer::Http(204, _) | Answer::Http(409, _) => Claim::Idle,
        Answer::Http(401, _) => Claim::Removed,
        Answer::Http(status, body) => Claim::Trouble(sentence(status, &body)),
        Answer::Transport(m) => Claim::Trouble(m),
    }
}

fn job_from(body: &str) -> Claim {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return Claim::Trouble(format!("the claim answer is not JSON: {e}")),
    };
    // `runner_url` is read deliberately not at all. It is the address the
    // server believes it is reachable at, which is wrong as often as not
    // from somebody else's machine — behind NAT, on the other side of a
    // proxy, or reaching the server by a name only it knows. The URL this
    // runner registered with is the one that has been proved to work
    // from here, so that is the one the job runs against.
    let (Some(job_id), Some(token)) = (v["job_id"].as_str(), v["token"].as_str()) else {
        return Claim::Trouble(format!(
            "the claim answer is missing job_id or token: {}",
            snippet(body)
        ));
    };
    // The job id becomes a path component on the operator's own machine.
    // Nothing in the product produces an id that is not a name, which is
    // exactly why this is checked here rather than trusted: the one case
    // it guards against is a server, or something in front of one, that is
    // not ours, and `work/../../.ssh` is not a directory this runner is
    // going to create on somebody's build box.
    if !is_name(job_id) {
        return Claim::Trouble(format!(
            "the claim answer names a job id that is not a name: {job_id:?}"
        ));
    }
    Claim::Job(Job {
        job_id: job_id.to_string(),
        token: token.to_string(),
    })
}

fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The job itself, in a directory of its own that exists only while it
/// runs.
///
/// Removed before *and* after: nothing tears this machine down between
/// two jobs, so the isolation between them has to be built out of a
/// directory. Before, because a machine that was killed mid-job left the
/// last one's checkout there; after, because the next job on this machine
/// may belong to somebody else and a leftover `.git` with somebody else's
/// credential in its config is exactly what a shared machine must not
/// accumulate.
fn run_job(url: &str, dir: &Path, job: &Job, p: &Params, stop: &'static AtomicBool) -> i32 {
    let work = dir.join("work").join(&job.job_id);
    let _ = std::fs::remove_dir_all(&work);
    let cfg = Config {
        base_url: url.to_string(),
        job_id: job.job_id.clone(),
        token: job.token.clone(),
        workdir: work.clone(),
        tuning: Tuning::default(),
        log: p.log,
        max_procs: p.max_procs,
        clone_via: Some(url.to_string()),
    };
    let code = crate::run_with(&cfg, stop);
    let _ = std::fs::remove_dir_all(&work);
    code
}

/// `clone_url` with its origin replaced by `base`'s.
///
/// The server builds a job's clone URL from its own `STRATUM_RUNNER_URL`
/// setting — a private listener, or `host.docker.internal` on a laptop —
/// which says nothing about what this machine can reach. The one address
/// this machine knows it can reach is the one it registered with, so the
/// repository path is kept and everything before it is swapped for that.
/// Only `http(s)` URLs have an origin to swap; anything else (a path, in
/// the unit tests) is returned unchanged, and so is a URL too malformed to
/// have a path.
pub(crate) fn clone_url_via(base: &str, clone_url: &str) -> String {
    let rest = clone_url
        .strip_prefix("http://")
        .or_else(|| clone_url.strip_prefix("https://"));
    match rest.and_then(|r| r.find('/')) {
        Some(at) => format!(
            "{}{}",
            base.trim_end_matches('/'),
            &rest.expect("prefix matched")[at..]
        ),
        None => clone_url.to_string(),
    }
}

/// Sleep, but notice a stop. Sleeping the whole five seconds would mean a
/// Ctrl-C during a backoff takes five seconds to be answered.
fn nap(stop: &AtomicBool, total: Duration, step: Duration) {
    let deadline = Instant::now() + total;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stop.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(step.min(left));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkout;
    use crate::fakecp::{spec_json, FakeCp, Reply};
    use crate::testdir::TestDir;

    /// A stop flag per test. The process-wide one a real signal raises is
    /// proven against the built binary, where it has a process to itself.
    macro_rules! stop_flag {
        () => {{
            static STOP: AtomicBool = AtomicBool::new(false);
            STOP.store(false, Ordering::SeqCst);
            &STOP
        }};
    }

    /// Root is warned about, by what it costs; nobody else is.
    #[test]
    fn only_an_agent_running_as_root_is_warned() {
        let said = root_warning(0).expect("root is warned");
        assert!(said.contains("running as root"), "{said}");
        assert!(said.contains("process ceiling"), "{said}");
        for uid in [1, 1000, 65534] {
            assert_eq!(root_warning(uid), None, "uid {uid}");
        }
    }

    /// The server's own address is swapped for the one this machine
    /// registered with; the repository path, and anything that is not an
    /// http(s) URL, is left exactly as it came.
    #[test]
    fn a_job_is_cloned_from_the_address_this_machine_registered_with() {
        let via = |base, url| clone_url_via(base, url);
        assert_eq!(
            via(
                "http://127.0.0.1:8090",
                "http://host.docker.internal:8090/acme/builds.git"
            ),
            "http://127.0.0.1:8090/acme/builds.git"
        );
        assert_eq!(
            via(
                "https://stratum.example/",
                "http://10.0.3.7:8080/acme/widget.git"
            ),
            "https://stratum.example/acme/widget.git"
        );
        // Not ours to touch: a path (what the unit tests clone from), a
        // scheme git understands and we do not, an origin with no path.
        assert_eq!(via("http://x", "/tmp/origin"), "/tmp/origin");
        assert_eq!(
            via("http://x", "ssh://git@h/acme/w.git"),
            "ssh://git@h/acme/w.git"
        );
        assert_eq!(via("http://x", "https://h"), "https://h");
    }

    fn params() -> Params {
        Params {
            claim_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_millis(500),
            backoff: Duration::from_millis(20),
            idle_floor: Duration::from_millis(10),
            poll: Duration::from_millis(5),
            log: LogConfig {
                flush: Duration::from_millis(5),
                heartbeat: Duration::from_millis(50),
                ..LogConfig::default()
            },
            max_procs: steps::MAX_PROCS,
        }
    }

    fn opts(url: &str, dir: &Path, token: &str) -> RegisterOpts {
        RegisterOpts {
            url: url.to_string(),
            token: token.to_string(),
            name: Some("box1".into()),
            labels: vec!["gpu".into()],
            ephemeral: false,
            dir: dir.to_path_buf(),
        }
    }

    /// Write a `.runner` without going through the server, for the tests
    /// that are about the loop rather than about registration.
    fn registered(cp: &FakeCp, dir: &Path, ephemeral: bool) -> Registration {
        let r = Registration {
            url: cp.base_url(),
            runner_id: "rnr1".into(),
            name: "box1".into(),
            credential: "weftr_secret".into(),
            ephemeral,
            labels: vec!["self-hosted".into(), "linux".into(), "gpu".into()],
        };
        r.write(dir).expect("write .runner");
        r
    }

    /// A claim answer that hands over a real job on this fake — and names
    /// a `runner_url` that nothing can reach. The job still has to run,
    /// because the URL it runs against is the one in `.runner`; see
    /// `job_from`.
    fn job_reply(_cp: &FakeCp, job_id: &str) -> Reply {
        Reply::body(
            200,
            &serde_json::json!({
                "job_id": job_id, "token": "job-token", "runner_url": "http://127.0.0.1:9",
            })
            .to_string(),
        )
    }

    /// Raise `stop` once the fake has seen `claims` claims, so a test does
    /// not have to guess how long a lap takes.
    fn stop_after_claims(cp: &FakeCp, claims: u32, stop: &'static AtomicBool) {
        let counter = cp.claim_counter();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while counter.load(Ordering::SeqCst) < claims && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            stop.store(true, Ordering::SeqCst);
        });
    }

    // ------------------------------------------------------- registering

    #[test]
    fn registering_writes_a_runner_file_only_its_owner_can_read() {
        let cp = FakeCp::start();
        let dir = TestDir::new("register");
        let d = dir.path().join("agent");
        let line = do_register(&opts(&cp.base_url(), &d, "weftg_token"), &params_tuning())
            .expect("register");
        assert_eq!(
            line,
            "registered box1 as rnr1 in group default with labels [self-hosted, linux, x64, gpu]"
        );

        // The body the server was sent: the platform this binary was built
        // for, its own version, and the labels as typed.
        let sent = cp.state().registered.expect("a registration body");
        assert_eq!(sent["name"], "box1");
        assert_eq!(sent["labels"], serde_json::json!(["gpu"]));
        assert_eq!(sent["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(sent["ephemeral"], false);
        let (os, arch) = cli::platform().expect("platform");
        assert_eq!(sent["os"], os);
        assert_eq!(sent["arch"], arch);
        assert_eq!(
            cp.state().auth_seen,
            vec!["Bearer weftg_token".to_string()],
            "the registration token travels in a header, never in the URL"
        );

        // …and what was written is what `run` reads back, at 0600.
        let path = d.join(".runner");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        let back = Registration::read(&d).expect("read back");
        assert_eq!(back.url, cp.base_url());
        assert_eq!(back.runner_id, "rnr1");
        assert_eq!(back.name, "box1");
        assert_eq!(back.credential, "weftr_secret");
        assert!(!back.ephemeral);
        assert_eq!(back.labels, ["self-hosted", "linux", "x64", "gpu"]);
    }

    fn params_tuning() -> Tuning {
        Tuning {
            retry_base_ms: 1,
            connect_timeout: Duration::from_millis(500),
            read_timeout: Duration::from_secs(5),
        }
    }

    /// A `.runner` that already exists with a loose mode is tightened, not
    /// left as it was: `OpenOptions::mode` applies to creation only.
    #[test]
    fn re_registering_tightens_a_runner_file_that_was_left_readable() {
        let cp = FakeCp::start();
        let dir = TestDir::new("rotate");
        std::fs::write(dir.path().join(".runner"), "{}").expect("write");
        std::fs::set_permissions(
            dir.path().join(".runner"),
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("chmod");
        do_register(
            &opts(&cp.base_url(), dir.path(), "weftg_token"),
            &params_tuning(),
        )
        .expect("register");
        let mode = std::fs::metadata(dir.path().join(".runner"))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    #[test]
    fn a_refused_registration_repeats_the_servers_own_sentence() {
        let cp = FakeCp::start();
        let dir = TestDir::new("refused");
        cp.script_register(vec![Reply::body(
            401,
            "{\"error\":\"that registration token has expired\"}",
        )]);
        assert_eq!(
            do_register(
                &opts(&cp.base_url(), dir.path(), "weftg_old"),
                &params_tuning()
            ),
            Err("that registration token has expired".to_string())
        );
        assert!(
            !dir.path().join(".runner").exists(),
            "a refused registration writes nothing"
        );

        // 422 the same way.
        cp.script_register(vec![Reply::body(
            422,
            "{\"error\":\"labels must be lowercase\"}",
        )]);
        assert_eq!(
            do_register(
                &opts(&cp.base_url(), dir.path(), "weftg_x"),
                &params_tuning()
            ),
            Err("labels must be lowercase".to_string())
        );
    }

    /// A proxy's HTML page reaches this code as readily as our JSON does.
    #[test]
    fn a_refusal_that_is_not_ours_still_says_something_actionable() {
        let cp = FakeCp::start();
        let dir = TestDir::new("proxy");
        cp.script_register(vec![Reply::body(407, "<html>\nproxy auth required</html>")]);
        assert_eq!(
            do_register(
                &opts(&cp.base_url(), dir.path(), "weftg_x"),
                &params_tuning()
            ),
            Err("407: <html> proxy auth required</html>".to_string())
        );

        // …and one that is JSON but not our shape.
        cp.script_register(vec![Reply::body(400, "{\"detail\":\"nope\"}")]);
        assert_eq!(
            do_register(
                &opts(&cp.base_url(), dir.path(), "weftg_x"),
                &params_tuning()
            ),
            Err("400: {\"detail\":\"nope\"}".to_string())
        );
    }

    #[test]
    fn a_server_that_is_not_there_names_the_url_the_operator_typed() {
        let cp = FakeCp::start();
        let dir = TestDir::new("dead");
        let dead = format!("http://127.0.0.1:{}/", cp.free_port());
        let e = do_register(&opts(&dead, dir.path(), "weftg_x"), &params_tuning())
            .expect_err("nothing is listening");
        assert!(
            e.starts_with(&format!("cannot reach {}", dead.trim_end_matches('/'))),
            "{e}"
        );
    }

    #[test]
    fn a_registration_answer_the_runner_cannot_use_is_refused_by_field() {
        let cp = FakeCp::start();
        let dir = TestDir::new("shape");
        cp.script_register(vec![Reply::body(201, "not json")]);
        let e = do_register(&opts(&cp.base_url(), dir.path(), "s"), &params_tuning())
            .expect_err("must refuse");
        assert!(e.starts_with("the registration answer is not JSON:"), "{e}");

        cp.script_register(vec![Reply::body(201, "{\"runner_id\":\"r\"}")]);
        assert_eq!(
            do_register(&opts(&cp.base_url(), dir.path(), "s"), &params_tuning()),
            Err("the registration answer: field \"group\" is missing or not a string".to_string())
        );

        cp.script_register(vec![Reply::body(
            201,
            "{\"group\":\"default\",\"credential\":\"weftr_x\"}",
        )]);
        assert_eq!(
            do_register(&opts(&cp.base_url(), dir.path(), "s"), &params_tuning()),
            Err("the registration answer: field \"runner_id\" is missing or not a string".into())
        );

        cp.script_register(vec![Reply::body(
            201,
            "{\"group\":\"default\",\"runner_id\":\"r\"}",
        )]);
        assert_eq!(
            do_register(&opts(&cp.base_url(), dir.path(), "s"), &params_tuning()),
            Err("the registration answer: field \"credential\" is missing or not a string".into())
        );
    }

    /// A directory nothing can be written into, both ways round.
    #[test]
    fn a_registration_that_cannot_be_stored_is_an_error_not_a_lost_credential() {
        let cp = FakeCp::start();
        let dir = TestDir::new("unwritable");
        std::fs::write(dir.path().join("file"), "").expect("write");
        let e = do_register(
            &opts(&cp.base_url(), &dir.path().join("file/agent"), "s"),
            &params_tuning(),
        )
        .expect_err("must refuse");
        assert!(e.starts_with("cannot create "), "{e}");

        // …and a `.runner` that is a directory: the parent is fine, the
        // file cannot be opened.
        std::fs::create_dir_all(dir.path().join("d/.runner")).expect("mkdir");
        let e = do_register(
            &opts(&cp.base_url(), &dir.path().join("d"), "s"),
            &params_tuning(),
        )
        .expect_err("must refuse");
        assert!(e.starts_with("cannot write "), "{e}");
    }

    /// The exit codes an operator's script reads.
    #[test]
    fn register_exits_zero_when_it_worked_and_one_when_the_server_said_no() {
        let cp = FakeCp::start();
        let dir = TestDir::new("codes");
        assert_eq!(register(&opts(&cp.base_url(), dir.path(), "weftg_x")), 0);
        cp.script_register(vec![Reply::body(401, "{\"error\":\"gone\"}")]);
        assert_eq!(register(&opts(&cp.base_url(), dir.path(), "weftg_x")), 1);
    }

    #[test]
    fn the_default_name_is_used_when_none_was_given() {
        let cp = FakeCp::start();
        let dir = TestDir::new("hostname");
        let mut o = opts(&cp.base_url(), dir.path(), "weftg_x");
        o.name = None;
        let line = do_register(&o, &params_tuning()).expect("register");
        assert!(
            line.starts_with(&format!("registered {} as rnr1", cli::default_name())),
            "{line}"
        );
    }

    // ------------------------------------------------------------ .runner

    #[test]
    fn a_runner_file_that_cannot_be_read_says_what_to_do_about_it() {
        let dir = TestDir::new("missing");
        let e = Registration::read(dir.path())
            .map(|_| ())
            .expect_err("nothing there");
        assert!(
            e.contains(".runner") && e.ends_with("run `weft-runner register` first"),
            "{e}"
        );

        std::fs::write(dir.path().join(".runner"), "{").expect("write");
        let e = Registration::read(dir.path())
            .map(|_| ())
            .expect_err("not JSON");
        assert!(e.starts_with(".runner is not JSON:"), "{e}");

        for (doc, missing) in [
            ("{}", "url"),
            ("{\"url\":\"u\"}", "runner_id"),
            ("{\"url\":\"u\",\"runner_id\":\"r\"}", "name"),
            (
                "{\"url\":\"u\",\"runner_id\":\"r\",\"name\":\"n\"}",
                "credential",
            ),
        ] {
            std::fs::write(dir.path().join(".runner"), doc).expect("write");
            assert_eq!(
                Registration::read(dir.path()).map(|_| ()),
                Err(format!(
                    ".runner: field {missing:?} is missing or not a string"
                ))
            );
        }
    }

    /// An older `.runner`, or one an operator hand-edited: the two
    /// optional fields default rather than refusing the file.
    #[test]
    fn a_runner_file_without_the_optional_fields_still_works() {
        let r = Registration::parse(
            "{\"url\":\"u\",\"runner_id\":\"r\",\"name\":\"n\",\"credential\":\"c\"}",
        )
        .expect("parse");
        assert!(!r.ephemeral);
        assert_eq!(r.labels, Vec::<String>::new());
        // A labels array with something that is not a string in it keeps
        // the strings rather than losing the file.
        let r = Registration::parse(
            "{\"url\":\"u\",\"runner_id\":\"r\",\"name\":\"n\",\"credential\":\"c\",\
             \"ephemeral\":true,\"labels\":[\"gpu\",7]}",
        )
        .expect("parse");
        assert!(r.ephemeral);
        assert_eq!(r.labels, ["gpu"]);
    }

    #[test]
    fn a_run_without_a_registration_exits_two() {
        let dir = TestDir::new("unregistered");
        assert_eq!(
            run(&RunOpts {
                dir: dir.path().to_path_buf()
            }),
            2
        );
    }

    // --------------------------------------------------------- the loop

    /// The whole of the agent in one test: claim, run a real step
    /// against a real checkout, report the verdict, and go back to asking.
    #[test]
    fn the_agent_claims_a_job_runs_it_reports_it_and_asks_again() {
        let cp = FakeCp::start();
        let dir = TestDir::new("loop");
        registered(&cp, dir.path(), false);
        let (url, shas) = checkout::tests::origin(dir.path());
        let mut doc = spec_json(serde_json::json!([{"name": "Read", "run": "cat README.md"}]));
        doc["clone_url"] = serde_json::Value::String(url);
        doc["commit_sha"] = serde_json::Value::String(shas.split(' ').nth(1).expect("sha").into());
        cp.set_spec(&doc);
        cp.script_claim(vec![job_reply(&cp, "job1")]);

        let stop = stop_flag!();
        stop_after_claims(&cp, 2, stop);

        assert_eq!(
            run_with(
                &RunOpts {
                    dir: dir.path().to_path_buf()
                },
                &params(),
                stop
            ),
            0
        );
        let st = cp.state();
        assert!(st.claim_calls >= 2, "it went back to asking: {st:?}");
        assert_eq!(st.finish, Some(("passed".into(), None)));
        assert!(
            st.final_log.expect("log").contains("two"),
            "the step ran inside the checked-out tree"
        );
        assert!(
            st.auth_seen.contains(&"Bearer weftr_secret".to_string()),
            "the claim carries the runner's credential: {:?}",
            st.auth_seen
        );
        assert!(
            st.auth_seen.contains(&"Bearer job-token".to_string()),
            "and the job carries the per-job one: {:?}",
            st.auth_seen
        );
        // The job's directory existed only while the job did.
        assert!(
            !dir.path().join("work/job1").exists(),
            "the workdir was removed"
        );
    }

    /// A machine that was killed mid-job left the last checkout behind.
    /// The next job must not see it — a `.git` with somebody else's
    /// credential in its config is exactly what must not accumulate on a
    /// shared machine.
    #[test]
    fn each_job_gets_a_directory_with_nothing_of_the_last_one_in_it() {
        let cp = FakeCp::start();
        let dir = TestDir::new("fresh");
        registered(&cp, dir.path(), true);
        std::fs::create_dir_all(dir.path().join("work/job1/repo")).expect("mkdir");
        std::fs::write(dir.path().join("work/job1/stale.txt"), "someone else's").expect("write");
        let (url, shas) = checkout::tests::origin(dir.path());
        // The step passes only if the leftover is gone: `..` from the
        // repository is the job's own workdir.
        let mut doc =
            spec_json(serde_json::json!([{"name": "Fresh", "run": "test ! -e ../stale.txt"}]));
        doc["clone_url"] = serde_json::Value::String(url);
        doc["commit_sha"] = serde_json::Value::String(shas.split(' ').nth(1).expect("sha").into());
        cp.set_spec(&doc);
        cp.script_claim(vec![job_reply(&cp, "job1")]);

        let stop = stop_flag!();
        assert_eq!(
            run_with(
                &RunOpts {
                    dir: dir.path().to_path_buf()
                },
                &params(),
                stop
            ),
            0,
            "an ephemeral runner exits 0 after its one job"
        );
        assert_eq!(cp.state().finish, Some(("passed".into(), None)));
        assert_eq!(cp.state().claim_calls, 1, "it did not ask for a second");
        assert!(!dir.path().join("work/job1").exists());
    }

    #[test]
    fn nothing_to_do_keeps_the_runner_listening() {
        let cp = FakeCp::start();
        let dir = TestDir::new("idle");
        registered(&cp, dir.path(), false);
        // 409 is a runner that already holds a job — the same instruction
        // as 204, and not an error to print about.
        cp.script_claim(vec![Reply::body(409, "{\"error\":\"busy\"}")]);
        // Slower than one look at the stop flag, so the wait for the
        // answer really does go round its loop rather than getting the
        // answer on the first try — which is what an idle runner against
        // a long-polling server does all day.
        cp.delay_claims(Duration::from_millis(60));

        let stop = stop_flag!();
        stop_after_claims(&cp, 3, stop);

        assert_eq!(
            run_with(
                &RunOpts {
                    dir: dir.path().to_path_buf()
                },
                &params(),
                stop
            ),
            0
        );
        assert!(cp.state().claim_calls >= 3);
        assert_eq!(cp.state().finish, None, "nothing ran");
    }

    #[test]
    fn a_removed_runner_stops_rather_than_asking_forever() {
        let cp = FakeCp::start();
        let dir = TestDir::new("removed");
        registered(&cp, dir.path(), false);
        cp.script_claim(vec![Reply::body(401, "{\"error\":\"no such runner\"}")]);
        let stop = stop_flag!();
        assert_eq!(
            run_with(
                &RunOpts {
                    dir: dir.path().to_path_buf()
                },
                &params(),
                stop
            ),
            2
        );
        assert_eq!(cp.state().claim_calls, 1, "it did not ask again");
    }

    /// A claim that was cut off mid-body, then one that works. The first
    /// is what a proxy dropping a long poll looks like from here, and the
    /// bug it guards against is a runner that treats it as fatal and stops
    /// a machine for a condition that clears by itself.
    #[test]
    fn a_claim_that_fails_in_transport_is_retried() {
        let cp = FakeCp::start();
        let dir = TestDir::new("retry");
        registered(&cp, dir.path(), true);
        cp.script_claim(vec![
            Reply::truncated(200, "{}", 4096),
            job_reply(&cp, "job1"),
        ]);
        let (url, shas) = checkout::tests::origin(dir.path());
        let mut doc = spec_json(serde_json::json!([{"run": "true"}]));
        doc["clone_url"] = serde_json::Value::String(url);
        doc["commit_sha"] = serde_json::Value::String(shas.split(' ').nth(1).expect("sha").into());
        cp.set_spec(&doc);

        let stop = stop_flag!();
        assert_eq!(
            run_with(
                &RunOpts {
                    dir: dir.path().to_path_buf()
                },
                &params(),
                stop
            ),
            0
        );
        assert_eq!(
            cp.state().claim_calls,
            2,
            "it asked again after the failure"
        );
        assert_eq!(cp.state().finish, Some(("passed".into(), None)));
    }

    /// The other transport arm: nothing is listening at all.
    #[test]
    fn a_server_that_cannot_be_reached_is_waited_out_rather_than_fatal() {
        let cp = FakeCp::start();
        let dir = TestDir::new("unreachable");
        let mut r = registered(&cp, dir.path(), false);
        r.url = format!("http://127.0.0.1:{}", cp.free_port());
        r.write(dir.path()).expect("write");
        let stop = stop_flag!();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            stop.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        assert_eq!(
            run_with(
                &RunOpts {
                    dir: dir.path().to_path_buf()
                },
                &params(),
                stop
            ),
            0,
            "a runner that cannot reach the server is stopped, not failed"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// A claim answer that is 200 but unusable, and the path-traversal
    /// case that is the reason the id is checked at all.
    #[test]
    fn a_claim_answer_that_is_not_a_job_is_refused_rather_than_acted_on() {
        assert!(
            matches!(job_from("{"), Claim::Trouble(m) if m.starts_with("the claim answer is not JSON:"))
        );
        assert!(matches!(
            job_from("{\"job_id\":\"j\"}"),
            Claim::Trouble(m) if m.starts_with("the claim answer is missing job_id or token:")
        ));
        assert!(matches!(
            job_from("{\"job_id\":\"../../etc\",\"token\":\"t\"}"),
            Claim::Trouble(m) if m == "the claim answer names a job id that is not a name: \"../../etc\""
        ));
        // A claim with no `runner_url` at all is still a job: the field is
        // read by nobody, so requiring it would refuse work over a value
        // that could not have been used.
        assert!(matches!(
            job_from("{\"job_id\":\"job-1_A\",\"token\":\"t\"}"),
            Claim::Job(_)
        ));

        assert!(is_name("job1") && is_name("a-b_C9"));
        assert!(!is_name(""));
        assert!(!is_name("../x") && !is_name("a/b") && !is_name("a.b"));
        assert!(!is_name(&"x".repeat(65)));
        assert!(is_name(&"x".repeat(64)));
    }

    /// A 5xx during a claim is neither "removed" nor "nothing to do": say
    /// what the server said, wait, and ask again.
    #[test]
    fn a_server_error_during_a_claim_is_printed_and_retried() {
        let cp = FakeCp::start();
        let dir = TestDir::new("5xx");
        registered(&cp, dir.path(), false);
        cp.script_claim(vec![
            Reply::body(503, "{\"error\":\"the control plane is restarting\"}"),
            Reply::body(500, "not json at all"),
        ]);
        let stop = stop_flag!();
        stop_after_claims(&cp, 3, stop);
        assert_eq!(
            run_with(
                &RunOpts {
                    dir: dir.path().to_path_buf()
                },
                &params(),
                stop
            ),
            0
        );
        assert!(cp.state().claim_calls >= 3);
    }

    /// A job whose verdict could not be delivered still ends the lap, and
    /// the line the operator reads says which job and how it ended.
    #[test]
    fn a_job_the_runner_could_not_report_is_still_one_lap_of_the_loop() {
        let cp = FakeCp::start();
        let dir = TestDir::new("undelivered");
        registered(&cp, dir.path(), true);
        cp.script_claim(vec![job_reply(&cp, "job1")]);
        // The spec fetch itself is refused: `run_with` returns 2 without
        // starting anything.
        cp.script_spec(vec![Reply::status(403); 2]);
        let stop = stop_flag!();
        assert_eq!(
            run_with(
                &RunOpts {
                    dir: dir.path().to_path_buf()
                },
                &params(),
                stop
            ),
            0,
            "the ephemeral runner still ends cleanly"
        );
        assert_eq!(cp.state().finish, None);
    }

    /// A stop that arrives while a job is running ends the *runner*, not
    /// only the job. The job takes the cancel path it already had — the
    /// step's process group is killed and no verdict is reported — and
    /// then the loop has to stop asking for more, rather than picking up
    /// somebody else's job on a machine whose operator has just asked it
    /// to shut down.
    #[test]
    fn a_stop_during_a_job_cancels_it_and_ends_the_run() {
        let cp = FakeCp::start();
        let dir = TestDir::new("stopjob");
        registered(&cp, dir.path(), false);
        let (url, shas) = checkout::tests::origin(dir.path());
        let mut doc =
            spec_json(serde_json::json!([{"name": "Slow", "run": "echo started; sleep 60"}]));
        doc["clone_url"] = serde_json::Value::String(url);
        doc["commit_sha"] = serde_json::Value::String(shas.split(' ').nth(1).expect("sha").into());
        cp.set_spec(&doc);
        cp.script_claim(vec![job_reply(&cp, "job1")]);

        let stop = stop_flag!();
        // Once the step's own output has reached the fake, the job is
        // genuinely running — waiting on a duration instead would race.
        let counter = cp.shared_chunks();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while counter.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            stop.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        assert_eq!(
            run_with(
                &RunOpts {
                    dir: dir.path().to_path_buf()
                },
                &params(),
                stop
            ),
            0
        );
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(40),
            "it waited out the sleep: {waited:?}"
        );
        assert_eq!(
            cp.state().finish,
            None,
            "a stopped job does not answer for itself"
        );
        assert_eq!(
            cp.state().claim_calls,
            1,
            "and the runner did not ask for another"
        );
    }

    #[test]
    fn a_runner_told_to_stop_before_its_first_claim_just_stops() {
        let cp = FakeCp::start();
        let dir = TestDir::new("stopfirst");
        registered(&cp, dir.path(), false);
        let stop = stop_flag!();
        stop.store(true, Ordering::SeqCst);
        assert_eq!(
            run_with(
                &RunOpts {
                    dir: dir.path().to_path_buf()
                },
                &params(),
                stop
            ),
            0
        );
        assert_eq!(cp.state().claim_calls, 0);
    }

    /// The property the whole claim-on-a-thread arrangement exists for.
    /// The server long-polls, so an idle runner spends nearly all of its
    /// life blocked in a claim; a Ctrl-C that had to wait for that claim
    /// to answer would take twenty-five seconds to be obeyed, and an
    /// operator — or a systemd `TimeoutStopSec` — would SIGKILL it long
    /// before then.
    #[test]
    fn a_stop_during_a_claim_ends_the_run_without_waiting_for_the_answer() {
        let cp = FakeCp::start();
        let dir = TestDir::new("stopclaim");
        registered(&cp, dir.path(), false);
        cp.delay_claims(Duration::from_secs(10));
        let stop = stop_flag!();
        stop_after_claims(&cp, 1, stop);

        let started = Instant::now();
        assert_eq!(
            run_with(
                &RunOpts {
                    dir: dir.path().to_path_buf()
                },
                &params(),
                stop
            ),
            0
        );
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(3),
            "it waited out the long poll: {waited:?}"
        );
        assert_eq!(cp.state().claim_calls, 1);
    }

    // -------------------------------------------------------- small parts

    #[test]
    fn a_nap_ends_early_when_a_stop_arrives() {
        static STOP: AtomicBool = AtomicBool::new(false);
        STOP.store(false, Ordering::SeqCst);
        let started = Instant::now();
        nap(&STOP, Duration::from_millis(30), Duration::from_millis(5));
        assert!(started.elapsed() >= Duration::from_millis(25), "it slept");

        STOP.store(true, Ordering::SeqCst);
        let started = Instant::now();
        nap(&STOP, Duration::from_secs(30), Duration::from_millis(5));
        assert!(started.elapsed() < Duration::from_secs(1), "it did not");

        // A budget already spent is not a sleep at all.
        nap(&STOP, Duration::ZERO, Duration::from_millis(5));
        STOP.store(false, Ordering::SeqCst);
    }

    #[test]
    fn the_defaults_are_the_ones_the_contract_names() {
        let p = Params::default();
        assert_eq!(p.claim_timeout, Duration::from_secs(25));
        assert_eq!(p.backoff, Duration::from_secs(5));
        assert_eq!(p.max_procs, steps::MAX_PROCS);
        assert_eq!(sentence(500, ""), "500: ");
        assert_eq!(snippet(&"x".repeat(400)).len(), 200);
    }

    // ------------------------------------------------------------ the knobs

    /// An environment of exactly these variables, so that no test here
    /// writes to the process's own — which every other test in the suite
    /// is reading at the same time.
    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn run_takes_the_process_ceiling_and_the_flush_cadence_from_its_environment() {
        let p = params_from(env_of(&[])).expect("nothing set");
        assert_eq!(p.max_procs, steps::MAX_PROCS);
        assert_eq!(p.log.flush, LogConfig::default().flush);

        let p = params_from(env_of(&[
            ("STRATUM_RUNNER_MAX_PROCS", "64"),
            ("STRATUM_RUNNER_FLUSH_MS", "50"),
        ]))
        .expect("both set");
        assert_eq!(p.max_procs, 64);
        assert_eq!(p.log.flush, Duration::from_millis(50));
        // Those two and nothing else: the claim cadence is not a knob.
        assert_eq!(p.claim_timeout, Params::default().claim_timeout);
        assert_eq!(p.log.heartbeat, LogConfig::default().heartbeat);

        // Set to nothing is unset — what `Environment=NAME=` in a service
        // unit leaves behind — rather than a malformed number.
        let p = params_from(env_of(&[
            ("STRATUM_RUNNER_MAX_PROCS", ""),
            ("STRATUM_RUNNER_FLUSH_MS", ""),
        ]))
        .expect("both cleared");
        assert_eq!(p.max_procs, steps::MAX_PROCS);
        assert_eq!(p.log.flush, LogConfig::default().flush);
    }

    /// Refused by name, never ignored — and zero with the rest, because an
    /// operator who writes `0` for the ceiling means "no limit" and would
    /// get "no forks".
    #[test]
    fn a_knob_that_is_not_a_whole_number_above_zero_is_refused_by_name() {
        for key in ["STRATUM_RUNNER_MAX_PROCS", "STRATUM_RUNNER_FLUSH_MS"] {
            for bad in ["lots", "0", "-5", "1.5", " 64", "99999999999999999999"] {
                let e = params_from(|k| (k == key).then(|| bad.to_string()))
                    .map(|_| ())
                    .expect_err("must refuse");
                assert_eq!(
                    e,
                    format!("{key} must be a whole number above zero, not {bad:?}")
                );
            }
        }
    }

    /// A malformed knob stops `run` before anything else happens: exit 2,
    /// the code a mistyped command line gets, and not one claim — a runner
    /// that took a job with a ceiling its operator did not mean is the
    /// failure the refusal exists for.
    #[test]
    fn a_run_with_a_malformed_knob_exits_two_without_asking_for_work() {
        let cp = FakeCp::start();
        let dir = TestDir::new("badknob");
        registered(&cp, dir.path(), false);
        let stop = stop_flag!();
        assert_eq!(
            start(
                &RunOpts {
                    dir: dir.path().to_path_buf()
                },
                env_of(&[("STRATUM_RUNNER_MAX_PROCS", "lots")]),
                stop
            ),
            2
        );
        assert_eq!(cp.state().claim_calls, 0, "it never asked for work");
    }

    /// …and a well-formed one reaches the step itself. `ulimit -u` is the
    /// shell reporting its own `RLIMIT_NPROC`, so this is the child's view
    /// of the ceiling, not the agent's idea of it.
    #[test]
    fn the_process_ceiling_from_the_environment_reaches_the_step() {
        let cp = FakeCp::start();
        let dir = TestDir::new("knob");
        registered(&cp, dir.path(), true);
        let (url, shas) = checkout::tests::origin(dir.path());
        let mut doc = spec_json(serde_json::json!([
            {"name": "Limits", "run": "ulimit -Su; ulimit -Hu"},
        ]));
        doc["clone_url"] = serde_json::Value::String(url);
        doc["commit_sha"] = serde_json::Value::String(shas.split(' ').nth(1).expect("sha").into());
        cp.set_spec(&doc);
        cp.script_claim(vec![job_reply(&cp, "job1")]);

        let stop = stop_flag!();
        assert_eq!(
            start(
                &RunOpts {
                    dir: dir.path().to_path_buf()
                },
                env_of(&[("STRATUM_RUNNER_MAX_PROCS", "512")]),
                stop
            ),
            0
        );
        let s = cp.state();
        assert_eq!(s.finish, Some(("passed".into(), None)));
        let log = s.final_log.expect("log");
        assert!(log.contains("512\n512\n"), "soft and hard, both 512: {log}");
    }
}
