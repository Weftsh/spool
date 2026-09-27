/// Finding a repository you were not sent a link to.
///
/// Everything else in this dashboard starts from the namespace in the
/// header. This screen does not: it crosses namespaces, because the
/// question "where did we put that?" does not know which org the answer
/// is in, and someone in three orgs should not have to guess twice
/// before finding it.
///
/// The server decides what comes back. This file never filters, and
/// never asks for a namespace by id — a visibility rule implemented
/// twice is a visibility rule that will disagree with itself.

import { useCallback, useEffect, useRef, useState } from "react";
import { api, type RepoHit, type Session } from "@/api";
import { href, navigateTo } from "@/router";
import { Alert } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";

export function Search(props: {
  session: Session;
  query: string;
  navigate: (to: string, replace?: boolean) => void;
  /// Switch the dashboard to another namespace, for a hit that is not in
  /// the one currently open.
  onOpenOrg: (org: string) => void;
}) {
  const { session, query } = props;
  const [text, setText] = useState(query);
  const [hits, setHits] = useState<RepoHit[] | null>(null);
  const [next, setNext] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Which query the rows on screen belong to. Without it, a slow answer
  // to an abandoned query can land after a fast answer to the current
  // one and replace it — the results silently stop matching the box.
  const want = useRef(query);

  useEffect(() => {
    setText(query);
  }, [query]);

  useEffect(() => {
    let live = true;
    want.current = query;
    setBusy(true);
    setError(null);
    // Clear the previous answer *before* asking for the next one. Left
    // in place, the old rows sit there under the new query with nothing
    // saying anything is in flight — so searching for something else
    // reads as "the same repositories matched", which is a wrong answer
    // rather than a slow one. The manual pass found exactly that: it
    // searched for a term that matches nothing, saw the previous hits
    // still on screen, and reported that an empty result says nothing
    // at all.
    setHits(null);
    setNext(null);
    api
      .searchRepos(session, query)
      .then((out) => {
        if (!live || want.current !== query) return;
        setHits(out.repos);
        setNext(out.next);
      })
      .catch((e: Error) => {
        if (!live) return;
        setHits([]);
        setNext(null);
        setError(e.message);
      })
      .finally(() => {
        if (live) setBusy(false);
      });
    return () => {
      live = false;
    };
  }, [session, query]);

  const more = useCallback(() => {
    if (!next) return;
    setBusy(true);
    api
      .searchRepos(session, query, next)
      .then((out) => {
        setHits((prev) => [...(prev ?? []), ...out.repos]);
        setNext(out.next);
      })
      .catch((e: Error) => setError(e.message))
      .finally(() => setBusy(false));
  }, [session, query, next]);

  return (
    <main className="mx-auto max-w-4xl px-4 py-8">
      <h1 className="text-xl font-semibold">Search</h1>
      <p className="mt-1 text-sm text-ink-2">
        Matches a repository&rsquo;s name, its namespace, or its description
        &mdash; across every namespace you belong to, plus everything public.
      </p>
      <form
        className="mt-5 flex gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          props.navigate(href(["search"], { q: text.trim() }));
        }}
      >
        <label className="sr-only" htmlFor="search-q">
          Search repositories
        </label>
        <input
          id="search-q"
          type="search"
          autoComplete="off"
          maxLength={128}
          className="min-w-0 flex-1 rounded-md border border-borderline bg-surface-0 px-3 py-2 text-sm"
          placeholder="name, namespace, or a word from the description"
          value={text}
          onChange={(e) => setText(e.target.value)}
        />
        <Button size="lg" type="submit">
          Search
        </Button>
      </form>

      {error && (
        <Alert variant="destructive" className="mt-4">
          {error}
        </Alert>
      )}

      {hits === null ? (
        <p className="mt-6 text-sm text-ink-3">Loading…</p>
      ) : hits.length === 0 ? (
        <p className="mt-6 text-sm text-ink-3">
          {query
            ? `Nothing you can see matches “${query}”.`
            : "No repositories yet."}
        </p>
      ) : (
        <ul className="mt-6 space-y-2">
          {hits.map((hit) => (
            <Hit
              key={hit.id}
              hit={hit}
              here={hit.org === session.org}
              navigate={props.navigate}
              onOpenOrg={props.onOpenOrg}
            />
          ))}
        </ul>
      )}

      {next && (
        <Button
          variant="outline"
          size="lg"
          className="mt-6"
          disabled={busy}
          onClick={more}
        >
          {busy ? "Loading…" : "Show more"}
        </Button>
      )}
    </main>
  );
}

function Hit(props: {
  hit: RepoHit;
  here: boolean;
  navigate: (to: string) => void;
  onOpenOrg: (org: string) => void;
}) {
  const { hit } = props;
  return (
    <li className="rounded-md border border-borderline bg-surface-1 p-3">
      <button
        className="text-left"
        onClick={() => {
          // The repository page is addressed by owner, so opening one
          // in another namespace no longer *needs* the switch — it used
          // to, when every repo screen read the dashboard's current org
          // and opening one without switching 404'd in a way that read
          // as the search having lied. The switch stays because coming
          // back to the dashboard afterwards should land in the
          // namespace you just went to, not the one you left.
          if (!props.here) props.onOpenOrg(hit.org);
          navigateTo(href([hit.org, hit.name]));
        }}
      >
        {/* Two nodes, never "org/name" in one string: a screen reader
            announcing them as one word is how a name gets misread. */}
        <span className="text-ink-3">{hit.org}</span>
        <span className="text-ink-3"> / </span>
        <span className="font-medium">{hit.name}</span>
      </button>
      <span className="ml-2 align-middle text-xs text-ink-3">
        {hit.public ? "public" : "private"}
        {hit.kind === "mirror" ? " · mirror" : ""}
        {props.here ? "" : " · another namespace"}
      </span>
      {hit.description && (
        <p className="mt-1 truncate text-sm text-ink-2" title={hit.description}>
          {hit.description}
        </p>
      )}
    </li>
  );
}
