import { useEffect, useState } from "react";
import { api, type Repo, type RepoMetrics, type Session } from "@/api";
import { ErrorBox, Loading } from "@/components/feedback";
import { KindRow } from "@/components/kind-row";
import { NotFound } from "@/components/not-found";
import { StatTile } from "@/components/stat-tile";
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
import { formatBytes, formatCount, formatMs } from "@/format";

/// What the repository is actually doing: traffic, latency and how
/// fresh a mirror is.
///
/// This is the last surface that lived only on the dashboard's own repo
/// screen, at an address nobody could send. The numbers, the per-kind
/// table and the CSV export moved here unchanged; what changed is that
/// they are now on the repository's one page, at
/// `/{owner}/{repo}/insights`, beside the code they are about.
///
/// **Members only, and the route agrees.** A repository's traffic — how
/// often it is cloned, how many bytes it serves, how far behind its
/// origin a mirror runs — belongs to the people who own it.
///
/// The gate here is `viewer_member`, and `GET …/repos/{repo}/metrics`
/// makes the *same* `authx::require` call to decide the same thing
/// (`crates/stratum-server/src/api/metrics_api.rs`). A gate only this
/// file enforced would have been a curtain over an open window.
///
/// Members, not admins: somebody holding the `viewer` role is on the
/// inside and may read the numbers without being able to change
/// anything. That is a weaker bar than the Settings tab's on purpose.
///
/// The refusal is `NotFound` and not "forbidden", matching
/// [`super::settings::RepoSettingsView`]: telling somebody they may not
/// see a page tells them there is a page to come back for.
export function RepoInsightsView(props: {
  session: Session;
  owner: string;
  repo: string;
  /// The repository row the screen already holds, so this page does not
  /// ask for it a second time. `null` until it arrives.
  row: Repo | null;
  /// `viewer_member` off that row, `null` until it arrives: whether the
  /// caller holds a role here at all. The server's own answer, handed down, so
  /// this page and the route behind it cannot come to different
  /// conclusions about who is looking.
  member: boolean | null;
  navigate: (to: string, replace?: boolean) => void;
}) {
  const { session, owner, repo } = props;
  const [m, setM] = useState<RepoMetrics | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [csvBusy, setCsvBusy] = useState(false);
  const [csvError, setCsvError] = useState<string | null>(null);

  // Only once the viewer is known to be a member. Firing the read while
  // `member` is still null would spend a request on every visitor to a
  // page most of them are about to be told does not exist, and would put
  // a refusal in the console of anybody who typed the URL.
  const mayRead = props.member === true;

  useEffect(() => {
    if (!mayRead) return;
    let alive = true;
    api
      .metrics(session, repo)
      .then((x) => alive && setM(x))
      .catch((e) => alive && setError(String(e)));
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mayRead, session.org, session.token, repo]);

  async function downloadCsv() {
    setCsvBusy(true);
    setCsvError(null);
    try {
      const blob = await api.metricsCsv(session, repo);
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `${repo}-metrics.csv`;
      document.body.appendChild(a);
      a.click();
      a.remove();
      URL.revokeObjectURL(url);
    } catch (e) {
      setCsvError(
        `Could not export CSV: ${e instanceof Error ? e.message : e}`,
      );
    } finally {
      setCsvBusy(false);
    }
  }

  if (props.member === null || !props.row) return <Loading />;
  if (!props.member)
    return <NotFound onNavigate={props.navigate} what={`${owner}/${repo}`} />;
  if (error) return <ErrorBox message={error} />;
  if (!m) return <Loading />;

  const info = props.row;
  // "1 clones (24h)" is what this said for three years on the screen
  // this page replaced. A count of one is the commonest interesting
  // case on a quiet repository, and it is the one the plural gets
  // wrong — invisible to an assertion looking for the substring, and
  // the first thing a person reads.
  const times = (n: number, one: string, many: string) =>
    `${formatCount(n)} ${n === 1 ? one : many} (24h)`;
  const clone = m.kinds["clone"];
  const fetch_ = m.kinds["fetch"];
  const freshness = m.kinds["freshness"];
  const totalRequests = Object.values(m.kinds).reduce((a, k) => a + k.count, 0);
  const totalBytes = Object.values(m.kinds).reduce((a, k) => a + k.bytes, 0);

  return (
    <div className="space-y-6 py-6">
      <div className="flex items-center gap-3 border-b border-borderline pb-3">
        <h1 className="text-xl font-semibold text-ink">Insights</h1>
        <Button
          type="button"
          variant="outline"
          className="ml-auto"
          disabled={csvBusy}
          onClick={downloadCsv}
        >
          {csvBusy ? "Exporting…" : "Export CSV"}
        </Button>
      </div>
      {csvError && (
        <p className="text-sm text-serious" role="alert">
          {csvError}
        </p>
      )}

      <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
        <StatTile
          label="Clone p50 / p99"
          value={`${formatMs(clone?.p50_ms ?? null)} / ${formatMs(clone?.p99_ms ?? null)}`}
          sub={times(clone?.count ?? 0, "clone", "clones")}
        />
        <StatTile
          label="Requests absorbed"
          value={formatCount(totalRequests)}
          sub="reads your origin never saw"
        />
        <StatTile label="Bytes served" value={formatBytes(totalBytes)} />
        {info.kind === "mirror" ? (
          <StatTile
            label="Freshness lag p50"
            value={formatMs(freshness?.p50_ms ?? null)}
            sub="webhook → servable"
          />
        ) : (
          <StatTile
            label="Fetch p50 / p99"
            value={`${formatMs(fetch_?.p50_ms ?? null)} / ${formatMs(fetch_?.p99_ms ?? null)}`}
            sub={times(fetch_?.count ?? 0, "fetch", "fetches")}
          />
        )}
        {/* What this repository is holding, which is the other half of
            what an owner comes to this page to find out: the tiles above
            are what it *moved*. Rendered only when the row carries it —
            the field is optional on the wire. */}
        {info.stored_bytes != null && (
          <StatTile label="Stored" value={formatBytes(info.stored_bytes)} />
        )}
      </div>

      {m.sync.sync_error && (
        <div className="rounded-lg border border-borderline bg-surface-1 p-4 text-sm">
          <div className="mb-1 flex items-center gap-2 font-medium">
            <span aria-hidden className="text-serious">
              ▲
            </span>
            Origin unreachable — serving last-known state (staleness headers
            active)
          </div>
          <div className="font-mono text-xs text-ink-2">
            {m.sync.sync_error}
          </div>
        </div>
      )}

      <div className="overflow-hidden rounded-lg border border-borderline bg-surface-1">
        <Table>
          <TableHeader>
            <TableHeadRow>
              <TableHead className="px-3 py-2.5">Kind</TableHead>
              <TableHead className="px-3 py-2.5 text-right">Count</TableHead>
              <TableHead className="px-3 py-2.5 text-right">Bytes</TableHead>
              <TableHead className="px-3 py-2.5 text-right">p50</TableHead>
              <TableHead className="px-3 py-2.5 text-right">p99</TableHead>
            </TableHeadRow>
          </TableHeader>
          <TableBody>
            {Object.entries(m.kinds).map(([kind, s]) => (
              <KindRow key={kind} kind={kind} s={s} />
            ))}
            {Object.keys(m.kinds).length === 0 && (
              <TableRow className="border-t-0">
                <TableCell
                  colSpan={5}
                  className="px-3 py-6 text-center text-sm text-ink-3"
                >
                  No traffic in the last 24 hours.
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </div>
      <p className="text-xs text-ink-3">
        {fetch_
          ? `${formatCount(fetch_.count)} incremental fetches served. `
          : ""}
        Window: last 24h. Percentiles from per-minute latency histograms.
      </p>
    </div>
  );
}
