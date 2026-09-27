// The card offers only organizations the person can actually bind an
// installation to; the server refuses the rest, and a select that
// offers a choice the server will refuse is a dead end with a button.

import { describe, expect, it } from "vitest";

import { claimableOrgs } from "./claim-install";

describe("claimableOrgs", () => {
  it("keeps owners and admins, in order, and drops members and viewers", () => {
    const orgs = [
      { name: "acme", role: "viewer" as const },
      { name: "beta", role: "owner" as const },
      { name: "gamma", role: "member" as const },
      { name: "delta", role: "admin" as const },
    ];
    expect(claimableOrgs(orgs as never).map((o) => o.name)).toEqual([
      "beta",
      "delta",
    ]);
  });

  it("is empty for a person with no organization to administer", () => {
    expect(claimableOrgs([])).toEqual([]);
    expect(
      claimableOrgs([{ name: "acme", role: "member" }] as never),
    ).toEqual([]);
  });
});
