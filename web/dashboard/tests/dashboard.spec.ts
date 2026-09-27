// Dashboard UI e2e against the production build with a mocked control-plane
// API — hermetic, exercises both ways of signing in, the org overview,
// incident mode, repo metrics, the chart tooltip, and the settings area
// where people and credentials are managed.

import { expect, test, type Page } from "@playwright/test";

import {
  ACCESS,
  AUDIT,
  AUDIT_TOTAL,
  INVITES,
  KEYS,
  ME,
  MEMBERS,
  METRICS,
  REPOS,
  TEAMS,
  TOKENS,
  USAGE,
  mockApi,
  signIn,
  signInAsPerson,
} from "./fixtures";

/// Open a repository from the dashboard's list.
///
/// The row is a link now, and it goes to the repository's one address —
/// `/{owner}/{repo}` — rather than to a dashboard-only screen that had
/// no address at all. Waiting for the URL rather than for a rendered
/// element waits on the thing every assertion after it depends on.
async function openRepo(page: Page, name: string) {
  await page.getByRole("link", { name, exact: true }).click();
  await page.waitForURL(`**/acme/${name}`);
}

/// A tab on the repository page.
///
/// Links, not `role="tab"`: `components/tab-strip.tsx` makes every tab a
/// real address on purpose, so that a reader can send one. Scoped to the
/// strip's own nav, because "Code" and "Settings" are words that appear
/// elsewhere on the page.
function repoTab(page: Page, name: string) {
  return page
    .getByRole("navigation", { name: "Repository" })
    .getByRole("link", { name, exact: true });
}

test("login rejects bad credentials with a visible error", async ({ page }) => {
  // This test mocks the two endpoints it needs rather than going
  // through `mockApi`, so it does not inherit that helper's catch-all.
  // Registered first, before the specific routes below.
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/auth/login", (r) =>
    r.fulfill({ status: 401, json: { error: "invalid email or password" } }),
  );
  await page.goto("/dashboard/");
  await page.getByLabel("Email").fill("owner@acme.test");
  await page.getByLabel("Password").fill("wrong");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("Could not sign in");

  // …and the same for the token path.
  await page.route("**/v1/orgs/acme/repos?limit=200", (r) =>
    r.fulfill({ status: 401, json: { error: "unauthorized" } }),
  );
  await page
    .getByRole("button", { name: "Sign in with an API token instead" })
    .click();
  await page.getByPlaceholder("acme").fill("acme");
  await page.getByPlaceholder("weft_…").fill("weft_bad");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("Could not sign in");
});

/// Signing in with a password must never leave a credential where a
/// script can reach it. This is the whole reason the session moved to an
/// HttpOnly cookie.
test("a password sign-in leaves no credential in browser storage", async ({
  page,
}) => {
  await signInAsPerson(page);
  const stored = await page.evaluate(() => JSON.stringify(localStorage));
  expect(stored).not.toContain("a long enough password");
  expect(stored).not.toContain("weft_");
  // Reloading restores the session from the cookie, via the server.
  await mockApi(page);
  await page.reload();
  await expect(page.getByText("Requests today")).toBeVisible();
  await expect(page.getByText("Ada Owner")).toBeVisible();
});

test("org overview shows stats, incident mode, and the repo table", async ({
  page,
}) => {
  await signIn(page);
  await expect(page.getByText("Requests today")).toBeVisible();
  await expect(page.getByText("420", { exact: true })).toBeVisible();
  // Incident mode lists the broken mirror with its error.
  await expect(page.getByText("Origin incident mode")).toBeVisible();
  await expect(
    page.getByText("origin unreachable: connection refused"),
  ).toBeVisible();
  // Healthy + broken badges both render.
  await expect(page.getByText("healthy").first()).toBeVisible();
  await expect(page.getByText("origin unreachable").first()).toBeVisible();
  // Usage chart tooltip on hover.
  const chart = page.locator("svg[role=img]");
  await expect(chart).toBeVisible();
  await chart.hover({ position: { x: 500, y: 90 } });
  await expect(page.getByTestId("chart-tooltip")).toBeVisible();
});

test("the repo table says whose history a fork is carrying", async ({
  page,
}) => {
  // The forge page has said "Forked from" under a fork's name since
  // forks existed; the organization's own repository table did not, so
  // a fork of somebody else's project listed exactly like an original.
  await signIn(page);
  await page.route("**/v1/orgs/acme/repos?limit=200", (r) =>
    r.fulfill({
      json: {
        repos: [
          {
            ...REPOS.repos[2],
            name: "widget",
            description: "the fast one",
            fork_parent: "upstream/widget",
            fork_state: "ready",
          },
          REPOS.repos[2],
        ],
      },
    }),
  );
  await page.goto("/dashboard/");
  const rows = page.getByRole("row").filter({ hasText: /widget|session-1/ });
  await expect(rows).toHaveCount(2);
  await expect(rows.filter({ hasText: "widget" })).toContainText(
    "Forked from upstream/widget",
  );
  await expect(rows.filter({ hasText: "session-1" })).not.toContainText(
    "Forked from",
  );
});

/// The complaint this whole change answers: you look at a repository and
/// cannot share the link.
///
/// The row used to be a `<TableRow onClick>` that set a `useState`, so
/// there was no URL at all — nothing to copy, and a reload lost the
/// repo. Asserting the `href` rather than only the navigation is the
/// point: a click handler alone would pass a "does clicking work" test
/// and still leave the row uncopyable, unmiddle-clickable and invisible
/// to anything that reads links.
test("a repository in the list is a link to its one address", async ({
  page,
}) => {
  await signIn(page);
  const row = page.getByRole("link", { name: "widget", exact: true });
  await expect(row).toHaveAttribute("href", "/acme/widget");
  await row.click();
  await expect(page).toHaveURL(/\/acme\/widget$/);
  // And it is the forge page, not a dashboard rendering of one: the
  // repository's own tab strip is what says so.
  await expect(
    page.getByRole("navigation", { name: "Repository" }),
  ).toBeVisible();
});

/// A repository has one address, and it is not under `/dashboard`.
///
/// `/dashboard/repos/{name}` was that second address and is gone. The
/// server answers `/dashboard/*` with the SPA shell whatever the path
/// is, so only the client can tell somebody the address is dead — and
/// before this it did not: every unrecognised dashboard path fell
/// through to the org overview, which is a different page rendered
/// under a URL that promised this one.
test("a dead dashboard address says so rather than showing another page", async ({
  page,
}) => {
  await signIn(page);
  for (const dead of ["/dashboard/repos/widget", "/dashboard/nonsense"]) {
    await page.goto(dead);
    await expect(page.getByText(/We couldn’t find/)).toBeVisible();
    // The tell for the old behaviour: the org overview's tiles. A test
    // that only asserted the 404 copy would pass against a page that
    // rendered both.
    await expect(page.getByText("Requests today")).toHaveCount(0);
  }
});

test("insights shows percentile tiles, kind table, and CSV export", async ({
  page,
}) => {
  await signIn(page);
  // These numbers used to be the dashboard's repo screen, which had no
  // address; they are a tab on the repository's own page now, offered
  // only to somebody `viewer_admin` says may administer it.
  await openRepo(page, "widget");
  await repoTab(page, "Insights").click();
  await expect(page.getByText("Clone p50 / p99")).toBeVisible();
  await expect(page.getByText("512 ms / 2.0 s")).toBeVisible();
  await expect(page.getByText("Requests absorbed")).toBeVisible();
  // Kind table rows.
  await expect(page.locator("td", { hasText: "clone" }).first()).toBeVisible();
  await expect(page.locator("td", { hasText: "fetch" }).first()).toBeVisible();
  // CSV export must carry the token. It used to be a plain <a href
  // download>, which a browser follows as a navigation with NO headers —
  // so against a real server the user got a 401 page instead of a file.
  // Asserting the href could never catch that; asserting the request can.
  let csvAuth: string | undefined;
  await page.route("**/metrics?format=csv", async (route) => {
    csvAuth = route.request().headers()["authorization"];
    await route.fulfill({
      status: 200,
      contentType: "text/csv",
      body: "kind,count,bytes,ms_sum,p50_ms,p99_ms\nclone,12,4096,120,10,20\n",
    });
  });
  const download = page.waitForEvent("download");
  await page.getByRole("button", { name: "Export CSV" }).click();
  const file = await download;
  expect(csvAuth).toBe("Bearer weft_test_token");
  expect(file.suggestedFilename()).toBe("widget-metrics.csv");
});

test("mirror-only chrome is hidden on a native repo", async ({ page }) => {
  await signIn(page);
  // `widget` is a mirror: the sync badge belongs beside its name, where
  // a reader who is about to clone a stale mirror actually sees it —
  // not on a tab only an admin is offered — and the freshness lag
  // belongs among its numbers.
  await openRepo(page, "widget");
  await expect(
    page.getByText(/healthy|not synced yet|origin unreachable/i).first(),
  ).toBeVisible();
  await repoTab(page, "Insights").click();
  await expect(page.getByText("Freshness lag p50")).toBeVisible();

  // `session-1` is native: it has no origin, so "not synced yet" and a
  // freshness lag are not merely empty, they are meaningless — and read
  // as "something is wrong" to anyone using the Repos SKU.
  await page.goto("/acme/session-1");
  await expect(
    page.getByText(/healthy|not synced yet|origin unreachable/i),
  ).toHaveCount(0);
  await repoTab(page, "Insights").click();
  await expect(page.getByText("Clone p50 / p99")).toBeVisible();
  await expect(page.getByText("Freshness lag p50")).toHaveCount(0);
  await expect(page.getByText("not synced yet")).toHaveCount(0);
  // It gets its own fourth tile instead of a ragged three-up grid.
  await expect(page.getByText("Fetch p50 / p99")).toBeVisible();
});

test("session persists across reloads and signs out cleanly", async ({
  page,
}) => {
  await signIn(page);
  await expect(page.getByText("Requests today")).toBeVisible();
  await mockApi(page);
  await page.reload();
  await expect(page.getByText("Requests today")).toBeVisible();
  await page.getByRole("button", { name: "Sign out" }).click();
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();
});

test("the repo page offers both clone URLs; ssh hidden when unconfigured", async ({
  page,
}) => {
  await signIn(page);
  // Behind the Code button, where a hand goes looking for it. It used to
  // be on the dashboard's repo screen, at no address of its own.
  await openRepo(page, "widget");
  await page.getByRole("button", { name: "Code" }).click();
  // By role, not `getByLabel`: the copy button beside each box is
  // labelled "Copy the HTTPS clone URL", and an accessible name is
  // matched as a substring, so the plain label resolves to two elements.
  const https = page.getByRole("textbox", { name: "HTTPS clone URL" });
  const ssh = page.getByRole("textbox", { name: "SSH clone URL" });
  await expect(https).toHaveValue("https://x/acme/widget.git");
  // One box, two transports: SSH is behind its own toggle rather than a
  // second field, which is how GitHub's Code button reads.
  await page.getByRole("button", { name: "SSH", exact: true }).click();
  await expect(ssh).toHaveValue("ssh://git@x:22/acme/widget.git");

  // A deployment without an SSH endpoint shows HTTPS only — and not a
  // disabled toggle, which would tell somebody a way in exists that
  // does not.
  await page.goto("/acme/session-1");
  await page.getByRole("button", { name: "Code" }).click();
  await expect(https).toHaveValue("https://x/acme/session-1.git");
  await expect(page.getByRole("button", { name: "SSH", exact: true })).toHaveCount(
    0,
  );
});

