//! Running one child and watching it: the only place in the runner that
//! executes somebody else's code.
//!
//! Three things here are load-bearing and easy to get subtly wrong.
//!
//! **Interleaving.** A build log where stderr arrives in a block after all
//! of stdout is unreadable — the compiler error no longer sits under the
//! command that produced it. Two pipes cannot be merged after the fact,
//! because the ordering information is gone by then. So the child is given
//! *one* pipe as both its stdout and its stderr (`std::io::pipe`, duped),
//! which is exactly what a terminal does and what `2>&1` does; production
//! order then falls out of the kernel rather than out of our scheduling.
//! It is done with a real pipe rather than by wrapping the command in
//! `bash -c '… 2>&1'` because the step's `run` is already going into a
//! shell, and wrapping it twice means quoting it twice.
//!
//! **Killing.** A step is a shell, and a shell's children are not its
//! process. `Child::kill` signals the leader, and the `cargo build` it
//! spawned keeps the CPU. Every step is therefore its own process group
//! (`process_group(0)`), and a timeout or a cancellation signals the
//! group.
//!
//! **Draining.** The pipe has a finite buffer. A child that fills it
//! blocks forever if nobody is reading, and "the job hung after exactly
//! 64 KiB of output" is a bug that only shows up on the noisy builds. A
//! reader thread drains it for the child's whole life, and is joined only
//! after the child is reaped.

use crate::log::Sink;
use crate::spec::{step_env, Assignment};
use crate::watch::{self, Abuse, Watch};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How a phase of the job ended. Shared by the checkout and the steps
/// because the runner reacts to them identically.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    /// The job failed, and this is what the run's `error` should say.
    Failed(String),
    /// The wall-clock budget ran out.
    TimedOut,
    /// The control plane answered 410: someone cancelled the run, or a
    /// newer push superseded it.
    Cancelled,
    /// Mining software was running inside a step. The string is the name
    /// it was recognised by, and this is the one outcome that also tells
    /// the control plane *what kind* of failure it was — see the `abuse`
    /// field on the verdict.
    Abuse(String),
}

/// How a single child ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    Exited(i32),
    TimedOut,
    Cancelled,
}

/// The three things every supervised child is watched against: where its
/// output goes, when the job's wall-clock budget runs out, and whether the
/// run has been cancelled underneath it. Carried together because they are
/// never apart, and because a checkout and a step are watched identically.
pub struct Ctx<'a> {
    pub sink: &'a Sink,
    pub deadline: Instant,
    pub cancelled: &'a AtomicBool,
    /// Where the process watch reports mining software it found in a
    /// child's process group. Deliberately not the `cancelled` flag:
    /// cancellation means nobody wants a verdict, and here the verdict is
    /// the entire point.
    pub abuse: Arc<Abuse>,
    /// The most processes the job's uid may have at once. See
    /// [`MAX_PROCS`].
    pub max_procs: u64,
}

/// How many processes one job is allowed. A fork bomb — deliberate, or a
/// build script's parallelism multiplied by itself — otherwise takes the
/// whole host down with it, and every other task on that host with it.
///
/// It lives here rather than in the task definition because Fargate
/// accepts only the `nofile` ulimit, so there is nowhere else on the
/// deployed fleet to put it. `RLIMIT_NPROC` is per *uid*, and the
/// container runs exactly one job as uid 10002, so the only processes
/// counted against this bound are that job's own.
///
/// The number is deliberately generous: a parallel build legitimately
/// runs hundreds of compilers, and a bound that fails an honest `make
/// -j$(nproc)` would be a worse bug than the one it prevents.
pub const MAX_PROCS: u64 = 4096;

/// How often the supervisor looks at the child, the clock and the
/// cancellation flag. Small enough that a cancelled job stops promptly,
/// large enough that a six-hour job does not spend a core on polling.
const POLL: Duration = Duration::from_millis(50);

