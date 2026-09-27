import {
  Activity,
  FolderGit2,
  KeyRound,
  Layers,
  Lock,
  LogOut,
  Mail,
  Server,
  Terminal,
  Users,
  UsersRound,
} from "lucide-react";
import { type Me } from "@/api";
import { href } from "@/router";
import { type SettingsSection } from "@/lib/settings-sections";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
} from "@/components/ui/sidebar";

/// Everything about who can create another organization lives in the
/// switcher: creating one is an entry in the list somebody is already
/// looking at when they think "I need another namespace" — a button
/// elsewhere would be a button nobody finds.
export const NEW_ORG = "+new";

/// One icon per section, and no two alike. Runners and Email addresses
/// used to fall through to the Members icon, so the collapsed rail showed
/// three identical people glyphs and nothing to tell them apart but a
/// hover. `tests/design-audit.spec.ts` holds the rail to distinct icons.
const SECTION_ICONS: Record<string, typeof Users> = {
  members: Users,
  runners: Server,
  emails: Mail,
  teams: UsersRound,
  activity: Activity,
  tokens: KeyRound,
  "ssh-keys": Terminal,
  password: Lock,
};

/// The left rail: org switcher, search, the screens, and the role-gated
/// settings sections as real links. Collapses to an icon rail — hidden
/// content uses display:none (the walkthrough's offscreen audit fails
/// anything positioned off-canvas). It deliberately lists no repository
/// names (the screenshots spec matches them with exact text) and has no
/// item named "New repository" (that button lives in the org overview
/// and must stay unique).
export function AppSidebar(props: {
  me: Me | null;
  org: string;
  sections: SettingsSection[];
  currentPath: string;
  onSwitchOrg: (value: string) => void;
  onNavigate: (to: string) => void;
  onSignOut: () => void;
}) {
  const { me, org, sections, currentPath } = props;

  return (
    <Sidebar collapsible="icon">
      <SidebarHeader>
        <div className="flex items-center gap-2 px-1 pt-1">
          <svg width="22" height="22" viewBox="0 0 32 32" aria-hidden>
            <rect
              x="4"
              y="6"
              width="24"
              height="5"
              rx="2"
              fill="var(--brand)"
            />
            <rect
              x="4"
              y="14"
              width="24"
              height="5"
              rx="2"
              fill="var(--accent)"
            />
            <rect
              x="4"
              y="22"
              width="24"
              height="5"
              rx="2"
              fill="var(--series-3)"
            />
          </svg>
          <span className="font-semibold tracking-tight group-data-[collapsible=icon]:hidden">
            Weft
          </span>
        </div>
        <div className="group-data-[collapsible=icon]:hidden">
          {me ? (
            <Select value={org} onValueChange={props.onSwitchOrg}>
              <SelectTrigger
                aria-label="Organization"
                className="w-full px-2 py-1 font-medium"
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {me.orgs.map((o) => (
                  <SelectItem key={o.id} value={o.name}>
                    {o.name}
                  </SelectItem>
                ))}
                <SelectItem value={NEW_ORG}>+ New organization…</SelectItem>
              </SelectContent>
            </Select>
          ) : (
            <SidebarMenu>
              <SidebarMenuItem>
                {/* Token sessions have no org list; the org name is the
                    "back to the overview" control, and must stay the
                    only control with that accessible name. */}
                <SidebarMenuButton
                  className="font-medium"
                  onClick={() => props.onNavigate("/")}
                >
                  {org}
                </SidebarMenuButton>
              </SidebarMenuItem>
            </SidebarMenu>
          )}
        </div>
      </SidebarHeader>
      <SidebarContent>
        <SidebarGroup className="group-data-[collapsible=icon]:hidden">
          <SidebarGroupContent>
            <form
              role="search"
              onSubmit={(e) => {
                e.preventDefault();
                const q = new FormData(e.currentTarget).get("q");
                props.onNavigate(
                  href(["search"], { q: String(q ?? "").trim() }),
                );
              }}
            >
              <label className="sr-only" htmlFor="sidebar-search">
                Search repositories
              </label>
              <Input
                id="sidebar-search"
                name="q"
                type="search"
                autoComplete="off"
                maxLength={128}
                placeholder="Search repositories"
                className="py-1"
              />
            </form>
          </SidebarGroupContent>
        </SidebarGroup>
        <SidebarGroup>
          <SidebarGroupContent>
            <SidebarMenu>
              <SidebarMenuItem>
                <SidebarMenuButton
                  asChild
                  isActive={currentPath === "/"}
                  tooltip="Repositories"
                >
                  <a
                    href="/dashboard/"
                    onClick={(e) => {
                      e.preventDefault();
                      props.onNavigate("/");
                    }}
                  >
                    <FolderGit2 aria-hidden />
                    <span>Repositories</span>
                  </a>
                </SidebarMenuButton>
              </SidebarMenuItem>
              {/* Active on the list and on a changeset within it: the
                  detail address is `/changesets/<key>`, and a sidebar
                  that lit up only on the list would read as "you have
                  left" the moment somebody opened one. */}
              <SidebarMenuItem>
                <SidebarMenuButton
                  asChild
                  isActive={
                    currentPath === "/changesets" ||
                    currentPath.startsWith("/changesets/")
                  }
                  tooltip="Changesets"
                >
                  <a
                    href="/dashboard/changesets"
                    onClick={(e) => {
                      e.preventDefault();
                      props.onNavigate("/changesets");
                    }}
                  >
                    <Layers aria-hidden />
                    <span>Changesets</span>
                  </a>
                </SidebarMenuButton>
              </SidebarMenuItem>
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
        {/* Two groups, by whose setting it is: the organization's, which
            change things for everybody in it, and the person's own.
            Deliberately non-interactive labels: nothing in the shell may
            carry the accessible name "Settings", or a stale test selector
            would silently keep matching. */}
        {(
          [
            ["org", "Organization"],
            ["account", "Your account"],
          ] as const
        ).map(([group, label]) => {
          const inGroup = sections.filter((s) => s.group === group);
          if (inGroup.length === 0) return null;
          return (
            <SidebarGroup key={group}>
              <SidebarGroupLabel>{label}</SidebarGroupLabel>
              <SidebarGroupContent>
                <SidebarMenu>
                  {inGroup.map((s) => {
                    const Icon = SECTION_ICONS[s.slug] ?? Users;
                    const to = `/settings/${s.slug}`;
                    return (
                      <SidebarMenuItem key={s.slug}>
                        <SidebarMenuButton
                          asChild
                          isActive={currentPath === to}
                          tooltip={s.label}
                        >
                          <a
                            href={`/dashboard${to}`}
                            onClick={(e) => {
                              e.preventDefault();
                              props.onNavigate(to);
                            }}
                          >
                            <Icon aria-hidden />
                            <span>{s.label}</span>
                          </a>
                        </SidebarMenuButton>
                      </SidebarMenuItem>
                    );
                  })}
                </SidebarMenu>
              </SidebarGroupContent>
            </SidebarGroup>
          );
        })}
      </SidebarContent>
      <SidebarFooter>
        {me && (
          <div
            className="truncate px-2 text-sm text-ink-3 group-data-[collapsible=icon]:hidden"
            title={me.email}
          >
            {me.name}
          </div>
        )}
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton onClick={props.onSignOut} tooltip="Sign out">
              <LogOut aria-hidden />
              <span>Sign out</span>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarFooter>
    </Sidebar>
  );
}
