//! The runner as an operator actually runs it: the built binary, a command
//! line, a `.runner` file, and an environment with nothing else in it.
//!
//! The in-crate tests drive the agent loop and the job in-process, which
//! proves the phases and their composition but not the artefact. What is
//! only true of the binary is the part this suite holds: that `cargo`
//! builds a bin target at all (a crate whose only tests are unit tests can
//! lose its `main` to a refactor and stay green), that `main` turns each
//! ending into the exit code and the line an operator reads, that a real
//! signal reaches a real process, and that a process started with an empty
//! environment finds everything it needs in its arguments and `.runner`.
//!
//! Hermetic, like everything else here: the control plane is a scripted
//! `TcpListener` on loopback and the origin is a real local repository
//! built with the real `git` CLI. Nothing reaches the network.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- the stub

/// What the stub saw. Most assertions are about this: the runner's own
/// output is narration for the operator, and the product surface is what
/// reached the control plane.
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
    /// The `Authorization` of every call, in order…
    auth: Vec<String>,
    /// …and of the per-job ones alone: the spec, the log, the lease, the
    /// verdict. Kept apart so a test can say every one of them carried the
    /// job's token and not the runner's.
    job_auth: Vec<String>,
    /// The body of `POST /v1/runners/register`, and how many times
    /// `POST /v1/runners/claim` was asked. The agent's whole product
    /// surface before a job starts is these two calls.
    register_body: Option<String>,
    claims: u32,
}

/// What the stub has been told to answer the two runner routes with.
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
    s.auth.push(auth.clone());

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
    s.job_auth.push(auth);

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

/// Run the built binary with arguments and an otherwise empty environment:
/// everything the runner needs comes from the command line and from
/// `.runner`, which is the point — a test that leaked the developer's own
/// variables in would not be testing that. `PATH` stays, because every
/// machine has one and the steps need `bash`.
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
/// The `runner_url` a claim carries is the server's own idea of where it
/// can be reached, not what this machine can reach: a runner in a
/// container behind NAT is handed `127.0.0.1:8080` and cannot use it. The
/// URL the runner registered with is the one that has actually been proved
/// to work, so the job goes there and this field is ignored. The bogus
/// value is what proves it: port 9 refuses every connection, so a runner
/// that believed the answer could not fetch a spec at all.
fn claim_reply(_stub: &Stub, job_id: &str) -> (u16, String) {
    (
        200,
        serde_json::json!({
            "job_id": job_id, "token": "tok-e2e", "runner_url": "http://127.0.0.1:9",
        })
        .to_string(),
    )
}

/// An ephemeral registration under `dir/agent`, and the stub told to hand
/// it `job-e2e` on its first claim. Ephemeral so that `run` ends once the
/// job does, which lets a test wait for the process rather than guess at a
/// duration.
fn agent_with_one_job(dir: &Path, stub: &Stub) -> PathBuf {
    let agent = dir.join("agent");
    write_runner_file(&agent, stub, true);
    stub.answer_claims(&[claim_reply(stub, "job-e2e")]);
    agent
}

/// `weft-runner run` over the registration in `agent`, with an empty
/// environment but for `PATH`, a short log flush and `extra`.
fn agent_command(agent: &Path, extra: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_weft-runner"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        // Short, so the streaming assertions do not wait a second per
        // chunk; the default is proven by `log`'s own tests.
        .env("STRATUM_RUNNER_FLUSH_MS", "50")
        .args(["run", "--dir", agent.to_str().expect("path")]);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd
}

/// Run a command to its end and hand back what it said. Stderr is printed
/// rather than asserted on: it is the agent's narration, and seeing it is
/// what makes a failure here diagnosable at all.
fn finish_run(mut cmd: Command) -> std::process::Output {
    let out = cmd.output().expect("spawn weft-runner");
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    out
}

