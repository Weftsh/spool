/// Every address this SPA answers to, in one table.
///
/// Dispatch used to be a chain of ternaries reading `segments(...)[0]`
/// inline, which was fine while there was one address space. There are
/// now two — the signed-in dashboard under `/dashboard`, and the public
/// forge at the root, where `/{owner}/{repo}` is a repository somebody
/// can read without an account — and a chain of ternaries that has to
/// decide *which space* before it decides which page is a chain that
/// gets one case wrong quietly.
///
/// The other reason this is a file rather than a function: [`TOP_LEVEL`]
/// is a contract with the server. A namespace named `explore` would be a
/// namespace nobody could ever reach, so every entry here has to be in
/// the reserved-name denylist in `crates/stratum-control/src/registry.rs`
/// — and a test in `crates/stratum-server/tests/docs_e2e.rs` reads this
/// array to prove it. The Rust-side scraper cannot infer these: it reads
/// `.route()` literals out of `app.rs`, and none of these are routes.

import { segments } from "./router";

/// The tabs a repository page has. Deliberately fewer than GitHub's:
/// a tab that leads nowhere is worse than an absent one.
export type RepoTab =
  | "code"
  | "commits"
  | "commit"
  | "issues"
  | "changes"
  | "checks"
  | "insights"
  | "settings";

/// The tabs a profile has, user or organization alike.
export type OwnerTab =
  "overview" | "repositories" | "stars" | "followers" | "following";

/// Every first path segment the SPA claims for itself.
///
/// Keep this sorted and keep it honest: adding a route here without
/// reserving the name server-side is how somebody signs up for a handle
/// they can never clone from.
export const TOP_LEVEL = [
  "dashboard",
  "explore",
  "feed",
  "issues",
  "login",
  "notifications",
  "orgs",
  "search",
  "stars",
  "topics",
] as const;

/// Where to return after signing in — but only somewhere on this site.
///
/// The obvious spelling is `startsWith("/") && !startsWith("//")`, which
/// is what this was, and it is wrong. WHATWG URL treats a backslash like
/// a slash for http(s), so `/\evil.com/p` begins with exactly one
/// slash, carries no scheme, looks relative to any reasonable eye — and
/// resolves to `https://evil.com/p`. An open redirect on a login page is
/// a phishing kit somebody else gets to host on our domain, which is the
/// thing this guard exists to prevent and did not.
///
/// So it does not pattern-match at all. It resolves the candidate the
/// way a browser would and keeps it only if it landed on our own origin.
/// The sentinel origin is a constant rather than `window.location` so the
/// rule is testable without a DOM and behaves identically wherever it
/// runs.
///
/// Found by propagating a `new URL` finding from the markdown renderer,
/// where the same parser quirk let a relative-looking image path escape
/// to another host. It is a property of URL parsing, not of markdown, so
/// it is worth suspecting anywhere a user-supplied path is resolved.
const SAME_SITE = "https://stratum.invalid";

/// Which form `/login` opens on. Anything that is not exactly `signup`
/// is sign-in: an unknown mode is a typo in a link, and a typo should
/// land somebody on the ordinary door rather than on nothing.
export type LoginMode = "signin" | "signup";

export function loginMode(candidate: string | null): LoginMode {
  return candidate === "signup" ? "signup" : "signin";
}

export function safeNext(candidate: string | null): string {
  if (!candidate) return "/";
  try {
    const url = new URL(candidate, SAME_SITE);
    if (url.origin !== SAME_SITE) return "/";
    return `${url.pathname}${url.search}${url.hash}`;
  } catch {
    return "/";
  }
}

