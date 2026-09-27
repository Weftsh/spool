// Blocked workflow runs, as a person meets them.
//
// Every refusal reaches the dashboard as one state. A workflow run that
// is `blocked` mirrors into the checks list as **queued** — deliberately,
// because nothing is wrong with the change and a red row would tell its
// author to go and fix code that is fine — so on every page that lists
// checks, a build waiting on a person is pixel-for-pixel a build about
// to start. The reason sits on the run, where nothing on those pages
// looks.
//
// What is pinned here is the half a unit test cannot see:
//
// - **the reason reaches the page at all**, joined from a second
//   request, and reads as `Blocked` rather than `Queued`;
// - **it is verbatim.** The walkthrough asserts these sentences
//   character for character, and a page that paraphrased would be
//   inventing a reason the server never gave;
// - **the runs arrive late.** Their route is delayed on purpose: the
//   approval panel and the row's `Blocked` both depend on a prop that
//   lands after the first paint, and a mock that answers instantly
//   cannot see that class of bug. This suite has shipped one;
// - **approval is a control, so it is gated** — offered to somebody who
//   may land the change, explained to somebody who may not, never a
//   browser dialog, and never fired by the first click;
// - **the tip is what is approved.** A run blocked against the previous
//   patchset must not put a button on this page.

import { expect, test, type Page } from "@playwright/test";
import { ME, REPOS } from "./fixtures";

const NOW = Date.now();
const TIP = "1".repeat(40);
const OLD = "9".repeat(40);

const FORK =
  "this change comes from a fork; a maintainer has to approve its workflows before they run";
/// A refusal that is not a fork's, under a code this bundle does not
/// know — the shape any reason other than `fork` arrives in.
const PAUSED =
  "workflows are paused for this repository by an organization administrator";

// `native`, and not incidentally: `REPOS.repos[0]` is a mirror, and a
// mirror can never carry a change — `changes_api::create` refuses one at
// the door ("a mirror's trunk belongs to its origin"), so no change row
// on one can exist. The repository page stopped offering a Changes tab
// to mirrors, which is what made this fixture's contradiction visible.
const widget = {
  ...REPOS.repos[0],
  name: "widget",
  kind: "native",
  viewer_admin: false,
  viewer_write: false,
};

interface Run {
  id: string;
  blocked_reason?: string | null;
  file: string;
  name: string;
  commit_sha: string;
  ref_name: string | null;
  event: string;
  change_key: string | null;
  state: string;
  error: string | null;
  created_at: number;
  updated_at: number;
  completed_at: number | null;
  jobs: unknown[];
}

function run(over: Partial<Run> = {}): Run {
  return {
    id: "wr1",
    file: ".weft/ci.yml",
    name: "CI",
    commit_sha: TIP,
    ref_name: null,
    event: "change",
    change_key: "Icafe1234",
    state: "blocked",
    blocked_reason: "fork",
    error: FORK,
    created_at: NOW - 60_000,
    updated_at: NOW - 60_000,
    completed_at: NOW - 60_000,
    jobs: [],
    ...over,
  };
}

/// The check row the mirror writes for a refused run: named for the
/// **file**, carrying the run's id, and coloured `queued`. Everything
/// this file is about follows from that last word.
function refusalRow(over: Record<string, unknown> = {}) {
  return {
    name: ".weft/ci.yml",
    state: "queued",
    url: null,
    required: false,
    source: "commit",
    posted_by: "weft",
    updated_at: NOW - 60_000,
    ...over,
  };
}

interface Options {
  /// `true` gives the viewer `repo:write` — the same scope the land
  /// route demands, and so the same one approval is gated on.
  write?: boolean;
  /// The workflow runs the repository answers with.
  runs?: Run[];
  /// The rows the change's checks route answers with.
  checks?: Record<string, unknown>[];
  /// How long the runs route waits. Never zero: the panel and the
  /// row's state both depend on a prop that arrives after the first
  /// paint.
  runsDelay?: number;
  /// Hold the runs route until the test says so.
  holdRuns?: boolean;
  /// What the change route says its `source` is. `null` is a change
  /// this repository does not know to be from a fork.
  source?: string | null;
  /// What the approve POST answers. `null` is the ordinary 202.
  approveStatus?: number;
  approveError?: string;
}

