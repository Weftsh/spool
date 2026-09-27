// Threading, resolution and anchoring, as arithmetic rather than as
// JSX. The conversation panel and the anchored rows inside the diff both
// render the same threads, and two places that grouped a flat comment
// list separately would one day group it differently — a reply nested
// under the wrong root in one of the two views, with nothing on the page
// to say which is right.
//
// Everything here is pure and unit-tested, because none of it needs a
// browser to be wrong.

import type { ChangeComment, CommentSide, Me } from "@/api";
import type { ReviewerStanding } from "@/views/changes";

/// Which half of the diff an anchor counts in.
///
/// `"old"` is a line the patchset **deleted**: the same number means a
/// different line on each side, which is why the server refuses to
/// coerce an unknown side to `new` and why nothing here defaults it
/// silently either.
export type Side = CommentSide;

/// A comment, read as a thread member. An alias rather than a second
/// interface: the threading fields live on `ChangeComment` where the
/// next person will find them, and two shapes for one JSON object is how
/// the two drift.
export type ThreadComment = ChangeComment;

/// One thread: the remark, and the replies under it. One level deep,
/// because the server refuses a reply to a reply — see `add_comment`.
export interface Thread {
  root: ThreadComment;
  replies: ThreadComment[];
  resolved: boolean;
  /// The resolver's display name when the server sent one. A thread can
  /// be resolved without a name — an account removed since — and the
  /// summary says "resolved" rather than inventing somebody.
  resolvedBy: string | null;
}

/// Which side a comment anchors to, defaulting to the new one.
///
/// A server older than 0050 sends no `side` at all and every comment it
/// ever stored was against the new side, so the default is the truth
/// there rather than a guess.
export function sideOf(c: ThreadComment): Side {
  return c.side === "old" ? "old" : "new";
}

/// The grouping key for one row: the server's if it sent one, otherwise
/// derived the same way the server derives it.
function threadIdOf(c: ThreadComment): string {
  return c.thread_id ?? c.parent_id ?? c.id;
}

/// Fold the flat, oldest-first conversation into threads, keeping that
/// order: the sequence a review was spoken in is meaning, and threads
/// are listed by when their root was written.
///
/// A reply whose root is not in the list is **promoted to a root of its
/// own** rather than dropped. That cannot happen against a healthy
/// server, and dropping it would be the worse failure of the two: a
/// silently vanished remark is indistinguishable from a review nobody
/// left, where a stray top-level comment is visibly odd and still says
/// what its author said.
export function groupThreads(comments: ThreadComment[] | null): Thread[] {
  if (!comments) return [];
  const byId = new Map<string, Thread>();
  const order: string[] = [];
  const claim = (c: ThreadComment) => {
    const t: Thread = {
      root: c,
      replies: [],
      resolved: c.resolved ?? c.resolved_at != null,
      resolvedBy: c.resolved_by ?? null,
    };
    byId.set(c.id, t);
    order.push(c.id);
    return t;
  };
  // Roots first, in one pass, so a reply that arrives before its root
  // still finds it. The server orders oldest-first and a root is always
  // older than its replies, but relying on that would make this break
  // on the day somebody adds a filter.
  for (const c of comments) if (!c.parent_id) claim(c);
  for (const c of comments) {
    if (!c.parent_id) continue;
    const root = byId.get(threadIdOf(c));
    if (root) root.replies.push(c);
    else claim(c);
  }
  return order.map((id) => byId.get(id) as Thread);
}

/// Whether the server behind this conversation knows about threads at
/// all.
///
/// A deployment older than migration 0050 answers with none of the seven
/// fields, and *absent* is not *empty*: every row would read as an
/// unresolved root, so the page would print "3 unresolved" about a
/// server with no notion of resolution and offer a Reply the route would
/// refuse. Both are the show-and-fail this codebase keeps taking out.
/// `thread_id` is the marker because the server computes it for **every**
/// row post-0050 — a root's is its own id — where `parent_id` and
/// `resolved_at` are legitimately null on a perfectly modern comment.
///
/// A conversation with nothing in it answers false, which costs nothing:
/// there is no thread to reply to or settle either way.
export function threadsSupported(comments: ThreadComment[] | null): boolean {
  return (comments ?? []).some((c) => c.thread_id !== undefined);
}

/// How many threads nobody has called settled.
///
/// A **fact**, not a gate: nothing about landing consults it, and the
/// header that prints it must not imply otherwise. Counted over threads
/// rather than comments, because six replies under one open remark are
/// one piece of unfinished business, not six.
export function unresolvedCount(threads: Thread[]): number {
  return threads.filter((t) => !t.resolved).length;
}

/// Where one thread hangs in the diff: the file, the side, and the last
/// line of its anchor.
///
/// The **last** line, so a comment written against lines 10–14 appears
/// under 14 — under the end of what it is about, which is where the
/// reader's eye already is and where the selection gesture left them.
export interface Anchor {
  path: string;
  side: Side;
  line: number;
}

