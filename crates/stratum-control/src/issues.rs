//! Issues, comments and labels: the place a stranger says something is
//! broken.
//!
//! The authorization rule this module exists to serve lives one layer
//! up, but it shapes everything here: **filing an issue needs read
//! access, not write.** Gating it on write scope would make issues
//! useless on open source, which is the entire point of the feature. So
//! the writer of a row here is very often somebody with no push
//! credential and no membership, and every bound below is written on the
//! assumption that the input is hostile — I13 refusals with a sentence,
//! never truncations, because silently storing half of what somebody
//! typed is answering a different question than the one they asked.
//!
//! Two things are worth reading the SQL for.
//!
//! **Numbers are allocated with `UPDATE … RETURNING`**, inside the same
//! transaction as the insert. Two people filing at once is the ordinary
//! case, not the edge case; a read-then-write would hand them both `#42`
//! and fail one of them on the unique index.
//!
//! **Conversations read by `seq`, not by time.** `issue_comments.seq` is
//! BIGSERIAL for migration 0017's reason: `created_at` is millisecond
//! wall-clock and a ULID's tail is *random* within one millisecond, so
//! ordering by `(created_at, id)` renders two comments posted in the
//! same millisecond in an order nobody typed. In a conversation, order
//! is meaning, and the database's own sequence is the only thing that
//! records who spoke first.

use crate::db::{is_unique_violation, Conn, ControlDb};
use crate::ids::{now_ms, ulid, valid_id};
use postgres::types::ToSql;
use std::collections::HashMap;

/// Longest title we will store. Longer is refused, not trimmed.
pub const MAX_TITLE: usize = 400;
/// Longest issue body, and the same bound for a comment: both are prose
/// somebody typed into the same kind of box.
pub const MAX_BODY: usize = 65536;
/// Longest label name. Labels are pills in a table row, and a 5000-character
/// one is a layout attack rather than a label.
pub const MAX_LABEL_NAME: usize = 50;
/// Longest label description — the tooltip beside the pill.
pub const MAX_LABEL_DESCRIPTION: usize = 400;
/// Most labels one issue may carry.
pub const MAX_LABELS: usize = 20;
/// Most issues one page may ask for.
pub const MAX_LIMIT: i64 = 100;

/// The label colours the design system publishes, and the only values
/// this column accepts.
///
/// These are **token names**, not hex, and the distinction is the whole
/// point: a hex is *unfixable later*. Change the palette and every
/// stored hex is silently wrong in both themes, with no migration that
/// can know what the author meant — a token name re-resolves. It is also
/// the one DESIGN.md violation no browser test can see, because the
/// database is where the design system's reach stops; validating here is
/// the only place it can be caught.
///
/// Every name is taken from `web/shared/tokens.css` — six are custom
/// properties there (`--series-1..3`, `--status-good/warning/serious`)
/// and `neutral` is the sentinel for "no tint", rendered from the
/// existing ink and border tokens rather than a `--neutral` property,
/// which the design system does not publish.
///
/// **Seven is a borrowed ceiling, not a considered maximum.** An issue
/// tracker wants more than this before anybody invents one of their own
/// — bug, enhancement, documentation, good first issue, help wanted is
/// the standard set, and "good first issue" is load-bearing on an
/// open-source forge rather than decoration. The list is capped by what
/// the design system currently publishes. Growing it means adding
/// tokens, and DESIGN.md requires the dataviz validator to be re-run
/// against both modes when the palette changes; a token invented here
/// would be an unvalidated colour dressed as a system one, which is
/// worse than a small palette. That growth is its own change, with a
/// validator run attached.
pub const LABEL_COLORS: &[&str] = &[
    "series-1",
    "series-2",
    "series-3",
    "status-good",
    "status-warning",
    "status-serious",
    "neutral",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub id: String,
    pub number: i32,
    pub title: String,
    pub body: String,
    pub state: String,
    /// The author's account id, and the **only** thing an authorization
    /// check may compare. `PATCH …/issues/:number` asks "is the caller
    /// this issue's author", and the caller resolves to a user id; a
    /// handle is a mutable display name, so keying that check on one is
    /// a rename away from locking an author out of their own issue or
    /// handing it to whoever took the name next.
    pub author_id: Option<String>,
    /// The author's handle, resolved — what gets rendered, never what
    /// gets compared. `None` for an imported issue whose author has no
    /// account here, and for one whose account is gone.
    pub author: Option<String>,
    /// The name an imported issue was filed under. `NULL` for native
    /// issues, so the join to `users` stays the single source of truth
    /// and an unmapped importee is never attributed to a local account.
    pub author_label: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
    /// Whole label objects, not names: the UI renders a coloured pill
    /// and cannot look each one up.
    pub labels: Vec<Label>,
    pub comment_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub id: String,
    pub name: String,
    pub color: String,
    pub description: String,
}

/// A milestone, as a reader sees one.
///
/// Carries the **number it came with**, for the same reason an issue
/// does: `%v1.0` and a milestone URL from an old changelog have to keep
/// meaning what they said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    pub id: String,
    pub number: i32,
    pub title: String,
    pub description: String,
    /// `open` or `closed`, enforced by the table's own CHECK.
    pub state: String,
    pub due_on: Option<i64>,
    /// How many issues point at it, open and closed.
    pub open_issues: i64,
    pub closed_issues: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: String,
    pub seq: i64,
    pub body: String,
    pub author: Option<String>,
    pub author_label: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// What a listing was asked for. `state` is `open`, `closed` or `all`;
/// `None` means all, because a caller who named no state asked no
/// question about state.
#[derive(Debug, Clone)]
pub struct Filter {
    pub state: Option<String>,
    pub label: Option<String>,
    pub author: Option<String>,
    pub q: Option<String>,
    pub limit: i64,
    pub before: Option<i32>,
    /// `newest` (the default) or `oldest`.
    ///
    /// **Both are keyed on the issue number, and that is the whole
    /// reason the set is only two.** The cursor is a number and the
    /// page is `number < before`; a sort on `updated_at` would order by
    /// one column and page by another, so a page boundary would land in
    /// the wrong place and rows would be skipped or repeated — silently,
    /// and only on the second page, which is the kind of bug nobody
    /// reports because it looks like the list simply being short.
    ///
    /// `updated` is what GitHub defaults to and it is worth having. It
    /// needs a compound cursor (`updated_at`, `number`) rather than an
    /// extra `ORDER BY`, so it is refused by name until somebody builds
    /// that, instead of shipped as a sort that breaks when paged.
    pub sort: Option<String>,
}

/// Both counts, always.
///
/// The UI shows "12 Open / 40 Closed" as a pair, and computing one of
/// them from a filtered list is exactly how that number goes wrong: a
/// page filtered to `open` knows nothing about how many are closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub open: i64,
    pub closed: i64,
}

const ISSUE_COLS: &str = "id, number, title, body, state, author_id, author_label, \
                          created_at, updated_at, closed_at";
const COMMENT_COLS: &str = "id, seq, body, author_id, author_label, created_at, updated_at";

// --- bounds: refusals with a sentence -------------------------------

/// Counted in characters, not bytes, everywhere below: a bound measured
/// in bytes refuses a title of 200 emoji and accepts one of 400 Latin
/// letters, which is a different rule for different alphabets.
fn check_title(title: &str) -> Result<(), String> {
    let n = title.trim().chars().count();
    if n == 0 {
        return Err("a title is required".to_string());
    }
    if n > MAX_TITLE {
        return Err(format!("title is too long: at most {MAX_TITLE} characters"));
    }
    Ok(())
}

