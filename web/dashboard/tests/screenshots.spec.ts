// Marketing screenshots: drives the real dashboard build with seeded data
// and captures the frames the site embeds. Skipped unless SCREENSHOT_DIR
// is set — CI's e2e job never produces artifacts, a developer regenerates
// them deliberately:
//
//   SCREENSHOT_DIR=../site/public/screenshots npx playwright test screenshots
//
// Keep the seed realistic: these numbers are what a prospect studies.

import { test, type Page } from "@playwright/test";
import * as path from "path";

const DIR = process.env.SCREENSHOT_DIR;
test.skip(!DIR, "SCREENSHOT_DIR not set");

const NOW = Date.now();

const REPOS = {
  repos: [
    repo("01a", "linux-mirror", "mirror", "torvalds/linux", 42_000, null),
    repo("01b", "webapp", "mirror", "acme/webapp", 18_000, null),
    repo("01c", "design-system", "mirror", "acme/design-system", 65_000, null),
    repo(
      "01d",
      "infra",
      "mirror",
      "acme/infra",
      12_000,
      "origin unreachable: connection refused",
    ),
    repo("01e", "agent-session-1", "native", null, null, null),
    repo("01f", "agent-session-2", "native", null, null, null),
  ],
};

function repo(
  id: string,
  name: string,
  kind: string,
  origin: string | null,
  syncAgeMs: number | null,
  syncError: string | null,
) {
  return {
    id,
    org_id: "01org",
    name,
    kind,
    default_branch: "main",
    origin_url: origin,
    last_sync_at: syncAgeMs == null ? null : NOW - syncAgeMs,
    last_synced_commit: origin ? "9f2c41d" : null,
    sync_error: syncError,
    created_at: NOW - 90 * 86_400_000,
    clone_url: `https://git.stratum.dev/acme/${name}.git`,
    ssh_clone_url: `ssh://git@git.stratum.dev/acme/${name}.git`,
    // The frame below is the Insights tab, which is offered to anybody
    // the server says holds a role here (`viewer_member`). The seed once
    // said only `viewer_admin`, from when Insights was admin-only; when
    // the gate moved, this frame 404'd, and because the spec only runs
    // when SCREENSHOT_DIR is set, nothing noticed until the next time
    // somebody regenerated the site's pictures.
    viewer_admin: true,
    viewer_member: true,
    viewer_write: true,
  };
}

const USAGE = {
  days: [
    day("2026-08-20", 6, 6, 48_211, 512),
    day("2026-08-19", 6, 6, 51_804, 587),
    day("2026-08-18", 5, 6, 44_310, 431),
    day("2026-08-17", 5, 6, 39_650, 402),
    day("2026-08-16", 4, 6, 21_020, 198),
    day("2026-08-15", 4, 6, 18_744, 171),
    day("2026-08-14", 5, 6, 40_112, 388),
  ],
};

function day(
  d: string,
  active: number,
  total: number,
  requests: number,
  mb: number,
) {
  return {
    day: d,
    active_repos: active,
    total_repos: total,
    requests,
    bytes_out: mb * 1_048_576,
    reported_at: 1,
  };
}

const METRICS = {
  repo: "linux-mirror",
  from: 0,
  to: 1,
  kinds: {
    clone: {
      count: 184,
      bytes: 412_316_860_416,
      ms_sum: 190_000,
      p50_ms: 41_000,
      p99_ms: 92_000,
    },
    fetch: {
      count: 12_408,
      bytes: 9_663_676_416,
      ms_sum: 420_000,
      p50_ms: 28,
      p99_ms: 210,
    },
    freshness: {
      count: 0,
      bytes: 0,
      ms_sum: 36_000,
      p50_ms: 3_200,
      p99_ms: 7_900,
    },
  },
  sync: { last_sync_at: NOW - 42_000, sync_error: null },
};

const ME = {
  id: "01user",
  email: "ada@acme.dev",
  name: "Ada Lovelace",
  created_at: NOW - 400 * 86_400_000,
  // Proved long ago. Without it the frame led with the "confirm your
  // address to create repositories" banner — an onboarding nag, shown
  // to every prospect as the first thing the product says.
  verified_at: NOW - 400 * 86_400_000,
  handle: "ada",
  orgs: [{ id: "01org", name: "acme", role: "owner" }],
};

async function seed(page: Page) {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/orgs/acme/repos?limit=200", (r) =>
    r.fulfill({ json: REPOS }),
  );
  await page.route("**/v1/orgs/acme/usage", (r) => r.fulfill({ json: USAGE }));
  await page.route("**/v1/orgs/acme/repos/linux-mirror/metrics", (r) =>
    r.fulfill({ json: METRICS }),
  );
  await page.route("**/v1/orgs/acme/repos/linux-mirror", (r) =>
    r.fulfill({ json: REPOS.repos[0] }),
  );
  await page.route("**/v1/orgs/acme/ssh-keys", (r) =>
    r.fulfill({
      json: {
        keys: [
          {
            id: "01k1",
            token_id: null,
            user_id: "01user",
            algo: "ssh-ed25519",
            public_key: "ssh-ed25519 AAAA…",
            fingerprint_sha256:
              "SHA256:xyrwjwNKqTIivpsCwlGBJoCJCtNo5voQKyNc3jGN9iI",
            label: "deploy@ci",
            created_at: NOW - 30 * 86_400_000,
            revoked_at: null,
          },
        ],
      },
    }),
  );
  // Sign in as a person, the way the product is meant to be used.
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await page.route("**/v1/auth/login", (r) => r.fulfill({ json: ME }));
  await page.goto("/dashboard/");
  await page.getByText("Requests today").waitFor();
}

for (const scheme of ["light", "dark"] as const) {
  test(`overview ${scheme}`, async ({ page }) => {
    await page.emulateMedia({ colorScheme: scheme });
    await page.setViewportSize({ width: 1440, height: 960 });
    await seed(page);
    await page.screenshot({
      path: path.join(DIR!, `overview-${scheme}.png`),
      fullPage: false,
    });
  });

  test(`repo detail ${scheme}`, async ({ page }) => {
    await page.emulateMedia({ colorScheme: scheme });
    // The repo view is compact; a tight frame keeps the site card dense.
    await page.setViewportSize({ width: 1440, height: 640 });
    await seed(page);
    // The same numbers this frame has always shown, at the address they
    // live at now: the repository's own page, Insights tab. They used to
    // be a dashboard screen reached by clicking a row, which had no URL.
    await page.goto("/acme/linux-mirror/insights");
    await page.getByText("Clone p50 / p99").waitFor();
    await page.screenshot({
      path: path.join(DIR!, `repo-${scheme}.png`),
      fullPage: false,
    });
  });
}
