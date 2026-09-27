import { useCallback, useEffect, useRef, useState } from "react";
import { api, type Billing, type Session } from "@/api";
import { Err } from "@/components/feedback";
import { Panel } from "@/components/panel";
import { Button } from "@/components/ui/button";
import {
  clearIntent,
  dollars,
  dollarsExact,
  intentLabel,
  outcomeLine,
  overageLine,
  parseSpendLimit,
  peekIntent,
  planState,
  readBillingOutcome,
  seatPools,
  spendLimitLine,
  type PendingIntent,
} from "@/lib/billing";
import { href, navigateTo } from "@/router";
import { FirstSync } from "@/views/newrepo";
import {
  SUSPENSION_NOTE,
  githubMinutesNote,
  meterLabel,
  meterLine,
  meterNote,
  meterOverageLine,
  minutesLine,
  minutesNote,
  rateText,
  readCiMinutes,
  readCiSuspension,
  readMeters,
  type Meter,
  type MeterKind,
} from "@/lib/meter";
import { formatDay } from "@/format";

/// What this organization costs, and the one button that changes it.
///
/// Three states — free, subscribed, payment failed — and each gets its
/// own sentence and its own button, from `planState`. The price is the
/// server's number, never typed here. No card is asked for before a
/// subscription: the provider is the merchant of record and meets the
/// card on its own subscription page.
///
/// Two seat figures, deliberately both shown once there is a
/// subscription. `billable_seats` is what we would charge for now;
/// `paid_seats` is what the provider has been told. They agree almost
/// always — and when they do not, somebody needs to see it, because the
/// difference is a push that did not land.
/// How long to keep asking after `?subscribed=done` before giving up
/// on the webhook: the provider sends it within seconds, and a person
/// staring at "Free" after paying deserves the page to look again
/// rather than to be told to refresh.
const CONFIRM_TRIES = 10;
const CONFIRM_EVERY_MS = 1000;

