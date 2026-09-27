import * as React from "react";
import { Toaster as Sonner, type ToasterProps } from "sonner";

// Toasts. theme="system" matches the dashboard's theming (color-scheme
// from the visitor's preference — no next-themes here); the CSS
// variables map Sonner's surfaces onto the Weft tokens.
function Toaster(props: ToasterProps) {
  return (
    <Sonner
      theme="system"
      // Not the library's bottom-right default. Every screen on this
      // product puts its decision rail in the right-hand column — the
      // approvals card, the Land/Revert/Abandon actions — and a toast
      // there covers it: a manual pass caught "Deleted squad-…", raised
      // by a delete on another screen, sitting on top of "Approvals on
      // patchset 1". A toast is an announcement about something that
      // already happened; it must never be over the controls somebody
      // is deciding with. Top-centre is clear of both rails and of the
      // sidebar.
      position="top-center"
      className="toaster group"
      style={
        {
          "--normal-bg": "var(--popover)",
          "--normal-text": "var(--popover-foreground)",
          "--normal-border": "var(--border)",
          "--success-bg": "var(--popover)",
          "--success-text": "var(--status-good)",
          "--error-bg": "var(--popover)",
          "--error-text": "var(--status-serious)",
        } as React.CSSProperties
      }
      {...props}
    />
  );
}

export { Toaster };
