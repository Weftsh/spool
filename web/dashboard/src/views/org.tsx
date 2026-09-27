import { useEffect, useState } from "react";
import {
  api,
  isUnauthenticated,
  type Repo,
  type Session,
  type Usage,
} from "@/api";
import { Bars } from "@/components/bars";
import { Err, ErrorBox, Loading } from "@/components/feedback";
import { StatTile } from "@/components/stat-tile";
import { SyncBadge } from "@/components/sync-badge";
import { Badge } from "@/components/ui/badge";
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
import { href } from "@/router";
import { STRUCTURAL_LINK } from "@/lib/links";
import { formatPoolBytes } from "@/lib/meter";
import { formatAgo, formatBytes, formatCount } from "@/format";

/// What creating an organization comes back with: its name, and the
/// server's own sentence about what it can hold.
export interface CreatedOrg {
  name: string;
  detail: string;
}

/// Making an organization.
///
/// Says up front what it costs and that a personal namespace costs
/// nothing — somebody who only wants somewhere to put a repository
/// should be told they already have one, before they name a company.
/// No card is asked for: the provider meets one on its subscription
/// page, the first time something here would cost money.
export function NewOrg(props: {
  onCreated: (org: CreatedOrg) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  return (
    <form
      className="mb-6 rounded-lg border border-borderline bg-surface-1 p-4"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        setError(null);
        try {
          const org = await api.createOrg(name.trim());
          props.onCreated({ name: org.name, detail: org.detail });
        } catch (err) {
          setError(
            `Could not create that organization: ${
              err instanceof Error ? err.message : err
            }`,
          );
        } finally {
          setBusy(false);
        }
      }}
    >
      <div className="mb-1 text-sm font-medium">New organization</div>
      <p className="mb-3 text-xs text-ink-3">
        An organization has members, teams and per-repo access. Public
        repositories and members are free; private repositories are billed per
        seat. Your own namespace is free and already exists — if you just want
        somewhere to put a repository, you have one.
      </p>
      <div className="flex flex-wrap gap-2">
        <input
          className="min-w-0 flex-1 rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 font-mono text-sm"
          aria-label="Organization name"
          placeholder="acme"
          value={name}
          onChange={(e) => setName(e.target.value)}
          autoFocus
          required
        />
        <Button disabled={busy}>
          {busy ? "Creating…" : "Create organization"}
        </Button>
        <Button type="button" variant="outline" onClick={props.onCancel}>
          Cancel
        </Button>
      </div>
      <Err message={error} />
      <p className="mt-2 text-xs text-ink-3">
        No card is needed. The first private repository takes you to the
        payment provider, where the card and the price are on one page.
      </p>
    </form>
  );
}

