//! `weft-runner`: the agent a spool operator runs on their own machine.
//!
//! `register` trades a registration token for this machine's own
//! credential; `run` then asks the server for work until it is stopped,
//! and runs each job it is given here — check out the tree, run the
//! steps, stream the log, report the verdict. Every call is outbound:
//! nothing listens, and the server never needs to reach this machine.
//! [`agent`] is the loop; [`run_with`] is one job.
//!
//! A job is kept apart from the machine it borrows: it runs in a
//! directory of its own that is removed before and after it, its steps
//! see an allowlisted environment rather than the operator's, and it
//! talks to the server with a per-job token that dies with the job.
//!
//! **Exit codes.** For one job, [`run_with`] answers 0 when a verdict was
//! reported, *or* when the job was cancelled, settled elsewhere or
//! stopped by a signal — endings nobody has to act on. 1 means the server
//! could not be reached, or the work happened and the verdict could not
//! be delivered; the server's overdue sweep settles that job. 2 means the
//! job never started: a refused token, an unreadable spec, a workdir that
//! cannot be made. The agent prints that code beside the job and carries
//! on. The process itself exits 0 when it is stopped or when an ephemeral
//! runner has done its one job, 1 when a registration is refused, and 2
//! when the operator has something to fix before it can run at all — a
//! mistyped command line, a malformed knob, a missing `.runner`, a runner
//! that has been removed.

mod agent;
mod checkout;
mod cli;
mod client;
mod log;
mod signals;
mod spec;
mod steps;
mod watch;

#[cfg(test)]
mod fakecp;
#[cfg(test)]
mod testdir;

use client::{CallError, Client, Tuning};
use log::{Log, LogConfig};
use spec::ChangesetSpec;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    std::process::exit(entry(&std::env::args().skip(1).collect::<Vec<String>>()));
}

/// Everything `main` does, minus the exit — so the composition itself is
/// covered by tests rather than only its parts.
///
/// The arguments are passed in rather than read here: `cargo test` runs
/// this crate's unit tests in a harness whose own argv is a test filter,
/// and an `entry` that read `std::env::args` would parse the filter as a
/// subcommand.
fn entry(args: &[String]) -> i32 {
    // Before anything else, so that no moment of the process's life is
    // on the default disposition — which is to die on the spot, mid-step
    // if a step is running, and leave its process group behind on the
    // operator's machine. The agent is long-lived; Ctrl-C has to reach it.
    let _handlers = match signals::install() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("weft-runner: {e}");
            return 2;
        }
    };
    match cli::parse(args) {
        Ok(cli::Command::Register(o)) => agent::register(&o),
        Ok(cli::Command::Run(o)) => agent::run(&o),
        Ok(cli::Command::Usage) => {
            println!("{}", cli::USAGE);
            0
        }
        // 2, and the usage with it: a mistyped flag — or no command at
        // all — is the one moment an operator is definitely reading the
        // terminal.
        Err(e) => {
            eprintln!("weft-runner: {e}\n\n{}", cli::USAGE);
            2
        }
    }
}

/// One claimed job, as the agent hands it to [`run_with`].
///
/// No `Debug`, deliberately: it holds the job token, and a derived one is
/// how a credential ends up in a panic message that gets pasted into an
/// issue.
pub struct Config {
    pub base_url: String,
    pub job_id: String,
    pub token: String,
    pub workdir: PathBuf,
    pub tuning: Tuning,
    pub log: LogConfig,
    /// The ceiling on the job's process count: [`steps::MAX_PROCS`], or
    /// `STRATUM_RUNNER_MAX_PROCS` when the operator lowered it — which is
    /// also how the end-to-end suite proves the bound without running a
    /// real fork bomb on the machine doing the testing.
    pub max_procs: u64,
    /// When set, the job's clone URL is re-based onto this origin before
    /// the checkout. The server builds `clone_url` from the address it
    /// believes it is reachable at; this machine clones from the address
    /// it registered with instead — the one it has already proved it can
    /// reach. The agent always sets it. `None` clones from exactly the URL
    /// the job names, which is what the in-crate tests use, their origins
    /// being local paths.
    pub clone_via: Option<String>,
}

