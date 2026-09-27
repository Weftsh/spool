import type { ReactNode } from "react";
import { AppSidebar } from "@/components/app-sidebar";
import type { Me } from "@/api";
import type { SettingsSection } from "@/lib/settings-sections";
import {
  SidebarInset,
  SidebarProvider,
  SidebarTrigger,
} from "@/components/ui/sidebar";
import { Toaster } from "@/components/ui/sonner";

/// The chrome the signed-in dashboard wears: a collapsible rail on the
/// left, a breadcrumb, and a deliberately narrow content column.
///
/// Lifted out of `App.tsx` unchanged when the forge arrived. The
/// two shells are split by *address space* rather than by page, and that
/// is the point: `AppSidebar` mounts only under `/dashboard`, so every
/// contract encoded in it — the DOM order that keeps its search input
/// `.first()`, the absence of repository names in the rail, the rule
/// that nothing may carry the accessible name "Settings" — stays true
/// without a single edit to that file, and every spec that asserts on
/// those contracts keeps visiting the addresses it always visited.
///
/// The narrow column is not an oversight either. `web/DESIGN.md` gives
/// the dashboard `max-w-5xl px-5` and says not to mix it with the
/// marketing width; the forge shell has its own, wider container for
/// pages that carry a file tree and an About panel side by side.
export function AdminShell(props: {
  me: Me | null;
  org: string;
  sections: SettingsSection[];
  currentPath: string;
  /// Rendered after the namespace, separated by slashes. The namespace
  /// itself is always first and is not passed in.
  crumbs: string[];
  defaultSidebarOpen: boolean;
  onSwitchOrg: (v: string) => void;
  onNavigate: (to: string) => void;
  onSignOut: () => void;
  children: ReactNode;
}) {
  return (
    <SidebarProvider defaultOpen={props.defaultSidebarOpen}>
      {/* Sidebar-before-inset DOM order is a test contract: it keeps the
          sidebar's "Search repositories" input as .first() and the search
          view's as .last() in both suites. */}
      <AppSidebar
        me={props.me}
        org={props.org}
        sections={props.sections}
        currentPath={props.currentPath}
        onSwitchOrg={props.onSwitchOrg}
        onNavigate={props.onNavigate}
        onSignOut={props.onSignOut}
      />
      <SidebarInset className="min-w-0">
        <div className="mx-auto w-full max-w-5xl px-5 py-6">
          <Toaster />
          <header className="mb-6 flex items-center gap-3">
            <SidebarTrigger aria-label="Toggle sidebar" />
            <span className="font-medium">{props.org}</span>
            {props.crumbs.map((c) => (
              <span key={c} className="flex items-center gap-3">
                <span className="text-ink-3">/</span>
                <span className="font-medium">{c}</span>
              </span>
            ))}
          </header>
          {props.children}
        </div>
      </SidebarInset>
    </SidebarProvider>
  );
}
