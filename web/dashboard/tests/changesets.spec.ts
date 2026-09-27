// Changesets in the dashboard: building one from the org's open changes,
// reading the composed verdict, landing it and watching the landing,
// then making the reverting changeset.
//
// The mock is stateful where the product is: a landing that turns
// `landed` on the second poll, a membership the picker must respect, a
// revert the server refuses with a list of conflicts. Every request body
// the page sends is recorded and asserted, because a page that renders
// the right thing while asking for the wrong one is exactly what a
// rendering-only test cannot see. The server's side of each contract is
// pinned in `crates/stratum-server/tests/changesets_e2e.rs` and
// `org_changes_e2e.rs`; this file holds that the page sends what those
// routes accept and shows what they answer.

import { expect, test, type Locator, type Page } from "@playwright/test";
import { ME, signIn } from "./fixtures";
import { PREVIEW_ORIGIN } from "./preview";

const SHA = (n: number) => String(n).repeat(40);

function change(
  repo: string,
  key: string,
  title: string,
  over: Record<string, unknown> = {},
) {
  return {
    key,
    title,
    target_branch: "main",
    source: null,
    state: "open",
    land_verdict: null,
    landed_commit: null,
    created_at: Date.now() - 3_600_000,
    updated_at: Date.now() - 60_000,
    patchset: {
      number: 2,
      commit: SHA(2),
      parent: SHA(1),
      message: title,
      created_at: Date.now() - 60_000,
    },
    // The row's repository is writable unless a test says otherwise:
    // the picker greys out a change the caller could not compose.
    viewer_write: true,
    ...over,
  };
}

/// Two members, listed out of landing order on purpose: `web` was added
/// first but depends on `api`, so `order` puts `api` first and the page
/// must follow `order`, not `members`.
function changeset(over: Record<string, unknown> = {}) {
  const api = { repo: "api", change: "c-api" };
  const web = { repo: "web", change: "c-web" };
  return {
    key: "rename-payments",
    title: "Rename the payments service",
    body: "Both halves at once or neither.",
    state: "open",
    created_at: Date.now() - 3_600_000,
    updated_at: Date.now() - 60_000,
    reverts: null,
    reverted_by: [],
    members: [
      { repo: "web", change: change("web", "c-web", "point web at billing") },
      { repo: "api", change: change("api", "c-api", "rename the service") },
    ],
    edges: [{ from: api, to: web }],
    order: [api, web],
    composition: "abcdef0123456789abcdef",
    checks: [
      {
        repo: "web",
        name: "ci / build",
        state: "passing",
        detail_url: null,
        run: "wr1",
      },
    ],
    landing: null,
    // Writable unless a test says otherwise: the actions are drawn only
    // for somebody the server says may take them.
    viewer_write: true,
    ...over,
  };
}

function verdict(over: Record<string, unknown> = {}) {
  const member = (repo: string, change: string, gate: string, why: string) => ({
    repo,
    change,
    state: "open",
    patchset: 2,
    commit: SHA(2),
    verdict: {
      landable: true,
      explanation: "all paths satisfied",
      per_path: [],
    },
    approvals: [
      { email: "olive@acme.test", name: "Olive Owner", created_at: 0 },
    ],
    landable: true,
    explanation: why,
    gate,
    waiting_on: [],
    reason: null,
  });
  return {
    changeset: "rename-payments",
    state: "open",
    landable: true,
    gate: "ready",
    explanation: "every member is ready to land",
    waiting_on: [],
    members: [
      member("api", "c-api", "ready", "approved by an owner of every path"),
      member("web", "c-web", "ready", "approved by an owner of every path"),
    ],
    ...over,
  };
}

interface State {
  changesets: Record<string, unknown>[];
  verdict: Record<string, unknown>;
  openChanges: Record<string, unknown>[];
  posts: { path: string; body: unknown }[];
  /// How many times the changeset has been read since landing began.
  reads: number;
  /// What `POST …/land` answers.
  land: { status: number; body: unknown } | "accept";
  /// What `POST …/revert` answers.
  revert: { status: number; body: unknown } | "accept";
  /// What `PUT …/edges` answers.
  edges: { status: number; body: unknown } | "accept";
  /// Per-member file lists, keyed by repository — the combined diff's
  /// whole data layer, one `GET …/repos/{repo}/diff` per member.
  files: Record<string, { status: string; path: string }[]>;
  /// Repositories whose `/diff` refuses once, then answers. A changeset
  /// can hold a repository this reader may not see, and the section has
  /// to degrade to *that group* saying so.
  refuseDiff: Set<string>;
  /// Paths already ticked, keyed `repo/change`.
  views: Record<string, string[]>;
  /// Every viewed write the page sent, URL and body. The URL is the
  /// assertion that matters: this surface is the one place in the
  /// product where one member's tick can be sent to another.
  viewPuts: { url: string; body: unknown }[];
  /// What `…/changesets/{key}/diffstat` answers: the server's `+N −M`
  /// per member and in total, or a status to refuse with. 404 is what a
  /// set with a member that has no patchset yet gets, and the page must
  /// render that as no number rather than as an error.
  diffstat: ChangesetDiffstatFixture | number;
}

interface ChangesetDiffstatFixture {
  changeset: string;
  members: {
    repo: string;
    change: string;
    patchset: number;
    files: number;
    insertions: number;
    deletions: number;
    truncated: boolean;
  }[];
  total: {
    files: number;
    insertions: number;
    deletions: number;
    truncated: boolean;
  };
}

/// The fixture's set counted: `api` two files, `web` one.
function diffstat(): ChangesetDiffstatFixture {
  return {
    changeset: "rename-payments",
    members: [
      {
        repo: "api",
        change: "c-api",
        patchset: 2,
        files: 2,
        insertions: 3,
        deletions: 1,
        truncated: false,
      },
      {
        repo: "web",
        change: "c-web",
        patchset: 2,
        files: 1,
        insertions: 2,
        deletions: 1,
        truncated: false,
      },
    ],
    total: { files: 3, insertions: 5, deletions: 2, truncated: false },
  };
}

async function mockChangesets(page: Page, over: Partial<State> = {}) {
  await signIn(page);
  return routeChangesets(page, over);
}

/// The same control plane, to a browser with no account at all.
///
/// A changeset over public repositories is readable signed-out on the
/// wire — `changesets_api`'s reads go through `Scope::RepoRead`, whose
/// `public_read` branch admits a `None` principal — so the only thing
/// standing between a stranger and the review was the address. These
/// tests drive the address.
async function anonymousChangesets(page: Page, over: Partial<State> = {}) {
  // Refuse anything this file has not deliberately mocked, registered
  // first so the routes below win. Without it an unmocked `/v1` call
  // goes through vite's proxy to whatever is on :8080, and the suite is
  // hermetic only while nobody has the manual stack up.
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  return routeChangesets(page, over);
}

