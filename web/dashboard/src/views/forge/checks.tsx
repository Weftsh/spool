import {
  useCallback,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import {
  Check,
  CircleDashed,
  CircleSlash,
  Clock,
  GitBranch,
  Loader,
  PauseCircle,
  SkipForward,
  X,
  type LucideIcon,
} from "lucide-react";
import { toast } from "sonner";
import {
  api,
  viewerSession,
  type CheckRun,
  type ChecksPoll,
  type RunQuery,
  type RunState,
  type Session,
} from "@/api";
import { RelativeTime } from "@/components/relative-time";
import { DetailLink } from "@/components/detail-link";
import { refusalsByRunId, rowRefusal } from "@/lib/workflow-runs";
import { Badge } from "@/components/ui/badge";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Err, Loading } from "@/components/feedback";
import { FOCUS_RING, guide, PROSE_LINK, STRUCTURAL_LINK } from "@/lib/links";
import { cn } from "@/lib/utils";
import { href, useQuery } from "@/router";

/// The Checks tab: every verdict about this repository's commits,
/// whoever reached it.
///
/// **This page is a summary and never a control.** There is no re-run
/// button, no artifact list and no "cancel" on a row here, because most
/// rows describe somebody else's build system and a button that cannot
/// do what it says is worse than an absent one. What a row has is a
/// verdict, enough context to know which build it was about, and a link
/// to where the log actually lives.
///
/// That link now goes to two kinds of place. A GitHub Actions or
/// Buildkite row leaves for their site; a row mirroring a **workflow**
/// run — one of this repository's own `.weft/*.yml` workflows, executed
/// on the organization's runners — points at our own run page, which does have a
/// log and a cancel because that run is ours to hold and to stop. The
/// row itself does not branch on which: `DetailLink` reads the URL, so
/// the day there is a fourth provider nothing here needs a fourth arm.
///
/// Two structural notes:
///
/// - The `workflows` list rides along with **every** page of runs, so the
///   left rail costs no second request. Drawing it from the runs on
///   screen would be worse than wrong: a workflow that last ran a month
///   ago would drop out of its own rail as soon as somebody filtered.
/// - Filters live in the query string, exactly as the issues index does
///   it, so a filtered list is a link somebody can send. "the arm64 build
///   has been red since Tuesday" should be a URL, not a description of
///   which dropdowns to set.
///
/// The tab is **Checks**, not Actions. Actions is one competitor's
/// product; this is the noticeboard every result lands on, whether it
/// came from theirs, from a third party posting to the intake, or from
/// our own runners. Naming it after one of the three would misdescribe
/// the other two.

// ---------------------------------------------------------------------------
// The pure logic, unit-tested in `checks.test.ts`.
// ---------------------------------------------------------------------------

/// The filter set, which is also the query string.
///
/// `state` is a plain string rather than [`RunState`] on purpose. The
/// server refuses an unrecognised state **by name** — the openapi text
/// spells out why: a filter silently dropped shows rows that do not match
/// what was asked for, which reads as the filter being broken rather than
/// as the word being wrong. So a hand-typed `?state=success` is carried
/// through to the server and the server's own sentence is what the reader
/// sees. Narrowing it here would turn that into a silently unfiltered
/// list.
export interface RunFilter {
  workflow: string | null;
  branch: string | null;
  state: string | null;
  event: string | null;
  actor: string | null;
}

export const NO_FILTER: RunFilter = {
  workflow: null,
  branch: null,
  state: null,
  event: null,
  actor: null,
};

/// The query-string keys, in the order a formatted URL spells them.
///
/// One list, read by both directions, because a parser and a formatter
/// that each carry their own copy of the key set are two places for a
/// filter to go missing — and it goes missing in only one of the two,
/// which is how a dropdown and the URL it produced end up disagreeing.
const KEYS = ["workflow", "branch", "state", "event", "actor"] as const;

/// Read the address bar into a filter.
///
/// Whitespace-only is absence: `?branch=%20` is somebody's stray
/// keystroke, not a branch named space, and sending it would filter the
/// list down to nothing with no visible reason.
export function parseRunQuery(params: URLSearchParams): RunFilter {
  const read = (k: string) => {
    const v = (params.get(k) ?? "").trim();
    return v === "" ? null : v;
  };
  return {
    workflow: read("workflow"),
    branch: read("branch"),
    state: read("state"),
    event: read("event"),
    actor: read("actor"),
  };
}

