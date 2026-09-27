// The batched review's three surfaces: the bar that says you have
// unsent work, the sheet that sends it as one act, and the standing
// "no"s that explain themselves.
//
// Kept out of `changes.tsx` because that file is already the whole
// review page and this is a self-contained pass over it — but the
// arithmetic they render lives one door further out again, in
// `review.ts`, so the questions worth pinning ("does absent mean
// empty", "does this block actually block") are answered without a
// browser.

import { useId, useState } from "react";
import type { ReviewVerdictKind } from "@/api";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { CommentComposer } from "@/views/change/composer";
import {
  NO_PICKER,
  REVIEW_VERDICTS,
  pendingLabel,
  reviewRefusal,
  type BlockStanding,
} from "@/views/change/review";
import { cn } from "@/lib/utils";

/// "You have said things nobody can see yet."
///
/// Above the review rather than inside the Actions rail on purpose: a
/// reviewer working through a forty-file diff is nowhere near the
/// sidebar, and a draft they forget to submit is a review that never
/// happened — silently, with the page looking exactly like one where
/// they had spoken. It is the one piece of this feature that has to
/// follow the reader.
export function PendingReviewBar(props: {
  count: number;
  busy: boolean;
  onFinish: () => void;
}) {
  return (
    <div
      // A landmark with a name, because it is a standing statement about
      // the page rather than a control somebody navigated to.
      role="status"
      aria-label="Pending review"
      className="flex flex-wrap items-center gap-x-3 gap-y-2 rounded-lg border border-brand/40 bg-brand/5 p-3"
    >
      <span className="text-sm font-medium text-ink">
        {pendingLabel(props.count)}
      </span>
      <span className="text-xs text-ink-3">
        Only you can see {props.count === 1 ? "it" : "them"} until you submit.
      </span>
      <Button
        type="button"
        size="xs"
        className="ml-auto"
        disabled={props.busy}
        onClick={props.onFinish}
      >
        Finish your review
      </Button>
    </div>
  );
}

/// The submit sheet: one verdict, one cover message, one request.
///
/// The verdicts are **radios**, not three buttons. Three buttons is
/// three ways to send, and a reviewer who has written a paragraph and
/// then presses the wrong one has published a verdict they did not
/// mean; a radio is a choice you can see before you commit to it, and
/// there is one Submit.
export function ReviewSheet(props: {
  /// How many drafted comments go out with it. Shown, because "submit"
  /// on a batch is a different act depending on the number, and because
  /// it is half of what makes `request_changes` legal with no words.
  drafts: number;
  busy: boolean;
  onSubmit: (verdict: ReviewVerdictKind, body: string) => void;
  onDiscard: () => void;
  onCancel: () => void;
}) {
  const [verdict, setVerdict] = useState<ReviewVerdictKind>("comment");
  const [body, setBody] = useState("");
  const [confirmingDiscard, setConfirmingDiscard] = useState(false);
  const id = useId();
  const refusal = reviewRefusal(verdict, body, props.drafts);

  return (
    <section
      aria-label="Submit your review"
      className="rounded-lg border border-borderline bg-surface-1 p-4"
    >
      <div className="mb-1 text-sm font-medium text-ink">
        Submit your review
      </div>
      <p className="mb-3 text-xs text-ink-3">
        Everything you drafted goes out together, with one notification —
        {props.drafts === 0
          ? " you have drafted nothing, so this sends the verdict and the message below."
          : ` ${pendingLabel(props.drafts)} and the message below.`}
      </p>
      <form
        className="space-y-3"
        onSubmit={(e) => {
          e.preventDefault();
          if (refusal || props.busy) return;
          props.onSubmit(verdict, body.trim());
        }}
      >
        {/* A real radiogroup: one stop in the tab order, arrow keys
            between the three, and a reader who cannot see which is lit
            is told by `aria-checked`. Three styled buttons would be
            three tab stops that look like a choice and are not. */}
        <div
          role="radiogroup"
          aria-label="Verdict"
          className="flex flex-col gap-1.5"
        >
          {REVIEW_VERDICTS.map((v) => (
            <label
              key={v.value}
              className={cn(
                "flex cursor-pointer items-start gap-2 rounded-md border p-2 transition",
                verdict === v.value
                  ? "border-brand/50 bg-surface-2"
                  : "border-borderline hover:border-ink-3",
              )}
            >
              {/* Named by the word, described by the sentence — and
                  the two are different things. A wrapping `<label>`
                  hands an input *all* of its text content as the
                  accessible name, so this control announced itself as
                  "Request changes Say no, and say what would make it a
                  yes. It survives the next patchset and ends when you
                  withdraw it." A name is what you call a thing; a
                  paragraph is not one, and hearing the whole rationale
                  read back on every arrow-key press is how somebody
                  stops listening to the names at all. `aria-labelledby`
                  wins over the wrapping label, so the help still
                  reaches a screen reader — as the description it is. */}
              <input
                type="radio"
                className="mt-0.5 size-3.5 shrink-0 accent-brand"
                name={`${id}-verdict`}
                value={v.value}
                aria-labelledby={`${id}-${v.value}-label`}
                aria-describedby={`${id}-${v.value}-help`}
                checked={verdict === v.value}
                onChange={() => setVerdict(v.value)}
              />
              <span className="min-w-0">
                <span
                  id={`${id}-${v.value}-label`}
                  className="block text-sm text-ink"
                >
                  {v.label}
                </span>
                <span
                  id={`${id}-${v.value}-help`}
                  className="block text-xs text-ink-3"
                >
                  {v.help}
                </span>
              </span>
            </label>
          ))}
        </div>
        {/* The composer that already exists, not a second one. The
            whole point of its Preview is that it renders through the
            same `Markdown` the posted body will be rendered through,
            and a hand-written box beside it is one more place for the
            preview to drift from the thing it is previewing. */}
        <CommentComposer
          value={body}
          onChange={setBody}
          label="Review message"
          placeholder="What should the author know about this pass?"
        >
          <div className="flex flex-wrap items-center gap-2">
            <Button type="submit" disabled={props.busy || refusal !== null}>
              Submit review
            </Button>
            <Button
              type="button"
              variant="ghost"
              disabled={props.busy}
              onClick={props.onCancel}
            >
              Keep drafting
            </Button>
            {props.drafts > 0 &&
              // Two steps, and the second one says what is being thrown
              // away. A draft that could not be abandoned would be a
              // trap, and one abandoned by a single stray click would
              // be worse.
              (confirmingDiscard ? (
                <span className="ml-auto flex flex-wrap items-center gap-2">
                  <span className="text-xs text-ink-2">
                    Throw away {pendingLabel(props.drafts)}?
                  </span>
                  <Button
                    type="button"
                    size="xs"
                    variant="destructive"
                    disabled={props.busy}
                    onClick={props.onDiscard}
                  >
                    Discard them
                  </Button>
                  <Button
                    type="button"
                    size="xs"
                    variant="ghost"
                    disabled={props.busy}
                    onClick={() => setConfirmingDiscard(false)}
                  >
                    Keep them
                  </Button>
                </span>
              ) : (
                <Button
                  type="button"
                  size="xs"
                  variant="ghost"
                  className="ml-auto"
                  disabled={props.busy}
                  onClick={() => setConfirmingDiscard(true)}
                >
                  Discard draft review
                </Button>
              ))}
          </div>
        </CommentComposer>
        {refusal && (
          <p className="text-xs text-warning" role="alert">
            {refusal}
          </p>
        )}
      </form>
      {/* The refusal, stated where somebody would reach for the thing
          that is missing. There is no "request a review from" here and
          there is not going to be one — see NO_PICKER. */}
      <p className="mt-3 text-xs text-ink-3">{NO_PICKER}</p>
    </section>
  );
}

