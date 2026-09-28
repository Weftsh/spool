// Signing in through the company's identity provider.
//
// An operator who configures single sign-on (Okta, Entra ID, Google
// Workspace, Keycloak…) means it to be the way in, and by default the
// only one: the server then refuses passwords, and turns "Continue with
// GitHub" off too — a linked GitHub account would otherwise let somebody
// the company has switched off keep signing in. What the server offers
// is its own to say, at `GET /v1/auth/methods`, and every screen that
// offers a way in is drawn from that answer:
//
// - the sign-in screen offers exactly what the server will honour, and
//   never a form whose every submission is a 403;
// - a round trip through the provider comes home as `?sso=<outcome>`,
//   and every refusal is a sentence on the signed-out screen — the only
//   place that can say it — said once, then gone from the address bar;
// - a mailed reset link, and an invitation opened by somebody with no
//   account, say plainly that this server signs in with the provider,
//   rather than asking for a password it will refuse;
// - a server that cannot say (an older one, a dropped request) gets
//   today's screen, not a blank one.
//
// Each answer below is registered after `mockApi`, which answers as a
// server with no single sign-on does, so these tests say what they are
// about on their own.

import { expect, test, type Page } from "@playwright/test";

import { ME, mockApi } from "./fixtures";

const SSO_ONLY = {
  password: false,
  github: false,
  sso: { name: "Okta", start: "/v1/auth/sso/start" },
};
const SSO_BESIDE_PASSWORD = { ...SSO_ONLY, password: true, github: true };
const NO_SSO = { password: true, github: true, sso: null };

/// A server answering `methods` — or failing to, for `"fail"` — with
/// nobody signed in unless `signedIn`.
async function server(
  page: Page,
  methods: unknown,
  { signedIn = false }: { signedIn?: boolean } = {},
) {
  await mockApi(page);
  if (!signedIn) {
    await page.route("**/v1/auth/me", (r) =>
      r.fulfill({ status: 401, json: { error: "not signed in" } }),
    );
  }
  await page.route("**/v1/auth/methods", (r) =>
    methods === "fail"
      ? r.fulfill({ status: 500, json: { error: "internal error" } })
      : r.fulfill({ json: methods }),
  );
}

/// Every request the page makes to a password route, so a test can say
/// none was made — a form that is not drawn cannot post, and a screen
/// that posts anyway is the thing being pinned.
function passwordPosts(page: Page): string[] {
  const seen: string[] = [];
  page.on("request", (r) => {
    if (
      /\/v1\/auth\/(login|forgot-password|reset-password|password|accept-invite)$/.test(
        r.url(),
      )
    )
      seen.push(r.url());
  });
  return seen;
}

const ssoLink = (page: Page) =>
  page.getByRole("link", { name: "Continue with Okta" });
const githubLink = (page: Page) =>
  page.getByRole("link", { name: "Continue with GitHub" });
const signInButton = (page: Page) =>
  page.getByRole("button", { name: "Sign in", exact: true });

test("single sign-on only: one way in for a person, and no form the server would refuse", async ({
  page,
}) => {
  await server(page, SSO_ONLY);
  const posts = passwordPosts(page);
  await page.goto("/dashboard/");

  // The positive first: the absences below mean something only once the
  // screen has been drawn from the server's answer.
  const link = ssoLink(page);
  await expect(link).toBeVisible();
  // A real link, to where the server said the round trip starts.
  await expect(link).toHaveAttribute("href", "/v1/auth/sso/start");

  await expect(page.getByLabel("Email")).toHaveCount(0);
  await expect(page.getByLabel("Password")).toHaveCount(0);
  await expect(signInButton(page)).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Forgot your password?" }),
  ).toHaveCount(0);
  await expect(githubLink(page)).toHaveCount(0);
  // "or" between one thing and nothing is a question with one answer.
  await expect(page.getByText("or", { exact: true })).toHaveCount(0);
  await expect(page.getByText(/No account yet\?/)).toHaveText(
    "No account yet? Sign in with Okta — your account is made the first time.",
  );
  await expect(page.getByText(/made by invitation/)).toHaveCount(0);
  expect(posts).toEqual([]);
});