export function BillingPanel(props: {
  session: Session;
  navigate: (to: string) => void;
}) {
  const { session } = props;
  const [billing, setBilling] = useState<Billing | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Read once, on arrival: the provider sends people back here with
  // `?subscribed=…`, and the sentence for each outcome is the only
  // confirmation they get that the trip did anything.
  const [outcome] = useState(() => readBillingOutcome(window.location.search));
  // The errand the paywall wrote down before leaving for the provider,
  // if it was this organization's. Offered once the subscription has
  // landed; dropped if the trip was cancelled.
  const [intent, setIntent] = useState<PendingIntent | null>(() => {
    if (outcome === "subscribe_cancelled") {
      clearIntent();
      return null;
    }
    return peekIntent(session.org);
  });
  // Still waiting for the provider's webhook to move the plan.
  const [confirming, setConfirming] = useState(outcome === "subscribed");
  // The repository the errand created, once it has: the clone command
  // is handed over here, the way the new-repository form does it — an
  // empty repository's page has no tree to show yet.
  const [made, setMade] = useState<string | null>(null);

  const refresh = useCallback(() => {
    api
      .billing(session)
      .then(setBilling)
      .catch((e) => setError(`Could not read billing: ${e}`));
  }, [session]);
  useEffect(refresh, [refresh]);

  // Back from the subscription page: the webhook that pays is on the
  // provider's schedule, not the browser's, so a first read that still
  // says "Free" is not the answer yet. Ask again, a few times, and
  // say so meanwhile.
  useEffect(() => {
    if (!confirming) return;
    if (billing && billing.plan !== "free") {
      setConfirming(false);
      return;
    }
    let tries = 0;
    let alive = true;
    const tick = () => {
      if (!alive) return;
      tries += 1;
      api
        .billing(session)
        .then((b) => {
          if (!alive) return;
          setBilling(b);
          if (b.plan !== "free") setConfirming(false);
          else if (tries >= CONFIRM_TRIES) setConfirming(false);
          else setTimeout(tick, CONFIRM_EVERY_MS);
        })
        .catch(() => {
          if (alive && tries < CONFIRM_TRIES)
            setTimeout(tick, CONFIRM_EVERY_MS);
          else if (alive) setConfirming(false);
        });
    };
    const t = setTimeout(tick, CONFIRM_EVERY_MS);
    return () => {
      alive = false;
      clearTimeout(t);
    };
    // Deliberately not re-armed by `billing` changing: one loop, which
    // reads and stops itself.
  }, [confirming, session]);

  async function provider() {
    setBusy(true);
    setError(null);
    try {
      const { url } = await api.startCheckout(session);
      window.location.assign(url);
    } catch (err) {
      setError(
        `Could not open billing: ${err instanceof Error ? err.message : err}`,
      );
      setBusy(false);
    }
  }

  async function subscribe() {
    setBusy(true);
    setError(null);
    try {
      const out = await api.subscribe(session);
      if ("url" in out) {
        // The provider's page: the saved card is already on it, and a
        // promotion code can be typed there. Back here afterwards.
        window.location.assign(out.url);
        return;
      }
      // Already paying — the webhook landed before the click.
      setBilling(out);
    } catch (err) {
      setError(
        `Could not subscribe: ${err instanceof Error ? err.message : err}`,
      );
    } finally {
      setBusy(false);
    }
  }

  /// Finish what the paywall wrote down, now that the organization
  /// pays for it: the same create or the same visibility change, and
  /// then the repository itself.
  async function finish(i: PendingIntent) {
    setBusy(true);
    setError(null);
    try {
      if (i.kind === "create") {
        await api.createRepo(session, {
          name: i.name,
          public: false,
          description: i.description,
        });
        clearIntent();
        setIntent(null);
        setMade(i.name);
        setBusy(false);
        return;
      }
      await api.patchRepo(session, i.name, { public: false });
      clearIntent();
      setIntent(null);
      navigateTo(href([session.org, i.name]));
    } catch (err) {
      setError(
        `Could not ${i.kind === "create" ? "create" : "make private"} ${i.name}: ${
          err instanceof Error ? err.message : err
        }`,
      );
      setBusy(false);
    }
  }

  if (error && !billing) return <Err message={error} />;
  if (!billing) return <Panel title="Billing">Loading…</Panel>;
  const state = planState(billing);
  const subscribed = billing.plan === "paid" || billing.plan === "past_due";
  const drift = subscribed && billing.billable_seats !== billing.paid_seats;
  const go = state.action === "subscribe" ? subscribe : provider;

  return (
    <div className="space-y-4">
      {outcome && (
        <p
          className="rounded-md border border-borderline bg-surface-1 px-3 py-2 text-sm"
          role="status"
        >
          {confirming && billing.plan === "free"
            ? "Confirming with the payment provider…"
            : outcomeLine(outcome)}
        </p>
      )}
      {made && (
        <FirstSync
          session={session}
          repo={made}
          mirror={false}
          navigate={props.navigate}
        />
      )}
      {intent && subscribed && !made && (
        <div
          className="flex flex-wrap items-center gap-3 rounded-md border border-brand/40 bg-brand/5 px-3 py-2 text-sm"
          role="region"
          aria-label="Pick up where you left off"
        >
          <span>
            {intent.kind === "create"
              ? `You were creating a private repository, ${intent.name}.`
              : `You were making ${intent.name} private.`}
          </span>
          <Button disabled={busy} onClick={() => void finish(intent)}>
            {busy ? "Working…" : intentLabel(intent)}
          </Button>
          <Button
            variant="outline"
            disabled={busy}
            onClick={() => {
              clearIntent();
              setIntent(null);
            }}
          >
            Not now
          </Button>
        </div>
      )}
      <Panel title="Subscription" hint={state.hint}>
        <Err message={error} />
        <dl
          className={`mb-3 grid grid-cols-2 gap-3 text-sm ${
            subscribed && billing.price_per_seat_cents !== null
              ? "sm:grid-cols-5"
              : "sm:grid-cols-4"
          }`}
        >
          <div>
            <dt className="text-xs text-ink-3">Status</dt>
            <dd className="font-medium">{state.label}</dd>
          </div>
          {subscribed ? (
            <>
              <div>
                <dt className="text-xs text-ink-3">Seats in use</dt>
                <dd className="font-medium">{billing.billable_seats}</dd>
              </div>
              <div>
                <dt className="text-xs text-ink-3">Seats billed</dt>
                <dd className="font-medium">{billing.paid_seats}</dd>
              </div>
              {billing.period_start !== undefined &&
              billing.period_start !== null ? (
                // Both ends once the server meters by period: a pool
                // is "this period's", and a reader past one wants to
                // know when it started as much as when it resets.
                <div>
                  <dt className="text-xs text-ink-3">Period</dt>
                  <dd className="font-medium">
                    {new Date(billing.period_start).toLocaleDateString()} –{" "}
                    {billing.current_period_end
                      ? new Date(
                          billing.current_period_end,
                        ).toLocaleDateString()
                      : "—"}
                  </dd>
                </div>
              ) : (
                <div>
                  <dt className="text-xs text-ink-3">Renews</dt>
                  <dd className="font-medium">
                    {billing.current_period_end
                      ? new Date(
                          billing.current_period_end,
                        ).toLocaleDateString()
                      : "—"}
                  </dd>
                </div>
              )}
              {billing.price_per_seat_cents !== null && (
                // The amount, not only the seat count: a person paying
                // wants to see what they pay, and "3 seats billed" made
                // them multiply. The invoice is the truth; this is the
                // same arithmetic the provider does, shown before it
                // arrives.
                <div>
                  <dt className="text-xs text-ink-3">Per month</dt>
                  <dd className="font-medium">
                    {dollars(billing.price_per_seat_cents * billing.paid_seats)}{" "}
                    <span className="text-xs font-normal text-ink-3">
                      {dollars(billing.price_per_seat_cents)} ×{" "}
                      {billing.paid_seats}
                    </span>
                  </dd>
                </div>
              )}
            </>
          ) : (
            <>
              <div>
                <dt className="text-xs text-ink-3">Seats in use</dt>
                <dd className="font-medium">{billing.billable_seats}</dd>
              </div>
              {billing.price_per_seat_cents !== null && (
                <div>
                  {/* "Price", not "Private repositories". The tile said
                      the latter over a figure in dollars, and beside a
                      paragraph that now names how many private
                      repositories this organization holds it reads as
                      that count — a screenshot of a lapsed organization
                      showed "Private repositories / $4 per seat /
                      month" directly under "its 1 private repository is
                      read-only". */}
                  <dt className="text-xs text-ink-3">Price</dt>
                  <dd className="font-medium">
                    {dollars(billing.price_per_seat_cents)} per seat / month
                  </dd>
                </div>
              )}
              {billing.private_repos !== undefined && (
                <div>
                  <dt className="text-xs text-ink-3">Private repositories</dt>
                  <dd className="font-medium">{billing.private_repos}</dd>
                </div>
              )}
            </>
          )}
        </dl>
        {drift && (
          <p className="mb-3 text-xs text-ink-2" role="status">
            {billing.billable_seats > billing.paid_seats
              ? "More people are in this organization than are billed for. The next change will settle it; if this persists, a seat update did not reach the payment provider."
              : "You are billed for seats nobody is using. They are yours until the end of the period — release them in the billing portal if you do not want them back."}
          </p>
        )}
        <div className="flex flex-wrap gap-2">
          <Button disabled={busy} onClick={go}>
            {busy ? "Working…" : state.button}
          </Button>
        </div>
        <p className="mt-2 text-xs text-ink-3">
          Cards, invoices and cancellation live with the payment provider —
          nothing about your card is stored here.
        </p>
      </Panel>
      <CiSuspended billing={billing} />
      {billing.meters === undefined ? (
        <HostedMinutes billing={billing} />
      ) : (
        <UsagePanel billing={billing} />
      )}
      {subscribed && billing.spend_limit_cents !== undefined && (
        <SpendLimitPanel
          session={session}
          billing={billing}
          onBilling={setBilling}
          busy={busy}
          onResubscribe={subscribe}
        />
      )}
      <Panel
        title="What counts as a seat"
        hint="Every member of this organization, plus anybody outside it who can reach a private repository. Collaborators on public repositories are free. Somebody who is both is one seat."
      >
        <p className="text-sm text-ink-2">
          Adding somebody is never refused while the subscription is active —
          the bill follows them the moment they join. Removing somebody does not
          cancel the seat: it stays yours for the rest of the period, so you can
          fill it again without paying twice.
        </p>
      </Panel>
    </div>
  );
}

