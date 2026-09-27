import { describe, expect, it } from "vitest";
import { STATES, type State } from "./state-icon";

// The table is the contract: every state carries a word and a shape,
// so that none of them is ever read by colour alone. These assertions
// are about that property, not about which icon is prettier.
describe("the state table", () => {
  const entries = Object.entries(STATES) as [State, (typeof STATES)[State]][];

  it("gives every state a non-empty accessible label", () => {
    for (const [state, meta] of entries) {
      expect(meta.label.trim(), `${state} has no label`).not.toBe("");
    }
  });

  it("gives every state a distinct glyph within its family", () => {
    // Open-vs-closed is drawn on the red/green axis, which is the axis
    // 8% of men cannot resolve — so two states in one family sharing a
    // shape would leave colour as the only difference.
    for (const family of ["issue", "check"]) {
      const icons = entries
        .filter(([state]) => state.startsWith(`${family}-`))
        .map(([, meta]) => meta.icon);
      expect(icons.length).toBeGreaterThan(1);
      expect(new Set(icons).size, `${family} reuses a glyph`).toBe(
        icons.length,
      );
    }
  });

  it("colours states with tokens only — no hex in a component", () => {
    for (const [state, meta] of entries) {
      expect(meta.className, `${state} is not a token class`).toMatch(
        /^text-(good|warning|serious|brand|ink-3)$/,
      );
    }
  });

  it("describes no pull request — a change lands, it is not merged", () => {
    // The table once carried GitHub's pr-open/draft/merged/closed states.
    // Nothing drew them, and the vocabulary was another product's.
    for (const [state] of entries) {
      expect(state).not.toMatch(/^pr-/);
    }
  });
});
