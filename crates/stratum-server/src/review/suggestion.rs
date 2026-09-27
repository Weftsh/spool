//! Suggested changes: the fenced *suggestion* block a reviewer writes
//! inside a comment, and the patchset that applying a set of them makes.
//!
//! **There is no `suggestion` column, and there must not be one.** A
//! suggestion is a fenced block inside `change_comments.body` — that is
//! how the reviewer typed it, that is what every importer and every
//! mirror already carries, and that is what the conversation renders. A
//! column beside it would be a second, ungoverned way to say the same
//! thing, and the two would disagree the first time somebody edited one
//! of them. So it is parsed where it is read.
//!
//! Layering, the same discipline as the rest of `review`:
//! - [`parse`] and [`apply`] are pure. Fences, CRLF, indentation, line
//!   terminators and the delete-these-lines case are all decided here,
//!   with no store and no database, and every one of them is unit-tested
//!   below.
//! - [`make`] does the store half: read the file at the patchset, refuse
//!   an anchor the patchset no longer has, and build the commit. It is
//!   given anchors and replacement lines and knows nothing about
//!   comments, authorization or HTTP.
//!
//! The one thing this module deliberately does *not* do is write. The
//! commit it makes goes through `changes_api::register_patchset` — the
//! same door a push goes through — because a second way to make a
//! patchset is how the two disagree later about pinning, CI, or who gets
//! told.

use crate::api::commits::{self, Leaf};
use stratum_engine::objwrite::{
    self, encode_commit, hex, new_object, CommitInfo, NewObject, OBJ_BLOB, OBJ_COMMIT,
};
use stratum_engine::read::LayoutReader;
use stratum_engine::refops::{NewPack, TxnError};
use stratum_store::{Manifest, ObjectStore};

/// One replacement, as written: the lines that should stand in place of
/// the ones the comment is anchored to.
///
/// An **empty** `lines` is not the absence of a suggestion — it is the
/// suggestion to delete those lines, which is a thing reviewers ask for
/// constantly. [`parse`] answers an empty `Vec<Suggestion>` for a
/// comment with no block at all, so the two are distinguishable, and the
/// apply route says two different things about them.
#[derive(Debug, PartialEq, Eq)]
pub struct Suggestion {
    /// Replacement lines, without terminators. The file's own line
    /// endings are re-applied by [`apply`], so a suggestion typed in a
    /// CRLF comment does not turn a LF file into a mixed one.
    pub lines: Vec<String>,
}

/// An open fence, as CommonMark reads one.
struct Fence {
    indent: usize,
    ticks: usize,
    suggestion: bool,
}

/// A line with the trailing `\r` of a CRLF comment removed.
///
/// The body is stored exactly as the client sent it, and a browser that
/// posts a textarea sends CRLF. Matching a fence with `\r` still on it
/// finds nothing, so a suggestion written in a browser would silently
/// not be one — the reviewer sees their block rendered and the author
/// sees no button.
fn unterminated(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

fn open_fence(line: &str) -> Option<Fence> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let ticks = rest.chars().take_while(|c| *c == '`').count();
    if ticks < 3 {
        return None;
    }
    Some(Fence {
        indent,
        ticks,
        // Exactly `suggestion`, and nothing that merely starts with it.
        // GitHub also understands `suggestion:-0+2`, which moves the
        // anchor; we do not implement that, and reading one as a plain
        // suggestion would apply the reviewer's text to lines they were
        // not talking about. An info string we do not know is not a
        // suggestion, and the apply route says so by name.
        suggestion: rest[ticks..].trim() == "suggestion",
    })
}

/// Does this line close a fence opened with `ticks` backticks?
///
/// At least as many backticks and nothing else, which is what makes a
/// four-backtick block able to contain a three-backtick one — the way a
/// reviewer quotes a fence inside their suggestion.
fn closes(line: &str, ticks: usize) -> bool {
    let t = line.trim();
    t.len() >= ticks && !t.is_empty() && t.chars().all(|c| c == '`')
}

