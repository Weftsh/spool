import { describe, expect, it } from "vitest";
import { href, segments } from "./router";
import { nameFromOrigin } from "./views/newrepo";

// The examples are repository addresses — `/{owner}/{repo}/tree/…`,
// the one address a repository has. They used to be `["repos", …]`,
// which was a route while the dashboard had a repository space of its
// own; an example that spells a dead route teaches the next reader a URL
// this app does not answer.
describe("href", () => {
  it("escapes every segment so a path cannot break out of its URL", () => {
    expect(href(["acme", "app"])).toBe("/acme/app");
    // A repository or a path with a slash, a space or a hash in it must
    // not silently become extra segments or a fragment.
    expect(href(["acme", "a b", "tree", "src/main.rs"])).toBe(
      "/acme/a%20b/tree/src%2Fmain.rs",
    );
    expect(href(["acme", "x#y?z"])).toBe("/acme/x%23y%3Fz");
  });

  it("omits empty query values rather than writing at=", () => {
    // The default branch is the *absence* of a revision: a permalink to
    // "whatever is current" must not pin itself to today's branch name.
    expect(href(["acme", "app"], { at: undefined })).toBe("/acme/app");
    expect(href(["acme", "app"], { at: "" })).toBe("/acme/app");
    expect(href(["acme", "app"], { at: "side" })).toBe("/acme/app?at=side");
  });
});

describe("segments", () => {
  it("decodes what href encoded, round trip", () => {
    for (const parts of [
      ["acme", "app"],
      ["acme", "a b", "tree", "src/main.rs"],
      ["acme", "app", "tree", "dir", "file.txt"],
    ]) {
      const url = href(parts);
      expect(segments(url.split("?")[0])).toEqual(parts);
    }
  });

  it("drops empty segments so a doubled slash is not a nameless directory", () => {
    expect(segments("/")).toEqual([]);
    expect(segments("//acme//app//")).toEqual(["acme", "app"]);
  });

  it("leaves a malformed escape alone instead of throwing", () => {
    // A URL somebody pasted badly should show an empty directory, not a
    // blank page — `decodeURIComponent` throws on a stray percent.
    expect(segments("/acme/%E0%A4%A")).toEqual(["acme", "%E0%A4%A"]);
  });
});

import { parseIdent } from "./api";

describe("parseIdent", () => {
  it("takes the name and the time out of a git identity line", () => {
    // The shape `/log` really returns, including the machine-generated
    // one a token-authored REST commit produces.
    expect(parseIdent("Ada Owner <ada@acme.test> 1787406946 +0000")).toEqual({
      name: "Ada Owner",
      time: 1787406946000,
    });
    expect(
      parseIdent("token:01hx <token:01hx@stratum.local> 1787406946 +0000"),
    ).toEqual({ name: "token:01hx", time: 1787406946000 });
  });

  it("is tolerant, because a bad ident should cost a timestamp not a page", () => {
    expect(parseIdent("Nobody")).toEqual({ name: "Nobody", time: 0 });
    expect(parseIdent("")).toEqual({ name: "", time: 0 });
    expect(parseIdent("<only@address>")).toEqual({
      name: "only@address",
      time: 0,
    });
  });
});

describe("nameFromOrigin", () => {
  it("takes the repository name out of whatever somebody pasted", () => {
    for (const [input, want] of [
      ["github.com/acme/widget", "widget"],
      ["https://github.com/acme/widget", "widget"],
      ["https://github.com/acme/widget.git", "widget"],
      ["https://github.com/acme/widget/", "widget"],
      ["acme/widget", "widget"],
      ["git@github.com:acme/widget.git", "widget"],
      // The browser URL, with the bit GitHub adds when you are looking
      // at a branch. Pasting that is the ordinary case, not a mistake.
      ["https://github.com/acme/widget/tree/main", "widget"],
      ["https://github.com/acme/widget/blob/main/src/lib.rs", "widget"],
      ["https://github.com/acme/widget?tab=readme", "widget"],
      ["", ""],
    ] as [string, string][]) {
      expect([input, nameFromOrigin(input)]).toEqual([input, want]);
    }
  });
});
