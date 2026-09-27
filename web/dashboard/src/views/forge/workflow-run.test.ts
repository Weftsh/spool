import { describe, expect, it } from "vitest";
import {
  canCancel,
  defaultJobIndex,
  hostedStatePresentation,
  isLive,
  jobLabel,
  runSubtitle,
} from "./workflow-run";
import { runStatePresentation } from "./checks";

/// The run page's decisions, with no DOM in them. What is asserted here
/// is the part a rendered test could not tell apart from a coincidence:
/// which job opens, which states are live, and — the one that matters
/// most — that a hosted state is never drawn as though it were a
/// different one.

describe("hostedStatePresentation", () => {
  it("translates the runner's words into the Checks tab's, and keeps the meaning", () => {
    // Two vocabularies exist on purpose — `passed` is what our runner
    // writes, `passing` is what every provider's verdict is translated
    // into — and this is the only place they meet. Getting the direction
    // wrong here would draw a failed run green, which is the single most
    // consequential bit on the page.
    expect(hostedStatePresentation("passed").label).toBe("Passing");
    expect(hostedStatePresentation("failed").label).toBe("Failing");
    expect(hostedStatePresentation("running").label).toBe("Running");
    expect(hostedStatePresentation("queued").label).toBe("Queued");
    expect(hostedStatePresentation("cancelled").label).toBe("Cancelled");
  });

  it("draws blocked as its own state, and not as a failure", () => {
    // A blocked run is a fork's change waiting for a maintainer to
    // approve running it. Nothing is wrong with it, so `serious` would
    // read as the change being bad; and folding it into "Queued" would
    // hide that a person has to act before anything moves.
    const b = hostedStatePresentation("blocked");
    expect(b.label).toBe("Blocked");
    expect(b.className).not.toContain("serious");
    expect(b.label).not.toBe(hostedStatePresentation("queued").label);
  });

  it("says an unknown state's own word rather than guessing", () => {
    // A seventh state means the server is newer than this bundle. Shown
    // green it is a lie that ships broken code; shown red it is a lie
    // that blocks good code. The literal word is the only answer that
    // leaves a trace of not knowing.
    const p = hostedStatePresentation("evaporated");
    expect(p.label).toBe("evaporated");
    expect(p.className).toBe("text-ink-3");
  });

  it("gives every state a distinct glyph, so none is read by colour alone", () => {
    const states = [
      "queued",
      "running",
      "passed",
      "failed",
      "cancelled",
      "blocked",
    ];
    const icons = states.map((s) => hostedStatePresentation(s).icon);
    expect(new Set(icons).size).toBe(states.length);
  });
});

describe("isLive", () => {
  it("is true for exactly the two states that still produce output", () => {
    expect(isLive("queued")).toBe(true);
    expect(isLive("running")).toBe(true);
    expect(isLive("passed")).toBe(false);
    expect(isLive("failed")).toBe(false);
    expect(isLive("cancelled")).toBe(false);
    expect(isLive("blocked")).toBe(false);
  });

  it("treats a word it does not know as settled", () => {
    // The failure directions are not symmetric. Guessing "settled" costs
    // a log that is fetched once instead of tailed; guessing "live"
    // holds a socket open forever against a job that will never write to
    // it again, one per reader with the tab open.
    expect(isLive("evaporated")).toBe(false);
  });
});

describe("defaultJobIndex", () => {
  const j = (state: string) => ({ state });

  it("opens on the first failure", () => {
    // The overwhelming reason somebody arrives here is a red check. A
    // twelve-leg matrix with one red leg that opened on a green one
    // would make them hunt for the thing the link was about.
    expect(defaultJobIndex([j("passed"), j("failed"), j("failed")])).toBe(1);
  });

  it("falls back to the job that is running", () => {
    expect(defaultJobIndex([j("passed"), j("running"), j("queued")])).toBe(1);
  });

  it("otherwise opens the first", () => {
    expect(defaultJobIndex([j("queued"), j("queued")])).toBe(0);
    expect(defaultJobIndex([j("passed"), j("passed")])).toBe(0);
  });

  it("says -1 rather than 0 when there are no jobs", () => {
    // A run with no jobs is exactly the case this page matters most for
    // — a workflow file we refused to run — and `0` would index past the
    // end of an empty array.
    expect(defaultJobIndex([])).toBe(-1);
  });

  it("prefers a failure even when it comes after a running job", () => {
    // "What is happening" is the second question; "what broke" is the
    // first, and a run can be both at once on a matrix.
    expect(defaultJobIndex([j("running"), j("failed")])).toBe(1);
  });
});

