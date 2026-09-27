import { describe, expect, it } from "vitest";
import { languageColor } from "./language-dot";

describe("languageColor", () => {
  it("assigns the series family in fixed order, never a per-language hue", () => {
    expect(languageColor(0)).toBe("var(--series-1)");
    expect(languageColor(1)).toBe("var(--series-2)");
    expect(languageColor(2)).toBe("var(--series-3)");
  });

  it("drops everything past third into the muted 'Other' slot", () => {
    // DESIGN.md caps a view at two accent hues; a twelve-language bar
    // in twelve colours is exactly what that rule exists to stop.
    expect(languageColor(3)).toBe("var(--text-muted)");
    expect(languageColor(200)).toBe("var(--text-muted)");
  });

  it("never emits anything but a token, whatever it is handed", () => {
    // The rank arrives from a response body, so a negative or
    // fractional one is a real input, not a hypothetical — and the
    // return value goes straight into a `style` attribute.
    expect(languageColor(-1)).toBe("var(--text-muted)");
    expect(languageColor(1.5)).toBe("var(--text-muted)");
    expect(languageColor(NaN)).toBe("var(--text-muted)");
  });
});