/// The changeset with every member's clone URL re-based onto `base`.
///
/// Same reason as the job's own clone URL: the server built these from
/// the address it believes it is reachable at, and this machine clones
/// from the one it registered with. `None` leaves them exactly as they
/// came, as [`Config::clone_via`] does for the job's own.
fn rebased(cs: &ChangesetSpec, base: Option<&str>) -> ChangesetSpec {
    ChangesetSpec {
        key: cs.key.clone(),
        members: cs
            .members
            .iter()
            .map(|m| spec::MemberSpec {
                clone_url: match base {
                    Some(b) => agent::clone_url_via(b, &m.clone_url),
                    None => m.clone_url.clone(),
                },
                ..m.clone()
            })
            .collect(),
    }
}

/// The whole job, with the flag a signal raises passed in.
///
/// Stderr here is the runner's own narration, not the build's: one line
/// per phase, which is what the operator's terminal or journal shows. The
/// build's output never comes here — it goes to the job log, which is
/// what the person who pushed actually reads.
///
/// The flag is injected rather than read from `signals` directly so the
/// whole of this can be tested. `cargo test` runs a suite as threads in
/// one process and a signal flag is process-global, so a test that raised
/// a real SIGTERM would cancel whatever job another test happened to be
/// running. The real signal is proven end to end against the built binary
/// instead, where it has a process to itself.
fn run_with(cfg: &Config, stop: &'static AtomicBool) -> i32 {
    let client = Arc::new(Client::new(
        &cfg.base_url,
        &cfg.job_id,
        &cfg.token,
        cfg.tuning,
    ));
    let assignment = match client.fetch() {
        Ok(a) => a,
        // Not an error: the job was cancelled or superseded between the
        // claim and the fetch. 0 keeps it out of the failures an operator
        // is meant to be able to trust.
        Err(CallError::Gone) => {
            eprintln!("job {}: no longer running, nothing to do", cfg.job_id);
            return 0;
        }
        Err(e @ CallError::Refused(_)) => {
            eprintln!("job {}: refused: {e}", cfg.job_id);
            return 2;
        }
        Err(e) => {
            eprintln!("job {}: cannot reach the control plane: {e}", cfg.job_id);
            return 1;
        }
    };
    eprintln!(
        "job {} attempt {}: {} on {} ({})",
        assignment.id, assignment.attempt, assignment.key, assignment.ref_name, assignment.event
    );

    if let Err(e) = std::fs::create_dir_all(&cfg.workdir) {
        eprintln!("job {}: cannot create the workdir: {e}", cfg.job_id);
        return 2;
    }
    let log_path = cfg.workdir.join("job.log");
    let cancelled = Arc::new(AtomicBool::new(false));
    // A signal from here on — the operator stopping the runner — gets the
    // ending a 410 does, on the same flag: the step's group is killed,
    // the log is flushed, and nothing is reported.
    let _stopping = signals::bridge(stop, Arc::clone(&cancelled));
    let log = match Log::start(
        Arc::clone(&client),
        &log_path,
        cfg.log,
        Arc::clone(&cancelled),
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("job {}: {e}", cfg.job_id);
            return 2;
        }
    };

    // One budget for the whole job, checkout included: a fetch that hangs
    // against a wedged proxy is exactly as expensive as a step that does.
    let deadline =
        Instant::now() + Duration::from_secs(u64::from(assignment.spec.timeout_minutes) * 60);
    let inherited = spec::inherited_env();

    let ctx = steps::Ctx {
        sink: log.sink(),
        deadline,
        cancelled: &cancelled,
        abuse: Arc::new(watch::Abuse::default()),
        max_procs: cfg.max_procs,
    };
    let clone_url = match &cfg.clone_via {
        Some(base) => agent::clone_url_via(base, &assignment.clone_url),
        None => assignment.clone_url.clone(),
    };
    // A composed job gets every member of its changeset, side by side
    // under `workspace`, and runs its steps in its own member's tree; an
    // ordinary job gets the one repository, as it always has.
    let composed = assignment
        .changeset
        .as_ref()
        .map(|cs| rebased(cs, cfg.clone_via.as_deref()));
    let (repo, workspace) = match &composed {
        Some(cs) => {
            let root = checkout::workspace(&cfg.workdir);
            (root.join(&cs.own().repo), Some(root))
        }
        None => (cfg.workdir.join("repo"), None),
    };
    let mut outcome = match &composed {
        Some(cs) => checkout::members(&cfg.workdir, cs, &cfg.token, &inherited, &ctx),
        None => checkout::checkout(
            &cfg.workdir,
            &clone_url,
            &assignment.fetch_ref,
            &assignment.commit_sha,
            &cfg.token,
            &inherited,
            &ctx,
        ),
    };
    if outcome == steps::Outcome::Passed {
        eprintln!("job {}: checked out, running steps", assignment.id);
        outcome = steps::run_steps(&assignment, &repo, workspace.as_deref(), &inherited, &ctx);
    } else {
        // The steps never ran; say which ones, so the log a person reads
        // is complete whether the checkout or a step was the problem.
        steps::not_run(&assignment, 0, log.sink());
    }

    let (state, error, abuse) = match outcome {
        steps::Outcome::Passed => ("passed", None, None),
        steps::Outcome::Failed(e) => ("failed", Some(e), None),
        steps::Outcome::TimedOut => (
            "failed",
            Some(format!(
                "timed out after {} minutes",
                assignment.spec.timeout_minutes
            )),
            None,
        ),
        steps::Outcome::Cancelled => ("cancelled", None, None),
        // The one verdict that says what *kind* of failure it was. The
        // server records it in the organisation's audit trail, so it is a
        // separate field rather than a phrase in `error` that somebody
        // would have to match on.
        steps::Outcome::Abuse(name) => (
            "failed",
            Some(format!("mining software detected: {name}")),
            Some("mining"),
        ),
    };
    if state != "cancelled" {
        log.sink().line(&match &error {
            Some(e) => format!("\n{state}: {e}"),
            None => format!("\n{state}"),
        });
    }
    log.finish();

    if cancelled.load(Ordering::SeqCst) || state == "cancelled" {
        eprintln!("job {}: cancelled, exiting quietly", assignment.id);
        return 0;
    }

    // The complete log first, then the verdict: the order a reader needs.
    // A dashboard that sees `passed` and then finds a truncated log reads
    // as a product that loses output.
    let complete = std::fs::read_to_string(&log_path).unwrap_or_default();
    if let Err(e) = client.put_log(&complete) {
        // Not fatal. The chunks are already there, and a verdict with a
        // partial log beats no verdict at all.
        eprintln!(
            "job {}: could not upload the complete log: {e}",
            assignment.id
        );
    }
    match client.finish(state, error.as_deref(), abuse) {
        Ok(()) => {
            eprintln!("job {}: reported {state}", assignment.id);
            0
        }
        // The run settled underneath us — someone cancelled it while the
        // last step was finishing. Its verdict is not ours to report.
        Err(CallError::Gone) => {
            eprintln!("job {}: settled elsewhere while finishing", assignment.id);
            0
        }
        Err(e) => {
            eprintln!("job {}: could not report {state}: {e}", assignment.id);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fakecp::{spec_json, FakeCp, Reply};
    use testdir::TestDir;

    /// The tests drive `run_with` off this rather than off the flag a real
    /// signal raises: that one is process-global, and `cargo test` runs
    /// this suite as threads in a single process, so one test's signal
    /// would cancel another test's job.
    static TEST_STOP: AtomicBool = AtomicBool::new(false);

    struct Fixture {
        cp: FakeCp,
        dir: TestDir,
    }

    impl Fixture {
        fn new(steps: serde_json::Value) -> Fixture {
            let f = Fixture {
                cp: FakeCp::start(),
                dir: TestDir::new("run"),
            };
            f.cp.set_spec(&spec_json(steps));
            f
        }

        /// The job's clone source: a real repository, checked out by the
        /// real `git` CLI, exactly as production does it.
        fn with_origin(self, steps: serde_json::Value) -> Fixture {
            let (url, shas) = checkout::tests::origin(self.dir.path());
            let head = shas.split(' ').nth(1).expect("two shas").to_string();
            let mut doc = spec_json(steps);
            doc["clone_url"] = serde_json::Value::String(url);
            doc["commit_sha"] = serde_json::Value::String(head);
            self.cp.set_spec(&doc);
            self
        }

        fn config(&self) -> Config {
            Config {
                base_url: self.cp.base_url(),
                job_id: "job1".into(),
                token: "tok".into(),
                workdir: self.dir.path().join("work"),
                tuning: Tuning {
                    retry_base_ms: 1,
                    ..Tuning::default()
                },
                log: LogConfig {
                    flush: Duration::from_millis(5),
                    heartbeat: Duration::from_millis(50),
                    ..LogConfig::default()
                },
                max_procs: steps::MAX_PROCS,
                clone_via: None,
            }
        }
    }

    #[test]
    fn a_passing_job_checks_out_runs_its_steps_and_reports_passed() {
        let f = Fixture::new(serde_json::json!([])).with_origin(serde_json::json!([
            {"name": "Read", "run": "cat README.md"},
            {"name": "Env", "run": "echo $WEFT_JOB $WEFT_EVENT $CI"},
        ]));
        assert_eq!(run_with(&f.config(), &TEST_STOP), 0);
        let s = f.cp.state();
        assert_eq!(s.finish, Some(("passed".into(), None)));
        assert_eq!(s.abuse, None, "an ordinary verdict carries no `abuse`");
        let log = s.final_log.clone().expect("the complete log was uploaded");
        assert!(log.starts_with("▶ Checkout\n"), "{log}");
        assert!(log.contains("▶ Read\ntwo\n✓ Read (0s)"), "{log}");
        assert!(log.contains("▶ Env\ntest push true\n"), "{log}");
        assert!(log.trim_end().ends_with("passed"), "{log}");
        // The live view saw it too, not only the final upload.
        assert!(s.streamed().contains("▶ Read"), "{}", s.streamed());
    }

    /// A composed job document over two real local repositories, with
    /// `api` as the job's own member. Returns the document it served, so
    /// that a test can break one member of it.
    fn composed(f: &Fixture, steps: serde_json::Value, via: Option<&str>) -> serde_json::Value {
        let (api, api_shas) = checkout::tests::origin_named(f.dir.path(), "api");
        let (web, web_shas) = checkout::tests::origin_named(f.dir.path(), "web");
        let api_head = api_shas.split(' ').nth(1).expect("two shas").to_string();
        let web_head = web_shas.split(' ').nth(1).expect("two shas").to_string();
        // A runner is handed the control plane's own URLs and
        // re-bases them onto the address it registered with; `via` is
        // what that address would be here.
        let url = |real: &str, name: &str| match via {
            Some(_) => format!("http://control-plane.invalid/{name}"),
            None => real.to_string(),
        };
        let mut doc = spec_json(steps);
        doc["event"] = serde_json::json!("changeset");
        doc["change_key"] = serde_json::json!("Iapi");
        doc["clone_url"] = serde_json::json!(url(&api, "api"));
        doc["commit_sha"] = serde_json::json!(api_head);
        doc["changeset"] = serde_json::json!({
            "key": "Ic5000001",
            "members": [
                {"repo": "api", "change": "Iapi", "clone_url": url(&api, "api"),
                 "fetch_ref": "refs/heads/main", "commit_sha": api_head,
                 "token": serde_json::Value::Null},
                {"repo": "web", "change": "Iweb", "clone_url": url(&web, "web"),
                 "fetch_ref": "refs/heads/main", "commit_sha": web_head,
                 "token": "member-s3cret"},
            ],
        });
        f.cp.set_spec(&doc);
        doc
    }

    #[test]
    fn a_composed_job_runs_its_steps_in_its_own_member_beside_the_others() {
        let f = Fixture::new(serde_json::json!([]));
        let doc = composed(
            &f,
            serde_json::json!([
                // `pwd` proves the cwd, not merely that a README is
                // readable from wherever the steps happen to start.
                {"name": "Where", "run": "pwd; cat README.md; git rev-parse HEAD"},
                {"name": "Sibling", "run": "cat $WEFT_WORKSPACE/web/README.md"},
                {"name": "Vars", "run": "echo $WEFT_EVENT $WEFT_CHANGESET $WEFT_CHANGE"},
                {"name": "Members", "run": "echo $WEFT_CHANGESET_MEMBERS"},
            ]),
            None,
        );
        let api_head = doc["commit_sha"].as_str().expect("a sha").to_string();
        assert_eq!(run_with(&f.config(), &TEST_STOP), 0);
        let s = f.cp.state();
        assert_eq!(s.finish, Some(("passed".into(), None)));
        let log = s.final_log.clone().expect("the complete log was uploaded");
        let work = f.dir.path().join("work/workspace");
        assert!(log.starts_with("▶ Checkout api\n"), "{log}");
        assert!(log.contains("▶ Checkout web\n"), "{log}");
        assert!(
            log.contains(&format!("{}\ntwo\n{api_head}", work.join("api").display())),
            "{log}"
        );
        assert!(log.contains("▶ Sibling\ntwo\n"), "{log}");
        assert!(log.contains("▶ Vars\nchangeset Ic5000001 Iapi\n"), "{log}");
        assert!(
            log.contains(&format!("\"path\":\"{}\"", work.join("web").display())),
            "{log}"
        );
        // The credential that fetched the sibling is not reachable from
        // the shell that was handed its tree.
        assert!(!log.contains("member-s3cret"), "{log}");
    }

    #[test]
    fn a_composed_job_on_a_self_hosted_runner_fetches_every_member_from_its_own_origin() {
        // The control plane's URLs are unreachable from here by
        // construction; the job can only pass if every member was
        // re-based onto the address this runner registered with.
        let f = Fixture::new(serde_json::json!([]));
        // The address this machine registered with: here, the directory
        // the two origins actually sit in.
        let via = f.dir.path().to_string_lossy().to_string();
        composed(
            &f,
            serde_json::json!([{"name": "Both", "run": "cat README.md $WEFT_WORKSPACE/web/README.md"}]),
            Some(&via),
        );
        let cfg = Config {
            clone_via: Some(via),
            ..f.config()
        };
        assert_eq!(run_with(&cfg, &TEST_STOP), 0);
        assert_eq!(f.cp.state().finish, Some(("passed".into(), None)));
    }

    #[test]
    fn a_member_that_cannot_be_materialised_fails_the_composed_job_by_name() {
        let f = Fixture::new(serde_json::json!([]));
        // Everything about the job is well-formed except the sibling's
        // ref, so the failure under test is the only one that can fire.
        let mut doc = composed(
            &f,
            serde_json::json!([{"name": "Test", "run": "echo ran"}]),
            None,
        );
        doc["changeset"]["members"][1]["fetch_ref"] = serde_json::json!("refs/heads/gone");
        f.cp.set_spec(&doc);
        assert_eq!(run_with(&f.config(), &TEST_STOP), 0);
        let s = f.cp.state();
        assert_eq!(
            s.finish,
            Some((
                "failed".into(),
                Some("member web: fetch of refs/heads/gone failed".into())
            ))
        );
        let log = s.final_log.expect("log");
        assert!(log.contains("– Test (not run)"), "{log}");
    }

    #[test]
    fn a_failing_step_is_reported_as_failed_and_names_the_step() {
        let f = Fixture::new(serde_json::json!([])).with_origin(serde_json::json!([
            {"name": "Test", "run": "echo nope >&2; exit 4"},
            {"name": "Ship", "run": "echo never"},
        ]));
        assert_eq!(
            run_with(&f.config(), &TEST_STOP),
            0,
            "a failed job still reported a verdict"
        );
        let s = f.cp.state();
        assert_eq!(
            s.finish,
            Some(("failed".into(), Some("step \"Test\" exited 4".into())))
        );
        let log = s.final_log.expect("log");
        assert!(log.contains("✗ Test exited 4"), "{log}");
        assert!(log.contains("– Ship (not run)"), "{log}");
    }

    #[test]
    fn a_job_that_outlives_its_budget_fails_with_the_timeout_wording() {
        let f = Fixture::new(serde_json::json!([]));
        let (url, shas) = checkout::tests::origin(f.dir.path());
        let mut doc = spec_json(serde_json::json!([{"name": "Sleep", "run": "sleep 60"}]));
        doc["clone_url"] = serde_json::Value::String(url);
        doc["commit_sha"] = serde_json::Value::String(shas.split(' ').nth(1).unwrap().into());
        // Zero minutes: the budget is spent before the first step starts.
        doc["timeout_minutes"] = serde_json::json!(0);
        f.cp.set_spec(&doc);
        assert_eq!(run_with(&f.config(), &TEST_STOP), 0);
        assert_eq!(
            f.cp.state().finish,
            Some(("failed".into(), Some("timed out after 0 minutes".into())))
        );
    }

    #[test]
    fn a_checkout_that_cannot_find_the_commit_fails_before_any_step_runs() {
        let f = Fixture::new(serde_json::json!([]));
        let (url, _) = checkout::tests::origin(f.dir.path());
        let mut doc = spec_json(serde_json::json!([{"name": "Test", "run": "echo ran"}]));
        doc["clone_url"] = serde_json::Value::String(url);
        f.cp.set_spec(&doc);
        assert_eq!(run_with(&f.config(), &TEST_STOP), 0);
        let s = f.cp.state();
        assert_eq!(
            s.finish,
            Some((
                "failed".into(),
                Some(
                    "commit 0000000000000000000000000000000000000000 is no longer on refs/heads/main"
                        .into()
                )
            ))
        );
        let log = s.final_log.expect("log");
        assert!(log.contains("– Test (not run)"), "{log}");
        assert!(!log.contains("ran"), "no step ran: {log}");
    }

    /// The same ending, reached the other way: not a 410 but the operator
    /// stopping the runner, which arrives as SIGTERM. The step here
    /// backgrounds a sleep and waits on it, so what is proven is that the
    /// whole process *group* goes — the bug was an orphan that outlived
    /// the runner and kept burning the machine's CPU with nobody watching.
    #[test]
    fn a_signal_kills_the_step_group_flushes_the_log_and_reports_nothing() {
        static STOP: AtomicBool = AtomicBool::new(false);
        let f = Fixture::new(serde_json::json!([])).with_origin(serde_json::json!([
            {"name": "Sleep", "run": "echo started; sleep 60 & echo $! > ../child.pid; wait"},
        ]));
        let cfg = f.config();
        let pidfile = cfg.workdir.join("child.pid");
        // The signal has to land while the step is genuinely running, so it
        // waits for the grandchild rather than for a duration.
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while std::fs::read_to_string(&pidfile).is_err() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            STOP.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        assert_eq!(run_with(&f.config(), &STOP), 0, "a stopped task exits 0");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "it waited out the sleep instead of being killed"
        );

        let grandchild: i32 = std::fs::read_to_string(f.config().workdir.join("child.pid"))
            .expect("the step recorded its child")
            .trim()
            .parse()
            .expect("a pid");
        assert!(
            steps::tests::stops_within(grandchild, Duration::from_secs(5)),
            "the step's process group outlived the runner (pid {grandchild})"
        );
        // What was written before the signal is on disk — the log writer is
        // flushed rather than abandoned mid-buffer.
        let log = std::fs::read_to_string(f.config().workdir.join("job.log")).expect("job.log");
        assert!(log.contains("▶ Sleep") && log.contains("started"), "{log}");
        let s = f.cp.state();
        assert_eq!(s.finish, None, "a stopped task does not answer for itself");
        assert_eq!(s.final_log, None);
    }

    #[test]
    fn a_cancelled_job_kills_its_step_reports_nothing_and_exits_zero() {
        let f = Fixture::new(serde_json::json!([])).with_origin(serde_json::json!([
            {"name": "Sleep", "run": "echo started; sleep 60"},
        ]));
        // 410 from the first chunk onwards — what a cancel looks like from
        // here.
        f.cp.gone_after_chunks(0);
        let started = Instant::now();
        assert_eq!(run_with(&f.config(), &TEST_STOP), 0);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "it waited out the sleep instead of being killed"
        );
        let s = f.cp.state();
        assert_eq!(s.finish, None, "a cancelled job reports no verdict");
        assert_eq!(s.final_log, None, "and uploads no authoritative log");
    }

    #[test]
    fn a_job_that_is_already_gone_exits_zero_without_doing_any_work() {
        let f = Fixture::new(serde_json::json!([{"run": "echo ran"}]));
        f.cp.script_spec(vec![Reply::status(410)]);
        assert_eq!(run_with(&f.config(), &TEST_STOP), 0);
        assert_eq!(f.cp.state().chunks.len(), 0);
        assert!(!f.dir.path().join("work").exists(), "no workdir was made");
    }

    #[test]
    fn bad_credentials_exit_two_and_an_unreachable_control_plane_exits_one() {
        let f = Fixture::new(serde_json::json!([{"run": "true"}]));
        f.cp.script_spec(vec![Reply::status(403)]);
        assert_eq!(run_with(&f.config(), &TEST_STOP), 2);

        let f = Fixture::new(serde_json::json!([{"run": "true"}]));
        f.cp.script_spec(vec![Reply::status(500); 6]);
        assert_eq!(run_with(&f.config(), &TEST_STOP), 1);
    }

    #[test]
    fn a_verdict_that_cannot_be_delivered_exits_one() {
        let f = Fixture::new(serde_json::json!([])).with_origin(serde_json::json!([]));
        // Every non-spec call fails: the chunks, the final PUT and all
        // three finish attempts.
        f.cp.script_other(vec![Reply::status(500); 40]);
        assert_eq!(run_with(&f.config(), &TEST_STOP), 1);
        // It tried — three times — and the control plane refused each one;
        // the exit code is the only thing left that can say so.
        assert_eq!(f.cp.state().finish_calls, 3);
    }

    #[test]
    fn a_run_that_settles_underneath_the_finish_call_is_not_our_failure() {
        let f = Fixture::new(serde_json::json!([])).with_origin(serde_json::json!([]));
        // The 410 arrives only at the very end, so the job has already
        // done its work and written its log.
        f.cp.gone_on_finish();
        assert_eq!(run_with(&f.config(), &TEST_STOP), 0);
    }

    #[test]
    fn a_workdir_that_cannot_be_created_exits_two() {
        let f = Fixture::new(serde_json::json!([{"run": "true"}]));
        let mut cfg = f.config();
        // A file where the parent directory should be: ENOTDIR whoever the
        // process is running as, where a path under `/` would succeed for
        // root and quietly stop testing anything.
        std::fs::write(f.dir.path().join("blocked"), "").expect("write");
        cfg.workdir = f.dir.path().join("blocked/work");
        assert_eq!(run_with(&cfg, &TEST_STOP), 2);

        // …and so does a workdir that exists but has no room for the log.
        let mut cfg = f.config();
        std::fs::create_dir_all(f.dir.path().join("work/job.log")).expect("mkdir");
        cfg.workdir = f.dir.path().join("work");
        assert_eq!(run_with(&cfg, &TEST_STOP), 2);
    }

    #[test]
    fn the_entry_point_routes_each_command_and_refuses_a_bare_one() {
        // `entry` installs the real handlers and `run` would poll the
        // process-wide signal flag, so it must not overlap the test that
        // raises a real signal.
        let _flag = signals::STOP_FLAG_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // No command is a usage error, not an attempt to run a job out of
        // whatever the environment happens to hold.
        assert_eq!(entry(&[]), 2);
        assert_eq!(entry(&["--help".to_string()]), 0);
        assert_eq!(entry(&["nonsense".to_string()]), 2);
        assert_eq!(
            entry(&["run".to_string(), "--dir".to_string()]),
            2,
            "a flag without its value is a usage error"
        );
        let dir = TestDir::new("entry-agent");
        assert_eq!(
            entry(&[
                "run".to_string(),
                "--dir".to_string(),
                dir.path().to_string_lossy().to_string(),
            ]),
            2,
            "and an unregistered directory is refused before any claim"
        );

        // …and `register` goes through the same door and leaves the file
        // `run` would have read.
        let cp = fakecp::FakeCp::start();
        assert_eq!(
            entry(&[
                "register".to_string(),
                "--url".to_string(),
                cp.base_url(),
                "--token".to_string(),
                "weftg_entry".to_string(),
                "--dir".to_string(),
                dir.path().to_string_lossy().to_string(),
            ]),
            0
        );
        assert!(dir.path().join(".runner").exists());
    }

    /// The whole of layer 3, from the step that starts a miner to the
    /// field the server records in the organisation's audit trail. The "miner"
    /// is `/bin/sleep` under the name `xmrig` — see the note on
    /// `steps::tests::a_step_that_starts_a_miner_is_killed_and_the_job_says_why`.
    ///
    /// The verdict is `failed` *and* carries `abuse`, which are two
    /// different statements: the first is what the change's author sees,
    /// the second is what tells the machine's owner what was tried on it.
    /// A job that reported only `failed` would look like any broken build.
    #[test]
    fn a_job_that_starts_a_miner_is_failed_as_abuse() {
        let f = Fixture::new(serde_json::json!([])).with_origin(serde_json::json!([
            {"name": "Build", "run": "ln -s /bin/sleep ./xmrig; ./xmrig 60"},
            {"name": "Test", "run": "echo never"},
        ]));
        let started = Instant::now();
        assert_eq!(run_with(&f.config(), &TEST_STOP), 0, "the verdict was sent");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "it waited out the miner instead of killing it"
        );
        let s = f.cp.state();
        assert_eq!(
            s.finish,
            Some((
                "failed".into(),
                Some("mining software detected: xmrig".into())
            ))
        );
        assert_eq!(s.abuse.as_deref(), Some("mining"));
        let log = s.final_log.expect("log");
        assert!(
            log.contains("\u{2717} Build stopped: mining software detected: xmrig"),
            "{log}"
        );
        assert!(log.contains("\u{2013} Test (not run)"), "{log}");
        assert!(
            log.trim_end()
                .ends_with("failed: mining software detected: xmrig"),
            "{log}"
        );
    }
}
