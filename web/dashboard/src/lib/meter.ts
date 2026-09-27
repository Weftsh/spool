/// A pool of something metered — hosted minutes, private transfer,
/// private storage — as a billing page has to say it.
///
/// Three states, and the difference between two of them is the whole
/// reason this is a function and not a subtraction in a template:
///
///   * **unmetered** — the deployment sets no pool, so there is nothing
///     to report. Null, not zero.
///   * **metered** — a pool, a figure spent, and what is left.
///   * **exhausted** — the pool is spent. What happens next is the
///     server's call, not ours: past the pool a paying organization is
///     metered until its spend limit, and only the server knows whether
///     that limit has been reached. So `refusing` is a flag it sends,
///     never something read off the numbers.
///
/// Rendering an unmetered deployment as "0 minutes left" would tell a
/// whole organisation its CI is dead when nothing is wrong, and it is
/// the mistake a page makes by reaching for `?? 0`. So the reading is
/// done once, here, where it is answerable without a browser.
import type { Billing, BillingMeter, BillingRates } from "@/api";

export type Meter =
  | { kind: "unmetered" }
  | {
      kind: "metered";
      /// What the seats bring. Minutes are whole; gigabytes are decimal.
      included: number;
      used: number;
      remaining: number;
      /// How far past the pool. Zero while inside it.
      overage: number;
      /// What that overage would cost so far, in cents, by the server's
      /// arithmetic.
      estimatedCents: number;
      /// Nothing left in the pool. Named rather than left to the reader
      /// of `remaining === 0`, because it is the state that changes what
      /// the page *says*.
      exhausted: boolean;
      /// The server is refusing at this pool's edge right now — the
      /// spend limit is reached, or there is none. **Never derived**:
      /// the refusal a reader is looking at was decided by the server.
      refusing: boolean;
      /// 0–100, clamped. Only for the bar; the words come from the
      /// numbers themselves.
      percent: number;
    };

/// The old name for the minutes meter, kept so the legacy adapter reads
/// the way it always did.
export type CiMinutes = Meter;

export type MeterKind = "minutes" | "egress" | "storage" | "packages";

/// One pool, from the server's own figures.
///
/// `included: null` — and an absent meter altogether — is unmetered.
/// A pool of zero is metered and empty, which is the case a `!included`
/// test would silently fold into "unmetered", the opposite answer.
export function readMeter(m: BillingMeter | null | undefined): Meter {
  if (m === null || m === undefined) return { kind: "unmetered" };
  const included = m.included;
  if (included === null || included === undefined) return { kind: "unmetered" };
  const used = Math.max(0, m.used ?? 0);
  // The server's own figure when it sends one: the arithmetic a refusal
  // runs on must be the arithmetic the page shows, or a reader sees
  // "120 left" beside a run that was refused for having none.
  const remaining = Math.max(0, m.remaining ?? Math.max(0, included - used));
  const overage = Math.max(0, m.overage ?? Math.max(0, used - included));
  return {
    kind: "metered",
    included,
    used,
    remaining,
    overage,
    estimatedCents: Math.max(0, m.estimated_cents ?? 0),
    exhausted: remaining === 0,
    refusing: m.refusing === true,
    percent:
      included === 0 ? 100 : Math.min(100, Math.round((used / included) * 100)),
  };
}

/// The minutes meter, from whichever shape the server sent.
///
/// A server with the pools sends `meters.minutes`; an older one sends
/// only `ci_minutes_*`, and that shape is read exactly as it always was
/// — its `refusing` is its exhaustion, because the old server refused
/// at the edge of the budget and had no spend limit to meter past.
export function readCiMinutes(billing: Billing): CiMinutes {
  if (billing.meters !== undefined) return readMeter(billing.meters.minutes);
  const limit = billing.ci_minutes_limit;
  if (limit === null || limit === undefined) return { kind: "unmetered" };
  const used = Math.max(0, billing.ci_minutes_used ?? 0);
  const remaining = Math.max(
    0,
    billing.ci_minutes_remaining ?? Math.max(0, limit - used),
  );
  return {
    kind: "metered",
    included: limit,
    used,
    remaining,
    overage: Math.max(0, used - limit),
    estimatedCents: 0,
    exhausted: remaining === 0,
    refusing: remaining === 0,
    percent:
      limit === 0 ? 100 : Math.min(100, Math.round((used / limit) * 100)),
  };
}

/// The three pools in the order the page draws them, only those the
/// server meters. Empty on a server without `meters`.
export function readMeters(
  billing: Billing,
): Array<{ kind: MeterKind; meter: Meter & { kind: "metered" } }> {
  const ms = billing.meters;
  if (ms === undefined) return [];
  const out: Array<{ kind: MeterKind; meter: Meter & { kind: "metered" } }> =
    [];
  const push = (kind: MeterKind, raw: BillingMeter | undefined) => {
    const meter = readMeter(raw);
    if (meter.kind === "metered") out.push({ kind, meter });
  };
  push("minutes", ms.minutes);
  push("egress", ms.egress_gb);
  push("storage", ms.storage_gb);
  push("packages", ms.packages_gb);
  return out;
}

