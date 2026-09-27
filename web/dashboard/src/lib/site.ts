import type { SiteDeploy, SiteStatus } from "@/api";

/// The decisions the repository's Site panel makes, kept out of the view
/// so they can be tested without a browser — the same split as
/// `lib/github-runners.ts`.
///
/// The one thing this module exists to stop the view from doing is
/// collapsing two independent facts into one. `enabled` and `deploys`
/// say what is being **served**; `config_state` says how
/// `.weft/site.yml` parses **now**. They disagree exactly when somebody
/// has just broken the file, and that is the moment the panel matters:
/// the site keeps serving the last good deploy, and the refusal is the
/// answer to "why has my site not updated". A single `switch` over one
/// of the two would print either the address or the error and never
/// both.

/// The file that turns a repository into a website. `site.yaml` is
/// accepted too, but one name has to be the one we teach.
export const CONFIG_PATH = ".weft/site.yml";

/// The smallest config that does something, exactly as it must be
/// typed. Two lines, because the second is the one people forget: the
/// directory is not guessed.
export const EXAMPLE_CONFIG = "publish: dist\n";

/// Said in the panel, in these words, whenever a site is live.
///
/// This is a footgun and it is not hypothetical: a private repository
/// with a `publish:` directory serves that directory to anybody who has
/// the address, with no sign-in, and nothing about the repository's own
/// visibility changes to say so. The sentence names the surprising half
/// first.
export const PUBLIC_WARNING =
  "A published site is public. Anyone with the address can read every " +
  "file in the published directory, signed in or not, even while this " +
  "repository stays private.";

/// How `.weft/site.yml` parses right now.
///
/// `unknown` is a fourth word from a server newer than this bundle. It
/// is reported as itself rather than folded into one of the three:
/// guessing `ok` would claim a config parsed that nobody checked.
export type ConfigReport =
  | { kind: "ok"; publish: string; spa: boolean; notFound: string | null }
  | { kind: "absent" }
  | { kind: "refused"; error: string }
  | { kind: "unknown"; word: string };

/// What is being served.
///
/// - `off`: no site row at all. Nothing has ever published.
/// - `pending`: a site exists and nothing has published to it yet —
///   the window between a config landing and the first push that
///   publishes. There is no address to hand out.
/// - `live`: a deploy is being served. `url` is still nullable, because
///   a deployment that does not host sites reports none, and inventing
///   one from `host` would produce an address that resolves nowhere.
export type SiteReport =
  | { kind: "off" }
  | { kind: "pending" }
  | {
      kind: "live";
      url: string | null;
      current: SiteDeploy;
      recent: SiteDeploy[];
    };

export interface SiteView {
  config: ConfigReport;
  site: SiteReport;
  /// The branch whose pushes publish, already resolved: the server
  /// reports `null` for "the repository's default branch", and a panel
  /// that printed "null" or "default" would be telling somebody to go
  /// and look it up.
  branch: string;
  /// The directory being served, or — when nothing is served yet — the
  /// one the config names. `null` when neither is known.
  ///
  /// When a site is live this is the **deploy's** directory, not the
  /// file's. The two differ for exactly as long as an edit has not been
  /// published, which is the interval somebody is staring at the panel
  /// wondering what is actually up there.
  publish: string | null;
}

/// How many deploys the panel lists. "The most recent few": enough to
/// see that publishing is happening and to spot the one that is being
/// served, not an archive. The route answers at most 20 regardless.
export const MAX_SHOWN_DEPLOYS = 5;

export function siteView(
  status: SiteStatus,
  /// The repository's own default branch, for resolving `branch: null`.
  defaultBranch: string,
): SiteView {
  const config = configReport(status);
  const site = siteReport(status);
  return {
    config,
    site,
    branch: status.branch ?? defaultBranch,
    publish:
      site.kind === "live"
        ? site.current.publish
        : config.kind === "ok"
          ? config.publish
          : null,
  };
}

function configReport(status: SiteStatus): ConfigReport {
  switch (status.config_state) {
    case "absent":
      return { kind: "absent" };
    case "refused":
      // The server's own sentence, with its file and line. A missing
      // one is still a refusal — reporting it as "the config is fine"
      // would be the one lie this panel must never tell.
      return {
        kind: "refused",
        error: status.config_error ?? "no reason was recorded",
      };
    case "ok":
      // `config` rides with `ok` and only with `ok`. A server that sent
      // one without the other is not a reason to render a panel full of
      // blanks: the config's own defaults are the honest fallback, and
      // they are the same defaults the parser applies.
      return {
        kind: "ok",
        publish: status.config?.publish ?? "dist",
        spa: status.config?.spa ?? false,
        notFound: status.config?.not_found ?? null,
      };
  }
  return { kind: "unknown", word: String(status.config_state) };
}

function siteReport(status: SiteStatus): SiteReport {
  if (!status.enabled) return { kind: "off" };
  // `current` names a deploy by id rather than being the newest one:
  // a rollback serves an older row, and taking `deploys[0]` would show
  // the wrong commit as the live one on exactly the occasion somebody
  // is checking which commit is live.
  const current = status.deploys.find((d) => d.id === status.current);
  if (!current) return { kind: "pending" };
  return {
    kind: "live",
    url: status.url,
    current,
    recent: status.deploys.slice(0, MAX_SHOWN_DEPLOYS),
  };
}

/// The address as a person reads it — no scheme, no trailing slash.
/// Same rule as the About panel's homepage, and for the same reason:
/// `https://` in front of every address is nine characters of noise.
export function siteLabel(url: string): string {
  return url
    .trim()
    .replace(/^https?:\/\//i, "")
    .replace(/\/+$/, "");
}

/// The link's accessible name. A bare URL read aloud is a string of
/// letters with no sentence around it; this says what following it
/// does, and still contains the visible text so voice control can
/// address it by what is on screen.
export function siteLinkLabel(url: string): string {
  return `Open the published site at ${siteLabel(url)}`;
}

/// Which deploy row is the one being served, for the badge beside it.
export function isCurrent(view: SiteView, deploy: SiteDeploy): boolean {
  return view.site.kind === "live" && view.site.current.id === deploy.id;
}

/// The line for a site that is not serving anything, or `null` when
/// there is nothing useful to add.
///
/// `null` in three of the four cases on purpose. A refusal and a missing
/// config each already have a block of their own that says what to do,
/// and a second sentence underneath repeating "nothing has published"
/// buries the one that carries the instruction. This is only for the
/// case with no other explanation on screen: the config is fine and the
/// push has not happened yet.
export function nothingPublishedLine(view: SiteView): string | null {
  if (view.site.kind === "live" || view.config.kind !== "ok") return null;
  return `Nothing has published yet. The next push to ${view.branch} publishes ${view.publish}/.`;
}

/// What to say about a `config_state` this bundle has never heard of.
/// The server's own word, quoted, rather than a guess at which of the
/// three it resembles.
export function unknownConfigLine(word: string): string {
  return `This server reports the site configuration as "${word}", which this page is too old to explain.`;
}
