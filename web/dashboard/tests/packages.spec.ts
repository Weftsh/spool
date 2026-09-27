// Settings → Packages: the ecosystem switches, the `.npmrc` snippet, and
// yanking a version.
//
// What a unit test cannot see, and what is therefore here:
//
// - **a write actually reaches the route it claims to.** A panel that
//   renders the new mode optimistically and sends nothing looks correct
//   in every screenshot and changes nothing on the server.
// - **the snippet is built from what the server reported.** The URL
//   shape is the server's business; a snippet assembled from a constant
//   in the client drifts the day the server's does, and the only symptom
//   is somebody's `npm install` failing for a reason the screen denies.
// - **an ecosystem that is not serving cannot be switched on.** The
//   server would accept `mode: private` for Maven today and then answer
//   nothing at all, which reads as a broken product rather than an
//   unfinished one.
import { expect, test, type Page } from "@playwright/test";
import { BILLING, ME, mockApi, signIn } from "./fixtures";

interface Eco {
  ecosystem: string;
  label: string;
  mode: string;
  license_unknown: string;
}

function ecosystems(npm = "off"): Eco[] {
  return [
    { ecosystem: "npm", label: "npm", mode: npm, license_unknown: "block" },
    { ecosystem: "maven", label: "Maven", mode: "off", license_unknown: "block" },
    { ecosystem: "pypi", label: "PyPI", mode: "off", license_unknown: "block" },
    { ecosystem: "cargo", label: "Cargo", mode: "off", license_unknown: "block" },
    { ecosystem: "oci", label: "OCI", mode: "off", license_unknown: "allow" },
  ];
}

const PACKAGE = {
  id: "01PKG",
  ecosystem: "npm",
  name: "@acme/widget",
  private: true,
  origin: "local",
  created_at: 1757000000000,
  updated_at: 1757000000000,
};

function versions(yanked: boolean) {
  return [
    {
      id: "01VER",
      version: "1.0.0",
      yanked,
      yank_reason: null,
      license: "MIT",
      license_source: "declared",
      size_bytes: 4096,
      repo_id: "01REPO",
      commit_sha: "c0ffeebabe1234",
      job_id: "01JOB",
      published_by: null,
      published_at: 1757000000000,
    },
  ];
}

const BASE_POLICY = {
  ecosystem: "npm",
  mode: "audit",
  cooldown_days: 0,
  license_mode: "deny_list",
  license_rules: [] as { spdx_id: string; disposition: string }[],
  reserved: [] as string[],
};

interface Mocked {
  puts: { ecosystem: string; mode: string }[];
  yanks: { yanked: boolean }[];
  policy: unknown[];
  licences: unknown[];
  reserves: unknown[];
  releases: string[];
  dismissals: string[];
}

const FINDING = {
  ecosystem: "npm",
  name: "left-pad",
  version: "1.3.0",
  disposition: "would_block",
  rule: "license",
  reason: "left-pad 1.3.0 is licensed WTFPL and this organization does not admit WTFPL.",
  hits: 12,
  first_at: 1757000000000,
  last_at: 1757000000000,
};

