// The batched review's arithmetic: what counts as a draft, whether the
// server has the feature at all, and what a standing "no" means.
//
// Two of these cannot be reached from a browser fixture without lying
// about the wire. A server older than migration 0051 sends no `reviews`,
// no `blocks` and no `pending`, and the whole point of the capability
// gate is that *absent* and *empty* are different claims — which is a
// distinction about the JSON, not about the page.

import { describe, expect, it } from "vitest";
import type {
  ChangeComment,
  ChangeDetail,
  ChangeVerdict,
  Me,
  StandingBlock,
} from "@/api";
import {
  batchedReviewSupported,
  draftComments,
  isDraft,
  pendingLabel,
  readBlocks,
  reviewRefusal,
} from "@/views/change/review";

function comment(id: string, over: Partial<ChangeComment> = {}): ChangeComment {
  return {
    id,
    patchset: 1,
    author: "Ada Owner",
    author_email: "owner@acme.test",
    author_principal: "user:01user",
    path: null,
    line: null,
    body: "words",
    created_at: 0,
    ...over,
  };
}

function detail(over: Partial<ChangeDetail> = {}): ChangeDetail {
  return {
    change: {
      key: "Icafe1234",
      title: "add gateway",
      target_branch: "main",
      state: "open",
      land_verdict: null,
      landed_commit: null,
      created_at: 0,
      updated_at: 0,
      patchset: null,
    },
    patchsets: [],
    approvals: [],
    ...over,
  };
}

function block(over: Partial<StandingBlock> = {}): StandingBlock {
  return {
    id: "01r1",
    verdict: "request_changes",
    body: "the retry loop is unbounded",
    author: "Olive Owner",
    author_email: "olive@acme.test",
    user_id: "01owner",
    patchset_id: "01ps1",
    state: "submitted",
    submitted_at: 10,
    withdrawn_at: null,
    created_at: 10,
    blocking: true,
    ...over,
  };
}

function verdict(blocks?: StandingBlock[]): ChangeVerdict {
  return {
    change: "Icafe1234",
    state: "open",
    patchset: 1,
    commit: "1".repeat(40),
    verdict: { landable: true, explanation: "ok", per_path: [] },
    ...(blocks === undefined ? {} : { blocks }),
  };
}

const ME: Me = {
  id: "01user",
  email: "owner@acme.test",
  name: "Ada Owner",
  handle: "ada",
  created_at: 0,
  orgs: [],
};

describe("batchedReviewSupported", () => {
  it("is false for a server that sends no reviews field at all", () => {
    // The pre-0051 shape. Everything else about the change read is
    // identical, which is exactly why the page cannot tell them apart
    // by looking at the comments.
    expect(batchedReviewSupported(detail())).toBe(false);
  });

  it("is true for a modern server with no reviews yet", () => {
    // The case the whole gate exists for. An empty list is a positive
    // answer — "nobody has reviewed" — and reading it as silence would
    // hide the feature on every change until somebody used it, which is
    // the one change nobody can use it on.
    expect(batchedReviewSupported(detail({ reviews: [] }))).toBe(true);
  });

  it("says nothing about a change that has not loaded", () => {
    expect(batchedReviewSupported(null)).toBe(false);
  });
});

describe("draftComments", () => {
  it("keeps only the rows the server marked pending", () => {
    const rows = [
      comment("a", { pending: false }),
      comment("b", { pending: true }),
      comment("c", { pending: true }),
    ];
    expect(draftComments(rows).map((c) => c.id)).toEqual(["b", "c"]);
  });

  it("treats a pre-0051 comment as published, not as an unsent draft", () => {
    // `published_at` is absent there too, so a client reading
    // `published_at == null` would report every comment ever written as
    // somebody's unsent draft and offer to submit a review of them.
    const old = comment("a");
    expect(old.pending).toBeUndefined();
    expect(isDraft(old)).toBe(false);
    expect(draftComments([old])).toEqual([]);
  });

  it("has nothing to say about a conversation that has not loaded", () => {
    expect(draftComments(null)).toEqual([]);
  });
});

describe("readBlocks", () => {
  it("carries the server's blocking answer through rather than deriving one", () => {
    // Both rows have verdict `request_changes`; only one of them stops
    // the change. Re-deriving that from the verdict string is the
    // GitHub behaviour this product deliberately does not have.
    const read = readBlocks(
      verdict([
        block({ id: "01r1", blocking: true }),
        block({ id: "01r2", blocking: false, user_id: "01stranger" }),
      ]),
      null,
    );
    expect(read.map((b) => b.authoritative)).toEqual([true, false]);
    expect(read[0].why).toContain("cannot land until they withdraw it");
    expect(read[1].why).toContain("does not block landing");
  });

  it("offers withdrawal to the block's author and to nobody else", () => {
    const read = readBlocks(
      verdict([
        block({ id: "01r1", user_id: ME.id }),
        block({ id: "01r2", user_id: "01someone-else" }),
      ]),
      ME,
    );
    expect(read.map((b) => b.mine)).toEqual([true, false]);
  });

  it("claims nothing for a reader the page has no identity for", () => {
    // A token session, or a mount that never threaded `me`. Offering
    // Withdraw there would be a control that answers 404 on a block
    // somebody else raised.
    const read = readBlocks(verdict([block({ user_id: ME.id })]), null);
    expect(read[0].mine).toBe(false);
  });

  it("is empty for a server that sends no blocks, and for one with none", () => {
    expect(readBlocks(verdict(), ME)).toEqual([]);
    expect(readBlocks(verdict([]), ME)).toEqual([]);
    expect(readBlocks(null, ME)).toEqual([]);
  });
});

describe("reviewRefusal", () => {
  it("refuses a block with neither words nor drafted comments", () => {
    // The server's own rule: "a block with nothing in it is a wall with
    // no door". Mirrored so the cost of learning it is not the
    // reviewer's paragraph coming back attached to a 400.
    expect(reviewRefusal("request_changes", "   ", 0)).toContain("needs words");
  });

  it("allows a block whose comments carry the argument", () => {
    expect(reviewRefusal("request_changes", "", 2)).toBeNull();
  });

  it("allows a block with a cover message and no comments", () => {
    expect(reviewRefusal("request_changes", "unbounded retry", 0)).toBeNull();
  });

  it("asks nothing of an approval or a plain comment", () => {
    // The drafted comments *are* the review; a cover message invented to
    // satisfy a form is a field asking somebody to repeat themselves.
    expect(reviewRefusal("approve", "", 0)).toBeNull();
    expect(reviewRefusal("comment", "", 0)).toBeNull();
  });
});

describe("pendingLabel", () => {
  it("counts one comment in the singular", () => {
    expect(pendingLabel(1)).toBe("1 pending comment");
    expect(pendingLabel(2)).toBe("2 pending comments");
  });
});
