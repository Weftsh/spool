import { beforeAll, describe, expect, it } from "vitest";
import type { Billing } from "@/api";
import {
  clearIntent,
  dollars,
  dollarsExact,
  hasPools,
  intentLabel,
  outcomeLine,
  overageLine,
  parseSpendLimit,
  peekIntent,
  planState,
  privateNeeds,
  readBillingOutcome,
  rememberIntent,
  seatPools,
  spendLimitLine,
} from "./billing";

/// The billing view as the server with pools sends it, in each of the
/// situations the page has to tell apart: inside the pool, past it and
/// metered, at the cap and refused, and the older server that has none
/// of these fields at all.
const METER = {
  included: 5000,
  used: 1240,
  remaining: 3760,
  overage: 0,
  estimated_cents: 0,
  refusing: false,
};
const POOLS: Partial<Billing> = {
  plan: "paid",
  paid_seats: 5,
  paid_minutes_per_seat: 1000,
  paid_egress_gb_per_seat: 10,
  paid_storage_gb_per_seat: 5,
  meters: {
    minutes: METER,
    egress_gb: { ...METER, included: 50, used: 12.4, remaining: 37.6 },
    storage_gb: { ...METER, included: 25, used: 9.1, remaining: 15.9 },
  },
  overage_estimated_cents: 0,
  spend_limit_cents: 0,
  may_raise_spend_limit: true,
  metering: "on",
  rates: {
    cents_per_1000_minutes: 800,
    cents_per_gb_egress: 10,
    cents_per_gb_month_storage: 10,
  cents_per_gb_month_packages: 15,
  },
};
const OVER: Partial<Billing> = {
  ...POOLS,
  meters: {
    ...POOLS.meters!,
    minutes: {
      ...METER,
      used: 6000,
      remaining: 0,
      overage: 1000,
      estimated_cents: 800,
    },
  },
  overage_estimated_cents: 800,
  spend_limit_cents: 2500,
};
const CAPPED: Partial<Billing> = {
  ...OVER,
  meters: {
    ...OVER.meters!,
    minutes: { ...OVER.meters!.minutes, refusing: true },
  },
  overage_estimated_cents: 2500,
};

function billing(over: Partial<Billing> = {}): Billing {
  return {
    org: "acme",
    plan: "free",
    billable_seats: 3,
    paid_seats: 0,
    status: null,
    current_period_end: null,
    may_create_public: true,
    may_create_private: false,
    may_add_people: true,
    price_per_seat_cents: 400,
    paid_minutes_per_seat: 2000,
    free_minutes: 500,
    ...over,
  };
}

describe("planState", () => {
  it("gives each of the three plans its own sentence and its own button", () => {
    const states = (["free", "paid", "past_due"] as Billing["plan"][]).map(
      (plan) => planState(billing({ plan })),
    );
    expect(new Set(states.map((s) => s.label)).size).toBe(3);
    expect(new Set(states.map((s) => s.hint)).size).toBe(3);
    expect(states.map((s) => s.action)).toEqual(["subscribe", "portal", "settle"]);
  });

  it("quotes the price on the free plan, from the server's numbers", () => {
    const s = planState(billing({ price_per_seat_cents: 700 }));
    expect(s.hint).toMatch(/\$7 per seat per month/);
    expect(s.hint).toMatch(/\$21\/month for the 3 seats/);
    expect(s.hint).toMatch(/2,000 hosted CI minutes/);
    // The button leaves for the provider's page now: a promotion code
    // can only be typed there. "Subscribe" promised something that
    // happened here.
    expect(s.button).toBe("Continue to checkout");
  });

  it("tells a lapsed organisation what it is still holding", () => {
    // `free` is where a cancelled subscription lands, private
    // repositories and all. The page told an organization holding three
    // of them that "the first private repository starts a subscription"
    // — found on a Billing page whose vault had just been made
    // read-only by the cancellation and nothing on the page said so.
    const s = planState(billing({ private_repos: 3 }));
    expect(s.hint).toMatch(/3 private repositories are read-only/);
    expect(s.hint).toMatch(/still readable/);
    expect(s.hint).toMatch(/Subscribing at \$4 per seat/);
    expect(s.hint).not.toMatch(/first private repository/);
    // One, singular.
    expect(planState(billing({ private_repos: 1 })).hint).toMatch(
      /1 private repository is read-only/,
    );
    // None — a genuinely new organization — keeps the original sentence,
    // and so does a server too old to send the count.
    expect(planState(billing({ private_repos: 0 })).hint).toMatch(
      /The first private repository starts a subscription/,
    );
    expect(planState(billing()).hint).not.toMatch(/read-only/);
  });

  it("says what a failed payment pauses, not only what it keeps", () => {
    const s = planState(billing({ plan: "past_due" }));
    expect(s.hint).toMatch(/still readable/);
    expect(s.hint).toMatch(/nothing has been deleted/);
    expect(s.hint).toMatch(/private repositories are paused/);
    // What it pauses is *private* creation. The server admits a public
    // repository into a past-due organization, and the page used to say
    // "creating repositories" stops — a person would not have tried.
    expect(s.hint).toMatch(/Creating private repositories/);
    expect(s.hint).not.toMatch(/Creating repositories/);
    expect(s.hint).toMatch(/Public repositories are unaffected/);
    expect(s.hint).toMatch(/can still be pushed to/);
    expect(s.button).toBe("Settle the payment");
  });
});

