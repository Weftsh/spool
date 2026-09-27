/// The lazy edge of the code surfaces. No `@pierre/*` import here: one
/// dynamic import of `./surface` is what gives Vite a single chunk to
/// split, and `React.lazy` is what keeps that chunk off every page that
/// shows no code. See `surface.tsx` for why.

import { lazy, Suspense, type ReactNode } from "react";
import { Loading } from "@/components/feedback";
import type { CodeDiffProps } from "./surface";

const surface = () => import("./surface");

export const LazyHighlightedFile = lazy(() =>
  surface().then((m) => ({ default: m.HighlightedFile })),
);
export const LazyRepoTree = lazy(() =>
  surface().then((m) => ({ default: m.RepoTree })),
);
export const LazyCodeTree = lazy(() =>
  surface().then((m) => ({ default: m.CodeTree })),
);
// `lazy` cannot carry a generic through, so the annotation metadata type
// is restored on the way out.
export const LazyCodeDiff = lazy(() =>
  surface().then((m) => ({ default: m.CodeDiff })),
) as unknown as <M = undefined>(props: CodeDiffProps<M>) => ReactNode;

/// Parse on demand; the parser is in the lazy chunk with the renderer.
export async function parseDiff(
  ...args: Parameters<typeof import("./surface").parseDiff>
) {
  return (await surface()).parseDiff(...args);
}

export function CodeBoundary(props: {
  children: ReactNode;
  fallback?: ReactNode;
}) {
  return (
    <Suspense fallback={props.fallback ?? <Loading />}>{props.children}</Suspense>
  );
}
