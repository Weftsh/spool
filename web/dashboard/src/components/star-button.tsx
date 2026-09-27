import { useEffect, useState } from "react";
import { Star } from "lucide-react";
import { toast } from "sonner";
import { api, type Session, type StarState } from "@/api";
import { CountButton } from "@/components/count-button";

/// Star a repository.
///
/// One control, drawn by `CountButton` exactly like Watch and Fork
/// beside it. It used to draw itself, as a `flex-col` holding the button
/// and a caption reading "60.3k on GitHub" — the mirrored project's own
/// count, which is the single most useful thing this page can tell
/// somebody deciding whether a migrated project is alive. The caption
/// was right and its placement was not: inside an `items-center` row it
/// made Star taller than its two siblings, pushed the row's baseline,
/// and read as an annotation on the Star button rather than as a fact
/// about the repository. It now lives in the About rail's counts block,
/// beside the stars, watchers and forks it belongs with.
///
/// The two counts are still rendered as two separate things and are
/// never added. That is the whole argument for having stars at all on a
/// forge that begins empty: a mirrored project's honest `☆ 4` here
/// beside a labelled "60.3k on GitHub" there tells the truth twice,
/// where our count alone says a migrated project is dead and a sum says
/// something no one can check (FORGE-UX §6).
export function StarButton(props: {
  session: Session;
  repo: string;
  /// Whether the viewer is signed in. A stranger sees the count and a
  /// prompt to sign in rather than a control that fails when clicked.
  signedIn: boolean;
  onSignIn?: () => void;
  className?: string;
}) {
  const { repo } = props;
  // The primitives, not the object — the same fix `WatchButton` next
  // door carries, for the same reason. A parent that builds its session
  // inline (`anon(owner)`, which is exactly how the forge repo page
  // calls its data) hands a fresh object every render, and an effect
  // depending on that object refetches forever. The one caller here
  // does memoise, and its own comment says "not every component does",
  // which is a note about the next caller rather than a defence: the
  // hazard belongs to this component, so the fix does too.
  const { org, token } = props.session;
  const [state, setState] = useState<StarState | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let alive = true;
    setState(null);
    api
      .getStars({ org, token }, repo)
      .then((s) => alive && setState(s))
      .catch(() => {
        // The count is an adornment on somebody else's page: failing to
        // read it hides the control rather than breaking the view.
        if (alive) setState(null);
      });
    return () => {
      alive = false;
    };
  }, [org, token, repo]);

  if (!state) return null;

  const toggle = async () => {
    if (!props.signedIn) {
      props.onSignIn?.();
      return;
    }
    setBusy(true);
    try {
      // The server is idempotent, so a double-click cannot double-count
      // — but it can still race, so the control is disabled while in
      // flight and the answer replaces local state rather than being
      // guessed at. No optimistic increment: a number that flickers to
      // a value the server then contradicts is worse than one that
      // takes a moment.
      const next = state.starred
        ? await api.unstar({ org, token }, repo)
        : await api.star({ org, token }, repo);
      setState(next);
    } catch (e) {
      // A toast, not a line of red under the button. The old rendering
      // put the message inside this component's own column, which is
      // the same reason the origin caption had to move: anything that
      // grows downward from one control in an `items-center` row shoves
      // its two siblings off the baseline. `ForkButton` next door
      // already reports its failures this way.
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <CountButton
      icon={
        <Star
          aria-hidden
          className={state.starred ? "size-4 fill-current" : "size-4"}
        />
      }
      label={state.starred ? "Starred" : "Star"}
      count={state.stars}
      active={state.starred}
      pressed={state.starred}
      // A stranger's press navigates to sign-in, so the name has to say
      // so: without this the accessible name was "Star 4" on a control
      // that does not star anything, which is the one reading of it
      // that is wrong. `ForkButton` says "Sign in to fork" and
      // `WatchButton` "Sign in to watch"; this was the only one of the
      // three masthead controls that did not explain itself.
      //
      // Signed in, the visible label *is* the whole sentence, so there
      // is no override — same as Watch. The count leaves the accessible
      // name in the signed-out case, deliberately: "Sign in to star 4"
      // reads as an instruction to star four things.
      ariaLabel={props.signedIn ? undefined : "Sign in to star"}
      disabled={busy}
      onClick={toggle}
      className={props.className}
    />
  );
}