/// The packages page, with nothing reachable but what this file mocked.
/// `signIn` registers the catch-all `**/v1/**` refusal first, so anything
/// neither of us named 404s here rather than falling through to whatever
/// is listening on :8080.
async function packagesPage(
  page: Page,
  opts: {
    npm?: string;
    packages?: unknown[];
    yanked?: boolean;
    findings?: unknown[];
    policy?: Partial<typeof BASE_POLICY>;
    /// Sign in as a token holder (`me` is null and the app treats that
    /// as an admin, which is what every other test here wants), or as a
    /// signed-in person whose role in this organization is `viewer`.
    asAdmin?: boolean;
    /// The billing view's `packages_refusal`; unset leaves billing
    /// unmocked, which the panel must survive.
    refusal?: string | null;
  } = {},
): Promise<Mocked> {
  const out: Mocked = {
    puts: [],
    yanks: [],
    policy: [],
    licences: [],
    reserves: [],
    releases: [],
    dismissals: [],
  };
  const state = { npm: opts.npm ?? "off", yanked: opts.yanked ?? false };
  const policy: {
    ecosystem: string;
    mode: string;
    cooldown_days: number;
    license_mode: string;
    license_rules: { spdx_id: string; disposition: string }[];
    reserved: string[];
  } = { ...BASE_POLICY, ...(opts.policy ?? {}) };
  let found = opts.findings ?? [];

  if (opts.asAdmin === false) {
    // A *person*, not a token. A token session never asks `/auth/me`
    // at all, so `me` stays null and the app rightly treats it as an
    // admin — the credential's own scopes are what gate it there. The
    // only way to be a viewer in this app is to be signed in as one.
    const viewer = {
      ...ME,
      orgs: [{ id: "01org", name: "acme", role: "viewer" }],
    };
    // A live cookie session, which is what the app boots from on every
    // load. Signing in through the form would work once and then break
    // on the very next `goto`: a cookie session stores an empty token,
    // so the boot path asks `/auth/me` again, and a fixture that
    // answered 401 there would drop straight back to the login form.
    await mockApi(page);
    await page.route("**/v1/auth/me", (r) => r.fulfill({ json: viewer }));
  } else {
    await signIn(page);
  }

  await page.route("**/v1/orgs/acme/packages/ecosystems", async (route) => {
    if (route.request().method() === "PUT") {
      const body = route.request().postDataJSON();
      out.puts.push(body);
      state.npm = body.mode;
      return route.fulfill({
        json: ecosystems(state.npm).find((e) => e.ecosystem === body.ecosystem),
      });
    }
    return route.fulfill({
      json: {
        ecosystems: ecosystems(state.npm),
        registry_base: "https://weft.test/v1/registry",
      },
    });
  });

  await page.route("**/v1/orgs/acme/packages", (route) =>
    route.fulfill({ json: { packages: opts.packages ?? [PACKAGE] } }),
  );

  await page.route(
    "**/v1/orgs/acme/packages/01PKG/versions/*/yank",
    async (route) => {
      const body = route.request().postDataJSON();
      out.yanks.push(body);
      state.yanked = body.yanked;
      return route.fulfill({ json: versions(state.yanked)[0] });
    },
  );

  await page.route("**/v1/orgs/acme/packages/01PKG", (route) =>
    route.fulfill({
      json: { ...PACKAGE, versions: versions(state.yanked), tags: [] },
    }),
  );

  // Registered after the bare `…/packages` route so these win: Playwright
  // matches handlers in reverse registration order, and a glob ending at
  // `packages` would otherwise be tried first for every sub-path.
  await page.route("**/v1/orgs/acme/packages/policy?*", async (route) => {
    if (route.request().method() === "PUT") {
      const body = route.request().postDataJSON();
      out.policy.push(body);
      Object.assign(policy, body);
    }
    return route.fulfill({ json: policy });
  });
  await page.route("**/v1/orgs/acme/packages/policy", async (route) => {
    if (route.request().method() === "PUT") {
      const body = route.request().postDataJSON();
      out.policy.push(body);
      Object.assign(policy, body);
    }
    return route.fulfill({ json: policy });
  });
  await page.route("**/v1/orgs/acme/packages/policy/licenses", async (route) => {
    const body = route.request().postDataJSON();
    out.licences.push(body);
    policy.license_rules = policy.license_rules.filter(
      (r) => r.spdx_id !== body.spdx_id.toLowerCase(),
    );
    if (body.disposition) {
      policy.license_rules = [
        ...policy.license_rules,
        { spdx_id: body.spdx_id.toLowerCase(), disposition: body.disposition },
      ];
    }
    return route.fulfill({ json: policy });
  });
  await page.route("**/v1/orgs/acme/packages/policy/namespaces*", async (route) => {
    if (route.request().method() === "DELETE") {
      const url = new URL(route.request().url());
      const pattern = url.searchParams.get("pattern") ?? "";
      out.releases.push(pattern);
      policy.reserved = policy.reserved.filter((r) => r !== pattern);
      return route.fulfill({ status: 204, body: "" });
    }
    const body = route.request().postDataJSON();
    out.reserves.push(body);
    policy.reserved = [...policy.reserved, body.pattern.toLowerCase()];
    return route.fulfill({ json: policy });
  });
  await page.route("**/v1/orgs/acme/packages/findings*", async (route) => {
    if (route.request().method() === "DELETE") {
      const url = new URL(route.request().url());
      out.dismissals.push(url.searchParams.get("name") ?? "");
      found = [];
      return route.fulfill({ status: 204, body: "" });
    }
    return route.fulfill({ json: { findings: found } });
  });

  if (opts.refusal !== undefined) {
    await page.route("**/v1/orgs/acme/billing", (route) =>
      route.fulfill({ json: { ...BILLING, packages_refusal: opts.refusal } }),
    );
  }

  await page.goto("/dashboard/settings/packages");
  return out;
}

