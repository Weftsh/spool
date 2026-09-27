import { describe, expect, it } from "vitest";
import { countLabel } from "./count-button";

describe("countLabel", () => {
  it("renders zero as a zero", () => {
    // FORGE-UX §1.2: the counts on the masthead are always visible,
    // "including zero". A young project's honest 0 and a control that
    // decided not to show you a number are different facts, and a
    // reader scanning for whether anybody is here can act on the first
    // and learns nothing from the second. Blank, "—" and a hidden cap
    // all say the second thing.
    expect(countLabel(0)).toBe("0");
  });

  it("compacts a large count rather than widening the row", () => {
    // Three controls sit side by side in a fixed-width identity row. A
    // literal 60300 in one cap pushes the other two, which is the
    // reflow this component exists to stop.
    expect(countLabel(999)).toBe("999");
    expect(countLabel(1200)).toBe("1.2k");
    expect(countLabel(60300)).toBe("60k");
    expect(countLabel(2_400_000)).toBe("2.4M");
  });

  it("admits it does not know rather than printing NaN", () => {
    // A non-count reaching here is a bug upstream, and `NaN` beside a
    // star glyph is the one rendering worse than saying nothing. It is
    // also the case an `?? 0` in the caller would silently turn into a
    // confident, wrong zero.
    expect(countLabel(Number.NaN)).toBe("–");
    expect(countLabel(Number.POSITIVE_INFINITY)).toBe("–");
  });

  it("refuses a negative count", () => {
    // No count on this row can be below zero. `-1` here means a
    // subtraction underflowed somewhere, and rendering it would put a
    // minus sign on a repository page rather than reporting a defect.
    expect(countLabel(-1)).toBe("–");
  });
});