test("ssh keys panel lists, adds, and revokes keys", async ({ page }) => {
  await signIn(page);
  await page.getByRole("link", { name: "SSH keys" }).click();
  await expect(page.getByText("No SSH keys yet.")).toHaveCount(0);
  // Seeded key with its truncated fingerprint and status.
  await expect(page.getByText("laptop")).toBeVisible();
  await expect(page.getByText("SHA256:xyrwjwNK…N9iI")).toBeVisible();
  await expect(page.getByText("active", { exact: true })).toBeVisible();

  // Add a key.
  await page
    .getByLabel("Public key")
    .fill("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG5ld2tleQ== me@new");
  await page.getByLabel("Key label").fill("desktop");
  await page.getByRole("button", { name: "Add key" }).click();
  await expect(page.getByText("desktop")).toBeVisible();

  // Revoke the first key. Revoked credentials are history, not
  // inventory: the row leaves the table and is reachable behind a count.
  // Revocation is irreversible, so a confirmation dialog stands in front.
  await page.getByRole("button", { name: "Revoke" }).first().click();
  await page
    .getByRole("alertdialog")
    .getByRole("button", { name: "Revoke" })
    .click();
  await expect(
    page.getByRole("button", { name: "Show 1 revoked" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Show 1 revoked" }).click();
  await expect(page.getByText("revoked", { exact: true })).toBeVisible();
});

test("settings manages members, invitations and tokens", async ({ page }) => {
  await signInAsPerson(page);
  await page.getByRole("link", { name: "Members" }).click();

  // Members: the roster, with a role control per person and no way to
  // remove yourself by accident.
  await expect(page.getByText("dev@acme.test")).toBeVisible();
  await expect(page.getByLabel("Role for dev@acme.test")).toContainText(
    "member",
  );
  let patched: string | undefined;
  await page.route("**/v1/orgs/acme/members/01user2", async (route) => {
    patched = JSON.stringify(route.request().postDataJSON());
    await route.fulfill({ status: 204, body: "" });
  });
  await page.getByLabel("Role for dev@acme.test").click();
  await page.getByRole("option", { name: "admin" }).click();
  await expect.poll(() => patched).toBe('{"role":"admin"}');

  // Every row's button reads "Remove"; the accessible name has to say
  // whom, or a screen reader announces a column of identical controls.
  await expect(page.getByLabel("Remove dev@acme.test")).toBeVisible();

  // Invitations: emailed, and the link is also shown once so an admin
  // can deliver it themselves. Which of the two happened has to be
  // visible — an admin who cannot tell will send it twice or not at all.
  await expect(page.getByText("pending@acme.test")).toBeVisible();
  await page.getByLabel("Invite email").fill("new@acme.test");
  await page.getByLabel("Invite role").click();
  await page.getByRole("option", { name: "member" }).click();
  await page.getByRole("button", { name: "Invite" }).click();
  await expect(page.getByText("stinv_01inv2_secretsecret")).toBeVisible();
  await expect(page.getByText(/^Emailed\./)).toBeVisible();

  // Tokens: minted plaintext is shown once and never again.
  await page.getByRole("link", { name: "Tokens" }).click();
  await expect(page.getByText("laptop")).toBeVisible();
  await page.getByLabel("Token label").fill("ci");
  await page.getByRole("button", { name: "Mint token" }).click();
  await expect(page.getByText("weft_01tok9_freshsecret")).toBeVisible();
  await expect(page.getByText(/cannot be shown again/)).toBeVisible();
});

test("a viewer sees their own credentials but not the org's people", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({
      json: { ...ME, orgs: [{ id: "01org", name: "acme", role: "viewer" }] },
    }),
  );
  await page.goto("/dashboard/");
  // A viewer may put only these on a token; the server says so, and the
  // dashboard offers exactly that rather than keeping its own copy.
  await page.route("**/v1/orgs/acme/tokens", (r) =>
    r.fulfill({
      json: { tokens: [], mintable_scopes: ["org:read", "repo:read"] },
    }),
  );
  // Members and invitations need org:admin — offering links that could
  // only ever answer 404 would be worse than not offering them.
  await expect(page.getByRole("link", { name: "Members" })).toHaveCount(0);
  await expect(page.getByRole("link", { name: "Tokens" })).toBeVisible();
  await expect(page.getByRole("link", { name: "SSH keys" })).toBeVisible();
  await expect(page.getByRole("link", { name: "Password" })).toBeVisible();

  // …and is offered only the scopes their role can actually carry. A
  // checkbox the server would refuse is a trap: the refusal is correct,
  // but they only find out after filling the form in.
  await page.getByRole("link", { name: "Tokens" }).click();
  const offered = page.locator("form").getByRole("checkbox");
  await expect(offered).toHaveCount(2);
  await expect(
    page.locator("form").getByText("repo:read", { exact: true }),
  ).toBeVisible();
  await expect(
    page.locator("form").getByText("repo:write", { exact: true }),
  ).toHaveCount(0);
  await expect(
    page.locator("form").getByText("org:admin", { exact: true }),
  ).toHaveCount(0);
});

test("changing a password reports success and its consequence", async ({
  page,
}) => {
  await signInAsPerson(page);
  await page.getByRole("link", { name: "Password" }).click();
  await expect(page.getByText(/signs out every other session/)).toBeVisible();

  let sent: string | undefined;
  await page.route("**/v1/auth/password", async (route) => {
    sent = JSON.stringify(route.request().postDataJSON());
    await route.fulfill({ status: 204, body: "" });
  });
  await page.getByLabel("Current password").fill("a long enough password");
  await page.getByLabel("New password").fill("a brand new password");
  await page.getByRole("button", { name: "Change password" }).click();
  await expect(page.getByRole("status")).toContainText("Password changed");
  expect(sent).toBe(
    '{"current_password":"a long enough password","new_password":"a brand new password"}',
  );
});

/// The activity feed: newest first, filtered, paged and exportable.
///
/// Order is the property worth a test. The API stores and ships the trail
/// oldest-first — that is what the log shipper wants — so a feed that took
/// the default would open on the hundred oldest events the org ever
/// recorded and never show what just happened. The panel has to ask for
/// `order=desc` and page with `before`, and this asserts the query string,
/// not just the rendering.
test("activity reads newest first, filters, pages and exports", async ({
  page,
}) => {
  const queries: URLSearchParams[] = [];
  await signIn(page, queries);
  await page.getByRole("link", { name: "Activity" }).click();

  // Newest first, and it said so.
  await expect(page.getByRole("cell", { name: "token.mint" })).toBeVisible();
  const first = queries.at(-1)!;
  expect(first.get("order")).toBe("desc");
  expect(first.get("limit")).toBe("100");
  expect(first.get("before")).toBeNull();
  const rows = page.locator("tbody tr");
  await expect(rows).toHaveCount(100);
  await expect(rows.first()).toContainText("token.mint");
  // The trail stores a repo id; the column has to say which repo that is.
  await expect(rows.first()).toContainText("widget");

  // A full page offers more, and asks for it from the older end.
  await page.getByRole("button", { name: "Load more" }).click();
  await expect(rows).toHaveCount(AUDIT_TOTAL);
  expect(queries.at(-1)!.get("before")).toBe("6");
  // The next page came back short, so there is nothing left to offer.
  await expect(page.getByRole("button", { name: "Load more" })).toHaveCount(0);

  // Filtering by person narrows the query, not just the table.
  await page.getByLabel("Filter by person").click();
  await page.getByRole("option", { name: "dev@acme.test" }).click();
  await expect.poll(() => queries.at(-1)!.get("user")).toBe("01user2");
  await expect(page.getByRole("cell", { name: "token.mint" })).toHaveCount(0);

  // …and by action, which here matches nothing that person did.
  await page.getByLabel("Filter by action").fill("token.mint");
  await expect(page.getByText("Nothing matches those filters.")).toBeVisible();
  expect(queries.at(-1)!.get("action")).toBe("token.mint");

  // A repo filter goes out by name, and the column resolves the id back.
  await page.getByLabel("Filter by action").fill("");
  await page.getByLabel("Filter by person").click();
  await page.getByRole("option", { name: "anyone" }).click();
  await page.getByLabel("Filter by repo").click();
  await page.getByRole("option", { name: "widget" }).click();
  await expect.poll(() => queries.at(-1)!.get("repo")).toBe("widget");
  await page.getByLabel("Filter by repo").click();
  await page.getByRole("option", { name: "any" }).click();

  // A date becomes epoch milliseconds, and it is midnight *local* — a
  // person picking the 4th means the 4th where they are, so reading it
  // as UTC would drop that morning and add the previous evening.
  await page.getByLabel("Filter from date").fill("2020-03-04");
  await expect
    .poll(() => queries.at(-1)!.get("since"))
    .toBe(String(new Date("2020-03-04T00:00:00").getTime()));

  // The export carries the credential and the same filters. A plain
  // <a download> would navigate with no Authorization header and hand
  // the user a 401 page named `.csv`.
  const download = page.waitForEvent("download");
  await page.getByRole("button", { name: "Export CSV" }).click();
  const file = await download;
  expect(file.suggestedFilename()).toBe("acme-activity.csv");
  const csv = queries.at(-1)!;
  expect(csv.get("format")).toBe("csv");
  expect(csv.get("since")).toBe(
    String(new Date("2020-03-04T00:00:00").getTime()),
  );
});

/// Teams: created, staffed, and deleted from one screen.
test("teams can be created, staffed and deleted", async ({ page }) => {
  const teams = TEAMS.map((t) => ({ ...t }));
  await signIn(page, [], teams);
  await page.getByRole("link", { name: "Teams" }).click();
  await expect(page.getByRole("button", { name: "payments" })).toBeVisible();

  await page.getByLabel("Team name").fill("infra");
  await page.getByLabel("Team description").fill("keeps the lights on");
  await page.getByRole("button", { name: "Create team" }).click();
  // Creating selects the new team, so its roster opens straight away.
  await expect(page.getByText("Nobody is in this team yet.")).toBeVisible();
  expect(teams.map((t) => t.name)).toContain("infra");

  // Staffing it: only people not already in it are offered.
  await page.getByLabel("Add to team").click();
  await page.getByRole("option", { name: "dev@acme.test" }).click();
  await page.getByRole("button", { name: "Add", exact: true }).click();
  await expect(
    page.getByRole("cell", { name: "dev@acme.test", exact: true }),
  ).toBeVisible();
  await expect(page.getByLabel("Add to team")).toContainText("choose a person");

  // …and removing them again, by a control that names who it removes.
  await page.getByLabel("Remove dev@acme.test from the team").click();
  await expect(page.getByText("Nobody is in this team yet.")).toBeVisible();

  // Deleting a team takes it out of the list, once its dialog confirms.
  await page
    .getByRole("row", { name: /payments/ })
    .getByRole("button", { name: "Delete" })
    .click();
  await page
    .getByRole("alertdialog")
    .getByRole("button", { name: "Delete" })
    .click();
  await expect(page.getByRole("button", { name: "payments" })).toHaveCount(0);
  expect(teams.map((t) => t.name)).not.toContain("payments");
});

/// The access map is the answer to "why can Dev Person write here?", so the
/// source of each row is the thing under test — a table that showed the
/// roles without saying where they came from would be no answer at all.
test("repo access says where each person's access came from", async ({
  page,
}) => {
  const grants: unknown[] = [];
  await signIn(
    page,
    [],
    TEAMS.map((t) => ({ ...t })),
    grants,
  );
  // The access map lives under the repository's Settings tab, which is
  // the only admin surface it has ever belonged on.
  await openRepo(page, "widget");
  await repoTab(page, "Settings").click();
  await expect(page.getByRole("cell", { name: /Ada Owner/ })).toBeVisible();

  const dev = page.getByRole("row", { name: /Dev Person/ });
  await expect(dev).toContainText("admin");
  await expect(dev).toContainText("team · payments");
  const ada = page.getByRole("row", { name: /Ada Owner/ });
  await expect(ada).toContainText("org role");
  // A role that came from the org is not revocable here — there is no
  // grant to withdraw, and offering the button would be a lie.
  await expect(ada.getByRole("button", { name: "Revoke" })).toHaveCount(0);

  // The granted team appears in its own right, with its size.
  const team = page.getByRole("row", { name: /payments/ }).first();
  await expect(team).toContainText("team · 1 person");

  // Granting a team goes out as a team grant, not as its people.
  //
  // `grants.length` observes the *mock*, which is satisfied the moment
  // the request arrives — before the page has finished with it. The
  // form clears itself when the grant lands, so waiting for that is
  // what makes the next interaction safe: selecting a subject before
  // the clear arrives loses the selection to it, and the Grant button
  // stays disabled forever. That is how this test failed on a slower
  // runner while passing here.
  const subject = page.getByLabel("Grant access to");
  await subject.click();
  await page.getByRole("option", { name: "payments (team)" }).click();
  await page.getByLabel("Role to grant").click();
  await page.getByRole("option", { name: "viewer" }).click();
  await page.getByRole("button", { name: "Grant", exact: true }).click();
  await expect.poll(() => grants.length).toBe(1);
  expect(grants[0]).toEqual({ team_id: "01team1", role: "viewer" });
  await expect(subject).toContainText("a person or a team");

  // Granting a person goes out as user_ids — the many-at-once shape.
  await subject.click();
  await page.getByRole("option", { name: "dev@acme.test" }).click();
  await page.getByRole("button", { name: "Grant", exact: true }).click();
  await expect.poll(() => grants.length).toBe(2);
  expect(grants[1]).toEqual({ user_ids: ["01user2"], role: "viewer" });
  await expect(subject).toContainText("a person or a team");

  // Withdrawing the team's grant hits the team-grants route.
  await team.getByRole("button", { name: "Revoke" }).click();
  await expect.poll(() => grants.length).toBe(3);
  expect(grants[2]).toEqual({ revoked_team: "01team1" });
});

/// A member may not read the access map, and a panel that showed them a
/// permanent error where a table should be would read as a broken page.
test("the access panel is absent for someone who may not read it", async ({
  page,
}) => {
  await signIn(page);
  await page.route("**/v1/orgs/acme/repos/widget/access", (r) =>
    r.fulfill({ status: 404, json: { error: "not found" } }),
  );
  await openRepo(page, "widget");
  await repoTab(page, "Settings").click();
  // The rest of the page is there — this is the panel disappearing, not
  // the page failing to load.
  await expect(page.getByText("Danger Zone")).toBeVisible();
  await expect(page.getByText("Access", { exact: true })).toHaveCount(0);
});

/// Stateful changes mock for one native repo (`session-1`): approvals
/// flip the verdict, landing transitions across polls, and an optional
/// scripted refusal exercises the error surface. Every route the tab
/// calls is mocked — a hermetic suite must not depend on a connection
/// being refused.
async function mockChanges(
  page: Page,
  opts: {
    landRefusal?: string;
    /// The **merged** shape the change route answers with: patchset rows
    /// unioned with the runs reported against the commit. `required` and
    /// `source` default rather than being omitted — a mock that leaves a
    /// field out is a mock that tests a response the server cannot send,
    /// and the panel reads both of these.
    checks?: {
      name: string;
      state: string;
      url: string | null;
      required?: boolean;
      source?: "patchset" | "commit";
      posted_by?: string;
    }[];
  } = {},
) {
  const ps1 = {
    number: 1,
    commit: "a".repeat(40),
    parent: "b".repeat(40),
    message: "add gateway\n\nChange-Id: Icafe1234\n",
    created_at: Date.now() - 60_000,
  };
  const s = {
    state: "open" as "open" | "landing" | "landed",
    approvals: [] as {
      email: string;
      name: string;
      patchset_id: string;
      created_at: number;
    }[],
    comments: [] as {
      id: string;
      patchset: number;
      author: string;
      author_email: string | null;
      author_principal: string;
      path: string | null;
      line: number | null;
      body: string;
      created_at: number;
    }[],
    landingPolls: 0,
    checks: (opts.checks ?? []).map((k) => ({
      required: false,
      source: "patchset" as const,
      posted_by: "token:01ci",
      ...k,
      updated_at: Date.now() - 30_000,
    })),
  };
  const change = () => ({
    key: "Icafe1234",
    title: "add gateway",
    target_branch: "main",
    state: s.state,
    land_verdict: s.state === "landed" ? "landed" : null,
    landed_commit: s.state === "landed" ? ps1.commit : null,
    created_at: ps1.created_at,
    updated_at: ps1.created_at,
    patchset: ps1,
  });
  const OWNERS = ["alice@acme.test", "@payments"];
  const verdict = () =>
    s.approvals.length > 0
      ? {
          landable: true,
          explanation: "ok: all 1 changed path(s) approved",
          per_path: [
            {
              path: "payments/gateway.rs",
              satisfied: true,
              owners: OWNERS,
              explanation:
                "ok: /payments/gateway.rs approved by alice@acme.test",
            },
          ],
        }
      : {
          landable: false,
          explanation:
            "blocked: needs an owner of /payments/gateway.rs (owners: alice@acme.test, @payments)",
          per_path: [
            {
              path: "payments/gateway.rs",
              satisfied: false,
              owners: OWNERS,
              explanation:
                "blocked: needs an owner of /payments/gateway.rs (owners: alice@acme.test, @payments)",
            },
          ],
        };
  await page.route("**/v1/orgs/acme/repos/session-1/changes", (r) => {
    if (r.request().method() === "POST") {
      return r.fulfill({
        status: 201,
        json: { change: change(), patchset: ps1 },
      });
    }
    return r.fulfill({ json: { changes: [change()] } });
  });
  await page.route("**/v1/orgs/acme/repos/session-1/changes/Icafe1234", (r) => {
    if (s.state === "landing" && ++s.landingPolls >= 2) {
      s.state = "landed";
    }
    return r.fulfill({
      json: { change: change(), patchsets: [ps1], approvals: s.approvals },
    });
  });
  await page.route(
    "**/v1/orgs/acme/repos/session-1/changes/Icafe1234/verdict",
    (r) =>
      r.fulfill({
        json: {
          change: "Icafe1234",
          state: s.state,
          patchset: 1,
          commit: ps1.commit,
          verdict: verdict(),
        },
      }),
  );
  await page.route(
    "**/v1/orgs/acme/repos/session-1/changes/Icafe1234/approve",
    (r) => {
      if (r.request().method() === "DELETE") {
        s.approvals = [];
      } else {
        s.approvals = [
          {
            email: "alice@acme.test",
            name: "Alice",
            patchset_id: "01ps1",
            created_at: Date.now(),
          },
        ];
      }
      return r.fulfill({ status: 204, body: "" });
    },
  );
  await page.route(
    "**/v1/orgs/acme/repos/session-1/changes/Icafe1234/comments",
    (r) => {
      if (r.request().method() === "POST") {
        const body = r.request().postDataJSON() as {
          body: string;
          path?: string;
          line?: number;
        };
        const c = {
          id: `01c${s.comments.length}`,
          patchset: 1,
          author: "Dev Person",
          author_email: "dev@acme.test",
          author_principal: "user:01user2",
          path: body.path ?? null,
          line: body.line ?? null,
          body: body.body,
          created_at: Date.now(),
        };
        s.comments.push(c);
        return r.fulfill({ status: 201, json: c });
      }
      return r.fulfill({ json: { comments: s.comments } });
    },
  );
  await page.route(
    "**/v1/orgs/acme/repos/session-1/changes/Icafe1234/checks",
    (r) => r.fulfill({ json: { patchset: 1, checks: s.checks } }),
  );
  await page.route("**/v1/orgs/acme/repos/session-1/diff?*", (r) =>
    r.fulfill({
      json: {
        from: ps1.parent,
        to: ps1.commit,
        changes: [
          {
            status: "modified",
            path: "payments/gateway.rs",
            old_oid: "1".repeat(40),
            new_oid: "2".repeat(40),
            old_mode: "100644",
            new_mode: "100644",
          },
        ],
      },
    }),
  );
  await page.route(
    "**/v1/orgs/acme/repos/session-1/files/payments/gateway.rs?*",
    (r) => {
      const at = new URL(r.request().url()).searchParams.get("at");
      const text =
        at === ps1.parent
          ? "fn main() {}\nlet fee = old_rate();\n"
          : "fn main() {}\nlet fee = new_rate();\n";
      return r.fulfill({
        status: 200,
        contentType: "application/octet-stream",
        body: text,
      });
    },
  );
  await page.route(
    "**/v1/orgs/acme/repos/session-1/changes/Icafe1234/land",
    (r) => {
      if (opts.landRefusal) {
        return r.fulfill({ status: 409, json: { error: opts.landRefusal } });
      }
      s.state = "landing";
      s.landingPolls = 0;
      return r.fulfill({
        status: 202,
        json: { queued: true, job: "01job1", change: "Icafe1234" },
      });
    },
  );
}

test("the changes tab lists changes and the verdict names what blocks", async ({
  page,
}) => {
  await signIn(page);
  await mockChanges(page);
  await openRepo(page, "session-1");
  await repoTab(page, "Changes").click();
  // The list shows the change with its state and patchset.
  await expect(page.getByText("add gateway")).toBeVisible();
  await expect(page.getByText("open")).toBeVisible();
  // Open the detail: the blocked verdict renders the engine's words.
  await page.getByText("add gateway").click();
  await expect(page.getByText("■ Blocked")).toBeVisible();
  await expect(
    page
      .getByText(
        "blocked: needs an owner of /payments/gateway.rs (owners: alice@acme.test, @payments)",
      )
      .first(),
  ).toBeVisible();
  // Landing is offered but held until the verdict passes.
  await expect(
    page.getByRole("button", { name: "Land on main" }),
  ).toBeDisabled();
  // The blocker, in the merge box, naming what is in the way. This used
  // to be a loose sentence beside the button reading "landing needs the
  // verdict above to pass"; it is now one entry in a list that can hold
  // several reasons at once, which is what a change blocked on both a
  // review and a red check actually needs.
  await expect(
    page.getByText("Review verdict not met — see the verdict above"),
  ).toBeVisible();
});

test("approving flips the verdict and landing walks to landed", async ({
  page,
}) => {
  await signIn(page);
  await mockChanges(page);
  await openRepo(page, "session-1");
  await repoTab(page, "Changes").click();
  await page.getByText("add gateway").click();
  await page.getByRole("button", { name: "Approve patchset 1" }).click();
  await expect(page.getByText("Landable")).toBeVisible();
  await expect(
    page.getByText("ok: all 1 changed path(s) approved"),
  ).toBeVisible();
  await expect(page.getByText("alice@acme.test").first()).toBeVisible();
  // Land: the view polls the change itself until it leaves `landing`.
  await page.getByRole("button", { name: "Land on main" }).click();
  await expect(page.getByText("Landed as")).toBeVisible({ timeout: 15_000 });
  await expect(page.getByText("aaaaaaaaaaaa").first()).toBeVisible();
});

test("a queue refusal surfaces the server's words, not a shrug", async ({
  page,
}) => {
  await signIn(page);
  await mockChanges(page, { landRefusal: "change is landing" });
  await openRepo(page, "session-1");
  await repoTab(page, "Changes").click();
  await page.getByText("add gateway").click();
  await page.getByRole("button", { name: "Approve patchset 1" }).click();
  await page.getByRole("button", { name: "Land on main" }).click();
  await expect(page.getByRole("alert")).toHaveText(
    "Could not land: change is landing",
  );
});

test("the start-review form registers a branch tip as a change", async ({
  page,
}) => {
  await signIn(page);
  await mockChanges(page);
  await openRepo(page, "session-1");
  await repoTab(page, "Changes").click();
  await page.getByLabel("Branch to review").fill("feature");
  await page.getByRole("button", { name: "Start review" }).click();
  // Success lands the author straight in the change's detail view.
  await expect(page.getByText("Patchsets")).toBeVisible();
  await expect(page.getByText("#1")).toBeVisible();
});

test("mirrors get no changes tab: their trunk belongs to the origin", async ({
  page,
}) => {
  await signIn(page);
  // `widget` is a mirror. The tab is absent from the strip, and — the
  // half that matters, because the address is still typable — so is the
  // page behind it: a review form the server can only refuse is worse
  // than no tab at all.
  await openRepo(page, "widget");
  await expect(repoTab(page, "Code")).toBeVisible();
  await expect(repoTab(page, "Changes")).toHaveCount(0);
  await page.goto("/acme/widget/changes");
  await expect(page.getByText(/couldn.t find/i)).toBeVisible();
  // A native repository in the same namespace still has both.
  await page.goto("/acme/session-1");
  await expect(repoTab(page, "Changes")).toBeVisible();
});

test("a reviewer reads the diff in place before approving", async ({
  page,
}) => {
  await signIn(page);
  await mockChanges(page);
  await openRepo(page, "session-1");
  await repoTab(page, "Changes").click();
  await page.getByText("add gateway").click();
  // The files panel names what the patchset touches...
  await expect(page.getByText("Files in this patchset")).toBeVisible();
  await page.getByRole("button", { name: /payments\/gateway\.rs/ }).click();
  // ...and expanding a file shows the actual line change, both sides.
  await expect(page.getByText("let fee = new_rate();")).toBeVisible();
  await expect(page.getByText("let fee = old_rate();")).toBeVisible();
});

test("the conversation posts and renders attributed comments", async ({
  page,
}) => {
  await signIn(page);
  await mockChanges(page);
  await openRepo(page, "session-1");
  await repoTab(page, "Changes").click();
  await page.getByText("add gateway").click();
  await expect(page.getByText("No comments yet.")).toBeVisible();
  await page
    .getByLabel("Comment on this change")
    .fill("the error arm needs a test");
  await page.getByRole("button", { name: "Comment", exact: true }).click();
  await expect(page.getByText("the error arm needs a test")).toBeVisible();
  await expect(page.getByText("Dev Person")).toBeVisible();
  await expect(page.getByText("patchset 1", { exact: true })).toBeVisible();
  // The box cleared — the observable the next comment needs.
  await expect(page.getByLabel("Comment on this change")).toHaveValue("");
});

test("a line comment anchors to the exact line and renders beneath it", async ({
  page,
}) => {
  await signIn(page);
  await mockChanges(page);
  await openRepo(page, "session-1");
  await repoTab(page, "Changes").click();
  await page.getByText("add gateway").click();
  await page.getByRole("button", { name: /payments\/gateway\.rs/ }).click();
  await expect(page.getByText("let fee = new_rate();")).toBeVisible();
  // The gutter affordance rides beside the hovered line, so hover it
  // first; then it opens a draft pinned to that line...
  await page.getByText("let fee = new_rate();").hover();
  await page.getByRole("button", { name: "Comment on line 2" }).click();
  await page
    .getByLabel("Comment on payments/gateway.rs line 2")
    .fill("why does the new rate skip rounding?");
  await page.getByRole("button", { name: "Post line comment" }).click();
  // ...and the posted comment renders both under its line in the diff
  // and in the conversation, so .first() is the inline thread.
  await expect(
    page.getByText("why does the new rate skip rounding?").first(),
  ).toBeVisible();
  // The conversation records the anchor as path:line.
  await expect(page.getByText("payments/gateway.rs:2")).toBeVisible();
});

test("branch policy protects trunk and moves the default branch", async ({
  page,
}) => {
  await signIn(page);
  // Stateful policy mocks: the panel re-reads after every action. The
  // admin probe must succeed here so the mutating forms render.
  await page.route("**/v1/orgs/acme/repos/session-1/access", (r) =>
    r.fulfill({ json: { people: [], teams: [] } }),
  );
  const state = { protections: [] as { branch: string; created_at: number }[] };
  let defaultBranch = "main";
  await page.route("**/v1/orgs/acme/repos/session-1/protections", (r) => {
    if (r.request().method() === "POST") {
      const b = (r.request().postDataJSON() as { branch: string }).branch;
      state.protections.push({ branch: b, created_at: Date.now() });
      return r.fulfill({ status: 201, json: { branch: b, protected: true } });
    }
    return r.fulfill({ json: { protections: state.protections } });
  });
  await page.route("**/v1/orgs/acme/repos/session-1/protections/main", (r) => {
    state.protections = state.protections.filter((p) => p.branch !== "main");
    return r.fulfill({ status: 204, body: "" });
  });
  await page.route("**/v1/orgs/acme/repos/session-1", (r) => {
    if (r.request().method() === "PATCH") {
      defaultBranch = (r.request().postDataJSON() as { default_branch: string })
        .default_branch;
    }
    return r.fulfill({
      json: {
        id: "01repo1",
        org_id: "01org1",
        name: "session-1",
        kind: "native",
        default_branch: defaultBranch,
        origin_url: null,
        last_sync_at: null,
        last_synced_commit: null,
        sync_error: null,
        created_at: Date.now() - 86_400_000,
        clone_url: "https://acme.stratum.dev/acme/session-1.git",
        ssh_clone_url: null,
        // This mock shadows the one in `mockApi`, so it has to carry
        // the viewer's authority too — the Settings tab this test
        // navigates through is gated on it.
        viewer_admin: true,
        viewer_write: true,
      },
    });
  });
  // Branch policy is a setting, and lives with the rest of them.
  await openRepo(page, "session-1");
  await repoTab(page, "Settings").click();

  // The empty state says exactly what is at stake.
  await expect(page.getByText("Branch policy")).toBeVisible();
  await expect(
    page.getByText("protect it to make review the only road to trunk"),
  ).toBeVisible();

  // Protect main: the fence appears with its meaning spelled out.
  await page.getByLabel("Branch to protect").fill("main");
  await page.getByRole("button", { name: "Protect", exact: true }).click();
  await expect(page.getByText("lands through review only")).toBeVisible();

  // Unprotect restores the empty state.
  await page.getByRole("button", { name: "Unprotect main" }).click();
  await expect(
    page.getByText("protect it to make review the only road to trunk"),
  ).toBeVisible();

  // Move the default branch; the chip follows the server's answer.
  await page.getByLabel("New default branch").fill("trunk");
  await page.getByRole("button", { name: "Set default" }).click();
  await expect(page.getByText("trunk", { exact: true })).toBeVisible();
});

test("CI checks render beside the human verdict and a red one blocks", async ({
  page,
}) => {
  await signIn(page);
  await mockChanges(page, {
    checks: [
      { name: "ci/lint", state: "passing", url: null },
      { name: "ci/perf", state: "pending", url: null },
      {
        name: "ci/tests",
        state: "failing",
        url: "https://ci.example.com/run/812",
      },
    ],
  });
  await openRepo(page, "session-1");
  await repoTab(page, "Changes").click();
  await page.getByText("add gateway").click();
  // Every check shows its name, its state **as a word**, and a link to
  // the log. The word is the assertion that matters: this panel used to
  // render a bare ✓/✗/◌ glyph, which makes "did CI pass" a question you
  // answer by colour — the one question a red-green colourblind reader
  // gets wrong every time (DESIGN.md).
  await expect(page.getByText("ci/lint")).toBeVisible();
  await expect(page.getByText("Passing", { exact: true })).toBeVisible();
  await expect(page.getByText("ci/perf")).toBeVisible();
  await expect(page.getByText("Pending", { exact: true })).toBeVisible();
  // Exact, because the name now legitimately appears twice: once as the
  // row, and once inside the blocker sentence under the land button.
  // That duplication is the panel working — the blocker names the check
  // rather than saying "a check is failing" and making the reader hunt.
  await expect(page.getByText("ci/tests", { exact: true })).toBeVisible();
  await expect(page.getByText("Failing", { exact: true })).toBeVisible();
  // And a headline that summarises them, GitHub's shape, so the state of
  // the whole set is legible without reading every row.
  await expect(page.getByText("Some checks were not successful")).toBeVisible();
  await expect(page.getByRole("link", { name: "Details" })).toHaveAttribute(
    "href",
    "https://ci.example.com/run/812",
  );
  // The machine's red blocks landing even after the humans say yes — and
  // the disabled button now names what is in the way, rather than
  // leaving a reader to infer it from a sentence elsewhere on the page.
  await page.getByRole("button", { name: "Approve patchset 1" }).click();
  await expect(page.getByText("✓ Landable")).toBeVisible();
  await expect(page.getByText("1 check failed: ci/tests")).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Land on main" }),
  ).toBeDisabled();
});

