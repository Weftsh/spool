// The Changes tab: stack-native review for one repo. A change is one
// commit's review identity (Change-Id keyed); approvals attach to its
// latest patchset; the verdict panel renders the sufficiency engine's
// explanations verbatim — the same words the API and the land queue use.

import { useCallback, useEffect, useRef, useState } from "react";
import {
  api,
  ApiError,
  type AuthorAssociation,
  type Change,
  type ChangeAssociations,
  type ChangeCheck,
  type ChangeComment,
  type ChangeDetail,
  type ChangeVerdict,
  type DiffEntry,
  type Me,
  type RequiredReviewer,
  type ReviewVerdictKind,
  type ReviewerSet,
  type Session,
  type WorkflowRun,
  requiredReviewers,
} from "@/api";
import { formatAgo } from "@/format";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Markdown } from "@/components/markdown";
import { CommentComposer } from "@/views/change/composer";
import { FilesPanel } from "@/views/change/files-panel";
import {
  anchorLabel,
  groupThreads,
  resolveStanding,
  threadSummary,
  threadsSupported,
  unresolvedCount,
  type Thread,
  type ThreadComment,
} from "@/views/change/threads";
import {
  batchedReviewSupported,
  draftComments,
  isDraft,
  readBlocks,
} from "@/views/change/review";
import {
  PendingReviewBar,
  ReviewSheet,
  StandingBlocks,
} from "@/views/change/review-sheet";
import {
  applyAnchor,
  displayAnchor,
  replacedLines,
  splitSuggestions,
  suggestionsIn,
  suggestionsSupported,
  type SuggestionAnchor,
} from "@/views/change/suggestions";
import {
  SuggestionActions,
  SuggestionBatchBar,
  SuggestionMiniDiff,
  SuggestionRefusal,
  type SuggestionActors,
} from "@/views/change/suggestion-view";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableHeadRow,
  TableRow,
} from "@/components/ui/table";
import { cn } from "@/lib/utils";
import { STRUCTURAL_LINK } from "@/lib/links";
import { dash, href } from "@/router";
import {
  approvableRuns,
  blockReason,
  panelRowRefusal,
  refusalsByFile,
} from "@/lib/hosted-runs";
import {
  ChangeChecksPanel,
  LandBlockers,
  checkBlockers,
  classifyLandVerdict,
  hasFailingCheck,
  type PanelCheck,
} from "@/views/forge/change-checks";

/// "Sign in", as a link when the page knows where that is and as plain
/// words when it does not — the dashboard mount has no login route of
/// its own to point at, and a dead link is worse than none.
function SignInWord(props: { href?: string }) {
  return props.href ? (
    <a className={STRUCTURAL_LINK} href={props.href}>
      Sign in
    </a>
  ) : (
    <>Sign in</>
  );
}

function StateBadge(props: { state: Change["state"] }) {
  const tone: Record<Change["state"], string> = {
    open: "text-ink-2",
    landing: "text-warning",
    landed: "text-good",
    abandoned: "text-ink-3",
  };
  const glyph: Record<Change["state"], string> = {
    open: "○",
    landing: "◌",
    landed: "●",
    abandoned: "⊘",
  };
  return (
    <Badge className={cn("gap-1.5", tone[props.state])}>
      <span aria-hidden>{glyph[props.state]}</span> {props.state}
    </Badge>
  );
}

/// Where a voice stands with this repository.
///
/// A thread full of strangers is unreadable without this: "looks fine to
/// me" from the person who owns the org and the same words from somebody
/// who has never landed anything here are different sentences, and a
/// reader should not have to go and look up which is which. The words are
/// the server's; only the tone and the hint are chosen here.
const ASSOCIATION: Record<
  AuthorAssociation,
  { label: string; tone: string; hint: string }
> = {
  owner: {
    label: "Owner",
    tone: "text-brand",
    hint: "Owns the organization this repository belongs to",
  },
  member: {
    label: "Member",
    tone: "text-ink-2",
    hint: "Has a role on this repository",
  },
  contributor: {
    label: "Contributor",
    tone: "text-ink-2",
    hint: "Not a member, but has landed a change here before",
  },
  "first-time": {
    label: "First-time",
    tone: "text-accent",
    hint: "Has not landed anything here yet",
  },
};

function AssociationBadge(props: { value?: AuthorAssociation | null }) {
  if (!props.value) return null;
  const a = ASSOCIATION[props.value];
  // An unknown value from a newer server is nothing rather than a crash:
  // a badge is decoration on somebody else's sentence.
  if (!a) return null;
  return (
    <Badge className={a.tone} title={a.hint}>
      {a.label}
    </Badge>
  );
}

/// What to put beside a name on one comment: where the speaker stands,
/// and whether they are the person whose change this is.
///
/// Both, or neither, in one place — because they are read together and
/// having three copies of "which badge goes here" is how they drift.
function CommentBadges(props: {
  associations: ChangeAssociations | null;
  principal: string;
}) {
  const { associations, principal } = props;
  return (
    <>
      <AssociationBadge value={associations?.authors[principal]} />
      {associations?.author_principal === principal && (
        <Badge className="text-ink-2" title="Wrote the change under review">
          Author
        </Badge>
      )}
    </>
  );
}

/// A comment's words, as markdown.
///
/// They were rendered `whitespace-pre-wrap` for a long time while
/// `components/markdown.tsx` sat next door rendering READMEs, which is
/// the worst of both: people write review comments in markdown whatever
/// the box does with them, so a list of three points arrived as three
/// lines beginning with hyphens and a fenced block arrived as prose with
/// backticks in it. The renderer is the hardened one — same allowlisted
/// schemes, same relative-link refusal — and no `base` is passed on
/// purpose: a comment is not a file in the repository, so a relative
/// link in one has no place to resolve against and renders as text
/// rather than as a link somewhere unintended.
/// A **suggestion** inside those words is not markdown, and drawing it
/// as a fence loses the whole point of it: a reviewer writing one is
/// saying "these lines, instead of those", which is a diff. So the body
/// is cut into the prose around its suggestion blocks and the blocks
/// themselves — the prose still going through the very same `Markdown`,
/// never a fork of it, so a comment with no suggestion in it renders
/// exactly as it always has.
function CommentBody(props: {
  comment: ThreadComment;
  /// Where the lines it replaces come from, and null on a page that
  /// cannot apply anything — a mini-diff still draws, because knowing
  /// what is proposed is not a permission.
  suggest: SuggestionActors | null;
  className?: string;
}) {
  const segments = splitSuggestions(props.comment.body);
  // The overwhelmingly common case, kept byte-identical to what it was:
  // one `Markdown` over the whole body. Cutting a body with no
  // suggestion in it into segments would be a rendering change to every
  // comment on the forge in exchange for nothing.
  if (!segments.some((s) => s.kind === "suggestion")) {
    return (
      <Markdown
        source={props.comment.body}
        headingLevel={3}
        className={cn("text-sm", props.className)}
      />
    );
  }
  // Which lines it is about, whether or not anybody may apply it: a
  // drafted suggestion and one carrying two blocks are both drawn
  // against the lines they replace and neither gets a control.
  const anchor = displayAnchor(props.comment);
  const replaced = anchor ? (props.suggest?.replaced(anchor) ?? null) : null;
  return (
    <div className={cn("text-sm", props.className)}>
      {segments.map((s, i) =>
        s.kind === "text" ? (
          <Markdown key={i} source={s.text} headingLevel={3} />
        ) : (
          <SuggestionMiniDiff
            key={i}
            lines={s.lines}
            anchor={anchor}
            replaced={replaced}
          />
        ),
      )}
    </div>
  );
}

/// One comment's words, and — when it carries a suggestion this viewer
/// could actually commit — what to do about it.
///
/// Used for a thread's root and for its replies alike, because a reply
/// can carry a suggestion too: the server gives it the root's anchor
/// whole, so it is as appliable as the remark it hangs under. Drawing
/// the control on roots only would hide half of them, on exactly the
/// comments a conversation ends with.
function CommentWords(props: {
  comment: ThreadComment;
  actors: ThreadActors;
  className?: string;
}) {
  const suggest = props.actors.suggest;
  const anchor = applyAnchor(props.comment);
  return (
    <>
      <CommentBody
        comment={props.comment}
        suggest={suggest}
        className={props.className}
      />
      {anchor && suggest && (
        <div className="mt-1.5">
          <SuggestionActions
            commentId={props.comment.id}
            author={props.comment.author}
            anchor={anchor}
            actors={suggest}
          />
        </div>
      )}
    </>
  );
}

/// The byline over one comment: who, where they stand, which patchset,
/// and — for a thread root in the conversation panel — what it is about.
///
/// A reply prints no anchor of its own. It has none: the server gives a
/// reply its root's anchor whole and refuses one sent with it, so
/// repeating the file and line under every reply would be saying the
/// same thing five times about one thread.
function CommentByline(props: {
  comment: ThreadComment;
  associations: ChangeAssociations | null;
  showAnchor: boolean;
}) {
  const c = props.comment;
  const anchor = props.showAnchor ? anchorLabel(c) : null;
  return (
    <div className="flex flex-wrap items-baseline gap-2 text-xs">
      <span className="font-medium text-ink">{c.author}</span>
      <CommentBadges
        associations={props.associations}
        principal={c.author_principal}
      />
      {c.author_email && <span className="text-ink-3">{c.author_email}</span>}
      <span className="rounded-full bg-surface-2 px-2 py-0.5 text-ink-3">
        patchset {c.patchset}
      </span>
      {anchor && <span className="font-mono text-ink-3">{anchor}</span>}
      <span className="ml-auto text-ink-3">{formatAgo(c.created_at)}</span>
    </div>
  );
}

