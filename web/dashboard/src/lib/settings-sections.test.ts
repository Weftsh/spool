import { describe, expect, it } from "vitest";
import { type Me } from "@/api";
import { settingsSections } from "./settings-sections";

function person(role: string, org = "acme"): Me {
  return {
    orgs: [{ id: "01org", name: org, role }],
  } as unknown as Me;
}

const slugs = (me: Me | null, org = "acme") =>
  settingsSections(me, org).map((s) => s.slug);

describe("settingsSections", () => {
  it("gives an owner every section, admin ones first", () => {
    expect(slugs(person("owner"))).toEqual([
      "members",
      "billing",
      "runners",
      "teams",
      "packages",
      "activity",
      "tokens",
      "ssh-keys",
      "emails",
      "password",
    ]);
    expect(slugs(person("admin"))).toEqual(slugs(person("owner")));
  });

  // Packages is visible to everybody, unlike Members, Billing and
  // Runners. A member is the person who publishes and installs, and the
  // screen is where the `.npmrc` snippet lives; switching an ecosystem
  // on is the admin-only part, and the panel and the server both gate
  // that rather than hiding the whole page.
  it("hides Members and Billing from members and viewers", () => {
    for (const role of ["member", "viewer"]) {
      expect(slugs(person(role))).toEqual([
        "teams",
        "packages",
        "activity",
        "tokens",
        "ssh-keys",
        "emails",
        "password",
      ]);
    }
  });

  it("treats a token session as admin, with no Password page", () => {
    // me === null: the credential's own scopes gate the API, and there
    // is no account whose password could be changed.
    // Neither Password nor Email addresses: both belong to a person,
    // and a token session has none. This is the assertion that catches
    // a new person-only section added without the `me` guard the
    // others have.
    expect(slugs(null)).toEqual([
      "members",
      "billing",
      "runners",
      "teams",
      "packages",
      "activity",
      "tokens",
      "ssh-keys",
    ]);
  });

  it("a person outside the org gets the unprivileged set", () => {
    expect(slugs(person("owner", "elsewhere"), "acme")).toEqual([
      "teams",
      "packages",
      "activity",
      "tokens",
      "ssh-keys",
      "emails",
      "password",
    ]);
  });

  it("never returns an empty list — the redirect target always exists", () => {
    expect(slugs(null).length).toBeGreaterThan(0);
    expect(slugs(person("viewer")).length).toBeGreaterThan(0);
  });

  it("files each section under whose it is, the organization's first", () => {
    // The rail renders one group per value, in this order. Billing and
    // Password used to share one "Settings" label.
    const groups = Object.fromEntries(
      settingsSections(person("owner"), "acme").map((s) => [s.slug, s.group]),
    );
    expect(groups).toEqual({
      members: "org",
      billing: "org",
      runners: "org",
      teams: "org",
      packages: "org",
      activity: "org",
      tokens: "account",
      "ssh-keys": "account",
      emails: "account",
      password: "account",
    });
    for (const me of [person("owner"), person("viewer"), null]) {
      const order = settingsSections(me, "acme").map((s) => s.group);
      expect(order.lastIndexOf("org")).toBeLessThan(order.indexOf("account"));
    }
  });
});
