/// Reading code in the browser.
///
/// Every read endpoint this uses already existed and none of them was
/// wired to anything — you could clone a repository or drive its API,
/// but you could not look at it. This is a directory listing, a file
/// view, a commit log and a ref switcher, all deep-linkable, because a
/// file you cannot send somebody is a file browser people use once.
///
/// Syntax highlighting arrived in September 2026, after a long refusal on
/// bundle-size grounds — a highlighter is the single largest thing that
/// could be added to this bundle. The shape that made it acceptable:
/// every byte of it lives behind `src/code/`, loaded lazily on the file
/// page only, highlighted in a worker; the listing you land on pays
/// nothing. `src/code/surface.tsx` says how, and
/// `tests/bundle-budget.setup.ts` says how much.
///
/// The file page also carries the whole repository as a tree beside the
/// file (`@pierre/trees`, the same lazy chunk), fed by one recursive
/// listing. The root stays a directory listing on purpose: a tree of ten
/// thousand paths is where you go once you know what you are looking
/// for, and the listing is where you find out.

import { useCallback, useEffect, useRef, useState } from "react";
import {
  api,
  ApiError,
  type FileContent,
  type LogEntry,
  type RefName,
  parseIdent,
  type Session,
  type Tree,
  type TreeEntry,
  type TreePaths,
} from "@/api";
import { CodeBoundary, LazyHighlightedFile, LazyRepoTree } from "@/code/lazy";
import { useIsMobile } from "@/lib/use-mobile";
// Structural links are ink and reveal on hover; only prose links carry
// the brand colour. See `web/FORGE-UX.md` §9.2 — the reason is that this
// browser now renders on a public repository page too, where GitHub
// would paint forty elements blue, and emerald spent that widely stops
// meaning "action" at all.
import { STRUCTURAL_LINK, STRUCTURAL_LINK_2 } from "@/lib/links";
import { formatAgo, formatBytes, formatDay } from "@/format";
import { CommitCheckGlyph } from "@/views/forge/commit-checks";
import { Markdown, type MarkdownBase } from "@/components/markdown";
import { Table, TableBody, TableCell, TableRow } from "@/components/ui/table";
import { Alert } from "@/components/ui/alert";
import { MirrorPushNotice } from "@/components/mirror-push-notice";
import { RepoWalls } from "@/components/repo-walls";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

