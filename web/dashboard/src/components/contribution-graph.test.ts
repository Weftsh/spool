import { describe, expect, it } from "vitest";
import {
  STEPS,
  cellLabel,
  cells,
  heatColor,
  heatStep,
  isoOfDay,
  monthSpans,
  weeks,
  type Cell,
} from "./contribution-graph";
import type { ContributionGraph } from "@/api";

const graph = (
  from: string,
  to: string,
  days: ContributionGraph["days"],
  extra?: Partial<ContributionGraph>,
): ContributionGraph => ({
  from,
  to,
  total: days.reduce((n, d) => n + d.count, 0),
  private_included: false,
  days,
  ...extra,
});

const day = (
  date: string,
  count: number,
  repos: Cell["repos"] = [],
): ContributionGraph["days"][number] => ({
  day: Math.round(Date.parse(`${date}T00:00:00Z`) / 86_400_000),
  date,
  count,
  repos,
});

describe("heatStep", () => {
  // Every pair below was read off github.com, not remembered. The ramp
  // is relative to the busiest day in the same graph, which is the
  // decision that keeps a modest year from rendering as one flat pale
  // wash — and ours will be modest more often than GitHub's, because we
  // count verified commit authorship where they also count issues,
  // pull requests and reviews.
  it("reproduces GitHub's ramp on a sparse year", () => {
    // github.com/mojombo, 2024: 34 contributions, busiest day 7.
    const max = 7;
    const observed: [number, number][] = [
      [1, 1],
      [2, 2],
      [3, 2],
      [4, 3],
      [7, 4],
    ];
    for (const [count, level] of observed) {
      expect(heatStep(count, max), `${count} of ${max}`).toBe(level);
    }
  });

  it("reproduces GitHub's ramp on a dense year", () => {
    // github.com/dtolnay, 2024: 4,984 contributions, busiest day 100.
    // The boundary sits between 25 and 26, which a fixed ramp cannot
    // produce at all.
    const max = 100;
    for (const [count, level] of [
      [1, 1],
      [25, 1],
      [26, 2],
      [50, 2],
      [51, 3],
      [75, 3],
      [76, 4],
      [100, 4],
    ] as [number, number][]) {
      expect(heatStep(count, max), `${count} of ${max}`).toBe(level);
    }
  });

  it("never renders a day's work as an empty square", () => {
    // Whatever the ceiling, a day with something on it must be visible:
    // an invisible contribution is worse than an imprecise one.
    expect(heatStep(1, 1)).toBe(4);
    expect(heatStep(1, 10_000)).toBe(1);
    expect(heatStep(1, 0)).toBe(1);
    // And a day with nothing is the empty step, never off the ramp.
    expect(heatStep(0, 50)).toBe(0);
    expect(heatStep(-1, 50)).toBe(0);
    expect(heatStep(0, 0)).toBe(0);
    // A count above the ceiling clamps rather than running past it.
    expect(heatStep(500, 100)).toBe(STEPS - 1);
  });
});

