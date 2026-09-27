// The public forge, as a signed-out stranger sees it.
//
// This is the suite that proves the shell split actually happened. The
// dashboard's chrome — the collapsible rail, the org switcher, the
// settings sections — is for people who are signed in and belong
// somewhere. A visitor following a link to a public repository belongs
// nowhere, has no namespace selected, and must still get a page rather
// than a login form. Asserting the *absence* of the rail is the point:
// a forge page that quietly rendered inside the admin shell would look
// almost right and be wrong in a way nobody notices until a stranger
// complains they were asked to sign in to read open source.

import { expect, test, type Page } from "@playwright/test";
import { REPOS } from "./fixtures";

const widget = REPOS.repos[0];

/// A signed-out browser: the boot probe must answer "nobody", and the
/// public reads must answer anyway. Both halves matter — a mock that
/// only did the first would prove the login form appears, which is the
/// opposite of what this file is about.
/// Refuse anything a test has not deliberately mocked.
///
/// Registered **first**, so the specific routes a test adds afterwards
/// win — Playwright matches the most recently registered handler.
///
/// Without this an unmocked `/v1` call goes through vite's proxy to
/// whatever is listening on :8080, which during development is a real
/// seeded server. The suite is then hermetic only while nobody has the
/// manual stack up, and a test passes or fails depending on that.
///
/// It is a named helper rather than four copies because the failure is
/// *silent* and arrives from a distance: a page gaining one new fetch
/// makes every test that renders it non-hermetic at once, and none of
/// them fail to say so. That is how `/meta` and `/users/:h/contributions`
/// both leaked into tests written long before either existed.
export async function hermetic(page: Page) {
  await page.route("**/v1/**", (r) =>
    r.fulfill({
      status: 404,
      json: { error: "not mocked by this test" },
    }),
  );
}

async function anonymous(page: import("@playwright/test").Page) {
  await hermetic(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: { ...widget, public: true } }),
  );
  // The profile reads search, which is the endpoint built to answer a
  // stranger; the namespace listing is protected and stays that way.
  await page.route("**/v1/search/repos*", (r) =>
    r.fulfill({
      status: 200,
      json: { repos: [{ ...widget, org: "acme", public: true }], next: null },
    }),
  );
}

test("a public repository opens for somebody with no account", async ({
  page,
}) => {
  await anonymous(page);
  await page.goto("/acme/widget");
  // The masthead link specifically — the code browser renders the repo
  // name again in its breadcrumb, and a bare text match would pass on
  // either, which would make this test unable to tell the forge header
  // from the browser it wraps.
  await expect(
    page.getByRole("link", { name: "widget", exact: true }).first(),
  ).toBeVisible();
  // Not the login form. The whole product claim is that open source is
  // readable without an account.
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toHaveCount(0);
});

test("the forge wears no dashboard chrome", async ({ page }) => {
  await anonymous(page);
  await page.goto("/acme/widget");
  // The rail's trigger is the cheapest proof the admin shell is not
  // mounted: it exists on every dashboard page and on none of these.
  await expect(
    page.getByRole("button", { name: "Toggle sidebar" }),
  ).toHaveCount(0);
});

test("signing in from a public page comes back to it", async ({ page }) => {
  await anonymous(page);
  await page.goto("/acme/widget");
  // An invitation to sign in must not be a one-way door: sending
  // somebody to a login form and then dropping them on a dashboard they
  // did not ask for loses the thing they were reading.
  await expect(page.getByRole("link", { name: "Sign in" })).toHaveAttribute(
    "href",
    "/login?next=%2Facme%2Fwidget",
  );
});

test("a namespace has a public face", async ({ page }) => {
  await anonymous(page);
  await page.goto("/acme");
  await expect(
    page.getByRole("heading", { name: "acme", level: 1 }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: /widget/ }).first(),
  ).toBeVisible();
});

