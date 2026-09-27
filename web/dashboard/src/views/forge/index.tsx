import { useEffect, useMemo, useState } from "react";
import { api, viewerSession, type Me, type Repo } from "@/api";
import { NotFound } from "@/components/not-found";
import { TabStrip } from "@/components/tab-strip";
import { ForgeContainer, ForgeShell } from "@/shells/forge-shell";
import { STRUCTURAL_LINK, STRUCTURAL_LINK_2 } from "@/lib/links";
import { Browser, CommitLog } from "@/views/browse";
import { ChangesPanel } from "@/views/changes";
import { ChangesetView } from "@/views/changesets";
import { forgeChangesetLinks } from "@/views/changesets/links";
import { CommitView } from "@/views/forge/commit";
import { CodeButton } from "@/components/code-button";
import { OwnerAvatar } from "@/components/owner-avatar";
import { Alert } from "@/components/ui/alert";
import { Markdown } from "@/components/markdown";
import { WatchButton } from "@/components/watch-button";
import { ForkButton } from "@/components/fork-button";
import { SyncBadge } from "@/components/sync-badge";
import { ProfileView } from "@/views/forge/profile";
import { SearchView } from "@/views/forge/search";
import { IssuesView } from "@/views/forge/issues";
import { ChecksView } from "@/views/forge/checks";
import { ChangesetBuilds } from "@/views/forge/changeset-builds";
import { WorkflowRunView } from "@/views/forge/workflow-run";
import { RepoInsightsView } from "@/views/forge/insights";
import { RepoSettingsView } from "@/views/forge/settings";
import { About } from "@/views/forge/about";
import { href, useQuery } from "@/router";
import type { Match, RepoTab } from "@/routes";

/// The forge: every address at the root — a namespace, a repository, a
/// changeset, a search. Only ever rendered for somebody signed in;
/// `App.tsx` sends anybody else to `/login` first.
///
/// The session handed to the data calls is `viewerSession(owner, token)`:
/// the namespace the request is about, plus the credential of whoever is
/// looking. Both halves matter and the second one was missing. A cookie
/// session hid that — the browser attaches the cookie itself — so every
/// forge read went out unauthenticated for a token holder, and because
/// the server filters by who is asking, a person was silently shown less
/// of their own work rather than being told anything.
/// Name the page in the browser's own chrome.
///
/// Every forge page reported "Weft Dashboard". The title is what
/// somebody bookmarks and what a shared tab says — two places where
/// "acme/forge-demo" is the whole answer and the product's name is
/// noise.
/// `null` means "somebody below me is naming this page".
///
/// React runs a child's effects before its parent's, so a parent that
/// sets a title unconditionally overwrites whatever its child just set —
/// which is how the repository screen's `acme/widget` kept losing to the
/// generic `Weft` one component above it. The hook still runs on
/// every render, because a conditional hook would break the order the
/// moment somebody navigated from a repository to a profile.
function useDocumentTitle(title: string | null) {
  useEffect(() => {
    if (title === null) return;
    const previous = document.title;
    document.title = title;
    return () => {
      document.title = previous;
    };
  }, [title]);
}

export function ForgeView(props: {
  match: Match;
  me: Me | null;
  /// The caller's API token, when they signed in with one rather than
  /// with a password; empty for a cookie session.
  ///
  /// The org in the path is the repository's owner; the token is the
  /// caller's. They are different things and the session carries both —
  /// a session built with an empty token went out unauthenticated for a
  /// person signed in with a service token, and every write bounced
  /// them to a login page they were already past.
  token: string | null;
  navigate: (to: string, replace?: boolean) => void;
  onSignOut: () => void;
}) {
  const m = props.match;
  useDocumentTitle(
    m.kind === "repo"
      ? null // `RepoScreen` names it, and only once it may.
      : m.kind === "owner"
        ? m.owner
        : m.kind === "changeset"
          ? // The key, because that is what somebody is being told about
            // — and the namespace with it, because two organizations may
            // both have a `rename-payments`.
            `${m.owner}/${m.key} — Weft`
          : m.kind === "search" && (m.q || m.topic)
            ? `${m.q || m.topic} — Weft`
            : "Weft",
  );
  // A repository owns its whole screen — masthead included — because
  // whether that screen exists at all depends on an answer only it
  // fetches. Deciding in the body alone left the name, the tabs and the
  // document title drawn above a hidden repository, which is the
  // existence oracle the server's masking exists to prevent.
  if (m.kind === "repo") return <RepoScreen {...props} m={m} />;
  return (
    <ForgeShell
      me={props.me}
      navigate={props.navigate}
      onSignOut={props.onSignOut}
    >
      {m.kind === "search" ? (
        <SearchView
          q={m.q}
          topic={m.topic}
          token={props.token}
          navigate={props.navigate}
        />
      ) : m.kind === "owner" ? (
        <ProfileView
          owner={m.owner}
          navigate={props.navigate}
          token={props.token ?? undefined}
        />
      ) : m.kind === "changeset" ? (
        // The same view the dashboard draws, inside this shell's column
        // rather than a `<main>` of its own — one rendering, not two.
        <ChangesetView
          session={viewerSession(m.owner, props.token)}
          changesetKey={m.key}
          links={forgeChangesetLinks(m.owner, props.navigate)}
          as="div"
        />
      ) : (
        <NotFound onNavigate={props.navigate} />
      )}
    </ForgeShell>
  );
}

