// Settings → Runners → GitHub Actions on Weft runners: what the
// installation may do, the `runs-on:` line, and what became of the jobs
// that used it.
//
// `src/lib/github-runners.test.ts` covers the sentences — which
// permissions are missing, what each state's row says, the snippet —
// and none of that is repeated here. What is here is the half a unit
// test cannot see:
//
// - **the approve link goes to GitHub's page, and names both
//   permissions.** A card that said "needs approval" with nowhere to
//   click would send somebody to GitHub's settings to guess which of
//   thirty permissions we meant.
// - **a refused job is legible from the row.** On GitHub it looks like
//   a job nobody picked up; the reason exists only here, and the row
//   also has to say that the job is *still sitting there* when we could
//   not cancel it.
// - **a permission refusal offers the fix, on the row.** The reader
//   arriving from an hour-old queued job wants the link, not the
//   diagnosis.
// - **nothing widens the layout.** Refusals are long sentences and a
//   repository name is whatever GitHub allows.
//
// Every name is matched by role and exact text: `getByText` matches a
// substring, and a lax match here would pass a row that said "not
// refused".

import { expect, test, type Page } from "@playwright/test";
import { BILLING, signIn, signInAsPerson } from "./fixtures";

const APPROVE_URL =
  "https://github.example/organizations/acme-inc/settings/installations/4001";

interface Detail {
  account: string;
  target_type: "Organization" | "User";
  administration_write: boolean;
  actions_write: boolean;
  runners_ready: boolean;
  approve_url: string;
  suspended: boolean;
}

function detail(over: Partial<Detail> = {}): Detail {
  return {
    account: "acme-inc",
    target_type: "Organization",
    administration_write: true,
    actions_write: true,
    runners_ready: true,
    approve_url: APPROVE_URL,
    suspended: false,
    ...over,
  };
}

function installation(d: Detail | { gone: true } | null) {
  return {
    installation_id: "4001",
    provider: "github",
    account: "acme-inc",
    created_at: Date.now() - 86_400_000,
    detail: d,
  };
}

const SIZES = [
  { label: "weft", multiplier: 1, cpu: 1024, memory_mib: 2048 },
  { label: "weft-2x", multiplier: 2, cpu: 2048, memory_mib: 4096 },
  { label: "weft-4x", multiplier: 4, cpu: 4096, memory_mib: 8192 },
];

interface Job {
  id: string;
  repo: string;
  private: boolean;
  github_job_id: number;
  github_run_id: number;
  run_attempt: number;
  name: string;
  html_url: string;
  labels: string[];
  size: string;
  multiplier: number;
  state: string;
  refusal: string | null;
  cancelled_on_github: boolean;
  needs_permission: boolean;
  error: string | null;
  conclusion: string | null;
  runner_name: string | null;
  minutes: number;
  queued_at: number | null;
  launched_at: number | null;
  started_at: number | null;
  completed_at: number | null;
}

function job(over: Partial<Job> = {}): Job {
  const now = Date.now();
  return {
    id: "gj-1",
    repo: "acme/pipeline",
    private: true,
    github_job_id: 77,
    github_run_id: 900,
    run_attempt: 1,
    name: "test",
    html_url: "https://github.example/acme/pipeline/actions/runs/900/job/77",
    labels: ["self-hosted", "weft"],
    size: "weft",
    multiplier: 1,
    state: "queued",
    refusal: null,
    cancelled_on_github: false,
    needs_permission: false,
    error: null,
    conclusion: null,
    runner_name: null,
    minutes: 0,
    queued_at: now - 60_000,
    launched_at: null,
    started_at: null,
    completed_at: null,
    ...over,
  };
}

const HOSTED_OFF =
  "hosted runners are switched off for this organisation — Settings → Runners";
const NEEDS_ADMIN =
  "this GitHub App installation cannot register a runner on this repository — it needs the Administration: write permission, which an owner of acme-inc approves on GitHub";

interface Options {
  installations?: ReturnType<typeof installation>[];
  jobs?: Job[];
}

