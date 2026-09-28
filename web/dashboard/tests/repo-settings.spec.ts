// The repository's Settings tab: who may see it, and what it can
// actually do.
//
// Two properties are load-bearing and each is asserted in both
// directions, because a rule with two halves and a test constraining one
// is how this repository has repeatedly shipped a bug next to a green
// suite.
//
//   * The tab exists for an admin and does **not** exist for anybody
//     else — not disabled, absent — and the address behind it answers
//     the same 404 a repository gives somebody who may not read it.
//   * Every destructive control is gated on typing the repository's
//     name. Asserting the confirmed path alone would pass against a
//     button that deletes on the first click, so each of those tests
//     first asserts the action is refused before the name is typed.
//
// "May I administer this?" is answered by `viewer_admin` on the
// repository row, which is the server's own answer from the same `authx`
// refinement the writes use. It used to be inferred client-side by
// probing `GET …/access` and reading success as yes — a different
// question, requiring org-wide admin where the writes accept a per-repo
// grant, so a per-repo admin was shown no settings surface at all. The
// tests below still hold both ends of that: the flag decides the tab,
// and `orgAdmin: false` is how a per-repo admin is expressed.
//
// The catch-all `**/v1/**` 404 is registered FIRST: an unmocked call
// otherwise proxies to whatever is listening on :8080, so the suite
// would be hermetic only when nobody had the manual stack up.

import { expect, test, type Page } from "@playwright/test";
import { ME, signIn } from "./fixtures";

const WIDGET = {
  id: "01aaa",
  org_id: "01org",
  name: "widget",
  description: "the fast one",
  kind: "native",
  default_branch: "main",
  origin_url: null,
  last_sync_at: null,
  last_synced_commit: null,
  sync_error: null,
  created_at: Date.now() - 86_400_000,
  clone_url: "https://x/acme/widget.git",
  ssh_clone_url: null,
  viewer_admin: true,
  viewer_member: true,
  // Not decoration: the Fork control prints this count, and it can
  // only print it once the row has arrived. It is what the absence
  // assertions below wait on — see `rowHasLanded`.
  fork_count: 7,
};

const ACCESS_OK = { people: [], teams: [] };