/// Everything a thread needs from the view around it, in one bag.
///
/// Eight props threaded by hand through two panels and a table row is
/// how one of them ends up missing on one of the two mounts — which is
/// exactly the bug this page has already been bitten by, when `me` was
/// passed on the forge and not on the dashboard and every question the
/// page asked about the viewer answered false on one of them.
interface ThreadActors {
  associations: ChangeAssociations | null;
  /// Whether the server behind this conversation knows about threads.
  /// False against a deployment older than migration 0050, where Reply
  /// and Resolve are routes that do not exist — so they are not drawn.
  threaded: boolean;
  me: Me | null;
  canWrite: boolean;
  standing: ReviewerStanding | null;
  signedIn: boolean;
  busy: boolean;
  /// Suggested changes: what a suggestion replaces, and what may be
  /// done about it. **Null** when this server cannot apply one at all —
  /// a deployment older than migration 0050 sends no `side`, so no
  /// comment names lines the apply route could replace and no control
  /// is drawn. Not the same question as `canApply` inside it, which is
  /// about this viewer rather than this server.
  suggest: SuggestionActors | null;
  /// Resolves true when the reply actually landed. A composer that
  /// closed on the promise settling would throw away somebody's words
  /// over a refusal.
  onReply: (parentId: string, body: string) => Promise<boolean>;
  onResolve: (commentId: string, resolved: boolean) => void;
}

/// One thread: a remark, its replies, and what this reader may do about
/// it.
///
/// Replies are one level deep because the server refuses a reply to a
/// reply, so a reply is drawn with no Reply control of its own — a
/// button that exists only to produce "replies are one level deep" is
/// the show-and-fail this page keeps avoiding.
///
/// **Resolution is a fact, not a gate.** Nothing about landing consults
/// it, and no sentence here may suggest it does.
function ThreadCard(props: {
  thread: Thread;
  actors: ThreadActors;
  /// Whether to print the file and line. False inside the diff, where
  /// the thread is already sitting on the line it names.
  showAnchor?: boolean;
}) {
  const { thread, actors } = props;
  const root = thread.root;
  // Still only this reader's. Nobody else has been shown a word of it,
  // so it is not yet a conversation: Reply and Resolve are both about a
  // remark somebody could have read, and neither is offered here. What
  // *is* offered is the sentence saying so — a drafted remark that
  // looked exactly like a published one is how a review gets forgotten
  // in a tab, with the page giving no sign that nothing was sent.
  const pending = isDraft(root);
  const [replying, setReplying] = useState(false);
  const [reply, setReply] = useState("");
  // A resolved thread collapses, but stays openable: "dealt with" is not
  // "deleted", and the next reviewer reading the change wants to know
  // what was decided, not only that something was.
  const [expanded, setExpanded] = useState(false);
  const may =
    actors.threaded && !pending
      ? resolveStanding(root, actors.me, actors.canWrite, actors.standing)
      : { may: false, why: null };
  const resolvedWord = thread.resolvedBy
    ? `Resolved by ${thread.resolvedBy}`
    : "Resolved";

  const control = may.may ? (
    <Button
      type="button"
      size="xs"
      variant="ghost"
      className="py-0.5"
      // Named, because a page can carry a dozen of these and "Resolve"
      // on its own says nothing about which thread it settles.
      aria-label={`${thread.resolved ? "Reopen" : "Resolve"} thread from ${root.author}`}
      disabled={actors.busy}
      onClick={() => actors.onResolve(root.id, !thread.resolved)}
    >
      {thread.resolved ? "Reopen" : "Resolve"}
    </Button>
  ) : null;

  if (thread.resolved && !expanded) {
    return (
      <div className="flex flex-wrap items-baseline gap-x-2 gap-y-1 text-xs">
        <span aria-hidden className="text-good">
          ✓
        </span>
        <span className="text-ink-2">{resolvedWord}</span>
        <span className="min-w-0 truncate text-ink-3">
          {root.author}: {threadSummary(root)}
        </span>
        <button
          type="button"
          className={cn(STRUCTURAL_LINK, "ml-auto")}
          aria-label={`Show resolved thread from ${root.author}`}
          onClick={() => setExpanded(true)}
        >
          Show
        </button>
        {control}
      </div>
    );
  }

  return (
    <div className="space-y-2">
      <div>
        <CommentByline
          comment={root}
          associations={actors.associations}
          showAnchor={props.showAnchor ?? true}
        />
        {pending && (
          // Said in words on the remark itself, not only in the bar at
          // the top of the page: the reviewer reading their own draft
          // three hundred lines into a diff is nowhere near the bar.
          <p className="mt-1 flex flex-wrap items-baseline gap-2 text-xs">
            <Badge className="gap-1.5 text-brand">
              <span aria-hidden>◇</span> Pending
            </Badge>
            <span className="text-ink-3">
              Only you can see this until you submit your review.
            </span>
          </p>
        )}
        <CommentWords comment={root} actors={actors} className="mt-1.5" />
      </div>
      {thread.replies.length > 0 && (
        // Indented under a rule rather than boxed: a reply is part of
        // one remark, and a second card would read as a second remark.
        <ul className="space-y-2 border-l-2 border-borderline pl-3">
          {thread.replies.map((r) => (
            <li key={r.id}>
              <CommentByline
                comment={r}
                associations={actors.associations}
                showAnchor={false}
              />
              <CommentWords comment={r} actors={actors} className="mt-1.5" />
            </li>
          ))}
        </ul>
      )}
      <div className="flex flex-wrap items-center gap-2">
        {thread.resolved && (
          <span className="text-xs text-good">
            <span aria-hidden>✓</span> {resolvedWord}
          </span>
        )}
        {actors.threaded && actors.signedIn && !pending && !replying && (
          <Button
            type="button"
            size="xs"
            variant="ghost"
            className="py-0.5"
            aria-label={`Reply to ${root.author}`}
            onClick={() => {
              setReplying(true);
              setReply("");
            }}
          >
            Reply
          </Button>
        )}
        {control}
      </div>
      {replying && (
        <form
          onSubmit={(e) => {
            e.preventDefault();
            const body = reply.trim();
            if (!body) return;
            // No anchor of any kind: the server gives a reply its root's
            // path, line, range and side, and refuses one that sends its
            // own. Sending an anchor "for completeness" is how a thread
            // ends up describing two places at once.
            void actors.onReply(root.id, body).then((ok) => {
              if (!ok) return;
              setReplying(false);
              setReply("");
            });
          }}
        >
          <CommentComposer
            value={reply}
            onChange={setReply}
            label={`Reply to ${root.author}`}
            placeholder="Add to this thread"
            autoFocus
          >
            <div className="flex gap-2">
              <Button
                type="submit"
                size="xs"
                variant="outline"
                disabled={actors.busy || !reply.trim()}
              >
                Post reply
              </Button>
              <Button
                type="button"
                size="xs"
                variant="ghost"
                onClick={() => {
                  setReplying(false);
                  setReply("");
                }}
              >
                Cancel
              </Button>
            </div>
          </CommentComposer>
        </form>
      )}
    </div>
  );
}

