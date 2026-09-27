/// How signing in with GitHub came back, in words.
///
/// The same shape as `connect-banner`, and for the same reason it was
/// written: the callback answers a redirect carrying an outcome, and an
/// outcome nothing renders is a person landing on a screen that has
/// nothing to say about the thing they just tried. Every refusal here
/// gets its sentence *and* its next step, because every one of them has
/// one — usually "use an email address and password", which still works.
///
/// `ok` and `new` are deliberately absent. They are successes, the
/// session cookie is set, and a banner congratulating somebody for
/// signing in is noise.
export type GithubSigninOutcome =
  | "denied"
  | "expired"
  | "noemail"
  | "emailtaken"
  | "disabled"
  | "unavailable"
  | "error";

export function githubOutcomeOf(
  value: string | null,
): GithubSigninOutcome | null {
  switch (value) {
    case "denied":
    case "expired":
    case "noemail":
    case "emailtaken":
    case "disabled":
    case "unavailable":
    case "error":
      return value;
    default:
      return null;
  }
}

export function githubOutcomeLine(o: GithubSigninOutcome): string {
  switch (o) {
    case "denied":
      return "You cancelled at GitHub, so nothing was signed in. Try again, or use an email address and password.";
    case "expired":
      return "That sign-in was not finished in the browser that started it, or it had already been used. Press Continue with GitHub again.";
    case "noemail":
      return "GitHub did not give us an address it has confirmed. Verify your primary email address on GitHub and allow Weft to read it, then try again — or sign up with an email address and password instead.";
    case "emailtaken":
      return "Your GitHub address is already listed on a different account here as an additional address. Remove it there first, or sign in to that account with its own email address and password.";
    case "disabled":
      return "This account has been disabled. Ask an owner of your organization to re-enable it.";
    case "unavailable":
      return "Signing in with GitHub is not set up on this server. Use an email address and password.";
    case "error":
      return "GitHub did not answer, so nothing was signed in. Try again in a moment.";
  }
}

export function GithubSigninBanner(props: { outcome: GithubSigninOutcome }) {
  return (
    <div
      className="mb-3 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-sm"
      role="status"
    >
      {githubOutcomeLine(props.outcome)}
    </div>
  );
}
