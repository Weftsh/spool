import { describe, expect, it } from "vitest";
import { reviewerStanding } from "@/views/changes";
import { requiredReviewers, type ChangeVerdict, type ReviewerSet } from "@/api";

function reviewer(id: string, name: string, approved: boolean) {
  return {
    user_id: id,
    name,
    email: `${name.toLowerCase()}@acme.test`,
    approved,
  };
}

function set(
  required: ReviewerSet["required"],
  anyone_with_write = false,
): ReviewerSet {
  return { required, anyone_with_write };
}

/// The reviewer set is the product's argument for itself — the list is
/// computed, not nominated — so the sentences it produces are worth
/// pinning without a browser.
describe("reviewerStanding", () => {
  it("counts the approvals and leaves the list as the server ordered it", () => {
    const s = reviewerStanding(
      set([
        reviewer("u1", "Alice", true),
        reviewer("u2", "Casey", false),
        reviewer("u3", "Dev", false),
      ]),
      null,
    );
    expect(s?.approved).toBe(1);
    expect(s?.reviewers.map((r) => r.name)).toEqual(["Alice", "Casey", "Dev"]);
    expect(s?.note).toBeNull();
    expect(s?.viewerRequired).toBe(false);
  });

  // The two empty lists are the reason `anyone_with_write` exists on the
  // wire at all: they are identical in `required` and opposite in what
  // they ask of a reader.
  it("tells a * rule apart from no rule at all", () => {
    const starred = reviewerStanding(set([], true), null);
    expect(starred?.note).toMatch(/anyone with write access/);
    expect(starred?.note).toMatch(/\*/);

    const ungoverned = reviewerStanding(set([], false), null);
    expect(ungoverned?.note).toMatch(/No OWNERS rule governs/);
    expect(ungoverned?.note).not.toEqual(starred?.note);
  });

  it("knows the viewer by id, and whether their own approval is in", () => {
    const waiting = reviewerStanding(
      set([reviewer("u1", "Alice", true), reviewer("u2", "Casey", false)]),
      "u2",
    );
    expect(waiting?.viewerRequired).toBe(true);
    expect(waiting?.viewerApproved).toBe(false);

    const done = reviewerStanding(
      set([reviewer("u1", "Alice", true), reviewer("u2", "Casey", false)]),
      "u1",
    );
    expect(done?.viewerRequired).toBe(true);
    expect(done?.viewerApproved).toBe(true);

    // Somebody who is merely reading is not on the list, and a signed
    // out reader has no id to match at all.
    expect(
      reviewerStanding(set([reviewer("u1", "Alice", true)]), "u9")
        ?.viewerRequired,
    ).toBe(false);
    expect(
      reviewerStanding(set([reviewer("u1", "Alice", true)]), null)
        ?.viewerRequired,
    ).toBe(false);
  });

  /// A server that predates the field says nothing, and the card must
  /// say nothing back. Rendering "nobody is required" over an absent
  /// field would invite an approval nobody asked for and, worse, tell
  /// the person the change is actually waiting on that it is not.
  it("renders nothing for a server that does not send a reviewer set", () => {
    const old = {
      change: "Icafe1234",
      state: "open",
      patchset: 1,
      commit: "a".repeat(40),
      verdict: { landable: false, explanation: "blocked", per_path: [] },
    } as ChangeVerdict;
    expect(requiredReviewers(old)).toBeNull();
    expect(reviewerStanding(requiredReviewers(old), "u1")).toBeNull();
  });
});
