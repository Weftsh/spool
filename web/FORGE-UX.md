# The forge surfaces

GitHub's information architecture, in Stratum's skin. This file is the
specification for the public, unauthenticated half of the product —
profiles, repo pages, issues, pull requests, discovery, and the intake
queue that has no GitHub equivalent to copy.

[`DESIGN.md`](DESIGN.md) owns the system: palette, tokens, typography,
the do-nots. It wins every conflict. This file owns *layout and
convention* — what goes in which region, in what order, and which of
GitHub's habits we adopt, translate, or refuse. Numbers below marked
"(GitHub)" were measured off github.com at a 1440×1000 viewport in
August 2026; they are the reference, not the target — the Stratum column
next to them is the target.

The one-sentence version: **copy the muscle memory, not the pixels.**
A maintainer who has used GitHub for a decade should find the About
sidebar on the right, the tab strip under the repo name, the state pill
top-left of an issue, and the heatmap on the profile — and should never
mistake the page for GitHub's.

---

## 0. The shell everything sits in

The dashboard has two shells and the split is by address space, not by
feel. `/dashboard/*` keeps the sidebar admin shell and its `max-w-5xl`
column. Everything in this document lives in the **forge shell**: a
global header over a wide centered column.

| | GitHub | Stratum |
|---|---|---|
| Header height | 64px, full-bleed | 56px, full-bleed on `--surface-1`, bottom hairline `--border` |
| Content column | ~1200px inside a 1440 viewport | `max-w-[1280px] px-4` |
| Body size | 14px (GitHub) | 14px — `text-sm`, matching the dashboard's density |
| Main / sidebar split | 904px + 272–296px (GitHub, repo overview) | `grid-cols-[minmax(0,1fr)_296px] gap-6`, collapsing to one column below `lg` |
| Depth | borders + surface steps | same; **no drop shadows in dark**, per DESIGN.md |

Header contents, left to right: the Stratum mark → global search →
spacer → `+ New` → user menu, or a **Sign in** button when anonymous.
The search input's accessible name is **"Search Stratum"**; the user
menu's settings item is **"Your settings"**; a repo's settings tab is
**"Repo settings"**. Nothing anywhere may carry the bare accessible name
"Settings" — the existing Playwright specs address controls by role and
name, and a bare "Settings" collides with the admin shell's.

`min-w-0` on the main grid child is mandatory. Without it a wide table
or a long branch name widens the page instead of ellipsising, and
`audit()` fails the build on `documentElement.scrollWidth`.

### The tab strip

One component, used by the repo page, the profile, and the org page.

- 48px tall (GitHub is 48px; keep it — it is the right target size).
- Sits directly under the identity block, full-bleed, with a bottom
  hairline `border-b border-borderline` running the full width while the
  tabs themselves stay inside the content column.
- Active tab: `text-ink font-medium` plus a **2px bottom rule in
  `--brand`**, inset to the tab's own width. GitHub uses an orange rule
  for this; ours is emerald, which is the one place the brand appears as
  a pure decoration and it earns it — it is the page's "you are here".
- Inactive: `text-ink-2 hover:text-ink`, hover moves *border and text*,
  never a fill.
- Counts render as a `Badge variant="neutral"` pill in **mono**, per
  DESIGN.md's "numbers always render in mono". A tab with a zero count
  renders no pill rather than a `0`.
- **Overflow lives on an inner container.** `audit()` measures
  `documentElement.scrollWidth`, so an outer scroller is a build
  failure and an inner one is invisible to it. The strip is
  `<div class="overflow-x-auto"><nav class="flex gap-1 whitespace-nowrap">`.
  Do not try to make the tabs wrap.

---

## 1. Repo identity — `/{owner}/{repo}`

The most-visited page in a forge, and the one a migrating project judges
us on in four seconds.

### Region order

1. **Identity row.** Owner avatar (20px, `rounded-full`) · `owner / repo`
   with the repo name in `font-semibold text-ink` and the owner in
   `text-ink-2` · a visibility pill (`Public` / `Private`) as
   `Badge variant="neutral"` · for a fork, a second line:
   `Forked from owner/repo`, in `text-ink-3 text-xs`, linked.
2. **Action group**, right-aligned on the same row: **Watch · Fork ·
   Star**, in that order (GitHub's order; keep it). Each is a split
   control: a labelled button plus a count, plus a caret for the menu
   variant. `h-8 rounded-lg border border-borderline bg-surface-1`,
   count in mono inside a `bg-surface-2` right cap. Counts are **always
   visible and always mono**, including zero.
3. **Tab strip**: Code · Issues · Changes · Checks · Insights ·
   Repo settings. Six tabs, and that is the whole list — see §5 and §7.
   Insights is offered to members and Repo settings to admins — two
   different questions, both the server's own answer (§1's Insights
   note) — and Changes is
   dropped on a **mirror**, whose trunk belongs to its origin and whose
   review routes the API refuses. Everything else is the same strip for
   a stranger and a maintainer alike.
   **Checks** is the one addition to the original five: it exists
   because there is now something behind it, which is the only rule
   this strip has ever had.
