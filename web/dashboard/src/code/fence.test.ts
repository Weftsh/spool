import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

/// The code libraries are imported in one place, and this notices when
/// they are imported in two.
///
/// `@pierre/diffs` and `@pierre/trees` are the largest things in this
/// bundle by a distance, and they are tolerated because `src/code/` loads
/// them lazily and the entry chunk never sees them. One ordinary import
/// anywhere else — a type someone reached for as a value, a helper that
/// looked handy — pulls the whole of it into the entry chunk, and nothing
/// on the page would look different. `tests/bundle-budget.setup.ts`
/// would catch the weight after the build; this catches the cause before
/// it, with a file name.
///
/// Type-only imports are erased by tsc and are fine, but the shapes are
/// re-exported from `src/code/types.ts` so nobody has to remember that.
describe("only src/code imports the code libraries", () => {
  const SRC = dirname(fileURLToPath(import.meta.url)).replace(/\/code$/, "");
  const files: string[] = [];
  const walk = (dir: string) => {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, e.name);
      if (e.isDirectory()) walk(p);
      else if (/\.(ts|tsx)$/.test(e.name)) files.push(p);
    }
  };
  walk(SRC);

  it("finds the sources at all", () => {
    expect(files.length).toBeGreaterThan(20);
  });

  const outside = files.filter((f) => !f.startsWith(join(SRC, "code") + "/"));
  it("no file outside src/code imports @pierre", () => {
    const leaking = outside
      .filter((f) => /from\s+["']@pierre\//.test(readFileSync(f, "utf8")))
      .map((f) => relative(SRC, f));
    expect(
      leaking,
      "these files import @pierre/* directly, which drags the highlighter " +
        "into the entry chunk; go through src/code/lazy.tsx (values) or " +
        "src/code/types.ts (types) instead",
    ).toEqual([]);
  });

  it("src/code itself imports the libraries — the fence is not vacuous", () => {
    const surface = readFileSync(join(SRC, "code", "surface.tsx"), "utf8");
    expect(surface).toMatch(/from "@pierre\/diffs/);
    expect(surface).toMatch(/from "@pierre\/trees/);
  });
});
