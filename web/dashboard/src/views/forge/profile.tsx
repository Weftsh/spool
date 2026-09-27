import { useEffect, useMemo, useState } from "react";
import {
  api,
  viewerSession,
  type ContributionGraph,
  type Pin,
  type Profile,
  type RepoHit,
} from "@/api";
import { ContributionGraphView } from "@/components/contribution-graph";
import { ErrorBox, Loading } from "@/components/feedback";
import { ProfileRail } from "@/components/profile-rail";
import { RepoCard } from "@/components/repo-card";
import { href } from "@/router";
import type { OwnerTab } from "@/routes";
import { FORGE_CONTAINER } from "@/lib/links";

/// A namespace's public face, for a person or an organization alike.
///
/// One component for both because a namespace is a namespace — the
/// control plane made that decision long before this page existed, and
/// two components would drift the moment one of them grew a field. What
/// differs between a person and an org is what the server says about
/// them, not how the page is shaped.
///
/// Repositories come from the ordinary repo listing rather than anything
/// new: the server's visibility rule already answers a stranger with the
/// public ones and a member with theirs, so this page needs no
/// permission logic of its own. That is the whole reason it could ship
/// before the profile endpoints existed.
///
/// It now reads three sources rather than one, and they are deliberately
/// not equal. The repository listing is the page: if it fails the page
/// has failed and says so. The profile and the pins are what the page
/// says *about* whoever this is, and a failure there degrades to the
/// page as it was — a handle, an avatar and a repo grid — instead of
/// replacing a working namespace with a red box. A namespace can also
/// legitimately have no profile row at all, and that is not an error
/// worth showing a stranger.
export function ProfileView(props: {
  owner: string;
  tab: OwnerTab;
  navigate: (to: string) => void;
  /// The viewer's API token, when they hold one.
  ///
  /// Not decoration, and not only about the star state: the repository
  /// grid is fed by `/v1/search/repos`, which the server filters by who
  /// is asking. Built as `anon(owner)` with an empty token, every read
  /// on this page goes out unauthenticated — so somebody signed in with
  /// a token, looking at their own profile, is told their private
  /// repositories are not there. A browser session survives that by
  /// accident, because the cookie is attached by the browser rather
  /// than by us; a token does not.
  token?: string;
}) {
  const { owner } = props;
  const [repos, setRepos] = useState<RepoHit[] | null>(null);
  const [profile, setProfile] = useState<Profile | null>(null);
  const [pins, setPins] = useState<Pin[]>([]);
  const [graph, setGraph] = useState<ContributionGraph | null>(null);
  const [error, setError] = useState<string | null>(null);

  // `anon()` builds a fresh object each call, so this is memoised to keep
  // the effect below from re-running on every render.
  const session = useMemo(
    () => viewerSession(owner, props.token ?? null),
    [owner, props.token],
  );

  useEffect(() => {
    let alive = true;
    setRepos(null);
    setProfile(null);
    setPins([]);
    setGraph(null);
    setError(null);
    // Search, not `GET /orgs/{org}/repos`.
    //
    // The listing is a protected endpoint and is meant to be: three
    // server suites rest on it refusing a caller with no credential, a
    // caller with a bad one, and a caller from another namespace. This
    // page originally read it, so a signed-out visitor was refused and
    // the page said "nothing public here yet" about a namespace with
    // public repositories in it — the exact opposite of its job.
    //
    // Search already answers "what may this caller see" with
    // `registry::Viewer`, for every kind of caller, which is the
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
    // Filtered by the caller's own visibility server-side, so what comes
    // back is already what this viewer may see.
    api
      .getPins(session, owner)
      .then((r) => alive && setPins(r.pins))
      .catch(() => {});
    // Degrades to absence, like the profile and the pins above it: an
    // organization namespace has no graph at all, and a person whose
    // graph could not be read is better served by the page they came
    // for than by a red box where their work should be.
    api
      .getContributions(session, owner)
      .then((g) => alive && setGraph(g))
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
        {graph && <ContributionGraphView graph={graph} />}
        {pins.length > 0 && (
          <div>
            <h2 className="mb-3 text-sm font-semibold text-ink">Pinned</h2>
            <ul className="grid gap-4 sm:grid-cols-2">
              {pins.map((p) => (
                <li key={`${p.org}/${p.name}`} className="flex">
                  <RepoCard
                    /* Shown only when the pin is somebody else's
                       repository: on a profile's own grid the owner is
                       the profile, and repeating it on six cards is six
                       lines of noise. */
                    owner={p.org === owner ? null : p.org}
                    name={p.name}
                    href={href([p.org, p.name])}
                    description={p.description}
                    visibility={p.public ? "public" : "private"}
                    onNavigate={props.navigate}
                    className="w-full"
                  />
                </li>
              ))}
            </ul>
          </div>
        )}
        <div>
          {error && <ErrorBox message={error} />}
          {!error && !repos && <Loading />}
          {repos && repos.length === 0 && (
            <p className="text-sm text-ink-3">Nothing public here yet.</p>
          )}
          {repos && repos.length > 0 && (
            <>
              {/* Labelled only once there is something above it to tell
                  it apart from. On a profile with no pins the grid is
                  the whole column and a heading over it is a heading
                  over the page. */}
              {pins.length > 0 && (
                <h2 className="mb-3 text-sm font-semibold text-ink">
                  Repositories
                </h2>
              )}
              <ul className="grid gap-4 sm:grid-cols-2">
                {repos.map((r) => (
                  <li key={r.id} className="flex">
                    <RepoCard
                      name={r.name}
                      href={href([owner, r.name])}
                      description={r.description}
                      visibility={r.public ? "public" : "private"}
                      onNavigate={props.navigate}
                      className="w-full"
                    />
                  </li>
                ))}
              </ul>
            </>
          )}
        </div>
      </section>
    </div>
  );
}
