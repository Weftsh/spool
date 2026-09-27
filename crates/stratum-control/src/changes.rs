//! Changes, patchsets, approvals: the control-plane truth for review.
//!
//! A change is one commit's review identity, keyed by `change_key`
//! (the Change-Id trailer, or an oid-derived key when there is none).
//! Its content moves through numbered patchsets; approvals attach to a
//! patchset, and sufficiency reads only the latest one — an approval of
//! yesterday's revision says nothing about today's.
//!
//! State machine, deliberately small:
//! `open` → `landing` (enqueued) → `landed`, or back to `open` on an
//! ejection with the verdict recorded; `open` → `abandoned` closes the
//! change without landing. Every transition is guarded by a `WHERE
//! state = …` and reports whether it won, so a lost race is a fact the
//! caller handles, never a silent overwrite.

use crate::db::{is_unique_violation, ControlDb};
use crate::ids::{now_ms, ulid, valid_id};

/// A change key is trailer-shaped or oid-derived: short, no whitespace,
/// nothing a URL or a query needs escaping for. Checked before SQL like
/// every identifier — a hostile shape is definitionally absent.
pub fn valid_change_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 72
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

#[derive(Debug, Clone)]
pub struct Change {
    pub id: String,
    pub org_id: String,
    pub repo_id: String,
    pub change_key: String,
    pub title: String,
    pub target_branch: String,
    pub state: String,
    pub land_job_id: Option<String>,
    pub land_verdict: Option<String>,
    pub landed_commit: Option<String>,
    pub created_by: Option<String>,
    /// The **fork** this change's commits live in, when they do not live
    /// in the repository being targeted.
    ///
    /// This is the whole contribution path for somebody with no push
    /// credential: they fork, push to the repository they own, and open
    /// a change against upstream. `None` means what it has always meant
    /// — the commits are already in the target — so every existing
    /// change and the entire enterprise flow are unaffected.
    ///
    /// It is load-bearing at landing time rather than merely
    /// informational: a change whose objects are in another prefix
    /// cannot be landed by writing a ref, because the objects the ref
    /// would point at are not there.
    pub source_repo_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct Patchset {
    pub id: String,
    pub change_id: String,
    pub number: i64,
    pub commit_oid: String,
    pub parent_oid: Option<String>,
    pub message: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct Approval {
    pub patchset_id: String,
    pub user_id: String,
    pub email: String,
    pub name: String,
    pub created_at: i64,
}

fn row_to_change(r: &postgres::Row) -> Change {
    Change {
        id: r.get("id"),
        org_id: r.get("org_id"),
        repo_id: r.get("repo_id"),
        change_key: r.get("change_key"),
        title: r.get("title"),
        target_branch: r.get("target_branch"),
        state: r.get("state"),
        land_job_id: r.get("land_job_id"),
        land_verdict: r.get("land_verdict"),
        landed_commit: r.get("landed_commit"),
        created_by: r.get("created_by"),
        source_repo_id: r.get("source_repo_id"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

fn row_to_patchset(r: &postgres::Row) -> Patchset {
    Patchset {
        id: r.get("id"),
        change_id: r.get("change_id"),
        number: r.get("number"),
        commit_oid: r.get("commit_oid"),
        parent_oid: r.get("parent_oid"),
        message: r.get("message"),
        created_at: r.get("created_at"),
    }
}

const CHANGE_COLS: &str = "id, org_id, repo_id, change_key, title, target_branch, state, \
                           land_job_id, land_verdict, landed_commit, created_by, \
                           source_repo_id, created_at, updated_at";

/// Register a commit under a change key: creates the change on first
/// sight, adds a numbered patchset for a new commit, and acks
/// idempotently for a commit it has already seen. Returns the change,
/// the patchset for this commit, and whether the patchset is new.
///
/// A change that is `landing`, `landed` or `abandoned` refuses *new*
/// patchsets (the inner error names the state); re-posting a commit it
/// already has stays idempotent in any state, because acking what
/// already happened is never wrong.
#[allow(clippy::too_many_arguments)]
pub fn create_or_update(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    change_key: &str,
    title: &str,
    target_branch: &str,
    commit_oid: &str,
    parent_oid: Option<&str>,
    message: &str,
    created_by: Option<&str>,
    // The fork the commits live in, or `None` when they are already in
    // the target repository. Set on creation only: a change does not
    // change which repository it came from, and letting a later patchset
    // move it would let a contributor swap the source out from under an
    // approval.
    source_repo_id: Option<&str>,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<Result<(Change, Patchset, bool), String>, String> {
    if !valid_change_key(change_key) {
        return Ok(Err(format!("invalid change key {change_key:?}")));
    }
    let now = now_ms();
    let change_id_new = ulid();
    let ps_id_new = ulid();
    let out = db
        .lock()
        .transaction(move |tx| {
            let existing = tx.query_opt(
                &format!(
                    "SELECT {CHANGE_COLS} FROM changes \
                     WHERE repo_id = $1 AND change_key = $2 FOR UPDATE"
                ),
                &[&repo_id, &change_key],
            )?;
            let (change, created) = match existing {
                Some(r) => (row_to_change(&r), false),
                None => {
                    tx.execute(
                        "INSERT INTO changes (id, org_id, repo_id, change_key, title, \
                         target_branch, state, created_by, source_repo_id, \
                         created_at, updated_at) \
                         VALUES ($1, $2, $3, $4, $5, $6, 'open', $7, $9, $8, $8)",
                        &[
                            &change_id_new,
                            &org_id,
                            &repo_id,
                            &change_key,
                            &title,
                            &target_branch,
                            &created_by,
                            &now,
                            &source_repo_id,
                        ],
                    )?;
                    let r = tx.query_one(
                        &format!("SELECT {CHANGE_COLS} FROM changes WHERE id = $1"),
                        &[&change_id_new],
                    )?;
                    (row_to_change(&r), true)
                }
            };
            // The same commit again: ack the standing patchset.
            if let Some(r) = tx.query_opt(
                "SELECT id, change_id, number, commit_oid, parent_oid, message, created_at \
                 FROM patchsets WHERE change_id = $1 AND commit_oid = $2",
                &[&change.id, &commit_oid],
            )? {
                return Ok(Ok((change, row_to_patchset(&r), false)));
            }
            if change.state != "open" {
                return Ok(Err(format!("change is {}", change.state)));
            }
            let number: i64 = tx
                .query_one(
                    "SELECT COALESCE(MAX(number), 0) + 1 FROM patchsets WHERE change_id = $1",
                    &[&change.id],
                )?
                .get(0);
            tx.execute(
                "INSERT INTO patchsets (id, change_id, number, commit_oid, parent_oid, \
                 message, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &ps_id_new,
                    &change.id,
                    &number,
                    &commit_oid,
                    &parent_oid,
                    &message,
                    &now,
                ],
            )?;
            tx.execute(
                "UPDATE changes SET title = $2, updated_at = $3 WHERE id = $1",
                &[&change.id, &title, &now],
            )?;
            if let Some(ctx) = audit {
                let action = if created {
                    "change.create"
                } else {
                    "change.patchset"
                };
                let blob = serde_json::json!({
                    "change_key": change_key,
                    "patchset": number,
                    "commit": commit_oid,
                });
                crate::audit::record_tx(tx, ctx, Some(&change.repo_id), action, Some(&blob))?;
            }
            let ps = tx.query_one(
                "SELECT id, change_id, number, commit_oid, parent_oid, message, created_at \
                 FROM patchsets WHERE id = $1",
                &[&ps_id_new],
            )?;
            let mut change = change;
            change.title = title.to_string();
            change.updated_at = now;
            Ok(Ok((change, row_to_patchset(&ps), true)))
        })
        .map_err(|e| {
            if is_unique_violation(&e) {
                // Two writers racing on the same key or commit: the loser
                // retries at the API layer by re-reading; surfacing the
                // conflict beats picking a winner silently here.
                "change conflict: retry".to_string()
            } else {
                format!("create change: {e}")
            }
        })?;
    Ok(out)
}

/// What one page of a change list asks for.
///
/// A struct rather than four positional arguments for the reason
/// [`NewComment`] is one: `Some("open")` and `Some(<a ulid>)` are both
/// `Option<&str>`, and in the wrong slot the second is a state nothing
/// matches — a page that is silently empty rather than wrong out loud.
#[derive(Debug, Clone, Default)]
pub struct Page<'a> {
    /// `open` / `landing` / `landed` / `abandoned`, or every state.
    pub state: Option<&'a str>,
    /// Only changes this person opened. The id, not the email: the
    /// caller resolves the person, because "who is @me" is a question
    /// about a credential and this module has never seen one.
    pub author_user_id: Option<&'a str>,
    /// **Keyset cursor**: the id of the last row of the previous page,
    /// as [`Listing::next`] handed it back.
    ///
    /// Keyset and not `OFFSET`, and the difference is correctness rather
    /// than speed. Rows arrive at the *top* of this ordering — a change
    /// opened between two requests has a higher id than everything
    /// already listed — so an offset of 50 walks past a row that has
    /// shifted down into it, and the reader never sees that change at
    /// all. A cursor names a position in the data instead of a count of
    /// rows, so a row added mid-page cannot make the next page skip one.
    /// It is also what keeps the query cheap: `id < $cursor` is an index
    /// seek where `OFFSET 5000` reads and discards five thousand rows.
    pub after: Option<&'a str>,
    pub limit: i64,
}

/// One page of changes, and where the next one starts.
#[derive(Debug, Clone)]
pub struct Listing {
    pub rows: Vec<Change>,
    /// The cursor to pass as [`Page::after`], or `None` when this page
    /// reached the end. It is the id of the **last row this query
    /// examined**, which is the last returned row here — a caller that
    /// filters the rows further afterwards must keep this value rather
    /// than deriving one from what survived, or the rows it dropped
    /// would be walked again on the next request.
    pub next: Option<String>,
}

/// Changes for a repo, newest first, optionally filtered by state.
///
/// The unpaged shape, for the callers that want a bounded sweep of one
/// repository rather than a page a person is reading: the land queue,
/// the badge's landed lookback, the lander's stack reconciliation.
pub fn list(
    db: &ControlDb,
    repo_id: &str,
    state: Option<&str>,
    limit: i64,
) -> Result<Vec<Change>, String> {
    let ids = [repo_id.to_string()];
    Ok(list_in_org(
        db,
        &ids,
        &Page {
            state,
            limit,
            ..Default::default()
        },
    )?
    .rows)
}

/// The same page, across several repositories at once.
///
/// The dashboard's changeset picker needs every open change a person may
/// see in an organization; asking `list` once per repository is a round
/// trip per repository, and the picker is the one screen where an org
/// with a hundred repositories is the ordinary case.
///
/// **The caller decides what may be seen.** This takes repository ids,
/// not an org id, precisely so that the authority question is answered
/// where it always is — one `rest_repo_auth` per repository — and never
/// re-derived in SQL. An empty slice is an empty page, not "everything":
/// `= ANY('{}')` matches nothing, which is the right answer for a caller
/// who may read nothing.
///
/// One statement with `$n IS NULL OR …` guards rather than a branch per
/// filter combination: four optional predicates are sixteen branches
/// written out, and the fifteenth is the one nobody tests.
pub fn list_in_org(db: &ControlDb, repo_ids: &[String], q: &Page<'_>) -> Result<Listing, String> {
    if repo_ids.is_empty() {
        return Ok(Listing {
            rows: Vec::new(),
            next: None,
        });
    }
    let limit = q.limit.clamp(1, 500);
    // A cursor that cannot be an id names no row, and must not reach the
    // query — where it would silently compare as a string and page from
    // the middle of nowhere rather than being refused. The caller
    // shape-checks it at the door; this is the same masking contract
    // every lookup here honours, kept as a floor.
    let after = q.after.filter(|a| valid_id(a));
    // One more than asked for, so "is there another page" is a fact
    // rather than the guess "this page was full, so probably".
    let probe = limit + 1;
    let ids = repo_ids.to_vec();
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {CHANGE_COLS} FROM changes \
                 WHERE repo_id = ANY($1) \
                   AND ($2::text IS NULL OR state = $2) \
                   AND ($3::text IS NULL OR created_by = $3) \
                   AND ($4::text IS NULL OR id < $4) \
                 ORDER BY id DESC LIMIT $5"
            ),
            &[&ids, &q.state, &q.author_user_id, &after, &probe],
        )
        .map_err(|e| format!("list changes in org: {e}"))?;
    let more = rows.len() as i64 > limit;
    let mut out: Vec<Change> = rows.iter().map(row_to_change).collect();
    out.truncate(limit as usize);
    let next = more.then(|| out.last().map(|c| c.id.clone())).flatten();
    Ok(Listing { rows: out, next })
}

pub fn by_key(db: &ControlDb, repo_id: &str, change_key: &str) -> Result<Option<Change>, String> {
    if !valid_change_key(change_key) {
        return Ok(None);
    }
    db.lock()
        .query_opt(
            &format!("SELECT {CHANGE_COLS} FROM changes WHERE repo_id = $1 AND change_key = $2"),
            &[&repo_id, &change_key],
        )
        .map_err(|e| format!("change by key: {e}"))
        .map(|r| r.as_ref().map(row_to_change))
}

/// By row id — the lander's lookup, straight from a job payload.
pub fn by_id(db: &ControlDb, change_id: &str) -> Result<Option<Change>, String> {
    if !valid_id(change_id) {
        return Ok(None);
    }
    db.lock()
        .query_opt(
            &format!("SELECT {CHANGE_COLS} FROM changes WHERE id = $1"),
            &[&change_id],
        )
        .map_err(|e| format!("change by id: {e}"))
        .map(|r| r.as_ref().map(row_to_change))
}

pub fn patchsets(db: &ControlDb, change_id: &str) -> Result<Vec<Patchset>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT id, change_id, number, commit_oid, parent_oid, message, created_at \
             FROM patchsets WHERE change_id = $1 ORDER BY number",
            &[&change_id],
        )
        .map_err(|e| format!("patchsets: {e}"))?;
    Ok(rows.iter().map(row_to_patchset).collect())
}

pub fn latest_patchset(db: &ControlDb, change_id: &str) -> Result<Option<Patchset>, String> {
    db.lock()
        .query_opt(
            "SELECT id, change_id, number, commit_oid, parent_oid, message, created_at \
             FROM patchsets WHERE change_id = $1 ORDER BY number DESC LIMIT 1",
            &[&change_id],
        )
        .map_err(|e| format!("latest patchset: {e}"))
        .map(|r| r.as_ref().map(row_to_patchset))
}

/// Record an approval on a patchset. Idempotent: an already-active
/// approval acks rather than errors. Authority moves here, so the audit
/// entry is written in the same transaction.
pub fn approve(
    db: &ControlDb,
    change_id: &str,
    patchset_id: &str,
    user_id: &str,
    audit: &crate::audit::AuditCtx,
) -> Result<bool, String> {
    if !valid_id(user_id) || !valid_id(patchset_id) {
        return Err("no such patchset or user".into());
    }
    let id = ulid();
    let now = now_ms();
    let repo_id = change_repo(db, change_id)?;
    db.lock()
        .transaction(move |tx| {
            let existing = tx.query_opt(
                "SELECT id FROM approvals \
                 WHERE patchset_id = $1 AND user_id = $2 AND revoked_at IS NULL",
                &[&patchset_id, &user_id],
            )?;
            if existing.is_some() {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO approvals (id, change_id, patchset_id, user_id, created_at) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[&id, &change_id, &patchset_id, &user_id, &now],
            )?;
            let blob = serde_json::json!({ "change_id": change_id, "patchset_id": patchset_id });
            crate::audit::record_tx(tx, audit, repo_id.as_deref(), "change.approve", Some(&blob))?;
            Ok(true)
        })
        .map_err(|e| format!("approve: {e}"))
}

/// Revoke one's active approval on a patchset. Returns whether there was
/// one to revoke.
pub fn unapprove(
    db: &ControlDb,
    change_id: &str,
    patchset_id: &str,
    user_id: &str,
    audit: &crate::audit::AuditCtx,
) -> Result<bool, String> {
    if !valid_id(user_id) || !valid_id(patchset_id) {
        return Ok(false);
    }
    let now = now_ms();
    let repo_id = change_repo(db, change_id)?;
    db.lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "UPDATE approvals SET revoked_at = $3 \
                 WHERE patchset_id = $1 AND user_id = $2 AND revoked_at IS NULL",
                &[&patchset_id, &user_id, &now],
            )?;
            if n > 0 {
                let blob =
                    serde_json::json!({ "change_id": change_id, "patchset_id": patchset_id });
                crate::audit::record_tx(
                    tx,
                    audit,
                    repo_id.as_deref(),
                    "change.unapprove",
                    Some(&blob),
                )?;
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("unapprove: {e}"))
}

/// Active approvals on a patchset, with the approver's identity joined
/// in — explanations display people, not ids.
pub fn approvals_for(db: &ControlDb, patchset_id: &str) -> Result<Vec<Approval>, String> {
    if !valid_id(patchset_id) {
        return Ok(Vec::new());
    }
    let rows = db
        .lock()
        .query(
            "SELECT a.patchset_id, a.user_id, a.created_at, u.email, u.name \
             FROM approvals a JOIN users u ON u.id = a.user_id \
             WHERE a.patchset_id = $1 AND a.revoked_at IS NULL \
             ORDER BY a.created_at",
            &[&patchset_id],
        )
        .map_err(|e| format!("approvals: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| Approval {
            patchset_id: r.get("patchset_id"),
            user_id: r.get("user_id"),
            email: r.get("email"),
            name: r.get("name"),
            created_at: r.get("created_at"),
        })
        .collect())
}

/// Which half of the diff a comment's anchor sits in.
///
/// A review has two things to say about a line: "this new code is
/// wrong", and "you should not have deleted this". Only the first was
/// expressible before, so a comment about a removal had to be written
/// against whatever line happened to occupy the position afterwards —
/// which is a different statement about a different line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Side {
    /// The post-image: the file as this patchset leaves it.
    #[default]
    New,
    /// The pre-image: a line the change removed or replaced.
    Old,
}

impl Side {
    /// The stored spelling, which is also the wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Side::New => "new",
            Side::Old => "old",
        }
    }

    /// Parse the wire spelling. `None` for anything else — an
    /// unrecognised side must be refused in words at the door, never
    /// silently coerced to `new`, which would move the anchor.
    pub fn parse(s: &str) -> Option<Side> {
        match s {
            "new" => Some(Side::New),
            "old" => Some(Side::Old),
            _ => None,
        }
    }
}

/// One comment in a change's conversation. The author is a person when
/// there was one; agents and service tokens speak through the principal.
#[derive(Debug, Clone, Default)]
pub struct Comment {
    pub id: String,
    pub change_id: String,
    pub patchset_number: i64,
    pub author_principal: String,
    pub author_user_id: Option<String>,
    pub author_email: Option<String>,
    pub author_name: Option<String>,
    pub path: Option<String>,
    /// 1-based line in the file as of the patchset commented on. Present
    /// only with `path` — a line number without a file means nothing.
    pub line: Option<i64>,
    /// Last line of a multi-line anchor, inclusive. Equal to `line` for
    /// a single-line comment; `None` exactly when `line` is `None`.
    pub line_end: Option<i64>,
    /// Which half of the diff `line` counts in.
    pub side: Side,
    /// The root of this thread when this row is a reply; `None` when
    /// this row *is* a root. Replies are one level deep by construction.
    pub parent_id: Option<String>,
    /// The anchor as first written, kept beside the live one so that a
    /// comment on a since-rewritten file can be placed rather than
    /// guessed at. Equal to `line`/`patchset_number` until something
    /// re-anchors the thread.
    pub original_line: Option<i64>,
    pub original_patchset: Option<i64>,
    /// The identity this comment had wherever it was imported from.
    /// `None` for a comment spoken here.
    pub external_id: Option<String>,
    /// The review this comment was drafted into, when it was drafted
    /// into one. Set on every pending comment and kept afterwards, so
    /// "these twelve remarks were one pass" survives submission.
    pub review_id: Option<String>,
    /// When this comment became visible to anybody but its author.
    /// `None` is the one thing it means: an unsubmitted draft.
    pub published_at: Option<i64>,
    /// When the thread was resolved, and by whom. Root rows only: a
    /// thread resolves, a sentence inside one does not.
    pub resolved_at: Option<i64>,
    pub resolved_by: Option<String>,
    pub resolved_by_name: Option<String>,
    pub body: String,
    pub created_at: i64,
}

