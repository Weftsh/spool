import { describe, expect, it } from "vitest";
import type { Billing, BillingMeter } from "@/api";
import {
  EGRESS_CAP_NOTE,
  MINUTES_CAP_NOTE,
  STORAGE_CAP_NOTE,
  SUSPENSION_NOTE,
  centsExact,
  formatPoolBytes,
  gbText,
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
  readMeter,
  readMeters,
  PACKAGES_CAP_NOTE,
} from "./meter";

function meter(over: Partial<BillingMeter> = {}): BillingMeter {
  return {
    included: 5000,
    used: 1240,
    remaining: 3760,
    overage: 0,
    estimated_cents: 0,
    refusing: false,
    ...over,
  };
}

const RATES = {
  cents_per_1000_minutes: 800,
  cents_per_gb_egress: 10,
  cents_per_gb_month_storage: 10,
  cents_per_gb_month_packages: 15,
};

/// A pool, read the way the minutes always were: null is unmetered and
/// is never zero, the server's figures win over our arithmetic, and
/// whether the server is refusing is its flag and not our subtraction.
describe("readMeter", () => {
  it("is unmetered on null, on a null pool, and never on zero", () => {
    expect(readMeter(undefined).kind).toBe("unmetered");
    expect(readMeter(null).kind).toBe("unmetered");
    expect(readMeter(meter({ included: null })).kind).toBe("unmetered");
    const zero = readMeter(meter({ included: 0, used: 0, remaining: 0 }));
    expect(zero.kind).toBe("metered");
    if (zero.kind !== "metered") return;
    expect(zero.exhausted).toBe(true);
    expect(zero.percent).toBe(100);
  });

  it("takes refusing from the server, not from the numbers", () => {
    // Past the pool and *not* refused is the ordinary paying case:
    // metered until the spend limit. A page that read `used >=
    // included` as "refused" would tell every organization with a
    // spend limit that its CI was dead the moment it left the pool.
    const past = readMeter(
      meter({ used: 6000, remaining: 0, overage: 1000, estimated_cents: 800 }),
    );
    if (past.kind !== "metered") throw new Error("metered");
    expect(past.exhausted).toBe(true);
    expect(past.refusing).toBe(false);
    expect(past.overage).toBe(1000);
    expect(past.estimatedCents).toBe(800);
    expect(past.percent).toBe(100);
    // And the other way: refusing with something left, which is what a
    // spend limit of $0 looks like on a pool that is about to run dry —
    // the flag is the fact.
    const refused = readMeter(meter({ refusing: true }));
    if (refused.kind !== "metered") throw new Error("metered");
    expect(refused.refusing).toBe(true);
    expect(refused.exhausted).toBe(false);
  });

  it("never reports a negative remainder or overage", () => {
    const m = readMeter(
      meter({ used: 0, remaining: -5, overage: -1, estimated_cents: -3 }),
    );
    if (m.kind !== "metered") throw new Error("metered");
    expect(m.remaining).toBe(0);
    expect(m.overage).toBe(0);
    expect(m.estimatedCents).toBe(0);
  });
});

describe("readCiMinutes with the pools present", () => {
  it("prefers meters.minutes to the legacy fields", () => {
    // Both shapes arrive from a server in transition. The pool is the
    // one the refusal runs on now, and it is the one that knows about
    // the spend limit.
    const m = readCiMinutes(
      billing({
        ci_minutes_limit: 2000,
        ci_minutes_used: 2000,
        ci_minutes_remaining: 0,
        meters: {
          minutes: meter(),
          egress_gb: meter({ included: null }),
          storage_gb: meter({ included: null }),
        },
      }),
    );
    if (m.kind !== "metered") throw new Error("metered");
    expect(m.included).toBe(5000);
    expect(m.refusing).toBe(false);
  });

  it("reads a legacy exhausted budget as refusing, because that server refused", () => {
    const m = readCiMinutes(
      billing({ ci_minutes_limit: 100, ci_minutes_used: 140 }),
    );
    if (m.kind !== "metered") throw new Error("metered");
    expect(m.refusing).toBe(true);
    expect(m.estimatedCents).toBe(0);
  });
});

