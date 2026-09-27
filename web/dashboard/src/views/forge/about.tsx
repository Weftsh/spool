import { useEffect, useState } from "react";
import {
  Activity,
  BookOpen,
  Eye,
  GitFork,
  HeartHandshake,
  Link as LinkIcon,
  Scale,
  Settings,
  ShieldAlert,
  Star,
  Tag,
  Users,
  X,
} from "lucide-react";
import {
  api,
  viewerSession,
  type Contributor,
  type ForkEntry,
  type License,
  type OriginStars,
  type RefName,
  type Repo,
  type RepoMeta,
  type StarState,
} from "@/api";
import { LanguageBar } from "@/components/language-bar";
import { OwnerAvatar } from "@/components/owner-avatar";
import { RelativeTime } from "@/components/relative-time";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Err } from "@/components/feedback";
import { formatCount } from "@/format";
import { STRUCTURAL_LINK_2, PROSE_LINK, FOCUS_RING } from "@/lib/links";
import { originHref, originLabel } from "@/lib/origin";
import { cn } from "@/lib/utils";
import { href } from "@/router";

/// The About rail: what the repository *is*, beside its files.
///
/// The page it sits on used to say a description and nothing else,
/// which is a directory listing with a sentence over it. What a visitor
/// is actually deciding on is the shape of the project — what it is
/// written in, what licence it carries, whether there is anywhere to
/// send a bug report, who else has touched it, and what the maintainers
/// say it is about. Most of that was already in the tree we serve and
/// had never been read.
///
/// The order is GitHub's, deliberately (FORGE-UX §1): description,
/// homepage, topics, the health files, the counts, then the
/// tags/forks/contributors/languages blocks. That order is what a
/// migrating maintainer's eye already knows how to scan, and copying the
/// muscle memory costs us nothing.
///
/// **The one place we refuse to copy GitHub is the health-file block.**
/// GitHub omits a row when the file is missing, so a project with no
/// code of conduct and no security policy looks exactly like a project
/// that has both — the reader cannot tell an absent row from a row that
/// was never going to be there. We draw all six, always: present rows
/// link in `--brand`, absent rows are muted and say the plain word
/// "None". A maintainer sees the gap; a contributor sees the honesty.
///
/// Everything *else* here degrades to nothing. A repository with no tags
/// renders no tags block, one with no recognised source renders no
/// language bar, and any of the four fetches below failing takes its own
/// block away and nothing else. The repository page is not broken
/// because its sidebar is, and an error box in a rail is furniture
/// nobody can act on.
///
/// The session is `viewerSession(owner, token)` and never `anon(owner)`:
/// the server filters this panel by who is asking, so a token-holding
/// member reading their own private repository must send their
/// credential or be shown nothing and told nothing about why.

// ---------------------------------------------------------------------------
// The pure logic. Everything below this line that a test can reach without a
// DOM lives here and is unit-tested in `about.test.ts`; the components under
// it are arrangement.
// ---------------------------------------------------------------------------

/// The six rows of the community contract, keyed on what they are and
/// not on the file that happens to carry them.
export type HealthKind =
  | "readme"
  | "license"
  | "code_of_conduct"
  | "contributing"
  | "security"
  | "activity";

export interface HealthRow {
  kind: HealthKind;
  /// What the row reads as. Constant for five of the six; the licence
  /// row's label *is* the answer, which is why it comes from
  /// [`licenseLabel`] rather than from a table.
  label: string;
  /// Tree paths this row links to. Empty is the absent case for the five
  /// file rows — and for `activity`, which is a page rather than a file
  /// and is therefore always present with nothing in the tree to point
  /// at. More than one path only ever happens for a dual-licensed
  /// project, and the rail lists them all rather than picking one.
  paths: string[];
  /// Whether the project has this at all. Derived rather than inferred
  /// from `paths.length` because `activity` breaks that equivalence, and
  /// a reader of this type should not have to know which rows are files.
  present: boolean;
}