async function routeChangesets(page: Page, over: Partial<State> = {}) {
  const s: State = {
    changesets: [changeset()],
    verdict: verdict(),
    openChanges: [
      change("api", "c-api", "rename the service", {
        changeset: "rename-payments",
        repo: "api",
      }),
      change("api", "c-api-2", "unrelated api fix", {
        changeset: null,
        repo: "api",
      }),
      change("api", "c-api-3", "another api fix", {
        changeset: null,
        repo: "api",
      }),
      change("web", "c-web-2", "web copy tweak", {
        changeset: null,
        repo: "web",
      }),
      change("docs", "c-docs", "document it", {
        changeset: null,
        repo: "docs",
      }),
    ],
    posts: [],
    reads: 0,
    land: "accept",
    revert: "accept",
    edges: "accept",
    // Both members have a `README.md`, on purpose: that collision is
    // the reason §6a forbids a flat path-sorted list and requires the
    // open file's header to name its repository first.
    files: {
      api: [
        { status: "modified", path: "README.md" },
        { status: "modified", path: "src/pay.rs" },
      ],
      web: [{ status: "modified", path: "README.md" }],
    },
    refuseDiff: new Set<string>(),
    views: {},
    viewPuts: [],
    diffstat: diffstat(),
    ...over,
  };
  const find = (key: string) => s.changesets.find((c) => c.key === key);

  await page.route("**/v1/orgs/acme/changes?**", (r) =>
    r.fulfill({ json: { changes: s.openChanges } }),
  );
  await page.route("**/v1/orgs/acme/changesets", (r) => {
    if (r.request().method() === "POST") {
      const body = r.request().postDataJSON();
      s.posts.push({ path: "changesets", body });
      const made = changeset({
        key: body.key,
        title: body.title,
        body: body.body ?? null,
        members: body.members.map((m: { repo: string; change: string }) => ({
          repo: m.repo,
          change: change(m.repo, m.change, `title of ${m.change}`),
        })),
        edges: [],
        order: body.members,
        checks: [],
        composition: null,
      });
      s.changesets.push(made);
      return r.fulfill({ status: 201, json: made });
    }
    return r.fulfill({ json: { changesets: s.changesets } });
  });
  await page.route("**/v1/orgs/acme/changesets?state=*", (r) => {
    const state = new URL(r.request().url()).searchParams.get("state");
    return r.fulfill({
      json: { changesets: s.changesets.filter((c) => c.state === state) },
    });
  });
  await page.route("**/v1/orgs/acme/changesets/*", (r) => {
    const key = decodeURIComponent(r.request().url().split("/").pop() ?? "");
    const cs = find(key);
    if (!cs)
      return r.fulfill({ status: 404, json: { error: "no such changeset" } });
    if (cs.state === "landing") {
      // The lander is another process; this page only asks. Two polls
      // in, it has finished.
      s.reads += 1;
      if (s.reads >= 2) {
        cs.state = "landed";
        cs.landing = {
          ...(cs.landing as Record<string, unknown>),
          finished_at: Date.now(),
          outcome: "landed",
          members: [
            {
              repo: "api",
              change: "c-api",
              ref: "refs/heads/main",
              old: SHA(1),
              new: SHA(2),
              state: "done",
              note: null,
            },
            {
              repo: "web",
              change: "c-web",
              ref: "refs/heads/main",
              old: SHA(1),
              new: SHA(2),
              state: "done",
              note: null,
            },
          ],
        };
        // What the server really answers once a set has landed: the
        // question "can this land" is closed, for the set and for each
        // member, and it says so in those words.
        const landed = verdict();
        s.verdict = verdict({
          state: "landed",
          landable: false,
          gate: "blocked",
          explanation: "changeset is landed",
          members: (landed.members as Record<string, unknown>[]).map((m) => ({
            ...m,
            state: "landed",
            landable: false,
            gate: "blocked",
            explanation: "change is landed",
          })),
        });
        for (const m of cs.members as Record<string, unknown>[])
          (m.change as Record<string, unknown>).state = "landed";
      }
    }
    return r.fulfill({ json: cs });
  });
  await page.route("**/v1/orgs/acme/changesets/*/verdict", (r) =>
    r.fulfill({ json: s.verdict }),
  );
  await page.route("**/v1/orgs/acme/changesets/*/diffstat", (r) =>
    typeof s.diffstat === "number"
      ? r.fulfill({
          status: s.diffstat,
          json: { error: "no diffstat for this changeset" },
        })
      : r.fulfill({ json: s.diffstat }),
  );
  await page.route("**/v1/orgs/acme/changesets/*/workspace", (r) =>
    r.fulfill({
      json: {
        key: "rename-payments",
        title: "Rename the payments service",
        state: "open",
        composition: "abcdef0123456789abcdef",
        tip: SHA(7),
        clone_url: "http://127.0.0.1:8080/acme/changesets/rename-payments.git",
        ssh_clone_url: null,
        members: [],
        note: null,
      },
    }),
  );
  await page.route("**/v1/orgs/acme/changesets/*/land", (r) => {
    s.posts.push({ path: "land", body: null });
    if (s.land !== "accept")
      return r.fulfill({ status: s.land.status, json: s.land.body });
    const cs = find("rename-payments") as Record<string, unknown>;
    cs.state = "landing";
    cs.landing = {
      id: "01landing",
      attempt: 1,
      started_at: Date.now(),
      finished_at: null,
      outcome: null,
      members: [
        {
          repo: "api",
          change: "c-api",
          ref: "refs/heads/main",
          old: SHA(1),
          new: SHA(2),
          state: "pending",
          note: null,
        },
        {
          repo: "web",
          change: "c-web",
          ref: "refs/heads/main",
          old: SHA(1),
          new: SHA(2),
          state: "pending",
          note: null,
        },
      ],
    };
    return r.fulfill({
      status: 202,
      json: {
        queued: true,
        job: "01job",
        changeset: "rename-payments",
        landing: "01landing",
        plan: [],
      },
    });
  });
  await page.route("**/v1/orgs/acme/changesets/*/revert", (r) => {
    const body = r.request().postDataJSON();
    s.posts.push({ path: "revert", body });
    if (s.revert !== "accept")
      return r.fulfill({ status: s.revert.status, json: s.revert.body });
    const made = changeset({
      key: body.key,
      title: body.title ?? 'Revert "Rename the payments service"',
      reverts: "rename-payments",
      members: [
        {
          repo: "web",
          change: change("web", "r-web", "Revert point web at billing"),
        },
        {
          repo: "api",
          change: change("api", "r-api", "Revert rename the service"),
        },
      ],
      edges: [
        {
          from: { repo: "web", change: "r-web" },
          to: { repo: "api", change: "r-api" },
        },
      ],
      order: [
        { repo: "web", change: "r-web" },
        { repo: "api", change: "r-api" },
      ],
      checks: [],
      composition: null,
    });
    s.changesets.push(made);
    (find("rename-payments") as Record<string, unknown>).reverted_by = [
      body.key,
    ];
    return r.fulfill({ status: 201, json: made });
  });
  // `PUT …/edges` replaces the set wholesale and answers the changeset
  // with the landing order the new edges imply — so the mock sorts,
  // rather than echoing a stale `order` the page would then render as
  // the plan.
  await page.route("**/v1/orgs/acme/changesets/*/edges", (r) => {
    const body = r.request().postDataJSON();
    s.posts.push({ path: "edges", body });
    if (s.edges !== "accept")
      return r.fulfill({ status: s.edges.status, json: s.edges.body });
    const cs = find("rename-payments") as Record<string, unknown>;
    cs.edges = body.edges;
    const label = (m: { repo: string; change: string }) =>
      `${m.repo}/${m.change}`;
    const all = (cs.members as { repo: string; change: { key: string } }[]).map(
      (m) => ({ repo: m.repo, change: m.change.key }),
    );
    const order: { repo: string; change: string }[] = [];
    while (order.length < all.length) {
      const next = all.find(
        (m) =>
          !order.some((o) => label(o) === label(m)) &&
          !(body.edges as { from: typeof m; to: typeof m }[]).some(
            (e) =>
              label(e.to) === label(m) &&
              !order.some((o) => label(o) === label(e.from)),
          ),
      );
      if (!next) break;
      order.push(next);
    }
    cs.order = order;
    return r.fulfill({ json: cs });
  });
  // The combined cross-repo diff's data layer: one structural diff and
  // one viewed-mark read per member, fanned out from the changeset page.
  await page.route("**/v1/orgs/acme/repos/*/diff?**", (r) => {
    const repo = r.request().url().split("/repos/")[1].split("/")[0];
    // Refuses until the test clears it, rather than disarming itself on
    // the first hit. A mock that changes behaviour as a side effect of
    // being *read* is order-dependent by construction: the fan-out can
    // ask twice, the second answer overwrites the error state, and the
    // group renders fine — which is what happened on CI while passing
    // here. The caller now says when the store recovers.
    if (s.refuseDiff.has(repo)) {
      return r.fulfill({ status: 403, json: { error: "no such repository" } });
    }
    return r.fulfill({
      json: {
        changes: (s.files[repo] ?? []).map((f) => ({
          ...f,
          old_oid: "aaa",
          new_oid: "bbb",
        })),
      },
    });
  });
  await page.route("**/v1/orgs/acme/repos/*/changes/*/views", (r) => {
    const url = r.request().url();
    const [, rest] = url.split("/repos/");
    const repo = rest.split("/")[0];
    const key = rest.split("/changes/")[1].split("/")[0];
    const id = `${repo}/${key}`;
    if (r.request().method() === "PUT") {
      const body = r.request().postDataJSON();
      s.viewPuts.push({ url, body });
      const at = new Set(s.views[id] ?? []);
      if (body.viewed) at.add(body.path);
      else at.delete(body.path);
      s.views[id] = [...at];
      return r.fulfill({ status: 204, body: "" });
    }
    return r.fulfill({ json: { patchset: 2, viewed: s.views[id] ?? [] } });
  });
  // Both sides of a file. The old side is whatever is at the parent and
  // the new side whatever is at the tip, so the panel really diffs two
  // texts rather than rendering a canned one.
  await page.route("**/v1/orgs/acme/repos/*/files/**", (r) => {
    const url = new URL(r.request().url());
    const [, rest] = url.pathname.split("/repos/");
    const repo = rest.split("/")[0];
    const path = decodeURIComponent(rest.split("/files/")[1]);
    const at = url.searchParams.get("at");
    const tail = at === SHA(1) ? "before" : "after";
    return r.fulfill({
      contentType: "text/plain",
      body: `${repo} ${path}\nline two\n${tail}\nline four\n`,
    });
  });

  await page.route("**/v1/orgs/acme/changesets/*/abandon", (r) => {
    s.posts.push({ path: "abandon", body: null });
    (find("rename-payments") as Record<string, unknown>).state = "abandoned";
    return r.fulfill({ status: 204, body: "" });
  });
  return s;
}