/// A repository page with nothing mocked but what the test names.
///
/// `admin` sets `viewer_admin` on the row, which is the one thing the
/// Settings tab is gated on.
///
/// `orgAdmin` is a **separate** knob on purpose, and defaults to
/// following `admin`. The two are different authorities on the server —
/// `viewer_admin` accepts a per-repo grant, `GET …/access` requires
/// org-wide admin — and a fixture that could not express "admin here,
/// not org-wide" could not express the case this whole gate exists for.
async function mockRepo(
  page: Page,
  opts: {
    repo?: Record<string, unknown>;
    admin: boolean;
    /// Whether the caller is *also* an org-wide admin, which is what
    /// `GET …/access` answers. Defaults to `admin`.
    orgAdmin?: boolean;
    /// Hold the repository row open this long, so the page's
    /// "still asking" state is a window a test can look at rather than
    /// a race.
    rowDelayMs?: number;
    /// Every GET of the row, with its request headers — for asserting
    /// which credential went out with it.
    onRowRead?: (headers: Record<string, string>) => void;
    onPatch?: (body: unknown) => void;
    /// Make the PATCH fail with this status and sentence, so a test can
    /// assert the page reports the *server's* words rather than its own
    /// paraphrase of them.
    patchStatus?: number;
    patchError?: string;
    onDelete?: () => void;
  },
) {
  const repo = { ...WIDGET, viewer_admin: opts.admin, ...(opts.repo ?? {}) };
  const orgAdmin = opts.orgAdmin ?? opts.admin;
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/access", (r) =>
    orgAdmin
      ? r.fulfill({ status: 200, json: ACCESS_OK })
      : r.fulfill({ status: 403, json: { error: "not an org admin" } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/protections", (r) =>
    r.fulfill({ status: 200, json: { protections: [] } }),
  );
  await page.route("**/v1/orgs/acme/teams", (r) =>
    r.fulfill({ status: 200, json: { teams: [] } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", async (r) => {
    const method = r.request().method();
    if (method === "PATCH") {
      const body = r.request().postDataJSON();
      opts.onPatch?.(body);
      if (opts.patchStatus && opts.patchStatus >= 400) {
        return r.fulfill({
          status: opts.patchStatus,
          json: { error: opts.patchError ?? "refused" },
        });
      }
      return r.fulfill({
        status: 200,
        json: { ...repo, ...(body as Record<string, unknown>) },
      });
    }
    if (method === "DELETE") {
      opts.onDelete?.();
      return r.fulfill({ status: 204, body: "" });
    }
    opts.onRowRead?.(r.request().headers());
    if (opts.rowDelayMs) {
      await new Promise((done) => setTimeout(done, opts.rowDelayMs));
    }
    return r.fulfill({ status: 200, json: repo });
  });
}

const repoTabs = (page: Page) =>
  page.getByRole("navigation", { name: "Repository" }).getByRole("link");

const settingsTab = (page: Page) =>
  page
    .getByRole("navigation", { name: "Repository" })
    .getByRole("link", { name: "Settings" });

const insightsTab = (page: Page) =>
  page
    .getByRole("navigation", { name: "Repository" })
    .getByRole("link", { name: "Insights" });

/// Wait until the repository row has actually rendered.
///
/// Every "no Settings tab" assertion needs this in front of it, and the
/// reason cost a green run: a web-first assertion retries until it
/// passes, so it passes at the **first** instant it holds — and "there
/// is no Settings tab" holds trivially while the row is still in
/// flight. `toHaveCount(0)` and even a full `toHaveText([...])` of the
/// three-tab strip both went green against a build that gave the tab to
/// everybody, because both were satisfied before the answer arrived.
///
/// The fork count is drawn from the row and from nothing else, so it is
/// the observable that says "the answer is here now". After it, an
/// absence can be read once, without retries, and mean something.
async function rowHasLanded(page: Page) {
  await expect(
    page.getByRole("button", { name: /fork this repository/i }),
  ).toContainText("7");
}

// ---------------------------------------------------------------------
// Who sees the tab.

test("an admin sees the Settings tab", async ({ page }) => {
  await signIn(page);
  await mockRepo(page, { admin: true });
  await page.goto("/acme/widget");

  await expect(repoTabs(page).first()).toBeVisible();
  await expect(settingsTab(page)).toBeVisible();
  await expect(settingsTab(page)).toHaveAttribute(
    "href",
    "/acme/widget/settings",
  );
});

test("a viewer sees Insights but no Settings tab at all", async ({ page }) => {
  await signIn(page);
  // The `viewer` role: on the inside, allowed to change nothing. The two
  // gates exist to tell this person apart from somebody with no role
  // here on one side and an admin on the other, so one page load asserts
  // both — a repository's traffic is the members' to read, its settings
  // the admins' to change.
  await mockRepo(page, { admin: false });
  await page.goto("/acme/widget");

  await rowHasLanded(page);
  // Counted reads, taken once, after the answer is in. Absent, not
  // disabled: a greyed-out tab still tells somebody who may not
  // administer this repository that there is a surface here.
  expect(await repoTabs(page).allInnerTexts()).toEqual([
    "Code",
    "Issues",
    "Changes",
    "Checks",
    "Insights",
  ]);
  await expect(insightsTab(page)).toHaveAttribute(
    "href",
    "/acme/widget/insights",
  );
  expect(await settingsTab(page).count()).toBe(0);
  expect(
    await page.getByRole("link", { name: "Settings", disabled: true }).count(),
  ).toBe(0);
});

/// Insights is gated on exactly what Settings is gated on, and both
/// halves have to be checked separately.
///
/// The tab being absent is the *offer* withdrawn; the page being absent
/// is the surface withdrawn. Only the second is a leak, and a build
/// could get the first right and the second wrong — the repository's
/// traffic is the sort of thing a competitor reads and a maintainer
/// assumes is theirs.
test("an admin gets both, Insights first", async ({ page }) => {
  await signIn(page);
  await mockRepo(page, { admin: true });
  await page.goto("/acme/widget");

  // `allInnerTexts()` is a one-shot read — no web-first retry — so it has
  // to come after the row, like every other counted read in this file.
  // Without it this asserted on a strip that was still three tabs short:
  // Settings waits on `viewer_admin`, Insights on `viewer_member`, and
  // Changes on knowing the repository is not a mirror, so *all three*
  // are absent until the row lands. It passed on a fast machine and went
  // red on CI, which is the whole reason `rowHasLanded` exists.
  await rowHasLanded(page);
  expect(await repoTabs(page).allInnerTexts()).toEqual([
    "Code",
    "Issues",
    "Changes",
    "Checks",
    "Insights",
    "Settings",
  ]);
});

test("a reader the server does not count as a member gets no Insights, tab or page", async ({
  page,
}) => {
  // `viewer_member` is the server's answer, and the gate is only as good
  // as its "no" half: a reader it says holds no role here may read the
  // code and not how often it is cloned or how many bytes it serves.
  await signIn(page);
  await mockRepo(page, {
    admin: false,
    repo: { viewer_member: false },
  });
  await page.goto("/acme/widget");

  await rowHasLanded(page);
  expect(await insightsTab(page).count()).toBe(0);
  expect(await repoTabs(page).allInnerTexts()).toEqual([
    "Code",
    "Issues",
    "Changes",
    "Checks",
  ]);

  // The half that matters: the address is typable whether or not a tab
  // points at it. Not "forbidden" — the same answer a repository gives
  // somebody who may not read it, because telling somebody they may not
  // see a page tells them there is a page to come back for.
  await page.goto("/acme/widget/insights");
  await expect(page.getByText(/We couldn’t find/)).toBeVisible();
  await expect(page.getByText("Clone p50 / p99")).toHaveCount(0);
});

test("an admin on this repository alone sees the tab and a working page", async ({
  page,
}) => {
  // The case the server's `viewer_admin` exists for, and the one no
  // client-side probe could get right. This person holds `admin` on
  // this repository through a grant and is an ordinary member of the
  // org — so `GET …/access`, which needs org-wide admin, refuses them,
  // while `PATCH …/repos/:repo` and the protections routes do not.
  await signIn(page);
  await mockRepo(page, { admin: true, orgAdmin: false });
  await page.goto("/acme/widget/settings");

  await expect(settingsTab(page)).toBeVisible();
  // Not a tab onto an empty page: the two panels whose endpoints accept
  // a per-repo grant are both here and both usable.
  await expect(page.getByLabel("Description")).toBeVisible();
  await expect(page.getByText("Branch policy")).toBeVisible();
  await expect(page.getByText("Danger Zone")).toBeVisible();
  // And the one panel whose endpoint really does need org-wide admin
  // renders nothing rather than a permanent error, which is its own
  // rule and not this gate's.
  await expect(page.getByRole("heading", { name: "Access" })).toHaveCount(0);
});

test("a reader without admin sees no tab, and nothing is asked on their behalf", async ({
  page,
}) => {
  let probes = 0;
  await signIn(page);
  await mockRepo(page, { admin: false });
  await page.route("**/v1/orgs/acme/repos/widget/access", (r) => {
    probes += 1;
    return r.fulfill({ status: 403, json: { error: "not an org admin" } });
  });
  await page.goto("/acme/widget");

  await rowHasLanded(page);
  expect(await repoTabs(page).allInnerTexts()).toEqual([
    "Code",
    "Issues",
    "Changes",
    "Checks",
    "Insights",
  ]);
  expect(await settingsTab(page).count()).toBe(0);
  // The row already said. Asking an admin-only endpoint on the most
  // visited page in the product — for an answer it carries — is a round
  // trip and a 403 in the log for every visitor, and it is how this
  // gate got the per-repo case wrong in the first place.
  expect(probes).toBe(0);
});

// Both ways of being signed in, asserted separately.
//
// The tab is now decided server-side, which moves the credential
// question one step back rather than removing it: the row is only
// answered for *this viewer* if the request carries this viewer's
// credential. `useRepoRow` builds `viewerSession(owner, token)` for
// exactly that reason; its predecessor sent an empty token, which is
// what silently showed a token holder less of their own work. One test per credential, because a single test can only ever
// hold up one half of that.

test("a token session sends its token with the row, and gets the tab", async ({
  page,
}) => {
  const seen: Record<string, string>[] = [];
  // `signIn` is the token flow: it pastes an API token and stores it.
  await signIn(page);
  await mockRepo(page, { admin: true, onRowRead: (h) => seen.push(h) });
  // Say the quiet part explicitly rather than leaning on the catch-all:
  // this caller has no person-session at all, so `me` is null and the
  // token is the only thing that can identify them.
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.goto("/acme/widget");

  await expect(settingsTab(page)).toBeVisible();
  expect(seen.length).toBeGreaterThan(0);
  expect(seen[0]["authorization"]).toBe("Bearer weft_test_token");
});

test("a cookie session sends no bearer header, and gets the tab", async ({
  page,
}) => {
  const seen: Record<string, string>[] = [];
  // The other half: a person signed in with a password. Nothing is in
  // localStorage — the cookie is HttpOnly and the page cannot read it —
  // so `/auth/me` answering is the whole proof of who this is, and the
  // browser attaches the credential itself.
  await mockRepo(page, { admin: true, onRowRead: (h) => seen.push(h) });
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 200, json: ME }),
  );
  await page.goto("/acme/widget");

  await expect(settingsTab(page)).toBeVisible();
  expect(seen.length).toBeGreaterThan(0);
  // Not `Bearer ` with nothing after it, which a server reads as a
  // malformed credential rather than as none at all.
  expect(seen[0]["authorization"]).toBeUndefined();
});

