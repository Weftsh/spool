// The combined cross-repo diff: the whole changeset's code, in one
// place.
//
// Until this existed, reading a four-repo set meant four page loads and
// the changeset page showed no code at all — the surface built to hold
// one review over several repositories could not be used to perform
// one. FORGE-UX §6a named it as the largest deliberate gap; this is it
// closed.
//
// Three rules from that section are load-bearing here, and each of them
// is the reason a line of this file looks the way it does:
//
//   * **Group by member, always.** Never a flat path-sorted list across
//     repositories. Two repos' `README.md` adjacent under a small
//     prefix is how somebody approves the wrong file, so the rail is one
//     collapsible group per member — each its own tree, in a region
//     named for the repository — and the open file's header names its
//     repository first. A row's own name is the file's; the region and
//     the header are what say which repository.
//   * **A viewed mark belongs to one member.** Every tick is written
//     with *that member's* repo and change key. This surface is the one
//     place in the product where the wrong pair is even expressible,
//     which is why the Playwright suite asserts the request URL and body
//     rather than the tick's appearance.
//   * **One unreadable member is not a broken page.** Members have
//     different ACLs — a set can hold a repository this reader may not
//     see — so the fan-out is `Promise.allSettled` and a refusal
//     degrades to that group saying so, with a Retry. "The page is
//     broken" and "one repo is" are different sentences.
//
// The rail auto-collapses a group whose files are all viewed, and that
// single behaviour is what makes sixteen members tractable: the rail
// shortens as you work, so what is on screen is what is left to read.
// The header's count deliberately does not — it is over everything the
// filter matched, never over what is left after collapsing, because a
// progress number that shrinks as you make progress is worse than none.
//
// `+N −M` is the server's: `GET …/changesets/{key}/diffstat` counts
// every member's latest patchset and says `truncated` when a file was
// too large or too different to count. Per member and in total, never
// per file — the set has no background pre-read of sixteen members'
// texts, and a count invented off a file list would be a number we could
// not stand behind. 404 while any member has no patchset, which is
// rendered as no number rather than as an error.
//
// The lines themselves are the library's (`src/code/`), the same
// surface the single change view draws, minus the conversation: no line
// comments here, by FORGE-UX §6a — a line belongs to a repository, and
// that repository's OWNERS decides who is required on it.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  type ChangesetDiffstat,
  type ChangesetMember,
  type ChangesetVerdict,
  type DiffEntry,
  type Session,
} from "@/api";
import { CodeBoundary, LazyCodeDiff, LazyCodeTree, parseDiff } from "@/code/lazy";
import { treeHeight, treeRows } from "@/code/paths";
import type {
  FileDiffMetadata,
  FileDiffOptions,
  FileTreeRowDecorationRenderer,
  GitStatusEntry,
} from "@/code/types";
import { DiffStat } from "@/components/diff-stat";
import { cn } from "@/lib/utils";
import {
  type MemberDiff,
  filterAcross,
  memberDiffProgress,
  refLabel,
  setProgress,
} from "@/lib/changesets";
import {
  readDiffStyle,
  writeDiffStyle,
  type DiffStyle,
} from "@/views/change/diff-prefs";

/// Both sides of a file, or the fact that there is nothing to render. A
/// side that does not exist (an added or deleted file) is null.
type FileTexts = { old: string | null; new: string | null } | "binary";

/// What one member's file list is doing. Four states rather than a
/// nullable list, because a nullable list cannot tell "still on the
/// wire" from "nothing was asked for" from "refused", and those are
/// three different sentences.
///
/// `none` earns its place as a *terminal* marker rather than as
/// something the rail reads: the rail says "no patchset yet" off the
/// member's own `patchset`, which is the one source of truth for it, and
/// this entry is what stops the fan-out asking again on every re-render.
type MemberLoad =
  | { state: "loading" }
  | { state: "none" }
  | { state: "ok"; files: DiffEntry[] }
  | { state: "error"; message: string };

/// How many lines one press of the library's expand control uncovers —
/// the same screenful `FilesPanel` uses, because a reviewer moving
/// between a member's own page and this one should not find the
/// controls behaving differently.
const EXPAND_STEP = 24;

const NO_STAT = null;

