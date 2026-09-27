# Dashboard components: how we use shadcn/ui

The dashboard's UI vocabulary is built on [shadcn/ui](https://ui.shadcn.com)
primitives, vendored into `src/components/ui/` and re-skinned to the
Stratum design system. [`web/DESIGN.md`](../DESIGN.md) owns the system —
palette, tokens, typography, the do-nots; this file owns the mechanics of
working inside it.

The one-sentence version: **components come from the shadcn registry,
colors come from `web/shared/tokens.css`, and the two meet only in
`src/shadcn.css` — a file of aliases with no color values of its own.**

## What lives where

| Path | Contents | Allowed vocabulary |
|---|---|---|
| `src/components/ui/` | vendored shadcn primitives, re-skinned | bridge names (`bg-background`, `bg-card`, `text-muted-foreground`, `border-border`, `ring-ring`) plus Stratum names where the bridge has no equivalent (`brand-strong`, `text-ink-2`, `text-serious`, `--glow-brand`) |
| `src/components/` | app widgets (StatTile, Bars, SyncBadge, CloneBlock, …) | Stratum names only (`bg-surface-1`, `text-ink`, `border-borderline`) |
| `src/views/` | screens and panels | Stratum names only; compose primitives |
| `src/shells/` | the two chromes: `admin-shell` (sidebar, `max-w-5xl`) and `forge-shell` (global header, `max-w-[1280px]`, no rail, renders signed-out) | Stratum names only |
| `src/lib/` | `cn()`, hooks, and `links.ts` — the link and container classes the design system settled | — |

Vendored primitives currently in use: `alert`, `alert-dialog`, `avatar`,
`badge`, `button`, `card`, `dialog`, `dropdown-menu`, `input`, `label`,
`popover`, `select`, `separator`, `sheet`, `sidebar`, `skeleton`,
`sonner`, `table`, `tabs`, `tooltip`.

A trap worth stating, because it cost a red run: `theme-contract.test.ts`
reads a primitive's **whole file text**, comments included. A comment
that *quotes* a forbidden class name while explaining why it was removed
fails exactly as using the class would. Describe the class, do not spell
it.

Application code never uses the bridge names. If you find yourself typing
`bg-background` in a view, write `bg-surface-0` — same pixel, right
vocabulary.

## The token bridge (`src/shadcn.css`)

Every color is defined **once**, in `web/shared/tokens.css`, as
`light-dark(light, dark)`. The bridge maps shadcn's variable names onto
those tokens and registers them with Tailwind via `@theme inline`, so
stock registry output (`bg-popover`, `border-input`, …) compiles without
edits. Rules, all load-bearing:

- **Aliases only.** Never a raw color, never a `.dark` block. Theming is
  `color-scheme` + `light-dark()`; Radix portals inherit it from `<body>`.
- **`--accent` is intentionally unbridged.** Stratum's `--accent` is the
  amber counterpoint (SVGs render `fill="var(--accent)"`); shadcn's
  "accent" is a neutral hover wash. When vendoring, replace `bg-accent` →
  `bg-secondary` (or `bg-surface-2`) and `text-accent-foreground` →
  `text-foreground`.
- **`--radius-*` is intentionally unbridged.** The house radius (8px)
  makes shadcn's derived scale identical to Tailwind's default
  `rounded-sm/md/lg/xl`, and `tokens.css` `--radius-lg` (14px) must not
  be shadowed.
- **`--border` is reused as-is** — tokens.css already defines it with the
  same meaning. `border-border` and `border-borderline` are synonyms;
  `ui/` uses the former, app code the latter.

## Adding a primitive

```sh
npx shadcn@latest add <name>       # never `init` — see below
```

Then, before committing:

1. **Diff `src/styles.css`.** If the CLI appended CSS variables or a
   theme block there, revert that hunk — the bridge is the only place
   shadcn variables live. (`npx shadcn init` is never run for the same
   reason: it rewrites `styles.css` with its own palette.)
2. **Re-skin to the house style**: `accent` → `secondary` as above; drop
   `shadow`/`shadow-sm`/`shadow-md` (dark never uses drop shadows —
   depth is borders and surface steps; if a surface truly needs it, use
   `shadow-[var(--shadow-1)]`, which resolves to a ring in dark); quiet
   destructive styling (`text-ink-2 hover:text-serious`), never
   white-on-red.
