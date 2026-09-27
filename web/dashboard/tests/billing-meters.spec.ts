// The three pools and the spend limit, on Settings → Billing, the
// repository page and the organization overview — against a mocked
// control plane, so every state the server can be in is one fixture
// away: inside the pool, past it and metered, at the cap and refused,
// a member who may not raise the limit, a free organization, and the
// older server that has none of these fields.
//
// Selectors are roles with exact names. A `getByText` substring match
// once hid a plural bug on this dashboard, and here "Hosted CI minutes"
// is both a panel title and a row heading.

import { expect, test, type Page } from "@playwright/test";

import {
  BILLING,
  FREE_BILLING,
  ME,
  REPOS,
  USAGE,
  mockApi,
  signInAsPerson,
} from "./fixtures";

const METER = {
  included: 5000,
  used: 1240,
  remaining: 3760,
  overage: 0,
  estimated_cents: 0,
  refusing: false,
};

/// The pooled shape a server with usage billing sends, inside every
/// pool. Rates and per-seat figures are the server's: the page types no
/// price of its own, and this is where the suite's numbers come from.
const POOLED = {
  ...BILLING,
  billable_seats: 5,
  paid_seats: 5,
  // Noon, not midnight: a UTC-midnight instant renders as the day
  // before west of Greenwich, and the assertion below is on the day.
  period_start: Date.UTC(2026, 8, 1, 12),
  current_period_end: Date.UTC(2026, 9, 1, 12),
  paid_minutes_per_seat: 1000,
  paid_egress_gb_per_seat: 10,
  paid_storage_gb_per_seat: 5,
  paid_packages_gb_per_seat: 2,
  ci_minutes_limit: 5000,
  ci_minutes_used: 1240,
  ci_minutes_remaining: 3760,
  meters: {
    minutes: METER,
    egress_gb: { ...METER, included: 50, used: 12.4, remaining: 37.6 },
    storage_gb: { ...METER, included: 25, used: 9.1, remaining: 15.9 },
    packages_gb: { ...METER, included: 10, used: 3.2, remaining: 6.8 },
  },
  overage_estimated_cents: 0,
  spend_limit_cents: 0,
  may_raise_spend_limit: true,
  metering: "on",
  rates: {
    cents_per_1000_minutes: 800,
    cents_per_gb_egress: 10,
    cents_per_gb_month_storage: 10,
    cents_per_gb_month_packages: 15,
  },
};

/// Past the minutes pool, with a limit to be metered against.
const OVER = {
  ...POOLED,
  meters: {
    ...POOLED.meters,
    minutes: {
      ...METER,
      used: 6000,
      remaining: 0,
      overage: 1000,
      estimated_cents: 800,
    },
  },
  overage_estimated_cents: 800,
  spend_limit_cents: 2500,
};

/// At the limit: the server is refusing, and says so with a flag.
const CAPPED = {
  ...OVER,
  meters: {
    ...OVER.meters,
    minutes: { ...OVER.meters.minutes, refusing: true },
  },
  overage_estimated_cents: 2500,
};

async function billingPage(page: Page, view: unknown) {
  await signInAsPerson(page);
  await page.route("**/v1/orgs/acme/billing", (r) => r.fulfill({ json: view }));
  await page.getByRole("link", { name: "Billing" }).click();
}

const day = (ms: number) => new Date(ms).toLocaleDateString();

/// The repository's numbers, at the address they live at.
///
/// This used to go to `/dashboard/`, click the row and wait for
/// "Requests absorbed", because the metrics screen was held in React
/// state and had no URL at all. It has one now — `/acme/widget/insights`,
/// a members-only tab on the repository's own page — so this asks for it.
/// The walls these tests are about are drawn over every tab, Insights
/// included.
async function openWidget(page: Page) {
  await page.goto("/acme/widget/insights");
  await expect(page.getByText("Requests absorbed")).toBeVisible();
}

/// The bar's fill. A class is not a selector this suite reaches for,
/// but the fill *is* the one thing here that is colour, and the design
/// contract says colour is never alone — so the test that reads the
/// words beside it reads the colour too, to prove they agree.
function fill(page: Page, line: string) {
  return page.getByRole("img", { name: line, exact: true }).locator("div");
}