/// Where a change's commits come from, and what it means to land on.
///
/// GitHub collects exactly four things on its compare page — base
/// repository, base branch, head repository, compare branch — and hides
/// the two repository pickers behind a "compare across forks" link so
/// the common case stays two fields. `POST …/changes` has accepted the
/// same four since forks landed (`repo`, `target`, `source`, `from`),
/// and this form sent one of them.
///
/// The cost was not a missing convenience. It was that the entire
/// contribute-without-write-access path — fork, push, open a change
/// against upstream, which `fork_pr_e2e.rs` covers end to end — had no
/// interface at all, and the server's refusal told people to "open the
/// change with `source` naming your fork": a REST field, in backticks,
/// with nothing on screen corresponding to it.
///
/// Worse, the form was drawn for signed-out visitors, who cannot open a
/// change under any circumstances, and refused them only after they had
/// filled it in.
function StartReview(props: {
  session: Session;
  repo: string;
  canWrite: boolean;
  signedIn: boolean;
  defaultBranch?: string;
  loginHref?: string;
  busy: boolean;
  error: string | null;
  onStart: (from: string, opts: { source?: string; target?: string }) => void;
}) {
  const { canWrite, signedIn } = props;
  const [fromBranch, setFromBranch] = useState("");
  const [sourceRepo, setSourceRepo] = useState("");
  const [target, setTarget] = useState("");
  // Whether the fork field is showing. `null` means "nobody has said" —
  // follow the default, which is open for somebody who cannot write,
  // because for them it is not an alternative route but the only one,
  // and a disclosure they have to find first is the bug this whole form
  // is fixing.
  //
  // Deliberately not `useState(!canWrite)`. `canWrite` arrives with the
  // repository row, one request after this panel first paints, so a
  // seeded initial value is computed from `false` — and a maintainer
  // with full write access landed on the Changes tab to find the fork
  // field already open, asking which fork their own commits were in.
  // A `useState` initialiser runs once; the answer arrives later.
  const [forkOverride, setForkOverride] = useState<boolean | null>(null);
  const fromFork = forkOverride ?? !canWrite;
  const setFromFork = (v: boolean) => setForkOverride(v);
  const [forks, setForks] = useState<string[] | null>(null);

  // The forks of this repository, to suggest in the picker. GitHub uses
  // a dropdown; this is a text field with suggestions, because the
  // listing is public forks only and a contributor's fork may be
  // private — so the field must still accept one that is not offered.
  useEffect(() => {
    if (!fromFork || forks !== null) return;
    let alive = true;
    api
      .forks(props.session, props.repo)
      .then((r) => alive && setForks(r.forks.map((f) => `${f.org}/${f.name}`)))
      // A suggestion list that could not be read is not an error worth
      // showing: the field works without it.
      .catch(() => alive && setForks([]));
    return () => {
      alive = false;
    };
  }, [fromFork, forks, props.session, props.repo]);

  if (!signedIn) {
    return (
      <div className="rounded-lg border border-borderline bg-surface-1 p-4">
        <div className="mb-1 text-sm font-medium text-ink">
          Propose a change
        </div>
        <p className="text-xs text-ink-3">
          Opening a change needs an account.{" "}
          {props.loginHref && (
            <a className={STRUCTURAL_LINK} href={props.loginHref}>
              Sign in
            </a>
          )}
          {props.loginHref && " to propose one. "}
          You do not need write access: fork this repository, push a branch to
          your fork, and open the change from there.
        </p>
      </div>
    );
  }

  const canSubmit =
    !!fromBranch.trim() && (!fromFork || !!sourceRepo.trim()) && !props.busy;

  return (
    <div className="rounded-lg border border-borderline bg-surface-1 p-4">
      <div className="mb-1 text-sm font-medium text-ink">
        {canWrite ? "Start a review" : "Propose a change"}
      </div>
      <p className="mb-3 text-xs text-ink-3">
        {canWrite ? (
          <>
            The branch tip becomes the change&apos;s patchset, keyed by its
            Change-Id trailer. Push a revision and register it again to add
            patchset 2.
          </>
        ) : (
          <>
            You do not have write access here, which is what forks are for: push
            your branch to a fork you own and name it below. The branch tip
            becomes the change&apos;s patchset, keyed by its Change-Id trailer.
          </>
        )}
      </p>
      <form
        className="space-y-3"
        onSubmit={(e) => {
          e.preventDefault();
          if (!canSubmit) return;
          props.onStart(fromBranch.trim(), {
            source: fromFork ? sourceRepo.trim() : undefined,
            target: target.trim() || undefined,
          });
        }}
      >
        {fromFork && (
          <div className="flex flex-col gap-1">
            <label
              className="text-xs font-medium text-ink-2"
              htmlFor="review-source"
            >
              Fork the commits are in
            </label>
            <input
              id="review-source"
              className="w-72 rounded-md border border-borderline bg-surface-0 px-3 py-1.5 text-sm"
              list={
                forks && forks.length > 0 ? "review-source-forks" : undefined
              }
              placeholder="owner/name"
              value={sourceRepo}
              onChange={(e) => setSourceRepo(e.target.value)}
            />
            {forks && forks.length > 0 && (
              <datalist id="review-source-forks">
                {forks.map((f) => (
                  <option key={f} value={f} />
                ))}
              </datalist>
            )}
          </div>
        )}
        <div className="flex flex-wrap items-end gap-2">
          <div className="flex flex-col gap-1">
            <label
              className="text-xs font-medium text-ink-2"
              htmlFor="review-from"
            >
              Branch to review
            </label>
            <input
              id="review-from"
              className="w-64 rounded-md border border-borderline bg-surface-0 px-3 py-1.5 text-sm"
              placeholder="branch, e.g. feature"
              value={fromBranch}
              onChange={(e) => setFromBranch(e.target.value)}
            />
          </div>
          <div className="flex flex-col gap-1">
            <label
              className="text-xs font-medium text-ink-2"
              htmlFor="review-target"
            >
              Land on
            </label>
            <input
              id="review-target"
              className="w-48 rounded-md border border-borderline bg-surface-0 px-3 py-1.5 text-sm"
              // The server falls back to the repository default when this
              // is absent, so an empty field is a real choice and not a
              // missing one — the placeholder says which branch that is.
              placeholder={props.defaultBranch ?? "default branch"}
              value={target}
              onChange={(e) => setTarget(e.target.value)}
            />
          </div>
          <Button type="submit" variant="outline" disabled={!canSubmit}>
            {props.busy ? "Registering…" : "Start review"}
          </Button>
        </div>
      </form>
      {canWrite && (
        // Only offered to somebody who has a choice. For everybody else
        // the fork field is already the form.
        <button
          type="button"
          className={cn(STRUCTURAL_LINK, "mt-3 text-xs")}
          onClick={() => setFromFork(!fromFork)}
        >
          {fromFork
            ? "The commits are already in this repository"
            : "Propose from a fork instead"}
        </button>
      )}
      <ErrLine message={props.error} />
    </div>
  );
}