/// How the licence row reads, in three answers.
///
/// One recognised file reads `MIT license` — GitHub's exact wording,
/// built from the SPDX id rather than the long name, because that is the
/// string somebody arriving from there is looking for.
///
/// One unrecognised file says so rather than being left blank or, far
/// worse, guessed at. The row still links to the file, which is what a
/// reader actually needs.
///
/// Several files is neither. Naming one would be the most misleading
/// thing here, and saying nothing makes a dual-licensed project read as
/// unlicensed — so it says how many and the row lists them.
export function licenseLabel(license: License | null): string {
  if (!license) return "License";
  if (license.files.length > 1) return `${license.files.length} licenses found`;
  return license.recognised && license.spdx
    ? `${license.spdx} license`
    : "License (unrecognised)";
}

/// The six health rows, in FORGE-UX §1's order, present or absent.
///
/// Always six, always in this order, whatever the server said. That is
/// the entire point: a shorter list is a list a reader cannot audit,
/// because they would have to know what could have been in it.
///
/// Takes a loaded `RepoMeta` rather than `RepoMeta | null` on purpose. A
/// null-tolerant version would render six rows reading "None" while the
/// panel was still loading, which says "this project has nothing" at
/// exactly the moment we do not know.
export function healthRows(meta: RepoMeta): HealthRow[] {
  const community = (kind: "code_of_conduct" | "contributing" | "security") =>
    meta.community.find((c) => c.kind === kind)?.path ?? null;
  const row = (
    kind: HealthKind,
    label: string,
    path: string | null,
  ): HealthRow => ({
    kind,
    label,
    paths: path === null ? [] : [path],
    present: path !== null,
  });

  return [
    row("readme", "Readme", meta.readme ?? null),
    {
      kind: "license",
      label: licenseLabel(meta.license),
      paths: meta.license?.files ?? [],
      present: meta.license !== null,
    },
    row("code_of_conduct", "Code of conduct", community("code_of_conduct")),
    row("contributing", "Contributing", community("contributing")),
    row("security", "Security policy", community("security")),
    // Not a file, and therefore never absent: it is the repository's own
    // history, which exists the moment the repository does.
    { kind: "activity", label: "Activity", paths: [], present: true },
  ];
}

/// A version tag, parsed far enough to be ordered against another one.
export interface Version {
  major: number;
  minor: number;
  patch: number;
  /// The prerelease identifiers, or `null` for a release. `1.0.0-rc.1`
  /// carries `"rc.1"`.
  pre: string | null;
}

/// Read a tag name as a version, or refuse.
///
/// `MAJOR.MINOR` is the minimum, with an optional `v` prefix and an
/// optional patch. A bare `2024` is not a version — it is a year, or a
/// build number, or a word that happens to be digits — and treating it
/// as `2024.0.0` is how a date-stamped tag jumps over every release.
/// Build metadata after `+` is stripped, because semver says it takes no
/// part in precedence.
export function parseVersionTag(name: string): Version | null {
  const core = name.split("+")[0];
  const m = /^[vV]?(\d+)\.(\d+)(?:\.(\d+))?(?:-(.+))?$/.exec(core);
  if (!m) return null;
  return {
    major: Number(m[1]),
    minor: Number(m[2]),
    patch: m[3] === undefined ? 0 : Number(m[3]),
    pre: m[4] ?? null,
  };
}

/// Semver's prerelease precedence, which is not string order.
///
/// `rc.10` is later than `rc.2` and a plain string comparison says the
/// opposite, which is the whole reason this is written out: identifiers
/// that are all digits compare numerically, a numeric identifier ranks
/// below an alphanumeric one, and a shorter identifier list ranks below
/// a longer one that shares its prefix.
function comparePre(a: string, b: string): number {
  const xs = a.split(".");
  const ys = b.split(".");
  for (let i = 0; i < Math.max(xs.length, ys.length); i++) {
    const x = xs[i];
    const y = ys[i];
    if (x === undefined) return -1;
    if (y === undefined) return 1;
    if (x === y) continue;
    const nx = /^\d+$/.test(x);
    const ny = /^\d+$/.test(y);
    if (nx && ny) return Number(x) - Number(y);
    if (nx !== ny) return nx ? -1 : 1;
    return x < y ? -1 : 1;
  }
  return 0;
}

