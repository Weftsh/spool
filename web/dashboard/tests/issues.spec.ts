// The issues tab: the index, one issue, and the form that files one.
//
// GitHub's information architecture in our skin, so most of what is
// pinned here is information architecture rather than markup: that the
// Open/Closed pair comes from the server's counts and not from the rows
// on screen, that the query bar is the URL, that a conversation reads in
// insertion order, and that a reader without write access gets the
// controls the server will accept and none that only error.
//
// Two hazards get their own tests because the manual browser gate is
// "0 problems" and both are invisible until it runs: the filter row
// must scroll inside itself rather than widening the document, and
// every control must have an accessible name.

import { expect, test, type Page, type Request } from "@playwright/test";
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
  // A **reader**, said rather than implied. This file's default caller
  // has no write access — the maintainer tests override it where they
  // need it — and that used to be true only because `REPOS` carried no
  // `viewer_write` at all. The moment the shared fixture started
  // describing the owner it signs in as, two "a reader is offered no
  // control" tests began asserting against a writer, and the controls
  // they exist to keep off the page came back.
  viewer_write: false,
};

/// Colours are one of the seven names the server validates at the
/// write, never hexes — a hex in the database is unfixable later:
/// change the palette and every stored hex is silently wrong in both
/// themes, with no migration that can know what the author meant.
const LABELS = [
  {
    id: "l1",
    name: "bug",
    color: "status-serious",
    description: "something broke",
  },
  {
    id: "l2",
    name: "good first issue",
    color: "series-1",
    description: "a gentle start",
  },
  {
    id: "l3",
    name: "wontfix",
    color: "neutral",
    description: "not being worked on",
  },
];

/// A colour this bundle has never heard of.
///
/// The server refuses unknown names at the write, so one arriving here
/// means the palette grew and this bundle is older than the data —
/// somebody else's deploy, which must not produce a blank issue list.
const FUTURE_LABEL = {
  id: "l9",
  name: "needs-triage",
  color: "series-11",
  description: "from a newer palette",
};

const DAY = 86_400_000;

function issue(over: Record<string, unknown>) {
  return {
    id: `i${over.number}`,
    number: 1,
    title: "an issue",
    body: "",
    state: "open",
    author: "ada",
    // Present on the wire and never rendered: it exists so the server
    // can decide "is this the caller's own issue" without keying
    // authorization on a mutable display name.
    author_id: "01ADAADAADAADAADAADAADAADA",
    author_label: null,
    created_at: Date.now() - DAY,
    updated_at: Date.now() - DAY,
    closed_at: null,
    labels: [],
    comment_count: 0,
    ...over,
  };
}

const OPEN_ISSUES = [
  issue({
    number: 7,
    title: "Push hangs on a large pack",
    body: "It stops at 97% every time.",
    labels: [LABELS[0]],
    comment_count: 2,
    updated_at: Date.now() - DAY,
  }),
  issue({
    number: 3,
    title: "Document the SSH front door",
    author: "bo",
    labels: [LABELS[1]],
    comment_count: 0,
    updated_at: Date.now() - 2 * DAY,
  }),
];

/// The counts the server reports over the *whole* repository. They are
/// deliberately unrelated to the length of `OPEN_ISSUES`: a page that
/// derives either number from the rows it drew would agree with a
/// fixture that made them match, and agree wrongly.
const COUNTS = { open: 2, closed: 40 };

interface Seen {
  /// Every issues-list URL the page asked for, in order.
  lists: URL[];
  /// The `Authorization` header on the most recent list request, or "".
  auth: string;
  /// Bodies of every mutating request, as objects.
  posted: unknown[];
}

