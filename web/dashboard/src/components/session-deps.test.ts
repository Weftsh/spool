import { describe, expect, it } from "vitest";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

/// No shared control may hang an effect off the whole session object.
///
/// `Session` is `{ org, token }` and callers build it inline. The forge
/// repository page calls `anon(owner)`, which is a fresh object on every
/// render — so an effect whose dependency array names `session` re-runs
/// forever, refetching on every pass. `WatchButton` was changed to
/// destructure `{ org, token }` for exactly that reason, with a comment
/// naming the loop; `StarButton` beside it was not, and was safe only
/// because its one caller memoises the session. That caller's own
/// comment says "not every component does, so the caller holds it still
/// as well" — which is a note about the next caller, not a defence. The
/// hazard belongs to the control, so the rule does.
///
/// **Structural, because no rendering test can fail on it.** The loop
/// only appears for a caller that does *not* memoise, and every caller
/// today does; a spec driving the real page would pass against the bug.
/// The same reason `hermetic-specs.test.ts` is written this way: the
/// failure arrives from a distance, and from a file nobody edited.
///
/// Scoped to `src/components`, which is where the controls a page drops
/// into a masthead live. `src/views` has the same hazard in places and
/// is not covered here — that is a finding rather than a silent
/// exemption, and widening this rule is the fix.
describe("shared controls depend on session primitives, not the object", () => {
  const dir = __dirname;
  const files = readdirSync(dir).filter((f) => f.endsWith(".tsx"));

  it("finds the components at all", () => {
    // A guard on the guard: if this directory ever moves, every
    // assertion below passes vacuously over an empty list, which is the
    // exact class of bug the file exists to catch.
    expect(files.length).toBeGreaterThan(5);
  });

  /// Every hook dependency array in a file, as a list of raw entries.
  ///
  /// Matched on the closing `}, [...]);` that every hook call in this
  /// codebase is written with. A hook spelled some other way is not
  /// checked, which is why the count is asserted below.
  const deps = (src: string): string[][] =>
    [...src.matchAll(/\}, \[([^\]]*)\]\)/g)].map((m) =>
      m[1]
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean),
    );

  let arrays = 0;
  for (const file of files) {
    const src = readFileSync(join(dir, file), "utf8");
    const lists = deps(src);
    arrays += lists.length;
    if (!lists.length) continue;

    it(`${file} names session primitives in its dependency arrays`, () => {
      for (const list of lists)
        for (const entry of list)
          expect(
            ["session", "props.session"].includes(entry),
            `${file} has an effect depending on \`${entry}\`. A caller ` +
              "that builds its session inline — `anon(owner)`, which is " +
              "how the forge repo page calls its data — hands a fresh " +
              "object every render, so this refetches forever. " +
              "Destructure `const { org, token } = props.session` and " +
              "depend on those, as WatchButton and StarButton do.",
          ).toBe(false);
    });
  }

  it("actually read some dependency arrays", () => {
    // Without this the regex could stop matching — a formatter change
    // is enough — and every assertion above would pass over nothing.
    expect(arrays).toBeGreaterThan(1);
  });
});
