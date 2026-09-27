// Where a changeset page's links point, per mount.
//
// A changeset now has two addresses for the same object —
// `/dashboard/changesets/{key}` for a member and
// `/{owner}/changesets/{key}` for anybody at all — and every link on the
// page resolves differently between them. This is the seam that lets one
// rendering serve both, and it is a module of its own so the order strip
// can read it without importing the page that draws it: a cycle between
// the two would work until the day a bundler ordered them the other way.

import type { ReactNode } from "react";
import { dash, href, navigateTo } from "@/router";

/// One address this page links to: where it points in the browser's own
/// bar, and how the mount follows it without a page load.
///
/// `open` is absent where the address belongs to a *different* mount — a
/// member's change is reviewed on a forge page even when this changeset
/// is the dashboard's — and the anchor is then an ordinary link and an
/// ordinary load, which is the honest answer rather than a client-side
/// navigation to a route that does not exist on this mount.
export interface MountLink {
  href: string;
  open?: () => void;
}

/// Everywhere one changeset page links to, in the terms of the mount it
/// is drawn on.
///
/// A prop rather than `href(...)` inline because this page now has two
/// addresses — `/dashboard/changesets/{key}` inside the dashboard and
/// `/{owner}/changesets/{key}` on the forge — and every link on it
/// resolves differently between them. Building them inline is what kept
/// the forge mount out of reach: `dash([...])` is a dashboard address,
/// and handing one to a reader who is already on the forge is a full
/// page load out of the shell they are standing in.
export interface ChangesetLinks {
  /// The org-wide list, or `null` on a mount that deliberately has none.
  /// The forge has none: the list lives on the dashboard, and one list
  /// has one address.
  list: MountLink | null;
  /// Another changeset in the same organization — the one this reverts,
  /// or one that reverts it.
  changeset: (key: string) => MountLink;
  /// A member repository.
  repo: (repo: string) => MountLink;
  /// A member's change, where it is reviewed under that repository's own
  /// OWNERS. Approval happens there and never here.
  member: (repo: string, change: string) => MountLink;
}

/// The links as the signed-in dashboard addresses them.
export function dashboardChangesetLinks(
  org: string,
  navigate: (to: string, replace?: boolean) => void,
): ChangesetLinks {
  const here = (parts: string[]): MountLink => ({
    // The anchor carries the full address so it can be copied, opened in
    // a new tab and read by anything that scrapes links; the click stays
    // inside the SPA.
    href: dash(parts),
    open: () => navigate(href(parts)),
  });
  // Somewhere on the forge, reached from here. Both mounts are the same
  // bundle behind the same `index.html`, so this is an ordinary
  // client-side move and not a reload — see `navigateTo`.
  const there = (parts: string[]): MountLink => ({
    href: href(parts),
    open: () => navigateTo(href(parts)),
  });
  return {
    list: here(["changesets"]),
    changeset: (key) => here(["changesets", key]),
    // A repository has one address, and it is not under `/dashboard`.
    // This used to be `here(["repos", repo])`, a second address for a
    // page that already had one.
    repo: (repo) => there([org, repo]),
    // A change is reviewed under its own repository's OWNERS, on that
    // repository's page. It had no `open` at all until `navigateTo`
    // existed, so following it took a full page load out of the shell.
    member: (repo, change) => there([org, repo, "changes", change]),
  };
}

/// The links as the forge addresses them, where every one of them
/// is a route on this same mount and a client-side move.
export function forgeChangesetLinks(
  owner: string,
  navigate: (to: string, replace?: boolean) => void,
): ChangesetLinks {
  const here = (parts: string[]): MountLink => ({
    href: href(parts),
    open: () => navigate(href(parts)),
  });
  return {
    // Deliberately absent: the forge has no org-wide changeset list —
    // it lives on the dashboard — so the page shows no way back to one
    // rather than a link to a 404.
    list: null,
    changeset: (key) => here([owner, "changesets", key]),
    repo: (repo) => here([owner, repo]),
    member: (repo, change) => here([owner, repo, "changes", change]),
  };
}

/// One [`MountLink`] as an anchor.
export function MountAnchor(props: {
  link: MountLink;
  className?: string;
  children: ReactNode;
}) {
  const { open } = props.link;
  return (
    <a
      href={props.link.href}
      className={props.className}
      onClick={
        open
          ? (e) => {
              e.preventDefault();
              open();
            }
          : undefined
      }
    >
      {props.children}
    </a>
  );
}

/// Go somewhere this page decided to go on its own — the revert it just
/// made — rather than somewhere the reader clicked. A mount with no
/// route for the destination still has to arrive, so it loads it.
export function follow(link: MountLink) {
  if (link.open) link.open();
  else window.location.assign(link.href);
}