/// A repository page with nothing mocked but what this file names.
///
/// The catch-all is registered FIRST: an unmocked `/v1` call otherwise
/// proxies to whatever is listening on :8080, so the suite would be
/// hermetic only when nobody had the manual stack up.
///
/// The caller is a person signed in with a browser session who wrote
/// none of these issues, unless `ownSession` says the test registered
/// its own — `signIn` with a token, or `asAuthor`.
async function onIssues(
  page: Page,
  opts: {
    issues?: ReturnType<typeof issue>[];
    counts?: { open: number; closed: number };
    detail?: Record<string, unknown>;
    comments?: Record<string, unknown>[];
    ownSession?: boolean;
  } = {},
): Promise<Seen> {
  const seen: Seen = { lists: [], auth: "", posted: [] };
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  if (!opts.ownSession) {
    await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  }
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: widget }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/labels", (r) =>
    r.fulfill({ status: 200, json: { labels: LABELS } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/issues*", (r) => {
    const req: Request = r.request();
    if (req.method() === "POST") {
      seen.posted.push(req.postDataJSON());
      return r.fulfill({ status: 201, json: issue({ number: 12 }) });
    }
    seen.lists.push(new URL(req.url()));
    seen.auth = req.headers()["authorization"] ?? "";
    return r.fulfill({
      status: 200,
      json: {
        issues: opts.issues ?? OPEN_ISSUES,
        counts: opts.counts ?? COUNTS,
        next: null,
      },
    });
  });
  await page.route("**/v1/orgs/acme/repos/widget/issues/*", (r) => {
    const req = r.request();
    if (req.method() === "PATCH") {
      const body = req.postDataJSON() as { state: string };
      seen.posted.push(body);
      return r.fulfill({
        status: 200,
        json: { ...(opts.detail ?? {}), ...issue({ number: 7 }), ...body },
      });
    }
    return r.fulfill({
      status: 200,
      json: opts.detail ?? { ...OPEN_ISSUES[0] },
    });
  });
  await page.route("**/v1/orgs/acme/repos/widget/issues/*/comments", (r) => {
    const req = r.request();
    if (req.method() === "POST") {
      seen.posted.push(req.postDataJSON());
      return r.fulfill({
        status: 201,
        json: {
          id: "c9",
          seq: 9,
          body: (req.postDataJSON() as { body: string }).body,
          author: "ada",
          author_label: null,
          created_at: Date.now(),
          updated_at: Date.now(),
        },
      });
    }
    return r.fulfill({ status: 200, json: { comments: opts.comments ?? [] } });
  });
  return seen;
}

const row = (page: Page, title: string) =>
  page.getByRole("listitem").filter({ hasText: title });

// ---------------------------------------------------------------------
// The index
// ---------------------------------------------------------------------

test("the Open/Closed pair is the repository's count, not the page's", async ({
  page,
}) => {
  await onIssues(page);
  await page.goto("/acme/widget/issues");
  // Two rows are on screen and forty issues are closed. A page that
  // counted what it drew would say "0 Closed" here — which is the
  // number a maintainer uses to decide whether anything is being closed
  // at all, so it is worse than no number.
  await expect(page.getByRole("button", { name: "2 Open" })).toBeVisible();
  await expect(page.getByRole("button", { name: "40 Closed" })).toBeVisible();
  await expect(page.getByRole("listitem")).toHaveCount(2);
});

test("a row carries state, title, labels, provenance and a comment count", async ({
  page,
}) => {
  await onIssues(page);
  await page.goto("/acme/widget/issues");
  const first = row(page, "Push hangs on a large pack");
  // State is a glyph with a word, never colour alone.
  await expect(first.getByRole("img", { name: "Open" })).toBeVisible();
  await expect(
    first.getByRole("link", { name: "Push hangs on a large pack" }),
  ).toHaveAttribute("href", "/acme/widget/issues/7");
  // The label is rendered as a pill on the row itself: an issue list
  // where the labels are one click away is a list you cannot triage.
  await expect(first.getByText("bug")).toBeVisible();
  await expect(first).toContainText("#7");
  await expect(first).toContainText("opened");
  await expect(first).toContainText("by ada");
  await expect(
    first.getByRole("link", { name: "2 comments on #7" }),
  ).toBeVisible();
  // A zero beside a tag reads as a defect, so it is not drawn at all.
  await expect(
    row(page, "Document the SSH front door").getByRole("link", {
      name: /comments on/,
    }),
  ).toHaveCount(0);
});

test("every relative time carries the absolute one", async ({ page }) => {
  await onIssues(page);
  await page.goto("/acme/widget/issues");
  const when = row(page, "Push hangs on a large pack").locator("time");
  // Both halves. A `<time>` without a `dateTime` is a `<span>` with
  // extra letters, and a relative time with no absolute one behind it
  // is unusable the moment anybody argues about what happened when.
  await expect(when).toHaveAttribute("datetime", /^\d{4}-/);
  await expect(when).toHaveAttribute("title", /.+/);
});