/// A directory or a file, decided by what the server says the entry is
/// rather than by guessing from the path.
export function Browser(props: {
  session: Session;
  repo: string;
  path: string[];
  at: string | undefined;
  navigate: (to: string) => void;
  /// Whether this browser draws the spend-limit walls itself. The repo
  /// screen draws them over every tab and hands `false`, or the Code
  /// tab would carry the same wall twice; a browser mounted anywhere
  /// with nothing above the tree wants `true`.
  walls: boolean;
  /// How this mount spells a path inside the repository.
  ///
  /// The browser used to build `/repos/{repo}/…` itself, which was true
  /// of the only place it was mounted and false the moment a public
  /// repository at `/{owner}/{repo}/tree/…` reused it. Every URL this
  /// component makes goes through one helper, so making that helper the
  /// caller's business is the whole of the change — and it is better
  /// than an adapter that reverse-engineers the URLs on the way out,
  /// which would have to know this shape anyway and would drift.
  ///
  /// Required, and it was not always: there used to be a default that
  /// built `/repos/{repo}/…` for the dashboard mount. That mount is
  /// gone — a repository has one address now — and a default is exactly
  /// how a second one would grow back without anybody deciding to.
  link: (parts: string[], at: string | undefined) => string;
  /// Where the commit count links.
  commitsHref: string;
  /// Where one commit goes, given its sha — forwarded to the commit bar
  /// and to the listing's per-entry history cells.
  commitHref?: (sha: string) => string;
  /// The repository's stored commit count and the tip it was counted
  /// from. The bar prints it only beside a head that matches, so a
  /// count a push behind — or one for the default branch while another
  /// is being viewed — is never printed as current. Absent on mounts
  /// that have no row to read it from.
  commitCount?: { count: number; tip: string; exact: boolean } | null;
  /// Rendered at the right of the header row, beside the ref switcher —
  /// where GitHub puts its Code button, and where a hand goes looking
  /// for the clone address.
  actions?: React.ReactNode;
  /// What to show in place of the listing when the repository has no
  /// commits yet. The forge puts the push instructions here, with the
  /// repository's own clone address; the default is one plain sentence.
  empty?: React.ReactNode;
}) {
  const { session, repo, path, at } = props;
  // `null` until the ref list has answered, because "no refs" is a
  // fact this page acts on and "not heard yet" must not read as it.
  const [refs, setRefs] = useState<RefName[] | null>(null);
  const [refsFailed, setRefsFailed] = useState(false);
  const [tree, setTree] = useState<Tree | null>(null);
  const [file, setFile] = useState<FileContent | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  // What the listing said each path was. A ref, not state: it is a hint
  // for the next fetch, and re-rendering on it would cost the fetch it
  // exists to save.
  const kinds = useRef<Record<string, "tree" | "blob">>({});
  // The whole repository, for the rail beside a file. Keyed by the commit
  // the file resolved to: the same commit is the same tree, whatever
  // `at` was spelled as, and switching versions of a file walks back to
  // a tree already held.
  const [paths, setPaths] = useState<TreePaths | null>(null);
  const [treeError, setTreeError] = useState<string | null>(null);
  const pathsFor = useRef<Record<string, TreePaths>>({});

  const joined = path.join("/");

  useEffect(() => {
    let alive = true;
    api
      .branches(session, repo)
      .then((b) => alive && setRefs(b))
      // One somebody may read but whose ref list failed: the switcher is
      // a convenience, not the page. Remembered, so a root listing that
      // also failed is shown as the error it is and not held on
      // "Loading…" waiting for an answer that is not coming.
      .catch(() => alive && setRefsFailed(true));
    return () => {
      alive = false;
    };
  }, [session, repo]);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    setError(null);
    // Remember what the path turned out to be, not just what a listing
    // said it was. A deep link has no listing behind it, so the first
    // visit has to guess — but every visit *after* that one knows, and
    // the ones after are not rare: switching between versions of a file
    // re-navigates to the same path, and without this each switch pays
    // for the same wrong guess and the same 404 again.
    // The listing first, then its history. The two used to be one
    // request, and the history is a walk the server may spend seconds
    // on — on a real project's mirror it ran past the edge's timeout, so
    // the page sat on "Loading…" and nobody ever saw the listing it had
    // been holding. Now the listing lands as soon as it is read and the
    // last-commit column fills in when the walk answers; if that answer
    // never comes, the listing is still a listing.
    const asTree = () =>
      api.tree(session, repo, joined, at).then((t) => {
        if (!alive) return;
        kinds.current[joined] = "tree";
        setTree(t);
        setFile(null);
        api
          .tree(session, repo, joined, at, true)
          .then((h) => {
            if (!alive) return;
            setTree((cur) =>
              cur && cur.commit === h.commit
                ? {
                    ...cur,
                    history_truncated: h.history_truncated,
                    entries: cur.entries.map((e) => ({
                      ...e,
                      last_commit:
                        h.entries.find((x) => x.name === e.name)?.last_commit ??
                        null,
                    })),
                  }
                : cur,
            );
          })
          // History is a column, not the page: a walk that failed or
          // timed out leaves the cells empty and the listing standing.
          .catch(() => undefined);
      });
    const asFile = () =>
      api.file(session, repo, joined, at).then((f) => {
        if (!alive) return;
        kinds.current[joined] = "blob";
        setFile(f);
        setTree(null);
      });
    // A path is a directory or a file and the URL does not say which.
    // The listing you clicked did say, so ask for that one directly —
    // guessing wrong costs a wasted round trip and a 404 in everybody's
    // logs on every file anyone opens. Only a link somebody was sent,
    // with no listing behind it, has to guess, and it guesses the
    // listing first because that is what most paths are.
    // ...and only a *404* is a wrong guess. The fallback exists because
    // a path is a directory or a file and the URL does not say which, so
    // "no such tree" earns a second try as a file. "You may not read
    // this repository" earns nothing: retrying spends a second request
    // to be refused again, and it turns the honest 401 the server gave
    // into a 404 further down the log, which reads as a missing file
    // rather than a private repo. The manual browser pass caught this on
    // a stranger opening a private repository's address.
    const wrongGuess = (e: unknown) =>
      e instanceof ApiError && e.status === 404;
    const orElse = (other: () => Promise<void>) => (e: unknown) =>
      wrongGuess(e) ? other() : Promise.reject(e);
    const known = kinds.current[joined];
    // The root is a directory or nothing; it is never a file, so a 404
    // there earns no second try. (It is also the one 404 an empty
    // repository answers on every visit — see `status` below.)
    (known === "blob"
      ? asFile().catch(orElse(asTree))
      : joined === ""
        ? asTree()
        : asTree().catch(orElse(asFile))
    )
      .catch((e) => alive && setError(String(e?.message ?? e)))
      .finally(() => alive && setLoading(false));
    return () => {
      alive = false;
    };
  }, [session, repo, joined, at]);

  // The rail is context, not the page: a file shows whether or not the
  // tree could be read, and a directory listing never asks for it at all.
  const fileCommit = file?.commit;
  useEffect(() => {
    if (fileCommit === undefined) return;
    const cached = pathsFor.current[fileCommit];
    if (cached) {
      setPaths(cached);
      setTreeError(null);
      return;
    }
    let alive = true;
    setPaths(null);
    setTreeError(null);
    api
      .treePaths(session, repo, at)
      .then((p) => {
        if (!alive) return;
        pathsFor.current[fileCommit] = p;
        setPaths(p);
      })
      .catch((e) => alive && setTreeError(String(e?.message ?? e)));
    return () => {
      alive = false;
    };
  }, [session, repo, at, fileCommit]);

  // A repository with no commits has no tree to list, and asking for
  // one 404s — twice, once as a directory and once as a file. That is
  // not an error about the repository, it is the shape of an empty one,
  // and the ref list is what says which: no refs at all means nothing
  // has been pushed. Found by walking the plan boundaries by hand: a
  // fork of an empty repository, and a repository somebody had just
  // created, both opened on a red "404" under their own name. Only the
  // root of the default branch is read this way — a path that 404s
  // inside a repository with commits is a path that is not there.
  const atRoot = path.length === 0 && at === undefined;
  const status: "loading" | "ok" | "empty" | "error" = loading
    ? "loading"
    : error === null
      ? "ok"
      : !atRoot
        ? "error"
        : refs === null
          ? refsFailed
            ? "error"
            : "loading"
          : refs.length === 0
            ? "empty"
            : "error";

  const at_ = at;
  // `mount` spells a path at whichever revision is asked for; `link`
  // is the same thing pinned to the one being viewed, which is what
  // almost every caller below wants.
  const mount = props.link;
  const link = (parts: string[]) => mount(parts, at_);

  return (
    <div className="space-y-4">
      {/* The spend-limit walls, over the tree: this is where a link to
          a private repository lands, and the browser has no row of its
          own, so the component reads visibility itself. */}
      {props.walls && (
        <RepoWalls session={props.session} repo={props.repo} />
      )}
      {/* Where a push to a mirror goes, over the tree: this is where a
          link to the repository lands, and the person about to add a
          remote is reading it.

          Not gated on `walls`. It was, and `walls={false}` is exactly
          what the forge's `RepoScreen` passes — it draws the walls over
          every tab itself — so on a repository's *only* real address
          the notice rendered nowhere at all. Whether somebody else
          already drew the spend-limit walls says nothing about whether
          this mirror's pushes are forwarded. */}
      <MirrorPushNotice session={props.session} repo={props.repo} />
      <div className="flex flex-wrap items-center gap-2">
        <Breadcrumbs
          repo={repo}
          path={path}
          onGo={(i) => props.navigate(link(path.slice(0, i)))}
        />
        {/* The ref switcher leads the row, and the actions close it.
            GitHub's arrangement, and it is the right one for a reason
            that only showed up once the breadcrumb stopped rendering at
            the repository root: with the switcher grouped on the right,
            the root listing opened with a third of the row empty and
            every control huddled in the corner. "Which branch am I
            looking at" is also the first question somebody arriving at
            a repository asks, and the first thing on a line is where
            they look for it. */}
        <RefSwitcher
          refs={refs ?? []}
          at={at}
          onPick={(name) =>
            props.navigate(
              mount(
                path,
                // The default branch is the absence of a revision, not
                // a revision that happens to match: a permalink to
                // "whatever is current" should stay current.
                refs?.find((r) => r.name === name)?.default ? undefined : name,
              ),
            )
          }
        />
        <div className="ml-auto flex items-center gap-2">{props.actions}</div>
      </div>

      {status === "error" && <Alert variant="destructive">{error}</Alert>}
      {status === "loading" && <p className="text-sm text-ink-3">Loading…</p>}
      {status === "empty" &&
        (props.empty ?? (
          <p className="text-sm text-ink-3">
            This repository has no commits yet.
          </p>
        ))}
      {tree && (
        <div>
          <LatestCommit
            session={session}
            repo={repo}
            at={at}
            commitsHref={props.commitsHref}
            commitHref={props.commitHref}
            commitCount={props.commitCount ?? null}
            onNavigate={props.navigate}
          />
          <Listing
            commitHref={props.commitHref}
            onNavigate={props.navigate}
            entries={tree.entries}
            onOpen={(entry) => {
              kinds.current[[...path, entry.name].join("/")] = entry.kind;
              props.navigate(link([...path, entry.name]));
            }}
          />
        </div>
      )}
      {file && (
        <div className="flex flex-col gap-4 md:flex-row">
          <TreeRail
            paths={paths}
            error={treeError}
            current={joined}
            onOpenFile={(p) => {
              kinds.current[p] = "blob";
              props.navigate(link(p.split("/")));
            }}
          />
          {/* `min-w-0` is what lets a long line scroll inside the code
              surface instead of widening the page — the same rule
              `FORGE_CONTAINER` documents in `src/lib/links.ts`. */}
          <div className="min-w-0 flex-1">
            <FileView
              session={session}
              repo={repo}
              path={joined}
              at={at}
              file={file}
              name={path[path.length - 1] ?? ""}
              // Where a markdown file's own relative links and images
              // resolve: its directory, in this mount's spelling for
              // links and the raw-bytes route for images — the same two
              // prefixes the README on the front page uses.
              markdownBase={{
                links: `${mount(path.slice(0, -1), at_)}/`,
                images: `/v1/orgs/${encodeURIComponent(session.org)}/repos/${encodeURIComponent(repo)}/files/${path
                  .slice(0, -1)
                  .map(encodeURIComponent)
                  .join("/")}${path.length > 1 ? "/" : ""}`,
              }}
              onPick={(rev) => props.navigate(mount(path, rev))}
              onLatest={() => props.navigate(mount(path, undefined))}
            />
          </div>
        </div>
      )}
    </div>
  );
}