test("every ecosystem is listed, and only the ones that serve can be switched on", async ({
  page,
}) => {
  await packagesPage(page);

  // Scoped to the ecosystems table. Unscoped, "npm" also matches the
  // ecosystem column of the published-packages table below, and a
  // locator that resolves to two things is one that would keep passing
  // if the row it was written for disappeared.
  const table = page.locator("table").filter({ has: page.getByTestId("mode-npm") });
  for (const label of ["npm", "Maven", "PyPI", "Cargo", "OCI"]) {
    await expect(table.getByRole("cell", { name: label, exact: true })).toBeVisible();
  }

  // npm serves, so it has a control.
  await expect(page.getByTestId("mode-npm")).toBeVisible();
  // All five serve now, so all five have a control. `SERVING` is not
  // decoration: an ecosystem listed but not serving would offer a
  // switch that promises a registry which is not there.
  for (const eco of ["maven", "pypi", "cargo", "oci"]) {
    await expect(page.getByTestId(`mode-${eco}`)).toBeVisible();
  }
});

test("switching npm on reaches the server and then shows the client snippet", async ({
  page,
}) => {
  const mocked = await packagesPage(page);

  // Nothing is on, so there is nothing to configure against yet.
  await expect(page.getByTestId("npmrc")).toBeHidden();

  await page.getByTestId("mode-npm").click();
  // Exact: "Private" is a prefix of "Private + proxy", and a lax name
  // match resolves to two options.
  await page.getByRole("option", { name: "Private", exact: true }).click();

  // The write actually went, with the ecosystem and mode it claimed.
  await expect
    .poll(() => mocked.puts)
    .toEqual([{ ecosystem: "npm", mode: "private" }]);

  const snippet = page.getByTestId("npmrc");
  await expect(snippet).toBeVisible();
  const text = (await snippet.textContent()) ?? "";
  // Built from the base the server reported — not from a constant here.
  expect(text).toContain("@acme:registry=https://weft.test/v1/registry/npm/acme/");
  expect(text).toContain("//weft.test/v1/registry/npm/acme/:_authToken=");
});

test("a version's provenance is on the screen, and yanking reaches the server", async ({
  page,
}) => {
  const mocked = await packagesPage(page, { npm: "private" });

  await expect(page.getByRole("cell", { name: "@acme/widget" })).toBeVisible();
  await page.getByRole("button", { name: "Versions" }).click();

  await expect(page.getByRole("cell", { name: "1.0.0" })).toBeVisible();
  await expect(page.getByRole("cell", { name: "MIT" })).toBeVisible();
  await expect(page.getByRole("cell", { name: "4.0 KB" })).toBeVisible();
  // The commit that built it — the reason this registry sits beside the
  // code — abbreviated the way every other sha on the dashboard is.
  await expect(page.getByRole("cell", { name: "c0ffeeba" })).toBeVisible();

  await page.getByRole("button", { name: "Yank" }).click();
  await expect.poll(() => mocked.yanks).toEqual([{ yanked: true, reason: null }]);
  // …and the row now says so, from the server's answer rather than
  // optimistically.
  await expect(page.getByText("yanked")).toBeVisible();
  await expect(page.getByRole("button", { name: "Un-yank" })).toBeVisible();
});

test("an organization with nothing published says so rather than showing an empty table", async ({
  page,
}) => {
  await packagesPage(page, { npm: "private", packages: [] });
  await expect(page.getByTestId("no-packages")).toHaveText(
    "Nothing published yet.",
  );
});

