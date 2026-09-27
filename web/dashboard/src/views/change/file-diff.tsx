import { useMemo, useRef, useState, type ReactNode } from "react";
import { CodeBoundary, LazyCodeDiff } from "@/code/lazy";
import type {
  DiffLineAnnotation,
  FileDiffMetadata,
  FileDiffOptions,
  SelectedLineRange,
} from "@/code/types";
import { lineAnchor } from "@/views/change/anchors";
import type { DiffStyle } from "@/views/change/diff-prefs";
import type { LineDraft } from "@/views/change/line-composer";
import { anchorOf, type Side, type Thread } from "@/views/change/threads";

/// What hangs off a line of the diff: a thread somebody already wrote,
/// or the form for the one being written.
type Hanging = { kind: "thread"; thread: Thread } | { kind: "draft" };

const SIDE: Record<Side, "additions" | "deletions"> = {
  new: "additions",
  old: "deletions",
};
const BACK: Record<"additions" | "deletions", Side> = {
  additions: "new",
  deletions: "old",
};

/// How many lines one press of an expand control uncovers. GitHub's is
/// twenty; twenty-four is one screenful at this font size, which is the
/// unit a reviewer is actually asking for when they say "show me a bit
/// more".
const EXPAND_STEP = 24;

/// Where the `+` sits. The library mounts its gutter utility flush
/// right in the number cell — over the right-aligned number — so the
/// two shared one hit area: a press on the number to start a drag
/// landed on the `+` and opened a comment on whichever line the pointer
/// let go over. The slot goes to the left instead, and the cell keeps
/// enough room there that a three-digit number and the control never
/// touch. Layout only; colours stay in `pierre.css`.
const GUTTER_CSS = `
[data-column-number] { padding-left: 3ch; }
[data-gutter-utility-slot] { right: auto; left: 0; justify-content: flex-start; }
`;