test("clicking Closed asks the server for closed, and the bar says so", async ({
  page,
}) => {
  const seen = await onIssues(page);
  await page.goto("/acme/widget/issues");
  await expect(page.getByRole("listitem")).toHaveCount(2);
  await page.getByRole("button", { name: "40 Closed" }).click();

  // Wait on the thing the assertion depends on — the request the click
  // caused — rather than on a proxy for it.
  await expect
    .poll(() => seen.lists.at(-1)?.searchParams.get("state"))
    .toBe("closed");
  // And the query bar is the URL: whatever the control did has to be
  // sendable to somebody else.
  await expect(page.getByLabel("Filter issues")).toHaveValue(
    "is:issue is:closed",
  );
  await expect(page).toHaveURL(/\?q=is%3Aissue\+is%3Aclosed$/);
});

test("the address is the query, on the way in as well as out", async ({
  page,
}) => {
  const seen = await onIssues(page);
  await page.goto(
    "/acme/widget/issues?q=" +
      encodeURIComponent('is:issue is:closed label:"good first issue"'),
  );
  await expect
    .poll(() => seen.lists.at(-1)?.searchParams.get("label"))
    .toBe("good first issue");
  expect(seen.lists.at(-1)?.searchParams.get("state")).toBe("closed");
  // The dropdown agrees with the URL. A filter that is in the address
  // but not in the control is a control that lies about the list.
  await expect(page.getByLabel("Labels")).toHaveText("good first issue");
});

test("typing filters nothing until you submit", async ({ page }) => {
  const seen = await onIssues(page);
  await page.goto("/acme/widget/issues");
  await expect(page.getByRole("listitem")).toHaveCount(2);
  const before = seen.lists.length;
  await page.getByLabel("Filter issues").fill("is:issue author:bo");
  // A request per keystroke is eighteen requests and a list that
  // flickers through every prefix of what you meant.
  expect(seen.lists.length).toBe(before);
  await page.getByLabel("Filter issues").press("Enter");
  await expect
    .poll(() => seen.lists.at(-1)?.searchParams.get("author"))
    .toBe("bo");
});

test("the clear button stops filtering rather than restoring the default", async ({
  page,
}) => {
  const seen = await onIssues(page);
  await page.goto("/acme/widget/issues");
  await expect(page.getByRole("listitem")).toHaveCount(2);
  await page.getByRole("button", { name: "Clear the filter" }).click();
  // × has to mean "stop filtering". Going back to `is:issue is:open`
  // would leave the closed ones hidden and read as having done nothing.
  await expect
    .poll(() => seen.lists.at(-1)?.searchParams.get("state"))
    .toBe("all");
  await expect(page.getByLabel("Filter issues")).toHaveValue("is:issue");
});

test("a token holder's credential goes with the read", async ({ page }) => {
  // The a3fece7 bug, pinned. The forge built every session with an
  // empty token, which sends no credential at all. On a cookie
  // session that is invisible — the browser attaches the cookie itself
  // — so it only ever showed for somebody signed in with an API token,
  // and it showed *silently*: the server filters by who is asking, so a
  // person was shown less of their own work and told nothing.
  await signIn(page);
  const seen = await onIssues(page, { ownSession: true });
  await page.goto("/acme/widget/issues");
  await expect(page.getByRole("listitem")).toHaveCount(2);
  expect(seen.auth).toBe("Bearer weft_test_token");
});

test("a reader without write access reads the issues and may file one", async ({
  page,
}) => {
  await onIssues(page);
  await page.goto("/acme/widget/issues");
  await expect(page.getByRole("listitem")).toHaveCount(2);
  // Filing needs `RepoRead` and nothing more, so the control is offered
  // — and there is no invitation to sign in, because the reader is.
  await expect(
    page.getByRole("link", { name: "New issue" }),
  ).toHaveAttribute("href", "/acme/widget/issues/new");
  await expect(page.getByRole("link", { name: /sign in/i })).toHaveCount(0);
});

// ---------------------------------------------------------------------
// The audit hazards
// ---------------------------------------------------------------------

/// A label and an author long enough that the filter row genuinely does
/// not fit. This matters more than it looks: the first version of the
/// test below used the ordinary fixtures at 1024px, where the row fits
/// with room to spare — so removing the scroller entirely left it green.
/// A layout test whose content does not overflow constrains nothing.
const LONG = {
  label: "needs-a-decision-from-somebody-who-owns-the-storage-layer",
  author: "a-contributor-whose-handle-is-not-short-at-all",
};