/// Spawn `cmd`, stream its combined output into `sink`, and wait — until
/// it exits, until `deadline`, or until `cancelled` is raised.
pub fn supervise(mut cmd: Command, ctx: &Ctx) -> Result<Ended, String> {
    let (reader, writer) = std::io::pipe().map_err(|e| format!("cannot create a pipe: {e}"))?;
    let writer2 = writer
        .try_clone()
        .map_err(|e| format!("cannot duplicate a pipe: {e}"))?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(writer))
        .stderr(Stdio::from(writer2))
        // Its own process group, so a timeout can reach the child's
        // children. See the module note.
        .process_group(0);
    let bound = ctx.max_procs;
    // SAFETY: this runs in the forked child, between fork and exec, where
    // only async-signal-safe calls are allowed. `getrlimit`/`setrlimit`
    // are two such calls and nothing here allocates, locks or touches a
    // pointer the parent owns.
    unsafe {
        cmd.pre_exec(move || limit_procs(bound));
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot spawn: {e}"))?;
    // Armed for the child's whole life and stopped when this drops. The
    // watch kills the group itself rather than reporting back: the
    // supervisor is asleep between polls, and the point is to stop paying
    // for the CPU. The step then ends the way any killed step does.
    let _watch = Watch::start(child.id() as i32, Arc::clone(&ctx.abuse), watch::SAMPLE);
    // The Command still holds the parent's copies of the write end; while
    // they are open the reader below never sees EOF and the job hangs at
    // the end of every step.
    drop(cmd);

    let pump = {
        let sink = ctx.sink.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.bytes(&buf[..n]),
                }
            }
        })
    };

    // `ok().flatten()` folds a wait error in with "not finished yet"
    // deliberately: the only way `try_wait` fails on a child we spawned is
    // if something reaped it behind our back, and then the deadline below
    // is the honest answer rather than an error nobody can act on.
    let ended = loop {
        if let Some(status) = child.try_wait().ok().flatten() {
            // A child killed by a signal has no exit code; report it the
            // way a shell does, so `✗ … exited 137` is recognisable.
            break Ended::Exited(
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            );
        }
        if ctx.cancelled.load(Ordering::SeqCst) {
            break kill_and_reap(&mut child, Ended::Cancelled);
        }
        if Instant::now() >= ctx.deadline {
            break kill_and_reap(&mut child, Ended::TimedOut);
        }
        std::thread::sleep(POLL);
    };
    // Only now: the write end is closed by the child's death, so the pump
    // is about to see EOF, and joining first guarantees the sink has every
    // byte before the caller writes the step's verdict line after it.
    let _ = pump.join();
    Ok(ended)
}

fn kill_and_reap(child: &mut std::process::Child, why: Ended) -> Ended {
    kill_group(child.id());
    let _ = child.wait();
    why
}

