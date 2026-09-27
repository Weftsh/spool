import { describe, expect, it } from "vitest";
import { isPersonalRefusal } from "./newrepo";

/// The two 402s a private create can get share a status and nothing
/// else: a free organization is offered a subscription, a personal
/// namespace is offered an organization. Telling them apart is a prefix
/// match on the server's sentence, and this pins the exact words so a
/// rewording server-side fails here rather than showing "Subscribe" on
/// a namespace that has nothing to subscribe to.
describe("isPersonalRefusal", () => {
  it("recognises the personal-namespace sentence, whole or prefixed", () => {
    expect(
      isPersonalRefusal(
        "quota: private repositories live in an organization — create one from the dashboard",
      ),
    ).toBe(true);
    expect(
      isPersonalRefusal("quota: private repositories live in an organization"),
    ).toBe(true);
  });

  it("does not claim the free-organization refusal", () => {
    expect(
      isPersonalRefusal(
        "quota: private repositories need a paid plan — subscribe from the billing page",
      ),
    ).toBe(false);
    expect(isPersonalRefusal("this organization has no card on file")).toBe(
      false,
    );
    // The sentence somewhere inside a different one is not the refusal.
    expect(
      isPersonalRefusal(
        "not quota: private repositories live in an organization",
      ),
    ).toBe(false);
  });
});
