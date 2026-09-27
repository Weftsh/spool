import { cn } from "@/lib/utils";

export function StatTile(props: {
  label: string;
  value: string;
  sub?: string;
  /// Reserve two lines for the label, so that in a row of tiles where
  /// one label wraps ("Private transfer today") every value still sits
  /// on the same line as its neighbours'.
  twoLineLabel?: boolean;
}) {
  return (
    <div className="rounded-lg border border-borderline bg-surface-1 p-4">
      <div
        className={cn(
          "text-xs font-medium uppercase tracking-wider text-ink-3",
          props.twoLineLabel && "min-h-[2lh]",
        )}
      >
        {props.label}
      </div>
      <div className="mt-1 font-mono text-2xl font-semibold tracking-tight text-ink">
        {props.value}
      </div>
      {props.sub && (
        <div className="mt-0.5 text-xs text-ink-2">{props.sub}</div>
      )}
    </div>
  );
}
