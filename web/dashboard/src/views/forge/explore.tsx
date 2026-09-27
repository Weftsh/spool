import { useEffect, useState } from "react";
import { anon, api, type RepoHit } from "@/api";
import { ErrorBox, Loading } from "@/components/feedback";
import { RepoCard } from "@/components/repo-card";
import { href } from "@/router";
import { FORGE_CONTAINER } from "@/lib/links";

/// Public repositories — the whole set, or the ones matching a query.
///
/// One component for `/explore` and `/search`, because they are the same
/// page with and without a `q`. Splitting them would mean two renderings
/// of a repository row that drift, and the difference a visitor cares
/// about is only whether they narrowed it.
///
/// It reads the search endpoint, which applies `registry::Viewer` and so
/// answers a stranger with public repositories and a member with theirs
/// as well. That is why this page works signed out, which is the point
/// of it existing at all.
///
/// There is deliberately no trending tab. Ranking a week of traffic
/// produces a random number with a confident face, and `/discover` on
/// the marketing site already says so in as many words.
export function ExploreView(props: {
  q: string;
  /// Narrow to one topic, exactly. Set by `/topics/{name}`.
  topic?: string;
  navigate: (to: string) => void;
}) {
  const [hits, setHits] = useState<RepoHit[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    setHits(null);
    setError(null);
    api
      // An empty query is "everything public", which is what an explore
      // page is; the endpoint treats it that way already.
      .searchRepos(anon(""), props.q, undefined, props.topic)
      .then((r) => alive && setHits(r.repos))
      .catch((e) => alive && setError(String(e)));
    return () => {
      alive = false;
    };
  }, [props.q, props.topic]);

  // Whether everything on screen is in fact public.
  //
  // This page reads a viewer-scoped endpoint, so a signed-in member
  // gets their own repositories here as well as the public ones — which
  // the heading and the line under it went on describing, unconditionally,
  // as "public" and "clonable without an account". The cards carried a
  // Private badge and the server never leaked anything, so nothing was
  // exposed; a maintainer looking at their private ledger under that
  // sentence had simply been told that it was. Being told you have a
  // breach you do not have costs as much as a small one.
  //
  // Decided from the answer rather than from the session: this component
  // asks anonymously and lets the cookie widen the result, so the list
  // is the only thing that knows.
  const everythingPublic = !hits || hits.every((r) => r.public);

  return (
    <div className={`${FORGE_CONTAINER} py-8`}>
      <h1 className="text-2xl font-semibold tracking-tight text-ink">
        {props.topic
          ? `Repositories tagged ${props.topic}`
          : props.q
            ? `Repositories matching “${props.q}”`
            : everythingPublic
              ? "Public repositories"
              : "Repositories you can see"}
      </h1>
      <p className="mt-1 text-sm text-ink-2">
        {everythingPublic
          ? "Everything here is public and clonable without an account."
          : "Public repositories are clonable without an account. Signed in, this list also includes repositories only you can see."}
      </p>

      {error && (
        <div className="mt-6">
          <ErrorBox message={error} />
        </div>
      )}
      {!error && !hits && (
        <div className="mt-6">
          <Loading />
        </div>
      )}
      {hits && hits.length === 0 && (
        <p className="mt-6 text-sm text-ink-3">
          {props.topic
            ? `No repository carries the topic ${props.topic}.`
            : props.q
              ? "Nothing matched. Search covers a repository's name, its namespace, the line it says about itself, and its topics."
              : "Nothing public yet."}
        </p>
      )}
      {hits && hits.length > 0 && (
        <ul className="mt-6 grid gap-4 sm:grid-cols-2">
          {hits.map((r) => (
            <li key={r.id}>
              <RepoCard
                owner={r.org}
                name={r.name}
                href={href([r.org, r.name])}
                description={r.description}
                visibility={r.public ? "public" : "private"}
                onNavigate={props.navigate}
              />
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
