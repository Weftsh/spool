// The Checks tab: verdicts other people's build systems reached.
//
// `checks.tsx` exports its logic and `checks.test.ts` covers it — the
// query string round-trips, the state table, the durations, the cursor
// rule. None of that is repeated here. What is here is the half a unit
// test cannot see:
//
// - **the word beside the glyph.** "Did the build pass" is drawn on the
//   red/green axis, which is precisely the axis a red-green colourblind
//   reader cannot resolve, and it is the most consequential bit on the
//   screen. `runStatePresentation` returning a label proves a table; only
//   a rendered row proves the label reached the DOM.
// - **the request, not the rendering.** A rail that filtered the rows it
//   already had would look identical to one that asked the server and
//   would be wrong the moment a workflow's runs are older than a page.
//   So the rail, the URL and "Load more" are all asserted on the query
//   string that left the browser.
// - **the four empty lists that are not the same empty list.** An empty
//   Checks tab is the one CI failure that renders identically to
//   success. A component that collapsed "we may not read your Actions"
//   into "this project has no CI" would pass every unit test in
//   `checks.test.ts` and would tell a maintainer something false in the
//   direction that makes them close the tab. Each state is asserted to
//   render text the other two do not.
// - **overflow and accessible names**, the two things `audit()` fails a
//   walkthrough on, against the inputs that actually produce them: a
//   branch name and a workflow name as long as somebody wants.

import { expect, test, type Page } from "@playwright/test";
import { ME, REPOS } from "./fixtures";

/// The repository the whole file reads, by a signed-in member.
/// `viewer_admin` decides only whether the two write-shaped controls
/// appear, and each test that cares says so.
///
/// `viewer_write: false` for the same reason it says `viewer_admin:
/// false` — the caller here is a reader. It used to be a reader by
/// omission, which stopped being true the moment `REPOS` began
/// describing the owner the signed-in suites sign in as. An authority
/// inherited by accident is one no test in this file is asserting.
const widget = {
  ...REPOS.repos[0],
  viewer_admin: false,
  viewer_write: false,
};

const NOW = Date.now();

interface Run {
  id: string;
  repo_id: string;
  commit_sha: string;
  ref_name: string | null;
  provider: string;
  external_id: string | null;
  name: string;
  run_number: number | null;
  event: string | null;
  state: string;
  detail_url: string | null;
  actor: string | null;
  started_at: number | null;
  completed_at: number | null;
  created_at: number;
  updated_at: number;
}

/// One run, with everything a row draws present unless overridden.
function run(over: Partial<Run> & { id: string; name: string }): Run {
  return {
    repo_id: "01aaa",
    commit_sha: "abc1234def5678901234567890abcdef12345678",
    ref_name: "main",
    provider: "github",
    external_id: "gh-1",
    run_number: 42,
    event: "push",
    state: "passing",
    detail_url: "https://ci.example.test/runs/42",
    actor: "ada",
    started_at: NOW - 300_000,
    completed_at: NOW - 240_000,
    created_at: NOW - 360_000,
    updated_at: NOW - 240_000,
    ...over,
  };
}

/// Three runs in three different states, which is what the colour
/// assertions need: a green, a red and an in-progress one, since those
/// are the three a reader is actually trying to tell apart.
const RUNS: Run[] = [
  run({ id: "r1", name: "build", state: "passing" }),
  run({
    id: "r2",
    name: "lint",
    state: "failing",
    run_number: 41,
    ref_name: "release-2",
    event: "pull_request",
    actor: "grace",
    created_at: NOW - 720_000,
  }),
  run({
    id: "r3",
    name: "e2e",
    state: "running",
    run_number: 40,
    completed_at: null,
    started_at: NOW - 60_000,
    created_at: NOW - 90_000,
  }),
];

const WORKFLOWS = ["build", "e2e", "lint"];

