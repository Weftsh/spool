//! Read API (R3): file-at-ref, tree-at-ref, diff between refs, log with
//! pagination, refs listing. ETags are content-addressed (blob/commit
//! oids) so If-None-Match is exact, never heuristic.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::collections::HashMap;
use stratum_control::auth::Scope;
use stratum_engine::objwrite::{self, hex, OBJ_BLOB, OBJ_COMMIT, OBJ_TREE};
use stratum_engine::read::LayoutReader;
use stratum_engine::treediff;
use stratum_store::Manifest;

/// Run `f` with a LayoutReader for the repo, on the blocking pool.
pub(crate) async fn with_reader<T, F>(
    state: &SharedState,
    prefix: String,
    f: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&LayoutReader) -> Result<T, String> + Send + 'static,
{
    // The store and the cache are the process's, not the request's. The
    // manifest is the one thing read fresh every time: it is the ref
    // truth (I9) and the only mutable object in the layout.
    let store = state.store.clone();
    let cache = state.read_cache.clone();
    tokio::task::spawn_blocking(move || {
        let mbytes = store.get(&format!("{prefix}/manifest.json"))?;
        let manifest: Manifest =
            serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;
        // Same sha1-only guard as the wire path: never misread a future
        // object format's offsets.
        if manifest.object_format != "sha1" {
            return Err(format!(
                "object_format {:?} not supported by this build (sha1 only)",
                manifest.object_format
            ));
        }
        let reader = LayoutReader::with_cache(&store, &prefix, &manifest, cache)?;
        f(&reader)
    })
    .await
    .unwrap_or_else(|e| Err(format!("task join: {e}")))
}

fn auth_read(
    state: &SharedState,
    headers: &HeaderMap,
    org: &str,
    repo: &str,
) -> Result<String, Response> {
    let (_, repo, _) = crate::app::rest_repo_auth(state, headers, org, repo, Scope::RepoRead)?;
    Ok(repo.prefix().as_str().to_string())
}

pub(crate) fn err_to_response(e: String) -> Response {
    // A path that exists but is the other kind — a directory asked for
    // as a file, or a file asked for as a directory — is a 404, not a
    // 500. There is no tree at that path; the client asking is a browser
    // following a link, and nothing here has broken. This answered 500
    // until the manual pass followed a file link in a fresh tab: a deep
    // link has no listing behind it, so the client has to guess which of
    // the two a path is, and the wrong guess is the one everybody hits.
    use stratum_engine::errclass as ec;
    if ec::is_absent(&e)
        || ec::is_missing_path(&e)
        || ec::is_unknown_rev(&e)
        || ec::is_wrong_object_kind(&e)
    {
        json_error(StatusCode::NOT_FOUND, e)
    } else {
        internal(e)
    }
}

/// GET /files/*path?at=<rev> — raw content, ETag = blob oid.
pub async fn file(
    State(state): State<SharedState>,
    Path((org, repo, path)): Path<(String, String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let prefix = match auth_read(&state, &headers, &org, &repo) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let at = params.get("at").cloned().unwrap_or_else(|| "HEAD".into());
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let name = path.rsplit('/').next().unwrap_or_default().to_string();
    // One authorization and one reader for the whole request. This used
    // to authorize twice and open a second reader for the content —
    // about fourteen extra store GETs and a second database round trip
    // on every file view, for an answer that had not changed since the
    // first one. The 304 path still costs no bytes: the ETag is decided
    // before the blob is fetched, inside the same reader.
    let out = with_reader(&state, prefix, move |reader| {
        let commit = reader
            .resolve_rev(&at)?
            .ok_or_else(|| format!("unknown rev {at:?}"))?;
        let entry = reader
            .entry_at(&commit, &path)?
            .ok_or_else(|| format!("{path:?} not in this layout at {at:?}"))?;
        let blob_hex = hex(&entry.oid);
        if if_none_match.as_deref() == Some(format!("\"{blob_hex}\"").as_str()) {
            return Ok((commit, entry.mode, blob_hex, None));
        }
        let (k, data) = reader.object(&blob_hex)?;
        if k != OBJ_BLOB {
            return Err(format!("{blob_hex} is not a blob"));
        }
        Ok((commit, entry.mode, blob_hex, Some(data)))
    })
    .await;
    let (commit, mode, blob_hex, content) = match out {
        Ok(x) => x,
        Err(e) => return err_to_response(e),
    };
    let etag = format!("\"{blob_hex}\"");
    let Some(data) = content else {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    };
    let binary = looks_binary(&data);
    (
        StatusCode::OK,
        [
            (header::ETAG, etag),
            (
                header::CONTENT_TYPE,
                content_type(&name, binary).to_string(),
            ),
            (axum::http::HeaderName::from_static("x-weft-commit"), commit),
            (axum::http::HeaderName::from_static("x-weft-mode"), mode),
            (
                axum::http::HeaderName::from_static("x-weft-binary"),
                binary.to_string(),
            ),
        ],
        data,
    )
        .into_response()
}

/// Whether this content should be shown as text.
///
/// A NUL byte in the first 8 KiB, which is what `git` itself uses. It is
/// a heuristic and says so: the point is not to classify files perfectly
/// but to keep a browser from rendering a PNG as mojibake, and to keep
/// this server from promising `text/plain` for something that is not.
fn looks_binary(data: &[u8]) -> bool {
    data.iter().take(8192).any(|&b| b == 0)
}

/// A content type for a path, or `application/octet-stream`.
///
/// Deliberately short. Everything textual is served as `text/plain` with
/// an explicit charset rather than its "real" type: this endpoint
/// returns whatever somebody committed, and answering `text/html` for a
/// file called `index.html` would let a repository serve script from
/// this origin. The extension list exists to spare the *client* a
/// guess, not to be complete.
fn content_type(name: &str, binary: bool) -> &'static str {
    if binary {
        // Images are named because a browser can show them and cannot
        // be tricked by them; nothing else executable is listed.
        return match name
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "ico" => "image/x-icon",
            "pdf" => "application/pdf",
            _ => "application/octet-stream",
        };
    }
    "text/plain; charset=utf-8"
}

