import * as React from "react";
import { formatCount } from "@/format";
import { FOCUS_RING } from "@/lib/links";
import { cn } from "@/lib/utils";

/// What a count reads as inside the cap.
///
/// Zero renders `0`, deliberately. FORGE-UX §1.2 says the counts on this
/// row are always visible "including zero", and the reason is that a
/// young project's honest `0` and a control that has decided not to show
/// you a number are different facts — a reader scanning for whether
/// anybody is here can act on the first and learns nothing from the
/// second. Blank, "—", and a hidden cap all say the second thing.
///
/// Anything that is not a count still says `–`. A NaN in the cap is a
/// bug upstream of here, and printing `NaN` beside a glyph is the one
/// rendering worse than admitting we do not know.
export function countLabel(n: number): string {
  return Number.isFinite(n) && n >= 0 ? formatCount(n) : "–";
}

/// The masthead's split control: a labelled action, and the number it
/// is about, in one bordered shape.
///
/// Watch and Fork are one row of siblings and a person reads them as one
/// object, so they are one component rather than two that happen to
/// agree. Separately drawn siblings drifted apart in height more than
/// once, which is the shape of a problem that keeps coming back until
/// they stop being separately drawn.
///
/// The count sits in its own `bg-surface-2` cap behind a hairline rather
/// than inline in the label, because that is the boundary that lets the
/// number be scanned without being read: a column of three caps at the
/// same x is a column of numbers. It is mono, per DESIGN.md.
///
/// One `<button>`, not a button and a link: there is no page listing
/// the people behind a count to send anybody to, and a second focus stop
/// that leads nowhere costs a keyboard user a keystroke on every
/// repository page in exchange for nothing.
export const CountButton = React.forwardRef<
  HTMLButtonElement,
  {
    /// The glyph, already sized by the caller. Always `aria-hidden`:
    /// the label beside it is the accessible name.
    icon: React.ReactNode;
    label: string;
    count: number;
    /// Whether this control reports a state the viewer has chosen —
    /// watching everything. Drawn as a filled surface rather than a
    /// colour, so it survives being read by somebody who cannot see the
    /// difference between our two greens.
    active?: boolean;
    /// A caret, for the variants that open a menu. Decoration inside a
    /// button that already carries its own text, so it is hidden from
    /// the accessibility tree rather than labelled.
    menu?: boolean;
    /// Overrides the accessible name. Give this only when the visible
    /// label is not the whole sentence — "Fork this repository" over a
    /// button reading "Fork". The count stays in the visible text
    /// either way, so a test can still read it off the control.
    ariaLabel?: string;
    pressed?: boolean;
  } & Omit<React.ComponentProps<"button">, "children">
>(function CountButton(
  { icon, label, count, active, menu, ariaLabel, pressed, className, ...rest },
  ref,
) {
  return (
    <button
      ref={ref}
      type="button"
      aria-label={ariaLabel}
      aria-pressed={pressed}
      className={cn(
        "inline-flex h-8 shrink-0 items-center overflow-hidden rounded-lg border text-sm font-medium transition",
        "disabled:pointer-events-none disabled:opacity-60",
        active
          ? "border-borderline bg-surface-2 text-ink"
          : "border-borderline bg-surface-1 text-ink-2 hover:text-ink",
        FOCUS_RING,
        className,
      )}
      {...rest}
    >
      <span className="flex h-full items-center gap-1.5 px-2.5">
        {icon}
        {label}
        {menu && <Caret />}
      </span>
      {/* The hairline is on the cap, not between two padded spans: a
          border on the label side would move when the word changes
          from "Watch" to "Watching" and the row would shuffle. */}
      <span className="flex h-full items-center border-l border-borderline bg-surface-2 px-2 font-mono text-xs">
        {countLabel(count)}
      </span>
    </button>
  );
});

function Caret() {
  return (
    <svg
      aria-hidden
      viewBox="0 0 16 16"
      className="size-3.5 text-ink-3"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d="M4 6l4 4 4-4" />
    </svg>
  );
}
