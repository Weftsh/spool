/// Making a repository, and mirroring one, from the browser.
///
/// Until now there was no way to create anything from the dashboard at
/// all, and mirroring — the product's lead pitch — asked a person to
/// find an opaque `installation_id` in a GitHub settings URL and paste
/// it next to an `owner/name` that had to match it. Nothing checked
/// either until the first sync failed, minutes later, on a repository
/// that just looked broken.
///
/// The happy path here is: **paste a URL**. A public origin mirrors
/// immediately with no credentials and nothing to install — that is the
/// evaluation path and it has to be instant. A private one leads to the
/// GitHub App, and then to a list of what that installation can actually
/// read, so the id is never seen or typed.

import { useCallback, useEffect, useState } from "react";
import {
  api,
  type GithubInstallation,
  type Probe,
  type RemoteRepo,
  type Session,
  type SyncStatus,
} from "@/api";
import { href, navigateTo } from "@/router";
import { formatBytes } from "@/format";
import { Alert } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { PROSE_LINK } from "@/lib/links";
import {
  CONTENTS_WRITE,
  approvePushLine,
  forwardingLine,
  installationPush,
  remoteAddLine,
} from "@/lib/mirror-push";
import { Table, TableBody, TableCell, TableRow } from "@/components/ui/table";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

type Mode = "empty" | "mirror";

