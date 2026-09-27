/// A router, in about a hundred lines, because the alternative was a
/// dependency.
///
/// The dashboard had none: the current repo lived in `useState`, so
/// nothing was linkable and the back button did nothing useful. A code
/// browser makes that intolerable — a file you cannot send somebody is
/// a file browser people use once.
///
/// It used to be mounted at `/dashboard` and strip that prefix on the
/// way in and add it back on the way out. It no longer can: a
/// repository lives at `/{owner}/{repo}` with no prefix at all, and a
/// router that assumed one would have had to be told to stop assuming
/// it on every second page. So this reads and writes the address bar
/// verbatim, and the two *address spaces* — the dashboard under
/// `/dashboard`, the forge at the root — are expressed by the shell
/// that mounts them, with [`mountedAt`].

import { useEffect, useMemo, useState } from "react";

/// Where the dashboard is mounted. The forge has no such prefix, which is the whole reason this is a value rather than a
/// constant baked into `read` and `navigate`.
export const DASH = "/dashboard";

export interface Route {
  /// The path as the address bar has it, always starting with `/`.
  path: string;
  /// `?`-parameters, which is where a revision lives — `at` is not part
  /// of a path because the same path at two revisions is the same page.
  query: URLSearchParams;
}

function read(): Route {
  return {
    path: window.location.pathname || "/",
    query: new URLSearchParams(window.location.search),
  };
}

/// The current route, kept in step with the address bar.
///
/// `popstate` is the browser's back and forward; `stratum:navigate` is
/// ours, because `pushState` deliberately does not fire an event and a
/// router that only heard the browser would miss every link it owns.
export function useRoute(): [Route, (to: string, replace?: boolean) => void] {
  const [route, setRoute] = useState<Route>(read);

  useEffect(() => {
    const sync = () => setRoute(read());
    window.addEventListener("popstate", sync);
    window.addEventListener("stratum:navigate", sync);
    return () => {
      window.removeEventListener("popstate", sync);
      window.removeEventListener("stratum:navigate", sync);
    };
  }, []);

  return [route, navigateTo];
}

/// Move the address bar, client-side, in the browser's own address space
/// — no mount prefix and no page load.
///
/// A module function rather than something only the hook hands out,
/// because the callers who need it are precisely the ones linking *out
/// of* their own mount: the dashboard's repository table, its search
/// results and its changeset page all point at `/{owner}/{repo}`, which
/// `mountedAt`'s navigator would have turned into
/// `/dashboard/{owner}/{repo}`. It is the counterpart of [`dash`] — that
/// one spells an absolute address, this one goes to one.
///
/// Safe to reach for, and this is why: the server serves the *same*
/// `index.html` for `/dashboard/*` and for `/{owner}/…` (`webassets.rs`),
/// and `match` in `routes.ts` decides which space a path is in from the
/// path alone. So a move between the two mounts is an ordinary
/// client-side navigation, not a page load pretending to be one. Before
/// this existed, a dashboard link to a forge address had to be an
/// anchor with no click handler and take the full reload.
///
/// [`useRoute`] returns this unchanged; `mountedAt` is what wraps it
/// with a base. One implementation, so the history entry a mounted view
/// pushes and the one a cross-mount link pushes cannot differ.
export function navigateTo(to: string, replace = false) {
  const url = to.startsWith("/") ? to : `/${to}`;
  if (url === window.location.pathname + window.location.search) return;
  // `replace` for corrections — landing on a bad URL and being moved
  // to a good one should not leave the bad one in history for the
  // back button to return to.
  window.history[replace ? "replaceState" : "pushState"](null, "", url);
  window.dispatchEvent(new Event("stratum:navigate"));
}

/// Read and write a route as if the SPA were mounted at `base`.
///
/// This is what lets a view be written once and mounted twice: the
/// dashboard's pages are written relative to `/dashboard` and the
/// forge's to the root, and neither spells its own prefix. A view that
/// needs to link *out* of its mount uses [`navigateTo`] instead.
export function mountedAt(
  base: string,
  route: Route,
  navigate: (to: string, replace?: boolean) => void,
): [Route, (to: string, replace?: boolean) => void] {
  const inside =
    route.path === base || route.path.startsWith(`${base}/`)
      ? route.path.slice(base.length) || "/"
      : "/";
  return [
    { path: inside, query: route.query },
    (to, replace) =>
      navigate(`${base}${to.startsWith("/") ? to : `/${to}`}`, replace),
  ];
}

/// Build a URL relative to whichever mount the caller is on. One place,
/// so a path and its escaping cannot drift between the link that makes
/// it and the router that reads it.
export function href(
  parts: string[],
  query?: Record<string, string | undefined>,
): string {
  const path = parts.map(encodeURIComponent).join("/");
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(query ?? {})) if (v) q.set(k, v);
  const s = q.toString();
  return `/${path}${s ? `?${s}` : ""}`;
}

/// The same, but absolute against the dashboard mount — for the handful
/// of links that must reach the signed-in dashboard from outside it,
/// such as the forge header's user menu.
export function dash(
  parts: string[],
  query?: Record<string, string | undefined>,
): string {
  return `${DASH}${href(parts, query)}`;
}

/// Split a route path into its segments, decoded.
///
/// Percent-escapes that cannot be decoded are left as they arrived
/// rather than throwing: a malformed URL somebody pasted should show an
/// empty directory, not a blank page.
export function segments(path: string): string[] {
  return path
    .split("/")
    .filter(Boolean)
    .map((s) => {
      try {
        return decodeURIComponent(s);
      } catch {
        return s;
      }
    });
}

/// One `?`-parameter set, kept in step with the address bar.
///
/// [`useRoute`] already returns the query, but only to whoever mounted
/// the app: a repository tab is several components down and is not
/// handed it. This is the same subscription — `popstate` for the
/// browser's back and forward, `stratum:navigate` for ours, which
/// exists because `pushState` deliberately fires nothing — available
/// wherever it is needed.
///
/// It lived in `views/forge/checks.tsx`, which is where it was first
/// needed. It is here now because the code browser needs it too, and
/// the second copy would have been the one that fell behind.
export function useQuery(): URLSearchParams {
  const [search, setSearch] = useState(() => window.location.search);
  useEffect(() => {
    const sync = () => setSearch(window.location.search);
    sync();
    window.addEventListener("popstate", sync);
    window.addEventListener("stratum:navigate", sync);
    return () => {
      window.removeEventListener("popstate", sync);
      window.removeEventListener("stratum:navigate", sync);
    };
  }, []);
  return useMemo(() => new URLSearchParams(search), [search]);
}