test("single sign-on only still lets a script sign in with its token", async ({
  page,
}) => {
  // A token is a machine credential, and single sign-on leaves tokens
  // alone: it is the one way in a script has, so it stays offered.
  await server(page, SSO_ONLY);
  const posts = passwordPosts(page);
  await page.goto("/dashboard/");
  await expect(ssoLink(page)).toBeVisible();
  await page
    .getByRole("button", { name: "Sign in with an API token instead" })
    .click();
  // The way back names a way in the server has.
  await expect(
    page.getByRole("button", { name: "Sign in with email and password" }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Sign in with Okta instead" }),
  ).toBeVisible();
  await page.getByPlaceholder("acme").fill("acme");
  await page.getByPlaceholder("weft_…").fill("weft_test_token");
  await signInButton(page).click();
  await expect(page.getByText("Requests today")).toBeVisible();
  expect(posts).toEqual([]);
});

test("single sign-on beside passwords offers all three, the provider first", async ({
  page,
}) => {
  await server(page, SSO_BESIDE_PASSWORD);
  await page.goto("/dashboard/");
  await expect(ssoLink(page)).toBeVisible();
  await expect(githubLink(page)).toBeVisible();
  await expect(page.getByLabel("Email")).toBeVisible();
  await expect(page.getByLabel("Password")).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Forgot your password?" }),
  ).toBeVisible();

  // The provider the operator set up leads, then GitHub, then the form.
  const top = async (l: ReturnType<Page["locator"]>) =>
    (await l.boundingBox())?.y ?? Number.NaN;
  const [okta, github, email] = [
    await top(ssoLink(page)),
    await top(githubLink(page)),
    await top(page.getByLabel("Email")),
  ];
  expect(okta).toBeLessThan(github);
  expect(github).toBeLessThan(email);

  // Anybody the provider signs in gets an account, so that is the
  // answer to "no account yet?" even with passwords on.
  await expect(page.getByText(/No account yet\?/)).toContainText(
    "Sign in with Okta — your account is made the first time",
  );

  // And the password path is a real one, not decoration.
  await page.getByLabel("Email").fill("owner@acme.test");
  await page.getByLabel("Password").fill("a long enough password");
  await signInButton(page).click();
  await expect(page.getByText("Requests today")).toBeVisible();
});

test("without single sign-on the screen is today's, and GitHub goes when the server says so", async ({
  page,
}) => {
  await server(page, NO_SSO);
  await page.goto("/dashboard/");
  await expect(githubLink(page)).toBeVisible();
  await expect(page.getByLabel("Email")).toBeVisible();
  await expect(page.getByRole("link", { name: /Continue with (?!GitHub)/ })).toHaveCount(0);
  await expect(page.getByText(/No account yet\?/)).toContainText(
    "Accounts on this server are made by invitation",
  );

  // No GitHub OAuth client on this server: the button used to be drawn
  // anyway, and a click came back `?github=unavailable`.
  await server(page, { ...NO_SSO, github: false });
  await page.goto("/dashboard/");
  await expect(page.getByLabel("Email")).toBeVisible();
  await expect(signInButton(page)).toBeVisible();
  await expect(githubLink(page)).toHaveCount(0);
  await expect(page.getByText("or", { exact: true })).toHaveCount(0);
});

test("a server that cannot say how to sign in gets today's screen, not a blank one", async ({
  page,
}) => {
  // Three ways of not answering: a server error, an older server that
  // has no such route (its 404), and a request that never completes.
  for (const fail of ["500", "404", "abort"]) {
    await mockApi(page);
    await page.route("**/v1/auth/me", (r) =>
      r.fulfill({ status: 401, json: { error: "not signed in" } }),
    );
    await page.route("**/v1/auth/methods", (r) =>
      fail === "abort"
        ? r.abort("failed")
        : r.fulfill({
            status: Number(fail),
            json: { error: fail === "404" ? "not found" : "internal error" },
          }),
    );
    await page.goto("/dashboard/");
    await expect(page.getByLabel("Email"), fail).toBeVisible();
    await expect(page.getByLabel("Password"), fail).toBeVisible();
    await expect(githubLink(page), fail).toBeVisible();
    await expect(page.getByText(/No account yet\?/), fail).toContainText(
      "made by invitation",
    );
  }
});

