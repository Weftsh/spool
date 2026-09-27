import { Alert } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";

/// Revoked credentials are history, not inventory. They stay reachable —
/// "did I revoke that key?" is a real question — but they do not get to
/// dominate the table you came here to read.
export function RevokedToggle(props: {
  count: number;
  shown: boolean;
  onToggle: () => void;
}) {
  if (props.count === 0) return null;
  return (
    <button
      type="button"
      className="mb-4 text-xs text-ink-3 underline-offset-2 hover:text-ink-2 hover:underline"
      onClick={props.onToggle}
    >
      {props.shown ? "Hide" : "Show"} {props.count} revoked
    </button>
  );
}

export function Status(props: { revoked: boolean }) {
  return props.revoked ? (
    <Badge variant="serious">revoked</Badge>
  ) : (
    <Badge variant="good">active</Badge>
  );
}

export function Err(props: { message: string | null }) {
  if (!props.message) return null;
  return (
    <p className="mb-3 text-sm text-serious" role="alert">
      {props.message}
    </p>
  );
}

export function Loading() {
  return <div className="py-20 text-center text-sm text-ink-3">Loading…</div>;
}

export function ErrorBox(props: { message: string }) {
  return <Alert className="rounded-lg p-4 text-serious">{props.message}</Alert>;
}
