// The doors into the product, and what a new repository asks for.
//
// Nobody signs themselves up. An account is made by accepting an
// organization's invitation, or by whoever runs the server; the sign-in
// screen has to say so, because a visitor with no account otherwise
// hunts for a "Create an account" that is not there. Accepting as a new
// person asks for a name, a password and — optionally — a handle, and
// shows the handle the server will make if the field is left empty. A
// handle somebody already has is refused without spending the link, so
// the form keeps the person on it with the refusal against that field.
//
// Every other page is for somebody signed in, so a visitor who is not
// is sent to `/login` with the address they asked for, and brought back
// to it. And a new repository, in a personal namespace or an
// organization, asks for a name and a description and nothing about who
// may read it: every repository is private to its namespace.

import { expect, test, type Page } from "@playwright/test";

import { ME, mockApi } from "./fixtures";

/// Nobody signed in: every API call refused, `me` says nobody is here.
/// Registered first so the specific routes below win.
async function signedOut(page: Page) {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
}

test("/login?mode=signup opens sign-in, which says accounts come by invitation", async ({
  page,
}) => {
  await signedOut(page);
  // Where the old "Sign up" links pointed. There is no form behind it
  // any more, so it is the one door — and the door says who to ask.
  await page.goto("/login?mode=signup");
  // The tab is the product's name. It read "Stratum Dashboard" for a
  // while after everything else was renamed.
  await expect(page).toHaveTitle("Weft");
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();
  await expect(page.getByText(/No account yet\?/)).toContainText(
    "Accounts on this server are made by invitation",
  );
  // Nothing of the sign-up form survives, nor a way to reach it.
  await expect(page.getByLabel("Your name")).toHaveCount(0);
  await expect(page.getByLabel("Namespace")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: /create (an )?account/i }),
  ).toHaveCount(0);
  // GitHub is still offered, where it was.
  await expect(
    page.getByRole("link", { name: "Continue with GitHub" }),
  ).toBeVisible();
});

test("/login from an old sign-up link still returns where it was going", async ({
  page,
}) => {
  await signedOut(page);
  await page.goto("/login?mode=signup&next=%2Facme%2Fwidget");
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();

  // The leftover mode and the return address are independent: signing
  // in still goes back to `next`.
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await page.route("**/v1/auth/login", (r) => r.fulfill({ json: ME }));
  await page.getByLabel("Email").fill("owner@acme.test");
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(page).toHaveURL(/\/acme\/widget$/);
});

test("a forge page sends somebody signed out to sign in, and back again", async ({
  page,
}) => {
  await signedOut(page);
  await page.goto("/acme/widget/issues?q=is%3Aopen");
  // The address they asked for rides along, query and all, and nothing
  // of the page they may not see is drawn first.
  await expect(page).toHaveURL(
    /\/login\?next=%2Facme%2Fwidget%2Fissues%3Fq%3Dis%253Aopen$/,
  );
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("navigation", { name: "Repository" }),
  ).toHaveCount(0);

  await page.route("**/v1/auth/login", (r) => r.fulfill({ json: ME }));
  await page.getByLabel("Email").fill("owner@acme.test");
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(page).toHaveURL(/\/acme\/widget\/issues\?q=is%3Aopen$/);
});

test("the front page is the way in, not a listing", async ({ page }) => {
  await signedOut(page);
  await page.goto("/");
  // Moved, not rendered: the dashboard's own sign-in form, at the
  // dashboard's address.
  await expect(page).toHaveURL(/\/dashboard\/?$/);
  await expect(
    page.getByRole("button", { name: "Sign in", exact: true }),
  ).toBeVisible();
});

const INVITED = "dev.eloper@acme.test";

/// The account the server answers with once the invitation is
/// accepted: a member of the organization that invited them, and the
/// owner of a personal namespace named by `handle`.
function joined(handle: string) {
  return {
    id: "01new",
    email: INVITED,
    name: "Dev Eloper",
    created_at: Date.now(),
    handle,
    orgs: [
      { id: "01org", name: "acme", role: "member" },
      { id: "01zpersonal", name: handle, role: "owner" },
    ],
  };
}