/// A poll status with nothing wrong with it: connected, allowed, and it
/// has looked. Every field is set because `emptyState` branches on three
/// of them and a fixture missing one would make an empty-state
/// assertion pass for the wrong reason.
const POLLED = {
  provider: "github",
  connected: true,
  polled: true,
  denied: false,
  error: null,
  high_water: NOW - 60_000,
  resuming_from: null,
  retry_in_ms: null,
};

interface Options {
  /// The runs the server has, newest first. The route pages over these
  /// with `before`, so "Load more" is exercised against real cursor
  /// arithmetic rather than against a canned second page.
  runs?: Run[];
  workflows?: string[];
  poll?: Record<string, unknown> | null;
  admin?: boolean;
}

interface Mocked {
  /// Every `checks/runs` URL the browser asked for, in order. Asserting
  /// on this rather than on the rows is the point: a client that
  /// filtered locally would leave the rows looking right and this list
  /// wrong.
  runRequests: string[];
  /// `POST /ci/poll` bodies. Counted separately from the reads, because
  /// a mock that did not branch on the method once counted a read as a
  /// write here and made a double-post test pass.
  pollWrites: number;
}

/// The Checks tab, with nothing reachable but what this file mocked.
///
/// The catch-all goes first: Playwright matches the **most recently
/// registered** handler, so registering it first leaves it as the
/// fallback, and every specific route below still wins. Without it a
/// call this file did not anticipate falls through vite's proxy to
/// whatever is on :8080 — during development a real seeded server — and
/// the suite is hermetic only while nobody has the manual stack up.
async function checksPage(page: Page, opts: Options = {}): Promise<Mocked> {
  const out: Mocked = { runRequests: [], pollWrites: 0 };
  const all = opts.runs ?? RUNS;
  const poll = opts.poll === undefined ? POLLED : opts.poll;

  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 200, json: ME }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({
      status: 200,
      json: { ...widget, viewer_admin: opts.admin === true },
    }),
  );

  await page.route(/\/checks\/runs(\?|$)/, (route) => {
    const url = new URL(route.request().url());
    out.runRequests.push(url.pathname + url.search);
    const limit = Number(url.searchParams.get("limit") ?? "30");
    const before = url.searchParams.get("before");
    const workflow = url.searchParams.get("workflow");
    const branch = url.searchParams.get("branch");
    const state = url.searchParams.get("state");
    // The server's own semantics, because the test is about the client
    // asking correctly: a filter the browser sent must actually narrow
    // the answer, or an assertion about the rows could pass against a
    // request that carried nothing.
    const matching = all.filter(
      (r) =>
        (!workflow || r.name === workflow) &&
        (!branch || r.ref_name === branch) &&
        (!state || r.state === state) &&
        (before === null || r.created_at < Number(before)),
    );
    const runs = matching.slice(0, limit);
    return route.fulfill({
      status: 200,
      json: {
        runs,
        // From the response every time, never from the rows: a workflow
        // whose last run is older than this page must not vanish from
        // its own rail the moment somebody pages or filters.
        workflows: opts.workflows ?? WORKFLOWS,
        next_before: runs.length ? runs[runs.length - 1].created_at : null,
      },
    });
  });

  await page.route(/\/ci\/poll(\?|$)/, (route) => {
    // Two verbs on one path. Branching is not defensive tidiness: the
    // read is made on every page view and the write only when somebody
    // presses the control, and a handler that answered both the same
    // way could not tell those apart.
    if (route.request().method() === "POST") {
      out.pollWrites += 1;
      return route.fulfill({ status: 204, body: "" });
    }
    return poll === null
      ? route.fulfill({ status: 500, json: { error: "no" } })
      : route.fulfill({ status: 200, json: poll });
  });

  return out;
}

/// The run rows, and only the run rows.
///
/// Addressed by "a list item carrying a timestamp" rather than by
/// position in the DOM: the workflow rail is a `<ul>` of `<li>` too, and
/// a locator that said "the second list" would go quietly wrong the day
/// the rail is absent — which is exactly the empty-state case.
function rows(page: Page) {
  return page.getByRole("listitem").filter({ has: page.locator("time") });
}

function row(page: Page, name: string) {
  return rows(page).filter({ hasText: name });
}

