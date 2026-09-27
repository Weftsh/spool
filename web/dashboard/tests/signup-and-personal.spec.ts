// The two doors the marketing site opens, and the rule a personal
// namespace lives under.
//
// `/login?mode=signup` is where weft.sh's "Sign up free" lands: a button
// that says sign up must open a form that says create an account, not
// the sign-in form with a small link under it. And a personal namespace
// holds public repositories only — private ones live in an organization
// with a subscription — so the new-repository form there starts public,
// says where private goes, and when the server refuses a private create
// with the personal-namespace sentence the answer is "create an
// organization", never "subscribe": a personal namespace has nothing to
// subscribe to, and the paywall's subscribe call would 402 in turn.

import { expect, test } from "@playwright/test";

import { ME, mockApi } from "./fixtures";

/// The signed-out forge: every API call refused, `me` says nobody is
/// here. Registered first so the specific routes below win.
async function signedOut(page: import("@playwright/test").Page) {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
}

test("/login?mode=signup opens on the create-account form", async ({
  page,
}) => {
  await signedOut(page);
  await page.goto("/login?mode=signup");
  // The tab is the product's name. It read "Stratum Dashboard" for a
  // while after everything else was renamed.
  await expect(page).toHaveTitle("Weft");
  // The sign-up form's own fields, not the sign-in form's link to it.
  await expect(page.getByLabel("Your name")).toBeVisible();
  await expect(page.getByLabel("Namespace")).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Create account", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toHaveCount(0);
  // The way back is the ordinary toggle, so nobody is trapped in
  // sign-up by the address they arrived at.
  await page.getByRole("button", { name: "Sign in instead" }).click();
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();
  await expect(page.getByLabel("Your name")).toHaveCount(0);
});

test("/login without a known mode is sign-in, and keeps its return address", async ({
  page,
}) => {
  await signedOut(page);
  // A typo in a link lands on the ordinary door rather than on nothing.
  await page.goto("/login?mode=register&next=%2Facme%2Fwidget");
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();
  await expect(page.getByLabel("Your name")).toHaveCount(0);

  // Signing in from a mode-carrying address still returns to `next`:
  // the mode and the return address are independent.
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await page.route("**/v1/auth/login", (r) => r.fulfill({ json: ME }));
  await page.getByLabel("Email").fill("owner@acme.test");
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(page).toHaveURL(/\/acme\/widget$/);
});

/// Free and nothing bought yet, the shape `dashboard.spec.ts`
/// uses for the same case.
const FREE_BILLING = {
  org: "acme",
  plan: "free",
  billable_seats: 1,
  paid_seats: 0,
  status: null,
  current_period_end: null,
  may_create_public: true,
  may_create_private: false,
  may_add_people: true,
  price_per_seat_cents: 400,
  paid_minutes_per_seat: 2000,
  free_minutes: 500,
  ci_minutes_limit: 500,
  ci_minutes_used: 0,
  ci_minutes_remaining: 500,
  ci_suspended_reason: null,
  ci_suspended_at: null,
};

/// The signed-in person whose current namespace is their own. `me`
/// names the handle, and the personal membership is listed first so it
/// is the org a fresh session lands in — exactly what a person who has
/// just signed up sees.
const PERSONAL_ME = {
  ...ME,
  handle: "ada",
  orgs: [{ id: "01ada", name: "ada", role: "owner" }, ...ME.orgs],
};

async function inPersonalNamespace(page: import("@playwright/test").Page) {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: PERSONAL_ME }));
  await page.route("**/v1/orgs/ada/repos?limit=200", (r) =>
    r.fulfill({ json: { repos: [] } }),
  );
}

test("a personal namespace's new-repository form starts public and says where private goes", async ({
  page,
}) => {
  await inPersonalNamespace(page);
  await page.goto("/dashboard/new");

  // Both ways of creating carry the visibility control, and both start
  // public here: the mirror form is the one that opens first.
  const box = page.getByRole("checkbox", {
    name: "Anyone can read this repository",
  });
  await expect(box).toBeChecked();
  await expect(
    page.getByText(/Private repositories live in an organization/),
  ).toBeVisible();

  await page.getByRole("tab", { name: "Empty repository" }).click();
  await expect(box).toBeChecked();
  await expect(
    page.getByText(/Private repositories live in an organization/),
  ).toBeVisible();

  // The note's link opens the organization form — the same one the
  // sidebar's switcher opens, not a page that does not exist.
  await page.getByRole("button", { name: "create one" }).click();
  await expect(page.getByLabel("Organization name")).toBeVisible();
});

