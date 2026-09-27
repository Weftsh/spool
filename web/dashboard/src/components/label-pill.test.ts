import { describe, expect, it } from "vitest";
import { hueDot, labelDot } from "./label-pill";

// A label's colour is imported data — it comes from whatever the
// project had on GitHub — and it ends up in a `style` attribute. So the
// interesting cases here are not the pretty ones.
describe("hueDot", () => {
  it("mixes a hex toward the muted text colour rather than using it raw", () => {
    // 70% is the number that keeps two labels distinguishable without
    // letting a saturated import out-shout the emerald.
    expect(hueDot("#d73a4a")).toBe(
      "color-mix(in oklab, #d73a4a 70%, var(--text-muted))",
    );
  });

  it("accepts the shorthand and a bare hex, as imports actually arrive", () => {
    expect(hueDot("#0e8")).toBe(
      "color-mix(in oklab, #0e8 70%, var(--text-muted))",
    );
    expect(hueDot("D73A4A")).toBe(
      "color-mix(in oklab, #D73A4A 70%, var(--text-muted))",
    );
    expect(hueDot("  #0e8  ")).toBe(
      "color-mix(in oklab, #0e8 70%, var(--text-muted))",
    );
  });

  it("renders no dot rather than somebody else's CSS", () => {
    // Anything that is not a hex is refused outright: an imported
    // label whose "colour" is a declaration would otherwise be a
    // style-injection point on every issue row that carries it.
    expect(hueDot("red; background: url(https://evil/)")).toBeUndefined();
    expect(hueDot("var(--brand)")).toBeUndefined();
    expect(hueDot("#12345")).toBeUndefined();
    expect(hueDot("#gggggg")).toBeUndefined();
    expect(hueDot("")).toBeUndefined();
    expect(hueDot(null)).toBeUndefined();
    expect(hueDot(undefined)).toBeUndefined();
  });
});

// The seven names `labels.color` is allowed to hold, and — the case
// that matters more — everything else. The server refuses an unknown
// name at the write, so an unknown one reaching the component means the
// palette grew and this bundle is older than the data. That is somebody
// else's deploy, and it must not produce a blank issue list.
describe("labelDot", () => {
  const PALETTE = [
    "series-1",
    "series-2",
    "series-3",
    "status-good",
    "status-warning",
    "status-serious",
    "neutral",
  ];

  it("resolves each of the seven to a token, and to a different one", () => {
    // Both halves. "Every name resolves" alone would pass against a
    // table that mapped all seven to the same grey, which is a palette
    // with one colour in it.
    for (const name of PALETTE) {
      expect(labelDot(name), name).toMatch(/^var\(--[a-z0-9-]+\)$/);
    }
    expect(new Set(PALETTE.map(labelDot)).size).toBe(PALETTE.length);
  });

  it("renders neutral from an ink token, not from var(--neutral)", () => {
    // `neutral` is a sentinel, not a token: there is no `--neutral` in
    // `web/shared/tokens.css`. Interpolating the stored name would
    // produce `var(--neutral)`, which resolves to nothing and draws an
    // invisible dot — a pill that reads as a rendering bug.
    expect(labelDot("neutral")).toBe("var(--text-muted)");
    expect(labelDot("neutral")).not.toBe("var(--neutral)");
  });

  it("degrades an unknown name to a readable pill rather than to nothing", () => {
    // A version skew, not bad data. Every one of these still has to
    // produce something a dot can be painted with.
    for (const unknown of ["series-9", "brand", "chartreuse", "", "  "]) {
      expect(labelDot(unknown), unknown).toBe("var(--text-muted)");
    }
    expect(labelDot(null)).toBe("var(--text-muted)");
    expect(labelDot(undefined)).toBe("var(--text-muted)");
  });

  it("never returns an empty value for any input at all", () => {
    // The property behind "a readable pill": whatever arrives, the
    // style attribute gets something. An empty string would collapse
    // the dot and read as a missing colour.
    for (const weird of ["", " ", "\n", "0", "null", "undefined", "#"]) {
      expect(labelDot(weird).length, JSON.stringify(weird)).toBeGreaterThan(0);
    }
  });

  it("matches exactly and case-sensitively, as the server does", () => {
    // `SERIES-1` is refused at the write, so accepting it here would
    // mean the UI could render a label the database can never hold.
    expect(labelDot("SERIES-1")).toBe("var(--text-muted)");
    expect(labelDot("Status-Good")).toBe("var(--text-muted)");
    expect(labelDot(" series-1 ")).toBe("var(--series-1)"); // trimmed, not case-folded
  });

  it("does not walk Object.prototype for a colour", () => {
    // A plain `Record` would answer `constructor` with a function and
    // `toString` with a method, and put it in a style attribute. This is
    // why the table is a Map.
    for (const key of ["constructor", "toString", "__proto__", "valueOf"]) {
      expect(labelDot(key), key).toBe("var(--text-muted)");
    }
  });

  it("still mutes an imported project's hex", () => {
    // The importer is out of this slice, but the path stays: a hex is
    // unfixable later — change the palette and every stored hex is
    // silently wrong in both themes, with no migration that can know
    // what the author meant. A token name re-resolves.
    expect(labelDot("#d73a4a")).toBe(
      "color-mix(in oklab, #d73a4a 70%, var(--text-muted))",
    );
  });

  it("never lets an unvalidated value through into the style attribute", () => {
    // The whole reason the fallback is safe: nothing that is not a
    // known name or a hex is ever echoed back.
    const hostile = "red; background: url(https://evil/)";
    expect(labelDot(hostile)).toBe("var(--text-muted)");
    expect(labelDot(hostile)).not.toContain("evil");
  });
});