// The admission policy and the findings it produces.
//
// These are the screens the feature is actually sold on, and the ones a
// unit test cannot check the important thing about: that the control a
// person moves reaches the route it claims to. A mode dropdown that
// renders `block` and sends nothing looks right in every screenshot and
// leaves the organization admitting everything.

test("the policy is hidden until something can actually be proxied", async ({
  page,
}) => {
  await packagesPage(page, { npm: "private" });
  // A purely private registry admits nothing from outside, so every
  // control here would be a knob that cannot change an answer.
  await expect(page.getByTestId("policy-mode")).toBeHidden();
  await expect(page.getByTestId("no-findings")).toBeHidden();
});

test("switching the mode to block reaches the server", async ({ page }) => {
  const mocked = await packagesPage(page, { npm: "proxy" });

  await expect(page.getByTestId("policy-mode")).toBeVisible();
  await expect(
    page.getByText("nothing is being refused yet"),
  ).toBeVisible();

  await page.getByTestId("policy-mode").click();
  await page.getByRole("option", { name: "Block — refuse" }).click();

  await expect
    .poll(() => mocked.policy)
    .toEqual([{ mode: "block", cooldown_days: 0, license_mode: "deny_list" }]);
  // …and the screen now says what that means, from the server's answer.
  await expect(page.getByText("refusing what the rules below say")).toBeVisible();
});

test("a cooldown is sent as a number, and the control only saves a change", async ({
  page,
}) => {
  const mocked = await packagesPage(page, { npm: "proxy" });

  // Nothing to save until something changes — a Save that is always
  // live invites a round trip that rewrites the policy with itself.
  const save = page.getByRole("button", { name: "Save" });
  await expect(save).toBeDisabled();

  await page.getByTestId("cooldown").fill("7");
  await save.click();
  await expect
    .poll(() => mocked.policy)
    .toEqual([{ mode: "audit", cooldown_days: 7, license_mode: "deny_list" }]);

  // Not a string. `cooldown_days: "7"` is a 400 from the server and a
  // silent no-op on the screen.
  expect(typeof (mocked.policy[0] as { cooldown_days: unknown }).cooldown_days).toBe(
    "number",
  );
});

test("a licence rule is added, listed and removed", async ({ page }) => {
  const mocked = await packagesPage(page, { npm: "proxy" });

  await expect(page.getByTestId("no-license-rules")).toContainText(
    "A deny list with no rules admits every licence.",
  );

  await page.getByTestId("spdx-id").fill("GPL-3.0");
  await page.getByRole("button", { name: "Deny" }).click();
  await expect.poll(() => mocked.licences).toEqual([
    { spdx_id: "GPL-3.0", disposition: "deny" },
  ]);
  await expect(page.getByRole("cell", { name: "gpl-3.0" })).toBeVisible();
  await expect(page.getByText("denied")).toBeVisible();
  // The field clears, so the next rule is not a typo of the last one.
  await expect(page.getByTestId("spdx-id")).toHaveValue("");

  await page.getByRole("button", { name: "Remove" }).click();
  // Removing is its own thing, and it is not "deny". Under a deny list
  // an absent rule admits; under an allow list it refuses. Sending
  // `disposition: "deny"` here would silently invert the meaning on any
  // organization using an allow list.
  await expect
    .poll(() => mocked.licences)
    .toEqual([
      { spdx_id: "GPL-3.0", disposition: "deny" },
      { spdx_id: "gpl-3.0" },
    ]);
});

test("an allow list with no rules says that nothing will resolve", async ({
  page,
}) => {
  await packagesPage(page, {
    npm: "proxy",
    policy: { license_mode: "allow_list" },
  });
  // The failure mode this warns about is the one that reads as a broken
  // registry: an allow list nobody has filled in refuses everything.
  await expect(page.getByTestId("no-license-rules")).toContainText(
    "An allow list with no rules admits none",
  );
});

