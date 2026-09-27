import { useCallback, useEffect, useState } from "react";
import {
  api,
  type Repo,
  type Runner,
  type RunnerGroup,
  type RunnerPolicy,
  type RunnerRegistrationToken,
  type Session,
} from "@/api";
import { Err, Loading } from "@/components/feedback";
import { Panel } from "@/components/panel";
import { RelativeTime } from "@/components/relative-time";
import { RunnerLabels } from "@/components/runner-labels";
import { Badge } from "@/components/ui/badge";
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
import { STRUCTURAL_LINK } from "@/lib/links";
import {
  TOKEN_EXPIRY_NOTE,
  groupAccessLine,
  orderedLabels,
  registrationCommands,
  runnerJobHref,
  runnerStatePresentation,
} from "@/lib/runners";
import { cn } from "@/lib/utils";

/// Settings → Runners: where this organisation's jobs are allowed to
/// run, and on whose machines.
///
/// Four panels, in the order somebody actually meets them: the policy
/// that decides whether runners are possible at all, the groups that
/// decide which repositories reach the organisation's machines, the
/// runners themselves, and the two commands that add one.
///
/// Every refusal is rendered as the server's own sentence. The trigger
/// refusals this page exists to prevent — "no runner group admits this
/// repository; add it to a group under Settings → Runners" — name this
/// page by name, so a reader arriving from one is looking for the
/// control that answers it, and a paraphrased error would leave them
/// guessing which of four panels was the one that said no.
export function RunnersPanel(props: { session: Session }) {
  const { session } = props;
  const [repos, setRepos] = useState<Repo[] | null>(null);
  const [groups, setGroups] = useState<RunnerGroup[] | null>(null);

  useEffect(() => {
    let alive = true;
    api
      .repos(session)
      .then((r) => alive && setRepos(r))
      // A token session without `org:read` on repositories still gets a
      // usable page: the multi-selects go empty and say so, rather than
      // the whole section refusing to render.
      .catch(() => alive && setRepos([]));
    return () => {
      alive = false;
    };
  }, [session]);

  const refreshGroups = useCallback(() => {
    api
      .runnerGroups(session)
      .then(setGroups)
      .catch(() => setGroups([]));
  }, [session]);
  useEffect(refreshGroups, [refreshGroups]);

  const names = (repos ?? []).map((r) => r.name);

  return (
    <div className="space-y-4">
      <PolicyPanel session={session} repos={names} />
      <GroupsPanel
        session={session}
        repos={names}
        groups={groups}
        onChanged={refreshGroups}
      />
      <RunnersTablePanel session={session} onChanged={refreshGroups} />
      <AddRunnerPanel session={session} groups={groups ?? []} />
    </div>
  );
}

/// Which repositories may run jobs on the organisation's runners at all.
function PolicyPanel(props: { session: Session; repos: string[] }) {
  const { session } = props;
  const [policy, setPolicy] = useState<RunnerPolicy | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    let alive = true;
    api
      .runnerPolicy(session)
      .then((p) => alive && setPolicy(p))
      .catch(
        (e) => alive && setError(`Could not read the runner policy: ${say(e)}`),
      );
    return () => {
      alive = false;
    };
  }, [session]);

  async function save() {
    if (!policy) return;
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      setPolicy(
        await api.updateRunnerPolicy(session, {
          self_hosted: policy.self_hosted,
          self_hosted_repos: policy.self_hosted_repos,
        }),
      );
      setSaved(true);
    } catch (e) {
      setError(`Could not save the runner policy: ${say(e)}`);
    } finally {
      setBusy(false);
    }
  }

  /// Any edit clears "Saved". A tick left standing beside a control
  /// somebody has since changed is a lie about what the server holds.
  function edit(patch: Partial<RunnerPolicy>) {
    setSaved(false);
    setPolicy((p) => (p ? { ...p, ...patch } : p));
  }

  if (!policy) {
    return (
      <Panel title="Runner policy">
        {error ? <Err message={error} /> : <Loading />}
      </Panel>
    );
  }

  return (
    <Panel
      title="Runner policy"
      hint="Which of this organisation's repositories may run jobs on its runners. A workflow this policy refuses fails with the reason, and nothing lifts it by itself."
    >
      <Err message={error} />
      <div className="space-y-4">
        <label className="flex flex-col gap-1 text-xs text-ink-3">
          Self-hosted runners
          <Select
            value={policy.self_hosted}
            onValueChange={(v) =>
              edit({ self_hosted: v as RunnerPolicy["self_hosted"] })
            }
          >
            <SelectTrigger aria-label="Self-hosted runners">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">All repositories</SelectItem>
              <SelectItem value="selected">Selected repositories</SelectItem>
              <SelectItem value="disabled">Disabled</SelectItem>
            </SelectContent>
          </Select>
        </label>
        {policy.self_hosted === "selected" && (
          <RepoPicker
            legend="Repositories that may use self-hosted runners"
            repos={props.repos}
            chosen={policy.self_hosted_repos}
            onChange={(self_hosted_repos) => edit({ self_hosted_repos })}
          />
        )}
        <div className="flex items-center gap-3">
          <Button disabled={busy} onClick={save}>
            {busy ? "Saving…" : "Save policy"}
          </Button>
          {saved && (
            <span className="text-xs text-good" role="status">
              Saved
            </span>
          )}
        </div>
      </div>
    </Panel>
  );
}