test("a namespace a stranger may not read is empty, not broken", async ({
  page,
}) => {
  await hermetic(page);
  // A private organization seen from outside should look like a profile
  // with nothing on it. Rendering a red error box instead would tell a
  // stranger that the namespace exists *and* that they were refused,
  // and it would read as the platform being broken rather than as the
  // org being private.
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  // A namespace with nothing published looks like a profile with
  // nothing on it. Rendering an error box would tell a stranger the
  // namespace exists *and* that they were refused, and would read as the
  // platform being broken rather than as the namespace being empty.
  await page.route("**/v1/search/repos*", (r) =>
    r.fulfill({ status: 200, json: { repos: [], next: null } }),
  );
  await page.goto("/acme");
  await expect(page.getByText("Nothing public here yet.")).toBeVisible();
  await expect(page.getByText(/not signed in/)).toHaveCount(0);
});

test("a refused repository is not asked for twice", async ({ page }) => {
  await hermetic(page);
  // The code browser guesses directory-or-file, because a URL does not
  // say which, and retries the other way when it guesses wrong. Being
  // refused is not a wrong guess: retrying spends a second request to be
  // refused again, and it buries the server's honest 401 under a 404
  // that reads as a missing file rather than a private repository.
  // The manual browser pass found this on a stranger opening a private
  // repo's address.
  const asked: string[] = [];
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/orgs/acme/repos/ledger/**", (r) => {
    asked.push(new URL(r.request().url()).pathname);
    return r.fulfill({ status: 401, json: { error: "not signed in" } });
  });
  await page.goto("/acme/ledger");
  // A repository the viewer may not see renders as not-found now, so
  // that is the observable to wait on before asking what was requested —
  // an absence checked before the page has settled is an absence that
  // passes for the wrong reason.
  await expect(page.getByText("404", { exact: true })).toBeVisible();
  // The tree's own path, and only that. The README is fetched
  // separately by the About/Readme pair and is a different question, not
  // a retry of this one — asserting on every `/files/` request would
  // make this test fail the moment the page legitimately reads a file,
  // which is not the defect it exists to hold.
  expect(
    asked.filter((p) => p.endsWith("/files/")),
    `the browser re-asked for the refused path as a file: ${asked.join(", ")}`,
  ).toHaveLength(0);
});

// ---------------------------------------------------------------------
// The five defects a person found by opening this in a browser, each
// pinned so it cannot come back quietly. None of them were visible to
// the suites that existed: every one is about what a *stranger* sees,
// and nothing had ever looked.

test("every tab on a repository page leads somewhere", async ({ page }) => {
  // Four tabs shipped that all answered 404 — and the not-found copy
  // offers to sign you in, because the ordinary reason a page is missing
  // here is that it is private. So a visitor was invited to authenticate
  // for features that do not exist and never would appear.
  //
  // The rule this enforces is `FORGE-UX.md`'s: a tab that leads nowhere
  // is worse than an absent one. Add a tab when its body exists, and
  // this test will let you.
  await anonymous(page);
  await page.goto("/acme/widget");
  const tabs = page
    .getByRole("navigation", { name: "Repository" })
    .getByRole("link");
  // Wait for the **repository row**, not for the first tab.
  //
  // `evaluateAll` does not auto-wait: it resolves with whatever matches
  // at that instant, zero included, and never retries. So the read
  // below has to be anchored on something. An earlier version of this
  // waited on `tabs.first()` and claimed in a comment that this was
  // "waiting on the thing the read actually depends on". That was true
  // when every tab came from a static array, and it stopped being true
  // the moment a tab's presence depended on a fetch: Code, Issues and
  // Changes render immediately, so the wait is satisfied by the
  // **loading** state and the read samples a strip that is still one
  // tab short of what the page will show.
  //
  // The general rule, and it is not about absences: a web-first
  // assertion passes at the first instant it holds, so any assertion
  // satisfied by the loading state passes regardless of what the loaded
  // state does. Asserting the positive does not help — "exactly these
  // three tabs" is also true, briefly, of a page that is about to have
  // four. Anchor on an observable that can only come from the fetch.
  //
  // The fork count is that observable here: it is drawn from the
  // repository row and from nothing else.
  await expect(
    page.getByRole("button", { name: /fork this repository|sign in to fork/i }),
  ).toBeVisible();
  // Read every destination *first*, then visit them. Clicking and going
  // back re-renders the strip under the loop, so the second read raced
  // the render and timed out on a locator that was about to exist. The
  // property under test is "each tab leads somewhere", which does not
  // depend on how you get there.
  const targets = await tabs.evaluateAll((links) =>
    links.map((a) => ({
      label: (a.textContent ?? "").trim(),
      href: a.getAttribute("href") ?? "",
    })),
  );
  expect(
    targets.length,
    "the repository page rendered no tabs at all",
  ).toBeGreaterThan(0);
  for (const { label, href } of targets) {
    await page.goto(href);
    // Anchor on something positive from this render *before* asserting
    // an absence. `toHaveCount(0)` is true of a page that has not drawn
    // yet, so an absence checked straight after navigation can pass for
    // the wrong reason — and can fail for one too, if the assertion
    // lands while the shell is between states. Waiting for the strip
    // means the page is really here when the question is asked.
    await expect(
      page.getByRole("navigation", { name: "Repository" }),
    ).toBeVisible();
    await expect(
      page.getByText("404", { exact: true }),
      `the "${label}" tab leads to a not-found page`,
    ).toHaveCount(0);
  }
});

test("a stranger can find out how to clone", async ({ page }) => {
  // The one thing a public repository page is for. The clone address
  // lived in the dashboard's own repo screen, behind a sign-in, and
  // nowhere a visitor could reach.
  await anonymous(page);
  await page.goto("/acme/widget");
  // Behind the Code button now, where GitHub puts it and where a hand
  // goes looking — not stacked and truncated in a side panel. The test
  // follows the affordance rather than reaching past it: if the button
  // stops opening the panel then a visitor cannot clone, and that is
  // exactly what should fail here.
  await page.getByRole("button", { name: "Code" }).click();
  await expect(
    page.getByRole("textbox", { name: "HTTPS clone URL" }),
  ).toHaveValue(/^https?:\/\/.+\.git$/);
});

test("an empty repository says so, and tells a writer how to fill it", async ({
  page,
}) => {
  // Found walking the plan boundaries by hand: a fork of a repository
  // nothing had been pushed to, and a repository created a moment
  // earlier, both opened on a red "404" under their own name. The tree
  // read 404s because there is no tree; the page took that as an error
  // about the repository rather than the shape of an empty one.
  //
  // Emptiness is decided by the ref list, not by the 404: a path that
  // is not there inside a repository with commits is still an error.
  await anonymous(page);
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({
      status: 200,
      json: {
        ...widget,
        public: true,
        viewer_write: true,
        clone_url: "http://forge.test/acme/widget.git",
        default_branch: "main",
      },
    }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/branches", (r) =>
    r.fulfill({ json: { branches: [], head: "refs/heads/main" } }),
  );
  await page.goto("/acme/widget");
  await expect(page.getByText("This repository is empty.")).toBeVisible();
  await expect(page.getByText(/^404$/)).toHaveCount(0);
  await expect(page.getByRole("alert")).toHaveCount(0);
  // The commands carry this repository's own address and branch.
  await expect(page.locator("pre")).toContainText(
    "git remote add origin http://forge.test/acme/widget.git",
  );
  await expect(page.locator("pre")).toContainText("git push -u origin main");
});

test("a repository nobody may write to says so, on every tab", async ({
  page,
}) => {
  // A private repository on an organization whose subscription had
  // ended looked exactly like every other repository — Code, Changes,
  // the clone button — until `git push` answered 402. The server's
  // `write_blocked` is that refusal; the page shows it to every reader
  // on every tab, without the `quota:` tag git gets.
  const REASON =
    "quota: this repository is private and the organization's subscription has ended — everything here is still readable; subscribe from Billing, or make the repository public, to write to it again";
  await anonymous(page);
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({
      status: 200,
      json: {
        ...widget,
        public: true,
        viewer_write: false,
        viewer_admin: false,
        write_blocked: REASON,
      },
    }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/branches", (r) =>
    r.fulfill({ json: { branches: [], head: "refs/heads/main" } }),
  );
  await page.goto("/acme/widget");
  const notice = page.getByTestId("read-only-notice");
  await expect(notice).toContainText("This repository is read-only.");
  await expect(notice).toContainText(
    "This repository is private and the organization's subscription has ended",
  );
  await expect(notice).not.toContainText("quota:");
  // A stranger cannot fix it and is told who can; no link to a page
  // they could not open.
  await expect(notice).toContainText("An owner of the organization can");
  await expect(notice.getByRole("link")).toHaveCount(0);
  // Not an alert: nothing is broken, and the empty-repository tests
  // assert `alert` is absent for exactly that reason.
  await expect(page.getByRole("alert")).toHaveCount(0);
  // Still there on the other tabs — the fact is about the repository.
  await page.goto("/acme/widget/changes");
  await expect(page.getByTestId("read-only-notice")).toBeVisible();

  // An administrator gets the way out.
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({
      status: 200,
      json: {
        ...widget,
        public: true,
        viewer_write: false,
        viewer_admin: true,
        write_blocked: REASON,
      },
    }),
  );
  await page.goto("/acme/widget");
  await expect(
    page
      .getByTestId("read-only-notice")
      .getByRole("link", { name: "Open Billing" }),
  ).toHaveAttribute("href", "/dashboard/settings/billing");

  // And a repository that may be written to carries no notice at all.
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({
      status: 200,
      json: {
        ...widget,
        public: true,
        viewer_write: true,
        write_blocked: null,
      },
    }),
  );
  await page.goto("/acme/widget");
  await expect(page.getByText("This repository is empty.")).toBeVisible();
  await expect(page.getByTestId("read-only-notice")).toHaveCount(0);
});

test("a reader of an empty repository is not handed commands they cannot run", async ({
  page,
}) => {
  await anonymous(page);
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({
      status: 200,
      json: { ...widget, public: true, viewer_write: false },
    }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/branches", (r) =>
    r.fulfill({ json: { branches: [], head: "refs/heads/main" } }),
  );
  await page.goto("/acme/widget");
  await expect(page.getByText("This repository is empty.")).toBeVisible();
  await expect(
    page.getByText("Nothing has been pushed to it yet."),
  ).toBeVisible();
  await expect(page.locator("pre")).toHaveCount(0);
});

test("a path that is not there is still an error, in a repository that has commits", async ({
  page,
}) => {
  // The empty notice must not swallow a real 404. With refs present the
  // root would list fine; a deep link to a path that does not exist
  // stays the error it is. (No file extension in the address: the
  // preview server hands dotted paths to the static file server.)
  await anonymous(page);
  await page.route("**/v1/orgs/acme/repos/widget/branches", (r) =>
    r.fulfill({
      json: {
        branches: [
          {
            name: "main",
            full: "refs/heads/main",
            oid: "a".repeat(40),
            default: true,
          },
        ],
        head: "refs/heads/main",
      },
    }),
  );
  await page.goto("/acme/widget/tree/no/such/path");
  await expect(page.getByRole("alert")).toBeVisible();
  await expect(page.getByText("This repository is empty.")).toHaveCount(0);
});

