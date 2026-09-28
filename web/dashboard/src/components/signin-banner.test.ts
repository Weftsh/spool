// The sign-in callback's outcomes, each with a sentence and none of
// them silent. `ok` is a success and is not this component's — a
// banner congratulating somebody for signing in is noise. GitHub never
// makes an account here, so there is no `new` any more, and no
// `emailtaken` either: both belonged to the sign-up that is gone.

import { describe, expect, it } from "vitest";

import { type AuthMethods } from "@/lib/auth-methods";

import {
  githubOutcomeLine,
  githubOutcomeOf,
  signinOutcomeLine,
  signinOutcomeOf,
  ssoOutcomeLine,
  ssoOutcomeOf,
  withoutSigninOutcome,
} from "./signin-banner";

const SSO_ONLY: AuthMethods = {
  password: false,
  github: false,
  sso: { name: "Okta", start: "/v1/auth/sso/start" },
};
const SSO_BESIDE_PASSWORD: AuthMethods = { ...SSO_ONLY, password: true, github: true };
const NO_SSO: AuthMethods = { password: true, github: true, sso: null };

const SSO_REFUSALS = [
  "denied",
  "expired",
  "noemail",
  "domain",
  "disabled",
  "unavailable",
  "error",
] as const;

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

describe("ssoOutcomeOf", () => {
  it("names every refusal the identity provider's callback can answer, and nothing else", () => {
    for (const o of SSO_REFUSALS) expect(ssoOutcomeOf(o)).toBe(o);
    // A success lands signed in and grows no banner.
    expect(ssoOutcomeOf("ok")).toBeNull();
    // GitHub's own refusal: the identity provider makes accounts, so
    // it never answers that nobody here is you.
    expect(ssoOutcomeOf("noaccount")).toBeNull();
    expect(ssoOutcomeOf(null)).toBeNull();
    expect(ssoOutcomeOf("")).toBeNull();
    expect(ssoOutcomeOf("nonsense")).toBeNull();
  });
});

