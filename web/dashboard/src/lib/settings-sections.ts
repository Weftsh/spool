import { type Me } from "@/api";

/// The settings sections a viewer may act on — the single source of
/// truth for the sidebar group and the /settings/:section redirect.
/// Members and Billing need org:admin (a token session counts: me is
/// null and the credential's own scopes gate the API); Password only
/// exists for password accounts. A section absent here is a section
/// whose page would only ever answer 404.
export interface SettingsSection {
  slug: string;
  label: string;
  /// Whose setting it is. The rail used to list all of them under one
  /// "Settings" label, so Billing sat two rows above Password and
  /// nothing said that one changes the organization for everybody and
  /// the other changes only you. The sidebar renders one group each.
  group: "org" | "account";
}

export function settingsSections(
  me: Me | null,
  org: string,
): SettingsSection[] {
  const role = me?.orgs.find((o) => o.name === org)?.role;
  const admin = role === "owner" || role === "admin" || me === null;
  return [
    ...(admin
      ? [
          { slug: "members", label: "Members", group: "org" as const },
          { slug: "billing", label: "Billing", group: "org" as const },
          // Where jobs may run, and on whose machines. Admin for the
          // same reason Billing is: the policy decides what every
          // repository in the org is allowed to do, and a runner
          // credential reaches a machine somebody owns.
          { slug: "runners", label: "Runners", group: "org" as const },
        ]
      : []),
    { slug: "teams", label: "Teams", group: "org" },
    // The registry. A member sees it — they are the ones publishing and
    // installing, and they need the `.npmrc` snippet — but only an admin
    // can switch an ecosystem on, which the panel enforces and the
    // server enforces again.
    //
    // **After Teams, deliberately.** The first entry a viewer can see is
    // where `/settings/:section` sends them when they deep-link one they
    // may not open, so its position is the default settings page for
    // every non-admin. Putting Packages first changed that default
    // silently — the ordering here is a product decision, not a list.
    { slug: "packages", label: "Packages", group: "org" },
    { slug: "activity", label: "Activity", group: "org" },
    { slug: "tokens", label: "Tokens", group: "account" },
    { slug: "ssh-keys", label: "SSH keys", group: "account" },
    // A person's addresses, and the only way to reach the contribution
    // graph: a commit counts for you only when the address on it is one
    // you have proved. Gated on `me` for the same reason as Password —
    // a token session has no person behind it and every call would 401.
    ...(me ? [{ slug: "emails", label: "Email addresses", group: "account" as const }] : []),
    ...(me ? [{ slug: "password", label: "Password", group: "account" as const }] : []),
  ];
}