test("a commit-scoped red check is on the page and blocks the land", async ({
  page,
}) => {
  // The merged read, through the browser. Two writers report on one
  // change: the signing intake against this patchset, and a provider
  // against its commit. Three things were wrong before the merged route
  // landed, and each is asserted here rather than in a unit test,
  // because each is only visible once a real response reaches a real
  // render.
  await signIn(page);
  await mockChanges(page, {
    checks: [
      {
        name: "ci/lint",
        state: "passing",
        url: null,
        required: true,
        source: "patchset",
        posted_by: "token:01ci",
      },
      {
        // `cancelled`, not `failing`: a word only the commit side uses.
        // The land button matched the string "failing", so this row left
        // it enabled and the server refused the land with a 409 the page
        // had not warned about.
        name: "build",
        state: "cancelled",
        url: "https://actions.example.com/run/7",
        required: true,
        source: "commit",
        posted_by: "github",
      },
    ],
  });
  await openRepo(page, "session-1");
  await repoTab(page, "Changes").click();
  await page.getByText("add gateway").click();

  // It is on the page at all — the whole defect in one assertion.
  await expect(page.getByText("build", { exact: true })).toBeVisible();
  await expect(page.getByText("Cancelled", { exact: true })).toBeVisible();
  // And it says where to go and look, which is the question a red row
  // raises. `github`, the writer — not "commit", the scope.
  await expect(page.getByText("github", { exact: true })).toBeVisible();
  // Required is a badge, not an inference.
  await expect(page.getByText("Required").first()).toBeVisible();
  // The headline counts a cancelled run as unsuccessful rather than
  // filing it under "hasn't completed yet", which would tell a
  // maintainer to wait for something that will never arrive.
  await expect(page.getByText("Some checks were not successful")).toBeVisible();

  // Humans satisfied, machine not: the button is off and says why.
  await page.getByRole("button", { name: "Approve patchset 1" }).click();
  await expect(page.getByText("✓ Landable")).toBeVisible();
  await expect(page.getByText("1 required check failed: build")).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Land on main" }),
  ).toBeDisabled();
});