/// The last runs request the browser made, as a `URLSearchParams`.
function lastQuery(mock: Mocked): URLSearchParams {
  const last = mock.runRequests[mock.runRequests.length - 1];
  return new URL(last, "http://x").searchParams;
}

test("a row carries every fact about a run, and its title leaves for the provider", async ({
  page,
}) => {
  await checksPage(page);
  await page.goto("/acme/widget/checks");

  const build = row(page, "build");
  await expect(build).toHaveCount(1);
  // Everything the row is for, in one read. A page that showed the
  // workflow name and the state alone would be a list of words with no
  // way to tell which build any of them was about.
  await expect(build).toContainText("Passing");
  await expect(build).toContainText("main"); // the branch
  await expect(build).toContainText("#42"); // the run number
  await expect(build).toContainText("abc1234"); // the short sha, not the full one
  await expect(build).not.toContainText("abc1234def"); // …and truncated, at seven
  await expect(build).toContainText("push"); // the event
  await expect(build).toContainText("ada"); // who caused it
  // A relative time, with the absolute one one hover away. The `time`
  // element is what makes it machine-readable; a span of text would
  // read the same and mean nothing.
  const at = build.locator("time");
  await expect(at).toHaveAttribute("datetime", /^\d{4}-/);
  await expect(at).toHaveAttribute("title", /\d/);

  // Out to somebody else's site. `noopener` and `noreferrer` are not
  // decoration on a URL a third party posted to our intake endpoint:
  // without the first the target page holds a handle on this one.
  const link = build.getByRole("link", { name: "build" });
  await expect(link).toHaveAttribute("href", "https://ci.example.test/runs/42");
  const rel = (await link.getAttribute("rel")) ?? "";
  expect(rel).toContain("noopener");
  expect(rel).toContain("noreferrer");
});

test("a run with no detail URL is still a row, and is not a dead link", async ({
  page,
}) => {
  await checksPage(page, {
    runs: [run({ id: "r1", name: "build", detail_url: null })],
  });
  await page.goto("/acme/widget/checks");

  const build = row(page, "build");
  await expect(build).toContainText("Passing");
  // A link that went nowhere would be worse than plain text: this page
  // is a summary and the provider's page is the truth, so a title that
  // looked clickable and did nothing would break the one promise the
  // row makes. Counted, because it is an absence on a settled page.
  expect(await build.getByRole("link").count()).toBe(0);
});

test("every state is a word, never a colour on its own", async ({ page }) => {
  await checksPage(page);
  await page.goto("/acme/widget/checks");

  // The assertion this file exists for. Green tick, red cross and a
  // spinner are three glyphs on the red/green axis, and 8% of men
  // cannot resolve two of them. The word is what makes the verdict
  // readable; asserting it in the row's *text* is what proves it was
  // rendered rather than merely being in the state table.
  await expect(row(page, "build")).toContainText("Passing");
  await expect(row(page, "lint")).toContainText("Failing");
  await expect(row(page, "e2e")).toContainText("Running");

  // And the glyph carries the same word as its accessible name, so a
  // screen reader arriving at the icon is not told "image".
  await expect(
    row(page, "build").getByRole("img", { name: "Passing" }),
  ).toBeVisible();
  await expect(
    row(page, "lint").getByRole("img", { name: "Failing" }),
  ).toBeVisible();
});

test("a state the bundle has never heard of says its own word", async ({
  page,
}) => {
  // A seventh state means the server is newer than this page. Shown
  // green it ships broken code; shown red it blocks good code. The only
  // honest answer is the unknown word itself, and this is the one place
  // it can be checked end to end.
  await checksPage(page, {
    runs: [run({ id: "r1", name: "build", state: "quarantined" })],
  });
  await page.goto("/acme/widget/checks");

  await expect(row(page, "build")).toContainText("quarantined");
  expect(await row(page, "build").getByText("Passing").count()).toBe(0);
});

