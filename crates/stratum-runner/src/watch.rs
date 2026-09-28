//! Watching a running step for mining software, and killing it.
//!
//! The server's parse-time refusal answers the author of a workflow that
//! names a miner. It is static — it is about what the file says — and it
//! does not see `curl -sL https://example.invalid/m -o m && ./m`. A job
//! that spends its whole six-hour budget spinning the CPU of somebody's
//! build machine costs its owner real money whether or not it ever found
//! a pool, and on a runner the owner's machine is the one at stake.
//!
//! So while a step runs, the processes in its group are sampled, and a
//! miner among them ends the job with a verdict that says so — which the
//! server records in the organisation's audit trail, so the machine's
//! owner can see what was tried on it.
//!
//! **What is matched, and what deliberately is not.** A process is a
//! miner if its *program* is one: `comm`, or the basename of `argv[0]`.
//! Never an argument. `grep -rn xmrig .`, a step that writes
//! `xmrig.log`, a README quoting this very list — all of those are
//! somebody working, and a watch that kills them is worse than no watch,
//! because the next person routes around it. The one exception is a pool
//! URL, which is matched anywhere in `argv`: a renamed binary still has
//! to be told where to send its shares, and no build has a use for
//! `stratum+tcp://`.
//!
//! **It is cheap.** One `readdir` of `/proc` every two seconds, one
//! `stat` read per process, and `cmdline` only for the processes in this
//! job's group — which is a shell and its children. Nothing is measured:
//! there is no CPU heuristic here on purpose, because a release build
//! with `-j8` looks exactly like a miner to one, and a compile flagged
//! as abuse is a person's honest build killed and written into the audit
//! trail as an attack.

#[cfg(any(target_os = "linux", test))]
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Programs whose only purpose is to mine a cryptocurrency.
///
/// The same list lives in `stratum-server`'s `workflow/parse.rs`, which
/// this crate deliberately does not link — the runner is a standalone
/// binary in a small image. `the_two_miner_lists_have_not_drifted` below
/// reads that file and fails if they disagree.
pub const MINERS: [&str; 17] = [
    "xmrig",
    "xmrigdaemon",
    "xmr-stak",
    "minerd",
    "cpuminer",
    "cpuminer-multi",
    "ethminer",
    "t-rex",
    "nbminer",
    "lolminer",
    "phoenixminer",
    "teamredminer",
    "gminer",
    "bfgminer",
    "cgminer",
    "nanominer",
    "srbminer",
];

/// The URL schemes a mining pool is spoken to over.
pub const POOL_SCHEMES: [&str; 4] = [
    "stratum+tcp://",
    "stratum+ssl://",
    "stratum2+tcp://",
    "stratum+tls://",
];

/// How often the step's process group is looked at.
pub const SAMPLE: Duration = Duration::from_secs(2);

/// How often the watcher wakes to notice it has been asked to stop. Only
/// this, not `SAMPLE`, bounds how long the end of a step waits for it.
const TICK: Duration = Duration::from_millis(25);

/// One process, reduced to the three things this decides on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub pgid: i32,
    /// The kernel's name for the program. On Linux this is truncated to
    /// 15 bytes, which is why `argv[0]` is looked at as well.
    pub comm: String,
    pub argv: Vec<String>,
}

/// Where the watch reports what it found, shared with the supervisor.
///
/// Separate from the job's `cancelled` flag on purpose: a cancellation
/// means "nobody wants this verdict", and this is the opposite — the
/// verdict is the entire point.
#[derive(Debug, Default)]
pub struct Abuse(Mutex<Option<String>>);

impl Abuse {
    pub fn found(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// First finding wins: a group with two miners in it is one abusive
    /// job, and the second name would only overwrite the report.
    fn record(&self, name: &str) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(name.to_string());
        }
    }
}

