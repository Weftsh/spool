import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import {
  ApiError,
  api,
  viewerSession,
  type LogEvent,
  type RunState,
  type WorkflowJob,
  type WorkflowRun,
} from "@/api";
import { RelativeTime } from "@/components/relative-time";
import { RunnerLabels } from "@/components/runner-labels";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ErrorBox, Loading } from "@/components/feedback";
import { FOCUS_RING, STRUCTURAL_LINK } from "@/lib/links";
import { cn } from "@/lib/utils";
import { dash, href } from "@/router";
import {
  BLOCKED_PRESENTATION,
  formatDuration,
  runDuration,
  runStatePresentation,
  shortSha,
} from "@/views/forge/checks";

/// One run of a `.weft/*.yml` workflow: what it was, what each job did,
/// and the log.
///
/// This page is the other end of a link. A workflow job mirrors itself
/// into a check row like any other CI verdict, and that row's
/// `detail_url` is this address — so for a third-party provider "Details"
/// leaves for their site, and for us it lands here. Before this existed
/// the same row carried `detail_url: null`, which meant a reader looking
/// at a red check on their own repository had no way at all to reach the
/// log that said why, and a workflow file we *refused to run* said only
/// "failed": the parse error was on the run record and nothing rendered
/// the run record.
///
/// Three deliberate choices, each of which is a bug somewhere else:
///
/// - **The run-level `error` is rendered verbatim, in full, at the top.**
///   It is the whole message for a refused workflow — the YAML line that
///   would not parse, the job that depends on itself — and paraphrasing
///   or truncating it turns something actionable into "something went
///   wrong". It is also where a cancellation says who asked for it.
/// - **A live job is read from the stream and from nowhere else.** See
///   `useJobLog` — this is not a preference.
/// - **Cancel is a two-step in place**, never `window.confirm`. A browser
///   dialog is untestable, unstyleable, and blocks the whole tab.

/// The Checks tab's six-word state vocabulary, which this page borrows
/// rather than growing a second one.
///
/// The runner's words and a check row's words are different on purpose —
/// `passed` is what our runner writes, `passing` is what every provider's
/// verdict is translated into — and translating here keeps one repository
/// from having two ideas of what green looks like. `blocked` has no
/// counterpart, because no third-party provider has the concept, so it is
/// the one state this page draws itself.
const AS_CHECK_STATE: Record<string, RunState> = {
  queued: "queued",
  running: "running",
  passed: "passing",
  failed: "failing",
  cancelled: "cancelled",
};

/// How a workflow run or job state reads, as a word and as a shape.
///
/// Never colour alone — `web/DESIGN.md`. Every state differs in glyph as
/// well as tone, and the word is rendered beside the glyph.
///
/// A state this bundle has never heard of falls through to
/// `runStatePresentation`'s own unknown arm, which prints the server's
/// literal word in neutral ink. That is the only honest rendering: a
/// verdict quietly shown green ships broken code and one quietly shown
/// red blocks good code.
export function workflowStatePresentation(state: string) {
  // The one shared with the Checks tab and the change panel, so a
  // blocked run looks the same wherever a reader meets it.
  if (state === "blocked") return BLOCKED_PRESENTATION;
  // `Object.hasOwn` for the same reason `runStatePresentation` uses it:
  // `AS_CHECK_STATE["constructor"]` is a function, not `undefined`, so
  // `??` would hand a function to the renderer instead of the literal
  // word this line exists to pass through.
  return runStatePresentation(
    Object.hasOwn(AS_CHECK_STATE, state) ? AS_CHECK_STATE[state] : state,
  );
}

/// Is this job still producing output?
///
/// The two states the live feed exists for. Everything else — including a
/// word from a newer server — is treated as settled, which fails towards
/// a log that is fetched once rather than a socket held open forever
/// against a job that will never write to it again.
export function isLive(state: string): boolean {
  return state === "queued" || state === "running";
}