test("the rail lists every workflow, and picking one asks the server", async ({
  page,
}) => {
  const mock = await checksPage(page);
  await page.goto("/acme/widget/checks");

  const rail = page.getByRole("navigation", { name: "Workflows" });
  await expect(rail.getByRole("button")).toHaveCount(WORKFLOWS.length + 1);
  await expect(
    rail.getByRole("button", { name: "All workflows" }),
  ).toBeVisible();
  for (const w of WORKFLOWS) {
    await expect(
      rail.getByRole("button", { name: w, exact: true }),
    ).toBeVisible();
  }

  await rail.getByRole("button", { name: "lint", exact: true }).click();

  // The **request**, not the rows. A rail that filtered the runs it
  // already had would leave the screen looking exactly like this and
  // would silently drop every matching run older than the first page.
  await expect(async () => {
    expect(lastQuery(mock).get("workflow")).toBe("lint");
  }).toPass();
  await expect(rows(page)).toHaveCount(1);
  await expect(row(page, "lint")).toBeVisible();

  // The rail keeps its full list while filtered — its names come from
  // the response, not from the rows on screen — and says which one is
  // current in markup rather than in colour.
  await expect(rail.getByRole("button")).toHaveCount(WORKFLOWS.length + 1);
  await expect(
    rail.getByRole("button", { name: "lint", exact: true }),
  ).toHaveAttribute("aria-current", "true");
});

test("a filter is written to the address bar", async ({ page }) => {
  await checksPage(page);
  await page.goto("/acme/widget/checks");

  await page
    .getByRole("navigation", { name: "Workflows" })
    .getByRole("button", { name: "lint", exact: true })
    .click();

  // "the arm64 build has been red since Tuesday" has to be a URL
  // somebody can paste, not a description of which controls to set.
  await expect(page).toHaveURL(/\/acme\/widget\/checks\?workflow=lint$/);
});

test("that address, opened cold, asks for the same filtered list", async ({
  page,
}) => {
  const mock = await checksPage(page);
  await page.goto("/acme/widget/checks?workflow=lint&state=failing");

  await expect(rows(page)).toHaveCount(1);
  // The other half of the previous test, and the half that makes the
  // link worth sending: the URL is read on the way in, not only written
  // on the way out. A page that wrote filters to the address bar and
  // ignored them at boot would pass the test above and hand somebody a
  // link that opens an unfiltered list.
  const q = lastQuery(mock);
  expect(q.get("workflow")).toBe("lint");
  expect(q.get("state")).toBe("failing");

  // And the dropdown shows the filter the URL carried, rather than
  // resetting itself to "Any state" and disagreeing with the address.
  await expect(page.getByRole("combobox", { name: "State" })).toContainText(
    "Failing",
  );
});

test("a filter that matches nothing says so, and does not explain CI setup", async ({
  page,
}) => {
  await checksPage(page);
  await page.goto("/acme/widget/checks?branch=no-such-branch");

  await expect(page.getByText("No runs match this filter.")).toBeVisible();
  // The trap: a filtered empty list is not an empty repository.
  // Explaining how to point CI at the intake — while this project's
  // runs sit one dropdown away — answers a question nobody asked and
  // states something false about the project.
  expect(await page.getByText("Point your CI at").count()).toBe(0);
  expect(await page.getByText("GitHub reported no runs").count()).toBe(0);
});

/// Thirty-one runs: a full first page under `PAGE_LIMIT`, and one more
/// behind the cursor. Thirty exactly would make "a short page ends the
/// list" and "there is a next page" indistinguishable.
const MANY: Run[] = Array.from({ length: 31 }, (_, i) =>
  run({
    id: `p${i}`,
    name: `job-${String(i).padStart(2, "0")}`,
    created_at: NOW - i * 60_000,
  }),
);

