import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";
import {
  api,
  type ImportStatus,
  type Repo,
  type Session,
  type Webhook,
} from "@/api";
import { Err, Loading } from "@/components/feedback";
import { NotFound } from "@/components/not-found";
import { Panel } from "@/components/panel";
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
import { AccessPanel } from "@/views/access";
import { PoliciesPanel } from "@/views/policies";
import { href } from "@/router";
import { formatAgo } from "@/format";
import { guide, PROSE_LINK } from "@/lib/links";

/// What `GET …/ci/secret` answers, and the whole of it.
///
/// Spelled here rather than inferred, because the shape is the design
/// constraint: `configured` and a timestamp, and **no field that could
/// carry the secret**. `rotated_at` is absent, not null, when nothing is
/// configured.
type CiIntakeStatus = { configured: boolean; rotated_at?: number };

/// A confirmation that costs the same as the mistake it prevents.
///
/// GitHub asks you to type the repository's name before anything
/// irreversible, and the reason is not ceremony: the dangerous actions
/// sit on a page you reached to do something else, and a single
/// mis-aimed click is otherwise the whole distance between "editing a
/// description" and "the repository is gone". Typing the name is the
/// one confirmation that cannot be satisfied by the same reflex that
/// caused the error.
function TypeToConfirm(props: {
  /// The word that must be typed — the repository's own name.
  name: string;
  trigger: string;
  title: string;
  description: string;
  action: string;
  busy: boolean;
  onConfirm: () => void;
}) {
  const [typed, setTyped] = useState("");
  const [open, setOpen] = useState(false);
  const inputId = `confirm-${props.action.replace(/\W+/g, "-").toLowerCase()}`;
  return (
    <AlertDialog
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        // A half-typed name must not survive a cancel: reopening the
        // dialog with the confirmation already satisfied is the guard
        // undoing itself.
        if (!o) setTyped("");
      }}
    >
      <AlertDialogTrigger asChild>
        <Button type="button" variant="destructive" disabled={props.busy}>
          {props.trigger}
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{props.title}</AlertDialogTitle>
          <AlertDialogDescription>{props.description}</AlertDialogDescription>
        </AlertDialogHeader>
        <label className="text-xs text-ink-3" htmlFor={inputId}>
          Type{" "}
          <code className="rounded bg-surface-2 px-1.5 py-0.5 font-mono">
            {props.name}
          </code>{" "}
          to confirm.
        </label>
        <Input
          id={inputId}
          value={typed}
          autoComplete="off"
          onChange={(e) => setTyped(e.target.value)}
        />
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction
            disabled={typed !== props.name || props.busy}
            onClick={props.onConfirm}
          >
            {props.action}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

/// The repository's own settings, in GitHub's information architecture:
/// what it says about itself, who may reach it, how its branches move,
/// and a danger zone at the bottom.
///
/// Everything here is an endpoint the server already has. Rename,
/// transfer and archive are conspicuously absent because there is no
/// route behind them — a control that leads nowhere is worse than one
/// that is missing, because the missing one does not teach a person to
/// distrust the page.
export function RepoSettingsView(props: {
  session: Session;
  owner: string;
  repo: string;
  row: Repo | null;
  /// `viewer_admin` off the repository row, and `null` until that row
  /// has arrived. Handed down rather than asked for again: the server
  /// decides this, from the same `authx` refinement the writes on this
  /// page use, so there is one opinion about it rather than one per
  /// surface.
  admin: boolean | null;
  navigate: (to: string, replace?: boolean) => void;
}) {
  const { session, owner, repo } = props;
  const [info, setInfo] = useState<Repo | null>(props.row);
  useEffect(() => setInfo(props.row), [props.row]);

  if (props.admin === null || !info) return <Loading />;
  // Not "you may not": the same answer a repository gives somebody who
  // may not read it. A page that says "forbidden" tells somebody who is
  // not an admin that there is an admin surface here to come back for.
  if (!props.admin)
    return <NotFound onNavigate={props.navigate} what={`${owner}/${repo}`} />;

  return (
    <div className="space-y-6 py-6">
      <h1 className="border-b border-borderline pb-3 text-xl font-semibold text-ink">
        Settings
      </h1>

      <GeneralPanel
        session={session}
        repo={repo}
        info={info}
        onRepo={setInfo}
      />

      <AccessPanel session={session} repo={repo} />

      {/* A mirror's branches belong to its origin — the API refuses to
          protect one — so the panel that would only ever error is not
          rendered rather than rendered disabled. */}
      {info.kind === "native" && (
        <PoliciesPanel
          session={session}
          repo={repo}
          info={info}
          onRepo={setInfo}
        />
      )}

      {/* Only for a mirror: an import reads the same GitHub origin the
          commits already come from, so a native repository has nothing
          to import *from*. */}
      {info.kind === "mirror" && (
        <ImportPanel
          session={session}
          repo={repo}
          installation={info.origin_installation}
        />
      )}

      {/* The two halves of CI on a repository hosted here, adjacent
          because they are one job: the webhook starts the build, the
          intake secret carries its verdict back. Both are for every
          repository, mirror or not — a mirror's Actions runs are polled
          in through the App, but nothing stops the same project's GitLab
          pipeline or nightly job posting a verdict here too. */}
      <WebhooksPanel session={session} repo={repo} />

      <CiIntakePanel session={session} owner={owner} repo={repo} />

      <DangerZone
        session={session}
        owner={owner}
        repo={repo}
        navigate={props.navigate}
      />
    </div>
  );
}