/// Hosted-runner minutes: what this organisation has spent, and what
/// happens when it runs out.
///
/// Absent entirely on a deployment that does not meter them. A panel
/// reading "unlimited" is a promise about capacity nobody made, and a
/// panel reading "0 minutes" would be the same sentence an exhausted
/// organisation sees — which is the one confusion worth designing
/// against here.
function HostedMinutes(props: { billing: Billing }) {
  const m = readCiMinutes(props.billing);
  if (m.kind === "unmetered") return null;
  const line = minutesLine(m);
  const note = minutesNote(m);
  // The GitHub share, under the line and only when there is one: both
  // doors spend the same pool, and a person who saw "1,240 used" and
  // ran no Weft workflows this month would otherwise have nowhere to
  // learn where the minutes went.
  const github = githubMinutesNote(props.billing);
  return (
    <Panel
      title="Hosted CI minutes"
      hint="Time spent on our runners, counted per job over the last 30 days. Self-hosted runners do not count against it."
    >
      <p className="text-sm text-ink">
        <span className="font-mono">{m.remaining.toLocaleString("en-US")}</span>{" "}
        {m.remaining === 1 ? "minute" : "minutes"} left
      </p>
      {/* The bar is the glance; the words are the fact. Never the bar
          alone — DESIGN.md, and a length is not readable to somebody
          using a screen reader. */}
      <div
        className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-surface-2"
        role="img"
        aria-label={line ?? ""}
      >
        <div
          className={m.exhausted ? "h-full bg-serious" : "h-full bg-brand"}
          style={{ width: `${m.percent}%` }}
        />
      </div>
      <p className="mt-2 text-xs text-ink-3">{line}</p>
      {github && <p className="mt-0.5 text-xs text-ink-3">{github}</p>}
      {/* Where the budget comes from, so a free organisation reading
          "500" knows what subscribing would change it to, and a paying
          one knows why adding a person raised it. Silent on a
          deployment that sells nothing: there the number is the
          operator's, and nothing here changes it. */}
      {allowance(props.billing) && (
        <p className="mt-1 text-xs text-ink-3">{allowance(props.billing)}</p>
      )}
      {note && (
        <p className="mt-2 text-xs text-serious" role="status">
          {note}
        </p>
      )}
    </Panel>
  );
}

