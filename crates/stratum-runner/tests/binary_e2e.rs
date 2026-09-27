//! The runner as the fleet actually runs it: the built binary, three
//! environment variables, and nothing else.
//!
//! The in-crate tests drive `entry()` in-process, which proves the phases
//! and their composition but not the artefact. What is only true of the
//! binary is the part this suite holds: that `cargo` builds a bin target at
//! all (a crate whose only tests are unit tests can lose its `main` to a
//! refactor and stay green), that `main` turns each verdict into the exit
//! code the dispatcher reads, and that a process started with an empty-ish
//! environment finds everything it needs in those variables.
//!
//! Hermetic, like everything else here: the control plane is a scripted
//! `TcpListener` on loopback and the origin is a real local repository
//! built with the real `git` CLI. Nothing reaches the network.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- the stub

/// What the stub saw. The assertions are all about this: the runner's own
/// stdout is a container log, and the product surface is what reached the
/// control plane.
#[derive(Default)]
struct Seen {
    chunks: Vec<String>,
    final_log: String,
    finish: Option<(String, Option<String>)>,
    /// The verdict's `abuse` field, absent on every job that was not
    /// stopped for it — kept apart from `finish` so a test can tell "not
    /// sent" from "sent as null".
    abuse: Option<String>,
    leases: u32,
    auth: Vec<String>,
    /// The body of `POST /v1/runners/register`, and how many times
    /// `POST /v1/runners/claim` was asked. Self-hosted mode's whole
    /// product surface before a job starts is these two calls.
    register_body: Option<String>,
    claims: u32,
}

/// What the stub has been told to answer the two self-hosted routes with.
/// `register` defaults to a successful exchange and `claims` to 204 —
/// nothing to run — which is what an idle runner sees all day.
#[derive(Default)]
struct Plan {
    register: Option<(u16, String)>,
    claims: VecDeque<(u16, String)>,
}

/// A control plane that answers the four calls a job makes. `spec_status`
/// is scripted so a refusal can be tested; everything else always succeeds,
/// because the retry and cancellation paths are already covered in-crate
/// and what is under test here is the binary.
struct Stub {
    addr: SocketAddr,
    seen: Arc<Mutex<Seen>>,
    plan: Arc<Mutex<Plan>>,
    stop: Arc<AtomicBool>,
}

impl Stub {
    fn start(spec_status: u16, spec_body: String) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let addr = listener.local_addr().expect("addr");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let plan = Arc::new(Mutex::new(Plan::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, p, st) = (Arc::clone(&seen), Arc::clone(&plan), Arc::clone(&stop));
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                if st.load(Ordering::SeqCst) {
                    return;
                }
                serve(conn, spec_status, &spec_body, &s, &p);
            }
        });
        Stub {
            addr,
            seen,
            plan,
            stop,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.seen.lock().expect("stub state")
    }

    fn answer_register(&self, status: u16, body: &str) {
        self.plan.lock().expect("plan").register = Some((status, body.to_string()));
    }

    /// Answers for the claim route, in order; once they run out the
    /// answer is 204.
    fn answer_claims(&self, replies: &[(u16, String)]) {
        self.plan.lock().expect("plan").claims = replies.iter().cloned().collect();
    }

    fn claims(&self) -> u32 {
        self.seen().claims
    }
}