interface Mocked {
  approvals: number;
  runsQueries: string[];
  /// Let the held runs route answer. Only meaningful with `holdRuns`.
  ///
  /// A gate rather than a longer delay: "assert the panel is absent
  /// before the runs arrive" is only a real assertion if the runs
  /// demonstrably have not arrived, and a timing race dressed as a test
  /// passes for the wrong reason on a slow machine — the exact class
  /// `workers: 1` is pinned to keep out of this suite.
  releaseRuns: () => void;
}

/// The change page on the forge mount, with nothing reachable but what
/// this file mocked.
///
/// The catch-all goes first: Playwright matches the most recently
/// registered handler, so registering it first leaves it as the
/// fallback. Without it a call this file did not anticipate falls
/// through vite's proxy to whatever is on :8080 — during development a
/// real seeded server.
async function changePage(page: Page, opts: Options = {}): Promise<Mocked> {
  let release = () => {};
  const held = new Promise<void>((r) => {
    release = r;
  });
  const out: Mocked = { approvals: 0, runsQueries: [], releaseRuns: release };
  let runs = opts.runs ?? [run()];

  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 200, json: ME }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({
      status: 200,
      json: { ...widget, viewer_write: opts.write === true },
    }),
  );

  // Anchored at the API path, not at `**/changes/…`: `page.route`
  // intercepts the *document* request too, and the change page's own
  // address ends in the same segments — a loose glob answers the
  // navigation itself with JSON, and the browser renders the API
  // response as the page.
  const ps = {
    number: 2,
    commit: TIP,
    parent: null,
    message: "add gateway\n\nChange-Id: Icafe1234\n",
    created_at: NOW - 60_000,
  };
  const change = {
    key: "Icafe1234",
    title: "add gateway",
    target_branch: "main",
    // From a fork. This is the structural half of the gate: the prose of
    // the refusal is never parsed to decide whether a button appears.
    source: opts.source === undefined ? "outsider/widget" : opts.source,
    state: "open",
    land_verdict: null,
    landed_commit: null,
    created_at: NOW - 60_000,
    updated_at: NOW - 60_000,
    patchset: ps,
  };

  await page.route("**/v1/orgs/acme/repos/widget/changes/Icafe1234", (r) =>
    r.fulfill({ json: { change, patchsets: [ps], approvals: [] } }),
  );
  await page.route(
    "**/v1/orgs/acme/repos/widget/changes/Icafe1234/verdict",
    (r) =>
      r.fulfill({
        json: {
          change: "Icafe1234",
          state: "open",
          patchset: 2,
          commit: TIP,
          verdict: {
            landable: true,
            explanation: "ok: all changed path(s) approved",
            per_path: [],
          },
        },
      }),
  );
  await page.route(
    "**/v1/orgs/acme/repos/widget/changes/Icafe1234/comments",
    (r) => r.fulfill({ json: { comments: [] } }),
  );
  await page.route(
    "**/v1/orgs/acme/repos/widget/changes/Icafe1234/checks",
    (r) =>
      r.fulfill({
        json: {
          patchset: 2,
          checks: opts.checks ?? [refusalRow()],
          required_checks: [],
        },
      }),
  );
  await page.route(
    "**/v1/orgs/acme/repos/widget/changes/Icafe1234/associations",
    (r) => r.fulfill({ json: { author: "contributor", authors: {} } }),
  );
  await page.route(
    "**/v1/orgs/acme/repos/widget/changes/Icafe1234/views",
    (r) => r.fulfill({ json: { patchset: 2, viewed: [] } }),
  );

  await page.route(
    "**/v1/orgs/acme/repos/widget/changes/Icafe1234/workflows/approve",
    (r) => {
      out.approvals += 1;
      if (opts.approveStatus)
        return r.fulfill({
          status: opts.approveStatus,
          json: { error: opts.approveError ?? "no" },
        });
      // What approval does: the blocked runs at this tip become real ones.
      runs = runs.map((x) =>
        x.state === "blocked" && x.commit_sha === TIP
          ? { ...x, state: "running", error: null }
          : x,
      );
      return r.fulfill({
        status: 202,
        json: { runs: runs.filter((x) => x.state === "running") },
      });
    },
  );

  await page.route(/\/workflow-runs(\?|$)/, async (r) => {
    out.runsQueries.push(new URL(r.request().url()).search);
    if (opts.holdRuns) await held;
    await new Promise((done) => setTimeout(done, opts.runsDelay ?? 300));
    return r.fulfill({ status: 200, json: { runs } });
  });

  await page.goto("/acme/widget/changes/Icafe1234");
  return out;
}

