/// What an organisation's billing state means, said once.
///
/// Three states, and the page has to say something different in each —
/// not a different colour, a different *sentence*, because the three are
/// three different situations for the person reading:
///
///   * **free** — public repositories and people are free, and no card
///     has been asked for. The first private repository sends you to
///     the provider's subscription page, where the card is met.
///   * **paid** — subscribed; everything works.
///   * **past_due** — subscribed, and the last invoice did not settle.
///     Readable; nothing new that costs money; hosted CI paused for
///     private repositories.
///
/// The prices are read from the server, never typed here: the site
/// quotes one number and this page quotes the same field, so the two
/// cannot drift apart by somebody editing one of them.
import type { Billing } from "@/api";

export interface PlanState {
  label: string;
  hint: string;
  /// What the one button does. `subscribe` sends somebody to the
  /// provider's subscription page, where the card and a promotion code
  /// are typed; `portal` opens the provider's page for cards, invoices
  /// and cancellation; `settle` is the portal too, said for what it is
  /// for.
  action: "subscribe" | "portal" | "settle";
  button: string;
}

export function planState(b: Billing): PlanState {
  switch (b.plan) {
    case "paid":
      return {
        label: "Subscribed",
        hint: "Everything here works.",
        action: "portal",
        button: "Manage billing",
      };
    case "past_due":
      return {
        label: "Payment failed",
        // Says *private*, because that is what the server gates: a
        // past-due organization may still create public repositories
        // and push to everything it has (`Plan::may_hold`, `may_write`).
        // The old sentence said "creating repositories" resumes, and a
        // person reading it would not have tried the public one that
        // works — found forking a public repository into a past-due
        // organization and watching it succeed under a hint that said
        // it would not.
        hint:
          "Everything here is still readable and can still be pushed to. Creating private repositories or mirrors, and adding people, resume as soon as the invoice is settled — nothing has been deleted. Public repositories are unaffected; hosted workflows on private repositories are paused until then." +
          // Only where there is a pool to be past: a server without
          // usage billing has no such sentence to be true.
          (hasPools(b) ? " Use past the pool is not billed until then." : ""),
        action: "settle",
        button: "Settle the payment",
      };
    case "free":
      return {
        label: "Free",
        hint: `Public repositories and members are free. ${lapsedHolds(b)}${privateNeeds(b)}`,
        action: "subscribe",
        button: "Continue to checkout",
      };
  }
}

/// `"$4.00"` — cents as a price. Whole dollars drop the cents, because
/// "$4 per seat" is how a person says it and "$4.00" reads as a receipt.
export function dollars(cents: number): string {
  const whole = cents % 100 === 0;
  return (cents / 100).toLocaleString("en-US", {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: whole ? 0 : 2,
    maximumFractionDigits: 2,
  });
}

/// What a free organization is holding that it may not write to.
///
/// `free` is the plan no subscription lands on — a
/// new organization, and equally one whose subscription was cancelled
/// with three private repositories still in it. The page said "the first
/// private repository starts a subscription" to both, and to the second
/// that was false twice: the repositories exist, and subscribing is the
/// only way to write to them again. Empty when there is nothing to say,
/// so `planState` can splice it in ahead of the price.
export function lapsedHolds(b: Billing): string {
  const n = b.private_repos ?? 0;
  if (n === 0) return "";
  const what = n === 1 ? "private repository is" : "private repositories are";
  return `This organization's ${n} ${what} read-only until it subscribes again — everything in them is still readable and can be made public. `;
}

/// The sentence that says what a private repository costs, from the
/// server's numbers. Empty when nothing is for sale, so a caller can
/// drop it into a paragraph without checking first.
export function privateNeeds(b: Billing): string {
  if (b.price_per_seat_cents === null) return "";
  const seats = Math.max(1, b.billable_seats);
  const perSeat = dollars(b.price_per_seat_cents);
  const total = dollars(b.price_per_seat_cents * seats);
  const pools = seatBrings(b);
  const minutes = pools === null ? "" : ` ${pools}`;
  const opener =
    (b.private_repos ?? 0) > 0
      ? "Subscribing"
      : "The first private repository starts a subscription";
  return `${opener} at ${perSeat} per seat per month — ${total}/month for the ${seats} ${seats === 1 ? "seat" : "seats"} in use today, paid on the provider's page.${minutes}`;
}

/// Whether the server meters by pools at all. Absent fields are an
/// older server, and every sentence about pools is left unsaid on it.
export function hasPools(b: Billing): boolean {
  return b.meters !== undefined || b.spend_limit_cents !== undefined;
}

/// `"1,000 hosted CI minutes, 10 GB of transfer and 5 GB of storage for
/// private work, pooled"` — what one seat adds, from the server's
/// per-seat figures. Only the minutes on a server that sells only
/// minutes; `null` on one that sells nothing.
export function seatPools(b: Billing): string | null {
  if (b.paid_minutes_per_seat === null) return null;
  const minutes = `${b.paid_minutes_per_seat.toLocaleString("en-US")} hosted CI minutes`;
  const egress = b.paid_egress_gb_per_seat;
  const storage = b.paid_storage_gb_per_seat;
  if (
    egress === null ||
    egress === undefined ||
    storage === null ||
    storage === undefined
  )
    return null;
  return `${minutes}, ${gb(egress)} GB of transfer and ${gb(storage)} GB of storage for private work, pooled`;
}

/// The seat sentence as `privateNeeds` says it: the pools when the
/// server sells them, today's minutes-only sentence when it does not.
function seatBrings(b: Billing): string | null {
  const pools = seatPools(b);
  if (pools !== null) return `Each seat brings ${pools}.`;
  if (b.paid_minutes_per_seat === null) return null;
  return `Each seat brings ${b.paid_minutes_per_seat.toLocaleString("en-US")} hosted CI minutes a month.`;
}