describe("readMeters", () => {
  it("keeps the page's order and drops the unmetered pools", () => {
    expect(readMeters(billing())).toEqual([]);
    const ms = readMeters(
      billing({
        meters: {
          minutes: meter(),
          egress_gb: meter({ included: null }),
          storage_gb: meter({ included: 25, used: 9.1, remaining: 15.9 }),
          packages_gb: meter({ included: 10, used: 3.2, remaining: 6.8 }),
        },
      }),
    );
    expect(ms.map((m) => m.kind)).toEqual(["minutes", "storage", "packages"]);
  });

  /// A server from before the registry sends three pools. The page must
  /// show three rows rather than an empty fourth — the field is optional
  /// for the same reason the whole object is.
  it("shows three pools against a server that does not know about packages", () => {
    const ms = readMeters(
      billing({
        meters: {
          minutes: meter(),
          egress_gb: meter({ included: 50 }),
          storage_gb: meter({ included: 25 }),
        },
      }),
    );
    expect(ms.map((m) => m.kind)).toEqual(["minutes", "egress", "storage"]);
  });
});

describe("the packages meter", () => {
  it("is labelled, lined and priced apart from git storage", () => {
    expect(meterLabel("packages")).toBe("Package storage");
    expect(meterLabel("storage")).toBe("Private storage");

    const pkg = readMeter(meter({ included: 10, used: 3.2, remaining: 6.8 }));
    expect(meterLine(pkg, "packages", "period")).toBe(
      "3.2 of 10 GB of packages, averaged over this period",
    );
    // Its own rate, which is the whole point of a fourth meter: reading
    // the storage rate here would under-quote every estimate.
    expect(rateText(RATES, "packages")).toBe("$0.15 a GB-month");
    expect(rateText(RATES, "storage")).toBe("$0.10 a GB-month");
  });

  /// Refusing to publish is not refusing to install. A build that only
  /// resolves keeps working at the spend limit, and the sentence has to
  /// say so or somebody goes looking for a fault in their runners.
  it("says that installing still works when publishing is refused", () => {
    const refused = readMeter(meter({ refusing: true, overage: 4 }));
    const note = meterNote(refused, "packages", 0, null);
    expect(note).toBe(PACKAGES_CAP_NOTE);
    expect(note).toContain("Installing");
  });
});

describe("meterLine", () => {
  const minutes = readMeter(meter());
  const gb = readMeter(meter({ included: 50, used: 12.4, remaining: 37.6 }));
  const stored = readMeter(meter({ included: 25, used: 9.1, remaining: 15.9 }));

  it("says the period for a paying organization and the window for a free one", () => {
    expect(meterLine(minutes, "minutes", "period")).toBe(
      "1,240 of 5,000 minutes used this period",
    );
    expect(meterLine(minutes, "minutes", "rolling")).toBe(
      "1,240 of 5,000 minutes used in the last 30 days",
    );
  });

  it("names what each pool measures", () => {
    expect(meterLine(gb, "egress", "period")).toBe(
      "12.4 of 50 GB transferred this period",
    );
    // Storage is an average over the period — a pool that was full for
    // a day and empty for the rest is not full.
    expect(meterLine(stored, "storage", "period")).toBe(
      "9.1 of 25 GB stored, averaged over this period",
    );
    expect(meterLine({ kind: "unmetered" }, "egress", "period")).toBe(null);
  });
});

describe("meterOverageLine", () => {
  it("is nothing inside the pool and the money outside it", () => {
    expect(meterOverageLine(readMeter(meter()), "minutes")).toBe(null);
    expect(
      meterOverageLine(
        readMeter(
          meter({
            used: 6000,
            remaining: 0,
            overage: 1000,
            estimated_cents: 800,
          }),
        ),
        "minutes",
      ),
    ).toBe("1,000 minutes past the pool, $8.00 estimated");
    expect(
      meterOverageLine(
        readMeter(
          meter({
            included: 50,
            used: 52.5,
            remaining: 0,
            overage: 2.5,
            estimated_cents: 25,
          }),
        ),
        "egress",
      ),
    ).toBe("2.5 GB past the pool, $0.25 estimated");
  });
});

