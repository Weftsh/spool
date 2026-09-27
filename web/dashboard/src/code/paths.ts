/// Pure helpers for the code surfaces, kept free of `@pierre/*` imports so
/// they run in vitest and ship in the main chunk. The libraries live in
/// `./surface.tsx` and nowhere else — see `fence.test.ts`.

/// Every directory above a file, as the tree component spells them:
/// trailing slash, outermost first. A root-level file has none.
export function ancestorsOf(path: string): string[] {
  const parts = path.split("/").filter((p) => p !== "");
  const out: string[] = [];
  for (let i = 1; i < parts.length; i++) {
    out.push(parts.slice(0, i).join("/") + "/");
  }
  return out;
}

/// Which theme the highlighter should commit to, from what the page has
/// committed to. `web/shared/tokens.css` switches every colour by
/// `color-scheme`: `light dark` means "follow the system", which is what
/// the dashboard does today, and a page that pins itself (the marketing
/// site) sets one of the two. The highlighter takes the same three
/// answers, so a future pin on the dashboard reaches the code with no
/// further plumbing.
export type Scheme = "system" | "light" | "dark";

export function schemeFrom(colorScheme: string): Scheme {
  const words = colorScheme.trim().split(/\s+/);
  if (words.includes("light") && words.includes("dark")) return "system";
  if (words.includes("dark")) return "dark";
  if (words.includes("light")) return "light";
  return "system";
}

export function pageScheme(): Scheme {
  if (typeof document === "undefined") return "system";
  return schemeFrom(getComputedStyle(document.documentElement).colorScheme);
}

/// What the Shiki CSS-variables theme falls back to when a `--diffs-*`
/// variable is unset. Every value is another variable, so the theme never
/// carries a colour: the colours live in `web/shared/tokens.css` as
/// `--code-*` and `src/code/pierre.css` aliases them onto the names the
/// highlighter reads. Keys are Shiki's, without the prefix.
export const SYNTAX_DEFAULTS: Record<string, string> = {
  foreground: "var(--code-fg)",
  background: "var(--surface-1)",
  "token-comment": "var(--code-comment)",
  "token-keyword": "var(--code-keyword)",
  "token-string": "var(--code-string)",
  "token-string-expression": "var(--code-string)",
  "token-constant": "var(--code-constant)",
  "token-function": "var(--code-function)",
  "token-parameter": "var(--code-parameter)",
  "token-punctuation": "var(--code-punctuation)",
  "token-link": "var(--code-link)",
};

/// The one theme name the code surfaces use, registered in `surface.tsx`.
/// Both halves name it because the library wants a light and a dark
/// theme; ours is one theme whose variables are themselves `light-dark()`.
export const WEFT_THEME = "weft";

/// How many rows a fully expanded tree of these paths draws, with empty
/// directory chains run together the way `flattenEmptyDirectories` does
/// — so a box can be sized to its content instead of guessed at. Files
/// count one each; a directory counts one unless its only child is
/// another directory, in which case the two are one row.
export function treeRows(paths: readonly string[]): number {
  const files = paths.filter((p) => !p.endsWith("/"));
  const children = new Map<string, Set<string>>();
  const note = (dir: string, child: string) => {
    const set = children.get(dir) ?? new Set<string>();
    set.add(child);
    children.set(dir, set);
  };
  for (const f of files) {
    const parts = f.split("/").filter((x) => x !== "");
    for (let i = 1; i < parts.length; i++) {
      note(parts.slice(0, i).join("/") + "/", parts.slice(0, i + 1).join("/") + (i + 1 < parts.length ? "/" : ""));
    }
  }
  let dirs = 0;
  for (const [, kids] of children) {
    const only = kids.size === 1 ? [...kids][0] : null;
    if (only && only.endsWith("/")) continue; // folded into its child
    dirs += 1;
  }
  return files.length + dirs;
}

/// The library's default row height, in pixels (`--trees-item-height`).
export const TREE_ROW_PX = 30;

/// A height that fits `rows` rows and no more, up to `max`.
export function treeHeight(rows: number, max: number): number {
  return Math.min(max, rows * TREE_ROW_PX + 8);
}
