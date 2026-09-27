//! An ECS that answers `RunTask` by starting the real runner.
//!
//! The dispatcher's production path is a signed `RunTask` to Fargate,
//! and the only thing this fake changes about it is what happens after
//! the request is accepted: instead of a task in a cluster, a child
//! process running the *real* `weft-runner` binary, with the same
//! three variables the container override would have carried. Every
//! seam the e2e suite cares about — the signature, the request shape,
//! the runner fetching its spec with the token it was given, cloning
//! over HTTP with it, streaming logs, reporting a verdict — is exercised
//! with the same code that runs in production.
//!
//! It can also be told to answer badly, per call: no capacity, a
//! refusal, throttling, or accepting a task and starting nothing — the
//! "runner lost" case that a lease expiry has to cover.
//!
//! It is not an ECS emulator. It knows `RunTask` and `StopTask`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The dispatch credential the fake accepts. Anything else is an
/// `AccessDeniedException`, which is what the real ECS would say to a
/// server signing with the store's key by mistake.
pub const ACCESS_KEY: &str = "AKIDFAKEDISPATCH";
pub const SECRET_KEY: &str = "fake-dispatch-secret";

/// What to do with the next `RunTask`. The queue is consumed front to
/// back; when it is empty every call launches.
#[derive(Debug, Clone)]
pub enum Answer {
    /// Accept and start the runner.
    Launch,
    /// A 200 with a `RESOURCE:MEMORY` failure and no task.
    NoCapacity,
    /// A 200 with the account's Fargate vCPU quota as the failure — the
    /// sentence the fleet answered on 2026-09-07, typographic apostrophe
    /// and all. The room exists; the account may not use it yet.
    QuotaExhausted,
    /// A 400 `ClientException` with this message.
    Refuse(String),
    /// A 400 `ThrottlingException`.
    Throttle,
    /// Accept, return an ARN, start nothing. The runner never reports.
    Vanish,
    /// Start the runner and answer `RunTask` only once it has **exited**:
    /// the runner has done everything it is going to do — fetched its
    /// job, run it, reported — before the dispatcher hears that a task
    /// exists. The extreme of what any platform does in miniature, since
    /// a task can be up and calling before `RunTask` returns.
    LaunchAndWait,
    /// Accept, but only after this long: the dispatcher is left holding
    /// an in-flight `RunTask` while the rest of the product goes on —
    /// long enough for the job to be cancelled underneath it. The
    /// runner is started when the answer is given, as ECS would.
    LaunchAfter(Duration),
}

/// One call the product made.
#[derive(Debug, Clone)]
pub struct EcsCall {
    /// `RunTask` or `StopTask`.
    pub target: String,
    pub body: serde_json::Value,
    pub authorization: Option<String>,
}

