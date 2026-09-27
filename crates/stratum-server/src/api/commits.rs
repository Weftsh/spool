//! The commit API (R2): tree construction in pure Rust, optimistic
//! concurrency on the parent (409 with the current tip on conflict),
//! durable at ack (the manifest CAS *is* the ack), audited with the
//! caller's context blob (R7).

use crate::api::refops_api::{forward_response, forward_rest};
use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::mirror::forward::Refusal;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::collections::BTreeMap;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::{Principal, Scope};
use stratum_control::registry::{Repo, RepoKind};
use stratum_control::users;
use stratum_engine::objwrite::{
    self, encode_commit, encode_tree, hex, new_object, CommitInfo, NewObject, TreeEntry, OBJ_BLOB,
    OBJ_COMMIT, OBJ_TREE,
};
use stratum_engine::read::LayoutReader;
use stratum_engine::refops::{transact, Expect, NewPack, RefUpdate, TxnError};
use stratum_store::{LatencyModel, Manifest, ObjectStore, Plane};

#[derive(Deserialize)]
pub struct CommitBody {
    #[serde(default = "default_branch")]
    pub branch: String,
    /// Optimistic concurrency: the branch must currently point here.
    /// Omitted = commit on top of whatever the branch points at now;
    /// explicit null = the branch must not exist yet.
    #[serde(default, with = "double_option")]
    pub expected_parent: Option<Option<String>>,
    pub message: String,
    #[serde(default)]
    pub author: Option<Author>,
    /// Free-form audit context (agent id, prompt id, …) — R7's product.
    #[serde(default)]
    pub context: Option<serde_json::Value>,
    pub operations: Vec<Operation>,
}

fn default_branch() -> String {
    "main".into()
}

#[derive(Deserialize, Clone)]
pub struct Author {
    pub name: String,
    pub email: String,
}

#[derive(Deserialize, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    Put { path: String, content: String },
    PutBase64 { path: String, content: String },
    Delete { path: String },
}

mod double_option {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<String>>, D::Error> {
        Ok(Some(Option::<String>::deserialize(d)?))
    }
}

pub async fn create(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<CommitBody>,
) -> Response {
    let (org, repo, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org_name, &repo_name, Scope::RepoWrite)
        {
            Ok(x) => x,
            Err(r) => return r,
        };
    // A protected branch moves only through the land queue — the same
    // sentence `git push` gets, from the API door.
    match stratum_control::protections::is_protected(&state.db, &repo.id, &body.branch) {
        Ok(true) => {
            return json_error(
                StatusCode::FORBIDDEN,
                stratum_control::protections::refusal(&body.branch),
            )
        }
        Ok(false) => {}
        Err(e) => return internal(e),
    }
    if body.operations.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "no operations");
    }
    if body.operations.len() > 10_000 {
        return json_error(StatusCode::BAD_REQUEST, "too many operations (max 10000)");
    }
    let author = acting_author(&state, principal.as_ref(), body.author.as_ref());

    let state2 = state.clone();
    let repo2 = repo.clone();
    let branch_ref = format!("refs/heads/{}", body.branch);
    let ops = body.operations.clone();
    let expected = body.expected_parent.clone();
    let message = body.message.clone();

    let result = tokio::task::spawn_blocking(move || {
        commit_blocking(&state2, &repo2, &branch_ref, expected, ops, author, message)
    })
    .await
    .unwrap_or_else(|e| Err(CommitError::Other(format!("task join: {e}"))));

    match result {
        Ok(out) => {
            // A mirror's commit went to the origin and came back through
            // the sync, which did the refresh, the fold, the CDN pack and
            // the authorship count itself; and its CI is the origin's.
            let forwarded = repo.kind == RepoKind::Mirror;
            if !forwarded {
                crate::storage::refresh_after_write(&state, &repo).await;
            }
            let ctx = AuditCtx::of(&org.id, principal.as_ref());
            let audit_blob = serde_json::json!({
                "commit": out.commit,
                "branch": body.branch,
                "context": body.context,
                "forwarded_to": forwarded.then(|| repo.origin_url.clone()),
            });
            crate::api::record_or_warn(
                &state.db,
                &ctx,
                Some(&repo.id),
                "repo.commit",
                Some(&audit_blob),
            );
            state.meter.record(&repo.id, "api", 0, None);
            crate::workers::notify::notify(
                &state,
                &repo.id,
                "push",
                serde_json::json!({
                    "via": "api", "commit": out.commit, "branch": body.branch,
                    "forwarded": forwarded,
                }),
            );
            if !forwarded {
                crate::workers::compactor::enqueue(&state, &org.id, &repo.id);
                crate::workers::cdnpack::enqueue(&state, &org.id, &repo.id);
                // The third door a commit can arrive through. A commit made
                // over the API counts exactly as one pushed over git does,
                // or the graph would quietly reward one workflow.
                crate::workers::contribs::enqueue(
                    &state,
                    &org.id,
                    &repo.id,
                    ctx.user_id.as_deref(),
                );
                // And it gets its CI the same way. The old tip is not known
                // here (the engine resolved it inside the transaction), and
                // the trigger only needs the new one.
                crate::workflow::trigger::on_push(
                    &state,
                    &repo,
                    &[stratum_proto::receive::Update {
                        old: String::new(),
                        new: out.commit.clone(),
                        name: format!("refs/heads/{}", body.branch),
                    }],
                )
                .await;
            }
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "commit": out.commit,
                    "tree": out.tree,
                    "parent": out.parent,
                    "branch": body.branch,
                })),
            )
                .into_response()
        }
        Err(CommitError::Conflict(current)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "expected_parent does not match the current branch tip",
                "current_tip": current,
            })),
        )
            .into_response(),
        Err(CommitError::BadRequest(e)) => json_error(StatusCode::BAD_REQUEST, e),
        Err(CommitError::Refused(r)) => forward_response(&state, &repo, r),
        Err(CommitError::Other(e)) => internal(e),
    }
}

