// The code view's front page and file page, after the speed work of
// September 2026: what the listing shows, where the commit count comes
// from, and how prose opens.
//
// Every number and column here used to be fetched or derived by the
// browser itself — the commit count from a thousand-entry log, the size
// column from one blob read per file — and on the production mirror the
// two together were most of a twenty-second wait for a page GitHub draws
// in under a second. The row carries the count now and the listing has
// no size column, so these tests are about what the page does with what
// it is handed, and about what it must no longer print.

import { expect, test, type Page } from "@playwright/test";
import { REPOS } from "./fixtures";

const widget = REPOS.repos[0];

/// Refuse anything a test has not deliberately mocked; the specific
/// routes registered after this win. Same helper as `public.spec.ts`,
/// which cannot be imported from here without registering its tests a
/// second time.
async function hermetic(page: Page) {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
}

/// A signed-out visitor on the public forge, with the front page's four
/// reads mocked: the row, the branches, the head commit and the listing.
/// `head` is the commit every read agrees on; `row` overrides the
/// repository row, which is where the commit count arrives.
async function frontPage(
  page: Page,
  opts: { head: string; row: Record<string, unknown> },
) {
  await hermetic(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: { ...widget, public: true, ...opts.row } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/branches*", (r) =>
    r.fulfill({
      json: {
        branches: [
          { name: "main", full: "refs/heads/main", oid: opts.head, default: true },
        ],
        head: "refs/heads/main",
      },
    }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/log*", (r) =>
    r.fulfill({
      json: {
        entries: [
          {
            commit: opts.head,
            message: "teach the parser about tabs",
            author: "Ada Owner <ada@acme.test> 1787406946 +0000",
            committer: "Ada Owner <ada@acme.test> 1787406946 +0000",
            parents: [],
            tree: "cccc1111",
          },
        ],
        next_after: null,
      },
    }),
  );
  await page.route(
    (u) => /\/repos\/widget\/tree$/.test(u.pathname),
    (r) =>
      r.fulfill({
        json: {
          commit: opts.head,
          entries: [
            { name: "src", mode: "40000", kind: "tree", oid: "1".repeat(40), size: null },
            // A size on the wire, to prove the table does not print one.
            { name: "GUIDE.md", mode: "100644", kind: "blob", oid: "2".repeat(40), size: 4096 },
          ],
        },
      }),
  );
}

test("the commit count is the row's, printed only beside the head it was counted from", async ({
  page,
}) => {
  // The bar used to count for itself: the log, limit a thousand,
  // clamped to five hundred first-parent commits by the server — nine
  // seconds on the production mirror for "119 commits" on a repository
  // with 483. The number is the row's now, taken by the job that follows
  // a write, and the row says which commit it was true for.
  const head = "a".repeat(40);
  await frontPage(page, {
    head,
    row: { commits: 483, commits_tip: head, commits_exact: true },
  });
  await page.goto("/acme/widget");
  await expect(page.getByRole("link", { name: "483 commits" })).toBeVisible();
  // No size column: the listing reads no blob for it any more, and the
  // fixture's 4096 must not surface as "4.0 KiB".
  await expect(page.getByRole("button", { name: "GUIDE.md" })).toBeVisible();
  await expect(page.getByText(/KiB/)).toHaveCount(0);

  // A count a push behind is not printed as current. The link to the
  // history stays; the number goes.
  await frontPage(page, {
    head,
    row: { commits: 483, commits_tip: "b".repeat(40), commits_exact: true },
  });
  await page.goto("/acme/widget");
  await expect(
    page.getByRole("link", { name: "commits", exact: true }),
  ).toBeVisible();
  await expect(page.getByText("483")).toHaveCount(0);

  // A walk that hit its cap says the number is a floor.
  await frontPage(page, {
    head,
    row: { commits: 100000, commits_tip: head, commits_exact: false },
  });
  await page.goto("/acme/widget");
  await expect(
    page.getByRole("link", { name: "100,000+ commits" }),
  ).toBeVisible();
});

test("a markdown file opens on its preview, with the source one tab away", async ({
  page,
}) => {
  // Somebody who clicked GUIDE.md wanted to read it, not its markup.
  // Reached by clicking rather than by address, because the preview
  // server hands a dotted path to its static file server.
  const head = "a".repeat(40);
  await frontPage(page, { head, row: {} });
  await page.route("**/v1/orgs/acme/repos/widget/files/GUIDE.md*", (r) =>
    r.fulfill({
      status: 200,
      headers: {
        "content-type": "text/markdown; charset=utf-8",
        "x-weft-binary": "false",
        "x-weft-commit": head,
      },
      body: "# Getting started\n\nRead *this* first.\n",
    }),
  );
  await page.route(
    (u) =>
      /\/repos\/widget\/tree$/.test(u.pathname) &&
      u.searchParams.get("recursive") === "1",
    (r) =>
      r.fulfill({
        json: { commit: head, paths: ["GUIDE.md", "src/"], truncated: false },
      }),
  );
  await page.goto("/acme/widget");
  await page.getByRole("button", { name: "GUIDE.md" }).click();
  await expect(
    page.getByRole("heading", { name: "Getting started" }),
  ).toBeVisible();
  const tabs = page.getByRole("tablist", { name: "File view" });
  await expect(tabs.getByRole("tab", { name: "Preview" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  await tabs.getByRole("tab", { name: "Code" }).click();
  // The source, as written: the heading marker is markup again.
  await expect(page.getByText("# Getting started")).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Getting started" }),
  ).toHaveCount(0);
});