test("inside every pool: three rows, both period ends, and a limit of nothing", async ({
  page,
}) => {
  await billingPage(page, POOLED);

  // One row per pool, in the order a reader is likely to hit them.
  const headings = page.getByRole("heading", { level: 3 });
  await expect(headings).toHaveText([
    "Hosted CI minutes",
    "Private transfer",
    "Private storage",
    "Package storage",
  ]);
  // Both figures on every line — "760 left" alone says nothing about
  // whether that is most of the period or the last of it — and the
  // bar carries the same words for anybody who cannot see a length.
  for (const line of [
    "3.2 of 10 GB of packages, averaged over this period",
    "1,240 of 5,000 minutes used this period",
    "12.4 of 50 GB transferred this period",
    "9.1 of 25 GB stored, averaged over this period",
  ]) {
    await expect(page.getByText(line, { exact: true })).toBeVisible();
    await expect(page.getByRole("img", { name: line, exact: true })).toBeVisible();
    await expect(fill(page, line)).not.toHaveClass(/bg-serious|bg-warning/);
  }
  await expect(
    page.getByText(
      "Counted for private work on our runners and our storage. Public traffic, self-hosted runners and a hosted job's traffic to Weft are never counted.",
      { exact: true },
    ),
  ).toBeVisible();
  // Where the pools come from, all three, from the per-seat fields.
  await expect(
    page.getByText(
      "Each paid seat brings 1,000 hosted CI minutes, 10 GB of transfer and 5 GB of storage for private work, pooled; 5 seats billed.",
      { exact: true },
    ),
  ).toBeVisible();
  // Nothing is happening at any edge, so nothing says so.
  await expect(
    page.getByRole("status").filter({ hasText: /pool|spend limit/ }),
  ).toHaveCount(0);

  // "Period", both ends, once the server sends the start; "Renews" is
  // the older server's cell and is gone.
  await expect(
    page.getByRole("term").filter({ hasText: /^Period$/ }),
  ).toBeVisible();
  await expect(
    page.getByRole("definition").filter({
      hasText: `${day(POOLED.period_start)} – ${day(POOLED.current_period_end)}`,
    }),
  ).toBeVisible();
  await expect(page.getByRole("term").filter({ hasText: /^Renews$/ })).toHaveCount(0);

  // The limit, the estimate, and the editor — this viewer may raise it.
  await expect(
    page.getByRole("term").filter({ hasText: /^Spend limit$/ }),
  ).toBeVisible();
  await expect(
    page.getByRole("definition").filter({ hasText: /^\$0$/ }),
  ).toBeVisible();
  await expect(
    page.getByRole("term").filter({ hasText: /^Estimated past the pool$/ }),
  ).toBeVisible();
  await expect(
    page.getByRole("definition").filter({ hasText: /^\$0\.00$/ }),
  ).toBeVisible();
  await expect(page.getByLabel("New limit, in dollars")).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Save spend limit", exact: true }),
  ).toBeVisible();
  // The rates are the server's numbers, and the page quotes them.
  await expect(
    page.getByText("Past the pool: $0.008 a minute, $0.10 a GB transferred, $0.10 a GB-month stored.", { exact: true }),
  ).toBeVisible();
});

test("past the pool with a limit: the overage, its cost, and the rate it is metered at", async ({
  page,
}) => {
  await billingPage(page, OVER);
  const line = "6,000 of 5,000 minutes used this period";
  await expect(page.getByText(line, { exact: true })).toBeVisible();
  await expect(
    page.getByText("1,000 minutes past the pool, $8.00 estimated", { exact: true }),
  ).toBeVisible();
  // Past the pool is the warning amber, and it is also a sentence:
  // metered, at the server's price, until the server's limit.
  await expect(fill(page, line)).toHaveClass(/bg-warning/);
  await expect(fill(page, line)).not.toHaveClass(/bg-serious/);
  await expect(
    page.getByRole("status").filter({
      hasText:
        "Past the pool: metered at $0.008 a minute until the spend limit of $25 is reached.",
    }),
  ).toBeVisible();
  await expect(
    page.getByRole("definition").filter({ hasText: /^\$25$/ }),
  ).toBeVisible();
  await expect(
    page.getByRole("definition").filter({ hasText: /^\$8\.00$/ }),
  ).toBeVisible();
  await expect(
    page.getByText(/^\$8\.00 estimated so far this period/),
  ).toBeVisible();
  // Not refused: nothing says jobs are waiting.
  await expect(page.getByText(/Hosted jobs are waiting/)).toHaveCount(0);
});

