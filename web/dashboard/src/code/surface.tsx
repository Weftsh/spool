/// The one module that imports `@pierre/diffs` and `@pierre/trees`.
///
/// A syntax highlighter was refused on this dashboard for a long time on
/// bundle-size grounds — larger than the rest of the bundle put together.
/// The decision was reversed in September 2026 with a shape that keeps
/// the reason honest: everything from both libraries is imported *here*
/// and only here, `lazy.tsx` loads this file on demand, so the entry
/// chunk never carries a byte of it and a directory listing or an org
/// page pays nothing. Highlighting runs in a worker pool; grammars are
/// fetched one at a time as file types turn up. `fence.test.ts` fails
/// the moment any other file imports `@pierre/*`, and
/// `tests/bundle-budget.setup.ts` fails the moment the entry chunk grows
/// past its budget or the worker stops being a separate asset.
///
/// Colours: none here. `pierre.css` aliases the libraries' variables onto
/// `web/shared/tokens.css`, and the "weft" Shiki theme resolves to those
/// same variables (`SYNTAX_DEFAULTS`), so both libraries follow
/// `color-scheme` like everything else on the page.

import "./pierre.css";
import {
  parseDiffFromFile,
  registerCustomCSSVariableTheme,
  type FileContents,
} from "@pierre/diffs";
import {
  File,
  FileDiff,
  WorkerPoolContextProvider,
  type DiffLineAnnotation,
  type FileDiffMetadata,
  type FileDiffOptions,
  type FileOptions,
  type SelectedLineRange,
} from "@pierre/diffs/react";
import { FileTree, useFileTree } from "@pierre/trees/react";
import type {
  FileTreeRowDecorationRenderer,
  GitStatusEntry,
} from "@pierre/trees";
import {
  useEffect,
  useMemo,
  useRef,
  type CSSProperties,
  type ReactNode,
} from "react";
import { ancestorsOf, pageScheme, SYNTAX_DEFAULTS, WEFT_THEME } from "./paths";
import { isUnchanged } from "@/views/change/diffstat";

// Registered once, at module load, before anything renders. The main
// thread resolves themes by name and ships the resolved registration to
// each worker, so a custom theme needs registering on this side only.
registerCustomCSSVariableTheme(WEFT_THEME, SYNTAX_DEFAULTS);

const THEMES = { light: WEFT_THEME, dark: WEFT_THEME } as const;
const SCHEME = pageScheme();

/// Everything a file or diff surface shares. Module constants because the
/// library re-renders when an options object changes identity.
const BASE = {
  theme: THEMES,
  themeType: SCHEME,
  preferredHighlighter: "shiki-js",
  disableFileHeader: true,
} as const;

const FILE_OPTIONS: FileOptions<undefined, undefined> = {
  ...BASE,
  overflow: "scroll",
};

// ---------------------------------------------------------------------
// Worker pool

const POOL = {
  poolSize: 2,
  workerFactory: () =>
    new Worker(new URL("@pierre/diffs/worker/worker.js", import.meta.url), {
      type: "module",
    }),
};

const HIGHLIGHTER = {
  theme: THEMES,
  preferredHighlighter: "shiki-js",
} as const;

/// Wraps a code surface in the shared worker pool. One per page is fine:
/// a route change tears the pool down and the next page builds its own.
export function CodePool(props: { children: ReactNode }) {
  return (
    <WorkerPoolContextProvider
      poolOptions={POOL}
      highlighterOptions={HIGHLIGHTER}
    >
      {props.children}
    </WorkerPoolContextProvider>
  );
}

// ---------------------------------------------------------------------
// One file, highlighted