/// Each outcome the identity provider's round trip can come home with,
/// the verdict, and the next step. A banner that only says what went
/// wrong is a dead end with better wording.
const SSO_SENTENCES: Array<[string, RegExp, RegExp]> = [
  ["denied", /You cancelled signing in with Okta/, /Press Continue with Okta to try again/],
  [
    "expired",
    /not finished in the browser that started it, or it took too long/,
    /Continue with Okta to start again/,
  ],
  [
    "noemail",
    /Okta did not give this server an email address it can trust/,
    /send a verified address, or to allow your address's domain/,
  ],
  ["domain", /not in a domain this server admits/, /ask whoever runs this server/],
  ["disabled", /This account has been disabled/, /Ask whoever runs this server to re-enable it/],
  ["unavailable", /Single sign-on is not set up on this server/, /nothing was signed in/],
  [
    "error",
    /Okta did not answer, or something failed on this server/,
    /tell whoever runs this server if it keeps happening/,
  ],
];

test("every refused sign-in with the identity provider is said once, then gone from the address", async ({
  page,
}) => {
  await server(page, SSO_ONLY);
  for (const [outcome, verdict, next] of SSO_SENTENCES) {
    await page.goto(`/dashboard/?sso=${outcome}`);
    const banner = page.getByRole("status").filter({ hasText: verdict });
    await expect(banner, outcome).toBeVisible();
    await expect(banner, outcome).toContainText(next);
    // Taken out of the address bar once read — and the sentence stays,
    // because it was read into the page, not off the URL.
    await expect.poll(() => new URL(page.url()).search, outcome).toBe("");
    await expect(banner, outcome).toBeVisible();
    // The refusal is not a dead end: the way in is still there.
    await expect(ssoLink(page), outcome).toBeVisible();
    // No stale sentence on the next load: a reload, or signing out an
    // hour later, is not a sign-in that just failed.
    await page.reload();
    await expect(ssoLink(page), outcome).toBeVisible();
    await expect(
      page.getByRole("status").filter({ hasText: verdict }),
      outcome,
    ).toHaveCount(0);
  }
});

test("`unavailable` names no provider there is not, and points at the way in there is", async ({
  page,
}) => {
  // The server answers `unavailable` when single sign-on is not set up
  // — which is exactly when `methods` has no name to give.
  await server(page, NO_SSO);
  await page.goto("/dashboard/?sso=unavailable");
  const banner = page
    .getByRole("status")
    .filter({ hasText: /Single sign-on is not set up on this server/ });
  await expect(banner).toContainText(
    "Sign in with an email address and password instead.",
  );
  await expect(page.getByLabel("Email")).toBeVisible();
});

test("a GitHub outcome is cleared the same way, and nothing else the address carries is", async ({
  page,
}) => {
  // The GitHub outcome was never taken out of the address bar: it was
  // read off the live URL on every render, so `?github=denied` said
  // "you cancelled at GitHub" again on every reload.
  await server(page, NO_SSO);
  await page.goto("/dashboard/?github=denied");
  const said = page
    .getByRole("status")
    .filter({ hasText: /You cancelled at GitHub/ });
  await expect(said).toBeVisible();
  await expect.poll(() => new URL(page.url()).search).toBe("");
  await page.reload();
  await expect(page.getByLabel("Email")).toBeVisible();
  await expect(said).toHaveCount(0);

  // Nor after signing in some other way and signing out again, with no
  // reload between: that sign-in screen is not the one GitHub sent the
  // person back to.
  await page.goto("/dashboard/?github=denied");
  await expect(said).toBeVisible();
  await page.getByLabel("Email").fill("owner@acme.test");
  await page.getByLabel("Password").fill("a long enough password");
  await signInButton(page).click();
  await expect(page.getByText("Requests today")).toBeVisible();
  await page.getByRole("button", { name: "Sign out" }).click();
  await expect(page.getByLabel("Email")).toBeVisible();
  await expect(said).toHaveCount(0);

  // The install round trip's own parameters belong to it: the notice
  // it reads is still there, and so is the parameter it read it from.
  await page.goto("/dashboard/?connect=claim&sso=denied");
  await expect(
    page.getByRole("status").filter({ hasText: /installation is ready to connect/ }),
  ).toBeVisible();
  await expect(
    page.getByRole("status").filter({ hasText: /You cancelled signing in with SSO/ }),
  ).toBeVisible();
  await expect.poll(() => new URL(page.url()).search).toBe("?connect=claim");
});

