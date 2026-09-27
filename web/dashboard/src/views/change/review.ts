// The batched review, as arithmetic rather than as JSX: which comments
// are still only the author's, whether this server knows about any of
// it, and what a standing "no" means to the person reading it.
//
// Everything here is pure and unit-tested, for the same reason the
// threading arithmetic next door is: none of it needs a browser to be
// wrong, and two of the questions — "does absent mean empty" and "does
// this block actually block" — are exactly the ones a page gets wrong
// silently.

import type {
  ChangeComment,
  ChangeDetail,
  ChangeVerdict,
  Me,
  ReviewVerdictKind,
  StandingBlock,
} from "@/api";

/// Whether the server behind this change knows about batched reviews at
/// all.
///
/// The same argument as `threadsSupported`, one migration later. A
/// deployment older than 0051 answers with no `reviews`, no `blocks`
/// and no `pending`, and *absent* is not *empty*: every comment would
/// read as published — which is right — but the page would also draw a
/// "Finish your review" bar over `…/review/submit`, a route that is not
/// there, and would have taken the standalone Approve button away in
/// exchange for a sheet that 404s. So the whole surface is gated on a
/// **positive** signal.
///
/// `reviews` is the marker rather than `blocks` or `pending`, because
/// the server sends the key on every change read post-0051 — an empty
/// array when nobody has reviewed yet. `blocks` rides on a second
/// request, and `pending` only exists on a row somebody drafted, so
/// both are silent in exactly the state a new change is in.
export function batchedReviewSupported(detail: ChangeDetail | null): boolean {
  return detail?.reviews !== undefined;
}

/// Whether one comment is still only its author's.
///
/// Read off `pending` and not off `published_at`, because they answer
/// different questions on an older server: `published_at` is absent
/// there too, and `published_at == null` would read every comment ever
/// written as an unsent draft.
export function isDraft(c: ChangeComment): boolean {
  return c.pending === true;
}

/// The caller's unsent comments, oldest first.
///
/// Nobody else's can be here — the server never returns another
/// person's pending row to anybody — so this is a filter and not a
/// permission check. It is still written as a filter over the whole
/// conversation rather than as a second fetch of `GET …/review`: one
/// list means the diff, the conversation panel and the count can never
/// disagree about what is drafted.
export function draftComments(
  comments: ChangeComment[] | null,
): ChangeComment[] {
  return (comments ?? []).filter(isDraft);
}

/// One standing request for changes, as the page has to say it.
export interface BlockStanding {
  block: StandingBlock;
  /// Whether it actually stops the change landing — the server's
  /// answer, never re-derived here.
  authoritative: boolean;
  /// Why, in words, for the reader who has to act on it.
  why: string;
  /// Whether the viewer is the person who raised it, and so the only
  /// person who can take it back.
  mine: boolean;
}

/// The words that go beside each kind of block.
///
/// Both are said out loud rather than left to a colour or a badge,
/// because they are opposite instructions to the change's author: one
/// means "this will not land until they say so", the other means "read
/// this, then decide". A page that rendered both as "changes
/// requested" would send an author chasing somebody whose opinion, on
/// this repository's own rules, does not gate anything.
///
/// Neither sentence opens with the word on the badge beside it. That is
/// not only tidier: "Advisory" as a badge and "Advisory — …" as the
/// sentence under it are two elements a reader — and a test — cannot
/// tell apart by their words, so the badge stops being the thing that
/// says which kind this is.
const AUTHORITATIVE =
  "OWNERS names them for a file this patchset touches, so this change " +
  "cannot land until they withdraw it.";
const ADVISORY =
  "OWNERS does not name them for anything this patchset touches, so it " +
  "is recorded but does not block landing.";

/// Read the verdict's standing blocks the way the panel renders them.
///
/// `[]` for a server that sends no `blocks` at all, which is the same
/// answer as "nobody has asked for changes" and is safe to conflate
/// here: there is nothing to draw either way. The distinction that
/// matters — whether to offer the *review* surface — is
/// `batchedReviewSupported`, and it is asked separately on purpose.
///
/// A withdrawn block never arrives: the server's query excludes it, and
/// filtering again here would be a second rule that could one day
/// disagree with the first.
export function readBlocks(
  verdict: ChangeVerdict | null,
  me: Me | null,
): BlockStanding[] {
  return (verdict?.blocks ?? []).map((block) => ({
    block,
    authoritative: block.blocking,
    why: block.blocking ? AUTHORITATIVE : ADVISORY,
    // Compared on the user id, which is what the server keys a
    // withdrawal by. Matching on the display name would offer the
    // control to a namesake, and matching on email would miss somebody
    // whose address the server declined to send.
    mine: me !== null && block.user_id === me.id,
  }));
}

/// The three verdicts, in the order they are offered, each with the
/// sentence that says what pressing it does.
///
/// There is no fourth, and there is deliberately no reviewer picker
/// beside them — see `NO_PICKER`.
export const REVIEW_VERDICTS: {
  value: ReviewVerdictKind;
  label: string;
  help: string;
}[] = [
  {
    value: "approve",
    label: "Approve",
    help: "Sign this patchset off. A new patchset starts the count over.",
  },
  {
    value: "comment",
    label: "Comment",
    help: "Publish what you wrote without a verdict — this leaves any approval of yours alone.",
  },
  {
    value: "request_changes",
    label: "Request changes",
    help: "Say no, and say what would make it a yes. It survives the next patchset and ends when you withdraw it.",
  },
];

/// Why the sheet cannot be submitted yet, or null when it can.
///
/// One rule, and it is the server's: asking for changes with neither a
/// cover message nor a drafted comment is refused with "a block with
/// nothing in it is a wall with no door". Mirrored here rather than
/// left to the round trip because the cost of discovering it late is
/// the reviewer's own words — the sheet would come back with a 400 over
/// a form they have to read twice to understand.
///
/// Nothing else is checked. An empty `approve` or `comment` is a
/// perfectly good review: the drafted comments are the review, and a
/// cover message that has to be invented to send them is a form asking
/// somebody to repeat themselves.
export function reviewRefusal(
  verdict: ReviewVerdictKind,
  body: string,
  drafts: number,
): string | null {
  if (verdict !== "request_changes") return null;
  if (body.trim().length > 0 || drafts > 0) return null;
  return "Asking for changes needs words: say what would make this a yes, here or on a line.";
}

/// The refusal, stated where somebody would look for the thing that is
/// missing.
///
/// This product does not have "request a review from", and the sheet is
/// exactly where a reviewer would reach for it. Saying nothing would
/// read as an oversight; saying it here points at the answer instead —
/// the set is computed from OWNERS on the target branch, and it is on
/// the page already.
export const NO_PICKER =
  "There is nobody to nominate: who this change needs is computed from " +
  "OWNERS on the target branch — see Required reviewers.";

/// How the bar counts what is waiting.
///
/// Spelled out rather than assembled at the call site so the singular
/// is not one interpolation away from "1 pending comments", which is
/// the first thing a reader notices and the last thing anybody tests.
export function pendingLabel(n: number): string {
  return n === 1 ? "1 pending comment" : `${n} pending comments`;
}