describe("privateNeeds", () => {
  it("is empty where nothing is for sale", () => {
    expect(
      privateNeeds(
        billing({ price_per_seat_cents: null, paid_minutes_per_seat: null }),
      ),
    ).toBe("");
  });

  it("never quotes zero seats: the founder is one", () => {
    expect(privateNeeds(billing({ billable_seats: 0 }))).toMatch(
      /\$4\/month for the 1 seat in use/,
    );
  });

  it("leaves the minutes out when the deployment does not meter them", () => {
    expect(privateNeeds(billing({ paid_minutes_per_seat: null }))).not.toMatch(
      /minutes/,
    );
  });
});

describe("the seat sentence with pools", () => {
  it("names all three pools once the server sells them", () => {
    const s = privateNeeds(billing(POOLS));
    expect(s).toContain(
      "Each seat brings 1,000 hosted CI minutes, 10 GB of transfer and 5 GB of storage for private work, pooled.",
    );
    expect(seatPools(billing(POOLS))).toBe(
      "1,000 hosted CI minutes, 10 GB of transfer and 5 GB of storage for private work, pooled",
    );
  });

  it("keeps today's sentence on a server that sells only minutes", () => {
    // Pixel-identical on the older shape: the fields are absent, not
    // zero, and "0 GB of transfer" would be a pool nobody sold.
    expect(privateNeeds(billing())).toContain(
      "Each seat brings 2,000 hosted CI minutes a month.",
    );
    expect(privateNeeds(billing())).not.toMatch(/transfer/);
    expect(seatPools(billing())).toBe(null);
    expect(seatPools(billing({ paid_minutes_per_seat: null }))).toBe(null);
  });

  it("tells a past-due organization that use past the pool waits too, only where there is a pool", () => {
    expect(planState(billing({ ...POOLS, plan: "past_due" })).hint).toMatch(
      /Use past the pool is not billed until then\.$/,
    );
    expect(planState(billing({ plan: "past_due" })).hint).not.toMatch(/pool/);
    expect(hasPools(billing(POOLS))).toBe(true);
    expect(hasPools(billing())).toBe(false);
  });
});

describe("parseSpendLimit", () => {
  it("reads dollars the ways people type them", () => {
    expect(parseSpendLimit("25")).toEqual({ cents: 2500 });
    expect(parseSpendLimit("25.50")).toEqual({ cents: 2550 });
    expect(parseSpendLimit("$25")).toEqual({ cents: 2500 });
    expect(parseSpendLimit(" $ 1,000.5 ")).toEqual({ cents: 100050 });
    expect(parseSpendLimit("0")).toEqual({ cents: 0 });
    expect(parseSpendLimit(".5")).toEqual({ cents: 50 });
  });

  it("refuses what the server would refuse, before it is sent", () => {
    // 400 on negative and fractional cents: the page says why rather
    // than sending it and showing the server's sentence back.
    expect(parseSpendLimit("-5")).toEqual({
      error: "A spend limit cannot be negative.",
    });
    expect(parseSpendLimit("25.005")).toEqual({ error: "Whole cents only." });
    expect(parseSpendLimit("")).toHaveProperty("error");
    expect(parseSpendLimit("   ")).toHaveProperty("error");
    expect(parseSpendLimit("abc")).toHaveProperty("error");
    expect(parseSpendLimit("NaN")).toHaveProperty("error");
    expect(parseSpendLimit("1e3")).toHaveProperty("error");
    expect(parseSpendLimit(".")).toHaveProperty("error");
    expect(parseSpendLimit("$")).toHaveProperty("error");
  });
});

