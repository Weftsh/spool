import { useEffect, useMemo, useState } from "react";
import { api, viewerSession, type WorkflowRun } from "@/api";
import { RelativeTime } from "@/components/relative-time";
import { STRUCTURAL_LINK } from "@/lib/links";
import { cn } from "@/lib/utils";
import { dash, href } from "@/router";
import { workflowStatePresentation } from "@/views/forge/workflow-run";

/// How many composed runs the panel shows. The panel is a pointer, not a
/// history: the changeset's own page has the whole story.
export const CHANGESET_BUILDS_LIMIT = 10;

/// The composed runs of a repository's workflows, under its Checks tab.
///
/// When a changeset is composed, every member's `.weft/*.yml` runs
/// again with **all** the members checked out together at the tips that
/// will land — and the verdict goes to the changeset, not to any one
/// member's commit (`changeset_checks`, deliberately apart from
/// `check_runs`, so a composed "ci / build" cannot collide with the
/// push-time row the land gate reads). The consequence nobody wrote
/// down: those runs never appeared on the member's Checks tab at all.
/// Composing three repositories and opening one of them showed the push
/// run and nothing else, which read as the composition having never
/// run.
///
/// So this panel asks for them by name — `event=changeset`, filtered on
/// the server, because a busy repository's push runs would otherwise
/// push them out of the window — and says in one sentence why they sit
/// apart. Each row leads to its run page here and to the changeset that
/// owns the verdict.
///
/// Never allowed to disturb the tab: a repository with no workflows, an
/// older server, or a refusal leaves the panel absent.
export function ChangesetBuilds(props: {
  owner: string;
  repo: string;
  token: string | null;
}) {
  const { owner, repo } = props;
  const session = useMemo(
    () => viewerSession(owner, props.token),
    [owner, props.token],
  );
  const [runs, setRuns] = useState<WorkflowRun[]>([]);
  useEffect(() => {
    let alive = true;
    api
      .workflowRuns(session, repo, {
        event: "changeset",
        limit: CHANGESET_BUILDS_LIMIT,
      })
      .then((rs) => alive && setRuns(rs.filter((r) => r.event === "changeset")))
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [session, repo]);

  if (runs.length === 0) return null;
  return (
    <section
      data-testid="changeset-builds"
      aria-labelledby="changeset-builds-heading"
      className="mt-8"
    >
      <h2
        id="changeset-builds-heading"
        className="text-base font-semibold text-ink"
      >
        Changeset builds
      </h2>
      <p className="mt-1 text-sm text-ink-3">
        Runs of this repository&apos;s workflows on a composed changeset — every
        member checked out together at the commits that will land. They report
        to the changeset, not to a commit here, which is why they are not among
        the checks above.
      </p>
      <ul className="mt-3 rounded-lg border border-borderline">
        {runs.map((run) => (
          <ChangesetBuildRow key={run.id} run={run} owner={owner} repo={repo} />
        ))}
      </ul>
    </section>
  );
}

function ChangesetBuildRow(props: {
  run: WorkflowRun;
  owner: string;
  repo: string;
}) {
  const { run, owner, repo } = props;
  const pres = workflowStatePresentation(run.state);
  const Icon = pres.icon;
  return (
    <li className="flex items-start gap-3 border-b border-borderline bg-surface-1 px-4 py-3 last:border-b-0">
      <span
        role="img"
        aria-label={pres.label}
        title={pres.label}
        className={cn("mt-0.5 inline-flex shrink-0", pres.className)}
      >
        <Icon aria-hidden className="size-4 shrink-0" />
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <a
            className={cn(STRUCTURAL_LINK, "min-w-0 truncate font-medium")}
            href={href([owner, repo, "checks", "runs", run.id])}
          >
            {run.name}
          </a>{" "}
          <span className={cn("text-xs", pres.className)}>{pres.label}</span>
        </div>
        <p className="mt-0.5 flex flex-wrap items-center gap-x-2 text-xs text-ink-3">
          {run.changeset && (
            <span className="min-w-0 truncate">
              for changeset{" "}
              {/* A full load: the changeset page lives on the dashboard
                  mount, which this router does not know about. */}
              <a
                className={STRUCTURAL_LINK}
                href={dash(["changesets", run.changeset.key])}
              >
                {run.changeset.key}
              </a>
            </span>
          )}
          <RelativeTime at={run.created_at} />
        </p>
      </div>
    </li>
  );
}