/// What the repository says about itself: its description and its
/// homepage.
///
/// One form and one save for both fields, because they are one thought.
/// Two forms would mean somebody who edited both and pressed the first
/// Save would silently lose the second edit — and the PATCH takes both
/// in one body anyway, so splitting them would be two round trips to
/// produce one result.
function GeneralPanel(props: {
  session: Session;
  repo: string;
  info: Repo;
  onRepo: (r: Repo) => void;
}) {
  const { info } = props;
  const [text, setText] = useState(info.description ?? "");
  const [home, setHome] = useState(info.homepage ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    setText(info.description ?? "");
  }, [info.description]);

  useEffect(() => {
    setHome(info.homepage ?? "");
  }, [info.homepage]);

  return (
    <Panel
      title="General"
      hint="How this repository introduces itself, wherever it is listed."
    >
      <Err message={error} />
      <form
        className="space-y-2"
        onSubmit={(e) => {
          e.preventDefault();
          setBusy(true);
          setError(null);
          setSaved(false);
          // An empty box is a clear, not a no-op: `null` is what the API
          // reads as removing the description, and `""` would leave a
          // repository describing itself as nothing at all.
          api
            .patchRepo(props.session, props.repo, {
              description: text.trim() || null,
              homepage: home.trim() || null,
            })
            .then((r) => {
              props.onRepo(r);
              setSaved(true);
            })
            .catch((e) =>
              // The server's own sentence, verbatim. It is the one that
              // says *which* field was refused and why — "must begin
              // with http:// or https://" is actionable where "could
              // not save" is a shrug, and a second copy of that rule
              // here would be a second place for it to drift.
              setError(`Could not save: ${e instanceof Error ? e.message : e}`),
            )
            .finally(() => setBusy(false));
        }}
      >
        <label
          className="block text-sm font-medium text-ink"
          htmlFor="settings-description"
        >
          Description
        </label>
        <Input
          id="settings-description"
          maxLength={512}
          value={text}
          placeholder="A short sentence about this repository"
          onChange={(e) => {
            setText(e.target.value);
            setSaved(false);
          }}
        />
        <label
          className="block pt-2 text-sm font-medium text-ink"
          htmlFor="settings-homepage"
        >
          Homepage
        </label>
        {/* `inputMode="url"` and NOT `type="url"`.

            The typed input looks like the right choice and is the wrong
            one, twice over. It makes the browser validate before the
            form submits, so a refused homepage produces a native tooltip
            and the server's own sentence — the one that says which rule
            was broken — never reaches the page at all. That is the
            second copy of the rule this component's comments are about,
            and it is not even the same rule: `type="url"` refuses a
            scheme-less `example.com`, which we also refuse, and
            *accepts* `javascript:alert(1)`, which we do not. Stricter in
            one direction and looser in the other, with the looser
            direction being the security-relevant one.
        
            `inputMode` gets the URL keyboard on a phone, which was the
            only thing worth having, and leaves the validation where the
            single copy of it lives. */}
        <Input
          id="settings-homepage"
          inputMode="url"
          maxLength={512}
          value={home}
          placeholder="https://example.com"
          onChange={(e) => {
            setHome(e.target.value);
            setSaved(false);
          }}
        />
        <p className="text-xs text-ink-3">
          Shown in the About panel beside the description. Must start with
          http:// or https://.
        </p>
        <Button type="submit" disabled={busy}>
          {busy ? "Saving…" : "Save"}
        </Button>
        {saved && (
          <p className="text-xs text-ink-3" role="status">
            Saved.
          </p>
        )}
      </form>
    </Panel>
  );
}

