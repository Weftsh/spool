// The suggestion arithmetic: what is a suggestion block, whether the
// server would take one, and which lines it stands in place of.
//
// **The parse cases below are the server's own, deliberately.** They
// are transcribed from `review::suggestion::parse`'s Rust tests, one
// for one, because the only thing that makes this parser correct is
// that it agrees with that one: a client that reads a fence more
// loosely draws an Apply the server refuses, and one that reads it more
// strictly hides a control that works. When that file's rules change,
// these are the tests that go red — which is the point of writing them
// twice rather than trusting two prose descriptions to stay equal.

import { describe, expect, it } from "vitest";
import type { ChangeComment } from "@/api";
import {
  MAX_APPLY,
  anchorRange,
  applyAnchor,
  applyLabel,
  applyRefusal,
  displayAnchor,
  replacedLines,
  splitSuggestions,
  suggestionsIn,
  suggestionsSupported,
} from "@/views/change/suggestions";

function comment(over: Partial<ChangeComment> = {}): ChangeComment {
  return {
    id: "01c1",
    patchset: 1,
    author: "Olive Owner",
    author_email: "olive@acme.test",
    author_principal: "user:01owner",
    path: "payments/gateway.rs",
    line: 4,
    line_end: 4,
    side: "new",
    pending: false,
    body: "```suggestion\nlet fee = round(x);\n```",
    created_at: 0,
    ...over,
  };
}

const lines = (body: string) => suggestionsIn(body).map((s) => s.lines);

describe("suggestionsIn", () => {
  it("finds a block, and an empty one is a deletion rather than nothing", () => {
    expect(lines("just a remark, no code at all")).toEqual([]);
    expect(lines("")).toEqual([]);
    expect(lines("try this:\n```suggestion\nlet x = 1;\n```\nthanks")).toEqual([
      ["let x = 1;"],
    ]);
    // The distinction the whole feature turns on: no block at all is
    // `[]`, and a block with no lines is "delete these lines" — one
    // suggestion carrying nothing. They must never collapse together.
    expect(lines("drop it:\n```suggestion\n```\n")).toEqual([[]]);
    // A blank line inside is a suggestion of one empty line, which is
    // not a deletion.
    expect(lines("```suggestion\n\n```")).toEqual([[""]]);
  });

  it("returns several blocks in the order they were written", () => {
    expect(
      lines(
        "first:\n```suggestion\na\n```\nand also:\n```suggestion\nb\nc\n```",
      ),
    ).toEqual([["a"], ["b", "c"]]);
  });

  it("yields nothing for a fence that never closes, and swallows the rest", () => {
    expect(lines("```suggestion\nlet x = 1;\n")).toEqual([]);
    expect(lines("```suggestion\na\n```\nthen:\n```suggestion\nb\n")).toEqual([
      ["a"],
    ]);
  });

  it("reads any other info string as somebody's code sample", () => {
    expect(lines("```rust\nlet x = 1;\n```")).toEqual([]);
    expect(lines("```\nplain\n```")).toEqual([]);
    expect(lines("```suggestion rust\nx\n```")).toEqual([]);
    // GitHub's anchor-moving form, which this product does not
    // implement: reading it as a plain suggestion would propose the
    // text against lines the reviewer was not talking about.
    expect(lines("```suggestion:-0+2\nx\n```")).toEqual([]);
    // A suggestion quoted inside another fence is being shown, not
    // proposed.
    expect(
      lines("here is what not to do:\n```rust\n```suggestion\nbad\n```\n"),
    ).toEqual([]);
  });

  it("lets a longer fence carry a shorter one inside it", () => {
    expect(lines("````suggestion\n```\ncode\n```\n````")).toEqual([
      ["```", "code", "```"],
    ]);
  });

  it("parses a CRLF body and keeps no carriage returns in the replacement", () => {
    // A browser posts a textarea as CRLF. Matching a fence with the
    // `\r` still on it finds nothing — the reviewer would see their
    // block rendered and the author would see no Apply.
    expect(lines("do this:\r\n```suggestion\r\nlet x = 1;\r\n```\r\n")).toEqual(
      [["let x = 1;"]],
    );
  });

  it("strips the opening fence's indentation and keeps the rest", () => {
    expect(
      lines("- like so:\n  ```suggestion\n  let x = 1;\n      deep\n  ```"),
    ).toEqual([["let x = 1;", "    deep"]]);
  });
});

describe("splitSuggestions", () => {
  it("hands the prose around a block back for the markdown renderer", () => {
    expect(
      splitSuggestions("try this:\n```suggestion\nlet x = 1;\n```\nthanks"),
    ).toEqual([
      { kind: "text", text: "try this:" },
      { kind: "suggestion", lines: ["let x = 1;"] },
      { kind: "text", text: "thanks" },
    ]);
  });

  it("leaves a non-suggestion fence inside the prose it was written in", () => {
    // Whole, and still fenced: cutting it out would hand `Markdown` an
    // opening fence with no close and turn the rest of the comment into
    // a code block.
    expect(splitSuggestions("look:\n```rust\nlet x = 1;\n```\nsee?")).toEqual([
      { kind: "text", text: "look:\n```rust\nlet x = 1;\n```\nsee?" },
    ]);
  });

  it("keeps a comment with no suggestion as one run of prose", () => {
    expect(splitSuggestions("ship it")).toEqual([
      { kind: "text", text: "ship it" },
    ]);
  });
});