test("coming home from the identity provider signed in lands on the overview, quietly", async ({
  page,
}) => {
  await server(page, SSO_ONLY);
  await page.goto("/dashboard/");
  const link = ssoLink(page);
  await expect(link).toBeVisible();

  // The round trip as the browser makes it: the start route sends it to
  // the provider, and the server's callback sends it home with the
  // session cookie set and `?sso=ok`. Both hops are the server's, so
  // one redirect stands in for them.
  await page.route("**/v1/auth/sso/start", (r) =>
    r.fulfill({ status: 303, headers: { location: "/dashboard/?sso=ok" } }),
  );
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await link.click();

  await expect(page.getByText("Requests today")).toBeVisible();
  // A success is not a sentence, and not a parameter left behind to be
  // bookmarked.
  await expect.poll(() => new URL(page.url()).search).toBe("");
  await expect(
    page.getByRole("status").filter({ hasText: /Okta|signed in/ }),
  ).toHaveCount(0);
});

test("a GitHub account nobody here has is sent to the provider that makes accounts", async ({
  page,
}) => {
  // Beside single sign-on, "accounts here are made by invitation" is no
  // longer the whole truth — the provider makes one on first sign-in —
  // and it was the only next step the sentence offered.
  await server(page, SSO_BESIDE_PASSWORD);
  await page.goto("/dashboard/?github=noaccount");
  const banner = page
    .getByRole("status")
    .filter({ hasText: /nobody on this server is you/ });
  await expect(banner).toContainText(
    "Continue with Okta instead — your account is made the first time",
  );
  await expect(banner).not.toContainText("invitation");
  await expect(ssoLink(page)).toBeVisible();
});

