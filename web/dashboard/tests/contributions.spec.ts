// The contribution graph on a profile page.
//
// Two things are worth driving a browser for, and neither can be
// asserted from a unit test of the pure helpers.
//
// **The private-work contract survives the render.** The server already
// refuses to name a private repository, and `contribution-graph.test.ts`
// holds the label logic. What this file holds is that nothing between
// those two puts a name back: the rendered document must carry the
// count and must not carry anything else about it, and the legend must
// say which of the two states the reader is looking at — a quiet
// fortnight and an opted-out one look identical otherwise.
//
// **A year of squares does not widen the page.** Fifty-three columns is
// wider than a phone, and a grid that pushes `documentElement.scrollWidth`
// past the viewport is the exact defect the manual walkthrough's
// overflow audit fails a build for. It is cheaper to catch here.

import { expect, test } from "@playwright/test";

type Json = Record<string, unknown>;

const ADA: Json = {
  handle: "ada",
  name: "ada",
  display_name: "Ada Lovelace",
  bio: null,
  location: null,
  company: null,
  pronouns: null,
  kind: "human",
  contrib_private_optin: true,
  profile_repo: null,
  created_at: 1_700_000_000,
  links: [],
  public_repos: 1,
};

/// A year ending on a Saturday, so the grid is a full 53 columns — the
/// widest a graph ever gets, which is the case the overflow audit is
/// about.
const FROM = "2024-01-01";
const TO = "2024-12-28";

const GRAPH = {
  from: FROM,
  to: TO,
  total: 7,
  private_included: true,
  days: [
    {
      day: 19723,
      date: "2024-01-01",
      count: 4,
      // One public repository accounts for two of the four. The other
      // two are private, and the page must say only that.
      repos: [{ org: "acme", name: "widget", count: 2 }],
    },
    { day: 19730, date: "2024-01-08", count: 3, repos: [] },
  ],
};

async function visit(
  page: import("@playwright/test").Page,
  graph: Json | number = GRAPH,
) {
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/search/repos*", (r) =>
    r.fulfill({ status: 200, json: { repos: [], next: null } }),
  );
  await page.route("**/v1/users/ada/pins", (r) =>
    r.fulfill({ status: 200, json: { pins: [] } }),
  );
  await page.route("**/v1/users/ada", (r) =>
    r.fulfill({ status: 200, json: ADA }),
  );
  await page.route("**/v1/users/ada/contributions*", (r) =>
    typeof graph === "number"
      ? r.fulfill({ status: graph, json: { error: "no" } })
      : r.fulfill({ status: 200, json: graph }),
  );
  const seen = page.waitForResponse((r) =>
    /\/v1\/users\/ada\/contributions$/.test(new URL(r.url()).pathname),
  );
  await page.goto("/ada");
  await seen;
}

test("a day's private work is a number and never a repository", async ({
  page,
}) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await visit(page);
  await expect(page.getByRole("heading", { name: /7 commits/ })).toBeVisible();

  const mixed = page.locator(
    '[data-testid="contribution-cell"][data-date="2024-01-01"]',
  );
  await expect(mixed).toHaveAttribute("data-count", "4");
  // The public repository is named; the two private commits are named
  // as private work and as nothing else.
  await expect(mixed).toHaveAttribute(
    "title",
    "4 contributions on 2024-01-01 — acme/widget, private work",
  );

  // A day that is *only* private names no repository at all — a green
  // square with nothing behind it, which is the whole contract.
  const hidden = page.locator(
    '[data-testid="contribution-cell"][data-date="2024-01-08"]',
  );
  await expect(hidden).toHaveAttribute(
    "title",
    "3 contributions on 2024-01-08 — private work",
  );

  // And the legend states which of the two states this is. Without it a
  // quiet fortnight and an opted-out account render identically, and a
  // reader has no way to tell them apart.
  await expect(
    page.getByText(/private repositories included as daily totals/),
  ).toBeVisible();
});

test("an opted-out graph says so rather than saying nothing", async ({
  page,
}) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await visit(page, { ...GRAPH, private_included: false, total: 2 });
  await expect(
    page.getByText("Commits you authored in public repositories."),
  ).toBeVisible();
  await expect(
    page.getByText(/private repositories included as daily totals/),
  ).toBeHidden();
});