impl EcsCall {
    /// Which task definition a `RunTask` named.
    pub fn task_definition(&self) -> String {
        self.body["taskDefinition"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// The task-level cpu/memory override, when the request carried one
    /// — a GitHub Actions launch always does; a Weft launch never does.
    pub fn overrides_cpu_memory(&self) -> Option<(String, String)> {
        let o = &self.body["overrides"];
        Some((
            o["cpu"].as_str()?.to_string(),
            o["memory"].as_str()?.to_string(),
        ))
    }

    /// The container environment a `RunTask` asked for.
    pub fn env(&self) -> BTreeMap<String, String> {
        self.body["overrides"]["containerOverrides"][0]["environment"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        Some((
                            e["name"].as_str()?.to_string(),
                            e["value"].as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

struct Inner {
    calls: Mutex<Vec<EcsCall>>,
    script: Mutex<VecDeque<Answer>>,
    runner: Option<PathBuf>,
    workroot: PathBuf,
    tasks: Mutex<HashMap<String, Child>>,
    exited: Mutex<HashMap<String, i32>>,
    /// `RunTask` handlers still holding a runner they are waiting out
    /// (`LaunchAndWait`). Those runners are in neither map until the
    /// handler reaps them, and `wait_all` must not return before it has.
    waiting: AtomicU64,
    seq: AtomicU64,
}

pub struct FakeEcs {
    pub url: String,
    inner: Arc<Inner>,
}

impl FakeEcs {
    /// Start one. `runner` is the binary to start per accepted task;
    /// `None` records calls and starts nothing (every `Launch` behaves
    /// as `Vanish`). `workroot` gets one directory per task.
    pub fn start(runner: Option<PathBuf>, workroot: impl Into<PathBuf>) -> FakeEcs {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake ecs");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let workroot = workroot.into();
        std::fs::create_dir_all(&workroot).expect("workroot");
        let inner = Arc::new(Inner {
            calls: Mutex::new(Vec::new()),
            script: Mutex::new(VecDeque::new()),
            runner,
            workroot,
            tasks: Mutex::new(HashMap::new()),
            exited: Mutex::new(HashMap::new()),
            waiting: AtomicU64::new(0),
            seq: AtomicU64::new(1),
        });
        let srv = inner.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let srv = srv.clone();
                std::thread::spawn(move || serve(stream, srv));
            }
        });
        FakeEcs { url, inner }
    }

    /// The env a server needs to dispatch through this.
    pub fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("STRATUM_RUNNER_ECS_CLUSTER", "fake-cluster".into()),
            ("STRATUM_RUNNER_ECS_TASK_DEFINITION", "fake-runner:1".into()),
            (
                "STRATUM_RUNNER_ECS_SUBNETS",
                "subnet-fake-a,subnet-fake-b".into(),
            ),
            ("STRATUM_RUNNER_ECS_SECURITY_GROUP", "sg-fake".into()),
            ("STRATUM_RUNNER_AWS_ACCESS_KEY_ID", ACCESS_KEY.into()),
            ("STRATUM_RUNNER_AWS_SECRET_ACCESS_KEY", SECRET_KEY.into()),
            ("STRATUM_RUNNER_AWS_REGION", "eu-west-1".into()),
            ("STRATUM_RUNNER_ECS_URL", self.url.clone()),
        ]
    }

    /// What a `RunTask` for a GitHub Actions runner names.
    pub const GITHUB_TASK_DEFINITION: &'static str = "fake-gh-runner:1";

    /// The env that adds the GitHub Actions door to `env()`. Separate,
    /// so a world that never asked for it is byte-for-byte the world
    /// the Weft runner suites run in.
    pub fn github_env(&self) -> Vec<(&'static str, String)> {
        vec![(
            "STRATUM_RUNNER_ECS_GITHUB_TASK_DEFINITION",
            Self::GITHUB_TASK_DEFINITION.into(),
        )]
    }

    /// Queue answers for the next `RunTask`s.
    pub fn script(&self, answers: impl IntoIterator<Item = Answer>) {
        self.inner.script.lock().unwrap().extend(answers);
    }

    pub fn calls(&self) -> Vec<EcsCall> {
        self.inner.calls.lock().unwrap().clone()
    }

    pub fn run_tasks(&self) -> Vec<EcsCall> {
        self.calls()
            .into_iter()
            .filter(|c| c.target == "RunTask")
            .collect()
    }

    pub fn stop_tasks(&self) -> Vec<EcsCall> {
        self.calls()
            .into_iter()
            .filter(|c| c.target == "StopTask")
            .collect()
    }

    /// Tasks whose runner process is still going.
    pub fn running(&self) -> usize {
        let mut tasks = self.inner.tasks.lock().unwrap();
        tasks.retain(|_, c| matches!(c.try_wait(), Ok(None)));
        tasks.len()
    }

    /// Wait for every started runner to exit; the exit codes by ARN.
    /// Panics past `within`, naming what is still running.
    pub fn wait_all(&self, within: Duration) -> HashMap<String, i32> {
        let deadline = Instant::now() + within;
        loop {
            let mut tasks = self.inner.tasks.lock().unwrap();
            let mut done = Vec::new();
            for (arn, child) in tasks.iter_mut() {
                if let Ok(Some(st)) = child.try_wait() {
                    done.push((arn.clone(), st.code().unwrap_or(-1)));
                }
            }
            for (arn, code) in done {
                tasks.remove(&arn);
                self.inner.exited.lock().unwrap().insert(arn, code);
            }
            // A `LaunchAndWait` runner is the handler's to reap, and the
            // product can have seen its verdict — and the test moved on —
            // before the process has actually exited and been recorded.
            if tasks.is_empty() && self.inner.waiting.load(Ordering::SeqCst) == 0 {
                return self.inner.exited.lock().unwrap().clone();
            }
            assert!(
                Instant::now() < deadline,
                "runners still going: {:?}",
                tasks.keys().collect::<Vec<_>>()
            );
            drop(tasks);
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for FakeEcs {
    fn drop(&mut self) {
        for (_, mut c) in self.inner.tasks.lock().unwrap().drain() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn reply(w: &mut impl Write, status: u16, body: &str) {
    let _ = write!(
        w,
        "HTTP/1.1 {status} X\r\ncontent-type: application/x-amz-json-1.1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn serve(stream: std::net::TcpStream, inner: Arc<Inner>) {
    let Ok(mut w) = stream.try_clone() else {
        return;
    };
    let mut r = BufReader::new(stream);
    let mut request_line = String::new();
    if r.read_line(&mut request_line).is_err() {
        return;
    }
    let (mut len, mut target, mut auth) = (0usize, String::new(), None);
    loop {
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let v = v.trim().to_string();
            match k.to_ascii_lowercase().as_str() {
                "content-length" => len = v.parse().unwrap_or(0),
                "x-amz-target" => {
                    target = v.rsplit('.').next().unwrap_or("").to_string();
                }
                "authorization" => auth = Some(v),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; len];
    if r.read_exact(&mut body).is_err() {
        return;
    }
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    inner.calls.lock().unwrap().push(EcsCall {
        target: target.clone(),
        body: body.clone(),
        authorization: auth.clone(),
    });

    // The signature has to be *this* credential's and scoped to ECS.
    // A server that signs with the store's key — the mistake the
    // separate `STRATUM_RUNNER_AWS_*` variables exist to prevent — is
    // refused the way AWS would refuse it.
    let signed_right = auth.as_deref().is_some_and(|a| {
        a.starts_with(&format!("AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/"))
            && a.contains("/ecs/aws4_request")
    });
    if !signed_right {
        reply(
            &mut w,
            403,
            r#"{"__type":"AccessDeniedException","message":"not the dispatch credential"}"#,
        );
        return;
    }

    match target.as_str() {
        "RunTask" => {
            let answer = inner
                .script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Answer::Launch);
            match answer {
                Answer::NoCapacity => reply(
                    &mut w,
                    200,
                    r#"{"tasks":[],"failures":[{"arn":"fake","reason":"RESOURCE:MEMORY","detail":null}]}"#,
                ),
                Answer::QuotaExhausted => reply(
                    &mut w,
                    200,
                    r#"{"tasks":[],"failures":[{"arn":"fake","reason":"You’ve reached the limit on the number of vCPUs you can run concurrently. For more information, see the Troubleshooting section of the Amazon ECS Developer Guide.","detail":null}]}"#,
                ),
                Answer::Throttle => reply(
                    &mut w,
                    400,
                    r#"{"__type":"ThrottlingException","message":"Rate exceeded"}"#,
                ),
                Answer::Refuse(msg) => reply(
                    &mut w,
                    400,
                    &serde_json::json!({"__type": "ClientException", "message": msg}).to_string(),
                ),
                Answer::Launch
                | Answer::Vanish
                | Answer::LaunchAndWait
                | Answer::LaunchAfter(_) => {
                    if let Answer::LaunchAfter(d) = answer {
                        std::thread::sleep(d);
                    }
                    let n = inner.seq.fetch_add(1, Ordering::SeqCst);
                    let arn =
                        format!("arn:aws:ecs:eu-west-1:000000000000:task/fake-cluster/{n:08}");
                    let env: BTreeMap<String, String> = body["overrides"]["containerOverrides"][0]
                        ["environment"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|e| {
                                    Some((
                                        e["name"].as_str()?.to_string(),
                                        e["value"].as_str()?.to_string(),
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if let (
                        Answer::Launch | Answer::LaunchAndWait | Answer::LaunchAfter(_),
                        Some(bin),
                    ) = (&answer, &inner.runner)
                    {
                        let work = inner.workroot.join(format!("task-{n:08}"));
                        let hold = matches!(answer, Answer::LaunchAndWait);
                        if hold {
                            inner.waiting.fetch_add(1, Ordering::SeqCst);
                        }
                        let child = Command::new(bin)
                            .envs(&env)
                            .env("STRATUM_RUNNER_WORKDIR", &work)
                            .env("STRATUM_RUNNER_FLUSH_MS", "100")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .spawn();
                        match child {
                            Ok(mut c) => {
                                if hold {
                                    let code =
                                        c.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                                    inner.exited.lock().unwrap().insert(arn.clone(), code);
                                    inner.waiting.fetch_sub(1, Ordering::SeqCst);
                                } else {
                                    inner.tasks.lock().unwrap().insert(arn.clone(), c);
                                }
                            }
                            Err(e) => {
                                if hold {
                                    inner.waiting.fetch_sub(1, Ordering::SeqCst);
                                }
                                reply(
                                    &mut w,
                                    200,
                                    &serde_json::json!({"tasks": [], "failures": [{"arn": arn, "reason": "AGENT", "detail": format!("spawn {}: {e}", bin.display())}]}).to_string(),
                                );
                                return;
                            }
                        }
                    }
                    reply(
                        &mut w,
                        200,
                        &serde_json::json!({"tasks": [{"taskArn": arn, "lastStatus": "PROVISIONING"}], "failures": []}).to_string(),
                    );
                }
            }
        }
        "StopTask" => {
            let arn = body["task"].as_str().unwrap_or("").to_string();
            let mut tasks = inner.tasks.lock().unwrap();
            if let Some(mut c) = tasks.remove(&arn) {
                let _ = c.kill();
                let code = c.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
                inner.exited.lock().unwrap().insert(arn.clone(), code);
            }
            reply(
                &mut w,
                200,
                &serde_json::json!({"task": {"taskArn": arn}}).to_string(),
            );
        }
        _ => reply(&mut w, 400, r#"{"__type":"InvalidAction"}"#),
    }
}

/// The runner binary next to a server binary, built if it is not there
/// or is older than the runner's source.
///
/// `cargo test --workspace` builds every package's binaries before it
/// runs anything, so the sibling exists; a single-suite run
/// (`--test runner_e2e`) does not build other packages, and building
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