impl Comment {
    /// The thread this comment belongs to — itself, if it is a root.
    /// The one grouping key a renderer needs, computed in one place so
    /// that two renderers cannot disagree about it.
    pub fn thread_id(&self) -> &str {
        self.parent_id.as_deref().unwrap_or(&self.id)
    }
}

/// Everything one new comment carries. A struct rather than eleven
/// positional arguments: the anchor alone is four fields, and `Some(2)`
/// in the wrong slot is a comment on the wrong line with nothing to
/// notice it.
#[derive(Debug, Clone, Default)]
pub struct NewComment<'a> {
    pub change_id: &'a str,
    pub patchset_number: i64,
    pub author_principal: &'a str,
    pub author_user_id: Option<&'a str>,
    /// The thread being replied to. A reply sets no anchor of its own.
    pub parent_id: Option<&'a str>,
    pub path: Option<&'a str>,
    pub line: Option<i64>,
    pub line_end: Option<i64>,
    pub side: Side,
    pub external_id: Option<&'a str>,
    /// Draft this comment into a review instead of publishing it. The
    /// id must be the caller's own pending review — this is what makes
    /// twelve remarks one act rather than twelve interruptions — and a
    /// comment written this way is invisible to everybody else until
    /// that review is submitted.
    pub review_id: Option<&'a str>,
    pub body: &'a str,
}

/// The longest comment worth storing (I13: bounded hostile input). Long
/// enough for a real review essay, short enough that nobody pastes a
/// core dump into the conversation.
pub const MAX_COMMENT_LEN: usize = 4000;

/// The deepest line a comment can anchor to. Far beyond any reviewable
/// file, small enough that a hostile number stays a number (I13).
pub const MAX_COMMENT_LINE: i64 = 1_000_000;

/// The longest foreign identity worth storing. Long enough for the
/// deep-link shapes real forges mint, short enough that it stays a
/// string rather than a payload (I13).
pub const MAX_EXTERNAL_ID_LEN: usize = 300;

/// What a human being may type into the review record: trimmed, never
/// empty, bounded, and free of control bytes.
///
/// One function because a comment body and a review's cover message are
/// the same kind of thing arriving through two doors, and two copies of
/// this rule would eventually differ — in the direction where the newer
/// door is the lax one. A NUL reaching the query would surface as a
/// database 500 where the contract promises a refusal in words (I13 at
/// the storage boundary); newline and tab are prose and stay.
fn prose(what: &str, raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(format!("a {what} needs words"));
    }
    if s.len() > MAX_COMMENT_LEN {
        return Err(format!("{what} is over {MAX_COMMENT_LEN} bytes"));
    }
    if s.bytes()
        .any(|b| (b < 0x20 && b != b'\n' && b != b'\t') || b == 0x7f)
    {
        return Err(format!("{what} contains control characters"));
    }
    Ok(s.to_string())
}

/// The columns every comment read selects, with both identities joined
/// in. One definition, so the row shape `row_to_comment` expects and the
/// shape a query produces cannot drift apart.
const COMMENT_COLUMNS: &str = "c.id, c.change_id, c.patchset_number, c.author_principal, \
     c.author_user_id, c.parent_id, c.path, c.line, c.line_end, c.side, \
     c.original_line, c.original_patchset, c.external_id, \
     c.review_id, c.published_at, \
     c.resolved_at, c.resolved_by, c.body, c.created_at, \
     u.email, u.name, r.name AS resolver_name \
     FROM change_comments c \
     LEFT JOIN users u ON u.id = c.author_user_id \
     LEFT JOIN users r ON r.id = c.resolved_by";

fn row_to_comment(r: &postgres::Row) -> Comment {
    let side: String = r.get("side");
    Comment {
        id: r.get("id"),
        change_id: r.get("change_id"),
        patchset_number: r.get("patchset_number"),
        author_principal: r.get("author_principal"),
        author_user_id: r.get("author_user_id"),
        author_email: r.get("email"),
        author_name: r.get("name"),
        path: r.get("path"),
        line: r.get("line"),
        line_end: r.get("line_end"),
        // The column carries a CHECK constraint, so an unknown spelling
        // cannot be in the table; falling back to `New` keeps a read
        // total rather than panicking on data that cannot exist.
        side: Side::parse(&side).unwrap_or_default(),
        parent_id: r.get("parent_id"),
        original_line: r.get("original_line"),
        original_patchset: r.get("original_patchset"),
        external_id: r.get("external_id"),
        review_id: r.get("review_id"),
        published_at: r.get("published_at"),
        resolved_at: r.get("resolved_at"),
        resolved_by: r.get("resolved_by"),
        resolved_by_name: r.get("resolver_name"),
        body: r.get("body"),
        created_at: r.get("created_at"),
    }
}

/// One comment by id, identities joined in. `None` for an id that
/// cannot exist — the same masking contract every lookup here honours.
pub fn comment_by_id(db: &ControlDb, comment_id: &str) -> Result<Option<Comment>, String> {
    if !valid_id(comment_id) {
        return Ok(None);
    }
    let rows = db
        .lock()
        .query(
            &format!("SELECT {COMMENT_COLUMNS} WHERE c.id = $1"),
            &[&comment_id],
        )
        .map_err(|e| format!("comment: {e}"))?;
    Ok(rows.first().map(row_to_comment))
}

/// Record a comment against the change at a patchset. The body is
/// bounded and never empty; the caller resolves the acting principal.
///
/// A reply — `parent_id` set — inherits its thread's anchor whole
/// (path, line, range, side, and the original those were first written
/// at) and may not set one of its own. It is part of that thread, not a
/// second remark that happens to sit near it, and letting a reply
/// re-anchor is how a thread ends up describing two places at once.
pub fn add_comment(
    db: &ControlDb,
    new: &NewComment<'_>,
) -> Result<Result<Comment, String>, String> {
    let body = match prose("comment", new.body) {
        Ok(b) => b,
        Err(e) => return Ok(Err(e)),
    };
    let body = body.as_str();
    if let Some(x) = new.external_id {
        // The same bound and the same byte rule as the body: an imported
        // identity is foreign text and gets no more trust than one
        // somebody typed.
        if x.is_empty() || x.len() > MAX_EXTERNAL_ID_LEN {
            return Ok(Err(format!(
                "external id must be 1..{MAX_EXTERNAL_ID_LEN} bytes"
            )));
        }
        if x.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Ok(Err("external id contains control characters".into()));
        }
    }

    // The thread rules come before the anchor rules, because a reply has
    // no anchor to check: validating one it may not set would report the
    // wrong thing entirely.
    let root = match new.parent_id {
        Some(pid) => {
            let Some(p) = comment_by_id(db, pid)? else {
                return Ok(Err("no such comment to reply to".into()));
            };
            if p.change_id != new.change_id {
                return Ok(Err("that comment belongs to another change".into()));
            }
            // One level, and this is the only place it is enforced —
            // the column cannot say it without a trigger. A tree is a
            // forum; a review has to stay readable top to bottom.
            if p.parent_id.is_some() {
                return Ok(Err(
                    "replies are one level deep: reply to the thread, not to a reply".into(),
                ));
            }
            // A thread that has not been published is not a thread yet,
            // and it answers with the *absent* sentence rather than a
            // truthful one: telling a stranger "that comment exists but
            // is a draft" is the leak this feature must not have, in
            // the one place an id could probe for it. Drafting a reply
            // into a published thread is the ordinary case and is fine
            // — it is the parent's publication that matters, not the
            // reply's.
            if p.published_at.is_none() {
                return Ok(Err("no such comment to reply to".into()));
            }
            if new.path.is_some()
                || new.line.is_some()
                || new.line_end.is_some()
                || new.side != Side::New
            {
                return Ok(Err("a reply inherits its thread's anchor".into()));
            }
            Some(p)
        }
        None => None,
    };

    let (path, line, line_end, side, original_line, original_patchset) = match &root {
        Some(p) => (
            p.path.clone(),
            p.line,
            p.line_end,
            p.side,
            p.original_line,
            p.original_patchset,
        ),
        None => {
            if new.side == Side::Old && new.path.is_none() {
                // The old side of *what*? Without a file there is no
                // pre-image to count lines in, so the anchor would name
                // nothing while looking like it named something.
                return Ok(Err("an old-side comment needs a path".into()));
            }
            if let Some(n) = new.line {
                if new.path.is_none() {
                    return Ok(Err("a line comment needs a path".into()));
                }
                if !(1..=MAX_COMMENT_LINE).contains(&n) {
                    return Ok(Err(format!("line must be 1..{MAX_COMMENT_LINE}")));
                }
            }
            if let Some(end) = new.line_end {
                let Some(start) = new.line else {
                    return Ok(Err("a line range needs a start line".into()));
                };
                if !(1..=MAX_COMMENT_LINE).contains(&end) {
                    return Ok(Err(format!("line_end must be 1..{MAX_COMMENT_LINE}")));
                }
                if end < start {
                    return Ok(Err("line_end must not precede line".into()));
                }
            }
            let path = new.path.map(str::to_string);
            // A single-line comment is the range [line, line], stored
            // that way rather than left NULL: a reader that has to
            // COALESCE is a reader that will one day forget to.
            let end = new.line_end.or(new.line);
            (
                path,
                new.line,
                end,
                new.side,
                new.line,
                Some(new.patchset_number),
            )
        }
    };

    let id = ulid();
    let now = now_ms();
    // A drafted comment is published when its review is submitted, so
    // it carries no `published_at` yet; everything else is published as
    // it is written, which is what every comment before this migration
    // was.
    let published_at = new.review_id.is_none().then_some(now);
    let res = db.lock().execute(
        "INSERT INTO change_comments (id, change_id, patchset_number, \
             author_principal, author_user_id, parent_id, path, line, line_end, side, \
             original_line, original_patchset, external_id, review_id, published_at, \
             body, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
        &[
            &id,
            &new.change_id,
            &new.patchset_number,
            &new.author_principal,
            &new.author_user_id,
            &new.parent_id,
            &path,
            &line,
            &line_end,
            &side.as_str(),
            &original_line,
            &original_patchset,
            &new.external_id,
            &new.review_id,
            &published_at,
            &body,
            &now,
        ],
    );
    if let Err(e) = res {
        // An import replayed over its own earlier run: the row is
        // already here under the same foreign identity, which is the
        // entire reason that column exists. A refusal in words, so the
        // importer can skip it rather than abort the run.
        if is_unique_violation(&e) {
            return Ok(Err("a comment with that external id already exists".into()));
        }
        return Err(format!("add comment: {e}"));
    }
    // The response is the comment as readers will see it, author
    // resolved — a person must never come back labeled "service".
    let author = match new.author_user_id {
        Some(uid) => crate::users::by_id(db, uid)?,
        None => None,
    };
    Ok(Ok(Comment {
        id,
        change_id: new.change_id.to_string(),
        patchset_number: new.patchset_number,
        author_principal: new.author_principal.to_string(),
        author_user_id: new.author_user_id.map(str::to_string),
        author_email: author.as_ref().map(|u| u.email.clone()),
        author_name: author.map(|u| u.name),
        path,
        line,
        line_end,
        side,
        parent_id: new.parent_id.map(str::to_string),
        original_line,
        original_patchset,
        external_id: new.external_id.map(str::to_string),
        review_id: new.review_id.map(str::to_string),
        published_at,
        resolved_at: None,
        resolved_by: None,
        resolved_by_name: None,
        body: body.to_string(),
        created_at: now,
    }))
}

/// Resolve or unresolve a thread, returning it as it now stands.
///
/// `Ok(Ok(None))` means no such comment; `Ok(Err(_))` is a refusal in
/// words. **Who** may call this is not decided here: it is an OWNERS
/// question about the commented path, and OWNERS lives in the server —
/// see `changes_api::may_resolve`. What is decided here is that only a
/// root can carry the state at all.
///
/// Resolving something already resolved keeps the first resolver and the
/// first timestamp. Two people pressing the button, or one person
/// pressing it twice, must not rewrite the record of who said "done".
pub fn set_resolved(
    db: &ControlDb,
    comment_id: &str,
    by_user_id: Option<&str>,
    resolved: bool,
) -> Result<Result<Option<Comment>, String>, String> {
    let Some(existing) = comment_by_id(db, comment_id)? else {
        return Ok(Ok(None));
    };
    if existing.parent_id.is_some() {
        return Ok(Err(
            "a reply cannot be resolved; resolve the thread it is in".into(),
        ));
    }
    let (at, by) = if resolved {
        if existing.resolved_at.is_some() {
            return Ok(Ok(Some(existing)));
        }
        (Some(now_ms()), by_user_id)
    } else {
        (None, None)
    };
    db.lock()
        .execute(
            "UPDATE change_comments SET resolved_at = $2, resolved_by = $3 WHERE id = $1",
            &[&comment_id, &at, &by],
        )
        .map_err(|e| format!("resolve comment: {e}"))?;
    comment_by_id(db, comment_id).map(Ok)
}

/// The conversation, oldest first — the order people read it in.
///
/// Flat, with `parent_id` on every row, rather than nested: the order a
/// review was spoken in is meaning (see 0017), and a nested shape would
/// have to invent a second ordering between threads. Grouping by
/// [`Comment::thread_id`] recovers the threads without losing that.
///
/// Published comments only. Everything that reads a conversation
/// *about* a change rather than *for* somebody — the notifier's
/// participant set, the change-views read — wants exactly this, because
/// an unsubmitted draft is not something its author has said yet.
pub fn comments_for(db: &ControlDb, change_id: &str) -> Result<Vec<Comment>, String> {
    comments_for_viewer(db, change_id, None)
}

/// The conversation as one person sees it: everything published, plus
/// that person's own unsubmitted drafts.
///
/// **This is the only place draft invisibility is enforced, and it must
/// stay that way.** A leaked unpublished comment is the worst bug this
/// feature can have — somebody's half-formed first reaction, published
/// under their name without them ever pressing submit — and a rule
/// spread across handlers is a rule the next handler forgets. So it is
/// one `WHERE` clause, [`comments_for`] delegates to it with no viewer,
/// and there is no third query to disagree with either.
///
/// `viewer` is a user id or `None` for "nobody in particular", which is
/// what an anonymous reader and a service principal both are here: a
/// token cannot hold a draft, so it can never be shown one.
pub fn comments_for_viewer(
    db: &ControlDb,
    change_id: &str,
    viewer: Option<&str>,
) -> Result<Vec<Comment>, String> {
    if !valid_id(change_id) {
        return Ok(Vec::new());
    }
    // An id-shaped viewer only. A malformed one is definitionally
    // nobody, the same masking contract every lookup here honours —
    // and it must not reach the query, where it would be a comparison
    // against a value that cannot exist rather than a refusal.
    let viewer = viewer.filter(|v| valid_id(v));
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {COMMENT_COLUMNS} WHERE c.change_id = $1 \
                 AND (c.published_at IS NOT NULL \
                      OR ($2::text IS NOT NULL AND c.author_user_id = $2)) \
                 ORDER BY c.seq"
            ),
            &[&change_id, &viewer],
        )
        .map_err(|e| format!("comments: {e}"))?;
    Ok(rows.iter().map(row_to_comment).collect())
}

/// What a reviewer says when they are done reading.
///
/// Three, and the third is the point of the whole table. Before it, the
/// only negative signal in the product was silence — which is exactly
/// what "hasn't looked yet" also looks like, so an author could not
/// tell a blocked change from an unread one, and a reviewer who wanted
/// to say "not like this" had no way to say it that the change itself
/// would remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewVerdict {
    /// Yes. Writes the `approvals` row sufficiency reads.
    Approve,
    /// Words, no verdict — the ordinary "here are my notes" pass.
    #[default]
    Comment,
    /// No. Durable across patchsets until its author withdraws it; what
    /// it blocks is decided where OWNERS is known, not here.
    RequestChanges,
}

impl ReviewVerdict {
    /// The stored spelling, which is also the wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewVerdict::Approve => "approve",
            ReviewVerdict::Comment => "comment",
            ReviewVerdict::RequestChanges => "request_changes",
        }
    }

    /// Parse the wire spelling. `None` for anything else — never
    /// coerced to `comment`, which would silently turn somebody's "no"
    /// into a shrug.
    pub fn parse(s: &str) -> Option<ReviewVerdict> {
        Some(match s {
            "approve" => ReviewVerdict::Approve,
            "comment" => ReviewVerdict::Comment,
            "request_changes" => ReviewVerdict::RequestChanges,
            _ => return None,
        })
    }
}

/// One review: a reviewer's pass over a patchset, as one act.
#[derive(Debug, Clone)]
pub struct Review {
    pub id: String,
    pub change_id: String,
    /// The patchset the verdict was about. A verdict about code
    /// describes the code it was about, exactly as an approval does.
    pub patchset_id: String,
    pub user_id: String,
    pub verdict: ReviewVerdict,
    pub body: Option<String>,
    /// `draft` until submitted, and never back again — a submitted
    /// review is on the record, and un-saying it is `withdraw`, which
    /// leaves a trace.
    pub state: String,
    pub submitted_at: Option<i64>,
    /// When a standing `request_changes` was taken back by its author.
    pub withdrawn_at: Option<i64>,
    pub external_id: Option<String>,
    pub created_at: i64,
    pub author_email: Option<String>,
    pub author_name: Option<String>,
}

/// One definition of the review row shape, so the query and
/// [`row_to_review`] cannot drift apart.
const REVIEW_COLUMNS: &str = "r.id, r.change_id, r.patchset_id, r.user_id, r.verdict, \
     r.body, r.state, r.submitted_at, r.withdrawn_at, r.external_id, r.created_at, \
     u.email, u.name \
     FROM reviews r LEFT JOIN users u ON u.id = r.user_id";

fn row_to_review(r: &postgres::Row) -> Review {
    let verdict: String = r.get("verdict");
    Review {
        id: r.get("id"),
        change_id: r.get("change_id"),
        patchset_id: r.get("patchset_id"),
        user_id: r.get("user_id"),
        // The column carries a CHECK constraint, so an unknown spelling
        // cannot be in the table; defaulting keeps the read total
        // rather than panicking on data that cannot exist.
        verdict: ReviewVerdict::parse(&verdict).unwrap_or_default(),
        body: r.get("body"),
        state: r.get("state"),
        submitted_at: r.get("submitted_at"),
        withdrawn_at: r.get("withdrawn_at"),
        external_id: r.get("external_id"),
        created_at: r.get("created_at"),
        author_email: r.get("email"),
        author_name: r.get("name"),
    }
}

/// The caller's own unsubmitted review on this change, if they have one.
pub fn pending_review(
    db: &ControlDb,
    change_id: &str,
    user_id: &str,
) -> Result<Option<Review>, String> {
    if !valid_id(change_id) || !valid_id(user_id) {
        return Ok(None);
    }
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {REVIEW_COLUMNS} WHERE r.change_id = $1 AND r.user_id = $2 \
                 AND r.state = 'draft'"
            ),
            &[&change_id, &user_id],
        )
        .map_err(|e| format!("pending review: {e}"))?;
    Ok(rows.first().map(row_to_review))
}

