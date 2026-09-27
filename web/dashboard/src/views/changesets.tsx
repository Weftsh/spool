// Changesets: one review and one landing across several repositories.
//
// A changeset is a set of open changes, at most one per repository, that
// land together or not at all. This screen creates one from the org's
// open changes, shows the composed verdict — every member's gate, the
// composed CI, the landing order — lands it, watches the landing member
// by member, and makes the reverting changeset for one that landed. The
// words are the server's throughout: gate names, explanations, refusals
// and step notes are rendered as they arrive, because they are the same
// sentences the API and the lander use and a paraphrase here would be a
// second vocabulary for the same fact.

import { useCallback, useEffect, useRef, useState } from "react";
import {
  api,
  ApiError,
  type Changeset,
  type ChangesetRef,
  type ChangesetState,
  type ChangesetVerdict,
  type ChangesetWorkspace,
  type OrgChange,
  type RevertConflict,
  type Session,
} from "@/api";
import { formatAgo } from "@/format";
import { href } from "@/router";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableHeadRow,
  TableRow,
} from "@/components/ui/table";
import { CloneBlock } from "@/components/clone-block";
import { cn } from "@/lib/utils";
import { HOSTED_PROVIDER } from "@/lib/hosted-runs";
import { STRUCTURAL_LINK } from "@/lib/links";
import {
  MAX_MEMBERS,
  canCreate,
  isPicked,
  landRefusal,
  landingSummary,
  // The one spelling of `repo/change` on these screens: the strip, the
  // table, the landing steps and the editor's picker all name a member
  // the same way, which is what lets a refusal about one be read beside
  // the other.
  refLabel as label,
  revertKey,
  stateGlyph,
  stateTone,
  verdictWord,
  suggestKey,
  togglePick,
  unpickable,
  validKey,
  validTitle,
} from "@/lib/changesets";
import {
  ChangeChecksPanel,
  LandBlockers,
  type PanelCheck,
} from "@/views/forge/change-checks";
import { CombinedFiles } from "@/views/changesets/files";
import { LandingOrder } from "@/views/changesets/order";
import {
  MountAnchor,
  dashboardChangesetLinks,
  follow,
  type ChangesetLinks,
} from "@/views/changesets/links";

const STATES: ChangesetState[] = [
  "open",
  "landing",
  "landed",
  "failed",
  "abandoned",
];

export function ChangesetsView(props: {
  session: Session;
  /// The changeset open, from the address; none for the list.
  selectedKey?: string;
  navigate: (to: string, replace?: boolean) => void;
}) {
  if (props.selectedKey)
    return (
      <ChangesetView
        session={props.session}
        changesetKey={props.selectedKey}
        links={dashboardChangesetLinks(props.session.org, props.navigate)}
        // Nothing reaches the dashboard without a session, so there is
        // never a sign-in prompt to draw here.
        signedIn
      />
    );
  return <ChangesetsList session={props.session} navigate={props.navigate} />;
}