const CROWDED = `is:issue is:open author:${LONG.author} label:"${LONG.label}"`;

test("the filter row scrolls inside itself, not the page", async ({ page }) => {
  // `audit()` measures `documentElement.scrollWidth`, so a scroller on
  // an outer container fails the manual gate while the identical one on
  // an inner container is invisible to it.
  //
  // 768 is the narrowest width at which the row is still `nowrap` —
  // below it the row wraps and stacks, which is what a phone should do
  // and is a different behaviour from the one under test here.
  await page.setViewportSize({ width: 768, height: 900 });
  await onIssues(page, {
    issues: [
      issue({
        number: 7,
        title:
          "A title long enough that nothing about this row may be allowed to widen the document",
        labels: LABELS,
        author: LONG.author,
      }),
    ],
  });
  await page.goto(`/acme/widget/issues?q=${encodeURIComponent(CROWDED)}`);
  await expect(page.getByRole("listitem")).toHaveCount(1);
  const { docOver, rowOver } = await page.evaluate(() => {
    // Walk up from a control that is actually in the row, and look at
    // computed `overflow-x` rather than at a class name: the assertion
    // is about the behaviour, and a class is only its spelling today.
    let el = document.querySelector<HTMLElement>('[aria-label="Sort"]');
    let scroller: HTMLElement | null = null;
    while (el && el !== document.body) {
      const o = getComputedStyle(el).overflowX;
      if (o === "auto" || o === "scroll") {
        scroller = el;
        break;
      }
      el = el.parentElement;
    }
    return {
      docOver:
        document.documentElement.scrollWidth -
        document.documentElement.clientWidth,
      // Positive means the row's content genuinely exceeds its box, so
      // something has to scroll — and this test is about which. Without
      // it the whole check passes on a row that simply fits, which is
      // how the first version of this test came out green against a
      // build with no scroller at all.
      rowOver: scroller ? scroller.scrollWidth - scroller.clientWidth : -1,
    };
  });
  expect(rowOver).toBeGreaterThan(0);
  expect(docOver).toBeLessThanOrEqual(0);
});

test("every control on the page has an accessible name", async ({ page }) => {
  // The walkthrough's `emptyButtons` audit, run here so it fails in CI
  // rather than in a person's browser. Icon-only controls are the ones
  // that trip it, and this page has three.
  await onIssues(page);
  await page.goto("/acme/widget/issues");
  await expect(page.getByRole("listitem")).toHaveCount(2);
  const nameless = await page.evaluate(() =>
    [...document.querySelectorAll("button, a")]
      .filter((el) => {
        const label = el.getAttribute("aria-label")?.trim();
        return !label && !(el.textContent ?? "").trim();
      })
      .map((el) => el.outerHTML.slice(0, 120)),
  );
  expect(nameless).toEqual([]);
});

// ---------------------------------------------------------------------
// The detail page
// ---------------------------------------------------------------------

const THREAD = [
  {
    id: "c2",
    seq: 2,
    body: "Second thing said.",
    author: "bo",
    author_label: null,
    // Same millisecond as the first, deliberately: neither the id nor
    // the timestamp can order these, only `seq` can.
    created_at: 1_700_000_000_000,
    updated_at: 1_700_000_000_000,
  },
  {
    id: "c1",
    seq: 1,
    body: "First thing said.",
    author: "ada",
    author_label: null,
    created_at: 1_700_000_000_000,
    updated_at: 1_700_000_000_000,
  },
];

test("one issue: title, number, state, body and a thread in seq order", async ({
  page,
}) => {
  await onIssues(page, { comments: THREAD });
  await page.goto("/acme/widget/issues/7");
  await expect(
    page.getByRole("heading", { name: /Push hangs on a large pack/ }),
  ).toBeVisible();
  await expect(page.getByRole("heading")).toContainText("#7");
  await expect(page.getByText("Open", { exact: true })).toBeVisible();
  await expect(page.getByText("It stops at 97% every time.")).toBeVisible();
  // The wire order is 2 then 1. A conversation that renders in the
  // order it arrived is a conversation nobody typed.
  const bodies = await page.getByRole("listitem").allTextContents();
  const first = bodies.findIndex((t) => t.includes("First thing said."));
  const second = bodies.findIndex((t) => t.includes("Second thing said."));
  expect(first).toBeGreaterThanOrEqual(0);
  expect(second).toBeGreaterThan(first);
});

