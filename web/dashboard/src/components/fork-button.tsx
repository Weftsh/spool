import { useState } from "react";
import { GitFork } from "lucide-react";
import { toast } from "sonner";
import { api, type Repo, type Session } from "@/api";
import { CountButton } from "@/components/count-button";

/// Fork this repository, and say how many people already have.
///
/// GitHub's shape: an outline button carrying the count, sitting with
/// Star and Watch. The count is public information and shows to
/// everybody — it is one of the few honest signals a stranger has about
/// whether a project is worked on — while the action needs a person.
///
/// A signed-out visitor gets the count and a prompt to sign in rather
/// than a button that fails when pressed. That is the same rule the star
/// control follows and the reason the watch control renders nothing at
/// all: a control is either usable or it explains itself, never a
/// surface that only errors.
export function ForkButton(props: {
  session: Session;
  repo: string;
  count: number;
  signedIn: boolean;
  onSignIn?: () => void;
  onForked: (repo: Repo) => void;
}) {
  const [busy, setBusy] = useState(false);

  async function fork() {
    if (!props.signedIn) {
      props.onSignIn?.();
      return;
    }
    setBusy(true);
    try {
      const { repo: made, existing } = await api.forkRepo(
        props.session,
        props.repo,
      );
      // Pressing Fork on something already forked lands on that fork,
      // as on GitHub. Said out loud, because the page that appears is
      // otherwise indistinguishable from a copy made just now — and a
      // person who has commits on that fork should know nothing was
      // reset.
      if (existing)
        toast.message("You already have a fork of this — here it is.");
      props.onForked(made);
    } catch (e) {
      // The server's own sentence, not a shrug. It is the one that
      // knows *why* — no namespace to fork into, a name already taken,
      // a plan without room — and each of those is something the person
      // can act on.
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <CountButton
      icon={<GitFork aria-hidden className="size-4" />}
      label={busy ? "Forking…" : "Fork"}
      count={props.count}
      disabled={busy}
      onClick={fork}
      // The visible word is "Fork" either way; the accessible name says
      // which of the two things this press will actually do. A stranger
      // pressing it is sent to sign in, and a control whose name
      // promised a fork would have lied about that.
      ariaLabel={props.signedIn ? "Fork this repository" : "Sign in to fork"}
    />
  );
}
