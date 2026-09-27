//! Protocol v2 upload-pack serving: capability advertisement, `ls-refs`, and
//! `fetch`. A clone is served as concat(pack header, segment payloads
//! streamed from object storage, sha1 trailer) — no pack-objects, no repo on
//! disk. Extracted verbatim from the research repo's `stratum-cgi/src/main.rs`
//! minus the CGI routing and response headers.

use crate::pktline::{self, read_section, write_data, write_delim, write_flush, write_text, Pkt};
use sha1::{Digest, Sha1};
use std::io::Write;
use stratum_store::{Manifest, ObjectStore};

const SIDEBAND_CHUNK: usize = 32 * 1024;

/// Smart-HTTP v2 capability advertisement body (the `info/refs` response for
/// `service=git-upload-pack`). The transport must have verified the client
/// sent `Git-Protocol: version=2` — we only speak v2 for fetches.
pub fn advertise(out: &mut impl Write, cdn: Option<&CdnPack>) -> Result<(), String> {
    (|| -> std::io::Result<()> {
        write_text(out, "# service=git-upload-pack")?;
        write_flush(out)
    })()
    .map_err(|e| e.to_string())?;
    advertise_body(out, cdn)
}

/// A CDN-hosted packfile the client may fetch instead of receiving the
/// bulk inline (git's `packfile-uri`). Pure data: the URI is resolved and
/// signed by the embedding transport, so this crate stays a protocol
/// library with no config, signing, or cloud dependency.
#[derive(Debug, Clone)]
pub struct CdnPack {
    /// Absolute URL the client fetches (already signed, if signing is on).
    pub uri: String,
    /// git's pack hash — the id advertised alongside the URI.
    pub pack_hash: String,
    /// Layout tip the pack was built at.
    pub tip: String,
    /// `manifest.total_entries()` at build time. Together with `tip` this
    /// proves the pack still covers the whole layout: entries only change
    /// when the layout does, so an equal pair means nothing was pushed or
    /// compacted since. HEAD alone would not prove it — the pack holds
    /// every ref, and a non-HEAD ref could have moved.
    pub total_entries: u64,
}

// STRATUM-CORE DIVERGENCE: the capability advert body split from the
// smart-HTTP service announcement — the SSH transport sends the body
// alone (the `# service` prefix is an HTTP-only framing detail).
pub fn advertise_body(out: &mut impl Write, cdn: Option<&CdnPack>) -> Result<(), String> {
    (|| -> std::io::Result<()> {
        write_text(out, "version 2")?;
        write_text(out, "agent=stratum/0.1")?;
        write_text(out, "ls-refs")?;
        // Only claim `packfile-uris` when we actually have a pack to hand
        // out: advertising it and then not delivering would leave a client
        // that opted in doing extra round trips for nothing.
        if cdn.is_some() {
            write_text(out, "fetch=shallow packfile-uris")?;
        } else {
            write_text(out, "fetch=shallow")?;
        }
        write_text(out, "object-format=sha1")?;
        write_flush(out)
    })()
    .map_err(|e| e.to_string())
}

// STRATUM-CORE: `pub(crate)` for `workspace`, which dispatches the same
// two v2 commands against a synthetic repository. One parser, so a
// changeset workspace cannot come to disagree with a repository about
// what a request even said.
pub(crate) struct Request {
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
}

pub(crate) fn parse_request(body: &[u8]) -> Result<Request, String> {
    let mut cur = std::io::Cursor::new(body);
    let (caps, term) = read_section(&mut cur).map_err(|e| e.to_string())?;
    let command = caps
        .iter()
        .find_map(|l| l.strip_prefix("command="))
        .ok_or("no command in request")?
        .to_string();
    let args = if term == Pkt::Delim {
        read_section(&mut cur).map_err(|e| e.to_string())?.0
    } else {
        Vec::new()
    };
    Ok(Request { command, args })
}