const n = (v: number) => v.toLocaleString("en-US");

/// `"12.4"`, `"50"`, `"0.04"` — decimal gigabytes as a person says them.
/// Whole numbers stay whole; fractions keep at most two places, because
/// a third is below what anybody is billed on.
export function gbText(gb: number): string {
  return gb.toLocaleString("en-US", { maximumFractionDigits: 2 });
}

/// Bytes in the units the pools are counted in. The server meters
/// transfer in megabytes of 1,024 KB and sells a pool GB as 1,024 of
/// them — a customer's "10 GB" is 10 GiB, in their favour — and the
/// billing panel divides the same way. So the overview's tiles and a
/// repository's "Stored" line must too: the first draft of this
/// formatted decimal gigabytes, and the manual pass read the same
/// forty gigabytes as "40 of 30 GB" on Billing and "42.9 GB" on the
/// overview. Labelled GB, not GiB, because that is the word the pool
/// and the pricing page use.
export function formatPoolBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = bytes;
  let u = -1;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return `${v >= 100 ? Math.round(v) : v.toFixed(1)} ${units[u]}`;
}

export function meterLabel(kind: MeterKind): string {
  switch (kind) {
    case "minutes":
      return "Hosted CI minutes";
    case "egress":
      return "Private transfer";
    case "storage":
      return "Private storage";
    case "packages":
      return "Package storage";
  }
}

/// `"1,240 of 5,000 minutes used this period"` — the line under the
/// heading. Both figures shown rather than only what is left: "760
/// left" alone gives a reader no idea whether that is most of the
/// period or the last of it.
///
/// `window` is the difference between a free organization's rolling
/// 30 days — no reset date to wait for, minutes free up as the jobs that
/// spent them age out — and a paying one's billing period.
export function meterLine(
  m: Meter,
  kind: MeterKind,
  window: "rolling" | "period",
): string | null {
  if (m.kind === "unmetered") return null;
  const when = window === "rolling" ? "in the last 30 days" : "this period";
  switch (kind) {
    case "minutes":
      return `${n(m.used)} of ${n(m.included)} minutes used ${when}`;
    case "egress":
      return `${gbText(m.used)} of ${gbText(m.included)} GB transferred ${when}`;
    case "storage":
      return `${gbText(m.used)} of ${gbText(m.included)} GB stored, averaged over ${
        window === "rolling" ? "the last 30 days" : "this period"
      }`;
    case "packages":
      return `${gbText(m.used)} of ${gbText(m.included)} GB of packages, averaged over ${
        window === "rolling" ? "the last 30 days" : "this period"
      }`;
  }
}

/// `"1,000 minutes past the pool, $8.00 estimated"` — only once there is
/// an overage, and always with the money beside it: a reader past the
/// pool wants to know what it is costing more than by how much.
export function meterOverageLine(m: Meter, kind: MeterKind): string | null {
  if (m.kind === "unmetered" || m.overage <= 0) return null;
  const amount =
    kind === "minutes"
      ? `${n(m.overage)} ${m.overage === 1 ? "minute" : "minutes"}`
      : `${gbText(m.overage)} GB`;
  return `${amount} past the pool, ${centsExact(m.estimatedCents)} estimated`;
}