/// Order two versions. Negative when `a` is older.
export function compareVersions(a: Version, b: Version): number {
  if (a.major !== b.major) return a.major - b.major;
  if (a.minor !== b.minor) return a.minor - b.minor;
  if (a.patch !== b.patch) return a.patch - b.patch;
  if (a.pre === null && b.pre === null) return 0;
  // A release outranks every prerelease of itself: `1.0.0` is later than
  // `1.0.0-rc.9`, which is the one rule people would notice us getting
  // backwards.
  if (a.pre === null) return 1;
  if (b.pre === null) return -1;
  return comparePre(a.pre, b.pre);
}

/// Which tag to show as the latest one.
///
/// **A `RefName` carries no date**, so "latest" cannot mean "most
/// recently created" here — the server does not tell us, and inventing
/// an ordering out of the list's arrival order would be a guess wearing
/// a badge that says "Latest". The only honest ordering a tag name
/// supports is the one its *name* declares, so:
///
/// - tags that parse as versions are ordered by version precedence, and
///   the greatest wins. That is what every package ecosystem means by
///   latest, and a maintainer who tagged `v2.0.0` after `v1.9.3` will
///   agree with the answer whatever order we read them in;
/// - a repository whose tags are *not* versions — `nightly`,
///   `2024-06-01`, `ship-it` — gets **no** latest tag rather than an
///   arbitrary one. The block still says how many tags there are, which
///   is true, and says nothing about which is newest, which is all we
///   know.
///
/// Ties (`v1.2.0` beside `1.2.0`) keep the first one seen, so the answer
/// is stable for a stable list.
export function latestTag(tags: RefName[]): RefName | null {
  let best: RefName | null = null;
  let bestVersion: Version | null = null;
  for (const tag of tags) {
    const v = parseVersionTag(tag.name);
    if (v === null) continue;
    if (bestVersion === null || compareVersions(v, bestVersion) > 0) {
      best = tag;
      bestVersion = v;
    }
  }
  return best;
}

/// Cut a list down to what fits, and say how much was cut.
///
/// The one non-obvious rule: **never hide exactly one item.** A "+1"
/// occupies about as much room as the thing it stands for, so trading
/// the last topic for a badge that says there is one more topic is a
/// worse rail and an ungenerous one. So the cap is soft by one.
export function overflow<T>(
  items: T[],
  max: number,
): { shown: T[]; extra: number } {
  if (items.length <= max + 1) return { shown: items, extra: 0 };
  return { shown: items.slice(0, max), extra: items.length - max };
}

/// How many topic pills before the "+N".
///
/// FORGE-UX asks for "max 2 rows then +N", and a row count is a layout
/// fact this code cannot see: measuring it means a `ResizeObserver` and
/// a re-render per pill, and clamping it in CSS means the clamp gradient
/// the spec explicitly refuses. Eight is two comfortable rows of
/// short topics in a 288px rail, and the cap is soft by one either way.
export const TOPIC_LIMIT = 8;

/// Six across, two rows, matching FORGE-UX's contributor grid.
export const CONTRIBUTOR_LIMIT = 12;

export const FORK_LIMIT = 5;

