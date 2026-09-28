import { safeNext } from "@/routes";

/// Where to go after a sign-in round trip through GitHub or the
/// company's identity provider.
///
/// `/login?next=/acme/widget` is how a signed-out visitor to a page is
/// brought back to it, and the password form honours it directly. A
/// round trip through a provider could not: the browser leaves for
/// `/v1/auth/sso/start` (or GitHub's), and the server's callback sends
/// it home to `/dashboard/?sso=ok`, with the page it came from gone. So
/// the address is kept here, in this tab's `sessionStorage`, for the
/// length of the trip — rather than handed to the server as a redirect
/// parameter, which would be a new open-redirect surface on the one page
/// people are taught to trust.
///
/// Three rules, and every one of them is about the value not outliving
/// the trip it belongs to or leaving this site:
///
/// - It is checked on the way in and again on the way out, by the same
///   `safeNext` the password path uses: a path on this origin, or
///   nothing. Storage is not ours alone — any script on the origin, or
///   an extension, can write it — so what comes back is not trusted for
///   having been written by us.
/// - It is taken once. Any return — a success or a refusal — removes
///   it, so a later sign-in cannot pick up a destination somebody asked
///   for an hour ago.
/// - Leaving with no address to return to removes any old one, for the
///   same reason.
///
/// Storage that refuses (a private window, disabled site data) only
/// means the trip lands on the overview, as it always did.
export const RETURN_TO_KEY = "stratum-signin-next";

type Store = Pick<Storage, "getItem" | "setItem" | "removeItem">;

/// This tab's `sessionStorage`, or nothing. Even reading the property
/// throws where site data is blocked.
export function tabStorage(): Store | null {
  try {
    return window.sessionStorage;
  } catch {
    return null;
  }
}

/// A destination worth going to: a path on this site other than its
/// front door, which is where a round trip lands anyway.
function destination(candidate: string | null | undefined): string | null {
  const to = safeNext(candidate ?? null);
  return to === "/" ? null : to;
}

/// Just before the browser leaves for a provider: keep `next`, or forget
/// any older one when there is none.
export function rememberReturn(
  storage: Store | null,
  next: string | null | undefined,
): void {
  const to = destination(next);
  try {
    if (to) storage?.setItem(RETURN_TO_KEY, to);
    else storage?.removeItem(RETURN_TO_KEY);
  } catch {
    // Storage refused: the trip lands on the overview.
  }
}

/// On the way home: the kept destination, removed whatever it was, and
/// returned only if it is still one of ours.
export function takeReturn(storage: Store | null): string | null {
  let kept: string | null = null;
  try {
    kept = storage?.getItem(RETURN_TO_KEY) ?? null;
    storage?.removeItem(RETURN_TO_KEY);
  } catch {
    return null;
  }
  return destination(kept);
}

/// How the address bar says a round trip came home: signed in (`ok`),
/// anything else (a refusal, or a value this page does not know), or
/// not a return at all.
export function signinReturnOf(
  query: URLSearchParams,
): "ok" | "other" | null {
  const values = ["sso", "github"]
    .filter((k) => query.has(k))
    .map((k) => query.get(k));
  if (values.length === 0) return null;
  return values.includes("ok") ? "ok" : "other";
}
