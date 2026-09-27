import { describe, expect, it } from "vitest";
import type { WorkflowRun } from "@/api";
import {
  UNEXPLAINED_BLOCK,
  approvableRuns,
  blockReason,
  blockedRuns,
  panelRowRefusal,
  refusalsByFile,
  refusalsByRunId,
  rowRefusal,
} from "./workflow-runs";

/// The join that puts a refusal's reason back on the row it belongs to.
/// Every case here is one a reader hits: a fork waiting on approval, a
/// run refused for some other reason, and — the ones that make the join
/// worth writing rather than assuming — a stale run from the previous
/// patchset, and a third party's row wearing one of our names.

const FORK =
  "this change comes from a fork; a maintainer has to approve its workflows before they run";
/// A refusal that is not a fork's, under a code this bundle has never
/// heard of — the shape any reason other than `fork` arrives in.
const OTHER =
  "workflows are paused for this repository by an organization administrator";

function run(over: Partial<WorkflowRun> = {}): WorkflowRun {
  return {
    id: "wr1",
    file: ".weft/ci.yml",
    name: "CI",
    commit_sha: "a".repeat(40),
    ref_name: "main",
    event: "change",
    change_key: "I1",
    state: "blocked",
    error: FORK,
    created_at: 1,
    updated_at: 2,
    completed_at: null,
    jobs: [],
    ...over,
  };
}

describe("blockReason", () => {
  it("gives the server's sentence back unchanged", () => {
    // Verbatim, deliberately: these strings are the product's answer to
    // "why has nothing run", they are asserted character for character
    // by the walkthrough, and a paraphrase here would be the page
    // inventing a reason the server never gave.
    expect(blockReason({ error: OTHER })).toBe(OTHER);
  });

  it("never renders a blocked row with nothing beside it", () => {
    // A blank reason recreates the exact defect this module removes: a
    // row that says it is stuck and does not say why.
    expect(blockReason({ error: null })).toBe(UNEXPLAINED_BLOCK);
    expect(blockReason({ error: "   " })).toBe(UNEXPLAINED_BLOCK);
  });

});

describe("blockedRuns", () => {
  it("takes only the blocked ones", () => {
    const runs = [run(), run({ id: "wr2", state: "running", error: null })];
    expect(blockedRuns(runs).map((r) => r.id)).toEqual(["wr1"]);
  });

  it("scopes to one commit when asked", () => {
    // Approval is per tip. A run blocked against the patchset before
    // this one is a question somebody already answered, and letting it
    // through would put an "Approve and run" button on a page whose
    // current tip has nothing blocked — which the server then refuses
    // with a 409 the reader was given no warning of.
    const tip = "b".repeat(40);
    const runs = [run(), run({ id: "wr2", commit_sha: tip })];
    expect(blockedRuns(runs, tip).map((r) => r.id)).toEqual(["wr2"]);
    expect(blockedRuns(runs, "c".repeat(40))).toEqual([]);
  });
});

describe("rowRefusal", () => {
  const byId = refusalsByRunId([run(), run({ id: "wr2", error: OTHER })]);

  it("annotates a workflow run's row with its run's reason", () => {
    expect(rowRefusal({ provider: "weft", external_id: "wr2" }, byId)).toBe(
      OTHER,
    );
  });

  it("refuses to lend a reason to another provider's row", () => {
    // `external_id` is the provider's own id for its own run, so two
    // providers can collide on one string. Without the provider gate a
    // third party's row would display our refusal as its own.
    expect(rowRefusal({ provider: "intake", external_id: "wr2" }, byId)).toBe(
      null,
    );
  });

  it("says nothing about a row with no id, or a run that is not blocked", () => {
    expect(rowRefusal({ provider: "weft", external_id: null }, byId)).toBe(
      null,
    );
    expect(
      rowRefusal({ provider: "weft", external_id: "job-9" }, byId),
    ).toBe(null);
  });
});