test("the settings address answers 404 to a non-admin", async ({ page }) => {
  await signIn(page);
  await mockRepo(page, { admin: false });
  await page.goto("/acme/widget/settings");

  await expect(page.getByText("404", { exact: true })).toBeVisible();
  // Same page as a repository that does not exist: "forbidden" would
  // tell somebody there is an admin surface here to come back for.
  await expect(page.getByText(/We couldn’t find/)).toBeVisible();
  await expect(page.getByText("Danger Zone")).toHaveCount(0);
});

test("the settings page renders for an admin who typed the address", async ({
  page,
}) => {
  await signIn(page);
  await mockRepo(page, { admin: true });
  await page.goto("/acme/widget/settings");

  await expect(
    page.getByRole("heading", { name: "Settings", exact: true }),
  ).toBeVisible();
  await expect(page.getByText("Danger Zone")).toBeVisible();
});

test("the page does not flash 404 at the admin while the row is in flight", async ({
  page,
}) => {
  // The answer arrives with the repository row, so on the first render
  // it is not yet known. Treating "not yet" as "not an admin" shows
  // this person — the one the page is for — "we couldn't find it" for
  // as long as the round trip takes. Held open for a second, that is
  // not a flicker, it is the page.
  await signIn(page);
  await mockRepo(page, { admin: true, rowDelayMs: 1000 });
  await page.goto("/acme/widget/settings");

  // Anchor on this render before asserting an absence: the masthead is
  // drawn from the address, not from the row, so it is there at once.
  await expect(
    page.getByRole("link", { name: "widget", exact: true }).first(),
  ).toBeVisible();
  // A counted read, not `toHaveCount(0)`. Web-first assertions retry
  // until they pass, so an absence asserted inside a window simply
  // waits the window out and goes green against the very bug it is
  // here for — which is exactly what it did when this was written that
  // way and the mutation was applied.
  expect(await page.getByText("404", { exact: true }).count()).toBe(0);
  await expect(page.getByText("Danger Zone")).toBeVisible();
});

