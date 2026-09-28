// What the dashboard makes of `GET /v1/auth/methods` — including when
// the answer is not that endpoint's at all. The failure this guards is a
// sign-in screen with nothing on it: an older server, a proxy page or a
// half-shaped body must read as today's screen, the password form and
// the GitHub button.

import { describe, expect, it } from "vitest";

import {
  authMethodsOf,
  FALLBACK_METHODS,
  loadAuthMethods,
  noAccountLine,
  passwordsOffLine,
  SSO_START,
} from "./auth-methods";

describe("authMethodsOf", () => {
  it("reads the server's answer as it says it", () => {
    expect(
      authMethodsOf({
        password: false,
        github: false,
        sso: { name: "Okta", start: "/v1/auth/sso/start" },
      }),
    ).toEqual({
      password: false,
      github: false,
      sso: { name: "Okta", start: "/v1/auth/sso/start" },
    });
    expect(authMethodsOf({ password: true, github: false, sso: null })).toEqual(
      { password: true, github: false, sso: null },
    );
  });

  it("falls back to what every server offered before, for a body that is not this endpoint's", () => {
    // What the fallback is: password and GitHub on, no SSO. Stated
    // rather than compared to the constant, so changing the constant is
    // a decision this test has to be told about.
    expect(FALLBACK_METHODS).toEqual({ password: true, github: true, sso: null });
    for (const body of [
      undefined,
      null,
      "<!doctype html>",
      42,
      [],
      {},
      { error: "not found" },
      // Half a shape is somebody else's answer, not ours with a gap.
      { password: false },
      { github: false },
      { password: "false", github: false },
      { password: false, github: 0, sso: { name: "Okta", start: "/x" } },
    ]) {
      expect(authMethodsOf(body), JSON.stringify(body)).toEqual(
        FALLBACK_METHODS,
      );
    }
  });

  it("never turns a failure into a screen with no way to sign in", () => {
    // The whole point of the fallback, said as the property: whatever
    // came back, a person is offered at least one way in.
    for (const body of [undefined, {}, { password: 1, github: 1 }]) {
      const m = authMethodsOf(body);
      expect(m.password || m.github || m.sso !== null).toBe(true);
    }
  });

  it("reads a missing or malformed sso block as not configured", () => {
    for (const sso of [undefined, null, false, "Okta", 1, []]) {
      expect(authMethodsOf({ password: true, github: true, sso }).sso).toBeNull();
    }
  });

  it("calls a provider the operator did not name what the server calls it", () => {
    for (const name of [undefined, "", "   ", 7]) {
      expect(
        authMethodsOf({ password: false, github: false, sso: { name, start: "/s" } })
          .sso?.name,
      ).toBe("SSO");
    }
    expect(
      authMethodsOf({ password: false, github: false, sso: { name: "  Entra ID ", start: "/s" } })
        .sso?.name,
    ).toBe("Entra ID");
  });

  it("only ever links to a path on this origin", () => {
    // The start link is an href. Anything that is not one of our own
    // paths goes to the server's own start route instead.
    for (const start of [
      "javascript:alert(1)",
      "//evil.example/start",
      "/\\evil.example",
      "https://evil.example/start",
      "v1/auth/sso/start",
      undefined,
      42,
    ]) {
      expect(
        authMethodsOf({ password: false, github: false, sso: { name: "Okta", start } })
          .sso?.start,
        String(start),
      ).toBe(SSO_START);
    }
    expect(
      authMethodsOf({ password: false, github: false, sso: { name: "Okta", start: "/v1/auth/sso/start?x=1" } })
        .sso?.start,
    ).toBe("/v1/auth/sso/start?x=1");
  });
});

describe("loadAuthMethods", () => {
  it("answers today's screen when the request fails, rather than failing", async () => {
    // An older server's 404, a network that dropped the request: the
    // transport rejects, and the sign-in screen must still be drawn.
    await expect(
      loadAuthMethods(() => Promise.reject(new Error("404"))),
    ).resolves.toEqual(FALLBACK_METHODS);
  });

  it("reads an answer through the same rules as everything else", async () => {
    await expect(
      loadAuthMethods(() =>
        Promise.resolve({
          password: false,
          github: false,
          sso: { name: "Okta", start: "javascript:alert(1)" },
        }),
      ),
    ).resolves.toEqual({
      password: false,
      github: false,
      sso: { name: "Okta", start: SSO_START },
    });
  });
});

describe("noAccountLine", () => {
  it("with single sign-on, says the first sign-in makes the account", () => {
    const line = noAccountLine({
      password: false,
      github: false,
      sso: { name: "Okta", start: SSO_START },
    });
    expect(line).toBe(
      "No account yet? Sign in with Okta — your account is made the first time.",
    );
  });

  it("says the same beside a password form: the provider makes accounts either way", () => {
    expect(
      noAccountLine({ password: true, github: true, sso: { name: "Okta", start: SSO_START } }),
    ).toMatch(/Sign in with Okta — your account is made the first time/);
  });

  it("without it, says accounts come by invitation, as it always has", () => {
    const line = noAccountLine(FALLBACK_METHODS);
    expect(line).toMatch(/^No account yet\? Accounts on this server are made by invitation\./);
    expect(line).toMatch(/admin of your organization/);
    expect(line).toMatch(/whoever runs this server/);
  });
});

describe("passwordsOffLine", () => {
  it("names the way in there is", () => {
    expect(
      passwordsOffLine({ password: false, github: false, sso: { name: "Okta", start: SSO_START } }),
    ).toBe("This server signs in with Okta");
    expect(passwordsOffLine({ password: false, github: true, sso: null })).toBe(
      "This server does not sign in with passwords",
    );
  });
});