describe("meterNote", () => {
  it("says nothing inside the pool", () => {
    expect(
      meterNote(readMeter(meter()), "minutes", 2500, "$0.008 a minute"),
    ).toBe(null);
    expect(meterNote({ kind: "unmetered" }, "minutes", 2500, null)).toBe(null);
  });

  it("says use is metered past the pool, at the server's price, until the limit", () => {
    const past = readMeter(meter({ used: 5000, remaining: 0 }));
    expect(meterNote(past, "minutes", 2500, "$0.008 a minute")).toBe(
      "Past the pool: metered at $0.008 a minute until the spend limit of $25 is reached.",
    );
    // Without a rate there is no price to quote, and without a limit
    // figure there is no amount — neither is invented.
    expect(meterNote(past, "minutes", undefined, null)).toBe(
      "Past the pool: metered until the spend limit is reached.",
    );
  });

  it("gives each refusing pool its own sentence, on the server's flag alone", () => {
    // Refusing with plenty left — the flag is the fact, and the note
    // follows the flag rather than the arithmetic.
    const refused = readMeter(meter({ refusing: true }));
    expect(meterNote(refused, "minutes", 0, null)).toBe(MINUTES_CAP_NOTE);
    expect(meterNote(refused, "egress", 0, null)).toBe(EGRESS_CAP_NOTE);
    expect(meterNote(refused, "storage", 0, null)).toBe(STORAGE_CAP_NOTE);
    // Each says what is *not* affected, so nobody goes looking for a
    // fault in the things that still work.
    expect(MINUTES_CAP_NOTE).toContain(
      "your own runners are running as normal",
    );
    expect(EGRESS_CAP_NOTE).toContain("Public repositories are unaffected");
    expect(STORAGE_CAP_NOTE).toContain(
      "Nothing already stored has been touched",
    );
  });
});

describe("rateText", () => {
  it("quotes the server's rates as a person says them", () => {
    expect(rateText(RATES, "minutes")).toBe("$0.008 a minute");
    expect(rateText(RATES, "egress")).toBe("$0.10 a GB");
    expect(rateText(RATES, "storage")).toBe("$0.10 a GB-month");
    expect(rateText(undefined, "minutes")).toBe(null);
  });
});

describe("gigabytes and cents", () => {
  it("keeps whole numbers whole and fractions short", () => {
    expect(gbText(50)).toBe("50");
    expect(gbText(12.4)).toBe("12.4");
    expect(gbText(0.041)).toBe("0.04");
    expect(gbText(1234.5)).toBe("1,234.5");
  });

  it("uses the decimal units the pools are sold in", () => {
    // The pool's own arithmetic: 1,024 MB to the GB, so a repository
    // and the billing panel read the same bytes as the same number.
    // Decimal here once said 42.9 GB where Billing said 40. It counts
    // a pool that says "5 GB", and 1.2 GiB would count 1.29.
    expect(formatPoolBytes(1_200_000_000)).toBe("1.1 GB");
    expect(formatPoolBytes(40 * 1024 ** 3)).toBe("40.0 GB");
    expect(formatPoolBytes(340_000_000)).toBe("324 MB");
    expect(formatPoolBytes(999)).toBe("999 B");
    expect(formatPoolBytes(-1)).toBe("—");
  });

  it("writes an estimate to the cent, always", () => {
    expect(centsExact(800)).toBe("$8.00");
    expect(centsExact(25)).toBe("$0.25");
    expect(centsExact(0)).toBe("$0.00");
  });
});

/// The one mistake this module exists to prevent: reading "no budget"
/// as "no minutes". They are opposite facts — the first means every run
/// is allowed, the second means every run is refused — and a page that
/// confuses them tells an organisation with working CI that its CI is
/// dead.

function billing(over: Partial<Billing> = {}): Billing {
  return {
    org: "acme",
    plan: "paid",
    billable_seats: 3,
    paid_seats: 3,
    status: null,
    current_period_end: null,
    may_create_public: true,
    may_create_private: true,
    may_add_people: true,
    price_per_seat_cents: 400,
    paid_minutes_per_seat: 2000,
    free_minutes: 500,
    ...over,
  };
}

