import { describe, expect, it } from "vitest";
import { absoluteTime, isoTime } from "./relative-time";

describe("the timestamp a relative time hangs off", () => {
  it("round-trips the instant exactly in the machine-readable half", () => {
    const at = Date.UTC(2026, 7, 26, 14, 5, 9);
    expect(isoTime(at)).toBe("2026-08-26T14:05:09.000Z");
    expect(Date.parse(isoTime(at))).toBe(at);
  });

  it("renders the absolute form in the viewer's own zone", () => {
    // The dashboard shipped a date filter once that read a picked day
    // as UTC midnight rather than the reader's day. The property that
    // catches that class is this one: the rendered text must agree
    // with the *local* calendar fields of the same instant, not the
    // UTC ones. Written against a fixed instant it would only be true
    // in whichever zone the author happened to be in, which is how a
    // test starts passing or failing by timezone.
    const at = Date.UTC(2026, 0, 1, 3, 30, 0);
    const local = new Date(at);
    const text = absoluteTime(at);
    expect(text).toContain(String(local.getFullYear()));
    expect(text).toMatch(new RegExp(`\\b0?${local.getDate()}\\b`));
  });

  it("separates two instants a minute apart", () => {
    const at = Date.UTC(2026, 7, 26, 14, 5, 0);
    expect(absoluteTime(at)).not.toBe(absoluteTime(at + 60_000));
  });
});
