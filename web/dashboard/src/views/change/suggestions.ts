// Suggested changes, as arithmetic rather than as JSX: where the
// suggestion blocks are inside a comment body, whether the server would
// accept one if somebody pressed Apply, and which lines of the file the
// reviewer is proposing to replace.
//
// All of it is pure and unit-tested, for the same reason the threading
// and batched-review arithmetic next door is: none of it needs a
// browser to be wrong, and the two questions that matter most — "is
// this actually a suggestion" and "would the server take it" — are
// exactly the ones a page gets wrong silently, by drawing a control
// that leads to a refusal.
//
// **The fence parser here mirrors `crates/stratum-server/src/review/
// suggestion.rs::parse`, deliberately and line for line.** The server
// is the only authority on what a suggestion block is; a client that
// read fences more loosely would offer Apply on a comment the server
// then refuses for carrying no suggestion, and one that read them more
// strictly would hide a control that works. Every rule below is the
// server's rule, and the unit tests carry the same cases its Rust tests
// do.

import type { ChangeComment } from "@/api";

/// One replacement, as written: the lines that should stand in place of
/// the ones the comment is anchored to.
///
/// An **empty** `lines` is not the absence of a suggestion — it is the
/// suggestion to delete those lines, which is a thing reviewers ask for
/// constantly. A comment with no block at all yields no `Suggestion` at
/// all, so the two are distinguishable here exactly as they are on the
/// server, and the page says two different things about them.
export interface Suggestion {
  lines: string[];
}

/// A comment body cut into the prose around its suggestions and the
/// suggestions themselves.
///
/// The prose runs are handed to the same `Markdown` a comment has
/// always been rendered through — not a fork of it — and only the
/// suggestion blocks are drawn specially. The cut is at whole lines, so
/// a suggestion written inside a list item loses that list's context in
/// the rendering: the block is drawn as the mini-diff it is, and the
/// bullet around it renders as two shorter lists. That is the honest
/// trade against forking the renderer, which is the one thing this
/// feature must not do.
export type BodySegment =
  { kind: "text"; text: string } | { kind: "suggestion"; lines: string[] };

/// An open fence, as CommonMark reads one.
interface Fence {
  indent: number;
  ticks: number;
  suggestion: boolean;
}

/// A line with the trailing `\r` of a CRLF comment removed.
///
/// A browser posting a textarea sends CRLF, and the body is stored
/// exactly as it arrived. Matching a fence with the `\r` still on it
/// finds nothing — so a suggestion written in this very dashboard would
/// render as a plain fence and offer no Apply, while the server (which
/// strips it) would happily have applied one.
function unterminated(line: string): string {
  return line.endsWith("\r") ? line.slice(0, -1) : line;
}

/// How many spaces this line opens with. Spaces only, as the server
/// counts them — a tab is not an indent here.
function indentOf(line: string): number {
  return line.length - line.replace(/^ +/, "").length;
}