/// Somebody new, holding an invitation to acme and no session. The
/// organization's pages are mocked so there is somewhere to land.
async function newcomer(page: Page) {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/auth/invite/preview", (r) =>
    r.fulfill({
      json: {
        org: "acme",
        role: "member",
        email: INVITED,
        expires_at: Date.now() + 86_400_000,
      },
    }),
  );
}

test("accepting as somebody new with no handle sends none, and shows the one the server makes", async ({
  page,
}) => {
  await newcomer(page);
  const posted: Array<Record<string, unknown>> = [];
  await page.route("**/v1/auth/accept-invite", async (route) => {
    posted.push(route.request().postDataJSON());
    await route.fulfill({ status: 201, json: joined("dev-eloper") });
  });

  await page.goto("/dashboard/#invite=stinv_01new_person");
  await expect(page.getByRole("heading", { name: "Join acme" })).toBeVisible();
  // The name the server will derive from the invited address, before
  // anybody submits: `dev.eloper` becomes `dev-eloper`.
  await expect(page.getByLabel("Handle")).toHaveAttribute(
    "placeholder",
    "dev-eloper",
  );
  await expect(page.getByLabel("Handle")).toHaveValue("");

  await page.getByLabel("Your name").fill("Dev Eloper");
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Accept invitation" }).click();
  await expect(page.getByText("Requests today")).toBeVisible();

  // Nothing about the handle is sent — not the placeholder, not an empty
  // string. Sending the derived name would turn "make me one" into "I
  // asked for this one", which the server refuses on a clash rather than
  // suffixing.
  expect(posted).toEqual([
    {
      invite: "stinv_01new_person",
      name: "Dev Eloper",
      password: "a long enough password",
    },
  ]);
  expect(posted[0]).not.toHaveProperty("handle");
  expect(new URL(page.url()).hash).toBe("");

  // Signed in to the organization that invited them, with their own
  // namespace there beside it — the one the server made.
  const switcher = page.getByRole("combobox", { name: "Organization" });
  await expect(switcher).toContainText("acme");
  await switcher.click();
  await expect(
    page.getByRole("option", { name: "dev-eloper", exact: true }),
  ).toBeVisible();
});

test("accepting with a handle sends exactly the one typed", async ({ page }) => {
  await newcomer(page);
  const posted: Array<Record<string, unknown>> = [];
  await page.route("**/v1/auth/accept-invite", async (route) => {
    posted.push(route.request().postDataJSON());
    await route.fulfill({ status: 201, json: joined("dev") });
  });

  await page.goto("/dashboard/#invite=stinv_01with_handle");
  await page.getByLabel("Your name").fill("Dev Eloper");
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByLabel("Handle").fill("  dev ");
  // What it will be, said as it is typed.
  await expect(page.locator("#handle-hint")).toContainText("/dev/repo");
  await page.getByRole("button", { name: "Accept invitation" }).click();
  await expect(page.getByText("Requests today")).toBeVisible();
  expect(posted).toEqual([
    {
      invite: "stinv_01with_handle",
      name: "Dev Eloper",
      password: "a long enough password",
      handle: "dev",
    },
  ]);

  await page.getByRole("combobox", { name: "Organization" }).click();
  await expect(
    page.getByRole("option", { name: "dev", exact: true }),
  ).toBeVisible();
});

