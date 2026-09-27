// The prices, in one place, for both apps.
//
// The server quotes the same figures to the dashboard from the
// `DEFAULT_*` constants in `crates/stratum-server/src/api/billing_api.rs`
// (overridable per fleet by the `STRATUM_*` variables `docs/operations.md`
// lists), and the marketing site renders them from here. Nothing compares
// the two at runtime, so `docs_e2e::the_site_quotes_the_prices_the_server_defaults_to`
// parses this file's `key: value` lines and insists each integer equals
// the server's constant. Keep the object literal flat and the values
// plain integers for that reason.
//
// Nothing here imports a framework: the same functions run in a vitest
// process, at Astro build time, and in the browser behind the pricing
// page's estimator.

export const PRICING = {
  // Cents per person per month for private work.
  seatCents: 400,
  // Hosted minutes a namespace that pays nothing may use per rolling 30 days.
  freeMinutes: 500,
  // What each paid seat adds to the organization's pool per billing period.
  minutesPerSeat: 1000,
  egressGbPerSeat: 10,
  storageGbPerSeat: 5,
  packagesGbPerSeat: 2,
  // Past the pool. Minutes are priced per thousand because $0.008 does
  // not fit in whole cents.
  overageCentsPer1000Minutes: 800,
  overageCentsPerGbEgress: 10,
  overageCentsPerGbMonthStorage: 10,
  overageCentsPerGbMonthPackages: 15,
  // A new organization's spend limit: use past the pool is refused
  // until an owner or admin raises it.
  defaultSpendLimitCents: 0,
} as const;

export interface EstimateInput {
  seats: number;
  /// Hosted-runner minutes in the period.
  minutes: number;
  /// Gigabytes transferred out of private repositories in the period.
  egressGb: number;
  /// Gigabytes stored in private repositories, averaged over the period.
  storageGb: number;
  /// Gigabytes of packages, averaged over the period.
  packagesGb: number;
}

export interface Estimate {
  seatsCents: number;
  pool: { minutes: number; egressGb: number; storageGb: number; packagesGb: number };
  over: { minutes: number; egressGb: number; storageGb: number; packagesGb: number };
  overageCents: { minutes: number; egress: number; storage: number; packages: number };
  totalCents: number;
  withinPool: boolean;
}

/// The worked example the pricing page renders and its tests pin:
/// five seats, past the pool on minutes only, $28 a month.
export const EXAMPLE: EstimateInput = {
  seats: 5,
  minutes: 6000,
  egressGb: 40,
  storageGb: 20,
  packagesGb: 5,
};

/// A number as typed by a person: NaN, negatives and infinities are 0.
function quantity(n: number): number {
  return Number.isFinite(n) && n > 0 ? n : 0;
}

/// What a month costs at these numbers, in integer cents. Seats are at
/// least one — an organization with nobody in it is not a thing that
/// pays — and every other input is clamped at zero.
export function estimate(input: EstimateInput): Estimate {
  const seats = Math.max(1, Math.round(quantity(input.seats)));
  const minutes = quantity(input.minutes);
  const egressGb = quantity(input.egressGb);
  const storageGb = quantity(input.storageGb);
  const packagesGb = quantity(input.packagesGb);

  const pool = {
    minutes: seats * PRICING.minutesPerSeat,
    egressGb: seats * PRICING.egressGbPerSeat,
    storageGb: seats * PRICING.storageGbPerSeat,
    packagesGb: seats * PRICING.packagesGbPerSeat,
  };
  const over = {
    minutes: Math.max(0, minutes - pool.minutes),
    egressGb: Math.max(0, egressGb - pool.egressGb),
    storageGb: Math.max(0, storageGb - pool.storageGb),
    packagesGb: Math.max(0, packagesGb - pool.packagesGb),
  };
  const overageCents = {
    minutes: Math.round((over.minutes * PRICING.overageCentsPer1000Minutes) / 1000),
    egress: Math.round(over.egressGb * PRICING.overageCentsPerGbEgress),
    storage: Math.round(over.storageGb * PRICING.overageCentsPerGbMonthStorage),
    packages: Math.round(over.packagesGb * PRICING.overageCentsPerGbMonthPackages),
  };
  const seatsCents = seats * PRICING.seatCents;
  const overage =
    overageCents.minutes + overageCents.egress + overageCents.storage + overageCents.packages;
  return {
    seatsCents,
    pool,
    over,
    overageCents,
    totalCents: seatsCents + overage,
    withinPool: overage === 0,
  };
}

/// "1,000" — grouped, no locale involved, so a build machine set to
/// de_DE renders the same page as everybody else.
export function num(n: number): string {
  const whole = Math.round(quantity(n));
  return String(whole).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

/// A price as the page says it: "$4" and "$20" when it is whole dollars,
/// "$8.00" and "$0.10" when it is not — a computed amount keeps its
/// cents so it never looks rounded.
export function money(cents: number): string {
  const c = Math.round(quantity(cents));
  if (c % 100 === 0) return `$${num(c / 100)}`;
  return moneyCents(c);
}

/// A price with its cents always shown: "$8.00", "$0.10". The estimator
/// uses it for everything past the pool, so a line of amounts lines up.
export function moneyCents(cents: number): string {
  const c = Math.round(quantity(cents));
  const dollars = Math.floor(c / 100);
  const rest = c % 100;
  return `$${num(dollars)}.${rest < 10 ? "0" : ""}${rest}`;
}

/// The per-minute rate past the pool — "$0.008" — which does not fit in
/// whole cents and so is the one price not said by `money`.
export function rateMinute(): string {
  return `$${(PRICING.overageCentsPer1000Minutes / 100_000).toFixed(3)}`;
}
