// The About rail on a repository: language mix, licence, community
// files and topics.
//
// The suite runs as a signed-in reader who may not administer the
// repository, because that is who the panel is mostly for: somebody
// arriving at a project they did not start, who wants to know what it
// is written in and what licence it carries without asking anybody.
//
// Two of the assertions here are not about rendering at all:
//
// - the licence must say "unrecognised" rather than name a guess, and
// - the topic write must send `{ topics: [...] }` as an object, not as
//   a string. `raw()` stringifies the body itself, and a client method
//   that stringified first would arrive double-encoded — a bug this
//   suite caught once already, on the team calls, and which no unit
//   test can see because the transport is the thing that is wrong.

import { expect, test } from "@playwright/test";
import { ME, REPOS } from "./fixtures";

const widget = {
  ...REPOS.repos[0],
  description: "the fast one",
  // The server's own answer to "may this viewer administer the repo",
  // which is what gates the topic editor. Explicitly false here: every
  // test in this file is a reader without admin unless it says
  // otherwise, and a fixture that quietly granted admin would make the
  // "no Edit topics button" assertions vacuous.
  viewer_admin: false,
};

const META = {
  topics: ["git", "object-storage", "rust"],
  languages: [
    { name: "Rust", bytes: 6000 },
    { name: "TypeScript", bytes: 3000 },
    { name: "Python", bytes: 900 },
    { name: "Shell", bytes: 60 },
    { name: "SQL", bytes: 40 },
  ],
  languages_truncated: false,
  license: {
    path: "LICENSE",
    spdx: "MIT",
    name: "MIT License",
    recognised: true,
    files: ["LICENSE"],
  },
  community: [
    { kind: "contributing", path: "CONTRIBUTING.md" },
    { kind: "security", path: ".github/SECURITY.md" },
  ],
};

/// Refuse every `/v1` call this file has not deliberately mocked.
///
/// Registered before any specific route, because Playwright matches the
/// **most recently registered** handler — so the specifics still win,
/// and anything else 404s here instead of falling through vite's proxy
/// to whatever is listening on :8080, which during development is a
/// real seeded server. Without it the suite is hermetic only while
/// nobody has the manual stack up.
///
/// It is a named helper rather than three copies of the same route
/// because `src/hermetic-specs.test.ts` checks this **per test**, and
/// it cannot see one level of indirection: `asAdmin` delegating to
/// `reader` looked like three unguarded tests. Both helpers now
/// establish the refusal themselves, which is also the honest reading —
/// each is a complete setup, not half of one.
async function hermetic(page: import("@playwright/test").Page) {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
}

/// A signed-in reader with nothing mocked but this page's reads.
async function reader(
  page: import("@playwright/test").Page,
  meta: unknown = META,
) {
  await hermetic(page);
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: widget }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/meta", (r) =>
    r.fulfill({ status: 200, json: meta }),
  );
}

test("the language bar names three languages and aggregates the rest", async ({
  page,
}) => {
  await reader(page);
  await page.goto("/acme/widget");

  const segments = page.getByTestId("language-segment");
  await expect(segments).toHaveCount(4);
  await expect(segments.nth(0)).toHaveAttribute("data-language", "Rust");
  await expect(segments.nth(3)).toHaveAttribute("data-language", "Other");

  // The legend is what carries the meaning: the bar is never read by
  // colour alone, because the colour means "biggest here" and not
  // "Rust" — the same emerald leads every repository's bar.
  await expect(page.getByText("60.0%", { exact: true })).toBeVisible();
  await expect(page.getByText("Other", { exact: true })).toBeVisible();
  await expect(page.getByText("1.0%", { exact: true })).toBeVisible();
});

