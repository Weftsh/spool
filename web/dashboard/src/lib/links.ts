/// The design system's link and layout vocabulary, spelled once.
///
/// These lived in `shells/forge-shell.tsx` while the forge was the only
/// thing that used them. They are not chrome, though — they are the
/// rules `web/FORGE-UX.md` §9.2 settled about what a link looks like,
/// and the code browser obeys them under *both* shells. A view reaching
/// into a shell to find out how to draw a link had the dependency
/// pointing the wrong way.
/// The forge column, spelled once.
///
/// `web/DESIGN.md` gives the dashboard `max-w-5xl px-5` and says not to
/// mix containers; this is the second one, wider because a repo page
/// carries a file tree and an About panel side by side. `min-w-0` is
/// not decoration: without it a wide table or a long branch name widens
/// the page instead of ellipsising, and the walkthrough's `audit()`
/// fails the build on `documentElement.scrollWidth`.
export const FORGE_CONTAINER = "mx-auto w-full min-w-0 max-w-[1280px] px-4";

/// A structural link — a repo name, an issue title, a username, a file
/// name, a breadcrumb segment.
///
/// It is ink and reveals itself on hover, rather than taking GitHub's
/// blue: adopting that with `--brand` would put emerald on forty
/// elements per page and destroy what DESIGN.md is protecting — let
/// charcoal stay neutral and emerald carry identity. Position and
/// weight already say "link"; the focus ring is separate and always
/// visible, because hover-only affordance is not an affordance for a
/// keyboard. (FORGE-UX §9.2.)
export const FOCUS_RING =
  "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand " +
  "focus-visible:ring-offset-2 focus-visible:ring-offset-surface-0";

export const STRUCTURAL_LINK = `rounded-sm text-ink hover:text-brand hover:underline underline-offset-2 ${FOCUS_RING}`;

/// The same, one step quieter: an owner name beside a repo name, a
/// secondary breadcrumb.
export const STRUCTURAL_LINK_2 = `rounded-sm text-ink-2 hover:text-brand hover:underline underline-offset-2 ${FOCUS_RING}`;

/// A prose link — inside a rendered README, a description, a banner
/// sentence, an inline `#123`. These do carry the brand colour: there
/// is no position or weight to make them legible in a paragraph.
export const PROSE_LINK = `rounded-sm text-brand hover:underline underline-offset-2 ${FOCUS_RING}`;