export type Match =
  /// Anything under `/dashboard`. The signed-in shell owns the rest of
  /// the path; this table deliberately does not re-decide it, because
  /// that dispatch is covered by ~75 tests that assert on those URLs.
  | { kind: "dash" }
  /// `/login`, and `/login?mode=signup` for the marketing site's "Sign
  /// up free" button. `mode` is a query parameter rather than a
  /// `/signup` path because a new first path segment is a new entry in
  /// `TOP_LEVEL`, which is a server-side reservation *and* an entry in
  /// `webassets.rs`'s `SPA_SEGMENTS` — three files for one link.
  | { kind: "login"; next: string; mode: LoginMode }
  /// `/explore`, optionally narrowed to one topic by `?topic=`.
  ///
  /// A query parameter rather than `/topics/{name}` because that is the
  /// address the About rail's pills have always pointed at and the one
  /// `topics::normalize` documents ("a topic ends up in
  /// `/explore?topic=…`"). Adding a second live URL for the same listing
  /// would be two things to share, two to keep in the header's active
  /// logic and two for a crawler to index — the same argument that keeps
  /// `/actions` a redirect rather than a second name for Checks.
  | { kind: "explore"; topic: string }
  | { kind: "search"; q: string }
  | { kind: "feed" }
  | { kind: "owner"; owner: string; tab: OwnerTab }
  /// One changeset, at `/{owner}/changesets/{key}` — the same object the
  /// dashboard shows at `/dashboard/changesets/{key}`, at an address a
  /// stranger can open.
  ///
  /// Safe as a second path segment because `changesets` is already a
  /// reserved repository name in
  /// `crates/stratum-control/src/registry.rs`: the workspace git front
  /// door has been serving `/{org}/changesets/{key}` over HTTP and SSH
  /// since changesets existed, so nobody can own a repository by that
  /// name and this arm can never shadow one. `routes.test.ts` asserts
  /// that reservation against the Rust source rather than trusting it.
  | { kind: "changeset"; owner: string; key: string }
  | {
      kind: "repo";
      owner: string;
      repo: string;
      tab: RepoTab;
      rest: string[];
      /// Set when the address typed was an alias for this tab's real
      /// one. The dispatcher navigates here with `replace: true`; the
      /// page itself is identical either way, so nothing below this
      /// needs to know.
      redirect?: string;
    }
  | { kind: "not-found" };

/// Every repo path this router understands. Not every one is a *tab* —
/// `commits` is reached from the commit bar above the file list, exactly
/// as GitHub reaches it, because a repository's history is something you
/// go and look at rather than a place you sit.
const REPO_TABS: RepoTab[] = [
  "code",
  "commits",
  // `/{owner}/{repo}/commit/{sha}` — singular, as git and GitHub both
  // spell it, and distinct from the plural list.
  "commit",
  "issues",
  "changes",
  // CI verdicts from whatever actually ran them, ours included: a
  // repository's `.weft/*.yml` workflows run on Weft's own
  // runners and each run has a page under this tab —
  // `/{owner}/{repo}/checks/runs/{id}`, dispatched from `rest`. Named
  // "checks" and not "actions" deliberately: the tab carries a
  // Buildkite or GitLab CI project's runs beside a GitHub Actions one
  // and beside a hosted one, and a tab named after a competitor's
  // product would imply the wrong thing about them and about the rest
  // of what is in it. `/actions` still answers — see `REPO_ALIASES` —
  // because it is the address muscle memory types, and it carries its
  // deep links across, so `/o/r/actions/runs/7` lands on the run.
  "checks",
  "insights",
  "settings",
];

/// Repo path segments that are not tabs but redirect to one.
///
/// `/{owner}/{repo}/actions` is what somebody arriving from GitHub
/// types, and 404ing them to prove a naming point helps nobody. It
/// redirects rather than being a second name for the tab, so there is
/// one address for the page: two live URLs for one thing is two things
/// to keep in the tab strip's `active` logic, two to share, and two for
/// a crawler to index.
///
/// Not in `TOP_LEVEL`: this is a third path segment, so it can never
/// collide with a namespace and needs no server-side reservation.
const REPO_ALIASES: Record<string, RepoTab> = { actions: "checks" };

const OWNER_TABS: OwnerTab[] = [
  "overview",
  "repositories",
  "stars",
  "followers",
  "following",
];

/// A profile's tab rides in `?tab=`, exactly as GitHub's does, and for
/// the same reason: if it were a path segment then `/ada/stars` would be
/// ambiguous with a repository of that name, and the disambiguation
/// would have to be a lookup rather than a parse.
/// A query string as it goes back into an address, or nothing.
///
/// `""` rather than `"?"` for an empty query: a bare trailing `?` is a
/// different URL to share, to bookmark and to compare in a test, for no
/// gain.
function suffix(query: URLSearchParams): string {
  const q = query.toString();
  return q ? `?${q}` : "";
}

function ownerTab(query: URLSearchParams): OwnerTab {
  const t = query.get("tab");
  return OWNER_TABS.find((x) => x === t) ?? "overview";
}

