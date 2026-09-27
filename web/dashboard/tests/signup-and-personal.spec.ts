// The doors into the product, and what a new repository asks for.
//
// `/login?mode=signup` is where a "Sign up" link lands: a button that
// says sign up must open a form that says create an account, not the
// sign-in form with a small link under it. Every other page is for
// somebody signed in, so a visitor who is not is sent to `/login` with
// the address they asked for, and brought back to it. And a new
// repository, in a personal namespace or an organization, asks for a
// name and a description and nothing about who may read it: every
// repository is private to its namespace.

import { expect, test } from "@playwright/test";

import { ME, mockApi } from "./fixtures";

/// Nobody signed in: every API call refused, `me` says nobody is here.
/// Registered first so the specific routes below win.
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

test("a forge page sends somebody signed out to sign in, and back again", async ({
  page,
}) => {
  await signedOut(page);
  await page.goto("/acme/widget/issues?q=is%3Aopen");
  // The address they asked for rides along, query and all, and nothing
  // of the page they may not see is drawn first.
  await expect(page).toHaveURL(
    /\/login\?next=%2Facme%2Fwidget%2Fissues%3Fq%3Dis%253Aopen$/,
  );
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("navigation", { name: "Repository" }),
  ).toHaveCount(0);

  await page.route("**/v1/auth/login", (r) => r.fulfill({ json: ME }));
  await page.getByLabel("Email").fill("owner@acme.test");
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(page).toHaveURL(/\/acme\/widget\/issues\?q=is%3Aopen$/);
});

test("the front page is the way in, not a listing", async ({ page }) => {
  await signedOut(page);
  await page.goto("/");
  // Moved, not rendered: the dashboard's own sign-in form, at the
  // dashboard's address.
  await expect(page).toHaveURL(/\/dashboard\/?$/);
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();
});

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

test("a new repository asks nobody who may read it, and sends nothing about it", async ({
  page,
}) => {
  await inPersonalNamespace(page);
  const creates: Array<Record<string, unknown>> = [];
  await page.route("**/v1/orgs/*/repos", (r) => {
    creates.push(r.request().postDataJSON());
    return r.fulfill({
      status: 409,
      json: { error: '"diary" already exists in ada' },
    });
  });
  await page.goto("/dashboard/new");
  // Neither way of creating carries a visibility control.
  await expect(page.getByLabel("Repository URL")).toBeVisible();
  await expect(page.getByRole("checkbox")).toHaveCount(0);
  await page.getByRole("tab", { name: "Empty repository" }).click();
  await expect(page.getByRole("checkbox")).toHaveCount(0);

  await page.getByLabel("Repository name").fill("diary");
  await page.getByRole("button", { name: "Create repository" }).click();
  // A refusal is the server's own sentence — there is no paywall and no
  // "create an organization" detour to sort it into.
  await expect(page.getByRole("alert")).toContainText("already exists");
  await expect(
    page.getByRole("region", {
      name: /Subscription needed|Organization needed/,
    }),
  ).toHaveCount(0);
  expect(creates).toEqual([{ name: "diary" }]);
});