test("an organization's new-repository form is unchanged: private by default, no note", async ({
  page,
}) => {
  await inPersonalNamespace(page);
  // Same person, other namespace: the rule is about where they are,
  // not who they are.
  await page.addInitScript(() => {
    try {
      localStorage.setItem(
        "stratum-session",
        JSON.stringify({ org: "acme", token: "" }),
      );
    } catch {
      /* storage blocked: the test then lands in the first org and fails loudly */
    }
  });
  await page.goto("/dashboard/new");
  await page.getByRole("tab", { name: "Empty repository" }).click();
  await expect(
    page.getByRole("checkbox", { name: "Anyone can read this repository" }),
  ).not.toBeChecked();
  await expect(
    page.getByText(/Private repositories live in an organization/),
  ).toHaveCount(0);
});

test("a private create refused in a personal namespace offers an organization, not a subscription", async ({
  page,
}) => {
  await inPersonalNamespace(page);
  const sentence =
    "quota: private repositories live in an organization — create one from the dashboard";
  const creates: Array<Record<string, unknown>> = [];
  await page.route("**/v1/orgs/*/repos", (r) => {
    creates.push(r.request().postDataJSON());
    return r.fulfill({ status: 402, json: { error: sentence } });
  });
  // The paywall reads billing before it draws. It must never be asked
  // for here; if it is, this answers with what the server would.
  let billingReads = 0;
  await page.route("**/v1/orgs/ada/billing", (r) => {
    billingReads += 1;
    return r.fulfill({
      status: 402,
      json: { error: "a personal namespace is free and has nothing to subscribe" },
    });
  });

  await page.goto("/dashboard/new");
  await page.getByRole("tab", { name: "Empty repository" }).click();
  await page.getByLabel("Repository name").fill("diary");
  // Untick the default: the person wants it private.
  await page
    .getByRole("checkbox", { name: "Anyone can read this repository" })
    .uncheck();
  await page.getByRole("button", { name: "Create repository" }).click();

  // The server's sentence, and the one action that helps.
  const region = page.getByRole("region", { name: "Organization needed" });
  await expect(region).toBeVisible();
  await expect(region).toContainText(sentence);
  await expect(
    region.getByRole("button", { name: "Create an organization" }),
  ).toBeVisible();
  // Not the subscribe paywall, not a red error: neither is an answer.
  await expect(
    page.getByRole("region", { name: "Subscription needed" }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Continue to checkout" }),
  ).toHaveCount(0);
  await expect(page.getByRole("alert")).toHaveCount(0);
  expect(creates).toEqual([
    { name: "diary", public: false, description: undefined },
  ]);
  expect(billingReads).toBe(0);

  // The action opens the organization form, in place, with the
  // repository form still filled in behind it.
  await region.getByRole("button", { name: "Create an organization" }).click();
  await expect(page.getByLabel("Organization name")).toBeVisible();
  await expect(region).toHaveCount(0);
  await expect(page.getByLabel("Repository name")).toHaveValue("diary");
});

test("the free-organization refusal still reaches the subscribe paywall", async ({
  page,
}) => {
  // The two 402s share a status and nothing else. Sorting one of them
  // to the organization form must not have moved the other.
  await inPersonalNamespace(page);
  await page.addInitScript(() => {
    try {
      localStorage.setItem(
        "stratum-session",
        JSON.stringify({ org: "acme", token: "" }),
      );
    } catch {
      /* see above */
    }
  });
  await page.route("**/v1/orgs/*/repos", (r) =>
    r.fulfill({
      status: 402,
      json: {
        error:
          "quota: private repositories need a paid plan — subscribe from the billing page",
      },
    }),
  );
  await page.route("**/v1/orgs/acme/billing", (r) =>
    r.fulfill({ json: FREE_BILLING }),
  );
  await page.goto("/dashboard/new");
  await page.getByRole("tab", { name: "Empty repository" }).click();
  await page.getByLabel("Repository name").fill("vault");
  await page.getByRole("button", { name: "Create repository" }).click();
  await expect(
    page.getByRole("region", { name: "Subscription needed" }),
  ).toBeVisible();
  await expect(
    page.getByRole("region", { name: "Organization needed" }),
  ).toHaveCount(0);
});