test("at the cap: the server's flag turns the row serious and says what is refused", async ({
  page,
}) => {
  await billingPage(page, CAPPED);
  const line = "6,000 of 5,000 minutes used this period";
  // Serious red *and* the words, as a status: never colour alone.
  await expect(fill(page, line)).toHaveClass(/bg-serious/);
  await expect(
    page.getByRole("status").filter({
      hasText:
        "Hosted jobs are waiting at the spend limit. They start again when minutes free up or the limit is raised; jobs on your own runners are running as normal.",
    }),
  ).toBeVisible();
  // The other two pools are fine and say nothing.
  await expect(
    page.getByRole("status").filter({ hasText: /refused/ }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("definition").filter({ hasText: /^\$25\.00$/ }),
  ).toBeVisible();
});

test("raising the limit is one PATCH in whole cents, and bad input never leaves the page", async ({
  page,
}) => {
  await billingPage(page, POOLED);
  const requests: Array<{ method: string; body: unknown }> = [];
  await page.route("**/v1/orgs/acme/billing/spend-limit", (r) => {
    requests.push({
      method: r.request().method(),
      body: r.request().postDataJSON(),
    });
    return r.fulfill({ json: { ...POOLED, spend_limit_cents: 2500 } });
  });

  const input = page.getByLabel("New limit, in dollars");
  await input.fill("25");
  await page.getByRole("button", { name: "Save spend limit", exact: true }).click();
  await expect(
    page.getByRole("status").filter({ hasText: /^Spend limit is now \$25\.$/ }),
  ).toBeVisible();
  expect(requests).toEqual([
    { method: "PATCH", body: { spend_limit_cents: 2500 } },
  ]);
  // The page re-renders from the server's answer, not from the input.
  await expect(
    page.getByRole("definition").filter({ hasText: /^\$25$/ }),
  ).toBeVisible();
  await expect(input).toHaveValue("");

  // What the server would answer 400 to is refused here, in words,
  // and nothing is sent.
  await input.fill("-5");
  await page.getByRole("button", { name: "Save spend limit", exact: true }).click();
  await expect(
    page.getByRole("alert").filter({ hasText: "A spend limit cannot be negative." }),
  ).toBeVisible();
  expect(requests).toHaveLength(1);

  await input.fill("25.005");
  await page.getByRole("button", { name: "Save spend limit", exact: true }).click();
  await expect(
    page.getByRole("alert").filter({ hasText: "Whole cents only." }),
  ).toBeVisible();
  expect(requests).toHaveLength(1);
  // Typing again clears the complaint and the old confirmation.
  await input.fill("30");
  await expect(page.getByRole("alert")).toHaveCount(0);
  await expect(
    page.getByRole("status").filter({ hasText: /Spend limit is now/ }),
  ).toHaveCount(0);
});

test("a server that refuses the limit is quoted, and the old figure stands", async ({
  page,
}) => {
  await billingPage(page, POOLED);
  await page.route("**/v1/orgs/acme/billing/spend-limit", (r) =>
    r.fulfill({
      status: 503,
      json: { error: "usage billing is not configured on this deployment" },
    }),
  );
  await page.getByLabel("New limit, in dollars").fill("25");
  await page.getByRole("button", { name: "Save spend limit", exact: true }).click();
  await expect(
    page.getByRole("alert").filter({ hasText: /not configured/ }),
  ).toBeVisible();
  await expect(
    page.getByRole("definition").filter({ hasText: /^\$0$/ }),
  ).toBeVisible();
});

test("a member who may not raise the limit is told who can, and gets no box", async ({
  page,
}) => {
  // The server's flag decides, never the viewer's role: the same
  // owner session, told "no" by the view, gets the sentence.
  await billingPage(page, { ...OVER, may_raise_spend_limit: false });
  await expect(
    page.getByText("Only an owner or admin can raise it.", { exact: true }),
  ).toBeVisible();
  await expect(page.getByLabel("New limit, in dollars")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Save spend limit" }),
  ).toHaveCount(0);
  // The figures are still shown; only the control is withheld.
  await expect(
    page.getByRole("definition").filter({ hasText: /^\$25$/ }),
  ).toBeVisible();
});

test("a subscription older than usage billing is offered the way to enable it", async ({
  page,
}) => {
  await billingPage(page, { ...POOLED, metering: "resubscribe" });
  let subscribed = 0;
  await page.route("**/v1/orgs/acme/billing/subscribe", (r) => {
    subscribed += 1;
    return r.fulfill({
      json: { url: "https://checkout.example.test/pay/cs_2", kind: "checkout" },
    });
  });
  await page.route("https://checkout.example.test/**", (r) =>
    r.fulfill({ contentType: "text/html", body: "<h1>Provider checkout</h1>" }),
  );
  await expect(page.getByLabel("New limit, in dollars")).toHaveCount(0);
  await expect(
    page.getByText(/Re-subscribe to set a spend limit/),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "Re-subscribe to enable usage billing", exact: true })
    .click();
  await expect(page).toHaveURL("https://checkout.example.test/pay/cs_2");
  expect(subscribed).toBe(1);
});