/// One repository page: masthead, tabs and body, or nothing at all.
function RepoScreen(props: {
  m: Extract<Match, { kind: "repo" }>;
  me: Me | null;
  token: string | null;
  navigate: (to: string, replace?: boolean) => void;
  onSignOut: () => void;
}) {
  const { owner, repo } = props.m;
  const state = useRepoRow(owner, repo, props.token);
  const row = asRepo(state);
  const hidden = "status" in state && state.status === "hidden";
  // Not even the name until we know the viewer may see it. "Refused"
  // and "no such repository" are one answer here on purpose.
  useDocumentTitle(hidden ? "Weft" : `${owner}/${repo}`);
  // The row already knows. `viewer_admin` is the server's own answer,
  // from the same `authx` refinement the writes use, so the tab and the
  // writes behind it cannot hold different opinions — and there is no
  // second request to make, on the most-visited page in the product.
  //
  // This was a probe of `GET …/access` and read success as "you may
  // administer this". That question requires org-wide admin while the
  // writes accept a per-repo grant, so somebody holding `admin` on one
  // repository was shown no settings surface at all. `null` until the
  // row lands: it is in flight on the first render, and a `false`
  // default would draw the tab absent and then present.
  const admin = row ? row.viewer_admin : null;
  // Whether this viewer is on the inside at all, which is a weaker
  // question than any of the others and the one Insights is gated on.
  // `null` until the row lands, like `admin`, so nothing is offered or
  // withheld on a guess.
  const member = row ? (row.viewer_member ?? false) : null;
  return (
    <ForgeShell
      me={props.me}
      navigate={props.navigate}
      onSignOut={props.onSignOut}
      // The tab strip is the shell's masthead, not page content, so its
      // hairline crosses the whole viewport while the tabs stay inside
      // the column. Rendering it in `children` would mean a negative
      // margin from inside the container — which is exactly how
      // `documentElement.scrollWidth` grows and the walkthrough's
      // `audit()` fails the build on horizontal overflow.
      masthead={
        hidden ? undefined : (
          <RepoTabs
            m={props.m}
            navigate={props.navigate}
            token={props.token}
            row={row}
            admin={admin}
            member={member}
          />
        )
      }
    >
      {hidden ? (
        <NotFound onNavigate={props.navigate} what={`${owner}/${repo}`} />
      ) : (
        <RepoBody
          m={props.m}
          navigate={props.navigate}
          row={row}
          token={props.token}
          me={props.me}
          admin={admin}
          member={member}
        />
      )}
    </ForgeShell>
  );
}

/// The tabs a repository page shows — and only the ones that lead
/// somewhere.
///
/// `FORGE-UX.md` says a tab that leads nowhere is worse than an absent
/// one, and then this file shipped five of them, four of which answered
/// 404. A surface that is honest about being small beats one that
/// gestures at a product it does not have.
///
/// Add a tab here when its body exists, not when its name is decided.
const TABS: { key: RepoTab; label: string }[] = [
  { key: "code", label: "Code" },
  { key: "issues", label: "Issues" },
  { key: "changes", label: "Changes" },
  // Named Checks and not Actions, deliberately (FORGE-UX §7). The tab
  // carries every verdict about a commit whoever reached it: a
  // repository's `.weft/*.yml` workflows, which run on the organization's
  // runners and have their own run page at `checks/runs/{id}`, beside a
  // Buildkite project's runs and a GitHub Actions one. Naming it after
  // one competitor's product would say the wrong thing about them and
  // about the two thirds of this tab that is not theirs.
  // `/{owner}/{repo}/actions` redirects here, because that is the
  // address muscle memory types and 404ing somebody to prove a naming
  // point helps nobody.
  { key: "checks", label: "Checks" },
];