4. **Body**, two columns: the code browser (or the tab's body) left, the
   About panel right.

### The About panel — the OSS calling card

GitHub's About sidebar is the single densest piece of community
signalling on the platform and we copy its *content and order* almost
exactly, because that order is what people scan.

```
About                                        (h2, text-sm font-semibold)
<description>                                text-ink-2, 3 lines max, no clamp gradient
<homepage link>                              link icon + brand link
<topic pills>                                wrap, max 2 rows then "+N"
─────
Readme · License · Code of conduct · Contributing · Security policy · Activity
─────
N stars · N watching · N forks                mono numbers, glyph + label each
Report repository
─────
Tags (N)          latest tag + "Latest" badge + date
Used by (N)       stacked avatars + "+N"
Contributors (N)  avatar grid, 6 per row, "+N contributors"
Languages         a single stacked bar + a legend
```

Three deliberate differences from GitHub:

- **The health-file rows are the community contract, so they render as
  present-or-absent, not present-or-missing.** GitHub silently omits a
  row when the file does not exist, which makes an incomplete project
  look identical to a complete one. We render every one of the six rows
  always: present rows are links in `--brand` with a filled glyph;
  absent rows are `text-ink-3` with an outline glyph and the plain word
  "None". A maintainer sees the gap; a contributor sees the honesty.
- **The Languages bar uses the series family in fixed order**
  (`--series-1`, `--series-2`, `--series-3`, then `--text-muted` for
  "Other"), never a per-language hue table. DESIGN.md caps a view at two
  accent hues; a 12-colour language bar is exactly the thing that rule
  exists to stop. Percentages in mono, and each legend entry is
  `dot + name + percent` so the bar is never read by colour alone.
- **`Used by` becomes `Forks of this` for us until a dependency graph
  exists.** Shipping an empty "Used by" is worse than not shipping it.
- **"Releases" is headed `Tags (N)` and renders git tags**, for the same
  reason. We have no releases feature — no notes, no attached assets —
  and a block headed "Releases" would promise both. What we can say
  honestly is how many tags there are and which is newest, and even
  "newest" is qualified: a repository whose tags are not versions
  (`nightly`, `2024-06-01`, `ship-it`) gets **no** latest tag rather
  than an arbitrary one. The count is still true; the ordering is
  simply not something we know.

Numbers reference (GitHub): About column 272px, topic pill 26px tall /
`2px 12px` / 12px-600 / full radius, file row 41px, contributor avatars
28px in a 6-wide grid.

Stratum topic pill: `rounded-full bg-surface-2 border border-borderline
px-3 py-0.5 text-xs font-medium text-ink-2 hover:border-brand/40
hover:text-ink`. Not brand-coloured — a row of twelve emerald pills
turns the page into a brand swatch and buries the description above it.

### Insights (`/graphs` in GitHub's URL space, a tab for us)

GitHub buries contributors, forks and community standards behind a
left-rail sub-navigation 280px wide (GitHub) under an "Insights" tab.
Keep the shape; cut the rail to four entries — **Contributors · Commit
activity · Forks · Community standards** — and drop Pulse, Code
frequency, Dependency graph, Network graph and the two Actions metrics
pages. Rail rows are 36px, active row gets a 2px left rule in `--brand`
and `bg-surface-2`.

The **Community standards** checklist (GitHub renders it as a progress
bar plus a checked list) is the highest-value page on the whole Insights
tab for our audience and it moves *up*: it is the rail's first entry, and
the repo's About panel links to it from the health-file block. The
progress bar is a single `--series-1` fill on a `--surface-2` track —
GitHub uses an amber→green gradient, which is two accent hues and a
gradient in one 8px element. Each row is `glyph + name + state word`
("Present" / "Missing"), never a bare coloured dot.

The existing `views/repo.tsx` stat tiles, `KindRow` table and CSV export
land on this tab unchanged.

**Status, and one decision that changed.** The traffic half is built:
`views/forge/insights.tsx` carries the stat tiles, the `KindRow` table,
the origin-unreachable panel, `stored_bytes` and the CSV export, which
is now their only home — `views/repo.tsx` is gone, along with the
dashboard repository address space it lived in. The four-entry rail is
**not** built: Contributors, Commit activity, Forks and Community
standards have no page yet. `crates/stratum-control/src/insights.rs`
already computes a repository's pulse and a namespace's — commits,
authors, top committers, changes opened and landed, issues, median
time-to-land — and nothing in the workspace calls it: there is no route
and no UI. That is the shape of the next slice here, and it would make
"drop Pulse" above worth revisiting, because Pulse is the part that is
already written.

The tab is **members-only**, gated on `viewer_member` — whether the
caller holds a role here at all, an org membership or a per-repo grant,
as opposed to reading a public repository as a stranger. So §1's "six
tabs" is six for an admin, five for any other member, and four for
everybody else.

Members and not admins, deliberately: somebody with the `viewer` role is
on the inside and may read the numbers while changing nothing. That is a
weaker bar than Repo settings', and the two gates exist to tell a
stranger, a member and an admin apart.

This is a departure from GitHub, where the Insights tab is public and
only Traffic is maintainer-only — but our numbers *are* traffic.
**Publishing the code does not publish how the code is used.** How often
a repository is cloned, how many bytes it serves and how far behind its
origin a mirror runs are the owner's, on a public repository as much as
a private one.

The gate is real and not a curtain: `GET …/repos/{repo}/metrics` makes
the same `authx::require` call the row does. It used to take
`Scope::RepoRead` through `rest_repo_auth`, whose public-read fallback
answered an anonymous caller **in full** on a public repository — and
nothing in the UI linked to it, which is exactly why that went
unnoticed. An endpoint nothing points at is still an endpoint.

---

## 2. Community and contribution

This is the half of the product the brief says to weight, and the half
GitHub treats as an afterthought. Four surfaces.

### `/{owner}/{repo}/contribute`

GitHub's version is a nearly empty page: a title, one line of copy, a
"Read the contributing guidelines" card top-right, and a list of
`good first issue` issues — which on both repos sampled was the empty
state *"This repo doesn't have any good first issues, yet"*. The empty
state is the common case, which tells you the page is under-built.

Ours does more, because it is the first page a drive-by contributor
lands on:

1. **Header**: `Contribute to owner/repo`, plus one sentence of the
   project's own words pulled from `CONTRIBUTING.md`'s first paragraph
   (not our boilerplate).
2. **The rules of the house**, as a three-item row of `--surface-2`
   wells, each a fact the contributor needs *before* writing code:
   - **Agents**: `welcome` / `labeled` / `human-only`, rendered as glyph
     + the policy word + one clause of explanation. This is the single
     most useful thing we can tell a contributor that GitHub cannot.
   - **Review gate**: "N approvals from OWNERS of the paths you touch",
     read from the repo's OWNERS config.
   - **Intake**: "First-time contributions run the automated checks
     before a human is asked", linked to §7.
3. **Where to start**: the `good first issue` list, each row identical
   to an issues-index row (§3) so nothing new is learned.
4. **The guidelines card**, right column: CONTRIBUTING · Code of conduct
   · Security policy, each with its present/absent treatment from §1.

The empty state is not "yet" — it says what to do instead: *"No issues
are labelled `good first issue`. Open an issue describing what you want
to change before writing code, or read the contributing guidelines."*

### The first-time banner

GitHub puts a dismissible banner at the top of `/pulls`:
*"First time contributing to owner/repo?"* with links to issues and the
contributing guidelines. Keep it, on both `/issues` and `/pulls`, but
make it *conditional on the viewer's trust rung* (§6) rather than on a
dismissal cookie: `first_timer` and `unknown` see it, `trusted` and
`collaborator` never do. A banner that a maintainer has to dismiss on
every repo is chrome; one that only strangers see is a service.

Render as `Alert role="status"` — calm, not an alarm — on `--surface-1`
with a hairline, brand-coloured links, and a text **Dismiss** button
(not an icon-only ×; an unlabelled `<button>` fails `audit()`).

### Label-filtered issue lists

The `good first issue` list is just the issues index (§3) with the query
bar pre-filled `is:issue is:open label:"good first issue"`. **The query
bar is the API.** Every filter chip in the UI writes into that text box
and every URL is shareable. This is GitHub's best idea and costs us
nothing to copy.

### Health-file rendering

GitHub renders README, Code of conduct, Contributing, License and
Security as a **tab strip on the overview panel** (`?tab=coc-ov-file`),
not as separate pages — so the reader never leaves the repo. Copy that
exactly: one `Tabs` component above the rendered markdown, underline
skin, `README · Code of conduct · Contributing · License · Security`,
with absent files omitted from the strip (here omission is right — the
About panel already reports the gap).

---

## 3. Issues and pull requests

### The index

Regions, left to right, top to bottom:

- **Left rail**, 256px (GitHub): Issues · Assigned to me · Created by me
  · Mentioned, a rule, then Milestones · Labels. The personal entries
  render but are disabled with a "Sign in" affordance when anonymous —
  they are the muscle memory, and hiding them makes the page look
  broken.
- **Banner** (§2), conditional.
- **Query bar**: a full-width mono input showing `is:issue is:open`,
  `bg-surface-2` with a hairline, a clear (×) and a submit (magnifier).
  Both icon buttons need `aria-label`.
- **Filter row**: `Open N` / `Closed N` on the left with a state glyph
  each, then Author · Labels · Milestones · Assignees · Sort dropdowns
  right-aligned. **This row is the second `audit()` hazard**: seven
  controls plus two counts do not fit at 1024px. Put
  `overflow-x-auto` on the *inner* flex container, and make the whole
  row `flex-wrap` at `md` and below so it stacks instead of scrolling on
  a phone.
- **Rows**: state glyph · title (`text-ink font-medium`, not a link
  colour — see §4) · inline label pills · a second line
  `#N · opened <relative> by <author>` in `text-ink-3`, with the
  author-association badge (§6) after the name · right-aligned comment
  count with a speech-bubble glyph, and for a PR the CI glyph and review
  state word.

Every relative time carries a `title` with the absolute timestamp in the
viewer's zone. The dashboard has already been bitten once by a date
filter that read a picked day as UTC midnight; do not reintroduce the
class.

### The detail page

- **Title row**: `<title>` at `text-2xl` followed by `#N` in
  `font-mono text-ink-3`. The number is a number: mono.
- **State pill** on its own line under the title: 32px tall (GitHub),
  `rounded-full px-3 py-1.5 text-sm font-semibold`, **glyph + word**
  always. See §4 for the icon table.
- **For a PR**, a one-line provenance sentence under the pill:
  `author wants to merge N commits into owner:main from author:branch`,
  with both refs as mono `--surface-2` chips. This sentence is doing a
  lot of work on a fork-based contribution and we keep it verbatim.
- **Sub-tabs** for a PR: Conversation · Commits · Checks · Files
  changed, each with a mono count. Plus the diffstat (`+N −M` in mono,
  green/red, followed by five squares) right-aligned — and the squares
  are decoration, so `aria-hidden`, with the accessible text being the
  `+N −M`.
- **Timeline**, left column: comment cards on `--surface-1` with a
  hairline; between them, thin *event rows* (`label added`,
  `assigned`, `commit pushed`, `review requested`) at `text-xs
  text-ink-3` with a 24px glyph in a gutter, on the page ground, no
  card. The visual difference between "someone said something" and
  "something happened" is the whole reason a timeline is readable.
- **Sticky sub-header**: on scroll, GitHub collapses the title row to a
  56px bar carrying the state pill + truncated title + `#N`. Copy it. It
  is how you keep your bearings in a 200-comment thread.
- **Right sidebar**, 296px (GitHub): Assignees · Labels · Type ·
  Milestone · Relationships · Development (linked PRs/changes) ·
  Participants. Each section is `h3 text-xs font-semibold uppercase
  tracking-widest text-ink-3` — the DESIGN.md eyebrow — over its
  content, separated by hairlines. Cut Projects and Notifications.

**Keep `{" "}` between any text and any adjacent `LabelPill`, `Badge` or
`<a>`.** `audit()` regexes `[A-Za-z0-9]{2,}<(a|span|code|strong|em)` over
`body.innerHTML` and fails the build on a match. The issue rows,
timeline event rows and the PR provenance sentence are all dense
text-plus-inline-tag constructions; this is where that check will fire.

---

## 4. The translation table

Every GitHub convention, and what it becomes. Token names are from
[`shared/tokens.css`](shared/tokens.css); utility names from
[`shared/theme.css`](shared/theme.css).

| GitHub | Value (measured) | Stratum |
|---|---|---|
| Link blue | `#4493f8` | **Not a colour.** See below. |
| Primary green button | `#238636`, white text, 32px, r6 | `bg-brand text-brand-ink rounded-lg px-3 py-1.5 hover:bg-brand-strong hover:shadow-[var(--glow-brand)]` — DESIGN.md's primary button |
| Secondary button | `#212830` fill, `#3d444d` border | `Button variant="outline"` — hairline, `text-ink-2 hover:text-ink` |
| Grey wells / code / table heads | `#151b23` | `bg-surface-2` |
| Page ground | `#0d1117` | `--surface-0` |
| Card | `#0d1117` + border | `rounded-xl border border-borderline bg-surface-1` |
| Hairline | `#3d444d` (solid) | `--border` — low-alpha white in dark, solid in light |
| Label pill | 20px, `0 8px`, r-full, 12/500, hue text on 18% hue fill + 30% hue border | See "Labels", below |
| Topic pill | 26px, `2px 12px`, r-full, 12/600, blue on 10% blue | `rounded-full bg-surface-2 border border-borderline px-3 py-0.5 text-xs text-ink-2` |
| Author-association badge | 20px outline pill, grey | `Badge variant="neutral"` in mono-uppercase at `text-[10px]`; see §6 |
| Active tab rule | 2px `#fd8c73` (orange) | 2px `--brand` |
| Heatmap ramp | 5 hardcoded greens | Derived from `--series-1`; see below |
| Diff added / removed | green / red fills | `--status-good` / `--status-serious` at 12% over `--surface-1`, with the `+`/`−` gutter glyph carrying the meaning |
| CI passing / failing / pending | ✓ green / ✗ red / ● amber | `--status-good` / `--status-serious` / `--status-warning`, **each with its glyph and an accessible label** |

### Links are ink, not brand

GitHub paints every repo name, username, issue title and inline
reference the same blue. Adopting that with `--brand` would put emerald
on forty elements per page and destroy the thing DESIGN.md is protecting
— *let charcoal stay neutral and emerald carry identity*.

The rule:

- **Structural links** — repo names, issue and PR titles, usernames,
  file names, breadcrumb segments — render `text-ink` (or `text-ink-2`
  for secondary ones) and reveal themselves on hover:
  `hover:text-brand hover:underline underline-offset-2`. They are already
  identifiable as links by position and weight.
- **Prose links** — inside a rendered README, a description, a banner
  sentence, an inline `#123` reference — render `text-brand
  hover:underline`, which is DESIGN.md's `link` button variant.
- **Focus** is always a visible `ring-2 ring-brand ring-offset-2
  ring-offset-surface-0`, on both kinds. Hover-only affordance is not an
  affordance for a keyboard.

### Labels

GitHub lets a project pick any hex per label and derives the pill's
text, fill and border from it. That is genuinely useful data — projects
encode meaning in their palettes — and it is also a direct collision with
DESIGN.md's "no more than two accent hues in a view": a rust-lang issue
row carries five differently-coloured pills.

**Ship monochrome pills in v1**, with the hue preserved as data:

```
rounded-full bg-surface-2 border border-borderline
px-2 py-0.5 text-xs font-medium text-ink-2
```

plus an optional **6px hue dot** at the leading edge, rendered from the
imported hex through `color-mix(in oklab, <hue> 70%, var(--text-muted))`
so it can never be a saturated flare against charcoal. The label *name*
carries the meaning; the dot carries the project's identity; the page
keeps its palette. This is a judgement call and it is flagged in §8.

### State icons — glyph plus label, never colour alone

DESIGN.md is absolute here, and issue/PR state is the place it matters
most because red/green is exactly the axis 8% of men cannot resolve.

| State | Glyph | Colour token | Accessible label |
|---|---|---|---|
| Issue open | open-circle with centre dot | `--status-good` | "Open" |
| Issue closed as completed | check in circle | `--brand` (muted to `--text-muted` in a list row) | "Closed as completed" |
| Issue closed as not planned | slash in circle | `--text-muted` | "Closed as not planned" |
| PR / change open | branch-with-arrow | `--status-good` | "Open" |
| PR draft | branch, hollow | `--text-muted` | "Draft" |
| PR merged / landed | branch-into-trunk | `--series-3` | "Merged" |
| PR closed unmerged | branch with × | `--status-serious` | "Closed without merging" |
| Check passing | ✓ | `--status-good` | "All checks passed" |
| Check failing | ✗ | `--status-serious` | "Checks failed" |
| Check pending | ● | `--status-warning` | "Checks running" |

Merged uses `--series-3` (blue) rather than a purple, because purple is
not in our palette and inventing one for a single state is exactly the
hex-in-a-component that DESIGN.md forbids. In a list row every one of
these is followed by visible text or, where space forbids, an
`aria-label` plus a `title` — an icon alone is never the only carrier.

### The heatmap ramp

Five steps, from `--series-1`, working in both themes, with **no new
hexes**. Declare once in the dashboard's stylesheet:

```css
--heat-0: var(--surface-2);
--heat-1: color-mix(in oklab, var(--series-1) 25%, var(--surface-1));
--heat-2: color-mix(in oklab, var(--series-1) 50%, var(--surface-1));
--heat-3: color-mix(in oklab, var(--series-1) 75%, var(--surface-1));
--heat-4: var(--brand);
```

Why this works in both directions without a second definition: in dark,
`--surface-1` is `#131615` and `--brand` (`#3ecf8e`) is *lighter* than
`--series-1` (`#29a86f`), so the ramp climbs toward light. In light,
`--surface-1` is `#ffffff` and `--brand` (`#047857`) is *darker* than
`--series-1` (`#059669`), so the same five declarations climb toward
dark. Contrast against the card increases monotonically in both modes,
which is the only property a sequential ramp has to have. Nothing is
hardcoded and nothing is duplicated.

`--heat-0` is `--surface-2`, not transparent: an empty day must be a
visible cell, or the grid's shape collapses on light backgrounds.

---

## 5. Profiles and the contribution graph

### `/{handle}`

Two columns: a **296px left rail** (GitHub) carrying identity, and the
content column carrying pinned items, the graph and activity.

Left rail order: avatar (GitHub renders it at 296px round, which is
absurd at 1440 — ours is **160px**, `rounded-full`, hairline ring) ·
display name (`text-xl font-semibold`) · `handle · pronouns` in
`text-ink-2` · the primary action (**Follow**, or **Edit profile** when
it is you) as a full-width button · `N followers · N following` with
mono numbers · bio · then a metadata list — company, location, email,
website, social links — each `glyph + value`, glyphs `aria-hidden` with
the value carrying the text.

Tab strip: **Overview · Repositories · Stars · Organizations**. Four.
Packages, Projects and Sponsoring are cut (§7).

Content column, in order:

1. **Pinned** — up to 6 cards in a 2-column grid, ~430px each. Card
   anatomy: repo glyph · `owner/name` (owner shown only when it is not
   this profile) · visibility pill · description clamped to 2 lines ·
   a footer row of `language dot + name`, `star glyph + count`,
   `fork glyph + count`, all counts mono. A pinned *landed change*
   uses the same card with a change glyph and `#N + title`.
2. **The contribution graph.**
3. **Activity overview** — "Contributed to a, b, c and N other
   repositories". Keep the sentence; cut GitHub's radar chart, which is
   four numbers drawn as a shape nobody can read.
4. **Contribution activity**, a reverse-chronological list grouped by
   month.

### The graph, in detail

Measured off GitHub: cell **10×10px**, `border-radius 2px`,
`border-spacing 3px` (so a 13px pitch), 7 rows × 53 columns, table
**723px** wide, month labels above, `Mon`/`Wed`/`Fri` labels in a left
gutter, a `Less ▪▪▪▪▪ More` legend bottom-right, and a **vertical year
list** in a right-hand rail with the current year as a filled pill.

Take all of it, with four changes:

- **Cells are `<td>` in a real `<table>`** with `border-spacing: 3px`,
  not a flex grid. It is tabular data, and border-spacing gives the gap
  without 371 margin calculations.
- Each cell gets `aria-label="N contributions on <date>"` and, where the
  viewer may see them, a tooltip naming the repos. **A cell is never a
  `<button>`** — see the `audit()` note below.
- **The private-contribution rule is rendered, not hidden.** The header
  line is `N contributions in the last year`, and when the viewer's
  response carried `private_included: true` it gains a second clause:
  `· includes private contributions`. When the profile owner has opted
  out it says `· public repositories only`. A quiet week and an opted-out
  week look different, which was the whole point of that flag.
- **Agent-authored days are hatched, not coloured.** An agent principal's
  commits never inflate a human's ramp (they are a separate series), and
  on an agent's own profile the same five steps render with a 45° 1px
  hatch overlay so the graph reads as machine output at a glance without
  a sixth colour.

The 53-column strip is the **third `audit()` hazard**. Mitigations, all
three required: `min-w-0` on the flex parent; `overflow-x-auto` on the
table's own wrapper (inner, so `documentElement.scrollWidth` never
grows); and cells rendered as `<td>` with `role="gridcell"` rather than
`<button>`, because 371 buttons with no text content is 371
`emptyButtons` failures if a single `aria-label` is ever missed, and the
cells are not actions.

