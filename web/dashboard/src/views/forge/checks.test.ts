import { describe, expect, it } from "vitest";
import type { CheckRun, ChecksPoll, RunState } from "@/api";
import {
  NO_FILTER,
  RUN_STATES,
  distinct,
  emptyState,
  formatDuration,
  formatRunQuery,
  nextCursor,
  parseRunQuery,
  runDuration,
  runQueryParams,
  retryLine,
  runStateLabel,
  runStatePresentation,
  runSubtitle,
  shortSha,
  toApiQuery,
  type RunFilter,
} from "./checks";

// The Checks tab's contract is mostly arithmetic and string handling, and
// every one of the interesting cases is a way the page could state
// something it does not know: a duration for a run that never started, a
// "Load more" that never ends, a verdict word invented for a state the
// server added after this bundle shipped, or an empty list standing in
// for "we are not allowed to look".

const parse = (qs: string) => parseRunQuery(new URLSearchParams(qs));
const filter = (over: Partial<RunFilter> = {}): RunFilter => ({
  ...NO_FILTER,
  ...over,
});

function run(over: Partial<CheckRun> = {}): CheckRun {
  return {
    id: "r1",
    repo_id: "repo1",
    commit_sha: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
    ref_name: "main",
    provider: "github",
    external_id: "42",
    name: "build",
    run_number: 42,
    event: "push",
    state: "passing",
    detail_url: "https://example.invalid/run/42",
    actor: "ada",
    started_at: null,
    completed_at: null,
    created_at: 1_000,
    updated_at: 1_000,
    ...over,
  };
}

describe("parseRunQuery / formatRunQuery", () => {
  it("round-trips every filter it can hold", () => {
    // Parse-then-format-then-parse being the identity is the whole
    // contract of putting filters in the URL: if the two disagree
    // anywhere, a dropdown and the address it produced describe
    // different lists, and the link somebody sends is not the list they
    // were looking at.
    const cases: RunFilter[] = [
      NO_FILTER,
      filter({ state: "failing" }),
      filter({ workflow: "build", branch: "main" }),
      filter({
        workflow: "release (arm64)",
        branch: "release/2.0",
        state: "passing",
        event: "pull_request",
        actor: "ada",
      }),
    ];
    for (const f of cases) {
      expect(parse(formatRunQuery(f))).toEqual(f);
    }
  });

  it("survives the characters a branch or workflow name really contains", () => {
    const f = filter({ workflow: "test / e2e", branch: "fix/#123 & more" });
    expect(parse(formatRunQuery(f))).toEqual(f);
  });

  it("formats no filter as an empty query string", () => {
    expect(formatRunQuery(NO_FILTER)).toBe("");
  });

  it("spells the keys in a stable order", () => {
    // So that the same filter is always the same URL — two links to the
    // same list that differ by key order are two links a person cannot
    // tell apart.
    expect(
      formatRunQuery(
        filter({
          actor: "ada",
          state: "failing",
          workflow: "build",
          branch: "main",
          event: "push",
        }),
      ),
    ).toBe("workflow=build&branch=main&state=failing&event=push&actor=ada");
  });

  it("reads a missing parameter as absent", () => {
    expect(parse("")).toEqual(NO_FILTER);
  });

  it("reads a whitespace-only parameter as absent, not as a branch named space", () => {
    expect(parse("branch=%20%20&actor=+")).toEqual(NO_FILTER);
  });

  it("trims a parameter rather than sending the spaces", () => {
    expect(parse("actor=%20ada%20").actor).toBe("ada");
  });

  it("keeps a state word it does not recognise", () => {
    // The server refuses an unknown state by name, and that refusal is
    // the useful answer: silently dropping the filter shows a list of
    // rows that do not match what was asked for, which reads as the
    // filter being broken rather than as the word being wrong.
    expect(parse("state=success").state).toBe("success");
  });
});

describe("runQueryParams", () => {
  it("omits the filters that are not set", () => {
    expect(runQueryParams(filter({ state: "failing" }))).toEqual({
      workflow: undefined,
      branch: undefined,
      state: "failing",
      event: undefined,
      actor: undefined,
    });
  });
});