/// GET /tree/*path?at=<rev> (and /tree for the root).
/// The commit that last touched each entry in a directory.
///
/// This is the column that makes a file list worth reading. A name and a
/// size tell you a file exists; "who last changed this, and when" tells
/// you where the project is moving, and it is the single thing people
/// name when they say GitHub's file browser is better than everyone
/// else's.
///
/// It is answered here rather than by the browser because the client
/// alternative is one path-filtered log per row — a directory of forty
/// files becoming forty requests, every one of them walking history from
/// the same commit. Once, server-side, sharing the walk, is the only
/// shape of this that is not wasteful.
///
/// **Bounded, and honest when the bound is hit.** It walks at most
/// `cap` commits and stops early once every entry is accounted for. A
/// directory whose entries were all last touched in antiquity leaves
/// some unattributed rather than walking the whole history to find out,
/// and those render with no message rather than a wrong one — the
/// alternative is an unbounded walk on a request path, which I13 rules
/// out for exactly this kind of "usually cheap" work.
///
/// Merges are diffed against their first parent, which is what makes
/// "last touched" mean what a reader expects: the change as it arrived
/// on this branch, not the same work attributed twice from a side.
/// How long one listing may spend walking history for its last-commit
/// column before answering with what it has.
///
/// The walk reads every commit's trees from object storage, and on a
/// mirror of a real project — thousands of commits, dozens of root
/// entries — that ran past CloudFront's sixty-second origin timeout: the
/// code browser sat on "Loading…" and the person never saw a listing at
/// all, because the listing was waiting for the column. The deadline
/// bounds the request; entries the walk did not reach in time are `null`
/// and the response says so with `history_truncated`, so a client can
/// tell "nothing touched it in the window" from "we ran out of time".
///
/// Configurable because the truncation is otherwise reachable only with
/// a history too deep to build in a test. `0` walks nothing: every
/// listing asked for history answers truncated at once, which is the
/// deterministic case the suite uses and a legitimate switch for an
/// operator whose object store cannot afford the column at all.
fn tree_history_budget() -> std::time::Duration {
    let ms = std::env::var("STRATUM_TREE_HISTORY_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2_500);
    std::time::Duration::from_millis(ms)
}

/// How many paths one recursive listing may name before it stops.
///
/// `?recursive=1` walks every tree under the requested directory, and
/// the number of paths under a directory is not something the server
/// controls: a hostile push can make one tree arbitrarily wide or
/// arbitrarily deep, and a legitimate monorepo is wide enough on its
/// own. I13 says every walk on a request path is bounded, so this one
/// stops at the cap and says so with `truncated: true` rather than
/// holding a connection open while it names a million files.
///
/// Configurable because the truncation is otherwise reachable only with
/// a tree too large to build in a test; a limit nothing ever exercises
/// is a limit nobody knows still works. The default is generous enough
/// that a tree component sees a real project whole.
fn tree_recursive_cap() -> usize {
    std::env::var("STRATUM_TREE_RECURSIVE_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(50_000)
}

/// Every path under `root`, in pre-order — a directory's line comes
/// before its children's — and whether the walk stopped at `cap`.
///
/// Paths are relative to `root`; a directory ends in `/`, anything else
/// (a blob, a symlink, a gitlink) is a plain leaf. Pre-order in git's own
/// tree order is what lets a client hand the list to a tree component
/// as it is, without sorting or re-deriving parents.
///
/// `treediff::diff_commits(reader, None, to)` would also enumerate the
/// tree, and was not reused: it emits blobs only, never a directory
/// line, and it is unbounded — it exists to diff two commits, which has
/// to see everything. This reads one object per tree and nothing per
/// blob, and stops the moment the output reaches the cap.
fn walk_paths(
    reader: &LayoutReader,
    root: &str,
    cap: usize,
) -> Result<(Vec<String>, bool), String> {
    let mut out = Vec::new();
    let truncated = walk_tree(reader, root, "", cap, &mut out)?;
    Ok((out, truncated))
}

/// One level of `walk_paths`. Answers `true` when the output reached
/// the cap, so every caller up the stack stops too rather than naming
/// the rest of its own directory after a truncated child.
fn walk_tree(
    reader: &LayoutReader,
    oid: &str,
    prefix: &str,
    cap: usize,
    out: &mut Vec<String>,
) -> Result<bool, String> {
    let (_, data) = reader.object(oid)?;
    for e in objwrite::parse_tree(&data)? {
        // Checked before each push, not after: the cap bounds what is
        // emitted, and one past it is one the client would have to
        // strip.
        if out.len() >= cap {
            return Ok(true);
        }
        let is_tree = e.mode == "40000" || e.mode == "040000";
        let path = if is_tree {
            format!("{prefix}{}/", e.name)
        } else {
            format!("{prefix}{}", e.name)
        };
        out.push(path.clone());
        if is_tree && walk_tree(reader, &hex(&e.oid), &path, cap, out)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The walk's answer: what last touched each entry it reached, and
/// whether it stopped short on the clock.
struct EntryHistory {
    last: std::collections::HashMap<String, serde_json::Value>,
    truncated: bool,
}

fn last_commit_per_entry(
    reader: &stratum_engine::read::LayoutReader,
    head: &str,
    dir: &str,
    names: &[String],
    cap: usize,
    budget: std::time::Duration,
) -> EntryHistory {
    use std::collections::{HashMap, HashSet};
    let mut want: HashSet<&str> = names.iter().map(String::as_str).collect();
    let mut out: HashMap<String, serde_json::Value> = HashMap::new();
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{}/", dir.trim_end_matches('/'))
    };

    // The budget is enforced by the reader, per store read, not by this
    // loop per commit. The loop used to check its clock between commits,
    // which bounded nothing: one commit's tree diff is dozens of object
    // reads, each a store round trip, and the first diff on a real
    // mirror ran for three minutes past a 2.5 s budget before the loop
    // got to look at the clock. A 504 from the edge was the result, and
    // the last-commit column never filled. Reads the cache can answer
    // are not charged — a walk over a warm cache finishes in memory.
    let deadline = std::time::Instant::now() + budget;
    reader.set_deadline(Some(deadline));
    let mut at = head.to_string();
    let mut truncated = false;
    let out_of_time = |e: &String| stratum_engine::read::is_read_budget_exhausted(e);
    for _ in 0..cap {
        if want.is_empty() {
            break;
        }
        let (_, data) = match reader.object(&at) {
            Ok(x) => x,
            Err(e) => {
                truncated = out_of_time(&e);
                break;
            }
        };
        let Ok(commit) = stratum_engine::objwrite::parse_commit(&data) else {
            break;
        };
        let parent = commit.parents.first().cloned();
        let changes = match stratum_engine::treediff::diff_commits(reader, parent.as_deref(), &at) {
            Ok(c) => c,
            Err(e) => {
                truncated = out_of_time(&e);
                break;
            }
        };
        for change in changes {
            let Some(rest) = change.path.strip_prefix(prefix.as_str()) else {
                continue;
            };
            // The entry *in this directory* that the change lands under:
            // a change to `src/net/tcp.rs` is what last touched `src/`.
            let name = rest.split('/').next().unwrap_or_default();
            if !want.remove(name) {
                continue;
            }
            out.insert(
                name.to_string(),
                serde_json::json!({
                    "commit": at,
                    // The subject only. A body belongs on the commit's
                    // own page, not wrapped into a table cell.
                    "message": commit.message.lines().next().unwrap_or_default(),
                    // The raw identity line, as `/log` returns it — the
                    // client already parses this shape, and inventing a
                    // second one here would mean two parsers to keep in
                    // step for one fact.
                    "author": commit.author,
                }),
            );
        }
        match parent {
            Some(p) => at = p,
            None => break,
        }
    }
    // The clock was the walk's; the listing after it reads on its own
    // terms.
    reader.set_deadline(None);
    EntryHistory {
        last: out,
        truncated,
    }
}

pub async fn tree(
    State(state): State<SharedState>,
    Path((org, repo, path)): Path<(String, String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    tree_inner(state, org, repo, path, params, headers).await
}

pub async fn tree_root(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    tree_inner(state, org, repo, String::new(), params, headers).await
}

async fn tree_inner(
    state: SharedState,
    org: String,
    repo: String,
    path: String,
    params: HashMap<String, String>,
    headers: HeaderMap,
) -> Response {
    let prefix = match auth_read(&state, &headers, &org, &repo) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let at = params.get("at").cloned().unwrap_or_else(|| "HEAD".into());
    let want_history = params.get("history").map(String::as_str) == Some("1");
    let want_recursive = params.get("recursive").map(String::as_str) == Some("1");
    let want_sizes = params.get("sizes").map(String::as_str) == Some("1");
    let out = with_reader(&state, prefix, move |reader| {
        let commit = reader
            .resolve_rev(&at)?
            .ok_or_else(|| format!("unknown rev {at:?}"))?;
        let entry = reader
            .entry_at(&commit, &path)?
            .ok_or_else(|| format!("{path:?} not in this layout at {at:?}"))?;
        let (k, data) = reader.object(&hex(&entry.oid))?;
        if k != OBJ_TREE {
            return Err(format!("{path:?} is not a tree"));
        }
        // `?recursive=1` answers a different shape: every path under the
        // directory, flat, `{ commit, paths, truncated }` — no entries,
        // no sizes, no modes. It is what a tree component needs to draw
        // a whole project at once instead of one request per fold.
        // `history=1` is ignored here, so there is no `history_truncated`
        // key: a last-commit column over fifty thousand paths is not a
        // walk anybody asked for, and the flat shape has no row to put
        // it on.
        if want_recursive {
            let (paths, truncated) = walk_paths(reader, &hex(&entry.oid), tree_recursive_cap())?;
            return Ok(serde_json::json!({
                "commit": commit,
                "paths": paths,
                "truncated": truncated,
            }));
        }
        let parsed = objwrite::parse_tree(&data)?;
        // Opt-in, because it is a history walk and the plain listing
        // must stay as cheap as it always was. The code browser asks for
        // it; anything scripting the API does not pay for it by default.
        let history = if want_history {
            let names: Vec<String> = parsed.iter().map(|e| e.name.clone()).collect();
            last_commit_per_entry(reader, &commit, &path, &names, 500, tree_history_budget())
        } else {
            EntryHistory {
                last: std::collections::HashMap::new(),
                truncated: false,
            }
        };
        let entries: Vec<serde_json::Value> = parsed
            .into_iter()
            .map(|e| {
                let is_tree = e.mode == "40000" || e.mode == "040000";
                // Sizes only on request. Learning a blob's length means
                // reading the blob, and a listing used to read every one
                // in the directory: on the production mirror that was
                // thirty sequential object reads — each a locator lookup
                // and a segment range from S3 — for a column the file
                // browser has since dropped, the way GitHub's has none.
                // The file page still says how big a file is, from the
                // one blob it reads anyway. Scripts that want the column
                // ask with `?sizes=1` and pay for it knowingly. Only
                // blobs are measured even then: a tree's "size" would be
                // the size of its listing, which is not what anybody
                // reading the column would assume.
                let size = if is_tree || !want_sizes {
                    None
                } else {
                    reader
                        .object(&hex(&e.oid))
                        .ok()
                        .map(|(_, d)| d.len() as u64)
                };
                serde_json::json!({
                    "name": e.name,
                    "mode": e.mode,
                    "kind": if is_tree { "tree" } else { "blob" },
                    "oid": hex(&e.oid),
                    "size": size,
                    "last_commit": history.last.get(&e.name),
                })
            })
            .collect();
        // `history_truncated` only when history was asked for: a plain
        // listing has nothing to be truncated.
        let mut body = serde_json::json!({ "commit": commit, "entries": entries });
        if want_history {
            body["history_truncated"] = serde_json::Value::Bool(history.truncated);
        }
        Ok(body)
    })
    .await;
    match out {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_to_response(e),
    }
}

/// What one commit did to one path.
///
/// Only meaningful for a path-filtered log; an unfiltered walk says
/// nothing about any particular file.
fn change_kind(before: Option<&[u8]>, after: Option<&[u8]>) -> Option<&'static str> {
    match (before, after) {
        (None, Some(_)) => Some("added"),
        (Some(_), None) => Some("deleted"),
        (Some(a), Some(b)) if a != b => Some("modified"),
        // Unchanged, or absent on both sides: this commit did not touch
        // the path, so it does not belong in its history.
        _ => None,
    }
}

/// How many commits one request will examine when filtering by path.
///
/// A file touched once at the start of a long history would otherwise
/// walk the whole thing to answer a single page. The walk stops here and
/// hands back a cursor instead, so the client asks again rather than the
/// server holding a connection open for an unbounded scan.
///
/// Configurable because otherwise the truncation is only reachable with
/// a history five hundred commits deep, and a limit nothing ever exercises
/// is a limit nobody knows still works. A test spawns a server with a
/// budget of one and asks for a file changed more often than that.
/// Floored at 1: a budget of zero would examine nothing and return a
/// cursor that never advances, which is an infinite loop in the client.
fn path_scan_budget() -> usize {
    std::env::var("STRATUM_LOG_SCAN_BUDGET")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(500)
        .max(1)
}

/// GET /log?rev=&limit=&after=&path= — first-parent walk, cursor = last commit.
///
/// With `path`, only commits that changed that path are returned, each
/// carrying what it did to it. This is what a file view needs to show
/// "who last touched this, and when" without asking for the whole
/// history and filtering in the browser.
pub async fn log(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let prefix = match auth_read(&state, &headers, &org, &repo) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let rev = params.get("rev").cloned().unwrap_or_else(|| "HEAD".into());
    let after = params.get("after").cloned();
    let path = params
        .get("path")
        .map(|p| p.trim_matches('/').to_string())
        .filter(|p| !p.is_empty());
    let limit: usize = params
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(50)
        .clamp(1, 500);
    let out = with_reader(&state, prefix, move |reader| {
        let mut cur = match &after {
            Some(a) => {
                // Cursor: continue from the first parent of `after`.
                let (k, data) = reader.object(a)?;
                if k != OBJ_COMMIT {
                    return Err(format!("{a} is not a commit"));
                }
                objwrite::parse_commit(&data)?.parents.first().cloned()
            }
            None => reader.resolve_rev(&rev)?,
        };
        // What the path pointed at in the commit *after* the one being
        // examined — walking newest-first, so this is the "after" side
        // of each comparison. Carried down the walk so each commit costs
        // one tree lookup rather than two.
        let mut newer: Option<Vec<u8>> = match &path {
            Some(p) => cur
                .as_ref()
                .and_then(|c| reader.entry_at(c, p).transpose())
                .transpose()?
                .map(|e| e.oid.to_vec()),
            None => None,
        };
        let mut entries = Vec::new();
        // The last commit actually examined, which is where a cursor has
        // to resume from. With a path filter that is not the last commit
        // *returned* — everything between them was examined and rejected,
        // and resuming from a returned commit would walk it all again.
        let mut last_seen: Option<String> = None;
        let mut scanned = 0usize;
        let budget = path_scan_budget();
        while let Some(oid) = cur.clone() {
            if entries.len() >= limit {
                break;
            }
            if path.is_some() && scanned >= budget {
                break;
            }
            scanned += 1;
            let (k, data) = reader.object(&oid)?;
            if k != OBJ_COMMIT {
                return Err(format!("{oid} is not a commit"));
            }
            let c = objwrite::parse_commit(&data)?;
            let parent = c.parents.first().cloned();
            let mut entry = serde_json::json!({
                "commit": oid,
                "tree": c.tree,
                "parents": c.parents,
                "author": c.author,
                "committer": c.committer,
                "message": c.message,
            });
            let keep = match &path {
                None => true,
                Some(p) => {
                    let older = match &parent {
                        Some(par) => reader.entry_at(par, p)?.map(|e| e.oid.to_vec()),
                        // No parent: whatever the path is in the root
                        // commit was added by it.
                        None => None,
                    };
                    let kind = change_kind(older.as_deref(), newer.as_deref());
                    newer = older;
                    match kind {
                        Some(k) => {
                            entry["change"] = serde_json::json!(k);
                            true
                        }
                        None => false,
                    }
                }
            };
            if keep {
                entries.push(entry);
            }
            last_seen = Some(oid);
            cur = parent;
        }
        // More to walk if the page filled, or if a filtered scan ran out
        // of budget with history still to go.
        let more = cur.is_some() && (entries.len() == limit || path.is_some());
        let next = last_seen.filter(|_| more);
        Ok(serde_json::json!({ "entries": entries, "next_after": next }))
    })
    .await;
    match out {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_to_response(e),
    }
}

/// GET /refs — branches and tags.
pub async fn refs(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let prefix = match auth_read(&state, &headers, &org, &repo) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let out = with_reader(&state, prefix, move |reader| {
        let refs = reader.all_refs()?;
        Ok(serde_json::json!({
            "head": reader.manifest.head,
            "refs": refs
                .into_iter()
                .map(|(n, o)| serde_json::json!({ "name": n, "oid": o }))
                .collect::<Vec<_>>(),
        }))
    })
    .await;
    match out {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_to_response(e),
    }
}

/// GET /branches and GET /tags.
///
/// `/refs` returns everything, unpaginated, which is fine for a client
/// that wants everything and wrong for a ref switcher that wants one
/// kind. Both paths were write-only until now — you could create a
/// branch and never list one.
pub async fn branches(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    refs_of_kind(state, org, repo, headers, "refs/heads/", "branches").await
}

pub async fn tags(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    refs_of_kind(state, org, repo, headers, "refs/tags/", "tags").await
}

async fn refs_of_kind(
    state: SharedState,
    org: String,
    repo: String,
    headers: HeaderMap,
    prefix_str: &'static str,
    field: &'static str,
) -> Response {
    let prefix = match auth_read(&state, &headers, &org, &repo) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let out = with_reader(&state, prefix, move |reader| {
        let head = reader.manifest.head.clone();
        let mut names: Vec<serde_json::Value> = reader
            .all_refs()?
            .into_iter()
            .filter_map(|(n, oid)| {
                let short = n.strip_prefix(prefix_str)?;
                Some(serde_json::json!({
                    "name": short,
                    "full": n,
                    "oid": oid,
                    "default": n == head,
                }))
            })
            .collect();
        // Sorted, because a ref switcher in store order is a ref
        // switcher nobody can find anything in.
        names.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(serde_json::json!({ field: names, "head": head }))
    })
    .await;
    match out {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_to_response(e),
    }
}

/// The recursive tree diff between two commits, in the one shape every
/// diff client in this product already parses:
/// `{ from, to, changes: [{status, path, old_oid, new_oid, …}] }`.
///
/// Factored out of [`diff`] so a surface with its own idea of what two
/// revisions *are* — the interdiff between two patchsets, which resolves
/// numbers to commit oids through the control plane rather than through
/// refs — cannot grow a second, subtly different entry shape. A client
/// that renders `/diff` renders that one with no new code, and there is
/// no patch format here to keep in step: hunks are assembled client-side
/// from `…/files/*path`, which is what keeps the server out of the
/// business of having an opinion about whitespace.
pub(crate) fn diff_entries(
    reader: &LayoutReader,
    from: &str,
    to: &str,
) -> Result<serde_json::Value, String> {
    let changes: Vec<serde_json::Value> = treediff::diff_commits(reader, Some(from), to)?
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "status": c.status.as_str(),
                "path": c.path,
                "old_oid": c.old_oid,
                "new_oid": c.new_oid,
                "old_mode": c.old_mode,
                "new_mode": c.new_mode,
            })
        })
        .collect();
    Ok(serde_json::json!({ "from": from, "to": to, "changes": changes }))
}

