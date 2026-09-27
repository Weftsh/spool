import type { FileDiffMetadata } from "@/code/types";

/// `+N −M` for one parsed diff: the lines the file gained and lost,
/// summed over every hunk's change blocks. Pure, over the shape the
/// parser produces, so the count and the rendering come from one parse
/// and cannot disagree about whitespace.
export function sumChanges(diff: FileDiffMetadata): {
  added: number;
  deleted: number;
} {
  let added = 0;
  let deleted = 0;
  for (const hunk of diff.hunks) {
    for (const block of hunk.hunkContent) {
      if (block.type === "change") {
        added += block.additions;
        deleted += block.deletions;
      }
    }
  }
  return { added, deleted };
}

/// Whether a parse found nothing at all: every hunk is context, or there
/// are no hunks. The parser answers this shape rather than throwing when
/// two sides differ only in what `ignoreWhitespace` discards, and a diff
/// with nothing in it renders as nothing — no lines, no sentence — so
/// the caller turns it into the sentence "No difference…" instead.
export function isUnchanged(diff: FileDiffMetadata): boolean {
  const { added, deleted } = sumChanges(diff);
  return added === 0 && deleted === 0;
}

/// The first line a reviewer would want to say something about: the
/// start of the first hunk on the new side, or line 1 of an empty diff.
export function firstChangedLine(diff: FileDiffMetadata): number {
  const h = diff.hunks[0];
  return h ? Math.max(1, h.additionStart) : 1;
}

/// Whether a new-side line is inside a hunk's rendered range — that is,
/// on screen without unfolding anything. A form hung on a folded line
/// has nowhere to sit.
export function lineInHunks(diff: FileDiffMetadata, line: number): boolean {
  return diff.hunks.some(
    (h) => line >= h.additionStart && line < h.additionStart + h.additionCount,
  );
}