describe("applyAnchor", () => {
  it("answers the file and the inclusive range for an appliable comment", () => {
    expect(applyAnchor(comment({ line: 4, line_end: 6 }))).toEqual({
      path: "payments/gateway.rs",
      start: 4,
      end: 6,
      patchset: 1,
    });
  });

  it("falls back to the start line when the row carries no line_end", () => {
    expect(applyAnchor(comment({ line: 9, line_end: null }))?.end).toBe(9);
  });

  it("refuses a draft, which is not a remark yet", () => {
    // Applying one would publish it in the one place its author cannot
    // take it back from.
    expect(applyAnchor(comment({ pending: true }))).toBeNull();
  });

  it("refuses a comment with nothing to replace", () => {
    expect(applyAnchor(comment({ path: null, line: null }))).toBeNull();
    expect(applyAnchor(comment({ line: null, line_end: null }))).toBeNull();
  });

  it("refuses the old side, which has no line in the file to stand in", () => {
    expect(applyAnchor(comment({ side: "old" }))).toBeNull();
    // A server older than migration 0050 sends no side at all, and its
    // rows cannot be applied either.
    expect(applyAnchor(comment({ side: undefined }))).toBeNull();
  });

  it("refuses a comment with no block, and one with two", () => {
    expect(applyAnchor(comment({ body: "please round this" }))).toBeNull();
    expect(
      applyAnchor(
        comment({ body: "```suggestion\na\n```\nor\n```suggestion\nb\n```" }),
      ),
    ).toBeNull();
  });

  it("takes an empty block, which is a suggestion to delete the lines", () => {
    expect(applyAnchor(comment({ body: "```suggestion\n```" }))).not.toBeNull();
  });
});

describe("displayAnchor", () => {
  it("still says which lines a comment nobody can apply is about", () => {
    // A drafted suggestion and one carrying two blocks are both drawn
    // against the lines they replace; neither gets a control. Folding
    // the two questions together would render those as a plain fence
    // with no sign of what they stand in place of.
    for (const c of [
      comment({ pending: true }),
      comment({ body: "```suggestion\na\n```\nor\n```suggestion\nb\n```" }),
    ]) {
      expect(applyAnchor(c)).toBeNull();
      expect(displayAnchor(c)).toEqual({
        path: "payments/gateway.rs",
        start: 4,
        end: 4,
        patchset: 1,
      });
    }
  });

  it("has nothing to say about a comment that names no line", () => {
    expect(displayAnchor(comment({ path: null, line: null }))).toBeNull();
    expect(displayAnchor(comment({ side: "old" }))).toBeNull();
  });
});

describe("suggestionsSupported", () => {
  it("reads the presence of side, not its value", () => {
    expect(suggestionsSupported([comment({ side: "old" })])).toBe(true);
    expect(suggestionsSupported([comment({ side: "new" })])).toBe(true);
  });

  it("says no for a conversation from a server older than threads", () => {
    // Absent, not empty: a pre-0050 row has no `side` key at all, and
    // the apply route cannot know which lines it would replace.
    expect(suggestionsSupported([comment({ side: undefined })])).toBe(false);
    expect(suggestionsSupported([])).toBe(false);
    expect(suggestionsSupported(null)).toBe(false);
  });
});

describe("replacedLines", () => {
  it("reads the anchored lines out of the file", () => {
    expect(replacedLines("a\nb\nc\n", 2, 3)).toEqual(["b", "c"]);
    expect(replacedLines("a\nb\nc", 1, 1)).toEqual(["a"]);
  });

  it("answers null for a range the file does not have", () => {
    // Not `[]`: an empty answer would draw a mini-diff claiming the
    // reviewer proposed to add their lines to nothing, where the truth
    // is that the file has moved and the server will refuse in words.
    expect(replacedLines("a\nb\n", 2, 3)).toBeNull();
    expect(replacedLines("", 1, 1)).toBeNull();
    expect(replacedLines("a\n", 0, 1)).toBeNull();
    expect(replacedLines("a\n", 2, 1)).toBeNull();
  });

  it("does not count the terminator of the last line as a line", () => {
    // "a\nb\n" is two lines, not three. Counting the trailing empty
    // string would make an off-by-one anchor look reachable.
    expect(replacedLines("a\nb\n", 3, 3)).toBeNull();
  });
});

describe("the words the page counts with", () => {
  it("names one line and a range differently", () => {
    const at = (start: number, end: number) => ({
      path: "a.rs",
      start,
      end,
      patchset: 1,
    });
    expect(anchorRange(at(4, 4))).toBe("line 4");
    expect(anchorRange(at(4, 6))).toBe("lines 4–6");
  });

  it("counts one suggestion in the singular", () => {
    expect(applyLabel(1)).toBe("1 suggestion");
    expect(applyLabel(2)).toBe("2 suggestions");
  });

  it("mirrors the server's two refusals before the trip", () => {
    expect(applyRefusal(0)).toMatch(/at least one/);
    expect(applyRefusal(1)).toBeNull();
    expect(applyRefusal(MAX_APPLY)).toBeNull();
    expect(applyRefusal(MAX_APPLY + 1)).toMatch(/more than the 50/);
  });
});