/// Start — or reopen, or re-title — the caller's pending review.
///
/// Get-or-create rather than create, and idempotent on purpose: the
/// client that drafts a comment does not want to know whether this is
/// the first one, and two tabs racing to draft the first remark of the
/// same pass must not end with two half-reviews. The database says so
/// too, through `reviews_one_draft`.
///
/// The draft is parked at `comment`; the verdict is chosen at submit
/// time, because it is the thing you know last.
pub fn open_review(
    db: &ControlDb,
    change_id: &str,
    patchset_id: &str,
    user_id: &str,
    body: Option<&str>,
) -> Result<Result<Review, String>, String> {
    if !valid_id(change_id) || !valid_id(user_id) || !valid_id(patchset_id) {
        return Ok(Err("no such change, patchset or user".into()));
    }
    let body = match body.map(|b| prose("review", b)).transpose() {
        Ok(b) => b,
        Err(e) => return Ok(Err(e)),
    };
    if let Some(existing) = pending_review(db, change_id, user_id)? {
        // A draft follows the change: a reviewer who started reading
        // patchset 1 and submits after patchset 2 arrives is submitting
        // about what they last had open, and the submit call re-pins it
        // anyway. What is saved here is the words.
        if body.is_some() {
            db.lock()
                .execute(
                    "UPDATE reviews SET body = $2 WHERE id = $1",
                    &[&existing.id, &body],
                )
                .map_err(|e| format!("open review: {e}"))?;
        }
        // Absent now means the same person discarded it in another tab
        // between the read and the write — nothing is wrong and nothing
        // was lost, but there is no draft to hand back and the caller
        // must not get somebody's stale copy of one.
        let r = read_back(pending_review(db, change_id, user_id)?)?;
        return Ok(Ok(r));
    }
    let id = ulid();
    let now = now_ms();
    db.lock()
        .execute(
            "INSERT INTO reviews (id, change_id, patchset_id, user_id, verdict, body, \
                 state, created_at) \
             VALUES ($1, $2, $3, $4, 'comment', $5, 'draft', $6) \
             ON CONFLICT DO NOTHING",
            &[&id, &change_id, &patchset_id, &user_id, &body, &now],
        )
        .map_err(|e| format!("open review: {e}"))?;
    Ok(Ok(read_back(pending_review(db, change_id, user_id)?)?))
}

/// A row this call has just written, read back for the identities the
/// caller needs joined in.
///
/// It cannot be absent: the write succeeded a statement ago. The only
/// way to see `None` is somebody deleting the row in the microseconds
/// between, and that is an error rather than a refusal — nothing the
/// caller did is wrong, and there is nothing they could do about it.
fn read_back(row: Option<Review>) -> Result<Review, String> {
    row.ok_or_else(|| "the review vanished between writing and reading it".to_string())
}

/// Throw the caller's pending review away, drafts and all.
///
/// The unpublished comments go with it — that is what "discard" means,
/// and leaving them behind would strand remarks nothing can ever
/// publish. `ON DELETE CASCADE` on `change_comments.review_id` does it,
/// and the constraint is the enforcement rather than this call, so a
/// second door cannot forget.
pub fn discard_review(db: &ControlDb, change_id: &str, user_id: &str) -> Result<bool, String> {
    if !valid_id(change_id) || !valid_id(user_id) {
        return Ok(false);
    }
    let n = db
        .lock()
        .execute(
            "DELETE FROM reviews WHERE change_id = $1 AND user_id = $2 AND state = 'draft'",
            &[&change_id, &user_id],
        )
        .map_err(|e| format!("discard review: {e}"))?;
    Ok(n > 0)
}

/// Every submitted review on a change, oldest first. Drafts are nobody
/// else's business and are never in here.
pub fn reviews_for(db: &ControlDb, change_id: &str) -> Result<Vec<Review>, String> {
    if !valid_id(change_id) {
        return Ok(Vec::new());
    }
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {REVIEW_COLUMNS} WHERE r.change_id = $1 AND r.state = 'submitted' \
                 ORDER BY r.submitted_at, r.id"
            ),
            &[&change_id],
        )
        .map_err(|e| format!("reviews: {e}"))?;
    Ok(rows.iter().map(row_to_review).collect())
}

/// Each reviewer's **latest** submitted word on this change, for the
/// callers that need to know who is still saying no.
///
/// Latest-per-person and not every-row, because a reviewer who blocked
/// on patchset 1 and approved on patchset 3 has changed their mind, and
/// a gate that read both would leave them blocking a change they signed
/// off. Withdrawal is the explicit form of the same thing and is
/// carried on the row; superseding is the implicit one and is this
/// query's shape.
///
/// Deliberately **not** filtered by patchset. An approval dies when new
/// code arrives, because the approver never saw it; a block must not,
/// or the author clears an objection by force-pushing over it — which
/// is exactly the move the objection existed to stop.
pub fn standing_blocks(db: &ControlDb, change_id: &str) -> Result<Vec<Review>, String> {
    if !valid_id(change_id) {
        return Ok(Vec::new());
    }
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {REVIEW_COLUMNS} WHERE r.id IN ( \
                     SELECT DISTINCT ON (user_id) id FROM reviews \
                     WHERE change_id = $1 AND state = 'submitted' \
                     ORDER BY user_id, submitted_at DESC, id DESC) \
                 AND r.verdict = 'request_changes' AND r.withdrawn_at IS NULL \
                 ORDER BY r.submitted_at, r.id"
            ),
            &[&change_id],
        )
        .map_err(|e| format!("standing blocks: {e}"))?;
    Ok(rows.iter().map(row_to_review).collect())
}

/// Everything one submitted review carries.
#[derive(Debug)]
pub struct SubmitReview<'a> {
    pub change_id: &'a str,
    pub patchset_id: &'a str,
    pub user_id: &'a str,
    pub verdict: ReviewVerdict,
    /// The cover message. Optional for `approve` and `comment`; a
    /// `request_changes` with neither words nor comments is refused.
    pub body: Option<&'a str>,
}

/// What submitting one review actually did.
#[derive(Debug)]
pub struct Submitted {
    pub review: Review,
    /// How many drafted comments became visible. This number is the
    /// answer to "did anything leak" in both directions, so it is
    /// returned rather than inferred.
    pub published: usize,
    /// Whether an `approvals` row was written, and whether one was
    /// revoked. Only one can be true.
    pub approved: bool,
    pub approval_revoked: bool,
}

/// Submit the caller's review: publish their drafts, record the
/// verdict, and move `approvals` to match — **all in one transaction**.
///
/// The atomicity is the feature, not a nicety. A crash between
/// publishing eleven comments and recording the twelfth would leave a
/// review that half happened, with no way for its author to finish it
/// or take it back; a crash between the verdict and the approval would
/// leave a change that says "approved" and cannot land, or worse, one
/// that can land and nobody approved.
///
/// [`approvals`](approve) stays the durable record sufficiency reads.
/// That seam is deliberate and load-bearing: a `verdict = 'approve'`
/// review writes exactly the row `POST …/approve` writes, so the
/// sufficiency engine, the land gate and the lander are untouched by
/// this feature and cannot disagree with it. `request_changes` revokes
/// any active approval by the same person on the same patchset —
/// nobody gets to both approve and block the same code — and `comment`
/// leaves approvals entirely alone, because notes are not a verdict.
///
/// Two audit rows on an approving review, on purpose: `change.review`
/// records the act, and `change.approve` records the approval, so an
/// auditor asking "who approved this" gets the same answer whichever
/// door it came through.
pub fn submit_review(
    db: &ControlDb,
    s: &SubmitReview<'_>,
    audit: &crate::audit::AuditCtx,
) -> Result<Result<Submitted, String>, String> {
    if !valid_id(s.user_id) || !valid_id(s.patchset_id) {
        return Ok(Err("no such patchset or user".into()));
    }
    let body = match s.body.map(|b| prose("review", b)).transpose() {
        Ok(b) => b,
        Err(e) => return Ok(Err(e)),
    };
    let repo_id = change_repo(db, s.change_id)?;
    let draft = pending_review(db, s.change_id, s.user_id)?;
    // A block with nothing in it is a wall with no door: the author is
    // told no and cannot learn what would make it a yes. Either the
    // cover message or the review's own comments have to say something.
    if s.verdict == ReviewVerdict::RequestChanges && body.is_none() {
        let drafted = match &draft {
            Some(d) => pending_comment_count(db, &d.id)?,
            None => 0,
        };
        if drafted == 0 {
            return Ok(Err(
                "asking for changes needs words: say what would make this a yes".into(),
            ));
        }
    }
    let id = draft.as_ref().map(|d| d.id.clone()).unwrap_or_else(ulid);
    let existed = draft.is_some();
    let now = now_ms();
    // The body the row ends with: new words replace old, and a submit
    // with no words keeps whatever the draft was saved with.
    let body = body.or_else(|| draft.as_ref().and_then(|d| d.body.clone()));
    let verdict = s.verdict.as_str();
    let (change_id, patchset_id, user_id) = (s.change_id, s.patchset_id, s.user_id);
    let outcome = db
        .lock()
        .transaction(move |tx| {
            if existed {
                tx.execute(
                    "UPDATE reviews SET verdict = $2, body = $3, state = 'submitted', \
                         submitted_at = $4, patchset_id = $5 \
                     WHERE id = $1",
                    &[&id, &verdict, &body, &now, &patchset_id],
                )?;
            } else {
                tx.execute(
                    "INSERT INTO reviews (id, change_id, patchset_id, user_id, verdict, \
                         body, state, submitted_at, created_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, 'submitted', $7, $7)",
                    &[
                        &id,
                        &change_id,
                        &patchset_id,
                        &user_id,
                        &verdict,
                        &body,
                        &now,
                    ],
                )?;
            }
            // The one statement that turns a private pass into a public
            // one. Every drafted comment becomes visible at the same
            // instant, which is what makes a review one act.
            let published = tx.execute(
                "UPDATE change_comments SET published_at = $2 \
                 WHERE review_id = $1 AND published_at IS NULL",
                &[&id, &now],
            )? as usize;

            let mut approved = false;
            let mut approval_revoked = false;
            match verdict {
                "approve" => {
                    let existing = tx.query_opt(
                        "SELECT id FROM approvals \
                         WHERE patchset_id = $1 AND user_id = $2 AND revoked_at IS NULL",
                        &[&patchset_id, &user_id],
                    )?;
                    if existing.is_none() {
                        let aid = ulid();
                        tx.execute(
                            "INSERT INTO approvals (id, change_id, patchset_id, user_id, \
                                 created_at) VALUES ($1, $2, $3, $4, $5)",
                            &[&aid, &change_id, &patchset_id, &user_id, &now],
                        )?;
                        approved = true;
                        let blob = serde_json::json!({
                            "change_id": change_id, "patchset_id": patchset_id,
                        });
                        crate::audit::record_tx(
                            tx,
                            audit,
                            repo_id.as_deref(),
                            "change.approve",
                            Some(&blob),
                        )?;
                    }
                }
                "request_changes" => {
                    // Nobody approves and blocks the same code. Saying
                    // no takes back the yes, in the same instant, so no
                    // reader ever sees both.
                    approval_revoked = tx.execute(
                        "UPDATE approvals SET revoked_at = $3 \
                         WHERE patchset_id = $1 AND user_id = $2 AND revoked_at IS NULL",
                        &[&patchset_id, &user_id, &now],
                    )? > 0;
                }
                // `comment` touches approvals not at all: notes are not
                // a verdict, and a reviewer leaving some must not have
                // their standing approval quietly withdrawn.
                _ => {}
            }
            let blob = serde_json::json!({
                "change_id": change_id,
                "patchset_id": patchset_id,
                "verdict": verdict,
                "published": published,
            });
            crate::audit::record_tx(tx, audit, repo_id.as_deref(), "change.review", Some(&blob))?;
            Ok((id, published, approved, approval_revoked))
        })
        .map_err(|e| format!("submit review: {e}"))?;

    let (id, published, approved, approval_revoked) = outcome;
    let review = read_back(review_by_id(db, &id)?)?;
    Ok(Ok(Submitted {
        review,
        published,
        approved,
        approval_revoked,
    }))
}

/// One review by id, identity joined in.
pub fn review_by_id(db: &ControlDb, review_id: &str) -> Result<Option<Review>, String> {
    if !valid_id(review_id) {
        return Ok(None);
    }
    let rows = db
        .lock()
        .query(
            &format!("SELECT {REVIEW_COLUMNS} WHERE r.id = $1"),
            &[&review_id],
        )
        .map_err(|e| format!("review: {e}"))?;
    Ok(rows.first().map(row_to_review))
}

/// How many comments are drafted into a review and not yet published.
fn pending_comment_count(db: &ControlDb, review_id: &str) -> Result<i64, String> {
    let rows = db
        .lock()
        .query(
            "SELECT COUNT(*) AS n FROM change_comments \
             WHERE review_id = $1 AND published_at IS NULL",
            &[&review_id],
        )
        .map_err(|e| format!("pending comments: {e}"))?;
    Ok(rows.first().map(|r| r.get::<_, i64>("n")).unwrap_or(0))
}

/// Take back one's standing request for changes.
///
/// Only its author, and only their own — which is why there is no
/// `by_user_id` parameter distinct from the reviewer. A block that
/// somebody else could clear is not a block, and the whole argument for
/// `request_changes` surviving a new patchset is that it ends when its
/// author says it does.
///
/// The row is kept with `withdrawn_at` set rather than deleted: "Alice
/// asked for changes and later withdrew it" is review history, and a
/// delete would render it as though she never objected.
pub fn withdraw_review(
    db: &ControlDb,
    change_id: &str,
    user_id: &str,
    audit: &crate::audit::AuditCtx,
) -> Result<bool, String> {
    if !valid_id(change_id) || !valid_id(user_id) {
        return Ok(false);
    }
    let now = now_ms();
    let repo_id = change_repo(db, change_id)?;
    db.lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "UPDATE reviews SET withdrawn_at = $3 \
                 WHERE change_id = $1 AND user_id = $2 AND state = 'submitted' \
                   AND verdict = 'request_changes' AND withdrawn_at IS NULL",
                &[&change_id, &user_id, &now],
            )?;
            if n > 0 {
                let blob = serde_json::json!({ "change_id": change_id });
                crate::audit::record_tx(
                    tx,
                    audit,
                    repo_id.as_deref(),
                    "change.review.withdraw",
                    Some(&blob),
                )?;
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("withdraw review: {e}"))
}

/// One CI verdict on a patchset. `posted_by` is the acting principal —
/// usually a service token, honestly recorded as one.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub state: String,
    pub detail_url: Option<String>,
    pub posted_by: String,
    pub updated_at: i64,
}

/// Check names are short CI-suite labels like "ci/tests" (I13: bounded,
/// printable, no shell or path metacharacters beyond '/').
pub const MAX_CHECK_NAME: usize = 100;
pub fn valid_check_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_CHECK_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:".contains(&b))
        && !name.contains("..")
}

/// A detail link points a reader at the run; anything but http(s) is a
/// mistake or an attack, refused in words.
pub const MAX_CHECK_URL: usize = 1000;
fn valid_check_url(url: &str) -> bool {
    url.len() <= MAX_CHECK_URL
        && (url.starts_with("http://") || url.starts_with("https://"))
        && url.bytes().all(|b| b.is_ascii_graphic())
}

/// The three states a **patchset-scoped** check may be in.
///
/// Exported because it is half of a split that a caller cannot see from
/// either half alone: `check_runs` takes six words (see
/// `checks::RunState`) and `change_checks` takes these three, and one
/// door — the signed CI intake — leads to both depending on whether the
/// body names a change. A refusal that lists one vocabulary without
/// saying which scope it belongs to sends a reporter to change a word
/// when the thing to change is the shape of their request.
///
/// Named here rather than restated there so the two cannot drift: this
/// is the array the guard below tests against.
pub const PATCHSET_CHECK_STATES: [&str; 3] = ["pending", "passing", "failing"];

/// Record (or update in place) one named check on a patchset. Returns
/// Ok(Ok(new)) where new = the first report under this name; the inner
/// Err is a refusal in words for the caller's 400.
pub fn set_check(
    db: &ControlDb,
    change_id: &str,
    patchset_id: &str,
    name: &str,
    state: &str,
    detail_url: Option<&str>,
    posted_by: &str,
) -> Result<Result<bool, String>, String> {
    if !valid_check_name(name) {
        return Ok(Err(format!("invalid check name {name:?}")));
    }
    if !PATCHSET_CHECK_STATES.contains(&state) {
        return Ok(Err(format!(
            "state must be pending, passing or failing, not {state:?}"
        )));
    }
    if let Some(u) = detail_url {
        if !valid_check_url(u) {
            return Ok(Err("url must be http(s) and at most 1000 bytes".into()));
        }
    }
    if !valid_id(patchset_id) || !valid_id(change_id) {
        return Ok(Err("no such patchset".into()));
    }
    let id = ulid();
    let now = now_ms();
    let n = db
        .lock()
        .execute(
            "INSERT INTO change_checks \
             (id, change_id, patchset_id, name, state, detail_url, posted_by, \
              created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8) \
             ON CONFLICT (patchset_id, name) DO UPDATE SET \
             state = EXCLUDED.state, detail_url = EXCLUDED.detail_url, \
             posted_by = EXCLUDED.posted_by, updated_at = EXCLUDED.updated_at \
             WHERE change_checks.id IS DISTINCT FROM EXCLUDED.id",
            &[
                &id,
                &change_id,
                &patchset_id,
                &name,
                &state,
                &detail_url,
                &posted_by,
                &now,
            ],
        )
        .map_err(|e| format!("set check: {e}"))?;
    // The upsert always writes one row; "new" is whether our fresh id
    // survived (an update keeps the original row's id).
    let created: bool = db
        .lock()
        .query_opt("SELECT 1 AS x FROM change_checks WHERE id = $1", &[&id])
        .map_err(|e| format!("set check: {e}"))?
        .is_some();
    let _ = n;
    Ok(Ok(created))
}

/// Every check on one patchset, alphabetical — the sidebar's order.
pub fn checks_for(db: &ControlDb, patchset_id: &str) -> Result<Vec<Check>, String> {
    if !valid_id(patchset_id) {
        return Ok(Vec::new());
    }
    let rows = db
        .lock()
        .query(
            "SELECT name, state, detail_url, posted_by, updated_at \
             FROM change_checks WHERE patchset_id = $1 ORDER BY name",
            &[&patchset_id],
        )
        .map_err(|e| format!("checks: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| Check {
            name: r.get("name"),
            state: r.get("state"),
            detail_url: r.get("detail_url"),
            posted_by: r.get("posted_by"),
            updated_at: r.get("updated_at"),
        })
        .collect())
}

/// The first failing check on a patchset, if any — what blocks landing.
pub fn failing_check(db: &ControlDb, patchset_id: &str) -> Result<Option<String>, String> {
    if !valid_id(patchset_id) {
        return Ok(None);
    }
    db.lock()
        .query_opt(
            "SELECT name FROM change_checks \
             WHERE patchset_id = $1 AND state = 'failing' ORDER BY name LIMIT 1",
            &[&patchset_id],
        )
        .map_err(|e| format!("checks: {e}"))
        .map(|r| r.map(|r| r.get("name")))
}

/// **What a check row is a statement about** — which is the fact the
/// merge turns on, and the one a reader needs when a name is red.
///
/// The two sources are the same namespace by design — see the
/// `required_checks` migration — so when one name reports twice, a
/// reader has to be able to tell which row they are looking at.
///
/// Named for the *scope* rather than the writer, because the writer is
/// not a partition. `check_runs` has two of them: the provider poller,
/// and the HMAC CI intake that signs a verdict in per commit. Calling
/// that side "polled" — as this enum did while nothing could read it —
/// would have sent somebody to a GitHub Actions tab to look for a run
/// their own CI posted. [`ChangeCheck::posted_by`] carries the writer,
/// which is the question "where do I go and look?" actually wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckSource {
    /// `change_checks`: posted against **this patchset**, so it can only
    /// ever be a statement about the revision under review.
    Patchset,
    /// `check_runs`: keyed on the **commit sha** alone. Written by the
    /// provider poller or by the HMAC CI intake — `posted_by` says
    /// which — and it outlives the commit being rewritten away.
    Commit,
}

impl CheckSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckSource::Patchset => "patchset",
            CheckSource::Commit => "commit",
        }
    }
}

/// One check as the change page and the land gate see it, after the two
/// sources have been merged.
#[derive(Debug, Clone)]
pub struct ChangeCheck {
    pub name: String,
    /// The raw stored state. `change_checks` says one of
    /// `pending`/`passing`/`failing`; `check_runs` adds `queued`,
    /// `running`, `cancelled` and `skipped`. Not normalised on the way
    /// out: the vocabularies are the providers' own words, and
    /// flattening `cancelled` into `failing` here would lose the only
    /// thing that tells a person why it is red.
    pub state: String,
    pub detail_url: Option<String>,
    /// Named in `required_checks` for this change's target branch.
    pub required: bool,
    pub source: CheckSource,
    /// Who says so, in the words the reader can act on: the intake's
    /// `posted_by` for a posted row, and the **provider** for a polled
    /// one. Not the same fact as [`source`], and both are needed — two
    /// deployments can post through the same intake, and one provider
    /// can be polled for several workflows.
    pub posted_by: String,
    /// When this row was last written, from whichever side wrote it.
    pub updated_at: i64,
}