test("Load more appends the next page and sends the cursor", async ({
  page,
}) => {
  const mock = await checksPage(page, { runs: MANY, workflows: [] });
  await page.goto("/acme/widget/checks");
  await expect(rows(page)).toHaveCount(30);

  const more = page.getByRole("button", { name: /Load more/ });
  await more.click();

  // Appends. A page that replaced would also end at a plausible number
  // of rows, so the assertion that matters is that the *first* page is
  // still there underneath.
  await expect(rows(page)).toHaveCount(31);
  await expect(row(page, "job-00")).toBeVisible();
  await expect(row(page, "job-30")).toBeVisible();

  // And it asked for the runs after the oldest one on screen, rather
  // than re-requesting page one and pasting it below itself.
  expect(lastQuery(mock).get("before")).toBe(String(MANY[29].created_at));
});

test("Load more disappears at the end of the list", async ({ page }) => {
  const mock = await checksPage(page, { runs: MANY, workflows: [] });
  await page.goto("/acme/widget/checks");
  await page.getByRole("button", { name: /Load more/ }).click();
  await expect(rows(page)).toHaveCount(31);

  // The case that gets missed. The server still answers with a cursor —
  // it is the `created_at` of the last row it sent — so a client
  // trusting `next_before` alone leaves the button on screen forever and
  // every press answers with nothing. A short page is the last page.
  expect(lastQuery(mock).get("before")).toBeTruthy();
  expect(await page.getByRole("button", { name: /Load more/ }).count()).toBe(0);
});

test("a list that fits on one page never offers Load more", async ({
  page,
}) => {
  await checksPage(page);
  await page.goto("/acme/widget/checks");
  // Positive assertion first: the rows are on screen, so the absence
  // below is an absence on a settled page rather than one measured
  // during the load. Counted rather than `toHaveCount(0)`, which would
  // retry for its whole window and go green against exactly the bug it
  // is here for.
  await expect(rows(page)).toHaveCount(3);
  expect(await page.getByRole("button", { name: /Load more/ }).count()).toBe(0);
});

// ---------------------------------------------------------------------------
// The empty states, which are four different sentences and not one.
// ---------------------------------------------------------------------------

/// Text belonging to each of the empty states, used both to assert the
/// right one and — the part that gives the tests their teeth — to assert
/// that the other two are absent. A component that collapsed all of them
/// into one panel would fail whichever it was not.
const DENIED = "We cannot read this project's checks";
const WAITING = "Looking for runs";
const INTAKE = "Nothing has reported a check for this repository yet";
const POLLED_EMPTY = "GitHub reported no runs";

async function absent(page: Page, ...texts: string[]) {
  for (const t of texts) {
    expect(await page.getByText(t).count(), `"${t}" should not be here`).toBe(
      0,
    );
  }
}

test("a denied installation says we may not look, and offers the fix", async ({
  page,
}) => {
  await checksPage(page, {
    runs: [],
    workflows: [],
    admin: true,
    poll: { ...POLLED, denied: true },
  });
  await page.goto("/acme/widget/checks");

  await expect(page.getByText(DENIED)).toBeVisible();
  // The sentence has to say the thing that is actually true, because
  // the reader's next move depends on it: their CI is fine and our
  // permission is not.
  await expect(page.getByText(/permission to read Actions/)).toBeVisible();
  await expect(page.getByText(/not a project without CI/)).toBeVisible();
  // And a way out, for somebody who could take it.
  await expect(
    page.getByRole("button", {
      name: "Review this installation's permissions",
    }),
  ).toBeVisible();

  // It must not read as "set your CI up". Offering the intake address
  // to somebody whose Actions we simply may not read answers a question
  // they did not ask, and the other two titles would each be a claim
  // about this repository that is false.
  await absent(page, WAITING, INTAKE, POLLED_EMPTY, "Point your CI at");
});

test("a connected repository that has not been polled yet says so", async ({
  page,
}) => {
  await checksPage(page, {
    runs: [],
    workflows: [],
    poll: { ...POLLED, polled: false },
  });
  await page.goto("/acme/widget/checks");

  await expect(page.getByText(WAITING)).toBeVisible();
  // "We have not looked" and "we looked and found nothing" are
  // different facts, and only one of them is about the project.
  await expect(page.getByText(/we have not looked/)).toBeVisible();
  await absent(page, DENIED, INTAKE, POLLED_EMPTY);
  // No re-approval offered: nothing has been refused.
  expect(
    await page
      .getByRole("button", { name: "Review this installation's permissions" })
      .count(),
  ).toBe(0);
});