test("a free organization sees its minutes on the rolling window, and no price", async ({
  page,
}) => {
  await billingPage(page, {
    ...FREE_BILLING,
    meters: {
      minutes: { ...METER, included: 500, used: 120, remaining: 380 },
      egress_gb: { ...METER, included: null, remaining: null, used: 0 },
      storage_gb: { ...METER, included: null, remaining: null, used: 0 },
    },
  });
  // The heading it always had, one row, the window rather than the
  // period — a free organization has no period, and no reset date.
  await expect(
    page.getByRole("heading", { level: 3, name: "Hosted CI minutes", exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("heading", { level: 3 })).toHaveCount(1);
  await expect(
    page.getByText("120 of 500 minutes used in the last 30 days", { exact: true }),
  ).toBeVisible();
  await expect(page.getByText("Usage this period")).toHaveCount(0);
  // Nothing is for sale past the pool, so no limit, no estimate, no rate.
  await expect(page.getByRole("term").filter({ hasText: /^Spend limit$/ })).toHaveCount(0);
  await expect(page.getByText(/\$0\.008/)).toHaveCount(0);
  await expect(page.getByText(/spend limit/)).toHaveCount(0);
  await expect(
    page.getByText(/500 minutes a month while free; 2,000 per seat once subscribed/),
  ).toBeVisible();
});

test("a free organization at its window's edge is told about the window, not a limit", async ({
  page,
}) => {
  await billingPage(page, {
    ...FREE_BILLING,
    meters: {
      minutes: { ...METER, included: 500, used: 500, remaining: 0, refusing: true },
      egress_gb: { ...METER, included: null, remaining: null, used: 0 },
      storage_gb: { ...METER, included: null, remaining: null, used: 0 },
    },
  });
  await expect(
    page.getByRole("status").filter({ hasText: /Hosted workflows are being refused/ }),
  ).toBeVisible();
  await expect(page.getByText(/30-day window/)).toBeVisible();
  await expect(page.getByText(/spend limit/)).toHaveCount(0);
});

test("a server without pools renders the minutes panel it always did", async ({
  page,
}) => {
  await billingPage(page, BILLING);
  // No `meters`: the legacy panel, to the word — the count left, the
  // rolling-window line, "Renews", and nothing about a limit.
  await expect(page.getByText("Hosted CI minutes", { exact: true })).toBeVisible();
  await expect(page.getByText("7,900", { exact: true })).toBeVisible();
  await expect(
    page.getByText("100 of 8,000 minutes used in the last 30 days", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("2,000 minutes per paid seat, 4 seats billed.", { exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("term").filter({ hasText: /^Renews$/ })).toBeVisible();
  await expect(page.getByRole("term").filter({ hasText: /^Period$/ })).toHaveCount(0);
  await expect(page.getByRole("term").filter({ hasText: /^Spend limit$/ })).toHaveCount(0);
  await expect(page.getByRole("heading", { level: 3 })).toHaveCount(0);
  await expect(page.getByText("Usage this period")).toHaveCount(0);
  await expect(page.getByText(/pool/)).toHaveCount(0);
});

test("a private repository whose organization is refusing transfer says so, with the way out", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/orgs/acme/billing", (r) =>
    r.fulfill({
      json: {
        ...POOLED,
        meters: {
          ...POOLED.meters,
          egress_gb: {
            ...POOLED.meters.egress_gb,
            used: 50,
            remaining: 0,
            refusing: true,
          },
        },
      },
    }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ json: { ...REPOS.repos[0], stored_bytes: 1_200_000_000 } }),
  );
  await openWidget(page);
  const wall = page.getByRole("region", { name: "Spend limit reached" });
  await expect(wall).toContainText(
    "Clones of private repositories are refused at the spend limit",
  );
  await expect(wall).toContainText(
    "This organization has transferred its 50 GB out of private repositories this period, and its spend limit is $0. Public repositories are unaffected.",
  );
  // The way out, for somebody who may take it: straight to the box.
  await expect(
    wall.getByRole("link", { name: "Raise the spend limit", exact: true }),
  ).toHaveAttribute("href", /\/dashboard\/settings\/billing#spend-limit$/);
  // Only one wall: storage is fine.
  await expect(page.getByRole("region", { name: "Spend limit reached" })).toHaveCount(1);
  // What this repository holds, in the units the pool is sold in.
  //
  // A tile now rather than the sentence "Stored: 1.1 GB": the figure
  // moved off the dashboard's repo screen onto the repository's own
  // Insights tab, where it sits beside the traffic it is the other half
  // of. Asserted as label-and-value so it cannot pass on a tile that
  // renders the number under the wrong heading.
  const stored = page
    .locator("div")
    .filter({ hasText: /^Stored1\.1 GB/ })
    .last();
  await expect(stored).toBeVisible();
});

test("the storage wall names the space held, and a viewer who cannot raise the limit is told who can", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/orgs/acme/billing", (r) =>
    r.fulfill({
      json: {
        ...OVER,
        may_raise_spend_limit: false,
        meters: {
          ...OVER.meters,
          minutes: METER,
          storage_gb: {
            ...OVER.meters.storage_gb,
            used: 25,
            remaining: 0,
            refusing: true,
          },
        },
      },
    }),
  );
  await openWidget(page);
  const wall = page.getByRole("region", { name: "Spend limit reached" });
  await expect(wall).toContainText(
    "This push would take private storage past the spend limit",
  );
  await expect(wall).toContainText(
    "Private repositories here hold 25 of 25 GB, and the spend limit is $25. The push was refused and nothing already stored was touched. Free space, or raise the limit.",
  );
  await expect(wall.getByRole("link", { name: "Raise the spend limit" })).toHaveCount(0);
  await expect(wall).toContainText(
    "Only an owner or admin can raise it, in Settings → Billing.",
  );
  // No size line without a measurement — never "0 B".
  await expect(page.getByText(/^Stored:/)).toHaveCount(0);
});

test("a public repository is never walled, and a viewer billing refuses is not either", async ({
  page,
}) => {
  await mockApi(page);
  // Every pool refusing — and none of it applies to a public repository.
  await page.route("**/v1/orgs/acme/billing", (r) =>
    r.fulfill({
      json: {
        ...CAPPED,
        meters: {
          minutes: { ...CAPPED.meters.minutes },
          egress_gb: { ...CAPPED.meters.egress_gb, refusing: true },
          storage_gb: { ...CAPPED.meters.storage_gb, refusing: true },
        },
      },
    }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ json: { ...REPOS.repos[0], public: true } }),
  );
  await openWidget(page);
  await expect(page.getByText("Public", { exact: true })).toBeVisible();
  await expect(page.getByRole("region", { name: "Spend limit reached" })).toHaveCount(0);

  // A member who is not an admin: billing answers 403, and the page
  // is the page, with nothing said — the same swallow the seat line
  // does.
  await page.route("**/v1/orgs/acme/billing", (r) =>
    r.fulfill({ status: 403, json: { error: "admin only" } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ json: REPOS.repos[0] }),
  );
  await openWidget(page);
  await expect(page.getByText("Private", { exact: true })).toBeVisible();
  await expect(page.getByRole("region", { name: "Spend limit reached" })).toHaveCount(0);
  await expect(page.getByText("Requests absorbed")).toBeVisible();
});

