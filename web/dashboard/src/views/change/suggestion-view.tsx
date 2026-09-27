// Suggested changes on the page: the mini-diff a suggestion block
// renders as, the controls that take one, and the bar that takes
// several as a single patchset.
//
// Kept out of `changes.tsx` for the same reason the review sheet is —
// that file is already the whole review page — and the arithmetic these
// render lives one door further out again, in `suggestions.ts`, where
// "is this a suggestion at all" and "would the server take it" are
// answered without a browser.

import { Button } from "@/components/ui/button";
import {
  anchorRange,
  applyLabel,
  applyRefusal,
  type SuggestionAnchor,
} from "@/views/change/suggestions";

/// Everything the suggestion controls need from the view around them.
///
/// One bag rather than six props threaded by hand through a thread
/// card, a reply and a diff row: the page has already been bitten once
/// by a prop that reached one mount and not the other, and every
/// question the viewer asked answered false on that one.
export interface SuggestionActors {
  /// Whether this viewer may commit at all — `repo:write`, the same
  /// scope the apply route demands, and the change still being open.
  /// False draws no control anywhere: the 403 is the honest fallback
  /// for a race, not the primary check, and a reader without write
  /// access must not be handed a button that leads to one.
  canApply: boolean;
  /// The comment ids gathered for the next patchset. Held by the view
  /// above rather than per card, because the whole point of the route
  /// is that several suggestions become **one** patchset.
  selected: Set<string>;
  onSelect: (commentId: string, on: boolean) => void;
  /// Apply this one on its own — the common case, and the reason a
  /// batch is not the only door.
  onApplyOne: (commentId: string) => void;
  /// The lines this anchor stands in place of, as the file read at the
  /// patchset the comment was written against. `null` while they are
  /// still being fetched, and for a range that file does not have.
  replaced: (anchor: SuggestionAnchor) => string[] | null;
  busy: boolean;
}

/// One suggestion block, drawn against the lines it replaces.
///
/// A plain code fence says "here is some code"; a reviewer suggesting a
/// change is saying "these lines, instead of those" — and that is a
/// diff. Rendering it as a fence leaves the author reading two
/// disconnected things and diffing them in their head, which is exactly
/// the work the button below is about to do for them.
///
/// **An empty block is a deletion and must read as one.** It is a
/// suggestion with no lines, which the parser keeps distinct from a
/// comment carrying no block at all; drawn naively it is an empty box,
/// which reads as a rendering failure rather than as "take these lines
/// out". So the deletion is said in words in the caption, whether or
/// not the replaced lines are on screen to strike through.
export function SuggestionMiniDiff(props: {
  lines: string[];
  anchor: SuggestionAnchor | null;
  replaced: string[] | null;
}) {
  const { lines, anchor, replaced } = props;
  const deletion = lines.length === 0;
  const where = anchor ? `${anchor.path} ${anchorRange(anchor)}` : null;
  const caption = deletion
    ? where
      ? `Suggested change: delete ${where}`
      : "Suggested change: delete these lines"
    : where
      ? `Suggested change to ${where}`
      : "Suggested change";
  return (
    <div className="my-2 overflow-hidden rounded-md border border-borderline bg-surface-0">
      <div className="border-b border-borderline bg-surface-1 px-3 py-1.5 text-xs text-ink-3">
        {caption}
      </div>
      {/* Mono, one line per row, and the same glyph-and-tint pair the
          file diff above uses — a second visual language for the same
          idea would be one more thing to learn on a page that already
          has a diff on it. Colour never carries it alone: the `+` and
          `−` are there for anybody who cannot tell the two tints
          apart. */}
      <div className="overflow-x-auto p-2 font-mono text-xs leading-5">
        {(replaced ?? []).map((text, i) => (
          <div key={`old-${i}`} className="whitespace-pre text-serious">
            <span aria-hidden className="select-none">
              {"− "}
            </span>
            {text}
          </div>
        ))}
        {lines.map((text, i) => (
          <div key={`new-${i}`} className="whitespace-pre text-good">
            <span aria-hidden className="select-none">
              {"+ "}
            </span>
            {text}
          </div>
        ))}
        {deletion && (
          // Said in words as well as drawn, and said even when the
          // replaced lines could not be read: a block with nothing in
          // it and a box that failed to render look identical, and only
          // one of them is a proposal.
          <div className="whitespace-pre-wrap font-sans text-ink-3">
            Nothing takes their place — this suggests removing them.
          </div>
        )}
      </div>
    </div>
  );
}

