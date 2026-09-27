import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { CommentComposer } from "@/views/change/composer";
import type { Side } from "@/views/change/threads";

/// The line comment being written: which side of the diff it anchors
/// to, the line it starts on, and the inclusive last line when it is
/// about a block rather than a line.
///
/// The side is carried rather than derived because the same number
/// means a different line on each half of the diff, and a range needs
/// both ends — a comment on "these five lines" written as five comments
/// is how a reviewer's one objection becomes five threads.
export interface LineDraft {
  side: Side;
  line: number;
  end: number | null;
}

/// The form under a line: the remark, and the two ways to send it.
///
/// Sits in the diff as an annotation on the line it is about. The line
/// and the other end of the range are plain number fields as well as
/// gestures, because a drag or a hover is not an affordance for a
/// keyboard, and the `Comment on a line…` button in the file's toolbar
/// opens this form with nothing but a keyboard involved.
/// A number field that commits on blur or Enter, not on every keystroke.
///
/// Committing live moved the form: the draft's line is where the form
/// hangs in the diff, so typing "34" moved it to line 3 after the first
/// digit, remounted the input under the pointer and lost the second
/// digit. A person types a number and then looks up; that is the moment
/// to act on it.
function LineField(props: {
  label: string;
  value: number | null;
  min: number;
  onCommit: (value: number | null) => void;
}) {
  const [text, setText] = useState(props.value === null ? "" : String(props.value));
  useEffect(() => {
    setText(props.value === null ? "" : String(props.value));
  }, [props.value]);
  const commit = () => {
    const v = text.trim() === "" ? null : Number(text);
    if (v !== null && (!Number.isInteger(v) || v < props.min)) {
      setText(props.value === null ? "" : String(props.value));
      return;
    }
    if (v !== props.value) props.onCommit(v);
  };
  return (
    <input
      type="number"
      aria-label={props.label}
      className="w-20 rounded-md border border-borderline bg-surface-0 px-2 py-1 font-mono text-xs"
      min={props.min}
      value={text}
      onChange={(e) => setText(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          commit();
        }
      }}
    />
  );
}

export function LineComposer(props: {
  path: string;
  draft: LineDraft;
  body: string;
  onBody: (body: string) => void;
  onDraft: (draft: LineDraft) => void;
  /// Send it, published or drafted into the reviewer's own review.
  onSubmit: (pending: boolean) => void;
  onCancel: () => void;
  busy: boolean;
  /// Whether drafting is offered at all — false against a server older
  /// than batched reviews.
  batched: boolean;
}) {
  const { draft } = props;
  return (
    <form
      className="border-y border-borderline bg-surface-1 px-4 py-2 font-sans text-xs"
      onSubmit={(e) => {
        e.preventDefault();
        props.onSubmit(false);
      }}
    >
      <CommentComposer
        value={props.body}
        onChange={props.onBody}
        label={
          draft.side === "old"
            ? `Comment on ${props.path} deleted line ${draft.line}`
            : `Comment on ${props.path} line ${draft.line}`
        }
        placeholder="Why this line?"
        autoFocus
      >
        <div className="flex flex-wrap items-center gap-2">
          <Button
            type="submit"
            variant="outline"
            className="py-1"
            disabled={props.busy || !props.body.trim()}
          >
            Post line comment
          </Button>
          {props.batched && (
            <Button
              type="button"
              variant="outline"
              className="py-1"
              disabled={props.busy || !props.body.trim()}
              onClick={() => props.onSubmit(true)}
            >
              Add to review
            </Button>
          )}
          <Button
            type="button"
            variant="ghost"
            className="py-1"
            onClick={props.onCancel}
          >
            Cancel
          </Button>
          {/* Both ends of the range, typed. The first is the line the
              form sits on; changing it moves the form. The second is
              empty for "this line", and an end below the start is
              dropped rather than refused: the fields are a convenience,
              not a form to get wrong. */}
          <label className="ml-auto flex items-center gap-1.5 text-xs text-ink-3">
            Line
            <LineField
              label="Line"
              value={draft.line}
              min={1}
              onCommit={(v) => {
                if (v !== null) props.onDraft({ ...draft, line: v });
              }}
            />
          </label>
          <label className="flex items-center gap-1.5 text-xs text-ink-3">
            Through line
            <LineField
              label="Through line"
              value={draft.end}
              min={draft.line}
              onCommit={(v) => props.onDraft({ ...draft, end: v })}
            />
          </label>
        </div>
      </CommentComposer>
    </form>
  );
}