function approvalPanel(page: Page) {
  return page.getByText("A workflow is waiting for approval");
}

test("a fork's blocked run is explained, and a writer is offered approval in two steps", async ({
  page,
}) => {
  const m = await changePage(page, { write: true, holdRuns: true });

  // The runs route is *held*, not merely slow. The rest of the change
  // page renders from its own requests, and while the runs have not
  // arrived there is nothing to approve and nothing to explain — the
  // page must not have guessed at either. This is the assertion a mock
  // that answers instantly cannot make.
  await expect(page.getByText("add gateway").first()).toBeVisible();
  await expect(approvalPanel(page)).toHaveCount(0);
  await expect(page.getByText(FORK)).toHaveCount(0);
  // And the review itself is not held hostage by its own garnish. The
  // runs read began life inside the load's `Promise.all`, which made the
  // diff, the verdict and the land button all wait on a request that
  // only annotates them: a deployment whose workflow-runs route was slow
  // showed "Loading…" over a review that had everything it needed.
  await expect(
    page.getByRole("button", { name: "Land on main" }),
  ).toBeVisible();

  m.releaseRuns();
  await expect(approvalPanel(page)).toBeVisible();
  // Verbatim, and beside the file it is about — twice, once on the
  // check row and once in the panel that offers to unblock it. Both are
  // deliberate: a reader scanning the checks list and a reader deciding
  // whether to press the button are two different moments, and neither
  // should have to go and find the sentence somewhere else.
  await expect(page.getByText(".weft/ci.yml").first()).toBeVisible();
  await expect(page.getByText(FORK)).toHaveCount(2);

  // One click arms; it does not run anything. A control that starts a
  // stranger's code on the organization's runners does not fire on a
  // stray click.
  await page.getByRole("button", { name: "Approve and run workflows" }).click();
  expect(m.approvals).toBe(0);
  await expect(
    page.getByText("This runs code from a fork on this organization’s runners."),
  ).toBeVisible();

  await page.getByRole("button", { name: "Run them" }).click();
  await expect.poll(() => m.approvals).toBe(1);

  // And the panel goes away, because the reason it was there did.
  await expect(approvalPanel(page)).toHaveCount(0);
});

test("backing out of the confirmation sends nothing", async ({ page }) => {
  const m = await changePage(page, { write: true });
  await expect(approvalPanel(page)).toBeVisible();
  await page.getByRole("button", { name: "Approve and run workflows" }).click();
  await page.getByRole("button", { name: "Not yet" }).click();
  await expect(
    page.getByRole("button", { name: "Approve and run workflows" }),
  ).toBeVisible();
  expect(m.approvals).toBe(0);
});

test("a viewer is told who has to act, and is offered no button", async ({
  page,
}) => {
  await changePage(page, { write: false });
  await expect(approvalPanel(page)).toBeVisible();
  // The reason is for everybody: the author of the fork change needs to
  // know their build is waiting on a maintainer, and telling them
  // nothing is how this page becomes a mystery.
  await expect(page.getByText(FORK).first()).toBeVisible();
  await expect(
    page.getByText("Somebody who can land this change has to approve"),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Approve and run workflows" }),
  ).toHaveCount(0);
});