3. **`npm run build`** — `tsc -b` runs with `noUnusedLocals`; generated
   files sometimes keep imports for variants you deleted.
4. **Wire it into at least one real screen in the same commit.** An
   unused primitive is drift waiting to happen.
5. **Commit `package-lock.json`** if a Radix dependency arrived — CI runs
   `npm ci` and fails on a stale lockfile.

Steps 1 and 2 are not left to memory: `src/theme-contract.test.ts` fails
the unit suite if `styles.css` grows a palette or `.dark` block (what
`shadcn add` writes, per https://ui.shadcn.com/docs/theming), if
`src/shadcn.css` contains anything but `var()` aliases, if a ui
component uses a token the bridge doesn't define, or if one slips
through still using `accent`. Both halves have happened for real — the
sidebar `add` appended an hsl palette, and the sidebar once landed with
its `--sidebar-*` aliases missing — which is why it is a test.

The shadcn MCP server is registered in the repo's `.mcp.json`, so a
Claude session can query the registry (`what components exist`, usage
examples) directly.

## Variant conventions

**Button** (consolidates what used to be nine drifted class strings):

| Variant | Use for | Look |
|---|---|---|
| `default` | the one primary action of a form/panel | emerald fill, glow on hover |
| `secondary` | prominent-but-not-primary | card fill, hairline border |
| `outline` | ordinary actions (Export, Grant, Add) | hairline border, ink-2 → ink |
| `ghost` | low-emphasis actions | text only, ink-3 → ink |
| `destructive` | irreversible things | quiet: hairline border, hover to serious — never a red fill |
| `link` | navigation that looks like text | brand, underline on hover |

Sizes: `default` (px-3 py-1.5) is the dashboard's density; `lg` for
full-width auth submits; `xs` for in-table row actions; `icon` **requires
`aria-label`** — the walkthrough fails any button with neither text nor a
label.

Not everything clickable is a `<Button>`: file-tree rows, breadcrumb
names, "Show N revoked"-style text links, and copy affordances stay
hand-styled `<button>` elements on purpose — wrapping them would trade a
deliberate text affordance for button chrome.

**Badge**: `good` / `warning` / `serious` / `neutral`. Status is never
color alone — pair with a glyph or plain words (`SyncBadge` is the
reference).

**Alert**: `role="alert"` by default; pass `role="status"` for calm
notices. Field-level errors stay the tiny inline `Err` paragraph.

**Tabs**: the underline skin is the default; newrepo's pills show how to
re-skin per-trigger via `className` with `data-[state=…]` variants.
Tests address tabs as `getByRole("tab", { name })`.

**Select**: triggers carry the same `aria-label` (or `htmlFor`/`id`) the
old native select had — `getByLabel` is a test contract. Two Radix rules
worth remembering: an item's `value` can never be `""` (clearable filters
map `""` to an explicit "anyone"/"any" item), and a **controlled** empty
value on the Root is what lets a form clear back to its placeholder.

**AlertDialog**: for the four irreversible actions (remove member, delete
team, revoke token, revoke SSH key). Reversible actions (remove from
team, withdraw a grant, revoke an approval) stay one-click. The action
button uses the quiet destructive style.

**Toasts (Sonner)**: success feedback for mutating actions, named
("Revoked laptop"), via `toast.success(...)`. Inline `role="status"` /
`role="alert"` messages the tests assert on are kept — a toast is an
addition, not a replacement. The `Toaster` mounts once in the App shell.

## The code surfaces

Syntax-highlighted files, diffs and file trees come from `@pierre/diffs`
and `@pierre/trees`, and every import of either lives in
`src/code/surface.tsx`. Nothing else may import `@pierre/*`
(`src/code/fence.test.ts` fails the build if it does); pages reach the
surfaces through `src/code/lazy.tsx`, which loads that one module on
demand, and take their types from `src/code/types.ts`. The reason is
weight: the highlighter is larger than the rest of the bundle, and the
entry chunk never carries it — `tests/bundle-budget.setup.ts` measures
that after every build.