describe("toApiQuery", () => {
  it("always asks for a page size and never for a cursor it does not have", () => {
    expect(toApiQuery(NO_FILTER, { limit: 30 })).toEqual({ limit: 30 });
  });

  it("passes every set filter through", () => {
    expect(
      toApiQuery(
        filter({
          workflow: "build",
          branch: "main",
          state: "failing",
          event: "push",
          actor: "ada",
        }),
        { limit: 30, before: 99 },
      ),
    ).toEqual({
      limit: 30,
      workflow: "build",
      branch: "main",
      state: "failing",
      event: "push",
      actor: "ada",
      before: 99,
    });
  });

  it("sends a cursor of zero rather than dropping it as falsy", () => {
    // `before: 0` is a real instant. A truthiness check here would ask
    // for the first page forever and "Load more" would loop.
    expect(toApiQuery(NO_FILTER, { limit: 30, before: 0 }).before).toBe(0);
  });
});

describe("runStateLabel", () => {
  const states: RunState[] = [
    "queued",
    "running",
    "passing",
    "failing",
    "cancelled",
    "skipped",
  ];

  it("has a word for all six states and no seventh", () => {
    expect(Object.keys(RUN_STATES).sort()).toEqual([...states].sort());
    expect(states.map(runStateLabel)).toEqual([
      "Queued",
      "Running",
      "Passing",
      "Failing",
      "Cancelled",
      "Skipped",
    ]);
  });

  it("gives every state a distinct glyph, so state is never colour alone", () => {
    // The rule that matters most on this page: pass/fail is drawn on the
    // red/green axis, which is exactly the axis a red-green colourblind
    // reader cannot resolve. Two states sharing an icon would leave the
    // tone as the only difference.
    const icons = states.map((s) => RUN_STATES[s].icon);
    expect(new Set(icons).size).toBe(states.length);
  });

  it("never colours cancelled or skipped as a failure", () => {
    // Neither is a failure. One is a build a person stopped and the
    // other one a provider decided did not apply, and painting a
    // docs-only change red is how a green project looks broken.
    expect(RUN_STATES.cancelled.className).not.toContain("serious");
    expect(RUN_STATES.skipped.className).not.toContain("serious");
  });

  it("says an unknown state's own word rather than guessing at one", () => {
    // A seventh state means the server is newer than this bundle.
    // Quietly green ships broken code and quietly red blocks good code;
    // saying the unknown word leaves a trace of not knowing.
    expect(runStateLabel("success")).toBe("success");
    const shown = runStatePresentation("success");
    expect(shown.label).toBe("success");
    expect(shown.className).not.toContain("good");
    expect(shown.className).not.toContain("serious");
  });

  it("presents a known state with its own glyph and tone", () => {
    expect(runStatePresentation("failing")).toEqual(RUN_STATES.failing);
  });
});

describe("shortSha", () => {
  it("keeps the seven characters a person reads", () => {
    expect(shortSha("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678")).toBe(
      "a1b2c3d",
    );
  });

  it("leaves a sha that is already short alone", () => {
    // A helper that threw or padded here would turn one odd row into a
    // blank tab.
    expect(shortSha("abc")).toBe("abc");
    expect(shortSha("")).toBe("");
  });
});

describe("runDuration", () => {
  const NOW = 100_000;

  it("measures a finished run between its own timestamps", () => {
    expect(
      runDuration({ started_at: 10_000, completed_at: 70_000 }, NOW),
    ).toEqual({ ms: 60_000, running: false });
  });

  it("measures a started-but-unfinished run against now, and says so", () => {
    expect(
      runDuration({ started_at: 40_000, completed_at: null }, NOW),
    ).toEqual({ ms: 60_000, running: true });
  });

  it("answers nothing for a run that never started", () => {
    // Not zero. `0s` beside a queued run reads as an instant pass.
    expect(runDuration({ started_at: null, completed_at: null }, NOW)).toBe(
      null,
    );
  });

  it("answers nothing for a run with an end and no beginning", () => {
    expect(runDuration({ started_at: null, completed_at: 70_000 }, NOW)).toBe(
      null,
    );
  });

  it("answers nothing when the clocks disagree", () => {
    // These timestamps come off somebody else's build machine. A
    // negative duration is a clock disagreement, and rendering `-3s`
    // invites a bug report about our arithmetic.
    expect(runDuration({ started_at: 70_000, completed_at: 10_000 }, NOW)).toBe(
      null,
    );
    expect(
      runDuration({ started_at: NOW + 5_000, completed_at: null }, NOW),
    ).toBe(null);
  });

  it("calls an instant run zero rather than nothing", () => {
    expect(
      runDuration({ started_at: 10_000, completed_at: 10_000 }, NOW),
    ).toEqual({ ms: 0, running: false });
  });
});

