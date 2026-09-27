//! The fleet: several stateless nodes over one bucket and one control
//! plane, which is how this runs in production and how nothing was
//! tested until now.
//!
//! `README.md` claims any stateless node serves any repo. Every
//! invariant that makes that true is about the object store rather than
//! the process — I7 (data first, pointer last), I8 (epoch data is
//! immutable once referenced), I9 (the manifest is the only commit point
//! and it moves only by CAS), I15 (a reader takes its data epoch from
//! `locator.hdr` and never from the manifest). The one existing
//! two-server test asserts a *database* fence, which proves the control
//! plane is shared and says nothing about any of these.
//!
//! So: four nodes racing each other into the same CAS, an acknowledged
//! push demanded from every other node with no eventual-consistency
//! slack, and — the one `reference/invariants.md` names as missing — a
//! clone parked mid-stream while a different node swaps the epoch out
//! from under it.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use stratum_testkit::fleet::Fleet;
use stratum_testkit::{gitcli, GateProxy, Minio};

const BIN: &str = env!("CARGO_BIN_EXE_stratum-server");

/// Compaction folds at eight WAL entries (`CompactionThresholds`), so
/// nine is "past the threshold" with one to spare.
const PAST_THRESHOLD: usize = 9;

fn fleet(bucket: &str, hint: &str, n: usize) -> Fleet {
    Fleet::builder(BIN, bucket).hint(hint).nodes(n).start()
}

/// One commit through the REST write path, on whichever node is asked.
fn commit(f: &Fleet, node: usize, repo: &str, path: &str, body: &str) -> String {
    let (st, out) = f.node(node).post(
        &format!("{}/commits", f.repo_path(repo)),
        &f.admin,
        Some(serde_json::json!({
            "message": format!("write {path}"),
            "operations": [{ "op": "put", "path": path, "content": body }],
        })),
    );
    assert_eq!(st, 201, "commit {path} on node {node}: {out}");
    out["commit"].as_str().expect("commit oid").to_string()
}

/// Fold the WAL on `node`, insisting it actually folded — a `NotNeeded`
/// here would leave the epoch unchanged and quietly turn an epoch-swap
/// test into a test of nothing.
fn compact(f: &Fleet, node: usize, repo: &str) {
    let (st, out) = f
        .node(node)
        .post(&format!("{}/compact", f.repo_path(repo)), &f.admin, None);
    assert_eq!(st, 200, "compact on node {node}: {out}");
    assert_eq!(out["outcome"], "Compacted", "compact on node {node}: {out}");
}

/// The epoch segment of a data key: `…/<prefix>/<epoch>/<name>`. Pointer
/// objects (`manifest.json`, `locator.hdr`) sit directly under the prefix
/// and have no epoch, so they answer `None`.
fn epoch_of(key: &str, prefix: &str) -> Option<String> {
    let (_, rest) = key.split_once(&format!("/{prefix}/"))?;
    let (epoch, tail) = rest.split_once('/')?;
    (!tail.is_empty()).then(|| epoch.to_string())
}

/// Clone `repo` from node `i` into a fresh directory under the fleet
/// scratch, run the I11 gate, and hand back the checkout.
fn clone_from(f: &Fleet, i: usize, repo: &str, name: &str) -> std::path::PathBuf {
    let dest = f.scratch().join(name);
    f.clone_and_fsck_from(i, repo, &dest);
    dest
}