/// The largest file this server will count the lines of.
///
/// The same 512 KiB the dashboard's `api.ts` refuses to fetch as text
/// for the diff view, and deliberately the same number: a file the
/// reader is never shown a diff of is a file whose `+N −M` would be a
/// claim about something nobody can check. Beyond it the member is
/// reported `truncated` instead.
const MAX_DIFF_FILE_BYTES: usize = 512 * 1024;

/// How much line-matching work one diffstat request may do.
///
/// A bound on a request path, for the reason I13 puts one on every walk:
/// the cost of matching two files is a function of how *different* they
/// are, so a wholesale rewrite of a large file is the case that runs
/// long, and it is also the case a hostile push can arrange on purpose.
/// Running out is `truncated: true` — never a guess.
const MAX_DIFF_WORK: u64 = 2_000_000;

/// What a diff adds up to: the numbers `+412 −77` is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiffStat {
    /// Every path the tree diff touched. Exact even when the counts are
    /// not: it comes from the tree walk, which never declines a path.
    pub files: usize,
    pub insertions: u64,
    pub deletions: u64,
    /// At least one file was **not** counted — too large, not text, not
    /// a blob, or past the work bound — so `insertions` and `deletions`
    /// are the total over the files that were.
    ///
    /// Said out loud rather than smuggled into the numbers. A count that
    /// silently omits the one enormous file in the change is wrong in
    /// the direction nobody checks, and a number invented for a file we
    /// declined to read is worse than an absent one.
    pub truncated: bool,
}