const NO_PATHS: ReadonlySet<string> = new Set<string>();

/// `repo/change`, which identifies a member. A changeset holds at most
/// one change per repository, so this is as exact as the change id.
function memberId(m: ChangesetMember): string {
  return refLabel({ repo: m.repo, change: m.change.key });
}

/// The same, plus the commit on screen. Loads are cached under this so a
/// new patchset is a different entry rather than a stale one: nothing
/// has to remember to invalidate, and a member that pushed while you
/// read another one simply reloads.
function loadId(m: ChangesetMember): string {
  return `${memberId(m)}@${m.change.patchset?.commit ?? "-"}`;
}

export function CombinedFiles(props: {
  session: Session;
  /// The set's key, for the server's count of it.
  changeset: string;
  /// Members in landing order — the same list, in the same order, as the
  /// members table above. The rail is a second rendering of the plan and
  /// must not disagree with the first.
  members: ChangesetMember[];
  /// The composed verdict, read only for its per-path lists. A root
  /// commit has no parent rev to diff from, so — exactly as the single
  /// change view does — every path the verdict names is an addition.
  verdict: ChangesetVerdict | null;
}) {
  const { session, members, verdict } = props;
  const [loads, setLoads] = useState<Map<string, MemberLoad>>(new Map());
  /// Viewed paths per member, keyed by `repo/change` rather than by
  /// commit: the server answers them for whatever the latest patchset
  /// is, and so does the write.
  const [views, setViews] = useState<Map<string, Set<string>>>(new Map());
  /// Groups whose collapsed state the reader has decided for
  /// themselves, which overrides the auto-collapse below.
  const [override, setOverride] = useState<Map<string, boolean>>(new Map());
  const [open, setOpen] = useState<{ member: string; path: string } | null>(
    null,
  );
  const [texts, setTexts] = useState<Map<string, FileTexts>>(new Map());
  const [filter, setFilter] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [ignoreWs, setIgnoreWs] = useState<Set<string>>(new Set());
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  /// What the parser made of each text, keyed by the file's key and the
  /// whitespace choice. `null` is a parse that found no difference.
  const [parsed, setParsed] = useState<Map<string, FileDiffMetadata | null>>(
    new Map(),
  );
  const parsing = useRef(new Set<string>());
  const [diffStyle, setDiffStyle] = useState<DiffStyle>(() =>
    readDiffStyle(typeof localStorage === "undefined" ? null : localStorage),
  );
  /// The server's `+N −M`, per member and in total. Null until it
  /// answers, and null when it refuses — a set with a member that has no
  /// patchset yet is 404 here, and that is "no number", not an error.
  const [diffstat, setDiffstat] = useState<ChangesetDiffstat | null>(null);
  /// Bumped by Retry, so the effect below re-runs for a member whose
  /// entry it is about to drop.
  const [attempt, setAttempt] = useState(0);
  const inflight = useRef(new Set<string>());

  /// The paths a root-commit member touches, from the composed verdict.
  /// Null while the verdict has not landed — which is not an error and
  /// not "nothing to read", it is still loading, and saying either of
  /// the other two would be a lie the reader would act on.
  const rootEntries = useCallback(
    (m: ChangesetMember): DiffEntry[] | null => {
      const mv = verdict?.members.find(
        (v) => v.repo === m.repo && v.change === m.change.key,
      );
      if (!mv) return null;
      return mv.verdict.per_path.map((pp) => ({
        status: "added" as const,
        path: pp.path,
        old_oid: null,
        new_oid: null,
      }));
    },
    [verdict],
  );

  /// Fan out over the members, one request each.
  ///
  /// `Promise.allSettled`, never `Promise.all`: the members of one
  /// changeset are changes in different repositories with different
  /// ACLs, and a reader who may see three of four must get three file
  /// lists and one sentence — not a blank panel because the fourth
  /// rejected. Each member also settles its own failure into its own
  /// entry, so the outer settle is belt and braces; both are deliberate,
  /// because the day somebody adds a `throw` above the try block is the
  /// day `Promise.all` would take the section down.
  useEffect(() => {
    let alive = true;
    const wanted = new Set(members.map(loadId));
    setLoads((prev) => {
      const next = new Map(prev);
      // Drop entries for commits nobody is looking at any more, so a
      // member that pushed does not keep its old file list.
      for (const k of next.keys()) if (!wanted.has(k)) next.delete(k);
      for (const m of members)
        if (!next.has(loadId(m))) next.set(loadId(m), { state: "loading" });
      return next;
    });

    const one = async (m: ChangesetMember) => {
      const id = loadId(m);
      const ps = m.change.patchset;
      const put = (v: MemberLoad) => {
        if (alive) setLoads((prev) => new Map(prev).set(id, v));
      };
      // Viewed marks are read-only garnish on a diff that works without
      // them — a service token has none and is told so — so a refusal
      // here must not cost the reader the file list.
      void api
        .changeViews(session, m.repo, m.change.key)
        .then((v) => {
          if (!alive) return;
          setViews((prev) => new Map(prev).set(memberId(m), new Set(v.viewed)));
        })
        .catch(() => {});
      if (!ps) {
        put({ state: "none" });
        return;
      }
      try {
        if (!ps.parent) {
          const entries = rootEntries(m);
          if (entries) put({ state: "ok", files: entries });
          return;
        }
        const files = await api.structuralDiff(
          session,
          m.repo,
          ps.parent,
          ps.commit,
        );
        put({ state: "ok", files });
      } catch (e) {
        put({
          state: "error",
          message: e instanceof Error ? e.message : String(e),
        });
      }
    };

    const pending = members.filter((m) => {
      const at = loads.get(loadId(m));
      return at === undefined || at.state === "loading";
    });
    void Promise.allSettled(pending.map(one));
    return () => {
      alive = false;
    };
    // `loads` is read to decide what still needs fetching and written by
    // the fetch; depending on it would re-run the effect on its own
    // result. The signature of the member set, the verdict the
    // root-commit fallback reads, and the Retry counter are what change
    // the answer.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session, members.map(loadId).join("\n"), rootEntries, attempt]);

  /// Read both sides of one file. Only ever for the file the reader
  /// asked for: sixteen members' worth of background pre-reading is a
  /// thousand requests to decorate rows nobody has scrolled to, and
  /// without `+N −M` there is nothing they would decorate.
  const fetchTexts = useCallback(
    async (m: ChangesetMember, entry: DiffEntry) => {
      const ps = m.change.patchset;
      if (!ps) return;
      const key = `${memberId(m)}\n${entry.path}`;
      if (inflight.current.has(key)) return;
      inflight.current.add(key);
      try {
        const missingOld = entry.status === "added" || !ps.parent;
        const missingNew = entry.status === "deleted";
        const [oldText, newText] = await Promise.all([
          missingOld
            ? Promise.resolve<string | null>(null)
            : api.fileText(session, m.repo, entry.path, ps.parent as string),
          missingNew
            ? Promise.resolve<string | null>(null)
            : api.fileText(session, m.repo, entry.path, ps.commit),
        ]);
        const value: FileTexts =
          (!missingOld && oldText === null) || (!missingNew && newText === null)
            ? "binary"
            : { old: oldText, new: newText };
        setTexts((t) => new Map(t).set(key, value));
      } catch (e) {
        inflight.current.delete(key);
        setError(
          `Could not load ${m.repo}/${entry.path}: ${
            e instanceof Error ? e.message : e
          }`,
        );
      }
    },
    [session],
  );

  const byId = useMemo(() => {
    const map = new Map<string, ChangesetMember>();
    for (const m of members) map.set(memberId(m), m);
    return map;
  }, [members]);

  /// The count, once per set of commits on screen. Read-only garnish: a
  /// refusal leaves the numbers off and the files readable.
  const signature = members.map(loadId).join("\n");
  useEffect(() => {
    let alive = true;
    setDiffstat(null);
    api
      .changesetDiffstat(session, props.changeset)
      .then((d) => alive && setDiffstat(d))
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [session, props.changeset, signature]);

  /// Parse whatever has text and no parse yet; the parser lives in the
  /// lazy chunk with the renderer. No cleanup flag: the effect re-runs
  /// whenever a text lands, and a result dropped after that ran would
  /// leave its key in `parsing` and the file at "Loading…" forever — the
  /// bug `FilesPanel` had. A late result is right for its key.
  useEffect(() => {
    for (const [key, t] of texts) {
      if (t === "binary") continue;
      const pk = `${key}\n${ignoreWs.has(key)}`;
      if (parsed.has(pk) || parsing.current.has(pk)) continue;
      parsing.current.add(pk);
      const path = key.split("\n")[1];
      void parseDiff(
        t.old === null ? null : { name: path, contents: t.old },
        t.new === null ? null : { name: path, contents: t.new },
        { ignoreWhitespace: ignoreWs.has(key) },
      ).then((d) => {
        setParsed((m) => new Map(m).set(pk, d));
      });
    }
  }, [texts, ignoreWs, parsed]);

  /// Every member as the pure helpers see it. The mapping is the whole
  /// seam: everything below this line is markup, and everything the
  /// arithmetic does — the progress counts, the filter, the rule that
  /// decides a group is finished — is in `lib/changesets.ts` where a
  /// unit test can reach it without a browser.
  const all: MemberDiff[] = members.map((m) => {
    const at = loads.get(loadId(m));
    return {
      repo: m.repo,
      change: m.change.key,
      patchset: m.change.patchset?.number ?? null,
      files: at?.state === "ok" ? at.files : null,
      viewed: views.get(memberId(m)) ?? NO_PATHS,
      error: at?.state === "error" ? at.message : null,
    };
  });
  const shown = filterAcross(all, filter);
  const progress = setProgress(shown);

  /// Whole set empty: the section does not render. A changeset with no
  /// members has no code to show and a heading saying so is furniture.
  if (members.length === 0) return null;

  function collapsedFor(m: MemberDiff): boolean {
    const id = refLabel({ repo: m.repo, change: m.change });
    const chosen = override.get(id);
    if (chosen !== undefined) return chosen;
    return memberDiffProgress(m).complete;
  }

  function toggleGroup(m: MemberDiff) {
    const id = refLabel({ repo: m.repo, change: m.change });
    setOverride((o) => new Map(o).set(id, !collapsedFor(m)));
  }

  function select(id: string, entry: DiffEntry) {
    const m = byId.get(id);
    if (!m) return;
    setError(null);
    setOpen({ member: id, path: entry.path });
    if (!texts.has(`${id}\n${entry.path}`)) void fetchTexts(m, entry);
  }

  function retry(id: string) {
    const m = byId.get(id);
    if (!m) return;
    setLoads((prev) => new Map(prev).set(loadId(m), { state: "loading" }));
    setAttempt((n) => n + 1);
  }

  /// Tick or untick one path, against **this member's** repo and change
  /// key. The optimistic write and its rollback are the single change
  /// view's, for the same reason: a tick that waits for a round trip
  /// reads as a broken checkbox.
  async function markViewed(id: string, path: string, next: boolean) {
    const m = byId.get(id);
    if (!m) return;
    setViews((prev) => {
      const now = new Map(prev);
      const set = new Set(now.get(id) ?? []);
      if (next) set.add(path);
      else set.delete(path);
      return now.set(id, set);
    });
    // The file stays open. On a single change, ticking folds the diff
    // away and the *list* is what shortens; here the rail is that list
    // and the group collapses on its own once the last file is ticked,
    // so closing the body as well would take the tick out from under the
    // cursor that just pressed it — and leave nothing to untick with.
    try {
      await api.setChangeViewed(session, m.repo, m.change.key, path, next);
    } catch (e) {
      setViews((prev) => {
        const now = new Map(prev);
        const set = new Set(now.get(id) ?? []);
        if (next) set.delete(path);
        else set.add(path);
        return now.set(id, set);
      });
      setError(
        `Could not mark ${m.repo}/${path} ${next ? "viewed" : "unviewed"}: ${
          e instanceof Error ? e.message : e
        }`,
      );
    }
  }

  const openMember = open ? byId.get(open.member) : undefined;
  const openKey = open ? `${open.member}\n${open.path}` : null;
  const openText = openKey ? texts.get(openKey) : undefined;
  const wsOff = openKey ? ignoreWs.has(openKey) : false;
  /// Undefined while the text or its parse is on the way; null for a
  /// parse that found no difference.
  const openDiff =
    openKey && openText && openText !== "binary"
      ? parsed.get(`${openKey}\n${wsOff}`)
      : undefined;
  const isExpanded = openKey ? expanded.has(openKey) : false;

  function setExpandedFor(key: string, on: boolean) {
    setExpanded((s) => {
      const next = new Set(s);
      if (on) next.add(key);
      else next.delete(key);
      return next;
    });
  }

  function setIgnoreWhitespace(key: string, on: boolean) {
    setIgnoreWs((s) => {
      const next = new Set(s);
      if (on) next.add(key);
      else next.delete(key);
      return next;
    });
  }

  function chooseStyle(style: DiffStyle) {
    setDiffStyle(style);
    writeDiffStyle(typeof localStorage === "undefined" ? null : localStorage, style);
  }

  /// Read-only, no comments, no selection: the lines and the folds.
  const diffOptions = useMemo<FileDiffOptions<undefined, undefined>>(
    () => ({
      diffStyle,
      expandUnchanged: isExpanded,
      expansionLineCount: EXPAND_STEP,
      hunkSeparators: "line-info",
      overflow: "wrap",
      stickyHeader: false,
      enableLineSelection: false,
    }),
    [diffStyle, isExpanded],
  );

  /// The server's count for one member, if it answered.
  const statFor = (m: MemberDiff) => {
    const row = diffstat?.members.find(
      (r) => r.repo === m.repo && r.change === m.change,
    );
    return row ? { added: row.insertions, deleted: row.deletions } : NO_STAT;
  };

  const repos = new Set(members.map((m) => m.repo)).size;

  return (
    <section
      className="rounded-lg border border-borderline bg-surface-1 p-4"
      aria-labelledby="combined-files-heading"
    >
      <div className="mb-1 flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <h2
          id="combined-files-heading"
          className="text-sm font-medium text-ink"
        >
          Files across {repos} {repos === 1 ? "repository" : "repositories"}
        </h2>
        <span className="text-xs text-ink-3">
          {progress.files} {progress.files === 1 ? "file" : "files"} ·{" "}
          {progress.seen} of {progress.files} viewed
        </span>{" "}
        {/* The server's arithmetic over the whole set. `truncated` is
            said out loud beside it: a count that silently omitted the
            one enormous file would be wrong in the direction nobody
            checks. */}
        {diffstat && (
          <>
            <DiffStat
              stat={{
                added: diffstat.total.insertions,
                deleted: diffstat.total.deletions,
              }}
            />
            {diffstat.total.truncated && (
              <span className="text-xs text-ink-3">(some files not counted)</span>
            )}
          </>
        )}
        <fieldset className="ml-auto flex items-center gap-2 text-xs text-ink-3">
          <legend className="sr-only">Diff layout</legend>
          {(["unified", "split"] as const).map((style) => (
            <label key={style} className="flex items-center gap-1">
              <input
                type="radio"
                name="changeset-diff-layout"
                className="accent-brand"
                checked={diffStyle === style}
                onChange={() => chooseStyle(style)}
              />
              {style === "unified" ? "Unified" : "Split"}
            </label>
          ))}
        </fieldset>
      </div>
      <p className="mb-3 text-xs text-ink-3">
        The whole set, grouped by member and in landing order. A group collapses
        once every file in it is viewed. Approval still happens on each member’s
        own page, under that repository’s OWNERS.
      </p>
      {error && (
        <p className="mb-2 text-sm text-serious" role="alert">
          {error}
        </p>
      )}

      {/* The rail beside the diff only on a wide screen; below that it
          sits above, full width, so the code keeps the room it needs. */}
      <div className="flex flex-col gap-3 2xl:flex-row 2xl:items-start">
        <nav
          aria-label="Files across this changeset"
          className="shrink-0 2xl:w-72 2xl:max-w-72"
        >
          <input
            className="mb-2 w-full rounded-md border border-borderline bg-surface-0 px-2 py-1 text-xs"
            aria-label="Filter files by repository or path"
            placeholder="filter by repo/path"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
          <div className="max-h-96 overflow-y-auto overflow-x-auto">
            {shown.length === 0 && (
              <p className="px-1.5 py-1 text-xs text-ink-3">
                No file in this changeset matches “{filter.trim()}”.
              </p>
            )}
            <ul className="space-y-2">
              {shown.map((m) => {
                const id = refLabel({ repo: m.repo, change: m.change });
                const p = memberDiffProgress(m);
                const shut = collapsedFor(m);
                return (
                  <li key={id}>
                    <button
                      type="button"
                      aria-expanded={!shut}
                      aria-label={`${m.repo} — ${m.change}${
                        m.patchset === null ? "" : `, patchset ${m.patchset}`
                      }, ${p.seen} of ${p.total} viewed`}
                      className="flex w-full items-baseline gap-1.5 rounded px-1.5 py-1 text-left hover:bg-surface-2"
                      onClick={() => toggleGroup(m)}
                    >
                      <span aria-hidden className="shrink-0 text-xs text-ink-3">
                        {shut ? "▸" : "▾"}
                      </span>
                      <span className="min-w-0 flex-1">
                        <span className="block break-words text-xs font-medium text-ink">
                          {m.repo}
                        </span>
                        <span className="block font-mono text-xs text-ink-3">
                          {m.change}
                          {m.patchset !== null && ` · patchset ${m.patchset}`}
                        </span>
                      </span>
                      <span
                        aria-hidden
                        className={cn(
                          "shrink-0 font-mono text-xs",
                          p.complete ? "text-good" : "text-ink-3",
                        )}
                      >
                        {p.seen}/{p.total}
                      </span>
                    </button>
                    {/* Beside the button, not inside it, so the group's
                        accessible name stays the sentence the specs and
                        the walkthrough read. */}
                    {(() => {
                      const stat = statFor(m);
                      return stat ? (
                        <div className="px-1.5 text-right">
                          <DiffStat stat={stat} hidden />
                        </div>
                      ) : null;
                    })()}
                    {!shut && (
                      <div className="mt-1 border-l border-borderline pl-1.5">
                        {m.error !== null ? (
                          <div className="px-1.5 py-1">
                            <p className="text-xs text-ink-3">
                              {m.repo} could not be read: {m.error}
                            </p>
                            <button
                              type="button"
                              className="mt-1 rounded px-1 text-xs text-brand hover:underline"
                              aria-label={`Retry ${m.repo}`}
                              onClick={() => retry(id)}
                            >
                              Retry
                            </button>
                          </div>
                        ) : m.patchset === null ? (
                          <p className="px-1.5 py-1 text-xs text-ink-3">
                            No patchset yet — nothing to read here.
                          </p>
                        ) : m.files === null ? (
                          <p className="px-1.5 py-1 text-xs text-ink-3">
                            Loading…
                          </p>
                        ) : m.files.length === 0 ? (
                          <p className="px-1.5 py-1 text-xs text-ink-3">
                            This patchset changes nothing against its parent.
                          </p>
                        ) : (
                          // One tree per member, in a region named for
                          // the repository. The rows carry only the
                          // file's own name; the region and the open
                          // file's header are what say which repository,
                          // and a spec that wants `web`'s README scopes
                          // through the region.
                          <div role="group" aria-label={`${m.repo} files`}>
                            <MemberTree
                              files={m.files}
                              open={open?.member === id ? open.path : null}
                              viewed={m.viewed}
                              onSelect={(path) => {
                                const entry = m.files?.find((f) => f.path === path);
                                if (entry) select(id, entry);
                              }}
                            />
                          </div>
                        )}
                      </div>
                    )}
                  </li>
                );
              })}
            </ul>
          </div>
        </nav>

        <div className="min-w-0 flex-1">
          {!open || !openMember ? (
            <p className="px-2 py-1.5 text-sm text-ink-3">
              Choose a file from the rail to read it. One at a time, so the
              header can always say which repository you are looking at.
            </p>
          ) : (
            <div className="overflow-hidden rounded-md border border-borderline bg-surface-0">
              {/* The repository first, always. Two members will both
                  have a `README.md`, and approving the wrong one is the
                  failure this header exists to prevent. */}
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-borderline px-3 py-1.5">
                <div className="min-w-0 font-mono text-xs">
                  <span className="text-ink">{openMember.repo}</span>{" "}
                  <span aria-hidden className="text-ink-3">
                    ·
                  </span>{" "}
                  <span className="break-all text-ink-2">{open.path}</span>
                </div>
                <label className="ml-auto flex items-center gap-1.5 text-xs text-ink-3">
                  <input
                    type="checkbox"
                    className="size-3.5 accent-brand"
                    aria-label={`Viewed ${openMember.repo}/${open.path}`}
                    checked={views.get(open.member)?.has(open.path) ?? false}
                    onChange={(e) =>
                      void markViewed(open.member, open.path, e.target.checked)
                    }
                  />
                  Viewed
                </label>
              </div>
              {openText && openText !== "binary" && openKey && (
                <div className="flex flex-wrap items-center gap-x-4 gap-y-1 border-b border-borderline px-3 py-1.5 text-xs text-ink-3">
                  <label className="flex items-center gap-1.5">
                    <input
                      type="checkbox"
                      className="size-3.5 accent-brand"
                      checked={wsOff}
                      onChange={(e) =>
                        setIgnoreWhitespace(openKey, e.target.checked)
                      }
                    />
                    Ignore whitespace
                  </label>
                  {openDiff && !isExpanded && (
                    <button
                      type="button"
                      className="rounded px-1 hover:text-ink focus:text-ink"
                      onClick={() => setExpandedFor(openKey, true)}
                    >
                      Expand whole file
                    </button>
                  )}
                  {openDiff && isExpanded && (
                    <button
                      type="button"
                      className="rounded px-1 hover:text-ink focus:text-ink"
                      onClick={() => setExpandedFor(openKey, false)}
                    >
                      Collapse to changes
                    </button>
                  )}
                </div>
              )}
              {openText === undefined && (
                <p className="p-3 text-sm text-ink-3">Loading diff…</p>
              )}
              {openText === "binary" && (
                <p className="p-3 text-sm text-ink-3">
                  Binary or too large to render — the content differs.
                </p>
              )}
              {openText && openText !== "binary" && openDiff === undefined && (
                <p className="p-3 text-sm text-ink-3">Loading diff…</p>
              )}
              {openDiff === null && (
                <p className="p-3 text-sm text-ink-3">
                  {wsOff
                    ? "No difference once whitespace is ignored."
                    : "No text difference (mode or metadata change)."}
                </p>
              )}
              {openDiff && (
                <CodeBoundary
                  fallback={<p className="p-3 text-sm text-ink-3">Loading diff…</p>}
                >
                  <LazyCodeDiff fileDiff={openDiff} options={diffOptions} />
                </CodeBoundary>
              )}
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

/// One member's rail: the files its patchset touches, as a tree, every
/// directory open, with the viewed ones ticked. A row's accessible name
/// is the file's own — the region around this tree carries the
/// repository, and the open file's header repeats it.
function MemberTree(props: {
  files: DiffEntry[];
  open: string | null;
  viewed: ReadonlySet<string>;
  onSelect: (path: string) => void;
}) {
  const paths = useMemo(() => props.files.map((f) => f.path), [props.files]);
  const status = useMemo<GitStatusEntry[]>(
    () => props.files.map((f) => ({ path: f.path, status: f.status })),
    [props.files],
  );
  const viewedRef = useRef(props.viewed);
  viewedRef.current = props.viewed;
  const decorate = useCallback<FileTreeRowDecorationRenderer>(
    ({ item }) =>
      item.kind === "file" && viewedRef.current.has(item.path)
        ? { text: "✓", title: "Viewed" }
        : null,
    [],
  );
  // The decoration reads through a ref; a fresh paths array is what
  // tells the tree a tick changed.
  const versioned = useMemo(
    () => [...paths],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [paths, props.viewed],
  );
  return (
    <CodeBoundary fallback={<p className="px-1.5 py-1 text-xs text-ink-3">Loading…</p>}>
      <LazyCodeTree
        label="Files"
        paths={versioned}
        gitStatus={status}
        renderRowDecoration={decorate}
        selectedPath={props.open}
        onSelect={props.onSelect}
        height={treeHeight(treeRows(paths), 320)}
      />
    </CodeBoundary>
  );
}