/// The repository's tree, beside the file you are reading.
///
/// On a desktop it is a rail that stays put while the file scrolls. On a
/// phone there is no room for a rail beside a file, so it sits behind a
/// "Files" button and renders only while open — not hidden offscreen,
/// which the walkthrough's audit rightly counts as a control nobody can
/// reach.
function TreeRail(props: {
  paths: TreePaths | null;
  error: string | null;
  current: string;
  onOpenFile: (path: string) => void;
}) {
  const mobile = useIsMobile();
  const [open, setOpen] = useState(false);
  const body = props.error ? (
    <p className="text-xs text-ink-3">Could not load the file tree.</p>
  ) : props.paths === null ? (
    <p className="text-xs text-ink-3">Loading…</p>
  ) : (
    <>
      <CodeBoundary fallback={<p className="text-xs text-ink-3">Loading…</p>}>
        <LazyRepoTree
          paths={props.paths.paths}
          current={props.current}
          onOpenFile={props.onOpenFile}
          height={mobile ? 320 : "calc(100vh - 9rem)"}
        />
      </CodeBoundary>
      {props.paths.truncated && (
        <p className="mt-1 text-xs text-ink-3">
          Showing the first {props.paths.paths.length} paths.
        </p>
      )}
    </>
  );
  if (mobile) {
    return (
      <div>
        <Button
          variant="outline"
          size="xs"
          aria-expanded={open}
          onClick={() => setOpen((o) => !o)}
        >
          Files
        </Button>
        {open && <div className="mt-2">{body}</div>}
      </div>
    );
  }
  return (
    <aside
      aria-label="Repository files"
      className="md:sticky md:top-4 md:w-60 md:shrink-0 md:self-start"
    >
      {body}
    </aside>
  );
}