test("the overview tiles read the metered day, and say nothing where the server sent nothing", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/orgs/acme/usage", (r) =>
    r.fulfill({
      json: {
        ...USAGE,
        days: [
          {
            ...USAGE.days[0],
            hosted_minutes: 42,
            private_bytes_out: 1_500_000,
            private_bytes_stored: 2_300_000_000,
          },
          {
            ...USAGE.days[1],
            hosted_minutes: 7,
            private_bytes_out: 0,
            private_bytes_stored: 2_100_000_000,
          },
        ],
      },
    }),
  );
  await page.goto("/dashboard/");
  const tile = (label: string) =>
    page.getByText(label, { exact: true }).locator("..");
  await expect(tile("Private transfer today")).toContainText("1.4 MB");
  await expect(tile("Private transfer today")).toContainText(
    "public traffic is never counted",
  );
  await expect(tile("Hosted minutes today")).toContainText("42");
  await expect(tile("Stored, private")).toContainText("2.1 GB");
  await expect(page.getByText("Egress today")).toHaveCount(0);
  await expect(
    page.getByRole("img", { name: "Hosted minutes per day", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("img", { name: "Private storage per day", exact: true }),
  ).toBeVisible();

  // The older server: the tiles are there, empty rather than zero,
  // and neither chart is drawn — a flat line of zeros is a claim.
  await page.route("**/v1/orgs/acme/usage", (r) => r.fulfill({ json: USAGE }));
  await page.goto("/dashboard/");
  await expect(page.getByText("Requests today")).toBeVisible();
  for (const label of ["Private transfer today", "Hosted minutes today", "Stored, private"]) {
    await expect(tile(label)).toContainText("—");
    await expect(tile(label)).not.toContainText(/\d/);
  }
  await expect(page.getByRole("img", { name: "Hosted minutes per day" })).toHaveCount(0);
  await expect(page.getByRole("img", { name: "Private storage per day" })).toHaveCount(0);
  await expect(page.getByRole("img", { name: "Requests per day", exact: true })).toBeVisible();
});

