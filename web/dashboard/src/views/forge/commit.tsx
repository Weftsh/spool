import { useEffect, useState } from "react";
import {
  api,
  parseIdent,
  type DiffEntry,
  type LogEntry,
  type Session,
} from "@/api";
import { ErrorBox, Loading } from "@/components/feedback";
import { formatAgo } from "@/format";
import { authorLabel } from "@/views/browse";
import { CommitChecks } from "@/views/forge/commit-checks";

/// One commit: what it says, who wrote it, and what it changed.
///
/// A forge where you cannot click a commit is a forge people leave in
/// order to read the history locally — and four places on this site
/// already print an abbreviated sha with nowhere to go. This is where
/// they go.
///
/// The diff is structural: which paths changed and how. Line-level
/// content is the review surface's job and lives on the change page,
/// which has the machinery for it; duplicating that here would be a
/// second renderer of the same thing, and the two would drift.
export function CommitView(props: {
  session: Session;
  repo: string;
  sha: string;
  /// Passed down only so the checks strip can navigate in-app when a
  /// row's `detail_url` is one of ours — a hosted run's page — instead
  /// of opening a new tab onto our own SPA.
  navigate: (to: string, replace?: boolean) => void;
}) {
  const { session, repo, sha } = props;
  const [head, setHead] = useState<LogEntry | null>(null);
  const [changes, setChanges] = useState<DiffEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    setHead(null);
    setChanges(null);
    setError(null);
    api
      .log(session, repo, { rev: sha, limit: 1 })
      .then(async (out) => {
        const entry = out.entries?.[0];
        if (!alive) return;
        if (!entry) {
          setError("No such commit in this repository.");
          return;
        }
        setHead(entry);
        // A root commit has no parent, and diffing against nothing is
        // the honest way to say "everything here is new".
        const from = entry.parents[0] ?? "";
        // `?? []` and not just the catch: a **200 with the wrong shape**
        // is not a rejection, and it put `undefined` where a list goes.
        // `changes.length` then threw during render and the page went
        // white — message, author and checks strip and all — for a fault
        // in the file list, which is the least of what a commit page is
        // for. A failed diff read already degrades to an empty list, so
        // a malformed one degrades the same way rather than taking the
        // rest of the page down.
        const diff = await api
          .structuralDiff(session, repo, from, entry.commit)
          .catch(() => [] as DiffEntry[]);
        if (alive) setChanges(diff ?? []);
      })
      .catch((e) => alive && setError(String(e?.message ?? e)));
    return () => {
      alive = false;
    };
  }, [session, repo, sha]);

  if (error) return <ErrorBox message={error} />;
  if (!head) return <Loading />;

  const who = parseIdent(head.author);
  const label = authorLabel(who.name);
  const [subject, ...body] = head.message.split("\n");

  return (
    <div className="py-6">
      <div className="rounded-lg border border-borderline bg-surface-1 p-5">
        <h1 className="text-lg font-semibold text-ink">{subject}</h1>
        {body.join("\n").trim() && (
          <pre className="mt-3 overflow-x-auto rounded bg-surface-2 p-3 font-mono text-xs text-ink-2">
            {body.join("\n").trim()}
          </pre>
        )}
        <div className="mt-4 flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-ink-3">
          <span
            className={label.machine ? "text-ink-3" : "text-ink-2"}
            title={who.name}
          >
            {label.label}
          </span>{" "}
          {who.time > 0 && <span>committed {formatAgo(who.time)}</span>}{" "}
          <code className="font-mono text-xs">{head.commit.slice(0, 12)}</code>
          {head.parents.length > 1 && (
            <span className="text-xs">
              a merge of {head.parents.length} parents
            </span>
          )}
        </div>
      </div>

      {/* What CI said about this commit. Directly under the header
          because "did this build" is the second question a reader has
          after "what does it say", and above the file list because a red
          build changes how the diff below is read. */}
      <CommitChecks
        session={session}
        repo={repo}
        sha={head.commit}
        navigate={props.navigate}
      />

      <h2 className="mt-6 mb-2 text-sm font-medium text-ink">
        {changes === null
          ? "Files"
          : `${changes.length} ${changes.length === 1 ? "file" : "files"} changed`}
      </h2>
      {changes === null && <Loading />}
      {changes?.length === 0 && (
        <p className="text-sm text-ink-3">
          This commit changed no files — it may be an empty commit or a merge
          that took nothing from either side.
        </p>
      )}
      {changes && changes.length > 0 && (
        <ul className="overflow-hidden rounded-lg border border-borderline">
          {changes.map((c) => (
            <li
              key={c.path}
              className="flex items-center gap-3 border-b border-borderline/60 px-4 py-2 text-sm last:border-0"
            >
              <span className="w-16 shrink-0 text-xs text-ink-3">
                {c.status}
              </span>{" "}
              <span className="min-w-0 flex-1 truncate font-mono text-xs text-ink-2">
                {c.path}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
