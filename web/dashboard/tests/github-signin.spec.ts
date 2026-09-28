// Coming back from signing in with GitHub.
//
// The same argument as `connect-outcome.spec.ts`, pointed at the other
// flow — and with one difference that is the whole reason this file
// exists separately: every *refusal* here lands the browser **signed
// out**, on the sign-in screen. The signed-in shell's banner never sees
// one. So a sentence that renders only inside the shell would be a
// sentence nobody ever reads, and the screen would go quiet at exactly
// the moment somebody needs telling why they are not signed in.
//
// And GitHub never makes an account here. It signs somebody in to one
// they already have, or it says nobody here is them — and then the only
// honest next step is the only way anybody gets an account: ask for an
// invitation.

import { expect, test } from "@playwright/test";

import { mockApi } from "./fixtures";

/// Signed out: `me` refuses, which is what the login screen is for.
async function signedOut(page: import("@playwright/test").Page) {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
}

/// Outcome, the verdict, and the next step. A banner that only says
/// what went wrong is a dead end with better wording.
const SENTENCES: Array<[string, RegExp, RegExp]> = [
  [
    "noaccount",
    /nobody on this server is you/,
    /ask an admin of your organization to invite you/,
  ],
  ["denied", /cancelled at GitHub/, /email address and password/],
  [
    "expired",
    /not finished in the browser that started it/,
    /Continue with GitHub/,
  ],
  [
    "noemail",
    /did not give us an address it has confirmed/,
    /email address and password still works/,
  ],
  ["disabled", /has been disabled/, /Ask an owner/],
  ["unavailable", /not set up on this server/, /email address and password/],
  ["error", /GitHub did not answer/, /try again in a moment/i],
];

test("every refused GitHub sign-in is said on screen, with what to do", async ({
  page,
}) => {
  await signedOut(page);
  for (const [outcome, verdict, next] of SENTENCES) {
    await page.goto(`/dashboard/?github=${outcome}`);
    const banner = page.getByRole("status").filter({ hasText: verdict });
    await expect(banner).toBeVisible();
    await expect(banner).toContainText(next);
    // Nor may it point at a door that is not there: there is no
    // sign-up, and the old `noemail` sentence ended by offering one.
    await expect(banner).not.toContainText(/sign(ing)? ?up|create an account/i);
    // The refusal must not also be a dead end: the password form is the
    // way in that still works whatever GitHub said.
    await expect(page.getByLabel("Email")).toBeVisible();
  }
});

test("outcomes of the old GitHub sign-up are not sentences any more", async ({
  page,
}) => {
  // `emailtaken` described an account GitHub can no longer make, and a
  // stale link carrying it must not tell somebody to go and fix one.
  await signedOut(page);
  await page.goto("/dashboard/?github=emailtaken");
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("status").filter({ hasText: /GitHub/ }),
  ).toHaveCount(0);
  await expect(page.getByText(/different account/)).toHaveCount(0);
});

test("the sign-in screen offers GitHub, and the link leaves for the server", async ({
  page,
}) => {
  await signedOut(page);
  await page.goto("/dashboard/");
  // A real anchor, not a button with a click handler: the flow is a
  // full-page leave, and it has to work with no script running.
  const link = page.getByRole("link", { name: "Continue with GitHub" });
  await expect(link).toBeVisible();
  await expect(link).toHaveAttribute("href", "/v1/auth/github/start");

  // Offered with the password path, not in place of it: the other
  // modes of the screen are not ways to sign in with GitHub.
  await page.getByRole("button", { name: "Forgot your password?" }).click();
  await expect(
    page.getByRole("link", { name: "Continue with GitHub" }),
  ).toHaveCount(0);
});

test("a successful GitHub sign-in shows no warning", async ({ page }) => {
  // `ok` is a success and arrives signed in, so the only thing to check
  // is that it grows no banner congratulating somebody for signing in.
  await mockApi(page);
  for (const outcome of ["ok", "nonsense"]) {
    await page.goto(`/dashboard/?github=${outcome}`);
    await expect(page.getByText("Requests today")).toBeVisible();
    await expect(
      page.getByRole("status").filter({ hasText: /GitHub/ }),
    ).toHaveCount(0);
  }
});

test("a stale `github=new` lands on the overview, not an onboarding", async ({
  page,
}) => {
  // `new` meant "GitHub just made you an account", and it opened the
  // page for picking repositories to mirror. GitHub makes no accounts
  // any more; somebody signed in who follows an old link carrying it is
  // an ordinary sign-in and belongs on the overview.
  await mockApi(page);
  await page.goto("/dashboard/?github=new");
  await expect(page.getByText("Requests today")).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "New repository" }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("status").filter({ hasText: /GitHub/ }),
  ).toHaveCount(0);
});
