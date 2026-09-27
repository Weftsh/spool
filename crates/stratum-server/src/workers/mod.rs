//! Background workers, all tokio tasks in the one binary: the compactor
//! (WAL fold), epoch GC + deleted-repo sweeps, the billing rollup, and the
//! audit shipper. Intervals are config; 0 disables a worker.

pub mod cdnpack;
pub mod changeset_lander;
pub mod changeset_notifier;
pub mod checks_poll;
pub mod commit_count;
pub mod compactor;
pub mod contribs;
pub mod forker;
pub mod gc;
pub mod importer;
pub mod lander;
pub mod notifier;
pub mod notify;
pub mod promoter;
pub mod runner;
pub mod shipper;
pub mod signals;
pub mod sitepublish;
pub mod storage;

use std::time::Duration;

use crate::app::SharedState;

pub fn spawn_all(state: &SharedState) {
    compactor::spawn(state.clone());
    compactor::spawn_sweep(state.clone());
    contribs::spawn(state.clone());
    cdnpack::spawn(state.clone());
    checks_poll::spawn(state.clone());
    gc::spawn(state.clone());
    forker::spawn(state.clone());
    promoter::spawn(state.clone());
    importer::spawn(state.clone());
    shipper::spawn(state.clone());
    lander::spawn(state.clone());
    notifier::spawn(state.clone());
    changeset_notifier::spawn(state.clone());
    signals::spawn(state.clone());
    runner::spawn(state.clone());
    sitepublish::spawn(state.clone());
    storage::spawn(state.clone());
}

/// A job lease, in milliseconds, from a seconds-valued env knob.
/// Saturating rather than wrapping: a nonsense `STRATUM_*_LEASE_SECS`
/// should mean "effectively forever", never a negative lease that hands
/// every job to every worker at once.
pub(crate) fn lease_ms(name: &str, default_secs: u64) -> i64 {
    i64::try_from(env_secs(name, default_secs).saturating_mul(1000)).unwrap_or(i64::MAX)
}

/// "Not claimable until `secs` from now", in epoch milliseconds.
///
/// Saturating throughout. A provider is free to send a nonsense
/// `Retry-After` — a header is not a promise — and an overflow that
/// wrapped round to a moment in the past would make the row instantly
/// ready, which is the exact behaviour being fixed rather than a
/// degraded version of it.
pub(crate) fn not_before_ms(secs: u64) -> i64 {
    let now = stratum_control::ids::now_ms();
    i64::try_from(secs.saturating_mul(1000))
        .map(|d| now.saturating_add(d))
        .unwrap_or(i64::MAX)
}

pub(crate) fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A worker's poll interval, from the same env knob, but fractional.
///
/// Seconds-only was one granularity short of useful. Every poll knob's
/// floor was `1`, so an e2e test that has to prove a *negative* — no run
/// appeared, the dispatcher really is off — had nothing to wait on and
/// slept three whole seconds to get past three ticks. There were dozens
/// of those, and they were a measurable slice of the correctness gate.
/// `STRATUM_RUNNER_POLL_SECS=0.1` makes the same proof in 300ms.
///
/// `0` still disables the worker, exactly as before and as the module
/// docs promise — the callers check `is_zero()` where they checked
/// `== 0`. A value that is negative, not a number, or too large to be a
/// `Duration` falls back to the default rather than panicking in
/// `from_secs_f64`, which is the same forgiveness `env_secs` gives a
/// value that will not parse as `u64`.
pub(crate) fn env_period(name: &str, default_secs: u64) -> Duration {
    match std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
    {
        Some(secs) if secs.is_finite() && (0.0..=1e9).contains(&secs) => {
            Duration::from_secs_f64(secs)
        }
        _ => Duration::from_secs(default_secs),
    }
}

/// The scratch directory for **one run** of a repository job.
///
/// Keyed by the job as well as the repository, and that is the whole
/// point. Every one of these workers begins by deleting the scratch it
/// is about to use — `compact()` removes `compact-seed.git` and
/// `compact-staging` before materialising into them — so two runs
/// against one repository on the same node deleted each other's working
/// tree mid-flight. It happens: the `compact` route is synchronous and
/// the compactor polls, and a person pressing it while a queued job is
/// running is the ordinary case, not an exotic one. The failure reads as
/// a store or git problem and says nothing about the cause:
///
/// ```text
/// materialized stream rejected: fatal: cannot change to
///   '…/data/compact/01m1pzhk…/compact-seed.git': No such file or directory
/// ```
///
/// Found on a CI runner slow enough for the two to overlap, in a test
/// about something else entirely. A job id is unique, so a run now owns
/// its directory outright and removes it when it is done.
pub(crate) fn job_work_dir(
    data_dir: &std::path::Path,
    kind: &str,
    repo_id: &str,
    job_id: &str,
) -> std::path::PathBuf {
    data_dir.join(kind).join(repo_id).join(job_id)
}