test("a 409 reads as nothing left to approve, not as a failure", async ({
  page,
}) => {
  // 409 is the *benign* outcome: somebody else approved these runs a
  // moment ago, or the fork pushed again and the tip moved under the
  // page. Nothing went wrong and nothing needs retrying, so it must not
  // arrive dressed as an error — "Could not start these workflows"
  // sends a maintainer looking for a permission problem they do not
  // have. The server's own sentence is still repeated verbatim, because
  // it is the only thing that says which of the two happened.
  const m = await changePage(page, {
    write: true,
    approveStatus: 409,
    approveError: "nothing is blocked at this change's current tip",
  });
  await expect(approvalPanel(page)).toBeVisible();
  await page.getByRole("button", { name: "Approve and run workflows" }).click();
  await page.getByRole("button", { name: "Run them" }).click();
  await expect(
    page.getByText("Nothing here is waiting for approval any more"),
  ).toBeVisible();
  await expect(
    page.getByText("nothing is blocked at this change's current tip"),
  ).toBeVisible();
  await expect(page.getByText("Could not start these workflows")).toHaveCount(
    0,
  );
  expect(m.approvals).toBe(1);
});

test("a real failure to approve still reads as a failure", async ({ page }) => {
  // The 409 softening is for 409 only. A 500 — or a 403 from a session
  // that lost its write scope between paint and click — is a failure,
  // and saying "nothing is waiting for approval any more" over one
  // would tell a maintainer their approval landed when it did not.
  const m = await changePage(page, {
    write: true,
    approveStatus: 500,
    approveError: "the workflow store is unavailable",
  });
  await expect(approvalPanel(page)).toBeVisible();
  await page.getByRole("button", { name: "Approve and run workflows" }).click();
  await page.getByRole("button", { name: "Run them" }).click();
  await expect(page.getByText("Could not start these workflows")).toBeVisible();
  await expect(
    page.getByText("the workflow store is unavailable"),
  ).toBeVisible();
  await expect(
    page.getByText("Nothing here is waiting for approval any more"),
  ).toHaveCount(0);
  expect(m.approvals).toBe(1);
});

test("only the tip's blocked runs are offered for approval", async ({
  page,
}) => {
  // Approval is per tip, like GitHub's "Approve and run": a new patchset
  // from the fork is blocked again, because what would run is not what
  // was approved. Without the sha scoping this panel offers a refusal
  // that was dealt with two patchsets ago, and the server answers 409 to
  // a reader who was given no warning.
  //
  // Asserted **positively** — the panel is present and names one file —
  // because "the panel is absent" is a condition that is also true
  // before the runs have arrived at all, and a poll for it passes on the
  // first tick without ever testing the rule.
  await changePage(page, {
    write: true,
    runs: [
      run({ id: "wr0", file: ".weft/old.yml", commit_sha: OLD }),
      run({ id: "wr1", file: ".weft/ci.yml", commit_sha: TIP }),
    ],
  });
  await expect(approvalPanel(page)).toBeVisible();
  const panel = page.locator("div").filter({ hasText: /waiting for approval/ });
  await expect(panel.getByText(".weft/old.yml")).toHaveCount(0);
  // Singular, and one row: the heading counts what is on offer.
  await expect(
    page.getByText("A workflow is waiting for approval"),
  ).toBeVisible();
});

test("the checks panel says Blocked with the reason, not Queued", async ({
  page,
}) => {
  // The row's stored state is `queued`, and that is the whole problem:
  // without the join this reads as CI that is about to start.
  await changePage(page, { write: true });
  await expect(page.getByText(FORK).first()).toBeVisible();
  const panel = page.getByText("Some checks haven't completed yet");
  await expect(panel).toBeVisible();
  await expect(page.getByText("1 pending, 1 blocked")).toBeVisible();
  await expect(
    page.getByText("Blocked", { exact: true }).first(),
  ).toBeVisible();
});

test("a refusal that is not a fork's is told in the server's words", async ({
  page,
}) => {
  // The same machinery, a different refusal — and the page renders it
  // without knowing which one it is.
  await changePage(page, {
    write: true,
    runs: [run({ error: PAUSED })],
  });
  // On the row and in the panel, and the page has not needed to know
  // which refusal this is to render either of them.
  await expect(page.getByText(PAUSED)).toHaveCount(2);
});