Below `lg` the year rail moves under the graph as a horizontal
`overflow-x-auto` chip row.

---

## 6. What a maintainer-attention forge shows that GitHub does not

GitHub's surfaces are tuned for contribution *volume*: everything
nudges toward opening a PR. The scarce resource is maintainer attention.
Five things follow, and they are the reason to build this rather than
skin Forgejo.

### The author-association badge, promoted

GitHub already renders `Member` / `Contributor` / `First-time
contributor` on comments and PR rows — quietly, in a grey 20px outline
pill at the right edge of the comment header. It is the single most
useful signal on the page and it is styled like a footnote.

Ours renders in the same place with the same restraint (`Badge
variant="neutral"`, `font-mono text-[10px] uppercase tracking-widest`)
but the vocabulary is the **trust ladder**, not GitHub's four-value
enum, and it appears on issue rows, PR rows, comments *and* the intake
queue:

| Rung | Badge | Meaning |
|---|---|---|
| `collaborator` | `OWNER` / `ADMIN` / `MEMBER` | `members::effective_role`, or an OWNERS entry resolves to them |
| `trusted` | `TRUSTED` | N landed changes, no recent ejections |
| `first_timer` | `FIRST-TIME` | no landed change here yet |
| `unknown` | *(no badge)* | not known to this repo |
| `blocked` | `BLOCKED` | manual-only, refused at both doors |