#[cfg(test)]
mod tests {
    /// **Every worker module is actually spawned, and the sweep's
    /// interval is not zero.**
    ///
    /// A worker nobody starts does nothing, and nothing else in the
    /// suite notices — its tests still pass, because they call it
    /// directly. That is how the compaction sweep would be lost:
    /// delete one line from `spawn_all`, or set its default interval to
    /// `0`, and repositories quietly stop being folded again. The sweep
    /// exists *because* silent non-compaction cost a customer a 79x
    /// slowdown that nothing reported, so losing it silently is the one
    /// failure it must not have.
    #[test]
    fn every_worker_is_wired_into_spawn_all() {
        let src = include_str!("mod.rs");
        let all = src
            .split_once("pub fn spawn_all")
            .expect("spawn_all exists")
            .1;
        let body = all.split_once("\n}").expect("spawn_all has a body").0;

        for module in src
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pub mod "))
            .filter_map(|l| l.strip_suffix(';'))
        {
            // `mod` here is also used for shared helpers with nothing to
            // start; those declare no `spawn`.
            let has_spawn = std::fs::read_to_string(format!(
                "{}/src/workers/{module}.rs",
                env!("CARGO_MANIFEST_DIR")
            ))
            .map(|m| m.contains("pub fn spawn("))
            .unwrap_or(false);
            if !has_spawn {
                continue;
            }
            assert!(
                body.contains(&format!("{module}::spawn(")),
                "{module} declares a worker that `spawn_all` never starts"
            );
        }

        // The sweep is a second entry point in an already-spawned
        // module, so the loop above cannot see it.
        assert!(
            body.contains("compactor::spawn_sweep("),
            "the compaction sweep is not started: repositories whose WAL \
             gets ahead would go unfolded again, silently"
        );
    }

    /// On by default. A sweep that ships disabled is a sweep that does
    /// not exist, and the only sign would be somebody outside timing a
    /// push a month later.
    #[test]
    fn the_compaction_sweep_is_on_unless_someone_turns_it_off() {
        let src = include_str!("compactor.rs");
        let line = src
            .lines()
            .find(|l| l.contains("STRATUM_COMPACT_SWEEP_SECS"))
            .expect("the sweep reads its interval from the environment");
        assert!(
            !line.contains(", 0)"),
            "the sweep's default interval is 0, which disables it: {line}"
        );
    }

    use super::*;

    /// A provider's `Retry-After` becomes a moment in the future, and a
    /// nonsense one never becomes a moment in the past.
    ///
    /// The past is the failure that matters: `not_before <= now` makes
    /// the row instantly claimable, which is precisely the busy-loop this
    /// exists to stop. So an overflow must saturate forward, not wrap.
    #[test]
    fn a_retry_after_never_lands_in_the_past_however_large_it_is() {
        let now = stratum_control::ids::now_ms();

        let soon = not_before_ms(5);
        assert!(
            soon >= now + 5_000,
            "{soon} is not five seconds after {now}"
        );
        assert!(soon <= now + 6_000, "{soon} is far too late");

        // Zero is "ready", which is what the ordinary budget-spent path
        // wants and why callers can pass it without a branch.
        assert!(not_before_ms(0) <= stratum_control::ids::now_ms());

        // Nonsense from a provider saturates forward.
        assert_eq!(not_before_ms(u64::MAX), i64::MAX);
        assert!(not_before_ms(u64::MAX / 1000) > now);
    }

    /// The knob a worker polls on, in all the shapes an operator or a
    /// test can hand it.
    ///
    /// The zero case is the one with teeth: `0 disables a worker` is a
    /// promise the module docs make and every `spawn` relies on, and
    /// moving from `u64` to `f64` is exactly the kind of edit that would
    /// quietly turn it into "poll as fast as you can".
    #[test]
    fn a_poll_interval_may_be_fractional_and_zero_still_disables_the_worker() {
        let name = "STRATUM_TEST_ENV_PERIOD";

        std::env::remove_var(name);
        assert_eq!(env_period(name, 5), Duration::from_secs(5));

        std::env::set_var(name, "0.1");
        assert_eq!(env_period(name, 5), Duration::from_millis(100));

        std::env::set_var(name, "2");
        assert_eq!(env_period(name, 5), Duration::from_secs(2));

        // Disabled, in both the spellings a person might write.
        std::env::set_var(name, "0");
        assert!(env_period(name, 5).is_zero());
        std::env::set_var(name, "0.0");
        assert!(env_period(name, 5).is_zero());

        // Nonsense falls back to the default rather than panicking:
        // `Duration::from_secs_f64` panics on a negative or overflowing
        // value, and a typo in a deployment env var must not take the
        // process down on boot.
        for bad in ["", "  ", "soon", "-1", "NaN", "inf", "1e300"] {
            std::env::set_var(name, bad);
            assert_eq!(
                env_period(name, 7),
                Duration::from_secs(7),
                "{bad:?} should have fallen back to the default"
            );
        }

        // Surrounding whitespace is an env-file accident, not a typo.
        std::env::set_var(name, " 0.25 ");
        assert_eq!(env_period(name, 5), Duration::from_millis(250));

        std::env::remove_var(name);
    }

    #[test]
    fn two_runs_against_one_repository_do_not_share_a_scratch_directory() {
        let root = std::path::Path::new("/data");
        let a = job_work_dir(root, "compact", "repo1", "job1");
        let b = job_work_dir(root, "compact", "repo1", "job2");
        assert_ne!(a, b, "two jobs may run at once and each clears its own");
        // Still under the repository, so an operator can find it and a
        // stray directory names what it belonged to.
        assert!(a.starts_with(root.join("compact").join("repo1")), "{a:?}");
        // And two repositories never collide whatever the job ids.
        assert_ne!(a, job_work_dir(root, "compact", "repo2", "job1"));
        assert_ne!(a, job_work_dir(root, "cdnpack", "repo1", "job1"));
    }
}
