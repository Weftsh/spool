import { describe, expect, it } from "vitest";
import { readDiffStyle, writeDiffStyle } from "./diff-prefs";

function memory(): Storage {
  const m = new Map<string, string>();
  return {
    getItem: (k) => m.get(k) ?? null,
    setItem: (k, v) => void m.set(k, v),
    removeItem: (k) => void m.delete(k),
    clear: () => m.clear(),
    key: () => null,
    length: 0,
  };
}

describe("the diff layout preference", () => {
  it("is unified until somebody says otherwise", () => {
    expect(readDiffStyle(memory())).toBe("unified");
    expect(readDiffStyle(null)).toBe("unified");
  });
  it("remembers split, and only split", () => {
    const s = memory();
    writeDiffStyle(s, "split");
    expect(readDiffStyle(s)).toBe("split");
    writeDiffStyle(s, "unified");
    expect(readDiffStyle(s)).toBe("unified");
  });
  it("treats a value it did not write as the default", () => {
    const s = memory();
    s.setItem("stratum-diff-style", "sideways");
    expect(readDiffStyle(s)).toBe("unified");
  });
  it("survives a storage that throws", () => {
    const broken = {
      getItem: () => {
        throw new Error("denied");
      },
      setItem: () => {
        throw new Error("denied");
      },
    };
    expect(readDiffStyle(broken)).toBe("unified");
    expect(() => writeDiffStyle(broken, "split")).not.toThrow();
  });
});
