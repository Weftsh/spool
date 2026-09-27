import { useEffect, useState } from "react";
import { api, type Billing, type Session } from "@/api";
import { Button } from "@/components/ui/button";
import { dollars, rememberIntent, type PendingIntent } from "@/lib/billing";
import { gbText, readMeter } from "@/lib/meter";
import { dash } from "@/router";

/// The server has just answered 402 to something that needs a paying
/// organization — a private repository, usually. This is the answer to
/// that answer: what it costs, said in numbers, and the one button that
/// makes the thing that was refused go through.
///
/// It exists so that the refusal is not a dead end. Before it, "this
/// organization needs a paid plan to hold a private repository" was a
/// red sentence over a form, and the way out was to find Settings, find
/// Billing, subscribe, come back, and fill the form in again. The price
/// is read from the server, not typed here, so this box and the pricing
/// page cannot disagree.
///
/// Two situations, decided by the billing view rather than by reading
/// the refusal's sentence: no subscription (the provider's subscription
/// page, where the card and a promotion code are typed), and a
/// subscription whose last payment failed (settle it with the
/// provider). The first leaves this page, so what was refused is
/// written down as `intent` first and the billing screen offers to
/// finish it on the way back. `onPaid` fires only when the server
/// answers that the organization already pays — the subscription
/// landed between the refusal and the click — so the caller can retry
/// on the spot.
///
/// A third family of refusal arrives with `reason`: a paying
/// organization at its **spend limit** — hosted minutes, private
/// transfer, or private storage. Nothing here is for sale then; the
/// way out is the limit on Billing, and the box says who can raise it
/// from the server's flag, never from the caller's role.
export type PaywallReason = "subscription" | "minutes" | "transfer" | "storage";

type SubscriptionProps = {
  reason?: "subscription";
  /// What was refused, as the object of a sentence: "a private
  /// repository", "making this repository private".
  what: string;
  onPaid: () => void;
  onDecline: () => void;
  /// What declining does, on the button. "Keep it public" on a form
  /// that has a visibility box; "Leave it public" on a repository that
  /// already exists.
  declineLabel: string;
  /// What was refused, so the billing screen can finish it after the
  /// provider sends the person back.
  intent: PendingIntent;
};

type LimitProps = {
  reason: "minutes" | "transfer" | "storage";
  /// The billing view, when the caller has already read it. Read here
  /// otherwise.
  billing?: Billing | null;
};

type PaywallProps =
  | ({ session: Session } & SubscriptionProps)
  | ({ session: Session } & LimitProps);

export function Paywall(props: PaywallProps) {
  if (isLimit(props)) return <SpendLimitWall {...props} />;
  return <SubscriptionWall {...props} />;
}

function isLimit(
  p: PaywallProps,
): p is { session: Session } & LimitProps {
  return p.reason !== undefined && p.reason !== "subscription";
}