/// Every node's answer to the same push, at once.
///
/// Four nodes, four branches, one bucket. Nothing coordinates them except
/// the CAS on `manifest.json`, so if the commit point were anything less
/// than a compare-and-swap — a read-modify-write, a last-writer-wins PUT
/// — this is where three of the four branches would vanish. The refs
/// converging is only half of it: the clone has to be *usable* from every
/// node afterwards, which is why each one is cloned and fsck'd rather
/// than just listed.
#[test]
fn concurrent_pushes_from_k_nodes_all_land_and_every_node_agrees() {
    let minio = Minio::shared();
    let bucket = minio.bucket("fleet-concurrent");
    let f = fleet(&bucket.base_url, "fleet-concurrent", 4);
    let prefix = f.create_repo("app");
    let base = commit(&f, 0, "app", "README.md", "base\n");

    // One clone per node, all rooted at the same base commit, each with
    // its own branch to push.
    let work: Vec<std::path::PathBuf> = (0..f.len())
        .map(|i| clone_from(&f, i, "app", &format!("work-{i}")))
        .collect();
    for (i, w) in work.iter().enumerate() {
        gitcli::git(w, &["checkout", "-q", "-b", &format!("b{i}")]);
        std::fs::write(w.join(format!("f{i}.txt")), format!("from node {i}\n")).unwrap();
        gitcli::git(w, &["add", "-A"]);
        gitcli::git(w, &["commit", "-q", "-m", &format!("branch b{i}")]);
    }

    let tips: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = work
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let url = f.url(i, "app");
                s.spawn(move || {
                    gitcli::git(w, &["push", "-q", &url, &format!("b{i}")]);
                    gitcli::git(w, &["rev-parse", "HEAD"]).trim().to_string()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    f.assert_refs_converged("app");
    let refs = f.refs_from(0, "app");
    for (i, tip) in tips.iter().enumerate() {
        assert_eq!(
            refs.get(&format!("refs/heads/b{i}")),
            Some(tip),
            "branch b{i} did not land: {refs:?}"
        );
    }
    assert_eq!(refs.get("refs/heads/main"), Some(&base), "{refs:?}");

    // Usable from every node, not merely listed by every node.
    for i in 0..f.len() {
        let c = clone_from(&f, i, "app", &format!("verify-{i}"));
        for tip in &tips {
            gitcli::git(&c, &["cat-file", "-e", tip]);
        }
    }

    // I5's push-side shape: each accepted push writes its own
    // content-addressed WAL object, and the manifest lists each once. A
    // repeated key would mean one push's entry was recorded twice, which
    // `index-pack` would later reject as a duplicate object.
    let wal = f.manifest(&prefix).wal;
    let unique: BTreeSet<&String> = wal.iter().map(|w| &w.key).collect();
    assert_eq!(unique.len(), wal.len(), "duplicate WAL keys: {wal:?}");
    for n in f.nodes() {
        assert!(n.healthy());
    }
}

/// Did the server fall over, or did git refuse the push on purpose?
///
/// This is a function rather than two `contains` calls at the assertion
/// because the obvious spelling of it is wrong, and was: `stderr`
/// carries the push URL, the URL carries the port, and the port is
/// whatever the OS handed out — so `stderr.contains("500")` fired on
/// `http://127.0.0.1:62500/acme/app.git` and reported an ordinary CAS
/// loss as an internal error. It passed under `cargo test` and failed
/// under `cargo llvm-cov` in the same run of the same commit, for no
/// better reason than a different ephemeral port, which is the most
/// expensive kind of red: it looks like the fleet is broken.
///
/// That is the third time this repository has been bitten by matching a
/// bare number in prose — the `401` in a scratch path, and the repo
/// named `rfc-403` reported to users as private. Match git's own
/// phrasing instead: it prints `HTTP 500` when a request fails with a
/// status, and the URL is never spelled that way.
fn is_server_error(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("internal") || lower.contains("http 500")
}

#[test]
fn a_port_number_is_not_a_status_code() {
    // The exact stderr that failed the fleet suite, port and all.
    let honest = "To http://127.0.0.1:62500/acme/app.git\n                   ! [remote rejected] main -> main                   (refs/heads/main: stale old value, fetch first)\n";
    assert!(!is_server_error(honest));
    // ...and a real one still is.
    assert!(is_server_error(
        "error: RPC failed; HTTP 500 curl 22 The requested URL returned error: 500"
    ));
    assert!(is_server_error("remote: internal error"));
}

/// Four nodes, one branch, one winner per round — and the losers told the
/// truth about it.
///
/// This is the case where a weak commit point does its worst damage
/// silently. All four pushes are fast-forwards of the same base, so at
/// most one can be a fast-forward of the result; if two ever "succeed",
/// one client has been told its commit landed when it did not. The other
/// failure worth naming is a 500: a lost CAS is an ordinary, expected
/// outcome of this design, and the client is supposed to see git's own
/// non-fast-forward refusal, not an internal error.
#[test]
fn rival_pushes_to_one_branch_lose_exactly_one_per_cas_round() {
    let minio = Minio::shared();
    let bucket = minio.bucket("fleet-rivals");
    let f = fleet(&bucket.base_url, "fleet-rivals", 4);
    f.create_repo("app");
    commit(&f, 0, "app", "README.md", "base\n");

    let work: Vec<std::path::PathBuf> = (0..f.len())
        .map(|i| clone_from(&f, i, "app", &format!("rival-{i}")))
        .collect();
    for (i, w) in work.iter().enumerate() {
        // Each rival touches its own file: the race under test is for the
        // ref, and a textual conflict would only be a race between my
        // fixture and `git rebase`.
        std::fs::write(w.join(format!("rival-{i}.txt")), format!("node {i}\n")).unwrap();
        gitcli::git(w, &["add", "-A"]);
        gitcli::git(w, &["commit", "-q", "-m", &format!("rival {i}")]);
    }

    let mut pending: Vec<usize> = (0..f.len()).collect();
    let mut rounds = 0;
    while !pending.is_empty() {
        rounds += 1;
        assert!(rounds <= f.len(), "more rounds than rivals: {pending:?}");
        // `git_expect_err` inverts the sense: Ok(stderr) is a push that
        // failed — a loser — and Err is one that went through.
        let results: Vec<(usize, Option<String>)> = std::thread::scope(|s| {
            let handles: Vec<_> = pending
                .iter()
                .map(|&i| {
                    let w = &work[i];
                    let url = f.url(i, "app");
                    s.spawn(move || {
                        (
                            i,
                            gitcli::git_expect_err(w, &["push", "-q", &url, "main"]).ok(),
                        )
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let winners: Vec<usize> = results
            .iter()
            .filter(|(_, e)| e.is_none())
            .map(|(i, _)| *i)
            .collect();
        assert_eq!(
            winners.len(),
            1,
            "round {rounds}: {} pushes claimed the branch",
            winners.len()
        );
        for (i, r) in &results {
            let Some(stderr) = r else { continue };
            let lower = stderr.to_lowercase();
            assert!(
                lower.contains("non-fast-forward") || lower.contains("fetch first"),
                "node {i} lost round {rounds} with something other than an \
                 honest git refusal:\n{stderr}"
            );
            assert!(
                !is_server_error(stderr),
                "node {i} lost round {rounds} with a server error:\n{stderr}"
            );
        }

        pending.retain(|i| !winners.contains(i));
        for &i in &pending {
            let url = f.url(i, "app");
            gitcli::git(&work[i], &["fetch", "-q", &url, "main"]);
            gitcli::git(&work[i], &["rebase", "-q", "FETCH_HEAD"]);
        }
    }

    // Nothing was lost on the way: every rival's commit is in the history
    // the fleet now serves, and every node serves the same one.
    f.assert_refs_converged("app");
    let final_clone = clone_from(&f, 1, "app", "rivals-final");
    let log = gitcli::git(&final_clone, &["log", "--format=%s", "main"]);
    for i in 0..f.len() {
        assert!(
            log.lines().any(|l| l == format!("rival {i}")),
            "rival {i}'s commit is missing from the landed history:\n{log}"
        );
    }
    for n in f.nodes() {
        assert!(n.healthy());
    }
}

/// An acknowledged push is visible everywhere *now*, not soon.
///
/// S3 has been read-after-write consistent since 2020 and this design
/// leans on it completely: there is no replication lag to wait out and no
/// cache to invalidate, so a node that answered "pushed" has already made
/// the new tip readable by every other node. Giving this test any
/// tolerance — a retry loop, a poll, a sleep — would hide precisely the
/// regression it exists to catch, so it asks once and requires the
/// answer.
#[test]
fn every_ackd_push_is_visible_from_every_other_node() {
    let minio = Minio::shared();
    let bucket = minio.bucket("fleet-visible");
    let f = fleet(&bucket.base_url, "fleet-visible", 3);
    f.create_repo("app");
    commit(&f, 0, "app", "README.md", "base\n");

    let work = clone_from(&f, 0, "app", "visible-work");
    for round in 0..f.len() {
        let writer = round % f.len();
        std::fs::write(work.join("f.txt"), format!("round {round}\n")).unwrap();
        gitcli::git(&work, &["add", "-A"]);
        gitcli::git(&work, &["commit", "-q", "-m", &format!("round {round}")]);
        let url = f.url(writer, "app");
        gitcli::git(&work, &["push", "-q", &url, "main"]);
        let tip = gitcli::git(&work, &["rev-parse", "HEAD"])
            .trim()
            .to_string();

        for reader in 0..f.len() {
            let refs = f.refs_from(reader, "app");
            assert_eq!(
                refs.get("refs/heads/main"),
                Some(&tip),
                "node {writer} acknowledged {tip} but node {reader} still \
                 advertises {:?}",
                refs.get("refs/heads/main")
            );
        }
    }
    for n in f.nodes() {
        assert!(n.healthy());
    }
}

/// I8, finally: a clone parked mid-segment survives another node
/// compacting the epoch out from under it.
///
/// `reference/invariants.md` said in as many words that this test did not
/// exist here — the straddled-pointer half was covered, the
/// swap-under-an-in-flight-clone half was not, and no suite ran a reader
/// on one node against a swapper on another. That is the production
/// topology, and it is the only arrangement in which the immutability
/// half of I8 does any work: if compaction overwrote or removed the keys
/// the old manifest names, this clone would die on a 404 partway through
/// a pack.
///
/// The clone's manifest is read before anything is allowed to move, so
/// the correct answer is the *pre-swap* tip. A clone that came back with
/// the newer commits would mean the stream had been recomposed under the
/// reader, which is a different and worse bug than a 404.
#[test]
fn a_clone_in_flight_survives_an_epoch_swap_from_another_node() {
    let minio = Minio::shared();
    let bucket = minio.bucket("fleet-epoch-swap");
    let gate = GateProxy::start(minio.authority());
    // Only the reader's node goes through the gate; the swapper talks to
    // the bucket directly, so nothing but the clone can trip the stall.
    let f = Fleet::builder(BIN, &bucket.base_url)
        .hint("fleet-epoch-swap")
        .nodes(2)
        .node_store_url(0, &format!("{}/{}", gate.url, bucket.name))
        .start();
    let prefix = f.create_repo("app");

    // A folded epoch to read from: cold segments only exist after a
    // compaction, and it is a cold segment the reader is going to park
    // inside.
    for i in 0..PAST_THRESHOLD {
        commit(&f, 1, "app", "stable.txt", &format!("rev {i}\n"));
    }
    compact(&f, 1, "app");
    let epoch_before = f.manifest(&prefix).epoch;
    let tip_before = f.refs_from(1, "app")["refs/heads/main"].clone();

    gate.handle.stall_get("cold-", 64);
    let dest = f.scratch().join("swap-clone");
    let done = AtomicBool::new(false);
    let cloned = std::thread::scope(|s| {
        let clone = s.spawn(|| {
            let head = f.clone_and_fsck_from(0, "app", &dest);
            done.store(true, Ordering::SeqCst);
            head
        });
        // Deterministic rendezvous: the swap must happen while the reader
        // is provably inside the segment, not merely around the same time.
        gate.handle.wait_stalled(Duration::from_secs(60));
        for i in 0..PAST_THRESHOLD {
            commit(&f, 1, "app", "stable.txt", &format!("after {i}\n"));
        }
        compact(&f, 1, "app");
        // The guard that keeps this test from passing for the boring
        // reason. Without the stall the clone would simply finish before
        // anything moved, and every assertion below would still hold
        // while proving nothing about a swap under a reader.
        assert!(
            !done.load(Ordering::SeqCst),
            "the clone finished before the swap: nothing was in flight"
        );
        gate.handle.release();
        clone.join().unwrap()
    });

    let epoch_after = f.manifest(&prefix).epoch;
    assert_ne!(
        epoch_before, epoch_after,
        "the swap never happened, so nothing was tested"
    );
    assert_eq!(
        cloned, tip_before,
        "the in-flight clone was recomposed under the reader"
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("stable.txt")).unwrap(),
        format!("rev {}\n", PAST_THRESHOLD - 1),
        "the clone's content moved with the swap"
    );
    // And the old epoch's data is still there, which is what made the
    // clone survivable (I8: nothing under a referenced epoch is removed
    // until GC's grace sweep).
    let survivors = f
        .store()
        .list(&format!("{prefix}/{epoch_before}/"))
        .expect("list the pre-swap epoch");
    assert!(
        !survivors.is_empty(),
        "the pre-swap epoch was emptied by compaction"
    );
    for n in f.nodes() {
        assert!(n.healthy());
    }
}

/// I15: the reader's data keys come from `locator.hdr`, even when the
/// manifest it is holding names a different epoch.
///
/// The two pointers lag each other by design — compaction writes the new
/// epoch's data, then `locator.hdr`, then `manifest.json` (I7), so there
/// is always a window where they disagree. This test opens that window on
/// purpose: the point read's `manifest.json` GET is parked mid-body, so
/// the reader ends up with the pre-swap manifest and a post-swap locator.
/// Every data key it then touches must come from the locator's epoch and
/// only that one. A reader that built a segment key out of
/// `manifest.epoch` would show up here as a request straddling two
/// epochs — and would read the wrong bytes the moment the two generations
/// differ in layout.
#[test]
fn a_reader_never_mixes_the_locator_epoch_with_the_manifest_epoch() {
    let minio = Minio::shared();
    let bucket = minio.bucket("fleet-epoch-mix");
    let gate = GateProxy::start(minio.authority());
    let f = Fleet::builder(BIN, &bucket.base_url)
        .hint("fleet-epoch-mix")
        .nodes(2)
        .node_store_url(0, &format!("{}/{}", gate.url, bucket.name))
        .start();
    let prefix = f.create_repo("app");

    for i in 0..PAST_THRESHOLD {
        commit(&f, 1, "app", "stable.txt", &format!("rev {i}\n"));
    }
    compact(&f, 1, "app");
    let epoch_before = f.manifest(&prefix).epoch;
    // A later push, so the manifest the reader parks on has a live WAL in
    // the old epoch as well as segments there.
    commit(&f, 1, "app", "other.txt", "later\n");
    let want = format!("rev {}\n", PAST_THRESHOLD - 1);

    gate.handle.clear();
    gate.handle.stall_get("manifest.json", 1);
    let path = format!("{}/files/stable.txt?at=main", f.repo_path("app"));
    let done = AtomicBool::new(false);
    let (status, body) = std::thread::scope(|s| {
        let read = s.spawn(|| {
            let r = f.node(0).get(&path, &f.admin);
            done.store(true, Ordering::SeqCst);
            r
        });
        gate.handle.wait_stalled(Duration::from_secs(60));
        for i in 0..PAST_THRESHOLD {
            commit(&f, 1, "app", "churn.txt", &format!("churn {i}\n"));
        }
        compact(&f, 1, "app");
        // Same guard as the epoch-swap test: if the read had already
        // finished, its manifest and its locator would trivially agree
        // and the assertion below would be decoration.
        assert!(
            !done.load(Ordering::SeqCst),
            "the point read finished before the swap: the pointers never disagreed"
        );
        gate.handle.release();
        read.join().unwrap()
    });

    let epoch_after = f.manifest(&prefix).epoch;
    assert_ne!(
        epoch_before, epoch_after,
        "the pointers never disagreed, so nothing was tested"
    );
    assert_eq!(status, 200, "point read across the swap: {body}");
    assert_eq!(body, serde_json::Value::String(want), "wrong content");

    // Every key this request touched, minus the pointers (no epoch) and
    // the WAL (absolute keys carried in the manifest, not derived from an
    // epoch at all). What is left is the plane's own data.
    let seen = gate.handle.keys();
    let plane_epochs: BTreeSet<String> = seen
        .iter()
        .filter(|(m, k)| m == "GET" && !k.contains("/wal/"))
        .filter_map(|(_, k)| epoch_of(k, &prefix))
        .collect();
    assert_eq!(
        plane_epochs.len(),
        1,
        "the reader mixed epochs in one request: {plane_epochs:?}\nkeys: {seen:?}"
    );
    assert_eq!(
        plane_epochs.iter().next(),
        Some(&epoch_after),
        "the reader took its data epoch from the manifest it was holding \
         ({epoch_before}) rather than from locator.hdr ({epoch_after})"
    );
    for n in f.nodes() {
        assert!(n.healthy());
    }
}

/// A push's existence oracle is `locator.hdr` plus the manifest's WAL,
/// and a fold on another node moves objects from the second into the
/// first: new generation, then `locator.hdr`, then `manifest.json` (I7).
/// receive-pack used to read the two the other way round — plane first,
/// manifest second — so a fold landing between those two GETs left it
/// holding a plane from before the fold and a manifest from after it,
/// and every object the fold had just moved was in neither. The
/// connectivity walk then refused the push with `missing object <oid>
/// (push incomplete)`, naming the very tip the server had advertised
/// and the client had built on.
///
/// The manual pass met it as one of six sibling pushes off one parent
/// failing while the five around it succeeded, which is what a race
/// looks like from outside. Here the window is opened on purpose: the
/// push's `locator.hdr` GET is parked one byte in, another node folds
/// the WAL that holds the parent, and only then is the push let go. It
/// must land, and the branch must be clonable from a node that never
/// saw the race.
#[test]
fn a_push_survives_a_fold_landing_between_its_two_pointer_reads() {
    let minio = Minio::shared();
    let bucket = minio.bucket("fleet-fold-under-push");
    let gate = GateProxy::start(minio.authority());
    let f = Fleet::builder(BIN, &bucket.base_url)
        .hint("fleet-fold-under-push")
        .nodes(2)
        .node_store_url(0, &format!("{}/{}", gate.url, bucket.name))
        .start();
    let prefix = f.create_repo("app");

    // A plane to be stale against, then one more commit that lives only
    // in the WAL: that is the parent the push builds on, and the object
    // the fold is going to move.
    for i in 0..PAST_THRESHOLD {
        commit(&f, 1, "app", "stable.txt", &format!("rev {i}\n"));
    }
    compact(&f, 1, "app");
    let parent = commit(&f, 1, "app", "later.txt", "later\n");
    let epoch_before = f.manifest(&prefix).epoch;

    let work = clone_from(&f, 1, "app", "work");
    assert_eq!(gitcli::git(&work, &["rev-parse", "HEAD"]).trim(), parent);
    gitcli::git(&work, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(work.join("topic.txt"), "on top of the WAL tip\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "topic"]);
    let want = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    gate.handle.clear();
    gate.handle.stall_get("locator.hdr", 1);
    let url = f.url(0, "app");
    let done = AtomicBool::new(false);
    let pushed = std::thread::scope(|s| {
        let push = s.spawn(|| {
            let r = gitcli::git_expect_err(&work, &["push", "-q", &url, "topic"]);
            done.store(true, Ordering::SeqCst);
            r
        });
        gate.handle.wait_stalled(Duration::from_secs(60));
        // Enough on `main` to fold, on the node that is not parked. The
        // fold takes `parent` with it.
        for i in 0..PAST_THRESHOLD {
            commit(&f, 1, "app", "churn.txt", &format!("churn {i}\n"));
        }
        compact(&f, 1, "app");
        assert!(
            !done.load(Ordering::SeqCst),
            "the push finished before the fold: the pointers never disagreed"
        );
        gate.handle.release();
        push.join().unwrap()
    });

    let epoch_after = f.manifest(&prefix).epoch;
    assert_ne!(
        epoch_before, epoch_after,
        "the fold never happened, so nothing was tested"
    );
    // `git_expect_err` answers Err with stdout when git *succeeded*, which
    // is the outcome wanted here; Ok carries the refusal.
    if let Ok(stderr) = pushed {
        panic!("the push was refused across the fold:\n{stderr}");
    }
    assert_eq!(
        f.refs_from(1, "app").get("refs/heads/topic"),
        Some(&want),
        "the push was accepted but the other node does not see the branch"
    );
    let dest = f.scratch().join("after-fold");
    f.clone_and_fsck_from(1, "app", &dest);
    gitcli::git(&dest, &["checkout", "-q", "topic"]);
    assert_eq!(
        std::fs::read_to_string(dest.join("topic.txt")).unwrap(),
        "on top of the WAL tip\n"
    );
    for n in f.nodes() {
        assert!(n.healthy());
    }
}

/// A push forwarded by a node that has never synced the mirror.
///
/// The seed clone a forward pushes from is per node, on local disk: a
/// node that has never synced this mirror has none, and the client's
/// thin pack has nothing to be fixed against until it does. So the
/// cold node fetches the origin first, forwards, reflects — and every
/// node serves the result, because the layout is shared and the seed
/// never was. Two nodes, one origin, one bucket.
#[test]
fn a_push_forwarded_by_a_cold_node_is_visible_from_every_node() {
    let minio = Minio::shared();
    let bucket = minio.bucket("fleet-mirror-push");
    let f = fleet(&bucket.base_url, "fleet-mirror-push", 2);

    // A bare origin under the fleet's scratch, reached over file://.
    let work = f.scratch().join("origin-work");
    let tip = gitcli::fixture_repo(&work, 3);
    let bare = f.scratch().join("origins").join("widget.git");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    gitcli::git(
        bare.parent().unwrap(),
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    gitcli::git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let origin_url = format!("file://{}", bare.display());

    // Registered and synced on node 0: node 0 has a seed, node 1 does not.
    let mirrors = f.repo_path("widget").replace("/repos/widget", "/mirrors");
    let (st, out) = f.node(0).post(
        &mirrors,
        &f.admin,
        Some(serde_json::json!({ "name": "widget", "provider": "generic", "origin": origin_url })),
    );
    assert_eq!(st, 202, "{out}");
    let (st, out) = f
        .node(0)
        .post(&format!("{mirrors}/widget/sync"), &f.admin, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["last_synced_commit"], tip, "{out}");

    // Cloned from and pushed through the cold node.
    let clone = clone_from(&f, 1, "widget", "cold-clone");
    std::fs::write(clone.join("cold.txt"), "forwarded by a cold node\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "through the cold node"]);
    let pushed = gitcli::git(&clone, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    gitcli::git(&clone, &["push", "-q", "origin", "main"]);
    assert_eq!(
        gitcli::git(&bare, &["rev-parse", "refs/heads/main"]).trim(),
        pushed,
        "the origin took the push first"
    );

    // Every node serves it, no sync asked of either.
    for i in 0..f.len() {
        let dest = clone_from(&f, i, "widget", &format!("after-{i}"));
        assert_eq!(
            gitcli::git(&dest, &["rev-parse", "HEAD"]).trim(),
            pushed,
            "node {i} does not serve the forwarded push"
        );
    }
    f.assert_refs_converged("widget");
}
