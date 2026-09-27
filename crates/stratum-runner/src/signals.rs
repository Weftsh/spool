//! Being stopped: what a `StopTask` has to mean to a job that is running.
//!
//! A superseded run is stopped with `docker stop` / an ECS `StopTask`,
//! which is SIGTERM and then SIGKILL thirty seconds later. Neither default
//! disposition is survivable for us. As an ordinary child the runner dies
//! *instantly*, in the middle of a step: the log writer's pending bytes are
//! never flushed, and — worse — the step is its own process group, so the
//! `cargo build` it spawned is orphaned and keeps burning the task's CPU
//! until the container itself is torn down. As PID 1 with no init to
//! forward it, SIGTERM is ignored outright and the job runs happily to the
//! end of a run nobody wants any more.
//!
//! So the runner catches it, and treats it as exactly what it is: the
//! control plane no longer wants this job. That is the same fact a 410
//! already conveys, and it is deliberately routed onto the same
//! `cancelled` flag rather than into a second ending of its own — the step
//! group is killed, the log is flushed, and the process exits 0 without
//! reporting a verdict, because a task that was stopped does not get to
//! answer for itself.
//!
//! **In signal context, only a store.** The handler sets one atomic and
//! returns; killing the group, writing the log and closing out the job all
//! happen on the threads that were already going to poll `cancelled`.
//! Anything else here — a `println!`, a `kill`, allocating — is not
//! async-signal-safe, and a runner that deadlocks inside a handler is a
//! runner that gets SIGKILLed with its log still in memory.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Raised by the handler, read by [`bridge`]. Process-global because a
/// signal handler cannot be given anything else.
static STOPPED: AtomicBool = AtomicBool::new(false);

/// The signals a stopped task arrives as: SIGTERM from the orchestrator,
/// SIGINT from a person with a terminal.
const CAUGHT: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGINT];

/// How often [`bridge`] looks. Well inside the runner's own 50 ms step
/// poll, so the signal is never the slow part of letting go.
const POLL: Duration = Duration::from_millis(20);

extern "C" fn on_stop(_sig: libc::c_int) {
    STOPPED.store(true, Ordering::SeqCst);
}

/// The flag the installed handlers raise. Taken by reference rather than
/// read directly so that a test can drive the whole of `run` off a flag of
/// its own: a real signal in the test process would be delivered to a
/// process-wide flag that every other test's job is polling.
/// Held by any test that raises a real signal or reads [`stop_flag`].
///
/// The flag is process-wide — a handler cannot be given anything else —
/// and `cargo test` runs this suite as threads in one process, so a test
/// that raises a signal would otherwise cancel whatever job another test
/// happens to be running.
#[cfg(test)]
pub(crate) static STOP_FLAG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn stop_flag() -> &'static AtomicBool {
    &STOPPED
}

/// Handlers installed for as long as this is held.
///
/// Restoring on drop is not tidiness. `cargo test` runs the suite in one
/// process, so a test that installed a permanent SIGINT handler would leave
/// the whole run unable to be interrupted with Ctrl-C for every test after
/// it.
pub struct Handlers(Vec<(libc::c_int, libc::sigaction)>);

impl Drop for Handlers {
    fn drop(&mut self) {
        for (sig, old) in &self.0 {
            // Nothing useful to do if this fails, and it cannot: the signal
            // number and the action both came from a call that succeeded.
            unsafe { libc::sigaction(*sig, old, std::ptr::null_mut()) };
        }
    }
}

pub fn install() -> Result<Handlers, String> {
    install_for(&CAUGHT)
}

fn install_for(sigs: &[libc::c_int]) -> Result<Handlers, String> {
    let mut installed = Handlers(Vec::with_capacity(sigs.len()));
    for &sig in sigs {
        // `?` drops `installed`, and its Drop puts back whatever has been
        // recorded so far — a failure halfway through a list does not leave
        // the process half-handled.
        installed.0.push((sig, install_one(sig)?));
    }
    Ok(installed)
}