/// Every check bearing on a change: the intake's rows for the **latest**
/// patchset, unioned with the polled runs for that patchset's commit,
/// deduplicated by name and marked against the target branch's
/// requirements. Ordered by name.
///
/// **When both sources report one name, the patchset row wins.** It is
/// keyed on the patchset, so it can only ever be a statement about the
/// revision under review; a `check_runs` row is joined on the commit sha
/// alone, is written by whichever providers report that sha, and
/// outlives the commit being rewritten away. When two systems claim
/// one name, the narrower-scoped statement is the one about *this*
/// review. The rejected alternative was "newest `updated_at` wins",
/// which makes a merge gate depend on clock skew between two writers
/// and is unexplainable to the person reading the row.
pub fn checks_for_change(db: &ControlDb, change_id: &str) -> Result<Vec<ChangeCheck>, String> {
    Ok(merged_checks(db, change_id)?.0)
}

/// The same merged read, **with the requirement list beside it**.
///
/// The rows alone cannot answer "may this land". A required check that
/// has never reported has no row — that is what never-reported means —
/// so a caller holding only [`checks_for_change`] sees a list in which
/// every entry is green and concludes everything passed, which is the
/// one wrong answer. [`land_gate`] has always read both halves; this is
/// how a *reader* gets them.
///
/// Both come out of one [`merged_checks`] call rather than two reads, so
/// a requirement added between them cannot produce a pair that describes
/// no state the database was ever in.
pub fn checks_and_required_for_change(
    db: &ControlDb,
    change_id: &str,
) -> Result<(Vec<ChangeCheck>, Vec<String>), String> {
    merged_checks(db, change_id)
}

/// The merged read and the requirement list it was marked against, in
/// one pass. The gate needs both — the requirements to spot a name that
/// has never reported at all — and reading them twice would let a
/// requirement added between the two queries produce a verdict that
/// matches neither state.
fn merged_checks(
    db: &ControlDb,
    change_id: &str,
) -> Result<(Vec<ChangeCheck>, Vec<String>), String> {
    let Some(change) = by_id(db, change_id)? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let required: std::collections::BTreeSet<String> =
        crate::protections::required_checks(db, &change.repo_id, &change.target_branch)?
            .into_iter()
            .map(|r| r.name)
            .collect();

    // BTreeMap so the merge and the ordering are the same fact.
    let mut merged: std::collections::BTreeMap<String, ChangeCheck> = Default::default();
    if let Some(ps) = latest_patchset(db, change_id)? {
        for run in crate::checks::latest_for_commit(db, &change.repo_id, &ps.commit_oid)? {
            merged.insert(
                run.name.clone(),
                ChangeCheck {
                    name: run.name,
                    state: run.state,
                    detail_url: run.detail_url,
                    required: false,
                    source: CheckSource::Commit,
                    posted_by: run.provider,
                    updated_at: run.updated_at,
                },
            );
        }
        // Second, so a collision resolves to the patchset row.
        for c in checks_for(db, &ps.id)? {
            merged.insert(
                c.name.clone(),
                ChangeCheck {
                    name: c.name,
                    state: c.state,
                    detail_url: c.detail_url,
                    required: false,
                    source: CheckSource::Patchset,
                    posted_by: c.posted_by,
                    updated_at: c.updated_at,
                },
            );
        }
    }
    let mut out: Vec<ChangeCheck> = merged.into_values().collect();
    for c in &mut out {
        c.required = required.contains(&c.name);
    }
    Ok((out, required.into_iter().collect()))
}

/// Whether a change may enter the land queue, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandGate {
    /// Nothing stands in the way.
    Ready,
    /// A verdict says no. Ejecting is correct; waiting will not help.
    Blocked { reason: String },
    /// A required check has not reached a verdict yet. The caller holds
    /// the change in the queue rather than ejecting it — the whole
    /// point of the distinction, and what turns "CI must not have
    /// failed yet" into "CI must pass".
    Waiting { on: Vec<String> },
}

/// Evaluate the gate for a change.
///
/// - Every **required** check must be `passing`. `failing`,
///   `cancelled` — or `skipped`, which means the provider decided
///   nothing was checked, and "must be passing" does not admit "did not
///   run" — blocks.
/// - A required check that is `pending`/`queued`/`running`, **or that
///   has not reported at all**, is [`LandGate::Waiting`].
///
///   The never-reported case is the ordinary one, not the exotic one:
///   push, press Land, CI has not started yet. Blocking there ejects
///   immediately and the author presses Land again once CI is green,
///   which is most of what this feature exists to abolish — "press Land
///   and walk away" was the requirement. A name that will *never*
///   report (an admin's typo) is caught by the caller's wait budget
///   instead, and caught better: `waited 30m for 'ci/tets', which never
///   reported` puts the typo next to the evidence that nothing was ever
///   going to report it, where `has not reported` at press time says
///   only that something is missing.
/// - A **non-required** check that is `failing` still blocks, so
///   nothing that blocks today stops blocking. Only `failing`: a
///   cancelled or skipped run nobody required has never blocked
///   anything and must not start.
/// - No required checks configured → exactly the old
///   [`failing_check`] behaviour.
///
/// `Blocked` beats `Waiting`: waiting on a build cannot rescue a change
/// something has already said no to.
///
/// **Unresolved comment threads are not here, and that is deliberate.**
/// This is the place a reader will come looking for them, so: the gate
/// blocks on things somebody said on purpose — a check that reported
/// red, and (in time) a reviewer who asked for changes. An unresolved
/// thread is neither. It is a rendered fact — the change view counts
/// them, the author works through them — and making it a hard blocker
/// is how "resolve everything" stops being information about a review
/// and becomes a ritual performed on the way to landing, with the nits
/// resolved unread. See migration 0050 for the columns.
pub fn land_gate(db: &ControlDb, change_id: &str) -> Result<LandGate, String> {
    let (checks, required) = merged_checks(db, change_id)?;

    let mut blocked: Vec<String> = Vec::new();
    let mut waiting: Vec<String> = Vec::new();
    for c in &checks {
        let why = match (c.required, c.state.as_str()) {
            (_, "passing") => None,
            (true, "failing") => Some(format!("required check '{}' is failing", c.name)),
            (true, "cancelled") => Some(format!("required check '{}' was cancelled", c.name)),
            (true, "skipped") => Some(format!(
                "required check '{}' was skipped, so nothing was checked",
                c.name
            )),
            (true, _) => {
                // pending / queued / running: a verdict is still coming.
                waiting.push(c.name.clone());
                None
            }
            (false, "failing") => Some(format!("check '{}' is failing", c.name)),
            (false, _) => None,
        };
        if let Some(why) = why {
            blocked.push(why);
        }
    }
    // A required check nobody has reported at all is still the case a
    // failing-only gate let straight through — "nothing has failed" is
    // true and means nothing — but the answer is to wait for it, not to
    // eject. Before CI has posted anything, a required name and a
    // required name that is queued are the same situation to an author,
    // and they must not have different outcomes.
    let reported: std::collections::BTreeSet<&str> =
        checks.iter().map(|c| c.name.as_str()).collect();
    for name in &required {
        if !reported.contains(name.as_str()) {
            waiting.push(name.clone());
        }
    }

    if !blocked.is_empty() {
        blocked.sort();
        return Ok(LandGate::Blocked {
            reason: blocked.join("; "),
        });
    }
    if !waiting.is_empty() {
        waiting.sort();
        return Ok(LandGate::Waiting { on: waiting });
    }
    Ok(LandGate::Ready)
}

/// Point a landing change at the job now driving it.
///
/// A compare-and-set on `land_job_id`, not an assignment. The reaper
/// creates a job and then adopts it, and two nodes sweeping the same
/// stranded change in the same instant both create one; whichever
/// adopts against the `land_job_id` it actually read wins, and the loser
/// tidies its job away. An unconditional `SET` would let both win and
/// leave two landers driving one change — the exact thing the `landing`
/// state exists to prevent.
///
/// `expect` is the job id the caller read, `None` for a change with no
/// job recorded at all. `IS NOT DISTINCT FROM` so that NULL compares as
/// a value: with `=`, the `None` case matches nothing and such a change
/// could never be adopted, so the reaper would sweep it forever.
///
/// Guarded on `state = 'landing'` for the same reason
/// [`set_land_waiting`] is: a change that landed or ejected while the
/// caller was making the job row must not be dragged back into the
/// queue.
///
/// Does not touch `updated_at` — same argument as [`set_land_waiting`].
/// Which job is driving a change is bookkeeping, not a modification a
/// reader should see as recent activity.
pub fn adopt_land_job(
    db: &ControlDb,
    change_id: &str,
    expect: Option<&str>,
    new_job_id: &str,
) -> Result<bool, String> {
    db.lock()
        .execute(
            "UPDATE changes SET land_job_id = $3 \
             WHERE id = $1 AND state = 'landing' AND land_job_id IS NOT DISTINCT FROM $2",
            &[&change_id, &expect, &new_job_id],
        )
        .map_err(|e| format!("adopt land job: {e}"))
        .map(|n| n > 0)
}

/// Changes stuck in `landing` with no live job driving them.
///
/// A change in `landing` is supposed to have a `land` job driving it.
/// When that job has finished — completed after a hold, failed,
/// dead-lettered, or vanished with its row — nothing re-enqueues it and
/// the change is stuck in a state no other writer accepts:
/// [`set_landing`] wants `open`, [`abandon`] wants `open`, and
/// [`set_ejected`] only ever runs from inside a land job. Recovery was a
/// manual `UPDATE`.
///
/// `idle_before_ms` is a grace: a job that finished a moment ago may
/// simply be between statements, and reaping it would double-drive the
/// change. It is compared against the **job's** `updated_at`, falling
/// back to the change's when there is no job row to look at.
///
/// `j.state IN ('done', 'failed')` and deliberately not `NOT IN
/// ('queued', 'running')`: a job state nobody has thought of yet must
/// read as "still driving" and be left alone, not swept. The same
/// reasoning as the check-run parser degrading an unknown verdict to
/// `queued` — an unrecognised state is never permission to act.
///
/// A member of a changeset that is `landing` is not here, however dead
/// its job: it is in `landing` because the *changeset* is, and the
/// changeset's own reaper (`changesets::stranded_landings`) resumes it
/// from the recorded plan. Handing it to a single-change land job would
/// land or eject one member of an all-or-nothing landing on its own —
/// exactly the half-landed changeset the plan row exists to make
/// impossible.
pub fn stranded_landings(
    db: &ControlDb,
    idle_before_ms: i64,
    limit: i64,
) -> Result<Vec<Change>, String> {
    // The column list is derived from `CHANGE_COLS` rather than written
    // out again with `c.` in front: the join makes qualification
    // necessary, and a second hand-maintained copy of that list is a
    // silent drift waiting for the next column.
    let cols = CHANGE_COLS
        .split(", ")
        .map(|c| format!("c.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let limit = limit.clamp(1, 1000);
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {cols} FROM changes c LEFT JOIN jobs j ON j.id = c.land_job_id \
                 WHERE c.state = 'landing' \
                   AND (j.id IS NULL OR j.state IN ('done', 'failed')) \
                   AND COALESCE(j.updated_at, c.updated_at) < $1 \
                   AND NOT EXISTS (SELECT 1 FROM changeset_members m \
                                   JOIN changesets cs ON cs.id = m.changeset_id \
                                   WHERE m.change_id = c.id AND m.active \
                                     AND cs.state = 'landing') \
                 ORDER BY COALESCE(j.updated_at, c.updated_at), c.id LIMIT $2"
            ),
            &[&idle_before_ms, &limit],
        )
        .map_err(|e| format!("stranded landings: {e}"))?;
    Ok(rows.iter().map(row_to_change).collect())
}

fn change_repo(db: &ControlDb, change_id: &str) -> Result<Option<String>, String> {
    if !valid_id(change_id) {
        return Ok(None);
    }
    db.lock()
        .query_opt("SELECT repo_id FROM changes WHERE id = $1", &[&change_id])
        .map_err(|e| format!("change repo: {e}"))
        .map(|r| r.map(|r| r.get("repo_id")))
}

/// open → landing, recording the job that will drive it. False = the
/// change was not open (someone else moved it first).
pub fn set_landing(db: &ControlDb, change_id: &str, job_id: &str) -> Result<bool, String> {
    let now = now_ms();
    db.lock()
        .execute(
            "UPDATE changes SET state = 'landing', land_job_id = $2, land_verdict = NULL, \
             updated_at = $3 WHERE id = $1 AND state = 'open'",
            &[&change_id, &job_id, &now],
        )
        .map_err(|e| format!("set landing: {e}"))
        .map(|n| n > 0)
}

/// Every waiting note starts with this, and nothing else in
/// `land_verdict` ever does.
///
/// `land_verdict` carries two kinds of sentence now, and one column
/// holding two meanings is exactly how a reader — or a query written
/// next year — gets it wrong. **The rule is the prefix, not the state.**
/// A note written by [`set_land_waiting`] begins with `waiting on `; an
/// ejection or a landing verdict never does, because both are written
/// by [`set_ejected`] and [`set_landed`] from the lander's own words.
/// Reading `state` instead would work today and rot the moment a note
/// outlives the `landing` state by a millisecond — which it does, since
/// nothing clears it on the way out and the two writes are not atomic
/// with each other.
pub const LAND_WAITING_PREFIX: &str = "waiting on ";

/// Record *what a landing is still waiting for*, without moving the
/// change out of `landing`.
///
/// [`LandGate::Waiting`] means the queue has not given up: the change
/// must stay `landing` and be retried. But `set_landing` nulls
/// `land_verdict`, so an author watching a change held for half an hour
/// on a slow build saw a spinner and no reason at all, which is worse
/// than an ejection they could act on. This is the sentence they read.
///
/// `waiting_on` is the human part only — the caller passes
/// `"ci/tests, ci/lint"` and the stored note is
/// `"waiting on ci/tests, ci/lint"`. The prefix is added here rather
/// than trusted from the caller so that [`LAND_WAITING_PREFIX`] is
/// actually a discriminator and not a convention two crates have to
/// remember separately.
///
/// Three properties this is called on every poll of a held change for:
///
/// - **It does not touch `updated_at`.** A change that is merely
///   waiting has not been modified, and `updated_at` is read as
///   "when did this row last change" by surfaces that sort and page on
///   it. `landed_at` exists as a sibling for precisely this class of
///   bug; making a waiting change look freshly edited every thirty
///   seconds would be the same mistake in a new place.
/// - **The same note twice is a no-op**, not a second write:
///   `IS DISTINCT FROM` means the steady state of a long wait is zero
///   row writes per poll.
/// - **It is guarded on `state = 'landing'`**, so a concurrent eject or
///   land wins and this can never resurrect a note onto a change that
///   has already left the queue.
///
/// Returns whether the note actually changed. `false` is the ordinary
/// answer on a repeat poll and is not an error; it also covers the lost
/// race, which the caller finds out about from the state, not from here.
pub fn set_land_waiting(db: &ControlDb, change_id: &str, waiting_on: &str) -> Result<bool, String> {
    let note = format!("{LAND_WAITING_PREFIX}{waiting_on}");
    db.lock()
        .execute(
            "UPDATE changes SET land_verdict = $2 \
             WHERE id = $1 AND state = 'landing' AND land_verdict IS DISTINCT FROM $2",
            &[&change_id, &note],
        )
        .map_err(|e| format!("set land waiting: {e}"))
        .map(|n| n > 0)
}

/// → landed, with the trunk commit and the verdict in the words the API
/// shows. Accepted from `landing` (the lander won) and from `open` (the
/// stack reconciliation found the commit already on trunk).
pub fn set_landed(
    db: &ControlDb,
    change_id: &str,
    landed_commit: &str,
    verdict: &str,
) -> Result<bool, String> {
    let now = now_ms();
    db.lock()
        .execute(
            // `landed_at` beside `updated_at`, and not instead of it.
            // The two answer different questions and the difference is
            // invisible until it bites: `updated_at` is "when did this
            // row last change", which any later edit moves, and
            // `landed_at` is a fact about the change that never moves
            // again. Insights reads the second — reading the first made
            // a title edit on a landed change silently shift it into a
            // later period and inflate its time-to-merge, in a number
            // nobody could have checked against anything.
            "UPDATE changes SET state = 'landed', land_verdict = $2, landed_commit = $3, \
             updated_at = $4, landed_at = $4 WHERE id = $1 AND state IN ('open', 'landing')",
            &[&change_id, &verdict, &landed_commit, &now],
        )
        .map_err(|e| format!("set landed: {e}"))
        .map(|n| n > 0)
}

/// landing → open, with the ejection verdict recorded for the author to
/// read. False = the change was not landing.
pub fn set_ejected(db: &ControlDb, change_id: &str, verdict: &str) -> Result<bool, String> {
    let now = now_ms();
    db.lock()
        .execute(
            "UPDATE changes SET state = 'open', land_verdict = $2, land_job_id = NULL, \
             updated_at = $3 WHERE id = $1 AND state = 'landing'",
            &[&change_id, &verdict, &now],
        )
        .map_err(|e| format!("set ejected: {e}"))
        .map(|n| n > 0)
}