Colours: none in the libraries' themes. `src/code/pierre.css` aliases
their variables onto `web/shared/tokens.css` (the `--code-*` block), and
the theme contract test holds it to aliases only, like `shadcn.css`.

Both libraries render inside open shadow roots. Playwright locators see
through them (`getByRole("treeitem")`, `getByText` on a line of code);
`document.querySelectorAll` does not, which is why the walkthrough's
`audit()` descends into shadow roots to find controls. The library's
own expand controls inside a diff are icon-only `div[role=button]`s
nobody here can label — known, accepted for now, raised upstream.

## Test-selector conventions

- Accessible names first: `getByRole`, `getByLabel`, `getByText`. Never
  CSS classes. The one test id is the chart tooltip. The one
  data-attribute family is the diff library's own — `[data-expand-up]`,
  `[data-expand-down]`, `[data-expand-both]`, `[data-column-number]`,
  `[data-selected-line]` — because its controls have no accessible
  names to select by.
- Tables are real `<table>/<tr>/<td>` — positional `td` selectors and
  `getByRole("row"/"cell")` depend on it. Don't replace a table with a
  div grid. (A diff is not a table: the code surfaces are the library's
  CSS grid inside a shadow root, and specs read them by text and by the
  data attributes above.)
- Radix Select interactions: click the trigger, then
  `getByRole("option", { name })`; assert selection as trigger text, not
  `toHaveValue`.
- Dialog confirms: `getByRole("alertdialog").getByRole("button", { name })`.
- Settings sections are sidebar links: `getByRole("link", { name: "Members" })`
  and friends — there is no button or tab named "Settings" anywhere, on
  purpose (a stale selector must fail, not silently match chrome).
- The sidebar renders before the content column in the DOM. That order is
  a contract: it keeps the sidebar's "Search repositories" input as
  `.first()` and the search view's as `.last()` in both suites.
- Absence checks scope to the container (`tbody`), not the page — success
  toasts name the thing that was acted on, and a page-wide text match
  will find the toast (this bit the walkthrough once; the fix is in its
  member-removal step).
- Keep `{" "}` between text and an inline component (`<Badge>`, `<code>`,
  `<span>`) — the walkthrough's jammed-tag audit flags `word<span`.

## The do-not list

- No `.dark` class, no duplicated palette blocks, no hex in a component —
  colors change in `web/shared/tokens.css` only.
- Never rename or remove anything in `web/shared/theme.css` — the Astro
  site consumes it and nothing in CI catches the breakage.
- No Radix Checkbox: the spec counts native checkboxes inside `<form>`.
- No `shadcn init`, ever.
- No new top-level CI jobs (a Rust test pins job names into
  `scripts/ci-local.sh` and `CLAUDE.md`).
- Don't position real controls offscreen (sr-only via negative offsets
  trips the walkthrough's offscreen audit; `display:none` is safe). The
  sidebar collapses to an icon rail for exactly this reason — never
  `collapsible="offcanvas"`, which translates a column of real controls
  past the left edge.
- Sidebar menu tooltips mount only while the rail is collapsed (a
  deliberate deviation from stock — a mounted tooltip's positioned
  internals read as jammed text to the copy audit). The sidebar lists no
  repository names and has no item named "New repository": both names
  are strict-mode contracts held by other screens.
- Hooks live in `src/lib` (`use-mobile`, `use-people`), not `src/hooks`.
- After a visual change, regenerate the marketing screenshots
  (`SCREENSHOT_DIR=../site/public/screenshots npx playwright test
  screenshots`) — the site embeds them with dimension checks.

## Verification recipe

```sh
cd web/dashboard
npm run build                # tsc -b && vite build — the only static gate
npx vitest run
CI=true npx playwright test  # AFTER the build: both browser gates read dist
scripts/ci-local.sh --only web           # from the repo root
scripts/manual-stack.sh up               # then the walkthrough (0 problems)
cd web/dashboard && BASE=http://127.0.0.1:8080 \
  STRATUM_MAIL_DIR=$PWD/../../.stack/mail node tools/walkthrough.mjs
scripts/manual-stack.sh down
```