/// The one irreversible thing this server can actually do to a
/// repository.
function DangerZone(props: {
  session: Session;
  owner: string;
  repo: string;
  navigate: (to: string, replace?: boolean) => void;
}) {
  const { session, repo } = props;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const act = useCallback(
    async (what: () => Promise<unknown>, verb: string) => {
      setError(null);
      setBusy(true);
      try {
        await what();
      } catch (e) {
        setError(`Could not ${verb}: ${e instanceof Error ? e.message : e}`);
      } finally {
        setBusy(false);
      }
    },
    [],
  );

  return (
    <section className="rounded-xl border border-serious/40 bg-surface-1 p-6">
      {/* "Danger Zone", capitalised the way GitHub capitalises it. It is
          effectively a proper noun: it is what people scroll to the
          bottom of a settings page looking for, and it is the phrase
          every piece of writing about GitHub uses. */}
      <h2 className="text-sm font-medium text-ink">Danger Zone</h2>
      <p className="mt-1 mb-4 text-xs text-ink-3">
        This asks you to type the repository&apos;s name first.
      </p>
      <Err message={error} />
      <div className="flex flex-wrap items-center gap-3 border-t border-borderline pt-4">
        <div className="min-w-0 flex-1">
          <div className="text-sm font-medium text-ink">
            Delete this repository
          </div>
          <p className="text-xs text-ink-3">
            Once you delete a repository, there is no going back. Please be
            certain. Forks of it survive and are promoted onto storage of their
            own; the name becomes available again immediately.
          </p>
        </div>
        <TypeToConfirm
          name={repo}
          busy={busy}
          trigger="Delete this repository"
          title={`Delete ${repo}?`}
          description="This action cannot be undone. Its issues, changes and history go with it."
          action="Delete this repository"
          onConfirm={() =>
            void act(async () => {
              await api.deleteRepo(session, repo);
              // Somewhere that still exists. Staying on the page would
              // leave the browser pointed at a repository the next
              // request would report as missing, which reads as the
              // delete having failed.
              props.navigate(href([props.owner]), true);
            }, "delete the repository")
          }
        />
      </div>
    </section>
  );
}

