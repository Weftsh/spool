import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { api, type DiffEntry, type Patchset, type Session } from "@/api";
import { CodeBoundary, LazyCodeTree, parseDiff } from "@/code/lazy";
import { treeHeight, treeRows } from "@/code/paths";
import type {
  FileDiffMetadata,
  FileTreeRowDecorationRenderer,
  GitStatusEntry,
} from "@/code/types";
import { DiffStat } from "@/components/diff-stat";
import { cn } from "@/lib/utils";
import { parseLineAnchor } from "@/views/change/anchors";
import {
  readDiffStyle,
  writeDiffStyle,
  type DiffStyle,
} from "@/views/change/diff-prefs";
import { firstChangedLine, lineInHunks, sumChanges } from "@/views/change/diffstat";
import { ReviewFileDiff } from "@/views/change/file-diff";
import { LineComposer, type LineDraft } from "@/views/change/line-composer";
import type { Side, Thread } from "@/views/change/threads";

/// Both sides of a file, or the fact that there is nothing to render.
type FileTexts = { old: string | null; new: string | null } | "binary";

/// Past this many files the panel stops pre-reading them for the `+N −M`
/// counts. See the effect that uses it.
const COUNT_BUDGET = 60;

function ErrLine(props: { message: string | null }) {
  if (!props.message) return null;
  return (
    <p className="mt-2 text-sm text-serious" role="alert">
      {props.message}
    </p>
  );
}

