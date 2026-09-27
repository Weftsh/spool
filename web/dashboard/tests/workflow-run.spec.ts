// The run page: one workflow run on **our own** runners.
//
// `workflow-run.test.ts` covers the decisions — which job opens, which
// states are live, how a runner's word translates into the Checks tab's
// vocabulary — and `lib/log-stream.test.ts` covers the SSE wire format
// down to the leading space after a colon. None of that is repeated
// here. What is here is the half a unit test cannot see:
//
// - **the log is read from one source, never two.** `…/log` answers with
//   everything so far and `…/log/stream` *replays* from chunk one of the
//   current attempt, so they are a superset and a snapshot rather than a
//   continuation of one another. A page that fetched the log and then
//   appended the stream renders every line that existed at page load
//   twice — which reads as the build having run twice, and is invisible
//   to any fixture whose log starts empty. So the fixture here starts
//   non-empty, deliberately, and the assertion counts occurrences.
// - **the stream is delayed.** A mocked route that answers instantly
//   hides late-arriving-prop bugs, which have shipped from this suite
//   before. Every stream here answers after a beat, and the assertions
//   about what is on screen *before* it answers are as load-bearing as
//   the ones after.
// - **the refused run.** A workflow file we would not run produces no
//   jobs at all, so every log assertion is vacuous and the only thing on
//   the page is the reason. That is exactly the case the page was built
//   for, and the one a fixture with jobs in it would never reach.
// - **cancel is a control, so it is gated.** A viewer must not be shown
//   a button the server would refuse, and pressing it must take two
//   deliberate steps and never a browser dialog.
// - **the link that gets you here.** A check row's `detail_url` used to
//   be somebody else's site without exception. One of ours has to
//   navigate in-app; the assertion is that the page changed without the
//   document being replaced.

import { expect, test, type Page } from "@playwright/test";
import { ME, REPOS } from "./fixtures";
import { PREVIEW_ORIGIN } from "./preview";

const NOW = Date.now();

/// The repository every test reads, by a signed-in reader;
/// `viewer_write` is what decides whether Cancel appears and each test
/// that cares says so.
const widget = {
  ...REPOS.repos[0],
  viewer_admin: false,
  viewer_write: false,
};

interface Job {
  id: string;
  job_id: string;
  key: string;
  matrix: Record<string, unknown>;
  state: string;
  attempts: number;
  error: string | null;
  detail_url: string | null;
  log_chunks: number;
  started_at: number | null;
  completed_at: number | null;
  labels?: string[];
  runner?: { id: string; name: string } | null;
}

interface Run {
  id: string;
  file: string;
  name: string;
  commit_sha: string;
  ref_name: string | null;
  event: string;
  change_key: string | null;
  changeset?: { key: string } | null;
  composition?: string | null;
  state: string;
  error: string | null;
  created_at: number;
  updated_at: number;
  completed_at: number | null;
  jobs: Job[];
}

function job(over: Partial<Job> & { id: string; key: string }): Job {
  return {
    job_id: over.key,
    matrix: {},
    state: "passed",
    attempts: 1,
    error: null,
    detail_url: null,
    log_chunks: 2,
    started_at: NOW - 300_000,
    completed_at: NOW - 240_000,
    ...over,
  };
}

function run(over: Partial<Run> = {}): Run {
  return {
    id: "wr1",
    file: ".weft/ci.yml",
    name: "CI",
    commit_sha: "abc1234def5678901234567890abcdef12345678",
    ref_name: "main",
    event: "push",
    change_key: null,
    state: "passed",
    error: null,
    created_at: NOW - 360_000,
    updated_at: NOW - 240_000,
    completed_at: NOW - 240_000,
    jobs: [job({ id: "j1", key: "build" })],
    ...over,
  };
}