fn check_body(body: &str) -> Result<(), String> {
    if body.chars().count() > MAX_BODY {
        return Err(format!("body is too long: at most {MAX_BODY} characters"));
    }
    Ok(())
}

fn check_comment(body: &str) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("a comment cannot be empty".to_string());
    }
    if body.chars().count() > MAX_BODY {
        return Err(format!(
            "comment is too long: at most {MAX_BODY} characters"
        ));
    }
    Ok(())
}

/// A label name is one line of visible text. Control characters are
/// refused rather than stripped: a name carrying a newline or a NUL
/// breaks the pill it renders in, and a NUL is text PostgreSQL refuses
/// outright — which would arrive as a 500 with a fragment of the
/// database layer in it instead of a sentence.
fn check_label_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a label name is required".to_string());
    }
    if name.chars().count() > MAX_LABEL_NAME {
        return Err(format!(
            "label name is too long: at most {MAX_LABEL_NAME} characters"
        ));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("a label name cannot contain control characters".to_string());
    }
    Ok(())
}

fn check_label_description(description: &str) -> Result<(), String> {
    if description.chars().count() > MAX_LABEL_DESCRIPTION {
        return Err(format!(
            "label description is too long: at most {MAX_LABEL_DESCRIPTION} characters"
        ));
    }
    Ok(())
}

/// A colour is a design-system token name, and the error says which
/// names exist — somebody sending `#ff0000` needs to be told what to
/// send instead, not just that they were wrong.
fn check_color(color: &str) -> Result<(), String> {
    if LABEL_COLORS.contains(&color) {
        return Ok(());
    }
    Err(format!(
        "invalid label colour {color:?}: expected one of {}",
        LABEL_COLORS.join(", ")
    ))
}

/// The states a listing may be filtered by. `all` is not a state a row
/// can be in; it is the absence of the filter.
fn check_filter_state(state: &str) -> Result<(), String> {
    if matches!(state, "open" | "closed" | "all") {
        return Ok(());
    }
    Err(format!(
        "invalid state {state:?}: expected open, closed or all"
    ))
}

/// The states an issue can actually be in.
fn check_state(state: &str) -> Result<(), String> {
    if matches!(state, "open" | "closed") {
        return Ok(());
    }
    Err(format!("invalid state {state:?}: expected open or closed"))
}

/// Escape a searcher's text for `LIKE`, so `%` matches a literal
/// percent. Without this a query of `_` matches every one-character
/// title, which reads as the search being broken.
fn like_pattern(q: &str) -> String {
    let mut out = String::with_capacity(q.len() + 2);
    out.push('%');
    for c in q.to_lowercase().chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

// --- filter → SQL ---------------------------------------------------

/// One bound parameter. The shaping is pure so it can be tested without
/// a database, which means it cannot hand back `&dyn ToSql` borrows into
/// a temporary; it hands back owned values that the caller binds.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Arg {
    Text(String),
    Int(i32),
}

/// A `WHERE` fragment and the arguments it expects, in order. `$1` is
/// always the repo id and is not in `args`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shaped {
    sql: String,
    args: Vec<Arg>,
}

impl Shaped {
    /// Bind the owned arguments for a query. The `Vec` borrows from
    /// `self`, so `self` must outlive the call.
    fn params(&self) -> Vec<&(dyn ToSql + Sync)> {
        self.args
            .iter()
            .map(|a| match a {
                Arg::Text(s) => s as &(dyn ToSql + Sync),
                Arg::Int(i) => i as &(dyn ToSql + Sync),
            })
            .collect()
    }
}

/// Shape a filter into SQL.
///
/// Called twice per listing, and the two calls are the point: the rows
/// are fetched with the state filter applied, and the counts are
/// computed with it dropped — `with_state: false` — so that a page
/// filtered to `open` still knows how many are closed. `with_before` is
/// dropped for the same reason: paging to the second page must not make
/// the header's totals shrink.
fn shape(f: &Filter, with_state: bool, with_before: bool) -> Result<Shaped, String> {
    let mut sql = String::from("i.repo_id = $1");
    let mut args: Vec<Arg> = Vec::new();
    let mut slot = 1;
    let mut next = |args: &mut Vec<Arg>, a: Arg| {
        args.push(a);
        slot += 1;
        slot
    };

    if let Some(state) = f.state.as_deref() {
        check_filter_state(state)?;
        if with_state && state != "all" {
            let n = next(&mut args, Arg::Text(state.to_string()));
            sql.push_str(&format!(" AND i.state = ${n}"));
        }
    }
    if let Some(label) = f.label.as_deref() {
        check_label_name(label)?;
        let n = next(&mut args, Arg::Text(label.trim().to_lowercase()));
        sql.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM issue_labels il \
             JOIN labels l ON l.id = il.label_id \
             WHERE il.issue_id = i.id AND lower(l.name) = ${n})"
        ));
    }
    if let Some(author) = f.author.as_deref() {
        let n = next(&mut args, Arg::Text(author.trim().to_lowercase()));
        sql.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM users u \
             WHERE u.id = i.author_id AND lower(u.handle) = ${n})"
        ));
    }
    if let Some(q) = f.q.as_deref() {
        // A NUL is text PostgreSQL refuses outright, and no title or
        // body can contain one, so it matches nothing. Saying that is
        // better than a 500 quoting the database layer.
        if q.contains('\0') {
            return Err("a search cannot contain a NUL byte".to_string());
        }
        if q.chars().count() > MAX_TITLE {
            return Err(format!(
                "search is too long: at most {MAX_TITLE} characters"
            ));
        }
        let n = next(&mut args, Arg::Text(like_pattern(q)));
        sql.push_str(&format!(
            " AND (lower(i.title) LIKE ${n} ESCAPE '\\' \
             OR lower(i.body) LIKE ${n} ESCAPE '\\')"
        ));
    }
    if with_before {
        if let Some(before) = f.before {
            let n = next(&mut args, Arg::Int(before));
            sql.push_str(&format!(" AND i.number < ${n}"));
        }
    }
    Ok(Shaped { sql, args })
}

/// The page size actually used. A caller asking for a million rows is
/// asking for the maximum page, not for an error — this is the one
/// bound the contract clamps rather than refuses, because `limit` is a
/// hint about rendering and not something the caller typed.
fn page_limit(limit: i64) -> i64 {
    limit.clamp(1, MAX_LIMIT)
}

// --- reads ----------------------------------------------------------

/// Resolve author ids to handles in one query. An id with no handle, or
/// no user left at all, is simply absent from the map — and absent is
/// exactly `author: None`.
fn handles(c: &mut Conn, ids: &[String]) -> Result<HashMap<String, String>, postgres::Error> {
    let ids = ids.to_vec();
    let rows = c.query(
        "SELECT id, handle FROM users WHERE id = ANY($1) AND handle IS NOT NULL",
        &[&ids],
    )?;
    Ok(rows
        .iter()
        .map(|r| (r.get("id"), r.get("handle")))
        .collect())
}