test("a repository with no GitHub origin is pointed at the intake", async ({
  page,
}) => {
  await checksPage(page, {
    runs: [],
    workflows: [],
    poll: { ...POLLED, connected: false, polled: false },
  });
  await page.goto("/acme/widget/checks");

  await expect(page.getByText(INTAKE)).toBeVisible();
  // The intake address, with this repository's own names in it, so it
  // is something to copy rather than something to adapt.
  await expect(
    page.getByText("POST /v1/orgs/acme/repos/widget/ci/checks"),
  ).toBeVisible();
  await absent(page, DENIED, WAITING, POLLED_EMPTY);
});

test("a poll status that could not be read admits it rather than guessing", async ({
  page,
}) => {
  // The fifth case, and the one every "empty means no CI" bug is made
  // of: with no poll status, all four sentences above are assertions we
  // cannot support.
  await checksPage(page, { runs: [], workflows: [], poll: null });
  await page.goto("/acme/widget/checks");

  await expect(page.getByText("No runs to show")).toBeVisible();
  await expect(
    page.getByText(/not a statement about whether the project has CI/),
  ).toBeVisible();
  await absent(page, DENIED, WAITING, INTAKE, POLLED_EMPTY);
});

test("a failing poll says the server's own sentence above the runs", async ({
  page,
}) => {
  await checksPage(page, {
    poll: {
      ...POLLED,
      error: "GitHub rate limit exceeded for this installation",
      retry_in_ms: 300_000,
    },
  });
  await page.goto("/acme/widget/checks");

  // A poll that started failing on a repository with a year of history
  // shows no empty panel at all — the rows are still there — so the
  // only place this can be said is above the list.
  await expect(
    page.getByText("GitHub rate limit exceeded for this installation"),
  ).toBeVisible();
  await expect(page.getByText("Retrying in 5m 0s")).toBeVisible();
  await expect(rows(page)).toHaveCount(3);
});

test("the re-poll control is offered only where it would work", async ({
  page,
}) => {
  // Subtractive, like the About rail's edit control: getting it wrong
  // hides a control the server would have accepted and never shows one
  // it will refuse. A reader who cannot poll loses nothing they can see.
  await checksPage(page, { admin: false });
  await page.goto("/acme/widget/checks");
  await expect(rows(page)).toHaveCount(3);
  expect(
    await page.getByRole("button", { name: /Check GitHub again/ }).count(),
  ).toBe(0);
});

test("the re-poll control asks GitHub once, and never says re-run", async ({
  page,
}) => {
  const mock = await checksPage(page, { admin: true });
  await page.goto("/acme/widget/checks");

  const button = page.getByRole("button", { name: "Check GitHub again" });
  await expect(button).toBeVisible();
  // The words are the assertion. Nothing on this page starts a build —
  // Weft runs nobody's code — so a control labelled "Re-run" would
  // promise the one thing the product deliberately does not do.
  expect(await page.getByRole("button", { name: /Re-run/i }).count()).toBe(0);

  const before = mock.runRequests.length;
  await button.click();

  // One write, and the list re-read afterwards. Counted through a mock
  // that branches on the method: a handler answering GET and POST alike
  // would have counted the poll status read on page load as a write and
  // made this pass without a press.
  await expect(async () => {
    expect(mock.pollWrites).toBe(1);
    expect(mock.runRequests.length).toBeGreaterThan(before);
  }).toPass();
});

test("a reader who may not administer reads the checks and is offered no controls", async ({
  page,
}) => {
  // A member with the `viewer` role reads every row, and is not handed a
  // control the server would refuse.
  await checksPage(page, { admin: false });
  await page.goto("/acme/widget/checks");

  await expect(rows(page)).toHaveCount(3);
  await expect(row(page, "build")).toContainText("Passing");
  // And neither write-shaped control, because both would only be
  // refused. Counted, on a page whose rows are already on screen.
  expect(
    await page.getByRole("button", { name: /Check GitHub again/ }).count(),
  ).toBe(0);
  expect(
    await page
      .getByRole("button", { name: "Review this installation's permissions" })
      .count(),
  ).toBe(0);
});