impl Drop for Stub {
    /// The accept loop is blocked in `accept`; setting the flag alone would
    /// leave the thread there for the life of the test binary, so it is
    /// woken with a connection it will drop.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

fn serve(
    mut conn: TcpStream,
    spec_status: u16,
    spec_body: &str,
    seen: &Arc<Mutex<Seen>>,
    plan: &Arc<Mutex<Plan>>,
) {
    let mut reader = BufReader::new(conn.try_clone().expect("clone conn"));
    let mut start = String::new();
    if reader.read_line(&mut start).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut length = 0usize;
    let mut auth = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
        if lower.starts_with("authorization:") {
            auth = line[line.find(':').unwrap_or(0) + 1..].trim().to_string();
        }
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).expect("read body");
    }
    let body = String::from_utf8_lossy(&body).to_string();

    let mut s = seen.lock().expect("stub state");
    s.auth.push(auth);

    // The two routes a runner uses before it has a job. Answered here,
    // ahead of the per-job routing below, because they are org-level and
    // folding them in with the job endpoints is how a mistyped path would
    // quietly be answered by the wrong one.
    if path == "/v1/runners/register" || path == "/v1/runners/claim" {
        let mut p = plan.lock().expect("plan");
        let (status, payload) = if path.ends_with("register") {
            s.register_body = Some(body);
            p.register.clone().unwrap_or((
                201,
                serde_json::json!({
                    "runner_id": "rnr-e2e", "credential": "strr-e2e",
                    "org": "acme", "group": "default",
                    "labels": ["self-hosted", "linux", "x64", "gpu"],
                })
                .to_string(),
            ))
        } else {
            s.claims += 1;
            p.claims.pop_front().unwrap_or((204, String::new()))
        };
        drop(p);
        drop(s);
        write_response(&mut conn, status, &payload);
        return;
    }

    let status = match (method.as_str(), path.rsplit('/').next().unwrap_or_default()) {
        ("POST", "log") => {
            let v: serde_json::Value = serde_json::from_str(&body).expect("chunk is JSON");
            s.chunks
                .push(v["text"].as_str().expect("chunk text").to_string());
            200
        }
        ("PUT", "log") => {
            s.final_log = body;
            200
        }
        ("POST", "finish") => {
            let v: serde_json::Value = serde_json::from_str(&body).expect("verdict is JSON");
            s.finish = Some((
                v["state"].as_str().expect("state").to_string(),
                v["error"].as_str().map(String::from),
            ));
            s.abuse = v["abuse"].as_str().map(String::from);
            200
        }
        ("POST", "lease") => {
            s.leases += 1;
            200
        }
        _ => spec_status,
    };
    drop(s);

    let payload = if status == 200 && method == "GET" {
        spec_body
    } else {
        ""
    };
    write_response(&mut conn, status, payload);
}

fn write_response(conn: &mut TcpStream, status: u16, payload: &str) {
    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(payload.as_bytes());
    let _ = conn.flush();
}

// ------------------------------------------------------------- the fixtures

/// A directory that goes away with the test, so a failed run does not leave
/// a checkout behind in the temp dir.
struct TestDir(PathBuf);