/// The walls where people actually land. The metrics page above is
/// reached only by clicking a row; a link to a private repository
/// opens the file browser at `/{owner}/<name>` or the public
/// forge at `/<owner>/<name>`, and both showed only the tree at the cap
/// — found on a real stack, with billing saying `refusing: true` and
/// the page saying nothing.

/// The organization with its transfer pool refusing.
const TRANSFER_REFUSED = {
  ...POOLED,
  meters: {
    ...POOLED.meters,
    egress_gb: { ...POOLED.meters.egress_gb, used: 50, remaining: 0, refusing: true },
  },
};

/// The reads the file browser makes at a repository's root. Enough of
/// `mockBrowse` for the tree to render; every other read 404s under
/// the catch-all, which is the point.
async function mockTree(page: Page) {
  await page.route("**/v1/orgs/*/repos/widget/tree*", (r) =>
    r.fulfill({
      json: {
        commit: "abc1234567890abc1234567890abc1234567890a",
        entries: [
          { name: "README.md", mode: "100644", kind: "blob", oid: "b1", size: 21 },
        ],
      },
    }),
  );
  await page.route("**/v1/orgs/*/repos/widget/branches*", (r) =>
    r.fulfill({
      json: {
        branches: [
          { name: "main", full: "refs/heads/main", oid: "abc", default: true },
        ],
        head: "refs/heads/main",
      },
    }),
  );
  await page.route("**/v1/orgs/*/repos/widget/log*", (r) =>
    r.fulfill({
      json: {
        entries: [
          {
            commit: "abc1234567890abc1234567890abc1234567890a",
            message: "first commit",
            author: "Ada Owner <ada@acme.test> 1787406946 +0000",
            committer: "Ada Owner <ada@acme.test> 1787406946 +0000",
            parents: [],
            tree: "t0",
          },
        ],
        next_after: null,
      },
    }),
  );
}

