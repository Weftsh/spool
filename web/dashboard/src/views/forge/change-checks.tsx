/// The checks panel on a change (PR) review page, in GitHub's shape: a
/// summary headline with its counts, a row per check, and — rendered
/// directly beneath it by the caller — the land box, which states its own
/// blockers.
///
/// Everything a reader has to *decide* on lives in the exported pure
/// functions below, and is unit-tested without a DOM. The component is
/// the arrangement; the arithmetic is not.
///
/// Two rules from `web/DESIGN.md` and `web/FORGE-UX.md` §9 shape almost
/// every choice here:
///
///  - **Never colour alone.** GitHub's checks list is a column of green
///    ticks and red crosses with the state carried by hue and glyph and
///    nothing else. Every row here also prints the state as a *word*.
///    "Did CI pass" read off a colour is the exact question a red-green
///    colourblind reader gets wrong.
///  - **A disabled control says why.** The land button's blockers are
///    computed here so they can be listed under it, each naming the thing
///    it names.
import { useState } from "react";
import {
  Check,
  ChevronDown,
  ChevronRight,
  CircleDashed,
  X,
  type LucideIcon,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { DetailLink } from "@/components/detail-link";
import { cn } from "@/lib/utils";
import {
  BLOCKED_PRESENTATION,
  formatDuration,
  runDuration,
  runStatePresentation,
} from "./checks";

/// One check as this panel needs it, merging the two writers.
///
/// `change_checks` (the signed intake, per patchset) and `check_runs`
/// (polled from GitHub Actions, per commit) are different tables with
/// different columns, and the panel is deliberately incurious about which
/// one a row came from — a project on Buildkite has to read like a
/// project on Actions.
///
/// Every field the current `ChangeCheck` in `@/api` does not carry is
/// optional, so this renders correctly against today's response and gains
/// detail when the merged route lands rather than waiting for it.
export interface PanelCheck {
  name: string;
  /// Free text on purpose. `change_checks` writes
  /// `pending|passing|failing`; `check_runs` adds
  /// `queued|running|cancelled|skipped`; a provider added after this
  /// bundle shipped writes something else again. See `classifyCheck`.
  state: string;
  /// The provider's own log. Somebody else's site — see the `rel` on the
  /// link.
  detail_url?: string | null;
  /// Whether the repo's policy requires this check to pass before a
  /// change can land. Absent means "not known to be required", never
  /// "not required" — the panel only ever *adds* a Required badge.
  required?: boolean;
  /// What the row is a statement about: `"patchset"` or `"commit"`.
  /// Not shown on the row — a reader does not act on the scope — but it
  /// keys the list, and `checkBlockers` reads `required` beside it.
  source?: string | null;
  /// Who reported it: an intake principal, or the provider for a
  /// commit-scoped run. **This** is the hint the row prints, because
  /// "where do I go and look when this is red" is the question a reader
  /// actually has. It used to print `source`, back when that field held
  /// the writer; a row reading "commit" tells nobody anything.
  posted_by?: string | null;
  started_at?: number | null;
  completed_at?: number | null;
  /// Why this check will never start, when it is a workflow run somebody
  /// has to unblock — a fork awaiting approval, most often.
  ///
  /// The reason lives on the run and not on the check row: a blocked run
  /// mirrors as `queued`, deliberately, because nothing is wrong with
  /// the change. The row is therefore indistinguishable from CI that is
  /// about to start, and the reader waits for something that will never
  /// arrive. The caller joins the run back on — see
  /// `@/lib/workflow-runs` — and this is where it lands.
  ///
  /// Rendered **verbatim**. These sentences are the product's answer to
  /// "why has nothing run"; a paraphrase here is the page inventing a
  /// reason the server never gave.
  refusal?: string | null;
}

/// Which of the four things a reader can do with a check.
export type CheckOutcome = "passing" | "failing" | "pending" | "neutral";

/// The bucket a state word falls in.
///
/// `cancelled` is counted with the failures rather than with the pending
/// checks: a cancelled run is finished and did not pass, and filing it
/// under "haven't completed yet" tells a maintainer to wait for something
/// that will never arrive.
///
/// A state this bundle has never heard of is **pending**, which is the
/// only safe direction. Guessing green ships code nobody verified;
/// guessing red blocks code that is fine. Pending claims neither, and the
/// row still prints the unknown word itself so the disagreement is
/// visible rather than swallowed.
export function classifyCheck(state: string): CheckOutcome {
  switch (state) {
    case "passing":
    case "success":
      return "passing";
    case "failing":
    case "failure":
    case "cancelled":
      return "failing";
    case "skipped":
      return "neutral";
    default:
      return "pending";
  }
}

/// Whether any check is red, in the sense the land gate means it.
///
/// Exported and named because the change page needs the same answer for
/// its land button, and the copy it had was `state === "failing"` — the
/// patchset route's single word. The merged read carries both
/// vocabularies, so a commit-scoped `failure` or `cancelled` slipped
/// past it: the button stayed enabled over a red check, and the server
/// refused the land with a 409 the page had given no warning of. One
/// predicate, used by the control and by the reasons printed under it.
export function hasFailingCheck(
  checks: readonly Pick<PanelCheck, "state">[],
): boolean {
  return checks.some((c) => classifyCheck(c.state) === "failing");
}

export interface ChecksSummary {
  successful: number;
  failing: number;
  pending: number;
  skipped: number;
  /// Required names with no row at all. Counted apart from `pending`
  /// because they are a different sentence to a reader — a check that is
  /// running will finish on its own, and one that has never reported may
  /// never do so — and because they have no row to appear in the list
  /// below, so the count is the only trace of them on the panel.
  missing: number;
  total: number;
  /// Rows that are waiting on a person rather than on a machine.
  ///
  /// Counted **from the refusal**, never from the state word: a blocked
  /// run's row says `queued`, which is the whole reason this exists.
  /// They are pending as well — they have not completed — so this is an
  /// extra fact about a subset and not a fifth bucket, and the counts
  /// still sum to the total.
  blocked: number;
  /// The headline's own state, which is not simply the commonest outcome:
  /// one failure out of two hundred passes is a failing headline.
  outcome: CheckOutcome;
}

/// The counts, and the single state the headline speaks in.
///
/// Precedence is failing → pending → passing, because the headline
/// answers "can I stop looking at this page", and any failure or any
/// unfinished run means no.
export function summarize(
  checks: readonly PanelCheck[],
  required: readonly string[] = [],
): ChecksSummary {
  let successful = 0;
  let failing = 0;
  let pending = 0;
  let skipped = 0;
  let blocked = 0;
  for (const c of checks) {
    if (c.refusal) blocked += 1;
    switch (classifyCheck(c.state)) {
      case "passing":
        successful += 1;
        break;
      case "failing":
        failing += 1;
        break;
      case "pending":
        pending += 1;
        break;
      case "neutral":
        skipped += 1;
        break;
    }
  }
  // A required name with no row is not in `checks` to be counted — that
  // is what never-reported means — and it is exactly the case that made
  // this panel announce "All checks have passed" over a change the land
  // queue was holding. The headline has to answer "can I stop looking at
  // this page", and the answer is no.
  const reported = new Set(checks.map((c) => c.name));
  const missing = required.filter((n) => !reported.has(n)).length;
  const outcome: CheckOutcome =
    failing > 0
      ? "failing"
      : pending > 0 || missing > 0
        ? "pending"
        : successful > 0
          ? "passing"
          : "neutral";
  return {
    successful,
    failing,
    pending,
    skipped,
    blocked,
    missing,
    total: checks.length,
    outcome,
  };
}

/// The sentence at the top of the panel.
///
/// The first three are GitHub's own wording, kept verbatim because it is
/// the wording a contributor already reads without parsing. The fourth is
/// ours: a list where every check was skipped is not "all checks have
/// passed", and saying so would be the panel's one outright lie.
export function headline(summary: ChecksSummary): string {
  switch (summary.outcome) {
    case "failing":
      return "Some checks were not successful";
    case "pending":
      return "Some checks haven't completed yet";
    case "passing":
      return "All checks have passed";
    case "neutral":
      return "No checks were run";
  }
}

/// `"3 successful, 1 failing"` — the line under the headline.
///
/// Zero categories are dropped rather than printed as `0 failing`. A
/// standing `0 failing` is a number a reader has to check every time to
/// learn nothing; its absence is the same fact, stated once.
export function countLine(summary: ChecksSummary): string {
  const parts: string[] = [];
  if (summary.successful > 0) parts.push(`${summary.successful} successful`);
  if (summary.failing > 0) parts.push(`${summary.failing} failing`);
  if (summary.pending > 0) parts.push(`${summary.pending} pending`);
  if (summary.skipped > 0) parts.push(`${summary.skipped} skipped`);
  // Before "not reported", because a blocked check is the one a person
  // can do something about right now. `pending` already counts these —
  // this line names the part of that number nothing is working on.
  if (summary.blocked > 0) parts.push(`${summary.blocked} blocked`);
  // Last, and worded as the absence it is. "1 not reported" beside
  // "1 successful" is the whole state of a change held by the queue.
  if (summary.missing > 0) parts.push(`${summary.missing} not reported`);
  return parts.join(", ");
}

/// How long a check took, as a row wants to say it.
///
/// Delegates the arithmetic to the Checks tab's `runDuration`, which
/// already refuses to invent a duration for a run that never started or
/// one whose build machine's clock disagrees with ours. The only thing
/// added here is the word "so far", so an in-progress number is never
/// read as a final one.
export function checkDurationLabel(
  check: Pick<PanelCheck, "started_at" | "completed_at">,
  now: number,
): string | null {
  const d = runDuration(
    {
      started_at: check.started_at ?? null,
      completed_at: check.completed_at ?? null,
    },
    now,
  );
  if (d === null) return null;
  return d.running ? `${formatDuration(d.ms)} so far` : formatDuration(d.ms);
}

/// The word and glyph for one row's state.
///
/// `check_runs`' six states are already settled in the Checks tab and are
/// reused wholesale. `pending` is the intake table's word and has no
/// entry there, so it gets one here rather than falling through to the
/// unknown-state rendering — it is not unknown, it is the other writer's
/// vocabulary.
export function checkStatePresentation(state: string): {
  label: string;
  icon: LucideIcon;
  className: string;
} {
  if (state === "pending")
    return { label: "Pending", icon: CircleDashed, className: "text-warning" };
  return runStatePresentation(state);
}

/// The glyph and ink for the headline itself. FORGE-UX §9's table.
function headlinePresentation(outcome: CheckOutcome): {
  icon: LucideIcon;
  className: string;
} {
  switch (outcome) {
    case "passing":
      return { icon: Check, className: "text-good" };
    case "failing":
      return { icon: X, className: "text-serious" };
    case "pending":
      return { icon: CircleDashed, className: "text-warning" };
    case "neutral":
      return { icon: CircleDashed, className: "text-ink-3" };
  }
}

/// The server's answer to "may this land, and if not, why not".
///
/// Mirrors `LandGate` in `crates/stratum-control/src/changes.rs`.
export type LandGate =
  | { state: "ready" }
  | { state: "blocked"; reason: string }
  | { state: "waiting"; on: string[] };

function plural(n: number, one: string, many: string): string {
  return n === 1 ? one : many;
}

function nameList(names: readonly string[]): string {
  return names.join(", ");
}

/// The blockers the server itself reports.
///
/// `Waiting { on }` carries check *names*, so it is rendered as one line
/// that both counts and names them: a bare "2 required checks have not
/// passed" leaves the reader to go find which two, which is the whole
/// failure mode a blockers list exists to fix.
export function gateBlockers(gate: LandGate): string[] {
  switch (gate.state) {
    case "ready":
      return [];
    case "blocked":
      return [gate.reason];
    case "waiting": {
      if (gate.on.length === 0) return [];
      const n = gate.on.length;
      return [
        `${n} required ${plural(n, "check", "checks")} ${plural(
          n,
          "has",
          "have",
        )} not passed: ${nameList(gate.on)}`,
      ];
    }
  }
}

/// The blockers derivable from the checks alone, for as long as there is
/// no `LandGate` on the wire.
///
/// `required` is the repo policy's list of names, which is a superset of
/// nothing: a name in it with no check at all is a *missing* required
/// check, and that is a different sentence from a failing one. A reader
/// told "build failed" opens the log; a reader told "build has not
/// reported" goes and looks at their CI configuration. Collapsing the two
/// sends half of them to the wrong place.
///
/// A **non-required** failing check is listed too, because the server's
/// land rule today refuses any change with a failing check regardless of
/// policy. It is worded without the word "required" so the list never
/// claims a policy that does not exist. If that rule is ever relaxed to
/// required-only, this branch is the one line to delete.
export function checkBlockers(
  checks: readonly PanelCheck[],
  required: readonly string[] = [],
): string[] {
  const byName = new Map(checks.map((c) => [c.name, c]));
  const requiredNames = new Set<string>(required);
  for (const c of checks) if (c.required) requiredNames.add(c.name);

  const failedRequired: string[] = [];
  const missingRequired: string[] = [];
  const unfinishedRequired: string[] = [];
  const failedOther: string[] = [];

  for (const name of requiredNames) {
    const c = byName.get(name);
    if (c === undefined) {
      missingRequired.push(name);
      continue;
    }
    const outcome = classifyCheck(c.state);
    if (outcome === "failing") failedRequired.push(name);
    else if (outcome !== "passing") unfinishedRequired.push(name);
  }
  for (const c of checks)
    if (!requiredNames.has(c.name) && classifyCheck(c.state) === "failing")
      failedOther.push(c.name);

  const out: string[] = [];
  if (failedRequired.length > 0) {
    const n = failedRequired.length;
    out.push(
      `${n} required ${plural(n, "check", "checks")} failed: ${nameList(
        failedRequired,
      )}`,
    );
  }
  if (missingRequired.length > 0) {
    const n = missingRequired.length;
    out.push(
      `${n} required ${plural(n, "check", "checks")} ${plural(
        n,
        "has",
        "have",
      )} not reported: ${nameList(missingRequired)}`,
    );
  }
  if (unfinishedRequired.length > 0) {
    const n = unfinishedRequired.length;
    out.push(
      `${n} required ${plural(n, "check", "checks")} ${plural(
        n,
        "has",
        "have",
      )} not passed yet: ${nameList(unfinishedRequired)}`,
    );
  }
  if (failedOther.length > 0) {
    const n = failedOther.length;
    out.push(
      `${n} ${plural(n, "check", "checks")} failed: ${nameList(failedOther)}`,
    );
  }
  return out;
}

/// The prefix every "still waiting" note carries, and nothing else in
/// `land_verdict` does.
///
/// The same constant as `LAND_WAITING_PREFIX` in
/// `crates/stratum-control/src/changes.rs`, which owns it: the control
/// plane adds the prefix in `set_land_waiting` rather than trusting a
/// caller to, precisely so that it is a discriminator and not a
/// convention. Reading `state === "landing"` instead would work today
/// and rot immediately — nothing clears the note on the way out, so it
/// outlives the state it was written under.
export const LAND_WAITING_PREFIX = "waiting on ";

/// What a `land_verdict` is telling the reader.
///
/// One column carries three different sentences and they want three
/// different presentations, so the reading is done once, here, where it
/// can be tested without a browser.
///
/// - **waiting** — the queue has *not* given up. The change is still
///   landing and these names are what it is holding for. This is the
///   ordinary case (push, press Land, CI has not started yet) and it is
///   the one that was invisible: the page rendered only `ejected`, so an
///   author watched a spinner for the full wait budget with a green
///   checks panel above it and no reason anywhere on the page.
/// - **ejected** — the queue gave up or a verdict said no. Actionable,
///   and already rendered.
/// - **note** — anything else the lander wrote, `"landed"` included.
///   Shown as itself rather than swallowed, because a verdict nobody
///   anticipated is still the only account of what the queue did.
export type LandVerdict =
  | { kind: "waiting"; on: string[] }
  | { kind: "ejected"; text: string }
  | { kind: "note"; text: string };

/// Read a stored `land_verdict`. `null` for absent or blank — a change
/// that has not been through the queue has nothing to say about it, and
/// a heading over an empty string is worse than no heading.
export function classifyLandVerdict(
  verdict: string | null | undefined,
): LandVerdict | null {
  const text = (verdict ?? "").trim();
  if (text === "") return null;
  if (text.startsWith(LAND_WAITING_PREFIX)) {
    const on = text
      .slice(LAND_WAITING_PREFIX.length)
      .split(",")
      .map((n) => n.trim())
      .filter((n) => n !== "");
    // `waiting on ` with nothing after it is a shape the lander does not
    // produce — it only writes the note when it has names. Falling back
    // to a plain note keeps the sentence on screen rather than rendering
    // a "waiting on" heading above an empty list.
    return on.length === 0 ? { kind: "note", text } : { kind: "waiting", on };
  }
  if (text.startsWith("ejected")) return { kind: "ejected", text };
  return { kind: "note", text };
}

/// Above this many rows the list starts collapsed.
///
/// GitHub collapses at around this point for the same reason: a checks
/// panel that pushes the merge box off the bottom of the screen has
/// stopped being a summary. Five fits beside the rest of the rail.
export const COLLAPSE_ABOVE = 5;

export function startsExpanded(count: number): boolean {
  return count <= COLLAPSE_ABOVE;
}

/// One row: state glyph, the state as a word, the name, whether it is
/// required, how long it took, and a way out to the provider's log.
function CheckRow(props: {
  check: PanelCheck;
  now: number;
  navigate: (to: string, replace?: boolean) => void;
}) {
  const { check, now } = props;
  // The refusal wins over the stored word. A blocked run mirrors as
  // `queued` on purpose, so the row would otherwise say "Queued" beside
  // the sentence explaining that nothing will ever pick it up.
  const pres = check.refusal
    ? BLOCKED_PRESENTATION
    : checkStatePresentation(check.state);
  const Icon = pres.icon;
  const duration = checkDurationLabel(check, now);
  return (
    <li className="flex items-start gap-2 py-1.5">
      <Icon
        aria-hidden
        className={cn("mt-0.5 size-4 shrink-0", pres.className)}
      />
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 items-center gap-1.5">
          <span
            className="truncate font-mono text-xs text-ink"
            title={check.name}
          >
            {check.name}
          </span>{" "}
          {check.required && (
            <Badge
              variant="neutral"
              className="shrink-0 px-1.5 py-0 text-[10px] uppercase tracking-wide"
            >
              Required
            </Badge>
          )}
        </div>
        <div className="mt-0.5 flex flex-wrap items-baseline gap-x-2 text-xs">
          <span className={pres.className}>{pres.label}</span>
          {duration !== null && (
            <span className="font-mono text-ink-3">{duration}</span>
          )}
          {check.posted_by && (
            <span className="text-ink-3">{check.posted_by}</span>
          )}
          {check.detail_url && (
            // The third place a `detail_url` is rendered, and it had the
            // same defect as the other two: `_blank` with `nofollow ugc`
            // on every row, including the ones pointing at our own
            // workflow-run page. `posted_by` is the server's word for who
            // wrote the row — a reporter cannot choose it — so it is the
            // provider gate one field over. See `DetailLink`.
            <DetailLink
              className="text-brand hover:underline"
              href={check.detail_url}
              provider={check.posted_by ?? ""}
              navigate={props.navigate}
              newTab
            >
              Details
            </DetailLink>
          )}
        </div>
        {check.refusal && (
          // Verbatim, and given its own line rather than a `title`: the
          // sentence is the only thing on the page that says why nothing
          // is running, and a tooltip is invisible to a reader who does
          // not already suspect there is something to hover.
          <p className="mt-1 whitespace-pre-wrap break-words text-xs text-ink-2">
            {check.refusal}
          </p>
        )}
      </div>
    </li>
  );
}

export interface ChangeChecksPanelProps {
  /// `null` while the request is in flight — distinct from `[]`, which
  /// means nothing has reported.
  checks: PanelCheck[] | null;
  /// Names the repo's policy requires on the target branch, if known.
  ///
  /// Read together with `checks`, never derived from it: a required check
  /// that has never reported has no row here to carry a `required` flag,
  /// and counting only the rows is how this panel came to say "All checks
  /// have passed" over a change the land queue was holding.
  required?: string[];
  /// Injected so the panel is answerable without a clock.
  now?: number;
  /// What the empty state names, e.g. `"patchset 3"`.
  scope?: string;
  /// How a row whose `detail_url` is one of our own pages navigates.
  /// Defaults to a full page load, which is the honest answer on the
  /// dashboard mount: the workflow-run page is a forge route and is not
  /// reachable from there client-side.
  navigate?: (to: string, replace?: boolean) => void;
}

export function ChangeChecksPanel(props: ChangeChecksPanelProps) {
  const checks = props.checks;
  const now = props.now ?? Date.now();
  const navigate =
    props.navigate ?? ((to: string) => window.location.assign(to));
  const [open, setOpen] = useState<boolean | null>(null);

  if (checks === null)
    return (
      <div className="rounded-lg border border-borderline bg-surface-1 p-4">
        <div className="text-sm font-medium text-ink">Checks</div>
        <p className="mt-1 text-xs text-ink-3">Loading checks…</p>
      </div>
    );

  const required = props.required ?? [];

  if (checks.length === 0)
    return (
      <div className="rounded-lg border border-borderline bg-surface-1 p-4">
        <div className="text-sm font-medium text-ink">Checks</div>
        <p className="mt-1 text-xs text-ink-3">
          No checks have reported on {props.scope ?? "this change"} yet.
        </p>
        {/* Silence and silence-that-blocks look identical without this.
            A change whose branch requires a check nobody has posted will
            sit in the land queue, and "nothing has reported" reads as
            "nothing needed to". */}
        {required.length > 0 && (
          <p className="mt-1 text-xs text-ink-3">
            This branch requires{" "}
            <span className="font-mono text-ink-2">{required.join(", ")}</span>{" "}
            before a change can land.
          </p>
        )}
      </div>
    );

  const summary = summarize(checks, required);
  const pres = headlinePresentation(summary.outcome);
  const Icon = pres.icon;
  const expanded = open ?? startsExpanded(checks.length);
  const counts = countLine(summary);

  return (
    <div className="rounded-lg border border-borderline bg-surface-1">
      <div className="flex items-start gap-2 p-4">
        <Icon
          aria-hidden
          className={cn("mt-0.5 size-4 shrink-0", pres.className)}
        />
        <div className="min-w-0 flex-1">
          <div className="text-sm font-medium text-ink">
            {headline(summary)}
          </div>
          {counts && <div className="mt-0.5 text-xs text-ink-3">{counts}</div>}
        </div>
        <button
          type="button"
          className="shrink-0 rounded-md px-1.5 py-0.5 text-xs text-ink-2 hover:text-brand focus-visible:ring-2 focus-visible:ring-brand"
          aria-expanded={expanded}
          onClick={() => setOpen(!expanded)}
        >
          {expanded ? (
            <ChevronDown aria-hidden className="inline size-3.5" />
          ) : (
            <ChevronRight aria-hidden className="inline size-3.5" />
          )}{" "}
          {expanded ? "Hide" : `Show ${checks.length}`}
        </button>
      </div>
      {expanded && (
        <ul className="border-t border-borderline px-4 py-1">
          {checks.map((c) => (
            <CheckRow
              key={`${c.source ?? ""}:${c.name}`}
              check={c}
              now={now}
              navigate={navigate}
            />
          ))}
        </ul>
      )}
    </div>
  );
}

/// The "why not" under a disabled land button.
///
/// Renders nothing when there is nothing to say, so a caller can hand it
/// an empty list unconditionally.
///
/// `pending` draws the rows as the Checks panel draws a running check —
/// the dashed circle — instead of the red cross. A changeset's
/// `waiting_on` is a list of checks that have not *finished*, and under
/// a verdict that said "waiting" the end-to-end pass showed each of them
/// with the glyph this product uses for "failed".
export function LandBlockers(props: { blockers: string[]; pending?: boolean }) {
  if (props.blockers.length === 0) return null;
  const Glyph = props.pending ? CircleDashed : X;
  return (
    <ul className="space-y-1 text-xs text-ink-2">
      {props.blockers.map((b) => (
        <li key={b} className="flex items-start gap-1.5">
          <Glyph
            aria-hidden
            className={cn(
              "mt-0.5 size-3.5 shrink-0",
              props.pending ? "text-warning" : "text-serious",
            )}
          />
          {/* A blocker names checks, and a workflow name is arbitrarily
              long with no spaces in it to wrap at. */}
          <span className="min-w-0 break-words">{b}</span>
        </li>
      ))}
    </ul>
  );
}
