import { cn } from "@/lib/utils";

/// The series family, in fixed order, is the whole language palette.
///
/// GitHub ships a table of 200+ hardcoded hues, one per language, and a
/// repo card carrying three of them is three accent hues in a view that
/// DESIGN.md caps at two. So a language's colour is its *rank* in this
/// repository — the first language gets `--series-1`, the second
/// `--series-2`, the third `--series-3` — and everything past third is
/// the muted text colour, the same "Other" slot the languages bar uses.
///
/// The consequence worth stating: the dot means "biggest language here",
/// not "Rust". Which is why nothing ever renders it alone — every call
/// site pairs it with the language's name, and the legend entry is
/// `dot + name + percent` so the bar is never read by colour alone.
const SERIES = ["var(--series-1)", "var(--series-2)", "var(--series-3)"];

export function languageColor(rank: number): string {
  if (!Number.isInteger(rank) || rank < 0 || rank >= SERIES.length) {
    return "var(--text-muted)";
  }
  return SERIES[rank];
}

export function LanguageDot(props: {
  name: string;
  /// Position in the repository's language list, biggest first.
  rank: number;
  className?: string;
}) {
  return (
    <span className={cn("inline-flex items-center gap-1.5", props.className)}>
      <span
        aria-hidden
        className="size-2.5 shrink-0 rounded-full"
        style={{ background: languageColor(props.rank) }}
      />
      {props.name}
    </span>
  );
}