test("commenting posts the body once and clears the box", async ({ page }) => {
  await signIn(page);
  const seen = await onIssues(page, { ownSession: true, comments: THREAD });
  await page.goto("/acme/widget/issues/7");
  const box = page.getByLabel("Comment on this issue");
  await box.fill("I can reproduce this.");
  await page.getByRole("button", { name: "Comment" }).click();
  // Cleared is the observable the next interaction depends on — waiting
  // on a mock's call count passes before the page has finished with the
  // response.
  await expect(box).toHaveValue("");
  // An object, not a string. A client that stringified before handing
  // the body to a transport that stringifies again sends every field
  // double-encoded, and every mock written from the same assumption
  // agrees with it.
  expect(seen.posted).toEqual([{ body: "I can reproduce this." }]);
  await expect(page.getByText("I can reproduce this.")).toBeVisible();
});

test("closing an issue says closed, and says so on the page", async ({
  page,
}) => {
  await signIn(page);
  const seen = await onIssues(page, { ownSession: true });
  // With write access: the server allows closing to a writer or to the
  // issue's own author, and this fixture is neither until it says so.
  // Before the control was gated, a signed-in stranger was offered it
  // and got a 403 for clicking.
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: { ...widget, viewer_write: true } }),
  );
  await page.goto("/acme/widget/issues/7");
  await page.getByRole("button", { name: "Close issue" }).click();
  await expect(page.getByText("Closed as completed")).toBeVisible();
  expect(seen.posted).toEqual([{ state: "closed" }]);
  // And the control turns into its inverse rather than disappearing.
  await expect(
    page.getByRole("button", { name: "Reopen issue" }),
  ).toBeVisible();
});

test("an address that is not an issue number is not an issue", async ({
  page,
}) => {
  const seen = await onIssues(page);
  // `Number("0x0c")` is 12. A parse that asked only "is this a number?"
  // would render issue 12 at an address no canonical link points at.
  await page.goto("/acme/widget/issues/0x0c");
  // `couldn’t` in the copy is a right single quote, not an apostrophe —
  // matched with `.` rather than reproduced, because a test that
  // depends on which glyph somebody typed breaks on a copy edit.
  await expect(
    page.getByRole("heading", { name: /We couldn.t find that issue/ }),
  ).toBeVisible();
  expect(seen.lists.length).toBe(0);
});

// ---------------------------------------------------------------------
// Filing one
// ---------------------------------------------------------------------

test("filing an issue lands you on the issue, not back on the list", async ({
  page,
}) => {
  await signIn(page);
  const seen = await onIssues(page, { ownSession: true });
  await page.goto("/acme/widget/issues/new");
  await page.getByLabel("Title").fill("Clone over SSH is refused");
  await page.getByLabel("Description").fill("With a fresh key.");
  await page.getByRole("button", { name: "Submit new issue" }).click();
  // The first thing anybody wants after filing is the address to send
  // somebody. A form that returns you to the list reads as having done
  // nothing at all.
  await expect(page).toHaveURL("/acme/widget/issues/12");
  expect(seen.posted).toEqual([
    { title: "Clone over SSH is refused", body: "With a fresh key." },
  ]);
});

test("the new-issue form refuses an empty title rather than the server", async ({
  page,
}) => {
  await signIn(page);
  const seen = await onIssues(page, { ownSession: true });
  await page.goto("/acme/widget/issues/new");
  const submit = page.getByRole("button", { name: "Submit new issue" });
  await expect(submit).toBeDisabled();
  // Whitespace is not a title. Trimming at the boundary rather than
  // sending it and rendering the refusal is the difference between a
  // form and a error message.
  await page.getByLabel("Title").fill("   ");
  await expect(submit).toBeDisabled();
  expect(seen.posted).toEqual([]);
});

test("the Issues tab is a link somebody can send", async ({ page }) => {
  await onIssues(page);
  await page.goto("/acme/widget");
  const tab = page
    .getByRole("navigation", { name: "Repository" })
    .getByRole("link", { name: "Issues" });
  await expect(tab).toHaveAttribute("href", "/acme/widget/issues");
  await tab.click();
  await expect(page.getByRole("button", { name: "2 Open" })).toBeVisible();
});

// ---------------------------------------------------------------------
// Ordering, and the two things it must never do
// ---------------------------------------------------------------------