test("a namespace is reserved and released", async ({ page }) => {
  const mocked = await packagesPage(page, { npm: "proxy" });

  await expect(page.getByTestId("no-reserved")).toContainText("@acme");

  await page.getByTestId("reserve-pattern").fill("@acme");
  await page.getByRole("button", { name: "Reserve" }).click();
  await expect
    .poll(() => mocked.reserves)
    .toEqual([{ ecosystem: "npm", pattern: "@acme" }]);
  await expect(page.getByTestId("reserved")).toContainText("@acme");

  await page.getByRole("button", { name: "Release" }).click();
  await expect.poll(() => mocked.releases).toEqual(["@acme"]);
});

test("findings show what blocking would cost, and dismissing says what it is not", async ({
  page,
}) => {
  const mocked = await packagesPage(page, {
    npm: "proxy",
    findings: [FINDING],
  });

  // The number that decides whether to switch to block.
  await expect(page.getByTestId("would-block-count")).toHaveText(
    "1 package was served that blocking would have refused.",
  );
  await expect(page.getByText("would refuse")).toBeVisible();
  // The reason is the sentence the client printed, not a code.
  await expect(page.getByText("does not admit WTFPL")).toBeVisible();
  await expect(page.getByRole("cell", { name: "12" })).toBeVisible();

  await page.getByRole("button", { name: "Dismiss" }).click();
  await expect.poll(() => mocked.dismissals).toEqual(["left-pad"]);
  // And the screen is explicit that this changed no decision, which is
  // the thing somebody will otherwise assume it did.
  await expect(page.getByTestId("no-findings")).toBeVisible();
});

test("a viewer reads the policy and cannot change any of it", async ({ page }) => {
  await packagesPage(page, {
    npm: "proxy",
    policy: {
      mode: "block",
      cooldown_days: 14,
      license_rules: [{ spdx_id: "gpl-3.0", disposition: "deny" }],
      reserved: ["@acme"],
    },
    findings: [FINDING],
    asAdmin: false,
  });

  // Everything is legible…
  await expect(page.getByTestId("policy-mode")).toHaveText("Block — refuse");
  await expect(page.getByTestId("cooldown")).toHaveValue("14");
  await expect(page.getByRole("cell", { name: "gpl-3.0" })).toBeVisible();
  await expect(page.getByTestId("reserved")).toContainText("@acme");
  await expect(page.getByText("does not admit WTFPL")).toBeVisible();

  // …and nothing is actionable. A control that renders and then 404s is
  // worse than one that is absent: it reads as the product being broken
  // rather than as a permission they do not have.
  await expect(page.getByTestId("cooldown")).toBeDisabled();
  await expect(page.getByTestId("spdx-id")).toBeHidden();
  await expect(page.getByTestId("reserve-pattern")).toBeHidden();
  await expect(page.getByRole("button", { name: "Remove" })).toBeHidden();
  await expect(page.getByRole("button", { name: "Release" })).toBeHidden();
  await expect(page.getByRole("button", { name: "Dismiss" })).toBeHidden();
});

test("an organization the server will not let publish is told so, with the way out", async ({
  page,
}) => {
  const sentence =
    "quota: packages are private to an organization and need a paid plan — subscribe from Settings → Billing to publish here";
  await packagesPage(page, { npm: "private", refusal: sentence });
  const notice = page.getByTestId("packages-refusal");
  await expect(notice).toBeVisible();
  // The server's sentence, without its machine prefix.
  await expect(notice).toContainText("need a paid plan");
  await expect(notice).not.toContainText("quota:");
  await expect(notice.getByRole("link", { name: "Go to Billing" })).toHaveAttribute(
    "href",
    "/dashboard/settings/billing",
  );
  // The registry is still shown: installing is never refused.
  await expect(page.getByTestId("npmrc")).toBeVisible();
});

test("an organization that may publish sees no notice", async ({ page }) => {
  // Absence is only evidence once the answer it depends on has arrived.
  const billed = page.waitForResponse("**/v1/orgs/acme/billing");
  await packagesPage(page, { npm: "private", refusal: null });
  await billed;
  await expect(page.getByTestId("npmrc")).toBeVisible();
  await expect(page.getByTestId("packages-refusal")).toHaveCount(0);
});
