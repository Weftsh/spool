import { useCallback, useEffect, useState } from "react";
import {
  api,
  type Invite,
  type Me,
  type Member,
  type Role,
  type Session,
} from "@/api";
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
import { formatAgo, formatIn } from "@/format";
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

const ROLES: Role[] = ["owner", "admin", "member", "viewer"];

/// Who is in this org, at what role, and who has been invited.
export function MembersPanel(props: { session: Session; me: Me | null }) {
  const { session, me } = props;
  const [members, setMembers] = useState<Member[] | null>(null);
  const [invites, setInvites] = useState<Invite[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<Role>("member");
  const [link, setLink] = useState<string | null>(null);
  const [delivery, setDelivery] = useState<{
    sent: boolean;
    error?: string;
  } | null>(null);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(() => {
    Promise.all([api.members(session), api.invites(session)])
      .then(([m, i]) => {
        setMembers(m);
        setInvites(i);
      })
      .catch((e) => setError(String(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token]);

  useEffect(refresh, [refresh]);

  async function act(what: () => Promise<unknown>, verb: string) {
    setError(null);
    try {
      await what();
      refresh();
      return true;
    } catch (err) {
      setError(
        `Could not ${verb}: ${err instanceof Error ? err.message : err}`,
      );
      return false;
    }
  }

  async function sendInvite(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const out = await api.invite(session, email.trim(), role);
      setLink(out.invite_link);
      setDelivery(out.mail);
      setEmail("");
      refresh();
    } catch (err) {
      setError(`Could not invite: ${err instanceof Error ? err.message : err}`);
    } finally {
      setBusy(false);
    }
  }

  const pending = (invites ?? []).filter((i) => i.accepted_at == null);

  return (
    <div className="space-y-4">
      <Panel
        title="Members"
        hint="A role applies across the org. A per-repo grant replaces it on that repo alone."
      >
        <Err message={error} />
        {!members ? (
          <Loading />
        ) : (
          <div className="overflow-x-auto">
            <Table>
              <TableHeader>
                <TableHeadRow>
                  <TableHead>Person</TableHead>
                  <TableHead>Role</TableHead>
                  <TableHead>Joined</TableHead>
                  <TableHead />
                </TableHeadRow>
              </TableHeader>
              <TableBody>
                {members.map((m) => (
                  <TableRow key={m.user_id}>
                    <TableCell>
                      <div className="font-medium">{m.name}</div>
                      <div className="text-xs text-ink-3">{m.email}</div>
                    </TableCell>
                    <TableCell>
                      <Select
                        value={m.role}
                        onValueChange={(v) =>
                          act(
                            () => api.setRole(session, m.user_id, v as Role),
                            "change that role",
                          )
                        }
                      >
                        <SelectTrigger
                          aria-label={`Role for ${m.email}`}
                          className="px-2 py-1"
                        >
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          {ROLES.map((r) => (
                            <SelectItem key={r} value={r}>
                              {r}
                            </SelectItem>
                          ))}
                        </SelectContent>
                      </Select>
                    </TableCell>
                    <TableCell className="text-ink-2">
                      {formatAgo(m.created_at)}
                    </TableCell>
                    <TableCell className="text-right">
                      {m.user_id !== me?.id && (
                        <AlertDialog>
                          <AlertDialogTrigger asChild>
                            <Button
                              variant="destructive"
                              size="xs"
                              // Every row's button says "Remove", so without
                              // a label a screen reader announces a column of
                              // identical controls and the person removed is
                              // whichever one you happened to be on.
                              aria-label={`Remove ${m.email}`}
                            >
                              Remove
                            </Button>
                          </AlertDialogTrigger>
                          <AlertDialogContent>
                            <AlertDialogHeader>
                              <AlertDialogTitle>
                                Remove {m.email}?
                              </AlertDialogTitle>
                              <AlertDialogDescription>
                                They lose access to every repo in this
                                organization. Their commits and activity stay.
                              </AlertDialogDescription>
                            </AlertDialogHeader>
                            <AlertDialogFooter>
                              <AlertDialogCancel>Cancel</AlertDialogCancel>
                              <AlertDialogAction
                                onClick={async () => {
                                  if (
                                    await act(
                                      () =>
                                        api.removeMember(session, m.user_id),
                                      "remove that member",
                                    )
                                  )
                                    toast.success(`Removed ${m.email}`);
                                }}
                              >
                                Remove
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
          </div>
        )}
      </Panel>

      <Panel
        title="Invitations"
        hint="An invitation is emailed. The link is also shown here once, so you can send it yourself if mail is not configured or did not go out."
      >
        {link && (
          <div className="mb-3 rounded-md border border-borderline bg-surface-0 p-2">
            <div className="mb-1 text-xs text-ink-3">
              {delivery?.sent
                ? "Emailed. The link below is the same one — it is not shown again."
                : delivery?.error
                  ? `Not emailed (${delivery.error}). Send this link yourself — it is not shown again.`
                  : "No mail transport is configured. Send this link yourself — it is not shown again."}
            </div>
            <code className="block break-all font-mono text-xs">{link}</code>
          </div>
        )}
        {pending.length > 0 ? (
          <ul className="mb-3 space-y-1 text-sm">
            {pending.map((i) => (
              <li key={i.id} className="flex items-center gap-2">
                <span className="font-medium">{i.email}</span>
                <span className="text-xs text-ink-3">{i.role}</span>
                <span className="text-xs text-ink-3">
                  {formatIn(i.expires_at) === "expired"
                    ? "expired"
                    : `expires ${formatIn(i.expires_at)}`}
                </span>
                <Button
                  variant="destructive"
                  size="xs"
                  className="ml-auto"
                  onClick={() =>
                    act(
                      () => api.revokeInvite(session, i.id),
                      "revoke that invitation",
                    )
                  }
                >
                  Revoke
                </Button>
              </li>
            ))}
          </ul>
        ) : (
          <p className="mb-3 text-sm text-ink-3">No outstanding invitations.</p>
        )}
        <form onSubmit={sendInvite} className="flex flex-wrap gap-2">
          <input
            className="min-w-0 flex-1 rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 text-sm"
            placeholder="person@example.com"
            aria-label="Invite email"
            type="email"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            required
          />
          <Select value={role} onValueChange={(v) => setRole(v as Role)}>
            <SelectTrigger aria-label="Invite role">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {ROLES.map((r) => (
                <SelectItem key={r} value={r}>
                  {r}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Button disabled={busy}>{busy ? "Inviting…" : "Invite"}</Button>
        </form>
      </Panel>
    </div>
  );
}
