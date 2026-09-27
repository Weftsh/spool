import { cn } from "@/lib/utils";

/// An issue label: monochrome pill, with the project's own hue kept as
/// a 6px dot.
///
/// A project encodes real meaning in its label palette, and GitHub
/// renders that as an 18%-fill / 30%-border pill in the label's hex —
/// which on a busy issue row is five saturated hues at once, exactly
/// what DESIGN.md's two-accent cap exists to stop. The decision
/// (FORGE-UX §9.1) keeps both: the *name* carries the meaning, the dot
/// carries the project's identity, the page keeps its palette.

/// An imported label's hex, turned into something that can never flare
/// against charcoal — and, just as importantly, into something safe.
///
/// **Nothing reaches this today.** The server validates `labels.color`
/// against seven token names at the write, so a hex cannot be stored,
/// and the importer that would bring one is not built. It is here
/// because the alternative — adding validation at the same time as the
/// untrusted input it guards — is how the guard gets skipped.
///
/// The hex would be imported data, arriving from whatever GitHub had,
/// so it is not trusted into a `style` attribute unvalidated. Anything
/// that is not a 3- or 6-digit hex gets nothing back, and `labelDot`
/// turns that into the neutral pill. The unit tests are the only
/// caller that passes it a real hex, and that is the honest state of
/// it — do not read this comment as evidence that import exists.
export function hueDot(color: string | null | undefined): string | undefined {
  if (!color) return undefined;
  const raw = color.trim();
  const hex = raw.replace(/^#/, "");
  if (!/^(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/.test(hex)) return undefined;
  // 70% of the label's hue mixed toward the muted text colour: enough
  // to tell two labels apart, never enough to out-shout the emerald.
  return `color-mix(in oklab, #${hex} 70%, var(--text-muted))`;
}

/// The un-tinted dot: the ink palette, no colour at all.
///
/// `neutral` is a **sentinel, not a token**. There is no `--neutral` in
/// `web/shared/tokens.css`, so interpolating the stored name would
/// produce `var(--neutral)`, which resolves to nothing and draws an
/// invisible dot — a pill that reads as a rendering bug rather than as
/// a label nobody has categorised. A grey dot from `--text-muted` reads
/// as deliberate, and keeps every pill in a row the same shape.
const NEUTRAL_DOT = "var(--text-muted)";

/// Every value the `labels.color` column is allowed to hold, mapped to
/// what it renders as.
///
/// A **lookup**, never string interpolation into a CSS variable name.
/// Interpolation is how an unexpected value becomes `var(--)`, and it is
/// also how a value with a `)` in it closes the `var()` and starts
/// writing declarations of its own.
///
/// A `Map` rather than an object literal, and that is not fussiness: a
/// plain `Record` inherits from `Object.prototype`, so a label whose
/// stored colour were `constructor` or `toString` would look up a
/// function and put it in a style attribute. A `Map` has no prototype
/// chain to walk.
///
/// Seven names, because three cannot carry an issue tracker — bug /
/// enhancement / documentation / good first issue / help wanted is the
/// standard set before a project adds one of its own. The series family
/// alone would also have been the wrong family: `web/DESIGN.md` assigns
/// its steps to charts in fixed order and never cycles them.
const LABEL_TOKENS = new Map<string, string>([
  ["series-1", "var(--series-1)"],
  ["series-2", "var(--series-2)"],
  ["series-3", "var(--series-3)"],
  ["status-good", "var(--status-good)"],
  ["status-warning", "var(--status-warning)"],
  ["status-serious", "var(--status-serious)"],
  ["neutral", NEUTRAL_DOT],
]);

/// The dot for a label, for any value at all.
///
/// Three sources, in order: one of the seven names the server validates
/// at the write; an imported project's own hex, muted; and, for anything
/// else, the neutral pill.
///
/// That last case is the one worth stating. The server refuses an
/// unknown name at the write, so an unknown one arriving here means the
/// palette grew and **this bundle is older than the data** — a version
/// skew, not bad data. Degrading to a readable grey pill is right;
/// rendering nothing, or throwing and taking the row with it, would turn
/// somebody else's deploy into a blank issue list.
///
/// Matching is exact and case-sensitive, because the server's is:
/// `SERIES-1` is refused at the write, so accepting it here would mean
/// the UI could render a label the database can never hold.
export function labelDot(color: string | null | undefined): string {
  const raw = (color ?? "").trim();
  return LABEL_TOKENS.get(raw) ?? hueDot(raw) ?? NEUTRAL_DOT;
}

export function LabelPill(props: {
  name: string;
  /// One of the seven names the server validates, or an imported
  /// project's hex. Anything else renders neutral rather than nothing.
  color?: string | null;
  className?: string;
}) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full border border-borderline bg-surface-2 px-2 py-0.5 text-xs font-medium text-ink",
        props.className,
      )}
    >
      <span
        aria-hidden
        className="size-1.5 shrink-0 rounded-full"
        style={{ background: labelDot(props.color) }}
      />
      {props.name}
    </span>
  );
}