`blocked` is the only rung that uses colour (`--status-serious` text on
a hairline pill), and it still carries the word.

### Agent attribution, everywhere a name appears

An agent principal never borrows a human name. Wherever an actor is
rendered — comment header, timeline row, commit author, intake queue row
— an agent gets a distinct square-cornered avatar (`rounded` not
`rounded-full`, which is GitHub's own convention for a bot and costs
nothing to adopt), the handle, and an `AGENT` badge. On a repo with
`agents: human-only`, the contribute page and the PR form say so *before*
the work happens, in the refusal's own words.

### Honest provenance on a mirrored repo

A migrated project with 60,300 GitHub stars must not render `☆ 0`. The
imported count lives in its own column and renders as a separate,
labelled row in the About panel:

```
☆ 0 stars                       ← ours, mono, honest
↗ 60.3k on GitHub               ← imported, text-ink-3, links to the origin
```

Two rows, never one summed number. The label is `on GitHub`, not
"total". This is a trust surface: a forge that inflates a counter on
day one has told you what it will do later.

### Intake state on the row, not in a tab

A PR row's third column shows the review state. GitHub's vocabulary is
`Review required` / `Changes requested` / `Approved`. Ours prefixes the
intake verdict when it is not yet `review`:

`In intake · 3 of 4 gates passed` — mono numbers, a `--status-warning`
glyph, linked to the queue. A maintainer scanning `/pulls` can see
which rows have not yet earned their attention *without opening them*,
which is the entire product thesis in one table cell.

