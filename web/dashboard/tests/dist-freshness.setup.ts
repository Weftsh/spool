import fs from "node:fs";
import path from "node:path";
import { test as setup, expect } from "@playwright/test";

/// Refuse to test a bundle older than the source it was built from.
///
/// Both browser gates read `dist`, never `src`. That is quiet in both
/// directions and it cost three false greens in one afternoon: a `tsc`
/// error fails `npm run build`, `vite preview` happily serves the
/// *previous* bundle, and the suite passes — so a fix looks like it did
/// not work, and, far worse, reverting a fix to check that its test
/// really goes red shows the test passing. That reads as "the test is
/// decoration" when in fact nothing was rebuilt.
///
/// Noticing is not a discipline anybody can keep. It is an interlock.
setup("the bundle under test is not older than the source", () => {
  const newest = (dir: string): number => {
    let max = 0;
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      const p = path.join(dir, e.name);
      max = Math.max(max, e.isDirectory() ? newest(p) : fs.statSync(p).mtimeMs);
    }
    return max;
  };

  const built = fs.existsSync("dist/index.html")
    ? fs.statSync("dist/index.html").mtimeMs
    : 0;
  expect(
    built,
    "dist/index.html is missing — run `npm run build` before the suite",
  ).toBeGreaterThan(0);

  const src = newest("src");
  expect(
    built,
    `dist is older than src: the build did not run, or it failed and left the ` +
      `previous bundle in place. Run \`npm run build\` and read its output — ` +
      `a tsc error there is why this suite would otherwise have tested ` +
      `yesterday's code and told you it was fine.`,
  ).toBeGreaterThan(src);
});