describe("ssoOutcomeLine", () => {
  it("gives each refusal its verdict and its next step, in the operator's name for the provider", () => {
    const line = (o: (typeof SSO_REFUSALS)[number]) =>
      ssoOutcomeLine(o, SSO_ONLY);
    expect(line("denied")).toMatch(/cancelled signing in with Okta/);
    expect(line("denied")).toMatch(/Press Continue with Okta to try again/);
    expect(line("expired")).toMatch(
      /not finished in the browser that started it, or it took too long/,
    );
    expect(line("expired")).toMatch(/Continue with Okta to start again/);
    expect(line("noemail")).toMatch(/Okta did not give this server an email address it can trust/);
    expect(line("noemail")).toMatch(/verified address, or to allow your address's domain/);
    expect(line("domain")).toMatch(/not in a domain this server admits/);
    expect(line("domain")).toMatch(/ask whoever runs this server/);
    expect(line("disabled")).toMatch(/has been disabled/);
    expect(line("disabled")).toMatch(/Ask whoever runs this server to re-enable it/);
    expect(line("unavailable")).toMatch(/Single sign-on is not set up on this server/);
    expect(line("error")).toMatch(/Okta did not answer, or something failed on this server/);
    expect(line("error")).toMatch(/tell whoever runs this server if it keeps happening/);
  });

  it("says every refusal left the person signed out, and never names the product", () => {
    for (const o of SSO_REFUSALS) {
      const line = ssoOutcomeLine(o, SSO_ONLY);
      expect(line).toMatch(/nothing was signed in/);
      expect(line).not.toMatch(/Weft/);
      // The identity provider makes accounts, so no refusal sends
      // somebody to a sign-up, and none to an invitation either.
      expect(line).not.toMatch(/sign(ing)? ?up|create an account|register|invitation/i);
    }
  });

  it("says `unavailable` without a label, and points at a way in that is really there", () => {
    // The server answers `unavailable` when SSO is not configured —
    // which is exactly when `/v1/auth/methods` has no name to give.
    expect(ssoOutcomeLine("unavailable", NO_SSO)).not.toMatch(/Okta|SSO\b/);
    expect(ssoOutcomeLine("unavailable", NO_SSO)).toMatch(
      /email address and password instead/,
    );
    expect(
      ssoOutcomeLine("unavailable", { password: false, github: true, sso: null }),
    ).toMatch(/Continue with GitHub instead/);
    expect(
      ssoOutcomeLine("unavailable", { password: false, github: false, sso: null }),
    ).toMatch(/Ask whoever runs this server how to sign in/);
  });

  it("calls a provider nobody named what the server calls it", () => {
    expect(ssoOutcomeLine("denied", NO_SSO)).toMatch(/Continue with SSO/);
  });
});

describe("a GitHub sign-in that finds nobody, beside single sign-on", () => {
  it("sends the person to the identity provider that makes accounts, not to an invitation", () => {
    const line = githubOutcomeLine("noaccount", SSO_BESIDE_PASSWORD);
    expect(line).toMatch(/nobody on this server is you/);
    expect(line).toMatch(/Continue with Okta instead — your account is made the first time/);
    expect(line).not.toMatch(/invitation/);
    // The password path is still true here, so it is still offered.
    expect(line).toMatch(/sign in with that address and its password/);
  });

  it("offers no password where the server takes none", () => {
    const line = githubOutcomeLine("noaccount", SSO_ONLY);
    expect(line).toMatch(/Continue with Okta instead/);
    expect(line).not.toMatch(/password/);
  });

  it("is unchanged without single sign-on", () => {
    expect(githubOutcomeLine("noaccount", NO_SSO)).toBe(
      githubOutcomeLine("noaccount"),
    );
    expect(githubOutcomeLine("noaccount", NO_SSO)).toMatch(/made by invitation/);
  });
});

describe("signinOutcomeOf", () => {
  it("reads either provider's refusal off the address bar", () => {
    expect(signinOutcomeOf(new URLSearchParams("sso=domain"))).toEqual({
      via: "sso",
      outcome: "domain",
    });
    expect(signinOutcomeOf(new URLSearchParams("github=denied"))).toEqual({
      via: "github",
      outcome: "denied",
    });
    expect(signinOutcomeOf(new URLSearchParams("sso=ok"))).toBeNull();
    expect(signinOutcomeOf(new URLSearchParams("github=ok"))).toBeNull();
    expect(signinOutcomeOf(new URLSearchParams(""))).toBeNull();
    // Each provider's words are its own: GitHub's `noaccount` is not an
    // SSO outcome, and SSO's `domain` is not GitHub's.
    expect(signinOutcomeOf(new URLSearchParams("sso=noaccount"))).toBeNull();
    expect(signinOutcomeOf(new URLSearchParams("github=domain"))).toBeNull();
  });

  it("says the sentence of the provider the person actually went through", () => {
    expect(
      signinOutcomeLine({ via: "sso", outcome: "denied" }, SSO_ONLY),
    ).toMatch(/Okta/);
    expect(
      signinOutcomeLine({ via: "github", outcome: "denied" }, SSO_ONLY),
    ).toMatch(/cancelled at GitHub/);
  });
});

describe("withoutSigninOutcome", () => {
  it("takes out every sign-in outcome, whatever its value", () => {
    for (const v of ["ok", "denied", "nonsense", ""]) {
      expect(withoutSigninOutcome(`?sso=${v}`)).toBe("");
      expect(withoutSigninOutcome(`?github=${v}`)).toBe("");
    }
  });

  it("leaves everything else the address carries exactly as it was", () => {
    // `connect` and `org` belong to the install round trip, which reads
    // them after this has run; `next` is where signing in returns to.
    expect(withoutSigninOutcome("?connect=claim&sso=denied")).toBe(
      "?connect=claim",
    );
    expect(withoutSigninOutcome("?next=%2Facme%2Fwidget&github=error")).toBe(
      "?next=%2Facme%2Fwidget",
    );
    // Nothing to take out: the very string that came in, so the caller
    // can tell that nothing needs rewriting.
    expect(withoutSigninOutcome("?connect=ok&org=acme")).toBe(
      "?connect=ok&org=acme",
    );
    expect(withoutSigninOutcome("")).toBe("");
  });
});