test("an invitation that was not emailed says so, with the reason", async ({
  page,
}) => {
  await signInAsPerson(page);
  await page.route("**/v1/orgs/acme/invites", (r) => {
    if (r.request().method() === "POST") {
      return r.fulfill({
        status: 201,
        json: {
          id: "01inv3",
          invite_link: "stinv_01inv3_secretsecret",
          mail: { sent: false, error: "smtp relay.example.com:25: refused" },
        },
      });
    }
    return r.fulfill({ json: { invites: [] } });
  });
  await page.getByRole("link", { name: "Members" }).click();
  await page.getByLabel("Invite email").fill("new@acme.test");
  await page.getByRole("button", { name: "Invite" }).click();
  await expect(page.getByText(/Not emailed \(smtp relay/)).toBeVisible();
  await expect(page.getByText("stinv_01inv3_secretsecret")).toBeVisible();
});

test("an emailed invitation link accepts and signs the new person in", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  let posted: Record<string, unknown> | undefined;
  await page.route("**/v1/auth/accept-invite", async (route) => {
    posted = route.request().postDataJSON();
    await route.fulfill({ status: 201, json: ME });
  });
  let previewed: Record<string, unknown> | undefined;
  await page.route("**/v1/auth/invite/preview", async (route) => {
    previewed = route.request().postDataJSON();
    await route.fulfill({
      json: {
        org: "acme",
        role: "member",
        email: "new@acme.test",
        expires_at: Date.now() + 86_400_000,
      },
    });
  });

  // The link out of the email, token in the fragment and percent-encoded.
  await page.goto("/dashboard/#invite=stinv_01inv2_secret%2Bslash");
  // The screen says what is being joined before asking for a password.
  await expect(page.getByRole("heading", { name: "Join acme" })).toBeVisible();
  await expect(
    page.getByText("new@acme.test was invited as member"),
  ).toBeVisible();
  expect(previewed).toEqual({ invite: "stinv_01inv2_secret+slash" });

  await page.getByLabel("Your name").fill("New Person");
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Accept invitation" }).click();

  // Signed in, and the credential is gone from the address bar rather
  // than sitting in history for the next person at this machine.
  await expect(page.getByText("Requests today")).toBeVisible();
  expect(posted).toEqual({
    invite: "stinv_01inv2_secret+slash",
    name: "New Person",
    password: "a long enough password",
  });
  expect(new URL(page.url()).hash).toBe("");
});

test("an invitation refused by the server explains itself and stays put", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/auth/accept-invite", (r) =>
    r.fulfill({
      status: 400,
      json: { error: "this invitation is not valid" },
    }),
  );
  await page.route("**/v1/auth/invite/preview", (r) =>
    r.fulfill({
      json: {
        org: "acme",
        role: "member",
        email: "new@acme.test",
        expires_at: Date.now() + 86_400_000,
      },
    }),
  );
  await page.goto("/dashboard/#invite=stinv_used_already");
  await page.getByLabel("Your name").fill("Too Late");
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Accept invitation" }).click();
  await expect(page.getByRole("alert")).toContainText(
    "this invitation is not valid",
  );
  // Still on the screen, so the person can read it — not bounced to a
  // login form that says nothing about why.
  await expect(page.getByRole("heading", { name: "Join acme" })).toBeVisible();
});

/// A link that is already spent must say so *before* somebody chooses a
/// password, not after. This is the case that turns "why doesn't this
/// work?" into "ask for a new one".
test("a spent invitation link says so before asking for anything", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/auth/invite/preview", (r) =>
    r.fulfill({
      status: 404,
      json: { error: "this invitation is not valid" },
    }),
  );
  let accepted = false;
  await page.route("**/v1/auth/accept-invite", (r) => {
    accepted = true;
    return r.fulfill({ status: 400, json: { error: "no" } });
  });
  await page.goto("/dashboard/#invite=stinv_spent_link");
  await expect(page.getByText(/already have been used/)).toBeVisible();
  // No form, no accept button: there is nothing useful to do here.
  await expect(page.getByLabel("Your name")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Accept invitation" }),
  ).toHaveCount(0);
  await page.getByRole("button", { name: "Go to sign in" }).click();
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();
  expect(accepted).toBe(false);
});

test("someone already signed in accepts with one button, not a new password", async ({
  page,
}) => {
  // A live cookie session, so the boot probe answers with an account —
  // which is what makes this the "already signed in" branch.
  await mockApi(page);
  let posted: Record<string, unknown> | undefined;
  await page.route("**/v1/auth/accept-invite", async (route) => {
    posted = route.request().postDataJSON();
    await route.fulfill({ status: 201, json: ME });
  });
  await page.route("**/v1/auth/invite/preview", (r) =>
    r.fulfill({
      json: {
        org: "acme",
        role: "admin",
        email: "someone@acme.test",
        expires_at: Date.now() + 86_400_000,
      },
    }),
  );
  await page.goto("/dashboard/#invite=stinv_01inv4_secret");
  await expect(page.getByText(/Accepting adds owner@acme.test/)).toBeVisible();
  await expect(page.getByLabel("Password")).toHaveCount(0);
  await page.getByRole("button", { name: "Accept invitation" }).click();
  await expect(page.getByText("Requests today")).toBeVisible();
  expect(posted).toEqual({ invite: "stinv_01inv4_secret", name: "" });
});

test("a malformed invitation fragment is ignored rather than sent on", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  let called = false;
  await page.route("**/v1/auth/accept-invite", (r) => {
    called = true;
    return r.fulfill({ status: 400, json: { error: "no" } });
  });
  // A stray percent escape cannot be decoded; the page must fall through
  // to the ordinary sign-in rather than posting garbage.
  await page.goto("/dashboard/#invite=%E0%A4%A");
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();
  expect(called).toBe(false);
});

test("signing up says what will happen without saying whether it did", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  let posted: Record<string, unknown> | undefined;
  await page.route("**/v1/auth/signup", async (route) => {
    posted = route.request().postDataJSON();
    await route.fulfill({
      status: 202,
      json: { status: "check your email", detail: "on its way" },
    });
  });

  await page.goto("/dashboard/");
  await page.getByRole("button", { name: "Create an account" }).click();
  await page.getByLabel("Your name").fill("Ada Lovelace");
  await page.getByLabel("Namespace").fill("ada");
  // The namespace preview is the thing somebody checks before committing
  // to a name that lands in every clone URL they hand out.
  await expect(page.getByText("/ada/repo")).toBeVisible();
  await page.getByLabel("Email").fill("ada@example.test");
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Create account" }).click();

  const status = page.getByRole("status");
  await expect(status).toContainText("If ada@example.test can receive mail");
  // It must not claim the account exists — the server refuses to say,
  // and a page that says "your account is ready" contradicts it.
  await expect(status).not.toContainText(/account is ready|welcome/i);
  expect(posted).toEqual({
    email: "ada@example.test",
    name: "Ada Lovelace",
    password: "a long enough password",
    handle: "ada",
  });
  // The form is gone: a form left standing with "Create account" still
  // lit read as "it did not work" — the first real sign-up clicked it
  // and was told to check the mail again. What remains is the one thing
  // to do, the way to ask for the message again, and the way in.
  await expect(
    page.getByRole("heading", { name: "Check your email" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Create account" }),
  ).toHaveCount(0);
  await expect(page.getByLabel("Password")).toHaveCount(0);
  // …and not the token form either: the first version of this screen
  // fell through to it, and showed "Organization / API token" under
  // "Check your email".
  await expect(page.getByLabel("API token")).toHaveCount(0);
  await expect(page.getByLabel("Organization")).toHaveCount(0);
  let resent: Record<string, unknown> | undefined;
  await page.route("**/v1/auth/resend-verification", async (route) => {
    resent = route.request().postDataJSON();
    await route.fulfill({ status: 202, json: { status: "check your email" } });
  });
  await page.getByRole("button", { name: "Send the message again" }).click();
  await expect(status).toContainText(
    "Another confirmation link is on its way to ada@example.test",
  );
  expect(resent).toEqual({ email: "ada@example.test" });
  await page.getByRole("button", { name: "Sign in instead" }).click();
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();
});

test("a refused handle is reported plainly, unlike anything about the address", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/auth/signup", (r) =>
    r.fulfill({ status: 409, json: { error: '"dashboard" is reserved' } }),
  );
  await page.goto("/dashboard/");
  await page.getByRole("button", { name: "Create an account" }).click();
  await page.getByLabel("Your name").fill("Squatter");
  await page.getByLabel("Namespace").fill("dashboard");
  await page.getByLabel("Email").fill("s@example.test");
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Create account" }).click();
  await expect(page.getByRole("alert")).toContainText("is reserved");
});