/// The runners page, with nothing reachable but what this file mocked.
/// `signIn` registers the catch-all `**/v1/**` refusal first, so the
/// routes below win and anything else 404s here rather than reaching
/// vite's proxy.
async function runnersPage(page: Page, opts: Options = {}) {
  await signIn(page);
  await page.route("**/v1/orgs/acme/runner-policy", (r) =>
    r.fulfill({
      json: { hosted: "allowed", self_hosted: "all", self_hosted_repos: [] },
    }),
  );
  await page.route("**/v1/orgs/acme/runner-groups", (r) =>
    r.fulfill({ json: { groups: [] } }),
  );
  await page.route("**/v1/orgs/acme/runners", (r) =>
    r.fulfill({ json: { runners: [] } }),
  );
  // The runners page asks for the Runners App's installations
  // (`?app=runners`); a glob without the query would not match it.
  await page.route(/\/v1\/orgs\/acme\/github\/installations(\?.*)?$/, (r) =>
    r.fulfill({
      json: { installations: opts.installations ?? [installation(detail())] },
    }),
  );
  await page.route("**/v1/orgs/acme/github-jobs", (r) =>
    r.fulfill({ json: { jobs: opts.jobs ?? [], sizes: SIZES } }),
  );
  await page.goto("/dashboard/settings/runners");
  await expect(
    page.getByText("GitHub Actions on Weft runners", { exact: true }),
  ).toBeVisible();
}

test("an installation missing both permissions is told what to approve, with the link", async ({
  page,
}) => {
  await runnersPage(page, {
    installations: [
      installation(
        detail({
          administration_write: false,
          actions_write: false,
          runners_ready: false,
        }),
      ),
    ],
  });
  const link = page.getByRole("link", {
    name: "Approve Administration: write and Actions: write on GitHub",
    exact: true,
  });
  await expect(link).toBeVisible();
  // GitHub's own page for the installation, in a new tab: the person is
  // coming back here to see the card turn green.
  await expect(link).toHaveAttribute("href", APPROVE_URL);
  await expect(link).toHaveAttribute("target", "_blank");
  await expect(page.getByText("Needs approval", { exact: true })).toBeVisible();
  // And not the ready sentence, which would contradict the link.
  await expect(
    page.getByText("GitHub Actions on acme-inc can use Weft runners", {
      exact: true,
    }),
  ).toHaveCount(0);
});

test("one missing permission names only that one", async ({ page }) => {
  await runnersPage(page, {
    installations: [
      installation(detail({ actions_write: false, runners_ready: false })),
    ],
  });
  await expect(
    page.getByRole("link", {
      name: "Approve Actions: write on GitHub",
      exact: true,
    }),
  ).toHaveAttribute("href", APPROVE_URL);
});

test("a ready installation says so and shows the three runs-on lines", async ({
  page,
}) => {
  await runnersPage(page);
  await expect(
    page.getByText("GitHub Actions on acme-inc can use Weft runners", {
      exact: true,
    }),
  ).toBeVisible();
  await expect(page.getByText("Ready", { exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: /^Approve / })).toHaveCount(0);

  // The snippet: the server's labels, each with what it buys, and the
  // multiplier a person is agreeing to.
  const sizes = page.getByRole("list", { name: "Runner sizes", exact: true });
  await expect(sizes.getByRole("listitem")).toHaveText([
    /^runs-on: weftweft — 1 vCPU, 2 GB, 1× minutes$/,
    /^runs-on: weft-2xweft-2x — 2 vCPU, 4 GB, 2× minutes$/,
    /^runs-on: weft-4xweft-4x — 4 vCPU, 8 GB, 4× minutes$/,
  ]);
  // What docker does and does not do here is said in full, above the
  // jobs, because the first job most people try has a services: block.
  await expect(
    page.getByText(
      "docker build and docker run work here without a daemon; jobs with container:, services: or a Docker-based action will fail.",
      { exact: true },
    ),
  ).toBeVisible();
  // Nothing has asked yet.
  await expect(
    page.getByText("No GitHub Actions job has asked for a Weft runner yet.", {
      exact: true,
    }),
  ).toBeVisible();
});

