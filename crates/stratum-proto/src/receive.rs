//! The write path: `git-receive-pack` over smart HTTP, WAL/CAS style.
//!
//! A push becomes (1) a verified thin pack payload stored append-only under
//! the layout's epoch (`.../wal/<digest>.seg` + a sorted oid sidecar), and
//! (2) a manifest swap guarded by If-Match — the manifest is the ref
//! transaction record, so concurrent pushes serialize on the store's CAS
//! and readers always see a complete, consistent state.
//!
//! Policy, enforced loudly: pack ≤ the front's body cap, and objects the
//! layout already holds are **dropped** from the segment this push writes
//! rather than carried into it — a concat stream never delivers an object
//! twice (I5). That used to be a refusal, which stranded anyone who pushed
//! a branch, deleted it and pushed it again; see the note beside the filter
//! for why dropping is both correct and what the other two writers of this
//! WAL already do.
//!
//! Verification is quarantine-shaped, like real forges: earlier WAL
//! entries are materialized, thin bases are prefetched through the read
//! plane, `git index-pack --fix-thin` validates hashes and deltas, and a
//! BFS over the pushed objects checks connectivity — frontier edges must
//! exist in the locator or an earlier WAL entry.
//!
//! STRATUM-CORE DIVERGENCE: takes an object-store key `prefix` (the product
//! addresses repos as `o/<org>/r/<repo>/<layout>`) instead of the research
//! build's `repo`/`layout` pair, and writes no HTTP headers.

use crate::pktline::{read_pkt, write_data, write_flush, Pkt};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::process::Command;
use stratum_store::manifest::WalEntry;
use stratum_store::pack::{hex, scan_pack, type_name};
use stratum_store::{gitobj, Manifest, ObjectStore, Plane, PutCond, PutError};

const ZERO: &str = "0000000000000000000000000000000000000000";

