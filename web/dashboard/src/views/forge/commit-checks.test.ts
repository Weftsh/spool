import { describe, expect, it } from "vitest";
import { BLOCKED_PRESENTATION, runStatePresentation } from "./checks";
import { GLYPH, rollup, rollupLabel, stripPresentation } from "./commit-checks";

const run = (state: string) => ({ state });

/// A commit's checks add up to one glyph, and the glyph is the only thing
/// most readers will ever look at. Every case below is a way it could
/// claim something the checks do not say.
describe("rollup", () => {
  it("is nothing at all when nothing reported", () => {
    // Not neutral, not pending — *nothing*. A grey mark beside every
    // commit in every repository without CI is noise on the majority
    // case, and it also reads as a claim ("we looked, there is nothing")
    // that the commit page has no standing to make. The Checks tab
    // answers that question properly, with five distinct reasons a list
    // can be empty.
    expect(rollup([])).toBeNull();
  });

  it("lets one failure decide, however many passed", () => {
    expect(rollup([run("passing"), run("passing"), run("failing")])).toBe(
      "failing",
    );
    // `cancelled` classifies as failing: it is finished and it did not
    // pass, and filing it under "still running" tells a reader to wait
    // for something that will never arrive.
    expect(rollup([run("passing"), run("cancelled")])).toBe("failing");
  });

  it("prefers a failure over an unfinished run", () => {
    // Both are "not green", but a reader told "still running" waits and
    // a reader told "failed" opens the log. The one that needs acting on
    // wins.
    expect(rollup([run("running"), run("failing")])).toBe("failing");
  });

  it("is pending while anything is unfinished", () => {
    expect(rollup([run("passing"), run("queued")])).toBe("pending");
    expect(rollup([run("passing"), run("running")])).toBe("pending");
    // A word this bundle has never seen classifies as pending, never as
    // a pass: guessing green marks a commit nobody verified.
    expect(rollup([run("passing"), run("blocked-on-approval")])).toBe(
      "pending",
    );
  });

  it("is neutral only when every check was skipped", () => {
    expect(rollup([run("skipped"), run("skipped")])).toBe("neutral");
    // One real pass beside a skip is a passing commit — a path filter
    // that skipped the docs job does not make a green build grey.
    expect(rollup([run("skipped"), run("passing")])).toBe("passing");
  });
});

describe("rollupLabel", () => {
  it("says the state in words, since a glyph carries colour alone", () => {
    expect(rollupLabel("passing", 3)).toBe("All 3 checks passed");
    expect(rollupLabel("failing", 2)).toBe("Some of 2 checks did not pass");
    expect(rollupLabel("pending", 1)).toBe("1 check, some still running");
    expect(rollupLabel("neutral", 4)).toBe("4 checks, all skipped");
  });

  it("agrees with itself about one", () => {
    expect(rollupLabel("passing", 1)).toBe("All 1 check passed");
  });
});

/// The strip's own presentation, and the one case it exists for.
///
/// A refused hosted run is mirrored as `queued` (`workflow/mirror.rs`),
/// so on the commit page — the surface a maintainer lands on for a
/// branch push, where there is no change to read instead — a build that
/// will never start read "Queued" with nothing beside it. "Why did my
/// build not start" gets asked here.
describe("stripPresentation", () => {
  it("says Blocked over the stored word when there is a refusal", () => {
    const p = stripPresentation("queued", "over budget");
    expect(p.label).toBe("Blocked");
    // The same warning tone the Checks tab and the change panel use. A
    // blocked run has not failed, and three pages must not have three
    // ideas of what blocked looks like.
    expect(p.tone).toBe(BLOCKED_PRESENTATION.className);
    expect(p.mark).not.toBe(GLYPH.pending.mark);
  });

  it("leaves an ordinary row exactly as it was", () => {
    const queued = stripPresentation("queued", null);
    expect(queued.label).toBe(runStatePresentation("queued").label);
    expect(queued.mark).toBe(GLYPH.pending.mark);
    const failed = stripPresentation("failure", null);
    expect(failed.label).toBe(runStatePresentation("failure").label);
    expect(failed.mark).toBe(GLYPH.failing.mark);
    expect(failed.tone).toBe(runStatePresentation("failure").className);
  });

  it("renders a state this bundle has never heard of as itself", () => {
    // A seventh word means the server is newer than the page. It must
    // not be silently folded into a state we do know — and a refusal is
    // still a refusal over one.
    expect(stripPresentation("quarantined", null).label).toBe("quarantined");
    expect(stripPresentation("quarantined", "suspended").label).toBe("Blocked");
  });
});