/// Import a mirrored project's issues from GitHub.
///
/// The one thing this panel has to communicate, because it is the whole
/// reason to trust the import: **the numbers come with them**. `#4721`
/// in a commit message or somebody's blog post keeps meaning what it
/// says. It is also why the import can only run once into an empty
/// tracker, and saying so before the button is pressed is better than a
/// 409 afterwards.
function ImportPanel(props: {
  session: Session;
  repo: string;
  /// The App installation this mirror came through, or `null` for one
  /// connected by bare URL.
  installation: string | null;
}) {
  const { session, repo } = props;
  const [status, setStatus] = useState<ImportStatus | null>(null);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(() => {
    api
      .importStatus(session, repo)
      .then(setStatus)
      // An adornment on a settings page: failing to read progress hides
      // the progress, it does not break the page around it.
      .catch(() => undefined);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token, repo]);

  useEffect(refresh, [refresh]);

  // Poll while an import is in flight, and **not** otherwise: a settings
  // page that requests forever is a settings page nobody leaves open.
  //
  // "In flight" cannot be read from the status alone, which is the bug
  // the first version had. A freshly started import reports every phase
  // as `null` for a second or two — identical to one that was never
  // started — so a condition keyed on "some phase is non-null" never
  // began polling, and somebody who pressed the button watched it say
  // "not started" while the import ran to completion behind them. The
  // work was fine; the page simply never looked again.
  //
  // So `started` is remembered here, and cleared when the issues phase
  // reports `done`. Between those two, this polls.
  //
  // A **failed** import is finished too, and for a long time this did not
  // know that: `finished` was `issues === "done"` alone, so a refused
  // import — the case the worker writes a careful sentence for — left
  // the panel saying "Importing…" and polling every 1.5 seconds forever,
  // with no reason anywhere on the page. The reason could not be shown
  // even in principle, because the route answered phase cursors and
  // nothing else. It answers `state` and `error` now, and both halves of
  // that are used here.
  const [started, setStarted] = useState(false);
  const failed = status?.state === "failed";
  const finished = status?.issues === "done" || failed;
  const running = started && !finished;
  useEffect(() => {
    if (finished) setStarted(false);
  }, [finished]);
  useEffect(() => {
    if (!running) return;
    const t = setInterval(refresh, 1500);
    return () => clearInterval(t);
  }, [running, refresh]);

  const start = async () => {
    setBusy(true);
    try {
      await api.startImport(session, repo);
      toast.success("Importing issues — their numbers come with them");
      setStarted(true);
      refresh();
    } catch (e) {
      // The server knows which refusal this is: no origin, or a tracker
      // that already has issues. Both are things the person can act on.
      toast.error(String((e as Error)?.message ?? e));
    } finally {
      setBusy(false);
    }
  };

  const phase = (v: string | null) =>
    v === null ? "not started" : v === "done" ? "done" : "in progress";

  // A mirror connected by bare URL syncs commits perfectly well and
  // cannot import issues: reading them needs the GitHub App, and a bare
  // URL carries no installation to read them with.
  //
  // **Explained rather than hidden.** Somebody looking at a mirror of a
  // GitHub project is looking for exactly this feature, and an absent
  // panel tells them it does not exist. A button that answers "this
  // repository has no GitHub origin to import from" after they press it
  // is worse still — which is what the first version of this did,
  // because it gated on `kind === "mirror"` while the server required
  // an installation too.
  if (!props.installation) {
    return (
      <Panel
        title="Import issues from GitHub"
        hint="Reading issues needs the GitHub App, and this mirror was connected with a plain URL. Reconnect it through the App to import its issues; its commits keep syncing either way."
      >
        <p className="text-sm text-ink-3">Not available for this mirror.</p>
      </Panel>
    );
  }

  return (
    <Panel
      title="Import issues from GitHub"
      hint="Issues keep the numbers they had upstream, so an old #reference still means what it says. That is also why an import can only go into a tracker with nothing in it yet."
    >
      <div className="flex flex-wrap items-center gap-4">
        <Button onClick={start} disabled={busy || running}>
          {running ? "Importing…" : "Import issues"}
        </Button>
        {status && (
          <dl className="flex flex-wrap gap-x-6 gap-y-1 text-sm text-ink-3">
            {(
              [
                ["Labels", status.labels],
                ["Milestones", status.milestones],
                ["Issues", status.issues],
              ] as const
            ).map(([label, v]) => (
              <div key={label} className="flex gap-2">
                <dt>{label}</dt>
                <dd className={v === "done" ? "text-good" : undefined}>
                  {phase(v)}
                </dd>
              </div>
            ))}
          </dl>
        )}
      </div>
      {failed && (
        <p role="alert" className="mt-3 text-sm text-serious">
          {/* The worker's own words. A refusal names the permission to
              grant; anything else keeps its status. Neither is improved
              by being restated here as "the import failed". */}
          Import failed: {status?.error ?? "no reason was recorded"}
        </p>
      )}
    </Panel>
  );
}