/// A file's lines, or the reason there are none to count.
enum Side {
    /// The file is not on this side of the diff at all.
    Absent,
    Lines(Vec<u8>),
    /// Countable lines are not what this is: too large, binary, or a
    /// submodule pointer, which is a commit oid rather than content.
    Uncountable,
}

/// Split content into lines the way a diff counts them: a trailing
/// newline ends the last line rather than starting an empty one, and
/// empty content is no lines at all.
fn lines(data: &[u8]) -> Vec<&[u8]> {
    if data.is_empty() {
        return Vec::new();
    }
    let body = data.strip_suffix(b"\n").unwrap_or(data);
    body.split(|&b| b == b'\n').collect()
}

/// One side of one file, fetched and judged.
fn side(reader: &LayoutReader, oid: Option<&str>, mode: Option<&str>) -> Result<Side, String> {
    let Some(oid) = oid else {
        return Ok(Side::Absent);
    };
    // Checked before the read rather than after it: a gitlink's oid names
    // a commit in *another* repository, so reading it here would fail
    // with an error about a missing object, and a genuine store failure
    // would then be indistinguishable from an ordinary submodule bump.
    if mode == Some("160000") {
        return Ok(Side::Uncountable);
    }
    let (kind, data) = reader.object(oid)?;
    if kind != OBJ_BLOB || data.len() > MAX_DIFF_FILE_BYTES || looks_binary(&data) {
        return Ok(Side::Uncountable);
    }
    Ok(Side::Lines(data))
}