/// The anchor of one thread, or null when it is a comment on the change
/// as a whole rather than on a line.
export function anchorOf(c: ThreadComment): Anchor | null {
  if (!c.path || c.line === null || c.line === undefined) return null;
  return {
    path: c.path,
    side: sideOf(c),
    line: c.line_end ?? c.line,
  };
}

/// The threads anchored to exactly one row of the rendered diff.
export function threadsAt(
  threads: Thread[],
  path: string,
  side: Side,
  line: number | null,
): Thread[] {
  if (line === null) return [];
  return threads.filter((t) => {
    const a = anchorOf(t.root);
    return a !== null && a.path === path && a.side === side && a.line === line;
  });
}

/// How an anchor reads beside a comment: `path:12`, or `path:10–14`.
///
/// The en dash is deliberate — a hyphen inside a line range reads as
/// part of a filename at this font size, and the range is the thing that
/// tells a reviewer the remark is about a block rather than a line.
export function anchorLabel(c: ThreadComment): string | null {
  if (!c.path) return null;
  if (c.line === null || c.line === undefined) return c.path;
  const end = c.line_end ?? c.line;
  const range = end > c.line ? `${c.line}–${end}` : `${c.line}`;
  return `${c.path}:${range}`;
}

/// The one line a resolved thread collapses to.
///
/// Its first non-empty line, with the markdown left in: a fenced block
/// or a table would be nonsense at this length whichever way it is
/// rendered, and stripping the syntax would take a second, weaker parser
/// beside the real one. Bounded so that a thread whose first line is a
/// paragraph cannot push the resolve control off the row.
export function threadSummary(c: ThreadComment, max = 72): string {
  const first = c.body
    .split("\n")
    .find((l) => l.trim().length > 0)
    ?.trim();
  if (!first) return "";
  return first.length > max ? `${first.slice(0, max - 1)}…` : first;
}

/// Whether the page may offer this viewer the Resolve control, and the
/// sentence to print when it may not.
export interface ResolveStanding {
  may: boolean;
  /// Why not, in words a reader can act on. Null when `may` is true.
  why: string | null;
}

/// Mirror of the server's `may_resolve`, as closely as a client can
/// stand it.
///
/// The server's rule is: the comment's own author always may, and
/// anybody else must **satisfy the commented path under OWNERS** — where
/// a `*` rule or an ungoverned path means write access is what satisfies
/// it. A service token never may, because resolution is a judgement and
/// a token is not a person.
///
/// Two of those three the page knows exactly. The third it knows only
/// per *change*: the verdict carries the reviewer set computed over
/// every path the patchset touches, not per path, so a viewer OWNERS
/// names for `payments/` and not for `docs/` reads as required for both.
/// That is an over-offer of one control on a mixed change, and it is the
/// direction chosen on purpose: the alternative is hiding Resolve from
/// somebody the server would admit, which is a control they can never
/// find. When it does happen the press surfaces the server's own
/// refusal, which is the only sentence that can name the path.
///
/// Everything it *can* be certain about it refuses up front, which is
/// the whole reason this is a function and not an `&&` at the call
/// site: signed in as a token rather than a person, and a reader with
/// no standing at all, are the cases somebody actually meets, and each
/// gets its own sentence.
export function resolveStanding(
  comment: ThreadComment,
  me: Me | null,
  canWrite: boolean,
  standing: ReviewerStanding | null,
): ResolveStanding {
  // A reply carries no state of its own; the thread it is in does. The
  // caller should never ask, but a control drawn on a reply would be one
  // the server refuses outright.
  if (comment.parent_id) {
    return { may: false, why: "Resolve the thread, not a reply in it." };
  }
  if (!me) {
    return {
      may: false,
      // A service token has no `me`, and the server refuses it: settling
      // a thread is something a person does.
      why: "Resolving a thread is a person's judgement — sign in as yourself, not with a token, to settle this one.",
    };
  }
  if (comment.author_principal === `user:${me.id}`)
    return { may: true, why: null };
  // No reviewer set at all: a server older than the computed-set field.
  // Fall back to write access, which is what resolution took before
  // OWNERS was consulted for it.
  if (!standing) {
    return canWrite
      ? { may: true, why: null }
      : {
          may: false,
          why: "Settling somebody else's remark takes write access here.",
        };
  }
  // An empty required list means one of two opposite things, and both
  // land in the same place for this question: with a `*` rule or with no
  // rule at all, write access is what satisfies the path.
  if (standing.note !== null) {
    return canWrite
      ? { may: true, why: null }
      : {
          may: false,
          why: "Nobody owns these files in particular, so settling a thread takes write access here.",
        };
  }
  if (standing.viewerRequired) return { may: true, why: null };
  return {
    may: false,
    why: "OWNERS does not name you for what this patchset touches, so this thread is not yours to settle.",
  };
}
