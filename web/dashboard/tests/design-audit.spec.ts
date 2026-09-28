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

test("the sign-in screen says what it is for, and how to get an account", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );

  // It read "Weft Dashboard", in a <span>, on every mode: no heading.
  await page.goto("/login");
  await expect(
    page.getByRole("heading", { level: 1, name: "Sign in", exact: true }),
  ).toBeVisible();
  // The mark leads back to the way in.
  await expect(page.getByRole("link", { name: "Weft", exact: true })).toHaveAttribute("href", "/");
  // The first field is the one with the cursor in it.
  await expect(page.getByLabel("Email")).toBeFocused();

  // Where "Create an account" used to be, the answer to the question a
  // visitor with no account actually has: who makes one. Said in words
  // true of any deployment — "this server", not a product name.
  const how = page.getByText(/No account yet\?/);
  await expect(how).toContainText("made by invitation");
  await expect(how).toContainText("admin of your organization");
  await expect(how).toContainText("whoever runs this server");
  await expect(
    page.getByRole("button", { name: /create (an )?account/i }),
  ).toHaveCount(0);
});

test("an invitation for somebody new starts at the top, and says what the handle will be", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/auth/invite/preview", (r) =>
    r.fulfill({
      json: {
        org: "acme",
        role: "member",
        email: "dev.eloper@acme.test",
        expires_at: Date.now() + 86_400_000,
      },
    }),
  );
  await page.goto("/dashboard/#invite=stinv_01_design");
  await expect(page.getByRole("heading", { name: "Join acme" })).toBeVisible();
  // The first field has the cursor, not the optional one below it.
  await expect(page.getByLabel("Your name")).toBeFocused();
  // Optional is said on the field, and what an empty field gets is
  // said before anybody submits — the name that goes in every clone URL
  // is not a surprise to find out afterwards.
  const handle = page.getByLabel("Handle");
  await expect(page.getByText("Handle (optional)")).toBeVisible();
  await expect(handle).toHaveAttribute("placeholder", "dev-eloper");
  await expect(handle).not.toHaveAttribute("required", /.*/);
  await expect(page.locator("#handle-hint")).toContainText("/dev-eloper/repo");
  await handle.fill("dev");
  await expect(page.locator("#handle-hint")).toContainText("/dev/repo");
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
