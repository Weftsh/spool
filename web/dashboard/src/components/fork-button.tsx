import { useState } from "react";
import { GitFork } from "lucide-react";
import { toast } from "sonner";
import { api, type Repo, type Session } from "@/api";
import { CountButton } from "@/components/count-button";

/// Fork this repository, and say how many people already have.
///
/// GitHub's shape: an outline button carrying the count, sitting beside
/// Watch.
export function ForkButton(props: {
  session: Session;
  repo: string;
  count: number;
  onForked: (repo: Repo) => void;
}) {
  const [busy, setBusy] = useState(false);

  async function fork() {
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
      // knows *why* — no namespace to fork into, a name already taken —
      // and each of those is something the person can act on.
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
      ariaLabel="Fork this repository"
    />
  );
}
