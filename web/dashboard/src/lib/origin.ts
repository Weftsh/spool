/// Where a mirrored repository came from, said in a way a reader can
/// check.
///
/// These two functions used to live in `components/star-button.tsx`,
/// which is where the imported star count was rendered. The count moved
/// to the About rail — a fact about the upstream project belongs beside
/// the other facts about the project, not hanging under a button as an
/// annotation on Star — and the star control is now a plain split
/// control with nothing to say about origins. Two callers make this a
/// vocabulary rather than one component's private helper.

/// How an imported count reads beside our own.
///
/// The provider is named from the origin URL's host rather than stored
/// separately, because the label has to match where the link goes. A
/// mirror of `github.com/rails/rails` says "on GitHub"; anything we
/// cannot name says "upstream", which is vaguer and still true — and is
/// much better than confidently naming the wrong forge.
export function originLabel(url: string | null): string {
  if (!url) return "upstream";
  const host = url
    .trim()
    .replace(/^https?:\/\//i, "")
    .split("/")[0]
    .toLowerCase();
  const known: Record<string, string> = {
    "github.com": "GitHub",
    "www.github.com": "GitHub",
    "gitlab.com": "GitLab",
    "www.gitlab.com": "GitLab",
    "codeberg.org": "Codeberg",
    "bitbucket.org": "Bitbucket",
  };
  if (known[host]) return known[host];
  // A bare `owner/name` origin has no host at all — the shorthand the
  // mirror API accepts. Saying "on owner" would be nonsense.
  return host && host.includes(".") ? host : "upstream";
}

/// Only render a link if it is one a browser may follow.
///
/// The origin URL reaches this component from a database column that a
/// person typed into a mirror form. Same reasoning as the profile
/// rail's guard: an allowlist, because a list of schemes to refuse is
/// never finished, and this is the last place the string is inert
/// before it becomes an `href`.
export function originHref(url: string | null): string | null {
  if (!url) return null;
  const trimmed = url.trim();
  const lower = trimmed.toLowerCase();
  if (!lower.startsWith("http://") && !lower.startsWith("https://")) {
    return null;
  }
  return trimmed;
}