describe("jobLabel", () => {
  it("uses the key, which already carries the matrix coordinates", () => {
    expect(jobLabel({ key: "build (linux, 1.83)", job_id: "build" })).toBe(
      "build (linux, 1.83)",
    );
  });

  it("falls back to the job id rather than rendering nothing", () => {
    // A blank button is one nobody can click on purpose.
    expect(jobLabel({ key: "", job_id: "build" })).toBe("build");
  });
});

describe("runSubtitle", () => {
  it("says what caused the run, on what, from which file", () => {
    expect(
      runSubtitle({ event: "push", ref_name: "main", file: ".weft/ci.yml" }),
    ).toBe("push on main · .weft/ci.yml");
  });

  it("omits the ref rather than saying `on null`", () => {
    // A run on a change has no branch. The version that interpolated it
    // unconditionally is the sort of defect that only ever appears in
    // the one case nobody has a fixture for.
    expect(
      runSubtitle({ event: "change", ref_name: null, file: ".weft/ci.yml" }),
    ).toBe("change · .weft/ci.yml");
  });

  it("names the changeset a composed run was started for", () => {
    // "changeset on main" said what kind of run it was and not which
    // one; the run of a combination read as one more run of the member.
    expect(
      runSubtitle({
        event: "changeset",
        ref_name: "main",
        file: ".weft/ci.yml",
        changeset: { key: "widget-2" },
      }),
    ).toBe("changeset widget-2 on main · .weft/ci.yml");
    // A member's own run carries no changeset, and says none.
    expect(
      runSubtitle({
        event: "change",
        ref_name: null,
        file: ".weft/ci.yml",
        changeset: null,
      }),
    ).toBe("change · .weft/ci.yml");
  });
});

describe("canCancel", () => {
  it("needs both write access and a run that is still going", () => {
    expect(canCancel({ state: "running" }, true)).toBe(true);
    // A viewer sees no button. The server would refuse them, and a
    // control that cannot do what it says is worse than an absent one.
    expect(canCancel({ state: "running" }, false)).toBe(false);
    // A settled run has nothing to cancel, and the server answers 409.
    expect(canCancel({ state: "passed" }, true)).toBe(false);
    expect(canCancel({ state: "cancelled" }, true)).toBe(false);
    expect(canCancel({ state: "blocked" }, true)).toBe(false);
  });
});

/// A state word that is also a name on `Object.prototype`.
///
/// Both tables a run state is looked up in — `AS_CHECK_STATE` here and
/// `RUN_STATES` in `checks.tsx` — were plain object literals, so
/// `TABLE["constructor"]` returned a *function* rather than `undefined`.
/// The `??` fallback that exists precisely to print an unknown word
/// never fired, and the row rendered with `label: undefined`: a blank
/// pill and a blank accessible name where a verdict should be, which
/// reads as a rendering bug rather than as "we do not know this state".
///
/// Found by the identical assertion on the runners' own state table
/// (`lib/runners.test.ts`), where it failed. `label-pill.tsx` had
/// already documented this exact hazard and defended against it with a
/// `Map`; these two lookups had not been given the same treatment.
describe("a state named after an Object.prototype member", () => {
  for (const word of ["constructor", "toString", "hasOwnProperty"]) {
    it(`prints ${word} as an unknown state rather than a blank`, () => {
      const p = hostedStatePresentation(word);
      expect(p.label).toBe(word);
      expect(typeof p.icon).not.toBe("undefined");
      const q = runStatePresentation(word);
      expect(q.label).toBe(word);
      expect(typeof q.icon).not.toBe("undefined");
    });
  }
});