fn install_one(sig: libc::c_int) -> Result<libc::sigaction, String> {
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = on_stop as extern "C" fn(libc::c_int) as usize;
        // SA_RESTART so that a signal arriving while the runner is blocked
        // in a read against the control plane resumes the read instead of
        // failing it with EINTR — the job is being cancelled, not broken,
        // and the difference shows up as a spurious error in the log.
        act.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut act.sa_mask);
        let mut old: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(sig, &act, &mut old) != 0 {
            return Err(format!(
                "cannot catch signal {sig}: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(old)
    }
}

/// Copies `source` onto the job's `cancelled` flag, so that a signal ends
/// the job through the path a 410 already takes.
///
/// A thread rather than a check bolted onto every polling loop: there are
/// three of them (the step watch, the checkout watch, the log writer) and a
/// fourth would be added by the next person who adds a loop and does not
/// know about this one.
pub struct Bridge {
    done: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub fn bridge(source: &'static AtomicBool, cancelled: Arc<AtomicBool>) -> Bridge {
    let done = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&done);
    let join = std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            if source.load(Ordering::SeqCst) {
                cancelled.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(POLL);
        }
    });
    Bridge {
        done,
        join: Some(join),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A signal's disposition is per *process*, not per test, and this
    /// suite runs as threads in one. Two tests installing a handler for the
    /// same signal at once read each other's state and fail on it — which
    /// is what these two did before this lock, in the parallel run only.
    /// Any test that touches a disposition must hold this.
    static DISPOSITION: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn disposition(sig: libc::c_int) -> libc::sighandler_t {
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            assert_eq!(libc::sigaction(sig, std::ptr::null(), &mut old), 0);
            old.sa_sigaction
        }
    }

    /// The guard exists so that `cargo test` stays interruptible: a test
    /// that installed a permanent SIGINT handler would swallow Ctrl-C for
    /// every test after it. SIGUSR2 stands in for the real pair here so
    /// that asserting on the restore cannot itself change how this process
    /// answers a stop.
    #[test]
    fn handlers_are_put_back_when_the_guard_goes() {
        let _guard = DISPOSITION.lock().unwrap_or_else(|e| e.into_inner());
        let before = disposition(libc::SIGUSR2);
        {
            let _h = install_for(&[libc::SIGUSR2]).expect("install");
            assert_ne!(
                disposition(libc::SIGUSR2),
                before,
                "the handler is installed while the guard is held"
            );
        }
        assert_eq!(disposition(libc::SIGUSR2), before);
    }

    /// SIGKILL cannot be caught, which is the one input that makes
    /// `sigaction` refuse. The signals before it in the list are put back
    /// on the way out rather than left installed.
    #[test]
    fn a_signal_that_cannot_be_caught_is_an_error_and_unwinds_the_list() {
        let _guard = DISPOSITION.lock().unwrap_or_else(|e| e.into_inner());
        let before = disposition(libc::SIGUSR2);
        let e = install_for(&[libc::SIGUSR2, libc::SIGKILL]).map(|_| ());
        assert!(e
            .expect_err("SIGKILL cannot be caught")
            .starts_with(&format!("cannot catch signal {}", libc::SIGKILL)),);
        assert_eq!(
            disposition(libc::SIGUSR2),
            before,
            "a failure halfway through does not leave the process half-handled"
        );
    }

    /// The handler itself, driven by a real signal. SIGUSR2 stands in for
    /// SIGTERM so that the process this suite runs in is never actually
    /// asked to stop; what is being proven is that a signal — any signal
    /// we catch — reaches the flag `bridge` watches, which is the one link
    /// in the chain a job test cannot exercise.
    #[test]
    fn a_caught_signal_raises_the_flag_the_bridge_watches() {
        let _disposition = DISPOSITION.lock().unwrap_or_else(|e| e.into_inner());
        let _flag = STOP_FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let _h = install_for(&[libc::SIGUSR2]).expect("install");
            assert!(!stop_flag().load(Ordering::SeqCst));
            assert_eq!(unsafe { libc::raise(libc::SIGUSR2) }, 0);
            assert!(
                stop_flag().load(Ordering::SeqCst),
                "the handler ran and raised the flag"
            );
        }
        // Put it back before another test's job starts polling it.
        STOPPED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn the_bridge_puts_a_raised_signal_onto_the_jobs_cancelled_flag() {
        static SOURCE: AtomicBool = AtomicBool::new(false);
        let cancelled = Arc::new(AtomicBool::new(false));
        let _b = bridge(&SOURCE, Arc::clone(&cancelled));
        assert!(
            !cancelled.load(Ordering::SeqCst),
            "nothing has happened yet"
        );
        SOURCE.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !cancelled.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the bridge never noticed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The ordinary ending: no signal ever arrives, and dropping the guard
    /// has to stop the thread rather than wait out a poll that never ends.
    #[test]
    fn a_job_that_is_never_stopped_leaves_no_thread_behind() {
        static QUIET: AtomicBool = AtomicBool::new(false);
        let cancelled = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        drop(bridge(&QUIET, Arc::clone(&cancelled)));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!cancelled.load(Ordering::SeqCst));
    }
}
