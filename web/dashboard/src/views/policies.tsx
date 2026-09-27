import { useCallback, useEffect, useState } from "react";
import {
  api,
  type BranchProtection,
  type Repo,
  type RequiredCheck,
  type Session,
} from "@/api";
import { Button } from "@/components/ui/button";

/// The checks one branch requires before a change may land on it.
///
/// Nested under the branch rather than given a picker of its own, because
/// the server will not have it any other way: `require_check` answers 409
/// on a branch nothing has protected — "protect it first, or a required
/// check is one anyone can push past" — so a protected row is the only
/// place a requirement can exist. Attaching the list there makes the
/// precondition part of the shape rather than a rule to read about, and
/// saves a reader from choosing a branch twice.
///
/// The names come from `check_runs`, which is the same namespace the gate
/// matches against — `changes.rs` says so — so a suggestion here is a name
/// that has genuinely reported on this repository. That matters more than
/// convenience: a required name nothing reports is legal, silent, and
/// holds every change on the branch until the queue's wait budget runs
/// out, and `ci/tets` looks exactly like a pipeline that has not started.
function RequiredChecks(props: {
  session: Session;
  repo: string;
  branch: string;
  canEdit: boolean;
  /// Names seen on this repository's Checks tab, to offer as suggestions.
  known: string[];
}) {
  const { session, repo, branch, canEdit } = props;
  const [rows, setRows] = useState<RequiredCheck[] | null>(null);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    api
      .requiredChecks(session, repo, branch)
      .then(setRows)
      .catch((e) => setError(String(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token, repo, branch]);

  useEffect(refresh, [refresh]);

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

  const listId = `known-checks-${branch}`;
  // Only names not already required — a suggestion that is already on the
  // list adds nothing and the server answers it with a duplicate.
  const required = new Set((rows ?? []).map((r) => r.name));
  const suggestions = props.known.filter((k) => !required.has(k));

  return (
    <div className="mt-2 border-l border-borderline pl-3">
      <div className="text-xs text-ink-2">Required checks</div>
      {!rows && <p className="mt-1 text-xs text-ink-3">Loading…</p>}
      {rows && rows.length === 0 && (
        <p className="mt-1 text-xs text-ink-3">
          None. A change lands as soon as it is approved, whatever CI said.
        </p>
      )}
      {rows && rows.length > 0 && (
        <ul className="mt-1 space-y-1">
          {rows.map((r) => (
            <li key={r.name} className="flex items-center gap-2 text-xs">
              <span aria-hidden className="text-ink-3">
                ✓
              </span>
              <span className="font-mono">{r.name}</span>
              <span className="text-ink-3">must pass</span>
              {canEdit && (
                <Button
                  type="button"
                  variant="outline"
                  size="xs"
                  className="ml-auto"
                  disabled={busy}
                  onClick={() =>
                    act(
                      () => api.unrequireCheck(session, repo, branch, r.name),
                      `stop requiring ${r.name} on ${branch}`,
                    )
                  }
                >
                  Stop requiring {r.name}
                </Button>
              )}
            </li>
          ))}
        </ul>
      )}
      {canEdit && (
        <form
          className="mt-2 flex items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            const n = name.trim();
            if (!n) return;
            act(async () => {
              await api.requireCheck(session, repo, branch, n);
              setName("");
            }, `require ${n} on ${branch}`);
          }}
        >
          <input
            className="w-52 rounded-md border border-borderline bg-surface-0 px-2 py-1 text-xs"
            aria-label={`Check to require on ${branch}`}
            placeholder="check name, e.g. ci/tests"
            list={listId}
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          {/* Suggestions, not a closed list: a maintainer wiring up a
              pipeline that has not run yet must still be able to name it,
              and a `select` would make that impossible. */}
          <datalist id={listId}>
            {suggestions.map((k) => (
              <option key={k} value={k} />
            ))}
          </datalist>
          <Button
            type="submit"
            variant="outline"
            className="px-2.5 py-1 text-xs"
            disabled={busy || !name.trim()}
          >
            Require
          </Button>
        </form>
      )}
      {error && (
        <p className="mt-1 text-xs text-serious" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

/// Branch policy: the default branch (where clones start and changes
/// land) and the protected set (branches only the land queue may move).
/// Everyone can read the fence; only an admin can move it, so the
/// mutating forms appear only when the admin probe succeeds.
export function PoliciesPanel(props: {
  session: Session;
  repo: string;
  info: Repo;
  onRepo: (r: Repo) => void;
}) {
  const { session, repo, info } = props;
  const [prot, setProt] = useState<BranchProtection[] | null>(null);
  const [canEdit, setCanEdit] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [toProtect, setToProtect] = useState("");
  const [newDefault, setNewDefault] = useState("");
  /// Check names this repository has actually seen, to suggest when
  /// requiring one. A failure here costs nothing but the suggestions —
  /// the field still accepts any name — so it never reaches `error`.
  const [known, setKnown] = useState<string[]>([]);

  const refresh = useCallback(() => {
    api
      .protections(session, repo)
      .then(setProt)
      .catch((e) => setError(String(e)));
    // Same admin probe AccessPanel uses: a viewer or member gets the
    // read-only view, never a form that can only 404.
    api
      .access(session, repo)
      .then(() => setCanEdit(true))
      .catch(() => setCanEdit(false));
    api
      .checkRuns(session, repo, { limit: 1 })
      .then((r) => setKnown(r.workflows))
      .catch(() => setKnown([]));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token, repo]);

  useEffect(refresh, [refresh]);

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

  return (
    <div className="rounded-lg border border-borderline bg-surface-1 p-4">
      <div className="mb-1 text-sm font-medium text-ink">Branch policy</div>
      <p className="mb-3 text-xs text-ink-3">
        A protected branch moves only through the land queue — direct pushes,
        API commits, resets and deletes are refused with the same sentence
        everywhere. Naming a check as required holds a change in that queue
        until the check passes, rather than landing it on an approval alone.
        This is repo policy, set by this repo&apos;s admins; org-wide people,
        teams and tokens live under Settings.
      </p>

      <div className="mb-4 flex flex-wrap items-center gap-2 text-sm">
        <span className="text-ink-2">Default branch</span>
        <span className="rounded bg-surface-2 px-2 py-0.5 font-mono text-xs">
          {info.default_branch}
        </span>
        {canEdit && (
          <form
            className="flex items-center gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              const b = newDefault.trim();
              if (!b) return;
              act(async () => {
                const r = await api.setDefaultBranch(session, repo, b);
                props.onRepo(r);
                setNewDefault("");
              }, "set the default branch");
            }}
          >
            <input
              className="w-44 rounded-md border border-borderline bg-surface-0 px-2 py-1 text-sm"
              aria-label="New default branch"
              placeholder="move to branch…"
              value={newDefault}
              onChange={(e) => setNewDefault(e.target.value)}
            />
            <Button
              type="submit"
              variant="outline"
              className="px-2.5 py-1"
              disabled={busy || !newDefault.trim()}
            >
              Set default
            </Button>
          </form>
        )}
      </div>

      <div className="mb-2 text-sm text-ink-2">Protected branches</div>
      {!prot && <p className="text-sm text-ink-3">Loading…</p>}
      {prot && prot.length === 0 && (
        <p className="text-sm text-ink-3">
          None. Anyone with write access can push {info.default_branch} directly
          — protect it to make review the only road to trunk.
        </p>
      )}
      {prot && prot.length > 0 && (
        <ul className="space-y-3">
          {prot.map((p) => (
            <li key={p.branch}>
              <div className="flex items-center gap-2 text-sm">
                <span aria-hidden className="text-good">
                  ⛨
                </span>
                <span className="font-mono text-xs">{p.branch}</span>
                <span className="text-xs text-ink-3">
                  lands through review only
                </span>
                {canEdit && (
                  <Button
                    type="button"
                    variant="outline"
                    size="xs"
                    className="ml-auto"
                    disabled={busy}
                    onClick={() =>
                      act(
                        () => api.unprotect(session, repo, p.branch),
                        `unprotect ${p.branch}`,
                      )
                    }
                  >
                    Unprotect {p.branch}
                  </Button>
                )}
              </div>
              <RequiredChecks
                session={session}
                repo={repo}
                branch={p.branch}
                canEdit={canEdit}
                known={known}
              />
            </li>
          ))}
        </ul>
      )}
      {/* Why there is no requirements form until something is protected.
          The server refuses a requirement on an unprotected branch —
          "protect it first, or a required check is one anyone can push
          past" — so a form here could only ever 409, and a control that
          cannot do what it says is worse than an absent one. Saying the
          order out loud is what an admin actually needs: the two settings
          look independent and are not. */}
      {prot && !prot.some((p) => p.branch === info.default_branch) && (
        <p className="mt-2 text-xs text-ink-3">
          Requiring a check needs a protected branch first — on one anyone can
          push to, the queue is not the only road in and a requirement is
          bypassable.
        </p>
      )}
      {canEdit && (
        <form
          className="mt-3 flex items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            const b = toProtect.trim();
            if (!b) return;
            act(async () => {
              await api.protect(session, repo, b);
              setToProtect("");
            }, `protect ${b}`);
          }}
        >
          <input
            className="w-44 rounded-md border border-borderline bg-surface-0 px-2 py-1 text-sm"
            aria-label="Branch to protect"
            placeholder="branch, e.g. main"
            value={toProtect}
            onChange={(e) => setToProtect(e.target.value)}
          />
          <Button
            type="submit"
            variant="outline"
            className="px-2.5 py-1"
            disabled={busy || !toProtect.trim()}
          >
            Protect
          </Button>
        </form>
      )}
      {error && (
        <p className="mt-2 text-sm text-serious" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
