import { useEffect, useState } from "react";
import { api, viewerSession, type RepoHit } from "@/api";
import { ErrorBox, Loading } from "@/components/feedback";
import { RepoCard } from "@/components/repo-card";
import { href } from "@/router";
import { FORGE_CONTAINER } from "@/lib/links";

/// The repositories matching the header's search box, or carrying one
/// topic exactly.
///
/// It reads the search endpoint, which applies `registry::Viewer`: the
/// caller is shown the repositories they may read, across every
/// namespace they belong to, and nothing else. The session carries the
/// caller's own credential for the reason `viewerSession` gives — a
/// token holder searching with an empty one would be shown nothing.
export function SearchView(props: {
  q: string;
  /// Narrow to one topic, exactly. Set by the About rail's pills.
  topic: string;
  token: string | null;
  navigate: (to: string) => void;
}) {
  const [hits, setHits] = useState<RepoHit[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    setHits(null);
    setError(null);
    api
      .searchRepos(viewerSession("", props.token), props.q, undefined, props.topic)
      .then((r) => alive && setHits(r.repos))
      .catch((e) => alive && setError(String(e)));
    return () => {
      alive = false;
    };
  }, [props.q, props.topic, props.token]);

  return (
    <div className={`${FORGE_CONTAINER} py-8`}>
      <h1 className="text-2xl font-semibold tracking-tight text-ink">
        {props.topic
          ? `Repositories tagged ${props.topic}`
          : props.q
            ? `Repositories matching “${props.q}”`
            : "Repositories you can see"}
      </h1>

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
            ? `No repository you can see carries the topic ${props.topic}.`
            : props.q
              ? "Nothing matched. Search covers a repository's name, its namespace, the line it says about itself, and its topics."
              : "No repositories yet."}
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
                onNavigate={props.navigate}
              />
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