// ---------------------------------------------------------------------
// General.

test("saving a description PATCHes the text that was typed", async ({
  page,
}) => {
  const patches: unknown[] = [];
  await signIn(page);
  await mockRepo(page, { admin: true, onPatch: (b) => patches.push(b) });
  await page.goto("/acme/widget/settings");

  const box = page.getByLabel("Description");
  await expect(box).toHaveValue("the fast one");
  await box.fill("  the careful one  ");
  await page.getByRole("button", { name: "Save", exact: true }).click();

  await expect(page.getByText("Saved.", { exact: true })).toBeVisible();
  // Exactly one, and the two General fields alone: a PATCH that carried
  // anything else would change something nobody asked to change while
  // they were editing a sentence. The homepage rides along because the two
  // fields are one form and one save — a fixture with no homepage sends
  // `null`, which is what "still has none" looks like on the wire.
  expect(patches).toEqual([{ description: "the careful one", homepage: null }]);
});

test("an emptied description clears it rather than setting an empty one", async ({
  page,
}) => {
  const patches: unknown[] = [];
  await signIn(page);
  await mockRepo(page, { admin: true, onPatch: (b) => patches.push(b) });
  await page.goto("/acme/widget/settings");

  await page.getByLabel("Description").fill("");
  await page.getByRole("button", { name: "Save", exact: true }).click();

  await expect(page.getByText("Saved.", { exact: true })).toBeVisible();
  // `null` is the API's "remove it"; `""` is a repository describing
  // itself as nothing at all, which is a different and worse state.
  expect(patches).toEqual([{ description: null, homepage: null }]);
});

// ---------------------------------------------------------------------
// Danger Zone. Each of these asserts the refusal before the success.

test("deleting takes the repository's name, typed", async ({ page }) => {
  let deletes = 0;
  await signIn(page);
  await mockRepo(page, { admin: true, onDelete: () => (deletes += 1) });
  await page.goto("/acme/widget/settings");

  await page.getByRole("button", { name: "Delete this repository" }).click();
  const dialog = page.getByRole("alertdialog");
  const confirm = dialog.getByRole("button", {
    name: "Delete this repository",
    exact: true,
  });

  // Nothing typed: refused.
  await expect(confirm).toBeDisabled();
  // The name of a *different* repository is not the name of this one.
  // Any non-empty text unlocking the button would make this ceremony.
  await dialog.getByRole("textbox").fill("widgets");
  await expect(confirm).toBeDisabled();
  expect(deletes).toBe(0);

  await dialog.getByRole("textbox").fill("widget");
  await expect(confirm).toBeEnabled();
  await confirm.click();

  // Somewhere that still exists — staying put would leave the browser
  // pointed at a repository the next request reports as missing, which
  // reads as the delete having failed.
  await expect(page).toHaveURL(/\/acme$/);
  expect(deletes).toBe(1);
});

test("a cancelled confirmation does not carry the typed name back", async ({
  page,
}) => {
  let deletes = 0;
  await signIn(page);
  await mockRepo(page, { admin: true, onDelete: () => (deletes += 1) });
  await page.goto("/acme/widget/settings");

  await page.getByRole("button", { name: "Delete this repository" }).click();
  let dialog = page.getByRole("alertdialog");
  await dialog.getByRole("textbox").fill("widget");
  await dialog.getByRole("button", { name: "Cancel" }).click();

  await page.getByRole("button", { name: "Delete this repository" }).click();
  dialog = page.getByRole("alertdialog");
  // A guard that remembers being satisfied is a guard that undoes
  // itself: reopening would arrive one click from deletion.
  await expect(dialog.getByRole("textbox")).toHaveValue("");
  await expect(
    dialog.getByRole("button", { name: "Delete this repository", exact: true }),
  ).toBeDisabled();
  expect(deletes).toBe(0);
});

// ---------------------------------------------------------------------
// Panels that would only ever error are not rendered.

test("a native repository gets the branch policy panel", async ({ page }) => {
  await signIn(page);
  await mockRepo(page, { admin: true });
  await page.goto("/acme/widget/settings");

  await expect(page.getByText("Branch policy")).toBeVisible();
});

test("a mirror gets no branch policy panel", async ({ page }) => {
  await signIn(page);
  await mockRepo(page, {
    admin: true,
    repo: { kind: "mirror", origin_url: "github.com/acme/widget" },
  });
  await page.goto("/acme/widget/settings");

  // Anchor on the page having drawn before asserting the absence.
  await expect(page.getByText("Danger Zone")).toBeVisible();
  // A mirror's branches belong to its origin and the API refuses to
  // protect one, so the panel would be a form that can only fail.
  await expect(page.getByText("Branch policy")).toHaveCount(0);
});