/// One file's diff, for review: the library renders the lines, and this
/// hangs the conversation on them.
///
/// Threads sit under the line they are about as annotations; the draft
/// form is one more. The `+` that starts a remark rides in the gutter
/// beside whichever line the pointer is on, labelled with that line so
/// a reader who cannot see the hover is told the same thing; dragging
/// across lines opens the form for the range. Clicking a new-side line
/// number writes the line's address into the URL, which is what a
/// reviewer copies to say "look at this".
export function ReviewFileDiff(props: {
  path: string;
  fileDiff: FileDiffMetadata;
  threads: Thread[];
  draft: LineDraft | null;
  onDraft: (draft: LineDraft | null) => void;
  /// The line a link into this diff named, if it named one in this file.
  linked: number | null;
  diffStyle: DiffStyle;
  expanded: boolean;
  renderThread: (thread: Thread) => ReactNode;
  composer: ReactNode;
}) {
  const { path, draft } = props;
  // Handlers read through a ref so the options object can stay stable:
  // the library re-renders the whole surface when options change
  // identity, and a fresh closure per render would be exactly that.
  const latest = useRef(props);
  latest.current = props;
  const [hover, setHover] = useState<{
    line: number;
    side: "additions" | "deletions";
  } | null>(null);

  const options = useMemo<FileDiffOptions<Hanging, undefined>>(
    () => ({
      diffStyle: props.diffStyle,
      expandUnchanged: props.expanded,
      expansionLineCount: EXPAND_STEP,
      hunkSeparators: "line-info",
      overflow: "wrap",
      stickyHeader: false,
      enableLineSelection: true,
      lineHoverHighlight: "both",
      // Not implied by `renderGutterUtility`: the library resolves this
      // flag on its own and defaults it off, so without it the container
      // the `+` mounts into is never created and no line ever offers a
      // comment. The three line-comment specs are what hold this.
      enableGutterUtility: true,
      unsafeCSS: GUTTER_CSS,
      onLineEnter: (e) =>
        setHover({ line: e.lineNumber, side: e.annotationSide }),
      onLineLeave: () => setHover(null),
      onLineNumberClick: (e) => {
        // The new-side number is the address of the line. Deleted lines
        // have no new number and so no anchor — the same rule the
        // comment gutter used to follow, because a line that is gone has
        // nowhere to point at.
        if (e.annotationSide !== "additions") return;
        window.location.hash = lineAnchor(latest.current.path, e.lineNumber);
      },
      onLineSelectionEnd: (range) => {
        // A drag across lines is a remark about the block. A click on
        // one line is a link, handled above, not a remark.
        if (!range || range.end === range.start) return;
        const side = BACK[range.side ?? "additions"];
        latest.current.onDraft({
          side,
          line: Math.min(range.start, range.end),
          end: Math.max(range.start, range.end),
        });
      },
    }),
    [props.diffStyle, props.expanded],
  );

  const annotations = useMemo<DiffLineAnnotation<Hanging>[]>(() => {
    const out: DiffLineAnnotation<Hanging>[] = [];
    for (const thread of props.threads) {
      const a = anchorOf(thread.root);
      if (!a || a.path !== path) continue;
      out.push({
        side: SIDE[a.side],
        lineNumber: a.line,
        metadata: { kind: "thread", thread },
      });
    }
    if (draft) {
      out.push({
        side: SIDE[draft.side],
        lineNumber: draft.end !== null && draft.end > draft.line ? draft.end : draft.line,
        metadata: { kind: "draft" },
      });
    }
    return out;
  }, [props.threads, path, draft]);

  // The range under construction is the selection; failing that, the
  // linked line, tinted so a person arriving by URL sees where they
  // landed.
  const selectedLines = useMemo<SelectedLineRange | null>(() => {
    if (draft) {
      return {
        start: draft.line,
        end: draft.end !== null && draft.end > draft.line ? draft.end : draft.line,
        side: SIDE[draft.side],
      };
    }
    if (props.linked !== null) {
      return { start: props.linked, end: props.linked, side: "additions" };
    }
    return null;
  }, [draft, props.linked]);

  const reveal = useMemo(
    () =>
      props.linked !== null
        ? { line: props.linked, side: "additions" as const }
        : null,
    [props.linked],
  );

  const gutterLabel = hover
    ? hover.side === "deletions"
      ? `Comment on deleted line ${hover.line}`
      : `Comment on line ${hover.line}`
    : "Comment on this line";

  return (
    <CodeBoundary
      fallback={<p className="p-3 text-sm text-ink-3">Loading diff…</p>}
    >
      <LazyCodeDiff<Hanging>
        fileDiff={props.fileDiff}
        options={options}
        lineAnnotations={annotations}
        selectedLines={selectedLines}
        reveal={reveal}
        renderAnnotation={(a) =>
          a.metadata.kind === "thread" ? (
            <div className="border-y border-borderline bg-surface-1 px-4 py-2 font-sans text-xs">
              {props.renderThread(a.metadata.thread)}
            </div>
          ) : (
            props.composer
          )
        }
        renderGutterUtility={() => (
          // Positioned and raised: the library paints the line number
          // above its utility slot (`[data-line-number-content]` has a
          // z-index, the slot has none), so without this the `+` sat
          // under the number and a click on it linked to the line
          // instead of opening the composer.
          <button
            type="button"
            aria-label={gutterLabel}
            title={gutterLabel}
            className="relative z-[2] rounded border border-borderline bg-surface-1 px-1 font-mono text-xs leading-4 text-ink-3 hover:text-ink"
            onClick={() => {
              const h = hover;
              if (!h) return;
              const side = BACK[h.side];
              const cur = latest.current.draft;
              latest.current.onDraft(
                cur && cur.side === side && cur.line === h.line
                  ? null
                  : { side, line: h.line, end: null },
              );
            }}
          >
            +
          </button>
        )}
      />
    </CodeBoundary>
  );
}