async function openList(page: Page) {
  // `exact`, because Playwright's accessible-name matching is a
  // substring by default and the changeset page's own "← All changesets"
  // back-link now contains this one. Not `.first()`: to a person the two
  // controls are plainly different — a rail item beside a layers glyph
  // and an arrowed way back — so the honest fix is to say which name is
  // meant, not to take whichever the DOM happens to order first.
  await page.getByRole("link", { name: "Changesets", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Changesets" })).toBeVisible();
}

test("the picker offers the org's open changes, one per repository, and says why a row is grey", async ({
  page,
}) => {
  const s = await mockChangesets(page);
  await openList(page);
  await expect(
    page.getByRole("link", { name: "rename-payments" }),
  ).toBeVisible();

  await page.getByRole("button", { name: "New changeset" }).click();
  // Grouped by repository, every open change listed — including the one
  // another changeset already holds, greyed with that changeset named.
  const held = page.getByLabel("api/c-api: rename the service");
  await expect(held).toBeDisabled();
  await expect(
    page.getByText("already in changeset rename-payments"),
  ).toBeVisible();

  // Picking one change in `api` greys the other `api` change with the
  // one-per-repository reason, and leaves `web` and `docs` alone.
  await page.getByLabel("api/c-api-2: unrelated api fix").check();
  await expect(page.getByLabel("api/c-api-3: another api fix")).toBeDisabled();
  await expect(
    page.getByText("one change per repository; api is already picked"),
  ).toBeVisible();
  await expect(page.getByLabel("web/c-web-2: web copy tweak")).toBeEnabled();
  await page.getByLabel("docs/c-docs: document it").check();
  await expect(page.getByText("2/16")).toBeVisible();

  // The key follows the title until somebody edits it.
  await page.getByLabel("Title").fill("Fix the API (round 2)!");
  await expect(page.getByLabel("Key")).toHaveValue("fix-the-api-round-2");
  await page.getByLabel("Key").fill("api-fix");
  await page.getByLabel("Title").fill("Fix the API, round two");
  await expect(page.getByLabel("Key")).toHaveValue("api-fix");
  await page.getByLabel("Key").fill("api fix");
  await expect(page.getByText(/a key is 1–72 letters/)).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Create changeset" }),
  ).toBeDisabled();
  await page.getByLabel("Key").fill("api-fix");

  await page.getByRole("button", { name: "Create changeset" }).click();
  // The body the server accepts: key, title, members as {repo, change}.
  expect(s.posts).toEqual([
    {
      path: "changesets",
      body: {
        key: "api-fix",
        title: "Fix the API, round two",
        members: [
          { repo: "api", change: "c-api-2" },
          { repo: "docs", change: "c-docs" },
        ],
      },
    },
  ]);
  // …and the page is the new changeset, at its own address.
  await expect(page).toHaveURL(/\/dashboard\/changesets\/api-fix$/);
  await expect(
    page.getByRole("heading", { name: "Fix the API, round two" }),
  ).toBeVisible();
  // Once in the landing-order strip and once in the members table: the
  // strip is the plan, the table is the detail, and both name a member
  // by its key.
  await expect(page.getByText("c-api-2", { exact: true })).toHaveCount(2);
  await expect(page.getByText("c-docs", { exact: true })).toHaveCount(2);
});

test("the address a changeset notification mails resolves to the changeset, not the shell", async ({
  page,
}) => {
  // The other half of a pair. `changeset_notify_e2e.rs` asserts the mail
  // builds `/dashboard/changesets/{key}` and that the server answers it —
  // but the dashboard is an SPA behind a catch-all, so **every**
  // `/dashboard/…` path answers 200 with the same shell.
  // `/dashboard/utter-nonsense` does too. That check therefore cannot
  // fail, and on its own it is worth very little.
  //
  // This is exactly how FORGE-PARITY §4.1 happened: review notifications
  // pointed at an address that returned 200, served the shell, and then
  // rendered "We couldn't find changes". The server was serving it. The
  // client router was not resolving it.
  //
  // So the claim "the link in the mail works" is only made by the two
  // halves together: the server serves that path, and the client router
  // renders the changeset there rather than a not-found. If the mail's
  // address ever changes, `changeset_url` in `mail/templates.rs`, the
  // `assert_eq!` beside it, and this literal must move together.
  await mockChangesets(page);
  await page.goto("/dashboard/changesets/rename-payments");

  await expect(
    page.getByRole("heading", { name: "Rename the payments service" }),
  ).toBeVisible();
  // Naming what must NOT be there: a shell that resolved nothing is the
  // failure this test exists for, and it is invisible to an assertion
  // that only looks for something present.
  await expect(page.getByText(/We couldn't find/)).toHaveCount(0);
});

test("a changeset shows its members in landing order with the composed verdict and checks", async ({
  page,
}) => {
  await mockChangesets(page);
  await page.goto("/dashboard/changesets/rename-payments");
  await expect(
    page.getByRole("heading", { name: "Rename the payments service" }),
  ).toBeVisible();

  // The composed gate, in the server's words.
  await expect(page.getByText("every member is ready to land")).toBeVisible();

  // `members` lists web first; `order` puts api first. The table follows
  // the plan the lander will walk.
  const rows = page.getByRole("row").filter({ hasText: /c-(api|web)/ });
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(0)).toContainText("c-api");
  await expect(rows.nth(1)).toContainText("c-web");
  await expect(rows.nth(0)).toContainText("approved by an owner of every path");
  // The order is a picture, not a line of edge text: two waves, `api`
  // alone in the first, `web` in the second. The wave heading is the
  // claim "these land together", so a member whose predecessors are the
  // whole previous wave needs no `after:` line and does not get one.
  const strip = page.locator("section", { hasText: "Landing order" }).first();
  await expect(strip).toContainText("lands first");
  await expect(strip).toContainText("then (wave 2)");
  await expect(strip.getByText(/^after:/)).toHaveCount(0);

  // A member is reviewed on its own change page, which is a forge
  // address. The row's key is that link; the repository name is the
  // repository. Both, because the first cut linked only the repository,
  // and it led to the code browser — a page with no approve button on it.
  await expect(
    rows.nth(0).getByRole("link", { name: "c-api" }),
  ).toHaveAttribute("href", "/acme/api/changes/c-api");
  await expect(
    rows.nth(0).getByRole("link", { name: "api", exact: true }),
  ).toHaveAttribute("href", "/acme/api");

  // Composed CI is named by repository as well as job.
  await expect(page.getByText("web: ci / build")).toBeVisible();

  // The workspace clone block, with the recursive-clone note.
  await expect(page.getByLabel("HTTPS clone URL")).toHaveValue(
    "http://127.0.0.1:8080/acme/changesets/rename-payments.git",
  );
  await expect(page.getByText(/--recurse-submodules/)).toBeVisible();

  // The sidebar knows where it is.
  // Exact, for the same reason `openList` is: the back-link on this very
  // page is also a link whose name contains "Changesets".
  await expect(
    page.getByRole("link", { name: "Changesets", exact: true }),
  ).toHaveAttribute("data-active", "true");
});

test("a verdict naming a member by a 40-hex key does not push the actions off the page", async ({
  page,
}) => {
  // Every revert's members carry a full-length Change-Id, and the
  // composed verdict names the blocking member by it:
  // "api/If6cd51c50e1f2124b8c7db1c2535aba4ad8bef0e: blocked: needs an
  // owner of …". One unbreakable token, and a `1fr` grid track grows to
  // its min-content width — so the walkthrough's revert screenshot had
  // the Actions column, Land button first, running off the right edge
  // of a 1440px viewport. The stage's audit ran after it had navigated
  // away and never saw it.
  const long = "If6cd51c50e1f2124b8c7db1c2535aba4ad8bef0e";
  const cs = changeset({
    members: [
      {
        repo: "api",
        change: change("api", long, 'Revert "rename the service"'),
      },
      { repo: "web", change: change("web", "c-web", "point web at billing") },
    ],
    edges: [
      {
        from: { repo: "web", change: "c-web" },
        to: { repo: "api", change: long },
      },
    ],
    order: [
      { repo: "web", change: "c-web" },
      { repo: "api", change: long },
    ],
  });
  const v = verdict();
  const members = v.members as Record<string, unknown>[];
  members[0] = {
    ...members[0],
    change: long,
    landable: false,
    gate: "ready",
    explanation: `blocked: needs an owner of /fees/tiers-1788461098060.txt (owners: ada@acme.dev)`,
  };
  await mockChangesets(page, {
    changesets: [cs],
    verdict: {
      ...v,
      landable: false,
      gate: "ready",
      explanation: `api/${long}: blocked: needs an owner of /fees/tiers-1788461098060.txt (owners: ada@acme.dev)`,
    },
  });
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/dashboard/changesets/rename-payments");
  await expect(
    page.locator("section", { hasText: "Verdict" }).first(),
  ).toContainText(`api/${long}`);

  const land = page.getByRole("button", { name: "Land all members" });
  const box = await land.boundingBox();
  expect(box, "the Land button has a box").not.toBeNull();
  expect(box!.x + box!.width).toBeLessThanOrEqual(1440);
  const overflow = await page.evaluate(() => {
    const de = document.documentElement;
    return de.scrollWidth - de.clientWidth;
  });
  expect(overflow, "the page scrolls horizontally").toBeLessThanOrEqual(1);
});

test("the members table stays inside its card, Remove column and all", async ({
  page,
}) => {
  // Seen in the end-to-end pass, at 1440px: the table was laid out by
  // its content, so a member's key and title took their full width, the
  // gate's sentence was squeezed to one word per line, and the table ran
  // past the card — the Remove column read "Remo", then "Re", reachable
  // only by a horizontal scroll nothing hinted at. The cells carried
  // `max-w-*` and `truncate`, which the auto layout ignores.
  const long = "If6cd51c50e1f2124b8c7db1c2535aba4ad8bef0e";
  const cs = changeset({
    members: [
      {
        repo: "api",
        change: change(
          "api",
          long,
          "Rename the payments service and move every caller onto the new name",
        ),
      },
      { repo: "web", change: change("web", "c-web", "point web at billing") },
    ],
    edges: [],
    order: [
      { repo: "api", change: long },
      { repo: "web", change: "c-web" },
    ],
  });
  const v = verdict();
  const members = v.members as Record<string, unknown>[];
  members[0] = {
    ...members[0],
    change: long,
    landable: false,
    gate: "ready",
    explanation:
      "blocked: needs an owner of /fees/tiers-1788461098060.txt (owners: ada@acme.dev, grace@acme.dev)",
  };
  members[1] = {
    ...members[1],
    landable: true,
    gate: "waiting",
    explanation:
      "ok: approved by an owner of every path; waiting on 1 check(s): ci / build",
  };
  await mockChangesets(page, {
    changesets: [cs],
    verdict: { ...v, landable: false, gate: "ready" },
  });
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/dashboard/changesets/rename-payments");

  const table = page.getByRole("table").filter({ hasText: "Repository" });
  await expect(table).toBeVisible();
  // The table is no wider than the scroller it sits in: nothing to
  // scroll to.
  const overflow = await table.evaluate((t) => {
    const scroller = t.parentElement!;
    return scroller.scrollWidth - scroller.clientWidth;
  });
  expect(
    overflow,
    "the members table scrolls horizontally",
  ).toBeLessThanOrEqual(1);
  // Every heading fits its column: the first cut gave Patchset a column
  // narrower than the word, and "PATCHSET" ran into "GATE".
  const cramped = await table.evaluate((t) =>
    Array.from(t.querySelectorAll("th"))
      .filter((th) => th.scrollWidth > th.clientWidth)
      .map((th) => th.textContent),
  );
  expect(cramped, "headings wider than their columns").toEqual([]);
  // And the last column is whole: the Remove button ends inside the
  // card that draws the table's border.
  const remove = page.getByRole("button", { name: `Remove api/${long}` });
  await expect(remove).toBeVisible();
  const [button, card] = await Promise.all([
    remove.boundingBox(),
    table
      .locator("xpath=ancestor::div[contains(@class,'rounded-lg')][1]")
      .boundingBox(),
  ]);
  expect(button, "the Remove button has a box").not.toBeNull();
  expect(card, "the card has a box").not.toBeNull();
  expect(button!.x + button!.width).toBeLessThanOrEqual(card!.x + card!.width);
  // The key ellipsises rather than widening its column: the line it is
  // on clips it, so there is more key than the cell shows.
  const key = page.getByRole("link", { name: long });
  const clipped = await key.evaluate((a) => {
    const line = a.parentElement!;
    return line.scrollWidth - line.clientWidth;
  });
  expect(clipped, "the 40-hex key is not being truncated").toBeGreaterThan(0);
});

test("a waiting verdict lists the checks it waits on as pending, not as failed", async ({
  page,
}) => {
  // Seen in the end-to-end pass: "waiting · ok: all 2 member(s)
  // approved; waiting on 1 check(s)" and, under it, the composed check
  // with a red cross — the glyph every other panel on this product uses
  // for a check that failed. Nothing had failed; the run was going.
  const v = verdict();
  await mockChangesets(page, {
    verdict: {
      ...v,
      landable: true,
      gate: "waiting",
      explanation: "ok: all 2 member(s) approved; waiting on 1 check(s)",
      waiting_on: ["web: ci / build", "api/c-api: ci / test"],
    },
  });
  await page.goto("/dashboard/changesets/rename-payments");
  const verdictBox = page.locator("section", { hasText: "Verdict" }).first();
  await expect(verdictBox).toContainText("waiting");
  const rows = verdictBox.getByRole("listitem");
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(0)).toContainText("web: ci / build");
  // The dashed circle the Checks panel draws for a running check…
  await expect(rows.locator("svg.lucide-circle-dashed")).toHaveCount(2);
  // …and never the cross it draws for a failed one.
  await expect(rows.locator("svg.lucide-x")).toHaveCount(0);
});

test("a composed check's Details link is one of our own pages, not a third party's", async ({
  page,
}) => {
  // The composed panel labelled each row with the repository it ran in
  // as `posted_by`, and the panel reads `posted_by` as the provider gate
  // on the Details link. So a composed run's link — pointing at our own
  // run page, one navigation away — opened in a new tab with
  // `rel="ugc"`, exactly as a URL a stranger posted to the intake would.
  // The end-to-end pass clicked Details and waited for a page that
  // opened somewhere else.
  // `DetailLink` treats only an absolute URL on this origin as ours, and
  // this origin is the preview server `playwright.config.ts` points at —
  // the same constant it reads, so the two cannot disagree about a port
  // that is per-worktree now.
  const origin = PREVIEW_ORIGIN;
  const cs = changeset({
    checks: [
      {
        repo: "web",
        name: "ci / build",
        state: "failing",
        detail_url: `${origin}/acme/web/checks/runs/wr1`,
        run: "wr1",
      },
    ],
  });
  await mockChangesets(page, { changesets: [cs] });
  await page.goto("/dashboard/changesets/rename-payments");
  await expect(page.getByText("web: ci / build")).toBeVisible();

  const details = page.getByRole("link", { name: "Details" });
  await expect(details).toHaveAttribute("href", "/acme/web/checks/runs/wr1");
  await expect(details).not.toHaveAttribute("target", "_blank");
  const rel = await details.getAttribute("rel");
  expect(
    rel ?? "",
    "our own page marked as user-generated content",
  ).not.toContain("ugc");
  await details.click();
  await page.waitForURL(/\/acme\/web\/checks\/runs\/wr1$/);
});

test("a member nobody approved reads blocked, for the row and for the set", async ({
  page,
}) => {
  // The wire keeps the check gate and the review verdict apart: an
  // unapproved member with nothing building is `gate: ready` and
  // `landable: false`. Rendered raw, the page said "ready · blocked:
  // needs an owner of fees/" — a contradiction a person has to
  // untangle. The review comes first, so the word is blocked.
  const v = verdict();
  const members = v.members as Record<string, unknown>[];
  members[0] = {
    ...members[0],
    landable: false,
    gate: "ready",
    explanation: "blocked: needs an owner of fees/ (ada@acme.dev)",
    approvals: [],
  };
  await mockChangesets(page, {
    verdict: {
      ...v,
      landable: false,
      gate: "ready",
      explanation: "api/c-api: blocked: needs an owner of fees/ (ada@acme.dev)",
    },
  });
  await page.goto("/dashboard/changesets/rename-payments");
  const section = page.locator("section", { hasText: "Verdict" }).first();
  await expect(section.getByText("blocked", { exact: true })).toBeVisible();
  await expect(section.getByText("ready", { exact: true })).toHaveCount(0);
  const row = page.getByRole("row").filter({ hasText: "c-api" });
  await expect(row.getByText("blocked", { exact: true })).toBeVisible();
  await expect(row).toContainText("needs an owner of fees/");
  // The other member is still ready on its own.
  await expect(
    page.getByRole("row").filter({ hasText: "c-web" }).getByText("ready", {
      exact: true,
    }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Land all members" }),
  ).toBeDisabled();
});

test("a reader who may not write is told what the actions take, not handed buttons that refuse", async ({
  page,
}) => {
  // Seen on the manual stack: a viewer opened a landed changeset and was
  // offered "Revert…", enabled; pressing through answered `no changeset
  // "…"` about the changeset on the screen. Every write route masks a
  // caller who may not write the same way, so the page has to know
  // before it draws — `viewer_write` is the server's own answer.
  const landed = changeset({
    state: "landed",
    viewer_write: false,
    members: [
      {
        repo: "web",
        change: change("web", "c-web", "point web at billing", {
          state: "landed",
        }),
      },
      {
        repo: "api",
        change: change("api", "c-api", "rename the service", {
          state: "landed",
        }),
      },
    ],
    landing: {
      id: "01landing",
      attempt: 1,
      started_at: Date.now() - 120_000,
      finished_at: Date.now() - 60_000,
      outcome: "landed",
      members: [],
    },
  });
  const open = changeset({ key: "next-one", viewer_write: false });
  await mockChangesets(page, {
    changesets: [landed, open],
    openChanges: [
      change("api", "c-api-2", "unrelated api fix", {
        changeset: null,
        repo: "api",
        viewer_write: false,
      }),
      change("web", "c-web-2", "web copy tweak", {
        changeset: null,
        repo: "web",
        viewer_write: false,
      }),
    ],
  });

  // A landed set: the revert a writer would be offered is not drawn.
  await page.goto("/dashboard/changesets/rename-payments");
  await expect(
    page.getByText(
      "Landing, reverting or abandoning this changeset takes write access to every member repository.",
    ),
  ).toBeVisible();
  for (const name of ["Land all members", "Revert…", "Abandon"])
    await expect(page.getByRole("button", { name })).toHaveCount(0);

  // An open one: nor are the membership controls.
  await page.goto("/dashboard/changesets/next-one");
  await expect(page.getByText("c-web", { exact: true }).first()).toBeVisible();
  // The order strip is drawn for a reader too, with the control
  // replaced by what changing it would take.
  await expect(
    page.getByText(
      "Changing the landing order takes write access to every member repository.",
    ),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Edit order" })).toHaveCount(0);
  for (const name of ["Add member", "Land all members", "Abandon"])
    await expect(page.getByRole("button", { name })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /^Remove / })).toHaveCount(0);

  // The picker still lists every change — a reader looking for one
  // should find it — and says why each is grey, once per row and once
  // where the button is.
  await openList(page);
  await page.getByRole("button", { name: "New changeset" }).click();
  await expect(
    page.getByLabel("api/c-api-2: unrelated api fix"),
  ).toBeDisabled();
  await expect(page.getByLabel("web/c-web-2: web copy tweak")).toBeDisabled();
  await expect(
    page.getByText("composing takes write access to api"),
  ).toBeVisible();
  await expect(
    page.getByText("composing takes write access to web"),
  ).toBeVisible();
  await expect(
    page.getByText(/none of these is in one you may write to/),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Create changeset" }),
  ).toBeDisabled();
});

test("landing polls until the lander is done, then offers the revert", async ({
  page,
}) => {
  const s = await mockChangesets(page);
  await page.goto("/dashboard/changesets/rename-payments");
  await page.getByRole("button", { name: "Land all members" }).click();
  // Progress is a sentence with a count, so a long landing does not
  // read as stuck; then the outcome.
  await expect(
    page.getByRole("status").filter({ hasText: /Land/ }),
  ).toContainText(
    /Landing: 0 of 2 landed so far…|Landed: 2 members on their trunks\./,
  );
  await expect(
    page.getByText("Landed: 2 members on their trunks."),
  ).toBeVisible();
  await expect(page.getByText("landed", { exact: true }).first()).toBeVisible();
  // A landed set is not a blocked one. The server's verdict for it is
  // `landable: false, "changeset is landed"`, and read as a gate that
  // painted the Verdict box and both rows red with the word "blocked" —
  // the first landed screenshot the walkthrough took. Once the set is
  // not open there is no gate, only the state.
  const section = page.locator("section", { hasText: "Verdict" }).first();
  await expect(section.getByText("landed", { exact: true })).toBeVisible();
  await expect(page.getByText("blocked", { exact: true })).toHaveCount(0);
  for (const key of ["c-api", "c-web"])
    await expect(
      page
        .getByRole("row")
        .filter({ hasText: key })
        .getByText("landed", { exact: true })
        .first(),
    ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Land all members" }),
  ).toBeDisabled();
  expect(s.posts.map((p) => p.path)).toEqual(["land"]);

  // Revert: the key is pre-filled from the original's, the body sends
  // only what was typed, and the page moves to the new changeset, which
  // says what it reverts.
  await page.getByRole("button", { name: "Revert…" }).click();
  await expect(page.getByLabel("Key for the reverting changeset")).toHaveValue(
    "revert-rename-payments",
  );
  await page.getByRole("button", { name: "Create the revert" }).click();
  expect(s.posts[1]).toEqual({
    path: "revert",
    body: { key: "revert-rename-payments" },
  });
  await expect(page).toHaveURL(
    /\/dashboard\/changesets\/revert-rename-payments$/,
  );
  await expect(page.getByText(/^Reverts rename-payments$/)).toBeVisible();
  await expect(
    page.getByRole("link", { name: "rename-payments" }),
  ).toBeVisible();
  // Reversed edges: web lands first now, api after it. The strip is
  // where that is said, and it says it as two waves rather than as the
  // edge it came from.
  const reverted = page
    .locator("section", { hasText: "Landing order" })
    .first();
  await expect(reverted).toContainText("2 waves");
  const first = reverted.locator("div", { hasText: /^lands first$/ }).first();
  await expect(first.locator("xpath=..")).toContainText("r-web");
});

test("a refused landing shows the gate's answer, not an error", async ({
  page,
}) => {
  // The verdict said ready; a check reported failing between the read
  // and the press. The 409's body is the answer, and it is a list.
  await mockChangesets(page, {
    land: {
      status: 409,
      body: {
        error: "waiting on web/c-web: check ci / build",
        gate: "waiting",
        waiting_on: ["web/c-web: ci / build", "api/c-api: ci / test"],
      },
    },
  });
  await page.goto("/dashboard/changesets/rename-payments");
  await page.getByRole("button", { name: "Land all members" }).click();
  const alert = page.getByRole("alert");
  await expect(alert).toContainText("waiting on web/c-web: check ci / build");
  await expect(alert).toContainText("web/c-web: ci / build");
  await expect(alert).toContainText("api/c-api: ci / test");
  await expect(page.getByText(/Could not land/)).toHaveCount(0);
  // Still open, still landable next time.
  await expect(
    page.getByRole("button", { name: "Land all members" }),
  ).toBeEnabled();
});

test("a revert the server refuses lists every conflicting member", async ({
  page,
}) => {
  await mockChangesets(page, {
    changesets: [
      changeset({
        state: "landed",
        landing: {
          id: "01landing",
          attempt: 1,
          started_at: 0,
          finished_at: 1,
          outcome: "landed",
          members: [],
        },
      }),
    ],
    verdict: verdict({
      state: "landed",
      landable: false,
      gate: "blocked",
      explanation: "changeset is landed",
    }),
    revert: {
      status: 409,
      body: {
        error:
          "api/c-api: refs/heads/main has changed since it landed at src/lib.rs",
        conflicts: [
          {
            repo: "api",
            change: "c-api",
            changed: ["src/lib.rs"],
            why: "refs/heads/main has changed since it landed at src/lib.rs",
          },
          {
            repo: "web",
            change: "c-web",
            branch_exists: true,
            why: "revert/revert-rename-payments already exists",
          },
        ],
      },
    },
  });
  await page.goto("/dashboard/changesets/rename-payments");
  await expect(
    page.getByRole("button", { name: "Land all members" }),
  ).toBeDisabled();
  await page.getByRole("button", { name: "Revert…" }).click();
  await page.getByRole("button", { name: "Create the revert" }).click();
  await expect(
    page.getByText(
      /Could not revert: api\/c-api: refs\/heads\/main has changed/,
    ),
  ).toBeVisible();
  const list = page
    .getByRole("alert")
    .filter({ has: page.getByText("web/c-web") });
  await expect(list).toContainText(
    "refs/heads/main has changed since it landed at src/lib.rs",
  );
  await expect(list).toContainText(
    "revert/revert-rename-payments already exists",
  );
  // Nothing moved: still on the original, still landed.
  await expect(page).toHaveURL(/\/dashboard\/changesets\/rename-payments$/);
});

test("the list filters by state and an abandoned changeset leaves the open list", async ({
  page,
}) => {
  const s = await mockChangesets(page, {
    changesets: [
      changeset(),
      changeset({
        key: "old-one",
        title: "An older landed set",
        state: "landed",
      }),
    ],
  });
  await openList(page);
  await expect(
    page.getByRole("link", { name: "rename-payments" }),
  ).toBeVisible();
  await expect(page.getByRole("link", { name: "old-one" })).toBeVisible();
  await page.getByRole("combobox", { name: "Filter by state" }).click();
  await page.getByRole("option", { name: "landed" }).click();
  await expect(page.getByRole("link", { name: "old-one" })).toBeVisible();
  await expect(page.getByRole("link", { name: "rename-payments" })).toHaveCount(
    0,
  );

  await page.getByRole("link", { name: "old-one" }).click();
  await expect(
    page.getByRole("heading", { name: "An older landed set" }),
  ).toBeVisible();
  // A link, not a button: the way back to the list is a real address,
  // so it can be copied, middle-clicked and opened in a new tab. It was
  // a `<button>` for as long as the dashboard was the only mount that
  // drew it — a control with a destination and no href.
  await page.getByRole("link", { name: "All changesets" }).click();
  await page.getByRole("link", { name: "rename-payments" }).click();
  await page.getByRole("button", { name: "Abandon" }).click();
  expect(s.posts.map((p) => p.path)).toEqual(["abandon"]);
  // The badge, and the verdict box: an abandoned set has no gate either.
  await expect(page.getByText("abandoned", { exact: true })).toHaveCount(2);
  await expect(
    page.getByRole("button", { name: "Land all members" }),
  ).toBeDisabled();
});

test("a member's repository is never abbreviated away", async ({ page }) => {
  // Seen in a manual pass over a changeset across three repositories of
  // one family: the Gate column's sentence — "ok: all 1 changed path(s)
  // approved", two lines of it — took the width, and every row's
  // Repository cell read "payments-…". On the page whose whole job is
  // saying which repositories are in this review, the three members
  // were indistinguishable. The gate wraps and loses nothing; an
  // identifier that is clipped is gone.
  const repos = ["payments-api-gateway", "payments-worker", "payments-web"];
  const cs = changeset({
    members: repos.map((repo, i) => ({
      repo,
      change: change(repo, `c-${i}`, "tier the fee schedule for enterprise"),
    })),
    edges: [],
    order: repos.map((repo, i) => ({ repo, change: `c-${i}` })),
  });
  const v = verdict();
  await mockChangesets(page, {
    changesets: [cs],
    verdict: {
      ...v,
      members: repos.map((repo, i) => ({
        ...(v.members as Record<string, unknown>[])[0],
        repo,
        change: `c-${i}`,
        explanation: "ok: all 1 changed path(s) approved",
      })),
    },
  });
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/dashboard/changesets/rename-payments");

  const table = page.getByRole("table").filter({ hasText: "Repository" });
  await expect(table).toBeVisible();
  // Nothing in a Repository cell is out of sight: the column wraps
  // where it has to, and never ellipsises an identifier.
  const clipped = await table.evaluate((t) =>
    Array.from(t.querySelectorAll("tbody tr")).map((tr) => {
      const cell = tr.querySelector("td")!;
      return [cell.textContent, cell.scrollWidth - cell.clientWidth];
    }),
  );
  expect(clipped, "a repository name is being clipped").toEqual(
    repos.map((repo) => [repo, 0]),
  );
  // …and the three are told apart by sight, which is the point.
  for (const repo of repos)
    await expect(
      table.getByRole("link", { name: repo, exact: true }),
    ).toBeVisible();
  // The table still fits its card, which is what the Gate column's
  // width was buying.
  const overflow = await table.evaluate((t) => {
    const scroller = t.parentElement!;
    return scroller.scrollWidth - scroller.clientWidth;
  });
  expect(
    overflow,
    "the members table scrolls horizontally",
  ).toBeLessThanOrEqual(1);
});

test("a toast does not cover the decision rail", async ({ page }) => {
  // Seen in a manual pass: a "Deleted squad-…" toast, raised by a delete
  // on another screen, rendered on top of the approvals card in the
  // right-hand rail. The rail is where the decisions are; a toast is an
  // announcement about something that already happened, and the two
  // must not be in the same place. This drives a real toast — the
  // mailed "prove this address" link, which is redeemed wherever the
  // reader happens to be — over the changeset's Actions card.
  await mockChangesets(page);
  // The mailed link is redeemed for the account that is signed in, so
  // this one needs a `me` with a handle. A stored API token is a
  // session the app never asks `/v1/auth/me` about — it is right there
  // in localStorage — so this signs in the other way, with the cookie
  // session the boot probe answers.
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ json: { ...ME, handle: "ada" } }),
  );
  await page.addInitScript(() =>
    localStorage.setItem(
      "stratum-session",
      JSON.stringify({ org: "acme", token: "" }),
    ),
  );
  await page.route("**/v1/users/*/emails/verify", (r) =>
    r.fulfill({ json: { address: "ada@acme.test" } }),
  );
  await page.setViewportSize({ width: 1280, height: 600 });
  await page.goto("/dashboard/changesets/rename-payments#verify-email=tok123");
  const toast = page.getByText(/ada@acme\.test is proved/);
  await expect(toast).toBeVisible();

  // The whole rail, not just the Actions card at the top of it:
  // everything in this column is something somebody is deciding with,
  // and the toast that was reported landed on a card further down it.
  const rail = page.locator("aside").filter({ hasText: "Actions" });
  const [t, c] = await Promise.all([toast.boundingBox(), rail.boundingBox()]);
  expect(t, "the toast has a box").not.toBeNull();
  expect(c, "the rail has a box").not.toBeNull();
  // A fixed toast is measured in viewport coordinates and a laid-out
  // rail in the document's. They are the same coordinates only at the
  // top of the page, which is where this is asserted.
  expect(await page.evaluate(() => window.scrollY)).toBe(0);
  const overlaps =
    t!.x < c!.x + c!.width &&
    c!.x < t!.x + t!.width &&
    t!.y < c!.y + c!.height &&
    c!.y < t!.y + t!.height;
  expect(overlaps, "the toast is over the decision rail").toBe(false);
});