describe("heatColor", () => {
  it("mixes one token family and never reaches for the accent", () => {
    // `--accent` is a counterpoint hue and cannot carry a sequential
    // ramp; a heat scale whose third step reads dimmer than its second
    // is a scale that reads backwards.
    const all = Array.from({ length: STEPS }, (_, i) => heatColor(i));
    for (const c of all.slice(1)) {
      expect(c).toContain("var(--series-1)");
      expect(c).not.toContain("--accent");
    }
    // No hex reaches a component (DESIGN.md), at any step.
    for (const c of all) expect(c).not.toMatch(/#[0-9a-f]{3,8}/i);
    // An empty day is the bare well, so it reads as absence rather than
    // as a very faint value.
    expect(heatColor(0)).toBe("var(--surface-2)");
    // The steps are strictly increasing in saturation — the property
    // that makes the ramp a ramp.
    const pcts = all.slice(1).map((c) => Number(/ (\d+)%/.exec(c)![1]));
    expect(pcts).toEqual([...pcts].sort((a, b) => a - b));
    expect(new Set(pcts).size).toBe(pcts.length);
    // A step past the end clamps rather than producing `undefined%`.
    expect(heatColor(99)).toBe(heatColor(STEPS - 1));
  });
});

describe("cells", () => {
  it("fills the days the server deliberately does not send", () => {
    // The server sends only days with something on them; sending 365
    // zeroes would be sending nothing 365 times.
    const out = cells(
      graph("2024-02-27", "2024-03-02", [day("2024-02-29", 4)]),
    );
    expect(out.map((c) => c.date)).toEqual([
      "2024-02-27",
      "2024-02-28",
      "2024-02-29",
      "2024-03-01",
      "2024-03-02",
    ]);
    expect(out.map((c) => c.count)).toEqual([0, 0, 4, 0, 0]);
  });

  it("crosses a daylight-saving boundary without gaining or losing a day", () => {
    // The bug this exists for: date arithmetic across a DST boundary
    // grows or drops a column once a year, in one hemisphere, for two
    // weeks. Counting in day numbers cannot.
    const out = cells(graph("2024-03-08", "2024-03-13", []));
    expect(out).toHaveLength(6);
    expect(out.map((c) => c.date)).toEqual([
      "2024-03-08",
      "2024-03-09",
      "2024-03-10",
      "2024-03-11",
      "2024-03-12",
      "2024-03-13",
    ]);
  });

  it("round-trips a day number through the date the server sends", () => {
    for (const d of [0, 19782, 19783, -1, 25000]) {
      const iso = isoOfDay(d);
      expect(Math.round(Date.parse(`${iso}T00:00:00Z`) / 86_400_000)).toBe(d);
    }
    expect(isoOfDay(19782)).toBe("2024-02-29");
  });
});

describe("weeks", () => {
  it("breaks columns on Sunday and leaves the first one short", () => {
    // 2024-02-27 is a Tuesday. Padding the first column with fake empty
    // days would draw squares for dates outside the window, which a
    // reader would hover and be told about.
    const out = weeks(cells(graph("2024-02-27", "2024-03-09", [])));
    expect(out[0].map((c) => c.date)).toEqual([
      "2024-02-27",
      "2024-02-28",
      "2024-02-29",
      "2024-03-01",
      "2024-03-02",
    ]);
    expect(out[1][0].date).toBe("2024-03-03");
    expect(out[1]).toHaveLength(7);
    expect(out).toHaveLength(2);
  });

  it("starts a full column when the window begins on a Sunday", () => {
    const out = weeks(cells(graph("2024-03-03", "2024-03-16", [])));
    expect(out).toHaveLength(2);
    expect(out.every((w) => w.length === 7)).toBe(true);
  });
});

describe("monthSpans", () => {
  it("gives each month exactly the columns it owns", () => {
    // A full calendar year: twelve labels, and their spans sum to the
    // number of week columns.
    const columns = weeks(cells(graph("2024-01-01", "2024-12-28", [])));
    const spans = monthSpans(columns);
    expect(spans.reduce((n, m) => n + m.span, 0)).toBe(columns.length);
    const named = spans.filter((m) => m.label).map((m) => m.label);
    expect(named).toEqual([
      "Jan",
      "Feb",
      "Mar",
      "Apr",
      "May",
      "Jun",
      "Jul",
      "Aug",
      "Sep",
      "Oct",
      "Nov",
      "Dec",
    ]);
  });

  it("leaves a one-column sliver at the edge unlabelled", () => {
    // A window starting mid-week in one month and ending in the next
    // would otherwise put two names three pixels apart.
    const columns = weeks(cells(graph("2024-02-28", "2024-03-16", [])));
    const spans = monthSpans(columns);
    expect(spans[0]).toEqual({ label: "", span: 1 });
    expect(spans.filter((m) => m.label).map((m) => m.label)).toEqual(["Mar"]);
  });
});

describe("cellLabel", () => {
  it("never leaves a square to speak by colour alone", () => {
    expect(cellLabel({ date: "2024-03-01", day: 0, count: 0, repos: [] })).toBe(
      "No contributions on 2024-03-01",
    );
    expect(
      cellLabel({
        date: "2024-03-01",
        day: 0,
        count: 1,
        repos: [{ org: "acme", name: "widget", count: 1 }],
      }),
    ).toBe("1 contribution on 2024-03-01 — acme/widget");
  });

  it("names private work as private work and never as a repository", () => {
    // The whole contract in one string: the count includes it, and the
    // only thing said about it is that it is private. "and 2 more"
    // would invite a reader to guess.
    const label = cellLabel({
      date: "2024-03-01",
      day: 0,
      count: 3,
      repos: [{ org: "acme", name: "widget", count: 1 }],
    });
    expect(label).toBe(
      "3 contributions on 2024-03-01 — acme/widget, private work",
    );
    // A day that is only private names no repository at all.
    expect(cellLabel({ date: "2024-03-02", day: 0, count: 2, repos: [] })).toBe(
      "2 contributions on 2024-03-02 — private work",
    );
    // ...and a day whose repositories account for the whole count says
    // nothing about private work, because there is none to say.
    expect(
      cellLabel({
        date: "2024-03-03",
        day: 0,
        count: 2,
        repos: [{ org: "acme", name: "widget", count: 2 }],
      }),
    ).toBe("2 contributions on 2024-03-03 — acme/widget");
  });
});