/// How many lines went in and how many came out, between two files.
///
/// Myers' greedy algorithm, run **forward only and with no trace kept**.
/// The `d` at which the two ends meet *is* the length of the shortest
/// edit script, and the split between insertions and deletions follows
/// from arithmetic rather than from walking that script: every edit is
/// one line in or one line out, so `ins + del == d` and
/// `ins - del == new_len - old_len`. That is the whole reason there is no
/// O(D²) trace and no backtracking here — the two numbers on the screen
/// never needed the script, only its length.
///
/// `Err(())` when `budget` runs out, and the caller reports `truncated`.
/// There is no cheap approximation worth substituting: a plausible wrong
/// `+412 −77` is worse than no number, because nothing about it looks
/// wrong.
fn edit_distance(a: &[&[u8]], b: &[&[u8]], budget: &mut u64) -> Result<usize, ()> {
    // Trim the ends they agree on first. A patchset usually touches a
    // handful of lines in a long file, and everything below is measured
    // on what is left after this — so the ordinary case costs almost
    // nothing and the bound is spent only on real rewrites.
    let mut lo = 0;
    while lo < a.len() && lo < b.len() && a[lo] == b[lo] {
        lo += 1;
    }
    let mut hi = 0;
    while hi < a.len() - lo && hi < b.len() - lo && a[a.len() - 1 - hi] == b[b.len() - 1 - hi] {
        hi += 1;
    }
    let a = &a[lo..a.len() - hi];
    let b = &b[lo..b.len() - hi];
    // One side empty is the answer outright: every remaining line of the
    // other is an insertion, or a deletion. Said explicitly because the
    // loop below would reach it only after `d` rounds of `O(d)` work,
    // which is quadratic in a file that was wholly added.
    if a.is_empty() || b.is_empty() {
        let d = a.len() + b.len();
        *budget = budget.saturating_sub(d as u64);
        return Ok(d);
    }
    let (n, m) = (a.len() as isize, b.len() as isize);
    // `d` can never exceed n + m, and it can never exceed what the budget
    // will pay for — so the furthest-reaching-path array is sized by the
    // smaller of the two, which is what keeps a large file from
    // allocating megabytes on its way to being refused.
    let dmax = ((n + m) as u64).min(*budget) as isize;
    let off = dmax + 1;
    let mut v = vec![0isize; (2 * dmax + 3) as usize];
    for d in 0..=dmax {
        let mut k = -d;
        while k <= d {
            if *budget == 0 {
                return Err(());
            }
            *budget -= 1;
            let mut x =
                if k == -d || (k != d && v[(k - 1 + off) as usize] < v[(k + 1 + off) as usize]) {
                    v[(k + 1 + off) as usize]
                } else {
                    v[(k - 1 + off) as usize] + 1
                };
            let mut y = x - k;
            let from = x;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            // The snake is work too, and charging for it is what keeps
            // the bound a bound: without it a long run of matching lines
            // is free and the budget stops describing the time spent.
            *budget = budget.saturating_sub((x - from) as u64);
            v[(k + off) as usize] = x;
            if x >= n && y >= m {
                return Ok(d as usize);
            }
            k += 2;
        }
    }
    Err(())
}