### The intake queue — `/{owner}/{repo}/intake`

GitHub has no equivalent, so there is nothing to copy and this is
specified from scratch. It is a maintainer-only tab (hidden entirely
from anonymous viewers, and from `Viewer`-level members).

**Anatomy:**

1. **Header**: `Intake queue` + a mono count, and one sentence of
   policy: *"Contributions from first-time and unknown authors run these
   gates before a review is requested."* Right-aligned: a link to
   **Firewall settings**.
2. **The gate legend**, a single row of four `--surface-2` wells naming
   the gates in the order they run — **Applies · Checks · Similarity ·
   Style** — each with its glyph, a one-clause description, and the
   current pass rate in mono. This row is the page's explanation of
   itself, and it means a maintainer never has to read a doc to know
   what a red square means.
3. **The queue table.** One row per change in `intake_state != review`,
   ranked — not chronological. Columns:

   | Column | Content | Width |
   |---|---|---|
   | Author | avatar · handle · trust badge · `AGENT` if applicable | 200px |
   | Change | `#N` mono + title, truncating with `text-ellipsis` | `minmax(0,1fr)` |
   | Gates | four fixed-position glyphs, each `aria-label`ed `"<gate>: passed/failed/pending"` | 120px |
   | Signal | the ranking number in mono, with the reason on hover | 90px |
   | Age | relative, `title` = absolute | 80px |
   | | a kebab menu, `aria-label="Change actions"` | 40px |

   The gate glyphs are **fixed-position**: gate 3 is always the third
   square, whether it ran or not, so the column reads as a pattern down
   the page. A pending gate is an outline square in `--text-muted`, not
   a gap.

