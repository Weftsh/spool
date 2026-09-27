import { formatAgo } from "@/format";

/// The absolute instant, in the *viewer's* zone.
///
/// The dashboard has already been bitten once by a date read as UTC
/// midnight rather than the reader's own day, and the fix is the same
/// every time: never build a calendar day by hand out of an epoch.
/// `toLocaleString` with no locale argument uses the browser's, which
/// is the one the reader set.
export function absoluteTime(at: number): string {
  return new Date(at).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

/// The machine-readable half. A `<time>` without a `dateTime` is a
/// `<span>` with extra letters.
export function isoTime(at: number): string {
  return new Date(at).toISOString();
}

/// "5d ago", with the exact timestamp one hover away.
///
/// Every relative time in the forge carries both: a maintainer triaging
/// a queue reads the relative form, and anybody arguing about what
/// happened when needs the absolute one without leaving the page.
export function RelativeTime(props: { at: number | null; className?: string }) {
  if (props.at == null) {
    return <span className={props.className}>never</span>;
  }
  return (
    <time
      dateTime={isoTime(props.at)}
      title={absoluteTime(props.at)}
      className={props.className}
    >
      {formatAgo(props.at)}
    </time>
  );
}