test("an uninstalled App and an unanswered GitHub are two different sentences, neither of them ready", async ({
  page,
}) => {
  await runnersPage(page, {
    installations: [
      { ...installation({ gone: true }), installation_id: "4001" },
      { ...installation(null), installation_id: "4003", account: "ada" },
    ],
  });
  await expect(page.getByText("Uninstalled", { exact: true })).toBeVisible();
  await expect(page.getByText("Not checked", { exact: true })).toBeVisible();
  await expect(page.getByText("Ready", { exact: true })).toHaveCount(0);
});

test("nothing connected offers the install, and the button leaves for GitHub", async ({
  page,
}) => {
  await runnersPage(page, { installations: [] });
  let started = 0;
  // The runners page starts the install for the Runners App
  // (`?app=runners`), which is the whole reason the URL differs from
  // the mirror picker's; the mock has to admit the query to see it.
  await page.route(/\/v1\/orgs\/acme\/github\/install(\?.*)?$/, (r) => {
    started += 1;
    return r.fulfill({
      json: { url: "/dashboard/?connect=ok", state: "s", expires_in: 600 },
    });
  });
  await expect(
    page.getByText(
      "No GitHub installation is connected to this organisation.",
      {
        exact: true,
      },
    ),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "Connect GitHub", exact: true })
    .click();
  await page.waitForURL(/connect=ok/);
  expect(started).toBe(1);
});

test("a refused job shows its reason, and that GitHub is still holding it", async ({
  page,
}) => {
  await runnersPage(page, {
    jobs: [
      job({
        id: "gj-refused",
        state: "refused",
        refusal: HOSTED_OFF,
        cancelled_on_github: false,
      }),
      job({
        id: "gj-refused-cancelled",
        name: "lint",
        github_job_id: 78,
        state: "refused",
        refusal: HOSTED_OFF,
        cancelled_on_github: true,
      }),
    ],
  });
  const table = page.getByRole("table").first();
  const rows = table.getByRole("row").filter({ hasText: "acme/pipeline" });
  await expect(rows).toHaveCount(2);

  const held = rows.filter({
    has: page.getByRole("link", { name: "test", exact: true }),
  });
  await expect(
    held.getByText(`refused — ${HOSTED_OFF}`, { exact: true }),
  ).toBeVisible();
  await expect(
    held.getByText("still queued on GitHub — cancel it there", { exact: true }),
  ).toBeVisible();
  // The job's own page on GitHub, in a new tab.
  await expect(
    held.getByRole("link", { name: "test", exact: true }),
  ).toHaveAttribute(
    "href",
    "https://github.example/acme/pipeline/actions/runs/900/job/77",
  );

  // The one we did cancel does not tell anybody to go and cancel it.
  const cancelled = rows.filter({
    has: page.getByRole("link", { name: "lint", exact: true }),
  });
  await expect(
    cancelled.getByText(`refused — ${HOSTED_OFF}`, { exact: true }),
  ).toBeVisible();
  await expect(
    cancelled.getByText("still queued on GitHub — cancel it there", {
      exact: true,
    }),
  ).toHaveCount(0);
  // No permission was at issue, so no approve link on either row.
  await expect(
    table.getByRole("link", {
      name: "Approve the permission on GitHub",
      exact: true,
    }),
  ).toHaveCount(0);
});

test("a refusal for want of a permission offers the approve link on the row", async ({
  page,
}) => {
  await runnersPage(page, {
    installations: [
      installation(
        detail({ administration_write: false, runners_ready: false }),
      ),
    ],
    jobs: [
      job({
        state: "refused",
        refusal: NEEDS_ADMIN,
        needs_permission: true,
        cancelled_on_github: true,
      }),
    ],
  });
  const row = page
    .getByRole("table")
    .first()
    .getByRole("row")
    .filter({ hasText: "acme/pipeline" });
  await expect(
    row.getByText(`refused — ${NEEDS_ADMIN}`, { exact: true }),
  ).toBeVisible();
  await expect(
    row.getByRole("link", {
      name: "Approve the permission on GitHub",
      exact: true,
    }),
  ).toHaveAttribute("href", APPROVE_URL);
});