4. **Row expansion**, in place, no navigation: the failing gate's actual
   output — the similarity gate's nearest neighbours as linked change
   rows with a mono percentage, the style gate's violated rule from
   `.weft/style.toml` quoted verbatim, the applies gate's refusal
   sentence from `ancestry::is_fast_forward_capped`. **Quote the
   machine, do not paraphrase it**: the refusal sentences are the
   product's house style and the maintainer will be pasting them to the
   contributor.
5. **Bulk actions**, above the table, disabled until rows are selected:
   *Promote to review* · *Request changes* · *Reject*. Each opens a
   confirm with the count in mono. The select-all checkbox needs a real
   `aria-label` ("Select all changes in intake").
6. **The empty state is the good state** and must read like one:
   *"Nothing is waiting. Every open contribution has passed intake and
   is with a reviewer."* — with a link to `/pulls`. Not an illustration,
   not "Get started".

The table is wide. `min-w-0` on the parent, `overflow-x-auto` on the
table's own wrapper, `truncate` on the Change cell. The dashboard has
already shipped a fix for "table cells that widened the layout instead
of ellipsising"; this is the same class.

---

## 6a. Changesets — the surface GitHub has no equivalent for

This section exists because its absence did damage. FORGE-UX, FORGE-PARITY
and FORGE-GAP were all silent about changesets, and a surface with no
specification drifts: the whole feature ended up dashboard-only, with no
public address and no notifications, while the *git* front door for a
changeset had been serving `/{org}/changesets/{key}` over HTTP and SSH the
whole time. Nothing caught that, because nothing had ever said what the
surface should be.

A **changeset** is one review, one verdict and one landing over up to 16
changes in one organization — one change per member repository. GitHub has
no equivalent, so there is no page to translate and §4's table cannot help
here. The rules below are the ones the existing screens already keep, plus
the ones they should.

### What the page is for

A reader arrives asking three questions in this order: *can it land*, *what
is in it*, and *what does it say*. The layout answers them in that order —
Verdict, then Members in landing order, then the code, then the machine's
opinion, then what happened. Anything that reorders those is wrong even if
every element is present.

### The rules

- **One verdict, in the engine's own words, naming the member in the way.**
  Never a fraction and never a percentage: sufficiency is per path, so
  "3 of 4" is a number that cannot be acted on. The composed explanation is
  prefixed `repo/change:` so a reader of one review over four repositories
  is told which repository to go to.
- **Members render in landing order**, and the order shown is the order the
  lander will walk. Where the two could disagree, the table is the truth —
  a strip that is right while the lander walks something else is worse than
  no strip.
- **Approval happens where the code is reviewed**, on the member change's
  own page under that repository's OWNERS, never here. The member's page
  says "Lands with changeset `<key>`" and turns its own Land off while held.
  That is why there are **no cross-repo line comments**: a line belongs to a
  repository, and that repository's OWNERS decides who is required on it. A
  second, ungoverned channel over the same lines would let a review
  conversation route around the computed reviewer set, which is the one
  thing this product is built to prevent.