describe("formatDuration", () => {
  it("uses seconds under a minute", () => {
    expect(formatDuration(0)).toBe("0s");
    expect(formatDuration(45_000)).toBe("45s");
    expect(formatDuration(59_999)).toBe("59s");
  });

  it("uses minutes and seconds under an hour", () => {
    expect(formatDuration(60_000)).toBe("1m 0s");
    expect(formatDuration(90_000)).toBe("1m 30s");
    expect(formatDuration(3_599_000)).toBe("59m 59s");
  });

  it("uses hours and minutes above one", () => {
    expect(formatDuration(3_600_000)).toBe("1h 0m");
    expect(formatDuration(5_400_000)).toBe("1h 30m");
  });
});

describe("runSubtitle", () => {
  it("says the run number, the commit, the event and the actor", () => {
    expect(runSubtitle(run())).toBe("#42: Commit a1b2c3d push by ada");
  });

  it("drops a run number the provider does not give", () => {
    // Not `#null`. A provider that does not number its runs has not told
    // us a number, and inventing one is inventing a fact about somebody
    // else's build.
    expect(runSubtitle(run({ run_number: null }))).toBe(
      "Commit a1b2c3d push by ada",
    );
  });

  it("drops an absent event", () => {
    expect(runSubtitle(run({ event: null }))).toBe(
      "#42: Commit a1b2c3d by ada",
    );
  });

  it("drops an absent actor", () => {
    expect(runSubtitle(run({ actor: null }))).toBe("#42: Commit a1b2c3d push");
  });

  it("falls back to the commit alone when that is all there is", () => {
    expect(
      runSubtitle(run({ run_number: null, event: null, actor: null })),
    ).toBe("Commit a1b2c3d");
  });
});

describe("nextCursor", () => {
  const page = (n: number, next: number | null) => ({
    runs: Array.from({ length: n }, (_, i) => i),
    next_before: next,
  });

  it("stops on a short page whatever the cursor says", () => {
    // Trusting `next_before` alone leaves "Load more" on screen forever
    // at the end of a feed, and every press answers with nothing.
    expect(nextCursor(page(12, 900), 30)).toBe(null);
  });

  it("stops on an empty page", () => {
    expect(nextCursor(page(0, 900), 30)).toBe(null);
  });

  it("carries the cursor on a full page", () => {
    expect(nextCursor(page(30, 900), 30)).toBe(900);
  });

  it("stops on a full page the server gave no cursor for", () => {
    expect(nextCursor(page(30, null), 30)).toBe(null);
  });
});

describe("distinct", () => {
  it("collects the values on this page, deduplicated and sorted", () => {
    const runs = [
      run({ id: "a", actor: "ada" }),
      run({ id: "b", actor: "bob" }),
      run({ id: "c", actor: "ada" }),
    ];
    expect(distinct(runs, "actor")).toEqual(["ada", "bob"]);
  });

  it("drops the runs that carry nothing for that field", () => {
    const runs = [
      run({ id: "a", ref_name: null }),
      run({ id: "b", ref_name: "main" }),
    ];
    // An option with no value would be an item Radix refuses and a
    // filter that means "the runs with no branch", which is not a thing
    // the API can be asked for.
    expect(distinct(runs, "ref_name")).toEqual(["main"]);
  });

  it("returns nothing for an empty page", () => {
    expect(distinct([], "event")).toEqual([]);
  });
});

