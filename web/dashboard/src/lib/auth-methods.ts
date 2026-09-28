/// How this server lets a person sign in, as `GET /v1/auth/methods`
/// says — and what the dashboard does when it cannot find out.
///
/// Three independent answers. `password` is the email-and-password form
/// and everything hanging off it (forgetting one, resetting one,
/// choosing one while accepting an invitation, changing one). `github`
/// is "Continue with GitHub", which is off both when the server has no
/// GitHub OAuth client and when single sign-on is the only way in — a
/// linked GitHub account would otherwise let somebody the company has
/// switched off at its identity provider keep signing in. `sso` is the
/// company's identity provider, named the way the operator named it.
///
/// **A failure reads as today's screen, never as a blank one.** A server
/// that predates the route answers 404, a proxy in the way may answer a
/// page of HTML, and a network can drop the request: in every one of
/// those cases the person is shown the password form and the GitHub
/// button, which is what every server offered before this endpoint
/// existed. If that server does refuse passwords, the refusal carries
/// its own sentence — worse than knowing, much better than nothing.
export interface SsoMethod {
  /// The operator's label for their identity provider: "Okta", "Entra
  /// ID". The server says "SSO" when the operator named nothing.
  name: string;
  /// Where the round trip starts. An ordinary link — a full-page leave
  /// for the identity provider, like the GitHub button's.
  start: string;
}

export interface AuthMethods {
  password: boolean;
  github: boolean;
  sso: SsoMethod | null;
}

/// What every server offered before it could say otherwise.
export const FALLBACK_METHODS: AuthMethods = Object.freeze({
  password: true,
  github: true,
  sso: null,
});

/// The server's own start route, for an `sso` block whose `start` is not
/// a path on this origin.
export const SSO_START = "/v1/auth/sso/start";

/// The body of `GET /v1/auth/methods`, read defensively.
///
/// Both flags have to be booleans or the whole body is somebody else's
/// answer — an older server's catch-all, a proxy's error page — and
/// reading half of it would be a guess about which half was meant. The
/// `sso` block is its own question: absent or malformed is "not
/// configured".
export function authMethodsOf(body: unknown): AuthMethods {
  if (!body || typeof body !== "object" || Array.isArray(body)) {
    return FALLBACK_METHODS;
  }
  const b = body as Record<string, unknown>;
  if (typeof b.password !== "boolean" || typeof b.github !== "boolean") {
    return FALLBACK_METHODS;
  }
  return { password: b.password, github: b.github, sso: ssoOf(b.sso) };
}

function ssoOf(v: unknown): SsoMethod | null {
  if (!v || typeof v !== "object" || Array.isArray(v)) return null;
  const s = v as Record<string, unknown>;
  const name =
    typeof s.name === "string" && s.name.trim() ? s.name.trim() : "SSO";
  return { name, start: sameOriginPath(s.start) ?? SSO_START };
}

/// A path on this origin, or nothing.
///
/// The link is rendered as an `href`, so a value that is not one of our
/// own paths — `javascript:…`, `//elsewhere`, an absolute URL — is not
/// trusted to be what the server meant. The server's own start route is
/// used instead: SSO is configured, so that is where the round trip
/// begins either way.
function sameOriginPath(v: unknown): string | null {
  if (typeof v !== "string") return null;
  if (!v.startsWith("/") || v.startsWith("//") || v.startsWith("/\\")) {
    return null;
  }
  return v;
}

/// Ask the server, and never fail: anything but an answer in the right
/// shape is today's screen. `ask` is the request, passed in so the rule
/// can be tested without one.
export function loadAuthMethods(
  ask: () => Promise<unknown>,
): Promise<AuthMethods> {
  return ask().then(authMethodsOf, () => FALLBACK_METHODS);
}

/// What the sign-in screen says to somebody who has no account.
///
/// With single sign-on configured, anybody the company's identity
/// provider signs in gets an account the first time — so that is the
/// answer, whatever else is on. Without it, accounts come from an
/// organization's invitation or from whoever runs the server.
export function noAccountLine(methods: AuthMethods): string {
  if (methods.sso) {
    return `No account yet? Sign in with ${methods.sso.name} — your account is made the first time.`;
  }
  return "No account yet? Accounts on this server are made by invitation. Ask an admin of your organization to invite you, or whoever runs this server to add you, then use the link in the email.";
}

/// What a screen that exists only for passwords — a reset link, an
/// invitation that would have asked for one — says first on a server
/// that has switched them off: the way in there is, by name.
export function passwordsOffLine(methods: AuthMethods): string {
  if (methods.sso) return `This server signs in with ${methods.sso.name}`;
  return "This server does not sign in with passwords";
}
