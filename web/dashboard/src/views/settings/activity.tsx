import { useCallback, useEffect, useState } from "react";
import { api, type AuditEntry, type Session } from "@/api";
import { Err, Loading } from "@/components/feedback";
import { Panel } from "@/components/panel";
import { Button } from "@/components/ui/button";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableHeadRow,
  TableRow,
} from "@/components/ui/table";
import { usePeople } from "@/lib/use-people";
import { formatAgo } from "@/format";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

/// Everything that has happened in this org, and who did it.
///
/// The filters are the questions an incident actually asks — what did
/// this person do, what happened to this repo, when did this start — and
/// the export is for the ticket that comes after.
///
/// Newest first, and "Load more" walks backwards. The stored order is the
/// other way round — the shipper reads the table forwards from a
/// watermark — so this asks for `order=desc` explicitly; the default
/// would open on the hundred oldest events the org ever recorded.
export function ActivityPanel(props: { session: Session; isAdmin: boolean }) {
  const { session } = props;
  const people = usePeople(session, props.isAdmin);
  // Entries carry a repo *id*; the filter takes a repo *name*. The repo
  // list is the only thing that knows both, so it drives the filter and
  // the column together — and a trail row naming an opaque ULID would be
  // no answer to "what happened to this repo".
  const [repos, setRepos] = useState<Record<string, string>>({});
  const [entries, setEntries] = useState<AuditEntry[] | null>(null);
  const [older, setOlder] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [user, setUser] = useState("");
  const [repo, setRepo] = useState("");
  const [action, setAction] = useState("");
  const [since, setSince] = useState("");

  const filters = useCallback((): Record<string, string> => {
    const f: Record<string, string> = { limit: "100", order: "desc" };
    if (user) f.user = user;
    if (repo.trim()) f.repo = repo.trim();
    if (action.trim()) f.action = action.trim();
    // A date input always gives YYYY-MM-DD whatever the browser shows;
    // the API takes epoch ms. Midnight *local*, not UTC — someone
    // picking "the 4th" means the 4th where they are, and reading it as
    // UTC would silently drop that morning and add the previous evening.
    if (since) {
      const t = new Date(`${since}T00:00:00`).getTime();
      if (!Number.isNaN(t)) f.since = String(t);
    }
    return f;
  }, [user, repo, action, since]);

  const load = useCallback(
    (before?: number) => {
      setError(null);
      const f = filters();
      if (before != null) f.before = String(before);
      api
        .audit(session, f)
        .then((out) => {
          setEntries((cur) =>
            before == null ? out.entries : [...(cur ?? []), ...out.entries],
          );
          // A short page is the last page. Trusting the cursor alone
          // leaves "Load more" showing forever on an exhausted feed.
          setOlder(out.entries.length < 100 ? null : out.next_before);
        })
        .catch((e) => setError(String(e)));
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [session.org, session.token, filters],
  );

  useEffect(() => {
    load();
  }, [load]);

  useEffect(() => {
    let live = true;
    api
      .repos(session)
      .then((rs) => {
        if (live) setRepos(Object.fromEntries(rs.map((r) => [r.id, r.name])));
      })
      // A trail is still readable without repo names, so this failing is
      // not worth an error banner over the rows themselves.
      .catch(() => {});
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token]);

  async function exportCsv() {
    setBusy(true);
    setError(null);
    try {
      const blob = await api.auditCsv(session, filters());
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `${session.org}-activity.csv`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      setError(`Could not export: ${err instanceof Error ? err.message : err}`);
    } finally {
      setBusy(false);
    }
  }

  const who = (e: AuditEntry) =>
    e.user_email ??
    (e.user_id ? (people[e.user_id] ?? "a person") : e.principal);

  return (
    <Panel
      title="Activity"
      hint="Every change, and who made it. A credential scoped to one repo sees only that repo's activity."
    >
      <Err message={error} />
      <div className="mb-3 flex flex-wrap items-end gap-2">
        <label className="flex flex-col gap-1 text-xs text-ink-3">
          Person
          <Select
            value={user || "all"}
            onValueChange={(v) => setUser(v === "all" ? "" : v)}
          >
            <SelectTrigger aria-label="Filter by person">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">anyone</SelectItem>
              {Object.entries(people).map(([id, email]) => (
                <SelectItem key={id} value={id}>
                  {email}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </label>
        <label className="flex flex-col gap-1 text-xs text-ink-3">
          Repo
          <Select
            value={repo || "all"}
            onValueChange={(v) => setRepo(v === "all" ? "" : v)}
          >
            <SelectTrigger aria-label="Filter by repo">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">any</SelectItem>
              {Object.entries(repos).map(([id, name]) => (
                <SelectItem key={id} value={name}>
                  {name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </label>
        <label className="flex flex-col gap-1 text-xs text-ink-3">
          Action
          <input
            className="rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 text-sm"
            aria-label="Filter by action"
            placeholder="any"
            value={action}
            onChange={(e) => setAction(e.target.value)}
          />
        </label>
        <label className="flex flex-col gap-1 text-xs text-ink-3">
          Since
          <input
            type="date"
            className="rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 text-sm"
            aria-label="Filter from date"
            value={since}
            onChange={(e) => setSince(e.target.value)}
          />
        </label>
        <Button variant="outline" onClick={exportCsv} disabled={busy}>
          {busy ? "Exporting…" : "Export CSV"}
        </Button>
      </div>
      {!entries ? (
        <Loading />
      ) : entries.length === 0 ? (
        <p className="text-sm text-ink-3">Nothing matches those filters.</p>
      ) : (
        <div className="overflow-x-auto">
          <Table>
            <TableHeader>
              <TableHeadRow>
                <TableHead>When</TableHead>
                <TableHead>Who</TableHead>
                <TableHead>Action</TableHead>
                <TableHead>Repo</TableHead>
                <TableHead>Detail</TableHead>
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {entries.map((e) => (
                <TableRow key={e.seq}>
                  <TableCell className="whitespace-nowrap text-ink-2">
                    {formatAgo(e.at)}
                  </TableCell>
                  {/* `max-width` on a `td` is advisory — the table just
                      gets wider. Truncation has to happen on a block
                      inside the cell, and the full value stays reachable
                      as a title: a trail that hides half a principal is
                      not much of a trail. */}
                  <TableCell>
                    <div className="max-w-[14rem] truncate" title={who(e)}>
                      {who(e)}
                    </div>
                  </TableCell>
                  <TableCell className="font-mono text-xs">
                    {e.action}
                  </TableCell>
                  <TableCell className="whitespace-nowrap text-ink-2">
                    {e.repo_id ? (repos[e.repo_id] ?? "a deleted repo") : "—"}
                  </TableCell>
                  {/* The last column absorbs whatever width is left and
                      truncates to it, rather than to a guessed number of
                      rem that pushes the table past its panel: `max-w-0`
                      collapses the cell to its minimum under auto layout
                      and `w-full` then hands it the remainder. */}
                  <TableCell
                    className="w-full max-w-0 truncate font-mono text-xs text-ink-3"
                    title={e.context ? JSON.stringify(e.context) : undefined}
                  >
                    {e.context ? JSON.stringify(e.context) : "—"}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      )}
      {older != null && (
        <button
          className="mt-3 text-xs text-ink-3 underline-offset-2 hover:text-ink-2 hover:underline"
          onClick={() => load(older)}
        >
          Load more
        </button>
      )}
    </Panel>
  );
}
