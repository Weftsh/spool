import { defineConfig } from "@playwright/test";
import { PREVIEW_ORIGIN, PREVIEW_PORT } from "./tests/preview";

export default defineConfig({
  testDir: "./tests",
  timeout: 30_000,
  // One worker, everywhere. Playwright's default is half the machine's
  // cores, so a two-core CI runner serialises the suite while a bigger
  // development machine does not — and "passes here, fails there" is
  // then a property of the hardware rather than of the code. Pinning it
  // costs about ten seconds and makes the local run a reproduction.
  workers: 1,
  // The freshness check runs first and everything depends on it, so a
  // stale bundle fails the run in a second rather than passing it in
  // thirty. See tests/dist-freshness.setup.ts for what that cost. The
  // bundle budget rides in the same project: it, too, reads dist and
  // has to run after the build (tests/bundle-budget.setup.ts).
  projects: [
    { name: "freshness", testMatch: /\.setup\.ts$/ },
    {
      name: "dashboard",
      testMatch: /.*\.spec\.ts/,
      dependencies: ["freshness"],
    },
  ],
  use: {
    baseURL: PREVIEW_ORIGIN,
    // Environments that pre-provision a Chromium (PLAYWRIGHT_BROWSERS_PATH
    // or CHROMIUM_PATH) use it; otherwise Playwright's own install.
    launchOptions: process.env.CHROMIUM_PATH
      ? { executablePath: process.env.CHROMIUM_PATH }
      : {},
  },
  webServer: {
    // Preview the production build; API calls are mocked per-test via
    // page.route so the suite is hermetic. --host 127.0.0.1 pins IPv4:
    // the default "localhost" bind can resolve to ::1 only (CI runners),
    // and the health-check URL below polls 127.0.0.1.
    // `--strictPort`, still: a preview that silently moved to another
    // port would serve a suite pointed at this one. The port itself is
    // per-worktree now (see tests/preview.ts) so the collision that
    // makes strictness bite cannot happen between checkouts.
    command: `npx vite preview --host 127.0.0.1 --port ${PREVIEW_PORT} --strictPort`,
    url: `${PREVIEW_ORIGIN}/dashboard/`,
    reuseExistingServer: !process.env.CI,
  },
});
