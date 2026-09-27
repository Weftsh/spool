// The grouping, counting and standing arithmetic behind review threads.
//
// None of this needs a browser to be wrong, and two of the cases below
// cannot be produced in one by construction: an orphaned reply is what a
// filtered read would leave behind, and a server that predates migration
// 0050 sends none of the fields at all. Both are held here rather than
// left to a Playwright fixture that would have to lie to reach them.

import { describe, expect, it } from "vitest";
import type { Me } from "@/api";
import type { ReviewerStanding } from "@/views/changes";
import {
  anchorLabel,
  anchorOf,
  groupThreads,
  resolveStanding,
  sideOf,
  threadSummary,
  threadsAt,
  threadsSupported,
  unresolvedCount,
  type ThreadComment,
} from "@/views/change/threads";

function comment(id: string, over: Partial<ThreadComment> = {}): ThreadComment {
  return {
    id,
    patchset: 1,
    author: "Olive Owner",
    author_email: "olive@acme.test",
    author_principal: "user:01owner",
    path: null,
    line: null,
    body: `body of ${id}`,
    created_at: 1,
    ...over,
  };
}

const ME: Me = {
  id: "01user",
  name: "Ada Reviewer",
  email: "ada@acme.test",
} as Me;

/// An OWNERS-governed change: `standing.note` is null, so there is a
/// real list and only the people on it satisfy a path.
const governed = (viewerRequired: boolean): ReviewerStanding => ({
  reviewers: [],
  approved: 0,
  note: null,
  viewerRequired,
  viewerApproved: false,
});

/// The two empty lists — a `*` rule, and no rule at all — which are
/// opposite sentences and the same answer to "may I resolve".
const ungoverned: ReviewerStanding = {
  reviewers: [],
  approved: 0,
  note: "No OWNERS rule governs what this patchset touches, so any approval with write access satisfies it.",
  viewerRequired: false,
  viewerApproved: false,
};

describe("groupThreads", () => {
  it("nests replies under their root and keeps the order it was spoken in", () => {
    const threads = groupThreads([
      comment("a"),
      comment("b"),
      comment("a1", { parent_id: "a", thread_id: "a" }),
      comment("b1", { parent_id: "b", thread_id: "b" }),
      comment("a2", { parent_id: "a", thread_id: "a" }),
    ]);
    expect(threads.map((t) => t.root.id)).toEqual(["a", "b"]);
    expect(threads[0].replies.map((r) => r.id)).toEqual(["a1", "a2"]);
    expect(threads[1].replies.map((r) => r.id)).toEqual(["b1"]);
  });

  it("finds a root that arrives after its own reply", () => {
    // The server orders oldest-first so this cannot happen today, and
    // relying on that is exactly how the day somebody adds a filter
    // becomes a day replies vanish.
    const threads = groupThreads([
      comment("a1", { parent_id: "a", thread_id: "a" }),
      comment("a"),
    ]);
    expect(threads).toHaveLength(1);
    expect(threads[0].replies.map((r) => r.id)).toEqual(["a1"]);
  });

  it("promotes an orphaned reply rather than dropping it", () => {
    // A remark that silently disappears is indistinguishable from a
    // review nobody left; a stray top-level one is visibly odd and still
    // says what its author said.
    const threads = groupThreads([comment("x1", { parent_id: "gone" })]);
    expect(threads.map((t) => t.root.id)).toEqual(["x1"]);
    expect(threads[0].replies).toEqual([]);
  });

  it("reads resolution off the root, by either field the server sends", () => {
    const [byFlag, byStamp, open] = groupThreads([
      comment("a", { resolved: true, resolved_by: "Olive Owner" }),
      comment("b", { resolved_at: 1_700_000_000 }),
      comment("c"),
    ]);
    expect(byFlag.resolved).toBe(true);
    expect(byFlag.resolvedBy).toBe("Olive Owner");
    // Resolved with nobody named — an account removed since. The summary
    // says "resolved" rather than inventing a person.
    expect(byStamp.resolved).toBe(true);
    expect(byStamp.resolvedBy).toBeNull();
    expect(open.resolved).toBe(false);
  });

  it("is empty for a conversation that has not loaded", () => {
    expect(groupThreads(null)).toEqual([]);
  });
});

describe("threadsSupported", () => {
  it("reads the marker the server sets on every row, not the nullable ones", () => {
    // `parent_id` and `resolved_at` are legitimately null on a perfectly
    // modern root, so neither can tell a 0050 server from an older one.
    // `thread_id` is computed for every row post-0050 — a root's is its
    // own id — which makes it the only honest marker.
    expect(threadsSupported([comment("a", { thread_id: "a" })])).toBe(true);
    expect(threadsSupported([comment("a")])).toBe(false);
    expect(threadsSupported([])).toBe(false);
    expect(threadsSupported(null)).toBe(false);
  });
});

describe("unresolvedCount", () => {
  it("counts threads, not comments", () => {
    // Six replies under one open remark are one piece of unfinished
    // business, not six.
    const threads = groupThreads([
      comment("a"),
      comment("a1", { parent_id: "a" }),
      comment("a2", { parent_id: "a" }),
      comment("b", { resolved: true }),
      comment("c"),
    ]);
    expect(unresolvedCount(threads)).toBe(2);
  });
});