test("the file browser walls a private repository at the cap, above the tree", async ({
  page,
}) => {
  await mockApi(page);
  await mockTree(page);
  await page.route("**/v1/orgs/acme/billing", (r) =>
    r.fulfill({ json: TRANSFER_REFUSED }),
  );
  // The browser holds no row, so it reads visibility itself: private.
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ json: REPOS.repos[0] }),
  );
  await page.goto("/acme/widget");
  const wall = page.getByRole("region", { name: "Spend limit reached" });
  await expect(wall).toContainText(
    "Clones of private repositories are refused at the spend limit",
  );
  await expect(
    wall.getByRole("link", { name: "Raise the spend limit", exact: true }),
  ).toHaveAttribute("href", /\/dashboard\/settings\/billing#spend-limit$/);
  // Above the tree, and the tree is still there.
  const tree = page.getByRole("button", { name: "README.md", exact: true });
  await expect(tree).toBeVisible();
  const wallY = (await wall.boundingBox())!.y;
  const treeY = (await tree.boundingBox())!.y;
  expect(wallY).toBeLessThan(treeY);
  await expect(page.getByRole("region", { name: "Spend limit reached" })).toHaveCount(1);
});

test("the file browser never walls a public repository, whatever billing says", async ({
  page,
}) => {
  await mockApi(page);
  await mockTree(page);
  await page.route("**/v1/orgs/acme/billing", (r) =>
    r.fulfill({ json: TRANSFER_REFUSED }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ json: { ...REPOS.repos[0], public: true } }),
  );
  await page.goto("/acme/widget");
  await expect(
    page.getByRole("button", { name: "README.md", exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("region", { name: "Spend limit reached" })).toHaveCount(0);
});

/// The public forge with nothing mocked but this page's reads. The
/// catch-all is registered first, so the specifics below win.
async function forge(
  page: Page,
  opts: { signedIn: boolean; row: unknown; billing: unknown },
) {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    opts.signedIn
      ? r.fulfill({ json: ME })
      : r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ json: opts.row }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/meta", (r) =>
    r.fulfill({
      json: {
        topics: ["git"],
        languages: [{ name: "Rust", bytes: 6000 }],
        languages_truncated: false,
        license: null,
        community: [],
      },
    }),
  );
  await page.route("**/v1/orgs/acme/billing", (r) =>
    opts.billing === null
      ? r.fulfill({ status: 404, json: { error: "not found" } })
      : r.fulfill({ json: opts.billing }),
  );
  await mockTree(page);
}

test("the forge walls an owner's private repository at the cap, on the row it holds", async ({
  page,
}) => {
  await forge(page, {
    signedIn: true,
    row: { ...REPOS.repos[0], viewer_admin: true, public: false },
    billing: TRANSFER_REFUSED,
  });
  await page.goto("/acme/widget");
  const wall = page.getByRole("region", { name: "Spend limit reached" });
  await expect(wall).toContainText(
    "Clones of private repositories are refused at the spend limit",
  );
  await expect(
    wall.getByRole("link", { name: "Raise the spend limit", exact: true }),
  ).toBeVisible();
  // Once, over the page — not again inside the Code tab's browser.
  await expect(page.getByRole("region", { name: "Spend limit reached" })).toHaveCount(1);
  await expect(page.getByRole("link", { name: "Code", exact: true })).toBeVisible();
});

test("a stranger on the forge is never walled, and the page is the page", async ({
  page,
}) => {
  // The repository is public to a stranger, and their billing read is
  // a masked refusal: neither reaches the wall, and nothing else on
  // the page is disturbed by the attempt.
  await forge(page, {
    signedIn: false,
    row: { ...REPOS.repos[0], viewer_admin: false, public: true },
    billing: null,
  });
  await page.goto("/acme/widget");
  await expect(page.getByRole("link", { name: "Code", exact: true })).toBeVisible();
  await expect(
    page.getByRole("button", { name: "README.md", exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("region", { name: "Spend limit reached" })).toHaveCount(0);
});