/// The miner among these processes, if there is one.
pub fn miner_in(procs: &[Proc]) -> Option<String> {
    for p in procs {
        let comm = basename(&p.comm);
        let argv0 = p.argv.first().map(|a| basename(a)).unwrap_or_default();
        if let Some(name) = [comm.clone(), argv0]
            .into_iter()
            .find(|n| MINERS.contains(&n.as_str()))
        {
            return Some(name);
        }
        if p.argv.iter().any(|a| {
            let l = a.to_ascii_lowercase();
            POOL_SCHEMES.iter().any(|s| l.contains(s))
        }) {
            return Some(comm);
        }
    }
    None
}

/// The program's own name: no directory, lower-cased, no surrounding
/// quotes. A miner invoked as `./miners/XMRig` is `xmrig`.
fn basename(s: &str) -> String {
    s.trim_matches(['"', '\'', ' '])
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Every process in `pgid`, read from a `/proc`-shaped directory.
///
/// Takes the root so that the parsing — the part with all the ways to be
/// wrong in it — is tested against fixture trees on any platform, rather
/// than against whatever happens to be running on the machine. That is
/// also why it is compiled under `test` off Linux: a development machine
/// has no `/proc` to sample, but the parsing is the half of this file
/// with all the ways to be wrong in it, and it should not go untested
/// until CI.
#[cfg(any(target_os = "linux", test))]
pub fn sample_in(root: &Path, pgid: i32) -> Vec<Proc> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        // A process that exits between the readdir and the read is the
        // ordinary case on a busy machine, not an error.
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        let Some(mut p) = parse_stat(pid, &stat) else {
            continue;
        };
        if p.pgid != pgid {
            continue;
        }
        p.argv = std::fs::read(e.path().join("cmdline"))
            .map(|b| split_nul(&b))
            .unwrap_or_default();
        out.push(p);
    }
    out
}

/// `pid (comm) state ppid pgrp …`, where `comm` may contain spaces and
/// parentheses — which is why everything is measured from the *last*
/// `)` rather than by splitting on whitespace. A process named
/// `(cargo) build` is not a hypothetical: the name is whatever the
/// program put there.
#[cfg(any(target_os = "linux", test))]
fn parse_stat(pid: i32, stat: &str) -> Option<Proc> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_string();
    let mut rest = stat.get(close + 1..)?.split_whitespace();
    let _state = rest.next()?;
    let _ppid = rest.next()?;
    let pgid = rest.next()?.parse().ok()?;
    Some(Proc {
        pid,
        pgid,
        comm,
        argv: Vec::new(),
    })
}

/// `argv` as `/proc` stores it: NUL-separated, usually with a trailing
/// NUL. An empty `cmdline` is a kernel thread and has no argv at all.
#[cfg(any(target_os = "linux", test))]
fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect()
}

/// The live sampler. On Linux — the only place a job ever runs — this is
/// `/proc`.
#[cfg(target_os = "linux")]
pub fn sample(pgid: i32) -> Vec<Proc> {
    sample_in(Path::new("/proc"), pgid)
}

/// The live sampler on everything else, which in practice means a
/// developer's macOS. It exists so that the *loop* — sample, match,
/// kill — can be run end to end on the machine the code is written on;
/// without it the whole of layer 3 would only ever be exercised in CI,
/// and a watch nobody can run is a watch nobody debugs.
#[cfg(not(target_os = "linux"))]
pub fn sample(pgid: i32) -> Vec<Proc> {
    sample_with("ps", pgid)
}