/// The same filter as query-string parameters, for [`href`].
///
/// Deliberately **not** including `limit` or `before`. A cursor in a
/// shared link points into a page that has moved since — the next push
/// puts a new run in front of it — so the link would open somewhere its
/// sender never saw. Paging is state this page holds; filtering is state
/// the URL holds.
export function runQueryParams(
  f: RunFilter,
): Record<string, string | undefined> {
  const out: Record<string, string | undefined> = {};
  for (const k of KEYS) out[k] = f[k] ?? undefined;
  return out;
}

/// The query string itself, without its `?`. Empty for no filter at all.
export function formatRunQuery(f: RunFilter): string {
  const p = new URLSearchParams();
  for (const k of KEYS) {
    const v = f[k];
    if (v) p.set(k, v);
  }
  return p.toString();
}

/// What the API is actually asked for.
///
/// `state` is cast rather than validated — see [`RunFilter`]. The cast is
/// the honest spelling of "the server owns this vocabulary", and it is
/// confined to this one function so there is exactly one place to look.
export function toApiQuery(
  f: RunFilter,
  page: { limit: number; before?: number },
): RunQuery {
  const q: RunQuery = { limit: page.limit };
  if (f.workflow) q.workflow = f.workflow;
  if (f.branch) q.branch = f.branch;
  if (f.state) q.state = f.state as RunState;
  if (f.event) q.event = f.event;
  if (f.actor) q.actor = f.actor;
  if (page.before !== undefined) q.before = page.before;
  return q;
}

/// How a run state reads, as a word and as a shape.
///
/// **Never colour alone**, and this is the page where that rule earns its
/// keep: "did the build pass" is drawn on exactly the red/green axis that
/// 8% of men cannot resolve, and it is the single most consequential bit
/// on the screen. So every state differs in *glyph shape* as well as
/// tone, and the word is always reachable — rendered beside the icon in
/// the row, and as the icon's accessible name wherever it is not.
///
/// `cancelled` and `skipped` are neutral, not red. Neither is a failure:
/// one is a build a person stopped, the other one a provider decided did
/// not apply. Colouring them red would make a docs-only change look like
/// a broken one.
///
/// This table lives here rather than in `components/state-icon.tsx`
/// because that component's check states are the *aggregate* three — its
/// labels read "All checks passed", which is a sentence about a commit
/// and not about the one run this page draws a row for. See the note in
/// the handover: the right home for six `run-*` states is that file.
export const RUN_STATES: Record<
  RunState,
  { label: string; icon: LucideIcon; className: string }
> = {
  queued: { label: "Queued", icon: Clock, className: "text-ink-3" },
  running: { label: "Running", icon: Loader, className: "text-warning" },
  passing: { label: "Passing", icon: Check, className: "text-good" },
  failing: { label: "Failing", icon: X, className: "text-serious" },
  cancelled: { label: "Cancelled", icon: CircleSlash, className: "text-ink-3" },
  skipped: { label: "Skipped", icon: SkipForward, className: "text-ink-3" },
};

/// A workflow run that is waiting on a **person**, not on a machine.
///
/// Not in `RUN_STATES` because it is not one of the six words a check
/// row can hold: the mirror writes a blocked run as `queued`, on purpose
/// — nothing is wrong with the change, and a red row would tell its
/// author to go and fix code that is fine. So this presentation is
/// reached by the pages that have joined the run back on and *know* the
/// row is blocked, and by the run page itself.
///
/// One definition, three callers: the Checks tab, the change panel and
/// the run page. Two of them drew their own before this existed, which
/// is how a product ends up with two ideas of what blocked looks like.
///
/// Warning and not `serious`: a blocked run has not failed.
export const BLOCKED_PRESENTATION: {
  label: string;
  icon: LucideIcon;
  className: string;
} = { label: "Blocked", icon: PauseCircle, className: "text-warning" };

/// The word for a state, including one this bundle has never heard of.
///
/// A seventh state means the server is newer than the page. It renders as
/// its own literal text in neutral ink and **never** as a default — a
/// verdict quietly shown green is a lie that ships broken code, and one
/// quietly shown red is a lie that blocks good code. Saying the unknown
/// word out loud is the only answer that leaves a trace of not knowing.
export function runStateLabel(state: string): string {
  return RUN_STATES[state as RunState]?.label ?? state;
}

export function runStatePresentation(state: string): {
  label: string;
  icon: LucideIcon;
  className: string;
} {
  // `Object.hasOwn`, not `RUN_STATES[state] ?? …`. A plain object
  // literal inherits from `Object.prototype`, so a state word of
  // `constructor` or `toString` returns a *function* — truthy, so the
  // `??` never fires — and the row renders `label: undefined`: a blank
  // pill and a blank accessible name where the verdict should be. The
  // fallback below exists to say the unknown word out loud, and it has
  // to actually be reachable for every unknown word.
  return Object.hasOwn(RUN_STATES, state)
    ? RUN_STATES[state as RunState]
    : { label: state, icon: CircleDashed, className: "text-ink-3" };
}