/// The CI intake secret: the credential a pipeline we do not run signs
/// its verdicts with.
///
/// This panel exists because the API was complete and unreachable. A
/// project on Buildkite or GitLab could report builds all along, and the
/// only way to obtain the credential for it was `curl` against a route
/// nothing in the product mentioned. The Checks tab's empty state named
/// the endpoint and stopped there.
///
/// The one contract that shapes everything here: **`GET` never returns
/// the secret.** The server stores it and reports only whether one is
/// configured and when it last moved; the value comes back exactly once,
/// from the `POST` that mints it. So this is the same idiom as the API
/// tokens page — show it once, say plainly that it will not be shown
/// again, and offer rotation rather than recovery — and not a field that
/// pretends to hold a value it can never read back.
function CiIntakePanel(props: {
  session: Session;
  owner: string;
  repo: string;
}) {
  const { session, owner, repo } = props;
  const [status, setStatus] = useState<CiIntakeStatus | null>(null);
  const [minted, setMinted] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    api
      .ciSecretStatus(session, repo)
      .then(setStatus)
      .catch((e: unknown) =>
        setError(
          `Could not read the intake secret's status: ${
            e instanceof Error ? e.message : e
          }`,
        ),
      );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token, repo]);

  useEffect(refresh, [refresh]);

  // Absolute, because the value of showing it at all is that it can be
  // pasted into a pipeline on another machine. A path alone leaves the
  // reader to guess this deployment's host, which is the one part of it
  // they cannot get wrong quietly.
  const endpoint = `${window.location.origin}/v1/orgs/${owner}/repos/${repo}/ci/checks`;

  async function rotate() {
    setBusy(true);
    setError(null);
    try {
      const out = await api.rotateCiSecret(session, repo);
      setMinted(out.secret);
      refresh();
    } catch (e) {
      setError(`Could not mint: ${e instanceof Error ? e.message : e}`);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Panel
      title="CI checks"
      hint="Weft never runs your code. A pipeline you already have posts its verdict here, signed with this secret, and it appears on this repository's Checks tab. The secret can write a check and nothing else — it cannot read the repository, approve, land or push."
    >
      <Err message={error} />

      {minted && (
        <OnceSecret
          value={minted}
          onDismiss={() => setMinted(null)}
          note="Copy this now. Only this response carries it — there is no endpoint that reads it back, and rotating is the only way to get another one."
        />
      )}

      <dl className="mb-4 space-y-1 text-sm">
        <div className="flex flex-wrap gap-2">
          <dt className="text-ink-3">Intake secret</dt>
          <dd className="text-ink" data-testid="ci-secret-status">
            {status === null
              ? "…"
              : status.configured
                ? `configured${
                    status.rotated_at
                      ? ` — last rotated ${formatAgo(status.rotated_at)}`
                      : ""
                  }`
                : "not configured"}
          </dd>
        </div>
        <div className="flex flex-wrap items-baseline gap-2">
          <dt className="shrink-0 text-ink-3">Post verdicts to</dt>
          <dd className="min-w-0">
            <code className="inline-block max-w-full break-all rounded bg-surface-2 px-1.5 py-0.5 font-mono text-xs text-ink-2">
              POST {endpoint}
            </code>
          </dd>
        </div>
      </dl>

      <p className="mb-4 text-xs text-ink-3">
        The body is signed HMAC-SHA256 with this secret, and there are
        ready-made steps for GitHub Actions, GitLab CI, Buildkite and CircleCI
        in{" "}
        <a
          href={guide("ci-integration")}
          target="_blank"
          rel="noreferrer"
          className={PROSE_LINK}
        >
          the CI integration guide
        </a>
        . A repository mirrored through the GitHub App needs none of this: its
        Actions runs are polled in already.
      </p>

      <div className="flex flex-wrap items-center gap-3">
        {status?.configured ? (
          // Rotating is not the same act as minting, and the difference
          // is invisible from here: it **replaces** the secret, so a
          // pipeline still holding the old one is answered like a
          // stranger from its next build onward. Nothing tells that
          // pipeline why — the refusal is deliberately the same 404 a
          // non-existent repository gives — so the consequence has to be
          // on screen before the press rather than discovered later by
          // somebody wondering where their checks went.
          <AlertDialog>
            <AlertDialogTrigger asChild>
              <Button type="button" disabled={busy}>
                {busy ? "Working…" : "Rotate secret"}
              </Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>
                  Rotate the intake secret for {repo}?
                </AlertDialogTitle>
                <AlertDialogDescription>
                  This replaces the current secret. Any pipeline still holding
                  the old one is refused from its next build, and it is not told
                  why — its checks simply stop appearing. Have somewhere to
                  paste the new one before you press this; it is shown once.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Cancel</AlertDialogCancel>
                <AlertDialogAction onClick={() => void rotate()}>
                  Rotate this secret
                </AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        ) : (
          // Minting the first one replaces nothing and breaks nothing.
          // A confirmation here would be ceremony, and ceremony is what
          // teaches people to click through the confirmation that
          // matters.
          <Button type="button" onClick={rotate} disabled={busy}>
            {busy ? "Working…" : "Mint secret"}
          </Button>
        )}
        {status?.configured && (
          // Revoking breaks a pipeline that is working right now, and it
          // is not undoable — the next mint is a different value. So it
          // is confirmed, exactly as revoking an access token is.
          <AlertDialog>
            <AlertDialogTrigger asChild>
              <Button type="button" variant="destructive" disabled={busy}>
                Revoke secret
              </Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>
                  Revoke the intake secret for {repo}?
                </AlertDialogTitle>
                <AlertDialogDescription>
                  Any pipeline still posting with it starts being refused on its
                  next build, and its verdicts stop reaching the Checks tab.
                  Minting again produces a different secret; this one cannot be
                  brought back.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Cancel</AlertDialogCancel>
                <AlertDialogAction
                  onClick={async () => {
                    setError(null);
                    setBusy(true);
                    try {
                      await api.revokeCiSecret(session, repo);
                      // Whatever was on screen is now worthless, and
                      // leaving it there invites somebody to paste a
                      // dead credential into a pipeline and debug a 404.
                      setMinted(null);
                      refresh();
                      toast.success("Intake secret revoked");
                    } catch (e) {
                      setError(
                        `Could not revoke: ${e instanceof Error ? e.message : e}`,
                      );
                    } finally {
                      setBusy(false);
                    }
                  }}
                >
                  Revoke this secret
                </AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        )}
      </div>
    </Panel>
  );
}