/// Where in the repository the listing is, as a path you can click back
/// along.
///
/// **Nothing at all at the repository root.** The crumb's root segment
/// is the repository's name, and at the root that is the only thing it
/// would render — directly under a masthead that has just said the same
/// name, in the same weight, twelve pixels higher. Two identical names
/// stacked reads as a rendering fault, and the lower one is a button
/// that navigates to the page it is already on.
///
/// GitHub does exactly this: the file table carries no breadcrumb until
/// you are inside a directory, at which point the root becomes a real
/// destination and earns its place. Both of our mounts want the same
/// thing — the dashboard's shell prints `acme / atlas-upstream` above
/// this too — so it is decided here rather than passed in.
function Breadcrumbs(props: {
  repo: string;
  path: string[];
  onGo: (index: number) => void;
}) {
  if (props.path.length === 0) return null;
  return (
    <nav
      className="flex flex-wrap items-center gap-1 text-sm"
      aria-label="Path"
    >
      <button
        className={`font-medium ${STRUCTURAL_LINK}`}
        onClick={() => props.onGo(0)}
      >
        {props.repo}
      </button>
      {props.path.map((seg, i) => (
        <span key={`${seg}-${i}`} className="flex items-center gap-1">
          <span className="text-ink-3">/</span>
          {i === props.path.length - 1 ? (
            <span className="font-medium text-ink">{seg}</span>
          ) : (
            <button
              className={STRUCTURAL_LINK_2}
              onClick={() => props.onGo(i + 1)}
            >
              {seg}
            </button>
          )}
        </span>
      ))}
    </nav>
  );
}