/// What the author may do about one suggestion: take it now, or gather
/// it with the others.
///
/// Both, because they are two different acts. Taking one is the common
/// case and a second click for it would be ceremony; gathering is the
/// whole reason the route accepts a list — a reviewer leaves five
/// remarks and the author who took them one at a time would have made
/// five patchsets, started CI five times and notified everybody OWNERS
/// names five times, for one act.
///
/// Every control is named with the author and the file, not with the
/// bare word "Apply": a change can carry a dozen of these, and a page
/// full of identically-named buttons is one a keyboard reader cannot
/// tell apart — and one no test can assert about without matching a
/// substring that hits all twelve.
export function SuggestionActions(props: {
  commentId: string;
  author: string;
  anchor: SuggestionAnchor;
  actors: SuggestionActors;
}) {
  const { commentId, author, anchor, actors } = props;
  if (!actors.canApply) return null;
  const where = `${anchor.path} ${anchorRange(anchor)}`;
  const inBatch = actors.selected.has(commentId);
  return (
    <div className="flex flex-wrap items-center gap-2">
      <Button
        type="button"
        size="xs"
        variant="outline"
        aria-label={`Apply ${author}'s suggestion on ${where}`}
        disabled={actors.busy}
        onClick={() => actors.onApplyOne(commentId)}
      >
        Apply suggestion
      </Button>
      <Button
        type="button"
        size="xs"
        variant="ghost"
        aria-label={
          inBatch
            ? `Leave ${author}'s suggestion on ${where} out of the next patchset`
            : `Add ${author}'s suggestion on ${where} to the next patchset`
        }
        disabled={actors.busy}
        onClick={() => actors.onSelect(commentId, !inBatch)}
      >
        {inBatch ? "Leave out of the batch" : "Add to the batch"}
      </Button>
      {inBatch && (
        <span className="text-xs text-ink-3">
          Waiting for the rest of the batch.
        </span>
      )}
    </div>
  );
}

/// "You have gathered these; here is the one commit they make."
///
/// Above the review rather than beside any one comment, and following
/// the reader for the same reason the pending-review bar does: the
/// suggestions being gathered are spread down a forty-file diff, and a
/// batch nobody applies is a set of changes the author believes they
/// took.
///
/// What the batch cannot be — more than one call takes — is said here
/// before the trip; what the *server* refuses is said by
/// `SuggestionRefusal`, which stands whether or not anything is
/// gathered, because applying one suggestion on its own can be refused
/// in exactly the same words.
export function SuggestionBatchBar(props: {
  count: number;
  busy: boolean;
  onApply: () => void;
  onClear: () => void;
}) {
  const tooMany = applyRefusal(props.count);
  return (
    <div
      // A landmark with a name: it is a standing statement about the
      // page rather than a control somebody navigated to.
      role="status"
      aria-label="Gathered suggestions"
      className="flex flex-wrap items-center gap-x-3 gap-y-2 rounded-lg border border-brand/40 bg-brand/5 p-3"
    >
      <span className="text-sm font-medium text-ink">
        {applyLabel(props.count)} gathered
      </span>
      <span className="text-xs text-ink-3">
        They go in together, as one patchset.
      </span>
      <span className="ml-auto flex flex-wrap items-center gap-2">
        <Button
          type="button"
          size="xs"
          disabled={props.busy || tooMany !== null}
          onClick={props.onApply}
        >
          Apply as one patchset
        </Button>
        <Button
          type="button"
          size="xs"
          variant="ghost"
          disabled={props.busy}
          onClick={props.onClear}
        >
          Clear
        </Button>
      </span>
      {tooMany && (
        <p className="w-full text-xs text-warning" role="alert">
          {tooMany}
        </p>
      )}
    </div>
  );
}

/// The apply route's refusal, in the route's own words.
///
/// Printed verbatim and with nothing in front of it — who has write
/// access here, which two comments overlap on which lines, which file
/// has moved since the patchset a comment anchors to. Every one of
/// those sentences is an instruction to the person who pressed the
/// button, and a "Could not apply the suggestions:" welded onto the
/// front would be the page apologising over the top of the answer.
export function SuggestionRefusal(props: { message: string | null }) {
  if (!props.message) return null;
  return (
    <p
      className="rounded-lg border border-borderline bg-surface-1 p-3 text-sm text-serious"
      role="alert"
    >
      {props.message}
    </p>
  );
}