/// The counts block: `N stars`, `N watching`, `N forks`.
///
/// Singular where English has one — "1 star", not "1 stars" — and
/// "watching" either way, because it is a participle and not a noun.
/// Zero is rendered, not hidden: a project with no stars is a fact about
/// the project, and a missing row would read as a missing feature.
export function countRows(input: {
  stars: number;
  watchers: number;
  forks: number;
}): { kind: "stars" | "watching" | "forks"; value: number; label: string }[] {
  return [
    {
      kind: "stars",
      value: input.stars,
      label: input.stars === 1 ? "star" : "stars",
    },
    { kind: "watching", value: input.watchers, label: "watching" },
    {
      kind: "forks",
      value: input.forks,
      label: input.forks === 1 ? "fork" : "forks",
    },
  ];
}

/// "60.3k on GitHub", or nothing at all.
///
/// The single most useful thing this page can tell somebody deciding
/// whether a migrated project is alive, and the single most damaging
/// thing it could say if it guessed. `origin` is `null` when we never
/// imported a count, which is *not* an origin that reported zero — so
/// this returns `null` rather than a `0 on GitHub` under a project with
/// sixty thousand stars.
///
/// The two counts are never added. A mirrored project's honest `☆ 4`
/// above a labelled "60.3k on GitHub" tells the truth twice, where our
/// count alone says a migrated project is dead and a sum says something
/// nobody can check (FORGE-UX §6).
export function originStarsLine(origin: OriginStars | null): string | null {
  if (!origin) return null;
  return `${formatCount(origin.stars)} on ${originLabel(origin.url)}`;
}