function StateBadge(props: { state: ChangesetState }) {
  return (
    <Badge className={cn("gap-1.5", stateTone(props.state))}>
      <span aria-hidden>{stateGlyph(props.state)}</span> {props.state}
    </Badge>
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

// ---------------------------------------------------------------------
// The list

const ALL = "all";

function ChangesetsList(props: {
  session: Session;
  navigate: (to: string, replace?: boolean) => void;
}) {
  const { session } = props;
  const [state, setState] = useState<string>(ALL);
  const [rows, setRows] = useState<Changeset[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);

  useEffect(() => {
    let alive = true;
    setRows(null);
    api
      .changesets(
        session,
        state === ALL ? undefined : (state as ChangesetState),
      )
      .then((c) => alive && setRows(c))
      .catch((e) => alive && setError(String(e)));
    return () => {
      alive = false;
    };
  }, [session, state]);

  return (
    <main className="mx-auto max-w-5xl px-4 py-8">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-xl font-semibold">Changesets</h1>
          <p className="mt-1 text-sm text-ink-2">
            Changes across repositories that land together or not at all.
          </p>
        </div>
        <div className="flex flex-wrap items-end gap-2">
          <label className="flex flex-col gap-1 text-xs text-ink-3">
            State
            <Select value={state} onValueChange={setState}>
              <SelectTrigger aria-label="Filter by state" className="w-36">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={ALL}>all</SelectItem>
                {STATES.map((s) => (
                  <SelectItem key={s} value={s}>
                    {s}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </label>
          <Button
            type="button"
            variant={creating ? "outline" : "default"}
            onClick={() => setCreating((v) => !v)}
          >
            {creating ? "Cancel" : "New changeset"}
          </Button>
        </div>
      </div>

      {creating && (
        <NewChangeset
          session={session}
          onCreated={(cs) => props.navigate(href(["changesets", cs.key]))}
        />
      )}

      <div className="mt-6">
        {error && !rows ? (
          <ErrLine message={error} />
        ) : !rows ? (
          <p className="text-sm text-ink-3">Loading…</p>
        ) : rows.length === 0 ? (
          <p className="text-sm text-ink-3">
            {state === ALL
              ? "No changesets yet. Pick open changes from two or more repositories above and they will land as one."
              : `No ${state} changesets.`}
          </p>
        ) : (
          <div className="overflow-hidden rounded-lg border border-borderline bg-surface-1">
            <Table>
              <TableHeader>
                <TableHeadRow>
                  <TableHead className="px-3 py-2.5">Changeset</TableHead>
                  <TableHead className="px-3 py-2.5">Title</TableHead>
                  <TableHead className="px-3 py-2.5">State</TableHead>
                  <TableHead className="px-3 py-2.5 text-right">
                    Members
                  </TableHead>
                  <TableHead className="px-3 py-2.5 text-right">
                    Updated
                  </TableHead>
                </TableHeadRow>
              </TableHeader>
              <TableBody>
                {rows.map((cs) => {
                  const to = href(["changesets", cs.key]);
                  return (
                    <TableRow
                      key={cs.key}
                      className="cursor-pointer hover:bg-surface-2 focus-within:bg-surface-2"
                      onClick={() => props.navigate(to)}
                    >
                      <TableCell className="max-w-48 truncate px-3 py-2.5 font-mono text-xs">
                        <a
                          href={`/dashboard${to}`}
                          className={STRUCTURAL_LINK}
                          onClick={(e) => {
                            e.preventDefault();
                            e.stopPropagation();
                            props.navigate(to);
                          }}
                        >
                          {cs.key}
                        </a>
                      </TableCell>
                      <TableCell className="max-w-72 truncate px-3 py-2.5">
                        {cs.title}
                      </TableCell>
                      <TableCell className="px-3 py-2.5">
                        <StateBadge state={cs.state} />
                      </TableCell>
                      <TableCell className="px-3 py-2.5 text-right font-mono">
                        {cs.members.length}
                      </TableCell>
                      <TableCell className="px-3 py-2.5 text-right text-xs text-ink-3">
                        {formatAgo(cs.updated_at)}
                      </TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          </div>
        )}
      </div>
    </main>
  );
}

// ---------------------------------------------------------------------
// Creating one

/// The org's open changes, grouped by repository, as tick boxes.
///
/// Every open change the caller may read is listed, including the ones
/// that cannot be ticked — held by another changeset, or a second change
/// in a repository already picked — greyed with the reason beside them.
/// Hiding them would send somebody looking through the repositories for
/// a change they know is open.
function ChangePicker(props: {
  changes: OrgChange[];
  picked: ChangesetRef[];
  onToggle: (ref: ChangesetRef) => void;
  /// Members already in the changeset being extended, so their
  /// repositories count as taken.
  taken?: ChangesetRef[];
}) {
  const byRepo = new Map<string, OrgChange[]>();
  for (const c of props.changes) {
    const list = byRepo.get(c.repo) ?? [];
    list.push(c);
    byRepo.set(c.repo, list);
  }
  const all = [...(props.taken ?? []), ...props.picked];
  if (props.changes.length === 0)
    return (
      <p className="text-sm text-ink-3">
        No open changes in any repository you can read. Start a review on a
        pushed branch first.
      </p>
    );
  return (
    <div className="space-y-3">
      {[...byRepo.entries()].map(([repo, list]) => (
        <fieldset key={repo}>
          <legend className="mb-1 font-mono text-xs font-medium text-ink">
            {repo}
          </legend>
          <ul className="space-y-1">
            {list.map((c) => {
              const ref = { repo: c.repo, change: c.key };
              const why = unpickable(c, all);
              const on = isPicked(props.picked, ref);
              return (
                <li key={c.key}>
                  <label
                    className={cn(
                      "flex items-start gap-2 text-sm",
                      why && !on ? "text-ink-3" : "text-ink-2",
                    )}
                  >
                    <input
                      type="checkbox"
                      className="mt-1"
                      checked={on}
                      disabled={!on && why !== null}
                      aria-label={`${c.repo}/${c.key}: ${c.title}`}
                      onChange={() => props.onToggle(ref)}
                    />
                    <span className="min-w-0">
                      <span className="font-mono text-xs">{c.key}</span>{" "}
                      <span className="break-words">{c.title}</span>
                      {why && !on && (
                        <span className="block text-xs text-ink-3">{why}</span>
                      )}
                    </span>
                  </label>
                </li>
              );
            })}
          </ul>
        </fieldset>
      ))}
    </div>
  );
}

function useOpenChanges(session: Session): {
  changes: OrgChange[] | null;
  error: string | null;
} {
  const [changes, setChanges] = useState<OrgChange[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    api
      .orgChanges(session, { state: "open", limit: 200 })
      .then((c) => alive && setChanges(c))
      .catch((e) => alive && setError(String(e)));
    return () => {
      alive = false;
    };
  }, [session]);
  return { changes, error };
}

function NewChangeset(props: {
  session: Session;
  onCreated: (cs: Changeset) => void;
}) {
  const { session } = props;
  const { changes, error: loadError } = useOpenChanges(session);
  const [title, setTitle] = useState("");
  const [key, setKey] = useState("");
  // Until somebody types in the key box it follows the title; after that
  // it is theirs. Overwriting a key somebody chose because they then
  // fixed a typo in the title would be maddening.
  const [keyEdited, setKeyEdited] = useState(false);
  const [body, setBody] = useState("");
  const [picked, setPicked] = useState<ChangesetRef[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const keyProblem =
    key.length > 0 && !validKey(key)
      ? "a key is 1–72 letters, digits, dots, underscores or dashes"
      : null;
  const titleProblem =
    title.length > 0 && !validTitle(title)
      ? "a title is at most 200 bytes"
      : null;

  return (
    <form
      className="mt-6 rounded-lg border border-borderline bg-surface-1 p-4"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        setError(null);
        try {
          const cs = await api.createChangeset(session, {
            key,
            title: title.trim(),
            ...(body.trim() ? { body: body.trim() } : {}),
            members: picked,
          });
          props.onCreated(cs);
        } catch (err) {
          setError(
            `Could not create the changeset: ${err instanceof Error ? err.message : err}`,
          );
        } finally {
          setBusy(false);
        }
      }}
    >
      <div className="mb-1 text-sm font-medium">New changeset</div>
      <p className="mb-3 text-xs text-ink-3">
        Pick one open change per repository. They are reviewed under each
        repository&rsquo;s own OWNERS and land together, in dependency order, or
        not at all.
      </p>
      <div className="grid gap-3 md:grid-cols-2">
        <label className="flex flex-col gap-1 text-xs text-ink-3">
          Title
          <input
            className="rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 text-sm text-ink"
            value={title}
            required
            autoFocus
            onChange={(e) => {
              setTitle(e.target.value);
              if (!keyEdited) setKey(suggestKey(e.target.value));
            }}
            placeholder="Rename the payments service"
          />
          {titleProblem && <span className="text-serious">{titleProblem}</span>}
        </label>
        <label className="flex flex-col gap-1 text-xs text-ink-3">
          Key
          <input
            className="rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 font-mono text-sm text-ink"
            value={key}
            required
            onChange={(e) => {
              setKeyEdited(true);
              setKey(e.target.value);
            }}
            placeholder="rename-payments"
          />
          {keyProblem && <span className="text-serious">{keyProblem}</span>}
        </label>
      </div>
      <label className="mt-3 flex flex-col gap-1 text-xs text-ink-3">
        Description (optional)
        <textarea
          className="min-h-16 rounded-md border border-borderline bg-surface-0 px-3 py-2 text-sm text-ink"
          value={body}
          onChange={(e) => setBody(e.target.value)}
          maxLength={4000}
        />
      </label>
      <div className="mt-4">
        <div className="mb-2 text-xs font-medium uppercase tracking-wider text-ink-3">
          Members{" "}
          <span className="font-mono normal-case tracking-normal">
            {picked.length}/{MAX_MEMBERS}
          </span>
        </div>
        {loadError && !changes ? (
          <ErrLine message={loadError} />
        ) : !changes ? (
          <p className="text-sm text-ink-3">Loading open changes…</p>
        ) : (
          <ChangePicker
            changes={changes}
            picked={picked}
            onToggle={(ref) => setPicked((p) => togglePick(p, ref))}
          />
        )}
      </div>
      <div className="mt-4 flex flex-wrap items-center gap-2">
        <Button disabled={busy || !canCreate(key, title, picked)}>
          {busy ? "Creating…" : "Create changeset"}
        </Button>
        {picked.length === 0 &&
          changes &&
          changes.length > 0 &&
          (changes.some((c) => c.viewer_write === true) ? (
            <span className="text-xs text-ink-3">
              Pick at least one change.
            </span>
          ) : (
            // Every row is grey for the same reason; say it once, where
            // the button that cannot be pressed is.
            <span className="text-xs text-ink-3">
              Composing takes write access to a member&rsquo;s repository, and
              none of these is in one you may write to.
            </span>
          ))}
      </div>
      <ErrLine message={error} />
    </form>
  );
}

// ---------------------------------------------------------------------
// One changeset

/// The composed CI rows in the shape the shared checks panel reads. Each
/// is named by its repository as well as its job, because two members'
/// workflows may well share a job name and the panel keys rows by name;
/// the repository doubles as the "where do I look" hint.
export function composedPanelChecks(cs: Changeset): PanelCheck[] {
  return cs.checks.map((c) => ({
    name: `${c.repo}: ${c.name}`,
    state: c.state,
    detail_url: c.detail_url,
    // Who wrote the row, which for a composed check is always our own
    // runner: the panel reads `posted_by` as the provider gate on its
    // Details link. This used to carry the repository name, so every
    // composed row's link was treated as a third party's site — opened
    // in a new tab with `rel="ugc"` — when it pointed at our own run
    // page one client-side navigation away. The repository is already
    // in the name.
    posted_by: HOSTED_PROVIDER,
    source: "composition",
  }));
}

/// One changeset, on either mount.
///
/// Deliberately one rendering rather than two, the same rule the
/// repository page keeps: signing in adds *actions* and never changes
/// what the page says. A stranger following a link to a public set gets
/// the verdict, the members in landing order, the order strip, the
/// composed checks and the workspace clone block — a public set's clone
/// URL is a public fact — and gets the writer's controls replaced by the
/// sentence saying what they take. The API was already answering an
/// anonymous reader over public repositories (`Scope::RepoRead` admits a
/// `None` principal); only the address was missing.
export function ChangesetView(props: {
  session: Session;
  changesetKey: string;
  /// Where this page's links point on the mount it is drawn on.
  links: ChangesetLinks;
  /// Whether the reader holds any credential at all — a browser session
  /// or a pasted API token. False only on the forge, and it decides
  /// whether the page offers a way in, not what it says.
  signedIn: boolean;
  /// Where "Sign in" goes, with a `next` back to this page.
  loginHref?: string;
  /// The element this page's body is drawn in.
  ///
  /// The dashboard shell renders no `<main>` of its own, so this view is
  /// the page's; the forge shell already renders one, and a second
  /// nested inside it is invalid HTML and a second `main` landmark for
  /// anything reading the page structurally — a screen reader included.
  /// The forge shell owns the column width there too, so the body takes
  /// none of its own.
  as?: "main" | "div";
}) {
  const { session, changesetKey, links } = props;
  const Frame = props.as ?? "main";
  const frameClass =
    Frame === "main" ? "mx-auto max-w-5xl px-4 py-8" : "min-w-0";
  const [cs, setCs] = useState<Changeset | null>(null);
  const [verdict, setVerdict] = useState<ChangesetVerdict | null>(null);
  const [verdictError, setVerdictError] = useState<string | null>(null);
  const [ws, setWs] = useState<ChangesetWorkspace | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [refusal, setRefusal] = useState<{
    message: string;
    waitingOn: string[];
  } | null>(null);
  const [conflicts, setConflicts] = useState<RevertConflict[] | null>(null);
  const [reverting, setReverting] = useState(false);
  const [adding, setAdding] = useState(false);
  const pollTimer = useRef<number | null>(null);

  const load = useCallback(() => {
    let alive = true;
    api
      .changeset(session, changesetKey)
      .then((c) => alive && setCs(c))
      .catch((e) => alive && setError(String(e)));
    // The verdict is a second request because it is a second question —
    // what would happen if this landed now — and it can be refused where
    // the changeset itself is fine: a member with no patchset yet has
    // nothing to judge. That refusal is printed where the verdict would
    // have gone, not over the whole page.
    api
      .changesetVerdict(session, changesetKey)
      .then((v) => {
        if (!alive) return;
        setVerdict(v);
        setVerdictError(null);
      })
      .catch((e) => {
        if (!alive) return;
        setVerdict(null);
        setVerdictError(e instanceof Error ? e.message : String(e));
      });
    api
      .changesetWorkspace(session, changesetKey)
      .then((w) => alive && setWs(w))
      // No workspace is not an error worth a line: a changeset with no
      // composable member has none, and the block simply does not show.
      .catch(() => alive && setWs(null));
    return () => {
      alive = false;
    };
  }, [session, changesetKey]);

  useEffect(load, [load]);
  useEffect(
    () => () => {
      if (pollTimer.current !== null) window.clearInterval(pollTimer.current);
    },
    [],
  );

  // A landing is driven by a job on some node; this page learns how it
  // went by asking. Once a second, until the changeset is no longer
  // `landing`, then one last read of everything so the verdict and the
  // workspace agree with the new state.
  function pollWhileLanding() {
    if (pollTimer.current !== null) window.clearInterval(pollTimer.current);
    pollTimer.current = window.setInterval(async () => {
      try {
        const c = await api.changeset(session, changesetKey);
        setCs(c);
        if (c.state !== "landing") {
          if (pollTimer.current !== null) {
            window.clearInterval(pollTimer.current);
            pollTimer.current = null;
          }
          load();
        }
      } catch {
        /* transient; the next tick retries */
      }
    }, 1000);
  }
  // Arriving at a changeset that is already landing — somebody else
  // pressed the button, or this is a reload — should watch it too.
  useEffect(() => {
    if (cs?.state === "landing" && pollTimer.current === null)
      pollWhileLanding();
  }, [cs?.state]);

  async function act(fn: () => Promise<unknown>, verb: string) {
    setBusy(true);
    setError(null);
    setRefusal(null);
    setConflicts(null);
    try {
      await fn();
      load();
    } catch (e) {
      setError(`Could not ${verb}: ${e instanceof Error ? e.message : e}`);
    } finally {
      setBusy(false);
    }
  }

  async function land() {
    setBusy(true);
    setError(null);
    setRefusal(null);
    try {
      await api.landChangeset(session, changesetKey);
      load();
      pollWhileLanding();
    } catch (e) {
      // A 409 here is the gate speaking, not a failure of the request:
      // it says what the changeset is waiting on, and that list is the
      // answer, so it is shown as one rather than as an error line.
      if (e instanceof ApiError && e.status === 409) {
        const r = landRefusal(e.body);
        setRefusal({ message: r.message, waitingOn: r.waitingOn });
      } else {
        setError(`Could not land: ${e instanceof Error ? e.message : e}`);
      }
    } finally {
      setBusy(false);
    }
  }

  async function revert(key: string, title: string) {
    setBusy(true);
    setError(null);
    setConflicts(null);
    try {
      const made = await api.revertChangeset(session, changesetKey, {
        key,
        ...(title.trim() ? { title: title.trim() } : {}),
      });
      setReverting(false);
      follow(links.changeset(made.key));
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        const body = (e.body ?? {}) as { conflicts?: unknown };
        setConflicts(
          Array.isArray(body.conflicts)
            ? (body.conflicts as RevertConflict[])
            : [],
        );
      }
      setError(`Could not revert: ${e instanceof Error ? e.message : e}`);
    } finally {
      setBusy(false);
    }
  }

  if (error && !cs) return <ErrLine message={error} />;
  if (!cs)
    return (
      <Frame className={frameClass}>
        <p className="text-sm text-ink-3">Loading…</p>
      </Frame>
    );

  const memberVerdict = (r: ChangesetRef) =>
    verdict?.members.find((m) => m.repo === r.repo && m.change === r.change);
  // Members in landing order, so the table reads as the plan the lander
  // will walk. A member the ordering left out — it cannot happen, but a
  // table that silently dropped a row would be the worst way to learn
  // that it had — is appended.
  const ordered = [
    ...cs.order
      .map((r) =>
        cs.members.find((m) => m.repo === r.repo && m.change.key === r.change),
      )
      .filter((m): m is Changeset["members"][number] => m !== undefined),
    ...cs.members.filter(
      (m) =>
        !cs.order.some((r) => r.repo === m.repo && r.change === m.change.key),
    ),
  ];
  const canLand =
    cs.state === "open" &&
    verdict?.landable === true &&
    verdict.gate === "ready";
  const landedSomething =
    cs.state === "landed" ||
    (cs.state === "failed" &&
      (cs.landing?.members.some((m) => m.state === "done") ?? false));
  // Subtractive, like every write control on the forge: the server's own
  // answer, and no until it has said yes. Land, revert, abandon, add and
  // remove all go through `load(.., RepoWrite)` and answer somebody who
  // may not write with the masked `no changeset` a stranger gets — about
  // a changeset they were looking at. So the controls are not drawn for
  // them, and the card says what they would need instead.
  const canWrite = cs.viewer_write === true;

  return (
    <Frame className={frameClass}>
      {/* Only where there is a list to go back to. The forge has none
          on purpose — see `forgeChangesetLinks` — and a back link to a
          404 is worse than no back link. */}
      {links.list && (
        <MountAnchor
          link={links.list}
          className={cn("text-xs", STRUCTURAL_LINK)}
        >
          ← All changesets
        </MountAnchor>
      )}
      <div className="mt-3 flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h1 className="break-words text-xl font-semibold">{cs.title}</h1>
          <div className="mt-1 flex flex-wrap items-center gap-2 text-xs text-ink-3">
            <span className="font-mono">{cs.key}</span>
            <StateBadge state={cs.state} />
            <span>updated {formatAgo(cs.updated_at)}</span>
          </div>
          {cs.reverts && (
            <p className="mt-2 text-sm text-ink-2">
              Reverts{" "}
              <MountAnchor
                link={links.changeset(cs.reverts)}
                className={cn("font-mono text-xs", STRUCTURAL_LINK)}
              >
                {cs.reverts}
              </MountAnchor>
            </p>
          )}
          {cs.reverted_by.length > 0 && (
            <p className="mt-2 text-sm text-ink-2">
              Reverted by{" "}
              {cs.reverted_by.map((k, i) => (
                <span key={k}>
                  {i > 0 && ", "}
                  <MountAnchor
                    link={links.changeset(k)}
                    className={cn("font-mono text-xs", STRUCTURAL_LINK)}
                  >
                    {k}
                  </MountAnchor>
                </span>
              ))}
            </p>
          )}
          {cs.body && (
            <p className="mt-3 whitespace-pre-wrap break-words text-sm text-ink-2">
              {cs.body}
            </p>
          )}
        </div>
      </div>

      {/* `minmax(0,1fr)`, not `1fr`: a grid track's floor is its
          content's min-content width, and a verdict that names a member
          by a 40-hex Change-Id — every revert has one — is a single
          unbreakable token wider than the track. With plain `1fr` the
          left column grew to fit it and pushed the Actions column, Land
          button first, off the right edge of a 1440px viewport. */}
      <div className="mt-6 grid gap-6 lg:grid-cols-[minmax(0,1fr)_20rem]">
        <div className="min-w-0 space-y-6">
          {/* The composed gate: one sentence for the whole set, then
              the list of what it is waiting on. This is the answer to
              "can I press Land", so it sits above the members. */}
          <section className="rounded-lg border border-borderline bg-surface-1 p-4">
            <div className="mb-2 text-sm font-medium text-ink">Verdict</div>
            {verdict ? (
              <>
                <p className="break-words text-sm text-ink-2">
                  <span
                    className={cn(
                      "font-medium",
                      verdictWord(cs.state, verdict).tone,
                    )}
                  >
                    {verdictWord(cs.state, verdict).word}
                  </span>{" "}
                  <span aria-hidden>·</span> {verdict.explanation}
                </p>
                {verdict.waiting_on.length > 0 && (
                  <div className="mt-2">
                    {/* Checks still running, not checks that failed: a
                        failing check is the sentence above, never a
                        row here. */}
                    <LandBlockers blockers={verdict.waiting_on} pending />
                  </div>
                )}
              </>
            ) : verdictError ? (
              <p className="text-sm text-ink-3" role="status">
                No verdict yet: {verdictError}
              </p>
            ) : (
              <p className="text-sm text-ink-3">Loading verdict…</p>
            )}
          </section>

          {/* The plan, above the table that renders it. The order used
              to be one line of edges under the table — the data, not
              the plan: three edges over four members is a picture
              nobody assembles from a sentence, and the one thing it
              never said was which members land at the same time. */}
          <LandingOrder
            session={session}
            links={links}
            cs={cs}
            members={ordered}
            verdict={verdict}
            canWrite={canWrite}
            busy={busy}
            onSaved={load}
          />

          <section>
            <div className="mb-2 flex items-center justify-between">
              <h2 className="text-sm font-medium text-ink">
                Members{" "}
                <span className="font-mono text-xs text-ink-3">
                  {cs.members.length}
                </span>
                <span className="ml-2 text-xs font-normal text-ink-3">
                  in landing order
                </span>
              </h2>
              {cs.state === "open" && canWrite && (
                <Button
                  type="button"
                  variant="outline"
                  size="xs"
                  disabled={busy || cs.members.length >= MAX_MEMBERS}
                  onClick={() => setAdding((v) => !v)}
                >
                  {adding ? "Cancel" : "Add member"}
                </Button>
              )}
            </div>
            {adding && (
              <AddMember
                session={session}
                taken={cs.members.map((m) => ({
                  repo: m.repo,
                  change: m.change.key,
                }))}
                onAdd={(ref) =>
                  act(async () => {
                    await api.addChangesetMember(session, changesetKey, ref);
                    setAdding(false);
                  }, "add that member")
                }
              />
            )}
            <div className="overflow-hidden rounded-lg border border-borderline bg-surface-1">
              {/* Fixed layout, with the columns sized here. Under the
                  default auto layout a cell's `max-w-*` is ignored and
                  the columns are sized by their content, so the
                  `truncate`s below never truncated anything: a member's
                  key and title took their full width, the gate's
                  sentence was squeezed to one word per line, and the
                  table ran past its card — the Remove column was cut
                  off at the edge, reachable only by a horizontal scroll
                  nothing hinted at. Fixed widths make the cells ellipsise
                  and wrap where they were written to. The numeric
                  columns are as wide as their headings, not their
                  numbers — "PATCHSET" ran into "GATE" when they were
                  sized for a digit.

                  Which column yields is not arbitrary either. The gate's
                  sentence wraps and stays whole at any width; a
                  repository name is an identifier, and clipping it is
                  how three members of one family — payments-api,
                  payments-worker, payments-web — all rendered
                  "payments-…" on the page whose whole job is saying
                  which repositories are in the review. So the gate gives
                  its width to the identifier columns, and the
                  identifiers wrap rather than truncate. The change key
                  is the exception and keeps its ellipsis: a 40-hex
                  Change-Id would take three lines of every row and says
                  nothing after the first few characters. */}
              <Table className="table-fixed">
                <TableHeader>
                  <TableHeadRow>
                    <TableHead className="w-[22%] px-3 py-2.5">
                      Repository
                    </TableHead>
                    <TableHead className="px-3 py-2.5">Change</TableHead>
                    <TableHead className="w-[13.5%] px-3 py-2.5 text-right">
                      Patchset
                    </TableHead>
                    <TableHead className="w-[20%] px-3 py-2.5">Gate</TableHead>
                    <TableHead className="w-[15%] px-3 py-2.5 text-right">
                      Approvals
                    </TableHead>
                    {cs.state === "open" && canWrite && (
                      <TableHead className="w-[10%] px-3 py-2.5" />
                    )}
                  </TableHeadRow>
                </TableHeader>
                <TableBody>
                  {ordered.map((m) => {
                    const ref = { repo: m.repo, change: m.change.key };
                    const mv = memberVerdict(ref);
                    return (
                      <TableRow key={label(ref)}>
                        <TableCell className="break-words px-3 py-2.5 font-mono text-xs">
                          <MountAnchor
                            link={links.repo(m.repo)}
                            className={STRUCTURAL_LINK}
                          >
                            {m.repo}
                          </MountAnchor>
                        </TableCell>
                        <TableCell className="px-3 py-2.5">
                          <div className="truncate font-mono text-xs">
                            {/* A member is reviewed where every change is
                                reviewed — on its own page, under its
                                repository's OWNERS. That page is a forge
                                address: a full load from the dashboard,
                                which has no route for it, and a
                                client-side move from the forge, which
                                does. The change page links back here. */}
                            <MountAnchor
                              link={links.member(m.repo, m.change.key)}
                              className={STRUCTURAL_LINK}
                            >
                              {m.change.key}
                            </MountAnchor>
                          </div>
                          {/* Wrapped, not truncated: the title is the
                              other half of "which change is this", and
                              at this column's width an ellipsis cut it
                              at about eighteen characters — "tier the
                              fee sc…" for three members of one family. */}
                          <div className="break-words text-xs text-ink-2">
                            {m.change.title}
                          </div>
                          <div className="text-xs text-ink-3">
                            {m.change.state}
                            {/* Plain text, not a nested tag: `open<span>`
                                is exactly the jammed-inline-tag shape the
                                walkthrough's audit fails. */}
                            {m.change.land_verdict && m.change.state !== "open"
                              ? ` — ${m.change.land_verdict}`
                              : null}
                          </div>
                        </TableCell>
                        <TableCell className="px-3 py-2.5 text-right font-mono text-xs">
                          {m.change.patchset?.number ?? "—"}
                        </TableCell>
                        <TableCell className="px-3 py-2.5 text-xs">
                          {mv ? (
                            <>
                              <span
                                className={cn(
                                  "font-medium",
                                  verdictWord(m.change.state, mv).tone,
                                )}
                              >
                                {verdictWord(m.change.state, mv).word}
                              </span>
                              <span className="block break-words text-ink-2">
                                {mv.explanation}
                              </span>
                            </>
                          ) : (
                            <span className="text-ink-3">—</span>
                          )}
                        </TableCell>
                        <TableCell className="px-3 py-2.5 text-right font-mono text-xs">
                          {mv ? mv.approvals.length : "—"}
                        </TableCell>
                        {cs.state === "open" && canWrite && (
                          <TableCell className="px-3 py-2.5 text-right">
                            <button
                              type="button"
                              className={cn("text-xs", STRUCTURAL_LINK)}
                              disabled={busy}
                              aria-label={`Remove ${label(ref)}`}
                              onClick={() =>
                                act(
                                  () =>
                                    api.removeChangesetMember(
                                      session,
                                      changesetKey,
                                      ref,
                                    ),
                                  `remove ${label(ref)}`,
                                )
                              }
                            >
                              Remove
                            </button>
                          </TableCell>
                        )}
                      </TableRow>
                    );
                  })}
                </TableBody>
              </Table>
            </div>
          </section>

          {/* What the set says, between what is in it and what the
              machine thinks of it. FORGE-UX §6a's reading order is
              Verdict → Members → Files → Checks → Landing, and the
              combined diff is the answer to "what does it say" — the
              question that used to take one page load per member. */}
          <CombinedFiles
            session={session}
            changeset={props.changesetKey}
            members={ordered}
            verdict={verdict}
          />

          <ChangeChecksPanel
            checks={composedPanelChecks(cs)}
            scope={
              cs.composition
                ? `composition ${cs.composition.slice(0, 12)}`
                : "this changeset"
            }
          />

          {cs.landing && (
            <section className="rounded-lg border border-borderline bg-surface-1 p-4">
              <div className="mb-2 text-sm font-medium text-ink">
                Landing{" "}
                <span className="font-mono text-xs text-ink-3">
                  attempt {cs.landing.attempt}
                </span>
              </div>
              <p
                className={cn(
                  "text-sm",
                  cs.landing.outcome === "landed"
                    ? "text-good"
                    : cs.landing.outcome === "failed"
                      ? "text-serious"
                      : "text-warning",
                )}
                role="status"
              >
                {landingSummary(cs.landing)}
              </p>
              <ul className="mt-3 space-y-1.5 text-xs">
                {cs.landing.members.map((s) => (
                  <li
                    key={label(s)}
                    className="flex flex-wrap items-baseline gap-2"
                  >
                    <span className="font-mono text-ink">{label(s)}</span>
                    <span className="font-mono text-ink-3">{s.ref}</span>
                    <span
                      className={cn(
                        "font-medium",
                        s.state === "done"
                          ? "text-good"
                          : s.state === "failed"
                            ? "text-serious"
                            : s.state === "reverted"
                              ? "text-warning"
                              : "text-ink-3",
                      )}
                    >
                      {s.state}
                    </span>
                    {s.state === "done" && (
                      <span className="font-mono text-ink-3">
                        {s.new.slice(0, 12)}
                      </span>
                    )}
                    {s.note && (
                      <span className="min-w-0 basis-full break-words text-ink-2">
                        {s.note}
                      </span>
                    )}
                  </li>
                ))}
              </ul>
            </section>
          )}
        </div>

        <aside className="space-y-4">
          <div className="rounded-lg border border-borderline bg-surface-1 p-4">
            <div className="mb-2 text-sm font-medium text-ink">Actions</div>
            {!canWrite ? (
              // Not a refusal to come: landing, reverting and abandoning
              // are a writer's actions, and a reader is told what they
              // take rather than handed three buttons that each answer
              // "no changeset" when pressed.
              <p className="text-xs text-ink-3">
                Landing, reverting or abandoning this changeset takes write
                access to every member repository.
                {/* Additive, not a different sentence. FORGE-UX §0: the
                    page reads the same signed in and signed out, and
                    signing in adds actions — so a stranger gets the way
                    in *appended* to what a signed-in reader without
                    write access is already told, rather than a second
                    wording for the same fact. */}
                {!props.signedIn && props.loginHref && (
                  <>
                    {" "}
                    <a className={STRUCTURAL_LINK} href={props.loginHref}>
                      Sign in
                    </a>{" "}
                    if you have it.
                  </>
                )}
              </p>
            ) : (
              <div className="flex flex-col gap-2">
                <Button
                  type="button"
                  disabled={busy || !canLand}
                  onClick={land}
                >
                  {cs.state === "landing" ? "Landing…" : "Land all members"}
                </Button>
                {cs.state === "open" && verdict && !canLand && (
                  <span className="text-xs text-ink-3">
                    Every member&rsquo;s gate must be ready — a check still
                    running holds a changeset where it would hold a single
                    change.
                  </span>
                )}
                {cs.state !== "open" && cs.state !== "landing" && (
                  <span className="text-xs text-ink-3">
                    Only an open changeset can land; this one is{" "}
                    <span className="font-mono">{cs.state}</span>.
                  </span>
                )}
                {refusal && (
                  <div role="alert" className="text-xs text-ink-2">
                    <p className="break-words">{refusal.message}</p>
                    <div className="mt-1">
                      <LandBlockers blockers={refusal.waitingOn} />
                    </div>
                  </div>
                )}
                <Button
                  type="button"
                  variant="outline"
                  disabled={busy || !landedSomething}
                  onClick={() => setReverting((v) => !v)}
                >
                  {reverting ? "Cancel revert" : "Revert…"}
                </Button>
                <Button
                  type="button"
                  variant="outline"
                  disabled={busy || cs.state !== "open"}
                  onClick={() =>
                    act(
                      () => api.abandonChangeset(session, changesetKey),
                      "abandon",
                    )
                  }
                >
                  Abandon
                </Button>
              </div>
            )}
            {reverting && (
              <RevertForm original={cs} busy={busy} onSubmit={revert} />
            )}
            {conflicts && conflicts.length > 0 && (
              <ul className="mt-3 space-y-1 text-xs text-ink-2" role="alert">
                {conflicts.map((c) => (
                  <li key={label(c)} className="break-words">
                    <span className="font-mono text-ink">{label(c)}</span>:{" "}
                    {c.why}
                  </li>
                ))}
              </ul>
            )}
            <ErrLine message={error} />
          </div>

          {ws && ws.tip && (
            <div className="space-y-2">
              <CloneBlock httpsUrl={ws.clone_url} sshUrl={ws.ssh_clone_url} />
              <p className="text-xs text-ink-3">
                A read-only repository with one submodule per member at its
                proposed head. Clone with{" "}
                <code className="font-mono">--recurse-submodules</code> to check
                the whole proposal out; it changes whenever a member gains a
                patchset, so refresh with{" "}
                <code className="font-mono">git fetch</code> and a reset to{" "}
                <code className="font-mono">origin/workspace</code> rather than
                a pull.
              </p>
              {ws.note && (
                <p className="text-xs text-warning" role="status">
                  {ws.note}
                </p>
              )}
            </div>
          )}
        </aside>
      </div>
    </Frame>
  );
}

function AddMember(props: {
  session: Session;
  taken: ChangesetRef[];
  onAdd: (ref: ChangesetRef) => void;
}) {
  const { changes, error } = useOpenChanges(props.session);
  const [picked, setPicked] = useState<ChangesetRef[]>([]);
  return (
    <div className="mb-3 rounded-lg border border-borderline bg-surface-1 p-4">
      {error && !changes ? (
        <ErrLine message={error} />
      ) : !changes ? (
        <p className="text-sm text-ink-3">Loading open changes…</p>
      ) : (
        <ChangePicker
          // The members already here are not on offer: a change is in one
          // changeset at a time and these already are.
          changes={changes.filter(
            (c) =>
              !props.taken.some((t) => t.repo === c.repo && t.change === c.key),
          )}
          picked={picked}
          taken={props.taken}
          onToggle={(ref) => setPicked(isPicked(picked, ref) ? [] : [ref])}
        />
      )}
      <div className="mt-3">
        <Button
          type="button"
          size="xs"
          disabled={picked.length !== 1}
          onClick={() => props.onAdd(picked[0])}
        >
          Add to changeset
        </Button>
      </div>
    </div>
  );
}

/// Naming the reverting changeset. The server fills in the title and
/// body when they are left blank — `Revert "{title}"`, `Reverts
/// changeset {key}.` — so only the key is asked for, pre-filled.
function RevertForm(props: {
  original: Changeset;
  busy: boolean;
  onSubmit: (key: string, title: string) => void;
}) {
  const [key, setKey] = useState(revertKey(props.original.key));
  const [title, setTitle] = useState("");
  return (
    <form
      className="mt-3 space-y-2 border-t border-borderline pt-3"
      onSubmit={(e) => {
        e.preventDefault();
        props.onSubmit(key, title);
      }}
    >
      <p className="text-xs text-ink-3">
        Makes one revert commit per landed member on a new{" "}
        <code className="font-mono">revert/{key || "…"}</code> branch, opens a
        change for each, and composes them into a new changeset with the edges
        reversed. Nothing lands until that changeset does.
      </p>
      <label className="flex flex-col gap-1 text-xs text-ink-3">
        Key for the reverting changeset
        <input
          className="rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 font-mono text-sm text-ink"
          value={key}
          required
          onChange={(e) => setKey(e.target.value)}
        />
      </label>
      <label className="flex flex-col gap-1 text-xs text-ink-3">
        Title (optional)
        <input
          className="rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 text-sm text-ink"
          value={title}
          placeholder={`Revert "${props.original.title}"`}
          onChange={(e) => setTitle(e.target.value)}
        />
      </label>
      <Button type="submit" size="xs" disabled={props.busy || !validKey(key)}>
        Create the revert
      </Button>
    </form>
  );
}