test("the licence and the community files are named and link to the files", async ({
  page,
}) => {
  await reader(page);
  await page.goto("/acme/widget");

  // "MIT license" — GitHub's exact wording, built from the SPDX id and
  // not from the long name, because that is the string somebody
  // arriving from there is looking for.
  await expect(page.getByRole("link", { name: "MIT license" })).toHaveAttribute(
    "href",
    "/acme/widget/tree/LICENSE",
  );
  await expect(
    page.getByRole("link", { name: "Contributing" }),
  ).toHaveAttribute("href", "/acme/widget/tree/CONTRIBUTING.md");
  // A path with a directory in it keeps its segments, and the leading
  // dot survives encoding.
  await expect(
    page.getByRole("link", { name: "Security policy" }),
  ).toHaveAttribute("href", "/acme/widget/tree/.github/SECURITY.md");
  // And the row for a file the project has NOT written is present and
  // says so. This assertion used to be its exact opposite — that no
  // "Code of conduct" row rendered at all — which was GitHub's
  // behaviour and is the thing FORGE-UX §1 deliberately refuses:
  // silently omitting the row makes an incomplete project look
  // identical to a complete one. A maintainer should see the gap and a
  // contributor should see the honesty.
  const coc = page.getByRole("listitem").filter({ hasText: "Code of conduct" });
  await expect(coc).toHaveCount(1);
  // "None" in words, never a grey dot: colour alone is not a state
  // anybody can read (DESIGN.md).
  await expect(coc).toContainText("None");
  // And it is not a link, because there is nothing to link to. A row
  // that looked clickable and 404'd would be worse than the omission
  // this replaced.
  await expect(coc.getByRole("link")).toHaveCount(0);
});

test("every one of the six health rows is present, whether the file is or not", async ({
  page,
}) => {
  // The whole contract in one assertion. A project with none of the
  // six still gets six rows — the rail is a checklist, and a checklist
  // that hides its unticked boxes is a list of things somebody already
  // did.
  await reader(page, {
    ...META,
    license: null,
    community: [],
    readme: null,
  });
  await page.goto("/acme/widget");

  for (const label of [
    "Readme",
    "License",
    "Code of conduct",
    "Contributing",
    "Security policy",
    "Activity",
  ]) {
    await expect(
      page.getByRole("listitem").filter({ hasText: label }),
      `the ${label} row is missing`,
    ).toHaveCount(1);
  }
});

test("the Activity row leads to a page that exists", async ({ page }) => {
  // It led to `/{owner}/{repo}/insights`, which 404'd — on every
  // repository page in the product. Insights had no body and was
  // deliberately kept out of the tab strip for that exact reason; the
  // rule is written above `TABS` in `forge/index.tsx` ("add a tab here
  // when its body exists, not when its name is decided"). This rail
  // kept linking there anyway.
  //
  // Asserting the destination and not merely that a link is present:
  // a link is what the bug had.
  await reader(page);
  await page.goto("/acme/widget");

  const activity = page.getByRole("link", { name: "Activity", exact: true });
  await expect(activity).toBeVisible();
  await expect(activity).toHaveAttribute("href", "/acme/widget/commits");

  // Follow it. A correct href to a page that does not render would be
  // the same defect wearing a different address.
  await activity.click();
  await expect(page).toHaveURL(/\/acme\/widget\/commits$/);
  await expect(
    page.getByText("We couldn’t find"),
    "the Activity row still lands on a 404",
  ).toHaveCount(0);
});

test("nothing in the About rail points at a tab that has no body", async ({
  page,
}) => {
  // The class, not the instance. Insights was one name; the rail links
  // to five other things and any of them could be pointed at a route
  // the tab strip deliberately does not carry.
  await reader(page);
  await page.goto("/acme/widget");

  // By element, not by role: an `<aside>` only exposes `complementary`
  // when it is not scoped to a sectioning element, and this one is —
  // so `getByRole("complementary")` matches nothing here.
  const rail = page.locator("aside").filter({ hasText: "About" }).first();
  // Anchored on a row that only exists once `/meta` has resolved. Read
  // too early the rail has no links at all, and a loop over nothing
  // passes while asserting nothing.
  await expect(
    rail.getByRole("link", { name: "Activity", exact: true }),
  ).toBeVisible();

  const hrefs = await rail
    .getByRole("link")
    .evaluateAll((els) => els.map((e) => e.getAttribute("href") ?? ""));
  expect(hrefs.length).toBeGreaterThan(0);
  for (const href of hrefs) {
    expect(href, `the About rail links to ${href}`).not.toMatch(
      /\/(insights|explore|feed|stars|topics|notifications)(\/|$)/,
    );
  }
});

