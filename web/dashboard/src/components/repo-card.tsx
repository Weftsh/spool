import { Book, GitFork } from "lucide-react";
import { formatCount } from "@/format";
import { LanguageDot } from "@/components/language-dot";
import { STRUCTURAL_LINK } from "@/lib/links";
import { cn } from "@/lib/utils";

/// A repository as a card: the grid on an owner's page, and search
/// results.
///
/// A count is always mono, including zero, and a row of counts that
/// appears and disappears is a row nobody can scan.
export function RepoCard(props: {
  /// Rendered only when it is not the page's own owner: on an owner's
  /// grid the owner is the page.
  owner?: string | null;
  name: string;
  href: string;
  description?: string | null;
  /// The repository's dominant language and its rank in the language
  /// list — the rank is what picks the dot's series colour.
  language?: { name: string; rank: number } | null;
  forks?: number;
  onNavigate: (to: string) => void;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "flex flex-col gap-2 rounded-xl border border-borderline bg-surface-1 p-4 transition hover:border-brand/40",
        props.className,
      )}
    >
      <div className="flex min-w-0 items-center gap-2">
        <Book aria-hidden className="size-4 shrink-0 text-ink-3" />
        <a
          href={props.href}
          onClick={(e) => {
            e.preventDefault();
            props.onNavigate(props.href);
          }}
          className={cn(STRUCTURAL_LINK, "truncate font-semibold")}
        >
          {props.owner && (
            <>
              <span className="font-normal text-ink-2">{props.owner}</span>
              <span className="font-normal text-ink-3"> / </span>
            </>
          )}
          {props.name}
        </a>
      </div>

      {props.description && (
        <p className="line-clamp-2 text-sm text-ink-2">{props.description}</p>
      )}

      <div className="mt-auto flex flex-wrap items-center gap-4 pt-1 text-xs text-ink-3">
        {props.language && (
          <LanguageDot name={props.language.name} rank={props.language.rank} />
        )}
        {/* Omitted, not zeroed, when the caller has no count to give:
            "nobody has forked this" and "nobody told us" are different
            sentences. */}
        {props.forks !== undefined && (
          <span className="inline-flex items-center gap-1.5">
            <GitFork aria-hidden className="size-3.5" />
            <span className="font-mono">{formatCount(props.forks)}</span>
            <span className="sr-only">forks</span>
          </span>
        )}
      </div>
    </div>
  );
}
