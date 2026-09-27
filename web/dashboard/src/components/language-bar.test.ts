import { describe, expect, it } from "vitest";
import { languageBar, percent, OTHER } from "./language-bar";
import { languageColor } from "./language-dot";

const langs = (...pairs: [string, number][]) =>
  pairs.map(([name, bytes]) => ({ name, bytes }));

describe("languageBar", () => {
  it("keeps three languages and aggregates the rest", () => {
    // Three colours in the series family, so three named segments and
    // one summary. A fourth named language would need a fourth hue,
    // which means either a hex in a component or two languages sharing
    // a colour — both worse than aggregating.
    const out = languageBar(
      langs(
        ["Rust", 500],
        ["Go", 300],
        ["Python", 100],
        ["Shell", 60],
        ["SQL", 40],
      ),
    );
    expect(out.map((s) => s.name)).toEqual(["Rust", "Go", "Python", OTHER]);
    expect(out[3].bytes).toBe(100);
    expect(out[3].share).toBeCloseTo(0.1);
  });

  it("gives every segment the colour its rank earns", () => {
    const out = languageBar(
      langs(["Rust", 4], ["Go", 3], ["Python", 2], ["Zig", 1]),
    );
    expect(out.map((s) => languageColor(s.rank))).toEqual([
      "var(--series-1)",
      "var(--series-2)",
      "var(--series-3)",
      // Other is the muted ink, not a fourth hue.
      "var(--text-muted)",
    ]);
  });

  it("draws no Other segment when there is nothing left over", () => {
    // A repository with exactly three languages must not grow a fourth
    // empty segment — a zero-width band with a label reads as a bug.
    const out = languageBar(langs(["Rust", 2], ["Go", 1], ["C", 1]));
    expect(out.map((s) => s.name)).toEqual(["Rust", "Go", "C"]);
    const one = languageBar(langs(["Rust", 1]));
    expect(one).toHaveLength(1);
    expect(one[0].share).toBe(1);
  });

  it("draws nothing at all rather than a bar of one colour", () => {
    // The failure this prevents: dividing by a zero total yields NaN
    // widths, and the "safe" fix of defaulting to 1 yields a full-width
    // band that reads as "100% of something" over an empty repository.
    expect(languageBar([])).toEqual([]);
    expect(languageBar(langs(["Rust", 0]))).toEqual([]);
    expect(languageBar(langs(["Rust", 0], ["Go", 0]))).toEqual([]);
  });

  it("does not draw a language with no bytes in it", () => {
    const out = languageBar(langs(["Rust", 10], ["Go", 0]));
    expect(out.map((s) => s.name)).toEqual(["Rust"]);
  });

  it("shares sum to the whole", () => {
    const out = languageBar(
      langs(["Rust", 7], ["Go", 5], ["C", 3], ["Zig", 2], ["Nim", 1]),
    );
    expect(out.reduce((n, s) => n + s.share, 0)).toBeCloseTo(1, 10);
  });
});

describe("percent", () => {
  it("renders one decimal place", () => {
    expect(percent(1)).toBe("100.0%");
    expect(percent(0.6234)).toBe("62.3%");
    expect(percent(0)).toBe("0.0%");
  });

  it("never labels a drawn segment as zero", () => {
    // A segment that is drawn and labelled 0.0% looks like a rendering
    // fault. It is small, not absent, and the label should say so.
    expect(percent(0.0001)).toBe("<0.1%");
    expect(percent(0.0004999)).toBe("<0.1%");
    // And the boundary is a real rounding, not the escape hatch.
    expect(percent(0.0006)).toBe("0.1%");
  });
});