/// Summary of a v2 `fetch` request body; None for other commands or
/// malformed bodies. (STRATUM-CORE addition: the mirror freshness contract
/// inspects wants before serving; metering classifies clone vs fetch by
/// the presence of haves.)
pub struct FetchSummary {
    pub wants: Vec<String>,
    pub haves: usize,
    /// The client opted into `packfile-uri` offload (`fetch.uriprotocols`).
    /// Only such a client can be sent to the CDN, so this is what makes a
    /// clone meterable as offloaded rather than served inline.
    pub wants_packfile_uris: bool,
}

pub fn parse_fetch_summary(body: &[u8]) -> Option<FetchSummary> {
    let req = parse_request(body).ok()?;
    if req.command != "fetch" {
        return None;
    }
    Some(FetchSummary {
        wants: req
            .args
            .iter()
            .filter_map(|a| a.strip_prefix("want ").map(str::to_string))
            .collect(),
        haves: req.args.iter().filter(|a| a.starts_with("have ")).count(),
        wants_packfile_uris: req.args.iter().any(|a| a.starts_with("packfile-uris")),
    })
}

pub fn parse_fetch_wants(body: &[u8]) -> Option<Vec<String>> {
    parse_fetch_summary(body).map(|s| s.wants)
}

/// Serve one `git-upload-pack` POST body: dispatches `ls-refs` and `fetch`.
pub fn upload_pack(
    store: &ObjectStore,
    manifest: &Manifest,
    body: &[u8],
    out: &mut impl Write,
    cdn: Option<&CdnPack>,
) -> Result<(), String> {
    let req = parse_request(body)?;
    match req.command.as_str() {
        "ls-refs" => ls_refs(store, manifest, &req.args, out),
        "fetch" => fetch(store, manifest, &req.args, out, cdn),
        other => Err(format!("unsupported command {other}")),
    }
}

/// Advertised refs, honoring ref-prefix filters. With a paged ref store
/// only the pages overlapping a requested prefix are fetched — the point
/// of gap 1: advert cost scales with the answer, not the repo's ref count.
fn gather_refs(
    store: &ObjectStore,
    manifest: &Manifest,
    prefixes: &[&str],
) -> Result<Vec<(String, String)>, String> {
    let mut listed: Vec<(String, String)> = manifest.refs.clone();
    if !manifest.ref_pages.is_empty() {
        let pages: Vec<usize> = if prefixes.is_empty() {
            (0..manifest.ref_pages.len()).collect()
        } else {
            let mut v: Vec<usize> = prefixes
                .iter()
                .flat_map(|p| manifest.pages_overlapping(p))
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        for idx in pages {
            listed.extend(stratum_store::refpages::load_page(
                store,
                &manifest.ref_pages[idx],
            )?);
        }
    }
    listed.sort();
    // Pages are the source of truth for paged repos; the small manifest
    // list only duplicates serving-critical tips (same values). Dedup.
    listed.dedup_by(|a, b| a.0 == b.0);
    Ok(listed)
}

fn ls_refs(
    store: &ObjectStore,
    manifest: &Manifest,
    args: &[String],
    out: &mut impl Write,
) -> Result<(), String> {
    let symrefs = args.iter().any(|a| a == "symrefs");
    let prefixes: Vec<&str> = args
        .iter()
        .filter_map(|a| a.strip_prefix("ref-prefix "))
        .collect();
    let want = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p));

    // STRATUM-CORE DIVERGENCE: the product serves freshly-created empty
    // repos (Repos R1) — an empty advert is a flush, not an error, and a
    // HEAD line only appears when the head branch actually resolves.
    let tip = manifest.tip();
    let listed = gather_refs(store, manifest, &prefixes)?;
    (|| -> std::io::Result<()> {
        if let Some(tip) = tip.filter(|_| want("HEAD")) {
            let line = if symrefs {
                format!("{tip} HEAD symref-target:{}", manifest.head)
            } else {
                format!("{tip} HEAD")
            };
            write_text(out, &line)?;
        }
        for (name, oid) in &listed {
            if want(name) {
                write_text(out, &format!("{oid} {name}"))?;
            }
        }
        write_flush(out)
    })()
    .map_err(|e| e.to_string())
}

