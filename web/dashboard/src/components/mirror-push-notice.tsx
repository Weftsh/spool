import { useEffect, useState } from "react";
import { api, type GithubInstallation, type Repo, type Session } from "@/api";
import { Alert } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { PROSE_LINK } from "@/lib/links";
import {
  CONTENTS_WRITE,
  approvePushLine,
  forwardingLine,
  installationPush,
  pushStatus,
} from "@/lib/mirror-push";

/// Where a push to this mirror goes, on the repository page.
///
/// Three things it can say, each decided by `pushStatus`: pushes are
/// forwarded (one line, naming the origin); the installation behind
/// the mirror has to approve `Contents: write` (the link); or nothing
/// behind the mirror can push at all — a pasted public URL — in which
/// case an admin can attach an installation the organization has
/// connected, right here, and the page updates from the server's
/// answer rather than assuming.
///
/// Given `info`, it reads it; without, it fetches the row itself, the
/// way `RepoWalls` does — the file browser is where a link to a
/// repository lands and it holds no row of its own.
export function MirrorPushNotice(props: {
  session: Session;
  repo: string;
  info?: Repo;
  onAttached?: (next: Repo) => void;
}) {
  const [fetched, setFetched] = useState<Repo | null>(null);
  const { session, repo } = props;
  const given = props.info;
  // The primitives, not the object: a caller that builds its session
  // inline hands a fresh object every render, and an effect keyed on
  // it would refetch forever. Same rule as `RepoWalls`.
  const { org, token } = session;
  useEffect(() => {
    if (given) return;
    let alive = true;
    api
      .repo({ org, token }, repo)
      .then((r) => alive && setFetched(r))
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [org, token, repo, given]);
  const info = given ?? fetched;
  if (!info) return null;
  const onAttached = (next: Repo) => {
    setFetched(next);
    props.onAttached?.(next);
  };
  const status = pushStatus(info);
  if (status.kind === "native" || status.kind === "unknown") return null;
  if (status.kind === "forwarding") {
    return (
      <p className="text-sm text-ink-3" data-testid="mirror-push">
        {forwardingLine(info.origin_url)}
      </p>
    );
  }
  if (status.kind === "approve") {
    return (
      <Alert variant="warning" role="status" data-testid="mirror-push">
        <span className="font-medium text-ink">
          Pushes to this mirror are refused.
        </span>{" "}
        The installation behind it was approved before the App asked to
        write.{" "}
        {status.url ? (
          <a
            className={PROSE_LINK}
            href={status.url}
            target="_blank"
            rel="noreferrer"
          >
            {approvePushLine(status.missing)}
          </a>
        ) : (
          approvePushLine(status.missing)
        )}
        .
      </Alert>
    );
  }
  return (
    <Alert variant="warning" role="status" data-testid="mirror-push">
      <span className="font-medium text-ink">
        Pushes to this mirror are refused.
      </span>{" "}
      {status.blocked}.
      {info.viewer_admin && (
        <AttachInstallation session={session} repo={repo} onAttached={onAttached} />
      )}
    </Alert>
  );
}

/// Attach one of the organization's connected installations, so a
/// mirror made from a pasted URL becomes one that pushes.
function AttachInstallation(props: {
  session: Session;
  repo: string;
  onAttached: (next: Repo) => void;
}) {
  const { session } = props;
  const { org, token } = session;
  const [installs, setInstalls] = useState<GithubInstallation[] | null>(null);
  const [chosen, setChosen] = useState<string>("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    api
      .githubInstallations({ org, token })
      .then((i) => {
        if (!alive) return;
        setInstalls(i);
        if (i.length === 1) setChosen(i[0].installation_id);
      })
      .catch(() => alive && setInstalls([]));
    return () => {
      alive = false;
    };
  }, [org, token]);

  const attach = async () => {
    if (!chosen) return;
    setBusy(true);
    setError(null);
    try {
      const next = await api.patchRepo(session, props.repo, {
        installation_id: chosen,
      });
      props.onAttached(next);
    } catch (e) {
      setError(String((e as Error)?.message ?? e));
    } finally {
      setBusy(false);
    }
  };

  if (installs === null || installs.length === 0) return null;
  return (
    <div className="mt-2 flex flex-wrap items-center gap-2" data-testid="attach-installation">
      <Select value={chosen} onValueChange={(v) => setChosen(v)}>
        <SelectTrigger aria-label="GitHub installation" className="w-56">
          <SelectValue placeholder="Choose an installation…" />
        </SelectTrigger>
        <SelectContent>
          {installs.map((i) => (
            <SelectItem key={i.installation_id} value={i.installation_id}>
              {i.account ?? i.installation_id}
              {installationPush(i) === "approve"
                ? ` — ${approvePushLine([CONTENTS_WRITE]).replace(/^Approve/, "approve")} first`
                : ""}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <Button type="button" size="xs" disabled={busy || !chosen} onClick={() => void attach()}>
        {busy ? "Attaching…" : "Attach and forward pushes"}
      </Button>
      {error && (
        <span className="text-sm text-serious" role="alert">
          {error}
        </span>
      )}
    </div>
  );
}
