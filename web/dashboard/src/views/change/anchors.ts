/// `#path:L<n>` — the address of a line in a change's diff.
///
/// What a line comment hands somebody, and what the review page opens
/// on arrival. Kept apart from the diff renderer because the renderer
/// has changed underneath it and this has not: the URL scheme is a
/// contract with every link already sent.

/** The anchor a line comment points at: `path:L<n>`, on the new side.
 *
 *  Deleted lines have no new number and so cannot be linked — the same
 *  rule the line-comment gutter already follows, since a comment on a
 *  line that is gone has nowhere to sit at the next patchset. */
export function lineAnchor(path: string, line: number): string {
  return `#${path}:L${line}`;
}

/** Read `#path:L<n>` back. Returns null for anything else, including an
 *  empty hash and a path that happens to contain a colon but no line —
 *  a half-parsed anchor that scrolled to the wrong file would be worse
 *  than none. */
export function parseLineAnchor(
  hash: string,
): { path: string; line: number } | null {
  const raw = hash.startsWith("#") ? hash.slice(1) : hash;
  if (!raw) return null;
  const decoded = (() => {
    try {
      return decodeURIComponent(raw);
    } catch {
      // A hash with a stray `%` is not ours; take it verbatim and let
      // the match below refuse it.
      return raw;
    }
  })();
  const at = decoded.lastIndexOf(":L");
  if (at <= 0) return null;
  const line = decoded.slice(at + 2);
  if (!/^[1-9][0-9]*$/.test(line)) return null;
  return { path: decoded.slice(0, at), line: Number(line) };
}
