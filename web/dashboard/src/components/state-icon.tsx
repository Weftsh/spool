import {
  Check,
  CircleCheck,
  CircleDashed,
  CircleDot,
  CircleSlash,
  X,
  type LucideIcon,
} from "lucide-react";
import { cn } from "@/lib/utils";

/// Issue and check state — glyph plus label, never colour alone.
///
/// DESIGN.md is absolute about this and state is where it matters most:
/// open-versus-closed is drawn on the red/green axis, which is exactly
/// the axis 8% of men cannot resolve. So the glyph differs in *shape*
/// between every pair, and the word is always reachable — rendered
/// beside the icon where there is room, and as the accessible name plus
/// a `title` where there is not.
///
/// Changes are not here: a change's state is a word in a badge
/// (`stateTone` in `lib/changesets.ts`), and this table has never
/// described one. It used to carry GitHub's pull-request states, which
/// no view drew — a change opens, lands or is abandoned; it is not
/// merged.
export type State =
  | "issue-open"
  | "issue-completed"
  | "issue-not-planned"
  | "check-passed"
  | "check-failed"
  | "check-pending";

export const STATES: Record<
  State,
  { icon: LucideIcon; label: string; className: string }
> = {
  "issue-open": { icon: CircleDot, label: "Open", className: "text-good" },
  "issue-completed": {
    icon: CircleCheck,
    label: "Closed as completed",
    className: "text-brand",
  },
  "issue-not-planned": {
    icon: CircleSlash,
    label: "Closed as not planned",
    className: "text-ink-3",
  },
  "check-passed": {
    icon: Check,
    label: "All checks passed",
    className: "text-good",
  },
  "check-failed": {
    icon: X,
    label: "Checks failed",
    className: "text-serious",
  },
  "check-pending": {
    icon: CircleDashed,
    label: "Checks running",
    className: "text-warning",
  },
};

export function StateIcon(props: {
  state: State;
  /// Render the word next to the glyph. Do this wherever the row has
  /// room; the label-only form is the concession, not the default.
  showLabel?: boolean;
  className?: string;
}) {
  const { icon: Icon, label, className } = STATES[props.state];
  if (props.showLabel) {
    return (
      <span
        className={cn(
          "inline-flex items-center gap-1.5",
          className,
          props.className,
        )}
      >
        <Icon aria-hidden className="size-4 shrink-0" />
        {label}
      </span>
    );
  }
  return (
    <span
      role="img"
      aria-label={label}
      title={label}
      className={cn("inline-flex", className, props.className)}
    >
      <Icon aria-hidden className="size-4 shrink-0" />
    </span>
  );
}
