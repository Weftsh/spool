// Display formatting: compact, unambiguous, unit-tested.

export function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "–";
  if (n < 1024) return `${n} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let v = n;
  let u = -1;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return `${v >= 100 ? Math.round(v) : v.toFixed(1)} ${units[u]}`;
}

export function formatMs(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return "–";
  if (ms < 1000) return `${ms} ms`;
  return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} s`;
}

export function formatCount(n: number): string {
  if (!Number.isFinite(n)) return "–";
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

/** Compact an OpenSSH `SHA256:…` fingerprint for table cells: keep the
 * hash-kind prefix plus the first and last characters of the digest —
 * enough to compare against `ssh-keygen -lf` at a glance. */
export function formatFingerprint(fp: string): string {
  const [kind, digest] = fp.includes(":") ? fp.split(/:(.*)/s, 2) : ["", fp];
  if (!digest || digest.length <= 16) return fp;
  const head = digest.slice(0, 8);
  const tail = digest.slice(-4);
  return `${kind ? `${kind}:` : ""}${head}…${tail}`;
}

export function formatAgo(ms: number | null | undefined): string {
  if (ms == null) return "never";
  const delta = Date.now() - ms;
  if (delta < 0) return "just now";
  const s = Math.floor(delta / 1000);
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

/** The countdown to an instant that is still ahead — an invitation's
 * expiry, a token's lifetime. `formatAgo` is the wrong tool for these:
 * it reads a future timestamp as "just now", which is how a seven-day
 * invitation came to be listed as expiring the moment it was sent. Past
 * the instant it just says so; nobody needs to know by how much. */
export function formatIn(ms: number | null | undefined): string {
  if (ms == null) return "never";
  const delta = ms - Date.now();
  if (delta < 0) return "expired";
  const s = Math.floor(delta / 1000);
  if (s < 60) return `in ${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `in ${m}m`;
  const h = Math.floor(m / 60);
  if (h < 48) return `in ${h}h`;
  return `in ${Math.floor(h / 24)}d`;
}

/// The day a commit was authored, for grouping a history list.
///
/// Local time, deliberately: a commit list is read as "what happened,
/// and when" by a person sitting in one timezone, and a date filter that
/// read a picked day as UTC midnight has already been a defect on this
/// dashboard once. The year is dropped when it is this one, because
/// "Aug 26" is what somebody scanning a recent history needs and the
/// year is noise until it is not.
export function formatDay(ms: number): string {
  const d = new Date(ms);
  const now = new Date();
  return d.toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
    ...(d.getFullYear() === now.getFullYear() ? {} : { year: "numeric" }),
  });
}
