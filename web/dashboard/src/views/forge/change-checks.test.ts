import { describe, expect, it } from "vitest";
import {
  COLLAPSE_ABOVE,
  checkBlockers,
  checkDurationLabel,
  checkStatePresentation,
  classifyCheck,
  classifyLandVerdict,
  countLine,
  gateBlockers,
  hasFailingCheck,
  headline,
  startsExpanded,
  summarize,
  type LandGate,
  type PanelCheck,
} from "./change-checks";

// The checks panel's job is to answer two questions without the reader
// opening anything: did CI pass, and if this cannot land, what is in the
// way. Every interesting case below is a way the panel could answer one
// of those two wrongly — a green headline over a skipped suite, a
// "waiting" over a run that was cancelled and will never arrive, a
// duration for a check that never started, or a disabled button with no
// reason attached to it.

function check(over: Partial<PanelCheck> = {}): PanelCheck {
  return { name: "build", state: "passing", ...over };
}

describe("classifyCheck", () => {
  it("reads both writers' vocabularies", () => {
    // `change_checks` writes three words and `check_runs` writes six; the
    // panel must not care which table a row came from.
    expect(classifyCheck("passing")).toBe("passing");
    expect(classifyCheck("success")).toBe("passing");
    expect(classifyCheck("failing")).toBe("failing");
    expect(classifyCheck("failure")).toBe("failing");
    expect(classifyCheck("pending")).toBe("pending");
    expect(classifyCheck("queued")).toBe("pending");
    expect(classifyCheck("running")).toBe("pending");
    expect(classifyCheck("skipped")).toBe("neutral");
  });

  it("counts a cancelled run as a failure, not as something to wait for", () => {
    // A cancelled run is finished and did not pass. Filing it under
    // pending tells a maintainer to wait for a result that is never
    // coming.
    expect(classifyCheck("cancelled")).toBe("failing");
  });

  it("treats a state it has never heard of as pending", () => {
    // Guessing green ships unverified code; guessing red blocks good
    // code. Pending claims neither.
    expect(classifyCheck("neutralised-by-a-newer-server")).toBe("pending");
    expect(classifyCheck("")).toBe("pending");
  });
});

describe("summarize and headline", () => {
  it("says all checks have passed only when some passed and none did not", () => {
    const s = summarize([check(), check({ name: "test" })]);
    expect(s).toMatchObject({
      successful: 2,
      failing: 0,
      pending: 0,
      total: 2,
    });
    expect(headline(s)).toBe("All checks have passed");
  });

  it("lets one failure outrank two hundred passes", () => {
    const many = Array.from({ length: 200 }, (_, i) =>
      check({ name: `pass-${i}` }),
    );
    const s = summarize([...many, check({ name: "lint", state: "failing" })]);
    expect(s.outcome).toBe("failing");
    expect(headline(s)).toBe("Some checks were not successful");
  });

  it("prefers failing over pending when both are present", () => {
    // The boundary that matters: the headline answers "can I stop reading
    // this page", and a failure is a no even while other runs continue.
    const s = summarize([
      check({ name: "lint", state: "failing" }),
      check({ name: "test", state: "running" }),
    ]);
    expect(headline(s)).toBe("Some checks were not successful");
  });

  it("says checks have not completed when only pending ones remain", () => {
    const s = summarize([check(), check({ name: "test", state: "queued" })]);
    expect(s).toMatchObject({ successful: 1, pending: 1 });
    expect(headline(s)).toBe("Some checks haven't completed yet");
  });

  it("refuses to call an all-skipped suite a pass", () => {
    // The one place we do not use GitHub's wording. "All checks have
    // passed" over a suite where nothing ran is the panel's only
    // opportunity to lie outright.
    const s = summarize([
      check({ name: "build", state: "skipped" }),
      check({ name: "test", state: "skipped" }),
    ]);
    expect(s).toMatchObject({ successful: 0, skipped: 2 });
    expect(headline(s)).toBe("No checks were run");
  });

  it("counts an unknown state as pending rather than dropping it", () => {
    const s = summarize([check({ name: "weird", state: "quiesced" })]);
    expect(s).toMatchObject({ pending: 1, total: 1 });
  });
});