describe("panelRowRefusal", () => {
  const tip = "b".repeat(40);
  const byFile = refusalsByFile(
    [
      run({ commit_sha: tip }),
      run({ id: "wr2", file: ".weft/old.yml", error: OTHER }),
    ],
    tip,
  );

  it("matches the change panel's row by workflow file", () => {
    // The change route does not expose `external_id`, and the mirror
    // names a refusal row for the file it refused — so the name *is*
    // the key on this side.
    expect(
      panelRowRefusal(
        { name: ".weft/ci.yml", source: "commit", posted_by: "weft" },
        byFile,
      ),
    ).toBe(FORK);
  });

  it("leaves a run from an earlier patchset out of it", () => {
    // `wr2` is blocked, but against a different commit; scoping the map
    // to the tip is what keeps its reason off this page.
    expect(
      panelRowRefusal(
        { name: ".weft/old.yml", source: "commit", posted_by: "weft" },
        byFile,
      ),
    ).toBe(null);
  });

  it("refuses a row somebody posted through the intake", () => {
    // A reporter chooses the name it posts under, so it can name itself
    // after one of our workflow files. `posted_by` is the server's word
    // for who wrote the row, not the reporter's word for itself.
    expect(
      panelRowRefusal(
        { name: ".weft/ci.yml", source: "commit", posted_by: "acme-ci" },
        byFile,
      ),
    ).toBe(null);
    expect(
      panelRowRefusal(
        { name: ".weft/ci.yml", source: "patchset", posted_by: "weft" },
        byFile,
      ),
    ).toBe(null);
  });
});

/// Which refusals a person can actually do something about.
///
/// Every blocked run is one word on the wire. A fork's run is waiting on
/// a maintainer, and a button is the whole answer. A run blocked for any
/// other reason is waiting on something the reader cannot press, and
/// approving it only triggers another run that blocks with the same
/// sentence — a button there is a control that cannot work.
describe("approvableRuns", () => {
  const TIP = "a".repeat(40);
  const ELSEWHERE = "b".repeat(40);
  const forked = run({ blocked_reason: "fork" });
  const paused = run({
    id: "wr2",
    blocked_reason: "paused",
    error: OTHER,
  });

  it("offers a fork's runs and nothing else", () => {
    expect(approvableRuns([forked, paused], TIP).map((r) => r.id)).toEqual([
      "wr1",
    ]);
  });

  it("offers nothing for a run blocked for another reason", () => {
    // Not "offers the fork run anyway": a fork change blocked for some
    // other reason carries *that* reason, so there is no fork-reasoned
    // run to find, and the panel must be silent about approving while
    // still showing why.
    expect(approvableRuns([paused], TIP)).toEqual([]);
  });

  it("treats a run with no reason code as a fork's, for an older server", () => {
    // The field is being added; a bundle that shipped before it must not
    // stop offering approval to every fork on the fleet. Absent is the
    // status quo, and the status quo is the fork case — the only one
    // that had a button at all.
    const { blocked_reason: _drop, ...older } = forked;
    expect(approvableRuns([older], TIP).map((r) => r.id)).toEqual(["wr1"]);
  });

  it("still scopes to the tip", () => {
    expect(
      approvableRuns([{ ...forked, commit_sha: ELSEWHERE }], TIP),
    ).toEqual([]);
  });

  it("ignores a reason code on a run that is not blocked", () => {
    // A stale code on a run that has since started is not a refusal.
    expect(
      approvableRuns([{ ...forked, state: "running", error: null }], TIP),
    ).toEqual([]);
  });
});

/// Why the `external_id` join needs no commit filter, asserted rather
/// than asserted-in-a-comment.
///
/// The worry is reasonable: the change panel's join *is* scoped to a
/// commit, because it matches on a workflow file's path and a path
/// repeats on every patchset. This one matches on the run's own id, and
/// the mirror upserts check rows on `(repo_id, provider, external_id)`
/// — one row per run, forever — so an id names exactly one row and the
/// question "which commit was that" cannot arise. Filtering by commit
/// here would be worse than useless: the Checks tab lists many commits
/// at once, so it would silence every row that is not the newest.
describe("refusalsByRunId across commits", () => {
  it("annotates each commit's own row and no other", () => {
    const older = run({ id: "wr0", commit_sha: "b".repeat(40), error: OTHER });
    const newer = run({ id: "wr1", commit_sha: "a".repeat(40), error: FORK });
    const by = refusalsByRunId([older, newer]);
    expect(rowRefusal({ provider: "weft", external_id: "wr0" }, by)).toBe(
      OTHER,
    );
    expect(rowRefusal({ provider: "weft", external_id: "wr1" }, by)).toBe(
      FORK,
    );
  });

  it("drops the annotation as soon as the run stops being blocked", () => {
    // The stale-reason case that a commit filter is sometimes reached
    // for: a run that has since been approved keeps its id and its
    // row, so what has to stop the old sentence is the run's *state*,
    // and it does.
    const approved = run({ state: "running", error: null });
    expect(
      rowRefusal(
        { provider: "weft", external_id: "wr1" },
        refusalsByRunId([approved]),
      ),
    ).toBe(null);
  });
});
