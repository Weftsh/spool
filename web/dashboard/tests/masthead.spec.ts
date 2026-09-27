// The repository identity row: who owns this, what it is called,
// whether a stranger can read it, and the three controls on the right.
//
// This is the four seconds a migrating project judges us in, and it is
// specified in `web/FORGE-UX.md` §1 — avatar, `owner / repo`, a
// visibility pill, a "Forked from" line under the name, and the action
// group right-aligned on the same line. What shipped before this was a
// single wrapping row with no avatar and no pill, whose "forked from"
// line was a full-width flex child that landed wherever the widths fell
// out, and whose three controls were three separately-drawn buttons of
// two different heights.
//
// The visibility pill is the one worth being careful about: it must
// never say "Public" about a repository that is not, including for the
// moment before the row has arrived.

import { expect, test, type Page } from "@playwright/test";
import { REPOS } from "./fixtures";

const widget = {
  ...REPOS.repos[0],
  org: "acme",
  public: true,
  fork_count: 3,
  watcher_count: 12,
};

/// A signed-out visitor on a repository page, with only what the test
/// names mocked. The catch-all goes first — Playwright matches the
/// most recently registered handler, and an unmocked `/v1` call
/// otherwise proxies to whatever is listening on :8080.
async function visit(page: Page, repo: Record<string, unknown>) {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route(`**/v1/orgs/acme/repos/${repo.name}`, (r) =>
    r.fulfill({ status: 200, json: repo }),
  );
  await page.goto(`/acme/${repo.name}`);
}

test("a public repository says so, next to its name", async ({ page }) => {
  await visit(page, widget);
  await expect(page.getByText("Public", { exact: true })).toBeVisible();
  await expect(page.getByText("Private", { exact: true })).toHaveCount(0);
});

test("a private repository is never described as public, at any moment", async ({
  page,
}) => {
  // The pill renders only once the row has arrived, so there is no
  // frame in which a default is on screen. A `?? true` would have made
  // a private repository announce itself as public for as long as the
  // read took, which is the one direction this pill must not be wrong
  // in — and it is invisible in a fast local test unless the assertion
  // runs before the response, so the response is held.
  let release = () => {};
  const held = new Promise<void>((r) => (release = r));
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/orgs/acme/repos/ledger", async (r) => {
    await held;
    return r.fulfill({
      status: 200,
      json: { ...widget, name: "ledger", public: false },
    });
  });
  const nav = page.goto("/acme/ledger");

  // Wait until the masthead is actually on screen with the row still in
  // flight. Without this the assertion below can run before the bundle
  // has executed at all, and "no Public on screen" is then true of a
  // blank page — which is how this test passed against the very default
  // it exists to forbid.
  await expect(
    page.getByRole("link", { name: "acme", exact: true }).first(),
  ).toBeVisible();

  // While the row is in flight: no claim either way.
  await expect(page.getByText("Public", { exact: true })).toHaveCount(0);
  release();
  await nav;
  await expect(page.getByText("Private", { exact: true })).toBeVisible();
  await expect(page.getByText("Public", { exact: true })).toHaveCount(0);
});

test("a fork says whose history it carries, under the name and not beside it", async ({
  page,
}) => {
  await visit(page, { ...widget, fork_parent: "upstream/widget" });

  const line = page.getByText(/Forked from/);
  await expect(line).toBeVisible();
  await expect(
    line.getByRole("link", { name: "upstream/widget" }),
  ).toHaveAttribute("href", "/upstream/widget");

  // Under the repo name, which is what "under" has to mean in pixels:
  // the line's top edge is below the name link's. As a full-width child
  // of one wrapping flex row this depended on the widths of the three
  // controls to its right, and a narrow viewport put it above them.
  const name = page.getByRole("link", { name: "widget", exact: true }).first();
  const a = await name.boundingBox();
  const b = await line.boundingBox();
  expect(a && b && b.y).toBeGreaterThan(a!.y);
});

test("the three controls sit on one line, right of the name", async ({
  page,
}) => {
  await visit(page, widget);

  const name = page.getByRole("link", { name: "widget", exact: true }).first();
  const nameBox = await name.boundingBox();
  for (const label of [/^Sign in to watch/, /^Sign in to fork/]) {
    const box = await page.getByRole("button", { name: label }).boundingBox();
    expect(box && box.x).toBeGreaterThan(nameBox!.x);
  }
});

test("every control in the identity row has an accessible name", async ({
  page,
}) => {
  await visit(page, widget);
  // The same rule the walkthrough's `audit()` enforces on every stage:
  // a `<button>` with neither text nor an `aria-label` is invisible to
  // a screen reader and to anybody driving this page by keyboard.
  const unlabelled = await page.evaluate(
    () =>
      [...document.querySelectorAll("header button")].filter(
        (b) => !(b.textContent ?? "").trim() && !b.getAttribute("aria-label"),
      ).length,
  );
  expect(unlabelled).toBe(0);
});

test("a long repository name ellipsises rather than widening the page", async ({
  page,
}) => {
  // `min-w-0` on the identity column is not decoration. Without it a
  // long name pushes the action group off the right edge and the
  // walkthrough fails the build on `documentElement.scrollWidth`.
  await page.setViewportSize({ width: 640, height: 900 });
  await visit(page, {
    ...widget,
    name: "a-repository-with-a-deliberately-and-extremely-long-name-for-this",
  });
  const overflow = await page.evaluate(
    () =>
      document.documentElement.scrollWidth -
      document.documentElement.clientWidth,
  );
  expect(overflow).toBeLessThanOrEqual(1);
});

test("the repository is named once at its root, and again only as a path", async ({
  page,
}) => {
  // The masthead names the repository. The file listing's breadcrumb
  // root names it too, and at the repository root that is the whole of
  // the breadcrumb — so the name rendered twice, in the same weight,
  // twelve pixels apart, the lower one a button that navigated to the
  // page it was already on. Two identical names stacked reads as a
  // rendering fault.
  await visit(page, widget);
  await expect(
    page.getByRole("button", { name: "widget", exact: true }),
  ).toHaveCount(0);
  // And it is still named — this is not a test that passes on a blank
  // page.
  await expect(
    page.getByRole("link", { name: "widget", exact: true }).first(),
  ).toBeVisible();

  // Inside a directory the root becomes a real destination and earns
  // its place back, which is exactly GitHub's rule.
  await page.goto("/acme/widget/tree/src");
  await expect(
    page.getByRole("button", { name: "widget", exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("navigation", { name: "Path" })).toBeVisible();
});
