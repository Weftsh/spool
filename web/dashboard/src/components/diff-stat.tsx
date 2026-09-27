/// `+N −M` for one file, in mono, with the meaning in words.
///
/// The glyphs are decoration — a `+` in emerald says "added" to a person
/// looking at it and nothing at all to anything that strips markup — so
/// they are `aria-hidden` and the sr-only sentence carries the number.
/// `hidden` is for a rail, where the same two numbers are repeated
/// beside a row whose accessible name is the file's segment: reading
/// them twice per file adds nothing, and the authoritative labelled copy
/// is on the file row in the list.
export function DiffStat(props: {
  stat: { added: number; deleted: number };
  hidden?: boolean;
}) {
  return (
    <span
      aria-hidden={props.hidden || undefined}
      className="shrink-0 font-mono text-xs"
      title={`${props.stat.added} added, ${props.stat.deleted} removed`}
    >
      {!props.hidden && (
        <span className="sr-only">
          {props.stat.added} added, {props.stat.deleted} removed
        </span>
      )}
      <span aria-hidden className="text-good">
        +{props.stat.added}
      </span>{" "}
      <span aria-hidden className="text-serious">
        −{props.stat.deleted}
      </span>
    </span>
  );
}
