import { useCallback, useEffect, useMemo, useState } from "react";
import { MessageSquare, Search, X } from "lucide-react";
import {
  api,
  viewerSession,
  type Issue,
  type IssueComment,
  type IssueCounts,
  type IssueLabel,
  type IssueQuery,
  type Me,
  type Session,
} from "@/api";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { LabelPill } from "@/components/label-pill";
import { StateIcon } from "@/components/state-icon";
import { RelativeTime } from "@/components/relative-time";
import { Err, Loading } from "@/components/feedback";
import { Markdown } from "@/components/markdown";
import { NotFound } from "@/components/not-found";
import { FOCUS_RING, STRUCTURAL_LINK } from "@/lib/links";
import { cn } from "@/lib/utils";
import { href } from "@/router";

/// The issues index, one issue, and the form that files one.
///
/// GitHub's information architecture, in our skin. The one idea worth
/// copying wholesale is that **the query bar is the API**: every control
/// on the page writes into that text box, the text box is the URL, and
/// so every filtered list a maintainer builds is a link they can send.
/// A filter that lives only in component state is a view nobody else can
/// ever be shown.

// ---------------------------------------------------------------------
// The query language. Pure, and tested as such.
// ---------------------------------------------------------------------

/// The two orders the server can actually page.
///
/// GitHub defaults to "recently updated" and we deliberately do not
/// offer it. The page cursor is an issue number and the page is
/// `number < before`; ordering by `updated_at` while paging by `number`
/// puts a page boundary in the wrong place and skips or repeats rows —
/// silently, only on the second page, which is the kind of bug nobody
/// reports because it looks like the list simply being short. It needs a
/// compound cursor, which is not this slice.
///
/// The server refuses `sort=updated` with a 400 that says why. That
/// refusal is for callers of the API; it is not an invitation to wire a
/// menu item to it and let the control visibly fail.
export type SortKey = "newest" | "oldest";

export const SORTS: { key: SortKey; label: string }[] = [
  { key: "newest", label: "Newest" },
  { key: "oldest", label: "Oldest" },
];

export interface IssueFilter {
  state: "open" | "closed" | "all";
  label: string | null;
  author: string | null;
  sort: SortKey;
  /// Whatever was not a `key:value` — the words to search for.
  text: string;
}

/// What the index asks for before anybody has typed anything.
export const DEFAULT_QUERY = "is:issue is:open";

/// Split on whitespace, except inside double quotes.
///
/// `label:"good first issue"` is one token, and it is the label whose
/// name has the spaces in it that makes the whole quoting rule
/// necessary — that exact label is the one every "help wanted" list on
/// GitHub is built from.
function tokenize(text: string): string[] {
  return text.match(/(?:[^\s"]|"[^"]*")+/g) ?? [];
}

function unquote(value: string): string {
  const m = /^"([^"]*)"$/.exec(value);
  return m ? m[1] : value;
}

function quote(value: string): string {
  return /\s/.test(value) ? `"${value}"` : value;
}

/// Read a query bar into a filter.
///
/// The absence of `is:open` and `is:closed` means **all**, not open —
/// GitHub's semantics, and the reason they are right is that deleting
/// `is:open` from the box has to do something. If absence meant "open"
/// there would be no way to ask for both from the bar, and the bar
/// would stop being the API.
///
/// Anything unrecognised is treated as words to search for rather than
/// dropped, because a typo'd `athor:ada` silently matching everything is
/// worse than it matching nothing.
export function parseQuery(text: string): IssueFilter {
  const filter: IssueFilter = {
    state: "all",
    label: null,
    author: null,
    sort: "newest",
    text: "",
  };
  const words: string[] = [];
  for (const token of tokenize(text)) {
    const m = /^(is|label|author|sort):(.*)$/.exec(token);
    if (!m) {
      words.push(token);
      continue;
    }
    const value = unquote(m[2]);
    if (m[1] === "is") {
      if (value === "open" || value === "closed") filter.state = value;
      // `is:issue` is the noun this index lists, not a filter on it. It
      // is kept in the bar because it is the muscle memory of everybody
      // who has used GitHub, and because it is where a `is:pr` will go
      // when there are pull requests to switch to.
      continue;
    }
    if (m[1] === "label" && value) filter.label = value;
    if (m[1] === "author" && value) filter.author = value;
    if (m[1] === "sort" && SORTS.some((s) => s.key === value)) {
      filter.sort = value as SortKey;
    }
  }
  filter.text = words.join(" ");
  return filter;
}

/// Write a filter back out as a query bar. The inverse of `parseQuery`
/// for every filter it can produce, which is what lets a dropdown and
/// the text box be the same state rather than two copies of it.
export function formatQuery(filter: IssueFilter): string {
  const parts = ["is:issue"];
  if (filter.state !== "all") parts.push(`is:${filter.state}`);
  if (filter.author) parts.push(`author:${quote(filter.author)}`);
  if (filter.label) parts.push(`label:${quote(filter.label)}`);
  if (filter.sort !== "newest") parts.push(`sort:${filter.sort}`);
  if (filter.text) parts.push(filter.text);
  return parts.join(" ");
}