- **Group the files by member, always.** Never a flat path-sorted list
  across repositories: two repos' `README.md` adjacent under a small prefix
  is how somebody approves the wrong file. The open file's header names its
  repository first.
- **An identifier a person reads to tell rows apart wraps; one nobody reads
  past the first few characters ellipsises.** The repository name and the
  change title wrap. The 40-hex Change-Id keeps its ellipsis — un-truncating
  it puts three lines of hex in every row of a revert's members table. The
  rule is not "identifiers never truncate", and getting that backwards
  produced a members table where every row read `payments-…`.
- **Draw the landing order as waves, not a graph.** One column per
  topological rank; members at the same rank stack, because they land
  together and the order among them does not matter — stacking says so
  without drawing an edge. Where a chip's predecessors are not simply the
  previous wave, it carries an `after: repo/change` line. **No SVG
  splines**: at sixteen members crossing unlabelled curves carry less than
  that sentence, they have no accessible name for a test or the walkthrough
  to assert on, and dark mode has no line vocabulary beyond the hairline.
  **No drag-and-drop**: it implies a total order the model does not have,
  it needs a keyboard path, and the manual pass cannot drive it.
- **The empty state is where the reader learns the feature exists.** With no
  edges declared the order section still renders, one wave, plus the
  sentence saying these land in the order they were added. Hiding the
  section when there is nothing to show hides the fact that it can be
  edited.
- **A refusal is the gate speaking, not an error.** A 409 renders as the
  sentence the server sent, in the same voice as the verdict — and the
  client's own mirrored checks must never *disagree* with the server. A
  form that refuses what the server would accept is a bug in the form.
- **A changeset is one review, so it is one notification.** Not one per
  member. And nobody is told about a set containing a repository they
  cannot read: the mail names its members, so mailing outside that
  boundary publishes a private repository's existence.
- **The page must read identically signed in and signed out**, per §0 —
  signing in adds actions, never changes what the page says. That includes
  the clone block: the workspace URL of a public set is a public fact.

### Still missing, deliberately named

A **combined cross-repo diff** — reading a four-repo set still means four
page loads, and the changeset page shows no code at all. A **changeset
conversation** for discussion of the set as a set, which today fragments
across four per-change threads where no participant sees the whole. A
**public forge address**, so a review can be linked, and read by somebody
without an account, the way a single change already can be.

There is deliberately **no public org-wide changeset list**: the row filter
is per-caller, so a public list would silently hide half its rows, and a
list that lies about its own completeness is worse than none.

---

## 7. What we deliberately do not copy

The plan cuts Actions, Packages, Projects, Security, Discussions,
Achievements and Sponsoring-as-a-product. Those cuts hold, and here is
the reasoning, plus what a look at the live surfaces adds.

| Cut | Why |
|---|---|
| **Packages / Projects / Security / Discussions tabs** | Nothing behind them. A tab that leads to an empty state is worse than an absent tab: it converts "we don't do that" into "this is broken". |
| **An Actions tab** — but see the row below | Still cut, and for the original reason: we do not execute anybody's workflows. The tab that *does* exist is called **Checks**. |
| **A CI runner** | The largest cut in the plan and the one that keeps costing. Running arbitrary code is a sandboxing project, not a feature. What we build instead is the **Checks** tab (§1) — one `check_runs` shape with two writers, a poller that reads GitHub Actions through the org's existing App installation and the signed `…/ci/checks` intake for everybody else. A project on Buildkite reads exactly like a project on GitHub Actions, logs link out, and there is no re-run button because a re-run button is a runner. Named Checks and not Actions deliberately: "Actions" is a competitor's product name and would imply we execute workflows. `/{owner}/{repo}/actions` redirects, because that is the address muscle memory types and 404ing somebody to prove a naming point helps nobody. |
| **Achievements / badges on profiles** | Gamification aimed at contribution volume, which is the metric we are explicitly not optimising. |
| **Sponsoring as a product** | A payments and compliance business. v1 is first-class links (Open Collective, Polar, Liberapay, Ko-fi) plus `funding.yml` import, rendered as a **Funding** row in the About panel. Real value, no regulated surface. |
| **Watch/notification level menus** | Watch stays as a count and a binary toggle. GitHub's four-level menu ("Participating and @mentions / All Activity / Ignore / Custom" with seven checkboxes) is a preferences panel wearing a button. |
| **`/trending`** | Ranks on 24-hour star velocity, which is the most gameable number in open source and the reason "trending" is full of AI-list repos. Per the plan, `sort=trending` returns `400 sort "trending" is not available yet` until `repo_signals` has 30 days of data — and when it does exist, the ranking rule is published on the page. A refusal you can test beats a ranking you cannot defend. |
| **`/explore`'s "based on your interests" feed** | GitHub's explore page leads with an embedded YouTube video and a personalised recommendation strip. Our `/explore` leads with curated topic pages, lighthouse spotlights and "new & notable", and says on the page how each list is built. |
| **The languages colour table** | 200+ hardcoded hues. See §1. |
| **The profile radar chart** | Four percentages drawn as a quadrilateral. Render the four numbers. |
| **The Network graph** (`/network`) | A canvas-drawn commit-topology visualisation nobody has read since 2013. The **Forks** list stays; the tree/graph rendering does not. |
| **GitHub's fork tree page** | It renders 3,400 forks as an indented list of `user / repo` with no signal — no ahead/behind, no last-push, no stars. Ours is a sortable **table**: `owner · stars · commits ahead/behind · last pushed`, defaulting to "forks with commits not in upstream first", which is the only question anyone actually opens the page to ask. |
| **Dismissible-forever banners** | Replaced by trust-rung-conditional banners (§2). |
| **Emoji in structural chrome** | GitHub's contribute page opens with a waving-hand emoji inside a heading. Emoji render in project *content* (a description, a README) and never in our own chrome. |