/// Which job to show when the reader has not chosen one.
///
/// The first that failed, because the overwhelming reason somebody
/// follows a red check to this page is to find out what broke, and a
/// twelve-job matrix with one red leg would otherwise open on a green
/// one. Then the first still going, for the same "what is happening"
/// reason on a live run. Then simply the first.
///
/// Returns an index rather than a job so that the "no jobs at all" case
/// — a refused workflow, which is precisely when this page matters most
/// — is `-1` and not a null the caller has to re-derive.
export function defaultJobIndex(jobs: readonly { state: string }[]): number {
  const failed = jobs.findIndex((j) => j.state === "failed");
  if (failed !== -1) return failed;
  const going = jobs.findIndex((j) => j.state === "running");
  if (going !== -1) return going;
  return jobs.length > 0 ? 0 : -1;
}

/// What to call a job on screen.
///
/// `key` already carries the matrix coordinates the server folded in, so
/// it is the name — but a job whose `key` is somehow empty must still be
/// addressable, and falling back to `job_id` beats rendering a blank
/// button nobody can click on purpose.
export function jobLabel(job: Pick<WorkflowJob, "key" | "job_id">): string {
  return job.key || job.job_id;
}

/// The subtitle under the run's name: what caused it, and on what.
///
/// A string rather than JSX because the parts are conditional in four
/// combinations and the version that built them inline read as a puzzle.
export function runSubtitle(run: {
  event: string;
  ref_name: string | null;
  file: string;
  changeset?: { key: string } | null;
}): string {
  const where = run.ref_name ? ` on ${run.ref_name}` : "";
  // A composed run is named for its changeset in the one line a reader
  // reads first. "changeset on main" said what kind of run it was and
  // not which one, and the page below it linked only the member's
  // commit and change — so the run of a *combination* looked like one
  // more run of the member, and nothing on the page led to the set it
  // was a verdict on.
  const which =
    run.event === "changeset" && run.changeset ? ` ${run.changeset.key}` : "";
  return `${run.event}${which}${where} · ${run.file}`;
}

/// May this viewer stop this run?
///
/// Subtractive, like every other write control on the forge: `canWrite`
/// is false until the repository row that says otherwise has arrived, so
/// the button appears a moment late rather than appearing and then being
/// refused. A settled run has nothing to cancel and the server answers
/// `409`, so offering it would be a button that cannot do what it says.
export function canCancel(run: { state: string }, canWrite: boolean): boolean {
  return canWrite && run.state === "running";
}

/// How often a live run is re-read.
///
/// Only while it is running, and it stops the moment it settles. Three
/// seconds is slower than the log feed's one, deliberately: the log is
/// what a person is watching and the run record only has to catch the
/// state changing.
const RUN_POLL_MS = 3000;