describe("countLine", () => {
  it("names every non-empty bucket in a fixed order", () => {
    const s = summarize([
      check(),
      check({ name: "b", state: "failing" }),
      check({ name: "c", state: "running" }),
      check({ name: "d", state: "skipped" }),
    ]);
    expect(countLine(s)).toBe("1 successful, 1 failing, 1 pending, 1 skipped");
  });

  it("drops zero buckets rather than printing them", () => {
    // A standing "0 failing" is a number a reader has to check every time
    // to learn nothing.
    expect(countLine(summarize([check(), check({ name: "t" })]))).toBe(
      "2 successful",
    );
  });

  it("is empty for an empty list", () => {
    expect(countLine(summarize([]))).toBe("");
  });
});

describe("checkDurationLabel", () => {
  const now = 100_000;

  it("prints the elapsed time of a finished run", () => {
    expect(
      checkDurationLabel({ started_at: 40_000, completed_at: 130_000 }, now),
    ).toBe("1m 30s");
  });

  it("flags a run that started and has not finished as so far", () => {
    // An in-progress number presented as a final one is how a reader
    // concludes the suite got faster.
    expect(checkDurationLabel({ started_at: 55_000 }, now)).toBe("45s so far");
  });

  it("says nothing at all for a check with neither timestamp", () => {
    // Not "0s" — an intake check carries no timing, and `0s` beside it
    // reads as an instant pass.
    expect(checkDurationLabel({}, now)).toBeNull();
    expect(
      checkDurationLabel({ started_at: null, completed_at: null }, now),
    ).toBeNull();
  });

  it("says nothing when the timestamps disagree with our clock", () => {
    expect(
      checkDurationLabel({ started_at: 90_000, completed_at: 80_000 }, now),
    ).toBeNull();
  });
});

describe("checkStatePresentation", () => {
  it("gives the intake table's pending its own word", () => {
    // `pending` is the other writer's vocabulary, not an unknown state,
    // so it must not fall through to the raw-word rendering.
    expect(checkStatePresentation("pending")).toMatchObject({
      label: "Pending",
      className: "text-warning",
    });
  });

  it("reuses the Checks tab's words for a polled run", () => {
    expect(checkStatePresentation("passing").label).toBe("Passing");
    expect(checkStatePresentation("cancelled").label).toBe("Cancelled");
  });

  it("prints an unknown state as its own literal word", () => {
    expect(checkStatePresentation("quiesced").label).toBe("quiesced");
  });
});

describe("checkBlockers", () => {
  it("has nothing to say when every required check passed", () => {
    expect(checkBlockers([check({ required: true })], ["build"])).toEqual([]);
  });

  it("distinguishes a required check that failed from one that never reported", () => {
    // Different sentences on purpose. "build failed" sends a reader to
    // the log; "build has not reported" sends them to their CI config.
    // Collapsing the two sends half of them to the wrong place.
    expect(
      checkBlockers([check({ name: "build", state: "failing" })], ["build"]),
    ).toEqual(["1 required check failed: build"]);
    expect(checkBlockers([check({ name: "build" })], ["docs"])).toEqual([
      "1 required check has not reported: docs",
    ]);
  });

  it("separates a required check still running from one that failed", () => {
    expect(
      checkBlockers([check({ name: "build", state: "running" })], ["build"]),
    ).toEqual(["1 required check has not passed yet: build"]);
  });

  it("words a non-required failure without claiming a policy", () => {
    // The server's land rule refuses any change with a failing check, so
    // this is still a blocker — but the list must not invent a
    // requirement the repo never configured.
    expect(checkBlockers([check({ name: "lint", state: "failing" })])).toEqual([
      "1 check failed: lint",
    ]);
  });

  it("takes required from the row as well as from the policy list", () => {
    // The merged route marks rows `required`; the repo policy names them
    // separately. Either is enough.
    expect(
      checkBlockers([
        check({ name: "build", state: "failing", required: true }),
      ]),
    ).toEqual(["1 required check failed: build"]);
  });

  it("counts and names several of each, pluralising as it goes", () => {
    const blockers = checkBlockers(
      [
        check({ name: "build", state: "failing" }),
        check({ name: "test", state: "failing" }),
        check({ name: "lint", state: "failing" }),
        check({ name: "docs", state: "queued" }),
      ],
      ["build", "test", "docs", "audit"],
    );
    expect(blockers).toEqual([
      "2 required checks failed: build, test",
      "1 required check has not reported: audit",
      "1 required check has not passed yet: docs",
      "1 check failed: lint",
    ]);
  });

  it("is empty for a change with no checks and no policy", () => {
    expect(checkBlockers([])).toEqual([]);
  });
});