function poll(over: Partial<ChecksPoll> = {}): ChecksPoll {
  return {
    provider: "github",
    connected: true,
    polled: true,
    denied: false,
    error: null,
    high_water: null,
    resuming_from: null,
    retry_in_ms: null,
    ...over,
  };
}

describe("emptyState", () => {
  it("says we may not look, never that there is no CI", () => {
    // The one CI failure that renders identically to success. For a
    // mirror whose Actions run on every push, "no runs" is false — and
    // false in the direction that makes a migrating maintainer close
    // the tab.
    const s = emptyState(poll({ denied: true }));
    expect(s.kind).toBe("denied");
    expect(s.title).toMatch(/cannot read/i);
    expect(s.body).toMatch(/permission to read Actions/);
    expect(s.body).toMatch(/not a project without CI/i);
    expect(s.reapprove).toBe(true);
    // Telling somebody to post to the intake when the real answer is
    // "grant the permission" answers a question they did not ask.
    expect(s.intake).toBe(false);
  });

  it("says we have not looked yet before the first poll finishes", () => {
    const s = emptyState(poll({ polled: false }));
    expect(s.kind).toBe("waiting");
    expect(s.body).toMatch(/have not looked/i);
    expect(s.intake).toBe(false);
    expect(s.reapprove).toBe(false);
  });

  it("points at the intake for a repository with no GitHub origin", () => {
    // The ordinary case for a native repository, and the only one where
    // "point your CI at the intake" is the next step rather than a
    // non-sequitur.
    const s = emptyState(poll({ connected: false }));
    expect(s.kind).toBe("intake");
    expect(s.intake).toBe(true);
    expect(s.reapprove).toBe(false);
  });

  it("distinguishes a completed poll that found nothing", () => {
    const s = emptyState(poll());
    expect(s.kind).toBe("polled");
    expect(s.body).toMatch(/no runs to report/i);
    expect(s.intake).toBe(true);
    expect(s.reapprove).toBe(false);
  });

  it("prefers `denied` over every other reason", () => {
    // A denied installation has usually never completed a poll either.
    // Reporting that as "looking for runs" would leave somebody waiting
    // for something that will never arrive.
    expect(emptyState(poll({ denied: true, polled: false })).kind).toBe(
      "denied",
    );
  });

  it("treats a repository with no origin as the intake case even when it never polled", () => {
    expect(emptyState(poll({ connected: false, polled: false })).kind).toBe(
      "intake",
    );
  });

  it("admits it does not know when the poll status could not be read", () => {
    // Every one of the four sentences would be an assertion this page
    // cannot support, so it makes none of them.
    const s = emptyState(null);
    expect(s.kind).toBe("unknown");
    expect(s.body).toMatch(/not a statement about whether the project has CI/i);
    expect(s.reapprove).toBe(false);
  });

  it("offers the re-approve control in exactly one state", () => {
    const all = [
      emptyState(null),
      emptyState(poll({ connected: false })),
      emptyState(poll({ polled: false })),
      emptyState(poll()),
      emptyState(poll({ denied: true })),
    ];
    expect(all.filter((s) => s.reapprove).map((s) => s.kind)).toEqual([
      "denied",
    ]);
  });

  it("gives every state a title and a body", () => {
    for (const s of [
      emptyState(null),
      emptyState(poll({ connected: false })),
      emptyState(poll({ polled: false })),
      emptyState(poll()),
      emptyState(poll({ denied: true })),
    ]) {
      expect(s.title.length).toBeGreaterThan(0);
      expect(s.body.length).toBeGreaterThan(0);
    }
  });
});

describe("retryLine", () => {
  it("says when the next attempt is", () => {
    expect(retryLine(300_000)).toBe("Retrying in 5m 0s");
  });

  it("says nothing when the server gave no answer", () => {
    expect(retryLine(null)).toBe(null);
  });

  it("says nothing for an attempt already due", () => {
    // A countdown reading "Retrying in 0s" that then sits there claims a
    // precision about somebody else's scheduler this page does not have.
    expect(retryLine(0)).toBe(null);
    expect(retryLine(-1_000)).toBe(null);
  });
});