describe("spendLimitLine and overageLine", () => {
  it("says what a limit of nothing, of something, and of no limit each mean", () => {
    expect(spendLimitLine(billing(POOLS))).toMatch(/^The spend limit is \$0/);
    expect(spendLimitLine(billing(POOLS))).toMatch(/hosted jobs wait/);
    expect(spendLimitLine(billing(OVER))).toMatch(/^Up to \$25 of use past the pool is billed this period/);
    expect(
      spendLimitLine(billing({ ...POOLS, spend_limit_cents: null })),
    ).toMatch(/^No spend limit is set/);
  });

  it("defers to the server about whether use is billed at all", () => {
    // `metering` is the server's flag, never inferred from the rates or
    // the limit: a deployment with no usage prices refuses at the
    // edge, and a subscription older than usage billing has to be made
    // again before any limit means anything.
    expect(spendLimitLine(billing({ ...OVER, metering: "off" }))).toMatch(
      /not billed on this deployment/,
    );
    expect(
      spendLimitLine(billing({ ...OVER, metering: "resubscribe" })),
    ).toMatch(/Re-subscribe to set a spend limit/);
  });

  it("writes the estimate to the cent, and says when there is none", () => {
    expect(overageLine(billing(POOLS))).toBe(
      "Nothing past the pool so far this period.",
    );
    expect(overageLine(billing())).toBe(
      "Nothing past the pool so far this period.",
    );
    expect(overageLine(billing(OVER))).toMatch(/^\$8\.00 estimated so far this period/);
    expect(overageLine(billing(CAPPED))).toMatch(/^\$25\.00 estimated/);
    expect(dollarsExact(800)).toBe("$8.00");
    expect(dollarsExact(5)).toBe("$0.05");
  });
});

describe("dollars", () => {
  it("drops the cents on whole dollars and keeps them otherwise", () => {
    expect(dollars(400)).toBe("$4");
    expect(dollars(450)).toBe("$4.50");
    expect(dollars(1200)).toBe("$12");
    expect(dollars(123456)).toBe("$1,234.56");
  });
});

describe("readBillingOutcome", () => {
  it("reads the two outcomes the subscription page can send", () => {
    expect(readBillingOutcome("?subscribed=done")).toBe("subscribed");
    expect(readBillingOutcome("?subscribed=cancelled")).toBe(
      "subscribe_cancelled",
    );
    expect(readBillingOutcome("?subscribed=maybe")).toBeNull();
    // The card page is gone with the setup-mode session: a
    // merchant-of-record account has none, and `?card=` reports nothing.
    expect(readBillingOutcome("?card=added")).toBeNull();
    expect(readBillingOutcome("")).toBeNull();
    expect(readBillingOutcome("?other=1")).toBeNull();
  });

  it("says what each trip did, and what it did not", () => {
    expect(outcomeLine("subscribed")).toMatch(/^Subscribed\./);
    expect(outcomeLine("subscribe_cancelled")).toMatch(/nothing was charged/);
    expect(outcomeLine("subscribed")).not.toBe(outcomeLine("subscribe_cancelled"));
  });
});

describe("the errand kept across the provider's page", () => {
  // Node has no session storage; the browser's is a string map with
  // three methods, which is all the errand uses.
  beforeAll(() => {
    const m = new Map<string, string>();
    Object.defineProperty(globalThis, "sessionStorage", {
      configurable: true,
      value: {
        getItem: (k: string) => m.get(k) ?? null,
        setItem: (k: string, v: string) => void m.set(k, String(v)),
        removeItem: (k: string) => void m.delete(k),
      },
    });
  });

  it("round-trips, is one organization's, and clears", () => {
    clearIntent();
    expect(peekIntent("acme")).toBeNull();
    rememberIntent({ org: "acme", kind: "create", name: "vault", description: "x" });
    expect(peekIntent("acme")).toEqual({
      org: "acme",
      kind: "create",
      name: "vault",
      description: "x",
    });
    // Another organization's billing page does not finish acme's errand.
    expect(peekIntent("other")).toBeNull();
    clearIntent();
    expect(peekIntent("acme")).toBeNull();
    rememberIntent({ org: "acme", kind: "private", name: "widget" });
    expect(peekIntent("acme")?.kind).toBe("private");
    clearIntent();
  });

  it("ignores something that is not an errand", () => {
    sessionStorage.setItem("weft.billing.intent", "{\"org\":\"acme\",\"kind\":\"delete\"}");
    expect(peekIntent("acme")).toBeNull();
    sessionStorage.setItem("weft.billing.intent", "not json");
    expect(peekIntent("acme")).toBeNull();
    clearIntent();
  });

  it("names the button after the thing that was refused", () => {
    expect(intentLabel({ org: "acme", kind: "create", name: "vault" })).toBe(
      "Create vault (private)",
    );
    expect(intentLabel({ org: "acme", kind: "private", name: "widget" })).toBe(
      "Make widget private",
    );
  });
});
