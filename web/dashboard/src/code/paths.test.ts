import { describe, expect, it } from "vitest";
import { ancestorsOf, schemeFrom, SYNTAX_DEFAULTS, treeHeight, treeRows } from "./paths";

describe("ancestorsOf", () => {
  it("names every directory above a nested file, outermost first", () => {
    expect(ancestorsOf("src/deep/nested.txt")).toEqual(["src/", "src/deep/"]);
  });
  it("has nothing to say about a root-level file", () => {
    expect(ancestorsOf("README.md")).toEqual([]);
  });
  it("ignores a stray leading or doubled slash", () => {
    expect(ancestorsOf("/src//main.rs")).toEqual(["src/"]);
  });
});

describe("schemeFrom", () => {
  it("reads a page that follows the system as system", () => {
    expect(schemeFrom("light dark")).toBe("system");
    expect(schemeFrom("dark light")).toBe("system");
  });
  it("reads a pinned page as its pin", () => {
    expect(schemeFrom("dark")).toBe("dark");
    expect(schemeFrom("light")).toBe("light");
    expect(schemeFrom("only light")).toBe("light");
  });
  it("falls back to system for anything else", () => {
    expect(schemeFrom("")).toBe("system");
    expect(schemeFrom("normal")).toBe("system");
  });
});

describe("the syntax theme carries no colour of its own", () => {
  // The do-not list: colours change in web/shared/tokens.css only. A hex
  // here would be a second place to edit, and the one nobody would find.
  for (const [key, value] of Object.entries(SYNTAX_DEFAULTS)) {
    it(`${key} resolves a variable`, () => {
      expect(value).toMatch(/^var\(--[\w-]+\)$/);
    });
  }
});

describe("treeRows", () => {
  it("counts files and the directories that hold them", () => {
    expect(treeRows(["README.md", "src/main.rs", "src/lib.rs"])).toBe(4);
  });
  it("folds a chain of single directories into one row", () => {
    // docs/guide/ is one row, as the tree draws it.
    expect(treeRows(["docs/guide/intro.md"])).toBe(2);
    // but a directory with a file and a subdirectory is its own row.
    expect(treeRows(["docs/README.md", "docs/guide/intro.md"])).toBe(4);
  });
  it("sizes a box to its rows, up to a ceiling", () => {
    expect(treeHeight(2, 400)).toBe(68);
    expect(treeHeight(50, 400)).toBe(400);
  });
});
