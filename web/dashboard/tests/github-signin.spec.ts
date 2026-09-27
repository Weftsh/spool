// Coming back from signing in with GitHub.
//
// The same argument as `connect-outcome.spec.ts`, pointed at the other
// flow — and with one difference that is the whole reason this file
// exists separately: every *refusal* here lands the browser **signed
// out**, on the sign-in screen. The signed-in shell's banner never sees
// one. So a sentence that renders only inside the shell would be a
// sentence nobody ever reads, and the screen would go quiet at exactly
// the moment somebody needs telling why they are not signed in.

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
  ["denied", /cancelled at GitHub/, /email address and password/],
  [
    "expired",
    /not finished in the browser that started it/,
    /Continue with GitHub/,
  ],
  [
    "noemail",
    /did not give us an address it has confirmed/,
    /Verify your primary email/,
  ],
  [
    "emailtaken",
    /already listed on a different account/,
    /Remove it there first/,
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
    // The refusal must not also be a dead end: the password form is the
    // way in that still works whatever GitHub said.
    await expect(page.getByLabel("Email")).toBeVisible();
  }
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

  // And it is offered on the sign-up form too, which is where the site's
  // "Sign up free" lands — that is the path the whole feature is for.
  await page.getByRole("button", { name: "Create an account" }).click();
  await expect(
    page.getByRole("link", { name: "Continue with GitHub" }),
  ).toBeVisible();
});

test("a successful GitHub sign-in shows no warning", async ({ page }) => {
  // `ok` and `new` are successes and arrive signed in, so the only thing
  // to check is that neither grows a banner congratulating somebody for
  // signing in.
  await mockApi(page);
  for (const outcome of ["ok", "nonsense"]) {
    await page.goto(`/dashboard/?github=${outcome}`);
    await expect(page.getByText("Requests today")).toBeVisible();
    await expect(
      page.getByRole("status").filter({ hasText: /GitHub/ }),
    ).toHaveCount(0);
  }
});