/// A homepage as a reader wants to see it: no scheme, no trailing slash.
///
/// Display only. The `href` is always the string the server gave us,
/// which it has already refused unless it was absolute http(s) — so this
/// never has to be safe, only short.
export function homepageLabel(url: string): string {
  return url
    .trim()
    .replace(/^https?:\/\//i, "")
    .replace(/\/+$/, "");
}

// ---------------------------------------------------------------------------
// The rail.
// ---------------------------------------------------------------------------

/// Which glyph each health row wears. Keyed on the stable `kind`, never
/// on a filename: the file may be `CONTRIBUTING`, `CONTRIBUTING.md` or
/// `.github/CONTRIBUTING.rst`, and the client has no business knowing
/// which.
const HEALTH_ICON: Record<HealthKind, typeof BookOpen> = {
  readme: BookOpen,
  license: Scale,
  code_of_conduct: Users,
  contributing: HeartHandshake,
  security: ShieldAlert,
  activity: Activity,
};

const COUNT_ICON = {
  stars: Star,
  watching: Eye,
  forks: GitFork,
} as const;

export function About(props: {
  owner: string;
  /// The row, when the page has it. `null` while it is loading or when
  /// the viewer may not see the repository — in the second case this
  /// component is never mounted.
  repo: Repo | null;
  /// The caller's API token, when they signed in with one. Empty for a
  /// stranger; see the note above about `anon()`.
  token: string | null;
  /// Whether to offer the topic editor. Computed by the page from the
  /// viewer's role in the namespace, and only ever *additive*: getting
  /// it wrong shows a form the server refuses, never hides a fact.
  canEdit?: boolean;
}) {
  const repo = props.repo;
  const name = repo?.name ?? null;
  // A fork has a row before it has objects. The licence, README and
  // tag rows are read out of the objects, so they wait for the row to
  // say `ready` — and are asked for then, on this page, rather than on
  // the reload somebody used to need. Stars, forks and contributors are
  // the row's own facts and answer at any time.
  const hasObjects =
    repo !== null &&
    repo.fork_state !== "pending" &&
    repo.fork_state !== "failed";
  const { owner, token } = props;
  const [meta, setMeta] = useState<RepoMeta | null>(null);
  const [stars, setStars] = useState<StarState | null>(null);
  const [tags, setTags] = useState<RefName[] | null>(null);
  const [forks, setForks] = useState<ForkEntry[] | null>(null);
  const [people, setPeople] = useState<Contributor[] | null>(null);
  const [editing, setEditing] = useState(false);

  useEffect(() => {
    let alive = true;
    setMeta(null);
    setStars(null);
    setTags(null);
    setForks(null);
    setPeople(null);
    if (!name) return;
    const session = viewerSession(owner, token);
    // Five independent reads, five independent failures. Each `catch`
    // is deliberately empty and deliberately per-request: a repository
    // with no tags endpoint on an older server still gets its licence
    // row, and a rail that blanked itself because one adornment was
    // refused would be a worse page than one that is simply shorter.
    const keep =
      <T,>(set: (v: T) => void) =>
      (v: T) => {
        if (alive) set(v);
      };
    if (hasObjects) {
      api
        .repoMeta(session, name)
        .then(keep(setMeta))
        .catch(() => undefined);
      api
        .tags(session, name)
        .then(keep(setTags))
        .catch(() => undefined);
    }
    api
      .getStars(session, name)
      .then(keep(setStars))
      .catch(() => undefined);
    api
      .forks(session, name)
      .then(keep((f: { forks: ForkEntry[] }) => setForks(f.forks)))
      .catch(() => undefined);
    api
      .contributors(session, name)
      .then(keep(setPeople))
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [owner, name, token, hasObjects]);

  if (!repo) return null;
  const filePath = (path: string) =>
    href([owner, repo.name, "tree", ...path.split("/")]);

  const homepage = repo.homepage ?? null;
  const latest = latestTag(tags ?? []);
  const forkList = overflow(forks ?? [], FORK_LIMIT);
  const contributors = overflow(people ?? [], CONTRIBUTOR_LIMIT);
  const originLine = originStarsLine(stars?.origin ?? null);

  return (
    <aside className="w-full min-w-0 shrink-0 space-y-4 md:w-72">
      <div>
        <div className="flex items-center justify-between gap-2">
          <h2 className="text-sm font-semibold text-ink">About</h2>
          {props.canEdit === true && (
            // A gear, not the word "Manage": the rail is 288px wide and
            // the header is the only place an edit affordance can sit
            // without displacing the description. It carries a label
            // because a button with neither text nor `aria-label` fails
            // the walkthrough's audit — and, rather more to the point,
            // is a button a screen reader announces as "button".
            <button
              type="button"
              aria-label={editing ? "Done editing topics" : "Edit topics"}
              aria-pressed={editing}
              className={cn(
                "rounded-sm p-0.5 text-ink-3 transition hover:text-ink",
                FOCUS_RING,
                editing && "text-ink",
              )}
              onClick={() => setEditing((e) => !e)}
            >
              <Settings aria-hidden className="size-4" />
            </button>
          )}
        </div>
        {/* Three lines and then it stops. No fade: a gradient over the
            fourth line is a decoration that hides whether there is more
            text, and the description is on the repository row anyway for
            anything that needs all of it. */}
        <p className="mt-1 line-clamp-3 text-sm text-ink-2">
          {repo.description ?? "No description."}
        </p>

        {homepage && (
          <p className="mt-2 flex items-center gap-1.5 text-sm">
            <LinkIcon aria-hidden className="size-4 shrink-0 text-ink-3" />
            <a
              className={cn(PROSE_LINK, "min-w-0 truncate font-medium")}
              href={homepage}
              rel="nofollow noopener noreferrer"
            >
              {homepageLabel(homepage)}
            </a>
          </p>
        )}
      </div>

      <Topics
        owner={owner}
        repo={repo.name}
        token={token}
        topics={meta?.topics ?? null}
        canEdit={props.canEdit === true}
        editing={editing}
        onSaved={(topics) => setMeta((m) => (m ? { ...m, topics } : m))}
      />

      {meta && (
        <>
          <hr className="border-borderline" />
          <ul className="space-y-1.5">
            {healthRows(meta).map((r) => (
              <HealthListRow
                key={r.kind}
                row={r}
                filePath={filePath}
                // Commits, not `insights`. There is no Insights page —
                // it was deliberately left out of the tab strip on the
                // rule stated above `TABS` in `forge/index.tsx`, "add a
                // tab here when its body exists, not when its name is
                // decided", because a 404 here invites a visitor to
                // sign in for a feature that does not exist. This rail
                // went on linking to it anyway, so every repository
                // page in the product carried one dead link.
                //
                // Commits is the honest target: this row is documented
                // as "the repository's own history, which exists the
                // moment the repository does", and that is the page
                // showing it.
                activityHref={href([owner, repo.name, "commits"])}
              />
            ))}
          </ul>
        </>
      )}

      <hr className="border-borderline" />
      <div>
        <ul className="space-y-1.5 text-sm">
          {countRows({
            stars: stars?.stars ?? 0,
            watchers: repo.watcher_count ?? 0,
            forks: repo.fork_count ?? 0,
          }).map((c) => {
            const Icon = COUNT_ICON[c.kind];
            return (
              <li key={c.kind} className="flex items-center gap-2">
                <Icon aria-hidden className="size-4 shrink-0 text-ink-3" />
                {/* Numbers always render in mono (DESIGN.md). */}
                <span className="font-mono text-ink">
                  {formatCount(c.value)}
                </span>{" "}
                <span className="min-w-0 truncate text-ink-2">{c.label}</span>
              </li>
            );
          })}
        </ul>
        {originLine && stars?.origin && (
          <p className="mt-1.5 pl-6 text-xs text-ink-3">
            {originHref(stars.origin.url) ? (
              <a
                className={STRUCTURAL_LINK_2}
                href={originHref(stars.origin.url) ?? undefined}
                rel="nofollow noopener noreferrer"
              >
                {originLine}
              </a>
            ) : (
              <span>{originLine}</span>
            )}
            {stars.origin.at !== null && (
              <>
                {" "}
                <span>
                  read{" "}
                  <RelativeTime at={stars.origin.at} className="text-ink-3" />
                </span>
              </>
            )}
          </p>
        )}
      </div>

      {tags && tags.length > 0 && (
        <>
          <hr className="border-borderline" />
          <div>
            <RailHeading label="Tags" count={tags.length} />
            {latest ? (
              <p className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 text-sm">
                <Tag aria-hidden className="size-4 shrink-0 text-ink-3" />
                <a
                  className={cn(
                    STRUCTURAL_LINK_2,
                    "min-w-0 truncate font-medium",
                  )}
                  href={href([owner, repo.name], { at: latest.name })}
                >
                  {latest.name}
                </a>{" "}
                <Badge variant="neutral">Latest</Badge>
              </p>
            ) : (
              // Every tag this project uses is a word, not a version.
              // We know how many there are and we do not know which is
              // newest, so the block says the first and not the second.
              <p className="mt-2 text-sm text-ink-3">
                No version tags — nothing here orders as a release.
              </p>
            )}
          </div>
        </>
      )}

      {forks && forks.length > 0 && (
        <>
          <hr className="border-borderline" />
          <div>
            <RailHeading label="Forks of this" count={forks.length} />
            <ul className="mt-2 space-y-1">
              {forkList.shown.map((f) => (
                <li key={`${f.org}/${f.name}`} className="flex min-w-0 text-sm">
                  <a
                    className={cn(STRUCTURAL_LINK_2, "min-w-0 truncate")}
                    href={href([f.org, f.name])}
                  >
                    {f.org}/{f.name}
                  </a>
                </li>
              ))}
            </ul>
            {forkList.extra > 0 && (
              <p className="mt-1 text-xs text-ink-3">
                <span className="font-mono">+{forkList.extra}</span> more
              </p>
            )}
          </div>
        </>
      )}

      {people && people.length > 0 && (
        <>
          <hr className="border-borderline" />
          <div>
            <RailHeading label="Contributors" count={people.length} />
            <ul className="mt-2 grid grid-cols-6 gap-1.5">
              {contributors.shown.map((c) => (
                <li key={c.user_id} className="min-w-0">
                  <a
                    href={href([c.handle])}
                    // The avatar is the whole link, so the name has to
                    // arrive some other way or this is an anchor a
                    // screen reader announces as "link".
                    aria-label={`${c.handle}, ${c.commits} commits`}
                    className={cn("inline-block rounded-full", FOCUS_RING)}
                  >
                    <OwnerAvatar name={c.handle} size={28} />
                  </a>
                </li>
              ))}
            </ul>
            {contributors.extra > 0 && (
              <p className="mt-2 text-xs text-ink-3">
                <span className="font-mono">+{contributors.extra}</span>{" "}
                contributors
              </p>
            )}
          </div>
        </>
      )}

      {meta && (
        <>
          <hr className="border-borderline" />
          <LanguageBar
            languages={meta.languages}
            partial={meta.languages_truncated}
          />
        </>
      )}
    </aside>
  );
}

/// A block heading and its count, spelled once so the four blocks under
/// the counts cannot drift apart.
function RailHeading(props: { label: string; count: number }) {
  return (
    <h3 className="flex items-baseline gap-1.5 text-sm font-semibold text-ink">
      <span className="min-w-0 truncate">{props.label}</span>{" "}
      <span className="font-mono text-xs font-normal text-ink-3">
        {formatCount(props.count)}
      </span>
    </h3>
  );
}

/// One row of the community contract.
///
/// Present and absent are two different rows, not one row with a colour
/// swapped: the present one is a brand-coloured link with a filled
/// glyph, and the absent one is muted, unclickable, and carries the word
/// "None". Colour is never the only carrier — DESIGN.md's rule, and here
/// it is also the difference between a reader believing a file exists
/// and knowing it does not.
///
/// "Filled" is a wash inside the glyph rather than a solid shape.
/// Lucide's icons are outlines, and a solid `Scale` or `BookOpen` is an
/// unreadable blob at 16px; `fill-brand/15` under a `--brand` stroke
/// reads as filled at that size and stays legible in both themes.
function HealthListRow(props: {
  row: HealthRow;
  filePath: (path: string) => string;
  activityHref: string;
}) {
  const { row } = props;
  const Icon = HEALTH_ICON[row.kind];
  return (
    <li className="flex items-start gap-2 text-sm">
      <Icon
        aria-hidden
        className={cn(
          "mt-0.5 size-4 shrink-0",
          row.present ? "fill-brand/15 text-brand" : "text-ink-3",
        )}
      />
      <div className="flex min-w-0 flex-1 items-baseline justify-between gap-2">
        <div className="min-w-0">
          {!row.present ? (
            <span className="text-ink-3">{row.label}</span>
          ) : row.kind === "activity" ? (
            <a className={PROSE_LINK} href={props.activityHref}>
              {row.label}
            </a>
          ) : row.paths.length === 1 ? (
            <a
              className={cn(PROSE_LINK, "block truncate")}
              href={props.filePath(row.paths[0])}
            >
              {row.label}
            </a>
          ) : (
            <>
              {/* A dual-licensed project. Naming one of the files would
                  be the most misleading thing this row could do, so it
                  names none of them and lists all of them. */}
              <span className="text-ink-2">{row.label}</span>
              <ul className="mt-0.5 flex flex-wrap gap-x-3 gap-y-0.5">
                {row.paths.map((p) => (
                  <li key={p} className="min-w-0">
                    <a
                      className={cn(PROSE_LINK, "block truncate")}
                      href={props.filePath(p)}
                    >
                      {p}
                    </a>
                  </li>
                ))}
              </ul>
            </>
          )}
        </div>
        {!row.present && (
          <span className="shrink-0 text-xs text-ink-3">None</span>
        )}
      </div>
    </li>
  );
}

/// The topic pills, and the form for changing them.
///
/// Each pill links to `/explore?topic=…`, which is what a topic is
/// *for*: a word that finds the other projects like this one. A pill
/// that only sat there would be a label, not a topic.
///
/// The pills are **not** brand-coloured. A row of twelve emerald pills
/// turns the page into a brand swatch and buries the description above
/// it (FORGE-UX §1); the hover is where the brand shows up.
///
/// The editor sends the whole set, because that is what it holds: the
/// server's `PUT` replaces rather than merges, and a form that showed
/// you five words and sent one would be lying about what it did.
///
/// It does not validate before sending. The rules live in the control
/// plane — lowercase, ASCII, at most twenty of at most thirty-five
/// characters — and a second copy here would be a second place for them
/// to drift, with the client's copy silently becoming the stricter one.
/// The server's own sentence is what the reader sees.
function Topics(props: {
  owner: string;
  repo: string;
  token: string | null;
  /// `null` while the panel is loading. An empty array is a different
  /// thing — "this project has chosen no topics" — and renders as
  /// nothing at all rather than as an empty row.
  topics: string[] | null;
  canEdit: boolean;
  /// Driven by the rail's own header gear, so that one control governs
  /// the whole panel's edit mode rather than each block growing its own.
  editing: boolean;
  onSaved: (topics: string[]) => void;
}) {
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const topics = props.topics;
  const editing = props.editing && props.canEdit;

  if (topics === null) return null;
  if (topics.length === 0 && !props.canEdit) return null;

  // Two rows of pills and then a count. `overflow` refuses to hide
  // exactly one, so a ninth topic is shown rather than counted.
  const shown = editing
    ? { shown: topics, extra: 0 }
    : overflow(topics, TOPIC_LIMIT);

  async function save(next: string[]) {
    setSaving(true);
    setError(null);
    try {
      const out = await api.setTopics(
        viewerSession(props.owner, props.token),
        props.repo,
        next,
      );
      props.onSaved(out.topics);
      setDraft("");
    } catch (e) {
      // The server's own sentence, verbatim. It is the one that names
      // which word was refused and why.
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div>
      {topics.length > 0 && (
        <ul className="flex flex-wrap items-center gap-1.5">
          {/* The remove control is a sibling of the link, never a child
              of it: a `<button>` inside an `<a>` is invalid HTML, and
              browsers recover from it differently — which is how a
              "remove" click ends up navigating instead. */}
          {shown.shown.map((t) => (
            <li
              key={t}
              className="inline-flex min-w-0 items-center gap-1 rounded-full border border-borderline bg-surface-2 px-3 py-0.5 text-xs font-medium text-ink-2 transition hover:border-brand/40 hover:text-ink"
            >
              <a
                href={href(["explore"], { topic: t })}
                className={cn("min-w-0 truncate rounded-sm", FOCUS_RING)}
              >
                {t}
              </a>
              {editing && (
                <button
                  type="button"
                  aria-label={`Remove topic ${t}`}
                  disabled={saving}
                  className={cn(
                    "shrink-0 rounded-sm text-ink-3 hover:text-serious",
                    FOCUS_RING,
                  )}
                  onClick={() => void save(topics.filter((x) => x !== t))}
                >
                  <X aria-hidden className="size-3" />
                </button>
              )}
            </li>
          ))}
          {shown.extra > 0 && (
            <li className="font-mono text-xs text-ink-3">+{shown.extra}</li>
          )}
        </ul>
      )}
      {topics.length === 0 && props.canEdit && !editing && (
        <p className="text-sm text-ink-3">
          No topics yet — a word or two makes this findable.
        </p>
      )}

      {editing && (
        <form
          className="mt-2 space-y-2"
          onSubmit={(e) => {
            e.preventDefault();
            const word = draft.trim();
            if (!word) return;
            void save([...topics, word]);
          }}
        >
          <Input
            value={draft}
            disabled={saving}
            placeholder="Add a topic"
            aria-label="Add a topic"
            onChange={(e) => setDraft(e.target.value)}
          />
          <Err message={error} />
          <Button type="submit" size="xs" disabled={saving || !draft.trim()}>
            Add
          </Button>
        </form>
      )}
    </div>
  );
}