export function NewRepo(props: {
  session: Session;
  navigate: (to: string) => void;
  /// Set when the person has just come back from installing the App, so
  /// the screen can open on the picker instead of making them start
  /// over.
  justConnected?: boolean;
}) {
  const { session } = props;
  const [mode, setMode] = useState<Mode>("mirror");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [created, setCreated] = useState<string | null>(null);

  const submitEmpty = async () => {
    setBusy(true);
    setError(null);
    try {
      const repo = await api.createRepo(session, {
        name,
        description: description.trim() || undefined,
      });
      setCreated(repo.name);
    } catch (err) {
      // The server's sentence, shown as it was said.
      setError(String((err as Error)?.message ?? err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="mx-auto max-w-2xl space-y-6">
      <div>
        <h2 className="text-xl font-semibold">New repository</h2>
        <p className="mt-1 text-sm text-ink-3">
          Start something empty, or mirror one that already exists.
        </p>
      </div>

      <Tabs
        value={mode}
        onValueChange={(v) => {
          setMode(v as Mode);
          setError(null);
        }}
      >
        <TabsList aria-label="What to create" className="gap-2 border-b-0">
          {(
            [
              ["mirror", "Mirror an existing one"],
              ["empty", "Empty repository"],
            ] as [Mode, string][]
          ).map(([m, label]) => (
            <TabsTrigger
              key={m}
              value={m}
              className="mb-0 rounded-md border-b-0 px-3 py-1.5 data-[state=active]:bg-brand data-[state=active]:text-brand-ink data-[state=inactive]:border data-[state=inactive]:border-borderline data-[state=inactive]:text-ink-2 data-[state=inactive]:hover:text-ink"
            >
              {label}
            </TabsTrigger>
          ))}
        </TabsList>
      </Tabs>

      {error && <Alert variant="destructive">{error}</Alert>}

      {created ? (
        <FirstSync
          session={session}
          repo={created}
          navigate={props.navigate}
          mirror={mode === "mirror"}
        />
      ) : mode === "empty" ? (
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            void submitEmpty();
          }}
        >
          <Field
            label="Repository name"
            value={name}
            onChange={setName}
            placeholder="widget"
          />
          <Field
            label="Description"
            value={description}
            onChange={setDescription}
            placeholder="the fast one"
            hint="Optional, one line. This is what search matches."
          />
          <Button disabled={busy || !name.trim()}>
            {busy ? "Creating…" : "Create repository"}
          </Button>
        </form>
      ) : (
        <MirrorFlow
          session={session}
          justConnected={props.justConnected}
          onCreated={setCreated}
          onError={setError}
        />
      )}
    </div>
  );
}

/// Paste a URL; everything else follows from what the probe says.
function MirrorFlow(props: {
  session: Session;
  justConnected?: boolean;
  onCreated: (repo: string) => void;
  onError: (e: string | null) => void;
}) {
  const { session } = props;
  const [origin, setOrigin] = useState("");
  const [probe, setProbe] = useState<Probe | null>(null);
  const [busy, setBusy] = useState(false);
  const [picking, setPicking] = useState(!!props.justConnected);

  const suggested = nameFromOrigin(origin);

  const check = async () => {
    setBusy(true);
    props.onError(null);
    setProbe(null);
    try {
      setProbe(await api.probeOrigin(session, origin));
    } catch (e) {
      props.onError(String((e as Error)?.message ?? e));
    } finally {
      setBusy(false);
    }
  };

  const create = async (full: string, installation?: string) => {
    setBusy(true);
    props.onError(null);
    try {
      const out = await api.createMirror(session, {
        name: nameFromOrigin(full) || full,
        origin: full,
        provider: "github",
        installation_id: installation,
      });
      props.onCreated(out.repo.name);
    } catch (e) {
      props.onError(String((e as Error)?.message ?? e));
    } finally {
      setBusy(false);
    }
  };

  if (picking) {
    return (
      <Picker
        session={session}
        onBack={() => setPicking(false)}
        onPick={(r) => create(r.full_name, r.installationId)}
        onError={props.onError}
      />
    );
  }

  return (
    <div className="space-y-4">
      <form
        className="space-y-4"
        onSubmit={(e) => {
          e.preventDefault();
          void check();
        }}
      >
        <Field
          label="Repository URL"
          value={origin}
          onChange={(v) => {
            setOrigin(v);
            setProbe(null);
          }}
          placeholder="github.com/owner/repo"
          hint="A GitHub URL, owner/repo, or any git URL."
        />
        <div className="flex flex-wrap gap-2">
          <Button variant="outline" disabled={busy || !origin.trim()}>
            {busy ? "Checking…" : "Check origin"}
          </Button>
          {probe?.reachable && (
            <Button
              type="button"
              disabled={busy}
              onClick={() => void create(origin)}
            >
              Mirror {suggested || "it"}
            </Button>
          )}
        </div>
      </form>

      {probe && (
        <ProbeResult probe={probe} onConnect={() => setPicking(true)} />
      )}

      <p className="text-sm text-ink-3">
        Already connected GitHub?{" "}
        <button
          className="text-brand hover:underline"
          onClick={() => setPicking(true)}
        >
          Pick from your installations
        </button>
        .
      </p>
    </div>
  );
}

/// What the probe found, said in words, with the one action that helps.
function ProbeResult(props: { probe: Probe; onConnect: () => void }) {
  const { probe } = props;
  if (probe.reachable) {
    return (
      <div className="space-y-2 rounded-md border border-good/40 bg-good/10 px-3 py-2 text-sm">
        <p>
          Found it — {probe.refs} ref{probe.refs === 1 ? "" : "s"}
          {probe.default_branch ? `, default ${probe.default_branch}` : ""}. No
          credentials needed to read it.
        </p>
        {/* A mirror made from a pasted URL fetches as a stranger and
            cannot push as one. Said here, where the choice is made,
            rather than at the first refused push. */}
        <p className="text-ink-3" data-testid="pasted-url-read-only">
          Read-only until you connect GitHub: pushes are forwarded to the
          origin only through an installation.{" "}
          <button
            type="button"
            className="text-brand hover:underline"
            onClick={props.onConnect}
          >
            Pick from your installations
          </button>{" "}
          to mirror it with one.
        </p>
      </div>
    );
  }
  return (
    <div className="space-y-2 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-sm">
      <p>{probe.reason ?? "That origin is not reachable."}</p>
      {/* `private` is the case with an answer, so it is the only one that
          gets a button. Offering "connect GitHub" for a typo would send
          somebody through an install that cannot help them. */}
      {probe.private && (
        <Button onClick={props.onConnect}>Connect GitHub</Button>
      )}
      {/* This button opens the picker, which — when nothing is connected
          yet — offers the install. The two used to carry the same label,
          so "Connect GitHub" led to a screen with a "Connect GitHub"
          button on it, and it read as a click that had not worked. The
          one that actually leaves for GitHub says so instead. */}
    </div>
  );
}

/// The list of what an installation can read. This is the screen that
/// replaces typing an id.
function Picker(props: {
  session: Session;
  onBack: () => void;
  onPick: (r: RemoteRepo & { installationId: string }) => void;
  onError: (e: string | null) => void;
}) {
  const { session } = props;
  const [installs, setInstalls] = useState<GithubInstallation[] | null>(null);
  const [chosen, setChosen] = useState<string | null>(null);
  const [repos, setRepos] = useState<RemoteRepo[] | null>(null);
  const [filter, setFilter] = useState("");

  useEffect(() => {
    let alive = true;
    api
      .githubInstallations(session)
      .then((i) => {
        if (!alive) return;
        setInstalls(i);
        // One installation is the ordinary case; making somebody choose
        // from a list of one is a click that teaches nothing.
        if (i.length === 1) setChosen(i[0].installation_id);
      })
      .catch((e) => alive && props.onError(String((e as Error)?.message ?? e)));
    return () => {
      alive = false;
    };
  }, [session, props]);

  useEffect(() => {
    if (!chosen) return;
    let alive = true;
    setRepos(null);
    api
      .githubRepos(session, chosen)
      .then((r) => alive && setRepos(r))
      .catch((e) => alive && props.onError(String((e as Error)?.message ?? e)));
    return () => {
      alive = false;
    };
  }, [session, chosen, props]);

  const connect = async () => {
    try {
      const out = await api.startGithubInstall(session);
      // A full page leave, not a fetch: the install happens on GitHub
      // and comes back to our callback.
      window.location.href = out.url;
    } catch (e) {
      props.onError(String((e as Error)?.message ?? e));
    }
  };

  if (installs === null)
    return <p className="text-sm text-ink-3">Loading your installations…</p>;

  if (installs.length === 0) {
    return (
      <div className="space-y-3">
        <p className="text-sm text-ink-3">
          GitHub is not connected to this organization yet. Installing the
          Weft app lets you mirror private repositories — you choose which
          ones it can read.
        </p>
        <div className="flex gap-2">
          <Button onClick={() => void connect()}>Install the GitHub app</Button>
          <Button variant="outline" onClick={props.onBack}>
            Back
          </Button>
        </div>
      </div>
    );
  }

  const shown = (repos ?? []).filter((r) =>
    r.full_name.toLowerCase().includes(filter.trim().toLowerCase()),
  );

  return (
    <div className="space-y-4">
      {installs.length > 1 && (
        <label className="block text-sm">
          <span className="mb-1 block font-medium">GitHub account</span>
          <Select
            value={chosen ?? ""}
            onValueChange={(v) => setChosen(v || null)}
          >
            <SelectTrigger aria-label="GitHub account" className="w-full">
              <SelectValue placeholder="Choose an account…" />
            </SelectTrigger>
            <SelectContent>
              {installs.map((i) => (
                <SelectItem key={i.installation_id} value={i.installation_id}>
                  {i.account ?? i.installation_id}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </label>
      )}

      {chosen && <PickerPushNote inst={installs.find((i) => i.installation_id === chosen)} />}

      {chosen && (
        <Field
          label="Find a repository"
          value={filter}
          onChange={setFilter}
          placeholder="widget"
        />
      )}

      {chosen && repos === null && (
        <p className="text-sm text-ink-3">Asking GitHub what it can read…</p>
      )}

      {repos !== null && shown.length === 0 && (
        <p className="text-sm text-ink-3">
          Nothing matches. The app can only see repositories you granted it —
          you can change that on GitHub.
        </p>
      )}

      {shown.length > 0 && (
        <Table className="table-fixed text-sm">
          <TableBody>
            {shown.map((r) => (
              <TableRow key={r.full_name}>
                <TableCell className="max-w-0 truncate py-1.5">
                  <button
                    className="text-brand hover:underline"
                    onClick={() =>
                      props.onPick({ ...r, installationId: chosen! })
                    }
                  >
                    {r.full_name}
                  </button>
                  {r.private && (
                    <span className="ml-2 rounded bg-surface-2 px-1.5 py-0.5 text-xs text-ink-3">
                      private
                    </span>
                  )}
                </TableCell>
                <TableCell className="w-24 py-1.5 text-right text-ink-3">
                  {r.size == null ? "" : formatBytes(r.size)}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}

      <div className="flex gap-2">
        <Button variant="outline" onClick={props.onBack}>
          Back
        </Button>
        <button
          className="text-sm text-brand hover:underline"
          onClick={() => void connect()}
        >
          Install on another account
        </button>
      </div>
    </div>
  );
}

/// Whether the chosen installation can push, said in the picker: an
/// installation approved before `Contents: write` was on the App reads
/// fine and forwards nothing, and the person picking should learn that
/// here, not from the first refused push.
function PickerPushNote(props: { inst: GithubInstallation | undefined }) {
  const status = installationPush(props.inst);
  if (status !== "approve") return null;
  const d = props.inst?.detail;
  const url = d && !("gone" in d) ? d.approve_url : null;
  return (
    <Alert variant="warning" role="status" data-testid="picker-push-note">
      Mirrors made through this installation cannot push back yet:{" "}
      {url ? (
        <a className={PROSE_LINK} href={url} target="_blank" rel="noreferrer">
          {approvePushLine([CONTENTS_WRITE])}
        </a>
      ) : (
        approvePushLine([CONTENTS_WRITE])
      )}
      .
    </Alert>
  );
}

/// Follow the first sync, then hand over the clone command.
///
/// Creation answers 202 and the ingest runs in the background, so this
/// is the difference between "it worked" and a screen that looks stuck.
function FirstSync(props: {
  session: Session;
  repo: string;
  mirror: boolean;
  navigate: (to: string) => void;
}) {
  const { session, repo } = props;
  const [status, setStatus] = useState<SyncStatus | null>(null);
  const [cloneUrl, setCloneUrl] = useState<string | null>(null);

  const poll = useCallback(async () => {
    if (!props.mirror) {
      const r = await api.repo(session, repo);
      setCloneUrl(r.clone_url);
      return true;
    }
    const s = await api.syncStatus(session, repo);
    setStatus(s);
    setCloneUrl(s.clone_url);
    return s.state !== "syncing";
  }, [session, repo, props.mirror]);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const tick = async () => {
      if (!alive) return;
      // A failed poll is not a failed sync — the answer is simply not
      // in yet — so it retries rather than reporting the repository
      // broken.
      const done = await poll().catch(() => false);
      if (!alive || done) return;
      timer = setTimeout(tick, 1500);
    };
    void tick();
    return () => {
      alive = false;
      if (timer) clearTimeout(timer);
    };
  }, [poll]);

  const failed = status?.state === "failed";
  return (
    <div className="space-y-4">
      <h3 className="text-lg font-semibold">{repo}</h3>
      {props.mirror && (
        <p className="text-sm" aria-live="polite">
          {status === null
            ? "Starting…"
            : status.state === "syncing"
              ? "Mirroring — reading refs and ingesting objects. Large origins take a minute."
              : failed
                ? "The first sync failed."
                : `Mirrored${status.commit ? ` at ${status.commit.slice(0, 7)}` : ""}.`}
        </p>
      )}
      {failed && status?.error && (
        <Alert variant="destructive">{status.error}</Alert>
      )}
      {cloneUrl && !failed && (
        <div>
          <p className="mb-1 text-sm font-medium">Clone it</p>
          <code className="block overflow-x-auto rounded-md border border-borderline bg-surface-1 px-3 py-2 font-mono text-xs">
            git clone {cloneUrl}
          </code>
        </div>
      )}
      {/* The end of the mirror flow is a remote, not only a clone: a
          push here is forwarded to the origin, so agents and CI can be
          pointed at Weft without anything moving. Said only when the
          server says the push will go through. */}
      {cloneUrl && !failed && status?.push?.forwarding && (
        <div data-testid="push-through">
          <p className="mb-1 text-sm font-medium">Push to it</p>
          <code className="block overflow-x-auto rounded-md border border-borderline bg-surface-1 px-3 py-2 font-mono text-xs">
            {remoteAddLine(cloneUrl)}
          </code>
          <p className="mt-1 text-sm text-ink-3">
            {forwardingLine(status.origin)}
          </p>
        </div>
      )}
      {cloneUrl && !failed && status?.push && status.push.needs_permission && (
        <Alert variant="warning" role="status" data-testid="push-needs-permission">
          Pushes to it will be refused until you{" "}
          {status.push.approve_url ? (
            <a
              className={PROSE_LINK}
              href={status.push.approve_url}
              target="_blank"
              rel="noreferrer"
            >
              {approvePushLine([CONTENTS_WRITE]).replace(/^Approve/, "approve")}
            </a>
          ) : (
            approvePushLine([CONTENTS_WRITE]).replace(/^Approve/, "approve")
          )}
          .
        </Alert>
      )}
      <div className="flex gap-2">
        <Button onClick={() => navigateTo(href([session.org, repo]))}>
          Open {repo}
        </Button>
        <Button variant="outline" onClick={() => props.navigate("/")}>
          Done
        </Button>
      </div>
    </div>
  );
}

function Field(props: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  hint?: string;
}) {
  return (
    <label className="block text-sm">
      <span className="mb-1 block font-medium">{props.label}</span>
      <input
        className="w-full rounded-md border border-borderline bg-surface-0 px-2 py-1.5 text-sm"
        value={props.value}
        placeholder={props.placeholder}
        onChange={(e) => props.onChange(e.target.value)}
      />
      {props.hint && (
        <span className="mt-1 block text-ink-3">{props.hint}</span>
      )}
    </label>
  );
}

/// The repository name to suggest from whatever was pasted.
///
/// Exported because the shapes it has to cope with — a browser URL with
/// `/tree/main` still on it, an scp-style remote, a bare `owner/repo` —
/// are worth pinning in tests rather than discovering from a support
/// message.
export function nameFromOrigin(origin: string): string {
  const s = origin
    .trim()
    .replace(/\.git$/, "")
    .replace(/\/+$/, "");
  if (!s) return "";
  // Everything after the host, with any GitHub browser suffix removed.
  const path = s
    .replace(/^[a-z]+:\/\//i, "")
    .replace(/^git@/, "")
    .replace(":", "/")
    .split(/[?#]/)[0]
    .split("/")
    .filter(Boolean);
  const at = path.findIndex((p) => p === "tree" || p === "blob");
  const parts = at > 0 ? path.slice(0, at) : path;
  return parts[parts.length - 1] ?? "";
}
