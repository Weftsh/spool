import type { Runner, RunnerGroup } from "@/api";

/// The decisions the Runners settings page makes, kept out of the view
/// so they can be tested without a browser.
///
/// Four of them, and each is a place a plausible-looking inline
/// expression would be quietly wrong: a state word rendered as colour
/// alone, a label list whose order changes between reads, a
/// registration command that names a host the operator cannot reach,
/// and a group whose access is described by a boolean nobody can read.

/// How a runner's state reads, as a word and as a tone.
///
/// Never colour alone — `web/DESIGN.md`. The word is the fact and the
/// `Badge` variant is the glance, and the three states are three
/// different words rather than three shades of one.
///
/// A state this bundle has never heard of prints the server's own word
/// in neutral ink, the same honesty rule `hostedStatePresentation`
/// follows: a runner quietly shown "Online" because we did not
/// recognise what the server said is a runner an operator will keep
/// sending work to.
export interface StatePresentation {
  label: string;
  variant: "good" | "warning" | "neutral";
}

/// A `Map`, not an object literal, for the reason `label-pill.tsx`
/// spells out at length: a plain `Record` inherits from
/// `Object.prototype`, so a state word of `constructor` or `toString`
/// looks up a *function*, the `??` fallback never fires, and the pill
/// renders with `label: undefined` — a blank cell where the state
/// should be. A `Map` has no prototype chain to walk.
const STATES = new Map<string, StatePresentation>([
  ["online", { label: "Online", variant: "good" }],
  ["busy", { label: "Busy", variant: "warning" }],
  ["offline", { label: "Offline", variant: "neutral" }],
]);

export function runnerStatePresentation(state: string): StatePresentation {
  return STATES.get(state) ?? { label: state, variant: "neutral" };
}

/// The labels of one runner, in an order that does not move between
/// reads.
///
/// The server composes a runner's labels as `self-hosted`, the OS, the
/// arch and then whatever custom labels the runner offered — but the
/// order it composes them in is the server's business, and a table
/// whose chips reshuffle when a runner re-registers reads as two
/// different runners. So the order is decided here: the three the
/// server always adds first, in that fixed sequence, then the custom
/// ones in the order they arrived.
///
/// Deduped, because a runner may register `--labels linux,gpu` on a
/// linux host and the server's own `linux` would then appear twice —
/// two identical chips side by side, which reads as a rendering bug.
export function orderedLabels(
  labels: readonly string[],
  os?: string,
  arch?: string,
): string[] {
  const rest = labels.filter(
    (l) => l !== "self-hosted" && l !== os && l !== arch,
  );
  const front = ["self-hosted", os, arch].filter(
    (l): l is string => !!l && labels.includes(l),
  );
  return [...new Set([...front, ...rest])];
}

/// How long a freshly minted registration token lives, in the words the
/// panel says it in.
///
/// One hour, from the contract. Stated as a duration rather than as a
/// clock time on purpose: an operator is about to paste two commands
/// into a terminal on another machine, and "expires in 60 minutes" is
/// what tells them whether they have time to go and find that machine.
export const TOKEN_EXPIRY_NOTE = "This token expires in 60 minutes.";

/// The two commands that turn a machine into a runner.
///
/// The URL is **this browser's own origin**, not the `command` string
/// the server composes from `STRATUM_PUBLIC_URL`. They are usually the
/// same and when they are not, the browser's is the one that is
/// demonstrably reachable: it is the address the operator is reading
/// this page at. A deployment whose `STRATUM_PUBLIC_URL` is stale would
/// otherwise hand out a command that fails with a connection error and
/// nothing to explain it.
///
/// The trailing slash is stripped so the command never reads
/// `--url https://stratum.test/`, which works and looks like a typo.
export function registrationCommands(origin: string, token: string): string {
  const url = origin.replace(/\/+$/, "");
  return `weft-runner register --url ${url} --token ${token}\nweft-runner run`;
}

/// What a group admits, in one line.
///
/// `repo_access` and `allow_public` are two booleans-in-a-trenchcoat
/// that together decide whether a job may run, and a table that printed
/// them as "selected" and a tick left the reader to compose the rule
/// themselves. The sentence composes it for them.
export function groupAccessLine(
  group: Pick<RunnerGroup, "repo_access" | "allow_public" | "repos">,
): string {
  const which =
    group.repo_access === "all"
      ? "Every repository"
      : group.repos.length === 1
        ? "1 repository"
        : `${group.repos.length} repositories`;
  return `${which} · ${
    group.allow_public
      ? "public repositories allowed"
      : "private repositories only"
  }`;
}

/// The job a busy runner is holding, as a link — when there is one to
/// build.
///
/// The run page is addressed `/{owner}/{repo}/checks/runs/{run}`, so a
/// job that does not name its repository cannot be linked to, only
/// named. Returning `null` rather than guessing a path is deliberate: a
/// link to `/acme/undefined/checks/runs/…` is worse than plain text,
/// because it looks like it works.
export function runnerJobHref(
  org: string,
  job: NonNullable<Runner["job"]>,
): string | null {
  if (!job.repo) return null;
  return `/${encodeURIComponent(org)}/${encodeURIComponent(job.repo)}/checks/runs/${encodeURIComponent(job.run_id)}`;
}