describe("readCiMinutes", () => {
  it("says nothing at all when the deployment does not meter", () => {
    // `STRATUM_RUNNER_MINUTES_PER_MONTH` unset. There is no budget, so
    // there is no number, and inventing one would be the page making up
    // a limit the server does not enforce.
    expect(readCiMinutes(billing()).kind).toBe("unmetered");
    expect(readCiMinutes(billing({ ci_minutes_limit: null })).kind).toBe(
      "unmetered",
    );
    expect(minutesLine(readCiMinutes(billing()))).toBe(null);
  });

  it("treats a budget of zero as metered and exhausted, not as unmetered", () => {
    // The assertion that pins the distinction. A `!limit` test folds
    // zero into "no budget" and would render "unlimited" over an
    // organisation whose every run is being refused.
    const m = readCiMinutes(
      billing({ ci_minutes_limit: 0, ci_minutes_used: 0 }),
    );
    expect(m.kind).toBe("metered");
    if (m.kind !== "metered") return;
    expect(m.exhausted).toBe(true);
    expect(m.percent).toBe(100);
  });

  it("prefers the server's own remaining figure to its own arithmetic", () => {
    // The refusal the reader is looking at was decided by the server's
    // number. If the two ever disagree — a window that rolled over
    // between the two figures, a job still running — showing our
    // subtraction would explain the refusal with a number that does not
    // match it.
    const m = readCiMinutes(
      billing({
        ci_minutes_limit: 2000,
        ci_minutes_used: 1240,
        ci_minutes_remaining: 42,
      }),
    );
    if (m.kind !== "metered") throw new Error("metered");
    expect(m.remaining).toBe(42);
  });

  it("falls back to the subtraction when only a limit and a usage arrive", () => {
    const m = readCiMinutes(
      billing({ ci_minutes_limit: 2000, ci_minutes_used: 1240 }),
    );
    if (m.kind !== "metered") throw new Error("metered");
    expect(m.remaining).toBe(760);
    expect(m.exhausted).toBe(false);
    expect(m.percent).toBe(62);
  });

  it("never reports a negative remainder or a bar past its end", () => {
    // Usage overshoots a budget routinely: a job that was inside it when
    // it was claimed finishes outside it, and a running job is never
    // killed for budget. "-40 minutes left" is arithmetic leaking onto
    // the page.
    const m = readCiMinutes(
      billing({ ci_minutes_limit: 100, ci_minutes_used: 140 }),
    );
    if (m.kind !== "metered") throw new Error("metered");
    expect(m.remaining).toBe(0);
    expect(m.exhausted).toBe(true);
    expect(m.percent).toBe(100);
  });
});

describe("minutesLine and minutesNote", () => {
  it("shows both figures, with thousands separated", () => {
    // "760 left" alone tells a reader nothing about whether that is
    // most of the month or the last of it.
    expect(
      minutesLine(
        readCiMinutes(
          billing({ ci_minutes_limit: 2000, ci_minutes_used: 1240 }),
        ),
      ),
    ).toBe("1,240 of 2,000 minutes used in the last 30 days");
  });

  it("says what is happening only when something is", () => {
    const fine = readCiMinutes(
      billing({ ci_minutes_limit: 2000, ci_minutes_used: 10 }),
    );
    expect(minutesNote(fine)).toBe(null);
    const spent = readCiMinutes(
      billing({ ci_minutes_limit: 2000, ci_minutes_used: 2000 }),
    );
    expect(minutesNote(spent)).toContain("refused");
    // A rolling 30-day window, not a calendar month: there is no reset
    // date to wait for, and "until the month rolls over" would have
    // somebody sitting on their hands until the 1st when the budget
    // actually frees up as the oldest jobs age out.
    expect(minutesNote(spent)).toContain("30-day window");
    expect(minutesNote(spent)).not.toContain("month");
    // And it says the thing a maintainer actually wonders about next.
    expect(minutesNote(spent)).toContain("runs already going are left alone");
  });
});