function gb(v: number): string {
  return v.toLocaleString("en-US", { maximumFractionDigits: 2 });
}

/// `"25"`, `"25.50"`, `"$25"` → cents. Anything else is a sentence for
/// the person typing, never a guess: a limit is money, and rounding
/// "25.005" to what we thought they meant is how somebody agrees to a
/// figure they did not type.
export function parseSpendLimit(
  input: string,
): { cents: number } | { error: string } {
  const text = input.trim().replace(/^\$\s*/, "").replace(/,/g, "");
  if (text === "") return { error: "Enter an amount in dollars." };
  const m = /^(-)?(\d+)?(?:\.(\d*))?$/.exec(text);
  if (m === null || (m[2] === undefined && (m[3] ?? "") === ""))
    return { error: "Enter an amount in dollars, like 25 or 25.50." };
  if (m[1] === "-") return { error: "A spend limit cannot be negative." };
  const frac = m[3] ?? "";
  if (frac.length > 2) return { error: "Whole cents only." };
  const cents = Number(m[2] ?? "0") * 100 + Number(frac.padEnd(2, "0") || "0");
  if (!Number.isSafeInteger(cents))
    return { error: "Enter an amount in dollars, like 25 or 25.50." };
  return { cents };
}

/// What the spend limit means for the person reading, from the server's
/// own account of whether use past the pool is billed at all.
export function spendLimitLine(b: Billing): string {
  if (b.metering === "off")
    return "Use past the pool is not billed on this deployment: at the edge of a pool, hosted jobs wait and private transfer and storage are refused.";
  if (b.metering === "resubscribe")
    return "This subscription was made before usage billing existed, so use past the pool is refused rather than billed. Re-subscribe to set a spend limit; nothing else about the subscription changes.";
  const limit = b.spend_limit_cents;
  if (limit === null || limit === undefined)
    return "No spend limit is set. At the edge of a pool, hosted jobs wait and private transfer and storage are refused.";
  if (limit === 0)
    return "The spend limit is $0, so nothing past the pool is billed: at the edge of a pool, hosted jobs wait and private transfer and storage are refused until the limit is raised.";
  return `Up to ${dollars(limit)} of use past the pool is billed this period, on the next invoice. At the limit, hosted jobs wait and private transfer and storage are refused until it is raised.`;
}

/// What use past the pool has come to so far, in the words a person
/// compares with the invoice.
export function overageLine(b: Billing): string {
  const cents = b.overage_estimated_cents ?? 0;
  if (cents <= 0) return "Nothing past the pool so far this period.";
  return `${dollarsExact(cents)} estimated so far this period, from use past the pool. The invoice is the truth; this is the same arithmetic, shown before it arrives.`;
}

/// `"$8.00"` — an estimate, always to the cent, because it is going to
/// be compared with an invoice.
export function dollarsExact(cents: number): string {
  return (cents / 100).toLocaleString("en-US", {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
}

/// What the query string says about a trip to the provider that just
/// ended. The subscription page comes back to
/// `…/settings/billing?subscribed=done` or `?subscribed=cancelled`.
/// Anything else is nothing to report.
export type BillingOutcome = "subscribed" | "subscribe_cancelled" | null;

export function readBillingOutcome(search: string): BillingOutcome {
  const q = new URLSearchParams(search);
  const sub = q.get("subscribed");
  if (sub === "done") return "subscribed";
  if (sub === "cancelled") return "subscribe_cancelled";
  return null;
}

export function outcomeLine(o: Exclude<BillingOutcome, null>): string {
  switch (o) {
    case "subscribed":
      return "Subscribed. Private repositories are on; the invoice is with the payment provider.";
    case "subscribe_cancelled":
      return "Checkout was cancelled and nothing was charged. This organization is still free, for public work.";
  }
}

/// The thing that was refused for want of a subscription, kept across
/// the trip to the provider so that coming back is not a dead end.
///
/// The paywall used to replay the refused request in place, because
/// the subscription was made without leaving the page. Now the page is
/// the provider's, and a redirect cannot resume a form — so the
/// paywall writes down what was asked, and the billing screen, once
/// the subscription has landed, offers the one button that finishes
/// it. Session storage: it is this tab's errand, and a tab closed on
/// the provider's page has dropped it.
export type PendingIntent =
  | { org: string; kind: "create"; name: string; description?: string }
  | { org: string; kind: "private"; name: string };

const INTENT_KEY = "weft.billing.intent";

export function rememberIntent(i: PendingIntent): void {
  try {
    sessionStorage.setItem(INTENT_KEY, JSON.stringify(i));
  } catch {
    // Storage refused (private mode, quota): the person retypes the
    // name, which is the old behaviour, not a broken one.
  }
}

/// The errand written down for this organization, if any. Another
/// organization's errand is not this page's to finish.
export function peekIntent(org: string): PendingIntent | null {
  try {
    const raw = sessionStorage.getItem(INTENT_KEY);
    if (!raw) return null;
    const i = JSON.parse(raw) as PendingIntent;
    if (i.org !== org || !i.name || (i.kind !== "create" && i.kind !== "private"))
      return null;
    return i;
  } catch {
    return null;
  }
}

export function clearIntent(): void {
  try {
    sessionStorage.removeItem(INTENT_KEY);
  } catch {
    // Nothing to clear, or nowhere to clear it from.
  }
}

/// The button that finishes an errand: what it was, as the person
/// would say it.
export function intentLabel(i: PendingIntent): string {
  return i.kind === "create"
    ? `Create ${i.name} (private)`
    : `Make ${i.name} private`;
}