/// What of a filter goes on the wire.
///
/// **Ordering is the server's**, not this page's. Sorting the page that
/// came back would make "oldest" mean "the oldest of the newest
/// hundred" — a control that silently sorts the wrong population, which
/// is worse than an absent one because it looks like it works and the
/// manual browser pass cannot catch it.
///
/// `parseQuery` only ever produces a `sort` the server accepts, so this
/// cannot send the `sort=updated` that earns a 400: a link somebody
/// shared from an older bundle falls back to the default rather than
/// breaking the page it was pasted into.
export function toApiQuery(filter: IssueFilter): IssueQuery {
  const query: IssueQuery = { state: filter.state, sort: filter.sort };
  if (filter.label) query.label = filter.label;
  if (filter.author) query.author = filter.author;
  if (filter.text) query.q = filter.text;
  return query;
}

/// A conversation reads in insertion order, always.
///
/// `seq` is a BIGSERIAL the database assigned; `id` is a ULID whose tail
/// is random within a millisecond and `created_at` has millisecond
/// resolution. Two comments posted in the same millisecond — which is
/// what happens when a script files one — would otherwise render in an
/// order nobody typed. The client sorts rather than trusting the wire so
/// that a proxy, a cache or a future paginated endpoint cannot reorder a
/// thread without anybody noticing.
/// "comment" or "comments", for a count.
///
/// Both places that render this count said "comments" unconditionally,
/// so every single-comment issue read "1 comments" — in the summary line
/// and, worse, in the `aria-label`, which is the whole accessible name a
/// screen reader announces for the link. The counts row directly above
/// already pluralises through its own `word`, so this was an
/// inconsistency rather than a house style.
export function commentWord(n: number): string {
  return n === 1 ? "comment" : "comments";
}

export function bySeq(comments: IssueComment[]): IssueComment[] {
  return [...comments].sort((a, b) => a.seq - b.seq);
}

export type IssuesRoute =
  | { kind: "index" }
  | { kind: "new" }
  | { kind: "labels" }
  | { kind: "detail"; number: number }
  | { kind: "not-found" };

/// Which of the three pages an address under `/issues` means.
///
/// The digit test is not decoration. `Number("0x0c")` is 12,
/// `Number(" 12 ")` is 12 and `Number("1e2")` is 100, so a parse that
/// asked only "is this a number?" would give three more addresses for
/// every issue — addresses that render the page and that no canonical
/// link ever points at, which is a duplicate-content bug and a cache
/// key nobody expected.
export function issuesRoute(rest: string[]): IssuesRoute {
  if (rest.length === 0) return { kind: "index" };
  if (rest.length > 1) return { kind: "not-found" };
  if (rest[0] === "new") return { kind: "new" };
  // `/issues/labels`, the triage vocabulary. Under `issues` rather than
  // a tab of its own because labels are not a kind of thing a repository
  // has beside issues and changes — they are what issues are sorted by,
  // and the strip names kinds of thing.
  if (rest[0] === "labels") return { kind: "labels" };
  if (!/^[1-9][0-9]*$/.test(rest[0])) return { kind: "not-found" };
  return { kind: "detail", number: Number(rest[0]) };
}

/// Who wrote this, in the words we are entitled to use.
///
/// A native issue has a resolved handle. An imported one may have only
/// the name it had on GitHub, which is shown as-is rather than being
/// matched to a local account that happens to share it.
export function authorName(who: {
  author: string | null;
  author_label: string | null;
}): string {
  return who.author ?? who.author_label ?? "somebody";
}

// ---------------------------------------------------------------------
// The address bar's query string, without a prop drilled through the
// forge shell.
// ---------------------------------------------------------------------

/// One `?`-parameter, kept in step with the address bar.
///
/// `ForgeView` is handed `currentPath`, which is the pathname alone, and
/// the router's `Route` — the thing that carries the parsed query — is
/// not passed down to a repository tab. Rather than drill a new prop
/// through a file three other tracks are editing today, this listens to
/// exactly the two events `useRoute` listens to: `popstate` for the
/// browser's own back and forward, and `stratum:navigate` for ours,
/// which exists because `pushState` deliberately fires nothing.
function useQueryParam(name: string): string {
  const read = useCallback(
    () => new URLSearchParams(window.location.search).get(name) ?? "",
    [name],
  );
  const [value, setValue] = useState(read);
  useEffect(() => {
    const sync = () => setValue(read());
    sync();
    window.addEventListener("popstate", sync);
    window.addEventListener("stratum:navigate", sync);
    return () => {
      window.removeEventListener("popstate", sync);
      window.removeEventListener("stratum:navigate", sync);
    };
  }, [read]);
  return value;
}

// ---------------------------------------------------------------------
// The screens.
// ---------------------------------------------------------------------

export interface IssuesProps {
  owner: string;
  repo: string;
  /// The path segments after `/issues`.
  rest: string[];
  /// The caller's API token, when they signed in with one.
  token: string | null;
  /// The signed-in person, when they signed in with a password.
  me: Me | null;
  /// Whether this viewer may push here, from the server's own answer.
  /// Labelling and the label vocabulary are both write-gated; this is
  /// what keeps the UI from offering a control that will only refuse.
  canWrite: boolean;
  navigate: (to: string, replace?: boolean) => void;
}

export function IssuesView(props: IssuesProps) {
  const route = issuesRoute(props.rest);
  if (route.kind === "new") return <NewIssueView {...props} />;
  if (route.kind === "labels") return <LabelsView {...props} />;
  if (route.kind === "detail")
    return <IssueDetailView {...props} number={route.number} />;
  if (route.kind === "not-found")
    return <NotFound onNavigate={props.navigate} what="that issue" />;
  return <IssueIndexView {...props} />;
}