/// The Checks tab, where the same refusal has to survive a different
/// join: these rows carry the run's own id in `external_id`, and the
/// change panel's rows do not carry it at all.
async function checksTab(
  page: Page,
  opts: { runs?: Run[] } = {},
): Promise<void> {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 200, json: ME }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: widget }),
  );
  await page.route(/\/checks\/runs(\?|$)/, (r) =>
    r.fulfill({
      status: 200,
      json: {
        runs: [
          {
            id: "c1",
            repo_id: "01aaa",
            commit_sha: TIP,
            ref_name: "main",
            provider: "weft",
            // The mirror writes the run's own id here for a refusal,
            // and names the row for the workflow file.
            external_id: "wr1",
            name: ".weft/ci.yml",
            run_number: null,
            event: "change",
            // The word that makes this whole join necessary.
            state: "queued",
            detail_url: null,
            actor: null,
            started_at: NOW - 60_000,
            completed_at: null,
            created_at: NOW - 60_000,
            updated_at: NOW - 60_000,
          },
        ],
        workflows: [".weft/ci.yml"],
        next_before: null,
      },
    }),
  );
  await page.route(/\/ci\/poll(\?|$)/, (r) =>
    r.fulfill({
      status: 200,
      json: {
        provider: "github",
        connected: false,
        polled: false,
        denied: false,
        error: null,
        high_water: null,
        resuming_from: null,
        retry_in_ms: null,
      },
    }),
  );
  await page.route(/\/workflow-runs(\?|$)/, (r) =>
    r.fulfill({ status: 200, json: { runs: opts.runs ?? [run()] } }),
  );
  await page.goto("/acme/widget/checks");
}

test("the Checks tab reads a blocked run as Blocked, with its reason", async ({
  page,
}) => {
  await checksTab(page);
  // Not "Queued", which is what the row itself says and what this tab
  // showed forever: a reader waiting for a build nothing will ever pick
  // up, with the reason on a page they have no cause to open.
  await expect(page.getByText(FORK)).toBeVisible();
  await expect(
    page.getByRole("img", { name: "Blocked" }).first(),
  ).toBeVisible();
  await expect(page.getByText("Queued")).toHaveCount(0);
});

test("a row whose run is not blocked is left exactly as it was", async ({
  page,
}) => {
  // The other direction, and the one a join like this gets wrong: an
  // annotation that fires on the wrong rows is worse than none, because
  // it is a refusal invented by the page.
  await checksTab(page, { runs: [run({ state: "running", error: null })] });
  await expect(page.getByText("Queued")).toBeVisible();
  await expect(page.getByText(FORK)).toHaveCount(0);
  await expect(page.getByRole("img", { name: "Blocked" })).toHaveCount(0);
});

test("a fork change blocked for another reason gets the reason and no button", async ({
  page,
}) => {
  // Every refusal arrives as one state, and only the fork's is
  // answerable from this page. A fork change blocked for some other
  // reason carries *that* reason: approving it would trigger another
  // run, which blocks again with the same sentence, and the server
  // answers 409 to somebody the page had just invited to press a
  // button. So the reason is shown and the control is not offered.
  //
  // The viewer here *may* land the change — the gate being tested is the
  // reason code, not the permission.
  await changePage(page, {
    write: true,
    runs: [
      run({
        blocked_reason: "paused",
        error: PAUSED,
      }),
    ],
    checks: [refusalRow()],
  });
  // Twice: once on the check row, once nowhere else — the panel that
  // would have carried the second copy is the thing that must be absent.
  await expect(page.getByText(PAUSED)).toHaveCount(1);
  await expect(page.getByText("waiting for approval")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Approve and run workflows" }),
  ).toHaveCount(0);
  // And the row still reads Blocked rather than Queued: nothing about
  // the missing button may make the refusal less visible.
  await expect(page.getByText("Blocked", { exact: true })).toBeVisible();
});

test("a fork change still gets its button when the block is the fork", async ({
  page,
}) => {
  // The other direction, and the one that says the gate is a gate and
  // not a switch that is always off.
  await changePage(page, { write: true });
  await expect(approvalPanel(page)).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Approve and run workflows" }),
  ).toBeVisible();
});