test("asking for a reset link says the same thing either way", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  let posted: Record<string, unknown> | undefined;
  await page.route("**/v1/auth/forgot-password", async (route) => {
    posted = route.request().postDataJSON();
    await route.fulfill({ status: 202, json: { status: "check your email" } });
  });
  await page.goto("/dashboard/");
  await page.getByRole("button", { name: "Forgot your password?" }).click();
  // No password field on this screen — there is nothing to type yet.
  await expect(page.getByLabel("Password")).toHaveCount(0);
  await page.getByLabel("Email").fill("who@example.test");
  await page.getByRole("button", { name: "Send a reset link" }).click();
  await expect(page.getByRole("status")).toContainText(
    "If who@example.test has an account here",
  );
  expect(posted).toEqual({ email: "who@example.test" });
});

test("a confirmation link redeems on arrival and signs the person in", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  let posted: Record<string, unknown> | undefined;
  await page.route("**/v1/auth/verify", async (route) => {
    posted = route.request().postDataJSON();
    await route.fulfill({ status: 200, json: ME });
  });
  // Straight from the mailbox: no button to press, because the link was
  // the button.
  await page.goto("/dashboard/#verify=weftv_01_secret");
  await expect(page.getByText("Requests today")).toBeVisible();
  await expect.poll(() => new URL(page.url()).hash).toBe("");
  expect(posted).toEqual({ token: "weftv_01_secret" });
});

test("a spent confirmation link says so instead of retrying forever", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/auth/verify", (r) =>
    r.fulfill({
      status: 404,
      json: { error: "this confirmation link is not valid any more" },
    }),
  );
  await page.goto("/dashboard/#verify=weftv_spent_link");
  await expect(
    page.getByRole("heading", { name: "Link expired" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Go to sign in" }).click();
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();
});

test("a reset link asks for the new password and warns what it costs", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  let posted: Record<string, unknown> | undefined;
  await page.route("**/v1/auth/reset-password", async (route) => {
    posted = route.request().postDataJSON();
    await route.fulfill({ status: 200, json: ME });
  });
  await page.goto("/dashboard/#reset=weftrs_01_secret");
  await expect(
    page.getByRole("heading", { name: "Choose a new password" }),
  ).toBeVisible();
  // Somebody about to be signed out of everything should be told first.
  await expect(page.getByText(/signs out every other session/)).toBeVisible();
  await page.getByLabel("New password").fill("a different long password");
  await page.getByRole("button", { name: "Set password" }).click();
  await expect(page.getByText("Requests today")).toBeVisible();
  await expect.poll(() => new URL(page.url()).hash).toBe("");
  expect(posted).toEqual({
    token: "weftrs_01_secret",
    new_password: "a different long password",
  });
});

test("an unconfirmed account is told what is blocked and can ask again", async ({
  page,
}) => {
  const unconfirmed = { ...ME, verified_at: null };
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: unconfirmed }));
  let resent: Record<string, unknown> | undefined;
  await page.route("**/v1/auth/resend-verification", async (route) => {
    resent = route.request().postDataJSON();
    await route.fulfill({ status: 202, json: { status: "check your email" } });
  });

  await page.goto("/dashboard/");
  const banner = page.getByRole("status").filter({ hasText: "Confirm" });
  await expect(banner).toContainText("to create repositories");
  await banner.getByRole("button", { name: "Send it again" }).click();
  await expect(page.getByRole("status")).toContainText(
    "Another confirmation link is on its way",
  );
  // Saying the previous link is dead matters: somebody with two messages
  // open otherwise clicks the older one and is told it is invalid.
  await expect(page.getByRole("status")).toContainText("no longer works");
  expect(resent).toEqual({ email: "owner@acme.test" });
});

test("a confirmed account sees no banner", async ({ page }) => {
  await signInAsPerson(page);
  await expect(page.getByText(/to create repositories/)).toHaveCount(0);
});

/// The case the manual browser pass caught and everything else missed.
///
/// Every test above lands on the dashboard *by loading it* with the token
/// already in the URL. A real person has the dashboard open in a tab and
/// clicks a link in their mail client: the browser sees only a fragment
/// change, which is a same-document navigation — no reload, no
/// re-mount — so a page that reads the token once at boot does nothing
/// at all. That is the ordinary case, not the exotic one.
test("a mailed link works when the dashboard is already open", async ({
  page,
}) => {
  const unconfirmed = { ...ME, verified_at: null };
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: unconfirmed }));
  let verified: Record<string, unknown> | undefined;
  await page.route("**/v1/auth/verify", async (route) => {
    verified = route.request().postDataJSON();
    await route.fulfill({ status: 200, json: ME });
  });

  // Already signed in, already looking at the dashboard, told to confirm.
  await page.goto("/dashboard/");
  await expect(page.getByText(/to create repositories/)).toBeVisible();

  // The click from the mail client: same document, fragment only.
  await page.evaluate(() => {
    window.location.hash = "verify=weftv_from_the_inbox";
  });

  // Wait on the thing under test, not on a proxy for it. The dashboard
  // was already visible before the hash changed, and the banner vanishes
  // the instant the confirming card mounts — so both are satisfied
  // before the round trip finishes, and reading the URL then catches it
  // mid-flight. The cleared fragment only exists once it is done.
  await expect.poll(() => new URL(page.url()).hash).toBe("");
  await expect(page.getByText("Requests today")).toBeVisible();
  await expect(page.getByText(/to create repositories/)).toHaveCount(0);
  expect(verified).toEqual({ token: "weftv_from_the_inbox" });
});

/// The same hazard for the other two mailed links.
test("an invitation and a reset link also work without a reload", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/invite/preview", (r) =>
    r.fulfill({
      json: {
        org: "acme",
        role: "member",
        email: "new@acme.test",
        expires_at: Date.now() + 86_400_000,
      },
    }),
  );
  await page.goto("/dashboard/");
  await expect(page.getByText("Requests today")).toBeVisible();

  await page.evaluate(() => {
    window.location.hash = "invite=stinv_from_the_inbox";
  });
  await expect(page.getByRole("heading", { name: "Join acme" })).toBeVisible();

  // Dismissing clears the fragment, so the next one is seen as new.
  await page.getByRole("button", { name: "Not now" }).click();
  await expect(page.getByText("Requests today")).toBeVisible();

  await page.evaluate(() => {
    window.location.hash = "reset=weftrs_from_the_inbox";
  });
  await expect(
    page.getByRole("heading", { name: "Choose a new password" }),
  ).toBeVisible();
});

test("an organization is created from the switcher and opens at once", async ({
  page,
}) => {
  await signInAsPerson(page);
  let posted: Record<string, unknown> | undefined;
  await page.route("**/v1/orgs", async (route) => {
    posted = route.request().postDataJSON();
    await route.fulfill({
      status: 201,
      json: {
        id: "01neworg",
        name: "newco",
        detail: "Ready. Invite people, then create or mirror a repository.",
      },
    });
  });
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({
      json: {
        ...ME,
        orgs: [...ME.orgs, { id: "01neworg", name: "newco", role: "owner" }],
      },
    }),
  );
  await page.route("**/v1/orgs/newco/repos", (r) => r.fulfill({ json: [] }));
  await page.route("**/v1/orgs/newco/usage", (r) =>
    r.fulfill({ json: { days: [] } }),
  );

  await page.getByLabel("Organization").click();
  await page.getByRole("option", { name: "+ New organization…" }).click();
  // It says up front that a personal namespace already exists, before
  // anybody names a company to hold one repository.
  await expect(
    page.getByText(/Your own namespace already exists/),
  ).toBeVisible();
  // Nothing here is for sale, and the form must not suggest otherwise.
  await expect(page.getByText(/card|billed|subscription/i)).toHaveCount(0);
  await page.getByLabel("Organization name").fill("newco");
  await page
    .getByRole("button", { name: "Create organization", exact: true })
    .click();
  expect(posted).toEqual({ name: "newco" });
  // Straight to the new organization, which is current, with the
  // server's own sentence. The switcher by role: while the form is
  // still closing, "Organization name" answers to the label too.
  await expect(
    page.getByRole("combobox", { name: "Organization" }),
  ).toContainText("newco");
  await expect(
    page.getByText("Ready. Invite people, then create or mirror a repository."),
  ).toBeVisible();
  await expect(page).toHaveURL(/\/dashboard\/?$/);
});

test("a refused organization name is reported where it was typed", async ({
  page,
}) => {
  await signInAsPerson(page);
  await page.route("**/v1/orgs", (r) =>
    r.fulfill({ status: 409, json: { error: '"acme" already exists' } }),
  );
  await page.getByLabel("Organization").click();
  await page.getByRole("option", { name: "+ New organization…" }).click();
  await page.getByLabel("Organization name").fill("acme");
  await page
    .getByRole("button", { name: "Create organization", exact: true })
    .click();
  await expect(page.getByRole("alert")).toContainText("already exists");
  // Still on the form, so the name can be corrected rather than retyped
  // from scratch.
  await expect(page.getByLabel("Organization name")).toHaveValue("acme");
});

const TREE_ROOT = {
  commit: "abc1234567890abc1234567890abc1234567890a",
  entries: [
    { name: "README.md", mode: "100644", kind: "blob", oid: "b1", size: 21 },
    { name: "src", mode: "40000", kind: "tree", oid: "t1", size: null },
    { name: "logo.png", mode: "100644", kind: "blob", oid: "b2", size: 4096 },
  ],
};

const TREE_SRC = {
  commit: TREE_ROOT.commit,
  entries: [
    { name: "main.rs", mode: "100644", kind: "blob", oid: "b3", size: 13 },
  ],
};

/// The whole repository, flat, as `GET /tree?recursive=1` answers it for
/// the tree beside a file: directories end in `/`, in tree order.
const TREE_PATHS = {
  commit: TREE_ROOT.commit,
  paths: ["README.md", "logo.png", "src/", "src/main.rs"],
  truncated: false,
};

const BRANCHES = {
  branches: [
    { name: "main", full: "refs/heads/main", oid: "abc", default: true },
    { name: "side", full: "refs/heads/side", oid: "def", default: false },
  ],
  head: "refs/heads/main",
};

/// Copied from what the server actually returns, not from what the
/// client wished it returned. The first version of this fixture invented
/// `{ commits: [{ oid, time }] }`; the real shape is `{ entries: [{
/// commit, author }] }` with the author as a git identity line — and
/// because the mock and the client shared one wrong assumption, every
/// test passed while the real page threw.
const LOG = {
  entries: [
    {
      commit: "abc1234567890abc1234567890abc1234567890a",
      message: "first commit\n\nwith a body nobody should see in the list",
      author: "Ada Owner <ada@acme.test> 1787406946 +0000",
      committer: "Ada Owner <ada@acme.test> 1787406946 +0000",
      parents: [],
      tree: "t0",
    },
  ],
  next_after: null,
};

/// Wire up the read endpoints a browser walks through.
///
/// Every pattern is scoped to `/v1/orgs/…` rather than matching a bare
/// `**/repos/widget/…`, because the loose form also matches the *page*
/// navigation to `/acme/widget/tree/logo.png` — `log*` swallowed
/// it and served the commit-log JSON as the document.
///
/// Ordered general-to-specific on purpose: Playwright matches routes
/// newest-first, so the broad `tree*` pattern has to be registered
/// before the paths that must not fall into it. A file asked for as a
/// tree answers 404, which is exactly what tells the browser to ask for
/// it as a file instead.
/// History per path, as `GET /log?path=…` answers it — each commit
/// carrying what it did to that file.
const FILE_LOG: Record<string, { entries: unknown[]; next_after: null }> = {
  "README.md": {
    entries: [
      {
        commit: "abc1234",
        message: "first commit",
        author: "Ada Owner <ada@acme.dev> 1766000000 +0000",
        committer: "Ada Owner <ada@acme.dev> 1766000000 +0000",
        parents: ["aaa1111"],
        tree: "t1",
        change: "modified",
      },
      {
        commit: "aaa1111",
        message: "add the readme",
        author: "Dev Person <dev@acme.dev> 1765000000 +0000",
        committer: "Dev Person <dev@acme.dev> 1765000000 +0000",
        parents: [],
        tree: "t0",
        change: "added",
      },
    ],
    next_after: null,
  },
};