test("an unrecognised licence says so rather than naming a guess", async ({
  page,
}) => {
  // The whole value of the badge is that a reader can believe it.
  // People redistribute code on the strength of one.
  await reader(page, {
    ...META,
    license: {
      path: "COPYING",
      spdx: null,
      name: null,
      recognised: false,
      files: ["COPYING"],
    },
  });
  await page.goto("/acme/widget");
  const link = page.getByRole("link", { name: "License (unrecognised)" });
  await expect(link).toBeVisible();
  // Still linked, because "we could not identify this, here it is" is a
  // useful answer and a blank is not.
  await expect(link).toHaveAttribute("href", "/acme/widget/tree/COPYING");
});

test("a dual-licensed project lists both rather than reading as unlicensed", async ({
  page,
}) => {
  // `LICENSE-APACHE` beside `LICENSE-MIT` is the convention across the
  // whole Rust ecosystem. Naming one of them would be the most
  // misleading thing this panel could say; saying nothing — which is
  // what it did until the GitHub comparison — makes the project read as
  // having no licence at all. GitHub answers "Apache-2.0 and 2 other
  // licenses found"; we decline to pick a primary and list them.
  await reader(page, {
    ...META,
    license: {
      path: null,
      spdx: null,
      name: null,
      recognised: false,
      files: ["LICENSE-APACHE", "LICENSE-MIT"],
    },
  });
  await page.goto("/acme/widget");
  await expect(page.getByText("2 licenses found")).toBeVisible();
  await expect(
    page.getByRole("link", { name: "LICENSE-APACHE" }),
  ).toHaveAttribute("href", "/acme/widget/tree/LICENSE-APACHE");
  await expect(page.getByRole("link", { name: "LICENSE-MIT" })).toHaveAttribute(
    "href",
    "/acme/widget/tree/LICENSE-MIT",
  );
  // And no guess anywhere on the page.
  await expect(page.getByText("MIT license")).toHaveCount(0);
});

test("a partial walk says the bar is partial", async ({ page }) => {
  await reader(page, { ...META, languages_truncated: true });
  await page.goto("/acme/widget");
  await expect(page.getByText(/larger than\s+one pass counts/)).toBeVisible();
});

test("topics are pills that lead to the other projects like this one", async ({
  page,
}) => {
  await reader(page);
  await page.goto("/acme/widget");
  await expect(
    page.getByRole("link", { name: "rust", exact: true }),
  ).toHaveAttribute("href", "/search?topic=rust");
  await expect(
    page.getByRole("link", { name: "object-storage" }),
  ).toHaveAttribute("href", "/search?topic=object-storage");
  // A reader without admin is shown the topics and never the form.
  await expect(page.getByRole("button", { name: "Edit topics" })).toHaveCount(
    0,
  );
});

test("a repository with no topics shows a reader nothing at all", async ({
  page,
}) => {
  // Not an empty "Topics" heading with a blank under it. A rail of
  // placeholders reads as an unfinished product rather than as a young
  // project.
  await reader(page, { ...META, topics: [] });
  await page.goto("/acme/widget");
  // Anchored on something that can only exist *after* `/meta` resolved.
  // A bare `toHaveCount(0)` is satisfied by the loading state, so it
  // would pass against a rail that renders "Topics" a moment later —
  // and against a component that had stopped rendering anything at all.
  // The languages are still populated in this fixture precisely so
  // there is something to wait on.
  await expect(page.getByTestId("language-segment").first()).toBeVisible();
  await expect(page.getByText("Topics")).toHaveCount(0);
});