test("a full year of squares scrolls inside its own box", async ({ page }) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  // 363 days plus the short first column; the point is that all of them
  // render and none of them widen the page.
  await visit(page);
  await expect(
    page.locator('[data-testid="contribution-cell"]').first(),
  ).toBeVisible();
  const count = await page.locator('[data-testid="contribution-cell"]').count();
  expect(count).toBe(363);

  await page.setViewportSize({ width: 390, height: 844 });
  const overflow = await page.evaluate(
    () =>
      document.documentElement.scrollWidth -
      document.documentElement.clientWidth,
  );
  expect(
    overflow,
    "the contribution grid widened the page instead of scrolling inside itself",
  ).toBeLessThanOrEqual(0);
});

test("a profile whose graph cannot be read is still the page it was", async ({
  page,
}) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  // An organization namespace has no graph, and a person whose graph
  // failed to load is better served by the page they came for than by a
  // red box where their work should be.
  await visit(page, 404);
  await expect(
    page.getByRole("heading", { name: "Ada Lovelace", level: 1 }),
  ).toBeVisible();
  await expect(page.locator('[data-testid="contribution-cell"]')).toHaveCount(
    0,
  );
});

test("an empty year draws the whole grid rather than nothing", async ({
  page,
}) => {
  // The state most of our users will have on the day they arrive, and
  // the one that is easiest to get wrong. GitHub's answer — checked on
  // a real profile with a zero year — is to draw all 53 columns in the
  // empty step, state the zero in the header, and keep the legend. A
  // grid of holes, or an empty-state illustration, reads as broken
  // rather than as quiet.
  await visit(page, { ...GRAPH, total: 0, days: [] });
  await expect(page.getByRole("heading", { name: "0 commits" })).toBeVisible();
  await expect(page.locator('[data-testid="contribution-cell"]')).toHaveCount(
    363,
  );
  // Every square is the empty step, and it is a tinted cell rather than
  // a hole: `--surface-2` against the page's `--surface-0` ground.
  const first = page.locator('[data-testid="contribution-cell"]').first();
  await expect(first).toHaveAttribute("data-count", "0");
  const bg = await first.evaluate((e) => getComputedStyle(e).backgroundColor);
  const body = await page.evaluate(
    () => getComputedStyle(document.body).backgroundColor,
  );
  expect(bg, "an empty day rendered as a hole in the page").not.toBe(body);
  expect(bg).not.toBe("rgba(0, 0, 0, 0)");
  // The legend still says what the colours would mean.
  await expect(page.getByText("Less")).toBeVisible();
  await expect(page.getByText("More")).toBeVisible();
  // And the month and weekday labels still orient the reader.
  for (const d of ["Mon", "Wed", "Fri"]) {
    await expect(page.getByText(d, { exact: true })).toBeVisible();
  }
});

test("the busiest day sets the ramp, so a modest year is not one pale wash", async ({
  page,
}) => {
  // Relative to the person's own year, as GitHub does it. A fixed ramp
  // would render every one of these days at the palest step, which
  // reads as "barely used" for somebody who worked most weeks — and our
  // counts are already lower than GitHub's for the same year.
  await visit(page, {
    ...GRAPH,
    private_included: false,
    total: 4,
    days: [
      { day: 19723, date: "2024-01-01", count: 1, repos: [] },
      { day: 19724, date: "2024-01-02", count: 3, repos: [] },
    ],
  });
  const step = (date: string) =>
    page
      .locator(`[data-testid="contribution-cell"][data-date="${date}"]`)
      .evaluate((e) => getComputedStyle(e).backgroundColor);
  const light = await step("2024-01-01");
  const dark = await step("2024-01-02");
  expect(light).not.toBe(dark);
  // The busiest day reaches the top of the ramp even though 3 is a
  // small number in absolute terms.
  const legendTop = await page
    .locator("span[aria-hidden]")
    .last()
    .evaluate((e) => getComputedStyle(e).backgroundColor);
  expect(dark).toBe(legendTop);
});
