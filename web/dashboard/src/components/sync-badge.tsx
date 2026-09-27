import { Badge } from "@/components/ui/badge";

export function SyncBadge(props: {
  error: string | null;
  lastSync: number | null;
}) {
  if (props.error) {
    return (
      <Badge variant="serious" className="gap-1.5">
        <span aria-hidden>▲</span> origin unreachable
      </Badge>
    );
  }
  if (props.lastSync == null) {
    return (
      <Badge className="gap-1.5">
        <span aria-hidden>◌</span> not synced yet
      </Badge>
    );
  }
  return (
    <Badge variant="good" className="gap-1.5">
      <span aria-hidden>●</span> healthy
    </Badge>
  );
}