/// The diffstat between two commits, over the same tree walk
/// [`diff_entries`] reports path by path.
///
/// `from` is `None` for a root commit, exactly as
/// [`treediff::diff_commits`] means it.
pub(crate) fn diffstat(
    reader: &LayoutReader,
    from: Option<&str>,
    to: &str,
) -> Result<DiffStat, String> {
    let entries = treediff::diff_commits(reader, from, to)?;
    let mut stat = DiffStat {
        files: entries.len(),
        insertions: 0,
        deletions: 0,
        truncated: false,
    };
    let mut budget = MAX_DIFF_WORK;
    for e in &entries {
        let old = side(reader, e.old_oid.as_deref(), e.old_mode.as_deref())?;
        let new = side(reader, e.new_oid.as_deref(), e.new_mode.as_deref())?;
        let (Some(old), Some(new)) = (countable(&old), countable(&new)) else {
            stat.truncated = true;
            continue;
        };
        let (a, b) = (lines(old), lines(new));
        let Ok(d) = edit_distance(&a, &b, &mut budget) else {
            // The bound is spent, and every file after this one would
            // report the same thing. Stopping here rather than reading
            // them is the point of having a bound at all; `files` is
            // already exact, because it came from the tree walk.
            stat.truncated = true;
            break;
        };
        let (insertions, deletions) = split_edits(d, a.len(), b.len());
        stat.insertions += insertions;
        stat.deletions += deletions;
    }
    Ok(stat)
}