/// The commit page. `/acme/widget/commit/<sha>` with its checks strip,
/// its workflow runs and nothing else it needs to render.
async function commitPage(
  page: Page,
  opts: {
    runs?: Run[];
    checks?: Record<string, unknown>[];
    diff?: Record<string, unknown>;
  } = {},
): Promise<void> {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 200, json: ME }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: widget }),
  );
  await page.route(/\/log(\?|$)/, (r) =>
    r.fulfill({
      json: {
        entries: [
          {
            commit: TIP,
            message: "add gateway\n",
            author: "Ada <ada@example.com> 1756000000 +0000",
            committer: "Ada <ada@example.com> 1756000000 +0000",
            parents: [OLD],
            tree: "t1",
          },
        ],
      },
    }),
  );
  // `{ changes: [] }` — the key `structuralDiff` reads. It is `entries`
  // on the log route one line up, and getting that wrong here blanked
  // the entire page, which is how the guard below came to be written.
  await page.route(/\/diff(\?|$)/, (r) =>
    r.fulfill({ json: opts.diff ?? { changes: [] } }),
  );
  await page.route(/\/commits\/.*\/checks(\?|$)/, (r) =>
    r.fulfill({
      json: {
        runs: opts.checks ?? [
          {
            id: "c1",
            repo_id: "01aaa",
            commit_sha: TIP,
            ref_name: "main",
            provider: "weft",
            external_id: "wr1",
            name: ".weft/ci.yml",
            run_number: null,
            event: "push",
            // The word the whole join exists to override.
            state: "queued",
            detail_url: null,
            actor: null,
            started_at: NOW - 60_000,
            completed_at: null,
            created_at: NOW - 60_000,
            updated_at: NOW - 60_000,
          },
        ],
      },
    }),
  );
  // Delayed, like every other runs route in this file: the strip paints
  // before this answers, so the refusal is a late-arriving prop and a
  // mock that replied instantly could not see a bug in that.
  await page.route(/\/workflow-runs(\?|$)/, async (r) => {
    await new Promise((done) => setTimeout(done, 300));
    return r.fulfill({ json: { runs: opts.runs ?? [run()] } });
  });
  await page.goto(`/acme/widget/commit/${TIP}`);
}

test("the commit strip reads a blocked run as Blocked, with its reason", async ({
  page,
}) => {
  // This is the surface a maintainer lands on for a **branch push**:
  // there is no change to read instead, so "why did my build not start"
  // is asked here. It read "Queued" with no sentence anywhere on it.
  await commitPage(page, {
    runs: [run({ blocked_reason: "paused", error: PAUSED })],
  });
  await expect(page.getByText(PAUSED)).toBeVisible();
  await expect(page.getByText("Blocked", { exact: true })).toBeVisible();
  await expect(page.getByText("Queued")).toHaveCount(0);
});

test("a commit whose run is not blocked keeps the word it had", async ({
  page,
}) => {
  // The direction a join like this gets wrong: an annotation that fires
  // on the wrong row is a refusal invented by the page.
  await commitPage(page, { runs: [run({ state: "running", error: null })] });
  await expect(page.getByText("Queued")).toBeVisible();
  await expect(page.getByText("Blocked", { exact: true })).toHaveCount(0);
  await expect(page.getByText(FORK)).toHaveCount(0);
});

test("a diff body with no changes in it does not blank the commit page", async ({
  page,
}) => {
  // Found by getting a mock wrong, which is the only way to reach it:
  // `structuralDiff` returns `out.changes`, and a 200 without that key
  // put `undefined` into the state that `changes.length` reads. The
  // whole page went white — message, author, checks strip and all —
  // for a fault in the *file list*, which is the least of what a commit
  // page is for. A failed diff read already degrades to an empty list;
  // a malformed one now does the same thing instead of taking the page
  // down with it.
  await commitPage(page, { diff: {} });
  await expect(page.getByText("add gateway")).toBeVisible();
  await expect(page.getByText(FORK)).toBeVisible();
});

test("the run's reason code decides the button, not the change's source", async ({
  page,
}) => {
  // One source of truth. A run blocked with `"fork"` is waiting on the
  // maintainer reading this page whatever our own change record happens
  // to say about where it came from — and two gates that can disagree
  // means the button vanishes on the day they do, with nothing on the
  // page to explain why. The server enforces that a blocked run always
  // carries a code, so the code is the gate.
  await changePage(page, { write: true, source: null });
  await expect(approvalPanel(page)).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Approve and run workflows" }),
  ).toBeVisible();
});