/// Every suggestion block in a comment body, in the order written.
///
/// Non-suggestion fences are walked over rather than ignored line by
/// line: a reviewer quoting somebody else's diff inside a fenced `rust`
/// block may well have a fenced suggestion *inside* it, and applying
/// that would apply text the reviewer was quoting, not proposing.
///
/// An **unterminated** fence yields nothing, and swallows the rest of
/// the body, because that is what it is: everything after it is inside a
/// block that never closed. The apply route then refuses the comment for
/// carrying no suggestion, which is the honest answer — silently
/// applying to the end of the comment would commit the reviewer's prose.
pub fn parse(body: &str) -> Vec<Suggestion> {
    let raw: Vec<&str> = body.split('\n').map(unterminated).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let Some(fence) = open_fence(raw[i]) else {
            i += 1;
            continue;
        };
        let mut lines = Vec::new();
        let mut j = i + 1;
        while j < raw.len() && !closes(raw[j], fence.ticks) {
            // CommonMark strips the opening fence's indentation from the
            // content, so a suggestion inside a list item is the text
            // the reviewer sees rather than that text with four spaces
            // welded onto every line.
            let line = raw[j];
            let strip = line.len() - line.trim_start_matches(' ').len();
            lines.push(line[strip.min(fence.indent)..].to_string());
            j += 1;
        }
        if j >= raw.len() {
            break;
        }
        if fence.suggestion {
            out.push(Suggestion { lines });
        }
        i = j + 1;
    }
    out
}

/// One line of a file, with the terminator it was written with.
struct Line<'a> {
    text: &'a str,
    term: &'a str,
}

/// Split a file into lines, keeping each line's terminator.
///
/// Keeping the terminator rather than normalizing is the whole point: a
/// CRLF file edited by a suggestion must stay a CRLF file, and a file
/// with no trailing newline must not silently grow one. Both of those
/// show up in a diff as every line changing.
fn lines_of(text: &str) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let (line, tail) = match rest.find('\n') {
            Some(n) => (&rest[..=n], &rest[n + 1..]),
            None => (rest, ""),
        };
        let body = line.strip_suffix('\n').unwrap_or(line);
        let stripped = body.strip_suffix('\r').unwrap_or(body);
        out.push(Line {
            text: stripped,
            term: &line[stripped.len()..],
        });
        rest = tail;
    }
    out
}

/// A replacement of a 1-based inclusive line range.
#[derive(Clone, Debug)]
pub struct Edit {
    pub start: i64,
    pub end: i64,
    pub lines: Vec<String>,
}

/// How many lines a file has, for the refusal that names the anchor.
pub fn line_count(text: &str) -> i64 {
    lines_of(text).len() as i64
}

/// Do these lines already read exactly as suggested?
///
/// Applying a suggestion that changes nothing would mint a patchset
/// identical to the one before it, and the author would be left looking
/// at a revision with an empty diff wondering what they had done.
pub fn is_noop(text: &str, edit: &Edit) -> bool {
    let lines = lines_of(text);
    let range = &lines[(edit.start as usize - 1)..(edit.end as usize)];
    range.len() == edit.lines.len() && range.iter().zip(&edit.lines).all(|(l, s)| l.text == s)
}

/// Replace each edit's lines. `Err` names what was wrong with a range.
///
/// Applied **bottom-up** so that an earlier edit's replacement does not
/// move the lines a later one is anchored to. The callers refuse
/// overlapping anchors before they get here; the guard below stays
/// anyway, because a silent misapplication would commit the reviewer's
/// text over somebody else's lines and nothing downstream would notice.
pub fn apply(text: &str, edits: &[Edit]) -> Result<String, String> {
    let lines = lines_of(text);
    let mut sorted: Vec<&Edit> = edits.iter().collect();
    sorted.sort_by_key(|e| e.start);
    for w in sorted.windows(2) {
        if w[0].end >= w[1].start {
            return Err(format!(
                "lines {}-{} and {}-{} overlap",
                w[0].start, w[0].end, w[1].start, w[1].end
            ));
        }
    }
    for e in &sorted {
        if e.start < 1 || e.end < e.start || e.end > lines.len() as i64 {
            return Err(format!(
                "lines {}-{} are not lines 1-{} of this file",
                e.start,
                e.end,
                lines.len()
            ));
        }
    }
    // The terminator every replacement line but the last one gets. The
    // last one inherits the terminator of the last line it replaces, so
    // a suggestion on the final line of a file with no trailing newline
    // leaves it with none.
    let default_term = lines
        .iter()
        .map(|l| l.term)
        .find(|t| !t.is_empty())
        .unwrap_or("\n");
    let mut out: Vec<String> = lines
        .iter()
        .map(|l| format!("{}{}", l.text, l.term))
        .collect();
    for e in sorted.iter().rev() {
        let start = e.start as usize - 1;
        let end = e.end as usize;
        let tail_term = lines[end - 1].term;
        let mut replacement: Vec<String> = Vec::with_capacity(e.lines.len());
        for (i, line) in e.lines.iter().enumerate() {
            let term = if i + 1 == e.lines.len() {
                tail_term
            } else {
                default_term
            };
            replacement.push(format!("{line}{term}"));
        }
        out.splice(start..end, replacement);
    }
    Ok(out.concat())
}

