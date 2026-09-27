import { useCallback, useEffect, useState } from "react";
import {
  api,
  type AccessRow,
  type Role,
  type Session,
  type Team,
  type TeamAccessRow,
} from "@/api";
import { Err, Loading } from "@/components/feedback";
import { Panel } from "@/components/panel";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableHeadRow,
  TableRow,
} from "@/components/ui/table";

/// Who can reach this repo, and why.
///
/// Three rules decide it — the org role, a grant naming the person, and a
/// grant to a team they are in — and they interact. Without this screen
/// the only way to find out which one applied is to try something and see
/// whether it works, so every row says where its access came from.
///
/// Renders nothing at all for a caller who may not read it: the endpoint
/// is `org:admin`, and a member seeing a permanent error where a panel
/// should be would read as something broken.
export function AccessPanel(props: { session: Session; repo: string }) {
  const { session, repo } = props;
  const [data, setData] = useState<{
    people: AccessRow[];
    teams: TeamAccessRow[];
  } | null>(null);
  const [allowed, setAllowed] = useState(true);
  const [teams, setTeams] = useState<Team[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [subject, setSubject] = useState("");
  const [role, setRole] = useState<Role>("member");

  const refresh = useCallback(() => {
    api
      .access(session, repo)
      .then((d) => {
        setData(d);
        setAllowed(true);
      })
      .catch(() => setAllowed(false));
    api
      .teams(session)
      .then(setTeams)
      .catch(() => setTeams([]));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token, repo]);

  useEffect(refresh, [refresh]);

  if (!allowed) return null;

  async function act(what: () => Promise<unknown>, verb: string) {
    setError(null);
    setBusy(true);
    try {
      await what();
      refresh();
    } catch (err) {
      setError(
        `Could not ${verb}: ${err instanceof Error ? err.message : err}`,
      );
    } finally {
      setBusy(false);
    }
  }

  // One control for both kinds of subject, prefixed so a team and a
  // person can never be confused for one another in the same list.
  const options = [
    ...teams.map((t) => ({ value: `team:${t.id}`, label: `${t.name} (team)` })),
    ...(data?.people ?? []).map((p) => ({
      value: `user:${p.user_id}`,
      label: p.email,
    })),
  ];

  function grant() {
    const [kind, id] = subject.split(":");
    return act(async () => {
      await api.grant(
        session,
        repo,
        kind === "team" ? { team_id: id } : { user_ids: [id] },
        role,
      );
      setSubject("");
    }, "grant that");
  }

  const SOURCE: Record<AccessRow["source"], string> = {
    org_role: "org role",
    direct_grant: "granted here",
    team: "team",
  };

  return (
    <Panel
      title="Access"
      hint="A grant naming a person replaces their org role on this repo, up or down. A team grant only ever raises."
    >
      <Err message={error} />
      {!data ? (
        <Loading />
      ) : (
        <div className="overflow-x-auto">
          <Table>
            <TableHeader>
              <TableHeadRow>
                <TableHead>Who</TableHead>
                <TableHead>Role here</TableHead>
                <TableHead>From</TableHead>
                <TableHead />
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {data.teams.map((t) => (
                <TableRow key={`team-${t.team_id}`}>
                  <TableCell>
                    {t.team_name}{" "}
                    <span className="text-xs text-ink-3">
                      team · {t.member_count}{" "}
                      {t.member_count === 1 ? "person" : "people"}
                    </span>
                  </TableCell>
                  <TableCell className="text-ink-2">{t.role}</TableCell>
                  <TableCell className="text-xs text-ink-3">
                    granted here
                  </TableCell>
                  <TableCell className="text-right">
                    <Button
                      variant="ghost"
                      size="xs"
                      className="hover:text-serious"
                      onClick={() =>
                        act(
                          () => api.revokeTeamGrant(session, repo, t.team_id),
                          "withdraw that grant",
                        )
                      }
                      disabled={busy}
                    >
                      Revoke
                    </Button>
                  </TableCell>
                </TableRow>
              ))}
              {data.people.map((p) => (
                <TableRow key={p.user_id}>
                  {/* The space is literal, not margin: a name run
                      straight into an inline tag reads as one word to
                      anything that strips markup — a screen reader, or
                      the site's own copy check. */}
                  <TableCell>
                    {p.name}{" "}
                    <span className="text-xs text-ink-3">{p.email}</span>
                  </TableCell>
                  <TableCell className="text-ink-2">{p.role}</TableCell>
                  <TableCell className="text-xs text-ink-3">
                    {SOURCE[p.source]}
                    {p.team_name ? ` · ${p.team_name}` : ""}
                  </TableCell>
                  <TableCell className="text-right">
                    {p.source === "direct_grant" && (
                      <Button
                        variant="ghost"
                        size="xs"
                        className="hover:text-serious"
                        onClick={() =>
                          act(
                            () => api.revokeGrant(session, repo, p.user_id),
                            "withdraw that grant",
                          )
                        }
                        disabled={busy}
                      >
                        Revoke
                      </Button>
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      )}
      <div className="mt-4 flex flex-wrap items-end gap-2">
        <label className="flex flex-col gap-1 text-xs text-ink-3">
          Grant to
          <Select value={subject} onValueChange={setSubject}>
            <SelectTrigger aria-label="Grant access to">
              <SelectValue placeholder="a person or a team" />
            </SelectTrigger>
            <SelectContent>
              {options.map((o) => (
                <SelectItem key={o.value} value={o.value}>
                  {o.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </label>
        <label className="flex flex-col gap-1 text-xs text-ink-3">
          Role
          <Select value={role} onValueChange={(v) => setRole(v as Role)}>
            <SelectTrigger aria-label="Role to grant">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {(["admin", "member", "viewer"] as const).map((r) => (
                <SelectItem key={r} value={r}>
                  {r}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </label>
        <Button variant="outline" onClick={grant} disabled={busy || !subject}>
          Grant
        </Button>
      </div>
    </Panel>
  );
}
