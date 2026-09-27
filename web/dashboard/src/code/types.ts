/// Type-only re-exports from the Pierre libraries.
///
/// Main-chunk code needs the *shapes* — a parsed diff, an annotation, a
/// git-status entry — without pulling the libraries in; `import type` is
/// erased by tsc, so this file costs nothing at runtime and keeps every
/// value import behind `surface.tsx` where `fence.test.ts` can see it.
export type {
  DiffLineAnnotation,
  FileDiffMetadata,
  Hunk,
  SelectedLineRange,
} from "@pierre/diffs";
export type { FileDiffOptions } from "@pierre/diffs/react";
export type {
  FileTreeRowDecorationRenderer,
  GitStatus,
  GitStatusEntry,
} from "@pierre/trees";