export function ChangesPanel(props: {
  session: Session;
  repo: string;
  /// Which change is open, when the caller wants that to be part of the
  /// address rather than local state.
  ///
  /// Optional so the dashboard keeps working exactly as it did — it
  /// opens a change by clicking one and closes it with Back, and the URL
  /// never changes. The forge needs the opposite: a change is something
  /// you are *sent*, by a notification mail among other things, so its
  /// address has to name it. Controlled when `selectedKey` is passed,
  /// uncontrolled when it is not.
  selectedKey?: string | null;
  onSelect?: (key: string | null) => void;
  /// Whether this viewer may push here, from the server's own answer.
  ///
  /// Defaults to `true` for the dashboard, which only ever lists
  /// repositories in a namespace the viewer belongs to and had no way to
  /// ask before `viewer_write` existed. The forge passes the real answer,
  /// because that is where a stranger arrives.
  canWrite?: boolean;
  /// Defaults to `true` for the same reason: nothing reaches the
  /// dashboard without a session.
  signedIn?: boolean;
  /// The repository's default branch, offered as the landing target.
  defaultBranch?: string;
  /// Where "Sign in" goes, with a `next` back to this page.
  loginHref?: string;
  /// Who is looking, when a browser session says. A change's author is
  /// allowed one thing a reader is not — to withdraw it — and the page
  /// can only offer that when it knows who the author is talking to.
  me?: Me | null;
  /// How an in-app link inside a change navigates — the checks panel's
  /// links to our own hosted-run pages. Absent on the dashboard mount,
  /// where that page is not a route and a full load is the honest
  /// answer.
  navigate?: (to: string, replace?: boolean) => void;
}) {
  const { session, repo } = props;
  const [changes, setChanges] = useState<Change[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ownSelection, setOwnSelection] = useState<string | null>(null);
  const controlled = props.selectedKey !== undefined;
  const selected = controlled ? (props.selectedKey ?? null) : ownSelection;
  const setSelected = (key: string | null) => {
    if (props.onSelect) props.onSelect(key);
    if (!controlled) setOwnSelection(key);
  };
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(() => {
    let alive = true;
    api
      .changes(session, repo)
      .then((c) => alive && setChanges(c))
      .catch((e) => alive && setError(String(e)));
    return () => {
      alive = false;
    };
  }, [session, repo]);

  useEffect(refresh, [refresh]);

  async function startReview(
    from: string,
    opts: { source?: string; target?: string },
  ) {
    setBusy(true);
    setError(null);
    try {
      const out = await api.createChange(session, repo, from, opts);
      refresh();
      setSelected(out.change.key);
    } catch (e) {
      setError(`Could not start review: ${e instanceof Error ? e.message : e}`);
    } finally {
      setBusy(false);
    }
  }

  if (selected) {
    return (
      <ChangeView
        session={session}
        repo={repo}
        changeKey={selected}
        canWrite={props.canWrite ?? true}
        signedIn={props.signedIn ?? true}
        loginHref={props.loginHref}
        me={props.me ?? null}
        navigate={props.navigate}
        onBack={() => {
          setSelected(null);
          refresh();
        }}
      />
    );
  }
  if (error && !changes) return <ErrLine message={error} />;
  if (!changes) return <p className="text-sm text-ink-3">Loading…</p>;

  return (
    <div className="space-y-4">
      <StartReview
        session={session}
        repo={repo}
        canWrite={props.canWrite ?? true}
        signedIn={props.signedIn ?? true}
        defaultBranch={props.defaultBranch}
        loginHref={props.loginHref}
        busy={busy}
        error={error}
        onStart={startReview}
      />

      {changes.length === 0 ? (
        <p className="text-sm text-ink-3">
          No changes yet. Reviews here are per commit: push a branch and start
          one above.
        </p>
      ) : (
        <div className="overflow-hidden rounded-lg border border-borderline bg-surface-1">
          <Table>
            <TableHeader>
              <TableHeadRow>
                <TableHead className="px-3 py-2.5">Change</TableHead>
                <TableHead className="px-3 py-2.5">Title</TableHead>
                <TableHead className="px-3 py-2.5">State</TableHead>
                <TableHead className="px-3 py-2.5 text-right">
                  Patchset
                </TableHead>
                <TableHead className="px-3 py-2.5">Verdict</TableHead>
                <TableHead className="px-3 py-2.5 text-right">
                  Updated
                </TableHead>
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {changes.map((c) => (
                <TableRow
                  key={c.key}
                  className="cursor-pointer hover:bg-surface-2 focus-within:bg-surface-2"
                  onClick={() => setSelected(c.key)}
                >
                  <TableCell className="max-w-40 truncate px-3 py-2.5 font-mono text-xs">
                    <button
                      type="button"
                      className="hover:underline"
                      onClick={(e) => {
                        e.stopPropagation();
                        setSelected(c.key);
                      }}
                    >
                      {c.key}
                    </button>
                  </TableCell>
                  <TableCell className="max-w-64 truncate px-3 py-2.5">
                    {c.title}
                  </TableCell>
                  <TableCell className="px-3 py-2.5">
                    <StateBadge state={c.state} />
                  </TableCell>
                  <TableCell className="px-3 py-2.5 text-right font-mono">
                    {c.patchset?.number ?? "—"}
                  </TableCell>
                  <TableCell className="max-w-72 truncate px-3 py-2.5 text-xs text-ink-2">
                    {c.land_verdict ?? "—"}
                  </TableCell>
                  <TableCell className="px-3 py-2.5 text-right text-xs text-ink-3">
                    {formatAgo(c.updated_at)}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      )}
    </div>
  );
}

function ErrLine(props: { message: string | null }) {
  if (!props.message) return null;
  return (
    <p className="mt-2 text-sm text-serious" role="alert">
      {props.message}
    </p>
  );
}

/// "Approve and run workflows": the one control that starts a fork's
/// build.
///
/// Its own panel rather than another button in the Actions list, because
/// it is not an action on the *change* — it does not approve the code,
/// it does not land it, and a reviewer who confuses it with the patchset
/// approval two rows below has agreed to something they did not mean to.
/// A fork's workflow runs a stranger's code on our runners, and the
/// panel says so in as many words before the button.
///
/// **What it shows a reader who may not press it.** The reasons are
/// rendered for everybody — an author watching their own fork change sit
/// there needs to know it is waiting on a maintainer, and telling them
/// nothing is how the page becomes a mystery. The button is what
/// `repo:write` gates, and its absence is explained rather than silent.
function ApproveWorkflows(props: {
  runs: WorkflowRun[];
  canWrite: boolean;
  busy: boolean;
  onApprove: () => void;
}) {
  const [confirming, setConfirming] = useState(false);
  const n = props.runs.length;
  return (
    <div className="rounded-lg border border-warning/40 bg-warning/5 p-4">
      <div className="mb-1 text-sm font-medium text-ink">
        {n === 1 ? "A workflow is" : `${n} workflows are`} waiting for approval
      </div>
      <ul className="mb-3 space-y-1.5">
        {props.runs.map((r) => (
          <li key={r.id} className="text-xs">
            <span className="font-mono text-ink-2">{r.file}</span>
            {/* Verbatim. Three different refusals reach this page as one
                state, and the sentence is the only thing that says which
                — over budget, suspended, or waiting on a person. */}
            <p className="mt-0.5 whitespace-pre-wrap break-words text-ink-3">
              {blockReason(r)}
            </p>
          </li>
        ))}
      </ul>
      {props.canWrite ? (
        confirming ? (
          <div className="flex flex-wrap items-center gap-2">
            {/* Two steps, and the second one says what is actually being
                agreed to. `window.confirm` is not available to this
                dashboard, and it would be the wrong shape anyway: the
                sentence a reviewer needs is longer than a dialog title. */}
            <span className="text-xs text-ink-2">
              This runs code from a fork on our runners.
            </span>
            <Button
              type="button"
              size="xs"
              disabled={props.busy}
              onClick={props.onApprove}
            >
              Run them
            </Button>
            <Button
              type="button"
              size="xs"
              variant="ghost"
              disabled={props.busy}
              onClick={() => setConfirming(false)}
            >
              Not yet
            </Button>
          </div>
        ) : (
          <Button
            type="button"
            variant="outline"
            size="xs"
            disabled={props.busy}
            onClick={() => setConfirming(true)}
          >
            Approve and run workflows
          </Button>
        )
      ) : (
        <p className="text-xs text-ink-3">
          Somebody who can land this change has to approve running its
          workflows.
        </p>
      )}
    </div>
  );
}

/// What the sidebar needs to say about the computed reviewer set.
export interface ReviewerStanding {
  reviewers: RequiredReviewer[];
  /// How many of them have approved this patchset.
  approved: number;
  /// The sentence to print in place of a list, or null when there is a
  /// list to print.
  note: string | null;
  /// The viewer is one of the people OWNERS names.
  viewerRequired: boolean;
  /// ...and their approval already stands on this patchset.
  viewerApproved: boolean;
}

/// Read the reviewer set the way the card renders it.
///
/// Pure and exported because the cases worth pinning need no browser:
/// two of them produce an **empty list for opposite reasons** — a `*`
/// rule means anyone with write may approve, no rule at all means
/// nothing here is owned — and printing "nobody is required" for either
/// would be the same wrong sentence twice. The third is the viewer
/// finding themselves on the list, which is the whole reason a computed
/// reviewer set beats a nominated one and is worth saying out loud.
///
/// `null` in, `null` out: an older server sends no reviewer set at all,
/// and a page that turned silence into "nobody is required" would be
/// making the claim the server declined to make.
export function reviewerStanding(
  set: ReviewerSet | null,
  meId: string | null,
): ReviewerStanding | null {
  if (!set) return null;
  const mine =
    meId === null ? undefined : set.required.find((r) => r.user_id === meId);
  return {
    reviewers: set.required,
    approved: set.required.filter((r) => r.approved).length,
    note:
      set.required.length > 0
        ? null
        : set.anyone_with_write
          ? "No one person is required: a * entry in OWNERS lets anyone with write access approve these files."
          : "No OWNERS rule governs what this patchset touches, so any approval with write access satisfies it.",
    viewerRequired: mine !== undefined,
    viewerApproved: mine?.approved ?? false,
  };
}

/// Who this change is waiting on, and why it is those people.
///
/// There is deliberately no control here. Every reviewer list on every
/// other forge is also a request form, and the moment it is, the set
/// stops being what the repository decided and becomes what the author
/// picked — which is the thing this product does differently. The card
/// says where the list came from instead of offering to change it.
function RequiredReviewersCard(props: { standing: ReviewerStanding | null }) {
  const s = props.standing;
  // An older server says nothing about reviewers; so does the card.
  if (!s) return null;
  return (
    // A landmark, not a bare div: this is the panel somebody arrives at
    // the page to read, and it should be reachable as one region by a
    // reader who is not looking at the layout.
    <section
      aria-label="Required reviewers"
      className="rounded-lg border border-borderline bg-surface-1 p-4"
    >
      <div className="mb-1 text-sm font-medium text-ink">
        Required reviewers
      </div>
      <p className="mb-2 text-xs text-ink-3">
        Computed from the OWNERS files on the target branch. Nobody nominated
        this list and it cannot be edited here.
      </p>
      {s.viewerRequired && (
        <p className="mb-2 text-xs text-ink-2">
          <span aria-hidden>★</span> You are a required reviewer
          {s.viewerApproved
            ? " — your approval is already in."
            : "; this change is waiting on you."}
        </p>
      )}
      {s.note ? (
        <p className="text-sm text-ink-3">{s.note}</p>
      ) : (
        <>
          <p className="mb-2 text-xs text-ink-2">
            <span className="font-mono">{s.approved}</span> of{" "}
            <span className="font-mono">{s.reviewers.length}</span> approved
          </p>
          <ul className="space-y-1 text-sm">
            {s.reviewers.map((r) => (
              <li
                key={r.user_id}
                className="flex flex-wrap items-baseline gap-x-2"
              >
                {/* Glyph and word together: a tick that only differed by
                    colour would say nothing to a reader who cannot see
                    the difference. */}
                {r.approved ? (
                  <span aria-hidden className="text-good">
                    ✓
                  </span>
                ) : (
                  <span aria-hidden className="text-ink-3">
                    ○
                  </span>
                )}
                <span className={cn(!r.approved && "text-ink-2")}>
                  {r.name}
                </span>
                <span className="ml-auto text-xs text-ink-3">
                  {r.approved ? "approved" : "waiting"}
                </span>
                <span className="w-full truncate pl-5 text-xs text-ink-3">
                  {r.email}
                </span>
              </li>
            ))}
          </ul>
        </>
      )}
    </section>
  );
}

/// One change: patchsets, approvals, the verdict with its per-path
/// explanations, and the actions. After a land is queued, the view
/// polls the change itself — the observable whose transition the next
/// interaction needs — until it leaves `landing`.
function ChangeView(props: {
  session: Session;
  repo: string;
  changeKey: string;
  onBack: () => void;
  /// Whether this viewer holds `repo:write`, which is the same scope the
  /// land route demands — so it is also the answer to "may this person
  /// approve running a fork's workflows here". Subtractive: false until
  /// the repository row says otherwise.
  canWrite: boolean;
  /// Whether anyone is signed in at all. A stranger on a public
  /// repository can read every word of a review; the things they cannot
  /// do — comment, approve — are shown as a way in, not as buttons that
  /// answer 401 when pressed.
  signedIn: boolean;
  /// Where "Sign in" goes, with a `next` back to this page.
  loginHref?: string;
  /// The browser session's person, if there is one. Compared with the
  /// change's author to offer Abandon to somebody who cannot write here
  /// but opened the change — the server admits exactly that person.
  me: Me | null;
  /// How an in-app link navigates. See `DetailLink`.
  navigate?: (to: string, replace?: boolean) => void;
}) {
  const { session, repo, changeKey, signedIn, canWrite } = props;
  const [detail, setDetail] = useState<ChangeDetail | null>(null);
  const [verdict, setVerdict] = useState<ChangeVerdict | null>(null);
  const [files, setFiles] = useState<DiffEntry[] | null>(null);
  /// Which earlier patchset the file list is against: null is the
  /// parent, the ordinary reading; a number is "what moved since I read
  /// patchset N", which the server answers as an interdiff.
  const [baseline, setBaseline] = useState<number | null>(null);
  const [comments, setComments] = useState<ChangeComment[] | null>(null);
  const [checks, setChecks] = useState<ChangeCheck[] | null>(null);
  /// The repository's hosted runs, read only to answer "why is this
  /// check never going to start".
  ///
  /// A refused run mirrors into the checks list as `queued`, so the
  /// reason it will never run exists nowhere on this page unless the
  /// runs are fetched as well. Never allowed to fail the load: a
  /// deployment with no hosted runner answers nothing useful here and
  /// the review page must still render.
  const [runs, setRuns] = useState<WorkflowRun[]>([]);
  /// The names the target branch requires, which is **not** derivable
  /// from `checks`: a required check that has never reported has no row
  /// there at all. Without this the blockers list can only ever mention
  /// checks that already spoke.
  const [requiredChecks, setRequiredChecks] = useState<string[]>([]);
  const [viewed, setViewed] = useState<Set<string>>(new Set());
  const [associations, setAssociations] = useState<ChangeAssociations | null>(
    null,
  );
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [approveNote, setApproveNote] = useState<string | null>(null);
  const [commentBody, setCommentBody] = useState("");
  /// Whether the submit sheet is up. Local rather than derived from the
  /// draft count: a reviewer with nothing drafted still opens it — that
  /// is the only door to Approve on a server that has the feature —
  /// and one with twelve drafts is usually still writing.
  const [sheetOpen, setSheetOpen] = useState(false);
  /// The suggestions gathered for the next patchset, by comment id.
  ///
  /// Held here rather than per comment card because the whole feature
  /// is that several of them make **one** patchset: a set that lived
  /// inside a card could only ever apply the one it was in.
  const [batch, setBatch] = useState<Set<string>>(new Set());
  /// The apply route's own refusal, kept apart from `error` so it can be
  /// printed with nothing in front of it. "Could not apply the
  /// suggestions: applying a suggestion commits it to acme/app, which
  /// needs write access…" is the page apologising over the top of the
  /// answer.
  const [applyRefused, setApplyRefused] = useState<string | null>(null);
  /// The text of each file a suggestion stands in place of, keyed by
  /// `<commit>\n<path>` — the commit being the patchset the comment was
  /// *written against*, which is not always the one on screen. Reading
  /// the tip instead would draw the mini-diff against lines the
  /// reviewer never saw, which is precisely the staleness the server
  /// refuses in words.
  const [sources, setSources] = useState<Map<string, string | null>>(new Map());
  const sourcesAsked = useRef(new Set<string>());
  const pollTimer = useRef<number | null>(null);

  const load = useCallback(() => {
    let alive = true;
    api
      .changeDetail(session, repo, changeKey)
      .then(async (d) => {
        const latest = d.patchsets[d.patchsets.length - 1];
        const [v, cs, ck, fs, vw, as] = await Promise.all([
          api.changeVerdict(session, repo, changeKey),
          api.comments(session, repo, changeKey),
          api.checks(session, repo, changeKey),
          // A root commit has no parent rev to diff from; everything it
          // touches is an addition, which the verdict already lists.
          // Against an earlier patchset the server does the tree diff
          // between the two, in the same shape.
          baseline !== null && latest
            ? api.interdiff(session, repo, changeKey, baseline, latest.number)
            : latest?.parent
              ? api.structuralDiff(session, repo, latest.parent, latest.commit)
              : Promise.resolve(null as DiffEntry[] | null),
          // Both of these are read-only garnish on a review that works
          // without them: viewed marks belong to a signed-in person (a
          // service token has none, and answers 403 saying so), and
          // associations are a hint about other people. A refusal or an
          // older server must not take the diff and the verdict down
          // with it, so neither is allowed to reject.
          api.changeViews(session, repo, changeKey).catch(() => null),
          api.changeAssociations(session, repo, changeKey).catch(() => null),
        ]);
        if (!alive) return;
        setDetail(d);
        setVerdict(v);
        setComments(cs);
        setChecks(ck.checks);
        setRequiredChecks(ck.required);
        setViewed(new Set(vw?.viewed ?? []));
        setAssociations(as);
        setFiles(
          fs ??
            v.verdict.per_path.map((pp) => ({
              status: "added" as const,
              path: pp.path,
              old_oid: null,
              new_oid: null,
            })),
        );
      })
      .catch((e) => alive && setError(String(e)));
    return () => {
      alive = false;
    };
  }, [session, repo, changeKey, baseline]);

  useEffect(load, [load]);

  /// Read the files the suggestions in this conversation propose to
  /// change, so each block can be drawn against the lines it replaces.
  ///
  /// One request per file that actually carries a suggestion, and each
  /// asked once: `sourcesAsked` is keyed by commit and path, so a
  /// reviewer who left four suggestions in one file costs one read and
  /// a re-render of the conversation costs none. A failure is silent on
  /// purpose — the suggestion still renders, with its proposed lines
  /// and without the ones they stand in place of, which is strictly
  /// more than the fence it used to be.
  useEffect(() => {
    const at = new Map(
      (detail?.patchsets ?? []).map((p) => [p.number, p.commit]),
    );
    for (const c of comments ?? []) {
      const a = displayAnchor(c);
      if (!a || suggestionsIn(c.body).length === 0) continue;
      const commit = at.get(a.patchset);
      if (!commit) continue;
      const key = `${commit}\n${a.path}`;
      if (sourcesAsked.current.has(key)) continue;
      sourcesAsked.current.add(key);
      void api
        .fileText(session, repo, a.path, commit)
        .then((text) => setSources((m) => new Map(m).set(key, text)))
        .catch(() => setSources((m) => new Map(m).set(key, null)));
    }
  }, [comments, detail, session, repo]);

  /// The hosted runs for the tip, fetched **beside** the review rather
  /// than as part of it.
  ///
  /// It was in the load's `Promise.all` first, which made the entire
  /// change page — diff, verdict, comments, the land button — wait on a
  /// request that only annotates it. A deployment whose hosted-runner
  /// route is slow, or hung, showed "Loading…" over a review that had
  /// everything it needed; the page was held hostage by its own
  /// garnish. Separately fetched, a slow answer costs the reasons a
  /// moment and nothing else.
  ///
  /// Keyed on `detail` so that it re-reads after an action reloads the
  /// change: approving the workflows must take the panel away.
  useEffect(() => {
    const tip = detail?.patchsets[detail.patchsets.length - 1]?.commit;
    if (tip === undefined) return;
    let alive = true;
    api
      // Filters this server may not have yet: `list` reads `limit` and
      // ignores what it does not know, so sending them is harmless today
      // and exact the moment they land. Without them the tip's runs fall
      // out of the newest-first window on a busy repository and the
      // reason for a blocked check silently disappears — which looks
      // like the page working.
      .workflowRuns(session, repo, { limit: 100, changeKey, commitSha: tip })
      .then((rs) => alive && setRuns(rs))
      // A deployment with no hosted runner, an older server, or a
      // refusal: the review renders exactly as it did before any of this
      // existed, with nothing annotated.
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [session, repo, changeKey, detail]);
  useEffect(
    () => () => {
      if (pollTimer.current !== null) window.clearInterval(pollTimer.current);
    },
    [],
  );

  function pollWhileLanding() {
    if (pollTimer.current !== null) window.clearInterval(pollTimer.current);
    pollTimer.current = window.setInterval(async () => {
      try {
        const d = await api.changeDetail(session, repo, changeKey);
        setDetail(d);
        if (d.change.state !== "landing") {
          if (pollTimer.current !== null) {
            window.clearInterval(pollTimer.current);
            pollTimer.current = null;
          }
          const v = await api.changeVerdict(session, repo, changeKey);
          setVerdict(v);
        }
      } catch {
        /* transient; the next tick retries */
      }
    }, 1000);
  }

  /// Tick or untick one file for this reviewer.
  ///
  /// Optimistic, because the box is the reviewer's own note to
  /// themselves and a round trip between the click and the tick makes a
  /// forty-file change feel broken. A refusal puts the box back where it
  /// was and says so — a tick that silently did not persist would be the
  /// same lie the patchset rule exists to prevent.
  async function toggleViewed(path: string, next: boolean) {
    setViewed((prev) => {
      const now = new Set(prev);
      if (next) now.add(path);
      else now.delete(path);
      return now;
    });
    try {
      await api.setChangeViewed(session, repo, changeKey, path, next);
    } catch (e) {
      setViewed((prev) => {
        const back = new Set(prev);
        if (next) back.delete(path);
        else back.add(path);
        return back;
      });
      setError(
        `Could not mark ${path} ${next ? "viewed" : "unviewed"}: ${
          e instanceof Error ? e.message : e
        }`,
      );
    }
  }

  /// Approving is not `act`, because its interesting outcome is not an
  /// error. A 409 means nothing is blocked at the tip any more —
  /// somebody else approved these runs, or the fork pushed again and the
  /// tip moved under this page. Nothing went wrong and nothing wants
  /// retrying, so dressing it in "Could not start these workflows" sends
  /// a maintainer hunting a permission problem they do not have. The
  /// server's sentence is still repeated verbatim underneath, because it
  /// is the only thing that says which of the two it was.
  ///
  /// Every other status is a real failure and keeps the error line: over
  /// a 500, "nothing is waiting for approval any more" would tell
  /// somebody their approval landed when it did not.
  async function approve() {
    setBusy(true);
    setError(null);
    setApproveNote(null);
    try {
      await api.approveWorkflows(session, repo, changeKey);
      load();
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        setApproveNote(
          "Nothing here is waiting for approval any more — somebody " +
            `approved it, or the change has a new patchset. ${e.message}`,
        );
        load();
      } else {
        setError(
          `Could not start these workflows: ${
            e instanceof Error ? e.message : e
          }`,
        );
      }
    } finally {
      setBusy(false);
    }
  }

  /// Run one action, reload, and say whether it worked.
  ///
  /// The boolean is not decoration. A composer that closes on the
  /// promise settling closes over a refusal too, so a 400 from the
  /// server threw away what somebody had just written and left them
  /// looking at an error with no way back to their own words. Every
  /// caller that clears a draft now clears it only on `true`.
  async function act(
    fn: () => Promise<unknown>,
    verb: string,
  ): Promise<boolean> {
    setBusy(true);
    setError(null);
    try {
      await fn();
      load();
      return true;
    } catch (e) {
      setError(`Could not ${verb}: ${e instanceof Error ? e.message : e}`);
      return false;
    } finally {
      setBusy(false);
    }
  }

  /// Post the change-wide comment, either straight out or into the
  /// pending review.
  ///
  /// One function for both, because they differ by a single field on
  /// the wire and the thing that must not drift between them is what
  /// happens to the reviewer's words on a refusal: cleared on `true`
  /// only, exactly as every other composer on this page.
  function postChangeComment(pending: boolean) {
    const body = commentBody.trim();
    if (!body) return;
    void act(
      () =>
        api.addComment(
          session,
          repo,
          changeKey,
          body,
          undefined,
          undefined,
          pending ? { pending: true } : undefined,
        ),
      pending ? "draft the comment" : "comment",
    ).then((ok) => ok && setCommentBody(""));
  }

  /// Send the whole review as one request, then close the sheet.
  ///
  /// The sheet closes on success only. A refusal — `request_changes`
  /// with nothing in it, or a change that stopped being open under the
  /// reviewer — has to leave the verdict and the cover message where
  /// they were written, or the error line is an apology for words the
  /// page just threw away.
  function submitReview(verdictKind: ReviewVerdictKind, body: string) {
    void act(
      () =>
        api.submitReview(session, repo, changeKey, {
          verdict: verdictKind,
          // Omitted rather than sent empty: the server keeps whatever
          // the draft was saved with when no words arrive, and an empty
          // string would overwrite a cover message with nothing.
          ...(body ? { body } : {}),
        }),
      "submit the review",
    ).then((ok) => ok && setSheetOpen(false));
  }

  /// Take these suggestions, as one patchset.
  ///
  /// Not `act`, because its interesting outcome is a refusal written to
  /// be read: the apply route answers 403 naming who *can* push here,
  /// and 409 naming the two comments that overlap or the file that has
  /// moved since the patchset a comment anchors to. Each of those is an
  /// instruction to the person who pressed the button, and wrapping it
  /// in "Could not apply the suggestions" would put an apology in front
  /// of the answer. A refusal with no sentence in it — a proxy's 502,
  /// a dead connection — still gets the ordinary error line, because
  /// "502" on its own tells nobody anything.
  ///
  /// The batch is cleared only on success. A 409 over two overlapping
  /// comments is answered by dropping *one* of them, and a page that
  /// had already thrown the gathering away would make the reader
  /// rebuild it from memory.
  async function applySuggestions(ids: string[]) {
    setBusy(true);
    setError(null);
    setApplyRefused(null);
    try {
      await api.applySuggestions(session, repo, changeKey, ids);
      setBatch(new Set());
      load();
    } catch (e) {
      const said =
        e instanceof ApiError &&
        typeof (e.body as { error?: unknown } | null)?.error === "string"
          ? (e.body as { error: string }).error
          : null;
      if (said) setApplyRefused(said);
      else {
        setError(
          `Could not apply the suggestions: ${
            e instanceof Error ? e.message : e
          }`,
        );
      }
    } finally {
      setBusy(false);
    }
  }

  if (error && !detail) return <ErrLine message={error} />;
  if (!detail || !verdict)
    return <p className="text-sm text-ink-3">Loading…</p>;

  const c = detail.change;
  const landable = verdict.verdict.landable;
  const latest = detail.patchsets[detail.patchsets.length - 1];
  // The same comparison `SpeakerBadge` makes to say "author": the
  // associations name the author as `user:<id>` and `/auth/me` names the
  // viewer. Unknown until the associations arrive, and false — never a
  // guess — until then.
  const isAuthor =
    props.me !== null &&
    associations?.author_principal === `user:${props.me.id}`;
  // The server revokes only the viewer's own approval on the latest
  // patchset and answers 404 "no active approval to revoke" otherwise;
  // `detail.approvals` is exactly that set, so the button is live only
  // while the viewer's address is in it. It used to be offered, enabled,
  // to somebody who had approved nothing — a reader on a landed change —
  // and pressing it produced a refusal the page had given no warning of.
  const hasApproved =
    props.me !== null &&
    detail.approvals.some((a) => a.email === props.me?.email);
  // The computed reviewer set rides on the verdict, which this view
  // already fetches — the same OWNERS resolution that decided whether
  // the change is landable also decided who it is waiting on, so the
  // two can never disagree on the page.
  const standing = reviewerStanding(
    requiredReviewers(verdict),
    props.me?.id ?? null,
  );
  // `classifyCheck`, not `state === "failing"`. The merged read carries
  // both vocabularies: a commit-scoped row says `failure` or `cancelled`
  // where the patchset route said `failing`, and matching the one word
  // left the land button enabled over a red check — the server then
  // refused it with a 409 the page had given no warning of.
  const failingCheck = hasFailingCheck(checks ?? []);
  // The merged route has landed, so the panel gets the fields it was
  // written for. Run timestamps are still absent — neither writer records
  // a start on the change route — and the panel leaves the duration off
  // rather than inventing one.
  // Why a check will never start, joined on from the runs. Scoped to the
  // tip: approval is per tip, and a refusal from the previous patchset
  // has already been dealt with.
  const tip = latest?.commit;
  const refusals = refusalsByFile(runs, tip ?? "");
  const panelChecks: PanelCheck[] | null =
    checks === null
      ? null
      : checks.map((k) => ({
          name: k.name,
          state: k.state,
          detail_url: k.url,
          required: k.required,
          source: k.source,
          posted_by: k.posted_by,
          refusal: panelRowRefusal(k, refusals),
        }));
  // The blocked hosted runs at the tip that approving would actually
  // start — a fork's, and nothing else. Gated on the run's reason
  // *code*, never on reading the refusal's prose: three refusals arrive
  // as one state with only a sentence to tell them apart, and a button
  // that appears or vanishes on a wording change is a button nobody can
  // rely on.
  //
  // A fork change in a suspended or out-of-minutes organisation is
  // blocked with *that* reason, so it gets the sentence and no control:
  // approving it would trigger another run, which blocks again
  // identically, and the server answers 409 to somebody the page had
  // just invited to press a button. The reason still renders — see the
  // checks panel above — because the missing button must not make the
  // refusal any less visible.
  const approvableHere = tip ? approvableRuns(runs, tip) : [];
  // The run's code is the only gate. `c.source != null` used to stand
  // beside it and is gone: a run blocked with `"fork"` is waiting on the
  // maintainer reading this page whatever our own change record says
  // about where the branch came from, and two gates that can disagree
  // means the button disappears on the day they do with nothing on the
  // page to explain it. The server enforces that a blocked run always
  // carries a code and a live one never does.
  const forkAwaitingApproval = approvableHere.length > 0 && c.state === "open";
  // Why the land button is off, in the words of the thing that is in the
  // way. A control that is disabled with no reason beside it is the
  // defect this panel exists to fix.
  const landVerdict = classifyLandVerdict(c.land_verdict);
  // The conversation, folded into threads once and read by both panels.
  // Grouping it twice is how the sidebar and the diff end up disagreeing
  // about which remark a reply belongs to.
  const threads = groupThreads(comments);
  // Whether the server behind this conversation knows about threads at
  // all. Against one that predates migration 0050 every row would read
  // as an unresolved root, so the page says nothing rather than a wrong
  // number and offers no control to a route that is not there.
  const threaded = threadsSupported(comments);
  // A **fact** about the review, never a gate: `landBlockers` below
  // deliberately does not grow an entry for it. An unresolved remark is
  // somebody's unfinished sentence, and a forge that turns that into a
  // lock teaches people to resolve threads in order to get their change
  // out rather than because they were addressed.
  // Drafts do not count. The number is a fact about the *conversation*,
  // and a reviewer's unsent remark is not in it yet: counting it would
  // print a number nobody else on the change can see, beside one they
  // can, with nothing on the page saying which is which.
  const unresolved = threaded
    ? unresolvedCount(threads.filter((t) => !isDraft(t.root)))
    : 0;
  // Whether this server knows about batched reviews at all. Positive
  // signal only — see `batchedReviewSupported`. Against a deployment
  // older than migration 0051 everything below stays exactly as it was:
  // comments post immediately and Approve is its own button.
  const batched = batchedReviewSupported(detail) && signedIn;
  const drafts = batched ? draftComments(comments) : [];
  // The standing "no"s, and the server's own judgement of which of them
  // actually stop the change. Never re-derived here: `blocking` is an
  // OWNERS question about the paths this patchset touches, and a second
  // answer computed on the client would one day disagree with the one
  // the lander uses.
  const blocks = readBlocks(verdict, props.me);
  // Suggested changes, and the two questions the page keeps apart.
  //
  // **The server**: `suggestionsSupported` is a floor rather than a
  // capability — applying a suggestion added no column and no field, so
  // there is nothing new on any read response to detect. What *is*
  // detectable is that a deployment older than migration 0050 sends no
  // `side`, and the apply route cannot know which lines it would
  // replace without one, so nothing there is appliable. A 0050 server
  // without the route is not distinguished and cannot be from data on
  // this page; its refusal is rendered verbatim, which is the same
  // honest fallback the 403 gets.
  //
  // **The viewer**: `repo:write`, because the button makes a commit —
  // the same scope the route demands, asked before the trip so nobody
  // is handed a control that leads to a 403. And the change still being
  // open: a landed or abandoned one has no next patchset to make, and
  // the server says so with a 409.
  const suggest: SuggestionActors | null = suggestionsSupported(comments)
    ? {
        canApply: canWrite && signedIn && c.state === "open",
        selected: batch,
        onSelect: (id, on) =>
          setBatch((prev) => {
            const now = new Set(prev);
            if (on) now.add(id);
            else now.delete(id);
            return now;
          }),
        onApplyOne: (id) => void applySuggestions([id]),
        // The lines the reviewer was looking at, read at the patchset
        // they wrote against rather than at the tip.
        replaced: (a: SuggestionAnchor) => {
          const commit = detail.patchsets.find(
            (p) => p.number === a.patchset,
          )?.commit;
          if (!commit) return null;
          const text = sources.get(`${commit}\n${a.path}`);
          return typeof text === "string"
            ? replacedLines(text, a.start, a.end)
            : null;
        },
        busy,
      }
    : null;
  const actors: ThreadActors = {
    associations,
    threaded,
    me: props.me,
    canWrite,
    standing,
    signedIn,
    busy,
    suggest,
    onReply: (parentId, body) =>
      act(
        () =>
          api.addComment(session, repo, changeKey, body, undefined, undefined, {
            parent_id: parentId,
          }),
        "reply",
      ),
    onResolve: (commentId, resolved) =>
      void act(
        () =>
          api.setCommentResolved(session, repo, changeKey, commentId, resolved),
        resolved ? "resolve the thread" : "reopen the thread",
      ),
  };
  // Why Resolve is nowhere on the page, said once rather than beside
  // every thread. A reader who cannot settle anything needs the sentence
  // exactly once; twenty copies of it is noise over the review itself —
  // and it is only printed when the reader can settle *nothing*, because
  // "you may not" beside a page that is offering the button elsewhere is
  // worse than silence.
  // Not for a signed-out reader: the panel already ends with "Sign in to
  // join the conversation", and a second sentence saying the same thing
  // about a control they cannot see is one invitation too many.
  const standings = (threaded && signedIn ? threads : [])
    .filter((t) => !t.resolved)
    .map((t) => resolveStanding(t.root, props.me, canWrite, standing));
  const noResolve =
    standings.length > 0 && standings.every((s) => !s.may)
      ? standings[0].why
      : null;
  const landBlockers = [
    ...(landable ? [] : ["Review verdict not met — see the verdict above"]),
    ...checkBlockers(panelChecks ?? [], requiredChecks),
  ];

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-3">
        <Button
          type="button"
          variant="outline"
          className="py-1"
          onClick={props.onBack}
        >
          ← Changes
        </Button>
        <h2 className="truncate text-lg font-semibold tracking-tight">
          {c.title}
        </h2>
        <span className="font-mono text-xs text-ink-3">{c.key}</span>
        <StateBadge state={c.state} />
        <AssociationBadge value={associations?.author} />
      </div>

      {/* Where these commits came from. Null for the ordinary case —
          somebody with push access working in this repository — and a
          fork's `owner/name` for a contribution from outside, which is
          the first thing a reviewer needs to know and the reason the
          association badge beside the title is worth reading. */}
      {c.source && (
        <p className="text-xs text-ink-3">
          Proposed from <span className="font-mono text-ink-2">{c.source}</span>{" "}
          onto <span className="font-mono text-ink-2">{c.target_branch}</span>
        </p>
      )}

      {/* What the land queue is doing, in the queue's own words.
          Every branch of `land_verdict` is rendered, not just `ejected`.
          The waiting note exists because a change held on a slow build
          showed a spinner and no reason at all — and then this page
          filtered it out on the prefix, so the note was written, stored,
          returned, shown in the changes *list*, and dropped on the one
          page an author opens to find out why nothing is happening. With
          a 30-minute default wait budget that is half an hour of "All
          checks have passed" over a change that cannot land. */}
      {landVerdict?.kind === "waiting" && (
        <div className="rounded-lg border border-borderline bg-surface-1 p-4 text-sm">
          <div className="mb-1 flex items-center gap-2 font-medium">
            <span aria-hidden className="text-warning">
              ◌
            </span>
            {landVerdict.on.length === 1
              ? "The queue is holding this change for a check that has not reported"
              : "The queue is holding this change for checks that have not reported"}
          </div>
          <div className="font-mono text-xs text-ink-2">
            {landVerdict.on.join(", ")}
          </div>
          {/* The bound, said out loud. "It is waiting" without "and it
              will give up" leaves an author refreshing a page forever
              over a name that will never report — a typo in the required
              list looks exactly like a slow build until this sentence. */}
          <p className="mt-2 text-xs text-ink-3">
            It will land on its own when they pass. If nothing reports, the
            queue gives up and ejects the change with a reason.
          </p>
        </div>
      )}
      {landVerdict?.kind === "ejected" && (
        <div className="rounded-lg border border-borderline bg-surface-1 p-4 text-sm">
          <div className="mb-1 flex items-center gap-2 font-medium">
            <span aria-hidden className="text-serious">
              ▲
            </span>
            The queue ejected this change
          </div>
          <div className="font-mono text-xs text-ink-2">{landVerdict.text}</div>
        </div>
      )}
      <ErrLine message={error} />

      {/* Above the fold and outside the grid: a reviewer three hundred
          lines into a diff is nowhere near the sidebar, and a draft
          nobody submits is a review that never happened — with the page
          looking exactly like one where it did. */}
      {/* The gathered suggestions, and the route's own words when it
          refuses them. Both live here rather than beside any one
          comment: the suggestions being gathered are spread down a
          forty-file diff, and a refusal shown only next to the last
          button pressed is one an author scrolls past. */}
      {suggest?.canApply && batch.size > 0 && (
        <SuggestionBatchBar
          count={batch.size}
          busy={busy}
          onApply={() => void applySuggestions([...batch])}
          onClear={() => setBatch(new Set())}
        />
      )}
      <SuggestionRefusal message={applyRefused} />
      {batched && drafts.length > 0 && !sheetOpen && (
        <PendingReviewBar
          count={drafts.length}
          busy={busy}
          onFinish={() => setSheetOpen(true)}
        />
      )}
      {batched && sheetOpen && (
        <ReviewSheet
          drafts={drafts.length}
          busy={busy}
          onSubmit={submitReview}
          onDiscard={() =>
            void act(
              () => api.discardReview(session, repo, changeKey),
              "discard the review",
            ).then((ok) => ok && setSheetOpen(false))
          }
          onCancel={() => setSheetOpen(false)}
        />
      )}

      {/* The review is read in the main column; the decision lives in a
          sidebar that follows the reader. On small screens the decision
          rail comes first — verdict before scroll. */}
      <div className="flex flex-col-reverse gap-4 lg:grid lg:grid-cols-[minmax(0,1fr)_20rem] lg:items-start">
        <div className="min-w-0 space-y-4">
          <FilesPanel
            session={session}
            repo={repo}
            files={files}
            parent={
              baseline !== null
                ? (detail.patchsets.find((p) => p.number === baseline)?.commit ??
                  null)
                : (latest?.parent ?? null)
            }
            commit={latest?.commit ?? null}
            threads={threads}
            unresolved={unresolved}
            renderThread={(t) => (
              <ThreadCard thread={t} actors={actors} showAnchor={false} />
            )}
            patchsets={detail.patchsets}
            baseline={baseline}
            onBaseline={setBaseline}
            busy={busy}
            batched={batched}
            viewed={viewed}
            onToggleViewed={toggleViewed}
            onLineComment={(path, side, line, lineEnd, body, pending) =>
              act(
                () =>
                  api.addComment(session, repo, changeKey, body, path, line, {
                    side,
                    line_end: lineEnd ?? undefined,
                    ...(pending ? { pending: true } : {}),
                  }),
                pending ? "draft the comment" : "comment",
              )
            }
          />

          <div className="rounded-lg border border-borderline bg-surface-1 p-4">
            <div className="mb-1 text-sm font-medium text-ink">
              Conversation
            </div>
            <p className="mb-3 text-xs text-ink-3">
              Say why, not just whether — comments record the patchset they were
              written against.
            </p>
            {threads.length > 0 && (
              <ul className="mb-4 space-y-3 text-sm">
                {threads.map((t) => (
                  <li
                    key={t.root.id}
                    className="rounded-md border border-borderline bg-surface-0 p-3"
                  >
                    <ThreadCard thread={t} actors={actors} />
                  </li>
                ))}
              </ul>
            )}
            {comments && threads.length === 0 && (
              <p className="mb-4 text-sm text-ink-3">No comments yet.</p>
            )}
            {noResolve && (
              <p className="mb-3 text-xs text-ink-3">{noResolve}</p>
            )}
            {!signedIn ? (
              <p className="text-xs text-ink-3">
                <SignInWord href={props.loginHref} /> to join the conversation.
              </p>
            ) : (
              <form
                onSubmit={(e) => {
                  e.preventDefault();
                  // Cleared on success only — see `act`. This used to
                  // clear inside the action, before the request had been
                  // awaited to a verdict.
                  postChangeComment(false);
                }}
              >
                <CommentComposer
                  value={commentBody}
                  onChange={setCommentBody}
                  label="Comment on this change"
                  placeholder="What should the author know?"
                >
                  <div className="flex flex-wrap gap-2">
                    <Button
                      type="submit"
                      variant="outline"
                      disabled={busy || !commentBody.trim()}
                    >
                      Comment
                    </Button>
                    {/* Both doors stay open. Saying one thing straight
                        out is not the same act as opening a pass over
                        the whole change, and a forge that only had the
                        batched one would make a one-line question cost
                        a verdict. */}
                    {batched && (
                      <Button
                        type="button"
                        variant="outline"
                        disabled={busy || !commentBody.trim()}
                        onClick={() => postChangeComment(true)}
                      >
                        Add to review
                      </Button>
                    )}
                  </div>
                </CommentComposer>
              </form>
            )}
          </div>
        </div>

        <aside className="space-y-4 lg:sticky lg:top-4">
          <div className="rounded-lg border border-borderline bg-surface-1 p-4">
            <div className="mb-1 flex items-center gap-2 text-sm font-medium">
              {landable ? (
                <span className="text-good">
                  <span aria-hidden>✓</span> Landable
                </span>
              ) : (
                <span className="text-warning">
                  <span aria-hidden>■</span> Blocked
                </span>
              )}
            </div>
            <p className="text-xs text-ink-2">{verdict.verdict.explanation}</p>
            {verdict.verdict.per_path.length > 0 && (
              <ul className="mt-2 space-y-1.5 text-xs">
                {verdict.verdict.per_path.map((p) => (
                  <li key={p.path} className="flex items-start gap-2">
                    {p.satisfied ? (
                      <span aria-hidden className="text-good">
                        ✓
                      </span>
                    ) : (
                      <span aria-hidden className="text-warning">
                        ■
                      </span>
                    )}
                    <span className="min-w-0">
                      <span className="break-all font-mono">{p.path}</span>{" "}
                      <span className="text-ink-3">{p.explanation}</span>
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </div>

          <ChangeChecksPanel
            checks={panelChecks}
            required={requiredChecks}
            scope={latest ? `patchset ${latest.number}` : "this change"}
            navigate={props.navigate}
          />

          {forkAwaitingApproval && (
            <ApproveWorkflows
              runs={approvableHere}
              canWrite={props.canWrite}
              busy={busy}
              onApprove={approve}
            />
          )}

          {/* Outside the panel above on purpose: the reload this note
              follows is exactly the one that empties `approvableHere`, and a
              note nested in the panel would be unmounted by the refresh
              that explains it. */}
          {approveNote && (
            <p
              role="status"
              className="whitespace-pre-wrap break-words text-xs text-ink-2"
            >
              {approveNote}
            </p>
          )}

          <div className="rounded-lg border border-borderline bg-surface-1 p-4">
            <div className="mb-2 text-sm font-medium text-ink">Actions</div>
            {!signedIn ? (
              // A stranger is shown the way in, not a row of buttons that
              // each answer "sign in" when pressed.
              <p className="text-xs text-ink-3">
                <SignInWord href={props.loginHref} /> to approve this change or
                join the conversation.
              </p>
            ) : (
              <div className="flex flex-col gap-2">
                {canWrite ? (
                  <Button
                    type="button"
                    disabled={
                      busy ||
                      c.state !== "open" ||
                      !landable ||
                      failingCheck ||
                      !!c.changeset
                    }
                    onClick={() =>
                      act(async () => {
                        await api.landChange(session, repo, changeKey);
                        pollWhileLanding();
                      }, "land")
                    }
                  >
                    {c.state === "landing"
                      ? "Landing…"
                      : `Land on ${c.target_branch}`}
                  </Button>
                ) : (
                  // Not a refusal to come: landing is a writer's action, and
                  // a reader who has approved is done. The blockers below
                  // still say what the writer is waiting on.
                  <p className="text-xs text-ink-3">
                    Landing on{" "}
                    <span className="font-mono">{c.target_branch}</span> takes
                    write access to this repository.
                  </p>
                )}
                {c.changeset && (
                  // Held, not blocked: nothing here is wrong, the change
                  // simply lands with the set it belongs to. The server
                  // refuses a solo land with a 409 that says the same; the
                  // page says it first, beside the button it turns off.
                  <p className="text-xs text-ink-2">
                    Lands with changeset{" "}
                    {/* The changeset's own page, which is now a route on
                      both mounts: `/dashboard/changesets/{key}` for a
                      member, and `/{owner}/changesets/{key}` for
                      anybody. `navigate` is passed on the forge mount
                      only, so there this is a client-side move; on the
                      dashboard the panel has no navigator and the full
                      dashboard address is the honest link. */}
                    <a
                      href={
                        props.navigate
                          ? href([session.org, "changesets", c.changeset])
                          : dash(["changesets", c.changeset])
                      }
                      className={cn("font-mono", STRUCTURAL_LINK)}
                      onClick={
                        props.navigate
                          ? (e) => {
                              e.preventDefault();
                              props.navigate?.(
                                href([
                                  session.org,
                                  "changesets",
                                  c.changeset as string,
                                ]),
                              );
                            }
                          : undefined
                      }
                    >
                      {c.changeset}
                    </a>{" "}
                    — remove it there to land or abandon it alone.
                  </p>
                )}
                {c.state === "open" ? (
                  <LandBlockers blockers={landBlockers} />
                ) : (
                  c.state !== "landing" && (
                    <span className="text-xs text-ink-3">
                      Only an open change can land; this one is{" "}
                      <span className="font-mono">{c.state}</span>.
                    </span>
                  )
                )}
                {batched ? (
                  // Approving *is* a review — the same act through the
                  // same door, so it records a verdict, publishes what
                  // you drafted and sends one notification like any
                  // other. A standalone Approve beside a sheet with an
                  // Approve radio in it is two ways to do one thing,
                  // and they would eventually disagree about what the
                  // second one does to your drafts.
                  <Button
                    type="button"
                    variant="outline"
                    disabled={busy || c.state !== "open" || sheetOpen}
                    onClick={() => setSheetOpen(true)}
                  >
                    Review patchset {latest?.number ?? ""}
                  </Button>
                ) : (
                  // A server older than migration 0051 has no review
                  // routes at all, so the button that predates them
                  // stays exactly where it was.
                  <Button
                    type="button"
                    variant="outline"
                    disabled={busy || c.state !== "open"}
                    onClick={() =>
                      act(
                        () => api.approve(session, repo, changeKey),
                        "approve",
                      )
                    }
                  >
                    Approve patchset {latest?.number ?? ""}
                  </Button>
                )}
                {/* Revoking stays where it is, and stays its own
                    control: taking an approval back is not a review —
                    there is nothing to say, nothing to publish and
                    nobody new to tell. */}
                <Button
                  type="button"
                  variant="outline"
                  disabled={busy || !hasApproved}
                  onClick={() =>
                    act(
                      () => api.unapprove(session, repo, changeKey),
                      "revoke approval",
                    )
                  }
                >
                  Revoke my approval
                </Button>
                {(canWrite || isAuthor) && (
                  // The server lets a writer abandon any change and lets
                  // the author abandon their own; the button follows the
                  // same two doors so nobody is offered one that is shut.
                  <Button
                    type="button"
                    variant="outline"
                    disabled={busy || c.state !== "open" || !!c.changeset}
                    onClick={() =>
                      act(
                        () => api.abandonChange(session, repo, changeKey),
                        "abandon",
                      )
                    }
                  >
                    Abandon
                  </Button>
                )}
              </div>
            )}
            {c.state === "landed" && c.landed_commit && (
              <p className="mt-3 text-sm text-good">
                <span aria-hidden>●</span> Landed as{" "}
                <span className="font-mono text-xs">
                  {c.landed_commit.slice(0, 12)}
                </span>{" "}
                on <span className="font-mono text-xs">{c.target_branch}</span>
                {c.land_verdict && c.land_verdict !== "landed" && (
                  <span className="text-ink-3"> ({c.land_verdict})</span>
                )}
              </p>
            )}
          </div>

          {/* Beside the land blockers, because half of these *are* one
              and the other half deliberately are not — and the reader
              deciding what to do next needs both in the same glance as
              the button they turn off. */}
          <StandingBlocks
            blocks={blocks}
            busy={busy}
            onWithdraw={() =>
              void act(
                () => api.withdrawReview(session, repo, changeKey),
                "withdraw the request for changes",
              )
            }
          />

          {/* Who the change needs, above who has spoken: "waiting on
              Casey" is the question a reader arrives with, and the
              approvals list below can only answer it by omission. */}
          <RequiredReviewersCard standing={standing} />

          <div className="rounded-lg border border-borderline bg-surface-1 p-4">
            <div className="mb-2 text-sm font-medium text-ink">
              Approvals on patchset {latest?.number ?? "—"}
            </div>
            <p className="mb-2 text-xs text-ink-3">
              A new patchset starts the count over — approvals never carry
              forward to code they never saw.
            </p>
            {detail.approvals.length === 0 ? (
              <p className="text-sm text-ink-3">None yet.</p>
            ) : (
              <ul className="space-y-1 text-sm">
                {detail.approvals.map((a) => (
                  <li
                    key={a.email}
                    className="flex flex-wrap items-baseline gap-x-2"
                  >
                    <span aria-hidden className="text-good">
                      ✓
                    </span>
                    <span>{a.name}</span>
                    <span className="ml-auto text-xs text-ink-3">
                      {formatAgo(a.created_at)}
                    </span>
                    <span className="w-full truncate pl-5 text-xs text-ink-3">
                      {a.email}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </div>

          <div className="rounded-lg border border-borderline bg-surface-1 p-4">
            <div className="mb-2 text-sm font-medium text-ink">Patchsets</div>
            <ul className="space-y-2 text-sm">
              {[...detail.patchsets].reverse().map((p) => (
                <li key={p.number} className="flex items-baseline gap-2">
                  <span className="font-mono text-xs text-ink-3">
                    #{p.number}
                  </span>
                  <span className="font-mono text-xs">
                    {p.commit.slice(0, 12)}
                  </span>
                  <span className="truncate text-ink-2">
                    {p.message.split("\n")[0]}
                  </span>
                  <span className="ml-auto shrink-0 text-xs text-ink-3">
                    {formatAgo(p.created_at)}
                  </span>
                </li>
              ))}
            </ul>
          </div>
        </aside>
      </div>
    </div>
  );
}