/// H1 + H2/H3 serving path. Clone (no haves) streams every segment;
/// incremental fetch from a spine commit streams the hot-tier suffix.
/// Anything else fails loudly so fallback classification stays honest.
fn fetch(
    store: &ObjectStore,
    manifest: &Manifest,
    args: &[String],
    out: &mut impl Write,
    cdn: Option<&CdnPack>,
) -> Result<(), String> {
    // STRATUM-CORE DIVERGENCE: tip may be absent (empty repo / detached
    // head branch); only the snapshot fast path needs it.
    let tip = manifest.tip();
    let tips = manifest.tips();
    let mut wants = Vec::new();
    let mut haves = Vec::new();
    let mut done = false;
    let mut ofs_delta = false;
    let mut deepen: Option<u64> = None;
    // Grafts the client says it is holding, so a deepen can lift them.
    let mut client_shallow: Vec<String> = Vec::new();
    // Set when the client opted into CDN-hosted packs (`fetch.uriprotocols`).
    let mut wants_packfile_uris = false;
    for arg in args {
        if let Some(oid) = arg.strip_prefix("want ") {
            wants.push(oid.to_string());
        } else if let Some(oid) = arg.strip_prefix("have ") {
            haves.push(oid.to_string());
        } else if let Some(n) = arg.strip_prefix("deepen ") {
            deepen = Some(n.parse().map_err(|_| "bad deepen value")?);
        } else {
            match arg.as_str() {
                "done" => done = true,
                "ofs-delta" => ofs_delta = true,
                "thin-pack" | "no-progress" | "include-tag" => {}
                // The grafts the client is currently holding. For a
                // shallow *corpus* ours never change after ingest, and
                // this was discarded on that basis — but a client that
                // took our own depth-1 answer holds a graft too, and
                // when it later asks to be deepened the boundary it
                // names has to be lifted by an `unshallow` line or git
                // keeps the graft and stays shallow for ever.
                other if other.starts_with("shallow ") => {
                    client_shallow.push(other["shallow ".len()..].to_string());
                }
                // The client lists the URI schemes it will fetch. We only
                // ever hand out the one absolute URI the transport
                // resolved, so the list itself needs no interpretation
                // beyond "the client accepts CDN packs at all".
                other if other.starts_with("packfile-uris") => wants_packfile_uris = true,
                other => return Err(format!("unsupported fetch arg: {other}")),
            }
        }
    }
    if !ofs_delta {
        return Err("client without ofs-delta support".into());
    }
    if wants.is_empty() {
        return Err("fetch with no wants".into());
    }
    // Tip-class wants (manifest tips and WAL-pushed tips) get the precise
    // plans below. Other advertised refs — page refs pointing into history
    // (tags, old branches) — are validated against the layout (spine or
    // locator membership proves the object is in the stream) and served
    // the full clone plan: a correct superset, at fallback-class cost.
    let tip_class = |w: &str| tips.contains(&w) || manifest.find_wal_tip(w).is_some();
    if wants.iter().any(|w| !tip_class(w)) {
        if manifest.ref_pages.is_empty() {
            return Err("want beyond advertised tips not served yet".into());
        }
        let repo_prefix = manifest
            .locator
            .as_ref()
            .map(|l| l.hdr_key.trim_end_matches("/locator.hdr").to_string())
            .ok_or("non-tip want needs the locator plane")?;
        let plane = stratum_store::Plane::load(store, &repo_prefix)?;
        for w in wants.iter().filter(|w| !tip_class(w)) {
            let known = manifest.find_spine(w).is_some()
                || plane
                    .lookup(store, &stratum_store::Plane::parse_oid(w)?)?
                    .is_some();
            if !known {
                return Err(format!("want {w} not known to this layout"));
            }
        }
        if deepen.is_some() {
            return Err("deepen with non-tip wants (fallback)".into());
        }
        if !done {
            // Push the client to `done`; the superset plan needs no ACKs.
            write_text(out, "acknowledgments").map_err(|e| e.to_string())?;
            write_text(out, "NAK").map_err(|e| e.to_string())?;
            return write_flush(out).map_err(|e| e.to_string());
        }
        if !manifest.shallow.is_empty() {
            write_text(out, "shallow-info").map_err(|e| e.to_string())?;
            for oid in &manifest.shallow {
                write_text(out, &format!("shallow {oid}")).map_err(|e| e.to_string())?;
            }
            write_delim(out).map_err(|e| e.to_string())?;
        }
        let (entries, plan) = (manifest.total_entries(), manifest.clone_plan());
        write_text(out, "packfile").map_err(|e| e.to_string())?;
        return stream_or_err(store, entries, &plan, out);
    }

    // depth-1 CI clone: served from the precomputed snapshot artifact
    // (tip commit + tree closure), no graph work. Deeper deepens and
    // deepen+secondary-tip wants stay fallback.
    if let Some(depth) = deepen {
        let snap = manifest.snapshot.as_ref();
        let tip = tip.ok_or("deepen on a repo with no head tip")?;
        // The snapshot is built at ingest; once WAL entries have moved the
        // tip, it is stale and depth-1 must fall back until compaction.
        if depth == 1
            && haves.is_empty()
            && wants.iter().all(|w| w.as_str() == tip)
            && manifest.wal.is_empty()
        {
            let snap = snap.ok_or("no snapshot artifact in layout")?;
            write_text(out, "shallow-info").map_err(|e| e.to_string())?;
            write_text(out, &format!("shallow {tip}")).map_err(|e| e.to_string())?;
            write_delim(out).map_err(|e| e.to_string())?;
            let plan = vec![stratum_store::manifest::StreamPart {
                key: snap.key.clone(),
                range: None,
                expect_bytes: snap.bytes,
            }];
            write_text(out, "packfile").map_err(|e| e.to_string())?;
            return stream_or_err(store, snap.entries, &plan, out);
        }
        // **Not on the precomputed path: serve the whole thing.**
        //
        // This used to `return Err(...)`, which the fronts turn into a
        // 500, and `fetch=shallow` is advertised unconditionally — so
        // `git clone --depth 1` failed outright for the entire early
        // life of every repository. The snapshot artifact is built by
        // ingest, and a repository created through the API has none
        // until a compaction has actually run; compaction no-ops below
        // `wal_entries` (8 by default), so a new repo with a handful of
        // commits has no artifact and never will until it grows. A
        // freshly created repository is exactly when somebody points CI
        // at it, and CI clones shallow.
        //
        // A full clone is a **correct superset** of a shallow one: the
        // client asked for a subset of history and is allowed to be
        // given more, and with no `shallow-info` section it simply ends
        // up unshallow. It is the same trade the non-tip-want branch
        // above already makes, in the same words — more bytes than the
        // client hoped for, and a clone that works. Erring here bought
        // nothing: nobody was reading the "(fallback)" classification on
        // this path, and what the user got was a 500.
        //
        // The precomputed answer above stays the fast path, and remains
        // the only one that reports the clone as shallow.
        let _ = depth;
        // A deepen that carries haves — `--unshallow` is the everyday
        // one — is a negotiation, and git waits for an `acknowledgments`
        // section before it will accept a pack. Answering `packfile`
        // straight away makes the client abort with "expected
        // 'acknowledgments'". Same shape as the non-tip-want branch
        // above: NAK, and the superset plan needs no further ACKs.
        if !done {
            write_text(out, "acknowledgments").map_err(|e| e.to_string())?;
            write_text(out, "NAK").map_err(|e| e.to_string())?;
            return write_flush(out).map_err(|e| e.to_string());
        }
        // The full plan carries every object this layout has, so every
        // graft the client is holding is now complete behind — except
        // any that are grafts of *ours*, which stay. Saying so is what
        // lets `--unshallow` finish: without an `unshallow` line git
        // keeps the boundary and the repository is still shallow after a
        // fetch that transferred the whole history.
        let lift: Vec<&String> = client_shallow
            .iter()
            .filter(|o| !manifest.shallow.contains(o))
            .collect();
        if !manifest.shallow.is_empty() || !lift.is_empty() {
            write_text(out, "shallow-info").map_err(|e| e.to_string())?;
            for oid in &manifest.shallow {
                write_text(out, &format!("shallow {oid}")).map_err(|e| e.to_string())?;
            }
            for oid in lift {
                write_text(out, &format!("unshallow {oid}")).map_err(|e| e.to_string())?;
            }
            write_delim(out).map_err(|e| e.to_string())?;
        }
        let (entries, plan) = (manifest.total_entries(), manifest.clone_plan());
        write_text(out, "packfile").map_err(|e| e.to_string())?;
        return stream_or_err(store, entries, &plan, out);
    }

    if haves.is_empty() {
        if !done {
            return Err("clone without done".into());
        }
        // CDN offload: hand the client a URI for the bulk and stream only
        // what the pack does not already carry. Only on a plain full clone
        // of a non-shallow layout — the shallow graft list and the CDN
        // pack's own boundary assumptions must not interact.
        if wants_packfile_uris && manifest.shallow.is_empty() {
            if let Some((entries, plan)) = cdn.and_then(|c| cdn_remainder(manifest, c)) {
                let c = cdn.expect("checked above");
                write_text(out, "packfile-uris").map_err(|e| e.to_string())?;
                write_text(out, &format!("{} {}", c.pack_hash, c.uri))
                    .map_err(|e| e.to_string())?;
                write_delim(out).map_err(|e| e.to_string())?;
                write_text(out, "packfile").map_err(|e| e.to_string())?;
                return stream_or_err(store, entries, &plan, out);
            }
        }
        // Shallow corpus: hand the client the graft list first, or
        // index-pack rejects boundary commits with absent parents.
        if !manifest.shallow.is_empty() {
            write_text(out, "shallow-info").map_err(|e| e.to_string())?;
            for oid in &manifest.shallow {
                write_text(out, &format!("shallow {oid}")).map_err(|e| e.to_string())?;
            }
            write_delim(out).map_err(|e| e.to_string())?;
        }
        let (entries, plan) = (manifest.total_entries(), manifest.clone_plan());
        write_text(out, "packfile").map_err(|e| e.to_string())?;
        return stream_or_err(store, entries, &plan, out);
    }

    // Incremental: the newest have on the spine pins the suffix.
    // WAL tips are the newest ACK-able states: a client at a pushed tip
    // needs exactly the later WAL entries.
    let wal_best = haves
        .iter()
        .filter_map(|h| manifest.find_wal_tip(h).map(|i| (i, h)))
        .max_by_key(|(i, _)| *i);
    if let Some((idx, have)) = wal_best {
        let (entries, plan) = manifest.wal_suffix_plan(idx);
        if !done {
            write_text(out, "acknowledgments").map_err(|e| e.to_string())?;
            write_text(out, &format!("ACK {have}")).map_err(|e| e.to_string())?;
            write_text(out, "ready").map_err(|e| e.to_string())?;
            write_delim(out).map_err(|e| e.to_string())?;
        }
        write_text(out, "packfile").map_err(|e| e.to_string())?;
        return stream_or_err(store, entries, &plan, out);
    }

    let best = haves
        .iter()
        .filter_map(|h| manifest.find_spine(h).map(|i| (i, h)))
        .max_by_key(|(i, _)| *i);
    let Some((idx, have)) = best else {
        if done {
            return Err("no spine commit among haves: bitmap-path miss (fallback)".into());
        }
        // Tell the client we have nothing in common yet; it will send more
        // haves (or give up and send done, landing in the arm above).
        write_text(out, "acknowledgments").map_err(|e| e.to_string())?;
        write_text(out, "NAK").map_err(|e| e.to_string())?;
        return write_flush(out).map_err(|e| e.to_string());
    };

    let (entries, plan) = manifest.suffix_plan(idx);
    if !done {
        write_text(out, "acknowledgments").map_err(|e| e.to_string())?;
        write_text(out, &format!("ACK {have}")).map_err(|e| e.to_string())?;
        write_text(out, "ready").map_err(|e| e.to_string())?;
        write_delim(out).map_err(|e| e.to_string())?;
    }
    write_text(out, "packfile").map_err(|e| e.to_string())?;
    stream_or_err(store, entries, &plan, out)
}