/// A single file with line numbers, the language inferred from its name.
/// The header strip (name, line count, size, sha) is the caller's; this
/// is only the code.
export function HighlightedFile(props: {
  name: string;
  text: string;
  commit: string;
}) {
  const key = `${props.commit}:${props.name}`;
  // No `cacheKey`. The library's render cache is keyed on it, and with a
  // key of commit and name the second file opened in a session came back
  // as the first — a fresh `File` under a new key, showing the README it
  // had replaced. The exact mechanism inside the library was not run to
  // ground; a page that renders each file once has nothing to gain from
  // that cache, so it is simply not used.
  const file = useMemo<FileContents>(
    () => ({
      name: props.name,
      // A trailing newline is a line ending, not an empty last line —
      // the same rule the caller's line count follows.
      contents: props.text.replace(/\n$/, ""),
    }),
    [props.name, props.text],
  );
  // Keyed, so a second file on the same page is a fresh surface rather
  // than an update the library may fold into the first: opening
  // `main.rs` from the rail beside `README.md` kept showing the README.
  // The pool around it is not keyed, so the workers stay.
  return (
    <CodePool>
      <File key={key} file={file} options={FILE_OPTIONS} />
    </CodePool>
  );
}

// ---------------------------------------------------------------------
// The whole-repository tree beside a file

const TREE_STYLE: CSSProperties = {
  borderRadius: "var(--radius)",
  border: "1px solid var(--border)",
};

/// Every path in the repository, the current file selected. Clicking a
/// file navigates; clicking a directory only opens or closes it, because
/// leaving the file you were reading to look at a listing is not what a
/// click on a folder in a rail means.
export function RepoTree(props: {
  paths: string[];
  current: string;
  onOpenFile: (path: string) => void;
  height: number | string;
}) {
  // The model is created once; callbacks it captured then would go stale,
  // so they read through a ref that always holds the latest props.
  const latest = useRef(props);
  latest.current = props;
  // Selection the page set, as opposed to selection a person made. The
  // page selects the current file after every navigation; that must not
  // read as "open this file", or the browser's back button lands on the
  // listing and is at once sent forward again to the file it left.
  const applying = useRef(false);
  const { model } = useFileTree({
    paths: props.paths,
    initialExpansion: "closed",
    initialExpandedPaths: ancestorsOf(props.current),
    initialSelectedPaths: [props.current],
    search: true,
    onSelectionChange: (selected) => {
      if (applying.current) return;
      const p = selected[0];
      if (p && !p.endsWith("/") && p !== latest.current.current) {
        latest.current.onOpenFile(p);
      }
    },
  });
  const first = useRef(true);
  useEffect(() => {
    if (first.current) return;
    model.resetPaths(props.paths, {
      initialExpandedPaths: ancestorsOf(latest.current.current),
    });
  }, [model, props.paths]);
  useEffect(() => {
    first.current = false;
    // Only a path the tree holds: while a navigation is in flight the
    // page may still show the last file against a path that is now a
    // directory, and there is nothing to select for that.
    const item = model.getItem(props.current);
    if (!item) return;
    applying.current = true;
    try {
      item.select();
      model.scrollToPath(props.current, { offset: "center" });
    } finally {
      applying.current = false;
    }
  }, [model, props.current]);
  return (
    <FileTree
      model={model}
      aria-label="Files"
      style={{ height: props.height, ...TREE_STYLE }}
    />
  );
}

// ---------------------------------------------------------------------
// A diff, for review

export type CodeDiffProps<M> = {
  fileDiff: FileDiffMetadata;
  options?: FileDiffOptions<M, undefined>;
  lineAnnotations?: DiffLineAnnotation<M>[];
  renderAnnotation?: (annotation: DiffLineAnnotation<M>) => ReactNode;
  renderGutterUtility?: () => ReactNode;
  selectedLines?: SelectedLineRange | null;
  /// Scroll a line into view once it has rendered. Applied once per
  /// distinct value; the line must be inside an expanded region — the
  /// caller expands the file when a deep link points into a fold.
  reveal?: { line: number; side: "additions" | "deletions" } | null;
  className?: string;
};