/// A ref transaction's failure as one sentence.
///
/// A free function rather than a `match` at the call site so that both
/// halves are reachable from a unit test: the conflict arm of a pin
/// written with `Expect::Any` cannot be provoked through the API, and an
/// arm nothing can execute is an arm nobody has read.
pub fn txn_message(e: TxnError) -> String {
    match e {
        TxnError::Conflict(name, _) => format!("{name} moved while the patchset was being written"),
        TxnError::Other(e) => e,
    }
}

/// One comment's anchor, for the overlap check.
pub struct Anchor {
    pub comment: String,
    pub path: String,
    pub start: i64,
    pub end: i64,
}

/// The first pair of anchors that fight over the same lines, in a
/// deterministic order.
///
/// Refused rather than resolved. Two suggestions over one line are two
/// reviewers disagreeing, and picking either one silently would commit a
/// hybrid neither of them proposed — the author would have to read the
/// patchset to find out which remark had been honoured. Sorting first is
/// what makes the sentence the same whichever order the client sent the
/// ids in.
pub fn first_overlap(anchors: &[Anchor]) -> Option<(&Anchor, &Anchor)> {
    let mut sorted: Vec<&Anchor> = anchors.iter().collect();
    sorted.sort_by(|a, b| {
        (&a.path, a.start, a.end, &a.comment).cmp(&(&b.path, b.start, b.end, &b.comment))
    });
    sorted
        .windows(2)
        .find(|w| w[0].path == w[1].path && w[0].end >= w[1].start)
        .map(|w| (w[0], w[1]))
}

/// Why a suggestion cannot be applied to the patchset as it stands.
///
/// Every one of these is about the *anchor*, not about the reviewer: the
/// comment may be from patchset 1 and the file has moved on since. The
/// sentence names the file and the lines, because "cannot apply" with no
/// noun in it sends the author to read four patchsets to find out which
/// remark it meant.
pub enum Stale {
    Gone {
        path: String,
        patchset: i64,
    },
    Moved {
        path: String,
        from: i64,
        patchset: i64,
    },
    PastEnd {
        path: String,
        start: i64,
        end: i64,
        have: i64,
    },
    NotText {
        path: String,
    },
    NoOp {
        path: String,
        start: i64,
        end: i64,
    },
}

impl Stale {
    pub fn sentence(&self) -> String {
        match self {
            Stale::Gone { path, patchset } => {
                format!("patchset {patchset} no longer has {path}")
            }
            Stale::Moved {
                path,
                from,
                patchset,
            } => format!(
                "{path} has changed since patchset {from}, so the lines this \
                 comment anchors are not the lines patchset {patchset} has — \
                 re-read the file and say it again"
            ),
            Stale::PastEnd {
                path,
                start,
                end,
                have,
            } => format!("it anchors lines {start}-{end} of {path}, which has {have} lines"),
            Stale::NotText { path } => {
                format!("{path} is not a text file, so it has no lines to replace")
            }
            Stale::NoOp { path, start, end } => {
                format!("lines {start}-{end} of {path} already read exactly as suggested")
            }
        }
    }
}