/// Which tab a path lights up.
///
/// Commits is not a tab, and briefly was. GitHub reaches its commit
/// history from the "N commits" link above the file table and from a
/// file's own history — never from the top strip — and checking rather
/// than assuming showed `/commits/main` there still lighting **Code**.
/// That is the right model: the strip names the *kinds of thing* a
/// repository has, and commits are not a kind of thing beside issues and
/// pull requests, they are what the code tab is made of. A tab for them
/// makes the strip a sitemap.
///
/// So the page keeps its own address and its own link, and the strip
/// keeps saying you are in the code.
function activeTab(tab: RepoTab): RepoTab {
  return tab === "commits" || tab === "commit" ? "code" : tab;
}

function RepoTabs(props: {
  m: Extract<Match, { kind: "repo" }>;
  navigate: (to: string, replace?: boolean) => void;
  token: string | null;
  row: Repo | null;
  /// Whether this viewer may administer this repository, `null` until
  /// the row that says so has arrived. A viewer who may not is shown no
  /// Settings tab at all — not a disabled one, which would still tell
  /// them the surface is there to come back for.
  admin: boolean | null;
  /// Whether this viewer holds a role here at all. The Insights tab's
  /// gate, and the same `null`-until-known rule.
  member: boolean | null;
}) {
  const { owner, repo, tab } = props.m;
  // One object per owner, not one per render.
  //
  // `viewerSession()` builds a fresh `Session` every time it is called,
  // and a child whose effect depends on that object re-runs it on every
  // render, sets state, and re-renders — a loop that freezes the tab
  // rather than failing. It froze this page the first time these
  // controls were mounted. `WatchButton` defends itself by depending on
  // the primitives inside the session; not every component does, so the
  // caller holds it still as well.
  const session = useMemo(
    () => viewerSession(owner, props.token),
    [owner, props.token],
  );
  return (
    <>
      <ForgeContainer>
        {/* Two rows, not one wrapping row. The identity and the actions
            are separate things that happen to share a line at desktop
            width, and expressing that as `flex-wrap` on one row meant a
            narrow viewport wrapped them in whatever order the widths
            fell out — with the "forked from" line, which belongs under
            the name, landing wherever there was room. `items-start`
            rather than `items-center`: the identity column is two lines
            tall on a fork and one otherwise, and the action group must
            sit on the first line in both cases. */}
        <header className="flex flex-col gap-3 py-4 sm:flex-row sm:items-start sm:gap-4">
          <div className="flex min-w-0 flex-col gap-1">
            <div className="flex min-w-0 flex-wrap items-center gap-2">
              <OwnerAvatar name={owner} size={20} />
              <span className="min-w-0 truncate">
                <a
                  className={`font-medium ${STRUCTURAL_LINK_2}`}
                  href={href([owner])}
                >
                  {owner}
                </a>{" "}
                <span className="text-ink-3">/</span>{" "}
                <a
                  className={`font-semibold ${STRUCTURAL_LINK}`}
                  href={href([owner, repo])}
                >
                  {repo}
                </a>
              </span>
              {/* Whether a mirror is keeping up with its origin, beside
                  the name rather than on Insights: it is the repository's
                  *state*, and a reader who is about to clone a stale
                  mirror needs to be told so on the page they landed on,
                  not on a tab only an admin is offered. */}
              {props.row?.kind === "mirror" && (
                <SyncBadge
                  error={props.row.sync_error}
                  lastSync={props.row.last_sync_at}
                />
              )}
            </div>
            {props.row?.fork_parent && (
              <span className="text-xs text-ink-3">
                Forked from{" "}
                <a
                  className={STRUCTURAL_LINK_2}
                  href={href(props.row.fork_parent.split("/"))}
                >
                  {props.row.fork_parent}
                </a>
              </span>
            )}
          </div>
          {/* Watch · Fork, GitHub's order (FORGE-UX §1.2), both drawn
              by one component so they cannot drift apart in height. */}
          <span className="flex shrink-0 items-center gap-2 sm:ml-auto">
            <WatchButton
              session={session}
              repo={repo}
              count={props.row?.watcher_count ?? 0}
            />
            <ForkButton
              session={session}
              repo={repo}
              count={props.row?.fork_count ?? 0}
              onForked={(made) =>
                props.navigate(href([made.org ?? owner, made.name]))
              }
            />
          </span>
        </header>
      </ForgeContainer>
      <TabStrip
        label="Repository"
        onNavigate={props.navigate}
        active={activeTab(tab)}
        tabs={[
          // A mirror's trunk belongs to its origin — the API refuses to
          // register a change on one — so the tab is not rendered rather
          // than rendered onto a form that can only fail. The dashboard's
          // repo screen had this guard from the beginning and this page
          // never did; it only became reachable when the dashboard's
          // screen went away and this became the one repository page.
          //
          // Withheld until the row has arrived, the way Settings is,
          // rather than shown and then taken away: a tab that appears
          // late is ordinary, and one that vanishes under the pointer is
          // not.
          ...TABS.filter(
            (t) =>
              t.key !== "changes" ||
              (props.row !== null && props.row.kind !== "mirror"),
          ),
          // Last, as GitHub puts it, and each only for somebody it
          // answers — on two different questions, which is the point.
          //
          // Insights is for **members**: a repository's traffic belongs
          // to the people who own it. That is a weaker bar than Settings,
          // deliberately — somebody with the `viewer` role may read the
          // numbers and change nothing.
          //
          // Insights before Settings, which is GitHub's order and the
          // useful one: the tab you read comes before the tab you change
          // things on, and the destructive one belongs at the far end.
          ...(props.member
            ? [{ key: "insights" as RepoTab, label: "Insights" }]
            : []),
          ...(props.admin
            ? [{ key: "settings" as RepoTab, label: "Settings" }]
            : []),
        ].map((t) => ({
          key: t.key,
          label: t.label,
          href:
            t.key === "code" ? href([owner, repo]) : href([owner, repo, t.key]),
        }))}
      />
    </>
  );
}