test("a completed job shows its conclusion and minutes; a running one its minutes so far", async ({
  page,
}) => {
  const now = Date.now();
  await runnersPage(page, {
    jobs: [
      job({
        id: "gj-running",
        name: "build",
        github_job_id: 79,
        state: "running",
        size: "weft-2x",
        multiplier: 2,
        runner_name: "weft-abc",
        minutes: 6,
        launched_at: now - 200_000,
        started_at: now - 180_000,
      }),
      job({
        id: "gj-done",
        state: "completed",
        conclusion: "success",
        runner_name: "weft-def",
        minutes: 4,
        launched_at: now - 900_000,
        started_at: now - 880_000,
        completed_at: now - 640_000,
      }),
    ],
  });
  const table = page.getByRole("table").first();
  const done = table.getByRole("row").filter({
    has: page.getByRole("link", { name: "test", exact: true }),
  });
  await expect(
    done.getByText("completed · success · 4 min", { exact: true }),
  ).toBeVisible();
  await expect(
    done.getByRole("cell", { name: "4", exact: true }),
  ).toBeVisible();
  await expect(
    done.getByRole("cell", { name: "weft", exact: true }),
  ).toBeVisible();

  const running = table.getByRole("row").filter({
    has: page.getByRole("link", { name: "build", exact: true }),
  });
  await expect(
    running.getByText("running · 6 min", { exact: true }),
  ).toBeVisible();
  await expect(
    running.getByRole("cell", { name: "weft-2x", exact: true }),
  ).toBeVisible();
});

test("a long refusal and a long repository name ellipsise rather than widen the page", async ({
  page,
}) => {
  await runnersPage(page, {
    jobs: [
      job({
        repo: `acme/${"a-very-long-repository-name-".repeat(6)}end`,
        state: "refused",
        refusal: `${NEEDS_ADMIN} ${"and some more words about it ".repeat(8)}`,
        needs_permission: true,
        cancelled_on_github: true,
      }),
    ],
  });
  await expect(page.getByRole("table").first()).toBeVisible();
  const overflow = await page.evaluate(() => {
    const de = document.documentElement;
    return de.scrollWidth - de.clientWidth;
  });
  expect(overflow).toBeLessThanOrEqual(1);
});

test("the billing page says how many minutes went through GitHub Actions, and only when some did", async ({
  page,
}) => {
  await signInAsPerson(page);
  await page.route("**/v1/orgs/acme/billing", (r) =>
    r.fulfill({
      json: {
        ...BILLING,
        ci_minutes_limit: 2000,
        ci_minutes_used: 1240,
        ci_minutes_remaining: 760,
        ci_minutes_github_used: 300,
      },
    }),
  );
  // Reached the way a person reaches it: a password session is a cookie
  // the mocks do not hold, so a `goto` would boot signed out.
  await page.getByRole("link", { name: "Billing" }).click();
  await expect(
    page.getByText("1,240 of 2,000 minutes used in the last 30 days", {
      exact: true,
    }),
  ).toBeVisible();
  await expect(
    page.getByText("of which 300 on GitHub Actions", { exact: true }),
  ).toBeVisible();

  // The same organisation with nothing through GitHub reads exactly as
  // it always did.
  await page.route("**/v1/orgs/acme/billing", (r) =>
    r.fulfill({
      json: {
        ...BILLING,
        ci_minutes_limit: 2000,
        ci_minutes_used: 1240,
        ci_minutes_remaining: 760,
        ci_minutes_github_used: 0,
      },
    }),
  );
  await page.getByRole("link", { name: "Runners" }).click();
  await page.getByRole("link", { name: "Billing" }).click();
  await expect(
    page.getByText("1,240 of 2,000 minutes used in the last 30 days", {
      exact: true,
    }),
  ).toBeVisible();
  await expect(page.getByText(/on GitHub Actions/)).toHaveCount(0);
});
