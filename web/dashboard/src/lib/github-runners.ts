import type { GithubInstallation, GithubJob, GithubRunnerSize } from "@/api";

/// The decisions the GitHub runners panel makes, kept out of the view
/// so they can be tested without a browser.
///
/// Three of them: what an installation's card should say, what one
/// job's row should say, and the `runs-on:` line a workflow author
/// copies. Each is a place an inline expression would be quietly
/// wrong — an installation whose detail could not be fetched rendered
/// as "ready", a running job with no minutes shown, a snippet that
/// spells the label from a string the page typed rather than the one
/// the server sent.

/// What the panel says about one installation.
///
/// - `ready`: both permissions held; jobs can run.
/// - `approve`: at least one missing, with GitHub's page for approving
///   it and the names of what is missing, in the order GitHub lists
///   them.
/// - `gone`: the App has been uninstalled on GitHub; the row we hold is
///   a memory.
/// - `unknown`: GitHub could not be asked right now (`detail: null`),
///   or the server predates the detail. **Never** rendered as ready:
///   a card that says "can use Weft runners" on the strength of not
///   having checked is how the first job gets refused for a permission
///   the page said was there.
/// - `none`: nothing is connected at all — the list was empty.
export type InstallationStatus =
  | { kind: "ready" }
  | { kind: "approve"; url: string; missing: string[] }
  | { kind: "gone" }
  | { kind: "unknown" }
  | { kind: "none" };

export const ADMINISTRATION_WRITE = "Administration: write";
export const ACTIONS_WRITE = "Actions: write";

export function installationStatus(
  inst: GithubInstallation | null | undefined,
): InstallationStatus {
  if (!inst) return { kind: "none" };
  const d = inst.detail;
  if (d === null || d === undefined) return { kind: "unknown" };
  if ("gone" in d) return { kind: "gone" };
  const missing: string[] = [];
  if (!d.administration_write) missing.push(ADMINISTRATION_WRITE);
  if (!d.actions_write) missing.push(ACTIONS_WRITE);
  // `runners_ready` is the server's conjunction and the two flags are
  // its inputs; when they disagree the flags win, because they are the
  // ones a person is about to go and approve.
  if (missing.length === 0) return { kind: "ready" };
  return { kind: "approve", url: d.approve_url, missing };
}

/// The sentence the ready card says. The account is GitHub's name for
/// where the App is installed, not ours for the organisation.
export function readyLine(account: string): string {
  return `GitHub Actions on ${account} can use Weft runners`;
}

/// "Approve Administration: write and Actions: write on GitHub", or
/// just the one that is missing.
export function approveLine(missing: readonly string[]): string {
  return `Approve ${missing.join(" and ")} on GitHub`;
}

/// What happened to a job that never reached a runner, as its row says.
export const ABANDONED_LINE =
  "abandoned — no job reached this runner before it was stopped; nothing was billed";

/// What the row says about a refused job GitHub is still holding.
export const STILL_QUEUED_NOTE = "still queued on GitHub — cancel it there";

/// One line per job: the state word first, then the one fact that
/// matters for that state, separated the way the rest of the dashboard
/// separates a state from its detail.
///
/// A refusal or a failure is the server's own sentence after a dash; a
/// duration is after a middle dot. Minutes are shown for anything that
/// has been on a runner, including a job still running — "running · 3
/// min" is the figure being spent right now, and a row that showed
/// minutes only once the job was over would tell a person watching a
/// runaway job nothing until it was too late to matter.
export function jobLine(job: GithubJob): string {
  const mins = minutesText(job.minutes);
  switch (job.state) {
    case "refused":
      return `refused — ${job.refusal ?? "no reason was recorded"}`;
    case "queued":
      return "queued — waiting for a runner";
    case "launching":
      return "launching a runner";
    case "running":
      return `running · ${mins}`;
    case "completed":
      return `completed · ${job.conclusion ?? "no conclusion"} · ${mins}`;
    case "failed":
      return `failed — ${job.error ?? job.conclusion ?? "no reason was recorded"}`;
    case "abandoned":
      return ABANDONED_LINE;
  }
  // A state this bundle has never heard of prints the server's own
  // word, the same honesty rule `runnerStatePresentation` follows.
  return String(job.state);
}

function minutesText(minutes: number): string {
  return `${minutes.toLocaleString("en-US")} min`;
}

/// The `runs-on:` line for one size. The label is the server's, never
/// retyped here: the intake matches on it, and a snippet that spelled
/// it differently would be a snippet that gets every job refused.
export function snippet(size: Pick<GithubRunnerSize, "label">): string {
  return `runs-on: ${size.label}`;
}

/// One size, described: "weft — 1 vCPU, 2 GB, 1× minutes".
///
/// `cpu` arrives in Fargate units and `memory_mib` in MiB — the shape
/// the task override is written in — so the arithmetic to the figures a
/// person recognises happens here, once. Fractions are shown only when
/// there are any: "1 vCPU", not "1.0 vCPU".
export function sizeLine(size: GithubRunnerSize): string {
  const vcpu = trim(size.cpu / 1024);
  const gb = trim(size.memory_mib / 1024);
  return `${size.label} — ${vcpu} vCPU, ${gb} GB, ${size.multiplier}× minutes`;
}

function trim(n: number): string {
  return Number.isInteger(n) ? String(n) : n.toFixed(2).replace(/0+$/, "");
}