test("the sort goes to the server, and the rows arrive already ordered", async ({
  page,
}) => {
  // Ordering is the server's. Re-sorting the page here would make
  // "oldest" mean "the oldest of the newest hundred" — a control that
  // silently sorts the wrong population, which looks like it works and
  // which the manual browser pass cannot catch.
  //
  // So the mock answers `sort=oldest` with rows in oldest-first order
  // and the test asserts the page did not touch them. Both halves: the
  // request has to carry the sort, AND the render has to trust it.
  const seen = await onIssues(page, {
    issues: [OPEN_ISSUES[1], OPEN_ISSUES[0]],
  });
  await page.goto(
    `/acme/widget/issues?q=${encodeURIComponent("is:issue is:open sort:oldest")}`,
  );
  await expect(page.getByRole("listitem")).toHaveCount(2);
  expect(seen.lists.at(-1)?.searchParams.get("sort")).toBe("oldest");
  const titles = await page.getByRole("listitem").allTextContents();
  expect(titles[0]).toContain("Document the SSH front door");
  expect(titles[1]).toContain("Push hangs on a large pack");
});

test("the sort the server answers 400 to is not reachable from the UI", async ({
  page,
}) => {
  const seen = await onIssues(page);
  // A link somebody shared from an older bundle, when the dropdown
  // still offered "Recently updated". It must fall back to the default
  // rather than break the page it was pasted into.
  await page.goto(
    `/acme/widget/issues?q=${encodeURIComponent("is:issue sort:updated")}`,
  );
  await expect(page.getByRole("listitem")).toHaveCount(2);
  expect(seen.lists.at(-1)?.searchParams.get("sort")).toBe("newest");

  // And the menu cannot produce it either. Ordering by `updated_at`
  // while paging by `number` skips or repeats rows on the second page.
  await page.getByLabel("Sort").click();
  const options = await page.getByRole("option").allInnerTexts();
  expect(options).toEqual(["Newest", "Oldest"]);
});

test("the author's id is on the wire and never on the page", async ({
  page,
}) => {
  await onIssues(page, { comments: THREAD });
  await page.goto("/acme/widget/issues/7");
  await expect(page.getByText("It stops at 97% every time.")).toBeVisible();
  // Putting an id in front of a person is how a byline turns into
  // `01j7f2…`. The handle is what a reader is owed.
  await expect(page.locator("body")).toContainText("ada");
  await expect(page.locator("body")).not.toContainText(
    "01ADAADAADAADAADAADAADAADA",
  );
});

test("a label colour this bundle has never seen still renders a pill", async ({
  page,
}) => {
  // A version skew, not bad data: the palette grew and this bundle is
  // older than the database. A blank or missing pill would read as the
  // row having failed to load.
  await onIssues(page, {
    issues: [
      issue({
        number: 7,
        title: "Widget wobbles",
        labels: [FUTURE_LABEL, LABELS[2]],
      }),
    ],
  });
  await page.goto("/acme/widget/issues");
  const only = page.getByRole("listitem").first();
  await expect(only.getByText("needs-triage")).toBeVisible();
  // Readable, not merely present: the name has to have a size on the
  // page, which is what a collapsed or hidden pill would not.
  const box = await only.getByText("needs-triage").boundingBox();
  expect(box && box.width).toBeGreaterThan(0);
  expect(box && box.height).toBeGreaterThan(0);
  // And the neutral pill beside it looks deliberate rather than broken:
  // a dot that resolved to nothing would paint no background at all.
  const painted = await page.evaluate(() => {
    const dots = [...document.querySelectorAll('span[aria-hidden="true"]')]
      .filter((el) => el.className.includes("rounded-full"))
      .map((el) => getComputedStyle(el).backgroundColor);
    return dots.filter(
      (c) => c && c !== "rgba(0, 0, 0, 0)" && c !== "transparent",
    ).length;
  });
  expect(painted).toBeGreaterThanOrEqual(2);
});

// ---------------------------------------------------------------------
// Labels as a thing a maintainer can actually change.
//
// The server has had `POST /labels`, `DELETE /labels/:name` and
// `PUT /issues/:n/labels` since labels existed; the client reached only
// the GET that fills the filter menu. So a repository that was not
// imported from GitHub started with an empty vocabulary and could never
// be given one from the browser: the "Any label" filter stayed empty, no
// issue could carry a pill, and `label:"good first issue"` — which the
// query parser itself calls the label every "help wanted" list depends
// on — was unreachable. Triage was curl or nothing.
//
// These assert the **requests**, because a control that looks right and
// sends the wrong body is the same defect wearing a nicer coat.

