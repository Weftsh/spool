// A profile page opened by somebody holding an API token.
//
// The forge's data calls used to be built as `anon(owner)` everywhere,
// so a caller signed in with a token was sent unauthenticated requests
// and shown a stranger's view of the world. core-1f fixed that for the
// repository page; this pins the profile page, which is reached through
// the same shell and had the same defect.
//
// The visible cost is not cosmetic: the repository grid is fed by
// `/v1/search/repos`, which the server filters by who is asking. With
// no credential on the request, a person looking at their own profile
// while signed in with a token does not see their own private
// repositories — the page tells them their work is not there.

import { expect, test } from "@playwright/test";

const TOKEN = "stk_test_token";

test("a profile read carries the viewer's token", async ({ page }) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  const auth: (string | null)[] = [];
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/search/repos*", (r) => {
    auth.push(r.request().headers()["authorization"] ?? null);
    return r.fulfill({ status: 200, json: { repos: [], next: null } });
  });
  await page.route("**/v1/users/ada/pins", (r) => {
    auth.push(r.request().headers()["authorization"] ?? null);
    return r.fulfill({ status: 200, json: { pins: [] } });
  });
  await page.route("**/v1/users/ada", (r) =>
    r.fulfill({
      status: 200,
      json: {
        handle: "ada",
        name: "ada",
        display_name: null,
        bio: null,
        location: null,
        company: null,
        pronouns: null,
        kind: "human",
        contrib_private_optin: false,
        profile_repo: null,
        created_at: 1,
        links: [],
        public_repos: 0,
      },
    }),
  );
  // A token session, exactly as the client stores one.
  await page.addInitScript(
    ([token]) => {
      try {
        localStorage.setItem(
          "stratum-session",
          JSON.stringify({ org: "ada", token }),
        );
      } catch {
        /* storage unavailable */
      }
    },
    [TOKEN],
  );
  await page.goto("/ada");
  await expect(page.getByRole("heading", { name: "ada" })).toBeVisible();
  await expect.poll(() => auth.length).toBeGreaterThan(0);
  // Every profile read must carry it. A single unauthenticated one is
  // enough to hide the viewer's own work from them.
  for (const h of auth) {
    expect(h).toBe(`Bearer ${TOKEN}`);
  }
});