test("/actions redirects to /checks and lands on the real tab", async ({
  page,
}) => {
  await checksPage(page);
  await page.goto("/acme/widget/actions");

  // The address people's fingers type. 404ing them to prove a naming
  // point helps nobody, and two live URLs for one page is two things to
  // share.
  await expect(page).toHaveURL(/\/acme\/widget\/checks$/);
  // And it is the tab, not merely the address: a redirect that landed
  // on a shell with no list would satisfy the URL assertion alone.
  await expect(rows(page)).toHaveCount(3);
  await expect(page.getByRole("heading", { name: "Checks" })).toBeVisible();
});

// ---------------------------------------------------------------------------
// The two things `audit()` fails a walkthrough on.
// ---------------------------------------------------------------------------

/// A branch and a workflow name as long as somebody actually names them.
/// Both are attacker-free, ordinary inputs — a release branch and a
/// matrix job — and both are what makes a missing `min-w-0` widen the
/// page instead of ellipsising.
///
/// The lengths are not arbitrary. They were chosen by defeating
/// `.truncate` in the browser and checking that both viewport widths
/// below then **fail**: a shorter branch name fitted a 1024px column
/// even untruncated, so the desktop test passed against a layout with
/// no truncation at all — an assertion that cannot fail is worse than
/// none, and this is a file where the wide layout is the one with a rail
/// beside a `flex-1` column and therefore the one that overflows.
const LONG = [
  run({
    id: "l1",
    name: "build and test everything on every supported architecture and libc, including the cross-compiled ones (matrix)",
    ref_name:
      "release/2026-08-27-the-one-where-we-finally-fixed-the-packfile-thing-and-also-the-manifest-cas-retry-loop-that-lost-a-push",
  }),
  run({
    id: "l2",
    name: "lint",
    ref_name: "main",
    created_at: NOW - 900_000,
  }),
];

test("every control on the page has a name somebody could say", async ({
  page,
}) => {
  await checksPage(page, { runs: LONG, admin: true });
  await page.goto("/acme/widget/checks");
  await expect(rows(page)).toHaveCount(2);

  // An icon-only button with no `aria-label` is a control that does not
  // exist for anybody not looking at it, and this page is built out of
  // icons. Collected in one pass so a failure names every offender
  // rather than the first.
  const unnamed = await page.evaluate(() => {
    const named = (el: Element): boolean => {
      const label = el.getAttribute("aria-label")?.trim();
      if (label) return true;
      const by = el.getAttribute("aria-labelledby");
      if (by && by.split(/\s+/).some((id) => document.getElementById(id)))
        return true;
      if (el.getAttribute("title")?.trim()) return true;
      return ((el as HTMLElement).innerText ?? "").trim() !== "";
    };
    return [...document.querySelectorAll("button, a, [role='button']")]
      .filter((el) => !named(el))
      .map((el) => el.outerHTML.slice(0, 120));
  });
  expect(unnamed).toEqual([]);
});

test("a long branch name ellipsises rather than widening the page", async ({
  page,
}) => {
  await checksPage(page, { runs: LONG, workflows: [LONG[0].name, "lint"] });
  // Narrow, because that is where a row that refuses to shrink shows
  // up. `audit()` measures `documentElement.scrollWidth` on exactly
  // this, and the defect it catches is a table cell that widens the
  // layout instead of truncating — which this project has shipped
  // before.
  await page.setViewportSize({ width: 380, height: 900 });
  await page.goto("/acme/widget/checks");
  await expect(rows(page)).toHaveCount(2);

  const overflow = await page.evaluate(() => {
    const d = document.documentElement;
    return d.scrollWidth - d.clientWidth;
  });
  expect(overflow).toBeLessThanOrEqual(1);
});