/// Who `git log` will show. An explicit author wins — an agent
/// committing on someone's behalf should be able to say so. Otherwise
/// it is the person acting, by name and address, because
/// `token:01hx… <token:01hx…@stratum.local>` is not a person and every
/// tool downstream that groups by author was reading it as one.
///
/// A service token — one that acts for no person — signs with its
/// **label**: `token:release-bot <token:01hx…@stratum.local>`. The
/// prefix keeps it recognisably a machine (the forge renders `token:`
/// authors as such and never lends them a human name), the label says
/// *which* machine, and the id stays in the address for whoever is
/// debugging. Before this the name was the id too, and every commit a
/// labelled automation made rendered as the one word "token" — a
/// repository seeded by a bootstrap token showed a history nobody had
/// written. An unlabelled token still signs with its id: there is
/// nothing truer to say.
pub(crate) fn acting_author(
    state: &SharedState,
    principal: Option<&Principal>,
    explicit: Option<&Author>,
) -> String {
    match explicit {
        Some(a) => format!("{} <{}>", a.name, a.email),
        None => principal
            .and_then(|p| p.user_id.as_deref())
            .and_then(|id| users::by_id(&state.db, id).ok().flatten())
            .map(|u| u.git_ident())
            .unwrap_or_else(|| {
                let who = principal
                    .map(|p| p.audit_id())
                    .unwrap_or_else(|| stratum_control::audit::ANONYMOUS.to_string());
                let name = principal
                    .filter(|p| p.user_id.is_none())
                    .and_then(|p| stratum_control::auth::label_of(&state.db, &p.token_id).ok())
                    .flatten()
                    .map(|label| format!("token:{label}"))
                    .unwrap_or_else(|| who.clone());
                format!("{name} <{who}@stratum.local>")
            }),
    }
}

pub struct CommitOut {
    pub commit: String,
    pub tree: String,
    pub parent: Option<String>,
}

pub enum CommitError {
    Conflict(Option<String>),
    BadRequest(String),
    /// A mirror's origin did not take the commit.
    Refused(Refusal),
    Other(String),
}

impl From<String> for CommitError {
    fn from(e: String) -> CommitError {
        CommitError::Other(e)
    }
}

// Same regime as the wire path's CAS_RETRIES (experiment 008): few
// attempts lose real races, ten with jitter land them all, and every
// retry revalidates against the fresh tip so extra attempts trade only
// tail latency, never correctness.
const UNPINNED_CAS_RETRIES: usize = 10;