function allowance(b: Billing): string | null {
  if (b.paid_minutes_per_seat === null) return null;
  const perSeat = b.paid_minutes_per_seat.toLocaleString("en-US");
  // All three pools once the server sells them; today's minutes-only
  // sentence, to the character, when it does not.
  const pools = seatPools(b);
  if (b.plan === "paid" || b.plan === "past_due") {
    const seats = `${Math.max(1, b.paid_seats)} ${b.paid_seats === 1 ? "seat" : "seats"} billed`;
    return pools === null
      ? `${perSeat} minutes per paid seat, ${seats}.`
      : `Each paid seat brings ${pools}; ${seats}.`;
  }
  const free =
    b.free_minutes === null
      ? ""
      : `${b.free_minutes.toLocaleString("en-US")} minutes a month while free; `;
  return pools === null
    ? `${free}${perSeat} per seat once subscribed.`
    : `${free}${pools} per seat once subscribed.`;
}

/// What this organization has used of each pool this period, and what
/// is happening at the edge of each.
///
/// One row per pool the server meters, in the order a reader is likely
/// to hit them — minutes, transfer, storage — and only the metered
/// ones: a pool with `included: null` is not sold here, and a row for
/// it would be a bar with nothing to measure against. A free
/// organization has only the minutes, on a rolling window, under the
/// heading it always had.
function UsagePanel(props: { billing: Billing }) {
  const b = props.billing;
  const rows = readMeters(b);
  if (rows.length === 0) return null;
  const free = b.plan === "free";
  const window = free ? "rolling" : "period";
  return (
    <Panel
      title={free ? "Hosted CI minutes" : "Usage this period"}
      hint={
        free
          ? "Time spent on our runners, counted per job over the last 30 days. Self-hosted runners do not count against it."
          : "Counted for private work on our runners and our storage. Public traffic, self-hosted runners and a hosted job's traffic to Weft are never counted."
      }
    >
      <div className="space-y-4">
        {rows.map(({ kind, meter }) => (
          <MeterRow
            key={kind}
            kind={kind}
            meter={meter}
            window={window}
            // The GitHub share of the minutes, when there is one.
            extra={kind === "minutes" ? githubMinutesNote(b) : null}
            // A free organization is refused at the edge of its window,
            // and its sentence is about the window rolling — never
            // about a spend limit it cannot have.
            note={
              free
                ? minutesNote(meter)
                : meterNote(
                    meter,
                    kind,
                    b.spend_limit_cents,
                    rateText(b.rates, kind),
                  )
            }
          />
        ))}
      </div>
      {allowance(b) && (
        <p className="mt-3 text-xs text-ink-3">{allowance(b)}</p>
      )}
    </Panel>
  );
}