/// Turn issue rows into issues: one query for the authors, one for the
/// labels, one for the comment counts. Not one per row — an issues index
/// is 25 rows, and doing it per row is how a list page becomes 75
/// round trips.
fn hydrate(c: &mut Conn, rows: &[postgres::Row]) -> Result<Vec<Issue>, postgres::Error> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = rows.iter().map(|r| r.get("id")).collect();
    let author_ids: Vec<String> = rows.iter().filter_map(|r| r.get("author_id")).collect();
    let by_id = handles(c, &author_ids)?;

    let mut labels: HashMap<String, Vec<Label>> = HashMap::new();
    for r in c.query(
        "SELECT il.issue_id, l.id, l.name, l.color, l.description \
         FROM issue_labels il JOIN labels l ON l.id = il.label_id \
         WHERE il.issue_id = ANY($1) ORDER BY lower(l.name)",
        &[&ids],
    )? {
        labels.entry(r.get("issue_id")).or_default().push(Label {
            id: r.get("id"),
            name: r.get("name"),
            color: r.get("color"),
            description: r.get("description"),
        });
    }

    let mut counts: HashMap<String, i64> = HashMap::new();
    for r in c.query(
        "SELECT issue_id, COUNT(*) AS n FROM issue_comments \
         WHERE issue_id = ANY($1) GROUP BY issue_id",
        &[&ids],
    )? {
        counts.insert(r.get("issue_id"), r.get("n"));
    }

    Ok(rows
        .iter()
        .map(|r| {
            let id: String = r.get("id");
            let author_id: Option<String> = r.get("author_id");
            Issue {
                author: author_id.as_ref().and_then(|a| by_id.get(a).cloned()),
                author_id,
                labels: labels.remove(&id).unwrap_or_default(),
                comment_count: counts.get(&id).copied().unwrap_or(0),
                number: r.get("number"),
                title: r.get("title"),
                body: r.get("body"),
                state: r.get("state"),
                author_label: r.get("author_label"),
                created_at: r.get("created_at"),
                updated_at: r.get("updated_at"),
                closed_at: r.get("closed_at"),
                id,
            }
        })
        .collect())
}

/// One issue, by its human-facing number.
pub fn get(db: &ControlDb, repo_id: &str, number: i32) -> Result<Option<Issue>, String> {
    let mut c = db.lock();
    let row = c
        .query_opt(
            &format!("SELECT {ISSUE_COLS} FROM issues i WHERE i.repo_id = $1 AND i.number = $2"),
            &[&repo_id, &number],
        )
        .map_err(|e| format!("read issue: {e}"))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let mut issues = hydrate(&mut c, &[row]).map_err(|e| format!("read issue: {e}"))?;
    Ok(Some(issues.remove(0)))
}

/// A page of issues, newest number first, and both counts.
/// The `ORDER BY` direction for a sort name, or a refusal naming the
/// ones that work.
///
/// Interpolated into SQL, which is only safe because it can never be
/// anything but one of two literals from this function — an unknown
/// name is refused here rather than defaulted. Defaulting would be the
/// dangerous shape twice over: it would put caller-supplied text one
/// edit away from the query string, and it would answer a request for
/// an ordering we do not have with a different ordering and no word
/// about it.
fn order_by(sort: Option<&str>) -> Result<&'static str, String> {
    match sort {
        None | Some("") | Some("newest") => Ok("DESC"),
        Some("oldest") => Ok("ASC"),
        // Named specifically rather than lumped in with a typo: this
        // one is a real ordering that is not built yet, and saying so
        // is more use than "unknown sort".
        Some("updated") => Err("sort \"updated\" is not available yet: paging by update \
             time needs a compound cursor, and ordering by one column while paging by \
             another skips rows"
            .to_string()),
        Some(other) => Err(format!(
            "unknown sort {other:?} — the sorts are \"newest\" and \"oldest\""
        )),
    }
}

pub fn list(db: &ControlDb, repo_id: &str, f: &Filter) -> Result<(Vec<Issue>, Counts), String> {
    let order = order_by(f.sort.as_deref())?;
    let rows_where = shape(f, true, true)?;
    let counts_where = shape(f, false, false)?;
    let limit = page_limit(f.limit);

    let mut c = db.lock();
    let mut params: Vec<&(dyn ToSql + Sync)> = vec![&repo_id];
    let bound = rows_where.params();
    params.extend(bound.iter().copied());
    params.push(&limit);
    let rows = c
        .query(
            &format!(
                "SELECT {ISSUE_COLS} FROM issues i WHERE {} \
                 ORDER BY i.number {order} LIMIT ${}",
                rows_where.sql,
                rows_where.args.len() + 2
            ),
            &params,
        )
        .map_err(|e| format!("list issues: {e}"))?;
    let issues = hydrate(&mut c, &rows).map_err(|e| format!("list issues: {e}"))?;

    let mut cparams: Vec<&(dyn ToSql + Sync)> = vec![&repo_id];
    let cbound = counts_where.params();
    cparams.extend(cbound.iter().copied());
    let crows = c
        .query(
            &format!(
                "SELECT i.state, COUNT(*) AS n FROM issues i WHERE {} GROUP BY i.state",
                counts_where.sql
            ),
            &cparams,
        )
        .map_err(|e| format!("count issues: {e}"))?;
    let mut counts = Counts { open: 0, closed: 0 };
    for r in &crows {
        let state: String = r.get("state");
        let n: i64 = r.get("n");
        if state == "open" {
            counts.open = n;
        } else {
            counts.closed = n;
        }
    }
    Ok((issues, counts))
}

/// A conversation, in the order it was spoken. `seq`, never time.
pub fn comments(db: &ControlDb, issue_id: &str) -> Result<Vec<Comment>, String> {
    if !valid_id(issue_id) {
        return Ok(Vec::new());
    }
    let mut c = db.lock();
    let rows = c
        .query(
            &format!("SELECT {COMMENT_COLS} FROM issue_comments WHERE issue_id = $1 ORDER BY seq"),
            &[&issue_id],
        )
        .map_err(|e| format!("read comments: {e}"))?;
    let author_ids: Vec<String> = rows.iter().filter_map(|r| r.get("author_id")).collect();
    let by_id = handles(&mut c, &author_ids).map_err(|e| format!("read comments: {e}"))?;
    Ok(rows.iter().map(|r| row_to_comment(r, &by_id)).collect())
}

fn row_to_comment(r: &postgres::Row, by_id: &HashMap<String, String>) -> Comment {
    let author_id: Option<String> = r.get("author_id");
    Comment {
        id: r.get("id"),
        seq: r.get("seq"),
        body: r.get("body"),
        author: author_id.and_then(|a| by_id.get(&a).cloned()),
        author_label: r.get("author_label"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

/// Every label defined on the repository, alphabetically.
pub fn labels(db: &ControlDb, repo_id: &str) -> Result<Vec<Label>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT id, name, color, description FROM labels \
             WHERE repo_id = $1 ORDER BY lower(name)",
            &[&repo_id],
        )
        .map_err(|e| format!("list labels: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| Label {
            id: r.get("id"),
            name: r.get("name"),
            color: r.get("color"),
            description: r.get("description"),
        })
        .collect())
}

/// Every milestone on the repository, by the number it came with.
///
/// This did not exist for as long as the importer had been writing
/// milestones: `put_milestone` filled a table that no function listed,
/// no route served and no view rendered, and `issues.milestone_id` was
/// never set — so a migrated project's milestones arrived as orphan rows
/// nobody could reach, under an import page that said "Milestones: done".
///
/// The counts are computed here rather than by the caller so that "3
/// open" on a milestone and the issue list filtered by it can never
/// disagree; they come from one join over the same rows.
pub fn milestones(db: &ControlDb, repo_id: &str) -> Result<Vec<Milestone>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT m.id, m.number, m.title, m.description, m.state, m.due_on, \
                    COUNT(i.id) FILTER (WHERE i.state = 'open')   AS open_issues, \
                    COUNT(i.id) FILTER (WHERE i.state = 'closed') AS closed_issues \
             FROM milestones m LEFT JOIN issues i ON i.milestone_id = m.id \
             WHERE m.repo_id = $1 \
             GROUP BY m.id, m.number, m.title, m.description, m.state, m.due_on \
             ORDER BY m.number",
            &[&repo_id],
        )
        .map_err(|e| format!("list milestones: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| Milestone {
            id: r.get("id"),
            number: r.get("number"),
            title: r.get("title"),
            description: r.get("description"),
            state: r.get("state"),
            due_on: r.get("due_on"),
            open_issues: r.get("open_issues"),
            closed_issues: r.get("closed_issues"),
        })
        .collect())
}

