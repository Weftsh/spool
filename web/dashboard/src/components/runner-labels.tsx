import { cn } from "@/lib/utils";

/// A `runs-on` label, as a chip.
///
/// One component and not two inline spellings, because the same tokens
/// appear in two places that must read as the same vocabulary: a
/// runner's labels in Settings → Runners, and the labels a job asked
/// for on the run page. An operator matching one against the other is
/// doing the routing rule in their head — `job.labels ⊆ runner.labels`
/// — and two chip styles would make that harder than it already is.
///
/// Mono and un-tinted, unlike `LabelPill`: an issue label is a name a
/// person chose and a runner label is a token a matcher compares
/// literally, so the difference between `gpu` and `gpu ` is worth
/// seeing.
export function RunnerLabels(props: {
  labels: readonly string[];
  className?: string;
}) {
  return (
    <span className={cn("flex flex-wrap gap-1", props.className)}>
      {props.labels.map((l) => (
        <span
          key={l}
          className="inline-flex items-center rounded-full border border-borderline bg-surface-2 px-2 py-0.5 font-mono text-xs text-ink-2"
        >
          {l}
        </span>
      ))}
    </span>
  );
}