/// A credential, shown for the only time it will ever be shown.
///
/// One component for both secrets on this page, because they are the
/// same promise and a second rendering of it would be a second place for
/// the sentence to drift. The API tokens page makes the same promise in
/// the same words — see `src/views/settings/tokens.tsx`; this is that
/// idiom with the copy and dismiss controls the two repo-level secrets
/// need, not a competing one.
function OnceSecret(props: {
  value: string;
  /// Why this is the only sighting, in the words of the thing that
  /// minted it.
  note: string;
  onDismiss: () => void;
}) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="mb-4 rounded-md border border-borderline bg-surface-0 p-3">
      <div className="mb-2 text-xs text-ink-3">{props.note}</div>
      {/* The value is long and arbitrary, so it wraps inside its own box:
          `audit()` fails the build on `documentElement.scrollWidth`, and
          a secret is exactly the kind of string that would widen the
          page. */}
      <div className="flex flex-wrap items-start gap-2">
        <code className="min-w-0 flex-1 break-all font-mono text-xs text-ink">
          {props.value}
        </code>
        {/* The clipboard is the only place this value is written. It is
            never put in a URL, a query string or a toast. */}
        <Button
          type="button"
          variant="outline"
          size="xs"
          onClick={async () => {
            try {
              await navigator.clipboard.writeText(props.value);
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            } catch {
              /* clipboard unavailable (permissions, http) — the text
                 stays selectable, which is why it is rendered rather
                 than masked */
            }
          }}
        >
          {copied ? "Copied" : "Copy secret"}
        </Button>
        {/* Dismissing removes it from the document rather than hiding
            it. A secret still sitting in the DOM behind a
            `display: none` is a secret in every screenshot,
            accessibility tree and "copy page" of the session that
            follows. */}
        <Button
          type="button"
          variant="ghost"
          size="xs"
          onClick={props.onDismiss}
        >
          Dismiss
        </Button>
      </div>
    </div>
  );
}

