import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  formatAgo,
  formatBytes,
  formatCount,
  formatFingerprint,
  formatIn,
  formatMs,
} from "./format";

describe("formatBytes", () => {
  it("scales through binary units", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(999)).toBe("999 B");
    expect(formatBytes(1024)).toBe("1.0 KiB");
    expect(formatBytes(1536)).toBe("1.5 KiB");
    expect(formatBytes(5 * 1024 * 1024)).toBe("5.0 MiB");
    expect(formatBytes(200 * 1024 * 1024)).toBe("200 MiB");
    expect(formatBytes(3 * 1024 ** 3)).toBe("3.0 GiB");
  });
  it("handles junk", () => {
    expect(formatBytes(-1)).toBe("–");
    expect(formatBytes(NaN)).toBe("–");
  });
});

describe("formatMs", () => {
  it("switches to seconds past 1000", () => {
    expect(formatMs(0)).toBe("0 ms");
    expect(formatMs(999)).toBe("999 ms");
    expect(formatMs(1500)).toBe("1.5 s");
    expect(formatMs(12_000)).toBe("12 s");
  });
  it("null-safe", () => {
    expect(formatMs(null)).toBe("–");
    expect(formatMs(undefined)).toBe("–");
  });
});

describe("formatCount", () => {
  it("compacts thousands and millions", () => {
    expect(formatCount(999)).toBe("999");
    expect(formatCount(1500)).toBe("1.5k");
    expect(formatCount(25_000)).toBe("25k");
    expect(formatCount(1_500_000)).toBe("1.5M");
  });
});

// The relative formatters read the clock themselves. A test that reads
// it too, a moment earlier, is racing them: `formatIn(now + 7d)` is
// `7d - 1ms` by the time the formatter looks, which floors to "in 6d"
// whenever the runner takes a millisecond between the two reads — and
// CI's did, once, and the same commit passed on the re-run. The `+ 1`
// slack the other assertions carried was the same race papered over one
// millisecond wide. The clock is frozen instead, so both reads agree.
const FROZEN = Date.UTC(2026, 8, 6, 12, 0, 0);
beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(FROZEN);
});
afterEach(() => {
  vi.useRealTimers();
});

describe("formatAgo", () => {
  it("buckets sensibly", () => {
    const now = FROZEN;
    expect(formatAgo(null)).toBe("never");
    expect(formatAgo(now - 5_000)).toBe("5s ago");
    expect(formatAgo(now - 5 * 60_000)).toBe("5m ago");
    expect(formatAgo(now - 3 * 3_600_000)).toBe("3h ago");
    expect(formatAgo(now - 72 * 3_600_000)).toBe("3d ago");
  });
});

describe("formatIn", () => {
  it("names a future instant as a countdown, not as the past", () => {
    // An invitation expiring in 7 days rendered as "expires just now",
    // because the only relative formatter assumed its argument was
    // behind the clock. A future timestamp is a countdown.
    const now = FROZEN;
    expect(formatIn(now + 7 * 24 * 3_600_000)).toBe("in 7d");
    expect(formatIn(now + 3 * 3_600_000)).toBe("in 3h");
    expect(formatIn(now + 5 * 60_000)).toBe("in 5m");
    expect(formatIn(now + 30_000)).toBe("in 30s");
  });
  it("says so once the instant has passed", () => {
    const now = FROZEN;
    expect(formatIn(now - 1)).toBe("expired");
    expect(formatIn(now - 2 * 3_600_000)).toBe("expired");
    expect(formatIn(null)).toBe("never");
  });
});

describe("formatFingerprint", () => {
  it("keeps the kind prefix and both ends of the digest", () => {
    expect(
      formatFingerprint("SHA256:xyrwjwNKqTIivpsCwlGBJoCJCtNo5voQKyNc3jGN9iI"),
    ).toBe("SHA256:xyrwjwNK…N9iI");
  });
  it("leaves short or unprefixed values alone", () => {
    expect(formatFingerprint("SHA256:short")).toBe("SHA256:short");
    expect(formatFingerprint("plain")).toBe("plain");
  });
});