/// stream_pack plus the terminating flush; a failure after bytes have gone
/// out is reported to the client as a sideband channel-3 ERR message
/// (readable by stock git) instead of a truncated stream with an HTTP error
/// blob appended, and the response ends there — headers are already sent,
/// so returning an error upward would only garble the framing further.
/// What still has to be streamed inline when the client also fetches
/// `cdn` from the CDN. `None` = do not offload at all (serve the normal
/// full clone), which is the safe answer whenever we cannot *prove* what
/// the pack covers.
///
/// A missing pack aborts an opted-in clone outright — git excludes the
/// offloaded objects from the inline stream and has no fallback — so this
/// function refuses to guess: it offloads only in the two cases where the
/// remainder is exactly computable.
fn cdn_remainder(
    manifest: &Manifest,
    cdn: &CdnPack,
) -> Option<(u64, Vec<stratum_store::manifest::StreamPart>)> {
    // The layout must be untouched since the pack was built: the pack then
    // holds every object and nothing is left to stream. An empty packfile
    // (header + zero entries + trailer) is well-formed and is what git
    // expects when everything moved to a URI.
    //
    // Why nothing else qualifies — including the tempting case where the
    // pack's tip is still in the WAL and the later entries look like an
    // exact delta: WAL payloads are the pushers' **thin** packs, whose
    // delta bases live back in the layout, i.e. inside the CDN pack. git
    // indexes the inline packfile BEFORE downloading the packfile-uris,
    // so those bases are not present yet and the clone dies with "pack has
    // N unresolved deltas". It only appears to work when a push happens
    // to contain no deltas against older objects, which makes it a
    // data-dependent failure — the worst kind. A lagging pack is therefore
    // simply not advertised, and the clone is served inline as before;
    // the packer's job is to keep the pack current.
    if manifest.tip() == Some(cdn.tip.as_str()) && manifest.total_entries() == cdn.total_entries {
        return Some((0, Vec::new()));
    }
    None
}

