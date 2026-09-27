import { describe, expect, it } from "vitest";
import { cn } from "./utils";

// The two behaviors every call site leans on: falsy branches vanish, and
// a later conflicting utility wins (so className overrides on a variant
// actually override).
describe("cn", () => {
  it("drops falsy values from conditional classes", () => {
    expect(cn("px-3", false && "hidden", undefined, null, "text-sm")).toBe(
      "px-3 text-sm",
    );
  });

  it("lets the later conflicting utility win", () => {
    expect(cn("px-3 text-sm", "px-4")).toBe("text-sm px-4");
  });

  it("cannot classify a bare var() shadow, so it does not merge it", () => {
    // tailwind-merge can't tell shadow-[var(--x)] apart from a shadow
    // *color*, so both classes survive — an override would be ambiguous.
    // Callers who need an overridable shadow must use the labeled form
    // below. Pinned so nobody "fixes" a double-shadow by guessing.
    expect(cn("shadow-[var(--shadow-1)]", "shadow-none")).toBe(
      "shadow-[var(--shadow-1)] shadow-none",
    );
  });

  it("merges the labeled arbitrary form like any other utility", () => {
    expect(cn("shadow-[shadow:var(--shadow-1)]", "shadow-none")).toBe(
      "shadow-none",
    );
  });
});
