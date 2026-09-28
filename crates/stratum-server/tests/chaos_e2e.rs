//! Chaos end-to-end: what survives a node dying mid-protocol, and what
//! survives a store that is lying to several clients at once.
//!
//! Every other suite kills the server politely or not at all. These tests
//! SIGKILL it at a *named* store operation — the manifest read, the
//! materialize, the epoch upload, either of the two pointer PUTs — and
//! then bring the same node back against the same database and the same
//! data directory and ask the questions a user would ask: is my ref still
//! there, does the clone still `fsck`, did the work finish. The kill
//! points come from `stratum-engine::compact`'s actual phase order, and
//! the sharpest of them is the window between the `locator.hdr` PUT and
//! the `manifest.json` PUT — the one place where "the manifest CAS is the
//! only commit point" (I9) is physically two steps, and where I15's
//! legal-but-notable pointer skew is manufactured on purpose.
//!
//! ## The rule this file lives under
//!
//! > **A chaos test may never be the sole cover for a product line.**
//!
//! Every test here is `#[ignore]`d, so `cargo test --workspace
//! --release` skips them and the `chaos` CI job runs them on their own
//! with `-- --ignored`. A behaviour reachable only from here is one the
//! ordinary suite never checks: give it a deterministic sibling test.
//!
//! A second reason the rule has to hold: a SIGKILLed child never writes
//! its `LLVM_PROFILE_FILE` profraw, so a killed process contributes no
//! coverage even when the run is instrumented. A line whose only witness
//! is a process we shoot is a line nobody is measuring.
//!
//! ## Seeds
//!
//! `DEFAULT_SEED` unless `STRATUM_CHAOS_SEED` overrides it, so CI is
//! reproducible and a red build names the seed that reproduces it.
//! `STRATUM_CHAOS_SEEDS=a,b,c` opts into a sweep; it is off by default,
//! because a job whose failures do not reproduce is a job people mute.
//!
//! Run them with:
//!
//! ```sh
//! cargo test -p stratum-server --test chaos_e2e --release -- --ignored
//! ```

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use stratum_store::{LatencyModel, Manifest, ObjectStore};
use stratum_testkit::browser::Browser;
use stratum_testkit::closure;
use stratum_testkit::faultproxy::{Fault, FaultHandle, FaultPlan, FaultRule};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::mailbox::Mailbox;
use stratum_testkit::{FaultProxy, Minio, Server};

/// The seed CI runs unless told otherwise. Changing it changes which
/// interleavings the `chaos` job explores, so treat it as a constant
/// worth a commit message, not a knob.
const DEFAULT_SEED: u64 = 0x5C1A_05E1;

