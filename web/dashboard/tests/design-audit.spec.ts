// What the design audit of September 2026 found in the signed-in
// product, each pinned where it was found. None of these broke a
// function — every one of them was a page that worked and read wrong.

import { expect, test, type Page } from "@playwright/test";

import { mockApi, signIn, signInAsPerson } from "./fixtures";

/// Every rendered element that draws a border in its own text colour.
///
/// Tailwind v4 draws a bare `border` in `currentColor` (v3 drew a light
/// gray), and the vendored sidebar says `border-r` with no colour,
/// relying on a base rule `shadcn init` would have written and we never
/// run. So the rail's edge was a full-strength text-coloured line down
/// every admin page — near-white on charcoal. No design here ever wants
/// a border in the ink colour, so the class is "none of them do".
async function inkBorders(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const out = new Set<string>();
    for (const el of document.querySelectorAll<HTMLElement>("body *")) {
      if (el.getClientRects().length === 0) continue;
      const cs = getComputedStyle(el);
      for (const side of ["Top", "Right", "Bottom", "Left"] as const) {
        const width = parseFloat(cs.getPropertyValue(`border-${side.toLowerCase()}-width`));
        const style = cs.getPropertyValue(`border-${side.toLowerCase()}-style`);
        const colour = cs.getPropertyValue(`border-${side.toLowerCase()}-color`);
        if (width > 0 && style !== "none" && colour === cs.color) {
          out.add(`${el.dataset.slot ?? el.tagName.toLowerCase()} (${side})`);
        }
      }
    }
    return [...out];
  });
}

for (const colorScheme of ["dark", "light"] as const) {
  test(`no border is drawn in the ink colour (${colorScheme})`, async ({ page }) => {
    await page.emulateMedia({ colorScheme });
    await signIn(page);
    await expect(page.getByText("Requests today")).toBeVisible();
    expect(await inkBorders(page)).toEqual([]);

    await page.getByRole("link", { name: "Members" }).click();
    await expect(page).toHaveURL(/settings\/members/);
    expect(await inkBorders(page)).toEqual([]);
  });
}

test("the rail files each setting under whose it is, each with its own icon", async ({
  page,
}) => {
  // One "Settings" label over every row put Runners two rows above
  // Password with nothing to say that one changes the organization for
  // everybody and the other changes only you. And Runners and Email
  // addresses fell through to the Members icon, so the collapsed rail
  // showed three identical glyphs.
  await signInAsPerson(page);
  const rail = page.locator('[data-slot="sidebar"]');
  const group = (label: string) =>
    rail.locator('[data-slot="sidebar-group"]').filter({
      has: page.locator('[data-slot="sidebar-group-label"]', { hasText: label }),
    });

  const org = group("Organization");
  const account = group("Your account");
  for (const name of ["Members", "Runners", "Teams", "Activity"]) {
    await expect(org.getByRole("link", { name, exact: true })).toBeVisible();
    await expect(account.getByRole("link", { name, exact: true })).toHaveCount(0);
  }
  for (const name of ["Tokens", "SSH keys", "Email addresses", "Password"]) {
    await expect(account.getByRole("link", { name, exact: true })).toBeVisible();
    await expect(org.getByRole("link", { name, exact: true })).toHaveCount(0);
  }

  const icons = await rail
    .locator('[data-slot="sidebar-group"] a svg')
    .evaluateAll((svgs) => svgs.map((s) => s.innerHTML));
  expect(icons.length).toBeGreaterThan(8);
  expect(new Set(icons).size, "two rail entries share an icon").toBe(icons.length);

  // The breadcrumb says the same thing the rail does.
  await group("Your account").getByRole("link", { name: "Tokens" }).click();
  await expect(page.locator("header").first()).toContainText("Your account");
});

test("the org overview lists repositories before the usage history", async ({
  page,
}) => {
  // The screen the rail calls "Repositories" opened with three
  // full-width charts, and the table began a screen and a half down.
  await signIn(page);
  const table = page.getByRole("link", { name: "widget", exact: true });
  const chart = page.getByRole("img", { name: "Requests per day", exact: true });
  await expect(table).toBeVisible();
  await expect(chart).toBeAttached();
  const tableTop = (await table.boundingBox())!.y;
  const chartTop = (await chart.boundingBox())!.y;
  expect(tableTop).toBeLessThan(chartTop);
  // And the page has headings to navigate by.
  await expect(page.getByRole("heading", { name: "Repositories", exact: true })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Usage", exact: true })).toBeVisible();
});

test("the sign-in screen says what it is for, and sign-up starts at the top", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );

  // It read "Weft Dashboard", in a <span>, on every mode: no heading,
  // and the same words on the screen that creates an account.
  await page.goto("/login");
  await expect(
    page.getByRole("heading", { level: 1, name: "Sign in to Weft" }),
  ).toBeVisible();
  // The mark leads back to the site the visitor came from.
  await expect(page.getByRole("link", { name: "Weft", exact: true })).toHaveAttribute("href", "/");

  // Two autoFocus props on one form, and the later one wins: sign-up
  // opened with the cursor in Email, the third field, below the two it
  // had not asked for yet.
  await page.goto("/login?mode=signup");
  await expect(
    page.getByRole("heading", { level: 1, name: "Create your Weft account" }),
  ).toBeVisible();
  await expect(page.getByLabel("Your name")).toBeFocused();
});

test("'Your settings' opens the person's settings, not the organization's", async ({
  page,
}) => {
  // A bare /settings lands on the first section the viewer may act on,
  // which for an owner is Members: the organization's member list,
  // under a menu item that says "Your settings".
  await signInAsPerson(page);
  // The cookie session, restored by the server on the next boot — the
  // same re-registration the storage test does before its reload.
  await mockApi(page);
  await page.goto("/acme/widget");
  await page.getByRole("button", { name: "Account menu" }).click();
  await page.getByRole("menuitem", { name: "Your settings" }).click();
  await expect(page).toHaveURL(/\/dashboard\/settings\/emails$/);
});
