/// Joining a workflow run back onto the check rows it produced.
///
/// A `.weft/*.yml` workflow run mirrors into `check_runs` so that a
/// project on the organization's own runners reads exactly like a
/// project on Buildkite. That is the right default and it loses one
/// thing: a run that was **refused** — a fork awaiting approval, say —
/// mirrors as `queued`, because nothing is wrong with the change and a
/// red row would tell its author to go and fix code that is fine
/// (`workflow/mirror.rs`).
///
/// The cost is a reader waiting forever for a check that will never
/// start, with the reason sitting on the run where nothing on the page
/// looks. The row says "Queued"; the run says "this change comes from a
/// fork; a maintainer has to approve its workflows before they run".
///
/// So the pages that list checks fetch the workflow runs as well and join
/// them here. The join is two-keyed because the mirror writes two shapes
/// of row and they are addressed differently:
///
///   * a **refusal** row is named for the workflow *file* and carries
///     the run's own id in `external_id`;
///   * a **job** row is named `"<workflow> / <cell>"` and carries the
///     job's id.
///
/// A refused run has no jobs, so only the first shape can ever be
/// blocked — but the change route does not expose `external_id` at all,
/// so that side has to match on the name. Both are here, both are pure,
/// and neither reads the reason's prose.
import type { CheckRun, WorkflowRun } from "@/api";

/// `check_runs.provider` for a row mirrored from a workflow run.
export const WORKFLOW_PROVIDER = "weft";

/// What a blocked run says when the server sent no sentence with it.
///
/// It should not happen — every `settle` call passes a reason — but a
/// row rendered as "Blocked" with nothing beside it is the exact defect
/// this module exists to remove, and an empty string would recreate it.
export const UNEXPLAINED_BLOCK =
  "This run is blocked, and the server gave no reason.";

/// The reason a run is blocked, never blank.
///
/// The server's own sentence when it sent one — verbatim, because a
/// paraphrase is a reason nobody gave.
export function blockReason(run: Pick<WorkflowRun, "error">): string {
  const text = (run.error ?? "").trim();
  return text !== "" ? text : UNEXPLAINED_BLOCK;
}

/// The blocked runs, optionally only those for one commit.
///
/// The sha filter is what makes "blocked at the current tip" answerable:
/// approval is per tip, so a run blocked against the patchset before
/// this one is somebody else's question and must not put a button on
/// this page.
export function blockedRuns(
  runs: readonly WorkflowRun[],
  commitSha?: string,
): WorkflowRun[] {
  return runs.filter(
    (r) =>
      r.state === "blocked" &&
      (commitSha === undefined || r.commit_sha === commitSha),
  );
}

/// The blocked runs at this tip that approving would actually start.
///
/// Only a fork's. Every refusal arrives as one state, and only a fork's
/// run is waiting on the maintainer reading the page. Offering a button
/// for any other reason offers a control that cannot work — pressing it
/// triggers another run, which blocks again with the same sentence, and
/// the server answers 409 to somebody who was given every reason to
/// expect otherwise.
///
/// Decided by `blocked_reason`, never by reading the sentence. The
/// prose is written to be read by people and will be reworded; the code
/// is the contract.
///
/// **Absent is treated as a fork**, which is deliberate and is not the
/// same as guessing: before the field existed the only blocked run that
/// ever got a button was the fork case, so a bundle talking to an older
/// server keeps exactly the behaviour it had rather than silently
/// dropping approval for every fork on the fleet.
export function approvableRuns(
  runs: readonly WorkflowRun[],
  commitSha: string,
): WorkflowRun[] {
  return blockedRuns(runs, commitSha).filter(
    (r) => r.blocked_reason === undefined || r.blocked_reason === "fork",
  );
}

/// Blocked runs by their own id, for a row that carries `external_id`.
export function refusalsByRunId(
  runs: readonly WorkflowRun[],
): Map<string, string> {
  return new Map(blockedRuns(runs).map((r) => [r.id, blockReason(r)]));
}

/// Blocked runs by workflow file, for a row that carries only a name.
///
/// Scoped to a commit on purpose: the file path is not unique across
/// history, and a stale run from an earlier patchset would otherwise
/// annotate the current one's row with a refusal that has been dealt
/// with.
export function refusalsByFile(
  runs: readonly WorkflowRun[],
  commitSha: string,
): Map<string, string> {
  return new Map(
    blockedRuns(runs, commitSha).map((r) => [r.file, blockReason(r)]),
  );
}

/// The refusal on one check row, or `null` if it is not a blocked
/// workflow run.
///
/// Provider-gated before anything else. A third party can post a check
/// row named after one of our workflow files, and without this it would
/// borrow that run's refusal and display it as its own.
export function rowRefusal(
  row: Pick<CheckRun, "provider" | "external_id">,
  byRunId: ReadonlyMap<string, string>,
): string | null {
  if (row.provider !== WORKFLOW_PROVIDER) return null;
  if (row.external_id === null) return null;
  return byRunId.get(row.external_id) ?? null;
}

/// The refusal on one change-panel row, which has no `external_id`.
///
/// `posted_by` is the provider for a commit-scoped row, so it is the
/// same gate one field over. A patchset-scoped row is somebody's intake
/// posting and never a workflow run, whatever it calls itself.
export function panelRowRefusal(
  row: { name: string; source?: string | null; posted_by?: string | null },
  byFile: ReadonlyMap<string, string>,
): string | null {
  if (row.posted_by !== WORKFLOW_PROVIDER) return null;
  if (
    row.source !== undefined &&
    row.source !== null &&
    row.source !== "commit"
  )
    return null;
  return byFile.get(row.name) ?? null;
}
