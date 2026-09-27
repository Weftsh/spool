import type { ReactNode } from "react";
import type { Me } from "@/api";
import { GlobalHeader } from "@/components/global-header";
import { Toaster } from "@/components/ui/sonner";
import { cn } from "@/lib/utils";
import { FORGE_CONTAINER } from "@/lib/links";

/// The public half of the product wears this: a global header over a
/// wide centered column, and **no left rail at all**.
///
/// The split from `AdminShell` is by address space, not by feel, and
/// that is what keeps the existing contracts alive: `AppSidebar` mounts
/// only under `/dashboard/*`, so its DOM-order contract, its lack of
/// repository names and the rule that nothing carries the bare
/// accessible name "Settings" all stay true without a line changing
/// there. This shell has two names it must get right for the same
/// reason, and both are noted where they are spelled — the search
/// input's "Search Weft" and the account menu's "Your settings".
///
/// It renders for a signed-out visitor. That is the normal case here,
/// not a degraded one: a repository page is the address every README
/// badge points at, and a stranger following one must get the page
/// rather than a sign-in wall.

export function ForgeContainer(props: {
  className?: string;
  children: ReactNode;
}) {
  return (
    <div className={cn(FORGE_CONTAINER, props.className)}>{props.children}</div>
  );
}

export function ForgeShell(props: {
  me: Me | null;
  /// The address bar's own path, verbatim — the header needs it to send
  /// a signed-out visitor back where they were after signing in.
  currentPath: string;
  /// The router's own `navigate`, passed straight through — absolute,
  /// in the browser's address space, not dashboard-relative. It is
  /// named for what it is rather than as an `onX` callback for the
  /// same reason `AdminShell` takes `onNavigate`: that one is a
  /// handler the shell's own chrome calls, this one is the mount's
  /// navigator, and a shell at the root has no base to add.
  navigate: (to: string, replace?: boolean) => void;
  onSignOut: () => void;
  /// Rendered full-bleed between the header and the column: the page's
  /// identity block and its `TabStrip`. It is a slot rather than part
  /// of `children` because the strip's hairline runs the full width of
  /// the viewport while its tabs stay inside the column, which cannot
  /// be expressed from inside the container without a negative margin —
  /// and a negative margin is exactly how you grow
  /// `documentElement.scrollWidth` by accident.
  masthead?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="min-h-svh bg-surface-0">
      <Toaster />
      <GlobalHeader
        me={props.me}
        currentPath={props.currentPath}
        onNavigate={props.navigate}
        onSignOut={props.onSignOut}
      />
      {props.masthead}
      <main className={cn(FORGE_CONTAINER, "py-6")}>{props.children}</main>
    </div>
  );
}
