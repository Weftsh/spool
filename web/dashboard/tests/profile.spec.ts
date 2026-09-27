// The profile page, reading the identity substrate.
//
// Migration 0020 and `profiles_api.rs` published a display name, a bio,
// pronouns, a company, a location, links and pinned items, and for a
// while the page rendered none of it: an avatar, a handle and a repo
// grid, exactly as it had looked before any of that existed. These
// tests are the contract that the page reads what the server serves.
//
// Two of them are about what is *not* rendered, and those are the ones
// worth keeping honest. A profile with nothing filled in must not grow
// a row of empty metadata or a "Pinned" heading over nothing — an
// empty section is a promise the product does not keep (FORGE-UX §5) —
// and a link whose scheme executes rather than navigates must not
// become an `href` at all.
//
// Why the filled-in profile is proved here and not by the manual pass:
// `scripts/manual-stack.sh` seeds the `acme` org and no personal
// handle, sets no profile fields and pins nothing, so the walkthrough's
// namespace stage can only ever see the empty rendering. It holds that
// one — a heading, no metadata list, no "Pinned" over nothing — and
// this file holds the other. Seeding a personal namespace with a bio
// and a pin would let the manual pass cover both, and is written up as
// a finding rather than done here, because the seed script is shared.

import { expect, test } from "@playwright/test";

type Json = Record<string, unknown>;

/// A full profile: every optional field present, so a test asserting an
/// absence has to remove one deliberately rather than inherit it.
const ADA: Json = {
  handle: "ada",
  name: "ada",
  display_name: "Ada Lovelace",
  bio: "Notes on the Analytical Engine.",
  location: "London",
  company: "Analytical Engines Ltd",
  pronouns: "she/her",
  kind: "human",
  contrib_private_optin: false,
  profile_repo: null,
  created_at: 1_700_000_000,
  links: [{ label: null, url: "https://ada.example/" }],
  public_repos: 2,
};

const WIDGET = {
  id: "r1",
  org: "ada",
  name: "widget",
  description: "A small widget.",
  public: true,
};

const ENGINE = {
  id: "r2",
  org: "ada",
  name: "engine",
  description: "The engine.",
  public: true,
};

/// A signed-out browser looking at `/ada`.
///
/// Every route the page reads is mocked, including the boot probe: a
/// mock that answered only the profile would prove the login form
/// appears, which is not what any of this is about.
async function visit(
  page: import("@playwright/test").Page,
  opts: {
    profile?: Json | number;
    pins?: Json[];
    repos?: Json[];
  } = {},
) {
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/search/repos*", (r) =>
    r.fulfill({
      status: 200,
      json: { repos: opts.repos ?? [WIDGET, ENGINE], next: null },
    }),
  );
  await page.route("**/v1/users/ada/pins", (r) =>
    r.fulfill({ status: 200, json: { pins: opts.pins ?? [] } }),
  );
  const profile = opts.profile ?? ADA;
  await page.route("**/v1/users/ada", (r) =>
    typeof profile === "number"
      ? r.fulfill({ status: profile, json: { error: "no such user" } })
      : r.fulfill({ status: 200, json: profile }),
  );
  // Wait for the identity response, not just for navigation.
  //
  // `page.goto` resolves on the document, and the rail's fields arrive
  // one fetch later. Every assertion in this file that checks something
  // is *absent* would otherwise be evaluated against a page that has
  // simply not loaded yet, and pass for that reason — which is exactly
  // what happened: with `httpUrl()` deleted outright, "a link whose
  // scheme executes is not rendered" still passed, because at the
  // moment it looked, no link of any kind had rendered. A test that
  // cannot fail is worse than no test, because it is counted.
  //
  // Waiting on the response is necessary and not sufficient — a
  // response is not a render — so each absence test also anchors on
  // something positive from the same render pass before asserting what
  // is missing.
  const seen = page.waitForResponse((r) =>
    /\/v1\/users\/ada$/.test(new URL(r.url()).pathname),
  );
  await page.goto("/ada");
  await seen;
}

test("the rail renders the identity the server publishes", async ({ page }) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await visit(page);
  // The display name is the heading, not the handle: what somebody
  // would rather be called is the first thing the page should say.
  await expect(
    page.getByRole("heading", { name: "Ada Lovelace", level: 1 }),
  ).toBeVisible();
  // Handle and pronouns share a line, in that order.
  await expect(page.getByText("ada · she/her")).toBeVisible();
  await expect(page.getByText("Notes on the Analytical Engine.")).toBeVisible();
  await expect(page.getByText("Analytical Engines Ltd")).toBeVisible();
  await expect(page.getByText("London")).toBeVisible();
});

test("an unlabelled link reads as its host, and leaves this tab alone", async ({
  page,
}) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await visit(page);
  const link = page.getByRole("link", { name: "ada.example" });
  await expect(link).toHaveAttribute("href", "https://ada.example/");
  // A URL somebody typed about themselves is user-generated content
  // aimed at the open internet: it must not pass this page's referrer
  // on, must not hand the destination a handle on this tab, and must
  // not make a profile a place to launder search ranking.
  const rel = (await link.getAttribute("rel")) ?? "";
  for (const token of ["nofollow", "ugc", "noopener", "noreferrer"]) {
    expect(rel.split(/\s+/)).toContain(token);
  }
});

