import { type AuthMethods, FALLBACK_METHODS } from "@/lib/auth-methods";

/// How a trip out to sign in — through GitHub, or through the company's
/// identity provider — came back, in words.
///
/// The same shape as `connect-banner`, and for the same reason it was
/// written: the callback answers a redirect carrying an outcome, and an
/// outcome nothing renders is a person landing on a screen that has
/// nothing to say about the thing they just tried. Every refusal here
/// gets its sentence *and* its next step, because every one of them has
/// one.
///
/// Both flows land here, signed out, as `/dashboard/?github=<outcome>`
/// or `/dashboard/?sso=<outcome>`. One banner for both, because they
/// are the same event said about two providers — and a second copy of
/// the component is the one that would drift.
///
/// `ok` is deliberately absent from both. It is a success, the session
/// cookie is set, and a banner congratulating somebody for signing in is
/// noise.

/// GitHub only ever signs somebody in to an account this server already
/// has; it never makes one. So `noaccount` — GitHub said who you are,
/// and nobody here is you — is an ordinary answer. `new` and
/// `emailtaken` are gone with the sign-up they belonged to; a stale link
/// carrying either reads as no outcome at all.
export type GithubSigninOutcome =
  | "noaccount"
  | "denied"
  | "expired"
  | "noemail"
  | "disabled"
  | "unavailable"
  | "error";

/// The company's identity provider. Unlike GitHub it *does* make
/// accounts — the first sign-in creates one — so it has no `noaccount`,
/// and two refusals GitHub cannot give: an address the provider did not
/// vouch for (`noemail`), and one in a domain this server does not
/// admit (`domain`).
export type SsoSigninOutcome =
  | "denied"
  | "expired"
  | "noemail"
  | "domain"
  | "disabled"
  | "unavailable"
  | "error";

export type SigninOutcome =
  | { via: "github"; outcome: GithubSigninOutcome }
  | { via: "sso"; outcome: SsoSigninOutcome };

/// The query parameters a sign-in round trip comes back with.
export const SIGNIN_OUTCOME_KEYS = ["github", "sso"] as const;

export function githubOutcomeOf(
  value: string | null,
): GithubSigninOutcome | null {
  switch (value) {
    case "noaccount":
    case "denied":
    case "expired":
    case "noemail":
    case "disabled":
    case "unavailable":
    case "error":
      return value;
    default:
      return null;
  }
}

export function ssoOutcomeOf(value: string | null): SsoSigninOutcome | null {
  switch (value) {
    case "denied":
    case "expired":
    case "noemail":
    case "domain":
    case "disabled":
    case "unavailable":
    case "error":
      return value;
    default:
      return null;
  }
}

/// Which refusal, if any, the address bar is carrying.
export function signinOutcomeOf(query: URLSearchParams): SigninOutcome | null {
  const sso = ssoOutcomeOf(query.get("sso"));
  if (sso) return { via: "sso", outcome: sso };
  const github = githubOutcomeOf(query.get("github"));
  if (github) return { via: "github", outcome: github };
  return null;
}

/// The query string with every sign-in outcome taken out, `?` and all
/// when nothing is left — and anything else it carried left alone.
///
/// An outcome is said once, on the screen the round trip lands on, and
/// then it is gone from the address bar. Left there, it is said again
/// on every reload, and again after the person signs out an hour later
/// — "you cancelled at GitHub", about nothing they just did — and it
/// travels with the address if it is bookmarked or sent to somebody.
export function withoutSigninOutcome(search: string): string {
  const q = new URLSearchParams(search);
  if (!SIGNIN_OUTCOME_KEYS.some((k) => q.has(k))) return search;
  for (const k of SIGNIN_OUTCOME_KEYS) q.delete(k);
  const rest = q.toString();
  return rest ? `?${rest}` : "";
}

export function githubOutcomeLine(
  o: GithubSigninOutcome,
  methods: AuthMethods = FALLBACK_METHODS,
): string {
  switch (o) {
    case "noaccount":
      // With the company's identity provider configured, an account is
      // one sign-in away, and "ask for an invitation" would send
      // somebody the long way round to a door that is open.
      if (methods.sso) {
        return `GitHub told us who you are, but nobody on this server is you. Continue with ${methods.sso.name} instead — your account is made the first time.${
          methods.password
            ? " If you already have an account under a different address, sign in with that address and its password."
            : ""
        }`;
      }
      return "GitHub told us who you are, but nobody on this server is you. Accounts here are made by invitation: ask an admin of your organization to invite you, and accept the invitation from the email. If you already have an account under a different address, sign in with that address and its password.";
    case "denied":
      return "You cancelled at GitHub, so nothing was signed in. Try again, or use an email address and password.";
    case "expired":
      return "That sign-in was not finished in the browser that started it, or it had already been used. Press Continue with GitHub again.";
    case "noemail":
      return "GitHub did not give us an address it has confirmed. Verify your primary email address on GitHub and allow this server to read it, then try again. Signing in with your email address and password still works.";
    case "disabled":
      return "This account has been disabled. Ask an owner of your organization to re-enable it.";
    case "unavailable":
      return "Signing in with GitHub is not set up on this server. Use an email address and password.";
    case "error":
      return "GitHub did not answer, so nothing was signed in. Try again in a moment.";
  }
}

/// Every one of these is about the operator's identity provider, so it
/// is called by the operator's name for it. When the server no longer
/// says SSO is configured at all there is no name to use, and "SSO" is
/// what the server itself calls one nobody named.
export function ssoOutcomeLine(
  o: SsoSigninOutcome,
  methods: AuthMethods = FALLBACK_METHODS,
): string {
  const name = methods.sso?.name ?? "SSO";
  switch (o) {
    case "denied":
      return `You cancelled signing in with ${name}, so nothing was signed in. Press Continue with ${name} to try again.`;
    case "expired":
      return `That sign-in was not finished in the browser that started it, or it took too long, so nothing was signed in. Press Continue with ${name} to start again.`;
    case "noemail":
      return `${name} did not give this server an email address it can trust, so nothing was signed in. Ask whoever runs this server to have ${name} send a verified address, or to allow your address's domain.`;
    case "domain":
      return `The address ${name} gave for you is not in a domain this server admits, so nothing was signed in. If you should have an account here, ask whoever runs this server.`;
    case "disabled":
      return "This account has been disabled, so nothing was signed in. Ask whoever runs this server to re-enable it.";
    case "unavailable":
      // Said without the label: the server answers this when single
      // sign-on is not configured, which is exactly when it has none.
      return `Single sign-on is not set up on this server, so nothing was signed in. ${
        methods.password
          ? "Sign in with an email address and password instead."
          : methods.github
            ? "Continue with GitHub instead."
            : "Ask whoever runs this server how to sign in."
      }`;
    case "error":
      return `${name} did not answer, or something failed on this server, so nothing was signed in. Try again in a moment, and tell whoever runs this server if it keeps happening.`;
  }
}

export function signinOutcomeLine(
  o: SigninOutcome,
  methods: AuthMethods = FALLBACK_METHODS,
): string {
  return o.via === "sso"
    ? ssoOutcomeLine(o.outcome, methods)
    : githubOutcomeLine(o.outcome, methods);
}

export function SigninBanner(props: {
  outcome: SigninOutcome;
  methods: AuthMethods;
}) {
  return (
    <div
      className="mb-3 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-sm"
      role="status"
    >
      {signinOutcomeLine(props.outcome, props.methods)}
    </div>
  );
}