---

## 8. Accessibility and the audit gate, per surface

`tools/walkthrough.mjs`'s `audit()` fails the manual pass on four
things, and the gate is **0 problems**:

1. `documentElement.scrollWidth > clientWidth + 1` — horizontal overflow
2. any `button, a, input, select` whose rect extends past `clientWidth`
   or left of 0 — an offscreen control
3. any `<button>` with neither text content nor `aria-label`
4. `body.innerHTML` matching `[A-Za-z0-9]{2,}<(a|span|code|strong|em)` —
   a word jammed against an inline tag

Where each fires on these surfaces, and the mitigation:

| Surface | Hazard | Mitigation |
|---|---|---|
| Every page | the tab strip at ≥6 tabs on a narrow viewport | `overflow-x-auto` on an **inner** wrapper — audit measures the document element, so an inner scroller is invisible to it. Never `overflow-x` on `body` or the page container. |
| Issues / PR index | the 7-control filter bar | inner scroller **and** `flex-wrap` below `md` |
| Issues / PR index | icon-only clear, search, kebab, star, fork | `aria-label` on every one (`"Clear search"`, `"Run search"`, `"Change actions"`, `"Star this repository"`, `"Fork this repository"`) |
| Issue / PR rows | `#161436 · opened 5d ago by <a>author</a>` and inline `LabelPill`s | `{" "}` between text and every inline tag, without exception |
| Profile | the 53×7 heatmap | `min-w-0` on the flex parent · `overflow-x-auto` on the table wrapper · cells are `<td role="gridcell">` with `aria-label`, **never** `<button>` |
| Profile | the vertical year rail | it is a list of links with text; safe. Below `lg` it becomes a chip row with an inner scroller. |
| Repo overview | the About panel's topic pills | they wrap; cap at two rows with a `+N` **text** button (`aria-label="Show all N topics"`) |
| Repo overview | long branch names, long file names in the code table | `truncate` + `title`; `min-w-0` on the grid child |
| Intake queue | the 6-column table and its checkboxes | `overflow-x-auto` on the table wrapper · `truncate` on the Change cell · a labelled select-all |
| Intake queue | four gate glyphs per row, 4 × N icon-only elements | render as `<span aria-label>`, not buttons; the row's expansion control is the one button and it is labelled |

Two habits that keep this cheap rather than a cleanup pass:

- **Any glyph that is the only content of a control is a bug until it
  has a label.** Write the `aria-label` in the same keystroke as the
  icon import.
- **Build before you look.** Both browser gates read `dist`. A source
  edit with no `npm run build` behind it is audited against the previous
  bundle — which is quiet in both directions, and is how "the fix didn't
  work" and "the test is decoration" both get concluded wrongly.

---

## 9. Judgement calls, decided

Three places where GitHub's convention and `DESIGN.md` genuinely
conflict. All three were put to the owner and are now settled — recorded
here with the alternative that was rejected, so a future reader knows the
choice was made rather than defaulted into.

1. **Label colours — monochrome pill with a hue dot.** A label pill
   renders on `--surface-2` with `text-ink`, carrying a 6px desaturated
   dot in the label's own hue. The imported information survives without
   turning the labels row into the loudest thing on the page.
   *Rejected:* letting GitHub's 18%-fill/30%-border hues through and
   exempting user-authored data from the two-accent rule.

2. **Link colour — ink, with emerald on hover.** Structural links (repo
   names, issue and PR titles, usernames, file names, breadcrumb
   segments) are `text-ink`/`text-ink-2` and reveal on
   `hover:text-brand hover:underline underline-offset-2`; they are
   already identifiable as links by position and weight. Prose links —
   inside a rendered README, a description, an inline `#123` — are
   `text-brand`. Focus is always a visible `ring-2 ring-brand`, on both
   kinds, because hover-only affordance is not an affordance for a
   keyboard. *Rejected:* `--series-3` as a dedicated GitHub-blue-alike,
   which would have been closer muscle memory at the cost of a third hue
   on every page.

3. **The active tab rule stays emerald.** A 2px `--brand` rule under the
   active tab is decoration, which `DESIGN.md`'s do-not list is wary of —
   but it is the page's primary orientation cue, and orientation earns
   the exception. It remains the one place this spec spends brand colour
   on something that is not an action.

## 10. The maintainer-attention posture is shipped, not implied

All four of these were confirmed as in scope for the UI itself rather
than left to marketing copy. They are what makes the forge legibly ours
on first sight:

- **The author-association badge is promoted.** GitHub tucks
  Member/Contributor/First-time next to a timestamp. Here it is a real
  signal on every change and issue row — the trust ladder, made visible,
  so a maintainer reads standing before they read the title.
- **Agent attribution appears wherever a name does.** Agent-authored
  commits, changes, issues and comments carry a distinct marker and never
  borrow a human name. The marker is neutral, not pejorative: a project
  that welcomes agents and one that refuses them both need to *see* which
  is which, and the per-repo policy decides what happens next.
- **Mirror provenance is stated honestly.** A mirrored repository shows
  its native star count beside an explicitly labelled imported one —
  "60.3k on GitHub" — because a zeroed counter makes a migrated project
  look dead, and that is a lie about the project rather than a blank.
- **Intake state renders on the row.** A contribution's firewall verdict
  (gates passed, similarity flag, triage summary) sits inline in the
  list, not behind a separate tab. The whole point is that the maintainer
  sees a ranked, annotated queue at a glance; a queue you have to click
  into is a second inbox.