fn commit_blocking(
    state: &SharedState,
    repo: &Repo,
    branch_ref: &str,
    expected: Option<Option<String>>,
    ops: Vec<Operation>,
    author: String,
    message: String,
) -> Result<CommitOut, CommitError> {
    // An omitted expected_parent means "on top of whatever the branch
    // points at now" — a promise, not a snapshot. If a landing or a
    // racing writer moves the tip between our read and the CAS, the
    // caller pinned nothing a 409 could tell them about, so re-read and
    // rebuild instead (CI caught the lander winning exactly that window).
    // An explicit expected_parent keeps strict optimistic concurrency:
    // there the conflict IS the answer.
    let unpinned = expected.is_none();
    let mut attempt = 0;
    loop {
        let out = commit_once(
            state,
            repo,
            branch_ref,
            expected.clone(),
            &ops,
            author.clone(),
            message.clone(),
        );
        match out {
            // A mirror that was behind its origin has caught up by the
            // time it says so: an unpinned commit rebuilds on the tip
            // it has now, exactly as it does when a local writer won.
            Err(CommitError::Conflict(_)) | Err(CommitError::Refused(Refusal::Behind(_)))
                if unpinned && attempt + 1 < UNPINNED_CAS_RETRIES =>
            {
                attempt += 1;
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as u64)
                    .unwrap_or(12345);
                let ms = 10 * attempt as u64 + nanos % 50;
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            out => return out,
        }
    }
}

fn commit_once(
    state: &SharedState,
    repo: &Repo,
    branch_ref: &str,
    expected: Option<Option<String>>,
    ops: &[Operation],
    author: String,
    message: String,
) -> Result<CommitOut, CommitError> {
    let store_url = state.store_url.as_str();
    let prefix = &repo.prefix().as_str().to_string();
    let store = ObjectStore::new(store_url, LatencyModel::None);
    let mbytes = match store.get(&format!("{prefix}/manifest.json")) {
        Ok(b) => b,
        // A mirror whose first sync never landed has no manifest; a
        // commit cannot build a tree on nothing. Say so, rather than
        // leak the store's 404 as a 500.
        Err(e) if repo.kind == RepoKind::Mirror && stratum_engine::errclass::is_absent(&e) => {
            return Err(CommitError::Refused(Refusal::Unreachable(
                crate::mirror::forward::NO_LAYOUT_YET.to_string(),
            )));
        }
        Err(e) => return Err(CommitError::Other(e)),
    };
    let manifest: Manifest = serde_json::from_slice(&mbytes)
        .map_err(|e| CommitError::Other(format!("manifest: {e}")))?;
    // Same sha1-only guard as the wire path (I-formats): refuse rather
    // than misread a future object format.
    if manifest.object_format != "sha1" {
        return Err(CommitError::Other(format!(
            "object_format {:?} not supported by this build (sha1 only)",
            manifest.object_format
        )));
    }
    let reader = LayoutReader::new(&store, prefix, &manifest).map_err(CommitError::Other)?;

    // Pin the parent this commit builds against; the CAS enforces it.
    let current = reader.ref_oid(branch_ref).map_err(CommitError::Other)?;
    let parent: Option<String> = match &expected {
        None => current.clone(),
        Some(None) => {
            if current.is_some() {
                return Err(CommitError::Conflict(current));
            }
            None
        }
        Some(Some(want)) => {
            if current.as_deref() != Some(want.as_str()) {
                return Err(CommitError::Conflict(current));
            }
            Some(want.clone())
        }
    };

    // Build the new tree from the parent's, along touched paths only.
    let parent_tree: Option<String> = match &parent {
        Some(p) => {
            let (k, data) = reader.object(p).map_err(CommitError::Other)?;
            if k != OBJ_COMMIT {
                return Err(CommitError::Other(format!("{p} is not a commit")));
            }
            Some(
                objwrite::parse_commit(&data)
                    .map_err(CommitError::Other)?
                    .tree,
            )
        }
        None => None,
    };

    let mut new_objects: Vec<NewObject> = Vec::new();
    let mut changes: Vec<(Vec<String>, Option<Leaf>)> = Vec::new(); // (path parts, blob or delete)
    for op in ops {
        match op {
            Operation::Put { path, content } => {
                let parts = split_path(path)?;
                let blob = new_object(OBJ_BLOB, content.clone().into_bytes());
                changes.push((parts, Some(Leaf::file(blob.oid))));
                new_objects.push(blob);
            }
            Operation::PutBase64 { path, content } => {
                let parts = split_path(path)?;
                let raw = b64decode(content)
                    .ok_or_else(|| CommitError::BadRequest(format!("bad base64 for {path}")))?;
                let blob = new_object(OBJ_BLOB, raw);
                changes.push((parts, Some(Leaf::file(blob.oid))));
                new_objects.push(blob);
            }
            Operation::Delete { path } => {
                changes.push((split_path(path)?, None));
            }
        }
    }

    let new_tree = build_tree(&reader, parent_tree.as_deref(), &changes, &mut new_objects)
        .map_err(CommitError::Other)?;

    let commit = new_object(
        OBJ_COMMIT,
        encode_commit(&CommitInfo {
            tree: new_tree,
            parents: parent
                .iter()
                .map(|p| objwrite::parse_hex(p))
                .collect::<Result<_, _>>()
                .map_err(CommitError::Other)?,
            author: author.clone(),
            committer: author,
            timestamp: stratum_control_free_now(),
            message,
        }),
    );
    let commit_hex = hex(&commit.oid);
    let tree_hex = hex(&new_tree);
    new_objects.push(commit);

    let to_pack =
        absent_from_layout(&store, prefix, &manifest, new_objects).map_err(CommitError::Other)?;

    // Idempotency: replaying an identical commit is a success, not a dup.
    if !to_pack.iter().any(|o| hex(&o.oid) == commit_hex) && current.as_deref() == Some(&commit_hex)
    {
        return Ok(CommitOut {
            commit: commit_hex,
            tree: tree_hex,
            parent,
        });
    }

    let (payload, entries) = objwrite::build_pack_payload(&to_pack).map_err(CommitError::Other)?;
    let mut oids: Vec<[u8; 20]> = to_pack.iter().map(|o| o.oid).collect();
    oids.sort();

    let update = RefUpdate {
        name: branch_ref.to_string(),
        expect: match &parent {
            Some(p) => Expect::Equals(p.clone()),
            None => Expect::Absent,
        },
        new: Some(commit_hex.clone()),
    };
    let pack = NewPack {
        payload,
        oids,
        entries,
    };
    if repo.kind == RepoKind::Mirror {
        // The origin first; the sync that follows lands the same objects
        // here, and the answer is the same shape as a native commit's.
        return match forward_rest(state, repo, vec![update], Some(pack)) {
            Ok(()) => Ok(CommitOut {
                commit: commit_hex,
                tree: tree_hex,
                parent,
            }),
            Err(r) => Err(CommitError::Refused(r)),
        };
    }
    match transact(&store, prefix, &[update], Some(&pack)) {
        Ok(_) => Ok(CommitOut {
            commit: commit_hex,
            tree: tree_hex,
            parent,
        }),
        Err(TxnError::Conflict(_, cur)) => Err(CommitError::Conflict(cur)),
        Err(TxnError::Other(e)) => Err(CommitError::Other(e)),
    }
}

