/// The "Details" link on a check row, which now points to two different
/// kinds of place.
///
/// Every check row carries a `detail_url` and until hosted runs existed
/// that URL was always somebody else's site — a GitHub Actions run, a
/// Buildkite build — so both places that render one opened it in a new
/// tab with `rel="nofollow ugc noopener noreferrer"`. That is exactly
/// right for a URL a third party posted to our intake endpoint, and
/// exactly wrong for the one the server now writes for a run on our own
/// runners: it points at this SPA, and a new tab with a full page load
/// is a reader losing their place, their scroll position and a second of
/// their time to reach a page that was one client-side navigation away.
/// `ugc` on a first-party URL is a claim about our own site that is
/// simply false.
///
/// Two things have to be true before a row is treated as ours, and the
/// reasons are different:
///
///   * `provider === "weft"`, which says *the server wrote this row*.
///     Nothing outside the mirror can spell that: `checks_intake` writes
///     `provider = "intake"` as a constant, so the field is ours and not
///     the reporter's.
///   * the URL is on this origin, which says *the link lands here*. A
///     client-side navigation to another origin's path is a 404 wearing
///     our chrome.
///
/// Either one alone is weaker than it looks. Origin alone would let a
/// URL a third party posted through the intake decide it navigates
/// inside the app, simply by naming our own host. Provider alone would
/// hand an in-app navigation to a deployment whose `STRATUM_PUBLIC_URL`
/// points somewhere else, which is a configuration mistake we would
/// rather degrade from than break on.

import { HOSTED_PROVIDER } from "@/lib/hosted-runs";

/// The path part of a URL that is on this origin, or `null`.
///
/// `origin` is a parameter rather than a read of `window.location` so
/// that this is answerable without a browser — the whole reason the
/// decision lives in a function.
///
/// Only an **absolute** URL is ever treated as internal. A relative
/// `detail_url` would resolve against our own origin and so would look
/// internal by construction, which would let a value posted to the
/// intake by a third party decide that a link navigates inside the app.
/// Nothing behind that is more dangerous than a wrong page, but "the
/// remote party chooses" is not a rule worth having: the server writes
/// hosted `detail_url`s absolute, so requiring it costs nothing.
export function sameOriginPath(url: string, origin: string): string | null {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  // `origin` comparison and not a host one: `http://x` and `https://x`
  // are different origins, and treating a downgrade as internal would
  // navigate in-app to a page loaded over the other scheme.
  if (parsed.origin !== origin) return null;
  return parsed.pathname + parsed.search + parsed.hash;
}

/// The in-app path a check row's link should navigate to, or `null` if
/// the row leaves for somebody else's site.
export function inAppPath(
  provider: string,
  url: string,
  origin: string,
): string | null {
  if (provider !== HOSTED_PROVIDER) return null;
  return sameOriginPath(url, origin);
}

export function DetailLink(props: {
  href: string;
  /// `check_runs.provider`. Server-assigned, never the reporter's word
  /// for itself — see the module comment.
  provider: string;
  navigate: (to: string, replace?: boolean) => void;
  className?: string;
  /// Whether an *outbound* link opens in a new tab. A row of ours never
  /// does, whatever this says: leaving the app is the thing a new tab is
  /// for.
  newTab?: boolean;
  children: React.ReactNode;
}) {
  const inApp = inAppPath(
    props.provider,
    props.href,
    // Guarded because this module is imported by unit tests that render
    // without a DOM; they pass the origin to `inAppPath` directly.
    typeof window === "undefined" ? "" : window.location.origin,
  );
  if (inApp === null) {
    // `ugc` because the URL arrived from a third party over an intake
    // endpoint, and `noopener` because without it the target page holds
    // a handle on this one.
    return (
      <a
        className={props.className}
        href={props.href}
        rel="nofollow ugc noopener noreferrer"
        target={props.newTab ? "_blank" : undefined}
      >
        {props.children}
      </a>
    );
  }
  // A real `href`, not a button styled as a link: middle-click, "open in
  // new tab" and a bare hover preview all have to keep working, and only
  // the plain left click is intercepted. `metaKey`/`ctrlKey`/`shiftKey`
  // are the modifiers a browser reads as "somewhere else", and
  // swallowing those would break the one interaction a link is for.
  return (
    <a
      className={props.className}
      href={inApp}
      onClick={(e) => {
        if (e.defaultPrevented) return;
        if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey)
          return;
        e.preventDefault();
        props.navigate(inApp);
      }}
    >
      {props.children}
    </a>
  );
}