/// The seven characters of a sha a person actually reads.
///
/// Left alone if it is already shorter — the server sends full hashes,
/// but a truncating helper that silently pads or throws on a short input
/// is a helper that turns one bad row into a blank tab.
export function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

/// How long a run took, or has been taking.
///
/// `now` is a parameter rather than a call to `Date.now()` so this is
/// answerable without a clock. Four cases, and three of them are the
/// reason this is not a subtraction:
///
/// - started and finished: the elapsed time;
/// - started and still going: how long so far, flagged `running` so the
///   row can say "so far" rather than presenting an in-progress number
///   as a final one;
/// - never started: **nothing**, not zero. A queued run has no duration,
///   and `0s` beside it reads as an instant pass;
/// - finished before it started, or started in the future: also nothing.
///   Those timestamps come from somebody else's build machine and its
///   clock is not ours; a negative duration is a clock disagreement, and
///   rendering `-3s` invites a bug report about our arithmetic.
export function runDuration(
  run: { started_at: number | null; completed_at: number | null },
  now: number,
): { ms: number; running: boolean } | null {
  const { started_at, completed_at } = run;
  if (started_at === null) return null;
  const end = completed_at ?? now;
  const ms = end - started_at;
  if (ms < 0) return null;
  return { ms, running: completed_at === null };
}

/// A duration as a build reader wants it: coarse, and never zero-padded
/// into false precision.
///
/// Seconds below a minute, minutes-and-seconds below an hour, then
/// hours-and-minutes. `formatMs` in `@/format` is the dashboard's
/// millisecond formatter and is right for a request; a CI run measured in
/// `2700.0 s` is not something anybody can compare at a glance.
export function formatDuration(ms: number): string {
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  return `${Math.floor(m / 60)}h ${m % 60}m`;
}

/// The line under a run's name: which commit, from what, by whom.
///
/// GitHub's is `{workflow} #{n}: Commit {sha} {event} by {actor}` and the
/// workflow name is dropped here because it is the *title directly
/// above*. GitHub can afford the repetition because its title line is the
/// commit message; a `CheckRun` carries no commit message, so our title
/// is the workflow name and printing it twice would be the whole row
/// saying one word.
///
/// Every part is optional on the wire and each is dropped rather than
/// rendered as a placeholder. A provider that does not number its runs
/// gets a line about a commit; `#null` or `by unknown` would be us
/// inventing a fact about somebody else's build.
export function runSubtitle(run: {
  commit_sha: string;
  run_number: number | null;
  event: string | null;
  actor: string | null;
}): string {
  const head = run.run_number === null ? "" : `#${run.run_number}: `;
  const tail = [
    `Commit ${shortSha(run.commit_sha)}`,
    run.event ?? "",
    run.actor ? `by ${run.actor}` : "",
  ]
    .filter(Boolean)
    .join(" ");
  return `${head}${tail}`;
}

/// The cursor for the next page, or `null` when this was the last one.
///
/// **A short page is the last page**, whatever the cursor says. Trusting
/// `next_before` alone leaves "Load more" on screen forever at the end of
/// a feed, and every press answers with nothing — the same rule the audit
/// trail settled, and for the same reason.
export function nextCursor(
  page: { runs: unknown[]; next_before: number | null },
  limit: number,
): number | null {
  if (page.runs.length < limit) return null;
  return page.next_before;
}

/// The distinct values of one field across the runs on screen, sorted.
///
/// The filter dropdowns' options. These are **what is on this page**, not
/// what exists — the server offers no facet endpoint, and inventing one
/// client-side would need the whole history. The current filter value is
/// added back by the dropdown itself, so a filter that matches nothing on
/// screen still shows as set rather than silently resetting itself and
/// disagreeing with the URL.
export function distinct(
  runs: CheckRun[],
  field: "event" | "actor" | "ref_name",
): string[] {
  const seen = new Set<string>();
  for (const r of runs) {
    const v = r[field];
    if (v) seen.add(v);
  }
  return [...seen].sort();
}

/// How many pages of runs to ask for at a time. Well under the server's
/// clamp of 100: this is a tab somebody glances at, and the cursor is
/// there for the person who wants the history.
export const PAGE_LIMIT = 30;

