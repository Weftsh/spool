// The sign-in callback's outcomes, each with a sentence and none of
// them silent. `ok` and `new` are successes and are not this
// component's — a banner congratulating somebody for signing in is
// noise, and `new` has a screen of its own to go to.

import { describe, expect, it } from "vitest";

import { githubOutcomeLine, githubOutcomeOf } from "./github-signin-banner";

describe("githubOutcomeOf", () => {
  it("names every refusal the callback can answer, and nothing else", () => {
    for (const o of [
      "denied",
      "expired",
      "noemail",
      "emailtaken",
      "disabled",
      "unavailable",
      "error",
    ]) {
      expect(githubOutcomeOf(o)).toBe(o);
    }
    // The two successes carry no banner.
    expect(githubOutcomeOf("ok")).toBeNull();
    expect(githubOutcomeOf("new")).toBeNull();
    expect(githubOutcomeOf(null)).toBeNull();
    expect(githubOutcomeOf("")).toBeNull();
    expect(githubOutcomeOf("nonsense")).toBeNull();
  });

  it("gives each outcome a sentence that says what to do next", () => {
    // Every one of these lands on a signed-out screen, so every one of
    // them has to leave a way in — usually the password path, which
    // still works whatever GitHub said.
    expect(githubOutcomeLine("denied")).toMatch(/cancelled at GitHub/);
    expect(githubOutcomeLine("expired")).toMatch(/Continue with GitHub/);
    expect(githubOutcomeLine("noemail")).toMatch(/confirmed/);
    expect(githubOutcomeLine("noemail")).toMatch(/email address and password/);
    expect(githubOutcomeLine("emailtaken")).toMatch(/different account/);
    expect(githubOutcomeLine("disabled")).toMatch(/disabled/);
    expect(githubOutcomeLine("unavailable")).toMatch(/not set up/);
    expect(githubOutcomeLine("error")).toMatch(/try again/i);
  });
});