test("declaring a dependency sends the edge the server takes, and the picture follows the draft", async ({
  page,
}) => {
  const api = { repo: "api", change: "c-api" };
  const web = { repo: "web", change: "c-web" };
  const docs = { repo: "docs", change: "c-docs" };
  const s = await mockChangesets(page, {
    changesets: [
      changeset({
        members: [
          { repo: "api", change: change("api", "c-api", "rename the service") },
          {
            repo: "web",
            change: change("web", "c-web", "point web at billing"),
          },
          { repo: "docs", change: change("docs", "c-docs", "document it") },
        ],
        edges: [],
        order: [api, web, docs],
      }),
    ],
  });
  await page.goto("/dashboard/changesets/rename-payments");
  // The empty state is the common case, and it is where a reader learns
  // that the order can be said at all.
  await expect(
    page.getByText(/No declared dependencies — these land in the order/),
  ).toBeVisible();

  await page.getByRole("button", { name: "Edit order" }).click();
  await page.getByRole("button", { name: "Add dependency" }).click();
  await page
    .getByRole("combobox", { name: "Dependency 1: lands first" })
    .click();
  await page.getByRole("option", { name: "web/c-web" }).click();
  await page
    .getByRole("combobox", { name: "Dependency 1: lands after" })
    .click();
  await page.getByRole("option", { name: "docs/c-docs" }).click();

  // Before saving: the strip is already the new plan. `docs` moved into
  // a second wave, and because the wave it follows also holds `api` —
  // which it does not depend on — the chip says which member it is
  // actually waiting for.
  const strip = page.locator("section", { hasText: "Landing order" }).first();
  await expect(strip).toContainText("then (wave 2)");
  await expect(strip.getByText("after: web/c-web")).toBeVisible();
  expect(s.posts, "the draft was written before Save").toEqual([]);

  await page.getByRole("button", { name: "Save order" }).click();
  expect(s.posts).toEqual([
    { path: "edges", body: { edges: [{ from: web, to: docs }] } },
  ]);
  // The editor closes onto the saved plan, read back from the server.
  await expect(page.getByRole("button", { name: "Edit order" })).toBeVisible();
  await expect(strip.getByText("after: web/c-web")).toBeVisible();
});

