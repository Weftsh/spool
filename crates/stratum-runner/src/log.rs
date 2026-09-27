//! The job log: one file on disk, streamed to the control plane in chunks
//! while it is being written.
//!
//! Two consumers with different needs. A person watching a running job
//! wants bytes now, which is what the chunk POSTs are for; the record that
//! survives the job wants to be complete and in order, which is what the
//! single `PUT …/log` at the end is for. Dropping a chunk under load is
//! therefore not a correctness failure — it costs a few seconds of live
//! view, and the authoritative upload fills it back in. That asymmetry is
//! why the chunk path retries once and gives up while the final upload
//! retries properly.
//!
//! Everything happens on one writer thread. Steps hand it bytes through a
//! channel and never block on the network: a control plane having a bad
//! minute must not slow down somebody's build.

use crate::client::{CallError, Client};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Where step output goes. Cheap to clone, and cloning is how a reader
/// thread gets one.
#[derive(Clone)]
pub struct Sink(Sender<Vec<u8>>);

impl Sink {
    /// Raw output, exactly as the child produced it.
    ///
    /// A closed channel is ignored: the writer thread only goes away when
    /// the job is over, and losing the tail of a killed step's output is
    /// not worth a panic in a process that is trying to report a verdict.
    pub fn bytes(&self, b: &[u8]) {
        let _ = self.0.send(b.to_vec());
    }

    /// One line of the runner's own narration — a step header, a verdict.
    pub fn line(&self, s: &str) {
        self.bytes(format!("{s}\n").as_bytes());
    }

    /// A sink over a channel the caller owns, so a test can read what a
    /// step wrote with no file and no control plane behind it.
    #[cfg(test)]
    pub fn from_sender(tx: Sender<Vec<u8>>) -> Sink {
        Sink(tx)
    }
}

/// Cadences. Fields rather than constants so a test can prove the
/// heartbeat fires without waiting thirty seconds for it.
#[derive(Debug, Clone, Copy)]
pub struct LogConfig {
    /// How often pending bytes are sent as a chunk.
    pub flush: Duration,
    /// How long the log may be silent before the lease is renewed anyway.
    pub heartbeat: Duration,
    /// Send early once this much is pending, so a chatty step does not
    /// build a multi-megabyte POST.
    pub max_chunk: usize,
}

impl Default for LogConfig {
    fn default() -> LogConfig {
        LogConfig {
            flush: Duration::from_millis(1000),
            heartbeat: Duration::from_secs(30),
            // The control plane refuses a chunk over 256 KiB; stay well
            // under it so a single oversized write can never be rejected.
            max_chunk: 64 * 1024,
        }
    }
}

pub struct Log {
    sink: Sink,
    handle: std::thread::JoinHandle<()>,
}