/// Which of the empty states this repository is actually in.
///
/// An empty list is the one CI failure that renders identically to
/// success, and there are **four** different truths behind it. Telling
/// them apart is the entire reason this page reads `/ci/poll` at all:
///
/// - `denied` — connected to GitHub, and the App installation may not
///   read Actions. Saying nothing here tells a maintainer their CI is
///   not set up when the truth is that we are not allowed to look, which
///   is false in the direction that makes them close the tab.
/// - `waiting` — connected, and no poll has finished yet. We have not
///   looked, which is not the same as having looked and found nothing.
/// - `polled` — connected, allowed, and GitHub genuinely has no runs.
/// - `intake` — no GitHub origin at all. The ordinary case for a native
///   repository, and the one where "point your CI at the intake" is the
///   right answer rather than a non-sequitur.
///
/// `unknown` is the fifth and is not one of the four: the poll status
/// itself could not be read. It says so instead of guessing, because
/// every one of the four sentences above would be an assertion we cannot
/// support.
export type EmptyKind = "denied" | "waiting" | "polled" | "intake" | "unknown";

export interface EmptyState {
  kind: EmptyKind;
  title: string;
  body: string;
  /// Whether the panel shows the intake address. Only where posting a
  /// verdict is actually the next step — offering it to somebody whose
  /// GitHub Actions we simply may not read is answering a question they
  /// did not ask.
  intake: boolean;
  /// Whether the panel offers the re-approve control. Gated again at the
  /// call site on whether the viewer could use it: a reader without
  /// admin can see *why* the list is empty and cannot fix it, and a
  /// control that only 403s is worse than none.
  reapprove: boolean;
}

export function emptyState(poll: ChecksPoll | null): EmptyState {
  if (poll === null) {
    return {
      kind: "unknown",
      title: "No runs to show",
      body: "We could not find out why this list is empty — the poll status could not be read. This is not a statement about whether the project has CI.",
      intake: true,
      reapprove: false,
    };
  }
  if (!poll.connected) {
    return {
      kind: "intake",
      title: "Nothing has reported a check for this repository yet",
      body: "Weft does not run your builds — it collects the verdicts from whatever already does.",
      intake: true,
      reapprove: false,
    };
  }
  if (poll.denied) {
    return {
      kind: "denied",
      title: "We cannot read this project's checks",
      body: "The GitHub App installation this repository came through does not have permission to read Actions, so its runs are not visible here. This is not a project without CI — approving the permission brings its history with it.",
      intake: false,
      reapprove: true,
    };
  }
  if (!poll.polled) {
    return {
      kind: "waiting",
      title: "Looking for runs",
      body: "This repository is connected to GitHub and its first poll has not finished. Nothing here yet means we have not looked, not that there is nothing to find.",
      intake: false,
      reapprove: false,
    };
  }
  return {
    kind: "polled",
    title: "GitHub reported no runs",
    body: "We polled this repository's Actions and GitHub has no runs to report. A project whose CI runs somewhere else can post its verdicts here instead.",
    intake: true,
    reapprove: false,
  };
}

/// "Retrying in 5m", when the server said when.
///
/// `null` for no answer and for one that has already come due — a
/// countdown reading "Retrying in 0s" that then sits there is a worse
/// statement than silence, because it claims a precision about somebody
/// else's scheduler that this page does not have.
export function retryLine(retryInMs: number | null): string | null {
  if (retryInMs === null || retryInMs <= 0) return null;
  return `Retrying in ${formatDuration(retryInMs)}`;
}

// ---------------------------------------------------------------------------
// The tab.
// ---------------------------------------------------------------------------

export interface ChecksProps {
  owner: string;
  repo: string;
  /// The caller's API token, when they signed in with one; empty for a
  /// cookie session.
  token: string | null;
  navigate: (to: string, replace?: boolean) => void;
  /// Whether to offer the two controls that need `repo:write` — asking
  /// GitHub for a fresh poll, and starting the App install flow to
  /// re-approve a permission.
  ///
  /// Computed by the page from the viewer's role, like the About rail's
  /// `canEdit`, and *subtractive* in the same way: getting it wrong
  /// hides a control the server would have accepted and never shows one
  /// it will refuse. Both actions are explanations of somebody else's
  /// build system, so withholding them costs a reader nothing they can
  /// see.
  canWrite?: boolean;
  /// Rendered under the list, in the main column. The repository page
  /// puts the composed changeset runs here.
  children?: ReactNode;
}

