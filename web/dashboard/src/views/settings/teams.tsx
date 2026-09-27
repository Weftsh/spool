import { useCallback, useEffect, useState } from "react";
import { api, type Session, type Team, type TeamMember } from "@/api";
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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { toast } from "sonner";

/// Teams: the way "the payments squad can write here" is said once
/// rather than once per person.
///
/// Two lists side by side rather than a page per team — an org has a
/// handful of teams and the thing you came to do is move people between
/// them, which is one screen's work, not a navigation problem.
export function TeamsPanel(props: { session: Session; isAdmin: boolean }) {
  const { session, isAdmin } = props;
  const people = usePeople(session, isAdmin);
  const [teams, setTeams] = useState<Team[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [members, setMembers] = useState<TeamMember[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [busy, setBusy] = useState(false);
  const [adding, setAdding] = useState("");

  const refresh = useCallback(() => {
    api
      .teams(session)
      .then(setTeams)
      .catch((e) => setError(String(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token]);

  useEffect(refresh, [refresh]);

  useEffect(() => {
    if (!selected) {
      setMembers(null);
      return;
    }
    let alive = true;
    api
      .teamMembers(session, selected)
      .then((m) => alive && setMembers(m))
      .catch((e) => alive && setError(String(e)));
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected, session.org, session.token]);

  async function act(what: () => Promise<unknown>, verb: string) {
    setError(null);
    setBusy(true);
    try {
      await what();
      refresh();
      if (selected) setMembers(await api.teamMembers(session, selected));
      return true;
    } catch (err) {
      setError(
        `Could not ${verb}: ${err instanceof Error ? err.message : err}`,
      );
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function create(e: React.FormEvent) {
    e.preventDefault();
    const n = name.trim();
    if (!n) return;
    await act(async () => {
      const t = await api.createTeam(session, n, description.trim());
      setName("");
      setDescription("");
      setSelected(t.id);
    }, "create that team");
  }

  const current = teams?.find((t) => t.id === selected) ?? null;
  // Only people not already in the team are worth offering.
  const inTeam = new Set((members ?? []).map((m) => m.user_id));
  const addable = Object.entries(people).filter(([id]) => !inTeam.has(id));

  return (
    <div className="space-y-4">
      <Panel
        title="Teams"
        hint="A team grant raises what its people can do on a repo. It never lowers anyone — that stays a per-person decision."
      >
        <Err message={error} />
        {!teams ? (
          <Loading />
        ) : teams.length === 0 ? (
          <p className="text-sm text-ink-3">No teams yet.</p>
        ) : (
          <Table>
            <TableHeader>
              <TableHeadRow>
                <TableHead>Team</TableHead>
                <TableHead>People</TableHead>
                <TableHead />
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {teams.map((t) => (
                <TableRow key={t.id}>
                  <TableCell>
                    <button
                      className={`text-left ${t.id === selected ? "text-ink" : "text-ink-2 hover:text-ink"}`}
                      onClick={() =>
                        setSelected(t.id === selected ? null : t.id)
                      }
                    >
                      {t.name}
                    </button>
                    {t.description && (
                      <div className="text-xs text-ink-3">{t.description}</div>
                    )}
                  </TableCell>
                  <TableCell className="text-ink-2">{t.member_count}</TableCell>
                  <TableCell className="text-right">
                    {isAdmin && (
                      <AlertDialog>
                        <AlertDialogTrigger asChild>
                          <Button
                            variant="ghost"
                            size="xs"
                            className="hover:text-serious"
                            disabled={busy}
                          >
                            Delete
                          </Button>
                        </AlertDialogTrigger>
                        <AlertDialogContent>
                          <AlertDialogHeader>
                            <AlertDialogTitle>
                              Delete the {t.name} team?
                            </AlertDialogTitle>
                            <AlertDialogDescription>
                              Its members keep their org roles; any access
                              granted through this team goes away.
                            </AlertDialogDescription>
                          </AlertDialogHeader>
                          <AlertDialogFooter>
                            <AlertDialogCancel>Cancel</AlertDialogCancel>
                            <AlertDialogAction
                              onClick={async () => {
                                if (t.id === selected) setSelected(null);
                                if (
                                  await act(
                                    () => api.deleteTeam(session, t.id),
                                    "delete that team",
                                  )
                                )
                                  toast.success(`Deleted ${t.name}`);
                              }}
                            >
                              Delete
                            </AlertDialogAction>
                          </AlertDialogFooter>
                        </AlertDialogContent>
                      </AlertDialog>
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
        {isAdmin && (
          <form
            className="mt-4 flex flex-wrap items-end gap-2"
            onSubmit={create}
          >
            <label className="flex flex-col gap-1 text-xs text-ink-3">
              New team
              <input
                className="rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 text-sm"
                aria-label="Team name"
                placeholder="payments"
                value={name}
                onChange={(e) => setName(e.target.value)}
              />
            </label>
            <label className="flex flex-col gap-1 text-xs text-ink-3">
              Description
              <input
                className="rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 text-sm"
                aria-label="Team description"
                placeholder="optional"
                value={description}
                onChange={(e) => setDescription(e.target.value)}
              />
            </label>
            <Button variant="outline" disabled={busy || !name.trim()}>
              Create team
            </Button>
          </form>
        )}
      </Panel>

      {current && (
        <Panel title={current.name} hint="Who is in this team.">
          {!members ? (
            <Loading />
          ) : members.length === 0 ? (
            <p className="text-sm text-ink-3">Nobody is in this team yet.</p>
          ) : (
            <Table>
              <TableBody>
                {members.map((m) => (
                  <TableRow key={m.user_id}>
                    <TableCell>{m.name}</TableCell>
                    <TableCell className="text-ink-2">{m.email}</TableCell>
                    <TableCell className="text-right">
                      {isAdmin && (
                        <Button
                          variant="ghost"
                          size="xs"
                          className="hover:text-serious"
                          aria-label={`Remove ${m.email} from the team`}
                          onClick={() =>
                            act(
                              () =>
                                api.removeTeamMember(
                                  session,
                                  current.id,
                                  m.user_id,
                                ),
                              "remove them from the team",
                            )
                          }
                          disabled={busy}
                        >
                          Remove
                        </Button>
                      )}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
          {isAdmin && (
            <div className="mt-4 flex flex-wrap items-end gap-2">
              <label className="flex flex-col gap-1 text-xs text-ink-3">
                Add someone
                <Select value={adding} onValueChange={setAdding}>
                  <SelectTrigger aria-label="Add to team">
                    <SelectValue placeholder="choose a person" />
                  </SelectTrigger>
                  <SelectContent>
                    {addable.map(([id, email]) => (
                      <SelectItem key={id} value={id}>
                        {email}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </label>
              <Button
                variant="outline"
                onClick={() =>
                  act(async () => {
                    await api.addTeamMember(session, current.id, adding);
                    setAdding("");
                  }, "add them to the team")
                }
                disabled={busy || !adding}
              >
                Add
              </Button>
            </div>
          )}
        </Panel>
      )}
    </div>
  );
}
