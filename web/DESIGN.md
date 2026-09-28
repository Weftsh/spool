# Spool design system

The system the dashboard is built on. Tokens live in
[`shared/tokens.css`](shared/tokens.css); the Tailwind utility mapping the
dashboard imports lives in [`shared/theme.css`](shared/theme.css). Change
colors in tokens.css only — components never hardcode a hex.

The look: **emerald-on-charcoal**, in the family of modern infra tools
without cloning any of them. Dark is the signature theme; the dashboard
follows the person's system preference. Every color token is written once
as `light-dark(light, dark)`, so there is exactly one place a value can
drift.

## Palette

### Dark (signature)

| Role | Token | Value |
|---|---|---|
| Page ground | `--surface-0` | `#0c0e0d` |
| Cards, chart surface | `--surface-1` | `#131615` |
| Wells, code, table heads | `--surface-2` | `#1b1f1d` |
| Hairline | `--border` | `rgb(255 255 255 / 0.09)` |
| Headings, values | `--text-primary` | `#f2f4f3` |
| Body copy | `--text-secondary` | `#a9b1ac` |
| Captions, eyebrows | `--text-muted` | `#7d8580` |
| Brand emerald | `--brand` | `#3ecf8e` |
| Brand hover (lighter in dark) | `--brand-strong` | `#65e2a9` |
| Ink on emerald | `--brand-ink` | `#052e1c` |
| Amber counterpoint | `--accent` | `#f0b429` |

### Light

| Role | Token | Value |
|---|---|---|
| Page ground | `--surface-0` | `#f8faf9` |
| Cards | `--surface-1` | `#ffffff` |
| Wells | `--surface-2` | `#eff2f0` |
| Border (solid, crisp) | `--border` | `#e2e6e4` |
| Headings | `--text-primary` | `#101211` |
| Body | `--text-secondary` | `#4b524e` |
| Muted | `--text-muted` | `#79817c` |
| Brand emerald (AA on white) | `--brand` | `#047857` |
| Brand hover (darker in light) | `--brand-strong` | `#065f46` |
| Ink on emerald | `--brand-ink` | `#ffffff` |
| Amber | `--accent` | `#b45309` |

### Status and series

Status is **never color alone** — always glyph + label (see `SyncBadge`).
Good *is* the brand emerald; warning is the amber; serious is a distinct red.
Charts lead with the emerald family (`--series-1`), assigned in fixed
order, never cycled. Series steps are validator-passed against the card
surface in both modes (dataviz six-checks); the dark steps sit in the
OKLCH L 0.48–0.67 band, which is why dark series-2 is an orange, not the
amber accent — amber cannot hold its chroma inside that band.

| | Light | Dark |
|---|---|---|
| `--status-good` | `#047857` | `#3ecf8e` |
| `--status-warning` | `#b45309` | `#f0b429` |
| `--status-serious` | `#dc2626` | `#f87171` |
| `--series-1` | `#059669` | `#29a86f` |
| `--series-2` | `#b45309` | `#d95926` |
| `--series-3` | `#2563eb` | `#5b93ea` |

### Code

The dashboard's code surfaces (a file, a diff, the tree beside them —
`@pierre/diffs` and `@pierre/trees`, behind `web/dashboard/src/code/`)
take their syntax colours from the `--code-*` block, one token per
token class, each a `light-dark()` pair like everything above. They are
deliberately quieter than the controls around them: lower chroma than
the brand emerald, hues from the familiar editor vocabulary (keywords
purple, strings teal, constants blue, functions orange) darkened for
paper and lifted for charcoal. Comments and punctuation borrow the text
tokens rather than owning a colour. The libraries' own variables are
aliased onto these in `web/dashboard/src/code/pierre.css`, which the
theme contract test holds to aliases only, so a repaint here reaches
every highlighted line and no component carries a colour of its own.

| | Light | Dark |
|---|---|---|
| `--code-comment` | `#6b7280` | `#7c8a82` |
| `--code-keyword` | `#a21caf` | `#d3a4f0` |
| `--code-string` | `#0f766e` | `#8fd9c3` |
| `--code-constant` | `#1d4ed8` | `#8fb8ff` |
| `--code-function` | `#7c2d12` | `#f0b48a` |
| `--code-parameter` | `#92400e` | `#e8c98a` |

`--code-fg`, `--code-punctuation` and `--code-link` alias
`--text-primary`, `--text-secondary` and `--brand`.

## Shape and depth

- Radius: `--radius` 8px (buttons, inputs, code wells), `--radius-lg` 14px;
  Tailwind: buttons `rounded-lg`, cards `rounded-xl`, badges `rounded-full`.
- **Dark uses borders and surface steps for depth, never drop shadows** —
  `--shadow-1` resolves to a 1px low-alpha ring in dark and a soft real
  shadow in light. Hairlines are low-alpha white, never solid gray.
- `--glow-brand` is the primary-CTA hover glow (reads in dark, harmless in
  light): `hover:shadow-[var(--glow-brand)]`.

## Typography