/// A checkbox per repository, rather than a multi-select widget.
///
/// Plain checkboxes because this list is read as much as it is edited —
/// "which repositories can reach the build farm" is a question somebody
/// answers by looking — and a collapsed multi-select answers it with a
/// count. They are also the one control a keyboard and a screen reader
/// both already understand without any work from us.
function RepoPicker(props: {
  legend: string;
  repos: string[];
  chosen: string[];
  onChange: (next: string[]) => void;
}) {
  const chosen = new Set(props.chosen);
  return (
    <fieldset className="rounded-md border border-borderline p-3">
      <legend className="px-1 text-xs text-ink-3">{props.legend}</legend>
      {props.repos.length === 0 ? (
        <p className="text-sm text-ink-3">
          No repositories to choose from yet.
        </p>
      ) : (
        <div className="max-h-48 overflow-y-auto">
          {props.repos.map((name) => (
            <label
              key={name}
              className="flex items-center gap-2 py-0.5 text-sm text-ink-2"
            >
              <input
                type="checkbox"
                checked={chosen.has(name)}
                aria-label={name}
                onChange={(e) =>
                  props.onChange(
                    e.target.checked
                      ? [...props.chosen, name]
                      : props.chosen.filter((r) => r !== name),
                  )
                }
              />
              <span className="min-w-0 truncate">{name}</span>
            </label>
          ))}
        </div>
      )}
    </fieldset>
  );
}