test("a cycle is refused in the server's words, without a request", async ({
  page,
}) => {
  // The mirror of `check_edges` exists so this refusal costs nothing and
  // reads the same as the server's would. A form that has to round-trip
  // to learn it is a form that shows a red banner over a plan somebody
  // can already see is wrong.
  const s = await mockChangesets(page);
  await page.goto("/dashboard/changesets/rename-payments");
  await page.getByRole("button", { name: "Edit order" }).click();
  await page.getByRole("button", { name: "Add dependency" }).click();
  // The saved edge is api → web; this second row closes the loop.
  await page
    .getByRole("combobox", { name: "Dependency 2: lands first" })
    .click();
  await page.getByRole("option", { name: "web/c-web" }).click();
  await page
    .getByRole("combobox", { name: "Dependency 2: lands after" })
    .click();
  await page.getByRole("option", { name: "api/c-api" }).click();

  await expect(
    page.getByText("edges form a cycle through api/c-api, web/c-web"),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Save order" })).toBeDisabled();
  expect(s.posts, "a cycle was sent to the server").toEqual([]);

  // Removing the row that closed the loop makes it saveable again.
  await page.getByRole("button", { name: "Remove dependency 2" }).click();
  await expect(page.getByRole("button", { name: "Save order" })).toBeEnabled();
});

test("a refused edge write is the gate speaking, not an error toast", async ({
  page,
}) => {
  // `PUT …/edges` refuses a changeset that stopped being open under
  // somebody's hands. That is the same kind of answer a refused landing
  // gives — the server's own sentence, where the form is — and it is
  // rendered as text, not thrown at the corner of the screen.
  await mockChangesets(page, {
    edges: {
      status: 409,
      body: { error: 'changeset "rename-payments" is not open' },
    },
  });
  await page.goto("/dashboard/changesets/rename-payments");
  await page.getByRole("button", { name: "Edit order" }).click();
  await page.getByRole("button", { name: "Add dependency" }).click();
  await page.getByRole("button", { name: "Save order" }).click();
  await expect(
    page.getByText('changeset "rename-payments" is not open'),
  ).toBeVisible();
  // In the server's words and nothing else: wrapped in "Could not save
  // the order: …" it would read as this page having failed, when what
  // happened is that the set stopped being open.
  await expect(page.getByText(/Could not save the order/)).toHaveCount(0);
  // The editor is still open, with the draft that was refused still in
  // it: the answer is beside the thing it is about.
  await expect(page.getByRole("button", { name: "Save order" })).toBeVisible();
});

test.describe("on the forge", () => {
  // A changeset lived only at `/dashboard/changesets/{key}` — behind
  // sign-in, and unreadable by a non-member — while a single change had
  // `/{owner}/{repo}/changes/{key}`, a real public forge address. That
  // was an inconsistency rather than a gap: the reads behind the page
  // already admit an anonymous caller over public repositories, and the
  // git front door had been serving `/{org}/changesets/{key}` over HTTP
  // and SSH the whole time. Only the human address was missing.
  //
  // FORGE-UX §6a and §0 say what the public page must be: the same page.
  // Signing in adds actions and never changes what it says. So these
  // tests assert *parity* — the same sentences a member reads, in the
  // same order — and then the absence of every control a member gets.

  test("a stranger reads the verdict, the members and the order, unchanged", async ({
    page,
  }) => {
    await anonymousChangesets(page, {
      changesets: [changeset({ viewer_write: false })],
    });
    await page.goto("/acme/changesets/rename-payments");

    await expect(
      page.getByRole("heading", { name: "Rename the payments service" }),
    ).toBeVisible();
    // Not the shell's not-found. A route that fell through to the repo
    // arm renders `acme/changesets`, a repository nobody can own, which
    // looks almost like a page and is the failure this exists to catch.
    await expect(page.getByText(/We couldn't find/)).toHaveCount(0);

    // The composed gate, in the server's own words — the same sentence
    // the signed-in test above asserts, character for character.
    await expect(page.getByText("every member is ready to land")).toBeVisible();

    // Members in landing order: `members` lists web first, `order` puts
    // api first, and a stranger sees the plan the lander will walk.
    const rows = page.getByRole("row").filter({ hasText: /c-(api|web)/ });
    await expect(rows).toHaveCount(2);
    await expect(rows.nth(0)).toContainText("c-api");
    await expect(rows.nth(1)).toContainText("c-web");
    await expect(rows.nth(0)).toContainText(
      "approved by an owner of every path",
    );

    // The order strip, waves and all.
    const strip = page.locator("section", { hasText: "Landing order" }).first();
    await expect(strip).toContainText("lands first");
    await expect(strip).toContainText("then (wave 2)");

    // The composed CI rows, named by repository as well as job.
    await expect(page.getByText("web: ci / build")).toBeVisible();

    // The workspace clone block, still drawn: per §6a the clone URL of a
    // public set is a public fact, and hiding it would make the page say
    // something different to a stranger.
    await expect(page.getByLabel("HTTPS clone URL")).toHaveValue(
      "http://127.0.0.1:8080/acme/changesets/rename-payments.git",
    );
    await expect(page.getByText(/--recurse-submodules/)).toBeVisible();

    // The forge shell, not the dashboard's. The rail's trigger is the
    // cheapest proof: it exists on every dashboard page and on none of
    // these, and a changeset quietly rendered inside the admin shell
    // would look almost right to a member and be a sign-in wall to
    // everybody else.
    await expect(
      page.getByRole("button", { name: "Toggle sidebar" }),
    ).toHaveCount(0);
    // And no way back to a list that does not exist here: the org-wide
    // list is per-caller-filtered, so there is deliberately no public
    // one, and a back link to a 404 is worse than no back link.
    await expect(page.getByText("← All changesets")).toHaveCount(0);
  });

  test("a stranger gets no writer control at all, only what one would take", async ({
    page,
  }) => {
    await anonymousChangesets(page, {
      changesets: [changeset({ viewer_write: false })],
    });
    await page.goto("/acme/changesets/rename-payments");
    await expect(
      page.getByRole("heading", { name: "Rename the payments service" }),
    ).toBeVisible();

    // Absent from the DOM, not merely disabled. Every write route masks
    // a caller who may not write with the same `no changeset "…"` a
    // stranger gets for one that does not exist, so a drawn-but-disabled
    // button would be a control that answers a lie when pressed.
    for (const name of [
      "Land all members",
      "Revert…",
      "Abandon",
      "Add member",
      "Edit order",
    ])
      await expect(page.getByRole("button", { name })).toHaveCount(0);
    await expect(page.getByRole("button", { name: /^Remove / })).toHaveCount(0);

    // The wording a signed-in reader without write access already gets —
    // the same sentence, not a second one for strangers.
    await expect(
      page.getByText(
        "Landing, reverting or abandoning this changeset takes write access to every member repository.",
      ),
    ).toBeVisible();
    // Plus the way in, which is an action added rather than a sentence
    // changed. It carries `next`, so signing in returns here.
    await expect(
      page
        .locator("div", { hasText: "Actions" })
        .getByRole("link", { name: "Sign in" })
        .first(),
    ).toHaveAttribute(
      "href",
      "/login?next=%2Facme%2Fchangesets%2Frename-payments",
    );
  });

  test("a member's links stay on the forge rather than the dashboard", async ({
    page,
  }) => {
    await anonymousChangesets(page, {
      changesets: [changeset({ viewer_write: false })],
    });
    await page.goto("/acme/changesets/rename-payments");
    const rows = page.getByRole("row").filter({ hasText: /c-(api|web)/ });
    await expect(rows.nth(0)).toContainText("c-api");

    // The forge address of the member's change, on both the row's key
    // and the order strip's chip. Approval happens there, under that
    // repository's own OWNERS, and never on this page.
    await expect(
      rows.nth(0).getByRole("link", { name: "c-api" }),
    ).toHaveAttribute("href", "/acme/api/changes/c-api");
    // The repository cell goes to the repository on *this* mount, not to
    // the dashboard's private code browser — which is where it pointed
    // when the only mount was the dashboard's.
    await expect(
      rows.nth(0).getByRole("link", { name: "api", exact: true }),
    ).toHaveAttribute("href", "/acme/api");
  });
});

// ---------------------------------------------------------------------
// The combined cross-repo diff
//
// Reading a four-repo set used to mean four page loads, and the
// changeset page showed no code at all. These drive the section that
// closed that gap, and three of them exist because of a specific way
// this surface can be wrong rather than because of a feature: two
// members' `README.md` are the same six characters, a viewed tick is
// addressed by `(repo, change)` and could be sent to the wrong pair, and
// the members of one set are changes in repositories with different
// ACLs.

/// A file row in one member's tree. Each member's rail is a tree in a
/// region named for the repository, and a row's own name is the file's
/// segment — so `web`'s README is reached through the region, which is
/// the whole point of grouping by member.
const memberFile = (files: Locator, repo: string, name: string) =>
  files
    .getByRole("group", { name: `${repo} files` })
    .getByRole("treeitem", { name, exact: true });

test("the whole set's files read in one place, and the open file's header names its repository", async ({
  page,
}) => {
  await mockChangesets(page);
  await page.goto("/dashboard/changesets/rename-payments");
  const files = page.getByRole("region", {
    name: /Files across 2 repositories/,
  });
  await expect(
    page.getByRole("heading", { name: "Files across 2 repositories" }),
  ).toBeVisible();
  // Three files over two members, none of them read yet. The count is
  // the whole set's, which is the point of the section.
  await expect(
    files.getByText("3 files · 0 of 3 viewed", { exact: true }),
  ).toBeVisible();

  // Groups, in landing order — `api` first, because `web` lands after
  // it — and never a flat path-sorted list, in which the two
  // `README.md`s would sit adjacent.
  const groups = files.getByRole("button", { expanded: true });
  await expect(groups).toHaveCount(2);
  await expect(groups.first()).toHaveAccessibleName(
    "api — c-api, patchset 2, 0 of 2 viewed",
  );
  await expect(groups.nth(1)).toHaveAccessibleName(
    "web — c-web, patchset 2, 0 of 1 viewed",
  );

  // Open the `README.md` of the SECOND member. Both members have one,
  // so the assertion that matters is which repository the body says it
  // is showing.
  await memberFile(files, "web", "README.md").click();
  const header = (repo: string) =>
    files.locator("div", { hasText: new RegExp(`^${repo} · README\\.md$`) });
  await expect(header("web").last()).toBeVisible();
  // …and it is not the other member's file of the same name.
  await expect(header("api")).toHaveCount(0);
  await expect(memberFile(files, "web", "README.md")).toHaveAttribute(
    "aria-selected",
    "true",
  );

  // The filter matches on `repo/path`, so a repository name narrows to
  // a repository and the rail shortens.
  await page.getByLabel("Filter files by repository or path").fill("api/");
  await expect(
    files.getByText("2 files · 0 of 2 viewed", { exact: true }),
  ).toBeVisible();
  await expect(memberFile(files, "api", "pay.rs")).toBeVisible();
  await expect(memberFile(files, "web", "README.md")).toHaveCount(0);
});

test("a viewed tick is written against that member's own repository and change", async ({
  page,
}) => {
  const s = await mockChangesets(page);
  await page.goto("/dashboard/changesets/rename-payments");
  const files = page.getByRole("region", {
    name: /Files across 2 repositories/,
  });
  await memberFile(files, "web", "README.md").click();
  await files.getByLabel("Viewed web/README.md").check();

  // The URL and the body, not the tick's appearance. Sending one
  // member's mark to another is the defect this surface most invites,
  // and it is invisible to anything that only looks at the rendering.
  await expect.poll(() => s.viewPuts.length).toBe(1);
  expect(s.viewPuts[0].url).toContain(
    "/v1/orgs/acme/repos/web/changes/c-web/views",
  );
  expect(s.viewPuts[0].body).toEqual({ path: "README.md", viewed: true });

  // The other member is untouched: `api/README.md` is the same six
  // characters and is still unread.
  expect(s.views).toEqual({ "web/c-web": ["README.md"] });
  await expect(
    files.getByText("3 files · 1 of 3 viewed", { exact: true }),
  ).toBeVisible();
});

test("a group whose files are all viewed collapses, and the count does not shrink with it", async ({
  page,
}) => {
  await mockChangesets(page);
  await page.goto("/dashboard/changesets/rename-payments");
  const files = page.getByRole("region", {
    name: /Files across 2 repositories/,
  });
  const web = files.getByRole("button", { name: /^web — c-web/ });
  await expect(web).toHaveAttribute("aria-expanded", "true");

  await memberFile(files, "web", "README.md").click();
  await files.getByLabel("Viewed web/README.md").check();

  // The rail shortens as you work — the single thing that makes sixteen
  // members tractable.
  await expect(web).toHaveAttribute("aria-expanded", "false");
  await expect(memberFile(files, "web", "README.md")).toHaveCount(0);
  // The count is over everything the filter matched, never over what is
  // left after hiding. A progress number that shrank as you made
  // progress would be worse than none.
  await expect(
    files.getByText("3 files · 1 of 3 viewed", { exact: true }),
  ).toBeVisible();
  // `api` is untouched and still open.
  await expect(
    files.getByRole("button", { name: /^api — c-api/ }),
  ).toHaveAttribute("aria-expanded", "true");

  // And the reader can open it back up: auto-collapse is a default, not
  // a decision taken away from them.
  await web.click();
  await expect(web).toHaveAttribute("aria-expanded", "true");
});

test("one unreadable member degrades to that group saying so, with a Retry", async ({
  page,
}) => {
  // Members have different ACLs, so this is an ordinary state and not an
  // error: the fan-out is `Promise.allSettled`, and "the page is broken"
  // and "one repository is" are different sentences.
  const state = await mockChangesets(page, { refuseDiff: new Set(["api"]) });
  await page.goto("/dashboard/changesets/rename-payments");
  const files = page.getByRole("region", {
    name: /Files across 2 repositories/,
  });
  // Wait for the section, then for the refusal — two observables, not one
  // budget covering both. This assertion used to go straight at the
  // refusal, so its five seconds had to cover the changeset load *and* a
  // per-member diff fan-out; it passed here and failed on a two-core CI
  // runner, which is the "passes here, fails there" class this repo pins
  // `workers: 1` to avoid. The sibling test above waits on the heading
  // first for the same reason.
  await expect(
    page.getByRole("heading", { name: "Files across 2 repositories" }),
  ).toBeVisible();
  // The fan-out is a request per member, so it gets its own budget rather
  // than sharing the default with everything before it.
  await expect(files.getByText(/api could not be read/)).toBeVisible({
    timeout: 15000,
  });
  // The other member rendered anyway.
  await expect(memberFile(files, "web", "README.md")).toBeVisible();
  await expect(
    files.getByText("1 file · 0 of 1 viewed", { exact: true }),
  ).toBeVisible();

  // The store recovers, then the reader asks again. Saying so explicitly
  // is what makes the refusal above hold for as long as the assertion
  // needs it.
  state.refuseDiff.delete("api");
  await files.getByRole("button", { name: "Retry api" }).click();
  await expect(memberFile(files, "api", "pay.rs")).toBeVisible();
  await expect(
    files.getByText("3 files · 0 of 3 viewed", { exact: true }),
  ).toBeVisible();
});

test("a member with no patchset says there is nothing to read yet, and the set still reads", async ({
  page,
}) => {
  const cs = changeset();
  (
    cs.members as { repo: string; change: Record<string, unknown> }[]
  )[1].change.patchset = null;
  await mockChangesets(page, { changesets: [cs], diffstat: 404 });
  await page.goto("/dashboard/changesets/rename-payments");
  const files = page.getByRole("region", {
    name: /Files across 2 repositories/,
  });
  await expect(
    files.getByText("No patchset yet — nothing to read here."),
  ).toBeVisible();
  await expect(memberFile(files, "web", "README.md")).toBeVisible();
  await expect(
    files.getByText("1 file · 0 of 1 viewed", { exact: true }),
  ).toBeVisible();
  // The server has no count for a set one of whose members has no
  // patchset — it answers 404 — and that is no number, not an error.
  await expect(files.getByText(/added, .* removed/)).toHaveCount(0);
  await expect(page.getByRole("alert")).toHaveCount(0);
});

test("the set carries its size, per member and in total", async ({
  page,
}) => {
  // The numbers are the server's — `…/diffstat` counts every member's
  // latest patchset — never something invented off a file list. The
  // total sits beside the file count with its meaning in words; each
  // member's is a decoration on its group, outside the group's button so
  // the button's name stays the sentence the other tests read.
  await mockChangesets(page);
  await page.goto("/dashboard/changesets/rename-payments");
  const files = page.getByRole("region", {
    name: /Files across 2 repositories/,
  });
  await expect(
    files.getByText("3 files · 0 of 3 viewed", { exact: true }),
  ).toBeVisible();
  await expect(files.getByText("5 added, 2 removed")).toHaveCount(1);
  await expect(files.getByText("(some files not counted)")).toHaveCount(0);

  // `has` is resolved inside each candidate, so the inner locator is
  // the page's, not the region's.
  const group = (repo: string) =>
    files.locator("li").filter({
      has: page.getByRole("button", { name: new RegExp(`^${repo} — `) }),
    });
  await expect(group("api")).toContainText("+3");
  await expect(group("api")).toContainText("−1");
  await expect(group("web")).toContainText("+2");
  await expect(group("web")).toContainText("−1");
  await expect(files.getByRole("button", { name: /^api — c-api/ })).toHaveAccessibleName(
    "api — c-api, patchset 2, 0 of 2 viewed",
  );
});