test("a reset link on a server with no passwords says how it signs in, and sends nothing", async ({
  page,
}) => {
  await server(page, SSO_ONLY);
  const posts = passwordPosts(page);
  await page.goto("/dashboard/#reset=weftrs_from_before_sso");
  await expect(
    page.getByRole("heading", { name: "This server signs in with Okta" }),
  ).toBeVisible();
  await expect(page.getByText(/this reset link has nothing to reset/)).toBeVisible();
  await expect(ssoLink(page)).toHaveAttribute("href", "/v1/auth/sso/start");
  await expect(page.getByLabel("New password")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Set password" })).toHaveCount(0);

  // The way on is the sign-in screen, and the link leaves the address.
  await page.getByRole("button", { name: "Go to sign in" }).click();
  await expect(ssoLink(page)).toBeVisible();
  await expect(page.getByLabel("Email")).toHaveCount(0);
  await expect.poll(() => new URL(page.url()).hash).toBe("");
  expect(posts).toEqual([]);
});

test("a reset link on a server that takes passwords still asks for one", async ({
  page,
}) => {
  // The same link beside single sign-on: passwords are on, so a
  // forgotten one is still reset.
  await server(page, SSO_BESIDE_PASSWORD);
  await page.goto("/dashboard/#reset=weftrs_01_secret");
  await expect(
    page.getByRole("heading", { name: "Choose a new password" }),
  ).toBeVisible();
  await expect(page.getByLabel("New password")).toBeVisible();
});

/// An invitation to acme, for somebody with an address at the company.
/// After `server`, which is what refuses everything else.
async function invitation(page: Page) {
  await page.route("**/v1/auth/invite/preview", (r) =>
    r.fulfill({
      json: {
        org: "acme",
        role: "member",
        email: "dev.eloper@acme.test",
        expires_at: Date.now() + 86_400_000,
      },
    }),
  );
}

test("an invitation opened signed out, on a server with no passwords, says to sign in with the provider first", async ({
  page,
}) => {
  await server(page, SSO_ONLY);
  await invitation(page);
  const posts = passwordPosts(page);
  await page.goto("/dashboard/#invite=stinv_01sso_newcomer");
  // What is being joined is still said: the preview is not a password
  // route, and a link that says nothing about itself asks for trust.
  await expect(page.getByRole("heading", { name: "Join acme" })).toBeVisible();
  await expect(
    page.getByText(
      "dev.eloper@acme.test was invited as member. This server signs in with Okta. Sign in with Okta, then open this link again.",
    ),
  ).toBeVisible();
  await expect(ssoLink(page)).toHaveAttribute("href", "/v1/auth/sso/start");
  // Nothing to type, and nothing to press that the server would refuse.
  await expect(page.getByLabel("Your name")).toHaveCount(0);
  await expect(page.getByLabel("Password")).toHaveCount(0);
  await expect(page.getByLabel("Handle")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Accept invitation" }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: /I already have an account/ }),
  ).toHaveCount(0);
  expect(posts).toEqual([]);
});

test("an invitation opened signed in, on a server with no passwords, is accepted as it always was", async ({
  page,
}) => {
  // The other half of the contract: somebody the provider already
  // signed in can still accept an invitation into another organization.
  await server(page, SSO_ONLY, { signedIn: true });
  await invitation(page);
  const posted: Array<Record<string, unknown>> = [];
  await page.route("**/v1/auth/accept-invite", async (route) => {
    posted.push(route.request().postDataJSON());
    await route.fulfill({ status: 200, json: ME });
  });
  await page.goto("/dashboard/#invite=stinv_01signed_in");
  await expect(page.getByRole("heading", { name: "Join acme" })).toBeVisible();
  await expect(
    page.getByText(/Accepting adds owner@acme\.test, the account you are signed in as/),
  ).toBeVisible();
  await expect(ssoLink(page)).toHaveCount(0);
  await page.getByRole("button", { name: "Accept invitation" }).click();
  await expect(page.getByText("Requests today")).toBeVisible();
  expect(posted).toEqual([{ invite: "stinv_01signed_in", name: "" }]);
});

test("settings offer no password to change on a server that takes none", async ({
  page,
}) => {
  await server(page, SSO_ONLY, { signedIn: true });
  await page.goto("/dashboard/settings/tokens");
  const rail = page.locator('[data-slot="sidebar"]');
  // Positive first: the account's own group is drawn.
  await expect(rail.getByRole("link", { name: "SSH keys" })).toBeVisible();
  await expect(rail.getByRole("link", { name: "Password" })).toHaveCount(0);

  // An old link to the page lands on the first section instead of a
  // form that could only be refused.
  await page.goto("/dashboard/settings/password");
  await expect(page).toHaveURL(/\/dashboard\/settings\/members$/);
  await expect(page.getByLabel("Current password")).toHaveCount(0);

  // And a server that takes passwords keeps the page.
  await server(page, SSO_BESIDE_PASSWORD, { signedIn: true });
  await page.goto("/dashboard/settings/password");
  await expect(page.getByLabel("Current password")).toBeVisible();
});

// ---------------------------------------------------------------------
// Coming back to the page somebody left from.
//
// `/login?next=/acme/widget` is how a signed-out visitor to a page is
// brought back to it. The password form honours it directly; a round
// trip through a provider used to lose it, because the server's callback
// always sends the browser home to `/dashboard/?<provider>=ok`. The
// address is kept in this tab's sessionStorage for the length of the
// trip instead of being handed to the server as a redirect parameter
// (`src/lib/return-to.ts`), so it is checked on the way out, spent on
// any return, and never taken somewhere off this site.
// ---------------------------------------------------------------------

/// The key `lib/return-to.ts` keeps the address under. Spelled here
/// rather than imported, so the spec reads storage the way anything
/// else on the origin could.
const RETURN_TO = "stratum-signin-next";

const keptReturn = (page: Page) =>
  page.evaluate((k) => sessionStorage.getItem(k), RETURN_TO);

/// The provider's round trip as the browser makes it: the start route
/// leaves, and the server's callback sends it home with `outcome`.
async function providerSendsHome(page: Page, start: string, home: string) {
  await page.route(start, (r) =>
    r.fulfill({ status: 303, headers: { location: home } }),
  );
}

test("a round trip through the identity provider comes back to the page it left from", async ({
  page,
}) => {
  await server(page, SSO_BESIDE_PASSWORD);
  await page.goto("/login?next=%2Facme%2Fwidget");
  await expect(ssoLink(page)).toBeVisible();
  await providerSendsHome(page, "**/v1/auth/sso/start", "/dashboard/?sso=ok");
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await ssoLink(page).click();

  await expect(page).toHaveURL(/\/acme\/widget$/);
  await expect(
    page.getByRole("navigation", { name: "Repository" }),
  ).toBeVisible();
  // Taken once: nothing is left for a later sign-in to find.
  expect(await keptReturn(page)).toBeNull();
});

test("a round trip through GitHub comes back to the page it left from", async ({
  page,
}) => {
  await server(page, SSO_BESIDE_PASSWORD);
  await page.goto("/login?next=%2Facme%2Fwidget%2Fissues%3Fq%3Dis%253Aopen");
  await expect(githubLink(page)).toBeVisible();
  await providerSendsHome(
    page,
    "**/v1/auth/github/start",
    "/dashboard/?github=ok",
  );
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await githubLink(page).click();

  await expect(page).toHaveURL(/\/acme\/widget\/issues\?q=is%3Aopen$/);
  await expect(
    page.getByRole("navigation", { name: "Repository" }),
  ).toBeVisible();
  expect(await keptReturn(page)).toBeNull();
});

test("a kept destination that leaves this site is ignored, and spent", async ({
  page,
}) => {
  // Storage is not only ours to write: any script on the origin, or an
  // extension, can put something there. What comes back is checked
  // again, by the same rule the password path's `next` is.
  await server(page, SSO_BESIDE_PASSWORD, { signedIn: true });
  const elsewhere: string[] = [];
  page.on("request", (r) => {
    if (new URL(r.url()).hostname !== "127.0.0.1") elsewhere.push(r.url());
  });
  for (const kept of [
    "//evil.example",
    "https://evil.example",
    "//evil.example/acme/widget",
    "https://evil.example/acme/widget",
    "/\\evil.example/acme/widget",
  ]) {
    await page.goto("/dashboard/");
    await expect(page.getByText("Requests today"), kept).toBeVisible();
    await page.evaluate(([k, v]) => sessionStorage.setItem(k, v), [
      RETURN_TO,
      kept,
    ]);

    await page.goto("/dashboard/?sso=ok");
    await expect(page.getByText("Requests today"), kept).toBeVisible();
    await expect
      .poll(() => {
        const u = new URL(page.url());
        return `${u.pathname}${u.search}`;
      }, kept)
      .toBe("/dashboard/");
    expect(await keptReturn(page), kept).toBeNull();
  }
  expect(elsewhere).toEqual([]);
});

test("a refused round trip spends the destination it was carrying", async ({
  page,
}) => {
  await server(page, SSO_BESIDE_PASSWORD);
  await page.goto("/login?next=%2Facme%2Fwidget");
  await expect(ssoLink(page)).toBeVisible();
  // The provider's page, on this origin so the test can look at this
  // tab's storage while the person is away.
  await page.route("**/v1/auth/sso/start", (r) =>
    r.fulfill({
      contentType: "text/html",
      body: "<!doctype html><p>the identity provider</p>",
    }),
  );
  await ssoLink(page).click();
  await expect(page.getByText("the identity provider")).toBeVisible();
  // Kept for the trip — the positive, without which the absence below
  // would hold against a page that never kept anything.
  expect(await keptReturn(page)).toBe("/acme/widget");

  // The person cancels, and the provider sends them home refused.
  await page.goto("/dashboard/?sso=denied");
  await expect(
    page.getByRole("status").filter({ hasText: /You cancelled signing in with Okta/ }),
  ).toBeVisible();
  expect(await keptReturn(page)).toBeNull();

  // So a later success with no trip of its own behind it — a bookmarked
  // start link, a provider session resumed — lands on the overview, not
  // on a page somebody asked for before they cancelled.
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await page.goto("/dashboard/?sso=ok");
  await expect(page.getByText("Requests today")).toBeVisible();
  await expect.poll(() => new URL(page.url()).pathname).toBe("/dashboard/");
});