/// The id of the milestone numbered `number`, for linking an issue to it.
///
/// Returns `None` rather than erroring for a number that is not here: an
/// import can meet an issue whose milestone was deleted upstream, and
/// refusing the whole issue over a dangling reference would lose the
/// issue to keep the pointer.
pub fn milestone_id_by_number(
    db: &ControlDb,
    repo_id: &str,
    number: i32,
) -> Result<Option<String>, String> {
    db.lock()
        .query_opt(
            "SELECT id FROM milestones WHERE repo_id = $1 AND number = $2",
            &[&repo_id, &number],
        )
        .map(|r| r.map(|r| r.get("id")))
        .map_err(|e| format!("milestone by number: {e}"))
}

/// Point an issue at a milestone, or at nothing.
pub fn set_issue_milestone(
    db: &ControlDb,
    repo_id: &str,
    number: i32,
    milestone_id: Option<&str>,
) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE issues SET milestone_id = $3 WHERE repo_id = $1 AND number = $2",
            &[&repo_id, &number, &milestone_id],
        )
        .map(|_| ())
        .map_err(|e| format!("set issue milestone: {e}"))
}

// --- writes ---------------------------------------------------------

/// File an issue.
///
/// The number is allocated inside the insert's transaction with
/// `UPDATE … RETURNING`, never read-then-write: the counter row is
/// created on first use with an upsert, and the `UPDATE` both reserves
/// the number and reports it in one statement, so two people filing in
/// the same second get `#1` and `#2` rather than a unique-index failure.
pub fn open(
    db: &ControlDb,
    repo_id: &str,
    author_id: &str,
    title: &str,
    body: &str,
) -> Result<Issue, String> {
    check_title(title)?;
    check_body(body)?;
    let id = ulid();
    let now = now_ms();
    let issue_id = id.clone();
    let title_owned = title.trim().to_string();
    let body_owned = body.to_string();
    let number: i32 = db
        .lock()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO issue_counters (repo_id, next_number) VALUES ($1, 1) \
                 ON CONFLICT (repo_id) DO NOTHING",
                &[&repo_id],
            )?;
            let number: i32 = tx
                .query_one(
                    "UPDATE issue_counters SET next_number = next_number + 1 \
                     WHERE repo_id = $1 RETURNING next_number - 1",
                    &[&repo_id],
                )?
                .get(0);
            tx.execute(
                "INSERT INTO issues \
                 (id, repo_id, number, title, body, state, author_id, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, 'open', $6, $7, $7)",
                &[
                    &issue_id,
                    &repo_id,
                    &number,
                    &title_owned,
                    &body_owned,
                    &author_id,
                    &now,
                ],
            )?;
            Ok(number)
        })
        .map_err(|e| format!("open issue: {e}"))?;
    let author = handles(&mut db.lock(), &[author_id.to_string()])
        .map_err(|e| format!("open issue: {e}"))?
        .remove(author_id);
    Ok(Issue {
        id,
        number,
        title: title.trim().to_string(),
        body: body.to_string(),
        state: "open".to_string(),
        author_id: Some(author_id.to_string()),
        author,
        author_label: None,
        created_at: now,
        updated_at: now,
        closed_at: None,
        labels: Vec::new(),
        comment_count: 0,
    })
}

/// Close or reopen. `closed_at` is set on the way in and cleared on the
/// way out, so "when was this closed" is never a stale answer from a
/// previous close.
pub fn set_state(
    db: &ControlDb,
    repo_id: &str,
    number: i32,
    state: &str,
    now: i64,
) -> Result<Issue, String> {
    check_state(state)?;
    let mut c = db.lock();
    let row = c
        .query_opt(
            &format!(
                "UPDATE issues i SET state = $3, updated_at = $4, \
                 -- `$4::BIGINT` and not a bare `$4`. Postgres deduces a
                 -- parameter's type from every place it appears, and
                 -- inside `CASE ... ELSE NULL` this one is `unknown`
                 -- while `updated_at = $4` says bigint — which is
                 -- 42P08 (inconsistent types deduced for parameter
                 -- $4), and made every close and reopen fail. The cast
                 -- settles it in the one place that was ambiguous.
                 closed_at = CASE WHEN $3 = 'closed' THEN $4::BIGINT ELSE NULL END \
                 WHERE i.repo_id = $1 AND i.number = $2 RETURNING {ISSUE_COLS}"
            ),
            &[&repo_id, &number, &state, &now],
        )
        .map_err(|e| format!("set issue state: {}", crate::db::detail(&e)))?;
    let Some(row) = row else {
        return Err(format!("no issue #{number}"));
    };
    let mut issues = hydrate(&mut c, &[row])
        .map_err(|e| format!("set issue state: {}", crate::db::detail(&e)))?;
    Ok(issues.remove(0))
}

/// Edit the title, the body, or both. `None` means "leave it alone" —
/// which is not the same as an empty string, and `COALESCE` in the SQL
/// is what keeps those two apart.
pub fn edit(
    db: &ControlDb,
    repo_id: &str,
    number: i32,
    title: Option<&str>,
    body: Option<&str>,
) -> Result<Issue, String> {
    let title = match title {
        Some(t) => {
            check_title(t)?;
            Some(t.trim().to_string())
        }
        None => None,
    };
    if let Some(b) = body {
        check_body(b)?;
    }
    let now = now_ms();
    let mut c = db.lock();
    let row = c
        .query_opt(
            &format!(
                "UPDATE issues i SET title = COALESCE($3::text, i.title), \
                 body = COALESCE($4::text, i.body), updated_at = $5 \
                 WHERE i.repo_id = $1 AND i.number = $2 RETURNING {ISSUE_COLS}"
            ),
            &[&repo_id, &number, &title, &body, &now],
        )
        .map_err(|e| format!("edit issue: {e}"))?;
    let Some(row) = row else {
        return Err(format!("no issue #{number}"));
    };
    let mut issues = hydrate(&mut c, &[row]).map_err(|e| format!("edit issue: {e}"))?;
    Ok(issues.remove(0))
}