/// open → abandoned. False = not open.
pub fn abandon(
    db: &ControlDb,
    change_id: &str,
    audit: &crate::audit::AuditCtx,
) -> Result<bool, String> {
    let now = now_ms();
    let repo_id = change_repo(db, change_id)?;
    db.lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "UPDATE changes SET state = 'abandoned', updated_at = $2 \
                 WHERE id = $1 AND state = 'open'",
                &[&change_id, &now],
            )?;
            if n > 0 {
                let blob = serde_json::json!({ "change_id": change_id });
                crate::audit::record_tx(
                    tx,
                    audit,
                    repo_id.as_deref(),
                    "change.abandon",
                    Some(&blob),
                )?;
            }
            Ok(n > 0)
        })
        .map_err(|e| format!("abandon: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditCtx;
    use crate::members::{self, Role};
    use crate::registry::{self, NewRepo, RepoKind};
    use crate::users;

    fn world(hint: &str) -> (ControlDb, String, String, String, AuditCtx) {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let repo = registry::create_repo(
            &db,
            &org.id,
            &NewRepo {
                name: "app",
                kind: RepoKind::Native,
                description: None,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        let alice = users::create(&db, "alice@example.com", "Alice", None).unwrap();
        members::add(&db, &org.id, &alice.id, Role::Member, None).unwrap();
        let ctx = AuditCtx {
            principal: format!("user:{}", alice.id),
            user_id: Some(alice.id.clone()),
            org_id: org.id.clone(),
        };
        (db, org.id, repo.id, alice.id, ctx)
    }

    fn register(
        db: &ControlDb,
        org: &str,
        repo: &str,
        key: &str,
        commit: &str,
        title: &str,
        ctx: &AuditCtx,
    ) -> (Change, Patchset, bool) {
        create_or_update(
            db,
            org,
            repo,
            key,
            title,
            "main",
            commit,
            Some("p".repeat(40).as_str()),
            &format!("{title}\n\nChange-Id: {key}\n"),
            ctx.user_id.as_deref(),
            None,
            Some(ctx),
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn change_keys_are_shape_checked() {
        for good in ["Iabc123", "g0123abc", "a.b_c-d"] {
            assert!(valid_change_key(good), "{good}");
        }
        let long = "a".repeat(73);
        for bad in [
            "",
            "has space",
            "semi;colon",
            "quo\"te",
            long.as_str(),
            "nul\0",
        ] {
            assert!(!valid_change_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_change_lives_through_patchsets_idempotently() {
        let (db, org, repo, _alice, ctx) = world("changes-lifecycle");
        let (c, ps, new) = register(&db, &org, &repo, "Iaaa111", &"a".repeat(40), "one", &ctx);
        assert!(new);
        assert_eq!(c.state, "open");
        assert_eq!(ps.number, 1);
        // The same commit again is an ack, not patchset 2.
        let (_, ps_again, new) =
            register(&db, &org, &repo, "Iaaa111", &"a".repeat(40), "one", &ctx);
        assert!(!new);
        assert_eq!(ps_again.id, ps.id);
        // A different commit under the same key is the next patchset,
        // and the change's title follows the newest message.
        let (c2, ps2, new) = register(&db, &org, &repo, "Iaaa111", &"b".repeat(40), "two", &ctx);
        assert!(new);
        assert_eq!(ps2.number, 2);
        assert_eq!(c2.title, "two");
        assert_eq!(c2.id, c.id);
        assert_eq!(patchsets(&db, &c.id).unwrap().len(), 2);
        assert_eq!(latest_patchset(&db, &c.id).unwrap().unwrap().id, ps2.id);
        // A second key is its own change.
        let (other, _, _) = register(&db, &org, &repo, "Ibbb222", &"c".repeat(40), "other", &ctx);
        assert_ne!(other.id, c.id);
        assert_eq!(list(&db, &repo, None, 50).unwrap().len(), 2);
        assert_eq!(list(&db, &repo, Some("open"), 50).unwrap().len(), 2);
        // An invalid key is definitionally absent, never SQL.
        assert!(by_key(&db, &repo, "no key").unwrap().is_none());
        assert!(create_or_update(
            &db,
            &org,
            &repo,
            "bad key",
            "t",
            "main",
            &"d".repeat(40),
            None,
            "m",
            None,
            None,
            None
        )
        .unwrap()
        .is_err());
    }

    /// The org-wide page is the per-repo page's rules applied to a set
    /// of repositories: the same ordering, the same state filter, the
    /// same clamp — and nothing at all from a repository the caller was
    /// not given.
    #[test]
    fn an_org_wide_page_spans_the_repositories_it_is_given_and_no_others() {
        let (db, org, app, _alice, ctx) = world("changes-org-page");
        let other = registry::create_repo(
            &db,
            &org,
            &NewRepo {
                name: "web",
                kind: RepoKind::Native,
                description: None,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        let (a, _, _) = register(&db, &org, &app, "Iaaa111", &"a".repeat(40), "one", &ctx);
        let (w, _, _) = register(
            &db,
            &org,
            &other.id,
            "Ibbb222",
            &"b".repeat(40),
            "two",
            &ctx,
        );
        abandon(&db, &w.id, &ctx).unwrap();

        let both = vec![app.clone(), other.id.clone()];
        let ids = |q: &Page<'_>| -> Vec<String> {
            list_in_org(&db, &both, q)
                .unwrap()
                .rows
                .iter()
                .map(|c| c.id.clone())
                .collect()
        };
        // Newest first, by id — `w` was registered second.
        assert_eq!(
            ids(&Page {
                limit: 50,
                ..Default::default()
            }),
            vec![w.id.clone(), a.id.clone()]
        );
        assert_eq!(
            ids(&Page {
                state: Some("open"),
                limit: 50,
                ..Default::default()
            }),
            vec![a.id.clone()]
        );
        assert_eq!(
            ids(&Page {
                state: Some("abandoned"),
                limit: 50,
                ..Default::default()
            }),
            vec![w.id.clone()]
        );
        // One repository is one repository's changes.
        assert_eq!(
            list_in_org(
                &db,
                std::slice::from_ref(&app),
                &Page {
                    limit: 50,
                    ..Default::default()
                }
            )
            .unwrap()
            .rows
            .iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>(),
            vec![a.id.clone()]
        );
        // A caller who may read nothing gets nothing — never everything.
        assert!(list_in_org(
            &db,
            &[],
            &Page {
                limit: 50,
                ..Default::default()
            }
        )
        .unwrap()
        .rows
        .is_empty());
        // The limit is a clamp, not a refusal, the same as `list`'s.
        for limit in [1, 0, -7] {
            assert_eq!(
                list_in_org(
                    &db,
                    &both,
                    &Page {
                        limit,
                        ..Default::default()
                    }
                )
                .unwrap()
                .rows
                .len(),
                1,
                "limit {limit}"
            );
        }
        // `list` is the same query over one repository, unpaged.
        assert_eq!(
            list(&db, &app, None, 50)
                .unwrap()
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>(),
            vec![a.id.clone()]
        );
    }

    /// The cursor names a row, so a change opened between two requests
    /// cannot make the next page skip one.
    ///
    /// This is the whole argument for keyset over `OFFSET`, and it is
    /// only visible with a write in the middle: rows arrive at the top of
    /// this ordering, so `OFFSET 2` after one insert re-reads a row it
    /// already returned and never reaches the one below it. Written as a
    /// walk of the *whole* list, asserting every row exactly once, rather
    /// than as two page fetches — the failure this pins is a gap, and a
    /// gap is invisible unless something counts the rows that came back.
    #[test]
    fn a_keyset_walk_sees_every_row_once_even_when_one_arrives_mid_walk() {
        let (db, org, repo, _alice, ctx) = world("changes-keyset");
        let mut made = Vec::new();
        for i in 0..5 {
            let (c, _, _) = register(
                &db,
                &org,
                &repo,
                &format!("Ikey0000{i}"),
                &format!("{i}").repeat(40),
                "one",
                &ctx,
            );
            made.push(c.id);
        }
        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut inserted_yet = false;
        loop {
            let page = list_in_org(
                &db,
                std::slice::from_ref(&repo),
                &Page {
                    after: cursor.as_deref(),
                    limit: 2,
                    ..Default::default()
                },
            )
            .unwrap();
            seen.extend(page.rows.iter().map(|c| c.id.clone()));
            // A sixth change, opened while the reader is halfway down.
            if !inserted_yet {
                inserted_yet = true;
                register(&db, &org, &repo, "Ikey00009", &"9".repeat(40), "late", &ctx);
            }
            match page.next {
                Some(n) => cursor = Some(n),
                None => break,
            }
        }
        made.reverse();
        assert_eq!(
            seen, made,
            "the walk returned every pre-existing row exactly once, newest \
             first, and the row added mid-walk — which sorts above the \
             cursor — displaced none of them"
        );

        // A cursor that cannot be an id names no row rather than paging
        // from the middle of nowhere: it is filtered out, so the page is
        // the first one.
        let page = list_in_org(
            &db,
            std::slice::from_ref(&repo),
            &Page {
                after: Some("not an id"),
                limit: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.rows[0].change_key, "Ikey00009");
    }

    /// `author:` filters on who opened the change, and an author nobody
    /// is returns nothing rather than everything.
    #[test]
    fn the_author_filter_is_the_person_who_opened_the_change() {
        let (db, org, repo, alice, ctx) = world("changes-author");
        register(&db, &org, &repo, "Iaut0001", &"a".repeat(40), "mine", &ctx);
        // A change nobody is recorded as opening — a service token's, or
        // an import's.
        create_or_update(
            &db,
            &org,
            &repo,
            "Iaut0002",
            "theirs",
            "main",
            &"b".repeat(40),
            None,
            "m",
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let ids = |author: Option<&str>| -> Vec<String> {
            list_in_org(
                &db,
                std::slice::from_ref(&repo),
                &Page {
                    author_user_id: author,
                    limit: 50,
                    ..Default::default()
                },
            )
            .unwrap()
            .rows
            .iter()
            .map(|c| c.change_key.clone())
            .collect()
        };
        assert_eq!(ids(None), vec!["Iaut0002", "Iaut0001"]);
        assert_eq!(ids(Some(&alice)), vec!["Iaut0001"]);
        // Somebody with nothing open here, not "everybody".
        assert!(ids(Some(&ulid())).is_empty());
    }

    #[test]
    fn approvals_are_per_patchset_and_survive_revoke_cycles() {
        let (db, org, repo, alice, ctx) = world("changes-approve");
        let (c, ps1, _) = register(&db, &org, &repo, "Iccc333", &"a".repeat(40), "one", &ctx);
        assert!(approve(&db, &c.id, &ps1.id, &alice, &ctx).unwrap());
        // Idempotent: approving again acks.
        assert!(!approve(&db, &c.id, &ps1.id, &alice, &ctx).unwrap());
        assert_eq!(approvals_for(&db, &ps1.id).unwrap().len(), 1);
        assert_eq!(
            approvals_for(&db, &ps1.id).unwrap()[0].email,
            "alice@example.com"
        );
        // Revoke, then approve again: the partial unique index only
        // covers active rows, so the cycle works.
        assert!(unapprove(&db, &c.id, &ps1.id, &alice, &ctx).unwrap());
        assert!(!unapprove(&db, &c.id, &ps1.id, &alice, &ctx).unwrap());
        assert!(approvals_for(&db, &ps1.id).unwrap().is_empty());
        assert!(approve(&db, &c.id, &ps1.id, &alice, &ctx).unwrap());
        // A new patchset starts with no approvals; the old rows stay put.
        let (_, ps2, _) = register(&db, &org, &repo, "Iccc333", &"b".repeat(40), "two", &ctx);
        assert!(approvals_for(&db, &ps2.id).unwrap().is_empty());
        assert_eq!(approvals_for(&db, &ps1.id).unwrap().len(), 1);
        // The trail recorded the authority moves.
        let entries = crate::audit::query(
            &db,
            &ctx.org_id,
            &crate::audit::AuditQuery {
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
        let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
        assert!(actions.contains(&"change.approve"), "{actions:?}");
        assert!(actions.contains(&"change.unapprove"), "{actions:?}");
    }

    #[test]
    fn state_transitions_are_guarded_and_report_lost_races() {
        let (db, org, repo, _alice, ctx) = world("changes-states");
        let (c, _, _) = register(&db, &org, &repo, "Iddd444", &"a".repeat(40), "one", &ctx);
        // Enqueue wins once; the second writer learns it lost.
        assert!(set_landing(&db, &c.id, "job1").unwrap());
        assert!(!set_landing(&db, &c.id, "job2").unwrap());
        // A landing change refuses new patchsets but still acks the
        // commit it already has.
        let refused = create_or_update(
            &db,
            &org,
            &repo,
            "Iddd444",
            "t",
            "main",
            &"b".repeat(40),
            None,
            "m",
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(refused.unwrap_err(), "change is landing");
        let acked = create_or_update(
            &db,
            &org,
            &repo,
            "Iddd444",
            "t",
            "main",
            &"a".repeat(40),
            None,
            "m",
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        assert!(!acked.2);
        // Ejection returns it to open with the verdict readable.
        assert!(set_ejected(&db, &c.id, "ejected: not fast-forward from abc123").unwrap());
        let back = by_key(&db, &repo, "Iddd444").unwrap().unwrap();
        assert_eq!(back.state, "open");
        assert_eq!(
            back.land_verdict.as_deref(),
            Some("ejected: not fast-forward from abc123")
        );
        assert!(back.land_job_id.is_none());
        // Land for real this time.
        assert!(set_landing(&db, &c.id, "job3").unwrap());
        assert!(set_landed(&db, &c.id, &"f".repeat(40), "landed").unwrap());
        assert!(!set_ejected(&db, &c.id, "late").unwrap());
        let done = by_key(&db, &repo, "Iddd444").unwrap().unwrap();
        assert_eq!(done.state, "landed");
        assert_eq!(done.landed_commit.as_deref(), Some("f".repeat(40).as_str()));
        // Landed is terminal for abandon too.
        assert!(!abandon(&db, &c.id, &ctx).unwrap());
        // A fresh open change can be abandoned, once.
        let (c2, _, _) = register(&db, &org, &repo, "Ieee555", &"c".repeat(40), "x", &ctx);
        assert!(abandon(&db, &c2.id, &ctx).unwrap());
        assert!(!abandon(&db, &c2.id, &ctx).unwrap());
        // And reconciliation may land an open change directly.
        let (c3, _, _) = register(&db, &org, &repo, "Ifff666", &"d".repeat(40), "y", &ctx);
        assert!(set_landed(&db, &c3.id, &"e".repeat(40), "landed: included in ffffff").unwrap());
    }

    /// Hostile-shaped ids are definitionally absent — answered, never
    /// round-tripped to SQL where a NUL would 500 instead of 404.
    #[test]
    fn hostile_ids_are_absent_not_errors() {
        let (db, _org, _repo, alice, ctx) = world("changes-hostile");
        assert!(by_id(&db, "not an id\0").unwrap().is_none());
        assert!(approvals_for(&db, "not an id").unwrap().is_empty());
        // The approve guard refuses before touching the tables.
        let e = approve(&db, "whatever", "bad ps", &alice, &ctx).unwrap_err();
        assert!(e.contains("no such patchset or user"), "{e}");
        assert!(!unapprove(&db, "whatever", "bad ps", &alice, &ctx).unwrap());
        // change_repo's guard, reached through abandon.
        assert!(!abandon(&db, "not an id", &ctx).unwrap());
    }

    /// The plainest comment there is: a service principal, no anchor.
    /// Every anchored case is this with `..` and the fields it varies,
    /// which keeps a test about `line_end` from restating ten fields
    /// that have nothing to do with `line_end`.
    fn say<'a>(change_id: &'a str, patchset: i64, body: &'a str) -> NewComment<'a> {
        NewComment {
            change_id,
            patchset_number: patchset,
            author_principal: "user:x",
            body,
            ..NewComment::default()
        }
    }

    /// The conversation: bounded, ordered, and honest about who spoke.
    #[test]
    fn comments_are_bounded_ordered_and_attributed() {
        let (db, org, repo, alice, ctx) = world("changes-comments");
        let (c, ps, _) = register(&db, &org, &repo, "Icc000001", &"a".repeat(40), "one", &ctx);
        // Words required, and not too many of them (I13).
        assert!(add_comment(&db, &say(&c.id, ps.number, "   "))
            .unwrap()
            .is_err());
        let long = "x".repeat(MAX_COMMENT_LEN + 1);
        assert!(add_comment(&db, &say(&c.id, ps.number, &long))
            .unwrap()
            .is_err());
        // Control bytes are refused in words, never surfaced as a
        // database error: a NUL would otherwise 500 at Postgres.
        for hostile in ["nul\0byte", "bell\x07", "del\x7f"] {
            assert!(
                add_comment(&db, &say(&c.id, ps.number, hostile))
                    .unwrap()
                    .is_err(),
                "{hostile:?}"
            );
        }
        // Newlines and tabs are prose, not attacks.
        assert!(
            add_comment(&db, &say(&c.id, ps.number, "line one\nline\ttwo"))
                .unwrap()
                .is_ok()
        );
        // A person and a service principal, in order.
        add_comment(
            &db,
            &NewComment {
                author_principal: &format!("user:{alice}"),
                author_user_id: Some(&alice),
                ..say(&c.id, ps.number, "needs tests")
            },
        )
        .unwrap()
        .unwrap();
        add_comment(
            &db,
            &NewComment {
                author_principal: "token:01ci",
                path: Some("a.rs"),
                line: Some(12),
                ..say(&c.id, ps.number, "perf suite regressed")
            },
        )
        .unwrap()
        .unwrap();
        let list = comments_for(&db, &c.id).unwrap();
        assert_eq!(list.len(), 3, "prose, person, service");
        assert_eq!(list[0].body, "line one\nline\ttwo");
        assert_eq!(list[1].body, "needs tests");
        assert_eq!(list[1].author_email.as_deref(), Some("alice@example.com"));
        assert_eq!(list[2].author_email, None);
        assert_eq!(list[2].path.as_deref(), Some("a.rs"));
        assert_eq!(list[2].line, Some(12));
        assert_eq!(list[2].patchset_number, ps.number);
        assert_eq!(list[0].line, None);
        // Hostile ids answer absence.
        assert!(comments_for(&db, "not an id").unwrap().is_empty());
    }

    /// The regression CI caught by losing a coin flip: two comments in
    /// the same millisecond ordered by (created_at, ulid), and a ulid's
    /// tail is random within one millisecond — so a fast conversation
    /// could render swapped. Order now comes from the insert sequence.
    /// A dozen back-to-back inserts land in very few milliseconds, so
    /// against the old ordering this fails with near certainty.
    #[test]
    fn a_burst_of_comments_reads_back_in_the_order_it_was_spoken() {
        let (db, org, repo, _alice, ctx) = world("changes-burst");
        let (c, ps, _) = register(&db, &org, &repo, "Icc000003", &"a".repeat(40), "one", &ctx);
        for i in 0..12 {
            add_comment(&db, &say(&c.id, ps.number, &format!("comment {i}")))
                .unwrap()
                .unwrap();
        }
        let bodies: Vec<String> = comments_for(&db, &c.id)
            .unwrap()
            .into_iter()
            .map(|cm| cm.body)
            .collect();
        let expected: Vec<String> = (0..12).map(|i| format!("comment {i}")).collect();
        assert_eq!(bodies, expected);
    }

    /// Checks: upsert by name per patchset, bounded shapes, and the
    /// failing lookup that gates landing.
    #[test]
    fn checks_upsert_by_name_and_report_the_first_failure() {
        let (db, org, repo, _alice, ctx) = world("changes-checks");
        let (c, ps, _) = register(&db, &org, &repo, "Icc000004", &"a".repeat(40), "one", &ctx);
        // First report is new; a re-post under the same name updates.
        assert!(set_check(
            &db,
            &c.id,
            &ps.id,
            "ci/tests",
            "pending",
            None,
            "token:01ci"
        )
        .unwrap()
        .unwrap());
        assert!(!set_check(
            &db,
            &c.id,
            &ps.id,
            "ci/tests",
            "failing",
            Some("https://ci.example.com/run/1"),
            "token:01ci",
        )
        .unwrap()
        .unwrap());
        assert!(
            set_check(&db, &c.id, &ps.id, "ci/perf", "passing", None, "token:01ci")
                .unwrap()
                .unwrap()
        );
        let listed = checks_for(&db, &ps.id).unwrap();
        assert_eq!(listed.len(), 2, "one row per name, updated in place");
        assert_eq!(listed[0].name, "ci/perf");
        assert_eq!(listed[1].name, "ci/tests");
        assert_eq!(listed[1].state, "failing");
        assert_eq!(
            listed[1].detail_url.as_deref(),
            Some("https://ci.example.com/run/1")
        );
        assert_eq!(
            failing_check(&db, &ps.id).unwrap().as_deref(),
            Some("ci/tests")
        );
        // Green it and the gate opens.
        set_check(
            &db,
            &c.id,
            &ps.id,
            "ci/tests",
            "passing",
            None,
            "token:01ci",
        )
        .unwrap()
        .unwrap();
        assert_eq!(failing_check(&db, &ps.id).unwrap(), None);

        // Shapes, refused in words (I13).
        for bad in ["", "a b", "x\0y", "a..b", &"n".repeat(MAX_CHECK_NAME + 1)] {
            let e = set_check(&db, &c.id, &ps.id, bad, "passing", None, "t")
                .unwrap()
                .unwrap_err();
            assert!(e.contains("invalid check name"), "{bad:?}: {e}");
        }
        let e = set_check(&db, &c.id, &ps.id, "ci/x", "green", None, "t")
            .unwrap()
            .unwrap_err();
        assert!(e.contains("state must be"), "{e}");
        for bad_url in ["ftp://x", "javascript:alert(1)", "http://x y"] {
            let e = set_check(&db, &c.id, &ps.id, "ci/x", "passing", Some(bad_url), "t")
                .unwrap()
                .unwrap_err();
            assert!(e.contains("http(s)"), "{bad_url:?}: {e}");
        }
        // Hostile ids answer absence, never SQL.
        assert!(checks_for(&db, "not an id\0").unwrap().is_empty());
        assert!(failing_check(&db, "not an id").unwrap().is_none());
        assert!(
            set_check(&db, &c.id, "bad id", "ci/x", "passing", None, "t")
                .unwrap()
                .is_err()
        );
    }

    /// A held landing says what it is holding for, in a sentence that
    /// cannot be mistaken for a verdict, without making the row look
    /// edited and without writing anything on a repeat poll.
    #[test]
    fn a_waiting_note_is_readable_cheap_and_not_a_verdict() {
        let (db, org, repo, _alice, ctx) = world("changes-land-waiting");
        let (c, _ps, _) = register(&db, &org, &repo, "Igate0009", &"5".repeat(40), "one", &ctx);
        assert!(set_landing(&db, &c.id, "job-1").unwrap());
        let landing = by_id(&db, &c.id).unwrap().unwrap();
        assert_eq!(landing.land_verdict, None, "set_landing nulls the verdict");

        // A real gap, so "did not move `updated_at`" is an assertion and
        // not two writes landing in one millisecond.
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(set_land_waiting(&db, &c.id, "ci/tests, ci/lint").unwrap());
        let held = by_id(&db, &c.id).unwrap().unwrap();
        assert_eq!(
            held.land_verdict.as_deref(),
            Some("waiting on ci/tests, ci/lint"),
        );
        assert_eq!(held.state, "landing", "a waiting change left the queue");
        assert_eq!(
            held.updated_at, landing.updated_at,
            "waiting made the change look freshly edited — the bug \
             `landed_at` exists to prevent, in a new place",
        );
        // The prefix is the discriminator, and it is added here rather
        // than trusted from the caller.
        assert!(held
            .land_verdict
            .as_deref()
            .unwrap()
            .starts_with(LAND_WAITING_PREFIX));

        // The steady state of a long wait is zero writes per poll.
        assert!(
            !set_land_waiting(&db, &c.id, "ci/tests, ci/lint").unwrap(),
            "the same note twice wrote a second time",
        );
        // A changed note does write.
        assert!(set_land_waiting(&db, &c.id, "ci/lint").unwrap());
        assert_eq!(
            by_id(&db, &c.id).unwrap().unwrap().land_verdict.as_deref(),
            Some("waiting on ci/lint"),
        );

        // An ejection overwrites it, and its sentence is not a waiting
        // one — the two are told apart by the prefix, in either state.
        assert!(set_ejected(&db, &c.id, "ejected: check 'ci/lint' is failing").unwrap());
        let ejected = by_id(&db, &c.id).unwrap().unwrap();
        assert_eq!(ejected.state, "open");
        assert!(!ejected
            .land_verdict
            .as_deref()
            .unwrap()
            .starts_with(LAND_WAITING_PREFIX));

        // And a note cannot be resurrected onto a change that has left
        // the queue — the lost race a poll and an eject can run into.
        assert!(!set_land_waiting(&db, &c.id, "ci/lint").unwrap());
        assert_eq!(
            by_id(&db, &c.id).unwrap().unwrap().land_verdict.as_deref(),
            Some("ejected: check 'ci/lint' is failing"),
        );

        // A wait that ends in a landing leaves no trace of having waited.
        assert!(set_landing(&db, &c.id, "job-2").unwrap());
        set_land_waiting(&db, &c.id, "ci/lint").unwrap();
        assert!(set_landed(&db, &c.id, &"9".repeat(40), "landed: fast-forward").unwrap());
        let landed = by_id(&db, &c.id).unwrap().unwrap();
        assert_eq!(landed.land_verdict.as_deref(), Some("landed: fast-forward"));
    }

    /// Two nodes sweeping one stranded change: exactly one adopts.
    ///
    /// The second call is the losing node — same stale `expect`, a job
    /// of its own, and it must be told no. Written as `land_job_id = $2`
    /// this test still passes on that pair and only the NULL case
    /// notices, which is why the `expect: None` case is here too.
    #[test]
    fn adopting_a_land_job_is_a_compare_and_set_that_one_node_wins() {
        let (db, org, repo, _alice, ctx) = world("changes-adopt");
        let (c, _, _) = register(&db, &org, &repo, "Iadopt001", &"a".repeat(40), "a", &ctx);
        let dead = crate::jobs::create(&db, &org, Some(&repo), "land", None).unwrap();
        assert!(set_landing(&db, &c.id, &dead.id).unwrap());
        crate::jobs::complete(&db, &dead.id, None).unwrap();

        let before = by_id(&db, &c.id).unwrap().unwrap();
        let mine = crate::jobs::create(&db, &org, Some(&repo), "land", None).unwrap();
        let theirs = crate::jobs::create(&db, &org, Some(&repo), "land", None).unwrap();

        // The winner adopts against the id it read.
        assert!(adopt_land_job(&db, &c.id, Some(&dead.id), &mine.id).unwrap());
        // The loser read the same stale id and must be refused, or two
        // landers drive one change.
        assert!(
            !adopt_land_job(&db, &c.id, Some(&dead.id), &theirs.id).unwrap(),
            "both nodes adopted the same stranded change",
        );
        let after = by_id(&db, &c.id).unwrap().unwrap();
        assert_eq!(after.land_job_id.as_deref(), Some(mine.id.as_str()));
        assert_eq!(
            after.updated_at, before.updated_at,
            "adopting made the change look freshly edited",
        );
        assert_eq!(after.state, "landing");

        // A change that left the queue while the job row was being made
        // must not be dragged back into it.
        assert!(set_landed(&db, &c.id, &"9".repeat(40), "landed: fast-forward").unwrap());
        assert!(!adopt_land_job(&db, &c.id, Some(&mine.id), &theirs.id).unwrap());
        assert_eq!(
            by_id(&db, &c.id).unwrap().unwrap().land_job_id.as_deref(),
            Some(mine.id.as_str()),
        );

        // `expect: None`. No writer produces a `landing` change with a
        // NULL `land_job_id` today — `set_landing` always writes one and
        // nothing nulls it in place — so the row is built here directly
        // rather than through a setter. The arm is defensive, and this
        // is the case that fails under `land_job_id = $2`: NULL never
        // equals NULL, so such a change could never be adopted and the
        // reaper would sweep it forever.
        let (n, _, _) = register(&db, &org, &repo, "Iadopt002", &"b".repeat(40), "b", &ctx);
        let j = crate::jobs::create(&db, &org, Some(&repo), "land", None).unwrap();
        assert!(set_landing(&db, &n.id, &j.id).unwrap());
        db.lock()
            .execute(
                "UPDATE changes SET land_job_id = NULL WHERE id = $1",
                &[&n.id],
            )
            .unwrap();
        assert!(adopt_land_job(&db, &n.id, None, &j.id).unwrap());
        assert_eq!(
            by_id(&db, &n.id).unwrap().unwrap().land_job_id.as_deref(),
            Some(j.id.as_str()),
        );
        // And having adopted it, the same `None` no longer matches.
        assert!(!adopt_land_job(&db, &n.id, None, &theirs.id).unwrap());
    }

    /// The reaper's read: a landing whose job has stopped driving it is
    /// stuck forever, because no other writer accepts `landing`. The
    /// assertion that carries the test is the *negative* one — a change
    /// whose job is still running must never be returned, however old,
    /// or the reaper double-drives every live landing.
    #[test]
    fn stranded_landings_finds_dead_jobs_and_never_live_ones() {
        let (db, org, repo, _alice, ctx) = world("changes-stranded");
        let far_future = now_ms() + 60_000;

        // A: the job finished without ejecting or landing — the hold
        // that completed and was never re-enqueued.
        let (a, _, _) = register(&db, &org, &repo, "Istuck001", &"a".repeat(40), "a", &ctx);
        let ja = crate::jobs::create(&db, &org, Some(&repo), "land", None).unwrap();
        assert!(set_landing(&db, &a.id, &ja.id).unwrap());
        crate::jobs::complete(&db, &ja.id, None).unwrap();

        // B: the job failed — `claim` never looks at it again.
        let (b, _, _) = register(&db, &org, &repo, "Istuck002", &"b".repeat(40), "b", &ctx);
        let jb = crate::jobs::create(&db, &org, Some(&repo), "land", None).unwrap();
        assert!(set_landing(&db, &b.id, &jb.id).unwrap());
        crate::jobs::fail(&db, &jb.id, "worker died").unwrap();

        // C: the job row is gone entirely.
        let (c, _, _) = register(&db, &org, &repo, "Istuck003", &"c".repeat(40), "c", &ctx);
        assert!(set_landing(&db, &c.id, "01JOBVANISHED").unwrap());

        // D: a live landing. Still `queued`, so still being driven.
        let (d, _, _) = register(&db, &org, &repo, "Istuck004", &"d".repeat(40), "d", &ctx);
        let jd = crate::jobs::create(&db, &org, Some(&repo), "land", None).unwrap();
        assert!(set_landing(&db, &d.id, &jd.id).unwrap());

        // E: a live landing that has been claimed and is running long.
        let (e, _, _) = register(&db, &org, &repo, "Istuck005", &"e".repeat(40), "e", &ctx);
        let je = crate::jobs::create(&db, &org, Some(&repo), "land", None).unwrap();
        assert!(set_landing(&db, &e.id, &je.id).unwrap());
        let claimed = crate::jobs::claim(&db, "land", 60_000).unwrap().unwrap();
        assert_eq!(claimed.state, "running");

        // F: never entered the queue at all.
        let (f, _, _) = register(&db, &org, &repo, "Istuck006", &"f".repeat(40), "f", &ctx);

        let stranded: std::collections::BTreeSet<String> = stranded_landings(&db, far_future, 100)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert!(stranded.contains(&a.id), "a completed hold was left stuck");
        assert!(stranded.contains(&b.id), "a failed land job was left stuck");
        assert!(
            stranded.contains(&c.id),
            "a vanished job row was left stuck"
        );
        assert!(
            !stranded.contains(&d.id) && !stranded.contains(&claimed.id),
            "a live landing was handed to the reaper, which would \
             double-drive it",
        );
        assert!(!stranded.contains(&e.id), "a running land job was reaped");
        assert!(!stranded.contains(&f.id), "an open change was reaped");

        // The grace is real: nothing is old enough yet.
        assert!(stranded_landings(&db, 0, 100).unwrap().is_empty());

        // And the limit is honoured and the order deterministic, so the
        // reaper pages instead of scanning.
        assert_eq!(stranded_landings(&db, far_future, 2).unwrap().len(), 2);
        assert_eq!(
            stranded_landings(&db, far_future, 100)
                .unwrap()
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>(),
            stranded_landings(&db, far_future, 100)
                .unwrap()
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>(),
        );

        // A reaped change is ejectable again, which is the whole point:
        // the state it was stuck in accepts `set_ejected`.
        assert!(set_ejected(&db, &a.id, "ejected: landing was abandoned").unwrap());
        assert_eq!(by_id(&db, &a.id).unwrap().unwrap().state, "open");
    }

    /// A polled run, as the Actions poller writes one.
    fn polled(db: &ControlDb, repo: &str, sha: &str, name: &str, state: crate::checks::RunState) {
        crate::checks::upsert(
            db,
            repo,
            &crate::checks::NewCheckRun {
                commit_sha: sha,
                ref_name: Some("refs/heads/main"),
                provider: "github",
                external_id: None,
                name,
                run_number: None,
                event: Some("push"),
                state,
                detail_url: Some("https://gh.example.com/run/1"),
                actor: None,
                started_at: None,
                completed_at: None,
            },
        )
        .unwrap();
    }

    fn require(db: &ControlDb, repo: &str, branch: &str, name: &str, ctx: &AuditCtx) {
        crate::protections::require_check(db, repo, branch, name, ctx).unwrap();
    }

    /// The case a failing-only gate lets straight through: a check that
    /// an admin required and that has never reported at all. "Nothing
    /// has failed" is true and says nothing.
    ///
    /// It **waits** rather than blocking. Push, press Land, CI has not
    /// started yet is the ordinary flow, and ejecting there sends the
    /// author back to press Land a second time — the thing the land
    /// queue exists to abolish. A name that never reports is the wait
    /// budget's problem, not this function's.
    #[test]
    fn a_required_check_that_never_reported_waits_where_the_old_gate_opened() {
        let (db, org, repo, _alice, ctx) = world("changes-gate-missing");
        let (c, ps, _) = register(&db, &org, &repo, "Igate0001", &"a".repeat(40), "one", &ctx);
        require(&db, &repo, "main", "ci/tests", &ctx);

        // The old gate: nothing is failing, so it would have landed.
        assert_eq!(failing_check(&db, &ps.id).unwrap(), None);
        assert!(checks_for_change(&db, &c.id).unwrap().is_empty());
        assert_eq!(
            land_gate(&db, &c.id).unwrap(),
            LandGate::Waiting {
                on: vec!["ci/tests".to_string()]
            },
            "a change with no build at all was cleared to land, or ejected \
             instead of waiting",
        );

        // A required name that has not reported and one that is queued
        // are the same situation to an author, and must have the same
        // outcome — they arrive here by two different code paths.
        require(&db, &repo, "main", "ci/lint", &ctx);
        set_check(&db, &c.id, &ps.id, "ci/lint", "pending", None, "token:01ci")
            .unwrap()
            .unwrap();
        assert_eq!(
            land_gate(&db, &c.id).unwrap(),
            LandGate::Waiting {
                on: vec!["ci/lint".to_string(), "ci/tests".to_string()]
            },
        );

        // And a real failure alongside them still ejects: Blocked beats
        // Waiting, so a slow build cannot hold a change something has
        // already said no to.
        set_check(&db, &c.id, &ps.id, "ci/lint", "failing", None, "token:01ci")
            .unwrap()
            .unwrap();
        assert_eq!(
            land_gate(&db, &c.id).unwrap(),
            LandGate::Blocked {
                reason: "required check 'ci/lint' is failing".into()
            },
        );
    }

    /// Queued is not a verdict. The change waits in the queue instead of
    /// being ejected — the distinction the whole gate exists for.
    #[test]
    fn required_checks_without_a_verdict_wait_and_never_read_as_ready() {
        let (db, org, repo, _alice, ctx) = world("changes-gate-waiting");
        let (c, ps, _) = register(&db, &org, &repo, "Igate0002", &"b".repeat(40), "one", &ctx);
        require(&db, &repo, "main", "ci/tests", &ctx);
        require(&db, &repo, "main", "Build and test", &ctx);
        set_check(
            &db,
            &c.id,
            &ps.id,
            "ci/tests",
            "pending",
            None,
            "token:01ci",
        )
        .unwrap()
        .unwrap();
        polled(
            &db,
            &repo,
            &ps.commit_oid,
            "Build and test",
            crate::checks::RunState::Running,
        );

        assert_eq!(
            land_gate(&db, &c.id).unwrap(),
            LandGate::Waiting {
                on: vec!["Build and test".to_string(), "ci/tests".to_string()],
            },
            "a build that has not answered yet was read as an answer",
        );

        // Green them both and the gate opens; both vocabularies count.
        set_check(
            &db,
            &c.id,
            &ps.id,
            "ci/tests",
            "passing",
            None,
            "token:01ci",
        )
        .unwrap()
        .unwrap();
        polled(
            &db,
            &repo,
            &ps.commit_oid,
            "Build and test",
            crate::checks::RunState::Passing,
        );
        assert_eq!(land_gate(&db, &c.id).unwrap(), LandGate::Ready);
    }

    /// The mirrored-Actions case, which is the whole reason for the
    /// merge: the verdict is in `check_runs`, keyed on the commit, and
    /// the intake never posted anything for this patchset.
    #[test]
    fn a_polled_run_satisfies_a_requirement_the_intake_never_reported() {
        let (db, org, repo, _alice, ctx) = world("changes-gate-polled");
        let (c, ps, _) = register(&db, &org, &repo, "Igate0003", &"c".repeat(40), "one", &ctx);
        require(&db, &repo, "main", "Build and test", &ctx);
        polled(
            &db,
            &repo,
            &ps.commit_oid,
            "Build and test",
            crate::checks::RunState::Passing,
        );
        assert!(
            checks_for(&db, &ps.id).unwrap().is_empty(),
            "no intake rows"
        );

        let merged = checks_for_change(&db, &c.id).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "Build and test");
        assert_eq!(merged[0].state, "passing");
        assert_eq!(merged[0].source, CheckSource::Commit);
        assert!(merged[0].required);
        assert_eq!(
            merged[0].detail_url.as_deref(),
            Some("https://gh.example.com/run/1")
        );
        assert_eq!(land_gate(&db, &c.id).unwrap(), LandGate::Ready);

        // A run against a *different* commit is a different patchset's
        // business and must not answer for this one — that change is
        // still waiting for its own.
        let (c2, _ps2, _) = register(&db, &org, &repo, "Igate0013", &"d".repeat(40), "two", &ctx);
        assert_eq!(
            land_gate(&db, &c2.id).unwrap(),
            LandGate::Waiting {
                on: vec!["Build and test".to_string()]
            },
            "one change's green run answered for another change's commit",
        );
    }

    /// Both sources naming one check is a real state, not a corner. The
    /// intake row wins, because it is the one keyed to this patchset.
    #[test]
    fn when_both_sources_name_one_check_the_intake_row_wins() {
        let (db, org, repo, _alice, ctx) = world("changes-gate-collision");
        let (c, ps, _) = register(&db, &org, &repo, "Igate0004", &"e".repeat(40), "one", &ctx);
        require(&db, &repo, "main", "ci/tests", &ctx);
        polled(
            &db,
            &repo,
            &ps.commit_oid,
            "ci/tests",
            crate::checks::RunState::Failing,
        );
        set_check(
            &db,
            &c.id,
            &ps.id,
            "ci/tests",
            "passing",
            None,
            "token:01ci",
        )
        .unwrap()
        .unwrap();

        let merged = checks_for_change(&db, &c.id).unwrap();
        assert_eq!(merged.len(), 1, "one name, one row");
        assert_eq!(merged[0].source, CheckSource::Patchset);
        assert_eq!(merged[0].state, "passing");
        assert_eq!(land_gate(&db, &c.id).unwrap(), LandGate::Ready);

        // And the other way round, so the winner is the source and not
        // whichever verdict happens to be greener.
        set_check(
            &db,
            &c.id,
            &ps.id,
            "ci/tests",
            "failing",
            None,
            "token:01ci",
        )
        .unwrap()
        .unwrap();
        polled(
            &db,
            &repo,
            &ps.commit_oid,
            "ci/tests",
            crate::checks::RunState::Passing,
        );
        let merged = checks_for_change(&db, &c.id).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, CheckSource::Patchset);
        assert_eq!(merged[0].state, "failing");
        assert_eq!(
            land_gate(&db, &c.id).unwrap(),
            LandGate::Blocked {
                reason: "required check 'ci/tests' is failing".into()
            },
        );
    }

    /// A required check that reached a verdict of "no verdict" is not
    /// passing, and blocks with words that say which.
    #[test]
    fn a_cancelled_or_skipped_required_check_blocks_and_says_why() {
        let (db, org, repo, _alice, ctx) = world("changes-gate-nonverdict");
        let (c, ps, _) = register(&db, &org, &repo, "Igate0005", &"f".repeat(40), "one", &ctx);
        require(&db, &repo, "main", "build", &ctx);
        require(&db, &repo, "main", "lint", &ctx);
        polled(
            &db,
            &repo,
            &ps.commit_oid,
            "build",
            crate::checks::RunState::Cancelled,
        );
        polled(
            &db,
            &repo,
            &ps.commit_oid,
            "lint",
            crate::checks::RunState::Skipped,
        );
        assert_eq!(
            land_gate(&db, &c.id).unwrap(),
            LandGate::Blocked {
                reason: "required check 'build' was cancelled; required check 'lint' \
                         was skipped, so nothing was checked"
                    .into()
            },
        );
    }

    /// Nothing that blocks today stops blocking: a failing check nobody
    /// required still blocks. Only `failing` — a cancelled run nobody
    /// asked for never blocked anything and must not start.
    #[test]
    fn a_failing_check_nobody_required_still_blocks() {
        let (db, org, repo, _alice, ctx) = world("changes-gate-unrequired");
        let (c, ps, _) = register(&db, &org, &repo, "Igate0006", &"1".repeat(40), "one", &ctx);
        require(&db, &repo, "main", "ci/tests", &ctx);
        set_check(
            &db,
            &c.id,
            &ps.id,
            "ci/tests",
            "passing",
            None,
            "token:01ci",
        )
        .unwrap()
        .unwrap();
        polled(
            &db,
            &repo,
            &ps.commit_oid,
            "nightly",
            crate::checks::RunState::Cancelled,
        );
        assert_eq!(
            land_gate(&db, &c.id).unwrap(),
            LandGate::Ready,
            "a cancelled run nobody required started blocking",
        );

        set_check(&db, &c.id, &ps.id, "ci/perf", "failing", None, "token:01ci")
            .unwrap()
            .unwrap();
        assert_eq!(
            land_gate(&db, &c.id).unwrap(),
            LandGate::Blocked {
                reason: "check 'ci/perf' is failing".into()
            },
        );
    }

    /// Empty means today's behaviour, exactly — and requirements are per
    /// branch, so a gate on `release/1.0` says nothing about a change
    /// targeting `main`.
    #[test]
    fn with_nothing_required_the_gate_matches_the_old_one() {
        let (db, org, repo, _alice, ctx) = world("changes-gate-empty");
        let (c, ps, _) = register(&db, &org, &repo, "Igate0007", &"2".repeat(40), "one", &ctx);
        require(&db, &repo, "release/1.0", "ci/tests", &ctx);

        // Nothing reported at all: the old gate opened, and so does this
        // one, because nothing is required *on this branch*.
        assert_eq!(failing_check(&db, &ps.id).unwrap(), None);
        assert_eq!(land_gate(&db, &c.id).unwrap(), LandGate::Ready);

        // Pending, with nothing required, is not something to wait on —
        // that would hold every change in the queue forever.
        set_check(
            &db,
            &c.id,
            &ps.id,
            "ci/tests",
            "pending",
            None,
            "token:01ci",
        )
        .unwrap()
        .unwrap();
        assert_eq!(land_gate(&db, &c.id).unwrap(), LandGate::Ready);

        set_check(
            &db,
            &c.id,
            &ps.id,
            "ci/tests",
            "failing",
            None,
            "token:01ci",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            failing_check(&db, &ps.id).unwrap().as_deref(),
            Some("ci/tests")
        );
        assert_eq!(
            land_gate(&db, &c.id).unwrap(),
            LandGate::Blocked {
                reason: "check 'ci/tests' is failing".into()
            },
        );
    }

    /// Approvals read the latest patchset and so does this: yesterday's
    /// green run says nothing about code it never built. Plus the
    /// ordering and the hostile-id answer.
    #[test]
    fn the_merged_read_follows_the_latest_patchset_and_is_ordered() {
        let (db, org, repo, _alice, ctx) = world("changes-gate-latest");
        let (c, ps1, _) = register(&db, &org, &repo, "Igate0008", &"3".repeat(40), "one", &ctx);
        require(&db, &repo, "main", "ci/tests", &ctx);
        set_check(
            &db,
            &c.id,
            &ps1.id,
            "ci/tests",
            "passing",
            None,
            "token:01ci",
        )
        .unwrap()
        .unwrap();
        polled(
            &db,
            &repo,
            &ps1.commit_oid,
            "Build and test",
            crate::checks::RunState::Passing,
        );
        assert_eq!(land_gate(&db, &c.id).unwrap(), LandGate::Ready);
        assert_eq!(
            checks_for_change(&db, &c.id)
                .unwrap()
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Build and test", "ci/tests"],
            "not ordered by name",
        );

        // A new patchset starts with nothing, on both sides.
        let (_c, ps2, new) = register(&db, &org, &repo, "Igate0008", &"4".repeat(40), "one", &ctx);
        assert!(new && ps2.id != ps1.id);
        assert!(checks_for_change(&db, &c.id).unwrap().is_empty());
        assert_eq!(
            land_gate(&db, &c.id).unwrap(),
            LandGate::Waiting {
                on: vec!["ci/tests".to_string()]
            },
            "yesterday's green run answered for code it never built",
        );

        // Hostile ids answer absence, never SQL.
        assert!(checks_for_change(&db, "not an id\0").unwrap().is_empty());
        assert_eq!(land_gate(&db, "not an id\0").unwrap(), LandGate::Ready);
    }

    /// Line anchors: a line needs a file, and stays a sane number (I13).
    #[test]
    fn line_comments_are_anchored_and_bounded() {
        let (db, org, repo, _alice, ctx) = world("changes-lines");
        let (c, ps, _) = register(&db, &org, &repo, "Icc000002", &"a".repeat(40), "one", &ctx);
        // A line without a path means nothing — refused in words.
        let e = add_comment(
            &db,
            &NewComment {
                line: Some(3),
                ..say(&c.id, ps.number, "why?")
            },
        )
        .unwrap()
        .unwrap_err();
        assert!(e.contains("needs a path"), "{e}");
        // Zero, negative and absurd lines are refused, not stored.
        for bad in [0, -1, MAX_COMMENT_LINE + 1] {
            let e = add_comment(
                &db,
                &NewComment {
                    path: Some("f.rs"),
                    line: Some(bad),
                    ..say(&c.id, ps.number, "why?")
                },
            )
            .unwrap()
            .unwrap_err();
            assert!(e.contains("line must be"), "{bad}: {e}");
        }
        // The boundary itself is fine.
        let ok = add_comment(
            &db,
            &NewComment {
                path: Some("f.rs"),
                line: Some(1),
                ..say(&c.id, ps.number, "first line")
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(ok.line, Some(1));
        // A single-line anchor is stored as the range it is, so no
        // reader has to remember to COALESCE line_end back to line.
        assert_eq!(ok.line_end, Some(1));
        assert_eq!(comments_for(&db, &c.id).unwrap().len(), 1);
    }

    /// Threads are exactly one level deep, and a reply is part of its
    /// thread rather than a second remark nearby: it inherits the whole
    /// anchor and may not set one of its own.
    #[test]
    fn a_reply_joins_its_thread_and_cannot_grow_a_third_level() {
        let (db, org, repo, _alice, ctx) = world("changes-threads");
        let (c, ps, _) = register(&db, &org, &repo, "Icc000005", &"a".repeat(40), "one", &ctx);
        let (other, ops, _) = register(&db, &org, &repo, "Icc000006", &"b".repeat(40), "two", &ctx);
        let root = add_comment(
            &db,
            &NewComment {
                path: Some("fees.rs"),
                line: Some(7),
                line_end: Some(9),
                side: Side::Old,
                ..say(&c.id, ps.number, "why was this deleted?")
            },
        )
        .unwrap()
        .unwrap();

        // A reply inherits path, line, range and side — all of it,
        // including the original the thread was first written at.
        let reply = add_comment(
            &db,
            &NewComment {
                parent_id: Some(&root.id),
                ..say(&c.id, ps.number, "it moved to fees/mod.rs")
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(reply.path.as_deref(), Some("fees.rs"));
        assert_eq!((reply.line, reply.line_end), (Some(7), Some(9)));
        assert_eq!(reply.side, Side::Old);
        assert_eq!(reply.original_line, Some(7));
        assert_eq!(reply.original_patchset, Some(ps.number));
        assert_eq!(reply.thread_id(), root.id);
        assert_eq!(root.thread_id(), root.id, "a root is its own thread");

        // A reply to a reply is a forum. Refused in words.
        let e = add_comment(
            &db,
            &NewComment {
                parent_id: Some(&reply.id),
                ..say(&c.id, ps.number, "and back again")
            },
        )
        .unwrap()
        .unwrap_err();
        assert!(e.contains("one level deep"), "{e}");

        // A reply that carries its own anchor is refused rather than
        // silently re-anchored: a thread that describes two places at
        // once is worse than a rejected request.
        for own in [
            NewComment {
                parent_id: Some(&root.id),
                path: Some("other.rs"),
                ..say(&c.id, ps.number, "elsewhere")
            },
            NewComment {
                parent_id: Some(&root.id),
                line: Some(3),
                ..say(&c.id, ps.number, "elsewhere")
            },
            NewComment {
                parent_id: Some(&root.id),
                line_end: Some(30),
                ..say(&c.id, ps.number, "elsewhere")
            },
            NewComment {
                parent_id: Some(&root.id),
                side: Side::Old,
                ..say(&c.id, ps.number, "elsewhere")
            },
        ] {
            let e = add_comment(&db, &own).unwrap().unwrap_err();
            assert!(e.contains("inherits its thread's anchor"), "{e}");
        }

        // A thread does not straddle two changes, and a parent that
        // does not exist answers absence rather than a foreign-key 500.
        let e = add_comment(
            &db,
            &NewComment {
                parent_id: Some(&root.id),
                ..say(&other.id, ops.number, "wrong change")
            },
        )
        .unwrap()
        .unwrap_err();
        assert!(e.contains("another change"), "{e}");
        for missing in ["not an id\0", &ulid()] {
            let e = add_comment(
                &db,
                &NewComment {
                    parent_id: Some(missing),
                    ..say(&c.id, ps.number, "into the void")
                },
            )
            .unwrap()
            .unwrap_err();
            assert!(e.contains("no such comment"), "{missing:?}: {e}");
        }
    }

    /// Ranges and the old side: both anchors are checked at the door, so
    /// a nonsense range never reaches a renderer that would have to
    /// invent a meaning for it.
    #[test]
    fn ranges_and_the_old_side_are_anchored_or_refused() {
        let (db, org, repo, _alice, ctx) = world("changes-ranges");
        let (c, ps, _) = register(&db, &org, &repo, "Icc000007", &"a".repeat(40), "one", &ctx);
        let cases: [(NewComment, &str); 4] = [
            (
                NewComment {
                    path: Some("f.rs"),
                    line_end: Some(9),
                    ..say(&c.id, ps.number, "which lines?")
                },
                "range needs a start line",
            ),
            (
                NewComment {
                    path: Some("f.rs"),
                    line: Some(9),
                    line_end: Some(4),
                    ..say(&c.id, ps.number, "backwards")
                },
                "must not precede",
            ),
            (
                NewComment {
                    path: Some("f.rs"),
                    line: Some(1),
                    line_end: Some(MAX_COMMENT_LINE + 1),
                    ..say(&c.id, ps.number, "to infinity")
                },
                "line_end must be",
            ),
            (
                NewComment {
                    side: Side::Old,
                    ..say(&c.id, ps.number, "the old side of what?")
                },
                "old-side comment needs a path",
            ),
        ];
        for (bad, want) in cases {
            let e = add_comment(&db, &bad).unwrap().unwrap_err();
            assert!(e.contains(want), "wanted {want:?}, got {e:?}");
        }

        // A real range on the pre-image round-trips whole.
        let ok = add_comment(
            &db,
            &NewComment {
                path: Some("f.rs"),
                line: Some(4),
                line_end: Some(9),
                side: Side::Old,
                ..say(&c.id, ps.number, "this loop was doing the retry")
            },
        )
        .unwrap()
        .unwrap();
        let back = &comments_for(&db, &c.id).unwrap()[0];
        assert_eq!(back.id, ok.id);
        assert_eq!((back.line, back.line_end), (Some(4), Some(9)));
        assert_eq!(back.side, Side::Old);
        assert_eq!(back.original_line, Some(4));
        assert_eq!(back.original_patchset, Some(ps.number));
    }

    /// Resolution is a property of a thread, and of the person who first
    /// said "done" — not of whoever pressed the button last.
    #[test]
    fn only_a_root_resolves_and_the_first_resolver_stands() {
        let (db, org, repo, alice, ctx) = world("changes-resolve");
        let (c, ps, _) = register(&db, &org, &repo, "Icc000008", &"a".repeat(40), "one", &ctx);
        let bob = users::create(&db, "bob@example.com", "Bob", None).unwrap();
        members::add(&db, &org, &bob.id, Role::Member, None).unwrap();
        let root = add_comment(&db, &say(&c.id, ps.number, "needs a test"))
            .unwrap()
            .unwrap();
        let reply = add_comment(
            &db,
            &NewComment {
                parent_id: Some(&root.id),
                ..say(&c.id, ps.number, "added one")
            },
        )
        .unwrap()
        .unwrap();

        // A sentence inside a thread does not resolve; the thread does.
        let e = set_resolved(&db, &reply.id, Some(&alice), true)
            .unwrap()
            .unwrap_err();
        assert!(e.contains("resolve the thread"), "{e}");
        // An id that names nothing answers absence, not an error.
        assert!(set_resolved(&db, "not an id\0", Some(&alice), true)
            .unwrap()
            .unwrap()
            .is_none());
        assert!(set_resolved(&db, &ulid(), Some(&alice), true)
            .unwrap()
            .unwrap()
            .is_none());

        let done = set_resolved(&db, &root.id, Some(&alice), true)
            .unwrap()
            .unwrap()
            .expect("the root resolves");
        assert!(done.resolved_at.is_some());
        assert_eq!(done.resolved_by.as_deref(), Some(alice.as_str()));
        assert_eq!(done.resolved_by_name.as_deref(), Some("Alice"));

        // Pressing it again — or somebody else pressing it — keeps the
        // record of who actually said "done", and when.
        let again = set_resolved(&db, &root.id, Some(&bob.id), true)
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(again.resolved_by.as_deref(), Some(alice.as_str()));
        assert_eq!(again.resolved_at, done.resolved_at);

        // Unresolving clears both halves: a thread that is open must not
        // still name a resolver.
        let open = set_resolved(&db, &root.id, Some(&bob.id), false)
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(open.resolved_at, None);
        assert_eq!(open.resolved_by, None);
        assert_eq!(open.resolved_by_name, None);
        // And the reply is untouched throughout — the state lives on
        // one row, so there is nothing for the two to disagree about.
        let rows = comments_for(&db, &c.id).unwrap();
        assert_eq!(rows[1].resolved_at, None);
    }

    /// The import identity: bounded like any other foreign text, and
    /// unique, so a review import can be run a second time — which is
    /// the run that matters, the first one always being incomplete.
    #[test]
    fn an_imported_identity_is_bounded_and_deduplicates() {
        let (db, org, repo, _alice, ctx) = world("changes-import");
        let (c, ps, _) = register(&db, &org, &repo, "Icc000009", &"a".repeat(40), "one", &ctx);
        let long = "x".repeat(MAX_EXTERNAL_ID_LEN + 1);
        for (bad, want) in [
            ("", "external id must be"),
            (long.as_str(), "external id must be"),
            ("has\0nul", "control characters"),
            ("has\nnewline", "control characters"),
        ] {
            let e = add_comment(
                &db,
                &NewComment {
                    external_id: Some(bad),
                    ..say(&c.id, ps.number, "imported")
                },
            )
            .unwrap()
            .unwrap_err();
            assert!(e.contains(want), "{bad:?}: {e}");
        }
        let ext = "github.com/acme/widget/pull/4721#discussion_r123";
        let first = add_comment(
            &db,
            &NewComment {
                external_id: Some(ext),
                ..say(&c.id, ps.number, "imported")
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(first.external_id.as_deref(), Some(ext));
        // The second run of the same import: refused in words, so the
        // importer skips the row instead of aborting the run.
        let e = add_comment(
            &db,
            &NewComment {
                external_id: Some(ext),
                ..say(&c.id, ps.number, "imported")
            },
        )
        .unwrap()
        .unwrap_err();
        assert!(e.contains("external id already exists"), "{e}");
        // NULLs do not collide, so comments spoken here are unaffected.
        for _ in 0..2 {
            add_comment(&db, &say(&c.id, ps.number, "spoken here"))
                .unwrap()
                .unwrap();
        }
        assert_eq!(comments_for(&db, &c.id).unwrap().len(), 3);
    }

    /// The security case, and the one this whole feature turns on: an
    /// unsubmitted comment belongs to its author and to nobody else.
    ///
    /// Asserted through every reader there is — the published view, a
    /// second person, and nobody-in-particular (which is what an
    /// anonymous reader and a service principal both are here) —
    /// because a leak through any one of them publishes somebody's
    /// half-formed first reaction under their name.
    #[test]
    fn a_draft_is_visible_to_its_author_and_to_no_other_reader() {
        let (db, org, repo, alice, ctx) = world("changes-draft-visibility");
        let bob = users::create(&db, "bob@example.com", "Bob", None).unwrap();
        let (c, ps, _) = register(
            &db,
            &org,
            &repo,
            "Idraft01",
            &"d".repeat(40),
            "drafts",
            &ctx,
        );
        let review = open_review(&db, &c.id, &ps.id, &alice, None)
            .unwrap()
            .unwrap();

        add_comment(
            &db,
            &NewComment {
                author_principal: &format!("user:{alice}"),
                author_user_id: Some(&alice),
                review_id: Some(&review.id),
                ..say(&c.id, ps.number, "not sure about this yet")
            },
        )
        .unwrap()
        .unwrap();
        add_comment(
            &db,
            &NewComment {
                author_principal: &format!("user:{alice}"),
                author_user_id: Some(&alice),
                ..say(&c.id, ps.number, "said out loud")
            },
        )
        .unwrap()
        .unwrap();

        let published = comments_for(&db, &c.id).unwrap();
        assert_eq!(published.len(), 1, "{published:?}");
        assert_eq!(published[0].body, "said out loud");
        assert_eq!(
            comments_for_viewer(&db, &c.id, None).unwrap().len(),
            1,
            "nobody in particular sees only what was published"
        );
        assert_eq!(
            comments_for_viewer(&db, &c.id, Some(&bob.id))
                .unwrap()
                .len(),
            1,
            "a second person must never see another person's draft"
        );
        let mine = comments_for_viewer(&db, &c.id, Some(&alice)).unwrap();
        assert_eq!(mine.len(), 2, "the author sees her own draft: {mine:?}");
        assert!(mine[0].published_at.is_none());
        assert_eq!(mine[0].review_id.as_deref(), Some(review.id.as_str()));
        assert!(mine[1].published_at.is_some());

        // A malformed viewer is nobody, not a query with a hostile
        // value in it: the same masking contract every lookup honours.
        assert_eq!(
            comments_for_viewer(&db, &c.id, Some("nul\0"))
                .unwrap()
                .len(),
            1
        );

        // A draft is not a thread, so nothing can reply into one — and
        // the refusal is the *absent* sentence, because the alternative
        // tells a stranger holding an id that a draft exists.
        let e = add_comment(
            &db,
            &NewComment {
                parent_id: Some(&mine[0].id),
                author_principal: &format!("user:{}", bob.id),
                author_user_id: Some(&bob.id),
                ..say(&c.id, ps.number, "replying to a secret")
            },
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(e, "no such comment to reply to");
    }

    /// Submitting is one act: every drafted comment becomes visible at
    /// the same instant, and the verdict lands on the record sufficiency
    /// already reads.
    #[test]
    fn submitting_publishes_every_draft_at_once_and_writes_the_approval() {
        let (db, org, repo, alice, ctx) = world("changes-submit");
        let (c, ps, _) = register(&db, &org, &repo, "Isubmit1", &"s".repeat(40), "pass", &ctx);
        let review = open_review(&db, &c.id, &ps.id, &alice, Some("  looks good  "))
            .unwrap()
            .unwrap();
        assert_eq!(review.body.as_deref(), Some("looks good"), "trimmed");
        assert_eq!(review.state, "draft");
        for i in 0..3 {
            add_comment(
                &db,
                &NewComment {
                    author_principal: &format!("user:{alice}"),
                    author_user_id: Some(&alice),
                    review_id: Some(&review.id),
                    ..say(&c.id, ps.number, &format!("note {i}"))
                },
            )
            .unwrap()
            .unwrap();
        }
        // Opening again is the same draft, not a second one.
        let again = open_review(&db, &c.id, &ps.id, &alice, None)
            .unwrap()
            .unwrap();
        assert_eq!(again.id, review.id);

        assert!(comments_for(&db, &c.id).unwrap().is_empty());
        let done = submit_review(
            &db,
            &SubmitReview {
                change_id: &c.id,
                patchset_id: &ps.id,
                user_id: &alice,
                verdict: ReviewVerdict::Approve,
                body: None,
            },
            &ctx,
        )
        .unwrap()
        .unwrap();
        assert_eq!(done.published, 3, "all three, or none");
        assert!(done.approved);
        assert!(!done.approval_revoked);
        assert_eq!(done.review.state, "submitted");
        assert_eq!(
            done.review.body.as_deref(),
            Some("looks good"),
            "a submit with no words keeps the draft's"
        );
        assert_eq!(comments_for(&db, &c.id).unwrap().len(), 3);
        // The approvals row is the one the sufficiency engine reads,
        // written by the review and indistinguishable from one written
        // by the approve route. That seam is what keeps this feature
        // from being an engine rewrite.
        let approvals = approvals_for(&db, &ps.id).unwrap();
        assert_eq!(approvals.len(), 1, "{approvals:?}");
        assert_eq!(approvals[0].user_id, alice);
        // Nothing is left pending, and the draft slot is free again.
        assert!(pending_review(&db, &c.id, &alice).unwrap().is_none());
        assert_eq!(reviews_for(&db, &c.id).unwrap().len(), 1);
    }

    /// A block needs words, survives a new patchset, takes back its
    /// author's approval, and ends only when its author says so.
    #[test]
    fn a_request_for_changes_is_durable_and_only_its_author_ends_it() {
        let (db, org, repo, alice, ctx) = world("changes-block");
        let (c, ps1, _) = register(&db, &org, &repo, "Iblock01", &"b".repeat(40), "no", &ctx);

        // Approve first, so the revocation below has something to take.
        approve(&db, &c.id, &ps1.id, &alice, &ctx).unwrap();
        assert_eq!(approvals_for(&db, &ps1.id).unwrap().len(), 1);

        // A wall with no door: the author is told no and cannot learn
        // what would make it a yes.
        let e = submit_review(
            &db,
            &SubmitReview {
                change_id: &c.id,
                patchset_id: &ps1.id,
                user_id: &alice,
                verdict: ReviewVerdict::RequestChanges,
                body: None,
            },
            &ctx,
        )
        .unwrap()
        .unwrap_err();
        assert!(e.contains("needs words"), "{e}");

        let done = submit_review(
            &db,
            &SubmitReview {
                change_id: &c.id,
                patchset_id: &ps1.id,
                user_id: &alice,
                verdict: ReviewVerdict::RequestChanges,
                body: Some("the retry loop is unbounded"),
            },
            &ctx,
        )
        .unwrap()
        .unwrap();
        assert!(
            done.approval_revoked,
            "nobody approves and blocks the same code"
        );
        assert!(approvals_for(&db, &ps1.id).unwrap().is_empty());
        let blocks = standing_blocks(&db, &c.id).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].verdict, ReviewVerdict::RequestChanges);
        assert!(blocks[0].withdrawn_at.is_none());
        assert_eq!(blocks[0].user_id, alice);

        // A new patchset. An approval would have died here — the
        // approver never saw this code — and a block must not, or the
        // author clears the objection by pushing over it.
        let (_, ps2, _) = register(&db, &org, &repo, "Iblock01", &"c".repeat(40), "no", &ctx);
        assert_ne!(ps2.id, ps1.id);
        assert_eq!(
            standing_blocks(&db, &c.id).unwrap().len(),
            1,
            "a block does not vanish on the next patchset"
        );

        assert!(withdraw_review(&db, &c.id, &alice, &ctx).unwrap());
        assert!(standing_blocks(&db, &c.id).unwrap().is_empty());
        assert!(
            !withdraw_review(&db, &c.id, &alice, &ctx).unwrap(),
            "withdrawing twice is nothing to withdraw, not a second act"
        );
        // The row stays: "Alice asked for changes and later withdrew
        // it" is review history, and a delete would render it as though
        // she never objected.
        let all = reviews_for(&db, &c.id).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].withdrawn_at.is_some());
    }

    /// A reviewer's latest word stands. Blocking on patchset 1 and
    /// approving on patchset 2 is a changed mind, not two verdicts.
    #[test]
    fn a_later_verdict_supersedes_the_same_persons_block() {
        let (db, org, repo, alice, ctx) = world("changes-supersede");
        let bob = users::create(&db, "bob@example.com", "Bob", None).unwrap();
        let (c, ps, _) = register(&db, &org, &repo, "Isuper01", &"e".repeat(40), "mind", &ctx);
        let block = |who: &str, what: ReviewVerdict| {
            submit_review(
                &db,
                &SubmitReview {
                    change_id: &c.id,
                    patchset_id: &ps.id,
                    user_id: who,
                    verdict: what,
                    body: Some("words"),
                },
                &ctx,
            )
            .unwrap()
            .unwrap()
        };
        block(&alice, ReviewVerdict::RequestChanges);
        block(&bob.id, ReviewVerdict::RequestChanges);
        assert_eq!(standing_blocks(&db, &c.id).unwrap().len(), 2);

        block(&alice, ReviewVerdict::Approve);
        let left = standing_blocks(&db, &c.id).unwrap();
        assert_eq!(left.len(), 1, "{left:?}");
        assert_eq!(left[0].user_id, bob.id);

        // A `comment` verdict is not a verdict about landing: it must
        // neither block nor clear an approval.
        assert_eq!(approvals_for(&db, &ps.id).unwrap().len(), 1);
        block(&alice, ReviewVerdict::Comment);
        assert_eq!(
            approvals_for(&db, &ps.id).unwrap().len(),
            1,
            "notes must not quietly withdraw a standing approval"
        );
    }

    /// Words are bounded and hostile bytes are refused at the door, on
    /// the review's cover message exactly as on a comment — one rule,
    /// two doors, so the newer door cannot be the lax one.
    #[test]
    fn a_reviews_words_are_bounded_and_discarding_takes_its_drafts_with_it() {
        let (db, org, repo, alice, ctx) = world("changes-review-bounds");
        let (c, ps, _) = register(
            &db,
            &org,
            &repo,
            "Ibound01",
            &"f".repeat(40),
            "bounds",
            &ctx,
        );
        let long = "x".repeat(MAX_COMMENT_LEN + 1);
        for bad in ["   ", long.as_str(), "nul\0here"] {
            let e = open_review(&db, &c.id, &ps.id, &alice, Some(bad))
                .unwrap()
                .unwrap_err();
            assert!(e.contains("review"), "{bad:?} answered {e}");
        }
        assert!(
            pending_review(&db, &c.id, &alice).unwrap().is_none(),
            "a refused body must not have opened a review anyway"
        );

        let review = open_review(&db, &c.id, &ps.id, &alice, None)
            .unwrap()
            .unwrap();
        add_comment(
            &db,
            &NewComment {
                author_principal: &format!("user:{alice}"),
                author_user_id: Some(&alice),
                review_id: Some(&review.id),
                ..say(&c.id, ps.number, "never mind")
            },
        )
        .unwrap()
        .unwrap();
        assert!(discard_review(&db, &c.id, &alice).unwrap());
        assert!(
            comments_for_viewer(&db, &c.id, Some(&alice))
                .unwrap()
                .is_empty(),
            "a discarded review takes its drafts with it, or they are \
             stranded where nothing can ever publish them"
        );
        assert!(!discard_review(&db, &c.id, &alice).unwrap());

        // The other half of that race, from the writing side: the tab
        // that was still open drafts one more remark into the review
        // the other tab just threw away. The row must be **refused**,
        // not written — a comment whose `review_id` names nothing is
        // one no submit can ever publish and no discard can ever reach,
        // and it would sit unpublished in its author's own view
        // forever. `review_id` is a foreign key precisely so the
        // database says so; this asserts the refusal reaches the caller
        // rather than being swallowed as an already-exists.
        let stranded = add_comment(
            &db,
            &NewComment {
                author_principal: &format!("user:{alice}"),
                author_user_id: Some(&alice),
                review_id: Some(&review.id),
                ..say(&c.id, ps.number, "one more thought")
            },
        )
        .unwrap_err();
        assert!(
            stranded.contains("add comment"),
            "a comment aimed at a discarded review: {stranded}"
        );
        assert!(
            comments_for_viewer(&db, &c.id, Some(&alice))
                .unwrap()
                .is_empty(),
            "the refused comment was written anyway"
        );

        // A verdict spelled anything else is refused rather than
        // coerced: coercion would turn somebody's "no" into a shrug.
        assert_eq!(
            ReviewVerdict::parse("request_changes"),
            Some(ReviewVerdict::RequestChanges)
        );
        assert_eq!(ReviewVerdict::parse("REQUEST_CHANGES"), None);
        assert_eq!(ReviewVerdict::parse(""), None);
        for v in [
            ReviewVerdict::Approve,
            ReviewVerdict::Comment,
            ReviewVerdict::RequestChanges,
        ] {
            assert_eq!(ReviewVerdict::parse(v.as_str()), Some(v));
        }

        // Ids that cannot exist are absent, never errors.
        assert!(pending_review(&db, "nul\0", &alice).unwrap().is_none());
        assert!(review_by_id(&db, "nul\0").unwrap().is_none());
        assert!(standing_blocks(&db, "nul\0").unwrap().is_empty());
        assert!(reviews_for(&db, "nul\0").unwrap().is_empty());
        assert!(!discard_review(&db, "nul\0", &alice).unwrap());
        assert!(!withdraw_review(&db, "nul\0", &alice, &ctx).unwrap());
        assert!(open_review(&db, &c.id, &ps.id, "nul\0", None)
            .unwrap()
            .is_err());
        assert!(submit_review(
            &db,
            &SubmitReview {
                change_id: &c.id,
                patchset_id: "nul\0",
                user_id: &alice,
                verdict: ReviewVerdict::Comment,
                body: None,
            },
            &ctx,
        )
        .unwrap()
        .is_err());

        // A *well-shaped* id that names nobody is a different thing
        // from one that could never exist. `valid_id` cannot tell it
        // from a real one, so it reaches the statement, and the foreign
        // key on `reviews.user_id` is what refuses it. That asymmetry
        // is deliberate: absent-is-empty is the right answer for a
        // read, but a write that shrugged here would put a verdict on
        // the record with nobody behind it — and the sufficiency engine
        // reads that row.
        let nobody = ulid();
        let e = submit_review(
            &db,
            &SubmitReview {
                change_id: &c.id,
                patchset_id: &ps.id,
                user_id: &nobody,
                verdict: ReviewVerdict::Comment,
                body: Some("words"),
            },
            &ctx,
        )
        .unwrap_err();
        assert!(e.contains("submit review"), "{e}");
        assert!(
            reviews_for(&db, &c.id).unwrap().is_empty(),
            "a verdict was recorded under a user who is not there"
        );
    }

    /// Two things a reviewer does that the first pass at this feature
    /// silently dropped: typing a cover message into a draft that is
    /// already open, and asking for changes with the words on the lines
    /// rather than in the cover.
    ///
    /// The first is a save, not a read — `open_review` is idempotent in
    /// its *identity*, never in its content, and a version that handed
    /// the existing row back untouched would lose whatever the reviewer
    /// had just typed with no error anywhere to show for it.
    ///
    /// The second is what "a block needs words" actually means. Twelve
    /// specific remarks on twelve specific lines already say what would
    /// make this a yes; demanding a cover message on top of them is a
    /// form, not a rule, and the refusal must count the drafted comments
    /// before it fires.
    #[test]
    fn a_reopened_draft_saves_its_words_and_drafted_comments_are_words_enough() {
        let (db, org, repo, alice, ctx) = world("changes-review-words");
        let (c, ps, _) = register(
            &db,
            &org,
            &repo,
            "Iwords001",
            &"9".repeat(40),
            "words",
            &ctx,
        );

        // Opened empty, then re-opened with a cover message: the same
        // row, with the words actually stored.
        let first = open_review(&db, &c.id, &ps.id, &alice, None)
            .unwrap()
            .unwrap();
        assert!(first.body.is_none());
        let again = open_review(&db, &c.id, &ps.id, &alice, Some("  on reflection  "))
            .unwrap()
            .unwrap();
        assert_eq!(again.id, first.id, "re-opening started a second draft");
        assert_eq!(again.body.as_deref(), Some("on reflection"), "trimmed");
        assert_eq!(
            pending_review(&db, &c.id, &alice)
                .unwrap()
                .unwrap()
                .body
                .as_deref(),
            Some("on reflection"),
            "the words were handed back but never written down"
        );

        // The cover message is prose at the submit door exactly as at
        // the open one — one rule, two doors, so the newer door cannot
        // be the lax one — and a refusal leaves the draft as it was.
        for bad in ["   ", "nul\0here"] {
            let e = submit_review(
                &db,
                &SubmitReview {
                    change_id: &c.id,
                    patchset_id: &ps.id,
                    user_id: &alice,
                    verdict: ReviewVerdict::Comment,
                    body: Some(bad),
                },
                &ctx,
            )
            .unwrap()
            .unwrap_err();
            assert!(e.contains("review"), "{bad:?} answered {e}");
        }
        assert!(
            pending_review(&db, &c.id, &alice).unwrap().is_some(),
            "a refused body threw the draft away"
        );

        // Bob blocks. With nothing drafted and no cover message there is
        // nothing for the author to act on, and it is refused — even
        // though a draft row exists, which is the case the count is for.
        let bob = users::create(&db, "bob@example.com", "Bob", None).unwrap();
        let bobs = open_review(&db, &c.id, &ps.id, &bob.id, None)
            .unwrap()
            .unwrap();
        let block = |body: Option<&str>| {
            submit_review(
                &db,
                &SubmitReview {
                    change_id: &c.id,
                    patchset_id: &ps.id,
                    user_id: &bob.id,
                    verdict: ReviewVerdict::RequestChanges,
                    body,
                },
                &ctx,
            )
            .unwrap()
        };
        let e = block(None).unwrap_err();
        assert!(e.contains("needs words"), "{e}");

        // One drafted remark is what would make this a yes, so the same
        // call now goes through — and invents no cover message on the
        // way.
        add_comment(
            &db,
            &NewComment {
                author_principal: &format!("user:{}", bob.id),
                author_user_id: Some(&bob.id),
                review_id: Some(&bobs.id),
                ..say(&c.id, ps.number, "this retry loop is unbounded")
            },
        )
        .unwrap()
        .unwrap();
        let done = block(None).unwrap();
        assert_eq!(done.published, 1, "the drafted remark was not published");
        assert!(
            done.review.body.is_none(),
            "a cover message was invented: {:?}",
            done.review.body
        );
        let blocks = standing_blocks(&db, &c.id).unwrap();
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        assert_eq!(blocks[0].user_id, bob.id);
        // Alice's draft is untouched by any of it: a review is one
        // person's act, and bob's submit must not publish her words.
        let mine = pending_review(&db, &c.id, &alice).unwrap().unwrap();
        assert_eq!(mine.id, first.id);
        assert_eq!(mine.body.as_deref(), Some("on reflection"));
    }

    /// Rename one table out from under a call, run it, and put the table
    /// back.
    ///
    /// Surgery on a single table rather than a dead database on purpose:
    /// every statement before the one that touches this table succeeds,
    /// so what fails is exactly the step being aimed at, and the
    /// assertion afterwards is about that step rather than about a
    /// server with nothing underneath it.
    fn hide_table<T>(db: &ControlDb, table: &str, f: impl FnOnce() -> T) -> T {
        db.lock()
            .execute(
                &format!("ALTER TABLE {table} RENAME TO {table}_hidden"),
                &[],
            )
            .unwrap_or_else(|e| panic!("hide {table}: {e}"));
        let out = f();
        db.lock()
            .execute(
                &format!("ALTER TABLE {table}_hidden RENAME TO {table}"),
                &[],
            )
            .unwrap_or_else(|e| panic!("restore {table}: {e}"));
        out
    }

    /// A review is one act, and a failure part way through it means none
    /// of it happened.
    ///
    /// `submit_review` writes four things in one transaction — the
    /// verdict, the publication of every drafted comment, the approval
    /// (or its revocation), and the audit trail — and each of them is
    /// somebody's evidence. A partial commit is the worst outcome
    /// available here, and each half is a different kind of wrong: an
    /// approval with no review behind it is a yes nobody said, published
    /// comments with no verdict are a reviewer's drafts leaked before
    /// they meant to speak, and a verdict with no audit row is an
    /// approval no auditor can attribute. `withdraw_review` is the same
    /// argument in miniature: the row must not stop blocking unless the
    /// record of who cleared it was written too.
    ///
    /// Each step is failed on its own by renaming the table it writes,
    /// and after each the assertion is that the record is untouched —
    /// not merely that an error came back.
    #[test]
    fn a_review_that_fails_part_way_through_leaves_nothing_behind() {
        let (db, org, repo, alice, ctx) = world("changes-review-atomic");
        let (c, ps, _) = register(&db, &org, &repo, "Iat0m1c01", &"a".repeat(40), "atom", &ctx);
        let review = open_review(&db, &c.id, &ps.id, &alice, None)
            .unwrap()
            .unwrap();
        add_comment(
            &db,
            &NewComment {
                author_principal: &format!("user:{alice}"),
                author_user_id: Some(&alice),
                review_id: Some(&review.id),
                ..say(&c.id, ps.number, "one drafted remark")
            },
        )
        .unwrap()
        .unwrap();

        let submit = |verdict: ReviewVerdict, body: Option<&str>| {
            submit_review(
                &db,
                &SubmitReview {
                    change_id: &c.id,
                    patchset_id: &ps.id,
                    user_id: &alice,
                    verdict,
                    body,
                },
                &ctx,
            )
        };
        // Nothing on the record, asserted the way a reader would find
        // out: no verdict, no approval, and the remark still unsaid.
        let nothing_happened = |what: &str| {
            assert!(
                reviews_for(&db, &c.id).unwrap().is_empty(),
                "{what}: a verdict was recorded"
            );
            assert!(
                approvals_for(&db, &ps.id).unwrap().is_empty(),
                "{what}: an approval stands with no review behind it"
            );
            assert!(
                comments_for(&db, &c.id).unwrap().is_empty(),
                "{what}: a drafted remark was published early"
            );
            assert!(
                pending_review(&db, &c.id, &alice).unwrap().is_some(),
                "{what}: the draft was consumed by a submit that failed"
            );
        };

        // The publication of the drafted comments, the approval, and the
        // audit row: each failed alone, each leaving the whole act
        // undone.
        for table in ["change_comments", "approvals", "audit_log"] {
            let e = hide_table(&db, table, || submit(ReviewVerdict::Approve, None)).unwrap_err();
            assert!(e.contains("submit review"), "{table}: {e}");
            nothing_happened(table);
        }
        // The other side of the verdict: asking for changes revokes the
        // author's own standing approval in the same instant, and if
        // that write cannot happen the "no" must not be recorded either
        // — a block beside a live approval by the same person is the one
        // state this feature exists to make impossible.
        approve(&db, &c.id, &ps.id, &alice, &ctx).unwrap();
        let e = hide_table(&db, "approvals", || {
            submit(ReviewVerdict::RequestChanges, Some("the retry loop"))
        })
        .unwrap_err();
        assert!(e.contains("submit review"), "{e}");
        assert!(
            reviews_for(&db, &c.id).unwrap().is_empty(),
            "a block was recorded while the approval it revokes still stands"
        );
        assert_eq!(approvals_for(&db, &ps.id).unwrap().len(), 1);

        // Healthy, the same call does all of it at once.
        let done = submit(ReviewVerdict::RequestChanges, Some("the retry loop"))
            .unwrap()
            .unwrap();
        assert!(done.approval_revoked);
        assert_eq!(done.published, 1);
        assert!(approvals_for(&db, &ps.id).unwrap().is_empty());
        assert_eq!(standing_blocks(&db, &c.id).unwrap().len(), 1);

        // Withdrawing is the same rule: the row stops blocking only if
        // the record of who cleared it was written too. Failing the
        // verdict row and failing the audit row are both a block that
        // still stands.
        for table in ["reviews", "audit_log"] {
            let e =
                hide_table(&db, table, || withdraw_review(&db, &c.id, &alice, &ctx)).unwrap_err();
            assert!(e.contains("withdraw review"), "{table}: {e}");
            assert_eq!(
                standing_blocks(&db, &c.id).unwrap().len(),
                1,
                "{table}: the block was cleared with no record of who cleared it"
            );
        }
        assert!(withdraw_review(&db, &c.id, &alice, &ctx).unwrap());
        assert!(standing_blocks(&db, &c.id).unwrap().is_empty());

        // Approving what you have already approved is not a second
        // approval: the standing row is left alone, `approved` comes
        // back false, and no second `change.approve` is written. An
        // auditor counting approvals must not find two where one person
        // said yes once, and the sufficiency engine counts these rows.
        approve(&db, &c.id, &ps.id, &alice, &ctx).unwrap();
        let again = submit(ReviewVerdict::Approve, None).unwrap().unwrap();
        assert!(
            !again.approved,
            "a standing approval was reported as newly given"
        );
        assert_eq!(
            approvals_for(&db, &ps.id).unwrap().len(),
            1,
            "one person said yes once and it was recorded twice"
        );

        // The same rule one level up. `list` is what the land queue, the
        // landed badge and the lander's stack reconciliation ask, and
        // every one of them reads an empty answer as a fact about the
        // world: nothing to land, nothing landed this week, no stack to
        // reconcile. A failure that came back as an empty page would be
        // acted on rather than reported.
        assert!(
            hide_table(&db, "changes", || list(&db, &repo, None, 50)).is_err(),
            "a database failure came back as a page of changes"
        );
    }
}