describe("githubMinutesNote", () => {
  it("says the GitHub share when there is one", () => {
    expect(
      githubMinutesNote(
        billing({
          ci_minutes_limit: 2000,
          ci_minutes_used: 1240,
          ci_minutes_github_used: 1200,
        }),
      ),
    ).toBe("of which 1,200 on GitHub Actions");
  });

  it("adds nothing when nothing came through GitHub, or the server is older", () => {
    // Both organisations read the minutes line byte for byte as they
    // always have: "of which 0" would be an advertisement.
    expect(
      githubMinutesNote(
        billing({
          ci_minutes_limit: 2000,
          ci_minutes_used: 12,
          ci_minutes_github_used: 0,
        }),
      ),
    ).toBe(null);
    expect(
      githubMinutesNote(
        billing({
          ci_minutes_limit: 2000,
          ci_minutes_used: 12,
          ci_minutes_github_used: null,
        }),
      ),
    ).toBe(null);
    expect(
      githubMinutesNote(
        billing({ ci_minutes_limit: 2000, ci_minutes_used: 12 }),
      ),
    ).toBe(null);
  });

  it("is silent on an unmetered deployment: a share of nothing", () => {
    expect(githubMinutesNote(billing({ ci_minutes_github_used: 30 }))).toBe(
      null,
    );
  });

  it("reads the share alongside the pools too", () => {
    expect(
      githubMinutesNote(
        billing({
          meters: {
            minutes: meter(),
            egress_gb: meter({ included: null }),
            storage_gb: meter({ included: null }),
          },
          ci_minutes_github_used: 40,
        }),
      ),
    ).toBe("of which 40 on GitHub Actions");
  });
});

/// A suspension is a different fact from an empty budget, and the
/// billing page had been silent about it: an organisation stopped for
/// mining saw "1,940 minutes left" here and learned why only by opening
/// a change. Minutes and suspension are independent — a suspended org
/// usually has plenty of minutes left, which is exactly why the minutes
/// panel alone reads as "everything is fine".
describe("readCiSuspension", () => {
  it("is nothing at all when the organisation is not suspended", () => {
    expect(readCiSuspension(billing())).toBe(null);
    expect(readCiSuspension(billing({ ci_suspended_reason: null }))).toBe(null);
  });

  it("treats an empty reason as not suspended", () => {
    // `NULL in ci_suspended_reason is the whole "not suspended" test`
    // (db.rs). An empty string is the same absence arriving badly, and
    // rendering it would put a panel headed "Hosted CI is suspended" on
    // the page with nothing under it.
    expect(readCiSuspension(billing({ ci_suspended_reason: "" }))).toBe(null);
    expect(readCiSuspension(billing({ ci_suspended_reason: "   " }))).toBe(
      null,
    );
  });

  it("carries the reason verbatim, untrimmed of its own words", () => {
    const s = readCiSuspension(
      billing({ ci_suspended_reason: "mining software detected: xmrig" }),
    );
    expect(s?.reason).toBe("mining software detected: xmrig");
  });

  it("passes the timestamp through as milliseconds", () => {
    // `ci_suspended_at` is epoch **milliseconds** — `runner_api.rs`
    // stores `now_ms()` and `billing_api.rs` hands it straight back —
    // and it reached me first as "seconds" in a brief. Converting one
    // that needs no converting dates a 2026 suspension to the year
    // 57000, which is the same class of wrong as the 1970 the other
    // direction gives, and just as obviously our bug rather than what
    // happened. The unit is asserted against the *server*, not against
    // what anybody said about it.
    const at = Date.UTC(2026, 7, 14, 12);
    const s = readCiSuspension(
      billing({ ci_suspended_reason: "mining", ci_suspended_at: at }),
    );
    expect(s?.atMs).toBe(at);
    expect(new Date(s!.atMs!).getUTCFullYear()).toBe(2026);
  });

  it("is still a suspension when the server sent no time", () => {
    // The reason is the fact; the time is a detail. A suspension with no
    // timestamp must still be reported, not swallowed.
    const s = readCiSuspension(billing({ ci_suspended_reason: "mining" }));
    expect(s).not.toBe(null);
    expect(s?.atMs).toBe(null);
    expect(
      readCiSuspension(
        billing({ ci_suspended_reason: "mining", ci_suspended_at: null }),
      )?.atMs,
    ).toBe(null);
  });

  it("says there is no self-serve way out, because there is not", () => {
    // Clearing a suspension is an operator action by SQL — there is no
    // route, deliberately (openapi, POST /v1/runner/jobs/{id}/finish).
    // A page that implied "contact billing" or offered a button would
    // send somebody looking for a control that does not exist.
    expect(SUSPENSION_NOTE).toContain("Hosted workflows are refused");
    expect(SUSPENSION_NOTE).toContain("cancelled");
    expect(SUSPENSION_NOTE).toContain("operator");
    // Pushing still works. The workflow is created and immediately
    // blocked, so telling somebody their pushes are blocked would be
    // wrong in the direction that makes them stop working.
    expect(SUSPENSION_NOTE).toContain("Pushing and landing still work");
  });
});
