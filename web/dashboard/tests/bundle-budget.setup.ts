import fs from "node:fs";
import path from "node:path";
import { test as setup, expect } from "@playwright/test";

/// The entry chunk carries no code library, and the code library is a
/// separate asset. Measured after the build, because that is the only
/// place it can be measured.
///
/// `src/code/fence.test.ts` catches the *cause* — an import of
/// `@pierre/*` outside `src/code/` — before the build. This catches the
/// *effect*: a Vite setting, a re-export, or a dependency that quietly
/// inlines the highlighter or its worker into the chunk every page
/// loads. Nothing on screen changes when that happens; only the number
/// does. It sits in Playwright's `freshness` project rather than vitest
/// because CI runs vitest before `npm run build`, and a vitest test that
/// read `dist/` would be measuring yesterday's bundle.
///
/// The budgets are the first real build plus a little: tighten them when
/// a change makes the entry smaller, never loosen one without saying why
/// in the commit.
// Raised from 880_000 on 2026-09-16, and the reason belongs here rather
// than only in a commit nobody will find. Two features landed together —
// the packages settings and findings screens, and signing in with GitHub
// — and neither crossed the line alone; together they were 1,782 bytes
// over, which is 0.2%. What this guard actually watches was checked
// before loosening it: the entry chunk contains no `@pierre`, no
// `diffs-container` and no `file-tree-container`, and the code library is
// still a separate asset. A highlighter leaking in is hundreds of
// kilobytes, so the headroom here does not blind it.
const ENTRY_BUDGET = 890_000;
const SURFACE_BUDGET = 1_200_000;

setup("the entry chunk stays free of the code libraries", () => {
  const html = fs.readFileSync("dist/index.html", "utf8");
  const entry = html.match(/<script[^>]+type="module"[^>]+src="([^"]+)"/)?.[1];
  expect(entry, "dist/index.html names no module script").toBeTruthy();
  const file = path.join("dist", entry!.replace(/^\/dashboard\//, ""));
  const size = fs.statSync(file).size;
  expect(
    size,
    `${file} is ${size} bytes, over the ${ENTRY_BUDGET} budget. If the ` +
      `growth is the highlighter or the tree, some file outside src/code/ ` +
      `now imports @pierre/* (src/code/fence.test.ts names it); if it is ` +
      `something else, decide whether it belongs on every page.`,
  ).toBeLessThanOrEqual(ENTRY_BUDGET);
  const text = fs.readFileSync(file, "utf8");
  for (const marker of [
    "diffs-container",
    "file-tree-container",
    "createHighlighter",
    "shiki",
  ]) {
    expect(
      text.includes(marker),
      `the entry chunk contains "${marker}": the code library is no ` +
        `longer behind the lazy import in src/code/lazy.tsx`,
    ).toBe(false);
  }
});

setup("the code library and its worker are separate assets", () => {
  const assets = fs.readdirSync("dist/assets");
  const surface = assets.filter((a) => /^surface-.*\.js$/.test(a));
  const worker = assets.filter((a) => /^worker-.*\.js$/.test(a));
  expect(
    surface,
    "no surface-*.js chunk: src/code/surface.tsx is no longer split out",
  ).toHaveLength(1);
  expect(
    worker,
    "no worker-*.js asset: the highlighting worker was inlined — check " +
      "`worker.format` in vite.config.ts",
  ).toHaveLength(1);
  const size = fs.statSync(path.join("dist/assets", surface[0])).size;
  expect(
    size,
    `${surface[0]} is ${size} bytes, over the ${SURFACE_BUDGET} budget: ` +
      `something that should be its own chunk (a grammar, the worker) ` +
      `was folded into the code surface`,
  ).toBeLessThanOrEqual(SURFACE_BUDGET);
});