test("a homepage is sent as typed, and an emptied one clears it", async ({
  page,
}) => {
  const patches: unknown[] = [];
  await signIn(page);
  await mockRepo(page, { admin: true, onPatch: (b) => patches.push(b) });
  await page.goto("/acme/widget/settings");

  const box = page.getByLabel("Homepage");
  // A repository with no homepage shows an empty box, not the word
  // "null" and not a placeholder pretending to be a value.
  await expect(box).toHaveValue("");
  await box.fill("  https://example.com/docs  ");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved.", { exact: true })).toBeVisible();
  expect(patches).toEqual([
    { description: "the fast one", homepage: "https://example.com/docs" },
  ]);
});

test("the server's refusal of a homepage is shown in the server's own words", async ({
  page,
}) => {
  await signIn(page);
  await mockRepo(page, {
    admin: true,
    patchStatus: 400,
    patchError: "invalid homepage: must begin with http:// or https://",
  });
  await page.goto("/acme/widget/settings");

  await page.getByLabel("Homepage").fill("example.com");
  await page.getByRole("button", { name: "Save", exact: true }).click();

  // The sentence that names which field was refused and why. The client
  // deliberately does not pre-validate: a second copy of the rule here
  // is a second place for it to drift, and the client's copy always
  // becomes the stricter one — refusing something the server would have
  // taken, with no way for the person to find out.
  await expect(
    page.getByText(/must begin with http:\/\/ or https:\/\//),
  ).toBeVisible();
});

// ---------------------------------------------------------------------
// CI checks: the intake secret.
//
// The one property worth more than all the others here is that the
// secret is a **write-only** credential to this page. `GET …/ci/secret`
// answers `{ configured, rotated_at }` and can never answer with the
// value; only the `POST` that mints one returns it, once. So the tests
// below hold both halves: the value is shown when it is minted, and it
// is not on the page — not hidden, not in the accessibility tree, not
// in any request URL — once it has been dismissed, or after a revoke.
//
// The revoke is asserted refusal-first, like every other consequential
// action on this page, because a suite that only walks the confirmed
// path passes just as green against a button that revokes on click.

const SECRET = "sci_9f3a1c7b5e2d4a6f8b0c1d2e3f4a5b6c";

/// The `…/ci/secret` routes, registered after `mockRepo` so they win
/// over its catch-all. `configured` is the state the GET reports; the
/// POST always mints [`SECRET`].
async function mockCiSecret(
  page: Page,
  opts: {
    configured: boolean;
    rotatedAt?: number;
    onPost?: () => void;
    onDelete?: () => void;
  },
) {
  let configured = opts.configured;
  await page.route("**/v1/orgs/acme/repos/widget/ci/secret", (r) => {
    const method = r.request().method();
    if (method === "POST") {
      opts.onPost?.();
      configured = true;
      return r.fulfill({
        status: 201,
        json: { secret: SECRET, rotated_at: Date.now() },
      });
    }
    if (method === "DELETE") {
      opts.onDelete?.();
      configured = false;
      return r.fulfill({ status: 204, body: "" });
    }
    // The shape that matters: never a field that could carry the value.
    return r.fulfill({
      status: 200,
      json: configured
        ? {
            configured: true,
            rotated_at: opts.rotatedAt ?? Date.now() - 3600_000,
          }
        : { configured: false },
    });
  });
}

test("a repository with no intake secret says so, and offers no revoke", async ({
  page,
}) => {
  await signIn(page);
  await mockRepo(page, { admin: true });
  await mockCiSecret(page, { configured: false });
  await page.goto("/acme/widget/settings");

  await expect(page.getByTestId("ci-secret-status")).toHaveText(
    "not configured",
  );
  // "Mint", not "Rotate": the verb has to match what pressing it does,
  // and "rotate" against nothing is a claim there is something there.
  await expect(page.getByRole("button", { name: "Mint secret" })).toBeVisible();
  // Nothing to revoke, so nothing offering to. A disabled revoke would
  // be a control whose only outcome is a 404 from the server.
  expect(
    await page.getByRole("button", { name: "Revoke secret" }).count(),
  ).toBe(0);
});

test("a configured secret reports when it moved, never what it is", async ({
  page,
}) => {
  await signIn(page);
  await mockRepo(page, { admin: true });
  await mockCiSecret(page, {
    configured: true,
    rotatedAt: Date.now() - 7200_000,
  });
  await page.goto("/acme/widget/settings");

  await expect(page.getByTestId("ci-secret-status")).toContainText(
    "configured",
  );
  await expect(page.getByTestId("ci-secret-status")).toContainText("rotated");
  // Now it is "Rotate", and the revoke exists.
  await expect(
    page.getByRole("button", { name: "Rotate secret" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Revoke secret" }),
  ).toBeVisible();
  // The address a pipeline posts to, absolute, with this repository's
  // own names in it — something to paste rather than something to
  // assemble.
  await expect(
    page.getByText("/v1/orgs/acme/repos/widget/ci/checks"),
  ).toBeVisible();
  // And the page that explains the exchange, at the built path.
  await expect(
    page.getByRole("link", { name: "the CI integration guide" }),
  ).toHaveAttribute(
    "href",
    "https://github.com/Weftsh/spool/blob/main/docs/guide/ci-integration.md",
  );
});

test("minting shows the secret once, says so, and takes it back out of the page", async ({
  page,
}) => {
  const urls: string[] = [];
  page.on("request", (r) => urls.push(r.url()));
  await signIn(page);
  await mockRepo(page, { admin: true });
  await mockCiSecret(page, { configured: false });
  await page.goto("/acme/widget/settings");

  await page.getByRole("button", { name: "Mint secret" }).click();

  await expect(page.getByText(SECRET)).toBeVisible();
  // The sentence is the whole contract. Without it somebody closes the
  // tab expecting to come back for it, and the only route back is a
  // rotation that breaks whatever they had already wired up.
  await expect(page.getByText(/no endpoint that reads it back/)).toBeVisible();
  await expect(page.getByRole("button", { name: "Copy secret" })).toBeVisible();

  // Dismiss removes it from the document rather than hiding it: a
  // secret behind `display: none` is still in every screenshot and
  // every "copy page" of the session that follows.
  await page.getByRole("button", { name: "Dismiss" }).click();
  expect(await page.getByText(SECRET).count()).toBe(0);
  expect(await page.content()).not.toContain(SECRET);

  // The status re-read after the mint reflects it, and — the reason the
  // GET shape matters — brings back no value of its own.
  await expect(page.getByTestId("ci-secret-status")).toContainText(
    "configured",
  );

  // A credential in a URL is a credential in an access log, a Referer
  // header and somebody's shell history. Every request this page made,
  // checked once, after the whole flow.
  expect(urls.filter((u) => u.includes(SECRET))).toEqual([]);
});

test("revoking the secret is confirmed, and refused before it is", async ({
  page,
}) => {
  let deletes = 0;
  await signIn(page);
  await mockRepo(page, { admin: true });
  await mockCiSecret(page, {
    configured: true,
    onDelete: () => (deletes += 1),
  });
  await page.goto("/acme/widget/settings");

  await page.getByRole("button", { name: "Revoke secret" }).click();
  const dialog = page.getByRole("alertdialog");
  // It names the repository and what actually breaks, because the cost
  // is somebody else's pipeline failing on its next build rather than
  // anything this person will see.
  await expect(dialog).toContainText("widget");
  await expect(dialog).toContainText(/refused on its next build/);
  // Opening the dialog is not revoking.
  expect(deletes).toBe(0);

  await dialog.getByRole("button", { name: "Cancel" }).click();
  expect(deletes).toBe(0);

  await page.getByRole("button", { name: "Revoke secret" }).click();
  await page
    .getByRole("alertdialog")
    .getByRole("button", { name: "Revoke this secret" })
    .click();

  await expect(page.getByTestId("ci-secret-status")).toHaveText(
    "not configured",
  );
  expect(deletes).toBe(1);
  // And the panel is back to offering the thing that is now true.
  await expect(page.getByRole("button", { name: "Mint secret" })).toBeVisible();
  expect(
    await page.getByRole("button", { name: "Revoke secret" }).count(),
  ).toBe(0);
});

test("a revoke takes a minted secret off the page with it", async ({
  page,
}) => {
  // The sequence somebody actually performs when they have pasted the
  // wrong thing into their pipeline: mint, then think better of it. A
  // dead credential left on screen is one that gets pasted anyway and
  // debugged as a 404.
  await signIn(page);
  await mockRepo(page, { admin: true });
  await mockCiSecret(page, { configured: false });
  await page.goto("/acme/widget/settings");

  await page.getByRole("button", { name: "Mint secret" }).click();
  await expect(page.getByText(SECRET)).toBeVisible();

  await page.getByRole("button", { name: "Revoke secret" }).click();
  await page
    .getByRole("alertdialog")
    .getByRole("button", { name: "Revoke this secret" })
    .click();

  await expect(page.getByTestId("ci-secret-status")).toHaveText(
    "not configured",
  );
  expect(await page.content()).not.toContain(SECRET);
});

// ---------------------------------------------------------------------
// Push webhooks: the other half of the CI loop.
//
// A repository hosted here gets CI from two things and neither works
// alone — a webhook that tells somebody's build system there is
// something to build, and the intake secret above that carries the
// verdict back. The panels are adjacent for that reason, and these tests
// hold the same three properties as the intake ones: the delivery secret
// is shown once and then gone, removing a subscription is confirmed
// refusal-first, and the list is read from the shape the **server**
// actually sends.
//
// That last one is not pedantry. `GET …/webhooks` answers
// `{ "subscriptions": [...] }` — `webhooks_api::list` and
// `openapi.json` agree — and the client shipped reading `out.webhooks`,
// which resolves to `undefined` and throws on the first `.map`. Mocking
// the client's mistaken shape would have made this suite pass against
// a panel that could not render a single row in production.

const HOOK = {
  id: "01hook",
  org_id: "01org",
  repo_id: "01aaa",
  url: "https://ci.example.com/hooks/stratum",
  created_at: Date.now() - 172_800_000,
};

const HOOK_SECRET = "whsec_4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f90";

/// The `…/webhooks` routes, in the server's own response shapes.
async function mockWebhooks(
  page: Page,
  opts: {
    initial?: (typeof HOOK)[];
    onCreate?: (url: string) => void;
    onDelete?: (id: string) => void;
    /// Refuse the POST with this status and sentence, so a test can
    /// assert the page prints the server's words rather than its own.
    createStatus?: number;
    createError?: string;
  },
) {
  let subscriptions = [...(opts.initial ?? [])];
  await page.route("**/v1/orgs/acme/repos/widget/webhooks", (r) => {
    if (r.request().method() === "POST") {
      const url = String(r.request().postDataJSON()?.url ?? "");
      opts.onCreate?.(url);
      if (opts.createStatus && opts.createStatus >= 400) {
        return r.fulfill({
          status: opts.createStatus,
          json: { error: opts.createError ?? "refused" },
        });
      }
      subscriptions = [...subscriptions, { ...HOOK, url }];
      return r.fulfill({
        status: 201,
        json: { id: HOOK.id, url, secret: HOOK_SECRET },
      });
    }
    // The key the server sends. Not `webhooks`.
    return r.fulfill({ status: 200, json: { subscriptions } });
  });
  await page.route("**/v1/orgs/acme/repos/widget/webhooks/*", (r) => {
    opts.onDelete?.(r.request().url().split("/").pop()!);
    subscriptions = [];
    return r.fulfill({ status: 204, body: "" });
  });
}

test("a repository with no webhook says nothing is being told about its pushes", async ({
  page,
}) => {
  await signIn(page);
  await mockRepo(page, { admin: true });
  await mockWebhooks(page, {});
  await page.goto("/acme/widget/settings");

  await expect(page.getByTestId("webhooks-empty")).toBeVisible();
  // The events, named on the page rather than only in the docs: a
  // reader wiring a merge gate needs to know a landing is one.
  await expect(page.getByText("change.landed")).toBeVisible();
  await expect(page.getByText("change.ejected")).toBeVisible();
  // And the limitation, said out loud. A git push delivery names
  // neither branch nor commit, and a receiver written on the
  // assumption that it does is a receiver that breaks on its first
  // real delivery.
  await expect(
    page.getByText(/does not name the branch or the commit/),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "the webhooks guide" }),
  ).toHaveAttribute(
    "href",
    "https://github.com/Weftsh/spool/blob/main/docs/guide/webhooks.md",
  );
});

test("an existing subscription is listed from the server's own response shape", async ({
  page,
}) => {
  // Reads `{ subscriptions: [...] }`, which is what the server sends.
  // Against a client unwrapping `out.webhooks` this test fails on the
  // row, which is the point of writing it this way round.
  await signIn(page);
  await mockRepo(page, { admin: true });
  await mockWebhooks(page, { initial: [HOOK] });
  await page.goto("/acme/widget/settings");

  await expect(page.getByText(HOOK.url)).toBeVisible();
  expect(await page.getByTestId("webhooks-empty").count()).toBe(0);
});

test("subscribing shows the delivery secret once and takes it back out", async ({
  page,
}) => {
  const urls: string[] = [];
  page.on("request", (r) => urls.push(r.url()));
  let created = "";
  await signIn(page);
  await mockRepo(page, { admin: true });
  await mockWebhooks(page, { onCreate: (u) => (created = u) });
  await page.goto("/acme/widget/settings");

  await page
    .getByLabel("Webhook endpoint URL")
    .fill("https://ci.example.com/hooks/stratum");
  await page.getByRole("button", { name: "Add endpoint" }).click();

  await expect(page.getByText(HOOK_SECRET)).toBeVisible();
  await expect(page.getByText(/shown once/)).toBeVisible();
  // The reader is told what the secret is *for*. A delivery secret
  // nobody verifies with is a signature nobody checks.
  await expect(page.getByText(/X-Weft-Signature-256/)).toBeVisible();
  expect(created).toBe("https://ci.example.com/hooks/stratum");
  // The field is cleared on success — and waiting on that, rather than
  // on a call count, is what makes the next interaction deterministic.
  await expect(page.getByLabel("Webhook endpoint URL")).toHaveValue("");

  await page.getByRole("button", { name: "Dismiss" }).click();
  expect(await page.content()).not.toContain(HOOK_SECRET);
  // The subscription itself is still there; only its credential is gone.
  await expect(
    page.getByText("https://ci.example.com/hooks/stratum"),
  ).toBeVisible();
  expect(urls.filter((u) => u.includes(HOOK_SECRET))).toEqual([]);
});

test("a refused webhook URL is reported in the server's own words, and the URL is kept", async ({
  page,
}) => {
  await signIn(page);
  await mockRepo(page, { admin: true });
  await mockWebhooks(page, {
    createStatus: 400,
    createError: "url must be http(s)",
  });
  await page.goto("/acme/widget/settings");

  await page.getByLabel("Webhook endpoint URL").fill("ftp://nope.example");
  await page.getByRole("button", { name: "Add endpoint" }).click();

  // The rule lives on the server and is stated by the server. A second
  // copy of it here is a second thing to drift.
  await expect(page.getByText(/url must be http\(s\)/)).toBeVisible();
  // And what was typed is still in the box: a refusal that also empties
  // the field makes somebody retype a URL to find out what was wrong
  // with it.
  await expect(page.getByLabel("Webhook endpoint URL")).toHaveValue(
    "ftp://nope.example",
  );
});

test("removing a webhook is confirmed, and refused before it is", async ({
  page,
}) => {
  const deleted: string[] = [];
  await signIn(page);
  await mockRepo(page, { admin: true });
  await mockWebhooks(page, {
    initial: [HOOK],
    onDelete: (id) => deleted.push(id),
  });
  await page.goto("/acme/widget/settings");

  await page.getByRole("button", { name: "Remove" }).click();
  const dialog = page.getByRole("alertdialog");
  // It says what actually stops, because the failure mode is silence:
  // nothing polls us, so a build that stops being triggered does not
  // report an error anywhere.
  await expect(dialog).toContainText(/they stop starting/);
  expect(deleted).toEqual([]);

  await dialog.getByRole("button", { name: "Cancel" }).click();
  expect(deleted).toEqual([]);

  await page.getByRole("button", { name: "Remove" }).click();
  await page
    .getByRole("alertdialog")
    .getByRole("button", { name: "Remove this endpoint" })
    .click();

  await expect(page.getByTestId("webhooks-empty")).toBeVisible();
  expect(deleted).toEqual([HOOK.id]);
});

test("rotating an existing intake secret warns that it breaks the pipeline holding the old one", async ({
  page,
}) => {
  // Rotation is not minting, and the difference is invisible from the
  // page: it replaces the secret, and the pipeline still holding the
  // old one is answered with the same 404 a stranger gets. Nothing
  // tells it why — so the warning has to be before the press.
  let posts = 0;
  await signIn(page);
  await mockRepo(page, { admin: true });
  await mockCiSecret(page, { configured: true, onPost: () => (posts += 1) });
  await page.goto("/acme/widget/settings");

  await page.getByRole("button", { name: "Rotate secret" }).click();
  const dialog = page.getByRole("alertdialog");
  await expect(dialog).toContainText(/refused from its next build/);
  await expect(dialog).toContainText(/not told why/);
  // Opening the dialog is not rotating.
  expect(posts).toBe(0);

  await dialog.getByRole("button", { name: "Cancel" }).click();
  expect(posts).toBe(0);

  await page.getByRole("button", { name: "Rotate secret" }).click();
  await page
    .getByRole("alertdialog")
    .getByRole("button", { name: "Rotate this secret" })
    .click();

  await expect(page.getByText(SECRET)).toBeVisible();
  expect(posts).toBe(1);
});

/// A refused import says why, and stops.
///
/// The panel used to be unable to do either. `GET …/import` answered the
/// three phase cursors and nothing else, so a refused import and one
/// still walking its first page were identical replies — and the panel's
/// "in flight" test was `issues !== "done"`, which a failure never
/// satisfies. Somebody who pressed Import against an installation
/// without `issues: read` watched "Importing…" forever, polling every
/// 1.5s, with the sentence naming the permission to grant sitting
/// unread on a job row.
test("a refused import stops polling and names the permission to grant", async ({
  page,
}) => {
  await signIn(page);
  // A mirror: an import reads the origin the commits already come from,
  // so the panel only exists on one.
  await mockRepo(page, {
    admin: true,
    repo: {
      kind: "mirror",
      origin_url: "acme/widget",
      origin_installation: "777",
    },
  });

  // Counted, because "stops" is half the claim and the only way to see
  // it is to watch the requests dry up.
  let reads = 0;
  await page.route("**/v1/orgs/acme/repos/widget/import", (r) => {
    if (r.request().method() === "POST") {
      return r.fulfill({ status: 202, json: { status: "queued" } });
    }
    reads += 1;
    return r.fulfill({
      status: 200,
      json: {
        labels: null,
        milestones: null,
        issues: null,
        state: "failed",
        error:
          "the GitHub App installation cannot read issues on this repository. It needs the `issues: read` permission",
        updated_at: Date.now(),
      },
    });
  });

  await page.goto("/acme/widget/settings");
  await page.getByRole("button", { name: "Import issues" }).click();

  // The worker's own words, not a paraphrase: the sentence names the
  // permission, which is the one thing the reader can act on.
  const importAlert = page
    .getByRole("alert")
    .filter({ hasText: "Import failed" });
  await expect(importAlert).toContainText("issues: read");
  // And the button is back — a finished import is finished whichever way
  // it finished.
  await expect(
    page.getByRole("button", { name: "Import issues" }),
  ).toBeVisible();

  const settled = reads;
  await page.waitForTimeout(4000);
  expect(
    reads - settled,
    "the panel kept polling a job that had already failed",
  ).toBeLessThanOrEqual(1);
});
