import { useCallback, useEffect, useState } from "react";
import { api, type Role, type Session, type Token } from "@/api";
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
import { formatAgo } from "@/format";
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

const SCOPES = ["org:admin", "org:read", "repo:read", "repo:write", "repo:cache"];

/// What a role can put on a token, from the org role alone. Used only
/// until the server answers with the real list — a per-repo grant can
/// raise it, and only the server knows about those.
function scopesFor(role: Role | undefined): string[] {
  switch (role) {
    case "viewer":
      return ["org:read", "repo:read"];
    case "member":
      return ["org:read", "repo:read", "repo:write", "repo:cache"];
    default:
      return SCOPES;
  }
}

/// Access tokens: what a script, a CI job or a `git` client authenticates
/// with over HTTPS.
export function TokensPanel(props: {
  session: Session;
  role?: Role;
  isAdmin: boolean;
}) {
  const { session } = props;
  const people = usePeople(session, props.isAdmin);
  const [mintable, setMintable] = useState<string[] | null>(null);
  const allowed = mintable ?? scopesFor(props.role);
  const [tokens, setTokens] = useState<Token[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Default to what a person actually wants: push, plus the org:read
  // that listing repos needs. `repo:write` alone is a trap — it clones
  // and pushes fine and then 404s on the first `GET /repos`.
  const [scopes, setScopes] = useState<string[] | null>(null);
  // Default to what a person actually wants: push, plus the org:read
  // that listing repos needs. `repo:write` alone is a trap — it clones
  // and pushes fine and then 404s on the first `GET /repos`. Chosen once
  // the server has said what this caller may ask for.
  const chosen = (
    scopes ??
    (allowed.includes("repo:write")
      ? ["org:read", "repo:write"]
      : ["org:read", "repo:read"])
  ).filter((s) => allowed.includes(s));
  const [label, setLabel] = useState("");
  const [minted, setMinted] = useState<string | null>(null);
  const [showRevoked, setShowRevoked] = useState(false);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(() => {
    api
      .tokens(session)
      .then((out) => {
        setTokens(out.tokens);
        setMintable(out.mintable_scopes);
      })
      .catch((e) => setError(String(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token]);

  useEffect(refresh, [refresh]);

  async function mint(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const out = await api.mintToken(session, {
        scopes: chosen,
        ...(label.trim() ? { label: label.trim() } : {}),
      });
      setMinted(out.token);
      setLabel("");
      refresh();
    } catch (err) {
      setError(`Could not mint: ${err instanceof Error ? err.message : err}`);
    } finally {
      setBusy(false);
    }
  }

  const all = tokens ?? [];
  const revokedCount = all.filter((t) => t.revoked_at != null).length;
  const visible = showRevoked ? all : all.filter((t) => t.revoked_at == null);

  return (
    <Panel
      title="Access tokens"
      hint="A token you mint here belongs to you and carries your role — never more. It stops working the moment your role narrows or you leave the org. Listing repos needs org:read; cloning and pushing need repo:read and repo:write."
    >
      <Err message={error} />
      {minted && (
        <div className="mb-3 rounded-md border border-borderline bg-surface-0 p-2">
          <div className="mb-1 text-xs text-ink-3">
            Copy this now — only its hash is stored, so it cannot be shown
            again.
          </div>
          <code className="block break-all font-mono text-xs">{minted}</code>
        </div>
      )}
      {visible.length > 0 ? (
        <div className="mb-4 overflow-x-auto">
          <Table>
            <TableHeader>
              <TableHeadRow>
                <TableHead>Label</TableHead>
                <TableHead>Scopes</TableHead>
                <TableHead>Owner</TableHead>
                <TableHead>Created</TableHead>
                <TableHead>Status</TableHead>
                <TableHead />
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {visible.map((t) => (
                <TableRow key={t.id}>
                  <TableCell>{t.label ?? "—"}</TableCell>
                  <TableCell className="font-mono text-xs text-ink-2">
                    {t.scopes.join(" ")}
                  </TableCell>
                  <TableCell className="text-xs text-ink-2">
                    {t.user_id ? (people[t.user_id] ?? "a person") : "service"}
                  </TableCell>
                  <TableCell className="text-ink-2">
                    {formatAgo(t.created_at)}
                  </TableCell>
                  <TableCell>
                    <Status revoked={t.revoked_at != null} />
                  </TableCell>
                  <TableCell className="text-right">
                    {t.revoked_at == null && (
                      <AlertDialog>
                        <AlertDialogTrigger asChild>
                          <Button variant="destructive" size="xs">
                            Revoke
                          </Button>
                        </AlertDialogTrigger>
                        <AlertDialogContent>
                          <AlertDialogHeader>
                            <AlertDialogTitle>
                              Revoke the {t.label} token?
                            </AlertDialogTitle>
                            <AlertDialogDescription>
                              Anything still using it stops working on its next
                              request. A revoked token cannot be reactivated.
                            </AlertDialogDescription>
                          </AlertDialogHeader>
                          <AlertDialogFooter>
                            <AlertDialogCancel>Cancel</AlertDialogCancel>
                            <AlertDialogAction
                              onClick={async () => {
                                setError(null);
                                try {
                                  await api.revokeToken(session, t.id);
                                  refresh();
                                  toast.success(`Revoked ${t.label}`);
                                } catch (err) {
                                  setError(
                                    `Could not revoke: ${err instanceof Error ? err.message : err}`,
                                  );
                                }
                              }}
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
        <p className="mb-4 text-sm text-ink-3">No tokens yet.</p>
      )}
      <RevokedToggle
        count={revokedCount}
        shown={showRevoked}
        onToggle={() => setShowRevoked((v) => !v)}
      />
      <form onSubmit={mint} className="flex flex-wrap items-center gap-2">
        <div className="flex flex-wrap gap-3">
          {allowed.map((s) => (
            <label
              key={s}
              className="flex items-center gap-1.5 text-xs text-ink-2"
            >
              <input
                type="checkbox"
                checked={chosen.includes(s)}
                onChange={(e) =>
                  setScopes(
                    e.target.checked
                      ? [...chosen, s]
                      : chosen.filter((x) => x !== s),
                  )
                }
              />
              <code className="font-mono">{s}</code>
            </label>
          ))}
        </div>
        <input
          className="min-w-0 flex-1 rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 text-xs"
          placeholder="label (optional)"
          aria-label="Token label"
          value={label}
          onChange={(e) => setLabel(e.target.value)}
        />
        <Button
          size="xs"
          className="px-3 py-1.5"
          disabled={busy || chosen.length === 0}
        >
          {busy ? "Minting…" : "Mint token"}
        </Button>
      </form>
    </Panel>
  );
}
