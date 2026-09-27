/// How the GitHub install round trip came back, in words.
///
/// The callback answers a redirect carrying `?connect=<outcome>`, and
/// for a long time only `ok` did anything on this side: every other
/// outcome landed the person on the overview with nothing to say. The
/// first real install on weft.sh came back without a `state` (GitHub
/// sends none after an installation is *edited*), the callback said
/// `missing`, and the screen showed a "GitHub is not connected" notice
/// with an Install button that led to an app already installed — a
/// loop with no exit. Each outcome now gets its sentence and its next
/// step.
export type ConnectOutcome =
  "notyours" | "expired" | "missing" | "which" | "taken" | "error";

export function connectOutcomeOf(value: string | null): ConnectOutcome | null {
  switch (value) {
    case "notyours":
    case "expired":
    case "missing":
    case "which":
    case "taken":
    case "error":
      return value;
    default:
      return null;
  }
}

export function connectOutcomeLine(o: ConnectOutcome): string {
  switch (o) {
    case "notyours":
      return "GitHub did not confirm that the installation is yours, so nothing was connected. Start again from Connect GitHub and finish on GitHub signed in as the account that owns the repositories.";
    case "expired":
      return "That connection link was already used or has expired. Start again from Connect GitHub.";
    case "missing":
      return "GitHub sent you back without saying which organization this was for. Start from Connect GitHub in the organization you meant, and finish on GitHub in the same tab.";
    case "which":
      return "You started connecting GitHub from more than one organization, so it is not clear which one this is for. Start again from the one you meant.";
    case "taken":
      return "That GitHub installation is already connected to another organization here. Each installation can serve one organization.";
    case "error":
      return "GitHub did not answer while we checked that the installation is yours. Nothing was connected; try again in a moment.";
  }
}

export function ConnectBanner(props: { outcome: ConnectOutcome }) {
  return (
    <div
      className="mb-6 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-sm"
      role="status"
    >
      {connectOutcomeLine(props.outcome)}
    </div>
  );
}
