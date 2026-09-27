// Every forge read carries the viewer's credential.
//
// This is a silent-failure test, which is why it asserts on the request
// rather than on the rendering. The forge built every session as
// `anon(owner)` — a name that reads as "the session for this page" and
// means "no credential at all". A cookie session hides that completely,
// because the browser attaches the cookie itself, so the defect only
// ever appeared for somebody signed in with an API token. And the
// server filters by who is asking, so the symptom was not an error: a
// person was shown less of their own work and told nothing.
//
// So there is no rendered thing to look at. The header is the fact.

import { expect, test, type Page } from "@playwright/test";
import { REPOS, signIn } from "./fixtures";

const widget = { ...REPOS.repos[0], org: "acme", public: false };

/// The `Authorization` header on the repository read, waited for rather
/// than looked up after the fact.
///
/// Waiting on the request is the point. The first version of this
/// recorded headers into a map and then asserted on the map once a link
/// was visible — and the masthead link is rendered from the URL, before
/// any fetch. So the map was empty, the header was `undefined` rather
/// than absent, and the test was measuring nothing. Wait on the thing
/// the assertion is about (CLAUDE.md, and the form-clear race that
/// taught it).
function repoRead(page: Page) {
  return page.waitForRequest(
    (r) => new URL(r.url()).pathname === "/v1/orgs/acme/repos/widget",
  );
}

test("a token viewer's credential reaches the repository read", async ({
  page,
}) => {
  await signIn(page);
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: widget }),
  );
  const read = repoRead(page);
  await page.goto("/acme/widget");
  const auth = (await read).headers()["authorization"];
  // Not `toBeTruthy()`: the whole bug was a header that was absent, and
  // a test that only asked whether something was there would pass
  // against a session carrying the wrong token.
  expect(auth).toBe("Bearer weft_test_token");
});

test("a stranger's repository read carries no credential", async ({ page }) => {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ status: 200, json: { ...widget, public: true } }),
  );
  const read = repoRead(page);
  await page.goto("/acme/widget");
  // The other half of the rule, and the half that makes the first one
  // mean something. Public read must still work with no credential at
  // all — a forge that quietly requires one has stopped being a forge.
  expect((await read).headers()["authorization"]).toBeUndefined();
});