/// Split one edit-script length into insertions and deletions.
///
/// Every edit is one line in or one line out, so `ins + del == d` and
/// `ins - del == new - old`; both numbers follow from those two facts and
/// nothing else has to be walked.
///
/// A function rather than three lines inside [`diffstat`] because it is
/// the arithmetic the whole approach rests on, and a test that restated
/// it would agree with a bug in it — as one here did, until the sign was
/// flipped on purpose and the unit test went on passing.
fn split_edits(d: usize, old_lines: usize, new_lines: usize) -> (u64, u64) {
    let delta = new_lines as i64 - old_lines as i64;
    let insertions = (d as i64 + delta) / 2;
    (insertions as u64, d as u64 - insertions as u64)
}

/// The bytes of a side that can be counted: absent is empty content,
/// uncountable is no content at all.
fn countable(s: &Side) -> Option<&[u8]> {
    match s {
        Side::Absent => Some(&[]),
        Side::Lines(d) => Some(d),
        Side::Uncountable => None,
    }
}

/// GET /diff?from=<rev>&to=<rev> — recursive tree diff, no renames (v1).
pub async fn diff(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let prefix = match auth_read(&state, &headers, &org, &repo) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let (Some(from), Some(to)) = (params.get("from").cloned(), params.get("to").cloned()) else {
        return json_error(StatusCode::BAD_REQUEST, "from and to are required");
    };
    let out = with_reader(&state, prefix, move |reader| {
        let a = reader
            .resolve_rev(&from)?
            .ok_or_else(|| format!("unknown rev {from:?}"))?;
        let b = reader
            .resolve_rev(&to)?
            .ok_or_else(|| format!("unknown rev {to:?}"))?;
        diff_entries(reader, &a, &b)
    })
    .await;
    match out {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_to_response(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How a diff counts lines: a trailing newline ends the last one
    /// rather than starting an empty one, and empty content is no lines.
    ///
    /// Off by one here is off by one in every `+N` the product renders,
    /// and nothing downstream could notice.
    #[test]
    fn a_trailing_newline_ends_a_line_it_does_not_start_one() {
        assert_eq!(lines(b"").len(), 0);
        assert_eq!(lines(b"a\nb\n"), vec![&b"a"[..], &b"b"[..]]);
        assert_eq!(lines(b"a\nb"), vec![&b"a"[..], &b"b"[..]]);
        // A file that is one empty line is one line, and a file of two
        // newlines is two empty ones.
        assert_eq!(lines(b"\n"), vec![&b""[..]]);
        assert_eq!(lines(b"\n\n"), vec![&b""[..], &b""[..]]);
    }

    /// Two files through exactly the path [`diffstat`] puts them
    /// through: the same line split, the same distance, the same
    /// arithmetic. Calling [`split_edits`] rather than restating it is
    /// the point — an earlier version of this helper had its own copy of
    /// the formula, and flipping the sign in the product left it green.
    fn split(old: &str, new: &str) -> (u64, u64) {
        let (a, b) = (old.as_bytes().to_vec(), new.as_bytes().to_vec());
        let (a, b) = (lines(&a), lines(&b));
        let mut budget = MAX_DIFF_WORK;
        let d = edit_distance(&a, &b, &mut budget).expect("within the bound");
        split_edits(d, a.len(), b.len())
    }

    /// The whole reason no edit script is walked: `d` plus the length
    /// difference is enough to recover both numbers.
    ///
    /// Every shape a diffstat meets is here, because a wrong split is
    /// the failure nobody can see — `+3 −1` and `+1 −3` are both
    /// plausible next to a file nobody opens.
    #[test]
    fn insertions_and_deletions_come_out_of_the_edit_distance() {
        assert_eq!(split("", ""), (0, 0), "nothing changed");
        assert_eq!(split("", "a\nb\nc\n"), (3, 0), "a file added whole");
        assert_eq!(split("a\nb\nc\n", ""), (0, 3), "a file deleted whole");
        assert_eq!(split("a\nb\nc\n", "a\nb\nc\n"), (0, 0), "identical");
        assert_eq!(
            split("a\nb\nc\n", "a\nB\nc\n"),
            (1, 1),
            "one line replaced is one in and one out"
        );
        assert_eq!(
            split("a\nc\n", "a\nb\nc\n"),
            (1, 0),
            "a line inserted in the middle"
        );
        assert_eq!(
            split("a\nb\nc\n", "a\nc\n"),
            (0, 1),
            "a line removed from the middle"
        );
        assert_eq!(
            split("a\nb\nc\nd\n", "d\nc\nb\na\n"),
            (3, 3),
            "a reversal is not four-and-four: the middle still matches"
        );
        // The common ends are trimmed before any matching happens, so a
        // one-line edit in a long file costs almost nothing — and still
        // counts as one line each way.
        let long_a: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let long_b = long_a.replace("line 250\n", "LINE 250\n");
        assert_eq!(split(&long_a, &long_b), (1, 1));
    }

    /// The bound is real, and running out is refused rather than
    /// approximated.
    ///
    /// A plausible wrong `+412 −77` is worse than no number, because
    /// nothing about it looks wrong — so the budget's only two outcomes
    /// are an exact answer and `truncated`.
    #[test]
    fn a_rewrite_past_the_work_bound_is_refused_not_estimated() {
        let a: Vec<Vec<u8>> = (0..400).map(|i| format!("a{i}").into_bytes()).collect();
        let b: Vec<Vec<u8>> = (0..400).map(|i| format!("b{i}").into_bytes()).collect();
        let a: Vec<&[u8]> = a.iter().map(Vec::as_slice).collect();
        let b: Vec<&[u8]> = b.iter().map(Vec::as_slice).collect();
        let mut tight = 100;
        assert!(
            edit_distance(&a, &b, &mut tight).is_err(),
            "a wholesale rewrite must not fit in a hundred steps"
        );
        // The same pair with room answers exactly: nothing matches, so
        // every line goes out and every line comes in.
        let mut room = MAX_DIFF_WORK;
        assert_eq!(edit_distance(&a, &b, &mut room).unwrap(), 800);
        assert!(room < MAX_DIFF_WORK, "the work was charged for");
    }

    /// What a side has to be to be counted, and what it costs when it is
    /// not: `truncated`, never a number.
    #[test]
    fn only_a_text_blob_within_the_limit_is_countable() {
        assert_eq!(countable(&Side::Absent), Some(&[][..]));
        assert_eq!(countable(&Side::Lines(b"a\n".to_vec())), Some(&b"a\n"[..]));
        assert_eq!(countable(&Side::Uncountable), None);
    }

    /// The binary heuristic is git's: a NUL in the first 8 KiB. It is a
    /// heuristic and the point is narrow — keep a browser from
    /// rendering a PNG as mojibake, and keep this server from promising
    /// `text/plain` for something that is not.
    #[test]
    fn binary_is_a_nul_in_the_first_eight_kilobytes() {
        assert!(!looks_binary(b""));
        assert!(!looks_binary(b"# hello\nsecond line\n"));
        // UTF-8 text with high bytes is still text.
        assert!(!looks_binary("héllo — ok".as_bytes()));
        assert!(looks_binary(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"));
        assert!(looks_binary(b"a\0b"));

        // Past the window it is not looked at, which is what keeps this
        // O(1) on a large file.
        let mut late = vec![b'a'; 8192];
        late.push(0);
        assert!(!looks_binary(&late));
        let mut just_inside = vec![b'a'; 8191];
        just_inside.push(0);
        assert!(looks_binary(&just_inside));
    }

    /// Everything textual is `text/plain`, whatever it is called.
    ///
    /// This endpoint returns whatever somebody committed. Answering
    /// `text/html` for a file named `index.html` would let a repository
    /// serve script from this origin, so the extension list exists to
    /// spare a *client* a guess about images, not to be complete.
    #[test]
    fn text_is_never_given_a_type_a_repository_could_abuse() {
        for name in [
            "index.html",
            "app.js",
            "style.css",
            "data.svg",
            "readme",
            "Makefile",
            "x.PNG",
        ] {
            assert_eq!(
                content_type(name, false),
                "text/plain; charset=utf-8",
                "{name} was given a non-text type"
            );
        }
    }

    /// Binary content gets a real type only for formats a browser can
    /// display and cannot be tricked by.
    #[test]
    fn binary_content_is_typed_by_name_and_defaults_to_octet_stream() {
        for (name, want) in [
            ("logo.png", "image/png"),
            ("photo.jpg", "image/jpeg"),
            ("photo.JPEG", "image/jpeg"),
            ("anim.gif", "image/gif"),
            ("shot.webp", "image/webp"),
            ("favicon.ico", "image/x-icon"),
            ("manual.pdf", "application/pdf"),
            // Anything else, including things that would be dangerous
            // to type honestly.
            ("app.wasm", "application/octet-stream"),
            ("index.html", "application/octet-stream"),
            ("archive.tar.gz", "application/octet-stream"),
            ("noextension", "application/octet-stream"),
            ("", "application/octet-stream"),
            (".hidden", "application/octet-stream"),
        ] {
            assert_eq!(content_type(name, true), want, "{name}");
        }
    }
}
