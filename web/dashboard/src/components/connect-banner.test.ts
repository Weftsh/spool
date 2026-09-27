// The install callback's outcomes, each with a sentence and none of
// them silent. `ok` is not a problem and is not this component's.

import { describe, expect, it } from "vitest";

import { connectOutcomeLine, connectOutcomeOf } from "./connect-banner";

describe("connectOutcomeOf", () => {
  it("names every outcome the callback can answer, and nothing else", () => {
    for (const o of [
      "notyours",
      "expired",
      "missing",
      "which",
      "taken",
      "error",
    ]) {
      expect(connectOutcomeOf(o)).toBe(o);
    }
    expect(connectOutcomeOf("ok")).toBeNull();
    // `claim` is not a problem either: the parked-installation card
    // handles it, and a warning banner over it would say something went
    // wrong when nothing did.
    expect(connectOutcomeOf("claim")).toBeNull();
    expect(connectOutcomeOf(null)).toBeNull();
    expect(connectOutcomeOf("")).toBeNull();
    expect(connectOutcomeOf("nonsense")).toBeNull();
  });

  it("gives each outcome a sentence that says what to do next", () => {
    expect(connectOutcomeLine("notyours")).toMatch(/installation is yours/);
    expect(connectOutcomeLine("expired")).toMatch(/Start again/);
    expect(connectOutcomeLine("missing")).toMatch(/which organization/);
    expect(connectOutcomeLine("which")).toMatch(/more than one organization/);
    expect(connectOutcomeLine("taken")).toMatch(/another organization/);
    expect(connectOutcomeLine("error")).toMatch(/try again/i);
  });
});
