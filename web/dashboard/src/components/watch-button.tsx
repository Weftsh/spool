import { useCallback, useEffect, useRef, useState } from "react";
import { Check, Eye, EyeOff } from "lucide-react";
import { toast } from "sonner";
import { api, type Session, type WatchLevel } from "@/api";
import { CountButton } from "@/components/count-button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/utils";

/// The three subscriptions, with the sentence each one is a promise
/// about. Exported because the wording *is* the feature — a menu of
/// three near-synonyms ("all", "participating", "ignore") with no
/// explanation asks the reader to guess what the product will do with
/// their attention, and they guess wrong.
///
/// The middle description is ours rather than GitHub's, and it is the
/// interesting one. On GitHub "involved in" is a bag of heuristics about
/// threads you have touched; here "needs your review" is a *computed*
/// fact — the OWNERS file decides which people a change actually
/// requires, so a change landing in a directory you own reaches you
/// whether or not you have ever spoken on it. Saying "that need your
/// review" out loud is only honest because something in the repository
/// answers the question.
///
/// GitHub has a fourth item, Custom. We deliberately do not: it opens a
/// dialog of event-type checkboxes with no backing here, and a menu
/// entry that leads nowhere is worse than an absent one (FORGE-UX).
export const WATCH_ITEMS: {
  level: WatchLevel;
  title: string;
  description: string;
}[] = [
  {
    level: "participating",
    title: "Participating and @mentions",
    description:
      "Only notified about changes you are involved in: ones you opened, " +
      "commented on, approved, or that need your review.",
  },
  {
    level: "all",
    title: "All Activity",
    description: "Notified of everything that happens in this repository.",
  },
  {
    level: "ignore",
    title: "Ignore",
    description: "Never notified.",
  },
];

/// What the control calls the state it is in.
///
/// `participating` is the default everybody starts at, so it reads as
/// the invitation — "Watch" — rather than as a state somebody chose;
/// the other two are states, and say so. Same three words GitHub's
/// button cycles through, for the same reason: this is the one control
/// in the product whose whole value is that it is already familiar.
export function watchLabel(level: WatchLevel): string {
  switch (level) {
    case "all":
      return "Watching";
    case "ignore":
      return "Ignoring";
    case "participating":
      return "Watch";
  }
}

/// The Watch control: how many people hear about this repository, and
/// the menu that changes the viewer's own answer.
///
/// The count rides on the repository row beside the fork count — see
/// `watches::watching_count` — so it is known before the subscription
/// read answers and the identity row never changes width after paint.
export function WatchButton(props: {
  session: Session;
  repo: string;
  /// How many people are subscribed to everything here. From the
  /// repository row, so it is known before the subscription read
  /// answers and the control never changes width.
  count: number;
}) {
  const { repo } = props;
  // The primitives, not the object: a parent that builds its session
  // inline hands a fresh object every render, and an effect depending
  // on that object refetches forever.
  const { org, token } = props.session;
  const [level, setLevel] = useState<WatchLevel | null>(null);

  // Which write is the live one. Two selections in quick succession must
  // not have the first one's failure roll back the second one's state:
  // only the newest request may still touch what is on screen.
  const newest = useRef(0);

  useEffect(() => {
    let alive = true;
    setLevel(null);
    api
      .watch({ org, token }, repo)
      .then((r) => alive && setLevel(r.level))
      // A subscription we could not read leaves the control on its
      // invitation — "Watch" — rather than removing it. The count
      // beside it is still true, and the menu still works: choosing a
      // level is a PUT that does not depend on having read the old one.
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [org, token, repo]);

  const choose = useCallback(
    async (next: WatchLevel) => {
      const previous = level;
      if (next === previous) return;
      const ticket = ++newest.current;
      // Optimistic: the menu closes on select, and a control that keeps
      // showing the old answer while a request is in flight reads as a
      // click that did not land.
      setLevel(next);
      try {
        const r = await api.setWatch({ org, token }, repo, next);
        if (newest.current === ticket) setLevel(r.level);
      } catch {
        if (newest.current !== ticket) return;
        setLevel(previous);
        toast.error("Could not change what you are notified about");
      }
    },
    [level, org, token, repo],
  );

  // `null` is "not read yet, or not readable" — either way the control
  // shows its invitation. It is never absent: the row's width must not
  // depend on how far the read has got.
  const shown: WatchLevel = level ?? "participating";
  const Glyph = shown === "ignore" ? EyeOff : Eye;

  const trigger = (
    <CountButton
      icon={<Glyph aria-hidden className="size-4" />}
      label={watchLabel(shown)}
      count={props.count}
      active={shown === "all"}
      menu
      aria-haspopup="menu"
    />
  );

  return (
    <DropdownMenu>
      {/* One button, not two halves. GitHub's Watch control opens its
          menu from anywhere on itself, and there is nothing for a left
          half to do here that the menu does not do better. The caret is
          decoration inside a button that already carries its own text,
          so `CountButton` hides it from the accessibility tree rather
          than labelling it. */}
      <DropdownMenuTrigger asChild>{trigger}</DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-80">
        <DropdownMenuLabel>Notifications</DropdownMenuLabel>
        <DropdownMenuSeparator />
        {WATCH_ITEMS.map((item) => {
          const current = item.level === level;
          return (
            <DropdownMenuItem
              key={item.level}
              // Radix's Item is a plain menuitem; the vendored primitive
              // was trimmed of its radio piece because nothing spoke
              // that vocabulary yet. These are one exclusive choice, so
              // they say so — and "which one is on" is then a state a
              // screen reader announces, not a glyph it has to see.
              role="menuitemradio"
              aria-checked={current}
              onSelect={() => void choose(item.level)}
              className="items-start gap-2 px-2 py-2"
            >
              <Check
                aria-hidden
                className={cn(
                  "mt-0.5 size-4 shrink-0",
                  current ? "text-brand" : "invisible",
                )}
              />
              <span className="flex min-w-0 flex-col gap-0.5">
                <span className="font-medium text-ink">{item.title}</span>
                <span className="text-xs leading-snug text-ink-2">
                  {item.description}
                </span>
              </span>
            </DropdownMenuItem>
          );
        })}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
