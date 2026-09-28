import { useEffect } from "react";
import { type Me, type Session } from "@/api";
import { settingsSections } from "@/lib/settings-sections";
import { ActivityPanel } from "@/views/settings/activity";
import { EmailsPanel } from "@/views/settings/emails";
import { MembersPanel } from "@/views/settings/members";
import { PasswordPanel } from "@/views/settings/password";
import { RunnersPanel } from "@/views/settings/runners";
import { SshKeysPanel } from "@/views/settings/ssh-keys";
import { TeamsPanel } from "@/views/settings/teams";
import { TokensPanel } from "@/views/settings/tokens";

/// One settings section per page, addressed as /settings/:section. The
/// sections a viewer sees are the ones they can act on (the sidebar
/// lists the same set, from the same helper). A missing, unknown, or
/// hidden section corrects the URL to the first visible one with
/// replace — the bad address never enters history.
export function SettingsView(props: {
  session: Session;
  me: Me | null;
  /// Whether this server takes passwords. Without them there is no
  /// Password section to open, and a link to one lands on the first
  /// section instead.
  passwords: boolean;
  section: string | undefined;
  navigate: (to: string, replace?: boolean) => void;
}) {
  const { session, me } = props;
  const role = me?.orgs.find((o) => o.name === session.org)?.role;
  const admin = role === "owner" || role === "admin" || me === null;
  const sections = settingsSections(me, session.org, props.passwords);
  const current = sections.find((s) => s.slug === props.section);

  const fallback = sections[0].slug;
  const { navigate } = props;
  useEffect(() => {
    if (!current) navigate(`/settings/${fallback}`, true);
  }, [current, fallback, navigate]);
  if (!current) return null;

  switch (current.slug) {
    case "members":
      return <MembersPanel session={session} me={me} />;
    case "runners":
      return <RunnersPanel session={session} />;
    case "teams":
      return <TeamsPanel session={session} isAdmin={admin} />;
    case "activity":
      return <ActivityPanel session={session} isAdmin={admin} />;
    case "tokens":
      return <TokensPanel session={session} role={role} isAdmin={admin} />;
    case "ssh-keys":
      return <SshKeysPanel session={session} isAdmin={admin} />;
    case "emails":
      return <EmailsPanel session={session} me={me} />;
    case "password":
      return <PasswordPanel />;
    default:
      return null;
  }
}