function SubscriptionWall(props: { session: Session } & SubscriptionProps) {
  const [billing, setBilling] = useState<Billing | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Primitives, not the session object: a caller that builds its
  // session inline hands a fresh object every render, and an effect
  // keyed on it would refetch forever.
  const { org, token } = props.session;
  useEffect(() => {
    let alive = true;
    api
      .billing({ org, token })
      .then((b) => alive && setBilling(b))
      .catch((e) => alive && setError(`Could not read billing: ${e}`));
    return () => {
      alive = false;
    };
  }, [org, token]);

  async function subscribe() {
    setBusy(true);
    setError(null);
    try {
      const out = await api.subscribe(props.session);
      if ("url" in out) {
        // Leaving for the provider: write the errand down first, so
        // the page that greets the person on the way back can offer
        // to finish it.
        rememberIntent(props.intent);
        window.location.assign(out.url);
        return;
      }
      props.onPaid();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  }

  async function provider() {
    setBusy(true);
    setError(null);
    try {
      const { url } = await api.startCheckout(props.session);
      window.location.assign(url);
    } catch (e) {
      setError(`Could not open billing: ${e instanceof Error ? e.message : e}`);
      setBusy(false);
    }
  }

  const seats = billing ? Math.max(1, billing.billable_seats) : null;
  return (
    <div
      className="space-y-3 rounded-lg border border-brand/40 bg-brand/5 p-4"
      role="region"
      aria-label="Subscription needed"
    >
      <p className="text-sm font-medium">
        {capitalise(props.what)} needs a subscription
      </p>
      {billing === null ? (
        <p className="text-sm text-ink-3">{error ?? "Reading the price…"}</p>
      ) : billing.plan === "past_due" ? (
        <p className="text-sm text-ink-2">
          This organization is subscribed, but its last payment did not settle.
          Nothing new that costs money can be added until it is — nothing has
          been deleted.
        </p>
      ) : (
        <p className="text-sm text-ink-2">
          {billing.price_per_seat_cents !== null && seats !== null ? (
            <>
              <span className="font-medium">
                {dollars(billing.price_per_seat_cents * seats)}/month
              </span>{" "}
              — {dollars(billing.price_per_seat_cents)} per seat for the {seats}{" "}
              {seats === 1 ? "seat" : "seats"} in use today, prorated as people
              join or leave. Checkout opens on the provider's page, where the
              card and a promotion code can be entered; you come back to
              Billing, and this is finished from there.
              {billing.paid_minutes_per_seat !== null &&
                ` Each seat brings ${billing.paid_minutes_per_seat.toLocaleString("en-US")} hosted CI minutes a month.`}
            </>
          ) : (
            "A subscription covers every seat in the organization."
          )}
        </p>
      )}
      {error && billing !== null && (
        <p className="text-sm text-serious" role="alert">
          {error}
        </p>
      )}
      <div className="flex flex-wrap gap-2">
        {billing?.plan === "past_due" ? (
          <Button type="button" disabled={busy} onClick={provider}>
            {busy ? "Opening…" : "Settle the payment"}
          </Button>
        ) : (
          <Button type="button" disabled={busy || !billing} onClick={subscribe}>
            {busy ? "Opening…" : "Continue to checkout"}
          </Button>
        )}
        <Button
          type="button"
          variant="outline"
          disabled={busy}
          onClick={props.onDecline}
        >
          {props.declineLabel}
        </Button>
      </div>
    </div>
  );
}

/// The heading and the sentence for each pool at its spend limit.
///
/// Every sentence names what is *not* affected — public repositories,
/// the organization's own runners, what is already stored — because
/// "at the spend limit" alone sends a reader checking the things that
/// still work for a fault. Numbers are the server's, and a sentence
/// that has not got them yet says less rather than inventing them.
export function spendLimitCopy(
  reason: "minutes" | "transfer" | "storage",
  billing: Billing | null,
): { heading: string; body: string } {
  const limit =
    billing?.spend_limit_cents === null || billing?.spend_limit_cents === undefined
      ? "its spend limit"
      : dollars(billing.spend_limit_cents);
  const ms = billing?.meters;
  switch (reason) {
    case "minutes": {
      const m = readMeter(ms?.minutes);
      const included =
        m.kind === "metered" ? `${m.included.toLocaleString("en-US")} ` : "";
      return {
        heading: "Hosted jobs are waiting at the spend limit",
        body: `This organization has used its ${included}pooled minutes this period, and its spend limit is ${limit}. Jobs on hosted runners wait until minutes free up or the limit is raised. Jobs on your own runners are running as normal.`,
      };
    }
    case "transfer": {
      const m = readMeter(ms?.egress_gb);
      const included = m.kind === "metered" ? `${gbText(m.included)} GB` : "pool";
      return {
        heading: "Clones of private repositories are refused at the spend limit",
        body: `This organization has transferred its ${included} out of private repositories this period, and its spend limit is ${limit}. Public repositories are unaffected.`,
      };
    }
    case "storage": {
      const m = readMeter(ms?.storage_gb);
      const held =
        m.kind === "metered"
          ? `${gbText(m.used)} of ${gbText(m.included)} GB`
          : "their pool";
      return {
        heading: "This push would take private storage past the spend limit",
        body: `Private repositories here hold ${held}, and the spend limit is ${limit}. The push was refused and nothing already stored was touched. Free space, or raise the limit.`,
      };
    }
  }
}

function SpendLimitWall(props: { session: Session } & LimitProps) {
  const [read, setRead] = useState<Billing | null>(null);
  const [error, setError] = useState<string | null>(null);
  const given = props.billing;
  const { org, token } = props.session;
  useEffect(() => {
    if (given !== undefined) return;
    let alive = true;
    api
      .billing({ org, token })
      .then((b) => alive && setRead(b))
      .catch((e) => alive && setError(`Could not read billing: ${e}`));
    return () => {
      alive = false;
    };
  }, [given, org, token]);
  const billing = given === undefined ? read : given;
  const copy = spendLimitCopy(props.reason, billing);
  // The server's flag, never the caller's role. `undefined` — an older
  // server, or one that has not answered yet — reads as "cannot", the
  // side that offers no control which would then be refused.
  const mayRaise = billing?.may_raise_spend_limit === true;
  return (
    <div
      className="space-y-3 rounded-lg border border-warning/40 bg-warning/10 p-4"
      role="region"
      aria-label="Spend limit reached"
    >
      <p className="text-sm font-medium">{copy.heading}</p>
      <p className="text-sm text-ink-2">
        {billing === null && error !== null ? error : copy.body}
      </p>
      {mayRaise ? (
        <div className="flex flex-wrap gap-2">
          <Button asChild>
            <a href={`${dash(["settings", "billing"])}#spend-limit`}>
              Raise the spend limit
            </a>
          </Button>
        </div>
      ) : (
        billing !== null && (
          <p className="text-sm text-ink-2">
            Only an owner or admin can raise it, in Settings → Billing.
          </p>
        )
      )}
    </div>
  );
}

function capitalise(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}