/// One anchored replacement, resolved from a comment.
pub struct Application {
    /// The comment it came from, for the sentence a refusal carries.
    pub comment: String,
    pub path: String,
    /// The patchset the comment's line numbers were counted against.
    pub patchset: i64,
    pub edit: Edit,
}

/// The commit applying these suggestions makes, and the objects it needs.
pub struct Made {
    pub parent: String,
    pub commit: String,
    pub pack: NewPack,
    /// Every path the new patchset rewrites, for the audit record.
    pub paths: Vec<String>,
}

/// Make the patchset that applies `apps` on top of `parent`. Reads only;
/// the outer `Err` is the store failing, the inner one is a refusal in
/// the author's words.
///
/// `at` maps a patchset number to the commit it was recorded at. It is
/// how the staleness check is honest rather than optimistic: a comment
/// written on patchset 1 names line 12 *of patchset 1*, and if the file
/// is not byte-identical at the latest patchset then line 12 is a
/// different line and applying the suggestion there would commit the
/// reviewer's text over somebody else's code.
#[allow(clippy::too_many_arguments)]
pub fn make(
    store: &ObjectStore,
    prefix: &str,
    manifest: &Manifest,
    parent: &str,
    latest_number: i64,
    at: &dyn Fn(i64) -> String,
    apps: &[Application],
    author: &str,
    message: &str,
) -> Result<Result<Made, (String, Stale)>, String> {
    let reader = LayoutReader::new(store, prefix, manifest)?;
    // No "is this a commit" guard: `parse_commit` refuses anything that
    // is not one, and a second check would be an arm nothing can reach —
    // a patchset row's oid came from a commit this server parsed.
    let (_, data) = reader.object(parent)?;
    let parent_tree = objwrite::parse_commit(&data)?.tree;

    // Group by path: several suggestions in one file are one blob, and
    // making one blob per suggestion would have the second overwrite the
    // first — the bug that makes "apply all" mean "apply the last one".
    let mut paths: Vec<String> = apps.iter().map(|a| a.path.clone()).collect();
    paths.sort();
    paths.dedup();

    let mut objects: Vec<NewObject> = Vec::new();
    let mut changes: Vec<(Vec<String>, Option<Leaf>)> = Vec::new();
    for path in &paths {
        let mine: Vec<&Application> = apps.iter().filter(|a| &a.path == path).collect();
        let entry = match reader.entry_at(parent, path)? {
            Some(e) => e,
            None => {
                return Ok(Err((
                    mine[0].comment.clone(),
                    Stale::Gone {
                        path: path.clone(),
                        patchset: latest_number,
                    },
                )))
            }
        };
        // The file as the reviewer read it. Byte-identical or the line
        // numbers mean something else; `entry_at` at the comment's own
        // patchset is the cheapest way to ask, because it compares oids
        // rather than content.
        for a in &mine {
            let then = reader.entry_at(&at(a.patchset), path)?;
            if then.map(|t| t.oid) != Some(entry.oid) {
                return Ok(Err((
                    a.comment.clone(),
                    Stale::Moved {
                        path: path.clone(),
                        from: a.patchset,
                        patchset: latest_number,
                    },
                )));
            }
        }
        let blob = hex(&entry.oid);
        let (kind, bytes) = reader.object(&blob)?;
        // A tree, a symlink, a gitlink and a file of bytes are the same
        // answer to a reviewer who suggested replacing lines 3 to 5, so
        // they share a refusal.
        let text = match (kind == OBJ_BLOB).then(|| String::from_utf8(bytes).ok()) {
            Some(Some(t)) => t,
            _ => {
                return Ok(Err((
                    mine[0].comment.clone(),
                    Stale::NotText { path: path.clone() },
                )))
            }
        };
        let have = line_count(&text);
        for a in &mine {
            if a.edit.start < 1 || a.edit.end < a.edit.start || a.edit.end > have {
                return Ok(Err((
                    a.comment.clone(),
                    Stale::PastEnd {
                        path: path.clone(),
                        start: a.edit.start,
                        end: a.edit.end,
                        have,
                    },
                )));
            }
            if is_noop(&text, &a.edit) {
                return Ok(Err((
                    a.comment.clone(),
                    Stale::NoOp {
                        path: path.clone(),
                        start: a.edit.start,
                        end: a.edit.end,
                    },
                )));
            }
        }
        let edits: Vec<Edit> = mine.iter().map(|a| a.edit.clone()).collect();
        let next = apply(&text, &edits)?;
        let obj = new_object(OBJ_BLOB, next.into_bytes());
        let leaf = Leaf {
            oid: obj.oid,
            mode: entry.mode.clone(),
        };
        objects.push(obj);
        // Safe to split structurally: `add_comment` refuses any path
        // `review::owners::valid_repo_path` rejects, so there is no
        // empty, `.` or `..` segment here — and this path just resolved
        // to a blob in the patchset's own tree, which is stronger still.
        changes.push((path.split('/').map(str::to_string).collect(), Some(leaf)));
    }

    let tree = commits::build_tree(&reader, Some(&parent_tree), &changes, &mut objects)?;
    let commit = new_object(
        OBJ_COMMIT,
        encode_commit(&CommitInfo {
            tree,
            parents: vec![objwrite::parse_hex(parent)?],
            author: author.to_string(),
            committer: author.to_string(),
            timestamp: commits::stratum_control_free_now(),
            message: message.to_string(),
        }),
    );
    let commit_hex = hex(&commit.oid);
    objects.push(commit);
    // A tree the layout already holds must not enter the WAL twice (I5).
    let to_pack = commits::absent_from_layout(store, prefix, manifest, objects)?;
    let (payload, entries) = objwrite::build_pack_payload(&to_pack)?;
    let mut oids: Vec<[u8; 20]> = to_pack.iter().map(|o| o.oid).collect();
    oids.sort();
    Ok(Ok(Made {
        parent: parent.to_string(),
        commit: commit_hex,
        pack: NewPack {
            payload,
            oids,
            entries,
        },
        paths,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(lines: &[&str]) -> Suggestion {
        Suggestion {
            lines: lines.iter().map(|l| l.to_string()).collect(),
        }
    }

    /// The ordinary shapes, and the one that decides the whole feature:
    /// a block with nothing in it is *delete these lines*, and it must be
    /// distinguishable from a comment carrying no block at all.
    #[test]
    fn a_block_is_found_and_an_empty_one_is_a_deletion() {
        assert_eq!(parse("just a remark, no code at all"), vec![]);
        assert_eq!(parse(""), vec![]);
        assert_eq!(
            parse("try this:\n```suggestion\nlet x = 1;\n```\nthanks"),
            vec![s(&["let x = 1;"])]
        );
        // Empty: one block, no lines. `vec![]` above is "no suggestion",
        // and these two must never collapse into each other.
        assert_eq!(
            parse("drop it:\n```suggestion\n```\n"),
            vec![Suggestion { lines: vec![] }]
        );
        // A blank line inside is a suggestion of one empty line, which is
        // not the same as a deletion.
        assert_eq!(parse("```suggestion\n\n```"), vec![s(&[""])]);
    }

    #[test]
    fn several_blocks_in_one_comment_come_back_in_order() {
        let body = "first:\n```suggestion\na\n```\nand also:\n```suggestion\nb\nc\n```";
        assert_eq!(parse(body), vec![s(&["a"]), s(&["b", "c"])]);
    }

    /// A fence that never closes is not a suggestion, and it swallows
    /// the rest of the comment: everything after it is inside a block
    /// that never ended. Applying to the end of the body would commit
    /// the reviewer's prose into the file.
    #[test]
    fn an_unterminated_fence_yields_nothing() {
        assert_eq!(parse("```suggestion\nlet x = 1;\n"), vec![]);
        assert_eq!(
            parse("```suggestion\na\n```\nthen:\n```suggestion\nb\n"),
            vec![s(&["a"])]
        );
    }

    /// Any other info string is somebody's code sample, and a block
    /// nested inside one is quoted, not proposed.
    #[test]
    fn a_language_tag_is_not_a_suggestion_and_a_quoted_one_is_walked_over() {
        assert_eq!(parse("```rust\nlet x = 1;\n```"), vec![]);
        assert_eq!(parse("```\nplain\n```"), vec![]);
        assert_eq!(parse("```suggestion rust\nx\n```"), vec![]);
        // GitHub's anchor-moving form. We do not implement it, and
        // reading it as a plain suggestion would apply the text to lines
        // the reviewer was not talking about.
        assert_eq!(parse("```suggestion:-0+2\nx\n```"), vec![]);
        assert_eq!(
            parse("here is what not to do:\n```rust\n```suggestion\nbad\n```\n"),
            vec![],
            "a suggestion quoted inside another fence is not proposed"
        );
    }

    /// Four backticks outside, three inside: how a reviewer suggests a
    /// line that is itself a fence.
    #[test]
    fn a_longer_fence_carries_a_shorter_one_inside_it() {
        assert_eq!(
            parse("````suggestion\n```\ncode\n```\n````"),
            vec![s(&["```", "code", "```"])]
        );
    }

    /// A browser posts a textarea as CRLF. Matching the fence with the
    /// `\r` still on it finds nothing, so the reviewer sees their block
    /// rendered and the author sees no button.
    #[test]
    fn crlf_bodies_parse_and_the_replacement_carries_no_carriage_returns() {
        assert_eq!(
            parse("do this:\r\n```suggestion\r\nlet x = 1;\r\n```\r\n"),
            vec![s(&["let x = 1;"])]
        );
    }

    /// CommonMark strips the opening fence's indentation, so a
    /// suggestion inside a list item is the text the reviewer sees and
    /// not that text with two spaces welded onto every line.
    #[test]
    fn an_indented_fence_loses_its_indentation_and_keeps_the_rest() {
        assert_eq!(
            parse("- like so:\n  ```suggestion\n  let x = 1;\n      deep\n  ```"),
            vec![s(&["let x = 1;", "    deep"])]
        );
    }

    #[test]
    fn lines_keep_their_own_terminators() {
        assert_eq!(line_count("a\nb\n"), 2);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("\n"), 1);
    }

    fn edit(start: i64, end: i64, lines: &[&str]) -> Edit {
        Edit {
            start,
            end,
            lines: lines.iter().map(|l| l.to_string()).collect(),
        }
    }

    #[test]
    fn a_replacement_keeps_the_files_own_line_endings() {
        assert_eq!(
            apply("a\nb\nc\n", &[edit(2, 2, &["B"])]).unwrap(),
            "a\nB\nc\n"
        );
        assert_eq!(
            apply("a\r\nb\r\nc\r\n", &[edit(2, 2, &["B"])]).unwrap(),
            "a\r\nB\r\nc\r\n",
            "a CRLF file must not come back mixed"
        );
        // No trailing newline, and the edit is the last line: the file
        // must not silently grow one, which reads as every tool that
        // wrote it being wrong.
        assert_eq!(apply("a\nb", &[edit(2, 2, &["B"])]).unwrap(), "a\nB");
        assert_eq!(
            apply("a\nb", &[edit(2, 2, &["B", "C"])]).unwrap(),
            "a\nB\nC"
        );
        // A one-line file with no terminator at all: the replacement has
        // nothing to copy, and `\n` is the only sane default.
        assert_eq!(apply("a", &[edit(1, 1, &["x", "y"])]).unwrap(), "x\ny");
    }

    #[test]
    fn several_edits_apply_bottom_up_and_a_range_can_grow_or_shrink() {
        assert_eq!(
            apply(
                "a\nb\nc\nd\ne\n",
                &[edit(1, 1, &["A", "A2"]), edit(4, 5, &["D"])]
            )
            .unwrap(),
            "A\nA2\nb\nc\nD\n",
            "the first edit's growth must not move the second's anchor"
        );
        // The delete case, which is what an empty suggestion means.
        assert_eq!(apply("a\nb\nc\n", &[edit(2, 2, &[])]).unwrap(), "a\nc\n");
        assert_eq!(apply("a\nb\nc\n", &[edit(1, 3, &[])]).unwrap(), "");
    }

    #[test]
    fn a_range_the_file_does_not_have_is_refused_rather_than_clamped() {
        for (e, quoted) in [
            (edit(0, 1, &["x"]), "lines 0-1"),
            (edit(2, 1, &["x"]), "lines 2-1"),
            (edit(3, 4, &["x"]), "lines 3-4"),
        ] {
            let err = apply("a\nb\n", &[e]).expect_err("accepted");
            assert!(err.contains(quoted), "{err}");
            assert!(err.contains("lines 1-2"), "{err}");
        }
    }

    /// The guard behind the route's own overlap refusal. A silent
    /// misapplication would commit one reviewer's text over another's
    /// lines, and nothing downstream would notice.
    #[test]
    fn overlapping_edits_are_refused_by_the_applier_too() {
        let err =
            apply("a\nb\nc\n", &[edit(1, 2, &["x"]), edit(2, 3, &["y"])]).expect_err("accepted");
        assert!(err.contains("lines 1-2 and 2-3 overlap"), "{err}");
    }

    #[test]
    fn a_suggestion_that_matches_the_file_is_a_no_op() {
        assert!(is_noop("a\nb\nc\n", &edit(2, 2, &["b"])));
        assert!(!is_noop("a\nb\nc\n", &edit(2, 2, &["B"])));
        assert!(!is_noop("a\nb\nc\n", &edit(2, 2, &["b", "b"])));
        // A deletion is never a no-op: it removes lines that are there.
        assert!(!is_noop("a\nb\nc\n", &edit(2, 2, &[])));
    }

    fn anchor(comment: &str, path: &str, start: i64, end: i64) -> Anchor {
        Anchor {
            comment: comment.to_string(),
            path: path.to_string(),
            start,
            end,
        }
    }

    /// Two suggestions over one line are two reviewers disagreeing, and
    /// the pair reported must not depend on the order the client sent
    /// the ids in.
    #[test]
    fn overlapping_anchors_are_found_in_a_stable_order() {
        let none = [
            anchor("c1", "a.rs", 1, 2),
            anchor("c2", "a.rs", 3, 4),
            anchor("c3", "b.rs", 1, 9),
        ];
        assert!(
            first_overlap(&none).is_none(),
            "adjacent is not overlapping"
        );
        let clash = [
            anchor("c2", "a.rs", 2, 9),
            anchor("c3", "b.rs", 1, 1),
            anchor("c1", "a.rs", 1, 4),
        ];
        let (a, b) = first_overlap(&clash).expect("a clash");
        assert_eq!((a.comment.as_str(), b.comment.as_str()), ("c1", "c2"));
        // Containment counts: a suggestion inside another's range is
        // still two people editing one line.
        let nested = [anchor("c1", "a.rs", 1, 10), anchor("c2", "a.rs", 4, 5)];
        assert!(first_overlap(&nested).is_some());
        // Two comments on one line of two different files are fine.
        let apart = [anchor("c1", "a.rs", 1, 1), anchor("c2", "b.rs", 1, 1)];
        assert!(first_overlap(&apart).is_none());
    }

    #[test]
    fn a_transaction_failure_reads_as_one_sentence_either_way() {
        assert_eq!(
            txn_message(TxnError::Other("store said no".into())),
            "store said no"
        );
        assert!(
            txn_message(TxnError::Conflict("refs/patchsets/abc".into(), None))
                .contains("refs/patchsets/abc moved")
        );
    }

    #[test]
    fn every_refusal_names_the_file_and_the_lines() {
        assert!(Stale::Gone {
            path: "core.rs".into(),
            patchset: 3
        }
        .sentence()
        .contains("patchset 3 no longer has core.rs"));
        assert!(Stale::Moved {
            path: "core.rs".into(),
            from: 1,
            patchset: 3
        }
        .sentence()
        .contains("changed since patchset 1"));
        assert!(Stale::PastEnd {
            path: "core.rs".into(),
            start: 8,
            end: 9,
            have: 2
        }
        .sentence()
        .contains("lines 8-9 of core.rs, which has 2 lines"));
        assert!(Stale::NotText {
            path: "logo.png".into()
        }
        .sentence()
        .contains("logo.png is not a text file"));
        assert!(Stale::NoOp {
            path: "core.rs".into(),
            start: 1,
            end: 1
        }
        .sentence()
        .contains("lines 1-1 of core.rs already read"));
    }
}