/// Say something on an issue.
///
/// The issue is checked in the same transaction as the insert rather
/// than left to the foreign key: a missing issue is a sentence the
/// caller can act on, where an FK violation is a 500 quoting a
/// constraint name at somebody who typed into a text box.
pub fn comment(
    db: &ControlDb,
    issue_id: &str,
    author_id: &str,
    body: &str,
) -> Result<Comment, String> {
    check_comment(body)?;
    if !valid_id(issue_id) {
        return Err("no such issue".to_string());
    }
    let id = ulid();
    let now = now_ms();
    let comment_id = id.clone();
    let body_owned = body.to_string();
    // `Option` rather than an error out of the closure: `postgres::Error`
    // cannot be constructed by us, and an empty committed transaction is
    // exactly as correct as a rolled-back one when nothing was written.
    let seq: Option<i64> = db
        .lock()
        .transaction(move |tx| {
            if tx
                .query_opt("SELECT 1 FROM issues WHERE id = $1", &[&issue_id])?
                .is_none()
            {
                return Ok(None);
            }
            let row = tx.query_one(
                "INSERT INTO issue_comments (id, issue_id, body, author_id, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $5) RETURNING seq",
                &[&comment_id, &issue_id, &body_owned, &author_id, &now],
            )?;
            Ok(Some(row.get(0)))
        })
        .map_err(|e| format!("comment: {e}"))?;
    let seq = seq.ok_or_else(|| "no such issue".to_string())?;
    let author = handles(&mut db.lock(), &[author_id.to_string()])
        .map_err(|e| format!("comment: {e}"))?
        .remove(author_id);
    Ok(Comment {
        id,
        seq,
        body: body.to_string(),
        author,
        author_label: None,
        created_at: now,
        updated_at: now,
    })
}

/// Define a label.
pub fn create_label(
    db: &ControlDb,
    repo_id: &str,
    name: &str,
    color: &str,
    description: &str,
) -> Result<Label, String> {
    check_label_name(name)?;
    check_color(color)?;
    check_label_description(description)?;
    let id = ulid();
    let name = name.trim().to_string();
    db.lock()
        .execute(
            "INSERT INTO labels (id, repo_id, name, color, description, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[&id, &repo_id, &name, &color, &description, &now_ms()],
        )
        .map_err(|e| {
            if is_unique_violation(&e) {
                format!("label {name:?} already exists")
            } else {
                format!("create label: {e}")
            }
        })?;
    Ok(Label {
        id,
        name,
        color: color.to_string(),
        description: description.to_string(),
    })
}

/// Remove a label from the repository, and from every issue carrying it
/// — `issue_labels` cascades, so a deleted label leaves no pill behind.
/// Write a milestone, keeping the number it came with.
///
/// Idempotent on `(repo_id, number)`: an interrupted import re-runs and
/// finds the row already there.
#[allow(clippy::too_many_arguments)]
pub fn put_milestone(
    db: &ControlDb,
    repo_id: &str,
    number: i32,
    title: &str,
    description: &str,
    state: &str,
    due_on: Option<i64>,
) -> Result<(), String> {
    check_title(title)?;
    check_state(state)?;
    db.lock()
        .execute(
            "INSERT INTO milestones \
             (id, repo_id, number, title, description, state, due_on, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (repo_id, number) DO UPDATE SET \
               title = EXCLUDED.title, description = EXCLUDED.description, \
               state = EXCLUDED.state, due_on = EXCLUDED.due_on",
            &[
                &ulid(),
                &repo_id,
                &number,
                &title.trim(),
                &description,
                &state,
                &due_on,
                &now_ms(),
            ],
        )
        .map(|_| ())
        .map_err(|e| format!("put milestone: {}", crate::db::detail(&e)))
}

/// Record the colour an imported label had upstream.
///
/// Separate from `create_label` because it is not a colour anybody
/// chose here: `labels.color` stays a design-system token, and this is
/// the project's own hex kept beside it so the pill can draw its hue
/// dot. A stored hex must never become the rendered colour — change the
/// palette and it is silently wrong in both themes, with no migration
/// able to know what was meant.
///
/// A malformed value is stored as NULL rather than refused: the label
/// itself is worth having, and the dot degrades to the neutral pill.
pub fn set_label_origin_color(
    db: &ControlDb,
    repo_id: &str,
    name: &str,
    hex: &str,
) -> Result<(), String> {
    let hex = hex.trim().trim_start_matches('#');
    let ok = matches!(hex.len(), 3 | 6) && hex.chars().all(|c| c.is_ascii_hexdigit());
    let value: Option<String> = ok.then(|| hex.to_lowercase());
    db.lock()
        .execute(
            "UPDATE labels SET origin_color = $3 WHERE repo_id = $1 AND lower(name) = lower($2)",
            &[&repo_id, &name, &value],
        )
        .map(|_| ())
        .map_err(|e| format!("set label colour: {}", crate::db::detail(&e)))
}

pub fn delete_label(db: &ControlDb, repo_id: &str, name: &str) -> Result<(), String> {
    check_label_name(name)?;
    let name = name.trim();
    let removed = db
        .lock()
        .execute(
            "DELETE FROM labels WHERE repo_id = $1 AND lower(name) = lower($2)",
            &[&repo_id, &name],
        )
        .map_err(|e| format!("delete label: {e}"))?;
    if removed == 0 {
        return Err(format!("no label {name:?}"));
    }
    Ok(())
}