describe("gateBlockers", () => {
  it("says nothing when the server says ready", () => {
    expect(gateBlockers({ state: "ready" })).toEqual([]);
  });

  it("passes the server's own reason through verbatim", () => {
    const gate: LandGate = {
      state: "blocked",
      reason: "review verdict not met",
    };
    expect(gateBlockers(gate)).toEqual(["review verdict not met"]);
  });

  it("counts the checks it is waiting on and names them", () => {
    // A bare "2 required checks have not passed" leaves the reader to go
    // and find which two, which is the failure a blockers list exists to
    // fix.
    expect(gateBlockers({ state: "waiting", on: ["build", "test"] })).toEqual([
      "2 required checks have not passed: build, test",
    ]);
    expect(gateBlockers({ state: "waiting", on: ["build"] })).toEqual([
      "1 required check has not passed: build",
    ]);
  });

  it("says nothing when it is waiting on nothing", () => {
    // Ready-in-all-but-name. A blocker line with no subject would leave a
    // disabled-looking button explained by an empty sentence.
    expect(gateBlockers({ state: "waiting", on: [] })).toEqual([]);
  });
});

describe("startsExpanded", () => {
  it("expands a short list and collapses a long one", () => {
    expect(startsExpanded(0)).toBe(true);
    expect(startsExpanded(COLLAPSE_ABOVE)).toBe(true);
    expect(startsExpanded(COLLAPSE_ABOVE + 1)).toBe(false);
  });
});

/// The land button's own predicate.
///
/// Split out of `changes.tsx`, where it was `state === "failing"` — the
/// patchset route's single word — and left the control enabled over a
/// commit-scoped red row. The merged read is the reason this can happen
/// at all: one panel now shows both writers' vocabularies.
describe("hasFailingCheck", () => {
  it("catches every word a red check can arrive as", () => {
    for (const state of ["failing", "failure", "cancelled"]) {
      expect(hasFailingCheck([{ state }])).toBe(true);
    }
  });

  it("does not call an unfinished or skipped run red", () => {
    for (const state of ["pending", "queued", "running", "skipped"]) {
      expect(hasFailingCheck([{ state }])).toBe(false);
    }
  });

  it("agrees with the panel: one red among many greens is red", () => {
    expect(
      hasFailingCheck([
        { state: "passing" },
        { state: "success" },
        { state: "cancelled" },
      ]),
    ).toBe(true);
  });

  it("is false for no checks at all, which is not a refusal", () => {
    expect(hasFailingCheck([])).toBe(false);
  });
});

describe("classifyLandVerdict", () => {
  // The regression this exists for: the queue writes "waiting on
  // ci/tests", the page rendered only notes beginning "ejected", and an
  // author watched a spinner under a green checks panel for the full
  // 30-minute wait budget with nothing on screen naming the check. The
  // prefix is the discriminator the control plane owns precisely so this
  // reading is possible; nothing here may key off the change's `state`,
  // which outlives the note in both directions.
  it("reads a waiting note as the names the queue is holding for", () => {
    expect(classifyLandVerdict("waiting on ci/tests")).toEqual({
      kind: "waiting",
      on: ["ci/tests"],
    });
    expect(classifyLandVerdict("waiting on ci/tests, ci/lint")).toEqual({
      kind: "waiting",
      on: ["ci/tests", "ci/lint"],
    });
  });

  it("still reads an ejection, which was the only branch that rendered", () => {
    const v = classifyLandVerdict(
      "ejected: waited 30m0s for ci/tests, which never reported",
    );
    expect(v?.kind).toBe("ejected");
  });

  it("keeps a sentence it did not anticipate rather than dropping it", () => {
    // `land_verdict` is the only account of what the queue did. A note
    // nobody planned for is still worth more on screen than nothing.
    expect(classifyLandVerdict("landed")).toEqual({
      kind: "note",
      text: "landed",
    });
  });

  it("is nothing for a change that has not been through the queue", () => {
    expect(classifyLandVerdict(null)).toBeNull();
    expect(classifyLandVerdict(undefined)).toBeNull();
    expect(classifyLandVerdict("   ")).toBeNull();
  });

  it("does not render a waiting heading over an empty list", () => {
    // A shape the lander does not produce — it only writes the note when
    // it has names — but a "waiting on" heading with nothing under it is
    // worse than the raw sentence.
    expect(classifyLandVerdict("waiting on ")).toEqual({
      kind: "note",
      text: "waiting on",
    });
  });
});

