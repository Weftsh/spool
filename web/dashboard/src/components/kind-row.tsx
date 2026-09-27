import { formatBytes, formatCount, formatMs } from "@/format";
import { TableCell, TableRow } from "@/components/ui/table";

export function KindRow(props: {
  kind: string;
  s: {
    count: number;
    bytes: number;
    p50_ms: number | null;
    p99_ms: number | null;
  };
}) {
  return (
    <TableRow>
      <TableCell className="px-3 font-mono">{props.kind}</TableCell>
      <TableCell className="px-3 text-right">
        {formatCount(props.s.count)}
      </TableCell>
      <TableCell className="px-3 text-right">
        {formatBytes(props.s.bytes)}
      </TableCell>
      <TableCell className="px-3 text-right">
        {formatMs(props.s.p50_ms)}
      </TableCell>
      <TableCell className="px-3 text-right">
        {formatMs(props.s.p99_ms)}
      </TableCell>
    </TableRow>
  );
}
