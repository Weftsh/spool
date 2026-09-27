// The box people write review comments in — the change-wide one, the
// line one, and the reply one — with a Preview beside it.
//
// One component for all three, because the whole point of the preview is
// that it renders through exactly the same `Markdown` the posted comment
// will be rendered through. Three hand-written composers is three places
// for the preview to drift from the thing it is previewing, which makes
// it worse than no preview: a reviewer who is shown a table and then
// posts a paragraph learns to distrust the button.

import { useId, useState, type ReactNode } from "react";
import { Markdown } from "@/components/markdown";
import { cn } from "@/lib/utils";

/// The 4000-byte bound the server enforces on a comment body, said in
/// the field rather than discovered as a 400.
const MAX_BODY = 4000;

export function CommentComposer(props: {
  value: string;
  onChange: (value: string) => void;
  /// The textarea's accessible name. Every one of these boxes is on a
  /// page with several others, so "Comment" alone would not say which.
  label: string;
  placeholder?: string;
  autoFocus?: boolean;
  className?: string;
  /// The submit and cancel controls, which differ per caller.
  children?: ReactNode;
}) {
  const [previewing, setPreviewing] = useState(false);
  const id = useId();
  const panel = `${id}-panel`;
  const tab = (name: "write" | "preview") => `${id}-${name}`;

  // Real tabs, not two styled buttons: the pair is a single stop in the
  // tab order with arrow keys between them, and a reader who cannot see
  // which one is lit is told by `aria-selected`.
  const tabClass = (on: boolean) =>
    cn(
      "rounded-md px-2 py-0.5 text-xs transition",
      on
        ? "bg-surface-2 font-medium text-ink"
        : "text-ink-3 hover:text-ink focus:text-ink",
    );

  return (
    <div className={cn("flex flex-col gap-2", props.className)}>
      <div
        role="tablist"
        // Deliberately *not* built out of `props.label`. Naming it
        // "Comment on this change: write or preview" makes the tablist's
        // accessible name a superstring of the textarea's, so every
        // `getByLabel("Comment on this change")` — and every assistive
        // technology doing the same substring match — resolves to two
        // elements. It took two unrelated tests down before it was
        // caught. The tabs say which is selected; the field beside them
        // says what is being written.
        aria-label="Write or preview this comment"
        className="flex items-center gap-1"
      >
        <button
          type="button"
          role="tab"
          id={tab("write")}
          aria-selected={!previewing}
          aria-controls={panel}
          className={tabClass(!previewing)}
          onClick={() => setPreviewing(false)}
        >
          Write
        </button>
        <button
          type="button"
          role="tab"
          id={tab("preview")}
          aria-selected={previewing}
          aria-controls={panel}
          className={tabClass(previewing)}
          onClick={() => setPreviewing(true)}
        >
          Preview
        </button>
        <span className="ml-auto text-xs text-ink-3">Markdown</span>
      </div>
      {/* One panel, switched — rather than two with one hidden. A
          hidden textarea keeps its value and its focus ring, and a
          reader tabbing through the form lands in a box they cannot
          see. */}
      <div
        role="tabpanel"
        id={panel}
        aria-labelledby={tab(previewing ? "preview" : "write")}
      >
        {previewing ? (
          <div className="min-h-16 rounded-md border border-borderline bg-surface-0 px-3 py-2">
            {props.value.trim() ? (
              // `headingLevel` 3: a comment's `#` sits under the panel
              // heading in the document outline, not beside the change's
              // own title.
              <Markdown
                source={props.value}
                headingLevel={3}
                className="text-sm"
              />
            ) : (
              <p className="text-sm text-ink-3">Nothing to preview yet.</p>
            )}
          </div>
        ) : (
          <textarea
            className="min-h-16 w-full rounded-md border border-borderline bg-surface-0 px-3 py-2 text-sm"
            aria-label={props.label}
            placeholder={props.placeholder}
            value={props.value}
            onChange={(e) => props.onChange(e.target.value)}
            maxLength={MAX_BODY}
            autoFocus={props.autoFocus}
          />
        )}
      </div>
      {props.children}
    </div>
  );
}