function RefSwitcher(props: {
  refs: RefName[];
  at: string | undefined;
  onPick: (name: string) => void;
}) {
  if (props.refs.length === 0) return null;
  const current =
    props.at ?? props.refs.find((r) => r.default)?.name ?? props.refs[0].name;
  return (
    <Select value={current} onValueChange={props.onPick}>
      <SelectTrigger aria-label="Branch" className="px-2 py-1">
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {props.refs.map((r) => (
          <SelectItem key={r.full} value={r.name}>
            {r.name}
            {r.default ? " (default)" : ""}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

function Listing(props: {
  entries: TreeEntry[];
  onOpen: (entry: TreeEntry) => void;
  /// Where the commit that last touched a row goes. GitHub links the
  /// message itself, which is the thing you were reading when you
  /// decided you wanted to know more about it.
  commitHref?: (sha: string) => string;
  onNavigate?: (to: string) => void;
}) {
  // Directories first, then names — the order every file browser uses,
  // and the one that makes a long listing scannable.
  const sorted = [...props.entries].sort((a, b) =>
    a.kind === b.kind
      ? a.name.localeCompare(b.name)
      : a.kind === "tree"
        ? -1
        : 1,
  );
  if (sorted.length === 0)
    return <p className="text-sm text-ink-3">This directory is empty.</p>;
  return (
    <Table className="table-fixed text-sm">
      <TableBody>
        {sorted.map((e) => (
          <TableRow key={e.name}>
            <TableCell className="max-w-0 truncate py-1.5">
              <button
                className={STRUCTURAL_LINK}
                onClick={() => props.onOpen(e)}
              >
                {e.kind === "tree" ? `${e.name}/` : e.name}
              </button>
            </TableCell>
            {/* What last touched this entry, and when. The column that
                makes a file list worth reading: a name and a size say a
                file exists, this says where the project is moving.
                Absent when nothing in the walked window touched it —
                which is a real answer and better than a wrong one. */}
            <TableCell className="max-w-0 truncate py-1.5 text-ink-3">
              {e.last_commit && props.commitHref && props.onNavigate ? (
                <a
                  className={STRUCTURAL_LINK_2}
                  href={props.commitHref(e.last_commit.commit)}
                  onClick={(ev) => {
                    ev.preventDefault();
                    props.onNavigate?.(
                      props.commitHref!(e.last_commit!.commit),
                    );
                  }}
                >
                  {e.last_commit.message}
                </a>
              ) : (
                (e.last_commit?.message ?? "")
              )}
            </TableCell>
            {/* When, and nothing else. This cell used to fall back to
                the file's size while the history column was empty, and
                the listing paid one blob read per file for it — on the
                production mirror, most of a twenty-second wait. GitHub's
                table has no size column; the file page says how big a
                file is, from the one blob it reads anyway. */}
            <TableCell className="w-28 py-1.5 text-right text-xs text-ink-3">
              {e.last_commit
                ? (() => {
                    const who = parseIdent(e.last_commit.author);
                    return who.time > 0 ? formatAgo(who.time) : "";
                  })()
                : ""}
            </TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}

/// One file, the way a file page reads: who last touched it, what they
/// did, and every version you can switch to.
///
/// The history is a *path-filtered* log — `GET /log?path=…` — not the
/// repository's log with the browser throwing rows away. That
/// distinction is the whole feature: a file changed once at the start of
/// a long history is one request here, and would be the entire history
/// downloaded and discarded the other way.
function FileView(props: {
  session: Session;
  repo: string;
  path: string;
  at: string | undefined;
  file: FileContent;
  name: string;
  /// Where a markdown file's relative links and images resolve.
  markdownBase: MarkdownBase;
  onPick: (rev: string) => void;
  onLatest: () => void;
}) {
  const { session, repo, path, file } = props;
  const [history, setHistory] = useState<LogEntry[] | null>(null);
  const [histError, setHistError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    setHistory(null);
    setHistError(null);
    api
      .log(session, repo, { path, limit: 20 })
      .then((out) => alive && setHistory(out.entries ?? []))
      // History is context, not the page. A repo whose log cannot be
      // read still shows its file.
      .catch((e: Error) => alive && setHistError(e.message));
    return () => {
      alive = false;
    };
  }, [session, repo, path]);

  const last = history?.[0];
  // `at` can be a branch name; the file's own commit is the resolved
  // one, so compare against that rather than against the label.
  const pinned = props.at != null && props.at === file.commit;

  return (
    <div className="space-y-3">
      {pinned && (
        <div className="flex flex-wrap items-center gap-2 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-sm">
          <span>
            Viewing this file at{" "}
            <span className="font-mono">{file.commit.slice(0, 7)}</span>, not
            the latest version.
          </span>
          <Button
            variant="outline"
            size="xs"
            className="ml-auto px-2 py-1"
            onClick={props.onLatest}
          >
            Back to latest
          </Button>
        </div>
      )}

      {last && (
        <div className="flex flex-wrap items-baseline gap-x-2 gap-y-1 rounded-md border border-borderline bg-surface-1 px-3 py-2 text-sm">
          <span className="font-medium">{parseIdent(last.author).name}</span>
          {/* Subject only: a body wrapped into this row makes it a
              paragraph, and the row is meant to be scannable. */}
          <span className="min-w-0 truncate text-ink-2">
            {last.message.split("\n")[0]}
          </span>
          <span className="ml-auto flex items-center gap-2 text-xs text-ink-3">
            <span className="font-mono">{last.commit.slice(0, 7)}</span>
            <span>{formatAgo(parseIdent(last.author).time)}</span>
          </span>
        </div>
      )}

      <FileBody file={file} name={props.name} markdownBase={props.markdownBase} />

      <details className="rounded-lg border border-borderline bg-surface-1">
        <summary className="cursor-pointer px-3 py-2 text-sm font-medium">
          History for this file
          {history
            ? ` (${history.length}${history.length === 20 ? "+" : ""})`
            : ""}
        </summary>
        <div className="border-t border-borderline px-3 py-2">
          {histError ? (
            <p className="text-sm text-ink-3">
              Could not read history: {histError}
            </p>
          ) : history === null ? (
            <p className="text-sm text-ink-3">Loading…</p>
          ) : history.length === 0 ? (
            <p className="text-sm text-ink-3">No commits touched this path.</p>
          ) : (
            <ul className="space-y-1">
              {history.map((c) => {
                const who = parseIdent(c.author);
                const here = c.commit === file.commit;
                return (
                  <li
                    key={c.commit}
                    className="flex flex-wrap items-baseline gap-x-2 border-b border-borderline/50 py-1.5 text-sm last:border-0"
                  >
                    <button
                      className={`font-mono text-xs ${STRUCTURAL_LINK_2}`}
                      onClick={() => props.onPick(c.commit)}
                      aria-label={`View this file at ${c.commit.slice(0, 7)}`}
                    >
                      {c.commit.slice(0, 7)}
                    </button>
                    {c.change && (
                      <span className="rounded border border-borderline px-1 py-0.5 font-mono text-[10px] uppercase tracking-wide text-ink-3">
                        {c.change}
                      </span>
                    )}
                    <span className="min-w-0 flex-1 truncate text-ink-2">
                      {c.message.split("\n")[0]}
                    </span>
                    <span className="text-xs text-ink-3" title={who.name}>
                      {authorLabel(who.name).label}
                    </span>
                    <span className="text-xs text-ink-3">
                      {formatAgo(who.time)}
                    </span>
                    {here && (
                      <span className="text-xs font-medium text-brand">
                        showing
                      </span>
                    )}
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      </details>
    </div>
  );
}

/// Whether a file is prose the page should render before it shows the
/// source — GitHub opens a `.md` on its preview and offers the code
/// behind a tab, because somebody who clicked `CONTRIBUTING.md` wanted
/// to read it, not to read its markup.
export function isMarkdown(name: string): boolean {
  const lower = name.toLowerCase();
  return lower.endsWith(".md") || lower.endsWith(".markdown");
}

function FileBody(props: {
  file: FileContent;
  name: string;
  markdownBase: MarkdownBase;
}) {
  const { file } = props;
  const prose = !file.binary && isMarkdown(props.name);
  // Preview first for prose, and only for prose: a source file has no
  // preview, and a control that toggles nothing is furniture.
  const [view, setView] = useState<"preview" | "code">("preview");
  if (file.binary) {
    return (
      <div className="rounded-lg border border-borderline bg-surface-1 p-4 text-sm">
        <p className="mb-1 font-medium">{props.name}</p>
        <p className="text-ink-3">
          {formatBytes(file.size)} of {file.contentType} — binary, not shown.
        </p>
      </div>
    );
  }
  const lines = file.text.split("\n");
  // A trailing newline is a line ending, not an empty last line.
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  return (
    <div className="overflow-hidden rounded-lg border border-borderline bg-surface-1">
      <div className="flex items-center gap-2 border-b border-borderline px-3 py-2 text-xs text-ink-3">
        {prose && (
          <div
            role="tablist"
            aria-label="File view"
            className="mr-1 flex rounded-md border border-borderline text-xs"
          >
            {(["preview", "code"] as const).map((v) => (
              <button
                key={v}
                role="tab"
                aria-selected={view === v}
                className={
                  view === v
                    ? "rounded-[5px] bg-surface-2 px-2 py-0.5 font-medium text-ink"
                    : "px-2 py-0.5 text-ink-3 hover:text-ink"
                }
                onClick={() => setView(v)}
              >
                {v === "preview" ? "Preview" : "Code"}
              </button>
            ))}
          </div>
        )}
        <span className="font-medium text-ink-2">{props.name}</span>
        <span>
          {lines.length} line{lines.length === 1 ? "" : "s"}
        </span>
        <span>{formatBytes(file.size)}</span>
        <span className="ml-auto font-mono">{file.commit.slice(0, 7)}</span>
      </div>
      {prose && view === "preview" ? (
        <div className="px-6 py-4">
          <Markdown
            source={file.text}
            base={props.markdownBase}
            headingLevel={2}
          />
        </div>
      ) : (
        /* The code itself is the library's: line numbers, a grammar for
           the file type, scrolling inside its own box. Loaded on demand,
           so the first file of a session shows "Loading…" for the moment
           the chunk takes. */
        <CodeBoundary
          fallback={<p className="px-3 py-2 text-xs text-ink-3">Loading…</p>}
        >
          <LazyHighlightedFile
            name={props.name}
            text={file.text}
            commit={file.commit}
          />
        </CodeBoundary>
      )}
    </div>
  );
}

/// How to render whoever authored a commit.
///
/// A commit made over REST carries the acting principal in its identity
/// line — `token:01m1055ewr1t0myw7kcccrpxfd` — and putting that on the
/// page verbatim is how the repository front page came to say a
/// twenty-six character ULID had last touched the code. It is unreadable
/// *and* it is the wrong shape of answer: nobody wants the id, they want
/// to know a machine did it.
///
/// So a service principal renders as what it is, and deliberately does
/// not borrow a human name — `docs/code-review.md` already fixes that
/// rule for review comments, and a commit author is the same question.
/// The id stays in `title` for whoever is actually debugging.
///
/// A service token that has a label signs as `token:<label>`
/// (`commits::acting_author`), and the label is the answer a reader
/// wants: "release-bot (token)" says which machine, where the bare word
/// "token" said only that it was one — a seeded repository read as a
/// history nobody had written. An id-shaped remainder (a ULID: 26
/// characters of Crockford base32) is not a name, so it still collapses
/// to the kind.
export function authorLabel(name: string): { label: string; machine: boolean } {
  for (const prefix of ["token:", "system:", "agent:"]) {
    if (name.startsWith(prefix)) {
      const kind = prefix.slice(0, -1);
      const rest = name.slice(prefix.length);
      const isId = /^[0-9a-hjkmnp-tv-z]{26}$/i.test(rest);
      return {
        label: rest && !isId ? `${rest} (${kind})` : kind,
        machine: true,
      };
    }
  }
  return { label: name, machine: false };
}

/// The latest commit, fused to the top of the file table.
///
/// This replaced a floating "History" box that sat between the file list
/// and the README — a panel of three commits, bordered like a card,
/// belonging to nothing on either side of it. GitHub puts one row above
/// the files and shares the table's border with it, and the reason that
/// reads better is structural rather than decorative: the bar is *about*
/// the tree beneath it. "This is the state you are looking at, and here
/// is who last touched it." A separate card says "here is some history,
/// somewhere near some files".
///
/// Full history lives behind the count on the right, not in a box on
/// this page. A repository has thousands of commits and the front page
/// has room for one.
function LatestCommit(props: {
  session: Session;
  repo: string;
  at: string | undefined;
  /// Where "N commits" goes. Absent on mounts that have no commits page,
  /// in which case the count renders as plain text rather than a link
  /// that leads nowhere.
  commitsHref?: string;
  /// Where one commit goes, given its sha. Same rule: absent means the
  /// sha stays text.
  commitHref?: (sha: string) => string;
  /// The stored count and the tip it was taken from; see `Browser`.
  commitCount: { count: number; tip: string; exact: boolean } | null;
  onNavigate?: (to: string) => void;
}) {
  const [head, setHead] = useState<LogEntry | null>(null);

  useEffect(() => {
    let alive = true;
    setHead(null);
    api
      .log(props.session, props.repo, { rev: props.at, limit: 1 })
      .then((out) => {
        if (!alive) return;
        setHead(out.entries?.[0] ?? null);
      })
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [props.session, props.repo, props.at]);

  if (!head) return null;
  const who = parseIdent(head.author);
  // The count is the row's, taken by the job that follows a write, and
  // it names the commit it was true for. It is printed only when that is
  // the commit this bar is showing: a count a push behind, or the
  // default branch's count beside another branch's head, would be a
  // number that is simply wrong. The bar used to fetch a thousand log
  // entries to count them itself — clamped to five hundred by the
  // server, first-parent only, nine seconds on a real mirror, and still
  // a different number from GitHub's.
  const count =
    props.commitCount && props.commitCount.tip === head.commit
      ? props.commitCount
      : null;
  const countLabel = (c: { count: number; exact: boolean }) =>
    `${c.count.toLocaleString()}${c.exact ? "" : "+"}`;
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-t-lg border border-borderline bg-surface-2 px-4 py-2.5 text-sm">
      <span
        className={
          authorLabel(who.name).machine
            ? "font-medium text-ink-3"
            : "font-medium text-ink"
        }
        title={who.name}
      >
        {authorLabel(who.name).label}
      </span>{" "}
      <span className="min-w-0 flex-1 truncate text-ink-2">
        {head.message.split("\n")[0]}
      </span>{" "}
      {props.commitHref && props.onNavigate ? (
        <a
          className={`font-mono text-xs ${STRUCTURAL_LINK_2}`}
          href={props.commitHref(head.commit)}
          onClick={(e) => {
            e.preventDefault();
            props.onNavigate?.(props.commitHref!(head.commit));
          }}
        >
          {head.commit.slice(0, 7)}
        </a>
      ) : (
        <code className="font-mono text-xs text-ink-3">
          {head.commit.slice(0, 7)}
        </code>
      )}
      {who.time > 0 && (
        <span className="text-xs text-ink-3">{formatAgo(who.time)}</span>
      )}
      {/* The history link is always there when there is a history page
          — GitHub's "Commits" with a clock — and carries the number when
          the number is known to be this head's. */}
      {props.commitsHref && props.onNavigate ? (
        <a
          className={`text-xs ${STRUCTURAL_LINK}`}
          href={props.commitsHref}
          onClick={(e) => {
            e.preventDefault();
            props.onNavigate?.(props.commitsHref!);
          }}
        >
          {count && (
            <>
              <span className="font-mono">{countLabel(count)}</span>{" "}
            </>
          )}
          {count && count.count === 1 && count.exact ? "commit" : "commits"}
        </a>
      ) : (
        count && (
          <span className="text-xs text-ink-3">
            <span className="font-mono">{countLabel(count)}</span>{" "}
            {count.count === 1 && count.exact ? "commit" : "commits"}
          </span>
        )
      )}
    </div>
  );
}

/// The full history, for the page that is about history.
///
/// Exported because the repository's front page no longer carries a
/// history panel — it carries one row and a link to here. Same walk,
/// given the room to be read.
export function CommitLog(props: {
  session: Session;
  repo: string;
  at: string | undefined;
  /// Where one commit goes. Absent means the sha stays text — the
  /// dashboard mount has no commit page.
  commitHref?: (sha: string) => string;
  onNavigate?: (to: string) => void;
  /// Rendered without the card and the "History" label — for the page
  /// that is *about* history, where both are furniture repeating what
  /// the tab already said.
  bare?: boolean;
}) {
  const [commits, setCommits] = useState<LogEntry[]>([]);
  const [next, setNext] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(
    async (after?: string) => {
      setBusy(true);
      try {
        const out = await api.log(props.session, props.repo, {
          rev: props.at,
          limit: 20,
          after,
        });
        const batch = out.entries ?? [];
        setCommits((prev) => (after ? [...prev, ...batch] : batch));
        setNext(out.next_after);
      } catch {
        // A repo with no commits yet answers this way; an empty log is
        // the honest rendering of one.
        if (!after) setCommits([]);
      } finally {
        setBusy(false);
      }
    },
    [props.session, props.repo, props.at],
  );

  useEffect(() => {
    void load();
  }, [load]);

  if (commits.length === 0) return null;
  return (
    <div
      className={
        props.bare ? "" : "rounded-lg border border-borderline bg-surface-1 p-4"
      }
    >
      {!props.bare && <div className="mb-2 text-sm font-medium">History</div>}
      <ul className={props.bare ? "text-sm" : "space-y-1 text-sm"}>
        {commits.map((c, i) => {
          const who = parseIdent(c.author);
          // A day heading, once, when the day changes. Only on the page
          // that is about history: in the compact panel it would be more
          // furniture than list.
          const day = who.time > 0 ? formatDay(who.time) : "";
          const prev =
            i > 0 ? parseIdent(commits[i - 1].author) : { name: "", time: 0 };
          const newDay =
            props.bare && day && (i === 0 || formatDay(prev.time) !== day);
          return (
            <li key={c.commit} className="flex flex-wrap items-baseline gap-2">
              {newDay && (
                <div className="mt-4 mb-1 w-full text-xs font-medium text-ink-3 first:mt-0">
                  Commits on {day}
                </div>
              )}
              {props.commitHref && props.onNavigate ? (
                <a
                  className={`font-mono text-xs ${STRUCTURAL_LINK_2}`}
                  href={props.commitHref(c.commit)}
                  onClick={(e) => {
                    e.preventDefault();
                    props.onNavigate?.(props.commitHref!(c.commit));
                  }}
                >
                  {c.commit.slice(0, 7)}
                </a>
              ) : (
                <code className="font-mono text-xs text-ink-3">
                  {c.commit.slice(0, 7)}
                </code>
              )}
              {/* Whether CI passed, beside the sha, as GitHub does it.
                  One request per row: the route is per-sha and there is
                  no batch form, which is worth a batch route if history
                  pages ever get long. Renders nothing when nothing
                  reported, so a repository with no CI is unmarked rather
                  than marked "unknown". */}
              <CommitCheckGlyph
                session={props.session}
                repo={props.repo}
                sha={c.commit}
              />
              <span className="min-w-0 flex-1 truncate">
                {/* A commit message's first line is its subject; the
                    rest belongs on a page this one links to. */}
                {c.message.split("\n")[0]}
              </span>
              <span className="text-xs text-ink-3" title={who.name}>
                {authorLabel(who.name).label}
              </span>
              {who.time > 0 && (
                <span className="text-xs text-ink-3">
                  {formatAgo(who.time)}
                </span>
              )}
            </li>
          );
        })}
      </ul>
      {next && (
        <Button
          variant="outline"
          size="xs"
          className="mt-3"
          disabled={busy}
          onClick={() => load(next)}
        >
          {busy ? "Loading…" : "Load more"}
        </Button>
      )}
    </div>
  );
}
