// The Fork control on a repository page, and the line that says whose
// history a fork is carrying.
//
// Three things are worth pinning here. The count is on the control,
// from the repository row. Forking navigates to the fork — a POST that
// leaves you on the page you were already on reads as having done
// nothing. And a fork says "forked from" under its name, because a
// fork that does not is a repository claiming somebody else's history
// as its own. (That it is the same size as Watch beside it is
// `watch.spec.ts`'s to hold.)

import { expect, test, type Page } from "@playwright/test";
import { ME, REPOS, signIn } from "./fixtures";

// `native`, and not incidentally: `REPOS.repos[0]` is a mirror, and a
// mirror can never carry a change — `changes_api::create` refuses one at
// the door ("a mirror's trunk belongs to its origin"), so no change row
// on one can exist. The repository page stopped offering a Changes tab
// to mirrors, which is what made this fixture's contradiction visible.
const widget = {
  ...REPOS.repos[0],
  org: "acme",
  kind: "native",
  fork_count: 2,
};

/// A repository page with nothing mocked but what the test names.
///
/// The catch-all is registered first: an unmocked `/v1` call otherwise
/// proxies to whatever is listening on :8080, so the suite would be
/// hermetic only when nobody had the manual stack up.
async function onRepo(page: Page, repo: Record<string, unknown>) {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: repo }),
  );
}

const forkButton = (page: Page) =>
  page.getByRole("button", { name: /fork this repository/i });

test("the fork count is on the control, from the repository row", async ({
  page,
}) => {
  await onRepo(page, widget);
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await page.goto("/acme/widget");
  await expect(forkButton(page)).toContainText("2");
});

test("forking lands you in the fork, not back on the upstream", async ({
  page,
}) => {
  // Signed in first: `signIn` registers its own mocks and navigates, so
  // anything this test needs to win has to be registered after it.
  await signIn(page);
  await onRepo(page, widget);
  const fork = {
    ...widget,
    name: "widget",
    org: "alice",
    fork_state: "pending",
    fork_parent: "acme/widget",
  };
  let posted = 0;
  // Discriminated by method, which it did not used to have to be. The
  // About rail now *reads* this path — "Forks of this" — so a handler
  // that counted every request counted the rail's GET as a second fork
  // and failed a test about double-posting with a page that had posted
  // exactly once. A mock that cannot tell a read from a write will
  // eventually accuse the product of the mock's own confusion.
  await page.route("**/v1/orgs/acme/repos/widget/forks", (r) => {
    if (r.request().method() !== "POST") {
      return r.fulfill({ status: 200, json: { forks: [], count: 0 } });
    }
    posted += 1;
    return r.fulfill({ status: 202, json: fork });
  });
  await page.route("**/v1/orgs/alice/repos/widget", (r) =>
    r.fulfill({ status: 200, json: fork }),
  );
  await page.goto("/acme/widget");

  await forkButton(page).click();
  await expect(page).toHaveURL(/\/alice\/widget$/);
  expect(posted).toBe(1);
});