async function mockBrowse(page: Page) {
  await page.route("**/v1/orgs/*/repos/widget/tree*", (r) =>
    r.fulfill({ json: TREE_ROOT }),
  );
  await page.route("**/v1/orgs/*/repos/widget/tree/src*", (r) =>
    r.fulfill({ json: TREE_SRC }),
  );
  for (const file of ["README.md", "logo.png", "nope"]) {
    await page.route(`**/v1/orgs/*/repos/widget/tree/${file}*`, (r) =>
      r.fulfill({ status: 404, json: { error: `"${file}" is not a tree` } }),
    );
  }
  await page.route("**/v1/orgs/*/repos/widget/branches*", (r) =>
    r.fulfill({ json: BRANCHES }),
  );
  // The log mock honours `?path=`, because the file page's history is a
  // *filtered* log — a mock that ignored the filter would let a client
  // that forgot to send it pass.
  await page.route("**/v1/orgs/*/repos/widget/log*", (r) => {
    const path = new URL(r.request().url()).searchParams.get("path");
    if (!path) return r.fulfill({ json: LOG });
    return r.fulfill({
      json: FILE_LOG[path] ?? { entries: [], next_after: null },
    });
  });
  await page.route("**/v1/orgs/*/repos/widget/files/README.md*", (r) => {
    // `at` decides which version comes back, so switching versions is
    // observable rather than a re-render of the same bytes.
    const at = new URL(r.request().url()).searchParams.get("at");
    const old = at === "aaa1111";
    return r.fulfill({
      status: 200,
      headers: {
        "content-type": "text/plain; charset=utf-8",
        "x-weft-binary": "false",
        "x-weft-commit": old ? "aaa1111" : TREE_ROOT.commit,
      },
      body: old ? "# hello\nthe first version\n" : "# hello\nsecond line\n",
    });
  });
  await page.route("**/v1/orgs/*/repos/widget/files/logo.png*", (r) =>
    r.fulfill({
      status: 200,
      headers: {
        "content-type": "image/png",
        "x-weft-binary": "true",
        "x-weft-commit": TREE_ROOT.commit,
      },
      body: "not really a png",
    }),
  );
  await page.route("**/v1/orgs/*/repos/widget/files/src/main.rs*", (r) =>
    r.fulfill({
      status: 200,
      headers: {
        "content-type": "text/plain; charset=utf-8",
        "x-weft-binary": "false",
        "x-weft-commit": TREE_ROOT.commit,
      },
      body: "fn main() {}\n",
    }),
  );
  // The recursive listing behind the tree rail. A predicate registered
  // last, so it wins over the broad `tree*` glob above: Playwright
  // matches newest-first, and `?recursive=1` is the same path as the
  // root listing.
  await page.route(
    (u) =>
      /\/repos\/widget\/tree$/.test(u.pathname) &&
      u.searchParams.get("recursive") === "1",
    (r) => r.fulfill({ json: TREE_PATHS }),
  );
}

/// How many times the page asked for the whole repository.
function countTreeAsks(page: Page): () => number {
  let n = 0;
  page.on("request", (r) => {
    const u = new URL(r.url());
    if (
      /\/repos\/widget\/tree$/.test(u.pathname) &&
      u.searchParams.get("recursive") === "1"
    )
      n += 1;
  });
  return () => n;
}

test("the commit log reads the shape the server really returns", async ({
  page,
}) => {
  await mockApi(page);
  await mockBrowse(page);
  await page.goto("/acme/widget");
  // The floating "History" panel is gone — the latest commit is a bar
  // fused to the top of the file table now, the way a repository page
  // reads. What this test is *for* is unchanged: that the client parses
  // the shape the server really returns, which the bar does from the
  // same walk.
  await expect(page.getByText("abc1234")).toBeVisible();
  // The subject only: a message body belongs on the page this links to,
  // not wrapped into the bar.
  await expect(page.getByText("first commit")).toBeVisible();
  await expect(page.getByText(/nobody should see/)).toHaveCount(0);
  // The name out of the identity line, without the address or the
  // timestamp that follow it. Scoped to the bar: the signed-in person is
  // also called Ada Owner, in the header, and an unscoped match would
  // pass on the wrong element.
  const bar = page.locator("div").filter({ hasText: "abc1234" }).last();
  await expect(bar.getByText("Ada Owner")).toBeVisible();
  await expect(bar.getByText(/ada@acme.test/)).toHaveCount(0);
});

test("the listing shows before its history arrives, and stands if history never does", async ({
  page,
}) => {
  // On a real project's mirror the history walk behind the last-commit
  // column ran past the edge's timeout, and because the page asked for
  // the listing *with* history in one request, it sat on "Loading…"
  // holding a listing the server had already read. The listing is its
  // own request now; history fills a column when it answers.
  await mockApi(page);
  await mockBrowse(page);
  let releaseHistory: () => void = () => undefined;
  const held = new Promise<void>((resolve) => (releaseHistory = resolve));
  // A predicate, not a glob: the request may carry `at=` before
  // `history=1`, and a glob that does not match simply lets the plain
  // mock answer — which is exactly how this test passed against the
  // old, blocking view the first time it ran.
  let historyAsked = 0;
  await page.route(
    (u) =>
      /\/repos\/widget\/tree$/.test(u.pathname) &&
      u.searchParams.get("history") === "1",
    async (r) => {
      historyAsked += 1;
      await held;
      return r.fulfill({
        json: {
          ...TREE_ROOT,
          history_truncated: false,
          entries: TREE_ROOT.entries.map((e) => ({
            ...e,
            last_commit: {
              commit: TREE_ROOT.commit,
              message: "first commit",
              author: "Ada Owner <ada@acme.test> 1787406946 +0000",
            },
          })),
        },
      });
    },
  );
  await page.goto("/acme/widget");
  // The listing, while history is still held — and history *was* asked
  // for, so the hold is real and not a route that never matched.
  await expect(page.getByRole("button", { name: "src/" })).toBeVisible();
  await expect.poll(() => historyAsked).toBe(1);
  await expect(page.getByRole("button", { name: "README.md" })).toBeVisible();
  await expect(page.getByText("Loading…")).toHaveCount(0);
  // The commit bar at the top comes from the log, so "first commit" can
  // be on the page once already; the column adds one line per entry.
  const before = await page.getByText("first commit").count();
  // History lands: the column fills in place, one cell per entry.
  releaseHistory();
  await expect(page.getByText("first commit")).toHaveCount(
    before + TREE_ROOT.entries.length,
  );
});

test("browsing walks a tree, opens a file, and keeps the URL linkable", async ({
  page,
}) => {
  await signInAsPerson(page);
  await mockBrowse(page);

  // One click, not two: the repository's page *is* the file browser, the
  // way GitHub's is. There used to be a "Browse files" button because
  // the repo screen and the browser were different pages at different
  // addresses.
  await openRepo(page, "widget");

  // Directories first, then names, with sizes on blobs only.
  await expect(page.getByRole("button", { name: "src/" })).toBeVisible();
  await expect(page.getByRole("button", { name: "README.md" })).toBeVisible();

  // Into a directory, and the breadcrumb follows.
  await page.getByRole("button", { name: "src/" }).click();
  await expect(page).toHaveURL(/\/acme\/widget\/tree\/src$/);
  await expect(page.getByRole("button", { name: "main.rs" })).toBeVisible();

  // Back out through the breadcrumb, then open a file.
  await page
    .getByRole("navigation", { name: "Path" })
    .getByRole("button", { name: "widget" })
    .click();
  await expect(page).toHaveURL(/\/acme\/widget$/);
  await page.getByRole("button", { name: "README.md" }).click();
  await expect(page).toHaveURL(/\/acme\/widget\/tree\/README.md$/);

  // Prose opens on its preview — the README's heading, rendered — and
  // the source is one tab away. Behind that tab, the file with line
  // numbers, the whole point of the view. The code is rendered by the
  // library inside a shadow root, which Playwright's locators see
  // through; the line count is ours, in the light DOM.
  await expect(page.getByRole("heading", { name: "hello" })).toBeVisible();
  await expect(page.getByText("2 lines")).toBeVisible();
  await page
    .getByRole("tablist", { name: "File view" })
    .getByRole("tab", { name: "Code" })
    .click();
  await expect(
    page.locator("diffs-container").getByText("2", { exact: true }),
  ).toBeVisible();
  // And the repository's tree beside it, with this file selected.
  await expect(
    page.getByRole("treeitem", { name: "README.md", exact: true }),
  ).toHaveAttribute("aria-selected", "true");

  // The browser's back button works, because these are real URLs.
  await page.goBack();
  await expect(page).toHaveURL(/\/acme\/widget$/);
  await expect(page.getByRole("button", { name: "src/" })).toBeVisible();
});