impl Log {
    /// Start the writer against `path`, streaming to `client`.
    ///
    /// `cancelled` is set — never cleared — the moment any call answers
    /// 410. It is the runner's only cancellation signal, and it is set
    /// here because the log is the one thing that talks to the control
    /// plane continuously while a step runs.
    pub fn start(
        client: Arc<Client>,
        path: &Path,
        cfg: LogConfig,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Log, String> {
        let mut file = std::fs::File::create(path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let handle = std::thread::spawn(move || {
            let mut pending: Vec<u8> = Vec::new();
            let mut seq: u32 = 0;
            let mut last_send = Instant::now();
            loop {
                match rx.recv_timeout(cfg.flush) {
                    Ok(b) => {
                        let _ = file.write_all(&b);
                        pending.extend_from_slice(&b);
                    }
                    // Every sender is gone: the job is over. Flush what is
                    // left and stop.
                    Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                let due = last_send.elapsed() >= cfg.flush || pending.len() >= cfg.max_chunk;
                if !pending.is_empty() && due {
                    seq += 1;
                    send_chunk(&client, seq, &pending, &cancelled);
                    pending.clear();
                    last_send = Instant::now();
                } else if pending.is_empty() && last_send.elapsed() >= cfg.heartbeat {
                    heartbeat(&client, &cancelled);
                    last_send = Instant::now();
                }
            }
            let _ = file.flush();
            if !pending.is_empty() {
                send_chunk(&client, seq + 1, &pending, &cancelled);
            }
        });
        Ok(Log {
            sink: Sink(tx),
            handle,
        })
    }

    pub fn sink(&self) -> &Sink {
        &self.sink
    }

    /// Close the channel and wait for the writer to drain it, so the file
    /// on disk is complete before it is read back for the final upload.
    pub fn finish(self) {
        let Log { sink, handle } = self;
        drop(sink);
        let _ = handle.join();
    }
}

/// One chunk, retried once.
///
/// The retry is safe because the control plane treats a repeated `seq` as
/// an overwrite; without that it would be a way to duplicate a build's
/// output into the middle of its own log.
fn send_chunk(client: &Client, seq: u32, bytes: &[u8], cancelled: &AtomicBool) {
    if cancelled.load(Ordering::SeqCst) {
        return;
    }
    // Lossy because a chunk boundary can land inside a multi-byte
    // character. The complete upload at the end is byte-exact, so the
    // damage is confined to the live view of one chunk edge.
    let text = String::from_utf8_lossy(bytes);
    match client.post_chunk(seq, &text) {
        Ok(()) => {}
        Err(CallError::Gone) => cancelled.store(true, Ordering::SeqCst),
        Err(_) => {
            if let Err(CallError::Gone) = client.post_chunk(seq, &text) {
                cancelled.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// Nothing to say, but still alive. Without this a job whose steps are
/// quiet for minutes loses its lease and is reclaimed by another runner.
fn heartbeat(client: &Client, cancelled: &AtomicBool) {
    if cancelled.load(Ordering::SeqCst) {
        return;
    }
    if let Err(CallError::Gone) = client.renew() {
        cancelled.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Tuning;
    use crate::fakecp::{FakeCp, Reply};

    fn tuning() -> Tuning {
        Tuning {
            retry_base_ms: 1,
            ..Tuning::default()
        }
    }

    struct Fixture {
        cp: FakeCp,
        dir: crate::testdir::TestDir,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture {
                cp: FakeCp::start(),
                dir: crate::testdir::TestDir::new("log"),
            }
        }

        fn log(&self, cfg: LogConfig, cancelled: &Arc<AtomicBool>) -> Log {
            let client = Arc::new(Client::new(&self.cp.base_url(), "job1", "tok", tuning()));
            Log::start(
                client,
                &self.dir.path().join("job.log"),
                cfg,
                Arc::clone(cancelled),
            )
            .expect("start log")
        }

        fn file(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("job.log")).expect("read log")
        }
    }

    #[test]
    fn everything_written_reaches_both_the_file_and_the_chunk_stream() {
        let f = Fixture::new();
        let cancelled = Arc::new(AtomicBool::new(false));
        let log = f.log(
            LogConfig {
                flush: Duration::from_millis(5),
                ..LogConfig::default()
            },
            &cancelled,
        );
        log.sink().line("▶ Checkout");
        log.sink().bytes(b"cloning\n");
        log.finish();
        assert_eq!(f.file(), "▶ Checkout\ncloning\n");
        assert_eq!(f.cp.state().streamed(), "▶ Checkout\ncloning\n");
        assert!(!cancelled.load(Ordering::SeqCst));
    }

    #[test]
    fn a_chunk_is_sent_early_once_enough_is_pending() {
        let f = Fixture::new();
        let cancelled = Arc::new(AtomicBool::new(false));
        let log = f.log(
            LogConfig {
                // A flush cadence long enough that only the size trigger
                // can be what sent the chunks.
                flush: Duration::from_secs(30),
                max_chunk: 16,
                ..LogConfig::default()
            },
            &cancelled,
        );
        for _ in 0..8 {
            log.sink().bytes(b"0123456789");
        }
        log.finish();
        let s = f.cp.state();
        assert!(s.chunks.len() >= 4, "{:?}", s.chunks);
        assert_eq!(s.streamed(), "0123456789".repeat(8));
    }

    #[test]
    fn a_silent_job_still_renews_its_lease() {
        let f = Fixture::new();
        let cancelled = Arc::new(AtomicBool::new(false));
        let log = f.log(
            LogConfig {
                flush: Duration::from_millis(2),
                heartbeat: Duration::from_millis(5),
                ..LogConfig::default()
            },
            &cancelled,
        );
        wait_until(|| f.cp.state().leases >= 2);
        log.finish();
        assert!(f.cp.state().leases >= 2);
        assert_eq!(f.cp.state().chunks.len(), 0, "silence is not a chunk");
    }

    #[test]
    fn a_failed_chunk_is_retried_once_and_then_dropped() {
        let f = Fixture::new();
        f.cp.script_other(vec![Reply::status(500), Reply::status(503)]);
        let cancelled = Arc::new(AtomicBool::new(false));
        let log = f.log(
            LogConfig {
                flush: Duration::from_millis(5),
                ..LogConfig::default()
            },
            &cancelled,
        );
        log.sink().bytes(b"lost\n");
        wait_until(|| f.cp.state().chunks.len() >= 2);
        log.sink().bytes(b"kept\n");
        log.finish();
        let s = f.cp.state();
        // Both attempts at seq 1 were recorded by the fake and refused; the
        // job carried on, and the file is still whole.
        assert!(
            s.chunks.iter().any(|(_, t)| t == "kept\n"),
            "{:?}",
            s.chunks
        );
        assert_eq!(f.file(), "lost\nkept\n");
        assert!(!cancelled.load(Ordering::SeqCst));
    }

    #[test]
    fn a_410_on_a_chunk_raises_cancellation_and_stops_the_stream() {
        let f = Fixture::new();
        // The first chunk lands; the run is cancelled before the second.
        f.cp.gone_after_chunks(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let log = f.log(
            LogConfig {
                flush: Duration::from_millis(5),
                ..LogConfig::default()
            },
            &cancelled,
        );
        log.sink().bytes(b"one\n");
        wait_until(|| !f.cp.state().chunks.is_empty());
        log.sink().bytes(b"two\n");
        wait_until(|| cancelled.load(Ordering::SeqCst));
        let sent = f.cp.state().chunks.len();
        log.sink().bytes(b"three\n");
        std::thread::sleep(Duration::from_millis(30));
        log.finish();
        assert_eq!(
            f.cp.state().chunks.len(),
            sent,
            "nothing is posted after a 410"
        );
        // …but the file is still complete, so the final upload can be.
        assert_eq!(f.file(), "one\ntwo\nthree\n");
    }

    #[test]
    fn a_410_on_the_retry_of_a_chunk_raises_cancellation_too() {
        // The first attempt fails with something retryable and the retry
        // is what discovers the cancellation — the arm that a scripted
        // straight 410 never reaches.
        let f = Fixture::new();
        f.cp.script_other(vec![Reply::status(500), Reply::status(410)]);
        let cancelled = Arc::new(AtomicBool::new(false));
        let log = f.log(
            LogConfig {
                flush: Duration::from_millis(5),
                ..LogConfig::default()
            },
            &cancelled,
        );
        log.sink().bytes(b"one\n");
        wait_until(|| cancelled.load(Ordering::SeqCst));
        log.finish();
        assert_eq!(f.file(), "one\n");
    }

    #[test]
    fn a_410_on_the_heartbeat_raises_cancellation_too() {
        let f = Fixture::new();
        f.cp.script_other(vec![Reply::status(410)]);
        let cancelled = Arc::new(AtomicBool::new(false));
        let log = f.log(
            LogConfig {
                flush: Duration::from_millis(2),
                heartbeat: Duration::from_millis(4),
                ..LogConfig::default()
            },
            &cancelled,
        );
        wait_until(|| cancelled.load(Ordering::SeqCst));
        let leases = f.cp.state().leases;
        std::thread::sleep(Duration::from_millis(30));
        log.finish();
        assert_eq!(f.cp.state().leases, leases, "no heartbeat after a 410");
    }

    #[test]
    fn a_log_path_that_cannot_be_created_is_reported_rather_than_panicked() {
        let cp = FakeCp::start();
        let client = Arc::new(Client::new(&cp.base_url(), "job1", "tok", tuning()));
        let err = Log::start(
            client,
            Path::new("/nonexistent-weft-runner/job.log"),
            LogConfig::default(),
            Arc::new(AtomicBool::new(false)),
        )
        // `map(|_| ())` because `Log` deliberately has no `Debug`: it owns
        // a `Client`, which owns the job token.
        .map(|_| ())
        .expect_err("must refuse");
        assert!(err.starts_with("cannot open /nonexistent-weft-runner/job.log:"));
        assert!(format!("{:?}", LogConfig::default()).contains("max_chunk"));
    }

    /// Spin until `f` holds, or fail the test after two seconds. Waiting on
    /// the observable rather than sleeping a guessed interval is what keeps
    /// these deterministic on a loaded machine.
    pub(crate) fn wait_until(mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("condition never held within 2s");
    }
}