/// A diff between two versions of one file. The caller parses (see
/// `parseDiff`) so that "these are the same" is a sentence the caller
/// owns rather than an exception the library throws.
export function CodeDiff<M = undefined>(props: CodeDiffProps<M>) {
  const options = useMemo<FileDiffOptions<M, undefined>>(
    () => ({ ...BASE, ...props.options }),
    [props.options],
  );
  const host = useRef<HTMLDivElement>(null);
  const revealKey = props.reveal
    ? `${props.reveal.side}:${props.reveal.line}`
    : null;
  useEffect(() => {
    if (!revealKey || !host.current) return;
    // The library renders asynchronously (in the worker), so the row is
    // found by looking a few frames, not by assuming it is there.
    let frames = 0;
    let handle = 0;
    const look = () => {
      const root = host.current
        ?.querySelector("diffs-container")
        ?.shadowRoot;
      const row = root?.querySelector("[data-selected-line]");
      if (row) {
        row.scrollIntoView({ block: "center" });
        return;
      }
      if (frames++ < 120) handle = requestAnimationFrame(look);
    };
    handle = requestAnimationFrame(look);
    return () => cancelAnimationFrame(handle);
  }, [revealKey]);
  return (
    <div ref={host} className={props.className}>
      <FileDiff<M, undefined>
        fileDiff={props.fileDiff}
        options={options}
        lineAnnotations={props.lineAnnotations}
        renderAnnotation={props.renderAnnotation}
        renderGutterUtility={props.renderGutterUtility}
        selectedLines={props.selectedLines}
      />
    </div>
  );
}

/// Parse two versions of a file into what `CodeDiff` renders. `null` for
/// a side that does not exist (an added or a deleted file); `null` back
/// when there is nothing to show — the library throws on identical
/// sides, and "no difference" is an ordinary answer here, not an error,
/// because the whitespace toggle can make any file identical.
export function parseDiff(
  oldFile: FileContents | null,
  newFile: FileContents | null,
  opts: { ignoreWhitespace: boolean },
): FileDiffMetadata | null {
  if (oldFile === null && newFile === null) return null;
  try {
    const diff = parseDiffFromFile(oldFile, newFile, {
      ignoreWhitespace: opts.ignoreWhitespace,
    });
    // The parser throws on two identical strings but *answers* — with a
    // diff of nothing but context — when they differ only in the
    // whitespace it was told to ignore. Rendered, that is an empty box
    // under a toolbar; the caller's sentence is the honest answer, and
    // it only says it for null.
    return isUnchanged(diff) ? null : diff;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------
// The files of a change

/// The files a change touches, as a tree, every directory open, empty
/// directories run together (`src/deep/` rather than `src/` then
/// `deep/`). Selection is the caller's: it says which path is open and
/// hears which was clicked.
export function CodeTree(props: {
  paths: string[];
  gitStatus?: GitStatusEntry[];
  renderRowDecoration?: FileTreeRowDecorationRenderer;
  selectedPath: string | null;
  onSelect: (path: string) => void;
  height: number | string;
  label: string;
}) {
  const latest = useRef(props);
  latest.current = props;
  const applying = useRef(false);
  const { model } = useFileTree({
    paths: props.paths,
    flattenEmptyDirectories: true,
    initialExpansion: "open",
    initialSelectedPaths: props.selectedPath ? [props.selectedPath] : [],
    gitStatus: props.gitStatus,
    renderRowDecoration: props.renderRowDecoration,
    onSelectionChange: (selected) => {
      if (applying.current) return;
      const p = selected[0];
      if (p && !p.endsWith("/") && p !== latest.current.selectedPath) {
        latest.current.onSelect(p);
      }
    },
  });
  const first = useRef(true);
  useEffect(() => {
    if (first.current) return;
    model.resetPaths(props.paths);
  }, [model, props.paths]);
  useEffect(() => {
    if (first.current) return;
    model.setGitStatus(props.gitStatus);
  }, [model, props.gitStatus]);
  useEffect(() => {
    first.current = false;
    const item = props.selectedPath ? model.getItem(props.selectedPath) : null;
    if (!item || !props.selectedPath) return;
    applying.current = true;
    try {
      item.select();
      model.scrollToPath(props.selectedPath, { offset: "nearest" });
    } finally {
      applying.current = false;
    }
  }, [model, props.selectedPath]);
  return (
    <FileTree
      model={model}
      aria-label={props.label}
      style={{ height: props.height, ...TREE_STYLE }}
    />
  );
}