export function ChecksView(props: ChecksProps) {
  const { owner, repo, navigate } = props;
  const session = useMemo(
    () => viewerSession(owner, props.token),
    [owner, props.token],
  );
  const params = useQuery();
  const filter = useMemo(() => parseRunQuery(params), [params]);

  const [runs, setRuns] = useState<CheckRun[] | null>(null);
  const [workflows, setWorkflows] = useState<string[]>([]);
  const [older, setOlder] = useState<number | null>(null);
  const [failed, setFailed] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  /// `null` until the poll status arrives, and `null` again if it never
  /// does. `emptyState` treats that as "we do not know why this list is
  /// empty" rather than as any of the four things it could mean.
  const [poll, setPoll] = useState<ChecksPoll | null>(null);
  const [polling, setPolling] = useState(false);
  /// Refusal sentences for the workflow runs, by run id.
  ///
  /// A blocked run mirrors into this list as `queued` — on purpose, see
  /// `workflow/mirror.rs` — so without this the tab shows a row that
  /// will never move and says nothing about why. The reason lives on the
  /// run, and the run has to be asked for separately.
  const [refusals, setRefusals] = useState<ReadonlyMap<string, string>>(
    new Map(),
  );

  // Depend on the *fields*, not the filter object: `parseRunQuery` builds
  // a fresh object per render, and an effect keyed on it would fetch, set
  // state, re-render and fetch again — a loop that freezes the tab rather
  // than failing where anybody can see it.
  const { workflow, branch, state, event, actor } = filter;

  const load = useCallback(
    (before?: number) => {
      const f = { workflow, branch, state, event, actor };
      if (before === undefined) {
        setRuns(null);
        setFailed(false);
      } else {
        setLoadingMore(true);
      }
      return api
        .checkRuns(session, repo, toApiQuery(f, { limit: PAGE_LIMIT, before }))
        .then((out) => {
          setRuns((cur) =>
            before === undefined ? out.runs : [...(cur ?? []), ...out.runs],
          );
          // The rail's names come from the response every time, not from
          // the rows: a workflow whose last run is older than this page
          // must not vanish from its own rail the moment somebody pages.
          setWorkflows(out.workflows);
          setOlder(nextCursor(out, PAGE_LIMIT));
        })
        .catch(() => {
          // The tab is not the page. A refused read leaves the repository
          // and its tab strip intact and says so here, in the one column
          // that could not load.
          if (before === undefined) setFailed(true);
        })
        .finally(() => setLoadingMore(false));
    },
    [session, repo, workflow, branch, state, event, actor],
  );

  useEffect(() => {
    void load();
  }, [load]);

  const readPoll = useCallback(() => {
    let alive = true;
    api
      .checksPoll(session, repo)
      // Why the list is empty is an adornment on the list. A refused or
      // missing poll status leaves the runs on screen and leaves the
      // empty panel saying the one honest thing left — that we do not
      // know — rather than blanking the tab.
      .then((p) => alive && setPoll(p))
      .catch(() => alive && setPoll(null));
    return () => {
      alive = false;
    };
  }, [session, repo]);

  useEffect(readPoll, [readPoll]);

  /// The workflow runs, read only for the reasons blocked ones carry.
  ///
  /// Never allowed to disturb the list: an older server, or a refusal
  /// here, leaves every row exactly as it was and simply annotates none
  /// of them.
  const readRefusals = useCallback(() => {
    let alive = true;
    api
      .workflowRuns(session, repo, { limit: PAGE_LIMIT })
      .then((rs) => alive && setRefusals(refusalsByRunId(rs)))
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [session, repo]);

  useEffect(readRefusals, [readRefusals]);

  /// Ask GitHub again. **Not a re-run**: it re-reads verdicts somebody
  /// else's build system already reached, and nothing on this page can
  /// start a build. Labelling it "Re-run" would promise the one thing
  /// this product deliberately does not do.
  const recheck = async () => {
    setPolling(true);
    try {
      await api.pollChecks(session, repo);
      readPoll();
      await load();
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setPolling(false);
    }
  };

  const go = useCallback(
    (next: RunFilter) =>
      navigate(href([owner, repo, "checks"], runQueryParams(next))),
    [navigate, owner, repo],
  );

  const rows = runs ?? [];
  const canWrite = props.canWrite === true;
  const retry = retryLine(poll?.retry_in_ms ?? null);

  return (
    <div className="py-6">
      <h1 className="mb-4 border-b border-borderline pb-3 text-xl font-semibold text-ink">
        Checks
      </h1>
      {/* Two columns, collapsing to one below `lg` — the same split
          FORGE-UX §1 gives Insights, because a reader who has learned one
          rail should not have to learn a second. `min-w-0` on the main
          child is mandatory: without it a long branch name widens the
          page instead of ellipsising, and `audit()` fails the build on
          `documentElement.scrollWidth`. */}
      <div className="flex flex-col gap-6 lg:flex-row">
        <WorkflowRail
          workflows={workflows}
          active={filter.workflow}
          onPick={(w) => go({ ...filter, workflow: w })}
        />
        <div className="min-w-0 flex-1">
          {/* The server's own sentence, verbatim, and never matched on.
              `denied` is the predicate — computed server-side precisely
              so no client parses a message that will be reworded — and
              this text is here to say what went wrong beside it. It sits
              above the list rather than inside the empty panel because a
              poll that started failing on a repository with a year of
              history is exactly when somebody needs to be told. */}
          {poll?.error && (
            <div
              role="status"
              className="mb-3 rounded-lg border border-borderline bg-surface-1 p-3 text-sm"
            >
              <p className="text-ink-2">{poll.error}</p>
              {retry && <p className="mt-1 text-xs text-ink-3">{retry}</p>}
            </div>
          )}
          <FilterRow runs={rows} filter={filter} onChange={go} />
          <div className="rounded-b-lg border border-t-0 border-borderline">
            {failed && (
              <div className="p-4">
                <Err message="These runs could not be read." />
              </div>
            )}
            {!failed && runs === null && <Loading />}
            {!failed && runs !== null && rows.length === 0 && (
              <NoRuns
                owner={owner}
                repo={repo}
                filter={filter}
                poll={poll}
                session={session}
                canWrite={canWrite}
              />
            )}
            {rows.length > 0 && (
              <ul>
                {rows.map((r) => (
                  <RunRow
                    key={r.id}
                    run={r}
                    refusal={rowRefusal(r, refusals)}
                    navigate={props.navigate}
                  />
                ))}
              </ul>
            )}
          </div>
          <div className="mt-3 flex items-center gap-4">
            {older !== null && (
              <button
                type="button"
                disabled={loadingMore}
                className={cn(
                  "rounded-sm text-xs text-ink-3 underline-offset-2 hover:text-ink-2 hover:underline",
                  FOCUS_RING,
                )}
                onClick={() => void load(older)}
              >
                {loadingMore ? "Loading…" : "Load more"}
              </button>
            )}
            {/* Only for somebody who could use it, and only where there
                is a GitHub origin to ask. Its words say what it does:
                nothing here re-runs anybody's build. */}
            {canWrite && poll?.connected && (
              <button
                type="button"
                disabled={polling}
                className={cn(
                  "rounded-sm text-xs text-ink-3 underline-offset-2 hover:text-ink-2 hover:underline",
                  FOCUS_RING,
                )}
                onClick={() => void recheck()}
              >
                {polling ? "Checking…" : "Check GitHub again"}
              </button>
            )}
          </div>
          {/* What the tab's owner wants under the list — the composed
              changeset runs, which are not check rows and would otherwise
              have nowhere on this repository to appear. Passed in rather
              than imported, so this module and the run page do not have
              to import each other. */}
          {props.children}
        </div>
      </div>
    </div>
  );
}

/// The workflow rail: "All workflows", then one row per name.
///
/// FORGE-UX §1's Insights treatment, deliberately identical — 36px rows,
/// a 2px `--brand` left rule and `bg-surface-2` on the active one. Two
/// pages with rails that behave differently teach a reader that rails are
/// unpredictable.
///
/// Picking a row navigates, which puts the workflow in the query string —
/// so "the arm64 build" ends up being an address somebody can send, which
/// is the whole reason the filters live in the URL.
function WorkflowRail(props: {
  workflows: string[];
  active: string | null;
  onPick: (workflow: string | null) => void;
}) {
  if (props.workflows.length === 0) return null;
  const row = (label: string, value: string | null) => {
    const active = props.active === value;
    return (
      <li key={value ?? "*"}>
        <button
          type="button"
          aria-current={active ? "true" : undefined}
          onClick={() => props.onPick(value)}
          className={cn(
            "flex h-9 w-full min-w-0 items-center border-l-2 px-3 text-left text-sm transition",
            FOCUS_RING,
            active
              ? "border-brand bg-surface-2 font-medium text-ink"
              : "border-transparent text-ink-2 hover:text-ink",
          )}
        >
          <span className="min-w-0 truncate">{label}</span>
        </button>
      </li>
    );
  };
  return (
    <nav aria-label="Workflows" className="w-full shrink-0 lg:w-56">
      <ul className="rounded-lg border border-borderline bg-surface-1 py-1">
        {row("All workflows", null)}
        {props.workflows.map((w) => row(w, w))}
      </ul>
    </nav>
  );
}

/// State, event, actor and branch, as clearable dropdowns.
///
/// The scroller is this INNER box. `audit()` measures
/// `documentElement.scrollWidth`, so a scroller on the page container
/// fails the build while the identical one here is invisible to it —
/// the same hazard, and the same fix, as the issues index's filter row.
/// Below `md` it wraps instead, because a phone should stack rather than
/// scroll sideways.
function FilterRow(props: {
  runs: CheckRun[];
  filter: RunFilter;
  onChange: (next: RunFilter) => void;
}) {
  const { filter, onChange } = props;
  return (
    <div className="rounded-t-lg border border-borderline bg-surface-2">
      <div className="overflow-x-auto">
        <div className="flex min-w-0 flex-wrap items-center gap-2 p-3 text-sm md:flex-nowrap md:whitespace-nowrap">
          <FilterSelect
            label="State"
            value={filter.state}
            anyLabel="Any state"
            options={Object.keys(RUN_STATES)}
            format={runStateLabel}
            onChange={(state) => onChange({ ...filter, state })}
          />
          <FilterSelect
            label="Event"
            value={filter.event}
            anyLabel="Any event"
            options={distinct(props.runs, "event")}
            onChange={(event) => onChange({ ...filter, event })}
          />
          <FilterSelect
            label="Actor"
            value={filter.actor}
            anyLabel="Anyone"
            options={distinct(props.runs, "actor")}
            onChange={(actor) => onChange({ ...filter, actor })}
          />
          <FilterSelect
            label="Branch"
            value={filter.branch}
            anyLabel="Any branch"
            options={distinct(props.runs, "ref_name")}
            onChange={(branch) => onChange({ ...filter, branch })}
          />
        </div>
      </div>
    </div>
  );
}

/// A clearable filter dropdown.
///
/// Radix refuses an item whose `value` is the empty string, so "any" is
/// an explicit item with a sentinel value rather than a blank one — the
/// same rule the issues index and the audit panel follow.
const ANY = " any";

function FilterSelect(props: {
  label: string;
  value: string | null;
  anyLabel: string;
  options: string[];
  /// How to render an option. The state dropdown shows "Passing" over a
  /// value of `passing`, because the URL's vocabulary is the server's and
  /// the reader's is English.
  format?: (value: string) => string;
  onChange: (value: string | null) => void;
}) {
  // A value the list does not offer still has to show: a `?actor=ada`
  // whose runs have all paged off must light the dropdown up, not
  // silently reset itself to "Anyone" and disagree with the URL.
  const options = props.value
    ? [...new Set([props.value, ...props.options])]
    : props.options;
  const label = props.format ?? ((v: string) => v);
  return (
    <Select
      value={props.value ?? ANY}
      onValueChange={(v) => props.onChange(v === ANY ? null : v)}
    >
      <SelectTrigger aria-label={props.label} className="h-8 px-2 py-1">
        <SelectValue placeholder={props.label} />
      </SelectTrigger>
      <SelectContent>
        <SelectItem value={ANY}>{props.anyLabel}</SelectItem>
        {options.map((o) => (
          <SelectItem key={o} value={o}>
            {label(o)}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

function RunRow(props: {
  run: CheckRun;
  /// Why this run will never start, when it is one of ours and blocked.
  /// See `@/lib/workflow-runs`.
  refusal: string | null;
  navigate: (to: string, replace?: boolean) => void;
}) {
  const { run } = props;
  // The refusal wins over the stored word: a blocked run is mirrored as
  // `queued`, so the row would otherwise say "Queued" above a sentence
  // explaining that nothing will ever pick it up.
  const {
    label,
    icon: Icon,
    className,
  } = props.refusal ? BLOCKED_PRESENTATION : runStatePresentation(run.state);
  const took = runDuration(run, Date.now());
  return (
    <li className="flex items-start gap-3 border-b border-borderline bg-surface-1 px-4 py-3 last:border-b-0">
      {/* Glyph *and* word: the icon carries the state as its accessible
          name and the word is rendered beside it further down the row, so
          "did it pass" is never a question about colour. */}
      <span
        role="img"
        aria-label={label}
        title={label}
        className={cn("mt-0.5 inline-flex shrink-0", className)}
      >
        <Icon aria-hidden className="size-4 shrink-0" />
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          {/* Where the log actually is. For a third party that is their
              site and never a viewer of ours — we do not hold their
              logs, and proxying somebody else's build output is a
              promise we cannot keep — so it leaves, hardened, `ugc`
              because the URL arrived over an intake endpoint. For a run
              on our own runners it is our own run page: `ugc` would be
              a false claim about our own site, and a full page load
              onto this very SPA costs a reader their place for nothing,
              so it navigates in-app instead. `DetailLink` branches on
              the provider and the origin together. */}
          {run.detail_url ? (
            <DetailLink
              className={cn(STRUCTURAL_LINK, "min-w-0 truncate font-medium")}
              href={run.detail_url}
              provider={run.provider}
              navigate={props.navigate}
            >
              {run.name}
            </DetailLink>
          ) : (
            <span className="min-w-0 truncate font-medium text-ink">
              {run.name}
            </span>
          )}{" "}
          <span className={cn("text-xs", className)}>{label}</span>
          {run.ref_name && (
            <Badge variant="neutral" className="min-w-0 max-w-full">
              <GitBranch aria-hidden className="size-3 shrink-0" />{" "}
              <span className="min-w-0 truncate">{run.ref_name}</span>
            </Badge>
          )}
        </div>
        <p className="mt-0.5 flex flex-wrap items-center gap-x-2 text-xs text-ink-3">
          <span className="min-w-0 truncate font-mono">{runSubtitle(run)}</span>{" "}
          <RelativeTime at={run.created_at} />
          {took && (
            <>
              {" "}
              <span>
                {/* Numbers always render in mono (DESIGN.md). "so far"
                    rather than a bare number: a running build's elapsed
                    time presented like a final one reads as a slow pass
                    that already happened. */}
                <span className="font-mono">{formatDuration(took.ms)}</span>
                {took.running ? " so far" : ""}
              </span>
            </>
          )}
        </p>
        {props.refusal && (
          // Verbatim and on its own line. This sentence is the only
          // thing on the tab that distinguishes a build about to start
          // from one that is waiting on something — a fork's approval,
          // most often.
          <p className="mt-1 whitespace-pre-wrap break-words text-xs text-ink-2">
            {props.refusal}
          </p>
        )}
      </div>
    </li>
  );
}

/// The empty list, and why it is empty.
///
/// Four different truths render identically as "no rows", and the whole
/// reason this tab reads `/ci/poll` is to tell them apart. `emptyState`
/// decides which; this draws it.
///
/// A **filtered** empty list is a fifth thing and is checked first.
/// Explaining how to set up CI because somebody filtered to
/// `state=failing` on a green project answers a question they did not
/// ask, and it would say the project has no CI while its runs sit one
/// dropdown away.
function NoRuns(props: {
  owner: string;
  repo: string;
  filter: RunFilter;
  poll: ChecksPoll | null;
  session: Session;
  canWrite: boolean;
}) {
  const filtered = Object.values(props.filter).some((v) => v !== null);
  if (filtered) {
    return (
      <p className="p-10 text-center text-sm text-ink-3">
        No runs match this filter.
      </p>
    );
  }
  const state = emptyState(props.poll);
  return (
    <div className="p-10 text-center">
      <h2 className="text-sm font-semibold text-ink">{state.title}</h2>
      <p className="mx-auto mt-2 max-w-lg text-sm text-ink-2">{state.body}</p>
      {state.intake && (
        <p className="mx-auto mt-2 max-w-lg text-sm text-ink-3">
          Point your CI at{" "}
          {/* The address wraps inside its own box rather than widening
              the page: `audit()` measures `documentElement.scrollWidth`,
              and an org and repo name are both as long as somebody
              wants. */}
          <code className="inline-block max-w-full break-all rounded bg-surface-2 px-1.5 py-0.5 font-mono text-xs text-ink-2">
            POST /v1/orgs/{props.owner}/repos/{props.repo}/ci/checks
          </code>{" "}
          with{" "}
          {/* The sentence names a credential and then leaves the reader
              to find out where one comes from. It comes from the repo's
              Settings, and the page that explains the whole exchange —
              signing, fields, provider snippets — is the docs page. This
              is the moment somebody needs it. */}
          <a
            href={guide("ci-integration")}
            target="_blank"
            rel="noreferrer"
            className={PROSE_LINK}
          >
            an intake secret
          </a>
          , or connect this repository through the GitHub App to have its
          Actions runs polled in.
        </p>
      )}
      {state.reapprove && props.canWrite && (
        <p className="mt-4">
          <ReapproveButton session={props.session} />
        </p>
      )}
    </div>
  );
}

/// Send somebody to GitHub to grant the permission.
///
/// A button rather than a link because the URL does not exist until it is
/// asked for: `startGithubInstall` mints a single-use `state` and the URL
/// carries it, so rendering one per page view would burn a nonce nobody
/// pressed. The navigation is a full page leave — the approval happens on
/// GitHub and comes back to our callback.
function ReapproveButton(props: { session: Session }) {
  const [busy, setBusy] = useState(false);
  return (
    <button
      type="button"
      disabled={busy}
      className={cn(
        "rounded-sm text-sm text-brand underline-offset-2 hover:underline",
        FOCUS_RING,
      )}
      onClick={async () => {
        setBusy(true);
        try {
          const out = await api.startGithubInstall(props.session);
          window.location.href = out.url;
        } catch (e) {
          // The server knows which refusal this is — no App configured,
          // no permission to install — and its sentence is the one that
          // says so.
          toast.error(String((e as Error)?.message ?? e));
          setBusy(false);
        }
      }}
    >
      Review this installation's permissions
    </button>
  );
}