/// The files the patchset touches, each expandable into a line diff —
/// a reviewer must be able to read what they are approving, in place,
/// and say why on the exact line.
///
/// The lines themselves are the library's (`src/code/`): highlighted,
/// unified or split, with the unchanged regions folded and the
/// library's own controls to unfold them. What is ours is everything a
/// review is made of — which files, which are read, the counts, the
/// whitespace choice, the conversation hanging off the lines, and the
/// addresses a link into the diff resolves to.
export function FilesPanel(props: {
  session: Session;
  repo: string;
  files: DiffEntry[] | null;
  parent: string | null;
  commit: string | null;
  /// The conversation, already threaded. Grouped once by the view above
  /// so the diff and the conversation panel cannot disagree about which
  /// remark a reply belongs to.
  threads: Thread[];
  /// How many threads nobody has settled — a fact printed in the header,
  /// beside the viewed count. It gates nothing.
  unresolved: number;
  /// One thread, drawn. The card lives beside the conversation panel so
  /// the two render a remark the same way.
  renderThread: (thread: Thread) => ReactNode;
  busy: boolean;
  /// Whether this server knows about batched reviews, which is the only
  /// thing that decides whether a line remark can be drafted rather than
  /// published.
  batched: boolean;
  /// Paths this reviewer has already read, as the server reports them
  /// for the patchset on screen.
  viewed: Set<string>;
  onToggleViewed: (path: string, viewed: boolean) => void;
  /// Post a comment anchored to one side of one file. `lineEnd` is the
  /// inclusive last line of a range, or null for a single line;
  /// `pending` drafts it into the reviewer's own review instead of
  /// publishing it.
  onLineComment: (
    path: string,
    side: Side,
    line: number,
    lineEnd: number | null,
    body: string,
    pending: boolean,
  ) => Promise<boolean>;
  /// Every patchset so far, oldest first, for "since patchset N".
  patchsets: Patchset[];
  /// Which earlier patchset the file list is against, or null for the
  /// parent. Owned by the view above, which fetches the entries.
  baseline: number | null;
  onBaseline: (n: number | null) => void;
}) {
  const { session, repo, files, parent, commit } = props;
  const [open, setOpen] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [hideViewed, setHideViewed] = useState(false);
  const [diffStyle, setDiffStyle] = useState<DiffStyle>(() =>
    readDiffStyle(typeof localStorage === "undefined" ? null : localStorage),
  );
  /// Both sides of every file whose text has been read, keyed by path.
  /// Kept as text rather than as a rendered diff because the whitespace
  /// toggle re-parses the same file, and the counts are a parse of every
  /// file at once.
  const [texts, setTexts] = useState<Map<string, FileTexts>>(new Map());
  /// What the parser made of each text, keyed by commit, whitespace
  /// choice and path — the three things that change the answer. `null`
  /// is a parse that found no difference.
  const [parsed, setParsed] = useState<Map<string, FileDiffMetadata | null>>(
    new Map(),
  );
  const [error, setError] = useState<string | null>(null);
  const [draft, setDraft] = useState<LineDraft | null>(null);
  const [draftBody, setDraftBody] = useState("");
  /// Paths a reviewer has asked to diff with whitespace ignored, and the
  /// files opened out to their full length. Both are per change rather
  /// than per file view: closing a file and coming back to it should
  /// not throw away what you had unfolded.
  const [ignoreWs, setIgnoreWs] = useState<Set<string>>(new Set());
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  /// The `#path:L<n>` a link into this diff carries, if any.
  const [anchor, setAnchor] = useState<{ path: string; line: number } | null>(
    () => parseLineAnchor(window.location.hash),
  );
  const inflight = useRef(new Set<string>());
  const parsing = useRef(new Set<string>());

  /// Read both sides of one file, once. `loud` is the difference between
  /// a reviewer who asked for this file — who must be told when it could
  /// not be fetched — and the background pass behind the counts, where
  /// the honest answer to a failure is no number rather than an error
  /// banner over a diff nobody asked for.
  const fetchTexts = useCallback(
    async (entry: DiffEntry, loud: boolean) => {
      if (!commit) return;
      if (inflight.current.has(entry.path)) return;
      inflight.current.add(entry.path);
      try {
        const [oldText, newText] = await Promise.all([
          entry.status === "added" || !parent
            ? Promise.resolve<string | null>(null)
            : api.fileText(session, repo, entry.path, parent),
          entry.status === "deleted"
            ? Promise.resolve<string | null>(null)
            : api.fileText(session, repo, entry.path, commit),
        ]);
        // A side that should exist and came back null is binary or too
        // large; a side that should not exist is simply absent.
        const missingOld = entry.status === "added" || !parent;
        const missingNew = entry.status === "deleted";
        const value: FileTexts =
          (!missingOld && oldText === null) || (!missingNew && newText === null)
            ? "binary"
            : { old: oldText, new: newText };
        setTexts((m) => new Map(m).set(entry.path, value));
      } catch (e) {
        inflight.current.delete(entry.path);
        if (loud) {
          setError(
            `Could not load the diff: ${e instanceof Error ? e.message : e}`,
          );
        }
      }
    },
    [session, repo, parent, commit],
  );

  /// A patchset lands and every cached text is about a commit nobody is
  /// looking at any more. Dropping them here rather than trusting the
  /// panel to be remounted is what stops a reviewer reading the previous
  /// revision's diff under the new revision's file list.
  useEffect(() => {
    inflight.current = new Set();
    parsing.current = new Set();
    setTexts(new Map());
    setParsed(new Map());
    setExpanded(new Set());
  }, [commit, parent]);

  /// Read every file of the change in the background, so the counts are
  /// on screen before anything is clicked. Bounded twice: a handful at a
  /// time, and not at all past `COUNT_BUDGET` files.
  useEffect(() => {
    if (!files || !commit || files.length > COUNT_BUDGET) return;
    let alive = true;
    const queue = [...files];
    const worker = async () => {
      for (let next = queue.shift(); next && alive; next = queue.shift()) {
        await fetchTexts(next, false);
      }
    };
    void Promise.all(Array.from({ length: Math.min(4, queue.length) }, worker));
    return () => {
      alive = false;
    };
  }, [files, commit, fetchTexts]);

  const parseKey = (path: string) =>
    `${commit}\n${ignoreWs.has(path)}\n${path}`;

  /// Parse whatever has text and no parse yet. Async because the parser
  /// lives in the lazy chunk with the renderer; each result lands under
  /// its own key, so a whitespace toggle mid-flight cannot mislabel one.
  ///
  /// No cleanup flag, on purpose. This effect re-runs every time a text
  /// lands, and the background pre-read lands three in quick succession;
  /// a version that dropped results after its own cleanup ran orphaned
  /// every parse but the last — the key stayed in `parsing`, so the file
  /// was never parsed again and its diff read "Loading…" for good. A
  /// late result is still the right answer for its key, and a key
  /// nobody asks for any more is a map entry, not a wrong number.
  useEffect(() => {
    for (const [path, t] of texts) {
      if (t === "binary") continue;
      const key = parseKey(path);
      if (parsed.has(key) || parsing.current.has(key)) continue;
      parsing.current.add(key);
      void parseDiff(
        t.old === null ? null : { name: path, contents: t.old },
        t.new === null ? null : { name: path, contents: t.new },
        { ignoreWhitespace: ignoreWs.has(path) },
      ).then((d) => {
        setParsed((m) => new Map(m).set(key, d));
      });
    }
    // parseKey closes over commit and ignoreWs, both listed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [texts, ignoreWs, commit, parsed]);

  /// `+N −M` per path. Undefined while the text or its parse is still
  /// on the way, null for a file there is nothing to count in.
  const statFor = (path: string) => {
    const t = texts.get(path);
    if (!t) return undefined;
    if (t === "binary") return null;
    const d = parsed.get(parseKey(path));
    if (d === undefined) return undefined;
    return d === null ? { added: 0, deleted: 0 } : sumChanges(d);
  };

  function toggle(entry: DiffEntry) {
    setDraft(null);
    setDraftBody("");
    if (open === entry.path) {
      setOpen(null);
      return;
    }
    setOpen(entry.path);
    setError(null);
    if (!texts.has(entry.path)) void fetchTexts(entry, true);
  }

  /// Send the line remark under construction, published or drafted.
  function postLineComment(path: string, body: string, pending: boolean) {
    if (!draft || !body) return;
    // Only a real range goes on the wire. `line_end` equal to `line` is
    // what the server stores for a single-line comment anyway, and
    // sending an end below the start is a 400 the reviewer would meet
    // after writing their remark.
    const end = draft.end !== null && draft.end > draft.line ? draft.end : null;
    void props
      .onLineComment(path, draft.side, draft.line, end, body, pending)
      .then((ok) => {
        if (!ok) return;
        setDraft(null);
        setDraftBody("");
      });
  }

  /// Opening from the rail is not a toggle: clicking a file in a tree
  /// means "show me this one", and closing what you just asked for is
  /// never what was meant.
  function select(path: string) {
    const entry = files?.find((f) => f.path === path);
    if (entry && open !== entry.path) toggle(entry);
  }

  function setIgnoreWhitespace(path: string, on: boolean) {
    setIgnoreWs((s) => {
      const next = new Set(s);
      if (on) next.add(path);
      else next.delete(path);
      return next;
    });
  }

  function setExpandedFor(path: string, on: boolean) {
    setExpanded((s) => {
      const next = new Set(s);
      if (on) next.add(path);
      else next.delete(path);
      return next;
    });
  }

  function chooseStyle(style: DiffStyle) {
    setDiffStyle(style);
    writeDiffStyle(typeof localStorage === "undefined" ? null : localStorage, style);
  }

  /// A link into this diff — `#path:L<n>`, which is what a line comment
  /// hands somebody — opens that file, unfolds it and lands on the line.
  useEffect(() => {
    const sync = () => setAnchor(parseLineAnchor(window.location.hash));
    window.addEventListener("hashchange", sync);
    return () => window.removeEventListener("hashchange", sync);
  }, []);

  /// Applied once per anchor: without that, expanding or collapsing
  /// anything would snap the view back to the linked line.
  const applied = useRef<string | null>(null);
  useEffect(() => {
    if (!anchor || !files) return;
    const id = `${anchor.path}:${anchor.line}`;
    if (applied.current === id) return;
    const entry = files.find((f) => f.path === anchor.path);
    if (!entry) return;
    applied.current = id;
    setOpen(anchor.path);
    setError(null);
    // The whole file, so the line is on screen whichever fold it was in.
    setExpandedFor(anchor.path, true);
    if (!texts.has(entry.path)) void fetchTexts(entry, true);
  }, [anchor, files, texts, fetchTexts]);

  const glyph: Record<DiffEntry["status"], { g: string; tone: string }> = {
    added: { g: "+", tone: "text-good" },
    modified: { g: "±", tone: "text-warning" },
    deleted: { g: "−", tone: "text-serious" },
  };

  /// Ticking the box folds the file away; unticking opens it back up.
  function markViewed(entry: DiffEntry, checked: boolean) {
    props.onToggleViewed(entry.path, checked);
    if (checked && open === entry.path) {
      setOpen(null);
    } else if (!checked && open !== entry.path) {
      toggle(entry);
    }
  }

  const needle = filter.trim().toLowerCase();
  const matched = (files ?? []).filter((f) =>
    f.path.toLowerCase().includes(needle),
  );
  // The count is over everything the filter matched, not over what is
  // left after hiding — a progress number that shrank as you made
  // progress would be worse than none.
  const seen = matched.filter((f) => props.viewed.has(f.path)).length;
  const shown = hideViewed
    ? matched.filter((f) => !props.viewed.has(f.path))
    : matched;

  // The rail, as the tree component wants it: paths, what happened to
  // each, and what to print beside each row. The viewed tick and the
  // counts are decoration, outside the row's accessible name, so a
  // screen reader hears the file and not its arithmetic twice.
  const railPaths = useMemo(() => shown.map((f) => f.path), [shown]);
  const railStatus = useMemo<GitStatusEntry[]>(
    () => shown.map((f) => ({ path: f.path, status: f.status })),
    [shown],
  );
  const viewedRef = useRef(props.viewed);
  viewedRef.current = props.viewed;
  const statRef = useRef(statFor);
  statRef.current = statFor;
  const decorate = useCallback<FileTreeRowDecorationRenderer>(({ item }) => {
    if (item.kind !== "file") return null;
    const stat = statRef.current(item.path);
    const tick = viewedRef.current.has(item.path) ? "✓ " : "";
    if (!stat) return tick ? { text: "✓", title: "Viewed" } : null;
    return {
      text: `${tick}+${stat.added} −${stat.deleted}`,
      title: `${stat.added} added, ${stat.deleted} removed`,
    };
  }, []);
  // The decoration reads through refs, so a count that lands after the
  // rail mounted needs the tree told to redraw; a new paths array does
  // that, and is cheap.
  const railVersion = [...texts.keys()].length + [...parsed.keys()].length + seen;
  const railPathsVersioned = useMemo(
    () => [...railPaths],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [railPaths, railVersion],
  );

  const openEntry = open ? files?.find((f) => f.path === open) : undefined;
  const openText = open ? texts.get(open) : undefined;
  const openDiff = open && openText && openText !== "binary" ? parsed.get(parseKey(open)) : undefined;

  return (
    <div className="rounded-lg border border-borderline bg-surface-1 p-4">
      <div className="mb-1 flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <div className="text-sm font-medium text-ink">
          Files in this patchset
        </div>
        {files && files.length > 0 && (
          <span className="text-xs text-ink-3">
            {seen} of {matched.length} viewed
          </span>
        )}
        {/* A count, and nothing more. It is deliberately not phrased as
            "N to resolve" and nothing on this page turns it into a
            blocker. */}
        {props.unresolved > 0 && (
          <span className="text-xs text-warning">
            {props.unresolved} unresolved
          </span>
        )}
        {files && files.length > 0 && seen > 0 && (
          <label className="flex items-center gap-1.5 text-xs text-ink-3">
            <input
              type="checkbox"
              className="size-3.5 accent-brand"
              checked={hideViewed}
              onChange={(e) => setHideViewed(e.target.checked)}
            />
            Hide viewed
          </label>
        )}
        {/* Which earlier reading this list is measured from. Only once
            there is an earlier one. */}
        {props.patchsets.length > 1 && (
          <select
            aria-label="Compare against"
            className="rounded-md border border-borderline bg-surface-0 px-2 py-0.5 text-xs"
            value={props.baseline ?? ""}
            onChange={(e) =>
              props.onBaseline(e.target.value === "" ? null : Number(e.target.value))
            }
          >
            <option value="">parent</option>
            {props.patchsets.slice(0, -1).map((p) => (
              <option key={p.number} value={p.number}>
                since patchset {p.number}
              </option>
            ))}
          </select>
        )}
        <fieldset className="ml-auto flex items-center gap-2 text-xs text-ink-3">
          <legend className="sr-only">Diff layout</legend>
          {(["unified", "split"] as const).map((style) => (
            <label key={style} className="flex items-center gap-1">
              <input
                type="radio"
                name="diff-layout"
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
        What you approve is what lands — read it here. Ticking a file remembers
        that you read it <em>at this patchset</em>: a revision that touches it
        brings it back unread.
      </p>
      {!files && <p className="text-sm text-ink-3">Loading…</p>}
      {files && files.length === 0 && (
        <p className="text-sm text-ink-3">
          {props.baseline !== null
            ? `Nothing moved since patchset ${props.baseline}.`
            : "This patchset changes nothing against its parent."}
        </p>
      )}
      {files && files.length > 0 && (
        <div className="flex flex-col gap-3 2xl:flex-row 2xl:items-start">
          {/* The rail. A flat list of forty paths is not a thing anybody
              can navigate, and the filter is the control a reviewer
              reaches for first. Beside the diff only on a wide screen:
              this panel already shares the page with the decision
              column, and a third split left the code a third of the
              panel — a diff at 330px is not a diff anybody reads. Below
              that width the tree sits above, full width, sized to its
              rows. */}
          <nav
            aria-label="Files in this patchset"
            className="shrink-0 2xl:w-64 2xl:max-w-64"
          >
            <input
              className="mb-2 w-full rounded-md border border-borderline bg-surface-0 px-2 py-1 text-xs"
              aria-label="Filter files by path"
              placeholder="filter by path"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
            {railPaths.length > 0 && (
              <CodeBoundary
                fallback={<p className="text-xs text-ink-3">Loading…</p>}
              >
                <LazyCodeTree
                  label="Changed files"
                  paths={railPathsVersioned}
                  gitStatus={railStatus}
                  renderRowDecoration={decorate}
                  selectedPath={open}
                  onSelect={select}
                  height={treeHeight(treeRows(railPaths), 420)}
                />
              </CodeBoundary>
            )}
          </nav>

          <ul className="min-w-0 flex-1 space-y-1">
            {shown.length === 0 && matched.length > 0 && (
              <li className="px-2 py-1.5 text-sm text-ink-3">
                Every file here is viewed. Untick “Hide viewed” to read one
                again.
              </li>
            )}
            {matched.length === 0 && (
              <li className="px-2 py-1.5 text-sm text-ink-3">
                No file in this patchset matches “{filter.trim()}”.
              </li>
            )}
            {shown.map((f) => (
              <li key={f.path}>
                <div
                  className={cn(
                    "flex items-center gap-2 rounded-md pl-2",
                    props.viewed.has(f.path) && "opacity-70",
                  )}
                >
                  <input
                    type="checkbox"
                    className="size-3.5 shrink-0 accent-brand"
                    aria-label={`Viewed ${f.path}`}
                    checked={props.viewed.has(f.path)}
                    onChange={(e) => markViewed(f, e.target.checked)}
                  />
                  <button
                    type="button"
                    className="flex min-w-0 flex-1 items-baseline gap-2 rounded-md px-2 py-1.5 text-left text-sm hover:bg-surface-2"
                    aria-expanded={open === f.path}
                    onClick={() => toggle(f)}
                  >
                    <span
                      aria-hidden
                      className={`font-mono ${glyph[f.status].tone}`}
                    >
                      {glyph[f.status].g}
                    </span>
                    <span className="truncate font-mono text-xs">{f.path}</span>
                    {(() => {
                      const stat = statFor(f.path);
                      return stat ? (
                        <span className="ml-auto">
                          <DiffStat stat={stat} />
                        </span>
                      ) : null;
                    })()}
                    <span
                      className={cn(
                        "shrink-0 text-xs text-ink-3",
                        !statFor(f.path) && "ml-auto",
                      )}
                    >
                      {f.status}
                    </span>
                  </button>
                </div>
                {open === f.path && openEntry && (
                  <div className="mb-2 mt-1 overflow-hidden rounded-md border border-borderline bg-surface-0">
                    <ErrLine message={error} />
                    {/* The controls that decide what the diff below
                        says: how much of the file is on screen, and
                        whether a reindent counts as a change. Both are
                        per file. Nothing here has any meaning over a
                        file that is still loading or has no text. */}
                    {openText && openText !== "binary" && (
                      <div className="flex flex-wrap items-center gap-x-4 gap-y-1 border-b border-borderline px-3 py-1.5 text-xs text-ink-3">
                        <label className="flex items-center gap-1.5">
                          <input
                            type="checkbox"
                            className="size-3.5 accent-brand"
                            checked={ignoreWs.has(f.path)}
                            onChange={(e) =>
                              setIgnoreWhitespace(f.path, e.target.checked)
                            }
                          />
                          Ignore whitespace
                        </label>
                        {openDiff && !expanded.has(f.path) && (
                          <button
                            type="button"
                            className="rounded px-1 hover:text-ink focus:text-ink"
                            onClick={() => setExpandedFor(f.path, true)}
                          >
                            Expand whole file
                          </button>
                        )}
                        {openDiff && expanded.has(f.path) && (
                          <button
                            type="button"
                            className="rounded px-1 hover:text-ink focus:text-ink"
                            onClick={() => setExpandedFor(f.path, false)}
                          >
                            Collapse to changes
                          </button>
                        )}
                        {/* The keyboard's way in: the gutter control
                            appears under a pointer, and a reviewer
                            without one still gets to say which line. */}
                        {openDiff && (
                          <button
                            type="button"
                            className="rounded px-1 hover:text-ink focus:text-ink"
                            onClick={() => {
                              // On the first changed line, which is on
                              // screen: a form hung on line 1 inside
                              // the top fold has nowhere to sit.
                              setDraft({
                                side: "new",
                                line: firstChangedLine(openDiff),
                                end: null,
                              });
                              setDraftBody("");
                            }}
                          >
                            Comment on a line…
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
                        {ignoreWs.has(f.path)
                          ? "No difference once whitespace is ignored."
                          : "No text difference (mode or metadata change)."}
                      </p>
                    )}
                    {openDiff && (
                      <ReviewFileDiff
                        path={f.path}
                        fileDiff={openDiff}
                        threads={props.threads}
                        draft={draft}
                        onDraft={(d) => {
                          setDraft(d);
                          if (!d) setDraftBody("");
                          // A typed line inside a fold: unfold the file
                          // so the form has a line to hang on.
                          if (d && !lineInHunks(openDiff, d.end ?? d.line)) {
                            setExpandedFor(f.path, true);
                          }
                        }}
                        linked={
                          anchor && anchor.path === f.path ? anchor.line : null
                        }
                        diffStyle={diffStyle}
                        expanded={expanded.has(f.path)}
                        renderThread={props.renderThread}
                        composer={
                          draft && (
                            <LineComposer
                              path={f.path}
                              draft={draft}
                              body={draftBody}
                              onBody={setDraftBody}
                              onDraft={setDraft}
                              onSubmit={(pending) =>
                                postLineComment(f.path, draftBody.trim(), pending)
                              }
                              onCancel={() => {
                                setDraft(null);
                                setDraftBody("");
                              }}
                              busy={props.busy}
                              batched={props.batched}
                            />
                          )
                        }
                      />
                    )}
                  </div>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