/// The log of one job, kept current.
///
/// **The read strategy is the load-bearing part.** `…/log` answers with
/// the whole log so far, and `…/log/stream` *replays* — it starts at
/// chunk one of the current attempt and then keeps going. They are not a
/// snapshot and a continuation of it; the stream is a superset. Fetching
/// the log and then appending the stream's chunks renders every line
/// that existed at page load twice, which reads as the build having run
/// twice and is invisible in any test whose fixture starts empty.
///
/// So: exactly one source at a time. A live job is read from the stream
/// alone; a settled one is fetched once. When a job settles under us the
/// effect re-runs and fetches — which is also the *right* read rather
/// than merely a tidy one, because the runner uploads a single
/// authoritative log object at the end and the chunk sequence it
/// replaces may have holes in it.
function useJobLog(
  org: string,
  token: string,
  repo: string,
  /// `null` while there is no job to read — a refused run has none.
  job: WorkflowJob | null,
  /// Bumped by the caller when the feed says the job is over, so the run
  /// record is re-read at once rather than at the next poll.
  onDone: () => void,
) {
  const [text, setText] = useState<string | null>(null);
  const [waiting, setWaiting] = useState(false);
  const [failed, setFailed] = useState<string | null>(null);

  const id = job?.id ?? null;
  const live = job !== null && isLive(job.state);
  // A retry restarts the chunk sequence, so it has to restart the read
  // as well: without this, a stream reopened on attempt 2 would be
  // appended to attempt 1's output.
  const attempt = job?.attempts ?? 0;

  useEffect(() => {
    if (id === null) {
      setText(null);
      setWaiting(false);
      setFailed(null);
      return;
    }
    let alive = true;
    const abort = new AbortController();
    setFailed(null);
    setWaiting(false);

    if (!live) {
      setText(null);
      api
        .jobLog({ org, token }, repo, id)
        .then((t) => alive && setText(t))
        .catch((e) => alive && setFailed(String(e?.message ?? e)));
      return () => {
        alive = false;
      };
    }

    // Live: the stream is the only source. `""` rather than `null` up
    // front so the log frame renders immediately — a running job that
    // showed a spinner until its first flush looks stuck, and the first
    // flush can be a minute away.
    setText("");
    api
      .streamJobLog(
        { org, token },
        repo,
        id,
        (event: LogEvent) => {
          if (!alive) return;
          if (event.kind === "queued") {
            setWaiting(true);
            return;
          }
          if (event.kind === "chunk") {
            // A chunk means a runner picked it up, whatever the job row
            // last said.
            setWaiting(false);
            setText((prev) => (prev ?? "") + event.text);
            return;
          }
          setWaiting(false);
          onDone();
        },
        abort.signal,
      )
      .catch((e) => {
        // An aborted read is this effect cleaning up after itself, not a
        // failure worth showing anybody.
        if (!alive || abort.signal.aborted) return;
        setFailed(String(e?.message ?? e));
      });

    return () => {
      alive = false;
      abort.abort();
    };
  }, [org, token, repo, id, live, attempt, onDone]);

  return { text, waiting, failed };
}