/// The line the agent prints once a job is over. `code` is what the job
/// itself ended with — 0 prints no code at all, anything else is named —
/// and it is the only place that code survives: an ephemeral agent exits
/// 0 after its one job however that job went.
fn finished_line(out: &std::process::Output, code: i32) {
    let said = String::from_utf8_lossy(&out.stdout);
    let want = match code {
        0 => "finished job job-e2e".to_string(),
        n => format!("finished job job-e2e (the runner exited {n})"),
    };
    assert!(
        said.lines().any(|l| l == want),
        "expected the line {want:?}: {said}"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "an ephemeral agent that has done its one job exits 0"
    );
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

/// Whether `pid` is still running.
///
/// Not `kill(pid, 0)` alone, which also answers for a zombie. A step's
/// grandchild is reparented to PID 1 when the step's shell dies, and stays
/// a zombie until PID 1 reaps it — which some inits do on a timer: the one
/// this suite was first run under took about two seconds, and every
/// assertion made straight after the kill failed on it. What the runner
/// promises is that the group is killed; how promptly somebody else's init
/// tidies up is not its to promise. (The in-crate twin is
/// `steps::tests::stopped`; this suite is a separate crate and cannot
/// share it.)
fn alive(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    !String::from_utf8_lossy(&out.stdout)
        .trim_start()
        .starts_with('Z')
}

/// `!alive(pid)`, given `budget` to come true: a group-wide SIGKILL is
/// delivered to each process as it is next scheduled, not all at once.
fn stops_within(pid: i32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
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

/// `nobody`, numerically: it need not be in `/etc/passwd` to be a uid the
/// kernel counts processes against.
const NOBODY: u32 = 65534;

/// The account a fork-bound test runs the agent as.
///
/// `RLIMIT_NPROC` does not bind root: the kernel does not enforce it for
/// root, nor for a process holding `CAP_SYS_RESOURCE` or `CAP_SYS_ADMIN`.
/// A suite run as root — a container, a CI image — would watch a fork bomb
/// succeed and blame the runner, and watch an honest fan-out pass without
/// the bound ever having been in play. So as root the agent runs as `nobody`, and
/// the bound is proven under the condition the kernel enforces it in —
/// which is also how an operator ought to run the agent. Anyone else runs
/// it as themselves.
fn bounded_account() -> u32 {
    match unsafe { libc::geteuid() } {
        0 => NOBODY,
        me => me,
    }
}

/// `cmd`, set to run as `uid`, with `dir` handed to that account first
/// when it is not the one running the test.
///
/// The whole directory, the origin included, and not only the agent's:
/// `git` refuses to fetch from a repository another account owns, and the
/// runner's `git` runs with its environment cleared to three variables, so
/// there is no way to hand it `safe.directory` from here.
fn run_as(cmd: &mut Command, dir: &Path, uid: u32) {
    if uid == unsafe { libc::geteuid() } {
        return;
    }
    let chown = Command::new("chown")
        .arg("-R")
        .arg(format!("{uid}:{uid}"))
        .arg(dir)
        .status()
        .expect("spawn chown");
    assert!(chown.success(), "chown -R {}", dir.display());
    cmd.uid(uid).gid(uid);
}

/// How many tasks `uid` already has, which is what `RLIMIT_NPROC` counts
/// against the bound. Linux counts *threads*, so the thread listing is
/// what is counted there; on macOS, where processes are what count, it is
/// the process listing.
///
/// The fork-bomb test below sets the runner's ceiling above this rather
/// than at some absolute number: the bound is per-uid and shared with
/// everything else that account is doing, so an absolute one would either
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
fn tasks_now(uid: u32) -> u64 {
    let uid = uid.to_string();
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

// ---------------------------------------------------------------- the tests

/// The helper the signal and miner tests lean on, held to the case it
/// exists for: a child this test killed and has not yet reaped is a
/// zombie, which `kill(pid, 0)` still answers for and which has
/// nonetheless stopped.
#[test]
fn a_killed_process_nobody_has_reaped_yet_is_not_alive() {
    let mut child = Command::new("sleep").arg("30").spawn().expect("spawn");
    let pid = child.id() as i32;
    assert!(
        !stops_within(pid, Duration::from_millis(60)),
        "a running sleep is alive"
    );
    unsafe { libc::kill(pid, libc::SIGKILL) };
    assert!(stops_within(pid, Duration::from_secs(5)), "it was killed");
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "and it is a zombie rather than gone, which is the point"
    );
    child.wait().expect("reap");
    assert!(!alive(pid), "and once reaped it is gone");
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

/// No command at all. There is no default to fall back on — a machine
/// that has not registered has nothing to ask for work with — so the
/// operator who types the bare name is shown what it does, as a usage
/// error: the same 2 and the same usage a mistyped flag gets.
#[test]
fn a_bare_weft_runner_prints_the_usage_and_exits_two() {
    let out = run_agent(&[]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("a command is needed: register or run"),
        "the operator is told what is missing: {err}"
    );
    assert!(
        err.contains("weft-runner register --url URL")
            && err.contains("weft-runner run [--dir DIR]"),
        "and shown both commands: {err}"
    );
    assert!(out.stdout.is_empty(), "a usage error is not output");
}

/// The whole of `run`, against the artefact: read the credential, claim,
/// check out a real repository, run real steps, stream the log, report the
/// verdict, and — because this runner is ephemeral — stop.
#[test]
fn the_agent_claims_a_job_runs_it_on_this_machine_and_reports_the_verdict() {
    let dir = TestDir::new("agent-job");
    let (url, sha) = origin(dir.path());
    let steps = serde_json::json!([
        { "name": "greet", "run": "echo \"$GREETING $WEFT_MATRIX_RUST on $WEFT_SHA\"" },
        { "name": "read the tree", "run": "cat VERSION; pwd" },
    ]);
    let stub = Stub::start(200, spec_json(&url, &sha, steps));
    let agent = agent_with_one_job(dir.path(), &stub);
    // Where the job's tree has to be, as the step's own `pwd` will print
    // it: physical, because that is what the kernel hands a shell.
    let tree = agent
        .canonicalize()
        .expect("the agent directory exists")
        .join("work/job-e2e/repo");

    let out = finish_run(agent_command(&agent, &[]));

    finished_line(&out, 0);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("listening as box1 (self-hosted, linux, x64, gpu)"),
        "it says what a job would have to match: {said}"
    );
    assert!(said.contains("took job job-e2e"), "{said}");

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
    // …and the tree is where the agent keeps a job's work, not somewhere
    // it invented: `DIR/work/<job>/repo`.
    assert!(
        s.final_log.contains(&format!("{}\n", tree.display())),
        "the tree was checked out under {}: {}",
        tree.display(),
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
        s.auth.contains(&"Bearer strr-e2e".to_string()),
        "the claim carried the runner's own credential: {:?}",
        s.auth
    );
    assert!(
        !s.job_auth.is_empty() && s.job_auth.iter().all(|a| a == "Bearer tok-e2e"),
        "and every call about the job carried the per-job one: {:?}",
        s.job_auth
    );
    // The job's directory existed only while the job did: a leftover
    // checkout is the next person's job reading somebody else's tree.
    assert!(
        !agent.join("work/job-e2e").exists(),
        "the workdir was removed"
    );
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
    let agent = agent_with_one_job(dir.path(), &stub);

    let out = finish_run(agent_command(&agent, &[]));

    // The job ended 0, not 1: the verdict was delivered. A failing build
    // is not a failing runner, and the operator reads the two differently.
    finished_line(&out, 0);
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

/// A job whose token the control plane refuses never starts, and the
/// agent says so with the job's own code — 2 — beside it, so an operator
/// can tell "the job never started" from "the job ran and failed". The
/// agent itself is fine, and an ephemeral one still exits 0.
#[test]
fn a_job_the_control_plane_refuses_is_reported_as_never_started() {
    let dir = TestDir::new("refused");
    let stub = Stub::start(403, String::new());
    let agent = agent_with_one_job(dir.path(), &stub);

    let out = finish_run(agent_command(&agent, &[]));

    finished_line(&out, 2);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("job job-e2e: refused: 403"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stub.seen().finish.is_none(),
        "nothing is reported for a job that was never fetched"
    );
}

#[test]
fn a_job_that_is_no_longer_running_ends_quietly_without_a_verdict() {
    let dir = TestDir::new("gone");
    let stub = Stub::start(410, String::new());
    let agent = agent_with_one_job(dir.path(), &stub);

    let out = finish_run(agent_command(&agent, &[]));

    // Cancelled or superseded between the claim and the fetch. An ending
    // nobody has to act on, so the job's code is 0 and no code is shown.
    finished_line(&out, 0);
    let s = stub.seen();
    assert!(s.finish.is_none(), "no verdict for a job that is over");
    assert!(s.chunks.is_empty(), "and no work was done for it");
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

/// An operator stopping the runner while a job is running on it: Ctrl-C,
/// `systemctl stop`, `docker stop` — SIGTERM, and then SIGKILL when the
/// service manager's patience runs out. Before the handler existed the
/// runner had no disposition for it: it died instantly, mid-step, leaving
/// the step's *process group* orphaned — the `sleep` here outlived the
/// runner and kept burning the machine's CPU with nobody left to stop it —
/// and as PID 1 in a container with no init the signal was ignored
/// outright and the job ran to the end.
///
/// The grandchild is the assertion that matters. A runner that merely dies
/// looks fine from the outside; only the surviving process shows the bug.
/// The registration is *not* ephemeral, so the exit also proves that the
/// stop ended the loop rather than the loop ending itself after one job.
#[test]
fn a_sigterm_during_a_job_kills_the_step_group_and_reports_nothing() {
    let dir = TestDir::new("agent-job-term");
    let (url, sha) = origin(dir.path());
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
    let agent = dir.path().join("agent");
    write_runner_file(&agent, &stub, false);
    stub.answer_claims(&[claim_reply(&stub, "job-e2e")]);

    let mut child = agent_command(&agent, &[])
        .spawn()
        .expect("spawn weft-runner");

    let grandchild = wait_for_pid(&pidfile);
    assert!(alive(grandchild), "the step's sleep is running");
    // What the step said before the signal has reached the control plane.
    // Waited for rather than assumed, and the only place it can be seen:
    // once the job is cancelled nothing more is sent, and the agent
    // removes the job's directory, log file and all. That the file itself
    // is flushed rather than abandoned mid-buffer is proven in-crate, by
    // `a_signal_kills_the_step_group_flushes_the_log_and_reports_nothing`.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !stub.seen().chunks.concat().contains("started") {
        assert!(
            Instant::now() < deadline,
            "the step's output never streamed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };

    // Within two seconds: a service manager gives more, but a runner that
    // needs seconds to let go is one that gets SIGKILLed with its log
    // unflushed.
    assert!(
        stops_within(grandchild, Duration::from_secs(2)),
        "the step's process group outlived the runner (pid {grandchild})"
    );
    assert_eq!(
        wait_with_timeout(&mut child, Duration::from_secs(5)),
        Some(0),
        "an operator stopping a runner is an expected ending, not a failure"
    );
    let s = stub.seen();
    let streamed = s.chunks.concat();
    assert!(
        streamed.contains("▶ slow") && streamed.contains("started"),
        "{streamed}"
    );
    assert!(
        s.finish.is_none(),
        "a job that was stopped does not answer for itself"
    );
    assert!(s.final_log.is_empty(), "nor upload an authoritative log");
    assert_eq!(s.claims, 1, "and the runner did not ask for another");
}

/// Layer 3 against the artefact: the built binary, its own process, its
/// own two-second sampler. The in-crate test drives the same path, but
/// only here is the watch running inside a `main` that an operator
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
    // The pid goes outside the job's directory, which the agent removes
    // when the job is over.
    let pidfile = dir.path().join("miner.pid");
    let steps = serde_json::json!([
        { "name": "build", "run": "ln -s /bin/sleep ./xmrig; ./xmrig 120 & echo $! > \"$PIDFILE\"; wait" },
        { "name": "test", "run": "echo never" },
    ]);
    let mut spec: serde_json::Value =
        serde_json::from_str(&spec_json(&url, &sha, steps)).expect("spec");
    spec["env"] = serde_json::json!({ "PIDFILE": pidfile.to_string_lossy() });
    let stub = Stub::start(200, spec.to_string());
    let agent = agent_with_one_job(dir.path(), &stub);

    let started = Instant::now();
    let out = finish_run(agent_command(&agent, &[]));

    finished_line(&out, 0);
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
    // Killed, not merely reported: the group is gone, so the step stops
    // costing the machine anything the moment the verdict is written.
    let pid = wait_for_pid(&pidfile);
    assert!(
        stops_within(pid, Duration::from_secs(5)),
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

/// The process ceiling, against the built binary. A fork bomb is the one
/// thing a step can do that costs somebody other than its own job: it
/// takes the machine down, and everything else its owner runs there. The
/// bound is `RLIMIT_NPROC`, set by the runner on each step, which means
/// this is the only place it can be proven end to end — and
/// `STRATUM_RUNNER_MAX_PROCS` on `run` is how it is proven without a real
/// fork bomb.
///
/// Two things are asserted, and the second is the point. The step fails,
/// *as the step*: the shell's own "fork: Resource temporarily
/// unavailable" reaches the job log and the verdict names the step, so
/// the author sees a step they can fix rather than a runner that broke.
/// And the runner itself is unharmed — it reports the verdict and ends the
/// job cleanly, which is what stops a bomb from taking the agent with it.
///
/// Nothing here forks a machine into the ground: the ceiling is set a few
/// hundred above what the agent's account already has, so the loop is
/// refused a fork long before it reaches its two-thousandth and bash gives
/// up. `sleep 5`, not a tenth of a second, so that the processes it does
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
    let agent = agent_with_one_job(dir.path(), &stub);
    let uid = bounded_account();
    let ceiling = (tasks_now(uid) + 300).to_string();
    let mut cmd = agent_command(&agent, &[("STRATUM_RUNNER_MAX_PROCS", &ceiling)]);
    run_as(&mut cmd, dir.path(), uid);

    let started = Instant::now();
    let out = finish_run(cmd);

    finished_line(&out, 0);
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
        "a fork bomb is a failed job, not an abuse report"
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
/// discovered by somebody's build rather than by us. Run under the same
/// account as the bomb above, so that as root the default ceiling is
/// really in play rather than waved through.
#[test]
fn a_step_that_forks_a_hundred_processes_is_not_a_fork_bomb() {
    let dir = TestDir::new("fanout");
    let (url, sha) = origin(dir.path());
    let steps = serde_json::json!([
        { "name": "build", "run": "for i in $(seq 1 100); do /bin/sleep 0.2 & done; wait; echo fanned out" },
    ]);
    let stub = Stub::start(200, spec_json(&url, &sha, steps));
    let agent = agent_with_one_job(dir.path(), &stub);
    let mut cmd = agent_command(&agent, &[]);
    run_as(&mut cmd, dir.path(), bounded_account());

    let out = finish_run(cmd);

    finished_line(&out, 0);
    assert_eq!(wait_for_finish(&stub), ("passed".to_string(), None));
    assert!(
        stub.seen().final_log.contains("fanned out"),
        "{}",
        stub.seen().final_log
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
