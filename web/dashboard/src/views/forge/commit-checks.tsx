/// What CI said about **one commit**, for the two places that ask: a
/// glyph beside a row in the commit list, and a strip on the commit page.
///
/// `GET …/commits/{sha}/checks` has existed since the Checks tab landed,
/// carries a comment saying it is "what a commit page needs", and had no
/// caller at all — so a reader looking at a commit could not tell a green
/// build from a red one from a project with no CI, while the answer sat
/// one request away. This is that caller.
///
/// The rollup rules are the change panel's, deliberately: one repository
/// must not have two different ideas of what "passing" means. They live
/// here rather than being imported wholesale because the question is
/// narrower — a commit has no *required* set, since requirements are a
/// property of the branch a change lands on, and a commit is not landing
/// anywhere.
import { useEffect, useState } from "react";
import { api, type CheckRun, type Session } from "@/api";
import { classifyCheck, type CheckOutcome } from "@/views/forge/change-checks";
import {
  BLOCKED_PRESENTATION,
  runStatePresentation,
} from "@/views/forge/checks";
import { refusalsByRunId, rowRefusal } from "@/lib/workflow-runs";
import { PROSE_LINK } from "@/lib/links";
import { DetailLink } from "@/components/detail-link";

/// The single state a commit's checks add up to.
///
/// `null` for "nothing has reported", which is **not** an outcome: a
/// commit with no checks is not neutral, or pending, or anything else —
/// it is a commit nobody built, and the only honest rendering is no
/// rendering. Collapsing it into a grey glyph would put a mark beside
/// every commit in every repository that has no CI at all, which is most
/// of them.
export function rollup(
  // `{ state: string }` and not `Pick<CheckRun, "state">`, whose `state`
  // is the narrow union of the six words we know today. The whole reason
  // `classifyCheck` has a default arm is that a provider added after this
  // bundle shipped writes a seventh, and a signature that cannot express
  // one makes that arm untestable — and quietly asserts on the wire a
  // guarantee the server does not give.
  runs: readonly { state: string }[],
): CheckOutcome | null {
  if (runs.length === 0) return null;
  let pending = false;
  let neutral = 0;
  for (const r of runs) {
    const o = classifyCheck(r.state);
    // Any failure decides it — one red out of two hundred green is a red
    // commit, the same precedence the change panel's headline uses.
    if (o === "failing") return "failing";
    if (o === "pending") pending = true;
    if (o === "neutral") neutral += 1;
  }
  if (pending) return "pending";
  return neutral === runs.length ? "neutral" : "passing";
}

/// The sentence a glyph carries for a screen reader, and a mouse's title.
///
/// Never colour alone — `web/DESIGN.md`, and the reason the change panel
/// prints every state as a word. In the commit *list* there is no room
/// for a word, so the glyph carries its meaning in text that assistive
/// technology reads and a hover reveals.
export function rollupLabel(outcome: CheckOutcome, count: number): string {
  const checks = `${count} ${count === 1 ? "check" : "checks"}`;
  switch (outcome) {
    case "passing":
      return `All ${checks} passed`;
    case "failing":
      return `Some of ${checks} did not pass`;
    case "pending":
      return `${checks}, some still running`;
    case "neutral":
      return `${checks}, all skipped`;
  }
}

export const GLYPH: Record<CheckOutcome, { mark: string; tone: string }> = {
  passing: { mark: "✓", tone: "text-good" },
  failing: { mark: "✕", tone: "text-serious" },
  pending: { mark: "◌", tone: "text-warning" },
  neutral: { mark: "⊘", tone: "text-ink-3" },
};

/// How one row of the strip reads, once the workflow runs have been
/// joined back on.
///
/// The refusal wins over the stored word, and that is the whole point:
/// a refused run is mirrored as `queued` (`workflow/mirror.rs`), so this
/// strip said "Queued" for a build that will never start, with the
/// reason sitting on the run where nothing here looked. The commit page
/// is where that gets noticed — a branch push has no change to read
/// instead, so "why did my build not start" is asked on this screen.
///
/// Pure, and separate from the component, because every interesting case
/// is a mapping question: which word wins, which mark goes with it, and
/// what happens to a state this bundle has never heard of.
export function stripPresentation(
  state: string,
  refusal: string | null,
): { mark: string; tone: string; label: string } {
  if (refusal !== null)
    return {
      // Not `pending`'s ◌. The glyph is the fastest thing on the row and
      // a blocked run is not a run that is about to start.
      mark: "⏸",
      tone: BLOCKED_PRESENTATION.className,
      label: BLOCKED_PRESENTATION.label,
    };
  const pres = runStatePresentation(state);
  return {
    mark: GLYPH[classifyCheck(state)].mark,
    tone: pres.className,
    label: pres.label,
  };
}