/// Seeds this run explores. One, unless `STRATUM_CHAOS_SEEDS` asks for a
/// sweep.
fn seeds() -> Vec<u64> {
    if let Ok(list) = std::env::var("STRATUM_CHAOS_SEEDS") {
        let parsed: Vec<u64> = list
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }
    vec![std::env::var("STRATUM_CHAOS_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SEED)]
}

/// What the faults actually did, tail first — the half of a red run that
/// says *why*.
fn trace_tail(handle: &FaultHandle) -> String {
    let trace = handle.trace();
    let tail: Vec<String> = trace
        .iter()
        .rev()
        .take(25)
        .rev()
        .map(|(f, key)| format!("      {f:?} {key}"))
        .collect();
    format!(
        "\n  {} fault(s) fired; last {}:\n{}",
        trace.len(),
        tail.len(),
        tail.join("\n")
    )
}

/// The above, plus the seed to re-run with. Only the seeded tests have
/// one; a kill-point run is not a draw from a distribution.
fn context(seed: u64, handle: &FaultHandle) -> String {
    format!(
        "\n  reproduce with STRATUM_CHAOS_SEED={seed}{}",
        trace_tail(handle)
    )
}

// --------------------------------------------------------------- harness

const BIN: &str = env!("CARGO_BIN_EXE_stratum-server");

struct Rig {
    /// The bucket URL the *test* reads through — never the proxy, so an
    /// oracle can never be lied to by the faults under test.
    direct: String,
    proxy: FaultProxy,
    scratch: Scratch,
}

fn rig(hint: &str) -> Rig {
    let minio = Minio::shared();
    let bucket = minio.bucket(hint);
    let upstream = bucket
        .base_url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let bucket_name = bucket.base_url.rsplit('/').next().unwrap().to_string();
    let raw = FaultProxy::start(&upstream);
    Rig {
        direct: bucket.base_url.clone(),
        // The proxy URL carries the bucket path just like the direct one.
        proxy: FaultProxy {
            url: format!("{}/{bucket_name}", raw.url),
            handle: raw.handle,
        },
        scratch: Scratch::new(hint),
    }
}

impl Rig {
    fn store(&self) -> ObjectStore {
        ObjectStore::new(&self.direct, LatencyModel::None)
    }

    fn server(&self, hint: &str, env: &[(&str, String)]) -> Server {
        Server::builder(BIN, &self.proxy.url)
            .db_hint(hint)
            .data_dir(self.scratch.path().join("data"))
            .envs(env)
            .start()
    }
}

/// `(admin token, repo REST path, store prefix)` for a fresh repo.
fn new_repo(server: &Server, org: &str, name: &str) -> (String, String, String) {
    let admin = server.bootstrap_org(org);
    let (st, repo) = server.req(
        "POST",
        &format!("/v1/orgs/{org}/repos"),
        &admin,
        Some(serde_json::json!({ "name": name })),
    );
    assert_eq!(st, 201, "{repo}");
    let prefix = format!(
        "o/{}/r/{}/prod",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );
    (admin, format!("/v1/orgs/{org}/repos/{name}"), prefix)
}

fn commit(server: &Server, token: &str, rp: &str, i: usize) -> String {
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/commits"),
        token,
        Some(serde_json::json!({
            "message": format!("step {i}"),
            "operations": [
                { "op": "put", "path": format!("file-{}.txt", i % 3), "content": format!("v{i}\n") },
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");
    out["commit"].as_str().unwrap().to_string()
}

/// Hold the sweeper's fleet-wide lock until the returned guard drops, so
/// no GC pass can begin meanwhile. To the server this is exactly what a
/// fleet-mate mid-sweep looks like, and it skips its tick.
///
/// Compaction depends on the grace window: `publish` uploads the new
/// epoch and only then swaps the pointers at it, so for that stretch the
/// epoch is referenced by nothing, and a sweep with no grace deletes it —
/// the pointers then land naming a locator that is gone, and the next
/// write to the repository is a 500 for good. That is I8's operational
/// clause ("grace must exceed the longest compaction"), and a rig that
/// sets `STRATUM_GC_GRACE_SECS=0` to race the sweeper against something
/// *else* has to give its own folds that protection by hand.
fn hold_sweeps(server: &Server) -> stratum_control::jobs::WorkerLock {
    let node = stratum_control::ControlDb::open(&server.db_url).expect("a second session");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // `None` means a pass is running right now; they are short.
        if let Some(held) =
            stratum_control::jobs::try_lock(&node, "gc-sweep").expect("take the gc lock")
        {
            return held;
        }
        assert!(
            Instant::now() < deadline,
            "a gc sweep held its lock for 30s"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The wire-visible ref set, read the way a client reads it. Kept as an
/// ordered map so a diff in a failure message is readable.
fn remote_refs(scratch: &Scratch, url: &str) -> BTreeMap<String, String> {
    gitcli::git(scratch.path(), &["ls-remote", url])
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(oid, name)| (name.to_string(), oid.to_string()))
        .collect()
}

/// `true` when git exited zero.
///
/// `gitcli::git_expect_err` is named for its usual job — asserting that a
/// command *fails* — and so reports a zero exit as `Err`. Under chaos
/// almost every git call is one whose success is the question rather than
/// the precondition, and reading `.is_err()` as "it worked" at thirty
/// call sites is how a suite ends up asserting the opposite of what it
/// means. The inversion is spelled out once, here.
fn git_ok(cwd: &std::path::Path, args: &[&str]) -> bool {
    gitcli::git_expect_err(cwd, args).is_err()
}

fn manifest_of(store: &ObjectStore, prefix: &str) -> Manifest {
    let bytes = store
        .get(&format!("{prefix}/manifest.json"))
        .unwrap_or_else(|e| panic!("read {prefix}/manifest.json: {e}"));
    serde_json::from_slice(&bytes).expect("manifest parses")
}

/// Poll `f` until it answers true. Returns false at the deadline rather
/// than panicking, so every caller writes its own failure message with
/// the seed and the trace in it.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_millis() as i64
}

fn wait_until(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Terminal states of every `compact` job in the control plane, newest
/// last. `("running", …)` means somebody still holds a lease on it.
fn job_states(db: &mut postgres::Client, kind: &str) -> Vec<(String, Option<String>)> {
    db.query(
        "SELECT state, COALESCE(result, error) FROM jobs WHERE kind = $1 ORDER BY created_at",
        &[&kind],
    )
    .expect("read the jobs table")
    .into_iter()
    .map(|r| (r.get(0), r.get(1)))
    .collect()
}

// ------------------------------------------------------------ kill points

/// The boundaries of one compaction, in the order
/// `stratum_engine::compact::compact` crosses them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KillPoint {
    /// Before the manifest the fold is planned against has been read.
    ManifestRead,
    /// Mid-materialize: a clone of the current layout is half built.
    Materialize,
    /// Mid-`upload_dir`: part of a fresh epoch is in the store and
    /// nothing points at it. Legal (I8 allows appending into an epoch no
    /// pointer references) and pure GC fodder.
    EpochUpload,
    /// The `locator.hdr` CAS never lands: a complete, unreferenced epoch.
    LocatorCas,
    /// **The window.** `locator.hdr` has landed and `manifest.json` has
    /// not, so the two pointers name different epochs. I15 says that is
    /// legal and that a reader must not mix them — this is where a reader
    /// that does gets caught.
    ManifestCas,
    /// The manifest CAS *lands* and the worker dies before it can say so:
    /// committed, unacknowledged, and the job still holds a lease.
    JobCompletion,
}

impl KillPoint {
    const ALL: [KillPoint; 6] = [
        KillPoint::ManifestRead,
        KillPoint::Materialize,
        KillPoint::EpochUpload,
        KillPoint::LocatorCas,
        KillPoint::ManifestCas,
        KillPoint::JobCompletion,
    ];

    fn slug(self) -> &'static str {
        match self {
            KillPoint::ManifestRead => "manifest-read",
            KillPoint::Materialize => "materialize",
            KillPoint::EpochUpload => "epoch-upload",
            KillPoint::LocatorCas => "locator-cas",
            KillPoint::ManifestCas => "manifest-cas",
            KillPoint::JobCompletion => "job-completion",
        }
    }

    /// Does this request line cross the boundary?
    fn matches(self, method: &str, key: &str, prefix: &str) -> bool {
        let manifest = format!("{prefix}/manifest.json");
        let hdr = format!("{prefix}/locator.hdr");
        let under_epoch = key.contains(&format!("{prefix}/"))
            && !key.ends_with(&manifest)
            && !key.ends_with(&hdr);
        match self {
            KillPoint::ManifestRead => method == "GET" && key.ends_with(&manifest),
            KillPoint::Materialize => method == "GET" && under_epoch,
            KillPoint::EpochUpload => method == "PUT" && under_epoch,
            KillPoint::LocatorCas => method == "PUT" && key.ends_with(&hdr),
            KillPoint::ManifestCas | KillPoint::JobCompletion => {
                method == "PUT" && key.ends_with(&manifest)
            }
        }
    }

    /// Must the killed-at request be stopped from reaching the store?
    ///
    /// A crash cannot un-send bytes already on the wire, so for the two
    /// pointer CASes the proxy has to refuse the request the dying
    /// process just issued — otherwise `LocatorCas` silently becomes
    /// `ManifestCas` and `ManifestCas` becomes `JobCompletion`, and three
    /// kill points collapse into one. Both are conditional PUTs, which is
    /// exactly what `Fault::CasReject` answers without relaying.
    ///
    /// The other four need no blocker: reads have no effect, and a
    /// half-written epoch nobody points at is the state under test.
    fn blocker(self, prefix: &str) -> Option<FaultPlan> {
        let key = match self {
            KillPoint::LocatorCas => format!("{prefix}/locator.hdr"),
            KillPoint::ManifestCas => format!("{prefix}/manifest.json"),
            _ => return None,
        };
        Some(
            FaultPlan::new(0).with(
                FaultRule::new(Fault::CasReject, 1.0)
                    .only_keys([key])
                    .only_methods(["PUT"]),
            ),
        )
    }
}

/// Arm a one-shot SIGKILL at `point`, and report the flag that says
/// whether it fired.
fn arm_kill(
    handle: &FaultHandle,
    point: KillPoint,
    prefix: &str,
    pid: u32,
    hits: Arc<Mutex<Vec<String>>>,
) -> Arc<AtomicBool> {
    let fired = Arc::new(AtomicBool::new(false));
    let flag = fired.clone();
    let prefix = prefix.to_string();
    let plan = point.blocker(&prefix);
    let h = handle.clone();
    handle.observe(move |line, _seq| {
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_ascii_uppercase();
        let target = parts.next().unwrap_or_default();
        let key = target.split('?').next().unwrap_or(target);
        if !point.matches(&method, key, &prefix) {
            return;
        }
        // One kill per arming. `clear_observer` is *not* called here: the
        // proxy holds the observer lock across this callback, so clearing
        // from inside it would deadlock the request we are killing at.
        if flag.swap(true, Ordering::SeqCst) {
            return;
        }
        hits.lock().unwrap().push(format!("{method} {key}"));
        Server::kill_pid(pid);
        // Installed *after* the kill and read by the proxy *after* this
        // callback returns, so it governs this very request.
        if let Some(plan) = plan.clone() {
            h.set_plan(plan);
        }
    });
    fired
}

// ------------------------------------------------------- the kill points

/// The headline test: crash the node at each compaction boundary in turn
/// and prove four things every time — the store is still closed under its
/// own pointers, the job converges instead of wedging, no ref is lost,
/// and a point read through the locator plane still resolves.
///
/// That last one is not decoration. At `ManifestCas` the two pointers
/// legally name different epochs, and a reader that derives a data key
/// from the manifest's epoch and an offset from the locator's (I15) reads
/// garbage or 404s exactly there and nowhere else.
#[test]
#[ignore = "chaos: SIGKILLs the server; run with --ignored (see the module docs)"]
fn a_crash_at_any_compaction_boundary_resumes_without_losing_a_ref() {
    for point in KillPoint::ALL {
        crash_at(point);
    }
}

fn crash_at(point: KillPoint) {
    let hint = format!("chaos-{}", point.slug());
    let rig = rig(&hint);
    let store = rig.store();
    // The poller is off while we seed: nothing may fold before the kill
    // point is armed, or the boundary under test is crossed by a server
    // nobody is watching.
    let mut server = rig.server(
        &hint,
        &[
            ("STRATUM_COMPACT_POLL_SECS", "0".into()),
            ("STRATUM_COMPACT_LEASE_SECS", "3".into()),
            ("STRATUM_GC_SECS", "0".into()),
        ],
    );
    let (admin, rp, prefix) = new_repo(&server, "acme", "app");

    // Seed past the fold thresholds (8 WAL entries), then give the repo a
    // ref set worth losing: a branch and a tag as well as the head.
    let mut tips = Vec::new();
    for i in 0..10 {
        tips.push(commit(&server, &admin, &rp, i));
    }
    let (st, _) = server.req(
        "POST",
        &format!("{rp}/branches"),
        &admin,
        Some(serde_json::json!({ "name": "release", "from": tips[3] })),
    );
    assert_eq!(st, 201);
    let (st, _) = server.req(
        "POST",
        &format!("{rp}/tags"),
        &admin,
        Some(serde_json::json!({ "name": "v1", "target": tips[5] })),
    );
    assert_eq!(st, 201);

    // Pre-kill truth, recorded through a healthy store.
    let url = server.authed_url(&admin, "acme", "app");
    let refs_before = remote_refs(&rig.scratch, &url);
    let tip_before = gitcli::clone_and_fsck(&url, &rig.scratch.path().join("before"));
    assert!(
        refs_before.len() >= 3,
        "{point:?}: the ref set under test is too thin to prove anything: {refs_before:?}"
    );
    let (st, file_before) = server.get(&format!("{rp}/files/file-0.txt"), &admin);
    assert_eq!(st, 200, "{file_before}");
    let manifest_before = manifest_of(&store, &prefix);
    assert!(
        manifest_before.wal.len() >= 8,
        "{point:?}: seeding did not cross the fold threshold ({} entries)",
        manifest_before.wal.len()
    );
    let keys_before = store.list(&format!("{prefix}/")).unwrap().len();

    // The rendezvous. The kill has to be armed against the pid that will
    // actually do the folding, and that pid does not exist until the node
    // with the poller enabled is up — by which time the compactor's first
    // `claim` has already happened, because its loop claims before it
    // sleeps. Arming against the *old* pid and re-arming afterwards loses
    // the race silently: the first observer eats the one fold there was
    // and kills a process that is already gone, and the test then waits
    // ninety seconds for a second fold that is never due.
    //
    // So the queued fold is held down in the control plane — a lease an
    // hour out is exactly what another node holding it looks like — and
    // released only once the kill point is armed. No sleep, no guess.
    let mut db = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
    let held_until = now_ms() + 3_600_000;
    let held = db
        .execute(
            "UPDATE jobs SET state = 'running', lease_until = $1 WHERE kind = 'compact'",
            &[&held_until],
        )
        .expect("hold the queued fold");
    assert_eq!(held, 1, "{point:?}: seeding did not queue exactly one fold");

    server.restart_with(&[("STRATUM_COMPACT_POLL_SECS", "1".into())]);
    let hits = Arc::new(Mutex::new(Vec::new()));
    let fired = arm_kill(
        &rig.proxy.handle,
        point,
        &prefix,
        server.pid(),
        hits.clone(),
    );
    let zero: i64 = 0;
    db.execute(
        "UPDATE jobs SET state = 'queued', lease_until = $1, attempts = $1 WHERE kind = 'compact'",
        &[&zero],
    )
    .expect("release the fold");

    assert!(
        wait_until(Duration::from_secs(90), || fired.load(Ordering::SeqCst)),
        "{point:?}: the kill point never fired — the fold never reached it{}",
        trace_tail(&rig.proxy.handle)
    );
    assert!(
        server.wait_for_exit(Duration::from_secs(30)),
        "{point:?}: SIGKILL did not take the server down"
    );
    // Whatever the kill point installed to stop that one request is gone
    // now; from here the store is honest again.
    rig.proxy.handle.clear_observer();
    rig.proxy.handle.heal();

    // (1) No dangling pointer, at any kill point. A partial epoch nobody
    // points at is legal, and so is a manifest/locator epoch skew — the
    // oracle reports the skew as an observation rather than a violation,
    // and so do we.
    let report = closure::assert_closed(&rig.direct);
    assert!(report.repos >= 1, "{point:?}: the oracle checked nothing");
    if point == KillPoint::ManifestCas {
        // The window this whole file exists for. If the two pointers do
        // *not* disagree here, the blocker failed and this run silently
        // became a `JobCompletion` run.
        assert!(
            report
                .observations
                .iter()
                .any(|o| o.contains("locator.hdr is on epoch") && o.contains("(I15)")),
            "{point:?}: locator.hdr and manifest.json agree, so the skew \
             window was never entered: {:?}",
            report.observations
        );
    }

    // Come back with the poller *off*, so the crash state is still on
    // disk while the reader questions are asked. Asking them after the
    // node has re-folded would prove only that a healthy repo reads
    // correctly, which is what every other suite already proves.
    server.restart_with(&[("STRATUM_COMPACT_POLL_SECS", "0".into())]);
    let url = server.authed_url(&admin, "acme", "app");

    // (2) Not one ref lost, and the clone still passes the I11 gate —
    // against the half-finished state itself.
    assert_eq!(
        remote_refs(&rig.scratch, &url),
        refs_before,
        "{point:?}: the ref set changed across a crash"
    );
    assert_eq!(
        gitcli::clone_and_fsck(&url, &rig.scratch.path().join("crashed")),
        tip_before,
        "{point:?}: the head tip moved across a crash"
    );

    // (3) A point read through the locator plane still resolves. This is
    // the I15 assertion, and `ManifestCas` is where it bites: a reader
    // that took the data key's epoch from one pointer and the offset from
    // the other reads garbage or 404s exactly in this window.
    let (st, file_crashed) = server.get(&format!("{rp}/files/file-0.txt"), &admin);
    assert_eq!(
        st, 200,
        "{point:?}: a point read through the locator plane broke: {file_crashed}"
    );
    assert_eq!(
        file_crashed, file_before,
        "{point:?}: a point read answered differently after a crash"
    );

    // (4) And the work converges once a worker is running again — done or
    // failed, never a job wedged behind a lease nobody will ever release.
    // The lease is 3s and the poll 1s, so a node that resumes at all
    // resumes well inside this bound; the bound only decides how long a
    // genuine wedge takes to report.
    server.restart_with(&[("STRATUM_COMPACT_POLL_SECS", "1".into())]);
    let url = server.authed_url(&admin, "acme", "app");
    // Keep the states that actually satisfied the predicate. Re-reading
    // them afterwards is a race the test loses: a compaction enqueued
    // between the wait's last poll and the second read is legitimately
    // `queued`, and asserting on *that* failed with "a crashed fold must
    // be finishable" while nothing was wrong — seen once on a full
    // `ci-local.sh`, and reproducible only under that load. Waiting on a
    // thing and then asking again is not the same as waiting on it.
    let mut settled_states: Vec<(String, Option<String>)> = Vec::new();
    let settled = wait_until(Duration::from_secs(120), || {
        let states = job_states(&mut db, "compact");
        let converged = !states.is_empty()
            && states.iter().all(|(s, _)| s == "done" || s == "failed")
            && manifest_of(&store, &prefix).wal.is_empty();
        if converged {
            settled_states = states;
        }
        converged
    });
    assert!(
        settled,
        "{point:?}: the fold never converged — jobs {:?}, WAL still {} entries{}",
        job_states(&mut db, "compact"),
        manifest_of(&store, &prefix).wal.len(),
        trace_tail(&rig.proxy.handle)
    );
    assert!(
        settled_states.iter().all(|(s, _)| s == "done"),
        "{point:?}: a crashed fold must be finishable, not dead-lettered: {settled_states:?}"
    );
    closure::assert_closed(&rig.direct);

    // Everything above, again, on the far side of the recovery.
    assert_eq!(
        remote_refs(&rig.scratch, &url),
        refs_before,
        "{point:?}: the ref set changed across the recovery"
    );
    assert_eq!(
        gitcli::clone_and_fsck(&url, &rig.scratch.path().join("recovered")),
        tip_before,
        "{point:?}: the head tip moved across the recovery"
    );
    let (st, file_after) = server.get(&format!("{rp}/files/file-0.txt"), &admin);
    assert_eq!(
        st, 200,
        "{point:?}: point read after recovery: {file_after}"
    );
    assert_eq!(file_after, file_before, "{point:?}: point read changed");

    // And the crash really did leave debris in the store rather than
    // being a no-op the assertions above would pass without noticing.
    let keys_after = store.list(&format!("{prefix}/")).unwrap().len();
    assert!(
        keys_after > keys_before,
        "{point:?}: nothing was ever written — is the kill point matching \
         the wrong request? hits: {:?}",
        hits.lock().unwrap()
    );
    assert!(server.healthy(), "{point:?}: the server is not serving");
}

/// I10, executably: a push writes its WAL segment before the manifest
/// that names it. Kill in exactly that gap and the segment is an orphan —
/// present in the store, referenced by nothing, and therefore invisible
/// to every reader. The push must look like it never happened, and the
/// next one must land.
#[test]
#[ignore = "chaos: SIGKILLs the server; run with --ignored (see the module docs)"]
fn a_crash_between_the_wal_segment_and_the_manifest_leaves_an_invisible_orphan() {
    let rig = rig("chaos-wal-orphan");
    let store = rig.store();
    let mut server = rig.server(
        "chaos-wal",
        &[
            ("STRATUM_COMPACT_POLL_SECS", "0".into()),
            ("STRATUM_GC_SECS", "0".into()),
        ],
    );
    let (admin, rp, prefix) = new_repo(&server, "acme", "app");
    for i in 0..3 {
        commit(&server, &admin, &rp, i);
    }

    let url = server.authed_url(&admin, "acme", "app");
    let clone = rig.scratch.path().join("work");
    let tip_before = gitcli::clone_and_fsck(&url, &clone);
    let refs_before = remote_refs(&rig.scratch, &url);
    let keys_before = store.list(&format!("{prefix}/")).unwrap().len();

    // Kill the moment the manifest CAS is issued, and refuse that CAS:
    // the WAL segment for this push has landed, its manifest never will.
    let hits = Arc::new(Mutex::new(Vec::new()));
    let fired = arm_kill(
        &rig.proxy.handle,
        KillPoint::ManifestCas,
        &prefix,
        server.pid(),
        hits.clone(),
    );

    std::fs::write(clone.join("orphan.txt"), "never acknowledged\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "the push that dies"]);
    let doomed = gitcli::git(&clone, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    // The push cannot succeed: the server it is talking to is about to
    // die mid-request. `git` reports that however it likes; the store is
    // the oracle, not the exit code.
    assert!(
        !git_ok(&clone, &["push", "-q", "origin", "main"]),
        "the push survived a server that died mid-CAS"
    );

    assert!(
        fired.load(Ordering::SeqCst),
        "the manifest CAS was never reached, so nothing was under test"
    );
    assert!(
        server.wait_for_exit(Duration::from_secs(30)),
        "SIGKILL did not take the server down"
    );
    rig.proxy.handle.clear_observer();
    rig.proxy.handle.heal();

    // The orphan is really there — data landed before the pointer (I7) —
    // and the closure oracle still passes, because nothing points at it.
    let keys_after = store.list(&format!("{prefix}/")).unwrap().len();
    assert!(
        keys_after > keys_before,
        "no WAL object landed before the manifest, so I10 was never exercised"
    );
    closure::assert_closed(&rig.direct);

    server.restart();
    let url = server.authed_url(&admin, "acme", "app");
    assert_eq!(
        remote_refs(&rig.scratch, &url),
        refs_before,
        "an unacknowledged push became visible"
    );
    let tip_after = gitcli::clone_and_fsck(&url, &rig.scratch.path().join("after"));
    assert_eq!(tip_after, tip_before, "an orphan WAL entry moved the tip");
    assert_ne!(tip_after, doomed, "the dead push is visible to readers");

    // …and the repo is not poisoned: the same push, retried, lands.
    gitcli::git(&clone, &["remote", "set-url", "origin", &url]);
    gitcli::git(&clone, &["push", "-q", "origin", "main"]);
    let (st, refs) = server.get(&format!("{rp}/refs"), &admin);
    assert_eq!(st, 200, "{refs}");
    assert_eq!(
        remote_refs(&rig.scratch, &url)["refs/heads/main"],
        doomed,
        "the retried push did not land"
    );
    gitcli::clone_and_fsck(&url, &rig.scratch.path().join("retried"));
    closure::assert_closed(&rig.direct);
    assert!(server.healthy());
}

/// A sweep is not a transaction, and it does not need to be: it is
/// idempotent by construction. Kill one mid-DELETE and the next sweep has
/// to finish the job rather than leave half a reclaimed epoch behind
/// forever.
#[test]
#[ignore = "chaos: SIGKILLs the server; run with --ignored (see the module docs)"]
fn a_crash_mid_gc_sweep_is_finished_by_the_next_sweep() {
    let rig = rig("chaos-gc");
    let store = rig.store();
    let mut server = rig.server(
        "chaos-gc",
        &[
            ("STRATUM_COMPACT_POLL_SECS", "0".into()),
            ("STRATUM_GC_SECS", "1".into()),
            ("STRATUM_GC_GRACE_SECS", "0".into()),
        ],
    );
    let (admin, rp, prefix) = new_repo(&server, "acme", "app");
    for i in 0..10 {
        commit(&server, &admin, &rp, i);
    }

    let url = server.authed_url(&admin, "acme", "app");
    let refs_before = remote_refs(&rig.scratch, &url);
    let tip_before = gitcli::clone_and_fsck(&url, &rig.scratch.path().join("before"));

    // The rendezvous, and it is the product's own: GC runs under a
    // fleet-wide lock, so holding that lock from this test is exactly what
    // another node sweeping looks like, and the server's sweeper waits.
    //
    // Both alternatives are races. Enabling GC with a restart means
    // arming against a pid that does not exist until the sweeper's first
    // pass has already run (`interval` ticks immediately), and the loser
    // watches a whole epoch get reclaimed cleanly. Arming before the fold
    // instead lets a sweep land inside the ~30 ms between the compaction
    // CAS and `POST /compact`'s response, killing the server underneath
    // the request and failing the test on the transport error.
    let other_node = stratum_control::ControlDb::open(&server.db_url).expect("a second session");
    let held = stratum_control::jobs::try_lock(&other_node, "gc-sweep")
        .expect("take the gc lock")
        .expect("no sweep can be running: nothing is collectable yet");

    let fired = Arc::new(AtomicBool::new(false));
    let flag = fired.clone();
    let watched = prefix.clone();
    let pid = server.pid();
    rig.proxy.handle.observe(move |line, _| {
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_ascii_uppercase();
        let target = parts.next().unwrap_or_default();
        if method != "DELETE" || !target.contains(&watched) {
            return;
        }
        if flag.swap(true, Ordering::SeqCst) {
            return;
        }
        Server::kill_pid(pid);
    });

    // Fold synchronously, so exactly one epoch is now unreferenced. No
    // sweep can touch it while the lock is held.
    let (st, out) = server.post(&format!("{rp}/compact"), &admin, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["outcome"], "Compacted", "{out}");

    let live_epoch = manifest_of(&store, &prefix).epoch;
    let stale_keys = |store: &ObjectStore| -> usize {
        store
            .list(&format!("{prefix}/"))
            .unwrap()
            .into_iter()
            .filter(|(k, _)| {
                !k.ends_with("manifest.json")
                    && !k.ends_with("locator.hdr")
                    && !k.contains(&format!("{prefix}/{live_epoch}/"))
            })
            .count()
    };
    assert!(
        stale_keys(&store) > 0,
        "the fold left nothing for GC to reclaim, so nothing is under test"
    );

    // Kill armed, epoch collectable, lock released: the next sweep dies
    // partway through it.
    drop(held);
    assert!(
        wait_until(Duration::from_secs(90), || fired.load(Ordering::SeqCst)),
        "the sweep never issued a DELETE under {prefix}"
    );
    assert!(server.wait_for_exit(Duration::from_secs(30)));
    rig.proxy.handle.clear_observer();
    rig.proxy.handle.heal();
    closure::assert_closed(&rig.direct);

    // The next sweep finishes what the dead one started.
    server.restart();
    assert!(
        wait_until(Duration::from_secs(120), || stale_keys(&store) == 0),
        "a half-finished sweep was never finished: {} stale key(s) left",
        stale_keys(&store)
    );
    closure::assert_closed(&rig.direct);
    let url = server.authed_url(&admin, "acme", "app");
    assert_eq!(remote_refs(&rig.scratch, &url), refs_before);
    assert_eq!(
        gitcli::clone_and_fsck(&url, &rig.scratch.path().join("after")),
        tip_before,
        "GC reclaimed something a reader still needed"
    );
    assert!(server.healthy());
}

// ------------------------------------------------------ safety, then liveness

/// **Safety.** Several writers and readers against a store that is
/// lying — hangs, latency, CAS rejections, and mutations that land and
/// then report failure — all at once. The contract is not "every push
/// succeeds"; under this much injury some must fail. The contract is that
/// *every push the server acknowledged is still there afterwards*, that
/// the clone passes `fsck --full --strict`, and that the store is closed
/// under its own pointers.
#[test]
#[ignore = "chaos: fault storm over concurrent clients; run with --ignored"]
fn every_acknowledged_write_survives_a_fault_storm() {
    for seed in seeds() {
        storm(seed);
    }
}

fn storm(seed: u64) {
    let rig = rig("chaos-storm");
    let server = rig.server(
        "chaos-storm",
        &[
            ("STRATUM_COMPACT_POLL_SECS", "1".into()),
            ("STRATUM_COMPACT_LEASE_SECS", "5".into()),
            ("STRATUM_GC_SECS", "0".into()),
        ],
    );
    let (admin, rp, prefix) = new_repo(&server, "acme", "app");
    commit(&server, &admin, &rp, 0);
    let url = server.authed_url(&admin, "acme", "app");
    let base = server.base.clone();

    // Committers race pushers race cloners. Each records only what the
    // server *acknowledged*; a refusal under injury is a legal outcome.
    rig.proxy.handle.set_plan(FaultPlan::chaos(seed, 0.04));
    let acked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut threads = Vec::new();
    for w in 0..3usize {
        let (acked, token, rp, base) = (acked.clone(), admin.clone(), rp.clone(), base.clone());
        threads.push(std::thread::spawn(move || {
            for i in 0..6 {
                let body = serde_json::json!({
                    "message": format!("committer {w} step {i}"),
                    "operations": [ { "op": "put",
                        "path": format!("w{w}/f{i}.txt"), "content": format!("{w}-{i}\n") } ],
                });
                let resp = ureq::post(&format!("{base}{rp}/commits"))
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Content-Type", "application/json")
                    .timeout(Duration::from_secs(30))
                    .send_string(&body.to_string());
                if let Ok(r) = resp {
                    if r.status() == 201 {
                        let text = r.into_string().unwrap_or_default();
                        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                        if let Some(c) = v["commit"].as_str() {
                            acked.lock().unwrap().push(c.to_string());
                        }
                    }
                }
            }
        }));
    }
    for p in 0..2usize {
        let (acked, url, dir) = (
            acked.clone(),
            url.clone(),
            rig.scratch.path().join(format!("pusher-{p}")),
        );
        threads.push(std::thread::spawn(move || {
            // A clone under injury may itself fail; that is a legal
            // outcome for a reader, so it is not an assertion.
            let parent = dir.parent().unwrap().to_path_buf();
            if !git_ok(&parent, &["clone", "-q", &url, dir.to_str().unwrap()]) {
                return;
            }
            for i in 0..4 {
                let f = dir.join(format!("p{p}-{i}.txt"));
                if std::fs::write(&f, format!("{p}-{i}\n")).is_err() {
                    return;
                }
                gitcli::git(&dir, &["add", "-A"]);
                gitcli::git(&dir, &["commit", "-q", "-m", &format!("push {p}-{i}")]);
                let oid = gitcli::git(&dir, &["rev-parse", "HEAD"]).trim().to_string();
                if git_ok(&dir, &["push", "-q", "origin", "main"]) {
                    // Acknowledged: the server said the ref moved.
                    acked.lock().unwrap().push(oid);
                } else {
                    // Refused: re-sync onto whatever truth won and carry on.
                    let _ = git_ok(&dir, &["fetch", "-q", "origin"]);
                    if !git_ok(&dir, &["reset", "-q", "--hard", "origin/main"]) {
                        return;
                    }
                }
            }
        }));
    }
    for c in 0..2usize {
        let (url, dir) = (url.clone(), rig.scratch.path().join(format!("cloner-{c}")));
        threads.push(std::thread::spawn(move || {
            for i in 0..3 {
                let dest = dir.join(format!("c{i}"));
                std::fs::create_dir_all(&dir).ok();
                if git_ok(&dir, &["clone", "-q", &url, dest.to_str().unwrap()]) {
                    // A clone that *completes* under injury still has to
                    // be a correct clone. I11 has no injury exemption.
                    gitcli::fsck(&dest);
                }
            }
        }));
    }
    for t in threads {
        t.join().expect("a chaos worker panicked");
    }

    // Convalescence: measure recovery, not the tail of the injury.
    rig.proxy.handle.heal();
    let store = rig.store();
    assert!(
        wait_until(Duration::from_secs(60), || server.healthy()),
        "the server never came back after the storm{}",
        context(seed, &rig.proxy.handle)
    );

    let stats = rig.proxy.handle.stats();
    assert!(
        stats.total() > 0,
        "the storm injected nothing at seed {seed}: the test proved nothing"
    );

    let acked = acked.lock().unwrap().clone();
    assert!(
        acked.len() >= 6,
        "too few acknowledged writes at seed {seed} ({}) to be a safety test{}",
        acked.len(),
        context(seed, &rig.proxy.handle)
    );

    let final_clone = rig.scratch.path().join("verdict");
    gitcli::clone_and_fsck(&url, &final_clone);
    for oid in &acked {
        assert!(
            git_ok(
                &final_clone,
                &["cat-file", "-e", &format!("{oid}^{{commit}}")]
            ),
            "acknowledged commit {oid} is not in the store{}",
            context(seed, &rig.proxy.handle)
        );
    }
    let report = closure::assert_closed(&rig.direct);
    assert!(report.keys_checked > 0, "the oracle checked nothing");
    assert!(!manifest_of(&store, &prefix).refs.is_empty());
}

/// **Liveness.** walgit heals a core and freezes the rest; we have one
/// process, so the equivalent is *key scoping* — three repos each frozen
/// a different way (reads black-holed, reads stale forever, writes
/// CAS-rejected) while a fourth is untouched. The healthy repo must keep
/// working on a wall clock: a push lands, a clone `fsck`s, a fold
/// completes. A design that serialises everything behind one sick prefix
/// fails here and nowhere else.
#[test]
#[ignore = "chaos: key-scoped freezes; run with --ignored"]
fn one_frozen_prefix_does_not_stop_a_healthy_one() {
    let rig = rig("chaos-liveness");
    let store = rig.store();
    let server = rig.server(
        "chaos-live",
        &[
            ("STRATUM_COMPACT_POLL_SECS", "1".into()),
            ("STRATUM_COMPACT_LEASE_SECS", "5".into()),
            ("STRATUM_GC_SECS", "0".into()),
        ],
    );
    let admin = server.bootstrap_org("acme");
    let mut prefixes = BTreeMap::new();
    for name in ["frozen-read", "frozen-stale", "frozen-cas", "healthy"] {
        let (st, repo) = server.req(
            "POST",
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name })),
        );
        assert_eq!(st, 201, "{repo}");
        let prefix = format!(
            "o/{}/r/{}/",
            repo["org_id"].as_str().unwrap(),
            repo["id"].as_str().unwrap()
        );
        let rp = format!("/v1/orgs/acme/repos/{name}");
        for i in 0..2 {
            commit(&server, &admin, &rp, i);
        }
        prefixes.insert(name, prefix);
    }

    // Freeze three prefixes, three different ways. Nothing is scoped to
    // the fourth, and nothing is unscoped.
    rig.proxy.handle.set_plan(
        FaultPlan::new(DEFAULT_SEED)
            .with(
                FaultRule::new(Fault::ReadDenied, 1.0)
                    .only_keys([prefixes["frozen-read"].clone()])
                    .only_methods(["GET", "HEAD"]),
            )
            .with(
                FaultRule::new(Fault::Stale, 1.0)
                    .only_keys([prefixes["frozen-stale"].clone()])
                    .only_methods(["GET"]),
            )
            .with(
                FaultRule::new(Fault::CasReject, 1.0)
                    .only_keys([prefixes["frozen-cas"].clone()])
                    .only_methods(["PUT"]),
            )
            .stale_fresh_etag(true),
    );

    let started = Instant::now();
    let rp = "/v1/orgs/acme/repos/healthy";
    let url = server.authed_url(&admin, "acme", "healthy");
    let clone = rig.scratch.path().join("healthy");
    gitcli::clone_and_fsck(&url, &clone);
    std::fs::write(clone.join("live.txt"), "still serving\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "liveness"]);
    gitcli::git(&clone, &["push", "-q", "origin", "main"]);
    let pushed = gitcli::git(&clone, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // Enough writes to make the healthy repo's fold due, then demand it.
    for i in 2..12 {
        commit(&server, &admin, rp, i);
    }
    let healthy_prefix = format!("{}prod", prefixes["healthy"]);
    assert!(
        wait_until(Duration::from_secs(120), || manifest_of(
            &store,
            &healthy_prefix
        )
        .wal
        .is_empty()),
        "the healthy repo never compacted while three others were frozen{}",
        context(DEFAULT_SEED, &rig.proxy.handle)
    );

    let after = rig.scratch.path().join("healthy-after");
    gitcli::clone_and_fsck(&url, &after);
    let (st, refs) = server.get(&format!("{rp}/refs"), &admin);
    assert_eq!(st, 200, "{refs}");
    assert!(
        git_ok(&after, &["cat-file", "-e", &format!("{pushed}^{{commit}}")]),
        "the push that landed under three frozen prefixes is gone"
    );
    assert!(
        started.elapsed() < Duration::from_secs(240),
        "the healthy prefix took {:?} — a frozen prefix is holding it up",
        started.elapsed()
    );

    // Healing the frozen prefixes must bring them back, not leave them
    // wedged: a liveness test that ends with three dead repos has proved
    // isolation and nothing about recovery.
    rig.proxy.handle.heal();
    for name in ["frozen-read", "frozen-stale", "frozen-cas"] {
        let rp = format!("/v1/orgs/acme/repos/{name}");
        let (st, out) = server.req(
            "POST",
            &format!("{rp}/commits"),
            &admin,
            Some(serde_json::json!({
                "message": "after the thaw",
                "operations": [ { "op": "put", "path": "thaw.txt", "content": "ok\n" } ],
            })),
        );
        assert_eq!(st, 201, "{name} never recovered: {out}");
        let url = server.authed_url(&admin, "acme", name);
        gitcli::clone_and_fsck(&url, &rig.scratch.path().join(format!("thawed-{name}")));
    }
    closure::assert_closed(&rig.direct);
    assert!(server.healthy());
}

/// Sign somebody up, verify them, and hand back a **personal** token.
///
/// Forking takes a person — `caller_person` refuses a repository-scoped
/// token because it has no namespace of its own — and only signup mints
/// a personal namespace, so `bootstrap_org` cannot stand in here the way
/// it does for the rest of this file.
///
/// A token rather than a session because this test restarts the node,
/// and a `Browser` borrows the `Server` it was made from.
fn person(server: &Server, mail: &Mailbox, handle: &str) -> String {
    let email = format!("{handle}@example.com");
    let (st, body) = server.req(
        "POST",
        "/v1/auth/signup",
        "",
        Some(serde_json::json!({
            "handle": handle,
            "email": email,
            "name": handle,
            "password": "a long enough password",
        })),
    );
    assert_eq!(st, 202, "signup {handle}: {body}");
    let msg = mail.wait_for(&email, Duration::from_secs(10));
    let link = msg.link().unwrap_or_else(|| panic!("no link in {msg:?}"));
    let verify = link
        .split_once("#verify=")
        .unwrap_or_else(|| panic!("{link} carries no #verify="))
        .1
        .to_string();
    let mut b = Browser::new(server);
    let (st, body) = b.req(
        "POST",
        "/v1/auth/verify",
        Some(serde_json::json!({ "token": verify })),
    );
    assert_eq!(st, 200, "verify {handle}: {body}");
    // No `repo` scope: bound to a repository it would be an automation
    // rather than a person, and forking would correctly refuse it.
    let (st, minted) = b.req(
        "POST",
        &format!("/v1/orgs/{handle}/tokens"),
        Some(serde_json::json!({ "scopes": ["repo:read", "repo:write"], "label": "cli" })),
    );
    assert_eq!(st, 201, "mint {handle} token: {minted}");
    minted["token"].as_str().expect("token").to_string()
}

/// A signed-in session for `handle`, as its cookie. A fork reads one
/// organisation and writes another, and a token is bound to one, so it
/// is the person's session that forks.
fn session(server: &Server, handle: &str) -> String {
    Browser::signed_in(
        server,
        &format!("{handle}@example.com"),
        "a long enough password",
    )
    .cookie
    .expect("a session cookie")
}

/// A request as the session `cookie` names.
fn as_session(
    server: &Server,
    cookie: &str,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let mut b = Browser::new(server);
    b.cookie = Some(cookie.to_string());
    b.req(method, path, body)
}

/// `owner` invites `guest` in as a viewer. Every repository is private
/// to its organisation, so this is the only way `guest` can read — and
/// so fork — one of `owner`'s.
fn let_in(server: &Server, owner: &str, guest: &str) {
    let mut b = Browser::signed_in(
        server,
        &format!("{owner}@example.com"),
        "a long enough password",
    );
    b.invite_and_accept(owner, &format!("{guest}@example.com"), "viewer");
}

/// A fork is two writes in two systems that share no transaction: an
/// `epoch_refs` row in Postgres, then a `locator.hdr` in the object
/// store. The order is the correctness argument — reference first,
/// pointer second — because a crash in between must leave storage
/// pinned slightly too long rather than a live fork reading data nothing
/// is protecting.
///
/// This kills the node in exactly that window and asks the questions the
/// rest of this file asks: does the work finish when the node comes
/// back, and does the clone still `fsck`.
///
/// The *other* half of the argument — that the reference actually keeps
/// upstream's epoch alive across a compaction — is held deterministically
/// by `gc_forks::a_fork_keeps_upstreams_old_epoch_alive_across_a_compaction`,
/// because a chaos test may never be the sole cover for a product line.
#[test]
#[ignore = "chaos: SIGKILLs the server; run with --ignored (see the module docs)"]
fn a_crash_between_the_epoch_reference_and_the_fork_pointer_resumes() {
    let rig = rig("chaos-fork");
    let store = rig.store();
    let mail = Mailbox::temp("chaos-fork");
    let mut env: Vec<(&str, String)> = vec![
        ("STRATUM_COMPACT_POLL_SECS", "0".into()),
        ("STRATUM_GC_SECS", "0".into()),
        ("STRATUM_FORK_POLL_SECS", "1".into()),
        // A crashed job holds its claim until the lease expires, so this
        // is how long the fork stays stuck after the node dies. Short
        // here; the production default is five minutes.
        ("STRATUM_FORK_LEASE_SECS", "3".into()),
    ];
    for (k, v) in mail.env() {
        env.push((k, v.to_string()));
    }
    let mut server = rig.server("chaos-fork", &env);

    let ada = person(&server, &mail, "ada");
    let bob = person(&server, &mail, "bob");
    let (st, body) = server.req(
        "POST",
        "/v1/orgs/ada/repos",
        &ada,
        Some(serde_json::json!({ "name": "widget" })),
    );
    assert_eq!(st, 201, "{body}");
    let_in(&server, "ada", "bob");
    let bob_session = session(&server, "bob");
    let up_prefix = format!(
        "o/{}/r/{}/prod",
        body["org_id"].as_str().unwrap(),
        body["id"].as_str().unwrap()
    );

    // Real content, and a locator to fork. The plane is compaction's
    // output, so without a fold there is no pointer for the fork to
    // write and no window to die in — and compaction only folds once the
    // WAL is deep enough, which is why this is nine commits and not one.
    for i in 0..9 {
        commit(&server, &ada, "/v1/orgs/ada/repos/widget", i);
    }
    let (st, out) = server.req("POST", "/v1/orgs/ada/repos/widget/compact", &ada, None);
    assert_eq!(st, 200, "compact: {out}");
    // The precondition, stated rather than assumed: a run where the fold
    // declined would arm a kill that never fires and prove nothing.
    assert_eq!(out["outcome"], "Compacted", "the fold declined: {out}");
    let direct = ObjectStore::new(&rig.direct, LatencyModel::None);
    assert!(
        direct.get(&format!("{up_prefix}/locator.hdr")).is_ok(),
        "compaction left upstream with no locator"
    );

    // Arm on the fork's locator write. Upstream's is already in place
    // and compaction is off, so the next `PUT …/locator.hdr` in this
    // bucket is the fork's and nothing else.
    let hits = Arc::new(Mutex::new(Vec::new()));
    let fired = Arc::new(AtomicBool::new(false));
    let flag = fired.clone();
    let seen = hits.clone();
    let pid = server.pid();
    rig.proxy.handle.observe(move |line, _seq| {
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_ascii_uppercase();
        let target = parts.next().unwrap_or_default();
        let key = target.split('?').next().unwrap_or(target);
        if method != "PUT" || !key.ends_with("/locator.hdr") {
            return;
        }
        if flag.swap(true, Ordering::SeqCst) {
            return;
        }
        seen.lock().unwrap().push(format!("{method} {key}"));
        Server::kill_pid(pid);
    });

    let (st, body) = as_session(
        &server,
        &bob_session,
        "POST",
        "/v1/orgs/ada/repos/widget/forks",
        None,
    );
    assert_eq!(st, 202, "{body}");
    let fork_prefix = format!(
        "o/{}/r/{}/prod",
        body["org_id"].as_str().unwrap(),
        body["id"].as_str().unwrap()
    );

    // Wait for the node to die in the window.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !fired.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        fired.load(Ordering::SeqCst),
        "the fork's locator write was never reached, so nothing was under test"
    );
    rig.proxy.handle.clear_observer();

    // The fork is half-made and, critically, is not serving anything
    // half-made: the manifest is the only ref truth (I9) and it never
    // landed, so the fork has no refs rather than wrong ones.
    assert!(
        direct.get(&format!("{fork_prefix}/manifest.json")).is_err(),
        "a fork published a manifest before its pointer survived a crash"
    );

    // Same node, same database, same data directory.
    server.restart();

    // The job is retried and finishes. Every step of the fork worker is
    // idempotent precisely so that this is all it takes.
    let mut ready = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let (st, body) = server.req("GET", "/v1/orgs/bob/repos/widget", &bob, None);
        if st == 200 && body["fork_state"] == "ready" {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(ready, "the fork never completed after the node came back");

    // And the question a user would ask, answered against upstream
    // rather than against a number hard-coded here: the fork must serve
    // exactly the history it forked, whatever that turns out to be.
    let up_clone = rig.scratch.path().join("upstream-clone");
    let up_tip = gitcli::clone_and_fsck(&format!("{}/ada/widget.git", server.base), &up_clone);
    let clone = rig.scratch.path().join("fork-clone");
    let fork_tip = gitcli::clone_and_fsck(&format!("{}/bob/widget.git", server.base), &clone);
    assert_eq!(
        fork_tip, up_tip,
        "the resumed fork does not serve upstream's tip"
    );
    assert_eq!(
        gitcli::git(&clone, &["log", "--oneline"]).lines().count(),
        gitcli::git(&up_clone, &["log", "--oneline"])
            .lines()
            .count(),
        "the resumed fork lost history upstream still has"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
    let _ = store;
}

/// **I8's unverified half, for forks.** `reference/invariants.md` says an
/// epoch directory dies only when no live pointer references it *and* its
/// newest object is older than a grace window that must exceed the
/// longest clone — so a reader that loaded an old manifest can finish
/// streaming from it — and then admits the clause is "not yet verified in
/// this repository".
///
/// Forks make it sharper than a grace window can answer. Upstream
/// compacts, its old epoch stops being referenced by anything *upstream*
/// can see, and a sweep at zero grace would take it — while a fork is
/// mid-clone out of exactly those objects. No amount of grace helps,
/// because the fork may hold that epoch for months. Only the
/// `epoch_refs` reference does.
///
/// So: park a clone of the fork inside upstream's data, compact and
/// sweep upstream underneath it at zero grace, and require both that the
/// objects survive and that the clone finishes and `fsck`s.
#[test]
#[ignore = "chaos: parks a clone mid-stream; run with --ignored (see the module docs)"]
fn upstream_may_not_sweep_an_epoch_a_fork_is_cloning_from() {
    let rig = rig("chaos-fork-i8");
    let mail = Mailbox::temp("chaos-fork-i8");
    let mut env: Vec<(&str, String)> = vec![
        ("STRATUM_COMPACT_POLL_SECS", "0".into()),
        ("STRATUM_GC_SECS", "0".into()),
        // The clone has to stream from segments for this to test
        // anything; served a prebuilt pack it would never touch
        // upstream's epoch at all.
        ("STRATUM_CDNPACK_POLL_SECS", "0".into()),
        ("STRATUM_FORK_POLL_SECS", "1".into()),
        ("STRATUM_FORK_LEASE_SECS", "3".into()),
    ];
    for (k, v) in mail.env() {
        env.push((k, v.to_string()));
    }
    let server = rig.server("chaos-fork-i8", &env);

    let ada = person(&server, &mail, "ada");
    let bob = person(&server, &mail, "bob");
    let (st, body) = server.req(
        "POST",
        "/v1/orgs/ada/repos",
        &ada,
        Some(serde_json::json!({ "name": "widget" })),
    );
    assert_eq!(st, 201, "{body}");
    let_in(&server, "ada", "bob");
    let bob_session = session(&server, "bob");
    let up_prefix = format!(
        "o/{}/r/{}/prod",
        body["org_id"].as_str().unwrap(),
        body["id"].as_str().unwrap()
    );

    for i in 0..9 {
        commit(&server, &ada, "/v1/orgs/ada/repos/widget", i);
    }
    let (st, out) = server.req("POST", "/v1/orgs/ada/repos/widget/compact", &ada, None);
    assert_eq!(st, 200, "compact: {out}");
    assert_eq!(out["outcome"], "Compacted", "the fold declined: {out}");

    // The epoch the fork is about to depend on.
    let direct = ObjectStore::new(&rig.direct, LatencyModel::None);
    let m: Manifest =
        serde_json::from_slice(&direct.get(&format!("{up_prefix}/manifest.json")).unwrap())
            .unwrap();
    let shared_epoch = m.epoch.clone();
    let shared_dir = format!("{up_prefix}/{shared_epoch}/");
    assert!(
        !direct.list(&shared_dir).unwrap().is_empty(),
        "upstream's epoch is empty; nothing to protect"
    );

    let (st, body) = as_session(
        &server,
        &bob_session,
        "POST",
        "/v1/orgs/ada/repos/widget/forks",
        None,
    );
    assert_eq!(st, 202, "{body}");
    let mut ready = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let (st, b) = server.req("GET", "/v1/orgs/bob/repos/widget", &bob, None);
        if st == 200 && b["fork_state"] == "ready" {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(ready, "the fork never became readable");

    // Move upstream on *before* parking anything: a parked responder
    // gives up after its own timeout, so the only thing that may happen
    // while the clone is held is the sweep itself.
    for i in 9..19 {
        commit(&server, &ada, "/v1/orgs/ada/repos/widget", i);
    }
    let (st, out) = server.req("POST", "/v1/orgs/ada/repos/widget/compact", &ada, None);
    assert_eq!(st, 200, "second compact: {out}");
    assert_eq!(out["outcome"], "Compacted", "{out}");
    let after: Manifest =
        serde_json::from_slice(&direct.get(&format!("{up_prefix}/manifest.json")).unwrap())
            .unwrap();
    assert_ne!(
        after.epoch, shared_epoch,
        "upstream did not move off the epoch the fork depends on"
    );

    // Park every read of upstream's shared epoch. A clone of the fork
    // resolves into exactly these keys, so it stops inside them.
    let touched = Arc::new(AtomicBool::new(false));
    let saw = touched.clone();
    let watch = shared_dir.clone();
    rig.proxy.handle.observe(move |line, _seq| {
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_ascii_uppercase();
        let target = parts.next().unwrap_or_default();
        let key = target.split('?').next().unwrap_or(target);
        if method == "GET" && key.contains(&watch) {
            saw.store(true, Ordering::SeqCst);
        }
    });
    rig.proxy.handle.set_plan(
        FaultPlan::new(DEFAULT_SEED).with(
            FaultRule::new(Fault::Hang, 1.0)
                .only_keys([shared_dir.clone()])
                .only_methods(["GET"]),
        ),
    );

    let fork_url = format!("{}/bob/widget.git", server.base);
    let clone_dir = rig.scratch.path().join("inflight-clone");
    let cloner = std::thread::spawn(move || {
        gitcli::clone_and_fsck(&fork_url, &clone_dir);
    });

    // Wait until the clone is actually inside upstream's epoch.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !touched.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        touched.load(Ordering::SeqCst),
        "the clone never reached upstream's epoch, so nothing was under test"
    );
    rig.proxy.handle.clear_observer();

    // Sweep upstream at zero grace with the clone still parked inside
    // the old epoch.
    //
    // The baseline is taken *here* rather than before the fork: pushes
    // after the first fold keep landing WAL entries in whichever epoch
    // was current, so an earlier count describes a directory that has
    // since grown, and would compare unequal for a reason that has
    // nothing to do with GC.
    let objects_before = direct.list(&shared_dir).unwrap().len();
    let (st, report) = server.req(
        "POST",
        "/v1/orgs/ada/repos/widget/gc",
        &ada,
        Some(serde_json::json!({ "grace_secs": 0 })),
    );
    assert_eq!(st, 200, "gc: {report}");

    // The crux. Upstream's own two pointers no longer name this epoch,
    // and the grace window is zero, so the only thing standing between
    // it and deletion is the fork's `epoch_refs` row.
    let objects_after = direct.list(&shared_dir).unwrap().len();
    assert_eq!(
        objects_after, objects_before,
        "upstream swept an epoch a fork is cloning from ({report})"
    );

    // Let the parked reads go, and require the clone to finish the job
    // it started — which is I8's grace clause, stated as a user question.
    rig.proxy.handle.heal();
    cloner
        .join()
        .expect("the in-flight clone failed while upstream compacted and swept beneath it");

    assert!(server.healthy(), "the server stopped serving");
}

/// **GC racing a fork's creation.** The window is narrow and the
/// consequence is not: between a fork job reading upstream's pointers
/// and its `epoch_refs` row landing, upstream's epoch is protected by
/// nothing at all. A sweep that lands inside it would take the objects
/// the fork is about to point at, and the fork would come out `ready`,
/// serving a manifest whose keys are gone.
///
/// The defence is the order — register the reference before publishing
/// the pointer — plus GC re-reading the resolver immediately before it
/// deletes, so a fork that registered while the sweep was scanning is
/// seen. This runs the two against each other and requires the fork to
/// be either fully absent or fully readable, never `ready` and hollow.
#[test]
#[ignore = "chaos: races GC against a fork; run with --ignored (see the module docs)"]
fn a_sweep_racing_a_fork_leaves_it_readable_or_absent_but_never_hollow() {
    let rig = rig("chaos-fork-race");
    let mail = Mailbox::temp("chaos-fork-race");
    let mut env: Vec<(&str, String)> = vec![
        ("STRATUM_COMPACT_POLL_SECS", "0".into()),
        // The sweeper runs continuously at the tightest interval it
        // takes, so it is genuinely racing rather than invited in.
        ("STRATUM_GC_SECS", "1".into()),
        ("STRATUM_GC_GRACE_SECS", "0".into()),
        ("STRATUM_CDNPACK_POLL_SECS", "0".into()),
        ("STRATUM_FORK_POLL_SECS", "1".into()),
        ("STRATUM_FORK_LEASE_SECS", "3".into()),
    ];
    for (k, v) in mail.env() {
        env.push((k, v.to_string()));
    }
    let server = rig.server("chaos-fork-race", &env);

    let ada = person(&server, &mail, "ada");
    let bob = person(&server, &mail, "bob");
    let (st, body) = server.req(
        "POST",
        "/v1/orgs/ada/repos",
        &ada,
        Some(serde_json::json!({ "name": "widget" })),
    );
    assert_eq!(st, 201, "{body}");
    let_in(&server, "ada", "bob");
    let bob_session = session(&server, "bob");

    for i in 0..9 {
        commit(&server, &ada, "/v1/orgs/ada/repos/widget", i);
    }
    // Every fold here runs with the sweeper held off: with no grace, a
    // sweep landing between the new epoch's upload and its pointer swap
    // deletes the epoch the pointers are about to name, and the next
    // commit fails with a 404 for a locator nobody can bring back. That
    // is a different race from the one under test (see `hold_sweeps`),
    // and it fails the run at a `commit`, not at a fork.
    let quiet = hold_sweeps(&server);
    let (st, out) = server.req("POST", "/v1/orgs/ada/repos/widget/compact", &ada, None);
    assert_eq!(st, 200, "compact: {out}");
    assert_eq!(out["outcome"], "Compacted", "{out}");
    drop(quiet);

    // Ten forks, each created while the sweeper is running with no
    // grace at all. One would be a coin toss; ten is a race the
    // scheduler has to lose consistently for this to pass by luck.
    let mut names = Vec::new();
    for n in 0..10 {
        let name = format!("fork-{n}");
        let (st, body) = as_session(
            &server,
            &bob_session,
            "POST",
            "/v1/orgs/ada/repos/widget/forks",
            Some(serde_json::json!({ "name": name })),
        );
        assert_eq!(st, 202, "fork {n}: {body}");
        names.push(name);
        // Keep upstream folding underneath them, so the epoch a fork
        // just read stops being upstream's current one while the job
        // that read it is still running.
        if n % 3 == 2 {
            for i in 0..9 {
                commit(&server, &ada, "/v1/orgs/ada/repos/widget", i);
            }
            let quiet = hold_sweeps(&server);
            let (st, out) = server.req("POST", "/v1/orgs/ada/repos/widget/compact", &ada, None);
            assert_eq!(st, 200, "fold {n}: {out}");
            drop(quiet);
        }
    }

    // Every fork must reach a terminal state, and every one that says it
    // is ready must actually serve — which is the whole claim. A hollow
    // fork is one that reports `ready` and then cannot be cloned.
    for name in &names {
        let path = format!("/v1/orgs/bob/repos/{name}");
        let mut state = String::new();
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            let (st, b) = server.req("GET", &path, &bob, None);
            assert_eq!(st, 200, "{name}: {b}");
            match b["fork_state"].as_str() {
                Some("pending") | None => std::thread::sleep(Duration::from_millis(100)),
                Some(other) => {
                    state = other.to_string();
                    break;
                }
            }
        }
        assert_eq!(state, "ready", "{name} never finished: {state:?}");

        let dest = rig.scratch.path().join(format!("clone-{name}"));
        let url = format!("{}/bob/{name}.git", server.base);
        gitcli::clone_and_fsck(&url, &dest);
        assert!(
            gitcli::git(&dest, &["log", "--oneline"]).lines().count() > 0,
            "{name} reported ready and served nothing"
        );
    }

    assert!(server.healthy(), "the server stopped serving");
}

// ------------------------------------------------ changeset landings

/// `refs/heads/main` as the store has it — the oracle, read directly.
fn trunk(store: &ObjectStore, prefix: &str) -> String {
    manifest_of(store, prefix)
        .refs
        .iter()
        .find(|(name, _)| name == "refs/heads/main")
        .map(|(_, oid)| oid.clone())
        .unwrap_or_else(|| panic!("{prefix} has no refs/heads/main"))
}

/// A user in `acme` who can approve, signed in as a browser would be.
fn acme_member<'a>(server: &'a Server, email: &str) -> Browser<'a> {
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            email,
            "--name",
            email,
            "--password",
            "a long enough password",
            "--role",
            "member",
        ])
        .unwrap_or_else(|e| panic!("user-create {email}: {e}"));
    Browser::signed_in(server, email, "a long enough password")
}

/// A repository owned (per `OWNERS`) by `owner`, with an approved open
/// change `key` one commit ahead of `main`. Returns the store prefix and
/// the patchset's commit.
fn approved_change(
    server: &Server,
    admin: &str,
    repo: &str,
    owner: &str,
    key: &str,
) -> (String, String) {
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        admin,
        Some(serde_json::json!({ "name": repo })),
    );
    assert_eq!(st, 201, "{out}");
    let prefix = format!(
        "o/{}/r/{}/prod",
        out["org_id"].as_str().unwrap(),
        out["id"].as_str().unwrap()
    );
    let rp = format!("/v1/orgs/acme/repos/{repo}");
    for (branch, message, path) in [
        ("main", format!("{owner}\n"), "OWNERS"),
        (
            "feature",
            format!("change {repo}\n\nChange-Id: {key}\n"),
            "feature.txt",
        ),
    ] {
        if branch == "feature" {
            let (st, out) = server.req(
                "POST",
                &format!("{rp}/branches"),
                admin,
                Some(serde_json::json!({ "name": "feature", "from": "main" })),
            );
            assert_eq!(st, 201, "{out}");
        }
        let (st, out) = server.req(
            "POST",
            &format!("{rp}/commits"),
            admin,
            Some(serde_json::json!({
                "branch": branch,
                "message": message,
                "operations": [{ "op": "put", "path": path, "content": message }],
            })),
        );
        assert_eq!(st, 201, "{out}");
    }
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/changes"),
        admin,
        Some(serde_json::json!({ "from": "feature" })),
    );
    assert_eq!(st, 201, "{out}");
    let commit = out["patchset"]["commit"].as_str().unwrap().to_string();
    let mut o = acme_member(server, owner);
    let (st, out) = o.req("POST", &format!("{rp}/changes/{key}/approve"), None);
    assert_eq!(st, 204, "{owner} approving {repo}/{key}: {out}");
    (prefix, commit)
}