describe("anchoring", () => {
  it("defaults a comment from a server that predates sides to the new one", () => {
    expect(sideOf(comment("a"))).toBe("new");
    expect(sideOf(comment("a", { side: "old" }))).toBe("old");
  });

  it("hangs a range under its last line, and a bare comment nowhere", () => {
    expect(
      anchorOf(comment("a", { path: "x.rs", line: 10, line_end: 14 })),
    ).toEqual({ path: "x.rs", side: "new", line: 14 });
    expect(anchorOf(comment("a", { path: "x.rs", line: 10 }))).toEqual({
      path: "x.rs",
      side: "new",
      line: 10,
    });
    // A comment on the change as a whole belongs in the conversation
    // panel and on no row of the diff.
    expect(anchorOf(comment("a"))).toBeNull();
    expect(anchorOf(comment("a", { path: "x.rs" }))).toBeNull();
  });

  it("keeps the two sides of one line number apart", () => {
    // The whole reason `side` is not coerced: line 10 of the old side
    // and line 10 of the new side are different lines, and a thread on
    // a deleted line rendered against the added one is a review comment
    // pointing at code it is not about.
    const threads = groupThreads([
      comment("n", { path: "x.rs", line: 10 }),
      comment("o", { path: "x.rs", line: 10, side: "old" }),
      comment("elsewhere", { path: "y.rs", line: 10 }),
    ]);
    expect(threadsAt(threads, "x.rs", "new", 10).map((t) => t.root.id)).toEqual(
      ["n"],
    );
    expect(threadsAt(threads, "x.rs", "old", 10).map((t) => t.root.id)).toEqual(
      ["o"],
    );
    // A context row on the other side of the file, and a row with no
    // number at all, hold nothing.
    expect(threadsAt(threads, "x.rs", "new", 11)).toEqual([]);
    expect(threadsAt(threads, "x.rs", "new", null)).toEqual([]);
  });

  it("labels a range as a range and a line as a line", () => {
    expect(
      anchorLabel(comment("a", { path: "x.rs", line: 10, line_end: 14 })),
    ).toBe("x.rs:10–14");
    expect(
      anchorLabel(comment("a", { path: "x.rs", line: 10, line_end: 10 })),
    ).toBe("x.rs:10");
    expect(anchorLabel(comment("a", { path: "x.rs" }))).toBe("x.rs");
    expect(anchorLabel(comment("a"))).toBeNull();
  });
});

describe("threadSummary", () => {
  it("takes the first line that says anything, and bounds it", () => {
    expect(threadSummary(comment("a", { body: "\n\nfirst\nsecond" }))).toBe(
      "first",
    );
    // A thread whose opening line is a paragraph must not push the
    // resolve control off the collapsed row.
    const long = comment("a", { body: "x".repeat(200) });
    expect(threadSummary(long, 10)).toBe(`${"x".repeat(9)}…`);
    expect(threadSummary(comment("a", { body: "   " }))).toBe("");
  });
});

describe("resolveStanding", () => {
  it("never offers the control on a reply", () => {
    const r = resolveStanding(
      comment("a1", { parent_id: "a", author_principal: `user:${ME.id}` }),
      ME,
      true,
      ungoverned,
    );
    expect(r.may).toBe(false);
    expect(r.why).toMatch(/not a reply/);
  });

  it("refuses a reader with no person behind them", () => {
    // Signed in with a service token has no `me`, and the server refuses
    // it.
    const r = resolveStanding(comment("a"), null, true, ungoverned);
    expect(r.may).toBe(false);
    expect(r.why).toMatch(/person's judgement/);
  });

  it("lets the comment's own author settle it, with no standing at all", () => {
    const mine = comment("a", { author_principal: `user:${ME.id}` });
    expect(resolveStanding(mine, ME, false, governed(false))).toEqual({
      may: true,
      why: null,
    });
  });

  it("takes write access where nobody owns the files in particular", () => {
    expect(resolveStanding(comment("a"), ME, true, ungoverned).may).toBe(true);
    const refused = resolveStanding(comment("a"), ME, false, ungoverned);
    expect(refused.may).toBe(false);
    expect(refused.why).toMatch(/write access/);
  });

  it("takes being named by OWNERS where OWNERS names anybody", () => {
    expect(resolveStanding(comment("a"), ME, false, governed(true)).may).toBe(
      true,
    );
    // ...and write access is *not* enough on a governed path. This is
    // the case that separates resolution from every other write action
    // on the page: the change's own author, with write, still cannot
    // silence a remark about a file they do not own.
    const refused = resolveStanding(comment("a"), ME, true, governed(false));
    expect(refused.may).toBe(false);
    expect(refused.why).toMatch(/does not name you/);
  });

  it("falls back to write access against a server that sends no reviewer set", () => {
    expect(resolveStanding(comment("a"), ME, true, null).may).toBe(true);
    expect(resolveStanding(comment("a"), ME, false, null).may).toBe(false);
  });
});