/// The repository row, fetched once for whoever on the page needs it.
///
/// The About panel and the Code button both want it, and asking twice
/// would be two requests and two chances to render different answers.
/// `loading` until the server answers, then the row or `"hidden"`.
///
/// "Hidden" covers refused and missing alike, and deliberately does not
/// distinguish them: somebody asking about a repository they may not
/// read and somebody asking about one that never existed must get the
/// same answer, or the URL bar becomes an oracle for which repositories
/// a namespace has. That masking is enforced server-side and was then
/// undone by the page, which drew the repository's name, its tab strip
/// and its document title before showing a red box with the four
/// characters `401` in it.
type RepoState = { status: "loading" } | { status: "hidden" } | Repo;

function useRepoRow(
  owner: string,
  name: string,
  token: string | null,
): RepoState {
  // The answer is stamped with the repository it is about. Navigating
  // from one repository to another re-renders this screen with the new
  // address before the effect below has run, so for that first render
  // the state still held the *previous* repository's row — a ready one.
  // Pressing Fork is exactly that navigation: the fork's page mounted
  // the file browser off the upstream's row and asked the fork for a
  // tree, log, README, tags, meta and branches it did not have yet,
  // eight 404s per fork, from the one path a person actually takes to
  // get here. The `pending` gate below was right and never got a say.
  const key = `${owner}/${name}`;
  const [state, setState] = useState<{ key: string; state: RepoState }>({
    key,
    state: { status: "loading" },
  });
  useEffect(() => {
    let alive = true;
    let again: ReturnType<typeof setTimeout> | undefined;
    const put = (s: RepoState) => setState({ key, state: s });
    put({ status: "loading" });
    // A fork's row exists the moment the POST answers 202; the storage
    // that makes it readable is written by a job a few seconds later,
    // and the row says which side of that it is on. Pressing Fork lands
    // here straight from the 202, so while the row says `pending` the
    // page asks again every second and lets the person watch it arrive,
    // rather than showing them the tree's 404 for a copy that has not
    // been made yet — a red "404" over a fork you just pressed the
    // button for reads as "the fork failed", and a reload was the only
    // way past it.
    const load = () =>
      api
        .repo(viewerSession(owner, token), name)
        .then((r) => {
          if (!alive) return;
          put(r);
          if (r.fork_state === "pending") again = setTimeout(load, 1000);
        })
        .catch(() => alive && put({ status: "hidden" }));
    load();
    return () => {
      alive = false;
      clearTimeout(again);
    };
  }, [owner, name]);
  return state.key === key ? state.state : { status: "loading" };
}