/// Where this repository's events are announced — the half of the loop
/// that starts a build.
///
/// This panel sits beside the intake secret deliberately, and the pair
/// is the whole answer to "how do I get CI on a repository hosted here".
/// Nothing on Weft runs a build, so for a native repository there are
/// two halves and neither is any use alone: **this** tells somebody's CI
/// there is something to build, and the intake carries the verdict back.
/// A maintainer should not have to discover that those are two separate
/// concepts documented on two separate pages, which is exactly what they
/// had to do before this existed.
function WebhooksPanel(props: { session: Session; repo: string }) {
  const { session, repo } = props;
  const [subs, setSubs] = useState<Webhook[] | null>(null);
  const [url, setUrl] = useState("");
  const [minted, setMinted] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    api
      .webhooks(session, repo)
      .then(setSubs)
      .catch((e: unknown) =>
        setError(
          `Could not list webhooks: ${e instanceof Error ? e.message : e}`,
        ),
      );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token, repo]);

  useEffect(refresh, [refresh]);

  async function add(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const out = await api.createWebhook(session, repo, url.trim());
      setMinted(out.secret);
      // The box is emptied on success and only on success. A refused
      // URL that vanishes from the field is a URL somebody has to
      // retype in order to find out what was wrong with it.
      setUrl("");
      refresh();
    } catch (err) {
      // The server's own sentence — it is the one that says
      // "url must be http(s)", which is actionable where a paraphrase
      // is not.
      setError(
        `Could not subscribe: ${err instanceof Error ? err.message : err}`,
      );
    } finally {
      setBusy(false);
    }
  }

  const rows = subs ?? [];

  return (
    <Panel
      title="Push webhooks"
      hint="What tells your CI there is something to build. Weft runs nothing itself, so for a repository hosted here this is the trigger and the intake secret below is the verdict coming back."
    >
      <Err message={error} />

      {minted && (
        <OnceSecret
          value={minted}
          onDismiss={() => setMinted(null)}
          note="Copy this now — it is shown once and there is no endpoint that reads it back. Every delivery is signed with it as X-Weft-Signature-256; verify that before trusting a delivery."
        />
      )}

      {rows.length > 0 ? (
        <div className="mb-4 overflow-x-auto">
          <Table>
            <TableHeader>
              <TableHeadRow>
                <TableHead>Endpoint</TableHead>
                <TableHead>Added</TableHead>
                <TableHead />
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {rows.map((s) => (
                <TableRow key={s.id}>
                  {/* A hook URL is somebody else's and can be any
                      length. It wraps in its cell rather than widening
                      the page. */}
                  <TableCell className="max-w-0 break-all font-mono text-xs text-ink-2">
                    {s.url}
                  </TableCell>
                  <TableCell className="whitespace-nowrap text-ink-2">
                    {formatAgo(s.created_at)}
                  </TableCell>
                  <TableCell className="text-right">
                    {/* Removing one stops the announcements, so whatever
                        it was driving stops being driven — and, like the
                        rotation above, nothing on the other end says so.
                        Confirmed for that reason. */}
                    <AlertDialog>
                      <AlertDialogTrigger asChild>
                        <Button type="button" variant="destructive" size="xs">
                          Remove
                        </Button>
                      </AlertDialogTrigger>
                      <AlertDialogContent>
                        <AlertDialogHeader>
                          <AlertDialogTitle>
                            Stop sending events to this endpoint?
                          </AlertDialogTitle>
                          <AlertDialogDescription>
                            Pushes and landings on {repo} stop being announced
                            there. If it is what starts your builds, they stop
                            starting — quietly, because nothing polls us. The
                            delivery secret is not recoverable; adding it back
                            mints a new one.
                          </AlertDialogDescription>
                        </AlertDialogHeader>
                        <AlertDialogFooter>
                          <AlertDialogCancel>Cancel</AlertDialogCancel>
                          <AlertDialogAction
                            onClick={async () => {
                              setError(null);
                              try {
                                await api.deleteWebhook(session, repo, s.id);
                                refresh();
                                toast.success("Endpoint removed");
                              } catch (e) {
                                setError(
                                  `Could not remove: ${e instanceof Error ? e.message : e}`,
                                );
                              }
                            }}
                          >
                            Remove this endpoint
                          </AlertDialogAction>
                        </AlertDialogFooter>
                      </AlertDialogContent>
                    </AlertDialog>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      ) : (
        <p className="mb-4 text-sm text-ink-3" data-testid="webhooks-empty">
          Nothing is told about pushes to this repository.
        </p>
      )}

      {/* What actually arrives, said here rather than only in the docs.
          Somebody wiring a merge gate needs to know a landing is an
          event, and somebody wiring per-push CI needs to know that a
          `git push` delivery does not name the branch. */}
      <p className="mb-4 text-xs text-ink-3">
        Deliveries fire on <code className="font-mono">push</code>,{" "}
        <code className="font-mono">change.landed</code> and{" "}
        <code className="font-mono">change.ejected</code>, signed with the
        secret above. A push over git or SSH says only that the repository was
        pushed to — it does not name the branch or the commit — so a receiver
        has to fetch to find out what moved. Details in{" "}
        <a
          href={guide("webhooks")}
          target="_blank"
          rel="noreferrer"
          className={PROSE_LINK}
        >
          the webhooks guide
        </a>
        , and the whole loop for a repository hosted here in{" "}
        <a
          href={guide("ci-integration")}
          target="_blank"
          rel="noreferrer"
          className={PROSE_LINK}
        >
          CI integration
        </a>
        .
      </p>

      <form onSubmit={add} className="flex flex-wrap items-center gap-2">
        <Input
          className="min-w-0 flex-1"
          inputMode="url"
          value={url}
          placeholder="https://ci.example.com/hooks/weft"
          aria-label="Webhook endpoint URL"
          onChange={(e) => setUrl(e.target.value)}
        />
        <Button type="submit" disabled={busy || url.trim() === ""}>
          {busy ? "Adding…" : "Add endpoint"}
        </Button>
      </form>
    </Panel>
  );
}