function openFence(line: string): Fence | null {
  const indent = indentOf(line);
  const rest = line.slice(indent);
  const ticks = rest.length - rest.replace(/^`+/, "").length;
  if (ticks < 3) return null;
  return {
    indent,
    ticks,
    // Exactly `suggestion`, and nothing that merely starts with it.
    // GitHub also understands `suggestion:-0+2`, which moves the
    // anchor; the server does not implement that, and reading one as a
    // plain suggestion would offer to apply the reviewer's text to
    // lines they were not talking about.
    suggestion: rest.slice(ticks).trim() === "suggestion",
  };
}

/// Does this line close a fence opened with `ticks` backticks?
///
/// At least as many backticks and nothing else, which is what lets a
/// four-backtick block contain a three-backtick one — the way a
/// reviewer quotes a fence inside their suggestion.
function closes(line: string, ticks: number): boolean {
  const t = line.trim();
  return t.length >= ticks && t.length > 0 && /^`+$/.test(t);
}

/// Cut a comment body into prose and suggestion blocks, in the order
/// written.
///
/// Non-suggestion fences are walked over whole rather than skipped line
/// by line: a reviewer quoting somebody else's diff inside a fenced
/// ```rust block may well have a fenced suggestion *inside* it, and
/// drawing that as a suggestion would offer to apply text they were
/// quoting rather than proposing.
///
/// An **unterminated** fence yields nothing and swallows the rest of
/// the body, because that is what it is: everything after it is inside
/// a block that never closed. The remainder still renders as prose —
/// the server would refuse such a comment for carrying no suggestion,
/// and this page draws no Apply on it for the same reason.
export function splitSuggestions(body: string): BodySegment[] {
  const raw = body.split("\n").map(unterminated);
  const out: BodySegment[] = [];
  let textFrom = 0;
  let i = 0;
  const flushText = (until: number) => {
    if (until <= textFrom) return;
    out.push({ kind: "text", text: raw.slice(textFrom, until).join("\n") });
  };
  while (i < raw.length) {
    const fence = openFence(raw[i]);
    if (!fence) {
      i += 1;
      continue;
    }
    const lines: string[] = [];
    let j = i + 1;
    while (j < raw.length && !closes(raw[j], fence.ticks)) {
      // CommonMark strips the opening fence's indentation from the
      // content, so a suggestion inside a list item is the text the
      // reviewer sees rather than that text with four spaces welded
      // onto every line.
      const line = raw[j];
      lines.push(line.slice(Math.min(indentOf(line), fence.indent)));
      j += 1;
    }
    if (j >= raw.length) break;
    if (fence.suggestion) {
      flushText(i);
      out.push({ kind: "suggestion", lines });
      textFrom = j + 1;
    }
    i = j + 1;
  }
  flushText(raw.length);
  return out;
}

/// Every suggestion block in a comment body, in the order written.
export function suggestionsIn(body: string): Suggestion[] {
  const out: Suggestion[] = [];
  for (const s of splitSuggestions(body)) {
    if (s.kind === "suggestion") out.push({ lines: s.lines });
  }
  return out;
}

/// Where one suggestion goes: the file, and the inclusive line range it
/// stands in place of.
export interface SuggestionAnchor {
  path: string;
  start: number;
  end: number;
  /// The patchset the comment was written against, which is the
  /// revision whose lines it replaces — not necessarily the one on
  /// screen. The server refuses an anchor whose file has moved since,
  /// in words, and the mini-diff must show what the reviewer read
  /// rather than what happens to be at those numbers now.
  patchset: number;
}

/// Whether the server would take this comment as a suggestion to apply,
/// answered here so no control is drawn that leads to a refusal.
///
/// Every clause is one of `apply_suggestions`'s own refusals, in the
/// order it makes them:
///
/// * a **draft** is not a remark yet — applying one would publish it in
///   the one place its author cannot take it back from;
/// * an unanchored comment has no lines to replace;
/// * `side: "old"` names a line the patchset deleted, so there is
///   nothing in the file to put anything in place of;
/// * **two** blocks on one anchor cannot both be those lines.
///
/// Write access is deliberately *not* asked here. It is not a property
/// of the comment, it is a property of the viewer, and folding the two
/// together is how "this cannot be applied" and "you cannot apply
/// things" become one indistinguishable silence on the page.
export function applyAnchor(c: ChangeComment): SuggestionAnchor | null {
  if (c.pending === true) return null;
  if (suggestionsIn(c.body).length !== 1) return null;
  return displayAnchor(c);
}

/// Which lines a suggestion in this comment is *about*, whether or not
/// it can be applied.
///
/// Separate from `applyAnchor` because the mini-diff and the button are
/// two different questions. A reviewer reading back their own drafted
/// suggestion, and an author reading a comment that carries two blocks,
/// both want to see what is being proposed against what — and neither
/// is offered a control. Folding the two together would leave those
/// comments rendering as a plain fence with no sign of which lines they
/// stand in place of.
export function displayAnchor(c: ChangeComment): SuggestionAnchor | null {
  if (!c.path || c.line == null) return null;
  // Absent `side` is a server older than migration 0050, which is also
  // a server with no apply route — see `suggestionsSupported`. `"old"`
  // names a line the patchset deleted, which the file on screen does
  // not have.
  if (c.side !== "new") return null;
  return {
    path: c.path,
    start: c.line,
    // The server stores `line_end` defaulted to `line`, so a one-line
    // anchor arrives with both. Falling back rather than refusing keeps
    // a row written before that default from reading as unanchored.
    end: c.line_end ?? c.line,
    patchset: c.patchset,
  };
}

/// Whether the server behind this change can apply a suggestion at all.
///
/// **This is a weaker signal than `batchedReviewSupported`, and the
/// difference is worth saying out loud.** Applying suggestions added no
/// column and no field: a suggestion is a fenced block inside a comment
/// body, which is how it has always travelled, so there is nothing new
/// on any read response to detect. What *is* detectable is the floor
/// below it — a deployment older than migration 0050 sends no `side` at
/// all, and the apply route needs `side` and `line_end` to know which
/// lines it is replacing, so no comment from such a server can be
/// applied and no control is drawn.
///
/// Read off the same key `threadsSupported` reads, and for the same
/// reason: `side` rides on every comment post-0050, where `line_end`
/// and `resolved` are null on rows that are simply not anchored or not
/// resolved. Absent is not empty.
///
/// A deployment that has 0050 but not the apply route is **not**
/// distinguished, and cannot be from data already on the page. A
/// probe request was declined deliberately — a HEAD or an empty POST
/// against every change read to find out whether a button may be drawn
/// is a request per page view to learn something no response says. The
/// honest fallback stands instead: the refusal comes back as words and
/// is rendered verbatim, exactly as the 403 and 409 are.
export function suggestionsSupported(
  comments: ChangeComment[] | null,
): boolean {
  return (comments ?? []).some((c) => c.side !== undefined);
}

/// The lines a suggestion stands in place of, read out of the file it
/// anchors to.
///
/// `null` — not `[]` — when the anchor is not in the text: a file
/// shorter than the line the comment names is the staleness the server
/// refuses in words, and an empty array here would draw a mini-diff
/// claiming the reviewer proposed to *add* their lines to nothing.
///
/// The text is split the way a file is, not the way a comment is: a
/// trailing newline ends the last line rather than starting an empty
/// one, which would otherwise make every file one line longer than it
/// is and let an off-by-one anchor look reachable.
export function replacedLines(
  text: string,
  start: number,
  end: number,
): string[] | null {
  if (start < 1 || end < start) return null;
  const all = text.split("\n");
  if (all.length > 0 && all[all.length - 1] === "") all.pop();
  if (end > all.length) return null;
  return all.slice(start - 1, end);
}

/// How the page names a line range, once, so the mini-diff caption and
/// the Apply control cannot say it two different ways.
export function anchorRange(a: SuggestionAnchor): string {
  return a.start === a.end ? `line ${a.start}` : `lines ${a.start}–${a.end}`;
}

/// How many suggestions one call may apply — `MAX_SUGGESTIONS` in
/// `changes_api.rs`, mirrored so the refusal arrives before the trip
/// rather than after it.
export const MAX_APPLY = 50;

/// Why this batch cannot be sent, or null when it can.
///
/// The server's two refusals, said in the sheet rather than discovered
/// by a round trip: nothing selected, and more than fifty. Mirrored for
/// the same reason `reviewRefusal` is — the cost of finding out late is
/// a reviewer's own choices coming back as a 400 over a control they
/// have to read twice to understand.
export function applyRefusal(count: number): string | null {
  if (count === 0) return "Choose at least one suggestion to apply.";
  if (count > MAX_APPLY) {
    return `${count} suggestions is more than the ${MAX_APPLY} one patchset takes — apply them in batches.`;
  }
  return null;
}

/// How the page counts what is selected.
///
/// Spelled out rather than interpolated at the call site so the
/// singular is not one edit away from "1 suggestions", which is the
/// first thing a reader notices and the last thing anybody tests.
export function applyLabel(n: number): string {
  return n === 1 ? "1 suggestion" : `${n} suggestions`;
}
