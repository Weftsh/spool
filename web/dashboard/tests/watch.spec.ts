// The Watch control on a repository page.
//
// A subscription is the one thing on a repo page that is about *you*
// rather than about the repository. So the things worth pinning are: it
// shows the state the server says, it sends the state you picked, it
// does not lie to you when the write fails, and it carries the count
// from the repository row, so the identity row never changes width
// after paint while the subscription read is in flight.

import { expect, test, type Page } from "@playwright/test";
import { ME, REPOS } from "./fixtures";

const widget = { ...REPOS.repos[0] };

const WATCH = "**/v1/orgs/acme/repos/widget/watch";

/// A signed-in person on a repository page.
///
/// `me` answers, because a subscription needs somebody to belong to;
/// the repo read answers, because the About panel and the masthead are
/// what put the control on screen in the first place.
async function onRepoPage(page: Page) {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 200, json: ME }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: widget }),
  );
}

/// The `/watch` endpoint, as a pair: what GET says now, and what PUT
/// does. Returns the bodies PUT was called with, so a test can assert on
/// the request rather than on the rendering that followed it.
function watchEndpoint(
  page: Page,
  opts: { level: string; put?: { status: number; body?: unknown } },
) {
  const puts: unknown[] = [];
  page.route(WATCH, (route) => {
    const request = route.request();
    if (request.method() === "PUT") {
      puts.push(request.postDataJSON());
      const put = opts.put ?? { status: 200 };
      return route.fulfill({
        status: put.status,
        json: put.body ?? { error: "no" },
      });
    }
    return route.fulfill({ status: 200, json: { level: opts.level } });
  });
  return puts;
}

/// The trigger, addressed by the state it is reporting. Naming it this
/// way is deliberate: a test that found the button by a fixed name could
/// not tell an optimistic update from a control that never moved.
///
/// Anchored rather than exact, because the accessible name now carries
/// the count as well — "Watch 0". The `\b` matters: without it `^Watch`
/// would also match "Watching", and the test that proves a refused write
/// puts "Watching" back would pass against a button reading "Watch".
function trigger(page: Page, label: "Watch" | "Watching" | "Ignoring") {
  return page.getByRole("button", { name: new RegExp(`^${label}\\b`) });
}

test("the level the server reports is the one carrying a check", async ({
  page,
}) => {
  await onRepoPage(page);
  watchEndpoint(page, { level: "all" });
  await page.goto("/acme/widget");

  await expect(trigger(page, "Watching")).toBeVisible();
  await trigger(page, "Watching").click();

  // Exclusive choice, announced as one: exactly one of the three is
  // checked, and it is the one the server named. Asserting the other two
  // are unchecked is not padding — a menu that drew a check against
  // everything would pass a single positive assertion.
  await expect(
    page.getByRole("menuitemradio", { name: /^All Activity/ }),
  ).toBeChecked();
  await expect(
    page.getByRole("menuitemradio", { name: /^Participating and @mentions/ }),
  ).not.toBeChecked();
  await expect(
    page.getByRole("menuitemradio", { name: /^Ignore/ }),
  ).not.toBeChecked();
});

test("each row says what it will do, and Custom is not among them", async ({
  page,
}) => {
  await onRepoPage(page);
  watchEndpoint(page, { level: "participating" });
  await page.goto("/acme/widget");
  await trigger(page, "Watch").click();

  // The descriptions are the feature — three near-synonyms with no
  // explanation ask the reader to guess what happens to their attention.
  await expect(
    page.getByText("Notified of everything that happens in this repository."),
  ).toBeVisible();
  await expect(page.getByText("Never notified.")).toBeVisible();
  // Ours, not GitHub's: "needs your review" is computed from OWNERS here.
  await expect(page.getByText(/that need your review/)).toBeVisible();
  // GitHub's fourth item, deliberately absent — it leads to a dialog
  // with nothing behind it.
  await expect(page.getByRole("menuitemradio", { name: /Custom/ })).toHaveCount(
    0,
  );
});

test("choosing a level PUTs that level and shows it at once", async ({
  page,
}) => {
  await onRepoPage(page);
  const puts = watchEndpoint(page, {
    level: "participating",
    put: { status: 200, body: { level: "all" } },
  });
  await page.goto("/acme/widget");

  await trigger(page, "Watch").click();
  await page.getByRole("menuitemradio", { name: /^All Activity/ }).click();

  await expect(trigger(page, "Watching")).toBeVisible();
  // The body the server actually receives. A client that posted the
  // label, or a double-encoded string, would still repaint the button.
  expect(puts).toEqual([{ level: "all" }]);
});

test("a refused write puts the old state back and says so", async ({
  page,
}) => {
  await onRepoPage(page);
  watchEndpoint(page, {
    level: "all",
    put: { status: 500, body: { error: "nope" } },
  });
  await page.goto("/acme/widget");

  await trigger(page, "Watching").click();
  await page.getByRole("menuitemradio", { name: /^Ignore/ }).click();

  // The optimistic update is a promise the server just broke. Leaving
  // "Ignoring" on screen would mean the page and the server disagree
  // about something a person will not check again for months.
  await expect(
    page.getByText("Could not change what you are notified about"),
  ).toBeVisible();
  await expect(trigger(page, "Watching")).toBeVisible();
  await expect(trigger(page, "Ignoring")).toHaveCount(0);
});

test("the control carries the count, and is the same size as Fork beside it", async ({
  page,
}) => {
  await onRepoPage(page);
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: { ...widget, watcher_count: 12 } }),
  );
  watchEndpoint(page, { level: "participating" });
  await page.goto("/acme/widget");

  // From the repository row, so it is there before the subscription
  // read answers.
  await expect(trigger(page, "Watch")).toContainText("12");

  // Watch and Fork are one row of siblings, and a row where one sibling
  // is a different height is the first thing a person notices about the
  // page. Heights, not widths: the widths differ legitimately with the
  // word and the count.
  const boxes = [];
  for (const b of [
    trigger(page, "Watch"),
    page.getByRole("button", { name: /^Fork this repository/ }),
  ]) {
    await expect(b).toBeVisible();
    boxes.push(await b.boundingBox());
  }
  const heights = boxes.map((b) => b?.height ?? -1);
  expect(Math.max(...heights) - Math.min(...heights)).toBeLessThanOrEqual(1);
  // And they share a top edge, which is the thing a baseline shift
  // actually breaks and equal heights alone would not catch.
  const tops = boxes.map((b) => b?.y ?? -1);
  expect(Math.max(...tops) - Math.min(...tops)).toBeLessThanOrEqual(1);
});