/// Every standing request for changes, and what each one means.
///
/// Rendered for **everybody**, including the blocks that do not block.
/// One that stops the change and one that does not are the same act
/// said by two people, and a page that showed only the first would hide
/// half the review from the author who has to answer it.
export function StandingBlocks(props: {
  blocks: BlockStanding[];
  busy: boolean;
  onWithdraw: () => void;
}) {
  if (props.blocks.length === 0) return null;
  return (
    <section
      aria-label="Requested changes"
      className="rounded-lg border border-borderline bg-surface-1 p-4"
    >
      <div className="mb-2 text-sm font-medium text-ink">Requested changes</div>
      <ul className="space-y-3">
        {props.blocks.map((s) => (
          <li key={s.block.id} className="space-y-1">
            <div className="flex flex-wrap items-baseline gap-2 text-xs">
              <span className="font-medium text-ink">{s.block.author}</span>
              {/* Glyph and word together, and never colour alone: a
                  reader who cannot tell the two tints apart still has
                  to know which of these stops their change. */}
              <Badge
                className={cn(
                  "gap-1.5",
                  s.authoritative ? "text-warning" : "text-ink-2",
                )}
              >
                <span aria-hidden>{s.authoritative ? "■" : "○"}</span>
                {s.authoritative ? "Blocking" : "Advisory"}
              </Badge>
              {s.block.author_email && (
                <span className="text-ink-3">{s.block.author_email}</span>
              )}
            </div>
            {/* The reviewer's own words, verbatim. A body is optional
                only when the review's comments carried the argument, so
                the fallback points at where those are rather than
                inventing a sentence nobody said. */}
            <p className="whitespace-pre-wrap break-words text-sm text-ink-2">
              {s.block.body ?? "See their comments on the diff."}
            </p>
            <p className="text-xs text-ink-3">{s.why}</p>
            {s.mine && (
              // Only its author. The server's route cannot name anybody
              // else's block, so offering this to a reader would be a
              // control that answers 404 — and a block somebody else
              // could clear is not a block.
              <Button
                type="button"
                size="xs"
                variant="outline"
                disabled={props.busy}
                onClick={props.onWithdraw}
              >
                Withdraw my request for changes
              </Button>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}