fn stream_or_err(
    store: &ObjectStore,
    entries: u64,
    plan: &[stratum_store::manifest::StreamPart],
    out: &mut impl Write,
) -> Result<(), String> {
    if let Err(e) = stream_pack(store, entries, plan, out) {
        eprintln!("weft: pack stream failed: {e}");
        let mut msg = vec![3u8];
        msg.extend_from_slice(format!("weft: {e}\n").as_bytes());
        msg.truncate(pktline::MAX_DATA);
        let _ = write_data(out, &msg);
    }
    write_flush(out).map_err(|e| e.to_string())
}

/// concat(header, planned payload ranges…, sha1 trailer), each streamed from
/// object storage, all framed in sideband-64k channel 1.
fn stream_pack(
    store: &ObjectStore,
    entries: u64,
    plan: &[stratum_store::manifest::StreamPart],
    out: &mut impl Write,
) -> Result<(), String> {
    let mut hasher = Sha1::new();
    let mut band = SidebandWriter::new(out);

    let mut header = Vec::with_capacity(12);
    header.extend_from_slice(b"PACK");
    header.extend_from_slice(&2u32.to_be_bytes());
    let count = u32::try_from(entries).map_err(|_| "too many entries")?;
    header.extend_from_slice(&count.to_be_bytes());
    hasher.update(&header);
    band.write_all(&header).map_err(|e| e.to_string())?;

    let mut chunk = vec![0u8; 512 * 1024];
    for part in plan {
        let mut reader = store.get_stream(&part.key, part.range)?;
        let mut seen = 0u64;
        loop {
            let n = reader
                .read(&mut chunk)
                .map_err(|e| format!("{}: {e}", part.key))?;
            if n == 0 {
                break;
            }
            seen += n as u64;
            hasher.update(&chunk[..n]);
            band.write_all(&chunk[..n]).map_err(|e| e.to_string())?;
        }
        if seen != part.expect_bytes {
            return Err(format!(
                "{}: got {seen} bytes, expected {}",
                part.key, part.expect_bytes
            ));
        }
    }

    let digest = hasher.finalize();
    band.write_all(&digest).map_err(|e| e.to_string())?;
    band.flush_band().map_err(|e| e.to_string())
}

