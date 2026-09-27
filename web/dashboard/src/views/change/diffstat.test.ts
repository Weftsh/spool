import { describe, expect, it } from "vitest";
import type { FileDiffMetadata, Hunk } from "@/code/types";
import { firstChangedLine, lineInHunks, isUnchanged, sumChanges } from "./diffstat";

/// A hunk with only the fields the count reads; the rest is what a real
/// parse would carry and the count must not care about.
function hunk(blocks: Hunk["hunkContent"], at = { start: 1, count: 0 }): Hunk {
  return {
    collapsedBefore: 0,
    additionStart: at.start,
    additionCount: at.count,
    additionLines: 0,
    additionLineIndex: 0,
    deletionStart: 1,
    deletionCount: 0,
    deletionLines: 0,
    deletionLineIndex: 0,
    hunkContent: blocks,
    splitLineStart: 0,
    splitLineCount: 0,
    unifiedLineStart: 0,
    unifiedLineCount: 0,
    noEOFCRDeletions: false,
    noEOFCRAdditions: false,
  };
}

function diff(hunks: Hunk[]): FileDiffMetadata {
  return {
    name: "a.txt",
    type: "change",
    hunks,
    splitLineCount: 0,
    unifiedLineCount: 0,
    isPartial: false,
    deletionLines: [],
    additionLines: [],
  };
}

const change = (additions: number, deletions: number) =>
  ({
    type: "change" as const,
    additions,
    deletions,
    additionLineIndex: 0,
    deletionLineIndex: 0,
  });
const context = (lines: number) => ({
  type: "context" as const,
  lines,
  additionLineIndex: 0,
  deletionLineIndex: 0,
});

describe("sumChanges", () => {
  it("adds up every change block across hunks", () => {
    const d = diff([
      hunk([context(3), change(2, 1), context(3)]),
      hunk([change(0, 4), context(1), change(5, 0)]),
    ]);
    expect(sumChanges(d)).toEqual({ added: 7, deleted: 5 });
  });
  it("counts nothing for a diff with no hunks", () => {
    expect(sumChanges(diff([]))).toEqual({ added: 0, deleted: 0 });
  });
  it("ignores context, however much of it there is", () => {
    expect(sumChanges(diff([hunk([context(400)])]))).toEqual({
      added: 0,
      deleted: 0,
    });
  });
});

describe("isUnchanged", () => {
  it("is true for a parse with only context, or no hunks at all", () => {
    expect(isUnchanged(diff([]))).toBe(true);
    expect(isUnchanged(diff([hunk([context(3)])]))).toBe(true);
  });
  it("is false the moment any block adds or removes a line", () => {
    expect(isUnchanged(diff([hunk([context(3), change(0, 1)])]))).toBe(false);
    expect(isUnchanged(diff([hunk([change(1, 0)])]))).toBe(false);
  });
});

describe("where a keyboard-opened comment lands", () => {
  const d = diff([hunk([change(1, 0)], { start: 20, count: 7 }), hunk([change(1, 1)], { start: 50, count: 5 })]);
  it("opens on the first hunk's first line", () => {
    expect(firstChangedLine(d)).toBe(20);
    expect(firstChangedLine(diff([]))).toBe(1);
  });
  it("knows which lines are on screen without unfolding", () => {
    expect(lineInHunks(d, 20)).toBe(true);
    expect(lineInHunks(d, 26)).toBe(true);
    expect(lineInHunks(d, 27)).toBe(false);
    expect(lineInHunks(d, 1)).toBe(false);
    expect(lineInHunks(d, 52)).toBe(true);
  });
});