impl TestDir {
    fn new(hint: &str) -> TestDir {
        let p = std::env::temp_dir().join(format!(
            "weft-runner-e2e-{hint}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mkdir");
        TestDir(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The same scrubbed-environment discipline the testkit uses: a developer's
/// own git config cannot change what these prove.
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

/// A real repository on `main` with one commit and a file the steps read,
/// so a step that succeeds proves the checkout landed rather than only that
/// `sh` runs.
fn origin(dir: &Path) -> (String, String) {
    let src = dir.join("origin");
    std::fs::create_dir_all(&src).expect("mkdir");
    run_git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("VERSION"), "from-the-checked-out-tree\n").expect("write");
    run_git(&src, &["add", "-A"]);
    run_git(&src, &["commit", "-q", "-m", "one"]);
    let sha = run_git(&src, &["rev-parse", "HEAD"]).trim().to_string();
    (src.to_string_lossy().to_string(), sha)
}

fn spec_json(clone_url: &str, sha: &str, steps: serde_json::Value) -> String {
    serde_json::json!({
        "id": "job-e2e", "run_id": "run-e2e", "attempt": 1,
        "key": "ci/test", "job": "test",
        "clone_url": clone_url, "fetch_ref": "refs/heads/main",
        "commit_sha": sha, "ref_name": "main", "event": "push",
        "change_key": serde_json::Value::Null,
        "image": "default", "timeout_minutes": 5,
        "env": { "GREETING": "hello" }, "matrix": { "rust": "stable" },
        "steps": steps,
    })
    .to_string()
}

/// Run the built binary against a stub, returning its exit code.
///
/// The environment is cleared rather than inherited: a runner task starts
/// with exactly what the task definition gives it, and a test that leaked
/// the developer's `PATH`-adjacent variables in would not be testing that.
/// `PATH` itself stays, because the image has one and the steps need `sh`.
fn run_binary(stub: &Stub, workdir: &Path) -> i32 {
    run_binary_with(stub, workdir, &[])
}

/// The same, plus whatever else the task definition would have set.
fn run_binary_with(stub: &Stub, workdir: &Path, extra: &[(&str, &str)]) -> i32 {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_weft-runner"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("STRATUM_RUNNER_URL", stub.base_url())
        .env("STRATUM_JOB_ID", "job-e2e")
        .env("STRATUM_JOB_TOKEN", "tok-e2e")
        .env("STRATUM_RUNNER_WORKDIR", workdir)
        // Short, so the streaming assertions do not wait a second per
        // chunk; the default is proven by `log`'s own tests.
        .env("STRATUM_RUNNER_FLUSH_MS", "50");
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn weft-runner");
    // Printed rather than asserted on: it is the container log, and seeing
    // it is what makes a failure here diagnosable at all.
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    out.status.code().expect("runner exited with a code")
}

/// The stub records under a lock the runner's log thread also writes
/// through, so "the verdict is in" is waited for rather than assumed.
fn wait_for_finish(stub: &Stub) -> (String, Option<String>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(f) = stub.seen().finish.clone() {
            return f;
        }
        assert!(Instant::now() < deadline, "no verdict was reported");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------- the tests

#[test]
fn the_binary_runs_a_whole_job_and_reports_it_passed() {
    let dir = TestDir::new("pass");
    let (url, sha) = origin(dir.path());
    let steps = serde_json::json!([
        { "name": "greet", "run": "echo \"$GREETING $WEFT_MATRIX_RUST on $WEFT_SHA\"" },
        { "name": "read the tree", "run": "cat VERSION" },
    ]);
    let stub = Stub::start(200, spec_json(&url, &sha, steps));
    let work = dir.path().join("work");

    let code = run_binary(&stub, &work);

    assert_eq!(code, 0, "a job whose steps all pass exits 0");
    assert_eq!(wait_for_finish(&stub), ("passed".to_string(), None));

    let s = stub.seen();
    assert!(
        s.final_log.contains(&format!("✓ Checkout {sha}")),
        "the checkout is in the log: {}",
        s.final_log
    );
    assert!(
        s.final_log.contains(&format!("hello stable on {sha}")),
        "spec env, matrix and the job's own variables all reach the step: {}",
        s.final_log
    );
    assert!(
        s.final_log.contains("from-the-checked-out-tree"),
        "the step ran inside the checked-out tree: {}",
        s.final_log
    );
    assert!(
        s.final_log.contains("✓ greet (") && s.final_log.contains("✓ read the tree ("),
        "each step is marked done: {}",
        s.final_log
    );
    // Streaming is the point of the chunk endpoint: a person watching a
    // running job sees output before the final PUT.
    assert!(!s.chunks.is_empty(), "the log was streamed while it ran");
    assert!(
        s.final_log.contains(s.chunks[0].trim_end()),
        "streamed chunks are the same text as the final log"
    );
    assert!(
        s.auth.iter().all(|a| a == "Bearer tok-e2e"),
        "every call carries the job token: {:?}",
        s.auth
    );
    // The tree is left where the contract says it is, not somewhere the
    // runner invented.
    assert!(work.join("repo/VERSION").exists());
    assert!(work.join("job.log").exists());
}

#[test]
fn a_failing_step_stops_the_job_and_names_it() {
    let dir = TestDir::new("fail");
    let (url, sha) = origin(dir.path());
    let steps = serde_json::json!([
        { "name": "build", "run": "echo building; exit 3" },
        { "name": "test", "run": "echo never" },
    ]);
    let stub = Stub::start(200, spec_json(&url, &sha, steps));

    let code = run_binary(&stub, &dir.path().join("work"));

    // 0, not 1: the verdict was delivered. A failing build is not a failing
    // runner, and the dispatcher reads the two differently.
    assert_eq!(code, 0);
    assert_eq!(
        wait_for_finish(&stub),
        (
            "failed".to_string(),
            Some("step \"build\" exited 3".to_string())
        )
    );
    let s = stub.seen();
    assert!(
        s.final_log.contains("✗ build exited 3 ("),
        "{}",
        s.final_log
    );
    assert!(
        s.final_log.contains("– test (not run)"),
        "a step after the failure is reported as not run: {}",
        s.final_log
    );
    assert!(
        !s.final_log.contains("never"),
        "and it really did not run: {}",
        s.final_log
    );
}

#[test]
fn a_job_the_control_plane_refuses_exits_two() {
    let dir = TestDir::new("refused");
    let stub = Stub::start(403, String::new());

    let code = run_binary(&stub, &dir.path().join("work"));

    assert_eq!(code, 2, "a job that never started is distinguishable");
    assert!(
        stub.seen().finish.is_none(),
        "nothing is reported for a job that was never fetched"
    );
}

#[test]
fn a_job_that_is_no_longer_running_exits_zero_without_a_verdict() {
    let dir = TestDir::new("gone");
    let stub = Stub::start(410, String::new());

    let code = run_binary(&stub, &dir.path().join("work"));

    // Cancelled or superseded between the launch and the task starting.
    // Exiting 0 is what keeps the failed-task count meaningful.
    assert_eq!(code, 0);
    assert!(stub.seen().finish.is_none());
}

#[test]
fn a_binary_with_no_environment_says_which_variable_is_missing() {
    let out = Command::new(env!("CARGO_BIN_EXE_weft-runner"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("spawn weft-runner");
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("STRATUM_RUNNER_URL is not set"),
        "the operator is told what to set: {err}"
    );
}

/// Not an assertion about the runner so much as about this file: the
/// helpers above must not quietly stop being used, which is how a suite
/// grows a fixture nobody drives.
#[test]
fn the_fixture_builds_a_repository_the_runner_could_check_out() {
    let dir = TestDir::new("fixture");
    let (url, sha) = origin(dir.path());
    assert_eq!(sha.len(), 40, "a real commit sha");
    let refs: BTreeMap<String, String> = run_git(Path::new(&url), &["show-ref"])
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(a, b)| (b.to_string(), a.to_string()))
        .collect();
    assert_eq!(refs.get("refs/heads/main"), Some(&sha));
}

/// A superseded run's task is stopped with `docker stop` / an ECS
/// `StopTask`, which is SIGTERM and then, thirty seconds later, SIGKILL.
/// Before the handler existed the runner had no disposition for it: as a
/// child it died instantly, mid-step, leaving the step's *process group*
/// orphaned — the `sleep` here outlived the runner and kept a container's
/// worth of CPU until the task was torn down — and as PID 1 with no init
/// the signal was ignored outright and the job ran to the end.
///
/// The grandchild is the assertion that matters. A runner that merely dies
/// looks fine from the outside; only the surviving process shows the bug.
#[test]
fn a_sigterm_kills_the_step_group_and_exits_quietly() {
    let dir = TestDir::new("sigterm");
    let (url, sha) = origin(dir.path());
    let work = dir.path().join("work");
    let pidfile = dir.path().join("child.pid");
    // The step backgrounds a long sleep, records its pid where the test can
    // read it, and waits: exactly the shape of a build that has spawned a
    // compiler and is blocked on it.
    let steps = serde_json::json!([
        { "name": "slow", "run": "echo started; sleep 30 & echo $! > \"$PIDFILE\"; wait" },
    ]);
    let mut spec: serde_json::Value =
        serde_json::from_str(&spec_json(&url, &sha, steps)).expect("spec");
    spec["env"] = serde_json::json!({ "PIDFILE": pidfile.to_string_lossy() });
    let stub = Stub::start(200, spec.to_string());

    let mut child = Command::new(env!("CARGO_BIN_EXE_weft-runner"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("STRATUM_RUNNER_URL", stub.base_url())
        .env("STRATUM_JOB_ID", "job-e2e")
        .env("STRATUM_JOB_TOKEN", "tok-e2e")
        .env("STRATUM_RUNNER_WORKDIR", &work)
        .env("STRATUM_RUNNER_FLUSH_MS", "50")
        .spawn()
        .expect("spawn weft-runner");

    let grandchild = wait_for_pid(&pidfile);
    assert!(alive(grandchild), "the step's sleep is running");

    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };

    // Within a second, per the contract: ECS gives thirty, but a runner
    // that needs seconds to let go is one that gets SIGKILLed with its log
    // unflushed.
    let deadline = Instant::now() + Duration::from_secs(2);
    while alive(grandchild) {
        assert!(
            Instant::now() < deadline,
            "the step's process group outlived the runner (pid {grandchild})"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let code = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert_eq!(
        code,
        Some(0),
        "a stopped task is an expected ending, not a failure"
    );
    // The log that was written before the signal is on disk, and no verdict
    // was reported: a task that was stopped does not answer for itself.
    let log = std::fs::read_to_string(work.join("job.log")).expect("job.log");
    assert!(log.contains("▶ slow") && log.contains("started"), "{log}");
    assert!(
        stub.seen().finish.is_none(),
        "no verdict for a stopped task"
    );
}

/// The pid the step wrote, once it has written it.
fn wait_for_pid(pidfile: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = std::fs::read_to_string(pidfile) {
            if let Ok(pid) = s.trim().parse() {
                return pid;
            }
        }
        assert!(Instant::now() < deadline, "the step never started");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `kill(pid, 0)` asks whether a pid is still there without signalling it.
/// A zombie still answers yes, which is why the runner reaps its child.
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_with_timeout(child: &mut std::process::Child, budget: Duration) -> Option<i32> {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait().expect("wait") {
            Some(st) => return st.code(),
            None => assert!(
                Instant::now() < deadline,
                "the runner did not exit after SIGTERM"
            ),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Layer 3 against the artefact: the built binary, its own process, its
/// own two-second sampler. The in-crate test drives the same path, but
/// only here is the watch running inside a `main` that a task definition
/// started — and only here would a `#[cfg(test)]`-shaped mistake, or a
/// sampler that reads `/proc` for the wrong process, show up.
///
/// The miner is `/bin/sleep` under the name `xmrig`: a *symlink*, because
/// a copied system binary is SIGKILLed on sight by macOS for an invalid
/// signature and the test would then pass against a zombie. Nothing that
/// mines anything is in this repository.
#[test]
fn the_binary_stops_a_step_that_starts_a_miner_and_says_it_was_abuse() {
    let dir = TestDir::new("miner");
    let (url, sha) = origin(dir.path());
    let steps = serde_json::json!([
        { "name": "build", "run": "ln -s /bin/sleep ./xmrig; ./xmrig 120 & echo $! > ../miner.pid; wait" },
        { "name": "test", "run": "echo never" },
    ]);
    let stub = Stub::start(200, spec_json(&url, &sha, steps));
    let work = dir.path().join("work");

    let started = Instant::now();
    let code = run_binary(&stub, &work);

    assert_eq!(code, 0, "the verdict was delivered, so the task exits 0");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "it waited out the miner instead of killing it"
    );
    assert_eq!(
        wait_for_finish(&stub),
        (
            "failed".to_string(),
            Some("mining software detected: xmrig".to_string())
        )
    );
    let s = stub.seen();
    assert_eq!(
        s.abuse.as_deref(),
        Some("mining"),
        "the verdict says what kind of failure it was"
    );
    // Killed, not merely reported: the group is gone, so the task stops
    // costing anything the moment the verdict is written.
    let pid: i32 = std::fs::read_to_string(work.join("miner.pid"))
        .expect("the step recorded its miner")
        .trim()
        .parse()
        .expect("a pid");
    assert!(
        unsafe { libc::kill(pid, 0) } != 0,
        "the miner outlived the runner (pid {pid})"
    );
    assert!(
        s.final_log
            .contains("\u{2717} build stopped: mining software detected: xmrig"),
        "{}",
        s.final_log
    );
    assert!(
        !s.final_log.contains("never"),
        "the step after it did not run: {}",
        s.final_log
    );
}

/// How many tasks the uid running this test already has, which is what
/// `RLIMIT_NPROC` counts against the bound. Linux counts *threads*, so the
/// thread listing is what is counted there; on macOS, where processes are
/// what count, it is the process listing.
///
/// The fork-bomb test below sets the runner's ceiling above this rather
/// than at some absolute number: the bound is per-uid and shared with
/// everything else this machine is doing, so an absolute one would either
/// be under the floor — failing the checkout, which forks too — or so far
/// above it that proving the bound would mean actually forking a machine
/// into the ground.
///
/// The headroom over the measured count is deliberately large. This is
/// one reading of a number that keeps moving: `RLIMIT_NPROC` is checked
/// against the uid's count at the moment of each `fork`, the tests in
/// this file run in parallel threads, and the sibling test below puts a
/// hundred processes on the same uid on purpose. A tight headroom means
/// that whenever the two overlap it is the fork-bomb test's *checkout*
/// that is refused its forks, failing as "fetch of refs/heads/main
/// failed" — the same failure as a ceiling set too low, arriving by a
/// different door, and likeliest under llvm-cov where everything is slow
/// enough to overlap. 300 is far more than the checkout and the sibling
/// together can take, and still well short of what the step below
/// attempts, so the bomb is refused in milliseconds either way.
fn tasks_now() -> u64 {
    let uid = unsafe { libc::getuid() }.to_string();
    // Not a fallback chain: BSD `ps` answers `-L` by printing the list of
    // format keywords and exiting 0, so "try the Linux spelling first"
    // would silently count nothing and set the ceiling below the
    // checkout's own needs.
    let args: &[&str] = if cfg!(target_os = "linux") {
        &["-eLo", "uid="]
    } else {
        &["-Ao", "uid="]
    };
    let listing = Command::new("ps").args(args).output().expect("ps");
    String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter(|l| l.trim() == uid)
        .count() as u64
}

/// The process ceiling, against the built binary. A fork bomb is the one
/// thing a step can do that costs somebody other than its own job: it
/// takes the host down, and every task sharing it. Fargate accepts only
/// the `nofile` ulimit, so the bound cannot live in the task definition
/// and lives in the runner instead — which means this is the only place
/// it can be proven.
///
/// Two things are asserted, and the second is the point. The step fails,
/// *as the step*: the shell's own "fork: Resource temporarily
/// unavailable" reaches the job log and the verdict names the step, so
/// the author sees a step they can fix rather than a runner that broke.
/// And the runner itself is unharmed — it reports the verdict and exits
/// 0, which is what stops a bomb from taking the fleet's dispatcher with
/// it.
///
/// Nothing here forks a machine into the ground: the ceiling is set a few
/// hundred above what this uid already has, so the loop is refused a fork
/// long before it reaches its two-thousandth and bash gives up on the
/// spot. `sleep 5`, not a tenth of a second, so that the processes it does
/// start are still alive while the rest are being refused: a sleep short
/// enough to exit under the loop would free slots as fast as they were
/// taken, and the bound would never be reached.
#[test]
fn the_binary_bounds_a_step_that_tries_to_fork_without_end() {
    let dir = TestDir::new("forkbomb");
    let (url, sha) = origin(dir.path());
    let steps = serde_json::json!([
        { "name": "build", "run": "for i in $(seq 1 2000); do /bin/sleep 5 & done; wait; echo unbounded" },
        { "name": "test", "run": "echo never" },
    ]);
    let stub = Stub::start(200, spec_json(&url, &sha, steps));
    let ceiling = (tasks_now() + 300).to_string();

    let started = Instant::now();
    let code = run_binary_with(
        &stub,
        &dir.path().join("work"),
        &[("STRATUM_RUNNER_MAX_PROCS", &ceiling)],
    );

    assert_eq!(code, 0, "the runner survived it and reported a verdict");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "it waited the forks out instead of being refused them"
    );
    let (state, error) = wait_for_finish(&stub);
    assert_eq!(state, "failed");
    let error = error.expect("a failed job says why");
    assert!(
        error.starts_with("step \"build\" exited"),
        "the step's own failure, not the runner's: {error}"
    );
    let s = stub.seen();
    assert!(
        s.abuse.is_none(),
        "a fork bomb is a failed job, not a suspended organisation"
    );
    assert!(
        !s.final_log.contains("unbounded"),
        "the loop never finished: {}",
        s.final_log
    );
    assert!(
        !s.final_log.contains("never"),
        "and the step after it did not run: {}",
        s.final_log
    );
}

/// The half that decides whether the bound is usable at all. A hundred
/// processes at once is an ordinary parallel build, not an attack, and
/// the default ceiling has to let it through — a bound that failed honest
/// work would be a worse bug than the one it prevents, and it would be
/// discovered by somebody's build rather than by us.
#[test]
fn a_step_that_forks_a_hundred_processes_is_not_a_fork_bomb() {
    let dir = TestDir::new("fanout");
    let (url, sha) = origin(dir.path());
    let steps = serde_json::json!([
        { "name": "build", "run": "for i in $(seq 1 100); do /bin/sleep 0.2 & done; wait; echo fanned out" },
    ]);
    let stub = Stub::start(200, spec_json(&url, &sha, steps));

    let code = run_binary(&stub, &dir.path().join("work"));

    assert_eq!(code, 0);
    assert_eq!(wait_for_finish(&stub), ("passed".to_string(), None));
    assert!(
        stub.seen().final_log.contains("fanned out"),
        "{}",
        stub.seen().final_log
    );
}

// ------------------------------------------------------- self-hosted mode

/// Run the built binary with arguments rather than an environment: the
/// self-hosted commands take everything they need from the command line
/// and from `.runner`, which is the point — a machine an operator owns has
/// no task definition to put variables in.
fn run_agent(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_weft-runner"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .args(args)
        .output()
        .expect("spawn weft-runner")
}

/// The `.runner` a successful registration would have left, written
/// directly for the tests that are about the loop rather than about the
/// exchange.
fn write_runner_file(dir: &Path, stub: &Stub, ephemeral: bool) {
    std::fs::create_dir_all(dir).expect("mkdir");
    std::fs::write(
        dir.join(".runner"),
        serde_json::json!({
            "url": stub.base_url(),
            "runner_id": "rnr-e2e",
            "name": "box1",
            "credential": "strr-e2e",
            "ephemeral": ephemeral,
            "labels": ["self-hosted", "linux", "x64", "gpu"],
        })
        .to_string(),
    )
    .expect("write .runner");
}

/// A claim answer that hands over a job — and points somewhere useless.
///
/// The `runner_url` a claim carries is the server's own public URL, which
/// is what *it* is reachable at, not what this machine can reach: a runner
/// in a container behind NAT is handed `127.0.0.1:8080` and cannot use it.
/// The URL the runner registered with is the one that has actually been
/// proved to work, so the job goes there and this field is ignored. The
/// bogus value is what proves it: port 9 refuses every connection, so a
/// runner that believed the answer could not fetch a spec at all.
fn claim_reply(_stub: &Stub, job_id: &str) -> (u16, String) {
    (
        200,
        serde_json::json!({
            "job_id": job_id, "token": "tok-e2e", "runner_url": "http://127.0.0.1:9",
        })
        .to_string(),
    )
}

/// The registration exchange, against the artefact. What is only true here
/// is that the binary has the subcommand at all, that it detects the
/// platform it was *built* for, and that the credential it writes down is
/// not readable by every other account on the machine — a `.runner` at
/// 0644 on a shared build box is the whole credential, handed over.
#[test]
fn registering_writes_a_private_runner_file_and_says_what_it_registered() {
    let dir = TestDir::new("register");
    let stub = Stub::start(200, String::new());
    let agent = dir.path().join("agent");

    let out = run_agent(
        &[
            "register",
            "--url",
            &stub.base_url(),
            "--token",
            "weftg_e2e",
            "--name",
            "box1",
            "--labels",
            "GPU, gpu ,big-mem",
        ]
        .iter()
        .copied()
        .chain(["--dir", agent.to_str().expect("path")])
        .collect::<Vec<_>>(),
    );

    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "registered box1 as rnr-e2e in group default with labels [self-hosted, linux, x64, gpu]"
    );

    let sent: serde_json::Value =
        serde_json::from_str(&stub.seen().register_body.clone().expect("a body")).expect("JSON");
    assert_eq!(sent["name"], "box1");
    assert_eq!(
        sent["labels"],
        serde_json::json!(["gpu", "big-mem"]),
        "lowercased, trimmed and deduplicated before they are sent"
    );
    assert_eq!(sent["ephemeral"], false);
    assert!(sent["os"].is_string() && sent["arch"].is_string(), "{sent}");
    assert_eq!(
        sent["version"], "0.1.0",
        "the server records which runner build this is"
    );
    assert_eq!(
        stub.seen().auth.first().map(String::as_str),
        Some("Bearer weftg_e2e"),
        "the registration token travels in a header, never in the URL"
    );

    let mode = std::fs::metadata(agent.join(".runner"))
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    let stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(agent.join(".runner")).expect("read"))
            .expect("JSON");
    assert_eq!(stored["credential"], "strr-e2e");
    assert_eq!(stored["url"], stub.base_url());
}

#[test]
fn a_registration_the_server_refuses_exits_one_and_repeats_its_sentence() {
    let dir = TestDir::new("register-refused");
    let stub = Stub::start(200, String::new());
    stub.answer_register(401, "{\"error\":\"that registration token has expired\"}");

    let out = run_agent(&[
        "register",
        "--url",
        &stub.base_url(),
        "--token",
        "weftg_old",
        "--dir",
        dir.path().to_str().expect("path"),
    ]);

    // 1, not 2: the arguments were right and the server said no.
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("that registration token has expired"),
        "the operator is told what the server said: {err}"
    );
    assert!(
        !dir.path().join(".runner").exists(),
        "a refused registration leaves nothing behind"
    );
}

#[test]
fn a_mistyped_command_line_is_a_usage_error_that_prints_the_usage() {
    let out = run_agent(&["register", "--url"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--url needs a value"), "{err}");
    assert!(err.contains("weft-runner run [--dir DIR]"), "{err}");

    let out = run_agent(&["--help"]);
    assert_eq!(out.status.code(), Some(0), "asking is not a usage error");
    assert!(String::from_utf8_lossy(&out.stdout).contains("weft-runner register --url URL"));
}

/// The whole of `run`, against the artefact: read the credential, claim,
/// check out a real repository, run a real step, report the verdict, and —
/// because this runner is ephemeral — stop.
#[test]
fn the_agent_claims_a_job_runs_it_on_this_machine_and_reports_the_verdict() {
    let dir = TestDir::new("agent-job");
    let (url, sha) = origin(dir.path());
    let steps = serde_json::json!([
        { "name": "read the tree", "run": "cat VERSION" },
    ]);
    let stub = Stub::start(200, spec_json(&url, &sha, steps));
    let agent = dir.path().join("agent");
    write_runner_file(&agent, &stub, true);
    stub.answer_claims(&[claim_reply(&stub, "job-e2e")]);

    let out = run_agent(&["run", "--dir", agent.to_str().expect("path")]);

    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("listening as box1 (self-hosted, linux, x64, gpu)"),
        "it says what a job would have to match: {said}"
    );
    assert!(said.contains("took job job-e2e"), "{said}");
    assert!(said.contains("finished job job-e2e"), "{said}");

    assert_eq!(wait_for_finish(&stub), ("passed".to_string(), None));
    let s = stub.seen();
    assert!(
        s.final_log.contains("from-the-checked-out-tree"),
        "the step ran inside the checked-out tree: {}",
        s.final_log
    );
    assert!(
        s.auth.contains(&"Bearer strr-e2e".to_string()),
        "the claim carried the runner's own credential: {:?}",
        s.auth
    );
    assert!(
        s.auth.contains(&"Bearer tok-e2e".to_string()),
        "and the job carried the per-job one: {:?}",
        s.auth
    );
    // The job's directory existed only while the job did: a leftover
    // checkout is the next person's job reading somebody else's tree.
    assert!(
        !agent.join("work/job-e2e").exists(),
        "the workdir was removed"
    );
}

#[test]
fn a_runner_that_was_removed_says_so_and_exits_two() {
    let dir = TestDir::new("agent-removed");
    let stub = Stub::start(200, String::new());
    write_runner_file(dir.path(), &stub, false);
    stub.answer_claims(&[(401, "{\"error\":\"no such runner\"}".to_string())]);

    let out = run_agent(&["run", "--dir", dir.path().to_str().expect("path")]);

    // 2 rather than 1, so a systemd unit with `Restart=on-failure` keeps
    // restarting a runner that lost its network and stops restarting one
    // whose credential an operator revoked.
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("this runner has been removed; register it again"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_run_without_a_registration_says_which_command_to_run_first() {
    let dir = TestDir::new("agent-unregistered");
    let out = run_agent(&["run", "--dir", dir.path().to_str().expect("path")]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("run `weft-runner register` first"), "{err}");
}

/// An idle runner is blocked in a long poll almost all of the time, and
/// this is the ending that has to be quick. A runner that finished its
/// poll before noticing Ctrl-C would take twenty-five seconds to stop, and
/// an operator — or a systemd `TimeoutStopSec` — would SIGKILL it first.
#[test]
fn a_sigterm_while_idle_stops_the_agent_promptly() {
    let dir = TestDir::new("agent-idle-term");
    let stub = Stub::start(200, String::new());
    write_runner_file(dir.path(), &stub, false);

    let mut child = Command::new(env!("CARGO_BIN_EXE_weft-runner"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .args(["run", "--dir", dir.path().to_str().expect("path")])
        .spawn()
        .expect("spawn weft-runner");

    // Wait until it is genuinely in the loop rather than still starting.
    let deadline = Instant::now() + Duration::from_secs(10);
    while stub.claims() == 0 {
        assert!(Instant::now() < deadline, "the agent never asked for a job");
        std::thread::sleep(Duration::from_millis(20));
    }

    let started = Instant::now();
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(
        wait_with_timeout(&mut child, Duration::from_secs(5)),
        Some(0),
        "an operator stopping a runner is an expected ending"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "it waited out the poll instead of letting go of it: {:?}",
        started.elapsed()
    );
    assert!(stub.seen().finish.is_none(), "nothing ran");
}

/// The same signal, arriving while a job is running on somebody's own
/// machine. The grandchild is the assertion that matters, as it is for the
/// hosted runner: a runner that merely dies looks fine from the outside,
/// and only the surviving process shows that the step's group was orphaned
/// — on a self-hosted runner, on hardware the operator keeps using.
#[test]
fn a_sigterm_during_a_self_hosted_job_kills_the_step_group_and_reports_nothing() {
    let dir = TestDir::new("agent-job-term");
    let (url, sha) = origin(dir.path());
    let pidfile = dir.path().join("child.pid");
    let steps = serde_json::json!([
        { "name": "slow", "run": "echo started; sleep 30 & echo $! > \"$PIDFILE\"; wait" },
    ]);
    let mut spec: serde_json::Value =
        serde_json::from_str(&spec_json(&url, &sha, steps)).expect("spec");
    spec["env"] = serde_json::json!({ "PIDFILE": pidfile.to_string_lossy() });
    let stub = Stub::start(200, spec.to_string());
    let agent = dir.path().join("agent");
    write_runner_file(&agent, &stub, false);
    stub.answer_claims(&[claim_reply(&stub, "job-e2e")]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_weft-runner"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .args(["run", "--dir", agent.to_str().expect("path")])
        .spawn()
        .expect("spawn weft-runner");

    let grandchild = wait_for_pid(&pidfile);
    assert!(alive(grandchild), "the step's sleep is running");

    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };

    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(grandchild) {
        assert!(
            Instant::now() < deadline,
            "the step's process group outlived the runner (pid {grandchild})"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        wait_with_timeout(&mut child, Duration::from_secs(5)),
        Some(0),
        "a stopped runner is an expected ending, not a failure"
    );
    assert!(
        stub.seen().finish.is_none(),
        "a job that was stopped does not answer for itself"
    );
}