export function WorkflowRunView(props: {
  owner: string;
  repo: string;
  runId: string;
  token: string | null;
  /// Whether this viewer holds `repo:write`, which is what Cancel needs.
  canWrite: boolean;
  navigate: (to: string, replace?: boolean) => void;
}) {
  const { owner, repo, runId, navigate } = props;
  const session = useMemo(
    () => viewerSession(owner, props.token),
    [owner, props.token],
  );
  const { org, token } = session;

  const [run, setRun] = useState<WorkflowRun | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [bump, setBump] = useState(0);
  /// The job the reader chose, by id. `null` means "whichever
  /// `defaultJobIndex` says", which has to stay a live derivation: on a
  /// running matrix the interesting job changes as legs go red, and
  /// pinning the choice at first render would leave the reader watching
  /// a green job while another one failed.
  const [chosen, setChosen] = useState<string | null>(null);
  const [arming, setArming] = useState(false);
  const [cancelling, setCancelling] = useState(false);

  useEffect(() => {
    let alive = true;
    api
      .workflowRun({ org, token }, repo, runId)
      .then((r) => {
        if (!alive) return;
        setRun(r);
        setError(null);
      })
      .catch((e) => {
        if (!alive) return;
        // A run of another repository answers 404 exactly as a missing
        // one does — that is the existence masking working — so the page
        // says the same thing for both.
        setError(
          e instanceof ApiError && e.status === 404
            ? "We couldn't find that run. It may have been in another repository, or swept."
            : String(e?.message ?? e),
        );
      });
    return () => {
      alive = false;
    };
  }, [org, token, repo, runId, bump]);

  // Re-read while it is running, and stop the moment it is not. An
  // interval that outlives the run would keep a settled page requesting
  // forever in a tab somebody left open.
  const running = run?.state === "running";
  useEffect(() => {
    if (!running) return;
    const t = setInterval(() => setBump((n) => n + 1), RUN_POLL_MS);
    return () => clearInterval(t);
  }, [running]);

  const refresh = useCallback(() => setBump((n) => n + 1), []);

  const jobs = run?.jobs ?? [];
  const fallback = defaultJobIndex(jobs);
  const selected =
    jobs.find((j) => j.id === chosen) ??
    (fallback === -1 ? null : jobs[fallback]);

  const log = useJobLog(org, token, repo, selected ?? null, refresh);

  if (error) return <ErrorBox message={error} />;
  if (!run) return <Loading />;

  const pres = workflowStatePresentation(run.state);
  const Icon = pres.icon;
  const took = runDuration(
    { started_at: run.created_at, completed_at: run.completed_at },
    Date.now(),
  );

  return (
    <div className="py-6">
      <div className="rounded-lg border border-borderline bg-surface-1 p-5">
        <div className="flex flex-wrap items-start gap-3">
          {/* Glyph *and* word. The word is rendered beside it below, so
              "did it pass" is never a question about colour. */}
          <span
            role="img"
            aria-label={pres.label}
            title={pres.label}
            className={cn("mt-1 inline-flex shrink-0", pres.className)}
          >
            <Icon aria-hidden className="size-5 shrink-0" />
          </span>
          <div className="min-w-0 flex-1">
            <h1 className="min-w-0 break-words text-lg font-semibold text-ink">
              {run.name}
            </h1>
            <p className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-ink-3">
              <span className={cn("font-medium", pres.className)}>
                {pres.label}
              </span>{" "}
              <span className="min-w-0 truncate font-mono">
                {runSubtitle(run)}
              </span>{" "}
              <RelativeTime at={run.created_at} />
              {took && (
                <span>
                  {/* "so far" rather than a bare number: a running
                      build's elapsed time presented like a final one
                      reads as a slow pass that already happened. */}
                  <span className="font-mono">{formatDuration(took.ms)}</span>
                  {took.running ? " so far" : ""}
                </span>
              )}
            </p>
            <p className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
              <a
                className={cn(STRUCTURAL_LINK, "font-mono")}
                href={href([owner, repo, "commit", run.commit_sha])}
                onClick={(e) => {
                  if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0)
                    return;
                  e.preventDefault();
                  navigate(href([owner, repo, "commit", run.commit_sha]));
                }}
              >
                {shortSha(run.commit_sha)}
              </a>
              {run.ref_name && (
                <Badge variant="neutral" className="min-w-0 max-w-full">
                  <span className="min-w-0 truncate">{run.ref_name}</span>
                </Badge>
              )}
              {run.change_key && (
                <a
                  className={STRUCTURAL_LINK}
                  href={href([owner, repo, "changes", run.change_key])}
                  onClick={(e) => {
                    if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0)
                      return;
                    e.preventDefault();
                    navigate(
                      href([owner, repo, "changes", run.change_key as string]),
                    );
                  }}
                >
                  the change it belongs to
                </a>
              )}
              {run.changeset && (
                // A full load, not `navigate`: this page is mounted on
                // the forge, where `navigate` is relative to a mount
                // that has no changesets route.
                <a
                  className={STRUCTURAL_LINK}
                  href={dash(["changesets", run.changeset.key])}
                >
                  the changeset it was composed for
                </a>
              )}
              {run.composition && (
                // Which combination of member tips this run saw. The
                // changeset page prints the same twelve characters over
                // its composed checks, so a reader can tell this run
                // from the one a new patchset replaced it with.
                <span
                  className="font-mono text-ink-3"
                  title={`composition ${run.composition}`}
                >
                  composition {run.composition.slice(0, 12)}
                </span>
              )}
            </p>
          </div>
          {canCancel(run, props.canWrite) && (
            <CancelControl
              armed={arming}
              busy={cancelling}
              onArm={() => setArming(true)}
              onDismiss={() => setArming(false)}
              onConfirm={async () => {
                setCancelling(true);
                try {
                  setRun(
                    await api.cancelWorkflowRun({ org, token }, repo, runId),
                  );
                  setArming(false);
                } catch (e) {
                  // The server's own sentence — "this run is already
                  // passed" is a different fact from "you may not" and
                  // the person needs to know which. Either way the page
                  // is stale, so re-read it.
                  toast.error(String((e as Error)?.message ?? e));
                  refresh();
                  setArming(false);
                } finally {
                  setCancelling(false);
                }
              }}
            />
          )}
        </div>

        {/* The run's own reason, verbatim and unabridged. This is the
            entire explanation for a workflow file we refused to run —
            the line of YAML, the cycle in `needs:` — and it is where a
            cancellation says who asked. `whitespace-pre-wrap` because
            the server's message is sometimes several lines and
            collapsing them puts a parse error on one unreadable line. */}
        {run.error && (
          <p
            role="status"
            className="mt-4 whitespace-pre-wrap break-words rounded-md border border-warning/40 bg-warning/10 px-3 py-2 font-mono text-xs text-ink-2"
          >
            {run.error}
          </p>
        )}
      </div>

      {jobs.length === 0 ? (
        <p className="mt-6 text-sm text-ink-3">
          {/* Not "no jobs", which reads as a bug. A run with no jobs is
              a run that never started one, and the reason is in the
              banner above. */}
          This run never started a job. The reason is above.
        </p>
      ) : (
        <div className="mt-6 grid gap-4 md:grid-cols-[minmax(0,16rem)_minmax(0,1fr)]">
          <div className="min-w-0">
            <h2 className="mb-2 text-sm font-medium text-ink">
              {jobs.length} {jobs.length === 1 ? "job" : "jobs"}
            </h2>
            <ul className="overflow-hidden rounded-lg border border-borderline">
              {jobs.map((j) => (
                <JobRow
                  key={j.id}
                  job={j}
                  selected={j.id === selected?.id}
                  onSelect={() => setChosen(j.id)}
                />
              ))}
            </ul>
          </div>
          <div className="min-w-0">
            <h2 className="mb-2 min-w-0 truncate text-sm font-medium text-ink">
              {selected ? jobLabel(selected) : "Log"}
            </h2>
            {/* What this job asked for, beside the log it produced.
                In file order, which is the order the server's "no
                runner with labels […]" refusal quotes them in — so the
                two lists can be compared by eye. */}
            {selected &&
              (selected.labels?.length ?? 0) > 0 && (
                <RunnerLabels
                  className="mb-2"
                  labels={selected.labels as string[]}
                />
              )}
            {selected?.error && (
              <p className="mb-2 whitespace-pre-wrap break-words rounded-md border border-serious/40 bg-serious/10 px-3 py-2 font-mono text-xs text-ink-2">
                {selected.error}
              </p>
            )}
            <LogPane
              text={log.text}
              waiting={log.waiting}
              failed={log.failed}
              live={selected !== null && isLive(selected.state)}
            />
          </div>
        </div>
      )}
    </div>
  );
}