/// Which page a browser address means.
///
/// `path` is the address bar's own path — not a dashboard-relative one —
/// because deciding between the two mounts is this function's first job.
export function match(path: string, query: URLSearchParams): Match {
  const parts = segments(path);
  const [first, second, third] = parts;

  if (first === undefined)
    return { kind: "explore", topic: query.get("topic") ?? "" };
  if (first === "dashboard") return { kind: "dash" };
  if (first === "login") {
    // Where to return after signing in. Only a path from this origin is
    // ever accepted: `?next=https://elsewhere/` on a login page is an
    // open redirect, and an open redirect on a login page is a phishing
    // kit somebody else gets to host on our domain.
    return {
      kind: "login",
      next: safeNext(query.get("next")),
      mode: loginMode(query.get("mode")),
    };
  }
  if (first === "explore")
    return { kind: "explore", topic: query.get("topic") ?? "" };
  // The header's search box has to land somewhere, and `/explore` is a
  // set of curated lists rather than a result page — sending a query
  // there would either be ignored or quietly redefine what explore
  // means. `search` was already a reserved name for exactly this.
  if (first === "search") return { kind: "search", q: query.get("q") ?? "" };
  if (first === "feed") return { kind: "feed" };
  // The remaining claimed segments have no page yet. They are reserved
  // rather than routed, so they must not fall through and be read as
  // somebody's namespace.
  if ((TOP_LEVEL as readonly string[]).includes(first)) {
    return { kind: "not-found" };
  }

  if (second === undefined)
    return { kind: "owner", owner: first, tab: ownerTab(query) };

  // A changeset, before the repository arm and not inside it: it is
  // owned by an organization rather than by any one repository, and
  // `changesets` is server-side reserved, so this can never shadow
  // somebody's repo.
  //
  // `/{owner}/changesets` with no key is deliberately not-found rather
  // than an org-wide list. That list's rows are filtered per caller —
  // you see the sets whose members you can read — so a public one would
  // silently hide half of itself, and a list that lies about its own
  // completeness is worse than no list. The signed-in list stays at
  // `/dashboard/changesets`, where the caller is known.
  if (second === "changesets") {
    if (third === undefined || parts.length > 3) return { kind: "not-found" };
    return { kind: "changeset", owner: first, key: third };
  }

  // A file path rides behind an explicit `tree`, the way GitHub's does,
  // and not because it is prettier. Without it a repository containing a
  // top-level directory named `issues` — which is not rare, docs
  // repositories do it — would have that directory shadowed by the
  // Issues tab, and the shadowing would depend on the repository's
  // contents, so it would be invisible until somebody's file stopped
  // opening.
  if (third === "tree") {
    return {
      kind: "repo",
      owner: first,
      repo: second,
      tab: "code",
      rest: parts.slice(3),
    };
  }

  // `/{owner}/{repo}` with nothing after it is the Code tab: that
  // address is what every README badge and every search result points
  // at, so it answers rather than 404s.
  if (third === undefined) {
    return { kind: "repo", owner: first, repo: second, tab: "code", rest: [] };
  }

  const alias = REPO_ALIASES[third];
  if (alias) {
    return {
      kind: "repo",
      owner: first,
      repo: second,
      tab: alias,
      rest: parts.slice(3),
      // Everything after the alias comes with it — segments *and*
      // query. A deep link — `/o/r/actions/runs/7` — that redirected to
      // the bare tab would silently drop what the reader was actually
      // asking for and land them on a list, which reads as the link
      // being stale rather than as us having truncated it. The query is
      // the same argument and was the same bug: the Checks page keeps
      // every one of its filters there, so `/o/r/actions?workflow=CI`
      // arrived at an unfiltered table with no sign that anything had
      // been dropped. Each segment is encoded, because these came out
      // of `segments()` already decoded and a `#` or `?` in one would
      // otherwise re-parse as syntax; `URLSearchParams.toString`
      // encodes the query for the same reason.
      redirect:
        [first, second, alias, ...parts.slice(3)]
          .map((p) => `/${encodeURIComponent(p)}`)
          .join("") + suffix(query),
    };
  }

  const tab = REPO_TABS.find((x) => x === third);
  if (!tab) return { kind: "not-found" };
  return {
    kind: "repo",
    owner: first,
    repo: second,
    tab,
    rest: parts.slice(3),
  };
}
