//! What a push is allowed to *cost*, as opposed to what it answers.
//!
//! The connectivity walk once ran `git cat-file --batch` as a fresh
//! process per object: a fork, an exec, git's startup and an
//! object-database open, to answer one question. It was perfectly
//! correct — it returned the right bytes — and no assertion about
//! answers could ever have caught it. What it did instead was make a
//! push's cost track its **object count**: ~20 ms each on the fleet, so
//! 0.2 MiB across 1,942 objects took three times as long as 6 MiB in a
//! single blob, and an ordinary source repository — object-heavy by
//! nature — outlived the gateway's 60 s limit and died with `HTTP 504`,
//! no ref created, 16 attempts out of 16. GitHub took the same push in
//! about four seconds.
//!
//! `receive.rs` now starts one reader for the whole walk, and the unit
//! tests beside it pin *that reader's* contract. They cannot pin the
//! thing that actually broke, which is **where the reader is started**:
//! move `CatFile::start` back inside the loop and every one of them
//! still passes.
//!
//! So this drives a real push through `receive()` in this process, where
//! the counter is visible, and asserts the whole walk costs **one**
//! process however many objects it visits. That is the regression, and
//! this is the only place it is caught.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use stratum_proto::receive::{cat_file_spawns, receive};
use stratum_store::{LatencyModel, ObjectStore, PutCond};
use stratum_testkit::gitcli::Scratch;
use stratum_testkit::minio::{ROOT_PASSWORD, ROOT_USER};
use stratum_testkit::Minio;

fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn pkt(line: &str) -> Vec<u8> {
    let mut v = format!("{:04x}", line.len() + 4).into_bytes();
    v.extend_from_slice(line.as_bytes());
    v
}

/// A repository whose history is deliberately object-heavy and
/// byte-light — the shape that used to fail, and the shape a real source
/// repository has.
fn many_object_repo(dir: &std::path::Path, files: usize, commits: usize) -> String {
    std::fs::create_dir_all(dir).unwrap();
    git_in(dir, &["init", "-q", "-b", "main", "."]);
    for c in 0..commits {
        let sub = dir.join(format!("d{c}"));
        std::fs::create_dir_all(&sub).unwrap();
        for f in 0..files {
            std::fs::write(sub.join(format!("f{f}.txt")), format!("c{c} f{f}\n")).unwrap();
        }
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-qm", &format!("c{c}")]);
    }
    git_in(dir, &["rev-parse", "HEAD"])
}

fn pack_of(dir: &std::path::Path, revs: &str) -> Vec<u8> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        // No deltas: a delta base that is not in the pack is a *thin*
        // base, and an empty layout has no locator to resolve one
        // against. The walk is what this test measures, not delta
        // resolution.
        .args([
            "pack-objects",
            "--revs",
            "--stdout",
            "-q",
            "--window=0",
            "--depth=0",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(revs.as_bytes())
        .unwrap();
    let mut buf = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut buf).unwrap();
    assert!(child.wait().unwrap().success());
    buf
}

/// The push a client sends: one command, a flush, then the pack.
fn push_body(old: &str, new: &str, name: &str, pack: &[u8]) -> Vec<u8> {
    let mut body = pkt(&format!(
        "{old} {new} {name}\0report-status ofs-delta agent=test\n"
    ));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(pack);
    body
}

/// An empty layout: the fields without serde defaults, and nothing else.
fn empty_layout(store: &ObjectStore, prefix: &str) {
    let manifest = serde_json::json!({
        "schema": 1,
        "repo": "app",
        "layout": "L1",
        "refs": [],
        "head": "refs/heads/main",
        // Every data key lives under an epoch; a layout without one is
        // refused as predating them.
        "epoch": "e1",
    })
    .to_string();
    store
        .put(
            &format!("{prefix}/manifest.json"),
            manifest.as_bytes(),
            PutCond::None,
        )
        .expect("seed manifest");
}

#[test]
fn a_push_costs_one_cat_file_however_many_objects_it_carries() {
    let minio = Minio::shared();
    let bucket = minio.bucket("proto-push-cost");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let _ = (ROOT_USER, ROOT_PASSWORD);

    let scratch = Scratch::new("push-cost");
    let repo = scratch.path().join("src");
    // 40 files x 12 commits: several hundred objects, a few kilobytes.
    // Under the old code this walk alone was hundreds of processes.
    let tip = many_object_repo(&repo, 40, 12);
    let objects = git_in(&repo, &["rev-list", "--objects", "HEAD"])
        .lines()
        .count();
    assert!(
        objects > 300,
        "fixture is not object-heavy enough to be a regression test: {objects}"
    );

    let prefix = "o/acme/r/app/L1";
    empty_layout(&store, prefix);

    let pack = pack_of(&repo, &format!("{tip}\n"));
    let body = push_body(
        "0000000000000000000000000000000000000000",
        &tip,
        "refs/heads/main",
        &pack,
    );

    let before = cat_file_spawns();
    let mut out = Vec::new();
    let accepted = receive(&store, prefix, &body, &[], None, &mut out).expect("push accepted");
    let spawned = cat_file_spawns() - before;

    assert!(accepted.is_some(), "push was refused: {out:?}");
    let report = String::from_utf8_lossy(&out);
    assert!(report.contains("unpack ok"), "{report}");

    assert_eq!(
        spawned, 1,
        "a push of {objects} objects started {spawned} cat-file processes. \
         The walk must start one reader and ask it every question: a process \
         per object is ~20ms each on the fleet, which is how an ordinary \
         repository's push came to exceed the gateway's timeout and answer 504."
    );
}