test("a repository with no recognised source draws no bar", async ({
  page,
}) => {
  // Not a full-width band of one colour, which is what a zero total
  // produces if it is "defended" with a default, and which reads as
  // "100% of something".
  await reader(page, { ...META, languages: [] });
  await page.goto("/acme/widget");
  // The topics are still populated in this fixture so the absence
  // below is asserted against a *loaded* rail rather than against the
  // loading state, which satisfies any `toHaveCount(0)` for free.
  await expect(
    page.getByRole("link", { name: "rust", exact: true }),
  ).toBeVisible();
  await expect(page.getByText("Languages")).toHaveCount(0);
  await expect(page.getByTestId("language-segment")).toHaveCount(0);
});

test("the panel failing does not take the repository page with it", async ({
  page,
}) => {
  // A rail that could not load renders nothing. The repository is not
  // broken because its sidebar is, and an error box in a sidebar is
  // furniture nobody can act on.
  //
  // **This test cannot anchor on the page, and that is worth saying
  // rather than hiding.** A failed `/meta` leaves the rail in exactly
  // the DOM state it starts in, so there is no observable that proves
  // the failure was handled rather than still pending — every
  // `toHaveCount(0)` below would pass against a page that had not
  // finished booting. So the absence is followed by a **positive
  // control**: the same page, the same fixture, `/meta` answering, and
  // the bar must appear. Without that second half this test would go on
  // passing against a component that had stopped rendering a language
  // bar under any circumstances at all.
  let failing = true;
  await reader(page);
  await page.route("**/v1/orgs/acme/repos/widget/meta", (r) =>
    failing
      ? r.fulfill({ status: 500, json: { error: "store unreachable" } })
      : r.fulfill({ status: 200, json: META }),
  );
  await page.goto("/acme/widget");
  // The description comes from the repository row, not from `/meta`, so
  // this really does prove the page around the rail survived.
  await expect(page.getByText("the fast one")).toBeVisible();
  await expect(page.getByTestId("language-segment")).toHaveCount(0);
  await expect(page.getByText("store unreachable")).toHaveCount(0);

  failing = false;
  await page.reload();
  await expect(page.getByTestId("language-segment").first()).toBeVisible();
});

/// A viewer who may administer the repository gets the editor.
///
/// `viewer_admin` on the repository row is the server's own answer,
/// from the same `authx` refinement the writes use — so this is the one
/// field that decides it. It used to be a probe of `GET …/access`;
/// mocking that endpoint here would now grant nothing, which is how
/// these three tests found the change.
async function asAdmin(page: import("@playwright/test").Page) {
  // Before `reader`, not after: a second catch-all registered later
  // would be the most recent handler and would win over every specific
  // route below it, refusing the very calls this file mocks.
  await hermetic(page);
  await reader(page);
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: { ...widget, viewer_admin: true } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({
      status: 200,
      json: {
        id: "01user",
        email: "owner@acme.test",
        name: "Ada Owner",
        created_at: 0,
        orgs: [{ id: "01org", name: "acme", role: "owner" }],
      },
    }),
  );
}

test("an admin adds a topic, and the whole set goes over the wire", async ({
  page,
}) => {
  await asAdmin(page);
  const bodies: unknown[] = [];
  await page.route("**/v1/orgs/acme/repos/widget/topics", async (r) => {
    // The request as it actually arrived. `postDataJSON()` parses the
    // body: a double-encoded body parses to a *string*, not an object,
    // which is precisely the assertion below.
    bodies.push(r.request().postDataJSON());
    await r.fulfill({
      status: 200,
      json: { topics: ["git", "object-storage", "rust", "storage"] },
    });
  });

  await page.goto("/acme/widget");
  await page.getByRole("button", { name: "Edit topics" }).click();
  await page.getByLabel("Add a topic").fill("storage");
  await page.getByRole("button", { name: "Add", exact: true }).click();

  // The new pill is on the page, which is the observable the next
  // interaction depends on — waiting on the mock's call count instead
  // would pass before the page had finished with the response.
  // `exact` matters: without it this also matches `object-storage`,
  // which is already on the page, so the assertion passed on the wrong
  // element and would have gone on passing had the new pill never
  // rendered at all.
  await expect(
    page.getByRole("link", { name: "storage", exact: true }),
  ).toBeVisible();

  expect(bodies).toHaveLength(1);
  // An object, not a string. A client method that stringified before
  // handing the body to the transport would arrive as `"{\"topics\":…}"`
  // and this would be a string.
  expect(bodies[0]).toEqual({
    topics: ["git", "object-storage", "rust", "storage"],
  });
});

