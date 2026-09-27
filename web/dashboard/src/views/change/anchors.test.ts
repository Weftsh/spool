import { describe, expect, it } from "vitest";
import { lineAnchor, parseLineAnchor } from "./anchors";

describe("line anchors", () => {
  it("round-trips a path and a line", () => {
    expect(lineAnchor("payments/gateway.rs", 42)).toBe(
      "#payments/gateway.rs:L42",
    );
    expect(parseLineAnchor("#payments/gateway.rs:L42")).toEqual({
      path: "payments/gateway.rs",
      line: 42,
    });
  });

  // The line number is at the *end*, so the split is the last `:L` and
  // not the first: a path may contain one, and splitting at the first
  // would hand back half a path and a line number that is not a number.
  it("reads a path that itself contains a colon, and one that contains :L", () => {
    expect(parseLineAnchor("#docs/a:b.md:L7")).toEqual({
      path: "docs/a:b.md",
      line: 7,
    });
    expect(parseLineAnchor("#docs/a:Lab.md:L7")).toEqual({
      path: "docs/a:Lab.md",
      line: 7,
    });
  });

  it("refuses everything that is not an anchor", () => {
    for (const hash of [
      "",
      "#",
      "#payments/gateway.rs",
      "#payments/gateway.rs:L",
      "#payments/gateway.rs:L0",
      "#payments/gateway.rs:Lx",
      "#:L4",
      "#%E0%A4%A",
    ]) {
      expect(parseLineAnchor(hash)).toBeNull();
    }
  });

  it("decodes a path the browser percent-encoded", () => {
    expect(parseLineAnchor("#a%20b/c.rs:L3")).toEqual({
      path: "a b/c.rs",
      line: 3,
    });
  });
});