/// **The landing protocol's SIGKILL sibling** (`docs/CHANGESETS.md`,
/// steps 3–5). A changeset over two repositories is two manifest CASes
/// with no transaction across them; the node is shot at the first and,
/// in a second round, at the second — the window between member CASes
/// where one trunk has moved and the other has not — and brought back
/// against the same database. The lease lapses, the same job is claimed
/// again, and the driver decides from the *store*: a trunk already at
/// the member's commit is done, one still at the plan's `old` is CASed
/// now, and one that is somewhere else fails the landing and reverts
/// the rest.
///
/// What is asserted is the promise, not a particular path through it:
/// every landing ends `landed` with both trunks at the patchset commits,
/// or `failed` with every landed member reverted — never one trunk moved
/// and the changeset silent about it — and every clone `fsck`s. Which
/// side the kill lands on is recorded in the failure message so a red
/// run says where it died.
///
/// Deterministic siblings in `changeset_landing_e2e.rs` cover the same
/// branches with faults instead of kills, so this file is the sole cover
/// for no line.
#[test]
#[ignore = "chaos: SIGKILLs the server; run with --ignored (see the module docs)"]
fn a_crash_between_member_cases_finishes_the_changeset_landing_either_way() {
    let rig = rig("chaos-changeset");
    let store = rig.store();
    let mut server = rig.server(
        "chaos-changeset",
        &[
            ("STRATUM_COMPACT_POLL_SECS", "0".into()),
            ("STRATUM_GC_SECS", "0".into()),
            ("STRATUM_LAND_POLL_SECS", "1".into()),
            // How long a dead driver holds the landing hostage.
            ("STRATUM_LAND_LEASE_SECS", "2".into()),
            ("STRATUM_LAND_RECHECK_SECS", "1".into()),
        ],
    );
    let admin = server.bootstrap_org("acme");

    // Round n kills at the n-th manifest PUT of the landing: at the first
    // member's CAS, then — fresh repositories, fresh changeset — at the
    // second's, with the first already landed.
    for (round, nth) in [(1usize, 1usize), (2, 2)] {
        let (api, web) = (format!("api{round}"), format!("web{round}"));
        let (ka, kw) = (format!("Iaa00000{round}"), format!("Ibb00000{round}"));
        let key = format!("Ic500000{round}");
        let (api_prefix, api_new) = approved_change(&server, &admin, &api, "oa@acme.test", &ka);
        let (web_prefix, web_new) = approved_change(&server, &admin, &web, "ow@acme.test", &kw);
        let api_old = trunk(&store, &api_prefix);
        let web_old = trunk(&store, &web_prefix);
        let (st, out) = server.req(
            "POST",
            "/v1/orgs/acme/changesets",
            &admin,
            Some(serde_json::json!({
                "key": key,
                "title": format!("round {round}"),
                "members": [
                    { "repo": web, "change": kw },
                    { "repo": api, "change": ka },
                ],
                "edges": [{
                    "from": { "repo": api, "change": ka },
                    "to": { "repo": web, "change": kw },
                }],
            })),
        );
        assert_eq!(st, 201, "{out}");

        // Arm: the n-th `PUT …/manifest.json` under either member. Setup
        // is over and the lander is the only writer left, so the count
        // starts at the landing's first CAS.
        let hits = Arc::new(Mutex::new(Vec::new()));
        let fired = Arc::new(AtomicBool::new(false));
        let (flag, seen) = (fired.clone(), hits.clone());
        let pid = server.pid();
        let prefixes = [api_prefix.clone(), web_prefix.clone()];
        let count = Arc::new(Mutex::new(0usize));
        rig.proxy.handle.observe(move |line, _seq| {
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_ascii_uppercase();
            let target = parts.next().unwrap_or_default();
            let key = target.split('?').next().unwrap_or(target);
            if method != "PUT"
                || !key.ends_with("/manifest.json")
                || !prefixes.iter().any(|p| key.contains(p.as_str()))
            {
                return;
            }
            let mut n = count.lock().unwrap();
            *n += 1;
            if *n != nth || flag.swap(true, Ordering::SeqCst) {
                return;
            }
            seen.lock().unwrap().push(format!("{method} {key}"));
            Server::kill_pid(pid);
        });

        let (st, out) = server.req(
            "POST",
            &format!("/v1/orgs/acme/changesets/{key}/land"),
            &admin,
            None,
        );
        assert_eq!(st, 202, "round {round}: {out}");
        assert!(
            wait_until(Duration::from_secs(30), || fired.load(Ordering::SeqCst)),
            "round {round}: manifest PUT #{nth} was never reached, so nothing was under test"
        );
        rig.proxy.handle.clear_observer();
        server.wait_for_exit(Duration::from_secs(10));

        // What the store says at the moment of death — the record can be
        // behind it, never ahead of it.
        let api_dead = trunk(&store, &api_prefix);
        let web_dead = trunk(&store, &web_prefix);
        assert!(
            (api_dead == api_old || api_dead == api_new)
                && (web_dead == web_old || web_dead == web_new),
            "round {round}: a trunk is at a commit nobody planned: api={api_dead} web={web_dead}"
        );
        assert!(
            web_dead == web_old || api_dead == api_new,
            "round {round}: web landed before api, against the plan's order: {:?}",
            hits.lock().unwrap()
        );

        // Same node, same database. The lease lapses, the job is claimed
        // again, and the landing is finished from what is in the store.
        server.restart();
        let mut cs = serde_json::Value::Null;
        assert!(
            wait_until(Duration::from_secs(60), || {
                let (st, body) = server.req(
                    "GET",
                    &format!("/v1/orgs/acme/changesets/{key}"),
                    &admin,
                    None,
                );
                cs = body;
                st == 200 && cs["state"] != "landing"
            }),
            "round {round}: the landing never finished after the node came back (killed at {:?}): {cs}",
            hits.lock().unwrap()
        );
        let api_now = trunk(&store, &api_prefix);
        let web_now = trunk(&store, &web_prefix);
        let ctx = format!(
            "round {round}, killed at {:?}, store at death api={api_dead} web={web_dead}: {cs}",
            hits.lock().unwrap()
        );
        match cs["state"].as_str() {
            Some("landed") => {
                assert_eq!(api_now, api_new, "{ctx}");
                assert_eq!(web_now, web_new, "{ctx}");
                assert_eq!(cs["landing"]["outcome"], "landed", "{ctx}");
                for m in cs["landing"]["members"].as_array().unwrap() {
                    assert_eq!(m["state"], "done", "{ctx}");
                }
            }
            Some("failed") => {
                // Nothing moved either trunk but us, so a failure here
                // would mean the resumed driver disbelieved its own CAS.
                // Held to the promise regardless: nothing left landed.
                for m in cs["landing"]["members"].as_array().unwrap() {
                    assert_ne!(
                        m["state"], "done",
                        "a member left landed in a failed landing: {ctx}"
                    );
                }
                assert_ne!(api_now, api_new, "{ctx}");
                assert_ne!(web_now, web_new, "{ctx}");
            }
            other => panic!("round {round}: changeset ended {other:?}: {ctx}"),
        }
        // A landed member is `landed`, a reopened one is `open`, and
        // never `landing` once the changeset has settled.
        for (repo, k) in [(&api, &ka), (&web, &kw)] {
            let (st, body) = server.req(
                "GET",
                &format!("/v1/orgs/acme/repos/{repo}/changes/{k}"),
                &admin,
                None,
            );
            assert_eq!(st, 200, "{body}");
            assert_ne!(body["change"]["state"], "landing", "{ctx}");
        }
        for repo in [&api, &web] {
            let dest = rig.scratch.path().join(format!("{repo}-clone"));
            gitcli::clone_and_fsck(&server.authed_url(&admin, "acme", repo), &dest);
        }
        assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
    }
    closure::assert_closed(&rig.direct);
}