/// SIGKILL the whole group. Negative pid is `kill(2)`'s spelling for
/// "the process group with this id", and there is no std equivalent —
/// `Child::kill` can only reach the leader.
fn kill_group(pid: u32) {
    // SAFETY: a plain libc call with no pointers. The pid is one we
    // spawned into its own group, so the negation cannot name another
    // job's group; the worst case is that the group is already gone and
    // kill returns ESRCH, which is why the result is ignored.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// Cap the number of processes the child's uid may have, soft *and* hard,
/// so the step cannot raise it back.
///
/// Clamped to the hard limit we already have: a process without privilege
/// may only lower its hard limit, and asking for more than it holds fails
/// with EPERM. Under the deployed image the hard limit is unlimited and
/// the clamp is a no-op; on a development machine it is what stops this
/// from turning every step into "cannot spawn".
///
/// Runs after `fork` and before `exec`. Anything it returns as an error
/// fails the spawn, which is the right end for a bound that could not be
/// applied: a step that runs without its ceiling is the case this exists
/// to prevent.
fn limit_procs(bound: u64) -> std::io::Result<()> {
    // SAFETY: two libc calls over stack-allocated `rlimit`s that are
    // initialised before they are read. See the `pre_exec` note above for
    // why nothing else may happen in here.
    unsafe {
        let mut cur: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NPROC, &mut cur) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let want = clamp_to_hard(bound, cur.rlim_max);
        let lim = libc::rlimit {
            rlim_cur: want,
            rlim_max: want,
        };
        if libc::setrlimit(libc::RLIMIT_NPROC, &lim) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// What to ask for, given what we already hold. Split out from
/// [`limit_procs`] because it is the half that can be decided without a
/// syscall — and because which branch runs depends on the machine: the
/// deployed image has no hard limit and takes the first, a development
/// machine has one and takes the second, so neither is reachable by a
/// test that goes through `getrlimit`.
fn clamp_to_hard(bound: u64, hard: libc::rlim_t) -> libc::rlim_t {
    if hard == libc::RLIM_INFINITY {
        bound as libc::rlim_t
    } else {
        std::cmp::min(hard, bound as libc::rlim_t)
    }
}

/// The command one step runs: `bash -e -c "<run>"` in the checkout.
///
/// **`bash`, not `sh`.** The premise of `.weft/` is that a person pastes
/// the workflow they already have, and the one they already have was
/// written for GitHub Actions, which runs a `run:` block under
/// `bash -e {0}`. On `debian:bookworm-slim` `/bin/sh` is dash, so `[[ ]]`,
/// arrays, `source` and `set -o pipefail` are all syntax errors — a
/// workflow that works everywhere else would fail here, on a line the
/// author has no reason to suspect. Deliberately *not* `-o pipefail`:
/// Actions only sets that for an explicit `shell: bash`, and turning it on
/// by default would fail steps that pass for everyone else, which is the
/// same class of surprise in the other direction.
///
/// `-e` because a step is a script and a failure halfway through it should
/// fail the step, not be swallowed by whatever ran next. It matches what
/// every other CI system does with a multi-line `run:` block, and a
/// workflow that wants the other behaviour can say `set +e` itself.
pub fn step_command(
    run: &str,
    dir: &Path,
    env: &std::collections::BTreeMap<String, String>,
) -> Command {
    let mut c = Command::new(shell(env));
    c.arg("-e").arg("-c").arg(run).current_dir(dir).env_clear();
    for (k, v) in env {
        c.env(k, v);
    }
    c
}

/// `bash` if the step will be able to find one, `sh` otherwise.
///
/// Resolved here, against the *step's* `PATH`, rather than left to `exec`:
/// the child's environment is cleared and rebuilt, so `exec` would look
/// along the step's `PATH` and a missing bash would come back as
/// "could not start" — a runner failure, reported to somebody who cannot
/// do anything about it — instead of a step that simply runs under sh.
/// The fallback exists for images the allowlist may add later; the one we
/// ship has bash.
fn shell(env: &std::collections::BTreeMap<String, String>) -> &'static str {
    let path = env.get("PATH").map(String::as_str).unwrap_or_default();
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        if executable(&Path::new(dir).join("bash")) {
            return "bash";
        }
    }
    "sh"
}

fn executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Run every step in order, narrating into the log, and stop at the first
/// one that fails.
///
/// Steps after a failure are listed rather than silently omitted: a reader
/// scrolling to the bottom of a failed job should be able to see what did
/// not get a chance to run without opening the workflow file.
///
/// `repo` is the working directory the steps get — the job's own tree,
/// which on a composed job is one member's directory inside `workspace`.
/// `workspace` is that root, and `None` on every other job.
pub fn run_steps(
    a: &Assignment,
    repo: &Path,
    workspace: Option<&Path>,
    inherited: &std::collections::BTreeMap<String, String>,
    ctx: &Ctx,
) -> Outcome {
    for (i, step) in a.spec.steps.iter().enumerate() {
        ctx.sink.line(&format!("▶ {}", step.label()));
        let started = Instant::now();
        let env = step_env(inherited, a, step, workspace);
        let ended = match supervise(step_command(&step.run, repo, &env), ctx) {
            Ok(e) => e,
            // The step never started — no shell, no fork. That is the
            // runner's failure, not the workflow's, so say so plainly.
            Err(e) => {
                ctx.sink
                    .line(&format!("✗ {} could not start: {e}", step.label()));
                return Outcome::Failed(format!("step \"{}\" could not start: {e}", step.label()));
            }
        };
        let secs = started.elapsed().as_secs();
        // Checked before the exit code, because what the exit code says
        // about a step whose group was killed is "137", and a job stopped
        // for mining must not be reported as an ordinary failure — the
        // control plane suspends the organisation on this and nothing
        // else. `xmrig || true` is why it is checked even when the step
        // exited 0.
        if let Some(name) = ctx.abuse.found() {
            ctx.sink.line(&format!(
                "\u{2717} {} stopped: mining software detected: {name} ({secs}s)",
                step.label()
            ));
            not_run(a, i + 1, ctx.sink);
            return Outcome::Abuse(name);
        }
        match ended {
            Ended::Exited(0) => ctx.sink.line(&format!("✓ {} ({secs}s)", step.label())),
            Ended::Exited(code) => {
                ctx.sink
                    .line(&format!("✗ {} exited {code} ({secs}s)", step.label()));
                not_run(a, i + 1, ctx.sink);
                return Outcome::Failed(format!("step \"{}\" exited {code}", step.label()));
            }
            Ended::TimedOut => {
                ctx.sink
                    .line(&format!("✗ {} timed out ({secs}s)", step.label()));
                not_run(a, i + 1, ctx.sink);
                return Outcome::TimedOut;
            }
            Ended::Cancelled => {
                ctx.sink
                    .line(&format!("✗ {} cancelled ({secs}s)", step.label()));
                not_run(a, i + 1, ctx.sink);
                return Outcome::Cancelled;
            }
        }
    }
    Outcome::Passed
}

