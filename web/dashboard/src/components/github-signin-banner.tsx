/// How signing in with GitHub came back, in words.
///
/// The same shape as `connect-banner`, and for the same reason it was
/// written: the callback answers a redirect carrying an outcome, and an
/// outcome nothing renders is a person landing on a screen that has
/// nothing to say about the thing they just tried. Every refusal here
/// gets its sentence *and* its next step, because every one of them has
/// one — usually "use an email address and password", which still works.
///
/// GitHub only ever signs somebody in to an account this server already
/// has; it never makes one. So `noaccount` — GitHub said who you are,
/// and nobody here is you — is an ordinary answer, and its next step is
/// the only way anybody gets an account: an invitation.
///
/// `ok` is deliberately absent. It is a success, the session cookie is
/// set, and a banner congratulating somebody for signing in is noise.
/// `new` and `emailtaken` are gone with the sign-up they belonged to; a
/// stale link carrying either reads as no outcome at all.
export type GithubSigninOutcome =
  | "noaccount"
  | "denied"
  | "expired"
  | "noemail"
  | "disabled"
  | "unavailable"
  | "error";

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

export function githubOutcomeLine(o: GithubSigninOutcome): string {
  switch (o) {
    case "noaccount":
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
