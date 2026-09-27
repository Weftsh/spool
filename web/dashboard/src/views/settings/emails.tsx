import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";
import { api, type EmailRow, type Me, type Session } from "@/api";
import { Err, Loading } from "@/components/feedback";
import { Panel } from "@/components/panel";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableHeadRow,
  TableRow,
} from "@/components/ui/table";
import { formatAgo } from "@/format";

/// The addresses on your account, and which of them are proved.
///
/// **This exists because the contribution graph is otherwise
/// unreachable.** A commit counts for you only when its author line
/// carries an address you have proved — anybody can put anybody's
/// address in `git config user.email`, so an unproved match must count
/// for nothing. Everything needed to prove one already existed on the
/// server; there was simply no way to ask for it, so somebody arriving
/// from another forge saw an empty graph and had no way to fix it. An
/// empty graph and no explanation is the worst possible answer to
/// "where did my decade of work go".
///
/// Proving an address is **retroactive**: the server re-walks the
/// repositories you can reach, so commits you authored under it years
/// ago start counting. That is said on the page, because a person who
/// does not know it will not bother.
export function EmailsPanel(props: { session: Session; me: Me | null }) {
  const handle = props.me?.handle ?? null;
  const { session } = props;
  const [rows, setRows] = useState<EmailRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState("");
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(() => {
    if (!handle) return;
    api
      .emails(session, handle)
      .then((r) => setRows(r.emails))
      .catch((e) => setError(String(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token, handle]);

  useEffect(refresh, [refresh]);

  // A token session has no person behind it, and addresses belong to a
  // person. Saying so beats rendering a form whose every submit is 401.
  if (!handle) {
    return (
      <Panel
        title="Email addresses"
        hint="Addresses belong to a person. Sign in with your account rather than an API token to manage them."
      >
        <p className="text-sm text-ink-3">
          Nothing to show for a token session.
        </p>
      </Panel>
    );
  }

  const add = async () => {
    const address = adding.trim();
    if (!address) return;
    setBusy(true);
    try {
      await api.addEmail(session, handle, address);
      setAdding("");
      toast.success(`Sent a link to ${address}`);
      refresh();
    } catch (e) {
      // The server's own sentence: it knows whether the address is
      // unusable, already yours, or spoken for, and those are three
      // different things for the person reading them.
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setBusy(false);
    }
  };

  const prove = async () => {
    const t = token.trim();
    if (!t) return;
    setBusy(true);
    try {
      const { address } = await api.proveEmail(session, handle, t);
      setToken("");
      toast.success(
        `${address} is now proved — your past commits under it will count`,
      );
      refresh();
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (address: string) => {
    setBusy(true);
    try {
      await api.removeEmail(session, handle, address);
      refresh();
    } catch (e) {
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-col gap-6">
      <Panel
        title="Email addresses"
        hint="Your commits count towards your contribution graph only when the address on the commit is one you have proved. Proving an address applies to work you have already pushed, not just to new commits."
      >
        {error && <Err message={error} />}
        {!error && !rows && <Loading />}
        {rows && rows.length > 0 && (
          <Table>
            <TableHeader>
              <TableHeadRow>
                <TableHead>Address</TableHead>
                <TableHead>State</TableHead>
                <TableHead>Added</TableHead>
                <TableHead />
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {rows.map((r) => (
                <TableRow key={r.address}>
                  <TableCell className="font-medium text-ink">
                    {r.address}{" "}
                    {r.primary && <Badge variant="neutral">Sign-in</Badge>}
                  </TableCell>
                  <TableCell>
                    {r.verified_at ? (
                      <span className="text-good">Proved</span>
                    ) : (
                      /* Not an error state: an unproved address is a
                         claim waiting on a link, and calling it
                         "unverified" in red reads as something having
                         gone wrong. */
                      <span className="text-ink-3">
                        Waiting for the link we sent
                      </span>
                    )}
                  </TableCell>
                  <TableCell className="text-ink-3">
                    {formatAgo(r.created_at)}
                  </TableCell>
                  <TableCell className="text-right">
                    {/* The sign-in address has no remove control at all
                        rather than one the server refuses: an account
                        with no reachable address is one nobody can
                        recover, and offering the button teaches people
                        it is a thing they might do. */}
                    {!r.primary && (
                      <Button
                        variant="ghost"
                        size="xs"
                        disabled={busy}
                        aria-label={`Remove ${r.address}`}
                        onClick={() => remove(r.address)}
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
      </Panel>

      <Panel
        title="Add an address"
        hint="Use the address your commits are authored with — check it with git config user.email in a repository you work on. We will send it a link."
      >
        <div className="flex flex-wrap items-end gap-2">
          <Input
            aria-label="Email address"
            placeholder="you@example.com"
            value={adding}
            onChange={(e) => setAdding(e.target.value)}
          />
          <Button onClick={add} disabled={busy || !adding.trim()}>
            Send the link
          </Button>
        </div>
      </Panel>

      <Panel
        title="Prove an address"
        hint="Paste the code from the link we emailed you. Once proved, commits you have already pushed under that address start counting towards your graph."
      >
        <div className="flex flex-wrap items-end gap-2">
          <Input
            aria-label="Verification code"
            placeholder="address:code from the email"
            value={token}
            onChange={(e) => setToken(e.target.value)}
          />
          <Button onClick={prove} disabled={busy || !token.trim()}>
            Prove it
          </Button>
        </div>
      </Panel>
    </div>
  );
}