/// The `ps` above, with the program to run named rather than assumed.
///
/// Split for the one branch that cannot otherwise be reached: `ps` not
/// being there at all. It is not a hypothetical worth ignoring — the step
/// runs with a `PATH` we cleared and rebuilt, and an image without `ps`
/// would otherwise be a watch that silently sees no processes. What it
/// must not do is fail the job: an empty sample means "nothing matched",
/// the step runs to its own end, and the layers either side of this one
/// still stand.
#[cfg(not(target_os = "linux"))]
fn sample_with(ps: &str, pgid: i32) -> Vec<Proc> {
    let Ok(out) = std::process::Command::new(ps)
        .args(["-A", "-o", "pid=,pgid=,comm=,args="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_ps)
        .filter(|p| p.pgid == pgid)
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn parse_ps(line: &str) -> Option<Proc> {
    let mut f = line.split_whitespace();
    let pid = f.next()?.parse().ok()?;
    let pgid = f.next()?.parse().ok()?;
    let comm = f.next()?.to_string();
    Some(Proc {
        pid,
        pgid,
        comm,
        argv: f.map(str::to_string).collect(),
    })
}

/// A background watch over one step's process group, stopped by dropping
/// it.
pub struct Watch {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Watch {
    /// Sample `pgid` every `every` until dropped; on a match, record the
    /// name and kill the group.
    ///
    /// The kill happens here rather than back in the supervisor because
    /// the supervisor is asleep between polls and the whole point is to
    /// stop paying for the CPU. The step then ends the way any killed
    /// step does, and `Abuse` is what tells the caller why.
    pub fn start(pgid: i32, abuse: Arc<Abuse>, every: Duration) -> Watch {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let join = std::thread::spawn(move || {
            let mut due = Instant::now();
            while !flag.load(Ordering::SeqCst) {
                if Instant::now() >= due {
                    if let Some(name) = miner_in(&sample(pgid)) {
                        abuse.record(&name);
                        kill_group(pgid);
                        return;
                    }
                    due = Instant::now() + every;
                }
                std::thread::sleep(TICK);
            }
        });
        Watch {
            stop,
            join: Some(join),
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// SIGKILL the group, the same way a timeout does. See `steps::kill_group`
/// for why the pid is negated.
fn kill_group(pgid: i32) {
    // SAFETY: a plain libc call with no pointers. `pgid` is the id of a
    // group this process created for one step, so the negation cannot
    // name anything else; ESRCH from an already-dead group is the reason
    // the result is ignored.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TestDir;
    use std::os::unix::process::CommandExt;

    fn proc(comm: &str, argv: &[&str]) -> Proc {
        Proc {
            pid: 7,
            pgid: 7,
            comm: comm.to_string(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The four ways a miner shows up, and the name each is reported by.
    #[test]
    fn a_miner_is_recognised_by_its_program_or_by_the_pool_it_talks_to() {
        // comm alone — a binary renamed on disk still says what it is.
        assert_eq!(miner_in(&[proc("xmrig", &[])]), Some("xmrig".into()));
        // argv[0], which is not truncated the way comm is.
        assert_eq!(
            miner_in(&[proc("teamredminer", &["/opt/TeamRedMiner", "-a", "x"])]),
            Some("teamredminer".into())
        );
        assert_eq!(
            miner_in(&[proc("m", &["./miners/PhoenixMiner"])]),
            Some("phoenixminer".into())
        );
        // Renamed, but it still has to say where the shares go.
        assert_eq!(
            miner_in(&[proc(
                "helper",
                &["./helper", "-o", "STRATUM+TCP://p.invalid:3333"]
            )]),
            Some("helper".into())
        );
        // The first finding in the group wins, and the rest of the group
        // does not have to be miners.
        assert_eq!(
            miner_in(&[
                proc("bash", &["bash", "-e", "-c", "./x"]),
                proc("cgminer", &["./cgminer"]),
            ]),
            Some("cgminer".into())
        );
    }

    /// The half that matters more. Every one of these mentions a miner,
    /// and killing any of them would be a person's build destroyed by a
    /// substring.
    #[test]
    fn a_step_that_only_mentions_a_miner_is_not_one() {
        for p in [
            proc("grep", &["grep", "-rn", "xmrig", "."]),
            proc("cat", &["cat", "/tmp/xmrig.log"]),
            proc(
                "bash",
                &["bash", "-e", "-c", "echo 'no xmrig here' > notes"],
            ),
            proc("cc1plus", &["/usr/lib/gcc/cc1plus", "-O2", "miner.cpp"]),
            proc("cargo", &["cargo", "build", "--release", "-j8"]),
            proc("curl", &["curl", "-sS", "https://pool.example/index.html"]),
            proc("", &[]),
        ] {
            assert_eq!(miner_in(std::slice::from_ref(&p)), None, "{p:?}");
        }
    }

    /// `/proc` is not a directory anyone can hand a unit test, so the
    /// parsing is pointed at a fixture tree instead — including the
    /// shapes that would otherwise be found in production: a `comm` with
    /// spaces and brackets in it, a pid that exits mid-sample, and the
    /// entries that are not pids at all.
    #[test]
    fn a_proc_tree_is_read_for_this_group_only() {
        let dir = TestDir::new("procfs");
        let root = dir.path();
        let write = |pid: &str, stat: &str, cmdline: &[u8]| {
            let d = root.join(pid);
            std::fs::create_dir_all(&d).expect("mkdir");
            std::fs::write(d.join("stat"), stat).expect("stat");
            if !cmdline.is_empty() {
                std::fs::write(d.join("cmdline"), cmdline).expect("cmdline");
            }
        };
        write(
            "11",
            "11 (bash) S 1 11 11 0 -1 4194304 0",
            b"bash\0-e\0-c\0./x\0",
        );
        // A program that put spaces and a bracket in its own name.
        write(
            "12",
            "12 (xmrig (worker)) R 11 11 11 0 -1 0 0",
            b"./xmrig\0-o\0p.invalid\0",
        );
        // Another job's group entirely: never read, never matched.
        write("13", "13 (xmrig) R 1 13 13 0 -1 0 0", b"./xmrig\0");
        // A kernel thread has an empty cmdline; nothing here may panic.
        write("14", "14 (kworker/0:1) S 2 11 11 0 -1 0 0", b"");
        // Not a process directory at all, and a pid whose stat is gone.
        std::fs::create_dir_all(root.join("sys")).expect("mkdir");
        std::fs::create_dir_all(root.join("15")).expect("mkdir");
        // A pid whose `stat` is there and is not a stat line. Read
        // partially while the kernel was writing it, or not a `/proc` at
        // all — either way it is one unreadable process, not a reason to
        // stop sampling and leave a miner running. It is named `xmrig` so
        // that a sampler which fell back to the directory name, or which
        // gave up on the whole tree here, would be caught by the count.
        write("16", "16 (xmrig", b"./xmrig\0");

        let mut got = sample_in(root, 11);
        got.sort_by_key(|p| p.pid);
        assert_eq!(
            got.len(),
            3,
            "the unparseable one is skipped, not fatal: {got:?}"
        );
        assert_eq!(got[0].comm, "bash");
        assert_eq!(got[0].argv, ["bash", "-e", "-c", "./x"]);
        assert_eq!(got[1].comm, "xmrig (worker)");
        assert_eq!(got[2].argv, Vec::<String>::new());
        assert_eq!(miner_in(&got), Some("xmrig".into()));
        // …and the group with nothing in it reads back empty rather than
        // as an error.
        assert_eq!(sample_in(root, 99), Vec::new());
        assert_eq!(sample_in(&root.join("nowhere"), 11), Vec::new());
    }

    /// A `stat` line that is not one. Every arm returns "not a process"
    /// rather than a panic, because this reads a file another program
    /// writes.
    #[test]
    fn a_stat_line_that_makes_no_sense_is_skipped() {
        for bad in [
            "",
            "11 bash S 1 11",
            "11 (bash",
            "11 (bash) S 1",
            "11 (bash) S 1 notanumber",
        ] {
            assert_eq!(parse_stat(11, bad), None, "{bad:?}");
        }
    }

    /// A sampler that cannot run `ps` reports an empty tree rather than
    /// failing the job. Only reachable by naming a program that is not
    /// there, which is why `sample` is split from `sample_with`.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_sampler_with_no_ps_to_run_sees_nothing_rather_than_failing() {
        let missing = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/no-such-ps-1a2b3c"
        );
        assert!(!std::path::Path::new(missing).exists());
        assert_eq!(sample_with(missing, 1), Vec::new());
        // …and with a real one it still reads its own group, so the split
        // did not quietly break the sampler it was split out of.
        let me = unsafe { libc::getpgrp() };
        assert!(
            sample_with("ps", me)
                .iter()
                .any(|p| p.pid == std::process::id() as i32),
            "the test's own process is in its own group"
        );
    }

    /// The list this crate refuses on and the list the parser refuses on
    /// are the same list, in two crates that do not link each other. A
    /// name added to one and not the other is a miner refused at parse
    /// time and ignored at runtime, or the reverse — which is exactly the
    /// gap a defence in depth is supposed not to have.
    #[test]
    fn the_two_miner_lists_have_not_drifted() {
        let parse = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../stratum-server/src/workflow/parse.rs"
        );
        let src = std::fs::read_to_string(parse).expect("the parser is where it was");
        assert_eq!(names_in(&src, "MINERS"), MINERS, "the miner lists differ");
        assert_eq!(
            names_in(&src, "POOL_SCHEMES"),
            POOL_SCHEMES,
            "the pool schemes differ"
        );
    }

    /// The quoted strings of one `const NAME: [&str; N] = [ … ];`.
    fn names_in(src: &str, name: &str) -> Vec<String> {
        let start = src
            .find(&format!("const {name}: "))
            .unwrap_or_else(|| panic!("no `{name}` in the parser"));
        let body = &src[start..];
        let end = body.find("];").expect("the list ends");
        body[..end]
            .split('"')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect()
    }

    /// The whole loop, against a process that is a miner as far as
    /// anything here can tell: `/bin/sleep` under the name `xmrig`. No
    /// mining software comes near this repository — the point of the
    /// watch is the *name*, and a renamed sleep exercises every step of
    /// it: the sample, the match, the kill, and the report.
    ///
    /// A *symlink* rather than a copy, and the difference matters on a
    /// development machine: macOS SIGKILLs a copied system binary for an
    /// invalid code signature the moment it execs, so the copy would be a
    /// zombie by the first sample and the test would pass on the wrong
    /// mechanism. Both `comm` and `argv[0]` follow the name the program
    /// was started under, which is the whole of what is matched.
    #[test]
    fn a_watched_group_running_a_miner_is_killed_and_named() {
        let dir = TestDir::new("watch-kill");
        let miner = dir.path().join("xmrig");
        std::os::unix::fs::symlink("/bin/sleep", &miner).expect("a sleep to rename");
        let mut child = std::process::Command::new(&miner)
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn the miner");
        let pgid = child.id() as i32;
        let abuse = Arc::new(Abuse::default());
        let watch = Watch::start(pgid, Arc::clone(&abuse), Duration::from_millis(20));

        let deadline = Instant::now() + Duration::from_secs(10);
        while abuse.found().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(watch);
        assert_eq!(abuse.found(), Some("xmrig".into()));
        // Killed, not merely reported: the CPU stops.
        let status = child.wait().expect("reap");
        assert!(!status.success(), "{status:?}");
        assert!(Instant::now() < deadline, "it waited out the sleep");
    }

    /// The other side of the same loop: an ordinary step is watched for
    /// its whole life and never touched.
    #[test]
    fn a_watched_group_that_is_only_working_is_left_alone() {
        let abuse = Arc::new(Abuse::default());
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("1")
            .process_group(0)
            .spawn()
            .expect("spawn");
        let watch = Watch::start(
            child.id() as i32,
            Arc::clone(&abuse),
            Duration::from_millis(20),
        );
        let status = child.wait().expect("reap");
        drop(watch);
        assert!(status.success(), "the watch killed an innocent step");
        assert_eq!(abuse.found(), None);
    }

    /// Two findings in one group report the first, so the log and the
    /// verdict say the same thing.
    #[test]
    fn the_first_finding_is_the_one_reported() {
        let a = Abuse::default();
        a.record("xmrig");
        a.record("cgminer");
        assert_eq!(a.found(), Some("xmrig".into()));
    }
}