export function OrgView(props: {
  session: Session;
  onAuthFailure: () => void;
  navigate: (to: string) => void;
  /// Where a repository in this list actually lives — `/{owner}/{name}`,
  /// the one address it has — as both an `href` to put on the anchor and
  /// an `open` to run when it is clicked.
  ///
  /// This used to be `onOpenRepo(name)`, which set a `useState` in
  /// `App` and rendered a repo screen at no address at all. A row you
  /// cannot copy the link of is the reason this table existed and the
  /// reason nobody could share what it opened. Handing down the address
  /// rather than a callback is what makes the row an anchor, so
  /// middle-click, copy-link and a crawler all behave.
  repoLink: (name: string) => { href: string; open: () => void };
}) {
  const { session } = props;
  const [repos, setRepos] = useState<Repo[] | null>(null);
  const [usage, setUsage] = useState<Usage | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    Promise.all([api.repos(session), api.usage(session)])
      .then(([r, u]) => {
        if (!alive) return;
        setRepos(r);
        setUsage(u);
      })
      .catch((e) => {
        if (!alive) return;
        if (isUnauthenticated(e)) props.onAuthFailure();
        else setError(String(e));
      });
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token]);

  if (error) return <ErrorBox message={error} />;
  if (!repos || !usage) return <Loading />;

  const today = usage.days[0];
  const incidents = repos.filter((r) => r.sync_error);
  const oldestFirst = [...usage.days].reverse();
  const usageBars = oldestFirst.map((d) => ({
    label: d.day.slice(5),
    value: d.requests,
    detail: formatBytes(d.bytes_out),
  }));
  // The metered figures are absent from a server older than usage
  // billing, and absent is "—", never 0: a zero would claim nothing
  // ran and nothing is stored. A chart is drawn only when some day
  // carries the field, for the same reason — a flat line of zeros is
  // a claim, an absent chart is not.
  const has = (k: "hosted_minutes" | "private_bytes_stored") =>
    usage.days.some((d) => d[k] !== undefined && d[k] !== null);
  const minuteBars = has("hosted_minutes")
    ? oldestFirst.map((d) => ({
        label: d.day.slice(5),
        value: d.hosted_minutes ?? 0,
      }))
    : null;
  const storageBars = has("private_bytes_stored")
    ? oldestFirst.map((d) => ({
        label: d.day.slice(5),
        value: d.private_bytes_stored ?? 0,
      }))
    : null;
  const metered = (v: number | null | undefined, f: (n: number) => string) =>
    v === undefined || v === null ? "—" : f(v);

  return (
    <div className="space-y-6">
      <div className="grid grid-cols-2 gap-3 md:grid-cols-3 lg:grid-cols-6">
        <StatTile
          twoLineLabel
          label="Repos"
          value={formatCount(repos.length)}
          sub={`plan: ${usage.plan}`}
        />
        <StatTile
          twoLineLabel
          label="Active today"
          value={today ? formatCount(today.active_repos) : "0"}
          sub="dormant repos are free"
        />
        <StatTile
          twoLineLabel
          label="Requests today"
          value={today ? formatCount(today.requests) : "0"}
        />
        <StatTile
          twoLineLabel
          label="Private transfer today"
          value={metered(today?.private_bytes_out, formatPoolBytes)}
          sub="public traffic is never counted"
        />
        <StatTile
          twoLineLabel
          label="Hosted minutes today"
          value={metered(today?.hosted_minutes, formatCount)}
        />
        <StatTile
          twoLineLabel
          label="Stored, private"
          value={metered(today?.private_bytes_stored, formatPoolBytes)}
        />
      </div>

      {incidents.length > 0 && (
        <div className="rounded-lg border border-borderline bg-surface-1 p-4">
          <div className="mb-2 flex items-center gap-2 text-sm font-medium">
            <span aria-hidden className="text-serious">
              ▲
            </span>
            Origin incident mode — {incidents.length} mirror
            {incidents.length > 1 ? "s" : ""} serving last-known state
          </div>
          <ul className="space-y-1 text-sm text-ink-2">
            {incidents.map((r) => (
              <li key={r.id} className="font-mono text-xs">
                {r.name}: {r.sync_error}
              </li>
            ))}
          </ul>
        </div>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <h2 className="text-sm font-medium">Repositories</h2>
        <Button
          className="ml-auto"
          onClick={() => props.navigate(href(["new"]))}
        >
          New repository
        </Button>
      </div>

      {/* An org with nothing in it used to be an empty table with column
          headings — a dead end at exactly the moment somebody is
          deciding whether this product does anything. */}
      {repos.length === 0 ? (
        <div className="rounded-lg border border-borderline bg-surface-1 p-6 text-center">
          <p className="text-sm font-medium">No repositories yet</p>
          <p className="mx-auto mt-1 max-w-md text-sm text-ink-3">
            Mirror one from GitHub by pasting its URL — public origins need no
            credentials — or start an empty one and push to it.
          </p>
          <Button
            className="mt-3"
            onClick={() => props.navigate(href(["new"]))}
          >
            Create your first repository
          </Button>
        </div>
      ) : (
        <div className="overflow-hidden rounded-lg border border-borderline bg-surface-1">
          <Table>
            <TableHeader>
              <TableHeadRow>
                <TableHead className="px-3 py-2.5">Repo</TableHead>
                <TableHead className="px-3 py-2.5">Visibility</TableHead>
                <TableHead className="px-3 py-2.5">Kind</TableHead>
                <TableHead className="px-3 py-2.5">Status</TableHead>
                <TableHead className="px-3 py-2.5">
                  Last sync / created
                </TableHead>
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {repos.map((r) => {
                const link = props.repoLink(r.name);
                return (
                <TableRow key={r.id} className="hover:bg-surface-2">
                  <TableCell className="max-w-[24rem] px-3">
                    {/* A real anchor, intercepted — the same shape
                        `RepoCard` and `TabStrip` use. The row used to
                        carry an `onClick` and nothing else, so there was
                        no URL to copy, no middle-click, and nothing for
                        a keyboard to reach. */}
                    <a
                      href={link.href}
                      onClick={(e) => {
                        e.preventDefault();
                        link.open();
                      }}
                      className={`font-medium ${STRUCTURAL_LINK}`}
                    >
                      {r.name}
                    </a>
                    {r.description && (
                      // Its own block, and truncated: a description run
                      // into the name reads as one string, and one long
                      // enough to widen the cell pushes every column
                      // after it off the screen.
                      <span
                        className="block truncate text-xs text-ink-3"
                        title={r.description}
                      >
                        {r.description}
                      </span>
                    )}
                    {r.fork_parent && (
                      // A fork carries somebody else's history, and the
                      // forge page says so under the name; this table
                      // did not, so an organization's fork of
                      // `acme/widget` sat beside its own repositories as
                      // if it were one. Found forking into a second
                      // organization and reading its repository list.
                      <span className="block truncate text-xs text-ink-3">
                        Forked from {r.fork_parent}
                      </span>
                    )}
                  </TableCell>
                  <TableCell className="px-3">
                    {/* The one fact about a repository that decides who
                        can see it and what it costs, and the table did
                        not show it: a lapsed organization's private
                        repositories sat beside its public ones with
                        nothing to tell them apart, on the page whose
                        job is deciding which to open and flip. The
                        toggle itself is on the repository's page
                        ("Make public" / "Make private"); this is where
                        you see which ones need it. A private one that
                        cannot be written to says so here, in the words
                        the push would be refused with. */}
                    <Badge
                      variant="neutral"
                      title={r.write_blocked?.replace(/^quota:\s*/, "")}
                    >
                      {r.public ? "Public" : "Private"}
                      {r.write_blocked ? " · read-only" : ""}
                    </Badge>
                  </TableCell>
                  <TableCell className="px-3 text-ink-2">{r.kind}</TableCell>
                  <TableCell className="px-3">
                    {r.kind === "mirror" ? (
                      <SyncBadge
                        error={r.sync_error}
                        lastSync={r.last_sync_at}
                      />
                    ) : (
                      <span className="text-ink-3">—</span>
                    )}
                  </TableCell>
                  <TableCell className="px-3 text-ink-2">
                    {r.kind === "mirror"
                      ? formatAgo(r.last_sync_at)
                      : formatAgo(r.created_at)}
                  </TableCell>
                </TableRow>
                );
              })}
            </TableBody>
          </Table>
        </div>
      )}

      {/* Usage last, after the repositories. This screen is the one the
          sidebar calls "Repositories", and three full-width charts used
          to sit above the table: on a laptop the list you came for began
          a screen and a half down. The tiles above still carry today's
          numbers; the history is here for whoever scrolls for it. */}
      {(usageBars.length > 0 || minuteBars || storageBars) && (
        <h2 className="pt-2 text-sm font-medium">Usage</h2>
      )}
      {usageBars.length > 0 && (
        <Bars title="Requests per day" bars={usageBars} format={formatCount} />
      )}
      {minuteBars && (
        <Bars
          title="Hosted minutes per day"
          bars={minuteBars}
          format={formatCount}
        />
      )}
      {storageBars && (
        <Bars
          title="Private storage per day"
          bars={storageBars}
          format={formatPoolBytes}
        />
      )}

    </div>
  );
}