function JobRow(props: {
  job: WorkflowJob;
  selected: boolean;
  onSelect: () => void;
}) {
  const { job } = props;
  const pres = workflowStatePresentation(job.state);
  const Icon = pres.icon;
  const took = runDuration(job, Date.now());
  return (
    <li className="border-b border-borderline last:border-b-0">
      <button
        type="button"
        // `aria-current` and not colour alone: which job is being read is
        // as much a state as which one failed.
        aria-current={props.selected ? "true" : undefined}
        onClick={props.onSelect}
        className={cn(
          "flex w-full items-start gap-2 px-3 py-2 text-left text-sm",
          FOCUS_RING,
          props.selected
            ? "bg-surface-2 text-ink"
            : "bg-surface-1 text-ink-2 hover:bg-surface-2",
        )}
      >
        <span
          role="img"
          aria-label={pres.label}
          title={pres.label}
          className={cn("mt-0.5 inline-flex shrink-0", pres.className)}
        >
          <Icon aria-hidden className="size-4 shrink-0" />
        </span>
        <span className="min-w-0 flex-1">
          <span className="block min-w-0 truncate font-medium">
            {jobLabel(job)}
          </span>
          <span className="mt-0.5 flex flex-wrap items-center gap-x-2 text-xs text-ink-3">
            <span className={pres.className}>{pres.label}</span>
            {took && (
              <span className="font-mono">{formatDuration(took.ms)}</span>
            )}
            {/* A retry is worth saying out loud: a green job that took
                three goes is not the same fact as a green job. */}
            {job.attempts > 1 && <span>attempt {job.attempts}</span>}
            {/* Which machine took it. The first thing an operator
                looking at a job that behaved oddly needs, because the
                next question is always "which box". */}
            {job.runner && (
              <span className="min-w-0 truncate">
                ran on <span className="font-mono">{job.runner.name}</span>
              </span>
            )}
          </span>
        </span>
      </button>
    </li>
  );
}