describe("checkBlockers with the required list the server now sends", () => {
  // The other half of the same defect. A required check that never
  // reported has no row to carry a `required` flag, so a caller passing
  // only the rows sees one green check and concludes everything passed —
  // which is exactly what the review page did while the queue held the
  // change. The names have to arrive separately or they cannot arrive.
  it("names a required check that has no row at all", () => {
    const green: PanelCheck[] = [
      {
        name: "ci/local",
        state: "passing",
        detail_url: null,
        required: true,
        source: "commit",
        posted_by: "intake",
      },
    ];
    expect(checkBlockers(green, ["ci/local", "ci/tests"])).toEqual([
      "1 required check has not reported: ci/tests",
    ]);
    // And the bug, stated as a test: with the names dropped, the same
    // rows look entirely clean.
    expect(checkBlockers(green)).toEqual([]);
  });
});

describe("summarize with the branch's required names", () => {
  const green: PanelCheck[] = [
    {
      name: "ci/local",
      state: "passing",
      detail_url: null,
      required: true,
      source: "commit",
      posted_by: "intake",
    },
  ];

  it("does not announce a pass while a required check has never reported", () => {
    // The headline answers "can I stop looking at this page". Over a
    // change the land queue is holding on ci/tests, the answer is no —
    // and this panel said "All checks have passed" for as long as it
    // counted only the rows that existed.
    const s = summarize(green, ["ci/local", "ci/tests"]);
    expect(s.outcome).toBe("pending");
    expect(headline(s)).toBe("Some checks haven't completed yet");
    expect(s.missing).toBe(1);
    expect(countLine(s)).toBe("1 successful, 1 not reported");
  });

  it("still passes when every required name reported green", () => {
    const s = summarize(green, ["ci/local"]);
    expect(s.outcome).toBe("passing");
    expect(s.missing).toBe(0);
    expect(headline(s)).toBe("All checks have passed");
  });

  it("counts a missing required check apart from a running one", () => {
    // Different facts: a running check finishes on its own, a name that
    // has never reported may never do so. The count line keeps them
    // separate so the reader can tell which they are waiting for.
    const running: PanelCheck[] = [
      { ...green[0], name: "ci/slow", state: "running" },
    ];
    const s = summarize(running, ["ci/slow", "ci/tests"]);
    expect(s.pending).toBe(1);
    expect(s.missing).toBe(1);
    expect(countLine(s)).toBe("1 pending, 1 not reported");
  });

  it("is unchanged when no requirements are known", () => {
    // An older server sends no list; the panel must behave exactly as
    // it did rather than inventing a blocker.
    expect(summarize(green).outcome).toBe("passing");
    expect(summarize(green).missing).toBe(0);
  });
});

describe("blocked checks", () => {
  const FORK =
    "this change comes from a fork; a maintainer has to approve its workflows before they run";

  it("counts a blocked row from its refusal, not from its state word", () => {
    // The whole difficulty in one assertion. A blocked workflow run
    // mirrors as `queued` — deliberately, because nothing is wrong with
    // the change — so nothing in `state` distinguishes "about to start"
    // from "will never start unless a person acts". The caller joins the
    // run back on and hands the sentence over; this is what reads it.
    const s = summarize([
      check({ name: "ci.yml", state: "queued", refusal: FORK }),
      check({ name: "test", state: "queued" }),
    ]);
    expect(s.blocked).toBe(1);
  });

  it("keeps a blocked row in the pending count as well", () => {
    // Not a fifth bucket: a blocked check has not completed, so it is
    // pending too, and the four buckets still sum to the total. Making
    // it exclusive would leave `1 pending` beside a two-row list with
    // nothing accounting for the other row.
    const s = summarize([
      check({ name: "ci.yml", state: "queued", refusal: FORK }),
      check({ name: "test", state: "queued" }),
    ]);
    expect(s.pending).toBe(2);
    expect(s.successful + s.failing + s.pending + s.skipped).toBe(s.total);
  });

  it("names the blocked ones in the count line", () => {
    // "2 pending" over a change where one of them is waiting on a person
    // is a number that tells a maintainer to go away and come back
    // later, which is precisely the wrong thing to do.
    const s = summarize([
      check({ name: "ci.yml", state: "queued", refusal: FORK }),
      check({ name: "test", state: "queued" }),
    ]);
    expect(countLine(s)).toBe("2 pending, 1 blocked");
  });

  it("says nothing about blocking when nothing is blocked", () => {
    // A standing "0 blocked" is a number a reader checks every time to
    // learn nothing.
    expect(countLine(summarize([check()]))).toBe("1 successful");
  });
});
