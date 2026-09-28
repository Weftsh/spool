// The sign-in callback's outcomes, each with a sentence and none of
// them silent. `ok` is a success and is not this component's — a
// banner congratulating somebody for signing in is noise. GitHub never
// makes an account here, so there is no `new` any more, and no
// `emailtaken` either: both belonged to the sign-up that is gone.

import { describe, expect, it } from "vitest";

import { githubOutcomeLine, githubOutcomeOf } from "./github-signin-banner";

describe("githubOutcomeOf", () => {
  it("names every refusal the callback can answer, and nothing else", () => {
    for (const o of [
      "noaccount",
      "denied",
      "expired",
      "noemail",
      "disabled",
      "unavailable",
      "error",
    ]) {
      expect(githubOutcomeOf(o)).toBe(o);
    }
    // The success carries no banner.
    expect(githubOutcomeOf("ok")).toBeNull();
    // Outcomes of the sign-up that no longer exists. A stale link that
    // still carries one must not draw a sentence about a flow nobody
    // can reach — least of all "a different account here", which
    // describes an account GitHub can no longer make.
    expect(githubOutcomeOf("new")).toBeNull();
    expect(githubOutcomeOf("emailtaken")).toBeNull();
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
    expect(githubOutcomeLine("noemail")).toMatch(
      /email address and password still works/,
    );
    expect(githubOutcomeLine("disabled")).toMatch(/disabled/);
    expect(githubOutcomeLine("unavailable")).toMatch(/not set up/);
    expect(githubOutcomeLine("error")).toMatch(/try again/i);
  });

  it("sends somebody GitHub knows and this server does not to an invitation", () => {
    const line = githubOutcomeLine("noaccount");
    expect(line).toMatch(/nobody on this server is you/);
    // The only way in for a person with no account, and who to ask.
    expect(line).toMatch(/made by invitation/);
    expect(line).toMatch(/admin of your organization to invite you/);
    // An account under another address is the other reason GitHub
    // finds nobody, and the password path is its way in.
    expect(line).toMatch(/sign in with that address and its password/);
  });

  it("never offers a way to make an account that does not exist", () => {
    // There is no sign-up, so no refusal may send a person to one. The
    // old `noemail` sentence ended "or sign up with an email address
    // and password instead" — a door that is now a 404.
    for (const o of [
      "noaccount",
      "denied",
      "expired",
      "noemail",
      "disabled",
      "unavailable",
      "error",
    ] as const) {
      expect(githubOutcomeLine(o)).not.toMatch(
        /sign(ing)? ?up|create an account|register/i,
      );
      // The server's product name is not the dashboard's to guess: say
      // "this server", which is true of every deployment.
      expect(githubOutcomeLine(o)).not.toMatch(/Weft/);
    }
  });
});