test("clicking a file asks for the file, not for a tree that cannot exist", async ({
  page,
}) => {
  await signInAsPerson(page);
  await mockBrowse(page);
  // The listing already said README.md is a blob. Asking /tree for it
  // first — which is what a "try one, fall back to the other" browser
  // does — spends a round trip to be told 404, on every file anybody
  // opens, and writes that 404 into the operator's logs.
  const treeAsks = [];
  page.on("request", (r) => {
    const u = new URL(r.url());
    if (/\/repos\/widget\/tree\//.test(u.pathname)) treeAsks.push(u.pathname);
  });

  await openRepo(page, "widget");
  await page.getByRole("button", { name: "README.md" }).click();
  await expect(page.getByRole("heading", { name: "hello" })).toBeVisible();
  expect(treeAsks.filter((p) => p.endsWith("/README.md"))).toEqual([]);
  // A link somebody was sent has no listing behind it, so that one is
  // still allowed to guess — covered by the deep-link test below, which
  // reloads and so needs a session that survives one.
});

test("a file URL opens straight to that file when somebody else follows it", async ({
  page,
}) => {
  await mockApi(page);
  await mockBrowse(page);
  // No clicking: this is the link arriving in somebody's inbox.
  await page.goto("/acme/widget/tree/README.md");
  await expect(page.getByRole("heading", { name: "hello" })).toBeVisible();
  await expect(
    page.getByRole("navigation", { name: "Path" }).getByText("README.md"),
  ).toBeVisible();
  // The tree found the file too, with no listing to learn it from.
  await expect(
    page.getByRole("treeitem", { name: "README.md", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
});

test("the tree opens files and only expands directories", async ({
  page,
}) => {
  await mockApi(page);
  await mockBrowse(page);
  const treeAsks: string[] = [];
  page.on("request", (r) => {
    const u = new URL(r.url());
    if (/\/repos\/widget\/tree\//.test(u.pathname)) treeAsks.push(u.pathname);
  });
  await page.goto("/acme/widget/tree/README.md");
  await expect(page.getByRole("heading", { name: "hello" })).toBeVisible();

  // A directory opens in place. Leaving the file you were reading to
  // look at a listing is not what a click on a folder in a rail means.
  await page.getByRole("treeitem", { name: "src", exact: true }).click();
  await expect(page).toHaveURL(/\/acme\/widget\/tree\/README.md$/);
  await expect(
    page.getByRole("treeitem", { name: "main.rs", exact: true }),
  ).toBeVisible();

  // A file navigates — and asks for the file, not for a tree that
  // cannot exist: the rail knows what it is pointing at.
  await page.getByRole("treeitem", { name: "main.rs", exact: true }).click();
  await expect(page).toHaveURL(/\/acme\/widget\/tree\/src\/main.rs$/);
  await expect(page.getByText("fn main")).toBeVisible();
  expect(treeAsks.filter((p) => p.endsWith("/main.rs"))).toEqual([]);
});

test("a directory listing does not fetch the whole tree", async ({
  page,
}) => {
  // The recursive walk is the expensive read, and the listing is the
  // page everybody lands on; only a file page has a rail to feed.
  await mockApi(page);
  await mockBrowse(page);
  const asks = countTreeAsks(page);
  await page.goto("/acme/widget");
  await expect(page.getByRole("button", { name: "src/" })).toBeVisible();
  await page.getByRole("button", { name: "src/" }).click();
  await expect(page.getByRole("button", { name: "main.rs" })).toBeVisible();
  expect(asks()).toBe(0);
  await page.getByRole("button", { name: "main.rs" }).click();
  await expect(page.getByText("fn main")).toBeVisible();
  await expect.poll(asks).toBe(1);
});

test.describe("on a phone", () => {
  test.use({ viewport: { width: 390, height: 800 } });

  test("the file tree is behind a Files button", async ({ page }) => {
    // No room beside a file for a rail; and never offscreen, which is a
    // control nobody can reach. Closed means not rendered.
    await mockApi(page);
    await mockBrowse(page);
    await page.goto("/acme/widget/tree/README.md");
    await expect(page.getByRole("heading", { name: "hello" })).toBeVisible();
    await expect(page.getByRole("tree")).toHaveCount(0);
    const files = page.getByRole("button", { name: "Files", exact: true });
    await expect(files).toHaveAttribute("aria-expanded", "false");
    await files.click();
    await expect(page.getByRole("tree")).toBeVisible();
    await expect(
      page.getByRole("treeitem", { name: "README.md", exact: true }),
    ).toBeVisible();
  });
});

test("switching branch pins the revision, and the default unpins it", async ({
  page,
}) => {
  await mockApi(page);
  await mockBrowse(page);
  await page.goto("/acme/widget");
  await expect(page.getByRole("button", { name: "src/" })).toBeVisible();

  await page.getByLabel("Branch").click();
  await page.getByRole("option", { name: "side" }).click();
  await expect(page).toHaveURL(/\/acme\/widget\?at=side$/);

  // Choosing the default is the *absence* of a revision: a permalink to
  // "whatever is current" should stay current rather than pin itself to
  // today's branch name.
  await page.getByLabel("Branch").click();
  await page.getByRole("option", { name: "main (default)" }).click();
  await expect(page).toHaveURL(/\/acme\/widget$/);
});

test("a binary file says so instead of rendering as mojibake", async ({
  page,
}) => {
  await mockApi(page);
  await mockBrowse(page);
  await page.goto("/acme/widget/tree/logo.png");
  await expect(page.getByText(/binary, not shown/)).toBeVisible();
  await expect(page.getByText(/image\/png/)).toBeVisible();
});

test("a path that does not exist explains itself rather than hanging", async ({
  page,
}) => {
  await mockApi(page);
  await mockBrowse(page);
  await page.route("**/v1/orgs/*/repos/widget/files/nope*", (r) =>
    r.fulfill({ status: 404, json: { error: '"nope" not in this layout' } }),
  );
  await page.goto("/acme/widget/tree/nope");
  await expect(page.getByRole("alert")).toContainText("not in this layout");
});

// ---------------------------------------------------------------------
// Making a repository, and mirroring one.
//
// Until this, the dashboard could not create anything at all, and the
// mirror API asked for an installation id a person had to dig out of a
// GitHub settings URL. These cover the flow that replaces it: paste a
// URL, and let what the probe says decide what happens next.

/// Route the create/mirror/connect surface. Registered specific-last,
/// because Playwright matches newest-first.
async function mockCreate(
  page: Page,
  opts: {
    probe?: Record<string, unknown>;
    installations?: unknown[];
    repositories?: unknown[];
    status?: Record<string, unknown>[];
  } = {},
) {
  const statuses = opts.status ?? [
    { state: "ready", commit: "abc1234def", error: null },
  ];
  let nth = 0;
  await page.route("**/v1/orgs/*/origins/probe", (r) =>
    r.fulfill({
      json: opts.probe ?? {
        reachable: true,
        private: false,
        default_branch: "main",
        refs: 12,
        reason: null,
      },
    }),
  );
  await page.route("**/v1/orgs/*/github/install", (r) =>
    r.fulfill({
      json: {
        url: "https://github.com/apps/stratum/installations/new?state=stinst_x_y",
        state: "stinst_x_y",
        expires_in: 600,
      },
    }),
  );
  await page.route("**/v1/orgs/*/github/installations", (r) =>
    r.fulfill({ json: { installations: opts.installations ?? [] } }),
  );
  await page.route("**/v1/orgs/*/github/installations/*/repos*", (r) =>
    r.fulfill({ json: { repositories: opts.repositories ?? [] } }),
  );
  await page.route("**/v1/orgs/*/mirrors", (r) =>
    r.fulfill({
      status: 202,
      json: {
        repo: { ...REPOS[0], name: "widget", kind: "mirror" },
        clone_url: "https://stratum.test/acme/widget.git",
      },
    }),
  );
  await page.route("**/v1/orgs/*/repos/*/sync-status", (r) => {
    const s = statuses[Math.min(nth++, statuses.length - 1)];
    r.fulfill({
      json: {
        origin: "acme/widget",
        provider: "github",
        last_sync_at: null,
        clone_url: "https://stratum.test/acme/widget.git",
        ...s,
      },
    });
  });
}

test("a public origin mirrors from a pasted URL, with nothing to install", async ({
  page,
}) => {
  await mockApi(page);
  await mockCreate(page);
  await page.goto("/dashboard/new");

  await page.getByLabel("Repository URL").fill("github.com/acme/widget");
  await page.getByRole("button", { name: "Check origin" }).click();

  // The probe's answer, in words, and the action it unlocks.
  await expect(page.getByText(/Found it — 12 refs/)).toBeVisible();
  await page.getByRole("button", { name: "Mirror widget" }).click();

  // And then the thing that makes 202 bearable: the first sync is
  // followed, and it ends on a command you can paste.
  await expect(page.getByText(/Mirrored at abc1234/)).toBeVisible();
  await expect(
    page.getByText("git clone https://stratum.test/acme/widget.git"),
  ).toBeVisible();
});

test("a pasted public URL says it will be read-only until GitHub is connected", async ({
  page,
}) => {
  await mockApi(page);
  await mockCreate(page);
  await page.goto("/dashboard/new");
  await page.getByLabel("Repository URL").fill("github.com/acme/widget");
  await page.getByRole("button", { name: "Check origin" }).click();
  // A mirror made this way fetches as a stranger and cannot push as
  // one: said where the choice is made, with the way to do it instead.
  const note = page.getByTestId("pasted-url-read-only");
  await expect(note).toContainText("Read-only until you connect GitHub");
  await note.getByRole("button", { name: "Pick from your installations" }).click();
  await expect(
    page.getByRole("button", { name: "Install the GitHub app" }),
  ).toBeVisible();
});

test("a fresh mirror hands over a remote as well as a clone, when it can push", async ({
  page,
}) => {
  await mockApi(page);
  await mockCreate(page, {
    status: [
      {
        state: "ready",
        commit: "abc1234def",
        error: null,
        push: {
          forwarding: true,
          blocked: null,
          needs_permission: false,
          approve_url: null,
        },
      },
    ],
  });
  await page.goto("/dashboard/new");
  await page.getByLabel("Repository URL").fill("github.com/acme/widget");
  await page.getByRole("button", { name: "Check origin" }).click();
  await page.getByRole("button", { name: "Mirror widget" }).click();
  await expect(page.getByText(/Mirrored at abc1234/)).toBeVisible();
  const push = page.getByTestId("push-through");
  await expect(push).toContainText(
    "git remote add weft https://stratum.test/acme/widget.git",
  );
  await expect(push).toContainText("forwarded to acme/widget");
});

test("a fresh mirror whose installation cannot push says what to approve", async ({
  page,
}) => {
  await mockApi(page);
  await mockCreate(page, {
    status: [
      {
        state: "ready",
        commit: "abc1234def",
        error: null,
        push: {
          forwarding: false,
          blocked: "this GitHub App installation cannot push to this repository …",
          needs_permission: true,
          approve_url: "https://github.com/settings/installations/4007",
        },
      },
    ],
  });
  await page.goto("/dashboard/new");
  await page.getByLabel("Repository URL").fill("github.com/acme/widget");
  await page.getByRole("button", { name: "Check origin" }).click();
  await page.getByRole("button", { name: "Mirror widget" }).click();
  await expect(page.getByText(/Mirrored at abc1234/)).toBeVisible();
  await expect(page.getByTestId("push-through")).toHaveCount(0);
  const warn = page.getByTestId("push-needs-permission");
  await expect(warn).toContainText("approve Contents: write on GitHub");
  await expect(warn.getByRole("link")).toHaveAttribute(
    "href",
    "https://github.com/settings/installations/4007",
  );
});

test("the picker says which installation cannot push yet", async ({ page }) => {
  await mockApi(page);
  await mockCreate(page, {
    installations: [
      {
        installation_id: "4007",
        provider: "github",
        account: "prepush-inc",
        created_at: 0,
        detail: {
          account: "prepush-inc",
          target_type: "Organization",
          contents_write: false,
          push_ready: false,
          approve_url:
            "https://github.com/organizations/prepush-inc/settings/installations/4007",
          suspended: false,
        },
      },
    ],
    repositories: [{ full_name: "prepush-inc/widget", private: true, size: 12 }],
  });
  await page.goto("/dashboard/new");
  await page.getByRole("button", { name: "Pick from your installations" }).click();
  const note = page.getByTestId("picker-push-note");
  await expect(note).toContainText("cannot push back yet");
  await expect(note.getByRole("link", { name: "Approve Contents: write on GitHub" })).toHaveAttribute(
    "href",
    "https://github.com/organizations/prepush-inc/settings/installations/4007",
  );
  // The repository is still offered: it mirrors fine, it just does not
  // push until the owner approves.
  await expect(page.getByRole("button", { name: "prepush-inc/widget" })).toBeVisible();
});

test("a mirror's page says where its pushes go, and offers the way to fix it", async ({
  page,
}) => {
  await signIn(page);
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await mockBrowse(page);
  const mirror = (push: unknown) =>
    page.route("**/v1/orgs/acme/repos/widget", (r) =>
      r.fulfill({
        json: {
          ...REPOS.repos[0],
          viewer_admin: true,
          viewer_write: true,
          push,
        },
      }),
    );

  // Forwarding: one line, naming the origin. Not an alert.
  await mirror({
    forwarding: true,
    blocked: null,
    needs_permission: false,
    approve_url: null,
  });
  await page.goto("/acme/widget");
  await expect(page.getByTestId("mirror-push")).toContainText(
    "forwarded to acme/widget",
  );
  await expect(page.getByRole("status")).toHaveCount(0);

  // An installation that predates the permission: the approve link.
  await mirror({
    forwarding: false,
    blocked: "this GitHub App installation cannot push to this repository …",
    needs_permission: true,
    approve_url: "https://github.com/settings/installations/4007",
  });
  await page.goto("/acme/widget");
  const approve = page.getByTestId("mirror-push");
  await expect(approve).toContainText("Pushes to this mirror are refused.");
  await expect(
    approve.getByRole("link", { name: "Approve Contents: write on GitHub" }),
  ).toHaveAttribute("href", "https://github.com/settings/installations/4007");

  // No credential at all: an admin attaches a connected installation
  // here, and the page shows what the server answered.
  await page.route("**/v1/orgs/*/github/installations", (r) =>
    r.fulfill({
      json: {
        installations: [
          {
            installation_id: "4001",
            provider: "github",
            account: "acme-inc",
            created_at: 0,
            detail: {
              account: "acme-inc",
              target_type: "Organization",
              contents_write: true,
              push_ready: true,
              approve_url:
                "https://github.com/organizations/acme-inc/settings/installations/4001",
              suspended: false,
            },
          },
        ],
      },
    }),
  );
  let patched: unknown = null;
  await page.route("**/v1/orgs/acme/repos/widget", (r) => {
    if (r.request().method() === "PATCH") {
      patched = r.request().postDataJSON();
      return r.fulfill({
        json: {
          ...REPOS.repos[0],
          origin_installation: "4001",
          viewer_admin: true,
          viewer_write: true,
          push: {
            forwarding: true,
            blocked: null,
            needs_permission: false,
            approve_url: null,
          },
        },
      });
    }
    return r.fulfill({
      json: {
        ...REPOS.repos[0],
        viewer_admin: true,
        viewer_write: true,
        push: {
          forwarding: false,
          blocked:
            "this mirror has no credential that can push to its origin (acme/widget); push there directly, or connect the GitHub App and attach the installation to this mirror",
          needs_permission: false,
          approve_url: null,
        },
      },
    });
  });
  await page.goto("/acme/widget");
  const none = page.getByTestId("mirror-push");
  await expect(none).toContainText("no credential that can push");
  await none
    .getByRole("button", { name: "Attach and forward pushes" })
    .click();
  await expect(page.getByTestId("mirror-push")).toContainText(
    "forwarded to acme/widget",
  );
  expect(patched).toEqual({ installation_id: "4001" });
});

test("a private origin offers the GitHub install, and a typo does not", async ({
  page,
}) => {
  await mockApi(page);
  await mockCreate(page, {
    probe: {
      reachable: false,
      private: true,
      default_branch: null,
      refs: 0,
      reason: "this looks private — connect GitHub to mirror it",
    },
  });
  await page.goto("/dashboard/new");
  await page.getByLabel("Repository URL").fill("github.com/acme/ledger");
  await page.getByRole("button", { name: "Check origin" }).click();
  await expect(page.getByText(/this looks private/)).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Connect GitHub" }),
  ).toBeVisible();

  // A typo is not a private repository, and offering an App install for
  // one sends somebody through a flow that cannot help them.
  await page.unroute("**/v1/orgs/*/origins/probe");
  await page.route("**/v1/orgs/*/origins/probe", (r) =>
    r.fulfill({
      json: {
        reachable: false,
        private: false,
        default_branch: null,
        refs: 0,
        reason: "that origin is not reachable as a git repository",
      },
    }),
  );
  await page.getByLabel("Repository URL").fill("github.com/acme/typpo");
  await page.getByRole("button", { name: "Check origin" }).click();
  await expect(
    page.getByText(/not reachable as a git repository/),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Connect GitHub" }),
  ).toHaveCount(0);
});

test("picking a private repository never shows an installation id", async ({
  page,
}) => {
  await mockApi(page);
  await mockCreate(page, {
    installations: [
      {
        installation_id: "4001",
        provider: "github",
        account: "acme-inc",
        created_at: Date.now(),
      },
    ],
    repositories: [
      {
        full_name: "acme-inc/ledger",
        private: true,
        default_branch: "main",
        description: null,
        size: 4096,
      },
      {
        full_name: "acme-inc/atlas",
        private: true,
        default_branch: "main",
        description: null,
        size: 8192,
      },
    ],
  });
  await page.goto("/dashboard/new");
  await page
    .getByRole("button", { name: "Pick from your installations" })
    .click();

  await expect(
    page.getByRole("button", { name: "acme-inc/ledger" }),
  ).toBeVisible();
  // The whole point: the id is never seen or typed.
  await expect(page.getByText("4001")).toHaveCount(0);

  // The filter is what makes a hundred repositories usable.
  await page.getByLabel("Find a repository").fill("atlas");
  await expect(
    page.getByRole("button", { name: "acme-inc/ledger" }),
  ).toHaveCount(0);
  await page.getByRole("button", { name: "acme-inc/atlas" }).click();
  await expect(page.getByText(/Mirrored at abc1234/)).toBeVisible();
});

test("the first sync is followed until it settles, and a failure says why", async ({
  page,
}) => {
  await mockApi(page);
  await mockCreate(page, {
    status: [
      { state: "syncing", commit: null, error: null },
      { state: "syncing", commit: null, error: null },
      { state: "failed", commit: null, error: "origin closed the connection" },
    ],
  });
  await page.goto("/dashboard/new");
  await page.getByLabel("Repository URL").fill("github.com/acme/widget");
  await page.getByRole("button", { name: "Check origin" }).click();
  await page.getByRole("button", { name: "Mirror widget" }).click();

  // It says it is working rather than looking stuck…
  await expect(page.getByText(/Mirroring — reading refs/)).toBeVisible();
  // …and when it stops, it says what went wrong instead of a spinner
  // that never ends.
  await expect(page.getByText("The first sync failed.")).toBeVisible();
  await expect(page.getByText("origin closed the connection")).toBeVisible();
  await expect(page.getByText(/git clone/)).toHaveCount(0);
});

test("an org with nothing in it leads somewhere", async ({ page }) => {
  await mockApi(page);
  await page.unroute("**/v1/orgs/acme/repos?**");
  await page.route("**/v1/orgs/acme/repos?**", (r) =>
    r.fulfill({ json: { repos: [] } }),
  );
  await page.goto("/dashboard/");
  await expect(page.getByText("No repositories yet")).toBeVisible();
  await page
    .getByRole("button", { name: "Create your first repository" })
    .click();
  await expect(page).toHaveURL(/\/dashboard\/new$/);
  await expect(page.getByLabel("Repository URL")).toBeVisible();
});

test("an empty repository is two fields and a clone command", async ({
  page,
}) => {
  await mockApi(page);
  await mockCreate(page);
  await page.route("**/v1/orgs/*/repos", (r) =>
    r.fulfill({
      status: 201,
      json: { ...REPOS[0], name: "scratch", kind: "native" },
    }),
  );
  // An empty repository has no sync to follow, so the screen reads the
  // repo itself for its clone URL. Registered after the collection
  // route because Playwright matches newest-first.
  await page.route("**/v1/orgs/*/repos/scratch", (r) =>
    r.fulfill({
      json: {
        ...REPOS[0],
        name: "scratch",
        kind: "native",
        clone_url: "https://stratum.test/acme/scratch.git",
      },
    }),
  );
  await page.goto("/dashboard/new");
  await page.getByRole("tab", { name: "Empty repository" }).click();
  await page.getByLabel("Repository name").fill("scratch");
  await page.getByRole("button", { name: "Create repository" }).click();
  await expect(page.getByText(/git clone/)).toBeVisible();
});

test("the button that leaves for GitHub says so, and is not a second Connect", async ({
  page,
}) => {
  await mockApi(page);
  await mockCreate(page, {
    probe: {
      reachable: false,
      private: true,
      default_branch: null,
      refs: 0,
      reason: "this looks private — connect GitHub to mirror it",
    },
    installations: [],
  });
  await page.goto("/dashboard/new");
  await page.getByLabel("Repository URL").fill("github.com/acme/ledger");
  await page.getByRole("button", { name: "Check origin" }).click();
  await page.getByRole("button", { name: "Connect GitHub" }).click();

  // The screen this lands on used to carry a second button with the
  // same words, so "Connect GitHub" led to "Connect GitHub" and read as
  // a click that had not worked. Whatever leaves for GitHub must not be
  // called the same thing as whatever got you here.
  await expect(
    page.getByRole("button", { name: "Install the GitHub app" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Connect GitHub" }),
  ).toHaveCount(0);
});

// ---------------------------------------------------------------- search

const HITS = {
  repos: [
    {
      id: "01aaa",
      org_id: "01org",
      org: "acme",
      name: "widget",
      description: "the fast one",
      kind: "mirror",
      created_at: Date.now(),
    },
    {
      id: "01zzz",
      org_id: "01other",
      org: "zeta",
      name: "atlas",
      description: null,
      kind: "native",
      created_at: Date.now(),
    },
  ],
  next: null,
};

/// The search endpoint, recording what the client actually asked for.
async function mockSearch(page: Page, asked: URLSearchParams[]) {
  await page.route("**/v1/search/repos*", (r) => {
    const u = new URL(r.request().url());
    asked.push(u.searchParams);
    const q = (u.searchParams.get("q") ?? "").toLowerCase();
    const repos = HITS.repos.filter(
      (h) =>
        !q ||
        h.name.includes(q) ||
        h.org.includes(q) ||
        (h.description ?? "").toLowerCase().includes(q),
    );
    return r.fulfill({ json: { repos, next: null } });
  });
}

test("the header search box goes to a result set with an address", async ({
  page,
}) => {
  const asked: URLSearchParams[] = [];
  await mockApi(page);
  await mockSearch(page, asked);
  await page.goto("/dashboard/");
  await page.getByLabel("Search repositories").first().fill("widget");
  await page.getByLabel("Search repositories").first().press("Enter");

  // The query is in the URL, which is what makes a result set something
  // you can send to somebody.
  await expect(page).toHaveURL(/\/dashboard\/search\?q=widget/);
  await expect(
    page.getByRole("listitem").filter({ hasText: "widget" }),
  ).toBeVisible();
  await expect(page.getByText("the fast one")).toBeVisible();
  // ...and it went to the server as a query, not as a path.
  expect(asked.at(-1)?.get("q")).toBe("widget");
});

test("a search reaching another namespace says so before opening it", async ({
  page,
}) => {
  await mockApi(page);
  await mockSearch(page, []);
  await page.goto("/dashboard/search?q=atlas");
  const hit = page.getByRole("listitem").filter({ hasText: "atlas" });
  await expect(hit).toBeVisible();
  // A hit outside the namespace in the header is labelled, because
  // opening it switches the whole dashboard.
  await expect(hit.getByText("another namespace")).toBeVisible();
  // The namespace and the name are separate nodes: run together they
  // read as one word to anything that strips markup.
  await expect(hit.getByText("zeta", { exact: true })).toBeVisible();
  await expect(hit.getByText("atlas", { exact: true })).toBeVisible();
});

test("a second search clears the first one's rows before answering", async ({
  page,
}) => {
  // The manual pass found this: search, then search for something else,
  // and the previous hits stay on screen with nothing saying a new
  // answer is in flight. That does not read as "loading" — it reads as
  // "the same repositories matched", which is a wrong answer rather
  // than a slow one.
  let release: (() => void) | null = null;
  await mockApi(page);
  await page.route("**/v1/search/repos*", async (r) => {
    const q = new URL(r.request().url()).searchParams.get("q") ?? "";
    if (q === "slow") {
      await new Promise<void>((resolve) => {
        release = resolve;
      });
      return r.fulfill({ json: { repos: [], next: null } });
    }
    return r.fulfill({ json: { repos: [HITS.repos[0]], next: null } });
  });
  await page.goto("/dashboard/search?q=widget");
  await expect(page.getByText("the fast one")).toBeVisible();

  // Second search, held open by the route above: while it is in flight
  // the first answer must be gone.
  await page.getByLabel("Search repositories").last().fill("slow");
  await page
    .getByRole("button", { name: "Search", exact: true })
    .last()
    .click();
  await expect(page.getByText("Loading…")).toBeVisible();
  await expect(page.getByText("the fast one")).toHaveCount(0);

  release!();
  await expect(page.getByText(/Nothing you can see matches/)).toBeVisible();
});

test("a search that matches nothing says so rather than looking broken", async ({
  page,
}) => {
  await mockApi(page);
  await mockSearch(page, []);
  await page.goto("/dashboard/search?q=nothinglikethis");
  await expect(page.getByText(/Nothing you can see matches/)).toBeVisible();
});

// "a description is edited in place" used to live here, against the
// dashboard's own repo screen. That screen is gone — a repository has
// one page now — and what it asserted is already held, harder, where
// the control actually lives: `repo-settings.spec.ts` pins the
// description PATCH to its exact body. Porting it here would have been
// a second, weaker copy of a test that already exists.

test("a file page shows who last touched it, and only its own history", async ({
  page,
}) => {
  const asked: (string | null)[] = [];
  await mockApi(page);
  await mockBrowse(page);
  await page.route("**/v1/orgs/*/repos/widget/log*", (r) => {
    const path = new URL(r.request().url()).searchParams.get("path");
    asked.push(path);
    if (!path) return r.fulfill({ json: LOG });
    return r.fulfill({
      json: FILE_LOG[path] ?? { entries: [], next_after: null },
    });
  });
  await page.goto("/acme/widget/tree/README.md");
  await expect(page.getByText("second line")).toBeVisible();

  // The last-commit bar names the person and the subject, not the body.
  await expect(page.getByText("Ada Owner").first()).toBeVisible();
  await expect(page.getByText("first commit").first()).toBeVisible();

  // The history it asked for was this file's, not the repository's.
  // Sending no `path` would return the whole log and look right on a
  // fixture this small, which is exactly why this is asserted.
  expect(asked).toContain("README.md");

  await page.getByText("History for this file").click();
  const rows = page.getByRole("listitem").filter({ hasText: "aaa1111" });
  await expect(rows).toBeVisible();
  await expect(rows.getByText("added")).toBeVisible();
});

test("switching to an older version pins it, and says how to get back", async ({
  page,
}) => {
  await mockApi(page);
  await mockBrowse(page);
  await page.goto("/acme/widget/tree/README.md");
  await expect(page.getByText("second line")).toBeVisible();
  await page.getByText("History for this file").click();

  await page.getByRole("button", { name: /View this file at aaa1111/ }).click();
  // The content really changed — a version switcher that re-renders the
  // same bytes is a link that does nothing.
  await expect(page.getByText("the first version")).toBeVisible();
  await expect(page.getByText(/Viewing this file at/)).toBeVisible();
  // ...and the revision is in the URL, so the old version is sendable.
  await expect(page).toHaveURL(/at=aaa1111/);

  await page.getByRole("button", { name: "Back to latest" }).click();
  await expect(page.getByText("second line")).toBeVisible();
  await expect(page.getByText(/Viewing this file at/)).toHaveCount(0);
  await expect(page).not.toHaveURL(/at=/);
});

// The sidebar made settings addressable; these pin the address book.

test("bare /settings corrects itself to the first visible section", async ({
  page,
}) => {
  await mockApi(page);
  await page.goto("/dashboard/settings");
  await expect(page).toHaveURL(/\/dashboard\/settings\/members$/);
  await expect(page.getByText("dev@acme.test")).toBeVisible();
  // replace, not push: the bad address never entered history, so going
  // back leaves settings entirely instead of bouncing off the redirect.
  await page.goBack();
  await expect(page).not.toHaveURL(/\/settings/);
});

test("a viewer deep-linking an admin section lands on their own first one", async ({
  page,
}) => {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({
      json: { ...ME, orgs: [{ id: "01org", name: "acme", role: "viewer" }] },
    }),
  );
  await page.goto("/dashboard/settings/members");
  await expect(page).toHaveURL(/\/dashboard\/settings\/teams$/);
});

test("an unknown settings section corrects itself", async ({ page }) => {
  await mockApi(page);
  await page.goto("/dashboard/settings/nonsense");
  await expect(page).toHaveURL(/\/dashboard\/settings\/members$/);
});

test("a settings section is a link somebody can send", async ({ page }) => {
  await mockApi(page);
  await page.goto("/dashboard/settings/tokens");
  await expect(page.getByText("laptop")).toBeVisible();
  await expect(page.getByRole("link", { name: "Tokens" })).toBeVisible();
});

test("the sidebar collapse survives a reload", async ({ page }) => {
  await mockApi(page);
  await page.goto("/dashboard/");
  await expect(page.getByLabel("Search repositories").first()).toBeVisible();
  await page.getByRole("button", { name: "Toggle sidebar" }).click();
  await expect(page.getByLabel("Search repositories").first()).toBeHidden();
  await page.reload();
  await expect(page.getByText("Requests today")).toBeVisible();
  // The cookie remembered the rail.
  await expect(page.getByLabel("Search repositories").first()).toBeHidden();
  await page.getByRole("button", { name: "Toggle sidebar" }).click();
  await expect(page.getByLabel("Search repositories").first()).toBeVisible();
});

test("a server error that merely contains 401 is not a sign-out", async ({
  page,
}) => {
  // The `401` substring bug, a second time — the first cost us a repo
  // named `rfc-403` being reported to users as private. Here the org
  // overview decided somebody's session had expired because the error
  // *text* happened to contain those three digits, threw them back to
  // the login form, and lost whatever they were doing. An authorization
  // outcome is a status code; it is never a search for digits in prose.
  await signIn(page);
  await page.route("**/v1/orgs/acme/repos*", (r) =>
    r.fulfill({
      status: 503,
      json: { error: "quota: 401 repositories exceeds the plan limit" },
    }),
  );
  await page.goto("/dashboard/");
  await expect(
    page.getByText("quota: 401 repositories exceeds the plan limit"),
  ).toBeVisible();
  // Still signed in: the credential form is not what an unrelated
  // server error looks like.
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toHaveCount(0);
});
