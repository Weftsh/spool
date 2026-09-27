import { useCallback, useEffect, useState } from "react";
import { api, type Session, type SshKey } from "@/api";
import { Err, RevokedToggle, Status } from "@/components/feedback";
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
import { formatAgo, formatFingerprint } from "@/format";
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

/// SSH keys. A key you add here is yours: it signs in as you, so it
/// carries your role — including any per-repo grant — and needs no token
/// id. An administrator can also register a deploy key bound to a token,
/// which is what an unattended machine uses.
export function SshKeysPanel(props: { session: Session; isAdmin: boolean }) {
  const { session } = props;
  const people = usePeople(session, props.isAdmin);
  const [keys, setKeys] = useState<SshKey[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pubKey, setPubKey] = useState("");
  const [tokenId, setTokenId] = useState("");
  const [label, setLabel] = useState("");
  const [deploy, setDeploy] = useState(false);
  const [showRevoked, setShowRevoked] = useState(false);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(() => {
    api
      .sshKeys(session)
      .then((k) => setKeys(k))
      .catch((e) => setError(String(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token]);

  useEffect(refresh, [refresh]);

  async function add(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.addSshKey(session, {
        public_key: pubKey.trim(),
        ...(deploy && tokenId.trim() ? { token_id: tokenId.trim() } : {}),
        ...(label.trim() ? { label: label.trim() } : {}),
      });
      setPubKey("");
      setTokenId("");
      setLabel("");
      refresh();
    } catch (err) {
      setError(
        `Could not add key: ${err instanceof Error ? err.message : err}`,
      );
    } finally {
      setBusy(false);
    }
  }

  async function revoke(id: string, label: string) {
    setError(null);
    try {
      await api.revokeSshKey(session, id);
      refresh();
      toast.success(`Revoked ${label}`);
    } catch (err) {
      setError(
        `Could not revoke key: ${err instanceof Error ? err.message : err}`,
      );
    }
  }

  const all = keys ?? [];
  const revokedCount = all.filter((k) => k.revoked_at != null).length;
  const visible = showRevoked ? all : all.filter((k) => k.revoked_at == null);

  return (
    <Panel
      title="SSH keys"
      hint="Paste the contents of your public key file — usually ~/.ssh/id_ed25519.pub. Revoking it cuts access on the next connection."
    >
      <Err message={error} />
      {visible.length > 0 ? (
        <div className="mb-4 overflow-x-auto">
          <Table>
            <TableHeader>
              <TableHeadRow>
                <TableHead>Label</TableHead>
                <TableHead>Fingerprint</TableHead>
                <TableHead>Signs in as</TableHead>
                <TableHead>Added</TableHead>
                <TableHead>Status</TableHead>
                <TableHead />
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {visible.map((k) => (
                <TableRow key={k.id}>
                  <TableCell>{k.label ?? "—"}</TableCell>
                  <TableCell
                    className="font-mono text-xs"
                    title={k.fingerprint_sha256}
                  >
                    {formatFingerprint(k.fingerprint_sha256)}
                  </TableCell>
                  <TableCell className="text-xs text-ink-2">
                    {k.user_id
                      ? (people[k.user_id] ?? "a person")
                      : "a token (deploy key)"}
                  </TableCell>
                  <TableCell className="text-ink-2">
                    {formatAgo(k.created_at)}
                  </TableCell>
                  <TableCell>
                    <Status revoked={k.revoked_at != null} />
                  </TableCell>
                  <TableCell className="text-right">
                    {k.revoked_at == null && (
                      <AlertDialog>
                        <AlertDialogTrigger asChild>
                          <Button variant="destructive" size="xs">
                            Revoke
                          </Button>
                        </AlertDialogTrigger>
                        <AlertDialogContent>
                          <AlertDialogHeader>
                            <AlertDialogTitle>
                              Revoke the {k.label ?? "unnamed"} key?
                            </AlertDialogTitle>
                            <AlertDialogDescription>
                              SSH clones and pushes with this key are refused
                              from now on. A revoked key cannot be reactivated.
                            </AlertDialogDescription>
                          </AlertDialogHeader>
                          <AlertDialogFooter>
                            <AlertDialogCancel>Cancel</AlertDialogCancel>
                            <AlertDialogAction
                              onClick={() =>
                                revoke(k.id, k.label ?? "that key")
                              }
                            >
                              Revoke
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
      ) : (
        <p className="mb-4 text-sm text-ink-3">No SSH keys yet.</p>
      )}
      <RevokedToggle
        count={revokedCount}
        shown={showRevoked}
        onToggle={() => setShowRevoked((v) => !v)}
      />
      <form onSubmit={add} className="space-y-2">
        <textarea
          className="w-full rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 font-mono text-xs"
          rows={2}
          placeholder="ssh-ed25519 AAAA… you@laptop"
          aria-label="Public key"
          value={pubKey}
          onChange={(e) => setPubKey(e.target.value)}
          required
        />
        <div className="flex flex-wrap gap-2">
          <input
            className="min-w-0 flex-1 rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 text-xs"
            placeholder="label (optional)"
            aria-label="Key label"
            value={label}
            onChange={(e) => setLabel(e.target.value)}
          />
          <Button size="xs" className="px-3 py-1.5" disabled={busy}>
            {busy ? "Adding…" : "Add key"}
          </Button>
        </div>
        {props.isAdmin && (
          <div className="space-y-2 border-t border-borderline pt-2">
            <label className="flex items-center gap-1.5 text-xs text-ink-2">
              <input
                type="checkbox"
                checked={deploy}
                onChange={(e) => setDeploy(e.target.checked)}
              />
              Deploy key — bind it to a token instead of to me
            </label>
            {deploy && (
              <input
                className="w-full rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 font-mono text-xs"
                placeholder="token id (the id from the token's mint response)"
                aria-label="Token id"
                value={tokenId}
                onChange={(e) => setTokenId(e.target.value)}
                required
              />
            )}
          </div>
        )}
      </form>
    </Panel>
  );
}