/// Set an issue's labels to exactly `names` — the whole set, not a
/// delta, because that is what a checkbox list in a UI actually means.
///
/// Every name must already be a label on this repository. Creating one
/// implicitly would let anybody with `RepoWrite` grow the label
/// vocabulary by typo, and a triage vocabulary that grows by typo is
/// no vocabulary.
pub fn set_labels(
    db: &ControlDb,
    repo_id: &str,
    number: i32,
    names: &[String],
) -> Result<Issue, String> {
    if names.len() > MAX_LABELS {
        return Err(format!(
            "too many labels: at most {MAX_LABELS} on one issue"
        ));
    }
    for name in names {
        check_label_name(name)?;
    }
    let mut issue = get(db, repo_id, number)?.ok_or_else(|| format!("no issue #{number}"))?;

    // Resolved before the write, so an unknown name is refused by name
    // rather than by a foreign key — and refused before anything has
    // been removed.
    let wanted: Vec<String> = names.iter().map(|n| n.trim().to_lowercase()).collect();
    let defined = labels(db, repo_id)?;
    let mut chosen: Vec<Label> = Vec::new();
    for want in &wanted {
        let found = defined
            .iter()
            .find(|l| l.name.to_lowercase() == *want)
            .ok_or_else(|| format!("no label {want:?}"))?;
        if !chosen.iter().any(|l| l.id == found.id) {
            chosen.push(found.clone());
        }
    }
    chosen.sort_by_key(|l| l.name.to_lowercase());

    let now = now_ms();
    let issue_id = issue.id.clone();
    let ids: Vec<String> = chosen.iter().map(|l| l.id.clone()).collect();
    db.lock()
        .transaction(move |tx| {
            tx.execute("DELETE FROM issue_labels WHERE issue_id = $1", &[&issue_id])?;
            for label_id in &ids {
                tx.execute(
                    "INSERT INTO issue_labels (issue_id, label_id) VALUES ($1, $2)",
                    &[&issue_id, label_id],
                )?;
            }
            tx.execute(
                "UPDATE issues SET updated_at = $2 WHERE id = $1",
                &[&issue_id, &now],
            )?;
            Ok(())
        })
        .map_err(|e| format!("set labels: {e}"))?;
    issue.labels = chosen;
    issue.updated_at = now;
    Ok(issue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, NewRepo, RepoKind};

    fn db(hint: &str) -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap()
    }

    fn person(db: &ControlDb, handle: &str, email: &str) -> (String, String) {
        let u = crate::users::create(db, email, handle, Some("a long enough password")).unwrap();
        crate::usertokens::mark_verified(db, &u.id).unwrap();
        let ns = registry::create_personal_namespace(db, &u.id, handle, None).unwrap();
        (u.id, ns.id)
    }

    fn repo(db: &ControlDb, org_id: &str, name: &str) -> registry::Repo {
        registry::create_repo(
            db,
            org_id,
            &NewRepo {
                description: Some("a repository"),
                name,
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap()
    }

    fn filter() -> Filter {
        Filter {
            state: None,
            label: None,
            author: None,
            q: None,
            limit: 25,
            before: None,
            sort: None,
        }
    }

    /// The sorts, as the only two directions there are.
    ///
    /// `order_by` interpolates its answer straight into SQL, so what
    /// keeps that safe is not the caller but this function's inability
    /// to return anything else. That is worth a test rather than a
    /// comment: the refusals are the safety property, and a later edit
    /// adding a default arm would be quiet and wrong.
    #[test]
    fn a_sort_is_one_of_two_directions_or_a_refusal_that_names_them() {
        assert_eq!(order_by(None).unwrap(), "DESC");
        assert_eq!(order_by(Some("")).unwrap(), "DESC");
        assert_eq!(order_by(Some("newest")).unwrap(), "DESC");
        assert_eq!(order_by(Some("oldest")).unwrap(), "ASC");

        // A real ordering we have not built, refused by name and saying
        // why — so it does not read as a typo.
        let e = order_by(Some("updated")).unwrap_err();
        assert!(e.contains("compound cursor"), "{e}");

        // A typo is a different problem and gets a different answer,
        // naming what would have worked.
        let e = order_by(Some("nweest")).unwrap_err();
        assert!(e.contains("newest") && e.contains("oldest"), "{e}");

        // Nothing caller-supplied can reach the query string. If this
        // ever passes, the interpolation in `list` has become an
        // injection.
        assert!(order_by(Some("DESC; DROP TABLE issues")).is_err());
    }

    #[test]
    fn a_title_is_required_and_bounded() {
        assert!(check_title("a real title").is_ok());
        // Whitespace is not a title: a row whose title renders as an
        // empty cell is unclickable in the index.
        assert!(check_title("   ").unwrap_err().contains("required"));
        assert!(check_title("").unwrap_err().contains("required"));
        assert!(check_title(&"x".repeat(MAX_TITLE)).is_ok());
        let e = check_title(&"x".repeat(MAX_TITLE + 1)).unwrap_err();
        assert!(e.contains("too long"), "{e}");
        // Counted in characters, so 400 emoji are 400, not 1600 bytes.
        assert!(check_title(&"🙂".repeat(MAX_TITLE)).is_ok());
        assert!(check_title(&"🙂".repeat(MAX_TITLE + 1)).is_err());
    }

    #[test]
    fn a_body_may_be_empty_but_not_endless() {
        assert!(check_body("").is_ok());
        assert!(check_body(&"x".repeat(MAX_BODY)).is_ok());
        let e = check_body(&"x".repeat(MAX_BODY + 1)).unwrap_err();
        assert!(e.contains("too long"), "{e}");
        // A refusal, not a truncation: nothing here returns a shortened
        // string, which is the whole of I13.
        assert!(e.contains(&MAX_BODY.to_string()));
    }

    #[test]
    fn a_comment_may_not_be_empty() {
        assert!(check_comment("looks broken to me").is_ok());
        assert!(check_comment("  \n ").unwrap_err().contains("empty"));
        assert!(check_comment(&"x".repeat(MAX_BODY)).is_ok());
        assert!(check_comment(&"x".repeat(MAX_BODY + 1))
            .unwrap_err()
            .contains("too long"));
    }

    #[test]
    fn a_label_name_is_one_line_of_visible_text() {
        assert!(check_label_name("good first issue").is_ok());
        assert!(check_label_name(" bug ").is_ok());
        assert!(check_label_name("   ").unwrap_err().contains("required"));
        assert!(check_label_name(&"x".repeat(MAX_LABEL_NAME)).is_ok());
        assert!(check_label_name(&"x".repeat(MAX_LABEL_NAME + 1))
            .unwrap_err()
            .contains("too long"));
        // A newline breaks the pill; a NUL is text PostgreSQL refuses
        // outright, which would arrive as a 500 rather than a sentence.
        assert!(check_label_name("bug\nnot a bug")
            .unwrap_err()
            .contains("control characters"));
        assert!(check_label_name("bug\0").unwrap_err().contains("control"));
    }

    #[test]
    fn a_label_description_is_bounded() {
        assert!(check_label_description("").is_ok());
        assert!(check_label_description(&"x".repeat(MAX_LABEL_DESCRIPTION)).is_ok());
        assert!(
            check_label_description(&"x".repeat(MAX_LABEL_DESCRIPTION + 1))
                .unwrap_err()
                .contains("too long")
        );
    }

    #[test]
    fn a_colour_is_a_token_name_and_never_a_hex() {
        for c in LABEL_COLORS {
            assert!(check_color(c).is_ok(), "{c}");
        }
        // The bug this check exists to prevent: a hex in the database
        // is unfixable later — change the palette and it is silently
        // wrong in both themes, with no migration that can know what the
        // author meant. A token name re-resolves.
        let e = check_color("#ff0000").unwrap_err();
        assert!(check_color("red").is_err());
        assert!(check_color("").is_err());
        assert!(check_color("SERIES-1").is_err(), "token names are exact");
        // A near-miss typo is the case that makes this load-bearing:
        // stored, it resolves to nothing and renders as an unstyled pill
        // that reads as a UI bug rather than a bad value.
        assert!(check_color("brand-strogn").is_err());
        // The refusal has to name the whole valid set. One that does not
        // say what would have worked costs the caller a round trip — and
        // this assertion is what stops somebody later "tidying" the
        // message into something useless.
        for c in LABEL_COLORS {
            assert!(e.contains(c), "the error must list {c}: {e}");
        }
    }

    #[test]
    fn the_colours_are_the_ones_the_design_system_publishes() {
        // Pinned against web/shared/tokens.css. Six are custom
        // properties there; `neutral` is the sentinel for the un-tinted
        // pill, and the design system publishes no `--neutral`. If the
        // palette grows, this test is the thing that says so — and the
        // growth needs a dataviz validator run, not an edit here.
        assert_eq!(
            LABEL_COLORS,
            &[
                "series-1",
                "series-2",
                "series-3",
                "status-good",
                "status-warning",
                "status-serious",
                "neutral",
            ]
        );
    }

    #[test]
    fn a_filter_state_may_be_all_but_an_issue_state_may_not() {
        assert!(check_filter_state("open").is_ok());
        assert!(check_filter_state("closed").is_ok());
        assert!(check_filter_state("all").is_ok());
        assert!(check_filter_state("deleted").is_err());
        assert!(check_state("open").is_ok());
        assert!(check_state("closed").is_ok());
        // "all" is the absence of a filter, not a state a row can be in.
        assert!(check_state("all").unwrap_err().contains("open or closed"));
    }

    #[test]
    fn a_like_query_matches_its_own_punctuation() {
        assert_eq!(like_pattern("Crash"), "%crash%");
        // Without escaping, `_` matches every one-character title and
        // `%` matches everything, which reads as the search being broken.
        assert_eq!(like_pattern("_"), "%\\_%");
        assert_eq!(like_pattern("100%"), "%100\\%%");
        assert_eq!(like_pattern("a\\b"), "%a\\\\b%");
    }

    #[test]
    fn an_unfiltered_listing_asks_only_for_the_repository() {
        let s = shape(&filter(), true, true).unwrap();
        assert_eq!(s.sql, "i.repo_id = $1");
        assert!(s.args.is_empty());
    }

    #[test]
    fn each_filter_binds_the_next_slot_in_order() {
        let f = Filter {
            state: Some("open".into()),
            label: Some("Bug".into()),
            author: Some("Ada".into()),
            q: Some("crash".into()),
            before: Some(40),
            ..filter()
        };
        let s = shape(&f, true, true).unwrap();
        assert!(
            s.sql.starts_with("i.repo_id = $1 AND i.state = $2"),
            "{}",
            s.sql
        );
        assert!(s.sql.contains("lower(l.name) = $3"), "{}", s.sql);
        assert!(s.sql.contains("lower(u.handle) = $4"), "{}", s.sql);
        assert!(s.sql.contains("lower(i.title) LIKE $5"), "{}", s.sql);
        assert!(s.sql.contains("lower(i.body) LIKE $5"), "{}", s.sql);
        assert!(s.sql.ends_with("AND i.number < $6"), "{}", s.sql);
        // Names are matched case-insensitively, so the bound value is
        // folded here rather than in the SQL — an index on lower(name)
        // can serve this, one on `name` cannot.
        assert_eq!(
            s.args,
            vec![
                Arg::Text("open".into()),
                Arg::Text("bug".into()),
                Arg::Text("ada".into()),
                Arg::Text("%crash%".into()),
                Arg::Int(40),
            ]
        );
    }

    #[test]
    fn the_counts_query_drops_the_state_filter_and_keeps_the_rest() {
        // The whole reason `shape` takes flags: "12 Open / 40 Closed" is
        // a pair, and a page filtered to `open` that computed its closed
        // count from the rows it fetched would always say 0.
        let f = Filter {
            state: Some("open".into()),
            label: Some("bug".into()),
            before: Some(40),
            ..filter()
        };
        let counts = shape(&f, false, false).unwrap();
        assert!(!counts.sql.contains("i.state"), "{}", counts.sql);
        assert!(!counts.sql.contains("i.number <"), "{}", counts.sql);
        assert!(counts.sql.contains("lower(l.name) = $2"), "{}", counts.sql);
        assert_eq!(counts.args, vec![Arg::Text("bug".into())]);
    }

    #[test]
    fn state_all_filters_nothing_but_is_still_a_valid_answer() {
        let f = Filter {
            state: Some("all".into()),
            ..filter()
        };
        let s = shape(&f, true, true).unwrap();
        assert_eq!(s.sql, "i.repo_id = $1");
        assert!(s.args.is_empty());
    }

    #[test]
    fn a_shaped_filter_refuses_hostile_input_rather_than_binding_it() {
        let bad_state = Filter {
            state: Some("'; DROP TABLE issues; --".into()),
            ..filter()
        };
        assert!(shape(&bad_state, true, true)
            .unwrap_err()
            .contains("invalid state"));

        let bad_label = Filter {
            label: Some("x".repeat(MAX_LABEL_NAME + 1)),
            ..filter()
        };
        assert!(shape(&bad_label, true, true)
            .unwrap_err()
            .contains("too long"));

        // A NUL is text PostgreSQL refuses outright: matching nothing is
        // the honest answer, and a 500 quoting the database layer is not.
        let nul = Filter {
            q: Some("cra\0sh".into()),
            ..filter()
        };
        assert!(shape(&nul, true, true).unwrap_err().contains("NUL"));

        let long_q = Filter {
            q: Some("x".repeat(MAX_TITLE + 1)),
            ..filter()
        };
        assert!(shape(&long_q, true, true).unwrap_err().contains("too long"));
    }

    #[test]
    fn the_state_filter_is_validated_even_when_it_is_not_applied() {
        // The counts query drops the filter, but dropping it must not
        // drop the check: `list` shapes both, and a caller sending
        // nonsense should hear about it either way.
        let f = Filter {
            state: Some("nonsense".into()),
            ..filter()
        };
        assert!(shape(&f, false, false).is_err());
    }

    #[test]
    fn a_page_is_clamped_rather_than_refused() {
        assert_eq!(page_limit(25), 25);
        assert_eq!(page_limit(MAX_LIMIT), MAX_LIMIT);
        assert_eq!(page_limit(1_000_000), MAX_LIMIT);
        // 0 and negatives come from a query string, not from a person:
        // the floor keeps them from being an error page.
        assert_eq!(page_limit(0), 1);
        assert_eq!(page_limit(-5), 1);
    }

    /// The read paths, against a real database.
    ///
    /// These exist because the coverage gate asked for them and it was
    /// right to. `issues_e2e` drives all of this through HTTP, but the
    /// server runs as a **separate process**, so none of it counts
    /// toward this crate — every sibling module here unit-tests its
    /// database functions directly, and this one had only pure ones.
    ///
    /// That is not merely a coverage technicality. The e2e asserted
    /// `comment_count` and never once read the conversation back, so
    /// `comments()` and `row_to_comment` had no test at all in either
    /// place. The gate found an untested product path by counting
    /// lines, which is exactly what it is for.
    #[test]
    fn a_conversation_reads_back_in_the_order_it_was_spoken() {
        let db = db("issues_conversation");
        let (ada, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        let i = open(&db, &r.id, &ada, "a bug", "it breaks").unwrap();

        assert!(comments(&db, &i.id).unwrap().is_empty());

        for body in ["first", "second", "third"] {
            comment(&db, &i.id, &ada, body).unwrap();
        }
        let got = comments(&db, &i.id).unwrap();
        let bodies: Vec<&str> = got.iter().map(|c| c.body.as_str()).collect();
        // `seq`, not the id. ULID tails are random within a
        // millisecond, so three comments written this fast are exactly
        // the case an id sort gets wrong.
        assert_eq!(bodies, vec!["first", "second", "third"]);
        assert!(got.windows(2).all(|w| w[0].seq < w[1].seq));
        // The author is resolved to a handle, and it is the renderable
        // one rather than the id.
        assert_eq!(got[0].author.as_deref(), Some("ada"));
        assert_eq!(got[0].author_label, None);
    }

    /// An id that could not name a row never reaches the query.
    ///
    /// Not a defensive nicety: `issue_id` arrives from a URL, and the
    /// empty answer is the same one a real-but-absent id gets, so a
    /// caller cannot tell a malformed id from an unused one.
    #[test]
    fn a_malformed_issue_id_reads_as_an_empty_conversation() {
        let db = db("issues_badid");
        assert!(comments(&db, "'; DROP TABLE issues; --")
            .unwrap()
            .is_empty());
        assert!(comments(&db, "").unwrap().is_empty());
    }

    #[test]
    fn an_issue_carries_its_labels_and_its_comment_count() {
        let db = db("issues_hydrate");
        let (ada, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        create_label(&db, &r.id, "bug", "status-serious", "").unwrap();
        create_label(&db, &r.id, "Docs", "series-3", "").unwrap();
        let i = open(&db, &r.id, &ada, "a bug", "").unwrap();
        comment(&db, &i.id, &ada, "me too").unwrap();
        set_labels(&db, &r.id, i.number, &["bug".into(), "Docs".into()]).unwrap();

        let got = get(&db, &r.id, i.number).unwrap().expect("the issue");
        assert_eq!(got.comment_count, 1);
        // Alphabetical, case-insensitively — otherwise a capitalised
        // label sorts into its own block ahead of everything.
        let names: Vec<&str> = got.labels.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["bug", "Docs"]);
        assert_eq!(got.labels[0].color, "status-serious");

        // A number nobody used is absent rather than an error.
        assert!(get(&db, &r.id, 9999).unwrap().is_none());
    }

    #[test]
    fn the_counts_are_both_returned_whatever_the_filter_asks_for() {
        let db = db("issues_counts");
        let (ada, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        for t in ["one", "two", "three"] {
            open(&db, &r.id, &ada, t, "").unwrap();
        }
        set_state(&db, &r.id, 1, "closed", 1_700_000_000_000).unwrap();

        // Filtered to open, and still told how many are closed. The
        // index renders "2 Open / 1 Closed" as a pair, and computing
        // either from the page above it is how that number goes wrong.
        let mut f = filter();
        f.state = Some("open".into());
        let (rows, counts) = list(&db, &r.id, &f).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!((counts.open, counts.closed), (2, 1));

        f.state = Some("closed".into());
        let (rows, counts) = list(&db, &r.id, &f).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((counts.open, counts.closed), (2, 1));

        f.state = Some("all".into());
        let (rows, counts) = list(&db, &r.id, &f).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!((counts.open, counts.closed), (2, 1));
    }

    #[test]
    fn the_filters_narrow_and_the_cursor_pages() {
        let db = db("issues_filters");
        let (ada, org) = person(&db, "ada", "ada@example.com");
        let (bob, _) = person(&db, "bob", "bob@example.com");
        let r = repo(&db, &org, "widget");
        create_label(&db, &r.id, "bug", "status-serious", "").unwrap();
        let a = open(&db, &r.id, &ada, "ada finds a crash", "").unwrap();
        open(&db, &r.id, &bob, "bob writes docs", "").unwrap();
        set_labels(&db, &r.id, a.number, &["bug".into()]).unwrap();

        let mut f = filter();
        f.author = Some("ada".into());
        assert_eq!(list(&db, &r.id, &f).unwrap().0.len(), 1);
        // The handle match is case-insensitive: a handle is an identity,
        // not a string, and "Ada" is the same person.
        f.author = Some("ADA".into());
        assert_eq!(list(&db, &r.id, &f).unwrap().0.len(), 1);

        let mut f = filter();
        f.label = Some("bug".into());
        let (rows, _) = list(&db, &r.id, &f).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].number, a.number);

        let mut f = filter();
        f.q = Some("docs".into());
        assert_eq!(list(&db, &r.id, &f).unwrap().0[0].title, "bob writes docs");

        // The cursor is a number and the page is `number < before`.
        let mut f = filter();
        f.before = Some(2);
        let (rows, _) = list(&db, &r.id, &f).unwrap();
        assert_eq!(rows.iter().map(|i| i.number).collect::<Vec<_>>(), vec![1]);

        // A repository with nothing in it is an empty page, not an
        // error — `hydrate` returns early rather than querying for the
        // labels of no rows.
        let empty = repo(&db, &org, "quiet");
        let (rows, counts) = list(&db, &empty.id, &filter()).unwrap();
        assert!(rows.is_empty());
        assert_eq!((counts.open, counts.closed), (0, 0));
    }

    #[test]
    fn editing_and_closing_change_only_what_they_name() {
        let db = db("issues_edit");
        let (ada, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        let i = open(&db, &r.id, &ada, "old title", "old body").unwrap();
        assert_eq!(i.closed_at, None);

        let e = edit(&db, &r.id, i.number, Some("new title"), None).unwrap();
        assert_eq!(e.title, "new title");
        assert_eq!(e.body, "old body", "editing the title rewrote the body");

        let closed = set_state(&db, &r.id, i.number, "closed", 1_700_000_000_000).unwrap();
        assert_eq!(closed.state, "closed");
        assert_eq!(closed.closed_at, Some(1_700_000_000_000));

        // Reopening clears the timestamp rather than leaving a closed_at
        // on an open issue — the CASE in `set_state` is the whole reason
        // that column needed an explicit cast.
        let open_again = set_state(&db, &r.id, i.number, "open", 1_700_000_001_000).unwrap();
        assert_eq!(open_again.state, "open");
        assert_eq!(open_again.closed_at, None);

        assert!(set_state(&db, &r.id, 9999, "closed", 1).is_err());
        assert!(edit(&db, &r.id, 9999, Some("x"), None).is_err());
    }

    #[test]
    fn labels_are_created_listed_replaced_and_deleted() {
        let db = db("issues_labels_crud");
        let (ada, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        create_label(&db, &r.id, "bug", "status-serious", "broken").unwrap();
        create_label(&db, &r.id, "chore", "neutral", "").unwrap();
        let all = labels(&db, &r.id).unwrap();
        assert_eq!(
            all.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
            vec!["bug", "chore"]
        );

        // A name already in use is refused rather than silently
        // producing a second label nobody can tell apart.
        assert!(create_label(&db, &r.id, "bug", "series-1", "").is_err());

        let i = open(&db, &r.id, &ada, "a bug", "").unwrap();
        set_labels(&db, &r.id, i.number, &["bug".into(), "chore".into()]).unwrap();
        // Replace, not add: the API is PUT.
        let one = set_labels(&db, &r.id, i.number, &["chore".into()]).unwrap();
        assert_eq!(
            one.labels
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>(),
            vec!["chore"]
        );
        // And an empty list clears them.
        let none = set_labels(&db, &r.id, i.number, &[]).unwrap();
        assert!(none.labels.is_empty());

        // A label that does not exist is refused, so a typo does not
        // quietly clear the labels it failed to apply.
        assert!(set_labels(&db, &r.id, i.number, &["nope".into()]).is_err());

        set_labels(&db, &r.id, i.number, &["bug".into()]).unwrap();
        delete_label(&db, &r.id, "bug").unwrap();
        // Deleting takes it off every issue carrying it, by the cascade.
        assert!(get(&db, &r.id, i.number)
            .unwrap()
            .unwrap()
            .labels
            .is_empty());
        assert!(delete_label(&db, &r.id, "bug").is_err());
    }

    /// The refusals on the write paths, which the e2e reaches only for
    /// the ones the HTTP layer happens to exercise.
    #[test]
    fn the_write_paths_refuse_what_they_promise_to_refuse() {
        let db = db("issues_write_refusals");
        let (ada, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        let i = open(&db, &r.id, &ada, "a bug", "").unwrap();
        create_label(&db, &r.id, "bug", "series-1", "").unwrap();

        // Editing the body alone still bounds it. The title arm and the
        // body arm are independent, and only the title arm had a test —
        // so an over-long body could be edited in where it could not be
        // filed in.
        let long = "x".repeat(MAX_BODY + 1);
        assert!(edit(&db, &r.id, i.number, None, Some(&long)).is_err());
        assert!(edit(&db, &r.id, i.number, None, Some("a new body")).is_ok());
        assert_eq!(
            get(&db, &r.id, i.number).unwrap().unwrap().body,
            "a new body"
        );

        // A cap on how many labels one issue may carry (I13). Bounded
        // input is bounded on every door, not just the ones a form
        // uses.
        let many: Vec<String> = (0..=MAX_LABELS).map(|n| format!("l{n}")).collect();
        let e = set_labels(&db, &r.id, i.number, &many).unwrap_err();
        assert!(e.contains("too many labels"), "{e}");

        // A comment on an id that could not name a row is refused
        // rather than orphaned — and an id that *could* be a row but is
        // not gets the same answer, so neither tells the caller which.
        assert!(comment(&db, "not-an-id", &ada, "hello").is_err());
        assert!(comment(&db, &ulid(), &ada, "hello").is_err());
        assert!(comment(&db, &i.id, &ada, "   ").is_err());

        // Labelling an issue that does not exist.
        assert!(set_labels(&db, &r.id, 9999, &["bug".into()]).is_err());
    }
}
