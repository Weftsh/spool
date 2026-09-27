import { Badge } from "@/components/ui/badge";
import { FORGE_CONTAINER } from "@/lib/links";
import { cn } from "@/lib/utils";

export interface Tab {
  /// Matched against `active`. Usually the route table's own tab name,
  /// so a page never has to translate between two vocabularies.
  key: string;
  label: string;
  /// A real address. Not optional, and not a convenience.
  href: string;
  /// Omitted, or zero, renders no pill — a `0` beside a tab is noise
  /// that reads as a defect.
  count?: number | null;
}

/// The repo page's, the profile's and the org page's one tab strip.
///
/// Two rules here are load-bearing rather than stylistic.
///
/// **Tabs are links.** A tab is a URL somebody can send; a state-only
/// button is a page nobody can link to, and a forge whose issue list
/// cannot be pasted into chat is a forge people work around. Clicking
/// one is intercepted for client-side navigation, but the `href` is
/// real, so middle-click, copy-link and a crawler all work.
///
/// **The overflow scroller is the inner container.** `audit()` measures
/// `documentElement.scrollWidth`, so a wide strip that scrolls the
/// document fails the manual gate while the identical strip scrolling
/// inside its own box is invisible to it. Never move `overflow-x` up to
/// the page container, and do not try to make the tabs wrap.
///
/// The active tab's 2px emerald rule is the one place this spec spends
/// brand colour on something that is not an action — orientation earns
/// the exception (FORGE-UX §9.3).
export function TabStrip(props: {
  tabs: Tab[];
  active: string;
  /// What this strip navigates, e.g. "Repository" — it becomes the
  /// nav's accessible name, and a page with two navs needs them told
  /// apart.
  label: string;
  onNavigate: (to: string) => void;
  className?: string;
}) {
  return (
    <div className={cn("border-b border-borderline", props.className)}>
      <div className={FORGE_CONTAINER}>
        <div className="overflow-x-auto">
          <nav
            aria-label={props.label}
            className="flex gap-1 whitespace-nowrap"
          >
            {props.tabs.map((tab) => {
              const active = tab.key === props.active;
              const count = tab.count ?? 0;
              return (
                <a
                  key={tab.key}
                  href={tab.href}
                  aria-current={active ? "page" : undefined}
                  onClick={(e) => {
                    e.preventDefault();
                    props.onNavigate(tab.href);
                  }}
                  className={cn(
                    "-mb-px flex h-12 items-center gap-2 border-b-2 px-3 text-sm transition",
                    "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand",
                    active
                      ? "border-brand font-medium text-ink"
                      : "border-transparent text-ink-2 hover:border-borderline hover:text-ink",
                  )}
                >
                  {tab.label}
                  {count > 0 && (
                    <>
                      {/* The space is not cosmetic: audit() fails the
                          build on a word jammed against an inline tag. */}{" "}
                      <Badge variant="neutral" className="px-2 py-0 font-mono">
                        {count}
                      </Badge>
                    </>
                  )}
                </a>
              );
            })}
          </nav>
        </div>
      </div>
    </div>
  );
}