use std::io::Read;

/// Buffers bytes and emits them as sideband-64k channel-1 pkt-lines.
///
/// `pub(crate)` for `workspace`: a changeset's pack is framed exactly
/// like a repository's, and a second implementation of the framing is a
/// second place for the chunk size to be wrong.
pub(crate) struct SidebandWriter<'a, W: Write> {
    out: &'a mut W,
    buf: Vec<u8>,
}

impl<'a, W: Write> SidebandWriter<'a, W> {
    pub(crate) fn new(out: &'a mut W) -> Self {
        SidebandWriter {
            out,
            buf: Vec::with_capacity(SIDEBAND_CHUNK + 1),
        }
    }

    pub(crate) fn write_all(&mut self, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            let room = SIDEBAND_CHUNK - self.buf.len();
            let take = room.min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == SIDEBAND_CHUNK {
                self.emit()?;
            }
        }
        Ok(())
    }

    fn emit(&mut self) -> std::io::Result<()> {
        let mut pkt = Vec::with_capacity(self.buf.len() + 1);
        pkt.push(1u8); // channel 1: pack data
        pkt.extend_from_slice(&self.buf);
        write_data(self.out, &pkt)?;
        self.buf.clear();
        Ok(())
    }

    pub(crate) fn flush_band(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            self.emit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod cdn_tests {
    use super::*;
    use stratum_store::manifest::Manifest;

    fn pack(tip: &str, entries: u64) -> CdnPack {
        CdnPack {
            uri: "https://cdn.example/o/a/r/b/prod/cdn/t-h.pack".into(),
            pack_hash: "abc123".into(),
            tip: tip.into(),
            total_entries: entries,
        }
    }

    fn manifest_at(tip: &str) -> Manifest {
        Manifest {
            schema: 3,
            repo: "o/a/r/b".into(),
            layout: "prod".into(),
            object_format: "sha1".into(),
            refs: vec![("refs/heads/main".into(), tip.into())],
            head: "refs/heads/main".into(),
            segments: Vec::new(),
            cold_segments: Vec::new(),
            hot_segments: Vec::new(),
            spine: Vec::new(),
            locator: None,
            shallow: Vec::new(),
            epoch: "e".into(),
            extra_emission: None,
            tail_emissions: Vec::new(),
            ref_pages: Vec::new(),
            snapshot: None,
            wal: Vec::new(),
        }
    }

    fn text(buf: &[u8]) -> String {
        String::from_utf8_lossy(buf).to_string()
    }

    /// The capability is advertised only when there is a pack to hand out:
    /// claiming `packfile-uris` and then delivering none would cost an
    /// opted-in client a wasted round trip.
    #[test]
    fn capability_tracks_the_presence_of_a_pack() {
        let mut with = Vec::new();
        advertise_body(&mut with, Some(&pack("tip", 3))).unwrap();
        assert!(text(&with).contains("fetch=shallow packfile-uris"));

        let mut without = Vec::new();
        advertise_body(&mut without, None).unwrap();
        assert!(text(&without).contains("fetch=shallow"));
        assert!(
            !text(&without).contains("packfile-uris"),
            "must not advertise what we cannot deliver"
        );
        // Everything else is unchanged for existing clients.
        for cap in ["version 2", "ls-refs", "object-format=sha1"] {
            assert!(text(&without).contains(cap));
        }
    }

    /// A pack built at the current tip of an unchanged layout covers
    /// everything, so nothing is left to stream inline.
    #[test]
    fn unchanged_layout_leaves_no_remainder() {
        let m = manifest_at("tipoid");
        let entries = m.total_entries();
        let r = cdn_remainder(&m, &pack("tipoid", entries));
        assert_eq!(r.map(|(n, p)| (n, p.len())), Some((0, 0)));
    }

    /// The refusal cases — each one would otherwise risk advertising a
    /// pack whose remainder we cannot compute, which breaks the clone.
    #[test]
    fn unprovable_coverage_refuses_to_offload() {
        let m = manifest_at("tipoid");
        let entries = m.total_entries();

        // Tip matches but the layout moved (entries differ) — a non-HEAD
        // ref could have changed, so the pack is not provably complete.
        assert!(cdn_remainder(&m, &pack("tipoid", entries + 5)).is_none());
        // Tip is unknown to this layout (compaction folded the WAL, or the
        // pack predates a rewrite).
        assert!(cdn_remainder(&m, &pack("someothertip", entries)).is_none());
    }
}