/// One SSE frame, spelled the way the server spells it.
function frame(event: string, data: unknown): string {
  return `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
}

interface Options {
  /// The run, as `GET workflow-runs/{id}` answers. A function is passed
  /// when the answer has to change between reads — a cancel, or a job
  /// settling under a live page — and it is called once per request.
  run?: Run | ((n: number) => Run);
  /// What `…/log` answers, per job id.
  log?: Record<string, string>;
  /// What `…/log/stream` answers, per job id. Delivered after
  /// `streamDelay`.
  stream?: Record<string, string>;
  /// How long the stream route waits before answering. Never zero: a
  /// mock that answers before React has committed the props that opened
  /// it cannot see a late-arriving-prop bug, and this suite has shipped
  /// one.
  streamDelay?: number;
  /// `true` gives the viewer `repo:write`.
  write?: boolean;
  /// What the cancel POST answers. `null` is the ordinary success —
  /// the run, cancelled.
  cancelStatus?: number;
  cancelError?: string;
}

interface Mocked {
  /// Every log URL asked for, in order. Asserting on this is how "read
  /// from one source" is proved: a page that fetched *and* streamed
  /// leaves both here for one live job.
  logRequests: string[];
  cancels: number;
  runReads: number;
}

/// The run page, with nothing reachable but what this file mocked.
///
/// The catch-all goes first: Playwright matches the most recently
/// registered handler, so registering it first leaves it as the
/// fallback and every specific route below still wins. Without it a call
/// this file did not anticipate falls through vite's proxy to whatever
/// is on :8080 — during development a real seeded server — and the suite
/// is hermetic only while nobody has the manual stack up.
async function runPage(page: Page, opts: Options = {}): Promise<Mocked> {
  const out: Mocked = { logRequests: [], cancels: 0, runReads: 0 };
  const answer = opts.run ?? run();
  const delay = opts.streamDelay ?? 400;

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

  await page.route(/\/workflow-runs\/[^/]+$/, (route) => {
    const n = out.runReads++;
    return route.fulfill({
      status: 200,
      json: typeof answer === "function" ? answer(n) : answer,
    });
  });

  await page.route(/\/workflow-runs\/[^/]+\/cancel$/, (route) => {
    out.cancels += 1;
    if (opts.cancelStatus) {
      return route.fulfill({
        status: opts.cancelStatus,
        json: { error: opts.cancelError ?? "no" },
      });
    }
    const current = typeof answer === "function" ? answer(-1) : answer;
    return route.fulfill({
      status: 200,
      json: {
        ...current,
        state: "cancelled",
        error: "cancelled by Ada Owner",
        completed_at: Date.now(),
        jobs: current.jobs.map((j) => ({ ...j, state: "cancelled" })),
      },
    });
  });

  // The stream is registered *before* the plain log route, so the more
  // recently registered plain-log handler would win — except that its
  // pattern ends at `/log`, which `…/log/stream` does not match. Both
  // patterns are anchored for exactly that reason: a `/log/` prefix
  // match would answer the stream with plain text and the page would sit
  // on an empty log with nothing to explain it.
  await page.route(/\/workflow-jobs\/([^/]+)\/log\/stream$/, async (route) => {
    const url = new URL(route.request().url());
    out.logRequests.push(url.pathname);
    const id = url.pathname.split("/").at(-3) as string;
    await new Promise((r) => setTimeout(r, delay));
    return route.fulfill({
      status: 200,
      headers: { "content-type": "text/event-stream" },
      body: opts.stream?.[id] ?? "",
    });
  });

  await page.route(/\/workflow-jobs\/([^/]+)\/log$/, (route) => {
    const url = new URL(route.request().url());
    out.logRequests.push(url.pathname);
    const id = url.pathname.split("/").at(-2) as string;
    return route.fulfill({
      status: 200,
      headers: { "content-type": "text/plain; charset=utf-8" },
      body: opts.log?.[id] ?? "",
    });
  });

  return out;
}

function logPane(page: Page) {
  return page.getByRole("log", { name: "Build log" });
}

/// How many times `needle` appears in `haystack`. The duplicate-log bug
/// is a counting bug, and `toContainText` cannot see it.
function occurrences(haystack: string, needle: string): number {
  return haystack.split(needle).length - 1;
}

test("a settled run shows its header, its jobs and the log of the one that failed", async ({
  page,
}) => {
  await runPage(page, {
    run: run({
      state: "failed",
      jobs: [
        job({ id: "j1", key: "lint" }),
        job({
          id: "j2",
          key: "build (linux)",
          state: "failed",
          error: "exit status 101",
        }),
      ],
    }),
    log: {
      j1: "▶ Lint\nok\n",
      j2: "▶ Build\nerror[E0308]: mismatched types\n",
    },
  });
  await page.goto("/acme/widget/checks/runs/wr1");

  // The header, in one read: which workflow, what caused it, on what,
  // from which file, and how it ended.
  await expect(page.getByRole("heading", { name: "CI" })).toBeVisible();
  const header = page.locator("h1").locator("..");
  await expect(header).toContainText("Failing");
  await expect(header).toContainText("push on main");
  await expect(header).toContainText(".weft/ci.yml");
  // The short sha, truncated at seven, and a link to the commit.
  const sha = page.getByRole("link", { name: "abc1234" });
  await expect(sha).toHaveAttribute(
    "href",
    "/acme/widget/commit/abc1234def5678901234567890abcdef12345678",
  );

  // Both jobs are listed, each with its state as a **word** — the same
  // rule the Checks tab lives by, because "did it pass" is drawn on the
  // exact red/green axis 8% of men cannot resolve.
  await expect(page.getByRole("button", { name: /lint/ })).toBeVisible();
  const failing = page.getByRole("button", { name: /build \(linux\)/ });
  await expect(failing).toContainText("Failing");

  // The failed job opened by itself. This is the whole reason somebody
  // followed a red check here, and a twelve-leg matrix that opened on a
  // green leg would make them go hunting.
  await expect(failing).toHaveAttribute("aria-current", "true");
  await expect(logPane(page)).toContainText("error[E0308]");
  // …and the job's own error, which is a different fact from its log.
  await expect(page.getByText("exit status 101")).toBeVisible();

  // Choosing the other job reads the other log.
  await page.getByRole("button", { name: /lint/ }).click();
  await expect(logPane(page)).toContainText("▶ Lint");
  await expect(logPane(page)).not.toContainText("E0308");
});

test("a composed run names its changeset and leads to it", async ({ page }) => {
  // Seen in the end-to-end pass: a composed run's page read "changeset
  // on main · 4fc919c · the change it belongs to" — the kind of run,
  // one member's commit, one member's change — and nowhere the set the
  // run was a verdict on. The server had been sending `changeset` and
  // `composition` on every run all along; the page did not read them.
  await runPage(page, {
    run: run({
      event: "changeset",
      change_key: "I01a069d84466",
      changeset: { key: "widget-2" },
      composition: "12e30c5de802f00d12e30c5de802f00d",
    }),
  });
  await page.goto("/acme/widget/checks/runs/wr1");
  const header = page.locator("h1").locator("..");
  await expect(header).toContainText("changeset widget-2 on main");
  const set = page.getByRole("link", {
    name: "the changeset it was composed for",
  });
  await expect(set).toHaveAttribute("href", "/dashboard/changesets/widget-2");
  // Twelve characters, as the changeset page spells the same hash.
  await expect(header).toContainText("composition 12e30c5de802");
  await expect(header).not.toContainText("12e30c5de802f");
  // The member's own change is still one click away.
  await expect(
    page.getByRole("link", { name: "the change it belongs to" }),
  ).toHaveAttribute("href", "/acme/widget/changes/I01a069d84466");
});

test("a running job's log comes from the stream alone, so no line is shown twice", async ({
  page,
}) => {
  // The fixture that makes this a real assertion: `…/log` already has
  // the first chunk in it, because that is what a page load in the
  // middle of a build actually finds. A client that fetched the log and
  // then appended the replaying stream would render "▶ Checkout" twice
  // — and against an empty starting log it would render it once and
  // look perfect.
  //
  // The feed deliberately never says `done`, so the job stays live for
  // the whole test and the count below cannot be rescued by a later
  // re-read. The settling half is the next test.
  const running = run({
    state: "running",
    completed_at: null,
    jobs: [
      job({
        id: "j1",
        key: "build",
        state: "running",
        completed_at: null,
        log_chunks: 1,
      }),
    ],
  });

  const mock = await runPage(page, {
    run: () => running,
    log: { j1: "▶ Checkout\n" },
    stream: {
      j1:
        frame("chunk", { text: "▶ Checkout\n" }) +
        frame("chunk", { text: "▶ Build\n" }),
    },
    streamDelay: 600,
  });
  await page.goto("/acme/widget/checks/runs/wr1");

  // Before the feed answers: the run says Running and the log frame is
  // already on screen with nothing in it. A running job that showed a
  // spinner until its first flush looks stuck, and a flush can be a
  // minute away.
  await expect(page.locator("h1").locator("..")).toContainText("Running");
  await expect(page.getByText("No output yet.")).toBeVisible();

  // Both chunks arrive, in order.
  await expect(logPane(page)).toContainText("▶ Build");
  const text = (await logPane(page).textContent()) ?? "";
  expect(text.indexOf("▶ Checkout")).toBeLessThan(text.indexOf("▶ Build"));
  // The assertion this test exists for.
  expect(occurrences(text, "▶ Checkout")).toBe(1);
  expect(occurrences(text, "▶ Build")).toBe(1);

  // One source, not two: while the job is live the plain log route is
  // never asked at all.
  expect(mock.logRequests).toEqual([
    "/v1/orgs/acme/repos/widget/workflow-jobs/j1/log/stream",
  ]);
});

test("a job that settles under the page is re-read from the authoritative log", async ({
  page,
}) => {
  // `done` is not merely the end of the feed. The runner uploads one
  // whole log object when a job ends and that object wins: a chunk it
  // failed to POST twice is dropped rather than retried, so the chunk
  // sequence can have holes the final file does not. A page that kept
  // showing what it had streamed would keep showing the version with
  // the hole in it — here, a build log missing its last line.
  const running = run({
    state: "running",
    completed_at: null,
    jobs: [
      job({
        id: "j1",
        key: "build",
        state: "running",
        completed_at: null,
        log_chunks: 1,
      }),
    ],
  });
  const settled = run({ jobs: [job({ id: "j1", key: "build" })] });

  const mock = await runPage(page, {
    // Running until the feed says otherwise; the page re-reads the run
    // the moment `done` arrives rather than waiting out its poll.
    run: (n) => (n === 0 ? running : settled),
    log: { j1: "▶ Checkout\n▶ Build\nall tests passed\n" },
    stream: {
      j1:
        frame("chunk", { text: "▶ Checkout\n" }) +
        frame("done", { state: "passed" }),
    },
    streamDelay: 400,
  });
  await page.goto("/acme/widget/checks/runs/wr1");

  // The line the stream never carried, which is the proof the final
  // object was read rather than the chunks kept.
  await expect(logPane(page)).toContainText("all tests passed");
  await expect(page.locator("h1").locator("..")).toContainText("Passing");
  const text = (await logPane(page).textContent()) ?? "";
  expect(occurrences(text, "▶ Checkout")).toBe(1);
  // The stream first, the plain log only once the job had settled.
  expect(mock.logRequests[0]).toMatch(/\/log\/stream$/);
  expect(mock.logRequests.at(-1)).toMatch(/\/log$/);
});

test("a job nothing has picked up says so, rather than showing an empty log", async ({
  page,
}) => {
  await runPage(page, {
    run: run({
      state: "running",
      completed_at: null,
      jobs: [
        job({
          id: "j1",
          key: "build",
          state: "queued",
          started_at: null,
          completed_at: null,
          log_chunks: 0,
        }),
      ],
    }),
    // The feed's one announcement, and then nothing: this is what
    // waiting for a runner actually looks like on the wire.
    stream: { j1: frame("queued", {}) },
  });
  await page.goto("/acme/widget/checks/runs/wr1");

  // An empty log frame and a queued job are indistinguishable to a
  // reader, and the difference is the whole question they have: is this
  // broken, or has it not started?
  await expect(page.getByText(/Waiting for a runner/)).toBeVisible();
  await expect(page.getByRole("button", { name: /build/ })).toContainText(
    "Queued",
  );
});

test("a refused workflow says why, verbatim, and does not pretend to have jobs", async ({
  page,
}) => {
  // The case the page was built for. A file we would not run produces a
  // run with no jobs, so every log surface is vacuous — and before this
  // page existed the reason lived on a run record nothing rendered, so
  // the check row said "failed" and nothing else.
  const reason =
    ".weft/ci.yml: line 7: job `test` depends on `build`, which depends on `test`";
  await runPage(page, {
    run: run({ state: "failed", error: reason, jobs: [] }),
  });
  await page.goto("/acme/widget/checks/runs/wr1");

  // Verbatim and unabridged: the line number and the cycle are the
  // whole value of the message, and a paraphrase turns something
  // actionable into "something went wrong".
  await expect(page.getByText(reason)).toBeVisible();
  await expect(page.getByText("This run never started a job")).toBeVisible();
  // No log frame at all, rather than an empty one implying output that
  // never existed.
  await expect(logPane(page)).toHaveCount(0);
});

test("a writer can stop a running run, in two deliberate steps", async ({
  page,
}) => {
  const running = run({
    state: "running",
    completed_at: null,
    jobs: [
      job({ id: "j1", key: "build", state: "running", completed_at: null }),
    ],
  });
  const mock = await runPage(page, {
    run: () => running,
    write: true,
    stream: { j1: "" },
  });
  await page.goto("/acme/widget/checks/runs/wr1");

  const cancel = page.getByRole("button", { name: "Cancel run" });
  await expect(cancel).toBeVisible();
  await cancel.click();

  // Nothing has been sent yet. The first press arms; the second acts.
  // No `window.confirm` anywhere near it: a browser dialog cannot be
  // styled, cannot be driven by the walkthrough, and blocks the tab.
  expect(mock.cancels).toBe(0);
  await expect(page.getByText("Stop this run?")).toBeVisible();

  // The confirming button says the whole thing. Beside "Keep it
  // running", a second button reading "Cancel" would mean the opposite
  // of the first one.
  await page.getByRole("button", { name: "Stop this run" }).click();
  await expect(page.locator("h1").locator("..")).toContainText("Cancelled");
  expect(mock.cancels).toBe(1);
  // The run's own reason names who asked, which is what the field is
  // for on a cancellation.
  await expect(page.getByText("cancelled by Ada Owner")).toBeVisible();
  // And the control is gone, because there is nothing left to stop.
  await expect(page.getByRole("button", { name: "Cancel run" })).toHaveCount(0);
});

test("backing out of the confirmation sends nothing", async ({ page }) => {
  const mock = await runPage(page, {
    run: run({
      state: "running",
      completed_at: null,
      jobs: [
        job({ id: "j1", key: "build", state: "running", completed_at: null }),
      ],
    }),
    write: true,
    stream: { j1: "" },
  });
  await page.goto("/acme/widget/checks/runs/wr1");

  await page.getByRole("button", { name: "Cancel run" }).click();
  await page.getByRole("button", { name: "Keep it running" }).click();
  await expect(page.getByRole("button", { name: "Cancel run" })).toBeVisible();
  expect(mock.cancels).toBe(0);
});

test("a viewer without write access is offered no Cancel at all", async ({
  page,
}) => {
  await runPage(page, {
    run: run({
      state: "running",
      completed_at: null,
      jobs: [
        job({ id: "j1", key: "build", state: "running", completed_at: null }),
      ],
    }),
    write: false,
    stream: { j1: "" },
  });
  await page.goto("/acme/widget/checks/runs/wr1");

  // The page itself renders, because reading a run needs only read
  // access. Counted rather than awaited-absent: this is an absence on a
  // settled page, and a control that appears and is then refused is
  // worse than one that never appeared.
  await expect(page.getByRole("heading", { name: "CI" })).toBeVisible();
  expect(await page.getByRole("button", { name: "Cancel run" }).count()).toBe(
    0,
  );
});

test("a run that settled under the page reports the refusal instead of swallowing it", async ({
  page,
}) => {
  // `409` is the server saying it had already stopped, and it means the
  // page is out of date — a fact the reader needs, and the reason the
  // route answers with a status rather than a silent no-op.
  await runPage(page, {
    run: (n) =>
      n === 0
        ? run({
            state: "running",
            completed_at: null,
            jobs: [
              job({
                id: "j1",
                key: "build",
                state: "running",
                completed_at: null,
              }),
            ],
          })
        : run(),
    write: true,
    stream: { j1: "" },
    cancelStatus: 409,
    cancelError: "this run is already passed",
  });
  await page.goto("/acme/widget/checks/runs/wr1");

  await page.getByRole("button", { name: "Cancel run" }).click();
  await page.getByRole("button", { name: "Stop this run" }).click();
  // The server's own sentence — "already passed" is a different fact
  // from "you may not", and only the server knows which.
  await expect(page.getByText("this run is already passed")).toBeVisible();
  // …and the page re-reads itself, so what is on screen is true again.
  await expect(page.locator("h1").locator("..")).toContainText("Passing");
});

test("a run in another repository is not an existence oracle", async ({
  page,
}) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 200, json: ME }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: { ...widget } }),
  );
  await page.route(/\/workflow-runs\/[^/]+$/, (r) =>
    r.fulfill({ status: 404, json: { error: "no such run" } }),
  );
  await page.goto("/acme/widget/checks/runs/somebody-elses");

  // The same sentence a genuinely missing run gets. A page that
  // distinguished "not yours" from "not here" would answer the question
  // the server's masking exists to refuse.
  await expect(page.getByText(/couldn't find that run/i)).toBeVisible();
});

test("a check row pointing at one of our own runs navigates in-app", async ({
  page,
}) => {
  // Every `detail_url` used to be somebody else's site, so both places
  // that render one opened a new tab with `rel="nofollow ugc noopener
  // noreferrer"`. That is right for a URL a third party posted to the
  // intake and wrong for the one the server writes for a workflow run: it
  // points at this very SPA, and a full page load costs a reader their
  // place to reach a page one client-side navigation away.
  // The origin `playwright.config.ts` serves the production build on.
  // Read from the config rather than from `page.url()`, which is
  // `about:blank` before the first `goto` — and an origin of the string
  // "null" makes the assertion below pass or fail for a reason that has
  // nothing to do with the code.
  const detail = `${PREVIEW_ORIGIN}/acme/widget/checks/runs/wr1`;

  await runPage(page, { run: run(), log: { j1: "▶ Build\n" } });
  await page.route(/\/checks\/runs(\?|$)/, (r) =>
    r.fulfill({
      status: 200,
      json: {
        runs: [
          {
            id: "c1",
            repo_id: "01aaa",
            commit_sha: "abc1234def5678901234567890abcdef12345678",
            ref_name: "main",
            provider: "weft",
            external_id: "wr1",
            name: "CI",
            run_number: 1,
            event: "push",
            state: "passing",
            detail_url: detail,
            actor: "ada",
            started_at: NOW - 300_000,
            completed_at: NOW - 240_000,
            created_at: NOW - 360_000,
            updated_at: NOW - 240_000,
          },
          {
            id: "c2",
            repo_id: "01aaa",
            commit_sha: "abc1234def5678901234567890abcdef12345678",
            ref_name: "main",
            provider: "github",
            external_id: "gh-1",
            name: "upstream",
            run_number: 2,
            event: "push",
            state: "passing",
            detail_url: "https://ci.example.test/runs/42",
            actor: "ada",
            started_at: NOW - 300_000,
            completed_at: NOW - 240_000,
            created_at: NOW - 720_000,
            updated_at: NOW - 240_000,
          },
          {
            id: "c3",
            repo_id: "01aaa",
            commit_sha: "abc1234def5678901234567890abcdef12345678",
            ref_name: "main",
            // A reporter cannot spell itself "weft" — `checks_intake`
            // writes this constant — but it *can* name our own host in
            // the `detail_url` it posts. Origin alone would read this
            // row as first-party and hand it an in-app navigation.
            provider: "intake",
            external_id: "ext-9",
            name: "borrowed",
            run_number: 3,
            event: "push",
            state: "passing",
            detail_url: `${PREVIEW_ORIGIN}/acme/widget/checks/runs/wr1`,
            actor: "ada",
            started_at: NOW - 300_000,
            completed_at: NOW - 240_000,
            created_at: NOW - 700_000,
            updated_at: NOW - 240_000,
          },
        ],
        workflows: ["CI", "upstream", "borrowed"],
        next_before: null,
      },
    }),
  );
  await page.route(/\/ci\/poll(\?|$)/, (r) =>
    r.fulfill({
      status: 200,
      json: {
        provider: "github",
        connected: true,
        polled: true,
        denied: false,
        error: null,
        high_water: NOW - 60_000,
        resuming_from: null,
        retry_in_ms: null,
      },
    }),
  );
  await page.goto("/acme/widget/checks");

  // Somebody else's row keeps the hardened outbound treatment it has
  // always had.
  const outbound = page.getByRole("link", { name: "upstream" });
  await expect(outbound).toHaveAttribute(
    "href",
    "https://ci.example.test/runs/42",
  );
  const rel = (await outbound.getAttribute("rel")) ?? "";
  expect(rel).toContain("noopener");
  expect(rel).toContain("ugc");

  // Ours is a path, not an absolute URL, and carries no `rel` — there is
  // no third party on the other end of it.
  const ours = page.getByRole("link", { name: "CI" }).first();
  await expect(ours).toHaveAttribute("href", "/acme/widget/checks/runs/wr1");
  expect(await ours.getAttribute("rel")).toBeNull();
  expect(await ours.getAttribute("target")).toBeNull();

  // A row a third party posted stays outbound even when it names our
  // own host: the link is decided by the provider *and* the origin, and
  // "the remote party chooses whether this navigates inside the app" is
  // not a rule worth having.
  const borrowed = page.getByRole("link", { name: "borrowed" });
  await expect(borrowed).toHaveAttribute(
    "href",
    `${PREVIEW_ORIGIN}/acme/widget/checks/runs/wr1`,
  );
  expect((await borrowed.getAttribute("rel")) ?? "").toContain("ugc");

  // A marker on the document that a full page load would destroy. This
  // is the assertion: "the URL changed and the run page rendered" is
  // equally true of a reload, and a reload is precisely the defect.
  await page.evaluate(() => {
    (window as unknown as { __same: boolean }).__same = true;
  });
  await ours.click();
  await expect(page.getByRole("heading", { name: "CI" })).toBeVisible();
  await expect(logPane(page)).toContainText("▶ Build");
  expect(await page.evaluate(() => (window as never)["__same"])).toBe(true);
  expect(new URL(page.url()).pathname).toBe("/acme/widget/checks/runs/wr1");
});

