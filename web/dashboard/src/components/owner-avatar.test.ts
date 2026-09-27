import { describe, expect, it } from "vitest";
import { initials } from "./owner-avatar";

describe("initials", () => {
  it("takes the first and last word of a name", () => {
    expect(initials("Ada Lovelace")).toBe("AL");
    expect(initials("Ada Byron King Lovelace")).toBe("AL");
  });

  it("takes two letters from a single word — a handle is one word", () => {
    expect(initials("stratum")).toBe("ST");
    expect(initials("x")).toBe("X");
  });

  it("never renders an empty square", () => {
    // A display name is user-controlled and may be whitespace; a blank
    // fallback reads as a broken image rather than as a person.
    expect(initials("   ")).toBe("?");
    expect(initials("")).toBe("?");
    expect(initials("  Ada   Lovelace  ")).toBe("AL");
  });
});