/// Dedup against the layout (I5): identical blobs/trees already present
/// must not enter the WAL — a concat stream never carries an object
/// twice. Content-addressing makes the skip safe. Answers the objects
/// that are new, each once, in the order given.
pub(crate) fn absent_from_layout(
    store: &ObjectStore,
    prefix: &str,
    manifest: &Manifest,
    objects: Vec<NewObject>,
) -> Result<Vec<NewObject>, String> {
    let plane = match Plane::load(store, prefix) {
        Ok(p) => Some(p),
        Err(e) if stratum_engine::errclass::is_absent(&e) => None,
        Err(e) => return Err(e),
    };
    let mut wal_oids = std::collections::HashSet::new();
    for w in &manifest.wal {
        let raw = store.get(&w.oids_key)?;
        for c in raw.as_chunks::<20>().0 {
            wal_oids.insert(*c);
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut to_pack: Vec<NewObject> = Vec::new();
    for o in objects {
        if !seen.insert(o.oid) {
            continue;
        }
        let exists = wal_oids.contains(&o.oid)
            || match &plane {
                Some(p) => p.lookup(store, &o.oid)?.is_some(),
                None => false,
            };
        if !exists {
            to_pack.push(o);
        }
    }
    Ok(to_pack)
}

/// A blob to put at a path, with the mode `git` will record for it. The
/// commit API only ever writes plain files; a revert puts back whatever
/// the entry was, executable bit included.
#[derive(Clone)]
pub(crate) struct Leaf {
    pub oid: [u8; 20],
    pub mode: String,
}

impl Leaf {
    pub(crate) fn file(oid: [u8; 20]) -> Leaf {
        Leaf {
            oid,
            mode: "100644".into(),
        }
    }
}

/// Rebuild the tree spine bottom-up for the touched paths.
pub(crate) fn build_tree(
    reader: &LayoutReader,
    parent_tree: Option<&str>,
    changes: &[(Vec<String>, Option<Leaf>)],
    new_objects: &mut Vec<NewObject>,
) -> Result<[u8; 20], String> {
    // Nested change map: name -> either a leaf change or a subtree map.
    #[derive(Default)]
    struct Node {
        children: BTreeMap<String, Node>,
        leaf: Option<Option<Leaf>>,
    }
    let mut root = Node::default();
    for (parts, change) in changes {
        let mut cur = &mut root;
        for p in &parts[..parts.len() - 1] {
            cur = cur.children.entry(p.clone()).or_default();
        }
        cur.children
            .entry(parts[parts.len() - 1].clone())
            .or_default()
            .leaf = Some(change.clone());
    }

    fn apply(
        reader: &LayoutReader,
        base_tree: Option<&str>,
        node: &Node,
        new_objects: &mut Vec<NewObject>,
    ) -> Result<Option<[u8; 20]>, String> {
        let mut entries: Vec<TreeEntry> = match base_tree {
            Some(oid) => {
                let (k, data) = reader.object(oid)?;
                if k != OBJ_TREE {
                    return Err(format!("{oid} is not a tree"));
                }
                objwrite::parse_tree(&data)?
            }
            None => Vec::new(),
        };
        for (name, child) in &node.children {
            let existing = entries.iter().position(|e| e.name == *name);
            if let Some(change) = &child.leaf {
                match change {
                    Some(leaf) => {
                        let entry = TreeEntry {
                            mode: leaf.mode.clone(),
                            name: name.clone(),
                            oid: leaf.oid,
                        };
                        match existing {
                            Some(i) => entries[i] = entry,
                            None => entries.push(entry),
                        }
                    }
                    None => {
                        if let Some(i) = existing {
                            entries.remove(i);
                        }
                    }
                }
            } else {
                // Descend into (possibly existing) subtree.
                let base = existing.and_then(|i| {
                    let e = &entries[i];
                    (e.mode == "40000" || e.mode == "040000").then(|| hex(&e.oid))
                });
                // A directory the change has emptied is not a directory
                // any more: git records no empty trees, and a clone would
                // never produce one.
                match (
                    apply(reader, base.as_deref(), child, new_objects)?,
                    existing,
                ) {
                    (Some(sub), _) => {
                        let entry = TreeEntry {
                            mode: "40000".into(),
                            name: name.clone(),
                            oid: sub,
                        };
                        match existing {
                            Some(i) => entries[i] = entry,
                            None => entries.push(entry),
                        }
                    }
                    (None, Some(i)) => {
                        entries.remove(i);
                    }
                    (None, None) => {}
                }
            }
        }
        if entries.is_empty() {
            return Ok(None);
        }
        let bytes = encode_tree(&mut entries);
        let obj = new_object(OBJ_TREE, bytes);
        let oid = obj.oid;
        new_objects.push(obj);
        Ok(Some(oid))
    }
    // The root is the one tree that may be empty: a commit of nothing at
    // all is the empty tree, not no tree.
    match apply(reader, parent_tree, &root, new_objects)? {
        Some(oid) => Ok(oid),
        None => {
            let obj = new_object(OBJ_TREE, encode_tree(&mut Vec::new()));
            let oid = obj.oid;
            new_objects.push(obj);
            Ok(oid)
        }
    }
}

pub(crate) fn split_path(path: &str) -> Result<Vec<String>, CommitError> {
    let parts: Vec<String> = path
        .split('/')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if parts.is_empty()
        || parts
            .iter()
            .any(|p| p == "." || p == ".." || p.contains('\0') || p == ".git")
    {
        return Err(CommitError::BadRequest(format!("invalid path {path:?}")));
    }
    Ok(parts)
}

pub(crate) fn stratum_control_free_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn b64decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s = s.trim().trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut nbits = 0;
    for &c in s {
        acc = (acc << 6) | val(c)? as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Some(out)
}