- Sans: **Inter Variable**, self-hosted via `@fontsource-variable/inter`
  (weights used: 400/500/600/700). Mono: **JetBrains Mono**
  (`@fontsource/jetbrains-mono`) — code, stat values, eyebrows, chart
  labels. No runtime font CDNs; the dashboard bundles the woff2 at build.
- Display headings tighten tracking: h1 `tracking-[-0.03em]`, section
  h2 `tracking-tight`. Body 16px / 1.65 in prose; UI copy `text-sm`.
- Numbers always render in mono.

## Recurring patterns

- Card: `rounded-xl border border-borderline bg-surface-1 p-6`. Only a
  card that **is a link** takes the hover `hover:border-brand/40
  transition`; a static panel has no hover state at all, because a card
  that lights up and then does nothing when clicked is a broken promise.
- Primary button: `rounded-lg bg-brand px-5 py-2.5 font-medium
  text-brand-ink hover:bg-brand-strong hover:shadow-[var(--glow-brand)]
  transition`.
- Secondary button: `rounded-lg border border-borderline bg-surface-1
  px-5 py-2.5 font-medium text-ink hover:border-ink-3`.
- Eyebrow: `font-mono text-xs font-semibold uppercase tracking-widest`
  in `text-brand`, `text-accent`, or `text-ink-3`.
- A grid that gains columns at a breakpoint states its base column:
  `grid grid-cols-1 md:grid-cols-2`, never `grid md:grid-cols-2`. An
  implicit column sizes to its widest child's min-content, so one `<pre>`
  in a card made two pages scroll sideways on a phone.
- A border with no colour of its own is the hairline: `tokens.css` sets
  `border-color: var(--border)` in the base layer, because Tailwind v4
  otherwise draws a bare `border` in `currentColor` (the dashboard
  sidebar's edge was a white line in dark until it did).

- Containers: dashboard content column `max-w-5xl px-5` (intentionally
  denser), centered inside the sidebar shell; **forge `max-w-[1280px]
  px-4`** — the repository and profile pages, which carry a file tree and
  an About panel side by side and cannot be read at the dashboard's
  width. Two containers, and they are never mixed within a page. All of them take `min-w-0`: without it a
  wide table or a long branch name widens the page instead of
  ellipsising, and the walkthrough audits `documentElement.scrollWidth`.
  Anything full-bleed — the tab strip's hairline crossing the viewport —
  is rendered *outside* the container by the shell, never pulled out of
  one with a negative margin, because a negative margin is exactly how
  that scrollWidth grows.
- Dashboard shell chrome: a left sidebar — 16rem expanded, 3rem icon
  rail, state persisted per browser — on `--surface-1` against the
  `--surface-0` page ground, separated by the hairline `--border`; depth
  is the surface step, never a drop shadow. Its colors are the
  `--sidebar-*` aliases in the dashboard's `src/shadcn.css`, aliases
  only. The content column keeps its own `max-w-5xl px-5` container with
  `min-w-0` on the flex child so wide tables ellipsise instead of
  widening the page. The top bar is collapse trigger + breadcrumb only;
  navigation lives in the sidebar. Settings are two groups in the rail,
  by whose setting it is — **Organization** (members, runners, teams,
  activity) and **Your account** (tokens, SSH keys, email addresses,
  password) — each entry with an icon no other entry shares,
  because the collapsed rail is icons alone.
- Inline code: `rounded bg-surface-2 px-1.5 py-0.5 font-mono` (+ hairline
  border in prose).

## Links

**Links.** Structural links — repo names, issue titles, usernames, file
names, breadcrumb segments — are `text-ink`/`text-ink-2` and reveal
themselves on `hover:text-brand hover:underline`; position and weight
already say "link". Only **prose** links, inside a rendered README or a
sentence, carry `--brand`. GitHub paints all of them blue, and doing that
with emerald would put brand color on forty elements of a page. Focus is
always a visible `ring-2 ring-brand` on both kinds: hover-only affordance
is not an affordance for a keyboard. The classes are spelled once in
`dashboard/src/lib/links.ts`.

**Do:** mono for every number · let charcoal stay neutral and emerald carry
identity · hover states move border color before anything glows.

**Don't:** multiple gradients · colored card fills · pure `#000` · drop
shadows in dark · emerald body text · more than two accent hues in a view.

## Changing the palette

1. Edit `shared/tokens.css` (both halves of each `light-dark()`).
2. Update the favicon data-URI in `dashboard/index.html` (the one place
   hexes are hardcoded).
3. Re-validate the series palette (dataviz validator) against both
   `--surface-1` values.

## Dashboard components

The dashboard implements this system on vendored
[shadcn/ui](https://ui.shadcn.com) primitives (`dashboard/src/components/ui/`),
re-skinned to the tokens above. How that works day to day — the token
bridge, adding and re-skinning a primitive, variant conventions, and the
test contracts a component change must not break — is documented in
[`dashboard/COMPONENTS.md`](dashboard/COMPONENTS.md). The short form: the
bridge (`dashboard/src/shadcn.css`) is aliases only, every color still
lives once in `shared/tokens.css`, and `shared/` itself is never touched
by dashboard work.