/// The groups, and the one form that edits any of them.
function GroupsPanel(props: {
  session: Session;
  repos: string[];
  groups: RunnerGroup[] | null;
  onChanged: () => void;
}) {
  const { session, groups } = props;
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [name, setName] = useState("");
  const [editing, setEditing] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  /// The group a successful save was about. The editor closes on save,
  /// and a save that failed and closed looks the same to a person — so
  /// the panel says the server took it, and which group it was.
  const [saved, setSaved] = useState<string | null>(null);

  async function act(what: () => Promise<unknown>, verb: string) {
    setError(null);
    setSaved(null);
    setBusy(true);
    try {
      await what();
      props.onChanged();
      return true;
    } catch (e) {
      setError(`Could not ${verb}: ${say(e)}`);
      return false;
    } finally {
      setBusy(false);
    }
  }

  return (
    <Panel
      title="Runner groups"
      hint="A group decides which repositories may reach the runners in it. Every runner is in exactly one; removing a group moves its runners to the default."
    >
      <Err message={error} />
      {saved && (
        <p className="mb-3 text-xs text-good" role="status">
          Saved {saved}
        </p>
      )}
      {!groups ? (
        <Loading />
      ) : groups.length === 0 ? (
        <p className="text-sm text-ink-3">No runner groups yet.</p>
      ) : (
        <div className="overflow-x-auto">
          <Table>
            <TableHeader>
              <TableHeadRow>
                <TableHead>Group</TableHead>
                <TableHead>Repository access</TableHead>
                <TableHead>Runners</TableHead>
                <TableHead />
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {groups.map((g) => (
                <TableRow key={g.id}>
                  <TableCell className="max-w-[16rem]">
                    <span className="block min-w-0 truncate">{g.name}</span>
                    {g.is_default && (
                      <span className="text-xs text-ink-3">
                        the default group
                      </span>
                    )}
                  </TableCell>
                  <TableCell className="text-ink-2">
                    {groupAccessLine(g)}
                  </TableCell>
                  <TableCell className="font-mono text-ink-2">
                    {g.runners}
                  </TableCell>
                  <TableCell className="text-right">
                    {confirming === g.id ? (
                      <span className="flex flex-wrap items-center justify-end gap-2">
                        <span className="text-xs text-ink-3">
                          Delete {g.name}? Its {g.runners}{" "}
                          {g.runners === 1 ? "runner moves" : "runners move"} to
                          the default group.
                        </span>
                        <Button
                          variant="destructive"
                          size="xs"
                          disabled={busy}
                          onClick={async () => {
                            if (
                              await act(
                                () => api.deleteRunnerGroup(session, g.id),
                                "delete that group",
                              )
                            )
                              setConfirming(null);
                          }}
                        >
                          Delete the group
                        </Button>
                        <Button
                          variant="ghost"
                          size="xs"
                          disabled={busy}
                          onClick={() => setConfirming(null)}
                        >
                          Keep it
                        </Button>
                      </span>
                    ) : (
                      <span className="flex flex-wrap items-center justify-end gap-2">
                        <Button
                          variant="outline"
                          size="xs"
                          onClick={() => {
                            setSaved(null);
                            setEditing(editing === g.id ? null : g.id);
                          }}
                        >
                          {editing === g.id ? "Close" : "Edit"}
                        </Button>
                        {/* The default group is never offered a Delete.
                            The server answers 422 for it, and a button
                            that cannot do what it says is worse than an
                            absent one. */}
                        {!g.is_default && (
                          <Button
                            variant="destructive"
                            size="xs"
                            onClick={() => setConfirming(g.id)}
                          >
                            Delete
                          </Button>
                        )}
                      </span>
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      )}

      {groups
        ?.filter((g) => g.id === editing)
        .map((g) => (
          <GroupEditor
            key={g.id}
            group={g}
            repos={props.repos}
            busy={busy}
            onSave={async (body) => {
              if (
                await act(
                  () => api.updateRunnerGroup(session, g.id, body),
                  "save that group",
                )
              ) {
                setEditing(null);
                setSaved(body.name ?? g.name);
              }
            }}
            onCancel={() => setEditing(null)}
          />
        ))}

      <form
        className="mt-4 flex flex-wrap items-end gap-2"
        onSubmit={async (e) => {
          e.preventDefault();
          const n = name.trim();
          if (!n) return;
          if (
            await act(
              () => api.createRunnerGroup(session, { name: n }),
              "create that group",
            )
          )
            setName("");
        }}
      >
        <label className="flex flex-col gap-1 text-xs text-ink-3">
          New group
          <input
            className="rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 text-sm"
            aria-label="Group name"
            placeholder="build-farm"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </label>
        <Button variant="outline" disabled={busy || !name.trim()}>
          Create group
        </Button>
      </form>
    </Panel>
  );
}

/// One group's editable half, opened in place under the table.
function GroupEditor(props: {
  group: RunnerGroup;
  repos: string[];
  busy: boolean;
  onSave: (body: {
    name?: string;
    repo_access?: RunnerGroup["repo_access"];
    repos?: string[];
  }) => void;
  onCancel: () => void;
}) {
  const { group } = props;
  const [name, setName] = useState(group.name);
  const [access, setAccess] = useState(group.repo_access);
  const [repos, setRepos] = useState(group.repos);

  return (
    <div className="mt-4 space-y-3 rounded-md border border-borderline p-3">
      <label className="flex flex-col gap-1 text-xs text-ink-3">
        Group name
        <input
          className="rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 text-sm disabled:opacity-60"
          aria-label="Group name to edit"
          value={name}
          // The default group's name is fixed server-side (422), so the
          // field is disabled rather than allowed to fail on Save.
          disabled={group.is_default}
          onChange={(e) => setName(e.target.value)}
        />
        {group.is_default && (
          <span className="text-ink-3">
            The default group's name cannot be changed.
          </span>
        )}
      </label>
      <label className="flex flex-col gap-1 text-xs text-ink-3">
        Repository access
        <Select
          value={access}
          onValueChange={(v) => setAccess(v as RunnerGroup["repo_access"])}
        >
          <SelectTrigger aria-label="Repository access">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">All repositories</SelectItem>
            <SelectItem value="selected">Selected repositories</SelectItem>
          </SelectContent>
        </Select>
      </label>
      {access === "selected" && (
        <RepoPicker
          legend="Repositories in this group"
          repos={props.repos}
          chosen={repos}
          onChange={setRepos}
        />
      )}
      <div className="flex flex-wrap gap-2">
        <Button
          disabled={props.busy}
          onClick={() =>
            props.onSave({
              ...(group.is_default ? {} : { name: name.trim() }),
              repo_access: access,
              repos,
            })
          }
        >
          {props.busy ? "Saving…" : "Save group"}
        </Button>
        <Button variant="ghost" disabled={props.busy} onClick={props.onCancel}>
          Cancel
        </Button>
      </div>
    </div>
  );
}

/// The machines themselves.
function RunnersTablePanel(props: { session: Session; onChanged: () => void }) {
  const { session } = props;
  const [runners, setRunners] = useState<Runner[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState<string | null>(null);

  const refresh = useCallback(() => {
    api
      .runners(session)
      .then(setRunners)
      .catch((e) => setError(`Could not read the runners: ${say(e)}`));
  }, [session]);
  useEffect(refresh, [refresh]);

  async function remove(r: Runner) {
    setError(null);
    setBusy(true);
    try {
      await api.removeRunner(session, r.id);
      setConfirming(null);
      refresh();
      // The group counts move with it.
      props.onChanged();
    } catch (e) {
      setError(`Could not remove that runner: ${say(e)}`);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Panel
      title="Runners"
      hint="Machines registered to this organisation. A runner is online when it has checked in within the last minute; removing one kills its credential and fails whatever it was running."
    >
      <Err message={error} />
      {!runners ? (
        <Loading />
      ) : runners.length === 0 ? (
        <p className="text-sm text-ink-3">
          No runners are registered. Add one below.
        </p>
      ) : (
        <div className="overflow-x-auto">
          <Table>
            <TableHeader>
              <TableHeadRow>
                <TableHead>Runner</TableHead>
                <TableHead>Labels</TableHead>
                <TableHead>State</TableHead>
                <TableHead>Last seen</TableHead>
                <TableHead>Group</TableHead>
                <TableHead />
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {runners.map((r) => (
                <RunnerRow
                  key={r.id}
                  org={session.org}
                  runner={r}
                  busy={busy}
                  confirming={confirming === r.id}
                  onArm={() => setConfirming(r.id)}
                  onDismiss={() => setConfirming(null)}
                  onRemove={() => remove(r)}
                />
              ))}
            </TableBody>
          </Table>
        </div>
      )}
    </Panel>
  );
}

function RunnerRow(props: {
  org: string;
  runner: Runner;
  busy: boolean;
  confirming: boolean;
  onArm: () => void;
  onDismiss: () => void;
  onRemove: () => void;
}) {
  const { runner: r } = props;
  const state = runnerStatePresentation(r.state);
  const href = r.job ? runnerJobHref(props.org, r.job) : null;
  return (
    <TableRow>
      <TableCell className="max-w-[14rem]">
        <span className="block min-w-0 truncate font-medium text-ink">
          {r.name}
        </span>
        <span className="mt-0.5 flex flex-wrap items-center gap-x-2 text-xs text-ink-3">
          <span className="font-mono">
            {r.os}/{r.arch}
          </span>
          <span className="font-mono">v{r.version}</span>
          {/* An ephemeral runner disappears after one job. A row that
              vanished between two reads with nothing to explain it
              reads as a machine that fell over. */}
          {r.ephemeral && <Badge variant="neutral">ephemeral</Badge>}
        </span>
      </TableCell>
      <TableCell className="max-w-[18rem]">
        <RunnerLabels labels={orderedLabels(r.labels, r.os, r.arch)} />
      </TableCell>
      <TableCell>
        {/* The word, always. Never the tone alone — DESIGN.md. */}
        <Badge variant={state.variant}>{state.label}</Badge>
        {r.job && (
          <span className="mt-1 block min-w-0 truncate text-xs text-ink-3">
            running{" "}
            {href ? (
              <a className={cn(STRUCTURAL_LINK, "font-mono")} href={href}>
                {r.job.key}
              </a>
            ) : (
              <span className="font-mono">{r.job.key}</span>
            )}
          </span>
        )}
      </TableCell>
      <TableCell className="text-ink-2">
        <RelativeTime at={r.last_seen_at} />
      </TableCell>
      <TableCell className="max-w-[10rem] text-ink-2">
        <span className="block min-w-0 truncate">{r.group.name}</span>
      </TableCell>
      <TableCell className="text-right">
        {props.confirming ? (
          <span className="flex flex-wrap items-center justify-end gap-2">
            {/* Said in full, because the two consequences are different
                facts and only one of them is obvious. */}
            <span className="text-xs text-ink-3">
              Remove {r.name}? Its credential stops working and anything it is
              running fails.
            </span>
            <Button
              variant="destructive"
              size="xs"
              disabled={props.busy}
              onClick={props.onRemove}
            >
              Remove the runner
            </Button>
            <Button
              variant="ghost"
              size="xs"
              disabled={props.busy}
              onClick={props.onDismiss}
            >
              Keep it
            </Button>
          </span>
        ) : (
          <Button
            variant="destructive"
            size="xs"
            aria-label={`Remove ${r.name}`}
            onClick={props.onArm}
          >
            Remove
          </Button>
        )}
      </TableCell>
    </TableRow>
  );
}

/// The two commands, and the credential that makes them work.
function AddRunnerPanel(props: { session: Session; groups: RunnerGroup[] }) {
  const { session } = props;
  const [minted, setMinted] = useState<RunnerRegistrationToken | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [group, setGroup] = useState("");
  const [copied, setCopied] = useState(false);

  async function mint() {
    setBusy(true);
    setError(null);
    setCopied(false);
    try {
      setMinted(
        await api.mintRunnerRegistrationToken(session, group || undefined),
      );
    } catch (e) {
      setError(`Could not mint a registration token: ${say(e)}`);
    } finally {
      setBusy(false);
    }
  }

  // `window.location.origin` and not the server's composed command: this
  // is the address the operator demonstrably reached, and a stale
  // STRATUM_PUBLIC_URL would otherwise hand out a command that fails
  // with a connection error and nothing to explain it.
  const commands = minted
    ? registrationCommands(window.location.origin, minted.token)
    : "";

  return (
    <Panel
      title="Add a runner"
      hint="Run these two commands on the machine. The first registers it and writes a long-lived credential; the second starts taking jobs."
    >
      <Err message={error} />
      <div className="flex flex-wrap items-end gap-2">
        {props.groups.length > 0 && (
          <label className="flex flex-col gap-1 text-xs text-ink-3">
            Group
            <Select value={group} onValueChange={setGroup}>
              <SelectTrigger aria-label="Register into group">
                <SelectValue placeholder="default" />
              </SelectTrigger>
              <SelectContent>
                {props.groups.map((g) => (
                  <SelectItem key={g.id} value={g.name}>
                    {g.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </label>
        )}
        <Button disabled={busy} onClick={mint}>
          {busy ? "Minting…" : minted ? "Mint another token" : "Add a runner"}
        </Button>
      </div>

      {minted && (
        <div className="mt-4 rounded-lg border border-borderline bg-surface-1 p-4">
          <div className="mb-2 flex items-center justify-between gap-2">
            <span className="text-sm font-medium text-ink">
              Run these on the machine
            </span>
            <button
              type="button"
              className="shrink-0 rounded-md border border-borderline px-2.5 py-1.5 text-xs font-medium text-ink-2 hover:text-ink"
              onClick={async () => {
                try {
                  await navigator.clipboard.writeText(commands);
                  setCopied(true);
                  setTimeout(() => setCopied(false), 1500);
                } catch {
                  /* clipboard unavailable (permissions, http) — the
                     text stays selectable */
                }
              }}
            >
              {copied ? "Copied" : "Copy"}
            </button>
          </div>
          <pre
            aria-label="Runner registration commands"
            tabIndex={0}
            className="overflow-x-auto rounded-md bg-surface-2 px-3 py-2 font-mono text-xs text-ink-2"
          >
            {commands}
          </pre>
          <p className="mt-2 text-xs text-ink-3" role="status">
            {TOKEN_EXPIRY_NOTE} It registers one machine into the{" "}
            <span className="font-mono">{minted.group}</span> group and cannot
            be used twice.
          </p>
        </div>
      )}
    </Panel>
  );
}

/// The server's own sentence, whatever shape the failure arrived in.
function say(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