test("an admin removes a topic by sending the set without it", async ({
  page,
}) => {
  await asAdmin(page);
  const bodies: unknown[] = [];
  await page.route("**/v1/orgs/acme/repos/widget/topics", async (r) => {
    bodies.push(r.request().postDataJSON());
    await r.fulfill({ status: 200, json: { topics: ["git", "rust"] } });
  });

  await page.goto("/acme/widget");
  await page.getByRole("button", { name: "Edit topics" }).click();
  await page
    .getByRole("button", { name: "Remove topic object-storage" })
    .click();

  await expect(
    page.getByRole("link", { name: "object-storage", exact: true }),
  ).toHaveCount(0);
  // The PUT replaces rather than merges, so removing one means sending
  // the other two — a form that sent only the removed word would be
  // lying about what it did.
  expect(bodies[0]).toEqual({ topics: ["git", "rust"] });
});

test("a refused topic shows the server's own sentence and changes nothing", async ({
  page,
}) => {
  // The client deliberately does not pre-validate: the rules live in
  // the control plane, and a second copy here would drift into being
  // the stricter one. So the refusal a user sees has to be the
  // server's, naming which word was refused and why.
  await asAdmin(page);
  await page.route("**/v1/orgs/acme/repos/widget/topics", (r) =>
    r.fulfill({
      status: 400,
      json: {
        error: 'topic "two words" may hold only letters, digits and hyphens',
      },
    }),
  );

  await page.goto("/acme/widget");
  await page.getByRole("button", { name: "Edit topics" }).click();
  await page.getByLabel("Add a topic").fill("two words");
  await page.getByRole("button", { name: "Add", exact: true }).click();

  await expect(
    page.getByText("may hold only letters, digits and hyphens"),
  ).toBeVisible();
  // The pills that were there are still there.
  await expect(
    page.getByRole("link", { name: "rust", exact: true }),
  ).toBeVisible();
});

test("a topic pill leads to the repositories carrying it", async ({ page }) => {
  // The rail invited this in as many words — "a word or two makes this
  // findable" — and it was false twice over: search never read the
  // topics table, so typing the word found nothing, and the pill linked
  // to an address whose `topic` nothing parsed, so clicking it landed
  // on the unfiltered list of everything. A maintainer following the
  // prompt got no findability and no sign that anything had gone wrong.
  await reader(page);
  let asked: string | null = null;
  await page.route("**/v1/search/repos*", (r) => {
    asked = new URL(r.request().url()).searchParams.get("topic");
    return r.fulfill({
      status: 200,
      json: {
        repos: [
          {
            id: "01aaa",
            org_id: "01org",
            org: "acme",
            name: "widget",
            description: "the fast one",
            kind: "native",
            created_at: Date.now(),
          },
        ],
        next: null,
      },
    });
  });
  await page.goto("/acme/widget");

  const pill = page.getByRole("link", { name: "rust", exact: true });
  await expect(pill).toHaveAttribute("href", "/search?topic=rust");
  await pill.click();

  await expect(page).toHaveURL(/\/search\?topic=rust$/);
  await expect(
    page.getByRole("heading", { name: "Repositories tagged rust" }),
  ).toBeVisible();
  // The exact facet, not the free-text box: a pill that sent its word as
  // `q` would also return every repository merely mentioning it, which
  // is the one thing a facet must not do.
  await expect
    .poll(() => asked, { message: "the pill did not send topic=" })
    .toBe("rust");
});
