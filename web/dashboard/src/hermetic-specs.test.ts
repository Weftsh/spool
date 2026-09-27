import { describe, expect, it } from "vitest";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

/// Every Playwright spec that drives a page must refuse unmocked calls.
///
/// Without a catch-all `**/v1/**` route, a request the spec did not mock
/// falls through vite's proxy to whatever is listening on **:8080**,
/// which during development is a real seeded server. The suite is then
/// hermetic only while nobody has the manual stack up, and a test passes
/// or fails depending on that — the sort of intermittency that gets
/// called a flake and re-run.
///
/// **This is checked structurally because the failure arrives from a
/// distance and in silence.** Nobody breaks it by editing a spec. It
/// breaks when a *page* gains one new fetch: every test that renders
/// that page becomes non-hermetic at once, and not one of them fails to
/// say so. It happened twice in a single afternoon — `/meta` on the
/// repository page and `/users/:handle/contributions` on the profile —
/// and both were found by noticing proxy errors scroll past a passing
/// run, which is not a method.
///
/// A spec that never navigates is exempt: with no page there is nothing
/// to intercept.
describe("playwright specs are hermetic", () => {
  const dir = join(__dirname, "..", "tests");
  const specs = readdirSync(dir).filter((f) => f.endsWith(".spec.ts"));

  it("finds the specs at all", () => {
    // A guard on the guard. If the directory moves, every assertion
    // below vacuously passes over an empty list and this file becomes
    // decoration — which is the exact class of bug it exists to catch.
    expect(specs.length).toBeGreaterThan(5);
  });

  for (const spec of specs) {
    const src = readFileSync(join(dir, spec), "utf8");
    const navigates = src.includes("page.goto(");
    if (!navigates) continue;

    it(`${spec} refuses calls it did not mock, in every test`, () => {
      // Per **test**, not per file. Checking the file merely contains a
      // catch-all is what let `watch.spec.ts` through: it had one, in a
      // setup helper, and one test set up its own mocks instead and
      // bypassed it. A file-level check would have called that spec
      // guarded while it leaked on every run.
      //
      // So each test body must either navigate through a helper — any
      // call to a local `async function` that itself registers the
      // catch-all — or register one directly. Tests that never navigate
      // are exempt: with no page there is nothing to intercept.
      // Shared setup helpers in `tests/fixtures.ts` that register the
      // catch-all themselves. Named explicitly because this file cannot
      // see across imports, and because if one of them ever stops
      // refusing, listing it here is what makes that a decision rather
      // than a silent regression.
      const SHARED = ["mockApi(", "signIn(", "signInAsPerson("];
      const guards = (text: string) =>
        text.includes('"**/v1/**"') ||
        text.includes("hermetic(page)") ||
        SHARED.some((h) => text.includes(h));

      // Helpers in this file that establish the refusal themselves.
      //
      // The parameter list is matched loosely on purpose: real helpers
      // here take options objects across several lines, and a regex
      // insisting on `(page` immediately after the name silently found
      // none of them — which made this guard report two dozen false
      // positives, and a guard that cries wolf is one somebody deletes.
      const helpers = [
        ...src.matchAll(/(?:async function|function) (\w+)\s*\(/g),
      ]
        .map((m) => ({ name: m[1], at: m.index ?? 0 }))
        .filter(({ at }) => {
          // The body runs to the next line that closes at column zero.
          const end = src.indexOf("\n}", at);
          return guards(src.slice(at, end === -1 ? src.length : end));
        })
        .map(({ name }) => name);

      const bodies = src.split(/\ntest(?:\.\w+)?\(/).slice(1);
      const leaking = bodies
        .filter((b) => b.includes("page.goto("))
        .filter((b) => {
          const body = b.split("\n});")[0];
          if (guards(body)) return false;
          return !helpers.some((h) => body.includes(`${h}(`));
        })
        .map((b) => (b.match(/^\s*"([^"]+)"/) ?? [, b.slice(0, 40)])[1]);

      expect(
        leaking,
        `these tests in ${spec} navigate without refusing unmocked /v1 ` +
          `calls, so a request they do not mock reaches whatever is ` +
          `listening on :8080`,
      ).toEqual([]);
    });
  }
});

/// Every credential the server mails is read by something in the client.
///
/// `mail/templates.rs` builds links as `/dashboard/#<key>=<token>`, and
/// `App.tsx` reads them with `tokenFromHash("<key>")`. Those two lists
/// have to match, and nothing made them: `verify-email` was mailed for
/// as long as adding an address has existed, and **nothing in the client
/// ever read it**. Clicking the link landed on the dashboard and did
/// nothing at all — a promise made in an email we send, in the one flow
/// where the person is already doing what we asked.
///
/// It is checked from the client side because that is the half that goes
/// missing: a mail template is written once and works; the reader is a
/// separate change in a separate file that can simply not be made.
///
/// **What this does and does not catch, stated plainly.** It is a
/// presence check, so it reddens when a key has *no* reader at all —
/// which is the failure that actually happened and the one worth a
/// gate. It does **not** notice a key that keeps one reader and loses
/// another: removing only the boot-time read left the `hashchange`
/// read behind and this stayed green, which I checked rather than
/// assumed. Catching that needs the client run, not a string search,
/// and the Playwright suite is where it belongs.
describe("every mailed link has a reader", () => {
  const app = readFileSync(join(__dirname, "App.tsx"), "utf8");
  const templates = readFileSync(
    join(
      __dirname,
      "..",
      "..",
      "..",
      "crates",
      "stratum-server",
      "src",
      "mail",
      "templates.rs",
    ),
    "utf8",
  );

  const mailed = [
    ...new Set(
      [...templates.matchAll(/token_url\([^,]+,\s*"([a-z-]+)"/g)].map(
        (m) => m[1],
      ),
    ),
  ];

  it("finds the mail templates and the keys they build", () => {
    // Guard on the guard: if the path moves or the helper is renamed,
    // every assertion below passes over an empty list.
    expect(mailed.length).toBeGreaterThan(2);
  });

  for (const key of mailed) {
    it(`the client reads #${key}=`, () => {
      expect(
        app.includes(`tokenFromHash("${key}")`),
        `the server mails a link containing #${key}= and nothing in ` +
          `App.tsx reads it, so clicking it does nothing`,
      ).toBe(true);
    });
  }
});