/// The two-step. Never `window.confirm`: a browser dialog cannot be
/// styled, cannot be driven by the walkthrough, and blocks the tab.
function CancelControl(props: {
  armed: boolean;
  busy: boolean;
  onArm: () => void;
  onDismiss: () => void;
  onConfirm: () => void;
}) {
  if (!props.armed) {
    return (
      <Button variant="outline" size="xs" onClick={props.onArm}>
        Cancel run
      </Button>
    );
  }
  return (
    <span className="flex flex-wrap items-center gap-2">
      {/* The confirming button says the whole thing, the way the
          repository settings' do: by the time it is on screen, "Cancel"
          on its own no longer names what is about to happen — and beside
          a "Keep it running" button, two words called "Cancel" would
          mean opposite things. */}
      <span className="text-xs text-ink-3">Stop this run?</span>
      <Button
        variant="destructive"
        size="xs"
        disabled={props.busy}
        onClick={props.onConfirm}
      >
        {props.busy ? "Stopping…" : "Stop this run"}
      </Button>
      <Button
        variant="ghost"
        size="xs"
        disabled={props.busy}
        onClick={props.onDismiss}
      >
        Keep it running
      </Button>
    </span>
  );
}

function LogPane(props: {
  text: string | null;
  waiting: boolean;
  failed: string | null;
  live: boolean;
}) {
  const box = useRef<HTMLPreElement>(null);
  const { text, live } = props;
  // Follow the tail while it is being written, and only then: yanking a
  // settled log back to the bottom would fight a reader who scrolled up
  // to find the failure.
  useEffect(() => {
    if (!live || !box.current) return;
    box.current.scrollTop = box.current.scrollHeight;
  }, [text, live]);

  if (props.failed !== null) {
    return (
      <p className="rounded-md border border-borderline bg-surface-1 px-3 py-2 text-sm text-ink-3">
        We couldn't read this job's log ({props.failed}).
      </p>
    );
  }
  if (text === null) return <Loading />;
  return (
    <div className="overflow-hidden rounded-lg border border-borderline bg-surface-1">
      {props.waiting && (
        <p
          role="status"
          className="border-b border-borderline bg-surface-2 px-3 py-2 text-xs text-ink-3"
        >
          Waiting for a runner — nothing has picked this job up yet.
        </p>
      )}
      {text === "" && !props.waiting ? (
        <p className="px-3 py-2 text-sm text-ink-3">
          {live ? "No output yet." : "This job produced no output."}
        </p>
      ) : (
        <pre
          ref={box}
          // `log` rather than `aria-live`: a build log is thousands of
          // lines, and announcing each arriving chunk would make the
          // page unusable with a screen reader on. The role says what it
          // is; the reader chooses when to read it.
          role="log"
          aria-label="Build log"
          tabIndex={0}
          className={cn(
            "max-h-[32rem] overflow-auto px-3 py-2 font-mono text-xs leading-relaxed text-ink-2",
            FOCUS_RING,
          )}
        >
          {text}
        </pre>
      )}
    </div>
  );
}