test("a handle somebody already has keeps the form, says so on the field, and the link still works", async ({
  page,
}) => {
  await newcomer(page);
  const posted: Array<Record<string, unknown>> = [];
  await page.route("**/v1/auth/accept-invite", async (route) => {
    const body = route.request().postDataJSON();
    posted.push(body);
    // The server's own answer: 409, nothing written, the invitation
    // untouched — so the second attempt with another name succeeds.
    if (body.handle === "ada")
      return route.fulfill({ status: 409, json: { error: '"ada" is taken' } });
    return route.fulfill({ status: 201, json: joined(body.handle) });
  });

  await page.goto("/dashboard/#invite=stinv_01taken_handle");
  await page.getByLabel("Your name").fill("Dev Eloper");
  await page.getByLabel("Password").fill("a long enough password");
  const handle = page.getByLabel("Handle");
  await handle.fill("ada");
  await page.getByRole("button", { name: "Accept invitation" }).click();

  // Said against the field it is about — marked invalid, described by
  // the refusal, and focused so the next keystroke fixes it.
  await expect(handle).toHaveAttribute("aria-invalid", "true");
  await expect(handle).toHaveAccessibleDescription(/"ada" is taken/);
  await expect(handle).toBeFocused();
  await expect(page.locator("#handle-error")).toContainText(
    "This invitation still works",
  );
  // Not the generic "could not accept" line: nothing is wrong with the
  // invitation, and saying so would send somebody to ask for a new one.
  await expect(page.getByText(/Could not accept/)).toHaveCount(0);
  // Still on the invitation, with everything already typed kept, and the
  // link still in hand.
  await expect(page.getByRole("heading", { name: "Join acme" })).toBeVisible();
  await expect(page.getByLabel("Your name")).toHaveValue("Dev Eloper");
  await expect(page.getByLabel("Password")).toHaveValue(
    "a long enough password",
  );
  expect(new URL(page.url()).hash).toBe("#invite=stinv_01taken_handle");

  // Typing clears the refusal: it was about the old name, not this one.
  await handle.fill("ada-l");
  await expect(handle).not.toHaveAttribute("aria-invalid", /.*/);
  await expect(page.locator("#handle-error")).toHaveCount(0);

  await page.getByRole("button", { name: "Accept invitation" }).click();
  await expect(page.getByText("Requests today")).toBeVisible();
  expect(posted).toEqual([
    {
      invite: "stinv_01taken_handle",
      name: "Dev Eloper",
      password: "a long enough password",
      handle: "ada",
    },
    {
      invite: "stinv_01taken_handle",
      name: "Dev Eloper",
      password: "a long enough password",
      handle: "ada-l",
    },
  ]);
});

/// The signed-in person whose current namespace is their own. `me`
/// names the handle, and the personal membership is listed first so it
/// is the org a fresh session lands in.
const PERSONAL_ME = {
  ...ME,
  handle: "ada",
  orgs: [{ id: "01ada", name: "ada", role: "owner" }, ...ME.orgs],
};

async function inPersonalNamespace(page: Page) {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: PERSONAL_ME }));
  await page.route("**/v1/orgs/ada/repos?limit=200", (r) =>
    r.fulfill({ json: { repos: [] } }),
  );
}

test("a new repository asks nobody who may read it, and sends nothing about it", async ({
  page,
}) => {
  await inPersonalNamespace(page);
  const creates: Array<Record<string, unknown>> = [];
  await page.route("**/v1/orgs/*/repos", (r) => {
    creates.push(r.request().postDataJSON());
    return r.fulfill({
      status: 409,
      json: { error: '"diary" already exists in ada' },
    });
  });
  await page.goto("/dashboard/new");
  // Neither way of creating carries a visibility control.
  await expect(page.getByLabel("Repository URL")).toBeVisible();
  await expect(page.getByRole("checkbox")).toHaveCount(0);
  await page.getByRole("tab", { name: "Empty repository" }).click();
  await expect(page.getByRole("checkbox")).toHaveCount(0);

  await page.getByLabel("Repository name").fill("diary");
  await page.getByRole("button", { name: "Create repository" }).click();
  // A refusal is the server's own sentence — there is no paywall and no
  // "create an organization" detour to sort it into.
  await expect(page.getByRole("alert")).toContainText("already exists");
  await expect(
    page.getByRole("region", {
      name: /Subscription needed|Organization needed/,
    }),
  ).toHaveCount(0);
  expect(creates).toEqual([{ name: "diary" }]);
});