/// The control that files an issue. Filing one needs `RepoRead`, not
/// `RepoWrite`, so anybody who can read the repository may.
function NewIssueButton(props: IssuesProps) {
  return (
    <Button asChild>
      <a
        href={href([props.owner, props.repo, "issues", "new"])}
        onClick={(e) => {
          e.preventDefault();
          props.navigate(href([props.owner, props.repo, "issues", "new"]));
        }}
      >
        New issue
      </a>
    </Button>
  );
}

function IssueIndexView(props: IssuesProps) {
  const { owner, repo } = props;
  const session = useMemo(
    () => viewerSession(owner, props.token),
    [owner, props.token],
  );
  // The URL is the state. `?q=` holds the whole query bar, so every
  // list this page can show has an address somebody can send.
  const urlQuery = useQueryParam("q") || DEFAULT_QUERY;
  const filter = useMemo(() => parseQuery(urlQuery), [urlQuery]);
  // The text box is a draft of the URL, not the URL: typing must not
  // fire a request per keystroke. Submitting is what navigates.
  const [draft, setDraft] = useState(urlQuery);
  useEffect(() => setDraft(urlQuery), [urlQuery]);

  const [rows, setRows] = useState<Issue[] | null>(null);
  const [counts, setCounts] = useState<IssueCounts>({ open: 0, closed: 0 });
  const [labels, setLabels] = useState<IssueLabel[]>([]);
  const [error, setError] = useState<string | null>(null);

  // Depend on the *fields*, not on the filter object: `parseQuery`
  // builds a fresh object per render, and an effect keyed on it would
  // fetch, set state, re-render and fetch again — a loop that freezes
  // the tab rather than failing. `sort` is in the list because ordering
  // is the server's: changing it is a new request, not a re-render.
  const { state, label, author, text, sort } = filter;
  useEffect(() => {
    let alive = true;
    setRows(null);
    setError(null);
    api
      .issues(session, repo, toApiQuery({ state, label, author, text, sort }))
      .then((out) => {
        if (!alive) return;
        setRows(out.issues);
        // Both counts come from the server, over the unfiltered set.
        // Deriving either from `out.issues` would report "0 Closed" on
        // an open-filtered list, which is the number a maintainer uses
        // to decide whether anything is being closed at all.
        setCounts(out.counts);
      })
      .catch((e: Error) => alive && setError(e.message));
    return () => {
      alive = false;
    };
  }, [session, repo, state, label, author, text, sort]);

  useEffect(() => {
    let alive = true;
    api
      .issueLabels(session, repo)
      .then((l) => alive && setLabels(l))
      // A repository with no labels, or a stranger who may not read
      // them, gets a dropdown with "Any label" in it — not an error
      // box over a list that loaded perfectly well.
      .catch(() => alive && setLabels([]));
    return () => {
      alive = false;
    };
  }, [session, repo]);

  const navigate = props.navigate;
  const go = useCallback(
    (next: IssueFilter) =>
      navigate(href([owner, repo, "issues"], { q: formatQuery(next) })),
    [navigate, owner, repo],
  );

  // Rendered in the order the server sent them. Re-sorting here would
  // reorder one page of a list the server ordered over all of them.
  const sorted = rows ?? [];

  // The authors offered are the ones on this page. There is no endpoint
  // that lists a repository's issue authors, and inventing one for a
  // dropdown would be a table nobody writes to. Somebody who needs an
  // author not on the page types `author:name` into the bar, which is
  // the point of the bar.
  const authors = useMemo(
    () =>
      [...new Set((rows ?? []).map(authorName))]
        .filter((a) => a !== "somebody")
        .sort(),
    [rows],
  );

  return (
    <div className="py-6">
      <form
        className="mb-4 flex flex-wrap items-center gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          props.navigate(href([owner, repo, "issues"], { q: draft }));
        }}
      >
        <div className="flex min-w-0 flex-1 items-center gap-1 rounded-lg border border-borderline bg-surface-2 px-2">
          <Button
            type="submit"
            variant="ghost"
            size="icon"
            aria-label="Search issues"
            className="size-7 shrink-0"
          >
            <Search aria-hidden className="size-4" />
          </Button>
          <Input
            aria-label="Filter issues"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            placeholder={DEFAULT_QUERY}
            className="h-9 min-w-0 flex-1 border-0 bg-transparent font-mono text-sm focus-visible:ring-0"
          />
          {draft !== "" && (
            <Button
              type="button"
              variant="ghost"
              size="icon"
              aria-label="Clear the filter"
              className="size-7 shrink-0"
              // Back to every issue, not to the default. Clearing the
              // box has to mean "stop filtering"; navigating to the
              // default `is:issue is:open` would leave the closed ones
              // hidden and read as the × having done nothing. `is:issue`
              // alone is the noun with no filter on it, and it is a
              // non-empty value so it survives `href`'s omit-if-falsy.
              onClick={() =>
                props.navigate(href([owner, repo, "issues"], { q: "is:issue" }))
              }
            >
              <X aria-hidden className="size-4" />
            </Button>
          )}
        </div>
        <NewIssueButton {...props} />
      </form>

      {/* The vocabulary behind the filter beside it. Only for somebody
          who may change it: a reader already sees every label as a pill
          and in the filter menu, so a link to a page whose controls all
          refuse them is a dead end. */}
      {props.canWrite && (
        <p className="mt-2 text-xs text-ink-3">
          <a
            className={STRUCTURAL_LINK}
            href={href([owner, repo, "issues", "labels"])}
            onClick={(e) => {
              e.preventDefault();
              props.navigate(href([owner, repo, "issues", "labels"]));
            }}
          >
            Manage labels
          </a>
        </p>
      )}

      {/* The filter row is the second `audit()` hazard on this page:
          two counts and three dropdowns do not fit at 1024px. The
          scroller is this INNER box — `audit()` measures
          `documentElement.scrollWidth`, so a scroller on the page
          container fails the build while the identical one here is
          invisible to it. Below `md` the row wraps instead, because a
          phone should stack rather than scroll sideways. */}
      <div className="rounded-t-lg border border-borderline bg-surface-2">
        <div className="overflow-x-auto">
          <div className="flex min-w-0 flex-wrap items-center gap-3 p-3 text-sm md:flex-nowrap md:whitespace-nowrap">
            <StateCount
              state="open"
              count={counts.open}
              active={filter.state === "open"}
              onClick={() => go({ ...filter, state: "open" })}
            />
            <StateCount
              state="closed"
              count={counts.closed}
              active={filter.state === "closed"}
              onClick={() => go({ ...filter, state: "closed" })}
            />
            <div className="flex items-center gap-2 md:ml-auto">
              <FilterSelect
                label="Author"
                value={filter.author}
                anyLabel="Anyone"
                options={authors}
                onChange={(author) => go({ ...filter, author })}
              />
              <FilterSelect
                label="Labels"
                value={filter.label}
                anyLabel="Any label"
                options={labels.map((l) => l.name)}
                onChange={(label) => go({ ...filter, label })}
              />
              <Select
                value={filter.sort}
                onValueChange={(sort) =>
                  go({ ...filter, sort: sort as SortKey })
                }
              >
                <SelectTrigger aria-label="Sort" className="h-8 px-2 py-1">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {SORTS.map((s) => (
                    <SelectItem key={s.key} value={s.key}>
                      {s.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>
        </div>
      </div>

      <div className="rounded-b-lg border border-t-0 border-borderline">
        {error && (
          <div className="p-4">
            <Err message={error} />
          </div>
        )}
        {!error && rows === null && <Loading />}
        {!error && rows !== null && sorted.length === 0 && (
          <p className="p-10 text-center text-sm text-ink-3">
            No issues match this filter.
          </p>
        )}
        {!error && sorted.length > 0 && (
          <ul>
            {sorted.map((issue) => (
              <IssueRow
                key={issue.id}
                issue={issue}
                owner={owner}
                repo={repo}
                navigate={props.navigate}
              />
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

/// `Open 12` / `Closed 40`, as a pair and as controls.
function StateCount(props: {
  state: "open" | "closed";
  count: number;
  active: boolean;
  onClick: () => void;
}) {
  const word = props.state === "open" ? "Open" : "Closed";
  return (
    <button
      type="button"
      onClick={props.onClick}
      aria-pressed={props.active}
      // The glyph inside carries its own `aria-label` — it has to, so
      // that state is never colour alone — and without this the button
      // would be named "Open 12 Open". An explicit label on the control
      // is the accessible name, and it is the one a test addresses.
      aria-label={`${props.count} ${word}`}
      className={cn(
        "inline-flex items-center gap-1.5 rounded-sm",
        FOCUS_RING,
        props.active ? "font-semibold text-ink" : "text-ink-2 hover:text-ink",
      )}
    >
      <StateIcon
        state={props.state === "open" ? "issue-open" : "issue-completed"}
        className={props.active ? undefined : "text-ink-3"}
      />
      {/* The number is a number: mono, per DESIGN.md. The space before
          the tag is not cosmetic — `audit()` fails the build on a word
          jammed against an inline element. */}
      <span className="font-mono">{props.count}</span> <span>{word}</span>
    </button>
  );
}

/// A clearable filter dropdown.
///
/// Radix refuses an item whose `value` is the empty string, so "any" is
/// an explicit item with a sentinel value rather than a blank one — the
/// same rule the members and audit panels follow.
const ANY = " any";

function FilterSelect(props: {
  label: string;
  value: string | null;
  anyLabel: string;
  options: string[];
  onChange: (value: string | null) => void;
}) {
  // A value the list does not offer still has to show: `author:ada`
  // typed into the bar must light the dropdown up, not silently reset
  // it to "Anyone" and disagree with the URL.
  const options = props.value
    ? [...new Set([props.value, ...props.options])]
    : props.options;
  return (
    <Select
      value={props.value ?? ANY}
      onValueChange={(v) => props.onChange(v === ANY ? null : v)}
    >
      <SelectTrigger aria-label={props.label} className="h-8 px-2 py-1">
        <SelectValue placeholder={props.label} />
      </SelectTrigger>
      <SelectContent>
        <SelectItem value={ANY}>{props.anyLabel}</SelectItem>
        {options.map((o) => (
          <SelectItem key={o} value={o}>
            {o}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

function IssueRow(props: {
  issue: Issue;
  owner: string;
  repo: string;
  navigate: (to: string) => void;
}) {
  const { issue, owner, repo } = props;
  const to = href([owner, repo, "issues", String(issue.number)]);
  return (
    <li className="flex items-start gap-3 border-b border-borderline bg-surface-1 px-4 py-3 last:border-b-0">
      <span className="mt-0.5 shrink-0">
        <StateIcon
          state={issue.state === "open" ? "issue-open" : "issue-completed"}
        />
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <a
            href={to}
            onClick={(e) => {
              e.preventDefault();
              props.navigate(to);
            }}
            className={cn("font-medium", STRUCTURAL_LINK)}
          >
            {issue.title}
          </a>{" "}
          {issue.labels.map((l) => (
            <LabelPill key={l.id} name={l.name} color={l.color} />
          ))}
        </div>
        <div className="mt-1 text-xs text-ink-3">
          <span className="font-mono">#{issue.number}</span> ·{" "}
          <span>
            opened <RelativeTime at={issue.created_at} /> by {authorName(issue)}
          </span>
        </div>
      </div>
      {issue.comment_count > 0 && (
        <a
          href={to}
          onClick={(e) => {
            e.preventDefault();
            props.navigate(to);
          }}
          aria-label={`${issue.comment_count} ${commentWord(
            issue.comment_count,
          )} on #${issue.number}`}
          // `cn` rather than a template string: `STRUCTURAL_LINK`
          // carries `text-ink`, and two Tailwind classes for the same
          // property are resolved by stylesheet order, not by the order
          // they appear in the attribute. Concatenating would make the
          // colour depend on which class Tailwind emitted first.
          className={cn(
            "mt-0.5 flex shrink-0 items-center gap-1 text-xs",
            STRUCTURAL_LINK,
            "text-ink-3",
          )}
        >
          <MessageSquare aria-hidden className="size-3.5" />
          <span className="font-mono">{issue.comment_count}</span>
        </a>
      )}
    </li>
  );
}

/// The colours a label may take, in the order the manager offers them.
///
/// Token names, never hex — the server accepts only these seven and says
/// so when it refuses. Kept here rather than fetched because it is a
/// design-system fact, not repository state, and a picker that had to
/// wait for a round trip to draw its swatches would flicker on every
/// open.
export const LABEL_COLORS = [
  "series-1",
  "series-2",
  "series-3",
  "status-good",
  "status-warning",
  "status-serious",
  "neutral",
] as const;

/// The repository's label vocabulary: what exists, and — for somebody
/// with write access — adding and removing.
///
/// Labels were readable and filterable long before they were creatable.
/// The server has had `POST /labels`, `DELETE /labels/:name` and
/// `PUT /issues/:n/labels` all along; the client reached only the GET
/// that fills the filter menu. So on a repository that was not imported
/// from GitHub the vocabulary started empty and could never be filled:
/// the "Any label" filter had nothing in it, no issue could carry a
/// pill, and `label:"good first issue"` — which this file's own query
/// parser calls the label every "help wanted" list depends on — was
/// unreachable from the browser.
function LabelsView(props: IssuesProps) {
  const { owner, repo, canWrite } = props;
  const session = useMemo(
    () => viewerSession(owner, props.token),
    [owner, props.token],
  );
  const [labels, setLabels] = useState<IssueLabel[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [name, setName] = useState("");
  const [color, setColor] = useState<string>(LABEL_COLORS[0]);
  const [description, setDescription] = useState("");

  const refresh = useCallback(() => {
    let alive = true;
    api
      .issueLabels(session, repo)
      .then((l) => alive && setLabels(l))
      .catch((e: Error) => alive && setError(e.message));
    return () => {
      alive = false;
    };
  }, [session, repo]);
  useEffect(refresh, [refresh]);

  async function add(e: React.FormEvent) {
    e.preventDefault();
    if (!name.trim() || busy) return;
    setBusy(true);
    setError(null);
    try {
      await api.createLabel(session, repo, {
        name: name.trim(),
        color,
        description: description.trim(),
      });
      setName("");
      setDescription("");
      refresh();
    } catch (err) {
      // The server's own words. It refuses a bad colour by naming the
      // seven it takes and a duplicate by saying so, and either is more
      // use than "could not create label".
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  async function remove(label: string) {
    setBusy(true);
    setError(null);
    try {
      await api.deleteLabel(session, repo, label);
      refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="py-6">
      <div className="mb-4 flex flex-wrap items-baseline justify-between gap-3 border-b border-borderline pb-3">
        <h1 className="text-xl font-semibold text-ink">Labels</h1>
        <a
          className={STRUCTURAL_LINK}
          href={href([owner, repo, "issues"])}
          onClick={(e) => {
            e.preventDefault();
            props.navigate(href([owner, repo, "issues"]));
          }}
        >
          Back to issues
        </a>
      </div>

      {canWrite && (
        <form
          className="mb-6 flex flex-wrap items-end gap-2 rounded-lg border border-borderline bg-surface-1 p-4"
          onSubmit={add}
        >
          <div className="flex flex-col gap-1">
            <label
              className="text-xs font-medium text-ink-2"
              htmlFor="label-name"
            >
              Name
            </label>
            <input
              id="label-name"
              className="w-56 rounded-md border border-borderline bg-surface-0 px-3 py-1.5 text-sm"
              placeholder="good first issue"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </div>
          <div className="flex flex-col gap-1">
            <label
              className="text-xs font-medium text-ink-2"
              htmlFor="label-color"
            >
              Colour
            </label>
            <select
              id="label-color"
              className="rounded-md border border-borderline bg-surface-0 px-3 py-1.5 text-sm"
              value={color}
              onChange={(e) => setColor(e.target.value)}
            >
              {LABEL_COLORS.map((c) => (
                <option key={c} value={c}>
                  {c}
                </option>
              ))}
            </select>
          </div>
          <div className="flex min-w-0 flex-1 flex-col gap-1">
            <label
              className="text-xs font-medium text-ink-2"
              htmlFor="label-desc"
            >
              Description
            </label>
            <input
              id="label-desc"
              className="min-w-0 rounded-md border border-borderline bg-surface-0 px-3 py-1.5 text-sm"
              placeholder="optional"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
          </div>
          <Button
            type="submit"
            variant="outline"
            disabled={busy || !name.trim()}
          >
            {busy ? "Saving…" : "Create label"}
          </Button>
        </form>
      )}

      <Err message={error} />

      {labels === null ? (
        <p className="text-sm text-ink-3">Loading…</p>
      ) : labels.length === 0 ? (
        <p className="text-sm text-ink-3">
          {canWrite
            ? "No labels yet. The first one is usually the one that matters most: a way to say which issues a newcomer could pick up."
            : "This repository has no labels yet."}
        </p>
      ) : (
        <ul className="divide-y divide-borderline rounded-lg border border-borderline">
          {labels.map((l) => (
            <li key={l.id} className="flex flex-wrap items-center gap-3 p-3">
              <LabelPill name={l.name} color={l.color} />
              <span className="min-w-0 flex-1 truncate text-sm text-ink-2">
                {l.description}
              </span>
              {canWrite && (
                <Button
                  variant="outline"
                  size="xs"
                  disabled={busy}
                  // Deleting a label takes it off every issue carrying
                  // it, and re-creating one with the same name does not
                  // bring those back.
                  aria-label={`Delete label ${l.name}`}
                  onClick={() => void remove(l.name)}
                >
                  Delete
                </Button>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/// An issue's labels: the pills, and — for somebody who may write — the
/// control that sets them.
///
/// Displaying them has always worked; applying one had no interface at
/// all, so triage was `curl` or nothing. The set is replaced wholesale
/// rather than added to, because that is the shape the endpoint takes
/// and inventing an add/remove pair on top of a PUT would be two round
/// trips that can disagree with each other.
///
/// A reader sees the pills and no control. Not only politeness: the
/// server refuses labelling from a reader with a sentence saying triage
/// is the maintainer's, and a control whose only job is to deliver that
/// sentence is the pattern this codebase already took off the Changes
/// tab.
function IssueLabels(props: {
  owner: string;
  repo: string;
  session: Session;
  issue: Issue;
  canWrite: boolean;
  navigate: (to: string, replace?: boolean) => void;
  onChanged: (issue: Issue) => void;
}) {
  const { issue, canWrite } = props;
  const [all, setAll] = useState<IssueLabel[] | null>(null);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Only once the control is opened. A reader never triggers it, and a
  // maintainer who is reading rather than triaging does not pay for it.
  useEffect(() => {
    if (!open || all !== null) return;
    let alive = true;
    api
      .issueLabels(props.session, props.repo)
      .then((l) => alive && setAll(l))
      .catch(() => alive && setAll([]));
    return () => {
      alive = false;
    };
  }, [open, all, props.session, props.repo]);

  async function toggle(name: string) {
    const has = issue.labels.some((l) => l.name === name);
    // The whole set, every time: the endpoint replaces rather than
    // appends, so sending just the one being added would silently strip
    // the rest.
    const next = has
      ? issue.labels.filter((l) => l.name !== name).map((l) => l.name)
      : [...issue.labels.map((l) => l.name), name];
    setBusy(true);
    setError(null);
    try {
      props.onChanged(
        await api.setIssueLabels(props.session, props.repo, issue.number, next),
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  if (!canWrite && issue.labels.length === 0) return null;

  const manage = href([props.owner, props.repo, "issues", "labels"]);
  const goManage = (e: React.MouseEvent) => {
    e.preventDefault();
    props.navigate(manage);
  };

  return (
    <div className="mt-3 flex flex-wrap items-center gap-2">
      {issue.labels.map((l) => (
        <LabelPill key={l.id} name={l.name} color={l.color} />
      ))}
      {canWrite && (
        <>
          <Button
            variant="outline"
            size="xs"
            disabled={busy}
            aria-expanded={open}
            onClick={() => setOpen((v) => !v)}
          >
            {issue.labels.length > 0 ? "Edit labels" : "Add a label"}
          </Button>
          {open && (
            <div className="mt-2 w-full rounded-lg border border-borderline bg-surface-1 p-3">
              {all === null ? (
                <p className="text-sm text-ink-3">Loading…</p>
              ) : all.length === 0 ? (
                <p className="text-sm text-ink-3">
                  This repository has no labels yet.{" "}
                  <a
                    className={STRUCTURAL_LINK}
                    href={manage}
                    onClick={goManage}
                  >
                    Create one
                  </a>
                  .
                </p>
              ) : (
                <ul className="flex flex-wrap gap-2">
                  {all.map((l) => {
                    const on = issue.labels.some((x) => x.name === l.name);
                    return (
                      <li key={l.id}>
                        <button
                          type="button"
                          disabled={busy}
                          aria-pressed={on}
                          aria-label={`${on ? "Remove" : "Apply"} label ${l.name}`}
                          className={cn(
                            "rounded-full",
                            FOCUS_RING,
                            on ? "opacity-100" : "opacity-50 hover:opacity-90",
                          )}
                          onClick={() => void toggle(l.name)}
                        >
                          <LabelPill name={l.name} color={l.color} />
                        </button>
                      </li>
                    );
                  })}
                </ul>
              )}
              <p className="mt-3 text-xs text-ink-3">
                <a className={STRUCTURAL_LINK} href={manage} onClick={goManage}>
                  Manage labels
                </a>
              </p>
            </div>
          )}
        </>
      )}
      <Err message={error} />
    </div>
  );
}

function IssueDetailView(props: IssuesProps & { number: number }) {
  const { owner, repo, number } = props;
  const session = useMemo(
    () => viewerSession(owner, props.token),
    [owner, props.token],
  );
  const [issue, setIssue] = useState<Issue | null>(null);
  const [comments, setComments] = useState<IssueComment[] | null>(null);
  const [missing, setMissing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  // The edit form, when it is open. `null` is "not editing" — distinct
  // from an empty draft, which is a title somebody has just cleared.
  const [edit, setEdit] = useState<{ title: string; body: string } | null>(
    null,
  );

  useEffect(() => {
    let alive = true;
    setIssue(null);
    setEdit(null);
    setComments(null);
    setMissing(false);
    setError(null);
    api
      .issue(session, repo, number)
      .then((i) => alive && setIssue(i))
      // Refused and absent are one answer, exactly as they are for a
      // repository: distinguishing them would make the address bar an
      // oracle for which issues a private repository has.
      .catch(() => alive && setMissing(true));
    api
      .issueComments(session, repo, number)
      .then((c) => alive && setComments(bySeq(c)))
      .catch(() => alive && setComments([]));
    return () => {
      alive = false;
    };
  }, [session, repo, number]);

  if (missing)
    return <NotFound onNavigate={props.navigate} what={`issue #${number}`} />;
  if (!issue) return <Loading />;

  const open = issue.state === "open";
  // The server allows editing the text, and closing, to a writer **or**
  // the issue's own author — somebody who filed a bug and worked out it
  // was their own mistake should not have to wait for a maintainer to
  // agree. Compared by id rather than by handle, because a handle is
  // mutable and renaming yourself must not hand somebody else your
  // issues or take away your own.
  const isAuthor =
    props.me !== null &&
    issue.author_id !== null &&
    props.me.id === issue.author_id;
  const canEdit = props.canWrite || isAuthor;

  return (
    <div className="py-6">
      <header className="border-b border-borderline pb-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <h1 className="text-2xl font-semibold tracking-tight text-ink">
            {issue.title}{" "}
            <span className="font-mono font-normal text-ink-3">
              #{issue.number}
            </span>
          </h1>
          {canEdit && edit === null && (
            <Button
              variant="outline"
              size="xs"
              aria-label={`Edit issue #${issue.number}`}
              onClick={() => setEdit({ title: issue.title, body: issue.body })}
            >
              Edit
            </Button>
          )}
        </div>
        <div className="mt-3 flex flex-wrap items-center gap-3">
          <span
            className={`inline-flex items-center rounded-full px-3 py-1.5 text-sm font-semibold ${
              open ? "bg-good/12 text-good" : "bg-surface-2 text-ink-2"
            }`}
          >
            <StateIcon
              state={open ? "issue-open" : "issue-completed"}
              showLabel
            />
          </span>{" "}
          <span className="text-sm text-ink-3">
            {authorName(issue)} opened this{" "}
            <RelativeTime at={issue.created_at} /> ·{" "}
            <span className="font-mono">{issue.comment_count}</span>{" "}
            {commentWord(issue.comment_count)}
          </span>
        </div>
        <IssueLabels
          owner={owner}
          repo={repo}
          session={session}
          issue={issue}
          canWrite={props.canWrite}
          navigate={props.navigate}
          onChanged={setIssue}
        />
      </header>

      <Err message={error} />

      {edit !== null ? (
        <form
          className="mt-4 space-y-3 rounded-xl border border-borderline bg-surface-1 p-4"
          onSubmit={(e) => {
            e.preventDefault();
            if (!edit.title.trim() || busy) return;
            setBusy(true);
            setError(null);
            api
              .editIssue(session, repo, number, {
                title: edit.title.trim(),
                body: edit.body,
              })
              .then((i) => {
                setIssue(i);
                setEdit(null);
              })
              .catch((err: Error) => setError(err.message))
              .finally(() => setBusy(false));
          }}
        >
          <div className="flex flex-col gap-1">
            <label
              className="text-sm font-medium text-ink"
              htmlFor="issue-edit-title"
            >
              Title
            </label>
            <Input
              id="issue-edit-title"
              value={edit.title}
              maxLength={512}
              onChange={(e) => setEdit({ ...edit, title: e.target.value })}
            />
          </div>
          <div className="flex flex-col gap-1">
            <label
              className="text-sm font-medium text-ink"
              htmlFor="issue-edit-body"
            >
              Description
            </label>
            <textarea
              id="issue-edit-body"
              className="min-h-40 rounded-lg border border-borderline bg-surface-0 px-3 py-2 text-sm text-ink"
              value={edit.body}
              maxLength={65536}
              onChange={(e) => setEdit({ ...edit, body: e.target.value })}
            />
          </div>
          <div className="flex items-center gap-2">
            <Button type="submit" disabled={busy || !edit.title.trim()}>
              {busy ? "Saving…" : "Save"}
            </Button>
            <Button
              type="button"
              variant="outline"
              disabled={busy}
              onClick={() => setEdit(null)}
            >
              Cancel
            </Button>
          </div>
        </form>
      ) : (
        <article className="mt-4 rounded-xl border border-borderline bg-surface-1 p-4">
          <div className="mb-2 text-sm font-medium text-ink">
            {authorName(issue)}
          </div>
          {issue.body.trim() === "" ? (
            <p className="text-sm text-ink-3">No description given.</p>
          ) : (
            <Markdown source={issue.body} headingLevel={3} />
          )}
        </article>
      )}

      {comments === null ? (
        <Loading />
      ) : (
        <ul className="mt-4 space-y-4">
          {comments.map((c) => (
            <li
              key={c.id}
              className="rounded-xl border border-borderline bg-surface-1 p-4"
            >
              <div className="mb-2 flex flex-wrap items-baseline gap-2 text-sm">
                <span className="font-medium text-ink">{authorName(c)}</span>{" "}
                <span className="text-xs text-ink-3">
                  commented <RelativeTime at={c.created_at} />
                </span>
              </div>
              <Markdown source={c.body} headingLevel={3} />
            </li>
          ))}
        </ul>
      )}

      <form
        className="mt-6 flex flex-col gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          const body = draft.trim();
          if (!body || busy) return;
          setBusy(true);
          setError(null);
          api
            .commentOnIssue(session, repo, number, body)
            .then((c) => {
              setComments((prev) => bySeq([...(prev ?? []), c]));
              setIssue((prev) =>
                prev
                  ? { ...prev, comment_count: prev.comment_count + 1 }
                  : prev,
              );
              setDraft("");
            })
            .catch((err: Error) => setError(err.message))
            .finally(() => setBusy(false));
        }}
      >
        <textarea
          aria-label="Comment on this issue"
          placeholder="Leave a comment"
          className="min-h-24 rounded-lg border border-borderline bg-surface-1 px-3 py-2 text-sm text-ink"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          maxLength={65536}
        />
        <div className="flex items-center gap-2">
          <Button type="submit" disabled={busy || draft.trim() === ""}>
            Comment
          </Button>
          {/* Only for somebody the server will let do it. Closing is
              a writer's or the author's, so a signed-in stranger was
              being offered a button that could only come back 403 —
              the same shape the Changes tab had. */}
          {canEdit && (
            <Button
              type="button"
              variant="outline"
              disabled={busy}
              onClick={() => {
                setBusy(true);
                setError(null);
                api
                  .setIssueState(
                    session,
                    repo,
                    number,
                    open ? "closed" : "open",
                  )
                  .then(setIssue)
                  .catch((err: Error) => setError(err.message))
                  .finally(() => setBusy(false));
              }}
            >
              {open ? "Close issue" : "Reopen issue"}
            </Button>
          )}
        </div>
      </form>
    </div>
  );
}

function NewIssueView(props: IssuesProps) {
  const { owner, repo } = props;
  const session = useMemo(
    () => viewerSession(owner, props.token),
    [owner, props.token],
  );
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  return (
    <div className="max-w-3xl py-6">
      <h1 className="mb-4 border-b border-borderline pb-3 text-xl font-semibold text-ink">
        New issue
      </h1>
      <Err message={error} />
      <form
        className="flex flex-col gap-3"
        onSubmit={(e) => {
          e.preventDefault();
          if (busy || title.trim() === "") return;
          setBusy(true);
          setError(null);
          api
            .openIssue(session, repo, title.trim(), body)
            .then((issue) =>
              // Land on the issue that was just filed. A form that
              // returns you to the list you came from reads as having
              // done nothing, and the first thing anybody wants after
              // filing is the address to send somebody.
              props.navigate(
                href([owner, repo, "issues", String(issue.number)]),
              ),
            )
            .catch((err: Error) => {
              setError(err.message);
              setBusy(false);
            });
        }}
      >
        <label className="text-sm font-medium text-ink" htmlFor="issue-title">
          Title
        </label>
        <Input
          id="issue-title"
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          placeholder="What happened?"
          maxLength={400}
        />
        <label className="text-sm font-medium text-ink" htmlFor="issue-body">
          Description
        </label>
        <textarea
          id="issue-body"
          className="min-h-40 rounded-lg border border-borderline bg-surface-1 px-3 py-2 text-sm text-ink"
          value={body}
          onChange={(e) => setBody(e.target.value)}
          placeholder="Markdown is supported."
          maxLength={65536}
        />
        <div className="flex items-center gap-2">
          <Button type="submit" disabled={busy || title.trim() === ""}>
            Submit new issue
          </Button>
          <Button
            type="button"
            variant="ghost"
            onClick={() => props.navigate(href([owner, repo, "issues"]))}
          >
            Cancel
          </Button>
        </div>
      </form>
    </div>
  );
}