// STRATUM-CORE DIVERGENCE: the walk caps are env-tunable so operators can
// tighten them per deployment and tests can exercise the fallback arms
// with tiny graphs; defaults match the research constants.
fn max_bfs_visits() -> usize {
    std::env::var("STRATUM_MAX_BFS_VISITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000)
}
fn max_frontier_lookups() -> usize {
    std::env::var("STRATUM_MAX_FRONTIER_LOOKUPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000)
}
// Measured (experiment 008): 3 attempts with no backoff lose half of 8
// concurrent same-repo pushes; 10 with jitter land them all. Each retry
// re-validates against the fresh manifest, so more attempts trade only
// tail latency, never correctness.
const CAS_RETRIES: usize = 10;

/// v0-style ref advertisement for receive-pack. Protocol v0 has no
/// prefix filtering, so a paged repo streams every page here — inherent
/// to git's push protocol (forges pay the same cost).
pub fn advertise(
    store: &ObjectStore,
    manifest: &Manifest,
    out: &mut impl Write,
) -> Result<(), String> {
    (|| -> std::io::Result<()> {
        crate::pktline::write_text(out, "# service=git-receive-pack")?;
        write_flush(out)
    })()
    .map_err(|e| e.to_string())?;
    advertise_body(store, manifest, out)
}

// STRATUM-CORE DIVERGENCE: the ref advert body split from the smart-HTTP
// service announcement — the SSH transport sends the body alone (the
// `# service` prefix is an HTTP-only framing detail).
pub fn advertise_body(
    store: &ObjectStore,
    manifest: &Manifest,
    out: &mut impl Write,
) -> Result<(), String> {
    let mut listed = manifest.refs.clone();
    for page in &manifest.ref_pages {
        listed.extend(stratum_store::refpages::load_page(store, page)?);
    }
    listed.sort();
    listed.dedup_by(|a, b| a.0 == b.0);
    (|| -> std::io::Result<()> {
        // `delete-refs` is a **client-side gate**: without it in the
        // advert, git refuses a deletion before sending anything, and
        // what the user sees is a bare `! [remote rejected] <ref>` with
        // no reason — because there is no server round trip to carry
        // one. The engine's deletion support is worth nothing until it
        // is advertised here.
        let caps = "report-status delete-refs ofs-delta agent=stratum/0.1";
        let mut first = true;
        for (name, oid) in &listed {
            let line = if first {
                first = false;
                format!("{oid} {name}\0{caps}\n")
            } else {
                format!("{oid} {name}\n")
            };
            write_data(out, line.as_bytes())?;
        }
        if first {
            // empty repo: capabilities^{} placeholder line
            write_data(
                out,
                format!("{ZERO} capabilities^{{}}\0{caps}\n").as_bytes(),
            )?;
        }
        write_flush(out)
    })()
    .map_err(|e| e.to_string())
}

/// One `<old> <new> <refname>` command from the client.
///
/// Public because the embedding server needs to know *what moved* once
/// a push is accepted: which branch, from where, to what. Before the
/// runner existed the answer was a bool, and the three side-effects a
/// push has (compaction, authorship, the webhook) were happy with it.
/// A workflow is triggered per ref, at the commit the ref now names,
/// and it must not be re-derived by reading the manifest afterwards —
/// a second push landing in between would have it run the wrong
/// commit under the first push's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub old: String,
    pub new: String,
    pub name: String,
}

/// Whether a buffered receive-pack request is already complete.
///
/// For a stream front there is no Content-Length to delimit the body, so
/// the natural thing is to read to EOF — and that is what the SSH door
/// did. It deadlocks on a **delete-only push**: git sends the command
/// list and a flush, sends no pack because nothing new is arriving, and
/// then waits for the status report without closing its side. The server
/// waits for an EOF that is never coming, the client waits for a report
/// that is never coming, and what the user sees ten minutes later is
/// "the remote end hung up unexpectedly".
///
/// A push that carries objects still needs the read-to-EOF: a pack's
/// length is not knowable without parsing it, and that path already
/// works because git closes its side once the pack is out.
///
/// So this answers only the question that unblocks the deadlock: are the
/// commands complete, and are they all deletions? Anything it cannot
/// parse yet is "not complete", which leaves the caller doing exactly
/// what it did before.
pub fn request_is_complete(body: &[u8]) -> bool {
    let mut cur = std::io::Cursor::new(body);
    let mut saw_command = false;
    loop {
        match read_pkt(&mut cur) {
            Ok(Pkt::Data(d)) => {
                let line = String::from_utf8_lossy(&d);
                let cmd = line.trim_end_matches('\n');
                let cmd = cmd.split('\0').next().unwrap_or("");
                let mut it = cmd.split(' ');
                let (_old, new) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
                if new != ZERO {
                    return false;
                }
                saw_command = true;
            }
            // The flush that ends the command list. Every command before
            // it was a deletion, so no pack follows and the request is
            // whole.
            Ok(Pkt::Flush) => return saw_command,
            _ => return false,
        }
    }
}

/// Returns `Ok(Some(updates))` when the push was accepted (WAL + manifest
/// landed) — the commands, in the order the client sent them — and
/// `Ok(None)` when it was rejected with an `ng` report or carried no
/// commands at all. The embedding server audits accepted pushes and
/// triggers workflows per accepted ref. (STRATUM-CORE DIVERGENCE: the
/// research CGI returned unit.)
///
/// STRATUM-CORE DIVERGENCE: `protected` names branches the embedding
/// product allows to move only through its review land queue. The check
/// must live here, before any ref precondition work, because this
/// function parses the update commands out of the pkt-line stream — the
/// callers never see ref names. The research build had no policy layer
/// and passed no list; both product fronts (HTTP and SSH) load it from
/// the control plane per push.
///
pub fn receive(
    store: &ObjectStore,
    prefix: &str,
    body: &[u8],
    protected: &[String],
    out: &mut impl Write,
) -> Result<Option<Vec<Update>>, String> {
    let Some(req) = parse_request(body)? else {
        return write_flush(out).map(|_| None).map_err(|e| e.to_string());
    };
    let verdict = process(store, prefix, &req.updates, protected, &req.pack);
    report(out, &req, verdict)
}

/// A receive-pack request, parsed: the commands and the pack that
/// follows them.
///
/// Parsing is separate from [`process`] because not every push is
/// landed here. A mirror's push is forwarded to its origin first and
/// only reflected locally once the origin has taken it, and the server
/// module doing that needs the commands — to build the forward — and
/// the report format — to answer the client in the words git expects —
/// without the quarantine and the manifest CAS in between. Both halves
/// live here so the wire format has one author.
pub struct Request {
    pub updates: Vec<Update>,
    /// The pack bytes exactly as sent, `PACK` header and trailer
    /// included; empty for a deletion-only push.
    pub pack: Vec<u8>,
    /// Whether the client asked for `side-band-64k`, which changes how
    /// the report is framed.
    pub sideband: bool,
}

/// Parse the commands and pack out of a receive-pack body.
///
/// `Ok(None)` is a request with no commands at all — everything up to
/// date — which the caller answers with a bare flush.
pub fn parse_request(body: &[u8]) -> Result<Option<Request>, String> {
    let mut cur = std::io::Cursor::new(body);
    let mut updates = Vec::new();
    let mut caps = String::new();
    loop {
        match read_pkt(&mut cur).map_err(|e| e.to_string())? {
            Pkt::Flush | Pkt::Eof => break,
            Pkt::Delim => return Err("unexpected delim in receive-pack".into()),
            Pkt::Data(d) => {
                let line = String::from_utf8_lossy(&d);
                let line = line.trim_end_matches('\n');
                let (cmd, cap_part) = match line.split_once('\0') {
                    Some((c, caps_s)) => (c, Some(caps_s)),
                    None => (line, None),
                };
                if let Some(c) = cap_part {
                    caps = c.to_string();
                }
                let mut it = cmd.split(' ');
                let (old, new, name) = (
                    it.next().unwrap_or("").to_string(),
                    it.next().unwrap_or("").to_string(),
                    it.next().unwrap_or("").to_string(),
                );
                if old.len() != 40 || new.len() != 40 || !name.starts_with("refs/") {
                    return Err(format!("malformed update command {line:?}"));
                }
                updates.push(Update { old, new, name });
            }
        }
    }
    if updates.is_empty() {
        return Ok(None);
    }
    let pack_at = cur.position() as usize;
    Ok(Some(Request {
        updates,
        pack: body[pack_at..].to_vec(),
        sideband: caps.contains("side-band-64k"),
    }))
}

/// Write the status report for a request and say what was accepted.
///
/// A push is atomic: `Ok` reports every command `ok` and hands the
/// commands back for the caller's side effects; `Err(reason)` reports
/// every command `ng` with that reason, which is the shape git's own
/// atomic refusal takes, and hands back nothing.
pub fn report(
    out: &mut impl Write,
    req: &Request,
    verdict: Result<(), String>,
) -> Result<Option<Vec<Update>>, String> {
    match verdict {
        Ok(()) => {
            let mut text = String::from("unpack ok\n");
            for u in &req.updates {
                text.push_str(&format!("ok {}\n", u.name));
            }
            send_report(out, &text, req.sideband).map(|_| Some(req.updates.clone()))
        }
        Err(reason) => {
            eprintln!("stratum-receive: rejected: {reason}");
            let mut text = String::from("unpack ok\n");
            for u in &req.updates {
                text.push_str(&format!("ng {} {}\n", u.name, reason.replace('\n', " ")));
            }
            send_report(out, &text, req.sideband).map(|_| None)
        }
    }
}

/// Write a status report with a verdict per command.
///
/// What a forwarded push needs and a local one never does: an origin
/// answering `--atomic` refuses the commands it objected to by name and
/// the rest with "atomic push failed", and the person reading the
/// report should see which was which. Any `Err` makes the whole push
/// refused; `Ok(Some(..))` only when every verdict is `Ok`.
pub fn report_each(
    out: &mut impl Write,
    req: &Request,
    verdicts: &[Result<(), String>],
) -> Result<Option<Vec<Update>>, String> {
    debug_assert_eq!(verdicts.len(), req.updates.len());
    let all_ok = verdicts.iter().all(|v| v.is_ok());
    let mut text = String::from("unpack ok\n");
    for (u, v) in req.updates.iter().zip(verdicts) {
        match v {
            Ok(()) if all_ok => text.push_str(&format!("ok {}\n", u.name)),
            // git's own wording for a command that was fine on its own
            // and refused because a sibling was not.
            Ok(()) => text.push_str(&format!("ng {} atomic push failed\n", u.name)),
            Err(reason) => text.push_str(&format!("ng {} {}\n", u.name, reason.replace('\n', " "))),
        }
    }
    if !all_ok {
        eprintln!("stratum-receive: rejected: {}", text.trim_end());
    }
    send_report(out, &text, req.sideband).map(|_| all_ok.then(|| req.updates.clone()))
}

fn send_report(out: &mut impl Write, report: &str, sideband: bool) -> Result<(), String> {
    (|| -> std::io::Result<()> {
        if sideband {
            // The whole report rides one channel-1 pkt, then a flush inside
            // the band, then the outer flush.
            let mut inner = Vec::new();
            for line in report.lines() {
                crate::pktline::write_text(&mut inner, line)?;
            }
            write_flush(&mut inner)?;
            let mut banded = vec![1u8];
            banded.extend_from_slice(&inner);
            write_data(out, &banded)?;
        } else {
            for line in report.lines() {
                crate::pktline::write_text(out, line)?;
            }
        }
        write_flush(out)
    })()
    .map_err(|e| e.to_string())
}

fn process(
    store: &ObjectStore,
    prefix: &str,
    updates: &[Update],
    protected: &[String],
    pack: &[u8],
) -> Result<(), String> {
    // The default branch, read before any validation: deleting the ref
    // HEAD points at leaves a repository every clone reports as empty,
    // so it is refused by name rather than discovered afterwards.
    let head_ref = {
        let (m, _) = store.get_with_etag(&format!("{prefix}/manifest.json"))?;
        let m: Manifest = serde_json::from_slice(&m).map_err(|e| format!("manifest: {e}"))?;
        m.head
    };
    let manifest0_head_hint = head_ref.as_str();
    for u in updates {
        // **`refs/heads/` and `refs/tags/`.**
        //
        // Tags used to be refused outright, which meant no project could
        // cut a release here: `git push --tags` is how every one of them
        // does it. A tag is a ref like any other to this layer — the
        // connectivity walk already understands the `tag` object type,
        // and an annotated tag reaches the store as a fourth kind of
        // object in the pack.
        let rest = u
            .name
            .strip_prefix("refs/heads/")
            .or_else(|| u.name.strip_prefix("refs/tags/"));
        let ok_name = rest.is_some_and(|r| {
            !r.is_empty()
                && r.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b))
        });
        if !ok_name {
            return Err(format!("ref name {} not accepted", u.name));
        }
        let is_branch = u.name.starts_with("refs/heads/");
        if u.new == ZERO {
            // A deletion. Tags delete freely; a branch has two rules,
            // and each is refused in words rather than by a status code.
            if is_branch {
                let branch = &u.name["refs/heads/".len()..];
                // The default branch. Deleting it leaves a repository
                // that every clone reports as empty.
                if u.name == manifest0_head_hint {
                    return Err(format!(
                        "'{branch}' is the default branch: point HEAD elsewhere first"
                    ));
                }
                // A protected branch, exactly as GitHub does it: the
                // fence stops a deletion and not only a push. Protecting
                // a branch against being *moved* while leaving it
                // deletable protects nothing — the land queue is the
                // only road to trunk, and removing trunk is the shortest
                // way around it.
                //
                // Worded for what was attempted: "land through review"
                // is the answer to a rejected push and no answer at all
                // to a rejected deletion, which review cannot perform.
                if protected.iter().any(|b| b == branch) {
                    return Err(format!(
                        "branch '{branch}' is protected: it cannot be deleted"
                    ));
                }
            }
            continue;
        }
        // STRATUM-CORE DIVERGENCE: a protected branch moves only through
        // the product's land queue. Checked before any store work — a
        // refused push must not cost a quarantine — and phrased exactly
        // like the REST doors so `git push` and the API disagree about
        // nothing. The whole push is refused, matching git's own atomic
        // report for a rejected command set.
        if is_branch {
            let branch = &u.name["refs/heads/".len()..];
            if protected.iter().any(|b| b == branch) {
                return Err(format!(
                    "branch '{branch}' is protected: land through review"
                ));
            }
        }
    }
    // A push that only deletes carries no pack, and needs none: nothing
    // new arrives, so there is no quarantine to verify and no
    // connectivity to prove. It goes straight to the ref transaction.
    let deletions_only = updates.iter().all(|u| u.new == ZERO);
    if deletions_only {
        return delete_refs(store, prefix, updates);
    }
    if pack.len() < 32 || &pack[..4] != b"PACK" {
        return Err("no pack in push".into());
    }
    let entries = u32::from_be_bytes(pack[8..12].try_into().unwrap()) as u64;
    let payload = &pack[12..pack.len() - 20];

    // The existence oracle is the locator plane plus the manifest's WAL,
    // and a fold moves objects from the second into the first: it writes
    // a new generation, swaps `locator.hdr`, then drops the folded entries
    // from `manifest.json` (I7). So the two have to be read **manifest
    // first**. Read the other way round, a fold landing between the two
    // GETs leaves this push holding a plane from before the fold and a
    // manifest from after it, and every object the fold moved is in
    // neither — including the advertised tip the client just built on.
    // The manual pass met exactly that: one of six sibling pushes off the
    // same parent, refused with `missing object <the parent> (push
    // incomplete)` while a compaction ran, and the five around it fine.
    // Manifest first, the worst a fold can do is hand us a plane that
    // already holds what the manifest still lists — a superset, which is
    // harmless. The CAS loop below re-reads the manifest on every attempt
    // and reloads the plane whenever the locator it names has moved, for
    // the same reason. See
    // `a_push_survives_a_fold_landing_between_its_two_pointer_reads`.
    let manifest_key = format!("{prefix}/manifest.json");
    let (m0, _) = store.get_with_etag(&manifest_key)?;
    let manifest0: Manifest = serde_json::from_slice(&m0).map_err(|e| format!("manifest: {e}"))?;
    let mut plane = load_plane(store, prefix)?;
    let mut plane_locator = manifest0.locator.as_ref().map(|l| l.key.clone());

    // Quarantine: verify hashes/deltas exactly the way forges do. The
    // guard removes the directory on every exit path.
    let qguard = QGuard(tempfile_dir()?);
    let qdir = qguard.0.clone();
    let q = qdir.as_str();
    run(git().args(["init", "-q", "--bare", q]))?;

    // Materialize existing WAL entries into the quarantine first: a push's
    // thin deltas may base on objects from earlier pushes that the locator
    // (built at ingest) doesn't know. Bounded by WAL size pre-compaction.
    for w in &manifest0.wal {
        let wpayload = store.get(&w.key)?;
        fetch_locator_bases(store, plane.as_ref(), q, &wpayload, w.entries)?;
        let mut full = Vec::with_capacity(wpayload.len() + 32);
        full.extend_from_slice(b"PACK");
        full.extend_from_slice(&2u32.to_be_bytes());
        full.extend_from_slice(&(w.entries as u32).to_be_bytes());
        full.extend_from_slice(&wpayload);
        let d = Sha1::digest(&full);
        full.extend_from_slice(&d);
        let mut c = git();
        c.args(["-C", q, "index-pack", "--fix-thin", "--stdin"]);
        run_with_stdin(&mut c, &full)
            .map_err(|e| format!("wal entry {} unreadable: {e}", w.key))?;
    }

    // Thin bases of the pushed pack itself.
    fetch_locator_bases(store, plane.as_ref(), q, payload, entries)?;

    // index-pack verifies every object hash and delta resolution. --strict
    // is deliberately absent: it additionally requires *referenced* objects
    // to exist locally, which only a full local odb can satisfy (and stock
    // git's receive.fsckObjects defaults off for the same reason). Server-
    // side connectivity is proven by the BFS below against locator + WAL.
    let mut c = git();
    c.args(["-C", q, "index-pack", "--fix-thin", "--stdin"]);
    let ipout =
        run_with_stdin(&mut c, pack).map_err(|e| format!("pack verification failed: {e}"))?;

    // index-pack prints "pack\t<sha>" (or keep\t<sha>); its idx names the
    // completed pack. The completed pack's first `entries` offsets
    // (ascending) are the client's objects; anything after is an appended
    // thin base.
    let pack_sha = ipout
        .split_whitespace()
        .last()
        .ok_or("index-pack: no pack id")?
        .to_string();
    let idx_path = format!("{q}/objects/pack/pack-{pack_sha}.idx");
    let idx = std::fs::read(&idx_path).map_err(|e| format!("read {idx_path}: {e}"))?;
    let mut pairs = gitobj::parse_idx(&idx)?;
    pairs.sort_by_key(|&(_, off)| off);
    let pushed: Vec<[u8; 20]> = pairs
        .iter()
        .take(entries as usize)
        .map(|&(o, _)| o)
        .collect();
    let pushed_set: HashSet<[u8; 20]> = pushed.iter().copied().collect();

    // CAS loop: validate against the current manifest, append the WAL
    // object, swap the manifest.
    let mut attempt = 0;
    loop {
        let (mbytes, etag) = store.get_with_etag(&manifest_key)?;
        let mut manifest: Manifest =
            serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;
        if manifest.epoch.is_empty() {
            return Err("layout predates epochs; re-ingest before pushing".into());
        }
        // A fold since the plane was loaded: the objects it moved out of
        // this manifest's WAL are only findable through the plane it
        // published, so take that one (see the note above `manifest0`).
        let locator_now = manifest.locator.as_ref().map(|l| l.key.clone());
        if locator_now != plane_locator {
            plane = load_plane(store, prefix)?;
            plane_locator = locator_now;
        }

        // Ref preconditions. Paged repos resolve through the page store
        // (one page GET per updated ref); the flat list still carries the
        // serving-critical tips and is checked first.
        let refmap: HashMap<&str, &str> = manifest
            .refs
            .iter()
            .map(|(n, o)| (n.as_str(), o.as_str()))
            .collect();
        for u in updates {
            let current: Option<String> = match refmap.get(u.name.as_str()) {
                Some(curr) => Some(curr.to_string()),
                None if !manifest.ref_pages.is_empty() => {
                    stratum_store::refpages::lookup(store, &manifest, &u.name)?
                }
                None => None,
            };
            match current {
                Some(curr) => {
                    if u.old == ZERO || curr != u.old {
                        return Err(format!("{}: stale old value, fetch first", u.name));
                    }
                }
                None => {
                    if u.old != ZERO {
                        return Err(format!("{}: does not exist here", u.name));
                    }
                    // Deleting a ref that exists takes the arm above.
                    // Deleting one that does not exist would land here,
                    // but git never sends
                    // `0{40} 0{40} <ref>`, and it would carry no objects
                    // and take no ref if it did — so there is nothing here
                    // to guard, and a branch nothing can reach is worse
                    // than none.
                }
            }
        }

        // Existence oracle: locator ∪ earlier WAL entries.
        let mut wal_oids: HashSet<[u8; 20]> = HashSet::new();
        for w in &manifest.wal {
            let raw = store.get(&w.oids_key)?;
            // STRATUM-CORE DIVERGENCE: chunks_exact -> as_chunks (newer
            // clippy denies constant-size chunks_exact); behavior identical.
            for c in raw.as_chunks::<20>().0 {
                wal_oids.insert(*c);
            }
        }
        let mut lookups = 0usize;
        let exists = |oid: &[u8; 20], lookups: &mut usize| -> Result<bool, String> {
            if wal_oids.contains(oid) {
                return Ok(true);
            }
            *lookups += 1;
            if *lookups > max_frontier_lookups() {
                return Err("push too large for the precomputed path (fallback)".into());
            }
            match &plane {
                Some(p) => Ok(p.lookup(store, oid)?.is_some()),
                None => Ok(false),
            }
        };

        // I5: a concat stream never carries an object twice. `clone_plan`
        // appends every WAL segment whole after the locator's segments, so
        // an object the layout already holds must not enter the WAL again.
        //
        // **On a create or a fast-forward, drop them. On a rewrite, still
        // refuse.** Which of those this push is comes out of the
        // connectivity walk below, so the decision is made after it.
        //
        // Refusing everywhere stranded people on an ordinary sequence —
        // push a branch, delete it, push it again — because deleting a ref
        // does not delete its objects, so the second push re-sent objects
        // the layout still held. The advice was unusable too: the leftover
        // is unreferenced, so no fetch brings it and the client already has
        // it. Dropping is what the other two writers of this WAL do
        // (`api::commits` skips objects the layout holds; `mirror::sync`
        // packs only what its walk found new).
        //
        // It was first bounded to creates because the refusal was also
        // doing a second job nobody wrote down. A stale client
        // force-pushing over somebody else's work re-sends live history it
        // cannot exclude — it does not have the tip the server advertised,
        // so it cannot use it as a negative base — and those duplicates are
        // what got it refused. The ref precondition does **not** catch that
        // case, despite the note below the connectivity walk claiming every
        // push here is force-with-lease: git fills `old` from the server's
        // own advertisement for a plain `--force`, so `curr == u.old`
        // always holds and the check passes. See
        // `a_force_push_rewrites_an_unprotected_branch_and_never_somebody_elses_work`.
        //
        // Bounding it to creates was too narrow, and it stranded people
        // again: `git revert`, restoring a deleted file, or changing a line
        // back all make a commit whose tree points at a blob the layout
        // already holds, and git sends that blob again. An ordinary
        // fast-forward on a branch nobody else touched was refused with the
        // same unusable advice, and that commit could never be pushed. A
        // fast-forward cannot be the stale case: its new tip descends from
        // the tip the server advertised, so the client *had* that tip, and
        // it takes nobody's work — exactly like a create. So the refusal
        // now stands only for an update whose old tip the walk did not
        // reach from the new one, which is the rewrite shape. (The walk
        // stops at the first object the server already holds, so a
        // fast-forward built on a commit that reached the server under
        // another ref is not recognised as one and keeps the refusal. That
        // is the old behaviour on a rarer shape, not a new refusal.) See
        // `a_fast_forward_that_reintroduces_a_held_blob_is_accepted`.
        //
        // And on a rewrite, only a re-sent **commit** (or tag) gives a
        // stale client away — a re-sent tree or blob does not. `git commit
        // --amend` then `git push --force-with-lease`, the most ordinary
        // answer to review there is, re-sends every subtree the amended
        // commit kept: git excludes the objects of the commit it replaces
        // at the commit level, but only walks the trees of *boundary*
        // commits, and the replaced tip is a sibling of the new one, not
        // its parent. So a contributor who added a directory and a note
        // in one commit, and reworded the note, was told "already present
        // (concurrent push?) — fetch and retry" against a branch nobody
        // else had touched, holding the very tip the advice told them to
        // fetch. A client that holds the advertised tip never re-sends a
        // commit from under it; a client that does not has no way to
        // exclude those commits and sends them — which is what
        // `a_force_push_rewrites_an_unprotected_branch_and_never_somebody_elses_work`
        // exercises, and what still refuses it. See
        // `an_amend_that_keeps_a_subtree_can_be_force_pushed`.
        let mut fresh: Vec<[u8; 20]> = Vec::with_capacity(pushed.len());
        let mut duplicated: Vec<[u8; 20]> = Vec::new();
        for oid in &pushed {
            let in_locator = match &plane {
                Some(p) => p.lookup(store, oid)?.is_some(),
                None => false,
            };
            if wal_oids.contains(oid) || in_locator {
                duplicated.push(*oid);
            } else {
                fresh.push(*oid);
            }
        }
        let duplicates = duplicated.len();

        // The old tip of every ref this push moves (not creates, not
        // deletes — neither can take anybody's work). A push is a
        // fast-forward when the walk from the new tips reaches all of
        // them, which a push of nothing but creates and deletes is
        // vacuously.
        let mut old_tips: HashSet<[u8; 20]> = HashSet::new();
        for u in updates {
            if u.old != ZERO && u.new != ZERO {
                old_tips.insert(Plane::parse_oid(&u.old)?);
            }
        }

        // Connectivity: BFS from each new tip through the quarantine;
        // every edge leaving it must exist server-side.
        //
        // No longer an ancestry proof. It used to also record whether
        // each update's old tip was reached, and refuse the push when it
        // was not — see the note below the walk for why that is gone and
        // what replaced it.
        let mut queue: VecDeque<[u8; 20]> = VecDeque::new();
        let mut visited: HashSet<[u8; 20]> = HashSet::new();
        for u in updates {
            // A deletion has no new tip to find. Walking it looks for the
            // all-zero oid, which is in no pack and no plane, so a push
            // that updated one ref and deleted another — `git push origin
            // main :old-branch`, what a cleanup step sends every day —
            // was refused with "new tip 000…0 not in push", and both
            // halves went down with it.
            if u.new == ZERO {
                continue;
            }
            let tip = Plane::parse_oid(&u.new)?;
            if !pushed_set.contains(&tip) && !exists(&tip, &mut lookups)? {
                return Err(format!("{}: new tip {} not in push", u.name, u.new));
            }
            queue.push_back(tip);
        }
        // One reader for the whole walk. Spawning one per object is what
        // made a push's cost track its object count and 504 at a few
        // thousand — see `CatFile`.
        let mut cat = CatFile::start(&qdir)?;
        while let Some(oid) = queue.pop_front() {
            if !visited.insert(oid) {
                continue;
            }
            if visited.len() > max_bfs_visits() {
                return Err("push graph too large (fallback)".into());
            }
            if !pushed_set.contains(&oid) {
                // Existing server-side history (locator or WAL): terminal.
                let h = hex(&oid);
                if !exists(&oid, &mut lookups)? {
                    return Err(format!("missing object {h} (push incomplete)"));
                }
                continue;
            }
            let (typ, body) = cat.object(&hex(&oid))?;
            let refs = match typ.as_str() {
                "commit" => gitobj::commit_refs(&body)?,
                "tree" => gitobj::tree_refs(&body)?,
                "tag" => gitobj::tag_refs(&body)?,
                _ => vec![],
            };
            for r in refs {
                queue.push_back(r);
            }
        }
        // Existing server-side objects enter `visited` before the walk
        // stops at them, so a moved ref's old tip is in it exactly when
        // the new tip descends from it through what was pushed.
        let fast_forward = old_tips.iter().all(|o| visited.contains(o));
        if !fast_forward && !duplicated.is_empty() {
            if let Some(oid) = first_history_object(&qdir, &duplicated)? {
                return Err(format!(
                    "object {} already present (concurrent push?) — fetch and retry",
                    hex(&oid)
                ));
            }
        }
        // **Force pushes are allowed on an unprotected ref, and every
        // push here is force-with-lease.**
        //
        // This used to refuse any update whose old tip was not an
        // ancestor of the new one, which made `git push --force`
        // impossible — on any branch, protected or not. Rebasing a review
        // branch and pushing it again is what a contributor does after
        // reading feedback, and there was no way to do it: the only
        // remedy was to push under a new name and abandon the old one.
        //
        // Dropping the ancestry proof does not drop the protection that
        // matters, because that protection is not the ancestry proof. Two
        // guards remain, and between them they are stricter than a plain
        // `--force` on GitHub:
        //
        // * the ref precondition above — the client's `old` must equal
        //   the value the ref holds right now, or the push is refused
        //   with "stale old value, fetch first". That is exactly
        //   `--force-with-lease`: you may overwrite the history you
        //   looked at, and never somebody's push that landed while you
        //   were rebasing. Git's own `--force` has no such check;
        //   here every push gets one whether it asks for it or not.
        // * a protected branch is refused outright, further up, before
        //   any of this runs. That is where "you may not rewrite trunk"
        //   is enforced — and it is the right place for it, because it is
        //   a policy about a branch rather than a property of a graph.
        //
        // The objects the old history was made of stay in the layout
        // until GC, so a force push is additive on disk and the ref move
        // is the only destructive part of it — which the manifest CAS
        // makes atomic.

        // What this attempt will actually store.
        //
        // Untouched when nothing was duplicated, which is every ordinary
        // push: the common path stores the client's pack byte for byte, as
        // it always has. Only when the layout already holds some of these
        // objects is the pack rebuilt from the ones it does not, so that
        // the segment appended to the clone stream carries each object
        // exactly once.
        //
        // Recomputed per attempt rather than before the CAS loop, because
        // *which* objects are duplicates is a fact about the manifest this
        // attempt read, and a racing push can change it.
        let repacked = if duplicates == 0 || fresh.is_empty() {
            None
        } else {
            Some(repack(q, &fresh)?)
        };
        let eff_payload: &[u8] = repacked.as_deref().unwrap_or(payload);
        let eff_pushed: &[[u8; 20]] = if duplicates == 0 { &pushed } else { &fresh };
        let eff_entries: u64 = if duplicates == 0 {
            entries
        } else {
            fresh.len() as u64
        };

        // WAL object + oid sidecar (content-addressed keys: idempotent).
        //
        // Skipped entirely when the push carried nothing the layout did not
        // already have — re-pushing a branch that was deleted, say. There is
        // no segment to append, and appending an empty one would put a WAL
        // entry with no objects into every future clone plan. The ref
        // update below is the whole of the transaction, and it is still
        // atomic against the manifest CAS.
        let wal_entry = if fresh.is_empty() {
            None
        } else {
            let digest = hex(&Sha1::digest(eff_payload));
            let wal_key = format!("{prefix}/{}/wal/{digest}.seg", manifest.epoch);
            let oids_key = format!("{prefix}/{}/wal/{digest}.oids", manifest.epoch);
            let mut sorted = eff_pushed.to_vec();
            sorted.sort();
            let oid_blob: Vec<u8> = sorted.iter().flat_map(|o| o.iter().copied()).collect();
            store
                .put(&wal_key, eff_payload, PutCond::None)
                .map_err(|e| e.to_string())?;
            store
                .put(&oids_key, &oid_blob, PutCond::None)
                .map_err(|e| e.to_string())?;
            Some((wal_key, oids_key))
        };

        // Manifest swap: the ref transaction commits here or not at all.
        // Paged repos write the ref into its page (a new content-addressed
        // object — O(page) write amplification, the point of gap 1); the
        // flat list is only touched for tips it already carries (HEAD /
        // primary). Unpaged repos keep the flat list as the store.
        let paged = !manifest.ref_pages.is_empty();
        let data_prefix = format!("{prefix}/{}", manifest.epoch);
        for u in updates {
            if u.new == ZERO {
                if paged {
                    stratum_store::refpages::remove(store, &mut manifest, &data_prefix, &u.name)?;
                }
                manifest.refs.retain(|(n, _)| n != &u.name);
                continue;
            }
            if paged {
                stratum_store::refpages::update(
                    store,
                    &mut manifest,
                    &data_prefix,
                    &u.name,
                    &u.new,
                )?;
                if let Some(r) = manifest.refs.iter_mut().find(|(n, _)| n == &u.name) {
                    r.1 = u.new.clone();
                }
            } else {
                match manifest.refs.iter_mut().find(|(n, _)| n == &u.name) {
                    Some(r) => r.1 = u.new.clone(),
                    None => manifest.refs.push((u.name.clone(), u.new.clone())),
                }
            }
        }
        if let Some((wal_key, oids_key)) = wal_entry {
            manifest.wal.push(WalEntry {
                key: wal_key,
                oids_key,
                entries: eff_entries,
                bytes: eff_payload.len() as u64,
                updates: updates
                    .iter()
                    .map(|u| (u.name.clone(), u.old.clone(), u.new.clone()))
                    .collect(),
            });
        }
        let new_manifest = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
        match store.put(&manifest_key, &new_manifest, PutCond::IfMatch(etag)) {
            Ok(()) => return Ok(()),
            Err(PutError::Conflict) => {
                attempt += 1;
                if attempt >= CAS_RETRIES {
                    return Err("concurrent pushes, retry".into());
                }
                // Jittered backoff so a herd of concurrent pushes fans out
                // instead of re-colliding in lockstep (no rand dependency:
                // clock nanos are jitter enough here).
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as u64)
                    .unwrap_or(12345);
                let ms = 10 * attempt as u64 + nanos % 50;
                std::thread::sleep(std::time::Duration::from_millis(ms));
                continue; // reload manifest, revalidate, retry
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// A push that only deletes refs.
///
/// Split out because it shares nothing with the pack path: there is no
/// quarantine to verify, no connectivity to prove, and no WAL entry to
/// write — nothing arrives. What is left is the ref transaction, and it
/// commits by the same manifest CAS as everything else so a deletion
/// cannot interleave with a concurrent push and lose.
///
/// The precondition is the client's `old` value. Git sends the oid it
/// last saw, and requiring it to match is the deletion's equivalent of
/// the fast-forward proof: you may only remove the ref you looked at, so
/// a delete cannot silently take somebody's push that landed in between.
fn delete_refs(store: &ObjectStore, prefix: &str, updates: &[Update]) -> Result<(), String> {
    let manifest_key = format!("{prefix}/manifest.json");
    let mut attempt = 0usize;
    loop {
        let (raw, etag) = store.get_with_etag(&manifest_key)?;
        let mut manifest: Manifest =
            serde_json::from_slice(&raw).map_err(|e| format!("manifest: {e}"))?;
        let data_prefix = format!("{prefix}/{}", manifest.epoch);
        let paged = !manifest.ref_pages.is_empty();

        for u in updates {
            let current: Option<String> = match manifest
                .refs
                .iter()
                .find(|(n, _)| n == &u.name)
                .map(|(_, o)| o.clone())
            {
                Some(curr) => Some(curr),
                None if paged => stratum_store::refpages::lookup(store, &manifest, &u.name)?,
                None => None,
            };
            match current {
                Some(curr) if curr == u.old => {}
                Some(_) => return Err(format!("{}: stale old value, fetch first", u.name)),
                None => return Err(format!("{}: does not exist here", u.name)),
            }
        }
        for u in updates {
            if paged {
                stratum_store::refpages::remove(store, &mut manifest, &data_prefix, &u.name)?;
            }
            manifest.refs.retain(|(n, _)| n != &u.name);
        }

        let new_manifest = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
        match store.put(&manifest_key, &new_manifest, PutCond::IfMatch(etag)) {
            Ok(()) => return Ok(()),
            Err(PutError::Conflict) => {
                attempt += 1;
                if attempt >= CAS_RETRIES {
                    return Err("concurrent pushes, retry".into());
                }
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as u64)
                    .unwrap_or(12345);
                std::thread::sleep(std::time::Duration::from_millis(
                    10 * attempt as u64 + nanos % 50,
                ));
                continue;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// The locator plane, or `None` for a repository that has none yet.
///
/// STRATUM-CORE DIVERGENCE: a freshly-created empty repo has no locator
/// plane yet — first pushes verify against the WAL alone. Any other load
/// failure still fails the push (a store outage must never be mistaken
/// for an empty history).
fn load_plane(store: &ObjectStore, prefix: &str) -> Result<Option<Plane>, String> {
    match Plane::load(store, prefix) {
        Ok(p) => Ok(Some(p)),
        Err(e) if e.contains("HTTP 404") => Ok(None),
        Err(e) => Err(e),
    }
}

/// Prefetch a payload's REF_DELTA bases that live in the locator plane
/// into the quarantine as loose objects (WAL-resident bases are already in
/// Rebuild a pack from exactly these objects, as a WAL segment body.
///
/// Only reached when a push carried objects the layout already holds. The
/// client's own pack cannot be stored in that case — appending it to the
/// clone stream would emit those objects a second time, which is the I5
/// violation the caller is avoiding — so the ones that are genuinely new
/// are re-packed out of the quarantine, which by this point holds the
/// pushed objects and every locator base they delta against.
///
/// Deliberately **not** `--thin`. A thin pack would delta against objects
/// left outside it, which is exactly the set being excluded, and the saving
/// does not justify the reasoning on a path this rare.
///
/// The 12-byte header and 20-byte trailer are stripped because a WAL
/// segment stores the pack *body* only; `entries` in the manifest is what
/// lets a reader rebuild the header (see `load_wal`, and the reconstruction
/// a few lines above `fetch_locator_bases`).
fn repack(q: &str, oids: &[[u8; 20]]) -> Result<Vec<u8>, String> {
    let mut list = String::with_capacity(oids.len() * 41);
    for o in oids {
        list.push_str(&hex(o));
        list.push('\n');
    }
    let mut c = git();
    c.args([
        "-C",
        q,
        "pack-objects",
        "--stdout",
        "--delta-base-offset",
        "-q",
    ]);
    let out = run_with_stdin_bytes(&mut c, list.as_bytes())
        .map_err(|e| format!("re-pack of {} object(s) failed: {e}", oids.len()))?;
    if out.len() < 32 || &out[..4] != b"PACK" {
        return Err("re-pack produced no pack".into());
    }
    Ok(out[12..out.len() - 20].to_vec())
}

/// the quarantine via earlier materialization).
fn fetch_locator_bases(
    store: &ObjectStore,
    plane: Option<&Plane>,
    q: &str,
    payload: &[u8],
    entries: u64,
) -> Result<(), String> {
    let scanned = scan_pack(payload, entries)?;
    let mut base_oids: HashSet<[u8; 20]> = HashSet::new();
    for e in &scanned {
        if let Some(b) = e.ref_base {
            base_oids.insert(b);
        }
    }
    for b in &base_oids {
        let oid = hex(b);
        // Already present (from a materialized WAL entry or earlier fetch)?
        let have = git()
            .args(["-C", q, "cat-file", "-e", &oid])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if have {
            continue;
        }
        let plane = plane.ok_or_else(|| format!("thin base {oid} not resolvable: no locator"))?;
        let (typ, data, _) = plane
            .read_object(store, store, &oid)
            .map_err(|e| format!("thin base {oid} not resolvable: {e}"))?;
        let mut c = git();
        c.args([
            "-C",
            q,
            "hash-object",
            "-w",
            "-t",
            type_name(typ),
            "--stdin",
        ]);
        let written = run_with_stdin(&mut c, &data)?;
        if written.trim() != oid {
            return Err(format!("thin base {oid} content mismatch"));
        }
    }
    Ok(())
}

/// The first of `oids` that is a commit or a tag, by asking the
/// quarantine for their types in one `cat-file --batch-check`.
///
/// One process for the lot rather than `cat_object` per object: on the
/// stale force push this exists to catch, the duplicates are the live
/// history the client did not have, and there can be a lot of it.
fn first_history_object(qdir: &str, oids: &[[u8; 20]]) -> Result<Option<[u8; 20]>, String> {
    let mut input = String::with_capacity(oids.len() * 41);
    for oid in oids {
        input.push_str(&hex(oid));
        input.push('\n');
    }
    let mut c = git();
    c.args(["-C", qdir, "cat-file", "--batch-check"]);
    let out = run_with_stdin(&mut c, input.as_bytes())
        .map_err(|e| format!("cat-file --batch-check: {e}"))?;
    for (line, oid) in out.lines().zip(oids) {
        let mut it = line.split(' ');
        let named = it.next().unwrap_or("");
        if named != hex(oid) {
            return Err(format!(
                "cat-file --batch-check: asked {}, told {line}",
                hex(oid)
            ));
        }
        match it.next().unwrap_or("") {
            "commit" | "tag" => return Ok(Some(*oid)),
            "tree" | "blob" => {}
            other => {
                return Err(format!(
                    "{} is {other} in the quarantine, and the pack was indexed",
                    hex(oid)
                ))
            }
        }
    }
    Ok(None)
}

/// One `git cat-file --batch` for a whole walk, spoken to rather than
/// respawned.
///
/// This used to be a function that spawned a process, wrote a single oid
/// to its stdin and waited for it to exit — once for **every object in
/// the push**. `--batch` is built to be asked many questions; asking it
/// one means paying a fork, an exec, git's own startup and an
/// object-database open per object. Measured at ~11 ms an object on an
/// idle laptop and ~20 ms on the fleet, against ~0.1 ms when one process
/// answers them all: a hundredfold.
///
/// The consequence was not slowness, it was failure, and it fell on
/// ordinary repositories rather than exotic ones. A push's cost tracked
/// its **object count** and barely noticed its size — 0.2 MiB across
/// 1,942 objects took three times as long as 6 MiB in one blob — so a
/// source repository, which is object-heavy by nature, spent longer in
/// this loop than the gateway would wait. At a few thousand objects the
/// push died at 60 s with `HTTP 504`, no ref created, and git printed
/// its cheerful "Everything up-to-date" *after* the fatal error. A
/// 58,000-object push failed 16 times out of 16; GitHub took the same
/// one in about four seconds.
///
/// `first_history_object` below already batched its lookups. This is the
/// streaming form of the same idea, because the walk cannot know what it
/// wants next until it has parsed what it just got.
struct CatFile {
    child: std::process::Child,
    /// `Option` only so `Drop` can close it: dropping stdin is what
    /// tells `cat-file` to exit, and it must happen before the wait.
    stdin: Option<std::process::ChildStdin>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

/// How many `cat-file --batch` processes this process has started.
///
/// The whole point of `CatFile` is that this does not grow with the
/// number of objects in a push, and that is not something a correctness
/// test can observe — the old spawn-per-object code returned exactly the
/// same answers, just slowly enough to be killed by a gateway. So the
/// count is the observable, and the tests assert on it.
static CAT_FILE_SPAWNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Reads [`CAT_FILE_SPAWNS`]. Diagnostics: a caller that walks a large
/// push can assert it started one reader and not one per object.
pub fn cat_file_spawns() -> u64 {
    CAT_FILE_SPAWNS.load(std::sync::atomic::Ordering::Relaxed)
}

impl CatFile {
    fn start(qdir: &str) -> Result<Self, String> {
        CAT_FILE_SPAWNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut child = git()
            .args(["-C", qdir, "cat-file", "--batch"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("cat-file --batch: {e}"))?;
        let stdin = child.stdin.take().ok_or("cat-file: no stdin")?;
        let stdout = child.stdout.take().ok_or("cat-file: no stdout")?;
        Ok(CatFile {
            child,
            stdin: Some(stdin),
            stdout: std::io::BufReader::new(stdout),
        })
    }

    /// Ask for one object. A missing one is an error, exactly as when
    /// this spawned a process per question — the walk treats it as a
    /// push that is not connected.
    fn object(&mut self, oid: &str) -> Result<(String, Vec<u8>), String> {
        let w = self.stdin.as_mut().ok_or("cat-file: stdin closed")?;
        w.write_all(format!("{oid}\n").as_bytes())
            .map_err(|e| format!("cat-file: write {oid}: {e}"))?;
        w.flush().map_err(|e| format!("cat-file: flush: {e}"))?;
        read_record(&mut self.stdout, oid)
    }
}

/// One `--batch` record, read from whatever the reader is.
///
/// Split from [`CatFile::object`] because every interesting thing that
/// can go wrong here is a shape of bytes, not a shape of process: a
/// batch that ended, a header naming no object, a body that stops short.
/// Reaching those through a real `git` means killing a subprocess at an
/// exact instant, and the write side fails first anyway — so the seam
/// moved to where the cases actually live.
fn read_record(r: &mut impl std::io::BufRead, oid: &str) -> Result<(String, Vec<u8>), String> {
    let mut header = String::new();
    // Nothing at all means the child is gone. A broken batch cannot be
    // resumed, and answering "missing" here would turn a crashed helper
    // into "your push is incomplete" — the client's fault, permanent,
    // and wrong.
    if r.read_line(&mut header)
        .map_err(|e| format!("cat-file: read header: {e}"))?
        == 0
    {
        return Err("cat-file: batch ended early".into());
    }
    let header = header.trim_end_matches('\n');
    let mut it = header.split(' ');
    let _oid = it.next().unwrap_or("");
    let typ = it.next().unwrap_or("").to_string();
    if typ == "missing" {
        return Err(format!("{oid} missing in quarantine"));
    }
    let size: usize = it
        .next()
        .unwrap_or("0")
        .parse()
        .map_err(|_| "cat-file: bad size")?;

    // `read_exact` rather than a slice of whatever arrived: the body
    // comes down a pipe and a large object arrives in several reads.
    let mut body = vec![0u8; size];
    r.read_exact(&mut body)
        .map_err(|e| format!("cat-file: short body for {oid}: {e}"))?;
    // Every record ends with a newline the next header must not see.
    let mut nl = [0u8; 1];
    r.read_exact(&mut nl)
        .map_err(|e| format!("cat-file: no record terminator for {oid}: {e}"))?;
    Ok((typ, body))
}

impl Drop for CatFile {
    fn drop(&mut self) {
        // Close stdin first so the child sees EOF and leaves on its own;
        // the walk returns early on any bad push, so without this a
        // refused push would leave a `cat-file` holding the quarantine
        // directory open. Then reap it, because dropping a `Child` does
        // not.
        self.stdin = None;
        let _ = self.child.wait();
    }
}

struct QGuard(String);
impl Drop for QGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempfile_dir() -> Result<String, String> {
    // STRATUM-CORE DIVERGENCE: the PID alone collides when one long-lived
    // server process verifies concurrent pushes; add a counter.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base =
        std::env::temp_dir().join(format!("stratum-quarantine-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    Ok(base.to_string_lossy().to_string())
}

/// A sandboxed `git` for quarantine work. The quarantine parses hostile
/// input (the pushed pack) with git binaries, so those processes run with:
/// - a scrubbed environment (no user/system gitconfig, no credentials,
///   no prompts) — nothing from the serving process leaks in, and no
///   attacker-influenced config path is consulted;
/// - kernel resource limits: 60s CPU, 2 GiB address space, 1 GiB file
///   size, 256 fds — a pack crafted to make index-pack spin or balloon
///   dies loudly instead of taking the node with it.
///
/// Full isolation (separate uid, no-network namespace) is product work,
/// tracked in the robustness review.
pub(crate) fn git() -> Command {
    let mut c = Command::new("git");
    c.env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // STRATUM-CORE DIVERGENCE: the resource argument to setrlimit is
        // `__rlimit_resource_t` on glibc but plain `c_int` on macOS and
        // the BSDs, so naming the glibc type under a bare `#[cfg(unix)]`
        // fails to compile everywhere else. The research repo only ever
        // built on Linux; this one is developed on darwin, where the
        // whole workspace — and therefore `scripts/ci-local.sh` — would
        // not build at all.
        #[cfg(target_env = "gnu")]
        type RlimitRes = libc::__rlimit_resource_t;
        #[cfg(not(target_env = "gnu"))]
        type RlimitRes = libc::c_int;
        // setrlimit is async-signal-safe, so it is legal in pre_exec.
        fn lim(res: RlimitRes, val: u64) {
            let rl = libc::rlimit {
                rlim_cur: val,
                rlim_max: val,
            };
            unsafe {
                libc::setrlimit(res, &rl);
            }
        }
        unsafe {
            c.pre_exec(|| {
                lim(libc::RLIMIT_CPU, 60);
                lim(libc::RLIMIT_AS, 2 << 30);
                lim(libc::RLIMIT_FSIZE, 1 << 30);
                lim(libc::RLIMIT_NOFILE, 256);
                Ok(())
            });
        }
    }
    c
}

fn run(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn run_with_stdin(cmd: &mut Command, input: &[u8]) -> Result<String, String> {
    let out = run_with_stdin_bytes(cmd, input)?;
    Ok(String::from_utf8_lossy(&out).to_string())
}

/// The same, keeping stdout as bytes.
///
/// `run_with_stdin` renders stdout through `from_utf8_lossy`, which is
/// right for the commands whose output is a line of text and silently
/// destructive for one whose output is a packfile: every byte that is not
/// valid UTF-8 becomes U+FFFD. `repack` needs the pack itself, so it goes
/// through here and the string version is a thin wrapper rather than a
/// second copy of the process plumbing.
fn run_with_stdin_bytes(cmd: &mut Command, input: &[u8]) -> Result<Vec<u8>, String> {
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input)
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod cat_file_tests {
    use super::*;
    use std::process::Command;

    /// `CAT_FILE_SPAWNS` counts the whole process, so a test that reads a
    /// delta from it cannot run beside another test that starts a reader
    /// — the first run of these two failed exactly that way, and passed
    /// alone. Every test here holds this while it has a reader open.
    static READERS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A repository with a handful of objects, one of them deliberately
    /// larger than a pipe buffer.
    fn fixture() -> (String, Vec<(String, String)>) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("weft-catfile-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap().to_string();

        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(["-C", &d])
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            assert!(out.status.success(), "git {args:?}: {stderr}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        run(&["init", "-q", "."]);
        for i in 0..12 {
            std::fs::write(dir.join(format!("f{i}.txt")), format!("content {i}\n")).unwrap();
        }
        // Bigger than a 64 KiB pipe buffer: the body cannot arrive in one
        // read, which is what `read_exact` is for.
        std::fs::write(dir.join("big.bin"), vec![b'x'; 200_000]).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "one"]);

        // (oid, type) for everything reachable.
        let listed = run(&["rev-list", "--objects", "HEAD"]);
        let oids: Vec<String> = listed
            .lines()
            .map(|l| l.split(' ').next().unwrap_or("").to_string())
            .filter(|s| s.len() == 40)
            .collect();
        let mut want = Vec::new();
        for oid in oids {
            let typ = run(&["cat-file", "-t", &oid]);
            want.push((oid, typ));
        }
        (d, want)
    }

    /// The regression this file exists for.
    ///
    /// `cat_object` used to spawn a process per object, so a push's cost
    /// tracked its object count and an ordinary repository's push died
    /// at the gateway's 60s timeout with a 504. The old code was
    /// perfectly *correct* — it returned these same bytes — so no
    /// assertion about the answers could have caught it. What changed is
    /// the number of processes, so that is what this asserts.
    #[test]
    fn one_reader_answers_every_object_whatever_the_count() {
        let _serial = READERS.lock().unwrap_or_else(|e| e.into_inner());
        let (qdir, want) = fixture();
        assert!(want.len() >= 14, "fixture too small: {}", want.len());

        let before = cat_file_spawns();
        let mut cat = CatFile::start(&qdir).unwrap();
        for (oid, typ) in &want {
            let (got_typ, body) = cat.object(oid).unwrap();
            assert_eq!(&got_typ, typ, "wrong type for {oid}");
            // The hash of what came back must be the object we asked
            // for: a batch reader that lost sync would answer the
            // *previous* object's bytes, and every one after it.
            let mut h = Sha1::new();
            h.update(format!("{typ} {}\0", body.len()).as_bytes());
            h.update(&body);
            let digest: [u8; 20] = h.finalize().into();
            assert_eq!(hex(&digest), *oid, "body does not hash to {oid}");
        }
        let spawned = cat_file_spawns() - before;
        let n = want.len();
        assert_eq!(
            spawned, 1,
            "{n} objects cost {spawned} cat-file processes; one reader must serve them all"
        );

        std::fs::remove_dir_all(&qdir).ok();
    }

    /// Every way a record can be wrong, without a process to break.
    ///
    /// These are the arms that decide what a *user* is told, and the
    /// distinction that matters most is the first one: "missing" is the
    /// client's fault and permanent — the walk reports a push that is
    /// not connected — while a batch that ended is ours and transient.
    /// Conflating them tells somebody their history is incomplete
    /// because a subprocess died.
    #[test]
    fn a_record_that_is_wrong_says_which_way_it_is_wrong() {
        // Nothing at all: the helper is gone.
        let e = read_record(&mut &b""[..], "abc").unwrap_err();
        assert_eq!(e, "cat-file: batch ended early");

        // The object genuinely is not there.
        let e = read_record(&mut &b"abc missing\n"[..], "abc").unwrap_err();
        assert!(e.contains("missing in quarantine"), "{e}");

        // A header we cannot make sense of is not silently a zero-length
        // object, which would hand the walk an empty commit.
        let e = read_record(&mut &b"abc blob notanumber\n"[..], "abc").unwrap_err();
        assert!(e.contains("bad size"), "{e}");

        // The body stops short: the pipe closed mid-object.
        let e = read_record(&mut &b"abc blob 10\nshort"[..], "abc").unwrap_err();
        assert!(e.contains("short body"), "{e}");

        // The body is whole but its terminator never arrives. Accepting
        // this would leave the next header misaligned and every later
        // answer wrong.
        let e = read_record(&mut &b"abc blob 5\nhello"[..], "abc").unwrap_err();
        assert!(e.contains("no record terminator"), "{e}");

        // And the shape that is right.
        let (typ, body) = read_record(&mut &b"abc blob 5\nhello\n"[..], "abc").unwrap();
        assert_eq!(typ, "blob");
        assert_eq!(body, b"hello");
    }

    /// A reader whose process has died says so, rather than answering
    /// "missing".
    ///
    /// The difference decides what a push is told. `missing` means the
    /// object is not in the quarantine, which the walk reports as a push
    /// that is not connected — the client's fault, and permanent. A dead
    /// helper is ours, and transient. Conflating them would tell somebody
    /// their history was incomplete because a subprocess was killed.
    #[test]
    fn a_dead_reader_is_an_error_not_a_missing_object() {
        let _serial = READERS.lock().unwrap_or_else(|e| e.into_inner());
        let (qdir, want) = fixture();
        let mut cat = CatFile::start(&qdir).unwrap();

        // It works, and then the process goes away underneath it.
        cat.object(&want[0].0).unwrap();
        cat.child.kill().unwrap();
        cat.child.wait().unwrap();

        let e = cat.object(&want[1].0).unwrap_err();
        assert!(
            e.contains("batch ended early") || e.contains("write"),
            "a dead reader must not read as a missing object: {e}"
        );
        assert!(!e.contains("missing in quarantine"), "{e}");

        std::fs::remove_dir_all(&qdir).ok();
    }

    /// A missing object is still an error — the walk reads that as a push
    /// that is not connected — and the reader stays usable afterwards, so
    /// one bad oid does not poison the rest of the walk.
    #[test]
    fn a_missing_object_errors_without_breaking_the_batch() {
        let _serial = READERS.lock().unwrap_or_else(|e| e.into_inner());
        let (qdir, want) = fixture();
        let mut cat = CatFile::start(&qdir).unwrap();

        let absent = "0123456789abcdef0123456789abcdef01234567";
        let e = cat.object(absent).unwrap_err();
        assert!(e.contains("missing in quarantine"), "{e}");

        let (typ, _) = cat.object(&want[0].0).unwrap();
        assert_eq!(typ, want[0].1);

        std::fs::remove_dir_all(&qdir).ok();
    }
}