function MeterRow(props: {
  kind: MeterKind;
  meter: Meter & { kind: "metered" };
  window: "rolling" | "period";
  note: string | null;
  /// A second line under the figure, in the same ink — nothing that
  /// needs acting on, unlike `note`.
  extra?: string | null;
}) {
  const { kind, meter: m } = props;
  const line = meterLine(m, kind, props.window) ?? "";
  const over = meterOverageLine(m, kind);
  // Three looks, and each is also words: refusing is the serious red
  // beside a `role="status"` sentence; past the pool is the warning
  // amber beside the overage line; inside the pool is the brand.
  const fill = m.refusing
    ? "h-full bg-serious"
    : m.overage > 0
      ? "h-full bg-warning"
      : "h-full bg-brand";
  return (
    <div>
      <h3 className="text-sm font-medium text-ink">{meterLabel(kind)}</h3>
      <p className="mt-1 text-xs text-ink-2">{line}</p>
      {props.extra && (
        <p className="mt-0.5 text-xs text-ink-2">{props.extra}</p>
      )}
      {/* The bar is the glance; the words are the fact. Never the bar
          alone — DESIGN.md, and a length is not readable to somebody
          using a screen reader. */}
      <div
        className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-surface-2"
        role="img"
        aria-label={line}
      >
        <div className={fill} style={{ width: `${m.percent}%` }} />
      </div>
      {over && <p className="mt-1 text-xs text-ink-2">{over}</p>}
      {props.note && (
        <p className="mt-2 text-xs text-serious" role="status">
          {props.note}
        </p>
      )}
    </div>
  );
}

/// How much use past the pools this organization will pay for, and the
/// box that changes it.
///
/// Paying organizations only, and only once the server sends a limit at
/// all — a free organization has no pool to be past, and an older
/// server has no limit to show. The editor is offered on the server's
/// `may_raise_spend_limit`, never on the caller's role: the server is
/// the one that refuses — with the masked 404 every admin route gives —
/// and a control that is then refused is worse than a sentence saying
/// who can use it.
function SpendLimitPanel(props: {
  session: Session;
  billing: Billing;
  onBilling: (b: Billing) => void;
  busy: boolean;
  onResubscribe: () => void;
}) {
  const b = props.billing;
  const [input, setInput] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  // `…/settings/billing#spend-limit` is where the paywall sends
  // somebody at the limit: the router keeps the fragment but nothing
  // scrolls to it on a pushState, so this does.
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (window.location.hash === "#spend-limit")
      ref.current?.scrollIntoView({ block: "start" });
  }, []);

  async function save(e: React.FormEvent) {
    e.preventDefault();
    setSaved(null);
    const parsed = parseSpendLimit(input);
    if ("error" in parsed) {
      setError(parsed.error);
      return;
    }
    setError(null);
    setSaving(true);
    try {
      const next = await api.setSpendLimit(props.session, parsed.cents);
      props.onBilling(next);
      setInput("");
      // The server's figure, not the one typed: the page says what
      // the limit *is* now, which is what was asked for only if the
      // server agreed.
      setSaved(
        `Spend limit is now ${
          next.spend_limit_cents === null ||
          next.spend_limit_cents === undefined
            ? dollars(parsed.cents)
            : dollars(next.spend_limit_cents)
        }.`,
      );
    } catch (err) {
      setError(
        `Could not set the spend limit: ${err instanceof Error ? err.message : err}`,
      );
    } finally {
      setSaving(false);
    }
  }

  const limit = b.spend_limit_cents;
  const rates = (["minutes", "egress", "storage"] as const)
    .map((k) => rateText(b.rates, k))
    .filter((r): r is string => r !== null);
  return (
    <div ref={ref} id="spend-limit">
      <Panel title="Spend limit" hint={spendLimitLine(b)}>
        <dl className="mb-3 grid grid-cols-2 gap-3 text-sm">
          <div>
            <dt className="text-xs text-ink-3">Spend limit</dt>
            <dd className="font-medium">
              {limit === null || limit === undefined ? "—" : dollars(limit)}
            </dd>
          </div>
          <div>
            <dt className="text-xs text-ink-3">Estimated past the pool</dt>
            <dd className="font-medium">
              {dollarsExact(b.overage_estimated_cents ?? 0)}
            </dd>
          </div>
        </dl>
        <p className="mb-3 text-xs text-ink-3">{overageLine(b)}</p>
        {b.metering !== "off" && rates.length === 3 && (
          <p className="mb-3 text-xs text-ink-3">
            Past the pool: {rates[0]}, {rates[1]} transferred, {rates[2]}{" "}
            stored.
          </p>
        )}
        {b.metering === "resubscribe" ? (
          <Button
            type="button"
            disabled={props.busy}
            onClick={props.onResubscribe}
          >
            {props.busy ? "Working…" : "Re-subscribe to enable usage billing"}
          </Button>
        ) : b.metering === "off" ? null : b.may_raise_spend_limit ? (
          <form className="space-y-2" onSubmit={(e) => void save(e)}>
            <label
              className="block text-xs text-ink-3"
              htmlFor="spend-limit-input"
            >
              New limit, in dollars
            </label>
            <div className="flex flex-wrap gap-2">
              <input
                id="spend-limit-input"
                className="w-40 rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 font-mono text-sm"
                inputMode="decimal"
                placeholder="25"
                value={input}
                onChange={(e) => {
                  setInput(e.target.value);
                  setError(null);
                  setSaved(null);
                }}
              />
              <Button disabled={saving}>
                {saving ? "Saving…" : "Save spend limit"}
              </Button>
            </div>
            <Err message={error} />
            {saved && (
              <p className="text-xs text-ink-2" role="status">
                {saved}
              </p>
            )}
          </form>
        ) : (
          <p className="text-xs text-ink-2">
            Only an owner or admin can raise it.
          </p>
        )}
      </Panel>
    </div>
  );
}

