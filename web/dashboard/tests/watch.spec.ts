// The Watch control on a repository page.
//
// A subscription is the one thing on a public repo page that is about
// *you* rather than about the repository, and it is the only control
// there that writes. So the things worth pinning are: it shows the state
// the server says, it sends the state you picked, it does not lie to you
// when the write fails, and — the case that changed — a stranger gets
// the public count and a way to get an account.
//
// That last one used to be the opposite: the control was absent
// entirely for somebody signed out, on the argument that a subscription
// belongs to a person and `GET …/watch` refuses anybody else, so there
// was no number to draw. The premise is gone. The watcher count now
// rides on the repository row beside the fork count because it is
// public, so Watch does what Fork and Star beside it always did. The
// old rule also cost a visible defect: the control appeared only once
// the auth probe answered, so the identity row changed width after
// paint on every repository page.

import { expect, test, type Page } from "@playwright/test";
import { ME, REPOS } from "./fixtures";

const widget = { ...REPOS.repos[0], public: true };

const WATCH = "**/v1/orgs/acme/repos/widget/watch";

/// A signed-in person on a public repository page.
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

/// A signed-out visitor, with a repository that has watchers.
///
/// Registered without `onRepoPage` so the catch-all is this test's own:
/// an unmocked `/v1` call otherwise proxies to whatever is listening on
/// :8080, and the suite would be hermetic only when nobody had the
/// manual stack up.
async function asStranger(page: Page) {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: { ...widget, watcher_count: 12 } }),
  );
}

test("a stranger sees the watcher count and is asked to sign in", async ({
  page,
}) => {
  await asStranger(page);
  let asked = 0;
  await page.route(WATCH, (r) => {
    asked += 1;
    return r.fulfill({ status: 401, json: { error: "not signed in" } });
  });
  await page.goto("/acme/widget");

  const control = page.getByRole("button", { name: "Sign in to watch" });
  await expect(control).toBeVisible();
  // The count is public information, and it is one of the few honest
  // signals a stranger has about whether anybody is paying attention to
  // this project. Hiding it from exactly the people deciding whether to
  // trust the project is the wrong way round.
  await expect(control).toContainText("12");

  await control.click();
  await expect(page).toHaveURL(/\/login/);

  // And the subscription read is never made. It would 401 by design,
  // and a request whose refusal we can predict is a red line in a
  // stranger's console on every repository page — which the
  // walkthrough's watcher reports as a problem, correctly.
  expect(asked).toBe(0);
});

test("a stranger is offered no menu, since there is nobody to save one for", async ({
  page,
}) => {
  await asStranger(page);
  await page.goto("/acme/widget");

  const control = page.getByRole("button", { name: "Sign in to watch" });
  await expect(control).toBeVisible();
  // Not `aria-haspopup="menu"`: a control that announced a menu and
  // then navigated away instead is worse than one that says plainly it
  // will take you somewhere to sign in.
  await expect(control).not.toHaveAttribute("aria-haspopup", "menu");
});

test("the control is the same size as its two siblings", async ({ page }) => {
  await asStranger(page);
  // An origin, deliberately. The caption Star used to hang under itself
  // was only rendered for a mirror with a known upstream count, so a
  // fixture with `origin: null` would pass against the very code this
  // test exists to pin. This is the state that was broken on screen.
  await page.route("**/v1/orgs/acme/repos/widget/star", (r) =>
    r.fulfill({
      status: 200,
      json: {
        starred: false,
        stars: 4,
        origin: { stars: 60300, at: 1, url: "https://github.com/acme/widget" },
      },
    }),
  );
  await page.goto("/acme/widget");

  // Watch, Fork and Star are one row of siblings, and a row where one
  // sibling is a different height is the first thing a person notices
  // about the page. Star was the odd one out — it wrapped itself in a
  // column to hang the "60.3k on GitHub" caption below the button, which
  // made it taller than the other two inside an `items-center` row and
  // pushed the whole row's baseline. Heights, not widths: the widths
  // differ legitimately with the word and the count.
  const boxes = [];
  // All three named by what they say to a stranger. Star used to be
  // matched as /^Star\b/ because it was the only one of the three with
  // no `ariaLabel` — a signed-out visitor's accessible name for it was
  // "Star 4" on a control that navigates to /login. Now it explains
  // itself like its siblings, so it is matched like them.
  for (const name of [
    /^Sign in to watch/,
    /^Sign in to fork/,
    /^Sign in to star/,
  ]) {
    const b = page.getByRole("button", { name });
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
