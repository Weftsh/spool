import { useEffect, useMemo, useState } from "react";
import { api, viewerSession, type Profile, type RepoHit } from "@/api";
import { ErrorBox, Loading } from "@/components/feedback";
import { ProfileRail } from "@/components/profile-rail";
import { RepoCard } from "@/components/repo-card";
import { href } from "@/router";
import { FORGE_CONTAINER } from "@/lib/links";

/// A namespace's page, for a person or an organization alike: who it is,
/// and the repositories in it that the viewer may read.
///
/// One component for both because a namespace is a namespace — the
/// control plane made that decision long before this page existed, and
/// two components would drift the moment one of them grew a field. What
/// differs between a person and an org is what the server says about
/// them, not how the page is shaped.
///
/// It reads two sources, and they are deliberately not equal. The
/// repository listing is the page: if it fails the page has failed and
/// says so. The profile is what the page says *about* whoever this is,
/// and a failure there degrades to a handle, an avatar and a repo grid
/// instead of replacing a working namespace with a red box. A namespace
/// can also legitimately have no profile row at all, and that is not an
/// error worth showing anybody.
export function ProfileView(props: {
  owner: string;
  navigate: (to: string) => void;
  /// The viewer's API token, when they hold one.
  ///
  /// Not decoration: the repository grid is fed by `/v1/search/repos`,
  /// which the server filters by who is asking. A session with an empty
  /// token goes out unauthenticated for somebody signed in with a token,
  /// who is then told their repositories are not there. A browser
  /// session survives that by accident, because the cookie is attached
  /// by the browser rather than by us; a token does not.
  token?: string;
}) {
  const { owner } = props;
  const [repos, setRepos] = useState<RepoHit[] | null>(null);
  const [profile, setProfile] = useState<Profile | null>(null);
  const [error, setError] = useState<string | null>(null);

  // A fresh session object each render would re-run the effect below
  // on every render, so it is held still.
  const session = useMemo(
    () => viewerSession(owner, props.token ?? null),
    [owner, props.token],
  );

  useEffect(() => {
    let alive = true;
    setRepos(null);
    setProfile(null);
    setError(null);
    // Search, not `GET /orgs/{org}/repos`.
    //
    // The listing is a members' endpoint, and this page is also read by
    // somebody who holds a grant on one repository here and no role in
    // the namespace. Search already answers "what may this caller see"
    // with `registry::Viewer`, for every kind of caller, which is the
    // question this page is asking. The namespace is matched server-side
    // and the exact match filtered here, because `q` also matches
    // descriptions and a repository elsewhere that mentions this name is
    // not one of its repositories.
    api
      .searchRepos(session, owner)
      .then((r) => {
        if (!alive) return;
        setRepos(r.repos.filter((x) => x.org === owner));
      })
      .catch((e) => alive && setError(String(e)));
    api
      .getProfile(session, owner)
      .then((p) => alive && setProfile(p))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [owner, session]);

  return (
    <div className={`${FORGE_CONTAINER} flex flex-col gap-8 py-8 md:flex-row`}>
      <ProfileRail
        handle={owner}
        displayName={profile?.display_name}
        pronouns={profile?.pronouns}
        bio={profile?.bio}
        company={profile?.company}
        location={profile?.location}
        links={profile?.links}
        kind={profile?.kind}
      />
      <section className="flex min-w-0 flex-1 flex-col gap-8">
        <div>
          {error && <ErrorBox message={error} />}
          {!error && !repos && <Loading />}
          {repos && repos.length === 0 && (
            <p className="text-sm text-ink-3">
              No repositories here that you can see.
            </p>
          )}
          {repos && repos.length > 0 && (
            <ul className="grid gap-4 sm:grid-cols-2">
              {repos.map((r) => (
                <li key={r.id} className="flex">
                  <RepoCard
                    name={r.name}
                    href={href([owner, r.name])}
                    description={r.description}
                    onNavigate={props.navigate}
                    className="w-full"
                  />
                </li>
              ))}
            </ul>
          )}
        </div>
      </section>
    </div>
  );
}