test("a public file page shows the tree beside the file and no About rail", async ({
  page,
}) => {
  // The same browser serves the forge, so a stranger reading a public
  // file gets the rail too. About steps aside off the root: a third
  // column beside a tree and a file would leave the code narrower than
  // a phone.
  await anonymous(page);
  const commit = "abc1234567890abc1234567890abc1234567890a";
  await page.route("**/v1/orgs/acme/repos/widget/branches*", (r) =>
    r.fulfill({
      json: {
        branches: [
          { name: "main", full: "refs/heads/main", oid: commit, default: true },
        ],
        head: "refs/heads/main",
      },
    }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/log*", (r) =>
    r.fulfill({ json: { entries: [], next_after: null } }),
  );
  // No extension in the address: the preview server hands dotted paths
  // to its static file server (see the 404 test above).
  await page.route("**/v1/orgs/acme/repos/widget/tree/LICENSE*", (r) =>
    r.fulfill({ status: 404, json: { error: '"LICENSE" is not a tree' } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/files/LICENSE*", (r) =>
    r.fulfill({
      status: 200,
      headers: {
        "content-type": "text/plain; charset=utf-8",
        "x-weft-binary": "false",
        "x-weft-commit": commit,
      },
      body: "MIT License\n",
    }),
  );
  await page.route(
    (u) =>
      /\/repos\/widget\/tree$/.test(u.pathname) &&
      u.searchParams.get("recursive") === "1",
    (r) =>
      r.fulfill({
        json: { commit, paths: ["LICENSE", "src/", "src/lib.rs"], truncated: false },
      }),
  );
  await page.goto("/acme/widget/tree/LICENSE");
  await expect(page.getByText("MIT License")).toBeVisible();
  await expect(
    page.getByRole("treeitem", { name: "LICENSE", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("heading", { name: "About" })).toHaveCount(0);
});

test("a repository page names itself in the browser", async ({ page }) => {
  // Every forge page was titled "Weft Dashboard" — the name of a
  // product this visitor has not signed in to and may never. It is what
  // they bookmark, what a shared tab says, and what a crawler indexes.
  await anonymous(page);
  await page.goto("/acme/widget");
  await expect(page).toHaveTitle("acme/widget");
  await page.goto("/acme");
  await expect(page).toHaveTitle("acme");
});

test("a count is absent rather than zero when there is nothing to count", async ({
  page,
}) => {
  // Stars and forks do not exist. Every repository card showed a star
  // and a fork reading 0, which is a promise of a feature: "nobody has
  // starred this" and "there is no such thing as starring here" are
  // different sentences and only one of them was true.
  await anonymous(page);
  await page.goto("/acme");
  const card = page.getByRole("link", { name: "widget" }).first();
  await expect(card).toBeVisible();
  await expect(
    page.getByText("stars", { exact: true }),
    "a star count appeared for a feature that does not exist",
  ).toHaveCount(0);
});

test("explore lists public repositories, and search narrows them", async ({
  page,
}) => {
  // `/explore` and `/search` were reserved server-side so no namespace
  // could shadow them, and the asset fallback refused reserved names —
  // two rules written hours apart that cancelled out, so both answered
  // 404 and the header's search box led nowhere at all.
  await anonymous(page);
  await page.goto("/explore");
  await expect(
    page.getByRole("heading", { name: "Public repositories" }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "widget" }).first(),
  ).toBeVisible();

  await page.getByRole("searchbox", { name: "Search Weft" }).fill("widget");
  await page.getByRole("searchbox", { name: "Search Weft" }).press("Enter");
  await expect(page).toHaveURL(/\/search\?q=widget/);
  await expect(
    page.getByRole("heading", { name: /Repositories matching/ }),
  ).toBeVisible();
});

test("explore does not call a member's private repositories public", async ({
  page,
}) => {
  // The search endpoint is viewer-scoped: a stranger gets the public
  // repositories, and somebody signed in gets theirs as well. The
  // heading and the line under it said "Public repositories" and
  // "Everything here is public and clonable without an account"
  // regardless — so a maintainer's private ledger was listed, correctly
  // badged Private, underneath a sentence promising anybody could clone
  // it. Nothing was ever exposed; they were just told it was, which is
  // the expensive half of a leak without the leak.
  await anonymous(page);
  await page.route("**/v1/search/repos*", (r) =>
    r.fulfill({
      status: 200,
      json: {
        repos: [
          { ...widget, org: "acme", name: "widget", public: true },
          {
            ...widget,
            id: "01ccc",
            org: "acme",
            name: "ledger",
            description: "private ledger work",
            public: false,
          },
        ],
        next: null,
      },
    }),
  );
  await page.goto("/explore");

  // The private one is on the page — this is not a test that it was
  // hidden, it is a test that the page stops lying about it.
  await expect(
    page.getByRole("link", { name: "ledger" }).first(),
  ).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Public repositories" }),
    "a list containing a private repository is still headed 'Public repositories'",
  ).toHaveCount(0);
  await expect(
    page.getByText(
      "Everything here is public and clonable without an account.",
    ),
    "a private repository is listed under a promise that everything here is clonable",
  ).toHaveCount(0);
  await expect(
    page.getByRole("heading", { name: "Repositories you can see" }),
  ).toBeVisible();
});

test("explore still says public when everything on it is", async ({ page }) => {
  // The control. Without it the fix above could be "never say public",
  // which loses the one sentence that tells a stranger they need no
  // account — the whole reason this page exists.
  await anonymous(page);
  await page.goto("/explore");
  await expect(
    page.getByRole("heading", { name: "Public repositories" }),
  ).toBeVisible();
  await expect(
    page.getByText(
      "Everything here is public and clonable without an account.",
    ),
  ).toBeVisible();
});

test("a repository a stranger may not see is not there at all", async ({
  page,
}) => {
  await hermetic(page);
  // The server masks a private repository so that its existence is not
  // something a URL can confirm. The page undid that: it drew the
  // repository's name, its tab strip and its document title, and then a
  // red box with the four characters `401` in it — which both leaks the
  // name and reads as our bug rather than as the repository being
  // private.
  //
  // Refused and missing are one answer here on purpose.
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/orgs/acme/repos/ledger", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/orgs/acme/repos/ledger/**", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.goto("/acme/ledger");

  await expect(page.getByText("404", { exact: true })).toBeVisible();
  await expect(
    page.getByRole("navigation", { name: "Repository" }),
    "the tab strip named a repository the viewer may not see",
  ).toHaveCount(0);
  await expect(page).not.toHaveTitle(/ledger/);
  await expect(
    page.getByText("401"),
    "a raw HTTP status was shown where a sentence belongs",
  ).toHaveCount(0);
});

test("the Sign in button on a public page actually signs you in", async ({
  page,
}) => {
  // It led to "We couldn't find that page" on every public forge page:
  // the route existed, `next` was carried and made safe, and nothing
  // rendered it. The one control offered to a signed-out visitor was
  // dead.
  await anonymous(page);
  await page.goto("/acme/widget");
  await page.getByRole("link", { name: "Sign in" }).click();
  await expect(page).toHaveURL(/\/login\?next=/);
  await expect(page.getByLabel("Email")).toBeVisible();
  await expect(page.getByText("404", { exact: true })).toHaveCount(0);
});

test("a commit is somewhere you can go", async ({ page }) => {
  // Four places printed an abbreviated sha with nowhere to go, which is
  // what makes people leave a forge and read the history locally. The
  // bar, the commits list, the file table's last-commit cells and the
  // file view all point here now.
  await anonymous(page);
  const sha = "aaaa111122223333444455556666777788889999";
  await page.route("**/v1/orgs/acme/repos/widget/log*", (r) =>
    r.fulfill({
      status: 200,
      json: {
        entries: [
          {
            commit: sha,
            message: "teach the parser about tabs\n\nlong body here",
            author: "Ada Owner <ada@acme.test> 1787406946 +0000",
            committer: "Ada Owner <ada@acme.test> 1787406946 +0000",
            parents: ["bbbb1111"],
            tree: "cccc1111",
          },
        ],
        next_after: null,
      },
    }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/diff*", (r) =>
    r.fulfill({
      status: 200,
      json: {
        changes: [
          {
            status: "modified",
            path: "src/parse.rs",
            old_oid: "1",
            new_oid: "2",
          },
        ],
      },
    }),
  );

  await page.goto(`/acme/widget/commit/${sha}`);
  await expect(page.getByText("teach the parser about tabs")).toBeVisible();
  await expect(page.getByText("1 file changed")).toBeVisible();
  await expect(page.getByText("src/parse.rs")).toBeVisible();
  // The body is shown, not swallowed — a commit page is the one place
  // the whole message belongs.
  await expect(page.getByText("long body here")).toBeVisible();
});