/// What the Code tab shows while a fork's objects are on their way, and
/// when they never arrived.
///
/// Neither state has a tree to show, and asking for one anyway is how
/// the page used to say "404" in red under the name of a repository the
/// person had created a second earlier. The pending notice names the
/// upstream because that is the one fact about an empty fork that is
/// already true; `useRepoRow` keeps asking until the row changes, so
/// this is replaced by the files without anybody pressing reload.
function ForkNotReady(props: {
  state: "pending" | "failed";
  parent: string | null;
}) {
  if (props.state === "failed")
    return (
      <Alert variant="destructive">
        This fork could not be made. Delete it and fork again, or ask an owner
        of {props.parent ?? "the upstream"} to look into it.
      </Alert>
    );
  return (
    <div
      role="status"
      className="rounded-lg border border-borderline bg-surface-2 px-4 py-6 text-center text-sm text-ink-3"
    >
      Forking {props.parent ?? "the upstream"}… your copy will be ready in a
      moment.
    </div>
  );
}

/// What the Code tab shows for a repository nothing has been pushed to.
///
/// GitHub's shape: say it is empty, and hand a writer the two commands
/// that fill it, with this repository's own address already in them —
/// the one thing a person on this page most often wants and would
/// otherwise have to assemble from the Code button. A reader gets the
/// fact and not the commands, which they could not run. Rendered by
/// `Browser` in place of the listing, so it only appears once the ref
/// list has confirmed the repository is empty and not merely slow.
function EmptyRepo(props: { row: Repo }) {
  const { row } = props;
  return (
    <div className="rounded-lg border border-borderline bg-surface-2 px-4 py-6 text-sm text-ink-3">
      <p className="font-medium text-ink">This repository is empty.</p>
      {row.viewer_write ? (
        <>
          <p className="mt-1">
            Push an existing repository from the command line:
          </p>
          <pre className="mt-3 overflow-x-auto rounded-md border border-borderline bg-surface-1 p-3 font-mono text-xs text-ink">
            {`git remote add origin ${row.clone_url}\ngit push -u origin ${row.default_branch}`}
          </pre>
        </>
      ) : (
        <p className="mt-1">Nothing has been pushed to it yet.</p>
      )}
    </div>
  );
}

function asRepo(state: RepoState): Repo | null {
  return "status" in state ? null : state;
}

/// The README, rendered, under the file list.
///
/// A repository page without one is a directory listing: the file is
/// right there and the project's own words are a click away, which is
/// the wrong way round. Only at the repository root — a README inside a
/// subdirectory belongs to that subdirectory, and hoisting it would
/// attribute somebody's `docs/README.md` to the whole project.
///
/// Absent, unreadable or binary renders nothing at all rather than an
/// empty bordered box, which reads as a README that failed to load.
function Readme(props: {
  owner: string;
  repo: string;
  path: string[];
  token: string | null;
}) {
  const { owner, repo } = props;
  const [text, setText] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    setText(null);
    if (props.path.length > 0) return;
    api
      .file(viewerSession(owner, props.token), repo, "README.md")
      .then((f) => alive && setText(f.binary ? null : f.text))
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [owner, repo, props.path.length]);

  if (!text) return null;
  return (
    <section className="mt-6 rounded-xl border border-borderline bg-surface-1 p-6">
      <Markdown
        source={text}
        // Two prefixes because a document link goes to the code browser
        // and an image has to reach bytes. A README's own `#` renders as
        // an h2 so it nests under the repository name rather than
        // competing with it in the page outline.
        base={{
          links: `/${owner}/${repo}/tree/`,
          images: `/v1/orgs/${owner}/repos/${repo}/files/`,
        }}
        headingLevel={2}
      />
    </section>
  );
}