test("and does not widen the page at a desktop width either", async ({
  page,
}) => {
  // The same content, wide. The narrow case can pass by wrapping while
  // a fixed-width rail beside a `flex-1` column with no `min-w-0`
  // overflows only once the two sit side by side, which is the `lg`
  // layout and not the stacked one.
  await checksPage(page, { runs: LONG, workflows: [LONG[0].name, "lint"] });
  await page.setViewportSize({ width: 1024, height: 900 });
  await page.goto("/acme/widget/checks");
  await expect(rows(page)).toHaveCount(2);

  const overflow = await page.evaluate(() => {
    const d = document.documentElement;
    return d.scrollWidth - d.clientWidth;
  });
  expect(overflow).toBeLessThanOrEqual(1);
});

test("composed changeset runs have a place on the member's Checks tab", async ({
  page,
}) => {
  // A changeset's composed runs report to the changeset, not to any one
  // member's commit — `changeset_checks` is kept apart from `check_runs`
  // so a composed "ci / build" cannot collide with the push-time row —
  // and the consequence was that they appeared on the member's Checks
  // tab nowhere at all. Composing three repositories and opening one of
  // them showed the push run and nothing else. Found on the manual pass.
  const mock = await checksPage(page, { admin: false });
  const asked: string[] = [];
  await page.route(/\/workflow-runs(\?|$)/, (route) => {
    const url = new URL(route.request().url());
    asked.push(url.search);
    // The server's own semantics: only a request that asked for the
    // composed runs gets them, so an assertion about the rows cannot
    // pass against a request that carried nothing.
    if (url.searchParams.get("event") !== "changeset")
      return route.fulfill({ status: 200, json: { runs: [] } });
    return route.fulfill({
      status: 200,
      json: {
        runs: [
          {
            id: "01composed",
            file: ".weft/build.yml",
            name: "build",
            commit_sha: "a".repeat(40),
            ref_name: null,
            event: "changeset",
            change_key: "c1",
            changeset: { key: "rel-1" },
            composition: "f".repeat(64),
            state: "passed",
            error: null,
            created_at: Date.now() - 60_000,
            updated_at: Date.now() - 30_000,
            completed_at: Date.now() - 30_000,
            jobs: [],
          },
        ],
      },
    });
  });
  await page.goto("/acme/widget/checks");

  // The push rows are still the push rows — counted outside the panel,
  // whose rows carry a timestamp too.
  await expect(
    page
      .locator('li:not([data-testid="changeset-builds"] li)')
      .filter({ has: page.locator("time") }),
  ).toHaveCount(3);
  const panel = page.getByTestId("changeset-builds");
  await expect(panel).toContainText("Changeset builds");
  await expect(panel).toContainText(
    "They report to the changeset, not to a commit here",
  );
  await expect(panel.getByRole("listitem")).toHaveCount(1);
  await expect(panel.getByRole("listitem")).toContainText("Passing");
  await expect(panel.getByRole("link", { name: "build" })).toHaveAttribute(
    "href",
    "/acme/widget/checks/runs/01composed",
  );
  // The changeset that owns the verdict, on the dashboard mount.
  await expect(panel.getByRole("link", { name: "rel-1" })).toHaveAttribute(
    "href",
    "/dashboard/changesets/rel-1",
  );
  // Asked of the server, by event: a busy repository's push runs would
  // otherwise push the composed ones out of the window.
  expect(
    asked.some((q) => new URLSearchParams(q).get("event") === "changeset"),
  ).toBe(true);
  // The check rows were read as before.
  expect(mock.runRequests.length).toBeGreaterThan(0);
});

test("a repository with no composed runs shows no changeset panel", async ({
  page,
}) => {
  await checksPage(page, { admin: false });
  await page.route(/\/workflow-runs(\?|$)/, (r) =>
    r.fulfill({ status: 200, json: { runs: [] } }),
  );
  await page.goto("/acme/widget/checks");
  await expect(rows(page)).toHaveCount(3);
  await expect(page.getByTestId("changeset-builds")).toHaveCount(0);
});