/// One commit's checks, fetched on mount.
///
/// A failure is `[]` and not an error box. This is garnish on a page that
/// works without it — the same rule the change page applies to viewed
/// marks and associations — and a commit whose checks could not be read
/// must not take the diff down with it.
function useCommitChecks(session: Session, repo: string, sha: string) {
  const [runs, setRuns] = useState<CheckRun[] | null>(null);
  // Depends on what is *inside* the session, not on the object.
  //
  // `viewerSession()` builds a fresh `Session` on every call, so an
  // effect keyed on the object re-runs on every render, sets state, and
  // re-renders — a loop that freezes the tab rather than failing, and one
  // that has already frozen a forge page once. Callers are asked to hold
  // the object still with `useMemo` and the forge does; but this hook is
  // mounted once per row of a commit list, so it is the last place that
  // should be trusting a caller to have remembered. `WatchButton` guards
  // itself the same way.
  const { org, token } = session;
  useEffect(() => {
    let alive = true;
    setRuns(null);
    api
      .commitChecks({ org, token }, repo, sha)
      .then((r) => alive && setRuns(r))
      .catch(() => alive && setRuns([]));
    return () => {
      alive = false;
    };
  }, [org, token, repo, sha]);
  return runs;
}

/// The refusals among this commit's workflow runs, by run id.
///
/// A second request, and garnish like the checks themselves: a repository
/// whose runs cannot be read still gets its strip, minus the sentences.
///
/// Keyed by the run's own id rather than filtered by commit, and that is
/// deliberate — `(repo_id, provider, external_id)` is the atomic key the
/// mirror upserts on, so an id names exactly one row and a run belonging
/// to another commit cannot annotate this one whatever the server sends
/// back. The `commitSha` below is a request for less data, not the thing
/// keeping the join honest.
function useCommitRefusals(session: Session, repo: string, sha: string) {
  const [refusals, setRefusals] = useState<ReadonlyMap<string, string>>(
    new Map(),
  );
  const { org, token } = session;
  useEffect(() => {
    let alive = true;
    setRefusals(new Map());
    api
      .workflowRuns({ org, token }, repo, { limit: 100, commitSha: sha })
      .then((rs) => alive && setRefusals(refusalsByRunId(rs)))
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [org, token, repo, sha]);
  return refusals;
}

/// The glyph for a commit-list row. Renders nothing at all until the
/// answer arrives, and nothing afterwards if nothing reported — a row
/// that briefly shows a placeholder and then loses it is a row that
/// flickers on every page of history.
export function CommitCheckGlyph(props: {
  session: Session;
  repo: string;
  sha: string;
}) {
  const runs = useCommitChecks(props.session, props.repo, props.sha);
  if (runs === null || runs.length === 0) return null;
  const outcome = rollup(runs);
  if (outcome === null) return null;
  const g = GLYPH[outcome];
  const label = rollupLabel(outcome, runs.length);
  return (
    <span className={`text-xs ${g.tone}`} title={label}>
      <span aria-hidden>{g.mark}</span>
      <span className="sr-only">{label}</span>
    </span>
  );
}

/// The strip on a commit page: one row per workflow, each one click from
/// the provider's own log.
///
/// Absent, not empty, when nothing reported. "No checks" on a commit page
/// is a claim about somebody's CI that we cannot make — the project may
/// build somewhere that has never been pointed at us — and the Checks tab
/// is where that question is answered properly, with the five distinct
/// reasons a list can be empty.
export function CommitChecks(props: {
  session: Session;
  repo: string;
  sha: string;
  /// How a row whose `detail_url` is one of our own pages navigates.
  /// Every one of these used to be somebody else's site; a workflow run's
  /// is not. See `DetailLink`.
  navigate: (to: string, replace?: boolean) => void;
}) {
  const runs = useCommitChecks(props.session, props.repo, props.sha);
  const refusals = useCommitRefusals(props.session, props.repo, props.sha);
  if (runs === null || runs.length === 0) return null;
  const outcome = rollup(runs);
  if (outcome === null) return null;
  const g = GLYPH[outcome];

  return (
    <div className="mt-4 overflow-hidden rounded-lg border border-borderline">
      <div className="flex items-center gap-2 border-b border-borderline bg-surface-2 px-4 py-2 text-sm">
        <span aria-hidden className={g.tone}>
          {g.mark}
        </span>
        <span className="font-medium text-ink">
          {rollupLabel(outcome, runs.length)}
        </span>
      </div>
      <ul>
        {runs.map((r) => {
          const refusal = rowRefusal(r, refusals);
          const pres = stripPresentation(r.state, refusal);
          return (
            <li
              key={r.id}
              className="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-borderline/60 px-4 py-2 text-sm last:border-0"
            >
              <span aria-hidden className={pres.tone}>
                {pres.mark}
              </span>
              <span className="font-mono text-xs text-ink">{r.name}</span>
              {/* The state as a word as well as a glyph. */}
              <span className={`text-xs ${pres.tone}`}>{pres.label}</span>
              <span className="text-xs text-ink-3">{r.provider}</span>
              {r.detail_url && (
                <DetailLink
                  className={`ml-auto text-xs ${PROSE_LINK}`}
                  href={r.detail_url}
                  provider={r.provider}
                  navigate={props.navigate}
                  // Somebody else's build opens where it always has, in
                  // a new tab. Our own run page is a route in this app
                  // and stays in this one.
                  newTab
                >
                  Details
                </DetailLink>
              )}
              {/* Beneath, on its own line — `basis-full` so it does not
                  compete with the row for width. Verbatim, wrapped, never
                  truncated: this sentence is the entire answer to "why
                  did my build not start". */}
              {refusal !== null && (
                <p className="basis-full whitespace-pre-wrap break-words text-xs text-ink-2">
                  {refusal}
                </p>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}
