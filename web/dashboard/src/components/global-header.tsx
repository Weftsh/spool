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
/// then `+ New` and the account menu — or a single **Sign in** button
/// when nobody is signed in.
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
  currentPath: string;
  onNavigate: (to: string) => void;
  onSignOut: () => void;
}) {
  const { me } = props;

  // A visitor who signs in from a repository page wants that page back,
  // not the dashboard. `routes.ts` only honours a same-origin path, so
  // handing it one is safe by construction.
  const signIn = href(["login"], { next: props.currentPath });

  return (
    <header className="h-14 border-b border-borderline bg-surface-1">
      <div className={cn(FORGE_CONTAINER, "flex h-full items-center gap-3")}>
        <a
          href="/"
          onClick={(e) => {
            e.preventDefault();
            props.onNavigate("/");
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
            // `/search` with the query, not `/explore` carrying one. Explore is
            // the whole public set; a search is a narrowing of it, and giving
            // the narrowing its own address is what makes a result set
            // something somebody can send.
            const typed = String(q ?? "").trim();
            props.onNavigate(
              typed ? href(["search"], { q: typed }) : href(["explore"]),
            );
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
          {me ? (
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
          ) : (
            <Button variant="outline" asChild>
              <a
                href={signIn}
                onClick={(e) => {
                  e.preventDefault();
                  props.onNavigate(signIn);
                }}
              >
                Sign in
              </a>
            </Button>
          )}
        </div>
      </div>
    </header>
  );
}
