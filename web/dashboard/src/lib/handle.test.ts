// The handle a new account gets when the person accepting an invitation
// leaves the field empty. The accept form shows it as the field's
// placeholder, so it has to be the name the server will actually make —
// these are the cases `handle_from`'s own tests in
// `crates/stratum-control/src/registry.rs` pin, plus the ones the
// invitation form is most likely to meet.

import { describe, expect, it } from "vitest";

import { handleFrom, handleFromAddress } from "./handle";

describe("handleFrom", () => {
  it("keeps a name that is already a handle", () => {
    expect(handleFrom("ada")).toBe("ada");
    expect(handleFrom("ada-lovelace")).toBe("ada-lovelace");
    expect(handleFrom("ada_lovelace")).toBe("ada_lovelace");
    expect(handleFrom("ada2")).toBe("ada2");
  });

  it("lowercases, because namespaces here are lowercase", () => {
    expect(handleFrom("AdaLovelace")).toBe("adalovelace");
  });

  it("turns every run of anything else into one dash, never a dot", () => {
    expect(handleFrom("dev.eloper")).toBe("dev-eloper");
    expect(handleFrom("Ada+Spool")).toBe("ada-spool");
    expect(handleFrom("ada..lovelace")).toBe("ada-lovelace");
    expect(handleFrom("ada/../root")).toBe("ada-root");
    expect(handleFrom("a b\tc")).toBe("a-b-c");
    // Outside ASCII is outside the alphabet, however it is written.
    expect(handleFrom("Zoë")).toBe("zo");
    expect(handleFrom("zoë.x")).toBe("zo-x");
  });

  it("never starts or ends with a dash or an underscore", () => {
    // A leading dash reads as a flag in every command line the name
    // appears in.
    expect(handleFrom("-ada-")).toBe("ada");
    expect(handleFrom("_ada_")).toBe("ada");
    expect(handleFrom(".ada")).toBe("ada");
    expect(handleFrom("ada.")).toBe("ada");
  });

  it("is `user` when nothing usable is left", () => {
    expect(handleFrom("..")).toBe("user");
    expect(handleFrom("---")).toBe("user");
    expect(handleFrom("!!!")).toBe("user");
    expect(handleFrom("")).toBe("user");
  });

  it("is at most 60 characters, and a cut never leaves a dash behind", () => {
    expect(handleFrom("a".repeat(200))).toBe("a".repeat(60));
    // Character 60 is the dash: the cut keeps it, and the trim after the
    // cut is what takes it off again.
    expect(handleFrom(`${"a".repeat(59)}.b`)).toBe("a".repeat(59));
    expect(handleFrom(`${"a".repeat(59)}_b`)).toBe("a".repeat(59));
    for (const seed of [
      "x".repeat(61),
      `${"ab.".repeat(40)}`,
      `${"a".repeat(58)}..bb`,
      `${"a".repeat(60)}-`,
    ]) {
      const h = handleFrom(seed);
      expect(h.length).toBeLessThanOrEqual(60);
      expect(h).not.toMatch(/[-_]$/);
      expect(h).toMatch(/^[a-z0-9]/);
    }
  });

  it("only ever makes names from the handle alphabet", () => {
    for (const seed of ["ada", "A.B+C", "ζ", "💥ada💥", "a\u0000b", "..", ""]) {
      expect(handleFrom(seed)).toMatch(/^[a-z0-9]([a-z0-9_-]*[a-z0-9])?$/);
    }
  });
});

describe("handleFromAddress", () => {
  it("uses the part of the invited address before the @", () => {
    expect(handleFromAddress("dev.eloper@acme.dev")).toBe("dev-eloper");
    expect(handleFromAddress("Ada+Spool@example.com")).toBe("ada-spool");
    expect(handleFromAddress("..@acme.dev")).toBe("user");
    // The first `@`, as the server splits it.
    expect(handleFromAddress("a@b@acme.dev")).toBe("a");
  });
});