function RepoBody(props: {
  m: Extract<Match, { kind: "repo" }>;
  navigate: (to: string, replace?: boolean) => void;
  row: Repo | null;
  token: string | null;
  /// Who is looking, when a browser session says: the Issues and
  /// Changes tabs compare it with an author.
  me: Me | null;
  /// Whether this viewer may administer the repository, decided once in
  /// `RepoScreen` so the tab and the page it leads to cannot disagree.
  admin: boolean | null;
  /// Whether this viewer holds a role here at all — the same value, the
  /// same reason, for Insights.
  member: boolean | null;
}) {
  const { owner, repo, tab, rest } = props.m;
  const session = useMemo(
    () => viewerSession(owner, props.token),
    [owner, props.token],
  );
  // The revision being viewed, which is a `?`-parameter rather than a
  // path segment because the same path at two revisions is the same
  // page.
  //
  // This page *wrote* `?at=` from the moment it had a ref switcher and
  // never read it back, so every branch permalink and every "view this
  // file at" link landed on the default branch showing today's bytes,
  // under a URL that said otherwise. The switcher then read as stuck:
  // it renders whatever `at` says, so it went on saying "main" after
  // you picked another branch, and picking "main" again was a no-op
  // because as far as it knew you were already there.
  const at = useQuery().get("at") ?? undefined;
  const row = props.row;
  if (tab === "commits")
    // No heading and no card. The page is reached from a tab that says
    // "Commits" and from a bar that says "N commits", so a heading
    // repeating the word is furniture — and wrapping the list in a
    // bordered box is the same floating-panel mistake this rework took
    // off the front page.
    return (
      <div className="py-6">
        {/* The page needs its own title now that no tab carries the
            word — GitHub's commits page has one for the same reason. */}
        <h1 className="mb-4 border-b border-borderline pb-3 text-xl font-semibold text-ink">
          Commits
        </h1>
        <CommitLog
          session={session}
          repo={repo}
          at={undefined}
          bare
          commitHref={(sha) => href([owner, repo, "commit", sha])}
          onNavigate={props.navigate}
        />
      </div>
    );
  if (tab === "commit")
    return rest[0] ? (
      <CommitView
        session={session}
        repo={repo}
        sha={rest[0]}
        navigate={props.navigate}
      />
    ) : (
      <NotFound onNavigate={props.navigate} what="that commit" />
    );
  if (tab === "changes") {
    // Hiding the tab is not enough: the address is still typable, and a
    // page that draws a "start a review" form for a repository the
    // server will refuse is the control-that-leads-nowhere this
    // codebase keeps refusing to ship. `null` is still loading — the
    // hidden case became `NotFound` in `RepoScreen` — so it waits
    // rather than guessing, which also keeps it from firing the panel's
    // reads at a repository that is about to turn out to be a mirror.
    if (row === null) return null;
    if (row.kind === "mirror")
      return <NotFound onNavigate={props.navigate} what={tab} />;
    // A change is something you are *sent* — by a review notification,
    // among other things — so it has to have an address. Every
    // notification this forge sent before today landed on a page that
    // said "We couldn't find changes", which is a feature working right
    // up to the last inch and then failing.
    return (
      <div className="py-6">
        <ChangesPanel
          session={session}
          repo={repo}
          // The server's own answer, not a guess from the row's presence:
          // a reader with the `viewer` role gets the row too, and must
          // not be shown a write form because of it.
          canWrite={row?.viewer_write ?? false}
          me={props.me}
          defaultBranch={row?.default_branch}
          // A workflow run's check row links to its run page, which is a
          // route on this mount. See `DetailLink`.
          navigate={props.navigate}
          selectedKey={rest[0] ?? null}
          onSelect={(key) =>
            props.navigate(
              key
                ? href([owner, repo, "changes", key])
                : href([owner, repo, "changes"]),
            )
          }
        />
      </div>
    );
  }
  if (tab === "checks") {
    // `/{owner}/{repo}/checks/runs/{id}` — one `.weft` workflow run.
    //
    // Under the Checks tab and not beside it, because that is what it
    // is: a workflow run mirrors itself into a check row like any other
    // provider's verdict, and this is that row's own page. It also keeps
    // the tab strip lit correctly with no extra case, and the `/actions`
    // alias carries its deep links here for free.
    if (rest[0] === "runs")
      return rest[1] ? (
        <WorkflowRunView
          owner={owner}
          repo={repo}
          runId={rest[1]}
          token={props.token}
          // The row's own answer and not `admin`: cancelling needs
          // `repo:write`, which a writer holds and an administrator is
          // merely also. Subtractive — false until the row lands.
          canWrite={row?.viewer_write ?? false}
          navigate={props.navigate}
        />
      ) : (
        <NotFound onNavigate={props.navigate} what="that run" />
      );
    return (
      <ChecksView
        owner={owner}
        repo={repo}
        token={props.token}
        navigate={props.navigate}
        // Subtractive, like the About rail's `canEdit`: `admin` is null
        // until the row lands, and `=== true` means a control appears a
        // moment late rather than appearing and then being refused.
        canWrite={props.admin === true}
      >
        <ChangesetBuilds owner={owner} repo={repo} token={props.token} />
      </ChecksView>
    );
  }
  if (tab === "issues")
    return (
      <IssuesView
        owner={owner}
        repo={repo}
        rest={rest}
        canWrite={row?.viewer_write ?? false}
        token={props.token}
        me={props.me}
        navigate={props.navigate}
      />
    );
  if (tab === "insights")
    return (
      <RepoInsightsView
        session={session}
        owner={owner}
        repo={repo}
        row={row}
        member={props.member}
        navigate={props.navigate}
      />
    );
  if (tab === "settings")
    return (
      <RepoSettingsView
        session={session}
        owner={owner}
        repo={repo}
        row={row}
        admin={props.admin}
        navigate={props.navigate}
      />
    );
  if (tab !== "code")
    return <NotFound onNavigate={props.navigate} what={tab} />;
  // A fork without its objects yet has no files to browse and no README
  // to render; the requests for them would only 404. The About rail
  // still stands: the description, the counts and "forked from" are the
  // row's, and the row is here.
  //
  // The files wait for the row, on purpose. Mounting the browser at
  // once and taking it down when the row said `pending` still sent the
  // first round of reads — the row and the tree used to be asked for in
  // parallel — so the 404 was fired every time and merely not shown.
  // One round trip later on the code tab is the price of not asking a
  // repository for files it has told us it does not have yet.
  const notReady =
    row === null
      ? "loading"
      : row.fork_state === "pending" || row.fork_state === "failed"
        ? row.fork_state
        : null;
  return (
    <div className="flex flex-col gap-8 py-6 md:flex-row">
      <div className="min-w-0 flex-1">
        {notReady === "loading" ? null : notReady ? (
          <ForkNotReady state={notReady} parent={row?.fork_parent ?? null} />
        ) : (
          <>
            <Browser
              session={session}
              repo={repo}
              path={rest}
              at={at}
              navigate={props.navigate}
              // A file path rides behind `tree` here, where `/dashboard` spells
              // the same thing `/repos/{repo}/…`. One helper, handed in, is why
              // the same component serves both mounts.
              commitsHref={href([owner, repo, "commits"])}
              commitHref={(sha) => href([owner, repo, "commit", sha])}
              // The row's stored count, with the tip it was true for.
              // The browser prints it only beside a head that matches.
              commitCount={
                row && row.commits != null && row.commits_tip
                  ? {
                      count: row.commits,
                      tip: row.commits_tip,
                      exact: row.commits_exact ?? true,
                    }
                  : null
              }
              actions={
                row && (
                  <CodeButton
                    httpsUrl={row.clone_url}
                    sshUrl={row.ssh_clone_url}
                  />
                )
              }
              empty={row && <EmptyRepo row={row} />}
              link={(parts, rev) =>
                parts.length === 0
                  ? href([owner, repo], { at: rev })
                  : href([owner, repo, "tree", ...parts], { at: rev })
              }
            />
            <Readme owner={owner} repo={repo} path={rest} token={props.token} />
          </>
        )}
      </div>
      {/* `viewer_admin` rather than a write answer, because the row
          carries the first and not the second. An admin is
          definitionally allowed to write, so this is a strict *subset*
          of who may edit topics: an org member with write access and no
          admin is shown the pills and not the form. That is a false
          negative and never a leak — the topics themselves are on the
          page for everybody, and getting this wrong only ever withholds
          a control the server would have accepted. The exact fix is one
          more boolean beside `viewer_admin`; it is not worth a second
          request on the most-visited page in the product. */}
      {/* Only at the root. About describes the repository, and a file
          page now carries the repository's tree beside the file; a
          third column would leave the code narrower than a phone. GitHub
          draws the same line. */}
      {rest.length === 0 && (
        <About
          owner={owner}
          repo={row}
          token={props.token}
          canEdit={props.admin === true}
        />
      )}
    </div>
  );
}