/// `"$8.00"` — an estimate, always to the cent, because it is going to
/// be compared with an invoice.
export function centsExact(cents: number): string {
  return (cents / 100).toLocaleString("en-US", {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
}

/// `"$0.008 a minute"`, `"$0.10 a GB"`, `"$0.10 a GB-month"` — the price
/// of use past the pool, from the server's rates. `null` without them:
/// a sentence that quotes no price is better than one that quotes a
/// price nobody sent.
export function rateText(
  rates: BillingRates | null | undefined,
  kind: MeterKind,
): string | null {
  if (rates === null || rates === undefined) return null;
  const money = (cents: number) =>
    (cents / 100).toLocaleString("en-US", {
      style: "currency",
      currency: "USD",
      minimumFractionDigits: 2,
      maximumFractionDigits: 3,
    });
  switch (kind) {
    case "minutes":
      return `${money(rates.cents_per_1000_minutes / 1000)} a minute`;
    case "egress":
      return `${money(rates.cents_per_gb_egress)} a GB`;
    case "storage":
      return `${money(rates.cents_per_gb_month_storage)} a GB-month`;
    case "packages":
      return `${money(rates.cents_per_gb_month_packages)} a GB-month`;
  }
}

/// What a pool refusing at its edge means for the person reading, one
/// sentence per pool. Each names what is refused *and* what is not,
/// because "at the spend limit" on its own sends somebody checking
/// their public repositories and their own runners for a fault.
export const MINUTES_CAP_NOTE =
  "Hosted jobs are waiting at the spend limit. They start again when " +
  "minutes free up or the limit is raised; jobs on your own runners are " +
  "running as normal.";
export const EGRESS_CAP_NOTE =
  "Clones and fetches of private repositories are refused at the spend " +
  "limit. Public repositories are unaffected.";
export const STORAGE_CAP_NOTE =
  "Pushes that would grow private storage are refused at the spend " +
  "limit. Nothing already stored has been touched.";
export const PACKAGES_CAP_NOTE =
  "Publishing is refused at the spend limit. Installing what is already " +
  "published is unaffected, so builds that only resolve keep working.";

/// What is actually happening at this pool, in a sentence a person can
/// act on — or nothing, while there is nothing to say.
///
/// Refusing wins: the server's flag, never the numbers. Past the pool
/// but not refused means use is being metered, and the sentence says
/// at what price and until when.
export function meterNote(
  m: Meter,
  kind: MeterKind,
  spendLimitCents: number | null | undefined,
  rate: string | null,
): string | null {
  if (m.kind === "unmetered") return null;
  if (m.refusing) {
    switch (kind) {
      case "minutes":
        return MINUTES_CAP_NOTE;
      case "egress":
        return EGRESS_CAP_NOTE;
      case "storage":
        return STORAGE_CAP_NOTE;
      case "packages":
        return PACKAGES_CAP_NOTE;
    }
  }
  if (m.exhausted) {
    const at = rate === null ? "" : ` at ${rate}`;
    const until =
      spendLimitCents === null || spendLimitCents === undefined
        ? "until the spend limit is reached"
        : `until the spend limit of ${wholeDollars(spendLimitCents)} is reached`;
    return `Past the pool: metered${at} ${until}.`;
  }
  return null;
}

function wholeDollars(cents: number): string {
  const whole = cents % 100 === 0;
  return (cents / 100).toLocaleString("en-US", {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: whole ? 0 : 2,
    maximumFractionDigits: 2,
  });
}

/// The legacy minutes line: a rolling window, not a calendar month.
export function minutesLine(m: CiMinutes): string | null {
  return meterLine(m, "minutes", "rolling");
}

/// The legacy note, for a budget with no spend limit behind it: the
/// only way out is the window rolling.
export function minutesNote(m: CiMinutes): string | null {
  if (m.kind === "unmetered") return null;
  if (m.exhausted)
    return (
      "Hosted workflows are being refused. Minutes free up as the jobs " +
      "that spent them age out of the 30-day window; runs already going " +
      "are left alone."
    );
  return null;
}

/// How many of the minutes were spent by GitHub Actions jobs on Weft
/// runners — "of which 120 on GitHub Actions" — or `null` when the
/// sentence has nothing to add.
///
/// Nothing to add on a server older than the GitHub door (the field is
/// absent) and when nothing came through it (`0`): both organisations
/// see the minutes line exactly as they always have. A line that read
/// "of which 0 on GitHub Actions" would be advertising a feature on a
/// billing panel, and one that read it on an unmetered deployment
/// would be a share of nothing.
export function githubMinutesNote(b: Billing): string | null {
  const used = b.ci_minutes_github_used;
  if (used === null || used === undefined || !(used > 0)) return null;
  if (readCiMinutes(b).kind === "unmetered") return null;
  return `of which ${n(used)} on GitHub Actions`;
}

/// A suspension, as the billing page needs it: the operator-facing
/// reason and when it landed.
///
/// Separate from the minutes above, because they are separate facts and
/// the page had been showing only one of them. A suspended organisation
/// usually still has most of its budget — so "1,940 minutes left" was
/// the entire answer Settings → Billing gave somebody whose CI had been
/// stopped for mining, and the reason appeared only on a change page
/// they had no cause to open.
export interface CiSuspension {
  /// The server's own words, passed through untouched. It is written by
  /// an operator, or by the runner that stopped the job, and a
  /// paraphrase would be a reason nobody gave.
  reason: string;
  /// Epoch **milliseconds**, or `null` when the server sent no time.
  ///
  /// Named for its unit because the unit was got wrong once. The field
  /// reached me as "seconds" secondhand; the server stores `now_ms()`
  /// (`runner_api.rs`) and hands it back untouched, so there is nothing
  /// to convert — and converting anyway dated a 2026 suspension to the
  /// year 57000. The name is here so the next caller does not have to
  /// go and find that out.
  atMs: number | null;
}

/// What follows from a suspension, in the words a reader can act on.
///
/// It says there is no button on purpose. Clearing a suspension is an
/// operator action by SQL — there is deliberately no route for it (see
/// the OpenAPI text on `POST /v1/runner/jobs/{id}/finish`) — so a page
/// that hinted at self-service would send somebody hunting a control
/// that does not exist. And it says pushing still works, because it
/// does: the push lands, the run is created, and the run is what is
/// blocked.
export const SUSPENSION_NOTE =
  "Hosted workflows are refused for this organisation, and anything that " +
  "was running when it happened was cancelled. Pushing and landing still " +
  "work. There is no way to lift this from here — an operator has to clear " +
  "it.";

export function readCiSuspension(billing: Billing): CiSuspension | null {
  const reason = billing.ci_suspended_reason;
  // Whitespace only is absence arriving badly: it would put a panel
  // headed "Hosted CI is suspended" on the page with nothing under it.
  if (reason === null || reason === undefined || reason.trim() === "")
    return null;
  const at = billing.ci_suspended_at;
  return {
    reason,
    atMs: at === null || at === undefined ? null : at,
  };
}
