import { useState } from "react";
import { api } from "@/api";
import { Button } from "@/components/ui/button";

/// The one thing an unconfirmed account needs to see.
///
/// Without it the server's refusal — "confirm your email address before
/// creating anything" — has nowhere to lead: the message may be lost, the
/// address may have a typo in it, and the only cure lives in an endpoint
/// nothing on screen calls.
export function ConfirmBanner(props: { email: string }) {
  const [sent, setSent] = useState(false);
  const [busy, setBusy] = useState(false);
  return (
    <div
      className="mb-6 flex flex-wrap items-center gap-3 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-sm"
      role="status"
    >
      <span className="min-w-0 flex-1">
        {sent ? (
          <>
            Another confirmation link is on its way to{" "}
            <strong>{props.email}</strong>. The previous one no longer works.
          </>
        ) : (
          <>
            Confirm <strong>{props.email}</strong> to create repositories. You
            can look around in the meantime.
          </>
        )}
      </span>
      <Button
        variant="outline"
        size="xs"
        disabled={busy || sent}
        onClick={async () => {
          setBusy(true);
          // The endpoint answers the same way for an address that is
          // already confirmed and one that has no account, so there is
          // nothing here worth reporting as a failure.
          await api.resendVerification(props.email).catch(() => undefined);
          setBusy(false);
          setSent(true);
        }}
      >
        {busy ? "Sending…" : sent ? "Sent" : "Send it again"}
      </Button>
    </div>
  );
}