/// A writer's view of the same page: the repo row is what carries the
/// answer, so it is re-registered after `onIssues` to win.
async function asWriter(page: Page) {
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: { ...widget, viewer_write: true } }),
  );
}

test("a reader sees labels and is offered no control that would refuse them", async ({
  page,
}) => {
  // The server refuses labelling from a reader with a sentence about
  // triage being the maintainer's. A control whose only job is to
  // deliver that sentence is the pattern this codebase already took off
  // the Changes tab.
  await onIssues(page, { detail: { ...OPEN_ISSUES[0] } });
  await page.goto("/acme/widget/issues/7");

  await expect(page.getByText("bug", { exact: true }).first()).toBeVisible();
  await expect(
    page.getByRole("button", { name: /add a label|edit labels/i }),
  ).toHaveCount(0);
  await expect(page.getByRole("link", { name: "Manage labels" })).toHaveCount(
    0,
  );
});

test("a maintainer applies a label, and the whole set is sent", async ({
  page,
}) => {
  await signIn(page);
  await onIssues(page, {
    ownSession: true,
    detail: { ...issue({ number: 7 }), labels: [LABELS[0]] },
  });
  await asWriter(page);
  let sent: unknown = null;
  await page.route("**/v1/orgs/acme/repos/widget/issues/*/labels", (r) => {
    sent = r.request().postDataJSON();
    return r.fulfill({
      status: 200,
      json: { ...issue({ number: 7 }), labels: [LABELS[0], LABELS[1]] },
    });
  });
  await page.goto("/acme/widget/issues/7");

  await page.getByRole("button", { name: "Edit labels" }).click();
  await page
    .getByRole("button", { name: "Apply label good first issue" })
    .click();

  // The endpoint replaces the set, so the request carries both — the one
  // already on the issue and the one just added. Sending only the new
  // name would silently strip the others.
  await expect
    .poll(() => sent, { message: "the label PUT never went" })
    .toEqual({ labels: ["bug", "good first issue"] });
});

test("a maintainer removes a label, and the remainder is sent", async ({
  page,
}) => {
  await signIn(page);
  await onIssues(page, {
    ownSession: true,
    detail: { ...issue({ number: 7 }), labels: [LABELS[0], LABELS[1]] },
  });
  await asWriter(page);
  let sent: unknown = null;
  await page.route("**/v1/orgs/acme/repos/widget/issues/*/labels", (r) => {
    sent = r.request().postDataJSON();
    return r.fulfill({
      status: 200,
      json: { ...issue({ number: 7 }), labels: [LABELS[1]] },
    });
  });
  await page.goto("/acme/widget/issues/7");

  await page.getByRole("button", { name: "Edit labels" }).click();
  await page.getByRole("button", { name: "Remove label bug" }).click();
  await expect.poll(() => sent).toEqual({ labels: ["good first issue"] });
});

test("the label manager creates one, with a token colour and not a hex", async ({
  page,
}) => {
  await signIn(page);
  await onIssues(page, { ownSession: true });
  await asWriter(page);
  let sent: unknown = null;
  await page.route("**/v1/orgs/acme/repos/widget/labels", (r) => {
    if (r.request().method() !== "POST") {
      return r.fulfill({ status: 200, json: { labels: LABELS } });
    }
    sent = r.request().postDataJSON();
    return r.fulfill({ status: 201, json: LABELS[0] });
  });
  await page.goto("/acme/widget/issues/labels");

  await page.getByLabel("Name").fill("needs repro");
  await page.getByLabel("Colour").selectOption("status-warning");
  await page.getByLabel("Description").fill("cannot act until it reproduces");
  await page.getByRole("button", { name: "Create label" }).click();

  // A token name, never a hex: a stored hex is unfixable when the
  // palette moves, and the server refuses one anyway.
  await expect
    .poll(() => sent, { message: "the create never went" })
    .toEqual({
      name: "needs repro",
      color: "status-warning",
      description: "cannot act until it reproduces",
    });
});