test("a link whose scheme executes is not rendered as a link", async ({
  page,
}) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  // The control plane refuses this at the write, so a row like this
  // should not exist. The test is here because the client is the last
  // place the string is a string before it becomes an `href`, and the
  // cost of the two disagreeing is script execution on a page any
  // stranger can open. Note the leading space: a browser strips it and
  // runs the URL, so a guard reading the raw first character would have
  // waved this one through.
  await visit(page, {
    profile: {
      ...ADA,
      links: [
        { label: "My site", url: " javascript:alert(1)" },
        { label: "Real", url: "https://ada.example/real" },
      ],
    },
  });
  // The good link first, and not only because it is worth asserting:
  // it is the proof that the links list has rendered at all. Checking
  // the absence of the bad one against a rail that has not rendered
  // yet is how this test passed with the guard deleted.
  await expect(page.getByRole("link", { name: "Real" })).toBeVisible();
  // The bad one is dropped, and the good one beside it survives: one
  // hostile link must not cost somebody their other three.
  await expect(page.getByRole("link", { name: "My site" })).toHaveCount(0);
  await expect(page.getByText("My site")).toHaveCount(0);
});

test("pins render above the repositories, and name a foreign owner", async ({
  page,
}) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await visit(page, {
    pins: [
      {
        kind: "repo",
        org: "ada",
        name: "engine",
        description: "The engine.",
        public: true,
      },
      {
        kind: "repo",
        org: "acme",
        name: "widget",
        description: "Somebody else's.",
        public: true,
      },
    ],
  });
  await expect(page.getByRole("heading", { name: "Pinned" })).toBeVisible();
  // Once there is a section above it, the grid below is labelled — with
  // nothing above it, a heading over the whole column says nothing.
  await expect(
    page.getByRole("heading", { name: "Repositories" }),
  ).toBeVisible();
  // One from elsewhere names its owner and links there rather than here.
  const foreign = page.getByRole("link", { name: "acme / widget" });
  await expect(foreign).toHaveAttribute("href", "/acme/widget");
  // ...and a pin of this profile's OWN repository does not repeat the
  // owner. This half was missing, and its absence made the test
  // decoration: rendering the owner on every card — the exact bug the
  // rule forbids — left the assertion above satisfied and the suite
  // green. Verified by mutation: `owner={p.org}` in place of the
  // conditional now fails here.
  //
  // `exact: true` is what does the work. A loose match for "engine"
  // also matches "ada / engine", so the sloppy version of this
  // assertion would pass against the bug too.
  //
  // Scoped to the Pinned section, because a pinned repository also
  // appears in the grid below it — as it does on GitHub — so a
  // page-wide match for "engine" resolves to two links and fails strict
  // mode instead of testing anything.
  const pinned = page.getByRole("heading", { name: "Pinned" }).locator("..");
  await expect(
    pinned.getByRole("link", { name: "engine", exact: true }),
  ).toBeVisible();
  await expect(pinned.getByRole("link", { name: "ada / engine" })).toHaveCount(
    0,
  );
});

test("a profile with nothing filled in grows no empty rows", async ({
  page,
}) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  // Every optional field null but the display name, which stays as this
  // test's proof that the profile response was rendered before the
  // absences below were looked for. An empty rail and an unrendered one
  // are the same DOM, so a version of this test with nothing set would
  // pass whether the page worked or not. The handle-as-fallback heading
  // is covered by the missing-profile test instead, which has the repo
  // grid as its own anchor.
  await visit(page, {
    profile: {
      ...ADA,
      bio: null,
      location: null,
      company: null,
      pronouns: null,
      links: [],
    },
  });
  await expect(
    page.getByRole("heading", { name: "Ada Lovelace", level: 1 }),
  ).toBeVisible();
  // The handle keeps its own line under the name, but with no pronouns
  // it is the handle alone — no orphaned separator.
  await expect(page.getByText("ada ·")).toHaveCount(0);
  // The rail carries no metadata list at all, rather than a list of
  // rows with nothing in them. Scoped to the rail, because the repo
  // grid beside it is a list and a page-wide count would find it.
  await expect(page.getByRole("complementary").getByRole("list")).toHaveCount(
    0,
  );
  // And no "Pinned" heading over an empty grid.
  await expect(page.getByRole("heading", { name: "Pinned" })).toHaveCount(0);
});

test("a namespace whose profile is missing is still a working page", async ({
  page,
}) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  // The repository listing is the page; the profile is what the page
  // says about whoever this is. A 404 from the identity route — a
  // namespace with no profile row — must degrade to the page as it was,
  // not replace a namespace full of public repositories with a red box.
  await visit(page, { profile: 404 });
  await expect(
    page.getByRole("heading", { name: "ada", level: 1 }),
  ).toBeVisible();
  await expect(page.getByRole("link", { name: "widget" })).toBeVisible();
  await expect(page.getByText(/no such user/)).toHaveCount(0);
});