test("a job says which machine ran it, and what it asked for", async ({
  page,
}) => {
  // The question an operator has about a job that behaved oddly is
  // always "which box", and before this the run page could not answer
  // it at all: every job looked like every other job, and the only way
  // to find the machine was to correlate timestamps in Settings.
  //
  // The labels are the other half of the same question. Routing is
  // `job.labels ⊆ runner.labels`, and the server's refusal quotes the
  // job's labels in file order — so the page shows them in file order
  // too, or the two lists cannot be compared by eye.
  await runPage(page, {
    run: run({
      jobs: [
        job({
          id: "j1",
          key: "build",
          labels: ["self-hosted", "gpu", "cuda"],
          runner: { id: "r1", name: "gpu-box" },
        }),
      ],
    }),
    log: { j1: "\u25b6 Build\n" },
  });
  await page.goto("/acme/widget/checks/runs/wr1");

  await expect(page.getByRole("button", { name: /build/ })).toContainText(
    "ran on gpu-box",
  );
  // In file order, which is the order the refusal sentence quotes.
  const chips = page.locator("span", {
    hasText: /^(self-hosted|gpu|cuda)$/,
  });
  expect(await chips.allTextContents()).toEqual(["self-hosted", "gpu", "cuda"]);
});

test("a job no runner has taken says what it asked for, and names no machine", async ({
  page,
}) => {
  // The labels are the question an operator asks of a job that sits
  // waiting — which of them does no machine offer — so they are shown
  // before anything has taken it. There is no runner to name yet, and
  // the page must not invent one.
  await runPage(page, {
    run: run({
      jobs: [
        job({
          id: "j1",
          key: "build",
          labels: ["self-hosted", "gpu"],
          runner: null,
        }),
      ],
    }),
    log: { j1: "\u25b6 Build\n" },
  });
  await page.goto("/acme/widget/checks/runs/wr1");

  await expect(logPane(page)).toContainText("\u25b6 Build");
  await expect(page.getByText(/ran on/)).toHaveCount(0);
  const chips = page.locator("span", { hasText: /^(self-hosted|gpu)$/ });
  expect(await chips.allTextContents()).toEqual(["self-hosted", "gpu"]);
});

test("a job whose server sends no labels or runner renders unchanged", async ({
  page,
}) => {
  // `labels` and `runner` are optional on the type because a fixture or
  // an older server does not send them, and a job that rendered "ran on
  // undefined" would be worse than one that said nothing.
  await runPage(page, {
    run: run({ jobs: [job({ id: "j1", key: "build" })] }),
    log: { j1: "\u25b6 Build\n" },
  });
  await page.goto("/acme/widget/checks/runs/wr1");

  await expect(logPane(page)).toContainText("\u25b6 Build");
  await expect(page.getByText(/ran on/)).toHaveCount(0);
  await expect(page.getByText(/undefined/)).toHaveCount(0);
});