test("the label manager deletes one by name", async ({ page }) => {
  await signIn(page);
  await onIssues(page, { ownSession: true });
  await asWriter(page);
  let deleted: string | null = null;
  await page.route("**/v1/orgs/acme/repos/widget/labels/*", (r) => {
    deleted = decodeURIComponent(
      new URL(r.request().url()).pathname.split("/").pop() ?? "",
    );
    return r.fulfill({ status: 204, body: "" });
  });
  await page.goto("/acme/widget/issues/labels");

  // By name, and encoded: "good first issue" has spaces in it, which is
  // the whole reason the query language quotes it too.
  await page
    .getByRole("button", { name: "Delete label good first issue" })
    .click();
  await expect.poll(() => deleted).toBe("good first issue");
});

// ---------------------------------------------------------------------
// Editing an issue's text, and who is offered the controls.
//
// `PatchIssue` has accepted `title`, `body` and `state` all along; the
// client's only PATCH was `setIssueState`, so once filed an issue's text
// was immutable in the browser — no fixing a typo, no adding the repro
// steps a maintainer asked for, no ticking a checklist you wrote.
//
// The server allows both editing and closing to a writer **or** the
// issue's author. The close button was gated on merely being signed in,
// so a reader who wrote none of it was offered a control that could
// only come back 403.

/// `signIn` authenticates with a token, so `/v1/auth/me` stays 401 and
/// `me` is null. Authorship is decided by comparing ids, so a test about
/// the author needs a real `me` whose id matches the issue's.
async function asAuthor(page: Page, id = "01ADAADAADAADAADAADAADAADA") {
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({
      status: 200,
      json: {
        id,
        email: "ada@acme.test",
        name: "Ada",
        handle: "ada",
        created_at: Date.now(),
        verified_at: Date.now(),
        orgs: [{ id: "01org", name: "acme", role: "viewer" }],
      },
    }),
  );
}

test("an issue's author can edit its title and body", async ({ page }) => {
  await onIssues(page, {
    ownSession: true,
    detail: { ...issue({ number: 7 }), title: "typo in teh title" },
  });
  await asAuthor(page);
  let sent: unknown = null;
  await page.route("**/v1/orgs/acme/repos/widget/issues/7", (r) => {
    if (r.request().method() !== "PATCH") {
      return r.fulfill({
        status: 200,
        json: { ...issue({ number: 7 }), title: "typo in teh title" },
      });
    }
    sent = r.request().postDataJSON();
    return r.fulfill({
      status: 200,
      json: { ...issue({ number: 7 }), title: "typo in the title" },
    });
  });
  await page.goto("/acme/widget/issues/7");

  await page.getByRole("button", { name: "Edit issue #7" }).click();
  await page.getByLabel("Title").fill("typo in the title");
  await page.getByLabel("Description").fill("now with repro steps");
  await page.getByRole("button", { name: "Save", exact: true }).click();

  await expect
    .poll(() => sent, { message: "the edit never went" })
    .toEqual({ title: "typo in the title", body: "now with repro steps" });
});

test("a reader who did not write the issue is offered neither edit nor close", async ({
  page,
}) => {
  // Not the author (different id) and no write access. The server would
  // refuse both with "only the issue's author or somebody with write
  // access may change it"; a button whose only outcome is that sentence
  // is the pattern this codebase keeps removing.
  await onIssues(page, {
    ownSession: true,
    detail: { ...issue({ number: 7 }) },
  });
  await asAuthor(page, "01SOMEBODYELSESOMEBODYELSE");
  await page.goto("/acme/widget/issues/7");

  // The page is there and readable, and commenting still is: filing and
  // commenting need only read access, which is what makes the tracker
  // usable by everybody who can see the repository.
  await expect(page.getByRole("button", { name: "Comment" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Edit issue #7" })).toHaveCount(
    0,
  );
  await expect(page.getByRole("button", { name: /close issue/i })).toHaveCount(
    0,
  );
});

test("a maintainer can edit somebody else's issue", async ({ page }) => {
  await signIn(page);
  await onIssues(page, {
    ownSession: true,
    detail: { ...issue({ number: 7 }) },
  });
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: { ...widget, viewer_write: true } }),
  );
  await page.goto("/acme/widget/issues/7");

  // Write access is the other half of the server's rule, and it does not
  // depend on `me` — a maintainer signed in with a token has no `me` at
  // all, which is exactly the case that would break if authorship were
  // the only path to the control.
  await expect(
    page.getByRole("button", { name: "Edit issue #7" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: /close issue/i }),
  ).toBeVisible();
});