test("pressing Fork does not ask the new fork for files it has no objects for yet", async ({
  page,
}) => {
  // The test above this one arrives at a pending fork by URL and proves
  // the page waits for the row before asking for files. Arriving from
  // the button is different: the repository screen is already mounted,
  // holding the *upstream's* row, and the first render under the fork's
  // address happened before the fork's row had been asked for. That
  // render saw a ready repository and mounted the browser, which fired
  // the whole first round of reads — tree, log, README, tags, meta,
  // branches — at a copy with nothing in it. Eight 404s per fork, from
  // the one path a person actually takes to get here.
  await signIn(page);
  await onRepo(page, { ...widget, fork_state: null });
  await page.route("**/v1/orgs/acme/repos/widget/*", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  const fork = {
    ...widget,
    org: "alice",
    fork_state: "pending",
    fork_parent: "acme/widget",
  };
  await page.route("**/v1/orgs/acme/repos/widget/forks", (r) =>
    r.request().method() === "POST"
      ? r.fulfill({ status: 202, json: fork })
      : r.fulfill({ status: 200, json: { forks: [], count: 0 } }),
  );
  await page.route("**/v1/orgs/alice/repos/widget", (r) =>
    r.fulfill({ status: 200, json: fork }),
  );
  const asked: string[] = [];
  await page.route("**/v1/orgs/alice/repos/widget/*", (r) => {
    const url = new URL(r.request().url()).pathname;
    if (/\/(tree|files|log|branches|tags|meta)(\?|\/|$)/.test(url))
      asked.push(url.replace(/^.*\/repos\/widget\//, ""));
    return r.fulfill({
      status: 404,
      json: { error: "not mocked by this test" },
    });
  });
  await page.goto("/acme/widget");
  await forkButton(page).click();
  await expect(page).toHaveURL(/\/alice\/widget$/);
  await expect(page.getByRole("status")).toContainText("Forking acme/widget");
  await expect(page.getByText(/^404$/)).toHaveCount(0);
  expect(asked).toEqual([]);
});

test("forking what you already forked takes you to that fork, and says so", async ({
  page,
}) => {
  // Found in the app: the second press toasted `repo "widget" already
  // exists` and stayed on upstream, which reads as a failure and leaves
  // the person hunting for a repository the server could have named.
  // The server now answers 200 with the fork that exists; the page has
  // to treat that as "here it is", not as a fresh copy.
  await signIn(page);
  await onRepo(page, widget);
  const fork = {
    ...widget,
    name: "widget",
    org: "alice",
    fork_state: "ready",
    fork_parent: "acme/widget",
  };
  await page.route("**/v1/orgs/acme/repos/widget/forks", (r) => {
    if (r.request().method() !== "POST") {
      return r.fulfill({
        status: 200,
        json: { forks: [{ org: "alice", name: "widget" }], count: 1 },
      });
    }
    return r.fulfill({ status: 200, json: fork });
  });
  await page.route("**/v1/orgs/alice/repos/widget", (r) =>
    r.fulfill({ status: 200, json: fork }),
  );
  await page.goto("/acme/widget");

  await forkButton(page).click();
  await expect(page).toHaveURL(/\/alice\/widget$/);
  await expect(
    page.getByText("You already have a fork of this — here it is."),
  ).toBeVisible();
  // And it is not reported as an error: that was the defect.
  await expect(page.getByText(/already exists/)).toHaveCount(0);
});

test("a fork still being made says so, then shows its files without a reload", async ({
  page,
}) => {
  // Found in the app: pressing Fork lands on the fork straight from the
  // 202, and for the few seconds before its objects are written every
  // read of it 404s. The page printed a red "404" under the name of a
  // repository the person had just created, and stayed that way until
  // somebody thought to reload. The fork *was* fine; the page asked for
  // its files before there were any and never asked again.
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  const fork = { ...widget, org: "alice", fork_parent: "acme/widget" };
  let rowReads = 0;
  await page.route("**/v1/orgs/alice/repos/widget", (r) => {
    rowReads += 1;
    return r.fulfill({
      status: 200,
      json: { ...fork, fork_state: rowReads < 3 ? "pending" : "ready" },
    });
  });
  // Content, which must not be asked for until the row says ready: a
  // pending fork has none, and the requests only produce the 404.
  let contentWhilePending = 0;
  await page.route("**/v1/orgs/alice/repos/widget/*", (r) => {
    const url = r.request().url();
    // Only what needs the fork's objects; the About rail's forks and
    // contributor reads are the row's facts and answer fine.
    if (
      rowReads < 3 &&
      /\/(tree|files|log|branches|tags|meta)(\?|\/|$)/.test(url)
    )
      contentWhilePending += 1;
    if (/\/tree(\?|$)/.test(url))
      return r.fulfill({
        json: {
          commit: "abc1234567890abc1234567890abc1234567890a",
          entries: [
            {
              name: "README.md",
              mode: "100644",
              kind: "blob",
              oid: "b1",
              size: 21,
            },
            { name: "src", mode: "40000", kind: "tree", oid: "t1", size: null },
          ],
        },
      });
    return r.fulfill({
      status: 404,
      json: { error: "not mocked by this test" },
    });
  });
  await page.goto("/alice/widget");

  // What was made is named, and what is happening to it is said.
  await expect(page.getByText("Forked from")).toBeVisible();
  await expect(page.getByRole("status")).toContainText("Forking acme/widget");
  await expect(page.getByText(/^404$/)).toHaveCount(0);

  // Then the files, on the same page, once the row says ready.
  await expect(page.getByText("README.md")).toBeVisible({ timeout: 10000 });
  await expect(page.getByRole("status")).toHaveCount(0);
  expect(rowReads).toBeGreaterThanOrEqual(3);
  expect(contentWhilePending).toBe(0);
});

test('a fork that could not be made says that, not "404"', async ({ page }) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await page.route("**/v1/orgs/alice/repos/widget", (r) =>
    r.fulfill({
      status: 200,
      json: {
        ...widget,
        org: "alice",
        fork_parent: "acme/widget",
        fork_state: "failed",
      },
    }),
  );
  await page.goto("/alice/widget");
  await expect(page.getByRole("alert")).toContainText(
    "This fork could not be made",
  );
  await expect(page.getByText(/^404$/)).toHaveCount(0);
});

test("a fork says whose history it is carrying", async ({ page }) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await page.route("**/v1/orgs/alice/repos/widget", (r) =>
    r.fulfill({
      status: 200,
      json: { ...widget, org: "alice", fork_parent: "acme/widget" },
    }),
  );
  await page.goto("/alice/widget");

  const from = page.getByText(/forked from/i);
  await expect(from).toBeVisible();
  // A link, not a sentence about a link: the whole use of the line is
  // getting back to the project this came from.
  await expect(from.getByRole("link", { name: "acme/widget" })).toHaveAttribute(
    "href",
    "/acme/widget",
  );
});

test("a repository that is not a fork says nothing about one", async ({
  page,
}) => {
  await onRepo(page, widget);
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await page.goto("/acme/widget");
  await expect(forkButton(page)).toBeVisible();
  await expect(page.getByText(/forked from/i)).toHaveCount(0);
});

// ---------------------------------------------------------------------
// Proposing a change from a fork — the contribution path itself.
//
// The server has accepted a fork-sourced change since forks landed:
// `POST …/changes` takes `source` naming the fork, checks it really is
// one, and `fork_pr_e2e.rs` drives fork → push → open → land end to end.
// The dashboard sent only `from`, so the whole outside-contributor
// story was reachable only by hand-writing REST calls — and the refusal
// a contributor got told them to "open the change with `source` naming
// your fork", a field that existed nowhere on screen.
//
// These pin the four states the form has to tell apart, and they assert
// the **request body**, because that is where the bug was: a form that
// looks right and posts three fields short is the same defect wearing a
// nicer coat.

/// A repository page whose Changes tab is reachable, with the change
/// list empty so the form is what renders.
async function onChangesTab(
  page: Page,
  repo: Record<string, unknown>,
  forks: { org: string; name: string }[] = [],
) {
  // The catch-all, here as well as in `onRepo`, because
  // `hermetic-specs.test.ts` reads this file structurally and will not
  // follow one helper into another — deliberately: it was written after
  // a spec that had a catch-all in a helper and a test that quietly
  // went around it. Insisting the guard is visible where a test can see
  // it is the point, so this states it rather than teaching the checker
  // to chase indirection. Re-registering is harmless; the specific
  // routes below still win.
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await onRepo(page, repo);
  await page.route("**/v1/orgs/acme/repos/widget/changes", (r) =>
    r.request().method() === "GET"
      ? r.fulfill({ status: 200, json: { changes: [] } })
      : r.fallback(),
  );
  await page.route("**/v1/orgs/acme/repos/widget/forks", (r) =>
    r.fulfill({ status: 200, json: { forks, count: forks.length } }),
  );
}

/// Capture the body of the change-creation POST.
async function capturePost(page: Page): Promise<() => unknown> {
  let body: unknown = null;
  await page.route("**/v1/orgs/acme/repos/widget/changes", async (r) => {
    if (r.request().method() !== "POST") return r.fallback();
    body = r.request().postDataJSON();
    return r.fulfill({
      status: 200,
      json: {
        change: {
          key: "I0",
          title: "t",
          target_branch: "main",
          source: null,
          state: "open",
          land_verdict: null,
          landed_commit: null,
          created_at: Date.now(),
          updated_at: Date.now(),
          patchset: null,
        },
        patchset: null,
      },
    });
  });
  return () => body;
}

test("a reader without write access is given the fork field, and it is sent", async ({
  page,
}) => {
  await signIn(page);
  await onChangesTab(page, { ...widget, viewer_write: false }, [
    { org: "bob", name: "widget" },
  ]);
  const body = await capturePost(page);
  await page.goto("/acme/widget/changes");

  // Not behind a disclosure: for this person the fork is the only route,
  // so a link they have to find first is the bug being fixed.
  const source = page.getByLabel("Fork the commits are in");
  await expect(source).toBeVisible();
  await expect(
    page.getByText(/You do not have write access here/),
  ).toBeVisible();

  await source.fill("bob/widget");
  await page.getByLabel("Branch to review").fill("feature");
  await page.getByRole("button", { name: "Start review" }).click();

  await expect
    .poll(body, { message: "the POST never carried the fork" })
    .toEqual({ from: "feature", source: "bob/widget" });
});

test("a writer posts no source, and can still choose a fork", async ({
  page,
}) => {
  await signIn(page);
  await onChangesTab(page, { ...widget, viewer_write: true }, []);
  const body = await capturePost(page);
  await page.goto("/acme/widget/changes");

  // The enterprise flow, unchanged: commits already here, no `source`.
  // Sending one would make every existing change a fork-sourced change.
  await expect(page.getByLabel("Fork the commits are in")).toHaveCount(0);
  await page.getByLabel("Branch to review").fill("feature");
  await page.getByRole("button", { name: "Start review" }).click();
  await expect.poll(body).toEqual({ from: "feature" });
});

test("a writer can propose from a fork, and name the branch it lands on", async ({
  page,
}) => {
  // GitHub hides its two repository pickers behind "compare across
  // forks" so the common case stays small; this is that link. A
  // maintainer reviewing a contributor's fork branch needs it too.
  await signIn(page);
  await onChangesTab(page, { ...widget, viewer_write: true }, [
    { org: "bob", name: "widget" },
  ]);
  const body = await capturePost(page);
  await page.goto("/acme/widget/changes");

  await page
    .getByRole("button", { name: "Propose from a fork instead" })
    .click();
  await page.getByLabel("Fork the commits are in").fill("bob/widget");
  await page.getByLabel("Branch to review").fill("feature");
  // `target` has been in the API since forks landed and no surface ever
  // set it, so every change silently aimed at the default branch.
  await page.getByLabel("Land on").fill("release-2");
  await page.getByRole("button", { name: "Start review" }).click();

  await expect
    .poll(body)
    .toEqual({ from: "feature", source: "bob/widget", target: "release-2" });
});

test("the fork field suggests this repository's forks", async ({ page }) => {
  await signIn(page);
  await onChangesTab(page, { ...widget, viewer_write: false }, [
    { org: "bob", name: "widget" },
    { org: "carol", name: "widget" },
  ]);
  await page.goto("/acme/widget/changes");

  // A datalist, not a select: the listing is only the forks this reader
  // may see, and a contributor's fork may not be one of them, so the
  // field has to accept one that was never offered.
  const options = page.locator("#review-source-forks option");
  await expect(options).toHaveCount(2);
  await expect(options.first()).toHaveAttribute("value", "bob/widget");
});

test("a writer whose row arrives late is not asked which fork they used", async ({
  page,
}) => {
  // Found by running the real thing rather than a mock. `canWrite`
  // arrives with the repository row, one request *after* the Changes
  // panel first paints, so seeding the disclosure with
  // `useState(!canWrite)` computed it from `false` — and a maintainer
  // with full write access landed on their own Changes tab to be asked
  // which fork their commits were in. Every mocked test passed, because
  // a mock answers before the panel has a chance to be wrong.
  await signIn(page);
  await onChangesTab(page, { ...widget, viewer_write: true }, []);
  // Registered last so it wins: the row, deliberately slow.
  await page.route("**/v1/orgs/acme/repos/widget", async (r) => {
    await new Promise((done) => setTimeout(done, 600));
    return r.fulfill({ status: 200, json: { ...widget, viewer_write: true } });
  });
  await page.goto("/acme/widget/changes");

  // The toggle only renders once the row says this viewer may write, so
  // waiting for it is waiting for the answer to have arrived.
  await expect(
    page.getByRole("button", { name: "Propose from a fork instead" }),
  ).toBeVisible();
  await expect(
    page.getByLabel("Fork the commits are in"),
    "the fork field stayed open after the row said this viewer may write",
  ).toHaveCount(0);
});