/// List the steps that never ran, from `from` onwards.
pub fn not_run(a: &Assignment, from: usize, sink: &Sink) {
    for step in a.spec.steps.iter().skip(from) {
        sink.line(&format!("– {} (not run)", step.label()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::parse_assignment;
    use crate::testdir::TestDir;
    use std::collections::BTreeMap;
    use std::sync::mpsc::Receiver;

    /// A sink that collects into a channel, so a test can read what a step
    /// wrote without a control plane or a file behind it.
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

    /// A watch with a deadline far enough away that only the test's own
    /// signal can end the child.
    fn ctx<'a>(sink: &'a Sink, cancelled: &'a AtomicBool) -> Ctx<'a> {
        Ctx {
            sink,
            deadline: far(),
            cancelled,
            abuse: Arc::new(Abuse::default()),
            max_procs: MAX_PROCS,
        }
    }

    #[test]
    fn stdout_and_stderr_arrive_interleaved_in_the_order_the_child_wrote_them() {
        let dir = TestDir::new("interleave");
        let (sink, rx) = collector();
        let env = BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap())]);
        let cmd = step_command(
            "echo one; echo two >&2; echo three; echo four >&2",
            dir.path(),
            &env,
        );
        assert_eq!(
            supervise(cmd, &ctx(&sink, &no())).expect("supervise"),
            Ended::Exited(0)
        );
        assert_eq!(drain(&rx), "one\ntwo\nthree\nfour\n");
    }

    #[test]
    fn a_child_that_outruns_the_pipe_buffer_does_not_wedge_the_job() {
        let dir = TestDir::new("flood");
        let (sink, rx) = collector();
        let env = BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap())]);
        // Well past a pipe's 64 KiB: without the draining thread this
        // blocks forever rather than failing, which is why it is a test.
        let cmd = step_command(
            "for i in $(seq 1 20000); do echo line $i; done",
            dir.path(),
            &env,
        );
        assert_eq!(
            supervise(cmd, &ctx(&sink, &no())).expect("supervise"),
            Ended::Exited(0)
        );
        let out = drain(&rx);
        assert!(out.len() > 200_000, "{} bytes", out.len());
        assert!(out.ends_with("line 20000\n"), "the tail is not lost");
    }

    #[test]
    fn a_step_that_fails_reports_its_code_and_a_signalled_one_reports_the_signal() {
        let dir = TestDir::new("codes");
        let (sink, _rx) = collector();
        let env = BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap())]);
        assert_eq!(
            supervise(step_command("exit 3", dir.path(), &env), &ctx(&sink, &no()),)
                .expect("supervise"),
            Ended::Exited(3)
        );
        assert_eq!(
            supervise(
                step_command("kill -9 $$", dir.path(), &env),
                &ctx(&sink, &no())
            )
            .expect("supervise"),
            Ended::Exited(137)
        );
    }

    #[test]
    fn a_deadline_kills_the_whole_group_not_just_the_shell() {
        let dir = TestDir::new("timeout");
        let (sink, _rx) = collector();
        let env = BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap())]);
        let marker = dir.path().join("grandchild-still-here");
        // The shell backgrounds a sleeper that outlives it and then waits.
        // If only the leader were signalled, the sleeper would go on to
        // create the marker.
        let script = format!("( sleep 5; touch {} ) & sleep 30", marker.display());
        let started = Instant::now();
        let ended = supervise(
            step_command(&script, dir.path(), &env),
            &Ctx {
                sink: &sink,
                deadline: Instant::now() + Duration::from_millis(150),
                cancelled: &no(),
                abuse: Arc::new(Abuse::default()),
                max_procs: MAX_PROCS,
            },
        )
        .expect("supervise");
        assert_eq!(ended, Ended::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it did not wait out the sleep"
        );
        std::thread::sleep(Duration::from_millis(600));
        assert!(
            !marker.exists(),
            "the backgrounded grandchild survived the kill"
        );
    }

    #[test]
    fn cancellation_stops_a_running_step() {
        let dir = TestDir::new("cancel");
        let (sink, rx) = collector();
        let env = BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap())]);
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            flag.store(true, Ordering::SeqCst);
        });
        let ended = supervise(
            step_command("echo started; sleep 30", dir.path(), &env),
            &ctx(&sink, &cancelled),
        )
        .expect("supervise");
        assert_eq!(ended, Ended::Cancelled);
        assert_eq!(drain(&rx), "started\n");
    }

    #[test]
    fn a_command_that_cannot_be_spawned_is_an_error_rather_than_a_panic() {
        let (sink, _rx) = collector();
        let mut c = Command::new("/nonexistent/weft-runner-no-such-binary");
        c.env_clear();
        let err = supervise(c, &ctx(&sink, &no())).expect_err("must fail");
        assert!(err.starts_with("cannot spawn:"), "{err}");
    }

    fn assignment(steps: serde_json::Value) -> Assignment {
        parse_assignment(&crate::fakecp::spec_json(steps).to_string()).expect("spec")
    }

    fn inherited() -> BTreeMap<String, String> {
        BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap())])
    }

    #[test]
    fn a_passing_job_narrates_every_step_and_sees_its_environment() {
        let dir = TestDir::new("steps-pass");
        let (sink, rx) = collector();
        let a = assignment(serde_json::json!([
            {"name": "Greet", "run": "echo hello"},
            {"run": "echo $WEFT_JOB/$WEFT_EVENT/$CI"},
        ]));
        assert_eq!(
            run_steps(&a, dir.path(), None, &inherited(), &ctx(&sink, &no())),
            Outcome::Passed
        );
        let out = drain(&rx);
        assert!(out.contains("▶ Greet\nhello\n✓ Greet (0s)\n"), "{out}");
        // The unnamed step is shown by its command, and it saw the job.
        assert!(
            out.contains("▶ echo $WEFT_JOB/$WEFT_EVENT/$CI\ntest/push/true\n"),
            "{out}"
        );
    }

    #[test]
    fn a_failing_step_stops_the_job_names_itself_and_lists_what_never_ran() {
        let dir = TestDir::new("steps-fail");
        let (sink, rx) = collector();
        let a = assignment(serde_json::json!([
            {"name": "Build", "run": "echo building"},
            {"name": "Test", "run": "echo boom >&2; exit 2"},
            {"name": "Publish", "run": "echo never"},
        ]));
        assert_eq!(
            run_steps(&a, dir.path(), None, &inherited(), &ctx(&sink, &no())),
            Outcome::Failed("step \"Test\" exited 2".into())
        );
        let out = drain(&rx);
        assert!(out.contains("✓ Build (0s)"), "{out}");
        assert!(
            out.contains("▶ Test\nboom\n✗ Test exited 2 (0s)\n"),
            "{out}"
        );
        assert!(out.contains("– Publish (not run)"), "{out}");
        assert!(
            !out.contains("never"),
            "the step after the failure did not run"
        );
    }

    #[test]
    fn a_timeout_and_a_cancellation_each_end_the_job_and_say_which() {
        let dir = TestDir::new("steps-stop");
        let (sink, rx) = collector();
        let a = assignment(serde_json::json!([
            {"name": "Sleep", "run": "sleep 30"},
            {"name": "After", "run": "echo never"},
        ]));
        assert_eq!(
            run_steps(
                &a,
                dir.path(),
                None,
                &inherited(),
                &Ctx {
                    sink: &sink,
                    deadline: Instant::now() + Duration::from_millis(120),
                    cancelled: &no(),
                    abuse: Arc::new(Abuse::default()),
                    max_procs: MAX_PROCS,
                }
            ),
            Outcome::TimedOut
        );
        let out = drain(&rx);
        assert!(out.contains("✗ Sleep timed out"), "{out}");
        assert!(out.contains("– After (not run)"), "{out}");

        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            flag.store(true, Ordering::SeqCst);
        });
        assert_eq!(
            run_steps(&a, dir.path(), None, &inherited(), &ctx(&sink, &cancelled)),
            Outcome::Cancelled
        );
        let out = drain(&rx);
        assert!(out.contains("✗ Sleep cancelled"), "{out}");
        assert!(out.contains("– After (not run)"), "{out}");
    }

    #[test]
    fn a_step_whose_shell_is_missing_fails_the_job_with_the_reason() {
        let dir = TestDir::new("steps-noshell");
        let (sink, rx) = collector();
        let a = assignment(serde_json::json!([{"name": "Test", "run": "true"}]));
        // A PATH with no shell on it: the step never starts. (An *empty*
        // environment would not do — the exec falls back to a default
        // PATH and finds /bin/sh after all.)
        let path = BTreeMap::from([(
            "PATH".to_string(),
            "/nonexistent-weft-runner-bin".to_string(),
        )]);
        let out = run_steps(&a, dir.path(), None, &path, &ctx(&sink, &no()));
        assert_eq!(
            out,
            Outcome::Failed("step \"Test\" could not start: cannot spawn: No such file or directory (os error 2)".into())
        );
        assert!(drain(&rx).contains("✗ Test could not start:"));
    }

    /// The reason `bash` is the default at all. `[[ ]]` is a syntax error
    /// under dash, which is `/bin/sh` on the image we ship, so a workflow
    /// copied from GitHub Actions — where a `run:` block is `bash -e {0}` —
    /// would fail on a line its author has no reason to suspect.
    #[test]
    fn a_step_runs_under_bash_so_a_workflow_copied_from_actions_works() {
        let dir = TestDir::new("bashism");
        let (sink, rx) = collector();
        let env = BTreeMap::from([
            ("PATH".to_string(), std::env::var("PATH").expect("PATH")),
            ("WEFT_CI".to_string(), "true".to_string()),
        ]);
        assert_eq!(shell(&env), "bash", "the test machine has a bash");
        let cmd = step_command(
            r#"[[ "$WEFT_CI" == true ]] && echo bash-ok"#,
            dir.path(),
            &env,
        );
        // Asserted on the command itself as well as on the output, because
        // on a developer's macOS `/bin/sh` *is* bash in POSIX mode and
        // would run the bashism happily; only on the image we ship is the
        // output alone enough to tell the two apart.
        assert_eq!(cmd.get_program(), "bash");
        assert_eq!(
            supervise(cmd, &ctx(&sink, &no())).expect("supervise"),
            Ended::Exited(0)
        );
        assert_eq!(drain(&rx), "bash-ok\n");
    }

    /// Deliberately not `-o pipefail`: Actions only sets it for an explicit
    /// `shell: bash`, so a step whose first command in a pipeline fails must
    /// still be judged by the last one, the way it is everywhere else.
    #[test]
    fn a_pipeline_is_judged_by_its_last_command() {
        let dir = TestDir::new("pipefail");
        let (sink, _rx) = collector();
        let env = BTreeMap::from([("PATH".to_string(), std::env::var("PATH").expect("PATH"))]);
        let cmd = step_command("false | true", dir.path(), &env);
        assert_eq!(
            supervise(cmd, &ctx(&sink, &no())).expect("supervise"),
            Ended::Exited(0)
        );
    }

    /// An image with no bash gets a step that runs, not a step that cannot
    /// start. The `PATH` here holds exactly one program.
    #[test]
    fn a_step_falls_back_to_sh_when_the_image_has_no_bash() {
        let dir = TestDir::new("nobash");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        std::os::unix::fs::symlink("/bin/sh", bin.join("sh")).expect("symlink sh");
        let (sink, rx) = collector();
        let env = BTreeMap::from([("PATH".to_string(), bin.to_string_lossy().to_string())]);
        assert_eq!(shell(&env), "sh");
        let cmd = step_command("echo posix-ok", dir.path(), &env);
        assert_eq!(
            supervise(cmd, &ctx(&sink, &no())).expect("supervise"),
            Ended::Exited(0)
        );
        assert_eq!(drain(&rx), "posix-ok\n");
    }

    /// The three ways a `PATH` entry can fail to yield a usable bash. A
    /// non-executable file is the one that matters: picking it would turn
    /// every step into "could not start".
    #[test]
    fn a_path_entry_that_is_not_a_runnable_bash_is_skipped() {
        let dir = TestDir::new("shellpath");
        let empty = dir.path().join("empty");
        let notexec = dir.path().join("notexec");
        std::fs::create_dir_all(&empty).expect("mkdir");
        std::fs::create_dir_all(&notexec).expect("mkdir");
        std::fs::write(notexec.join("bash"), "#!/bin/sh\n").expect("write");
        std::fs::set_permissions(notexec.join("bash"), std::fs::Permissions::from_mode(0o644))
            .expect("chmod");
        // A directory named `bash` is not a bash either.
        let isdir = dir.path().join("isdir");
        std::fs::create_dir_all(isdir.join("bash")).expect("mkdir");
        let path = format!(
            "{}::{}:{}:{}",
            dir.path().join("missing").display(),
            empty.display(),
            notexec.display(),
            isdir.display()
        );
        let env = BTreeMap::from([("PATH".to_string(), path)]);
        assert_eq!(shell(&env), "sh");
    }

    /// No `PATH` at all — a spec that cleared it — still yields a command
    /// rather than a panic.
    #[test]
    fn a_step_with_no_path_still_gets_a_shell() {
        assert_eq!(shell(&BTreeMap::new()), "sh");
    }

    /// Layer 3, through the supervisor a real job uses. The "miner" is
    /// `/bin/sleep` started under the name `xmrig`: no mining software
    /// comes near this repository, and it does not need to — the watch
    /// matches on the program's name, so a renamed sleep exercises the
    /// sample, the match, the kill and the verdict exactly as the real
    /// thing would. (A symlink rather than a copy; see the note on
    /// `watch::tests::a_watched_group_running_a_miner_is_killed_and_named`.)
    ///
    /// The step backgrounds it and waits, so what is proven is that the
    /// whole *group* dies. A step that merely returned while its miner
    /// carried on would keep burning the task's CPU until the container
    /// was torn down, which is the cost this exists to stop.
    #[test]
    fn a_step_that_starts_a_miner_is_killed_and_the_job_says_why() {
        let dir = TestDir::new("steps-miner");
        let (sink, rx) = collector();
        let a = assignment(serde_json::json!([
            {"name": "Build", "run": "ln -s /bin/sleep ./xmrig; ./xmrig 60 & echo $! > child.pid; wait"},
            {"name": "Test", "run": "echo never"},
        ]));
        let started = Instant::now();
        let outcome = run_steps(&a, dir.path(), None, &inherited(), &ctx(&sink, &no()));
        assert_eq!(outcome, Outcome::Abuse("xmrig".into()));
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "it waited out the sleep instead of killing it"
        );
        let miner: i32 = std::fs::read_to_string(dir.path().join("child.pid"))
            .expect("the step recorded its miner")
            .trim()
            .parse()
            .expect("a pid");
        assert!(
            unsafe { libc::kill(miner, 0) } != 0,
            "the miner outlived the step (pid {miner})"
        );
        let out = drain(&rx);
        assert!(
            out.contains("\u{2717} Build stopped: mining software detected: xmrig"),
            "{out}"
        );
        assert!(out.contains("\u{2013} Test (not run)"), "{out}");
        assert!(!out.contains("never"), "the next step did not run: {out}");
    }

    /// The half that decides whether anybody can use the forge. This step
    /// writes the name, greps for it, and names a file after it — and it
    /// runs long enough to be sampled more than once, so a watch matching
    /// the wrong thing has every chance to fire.
    #[test]
    fn a_step_that_only_writes_about_mining_runs_to_completion() {
        let dir = TestDir::new("steps-innocent");
        let (sink, rx) = collector();
        let a = assignment(serde_json::json!([
            {"name": "Notes", "run": "echo 'we refuse xmrig here' > xmrig.log; grep -c xmrig xmrig.log; sleep 3"},
        ]));
        assert_eq!(
            run_steps(&a, dir.path(), None, &inherited(), &ctx(&sink, &no())),
            Outcome::Passed
        );
        let out = drain(&rx);
        assert!(out.contains("\u{2713} Notes"), "{out}");
    }
    /// A `Ctx` with a process ceiling of `n`, for the two tests that care
    /// what the bound actually is.
    fn ctx_procs<'a>(sink: &'a Sink, cancelled: &'a AtomicBool, n: u64) -> Ctx<'a> {
        Ctx {
            max_procs: n,
            ..ctx(sink, cancelled)
        }
    }

    /// Both branches of the clamp, neither of which a `getrlimit` on this
    /// machine could reach: without a hard limit the bound is what we
    /// asked for, and with one below it the bound is the hard limit —
    /// because a process without privilege may only lower its hard limit,
    /// and asking for more fails the spawn outright.
    #[test]
    fn the_ceiling_is_clamped_to_the_hard_limit_we_already_hold() {
        assert_eq!(clamp_to_hard(4096, libc::RLIM_INFINITY), 4096);
        assert_eq!(clamp_to_hard(4096, 9000), 4096);
        assert_eq!(clamp_to_hard(4096, 512), 512);
        assert_eq!(clamp_to_hard(4096, 4096), 4096);
    }

    /// The bound reaches the step itself, not just the runner's idea of
    /// it: `ulimit -u` is the shell reporting its own `RLIMIT_NPROC`, so
    /// this is the child's view. Both halves are asserted because a soft
    /// limit the step can raise back is no bound at all.
    #[test]
    fn a_step_runs_under_a_process_ceiling() {
        let dir = TestDir::new("nproc-limit");
        let (sink, rx) = collector();
        let a = assignment(serde_json::json!([
            {"name": "Limits", "run": "ulimit -Su; ulimit -Hu"},
        ]));
        assert_eq!(
            run_steps(
                &a,
                dir.path(),
                None,
                &inherited(),
                &ctx_procs(&sink, &no(), 512)
            ),
            Outcome::Passed
        );
        let out = drain(&rx);
        assert!(out.contains("512\n512\n"), "soft and hard, both 512: {out}");
    }

    /// A step that forks past the bound fails as *the step's* failure —
    /// the workflow's problem, with the shell's own message in the log —
    /// and the runner goes on to report a verdict rather than wedging.
    /// The ceiling is 1, which the uid running this test is already over,
    /// so nothing here actually forks a machine into the ground: the
    /// first fork the step attempts is refused. That is the same
    /// mechanism a fork bomb hits, at its first refused fork rather than
    /// its four-thousandth.
    #[test]
    fn a_step_that_forks_past_the_ceiling_fails_without_wedging_the_runner() {
        let dir = TestDir::new("nproc-bomb");
        let (sink, rx) = collector();
        let a = assignment(serde_json::json!([
            {"name": "Fork", "run": "for i in 1 2 3 4 5 6 7 8; do /bin/sleep 30 & done; wait"},
            {"name": "Test", "run": "echo never"},
        ]));
        let started = Instant::now();
        let outcome = run_steps(
            &a,
            dir.path(),
            None,
            &inherited(),
            &ctx_procs(&sink, &no(), 1),
        );
        assert!(
            matches!(outcome, Outcome::Failed(ref e) if e.starts_with("step \"Fork\" exited")),
            "{outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "it waited out the sleeps instead of being refused the forks"
        );
        let out = drain(&rx);
        assert!(out.contains("\u{2717} Fork exited"), "{out}");
        assert!(out.contains("\u{2013} Test (not run)"), "{out}");
        assert!(!out.contains("never"), "the next step did not run: {out}");
    }

    /// The other half, and the one that decides whether the bound is
    /// usable: an honest step that forks a hundred processes is not a
    /// fork bomb, and the default ceiling leaves it alone.
    #[test]
    fn a_step_that_forks_a_hundred_processes_still_passes() {
        let dir = TestDir::new("nproc-honest");
        let (sink, rx) = collector();
        let a = assignment(serde_json::json!([
            {"name": "Fan out", "run": "for i in $(seq 1 100); do (echo $i > /dev/null) & done; wait; echo fanned"},
        ]));
        assert_eq!(
            run_steps(&a, dir.path(), None, &inherited(), &ctx(&sink, &no())),
            Outcome::Passed
        );
        let out = drain(&rx);
        assert!(out.contains("fanned"), "{out}");
    }
}
