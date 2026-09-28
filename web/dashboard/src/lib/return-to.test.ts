// The address a sign-in round trip through a provider comes back to.
// What is pinned: only a path on this site is ever kept or used, it is
// used once, any return spends it, and storage that refuses is not an
// error.

import { describe, expect, it } from "vitest";

import {
  rememberReturn,
  RETURN_TO_KEY,
  signinReturnOf,
  takeReturn,
} from "./return-to";

function memory(initial: Record<string, string> = {}) {
  const m = new Map(Object.entries(initial));
  return {
    m,
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
    removeItem: (k: string) => void m.delete(k),
  };
}

/// Storage that refuses every call, as a private window with site data
/// blocked does.
const refusing = {
  getItem: () => {
    throw new Error("SecurityError");
  },
  setItem: () => {
    throw new Error("SecurityError");
  },
  removeItem: () => {
    throw new Error("SecurityError");
  },
};

const OFF_SITE = [
  "//evil.example",
  "//evil.example/acme/widget",
  "https://evil.example",
  "https://evil.example/acme/widget",
  "/\\evil.example/p",
  "javascript:alert(1)",
];

describe("rememberReturn", () => {
  it("keeps a path on this site", () => {
    const s = memory();
    rememberReturn(s, "/acme/widget/issues?q=is%3Aopen");
    expect(s.m.get(RETURN_TO_KEY)).toBe("/acme/widget/issues?q=is%3Aopen");
  });

  it("keeps nothing that leaves this site, and forgets what was there", () => {
    for (const next of OFF_SITE) {
      const s = memory({ [RETURN_TO_KEY]: "/acme/old" });
      rememberReturn(s, next);
      expect(s.m.has(RETURN_TO_KEY), next).toBe(false);
    }
  });

  it("leaving with nowhere to return to forgets an older destination", () => {
    for (const next of [undefined, null, "", "/"]) {
      const s = memory({ [RETURN_TO_KEY]: "/acme/old" });
      rememberReturn(s, next);
      expect(s.m.has(RETURN_TO_KEY), String(next)).toBe(false);
    }
  });

  it("does not throw when storage refuses", () => {
    expect(() => rememberReturn(refusing, "/acme/widget")).not.toThrow();
    expect(() => rememberReturn(null, "/acme/widget")).not.toThrow();
  });
});

describe("takeReturn", () => {
  it("gives the kept path back once", () => {
    const s = memory({ [RETURN_TO_KEY]: "/acme/widget" });
    expect(takeReturn(s)).toBe("/acme/widget");
    expect(s.m.has(RETURN_TO_KEY)).toBe(false);
    expect(takeReturn(s)).toBeNull();
  });

  it("checks what comes back again, because storage is not only ours to write", () => {
    for (const kept of OFF_SITE) {
      const s = memory({ [RETURN_TO_KEY]: kept });
      expect(takeReturn(s), kept).toBeNull();
      // Spent even though it was not used.
      expect(s.m.has(RETURN_TO_KEY), kept).toBe(false);
    }
  });

  it("answers nothing, and does not throw, when storage refuses or is empty", () => {
    expect(takeReturn(refusing)).toBeNull();
    expect(takeReturn(null)).toBeNull();
    expect(takeReturn(memory())).toBeNull();
  });
});

describe("signinReturnOf", () => {
  it("tells a success from any other return, and from no return at all", () => {
    expect(signinReturnOf(new URLSearchParams("sso=ok"))).toBe("ok");
    expect(signinReturnOf(new URLSearchParams("github=ok"))).toBe("ok");
    for (const q of ["sso=denied", "github=noaccount", "sso=", "github=new", "sso=nonsense"])
      expect(signinReturnOf(new URLSearchParams(q)), q).toBe("other");
    for (const q of ["", "connect=ok", "next=%2Facme", "ok=sso"])
      expect(signinReturnOf(new URLSearchParams(q)), q).toBeNull();
  });
});
