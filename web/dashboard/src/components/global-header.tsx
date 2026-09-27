import { Plus, Search } from "lucide-react";
import type { Me } from "@/api";
import { dash, href } from "@/router";
import { OwnerAvatar } from "@/components/owner-avatar";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { FORGE_CONTAINER } from "@/lib/links";
import { cn } from "@/lib/utils";

/// The forge's one piece of persistent chrome: mark, global search,
/// then `+ New` and the account menu.
///
/// The menus need a person. A session held by an API token has none —
/// `me` is null — and gets the mark and the search box alone.
///
/// Two accessible names here are contracts with suites this file cannot
/// see, and both are collisions rather than preferences:
///
/// - The search input is labelled **"Search Weft"**, not "Search
///   repositories". The sidebar owns that second name, and a
///   `.first()`/`.last()` ordering contract in both suites depends on
///   there being exactly two inputs with it.
/// - Nothing may carry the bare accessible name **"Settings"** — the
///   account item is "Your settings" and a repo's tab is "Repo
///   settings". A stale selector has to fail, not silently match
///   whichever chrome happens to be mounted.
///
/// It is also why the `+ New` menu's item is "Repository" and not "New
/// repository": that exact name belongs to the org overview's button
/// and is matched in strict mode, where a second match is an error.
export function GlobalHeader(props: {
  me: Me | null;
  onNavigate: (to: string) => void;
  onSignOut: () => void;
}) {
  const { me } = props;

  return (
    <header className="h-14 border-b border-borderline bg-surface-1">
      <div className={cn(FORGE_CONTAINER, "flex h-full items-center gap-3")}>
        <a
          href={dash([])}
          onClick={(e) => {
            e.preventDefault();
            props.onNavigate(dash([]));
          }}
          className="flex shrink-0 items-center gap-2 rounded-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand"
        >
          <svg width="22" height="22" viewBox="0 0 32 32" aria-hidden>
            <rect
              x="4"
              y="6"
              width="24"
              height="5"
              rx="2"
              fill="var(--brand)"
            />
            <rect
              x="4"
              y="14"
              width="24"
              height="5"
              rx="2"
              fill="var(--accent)"
            />
            <rect
              x="4"
              y="22"
              width="24"
              height="5"
              rx="2"
              fill="var(--series-3)"
            />
          </svg>
          <span className="font-semibold tracking-tight">Weft</span>
        </a>

        <form
          role="search"
          className="min-w-0 max-w-md flex-1"
          onSubmit={(e) => {
            e.preventDefault();
            const q = new FormData(e.currentTarget).get("q");
            // Its own address, so a result set is something somebody can
            // send. An empty query lists everything the viewer can see.
            const typed = String(q ?? "").trim();
            props.onNavigate(href(["search"], { q: typed || undefined }));
          }}
        >
          <label className="sr-only" htmlFor="forge-search">
            Search Weft
          </label>
          <div className="relative">
            <Search
              className="pointer-events-none absolute left-2.5 top-1/2 size-4 -translate-y-1/2 text-ink-3"
              aria-hidden
            />
            <Input
              id="forge-search"
              name="q"
              type="search"
              autoComplete="off"
              maxLength={128}
              placeholder="Search Weft"
              className="py-1 pl-8"
            />
          </div>
        </form>

        <div className="ml-auto flex shrink-0 items-center gap-2">
          {me && (
            <>
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button variant="outline">
                    <Plus aria-hidden className="size-4" /> New
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  {/* Only one entry, deliberately: creating an
                      organization is an item in the sidebar's org
                      switcher (app-sidebar's NEW_ORG) and has no
                      address of its own, so it cannot be linked to from
                      here without inventing a route nothing serves. */}
                  <DropdownMenuItem
                    onSelect={() => props.onNavigate(dash(["new"]))}
                  >
                    Repository
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>

              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  {/* Icon-only, so it is labelled: the walkthrough's
                      audit fails any button with neither text nor an
                      aria-label. */}
                  <button
                    type="button"
                    aria-label="Account menu"
                    className="rounded-full focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand"
                  >
                    <OwnerAvatar name={me.name} size={28} />
                  </button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  <DropdownMenuLabel>{me.name}</DropdownMenuLabel>
                  <DropdownMenuSeparator />
                  <DropdownMenuItem onSelect={() => props.onNavigate(dash([]))}>
                    Your repositories
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    // The person's own settings, not the organization's:
                    // a bare /settings lands on the first section the
                    // viewer may act on, which for an admin is Members —
                    // somebody else's settings, under "Your settings".
                    // This menu only renders with a person signed in, so
                    // Email addresses always exists.
                    onSelect={() =>
                      props.onNavigate(dash(["settings", "emails"]))
                    }
                  >
                    Your settings
                  </DropdownMenuItem>
                  <DropdownMenuSeparator />
                  <DropdownMenuItem onSelect={props.onSignOut}>
                    Sign out
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            </>
          )}
        </div>
      </div>
    </header>
  );
}
