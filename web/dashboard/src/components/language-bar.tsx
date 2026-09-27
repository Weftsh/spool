import { languageColor } from "@/components/language-dot";
import type { Language } from "@/api";

/// One segment of the bar: a language, its share, and the rank that
/// picks its colour.
export interface Segment {
  name: string;
  bytes: number;
  /// 0..1. The width of the segment, and what the legend renders as a
  /// percentage.
  share: number;
  /// Position in the list, biggest first. `languageColor` turns 0/1/2
  /// into the three series steps and everything past that into the
  /// muted ink — which is exactly what "Other" should be.
  rank: number;
}

/// How many named languages the bar shows before it starts aggregating.
///
/// **Three, because there are three series colours.** `web/DESIGN.md`
/// assigns the `--series-*` family in fixed order and never cycles it,
/// and a fourth language would need a fourth hue — which means either
/// inventing one (a hex in a component, which the design system
/// forbids) or reusing one (two languages the same colour, which is
/// worse than not showing the fourth at all). GitHub ships a table of
/// two hundred hardcoded hues and can afford a segment per language; a
/// three-colour system cannot, and the honest way to spend three
/// colours is on the three biggest.
const NAMED = 3;

/// The label the remainder carries. Not a language, and the legend
/// renders it in the same muted ink `languageColor` gives rank 3+, so
/// it reads as a summary rather than as a language called "Other".
export const OTHER = "Other";

/// Turn the server's byte counts into the segments the bar draws.
///
/// The server sends every language it counted, sorted, because the
/// palette is not the API's business. Everything below the third is
/// summed here into one `Other` segment.
///
/// Three things this is careful about, each of which is a way a
/// proportional bar lies:
///
/// - **The shares are of the counted bytes, not of the repository.**
///   The server does not count Markdown, JSON or an extension it does
///   not know, so `share` means "of the code we could classify". The
///   panel says so beside the bar rather than leaving a reader to
///   assume the denominator.
/// - **A language with no bytes gets no segment**, which is also what
///   makes the division safe: `total` is only ever divided by after a
///   segment with `bytes > 0` has been kept, and one of those cannot
///   exist unless the total is positive. An empty repository, or one
///   with no recognised source, therefore falls out as an empty list
///   rather than as a full-width bar of one colour — which is what a
///   zero total produces if you "defend" it with a default of 1, and
///   which reads as "100% of something".
///
///   A separate `total <= 0` early return was here and was removed: the
///   filter already made it unreachable, a mutation of it changed no
///   test, and a guard that cannot fire is a guard nobody can tell has
///   stopped working.
/// - **`Other` is only drawn when it holds something.** A repository
///   with exactly three languages must not grow a fourth empty segment.
export function languageBar(langs: Language[]): Segment[] {
  const total = langs.reduce((sum, l) => sum + l.bytes, 0);
  const named = langs.slice(0, NAMED).filter((l) => l.bytes > 0);
  const rest = langs.slice(NAMED).reduce((sum, l) => sum + l.bytes, 0);
  const out: Segment[] = named.map((l, i) => ({
    name: l.name,
    bytes: l.bytes,
    share: l.bytes / total,
    rank: i,
  }));
  if (rest > 0) {
    out.push({
      name: OTHER,
      bytes: rest,
      share: rest / total,
      rank: NAMED,
    });
  }
  return out;
}

/// A share as a percentage, the way a reader expects to see one.
///
/// One decimal place, except that a language too small to round to
/// 0.1% renders as `<0.1%` rather than as `0.0%`. A segment that is
/// drawn and labelled zero is a segment that looks like a bug.
export function percent(share: number): string {
  const pct = share * 100;
  if (pct > 0 && pct < 0.05) return "<0.1%";
  return `${pct.toFixed(1)}%`;
}

/// The language bar and its legend.
///
/// The bar is never read by colour alone — the legend under it is
/// `dot + name + percent`, the same rule `LanguageDot` is written to
/// (`web/DESIGN.md`: status is never colour alone). That is an
/// accessibility rule and also a plain-honesty one here, because the
/// colour means "biggest language in this repository" and not "Rust":
/// the same emerald leads every repository's bar.
export function LanguageBar(props: {
  languages: Language[];
  partial?: boolean;
}) {
  const segments = languageBar(props.languages);
  if (segments.length === 0) return null;
  return (
    <div>
      <h3 className="text-sm font-medium text-ink">Languages</h3>
      <div
        className="mt-2 flex h-2 w-full overflow-hidden rounded-full bg-surface-2"
        role="img"
        aria-label={segments
          .map((s) => `${s.name} ${percent(s.share)}`)
          .join(", ")}
      >
        {segments.map((s) => (
          <span
            key={s.name}
            data-testid="language-segment"
            data-language={s.name}
            // The only inline style here is the width, which is data,
            // and a token name, which is the design system's own
            // vocabulary. No hex reaches a component.
            style={{
              width: `${s.share * 100}%`,
              background: languageColor(s.rank),
            }}
          />
        ))}
      </div>
      <ul className="mt-3 flex flex-wrap gap-x-4 gap-y-1">
        {segments.map((s) => (
          <li
            key={s.name}
            className="flex items-center gap-1.5 text-xs text-ink-2"
          >
            <span
              aria-hidden
              className="size-2 shrink-0 rounded-full"
              style={{ background: languageColor(s.rank) }}
            />
            <span className="text-ink">{s.name}</span>
            {/* Numbers always render in mono (DESIGN.md). */}
            <span className="font-mono text-ink-3">{percent(s.share)}</span>
          </li>
        ))}
      </ul>
      {props.partial && (
        <p className="mt-2 text-xs text-ink-3">
          Measured from part of the tree — this repository is larger than one
          pass counts.
        </p>
      )}
    </div>
  );
}