/// A suspension, said where somebody looking for the bill will see it.
///
/// Above the minutes panel deliberately. The two are independent facts,
/// and a suspended organisation usually still has most of its budget —
/// so an org stopped for mining read "1,940 minutes left" here and
/// nothing else, and learned why only by opening a change. The panel
/// with the reassuring number must not be the first one it reads.
///
/// The reason is quoted, not summarised: it is written by the operator
/// or by the runner that stopped the job, and it is the only thing that
/// says what actually happened.
function CiSuspended(props: { billing: Billing }) {
  const s = readCiSuspension(props.billing);
  if (!s) return null;
  return (
    <Panel title="Hosted CI is suspended">
      <p className="text-sm text-ink">
        {/* Quoted, and marked as a quotation rather than as our own
            words — "mining software detected: xmrig" is a finding, and
            the page should not read as though it were phrasing it. */}
        <span className="text-ink-3">Reason given: </span>
        <span className="whitespace-pre-wrap break-words">{s.reason}</span>
      </p>
      {s.atMs !== null && (
        <p className="mt-1 text-xs text-ink-3">
          Suspended {formatDay(s.atMs)}.
        </p>
      )}
      {/* `text-serious` and the word "refused" — never colour alone. */}
      <p className="mt-2 text-xs text-serious" role="status">
        {SUSPENSION_NOTE}
      </p>
    </Panel>
  );
}

/// What the next person costs, said before the Invite button is
/// pressed rather than after.
///
/// Silent on a personal namespace and on a deployment that sells
/// nothing, because there is no cost to state — a line reading "0 seats"
/// on somebody's own account is noise dressed as information.
export function SeatLine(props: { session: Session; members: number }) {
  const [billing, setBilling] = useState<Billing | null>(null);
  useEffect(() => {
    let alive = true;
    api
      .billing(props.session)
      .then((b) => alive && setBilling(b))
      // A viewer, a personal namespace, or a build with no billing at
      // all: there is nothing to say, and saying it badly is worse.
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [props.session, props.members]);

  if (!billing || billing.plan === "free") return null;
  return (
    <p className="mb-3 text-xs text-ink-3" role="status">
      {billing.billable_seats} seat
      {billing.billable_seats === 1 ? "" : "s"} in use, {billing.paid_seats}{" "}
      billed.{" "}
      {billing.plan === "past_due"
        ? "This organization's last payment failed — nobody new can be added until it is settled."
        : billing.billable_seats >= billing.paid_seats
          ? "Adding somebody adds a seat to your next invoice, prorated from today."
          : "There is a paid seat free, so the next person costs nothing more this period."}
    </p>
  );
}
