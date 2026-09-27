import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

// The theming contract, as a test instead of a convention.
//
// `npx shadcn add` writes raw palettes and `.dark` blocks into the css
// file it is pointed at (https://ui.shadcn.com/docs/theming), and a
// vendored component can arrive using a token nobody bridged. Both have
// actually happened here: the sidebar `add` appended an hsl palette to
// styles.css, and the sidebar landed once with its `--sidebar-*`
// aliases missing. COMPONENTS.md says what to do; this file notices
// when it wasn't done.

// node:fs rather than ?raw imports: vitest's CSS pipeline stubs .css
// modules to empty strings before the raw query applies.
const SRC = dirname(fileURLToPath(import.meta.url));
const stylesCss = readFileSync(join(SRC, "styles.css"), "utf8");
const shadcnCss = readFileSync(join(SRC, "shadcn.css"), "utf8");
const pierreCss = readFileSync(join(SRC, "code", "pierre.css"), "utf8");
const uiDir = join(SRC, "components/ui");
const uiSources = readdirSync(uiDir)
  .filter((f) => f.endsWith(".tsx"))
  .map((f) => ({ file: f, text: readFileSync(join(uiDir, f), "utf8") }));

describe("the theming contract", () => {
  it("styles.css stays imports-only — no palette the CLI appended", () => {
    // One value per color, in web/shared/tokens.css, via light-dark().
    expect(stylesCss).not.toMatch(/\.dark\b/);
    expect(stylesCss).not.toMatch(/@custom-variant/);
    expect(stylesCss).not.toMatch(/\bhsl\(|\boklch\(|#[0-9a-fA-F]{3,8}\b/);
    expect(stylesCss).not.toMatch(/:root/);
  });

  it("shadcn.css is aliases only — every declaration resolves a var", () => {
    // The file's own comments are allowed to say ".dark"; the rules
    // apply to what the browser reads.
    const code = shadcnCss.replace(/\/\*[\s\S]*?\*\//g, "");
    expect(code).not.toMatch(/\.dark\b/);
    const declarations = code.match(/--[\w-]+\s*:\s*[^;]+;/g) ?? [];
    expect(declarations.length).toBeGreaterThan(0);
    for (const d of declarations) {
      expect(d, `raw value in shadcn.css: ${d}`).toMatch(
        /--[\w-]+\s*:\s*var\(--[\w-]+\)\s*;/,
      );
    }
  });

  it("pierre.css is aliases only — the code libraries wear the tokens", () => {
    // Same rule as shadcn.css: the highlighter and the tree are themed by
    // pointing their variables at ours, never by a colour of their own.
    const code = pierreCss.replace(/\/\*[\s\S]*?\*\//g, "");
    expect(code).not.toMatch(/\.dark\b/);
    expect(code).not.toMatch(/\bhsl\(|\boklch\(|#[0-9a-fA-F]{3,8}\b/);
    const declarations = code.match(/--[\w-]+\s*:\s*[^;]+;/g) ?? [];
    expect(declarations.length).toBeGreaterThan(10);
    for (const d of declarations) {
      expect(d, `raw value in pierre.css: ${d}`).toMatch(
        /--[\w-]+\s*:\s*var\(--[\w-]+\)\s*;/,
      );
    }
  });

  it("every shadcn token a ui component uses is bridged", () => {
    // The namespace shadcn components draw from. A class like
    // bg-sidebar-accent only styles anything if @theme knows
    // --color-sidebar-accent — an unbridged token fails silently in the
    // browser, so it must fail loudly here.
    const token =
      /(?:bg|text|border|ring|fill|stroke|outline)-((?:background|foreground|card|popover|primary|secondary|muted|destructive|input|ring|border|sidebar)(?:-[a-z]+)*)/g;
    const used = new Set<string>();
    for (const { text } of uiSources) {
      for (const m of text.matchAll(token)) used.add(m[1].split("/")[0]);
    }
    expect(used.size).toBeGreaterThan(0);
    for (const t of used) {
      expect(
        shadcnCss.includes(`--color-${t}:`),
        `--color-${t} is used by a ui component but not bridged in shadcn.css`,
      ).toBe(true);
    }
  });

  it("no ui component uses shadcn's accent — it means amber here", () => {
    // Weft's --accent is the amber counterpoint; shadcn's is a
    // neutral hover wash. Vendored components are re-skinned to
    // bg-secondary / text-foreground instead (COMPONENTS.md).
    for (const { file, text } of uiSources) {
      expect(
        /(?:bg|text)-accent(?:-foreground)?\b/.test(text),
        `${file} uses an accent utility — re-skin it (accent → secondary)`,
      ).toBe(false);
    }
  });
});
