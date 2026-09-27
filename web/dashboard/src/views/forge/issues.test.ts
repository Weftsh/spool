import { describe, expect, it } from "vitest";
import type { IssueComment } from "@/api";
import {
  DEFAULT_QUERY,
  authorName,
  bySeq,
  issuesRoute,
  commentWord,
  formatQuery,
  parseQuery,
  toApiQuery,
  type IssueFilter,
} from "./issues";

// The query bar is the API: every control on the index writes into that
// text box and the text box is the URL. So these are not string-handling
// tests, they are the tests for the page's whole filtering contract —
// if `parseQuery` and `formatQuery` disagree anywhere, a dropdown and
// the URL it produced say different things about the same list.

describe("parseQuery", () => {
  it("reads the default the index opens with", () => {
    expect(parseQuery(DEFAULT_QUERY)).toEqual({
      state: "open",
      label: null,
      author: null,
      sort: "newest",
      text: "",
    });
  });

  it("treats the absence of a state token as ALL, not as open", () => {
    // GitHub's semantics, and the reason they are right is that
    // deleting `is:open` from the box has to do something. If absence
    // meant "open" there would be no way to ask for both from the bar,
    // and the bar would stop being the API.
    expect(parseQuery("is:issue").state).toBe("all");
    expect(parseQuery("").state).toBe("all");
    expect(parseQuery("is:closed").state).toBe("closed");
  });

  it("keeps a quoted label whole", () => {
    // `good first issue` is the label every "help wanted" list on
    // GitHub is built from, and it is the reason quoting exists at all.
    const f = parseQuery('is:issue label:"good first issue" is:open');
    expect(f.label).toBe("good first issue");
    expect(f.state).toBe("open");
    // The quoted run must not leak into the free-text search either —
    // both halves, because a parser that got the label right and still
    // searched for `first` would filter to nothing.
    expect(f.text).toBe("");
  });

  it("reads author and sort", () => {
    const f = parseQuery("is:issue author:ada sort:oldest");
    expect(f.author).toBe("ada");
    expect(f.sort).toBe("oldest");
  });

  it("ignores a sort nobody offers rather than passing it on", () => {
    // Both halves: it must not become the sort, and it must not fall
    // through into the free-text search either, where it would filter
    // the list to nothing and read as "no issues match".
    for (const bad of ["chaos", "updated", "comments", ""]) {
      expect(parseQuery(`sort:${bad}`).sort, bad).toBe("newest");
      expect(parseQuery(`sort:${bad}`).text, bad).toBe("");
    }
  });

  it("keeps unrecognised words as words to search for", () => {
    // A typo'd `athor:ada` silently matching every issue is worse than
    // it matching none: the maintainer reads the list as the answer.
    const f = parseQuery("is:issue crash on push athor:ada");
    expect(f.author).toBeNull();
    expect(f.text).toBe("crash on push athor:ada");
  });

  it("does not let an empty value stand in for a filter", () => {
    // `author:` is not "the person whose handle is the empty string",
    // it is somebody halfway through typing.
    expect(parseQuery("author:").author).toBeNull();
    expect(parseQuery("label:").label).toBeNull();
  });
});

describe("formatQuery", () => {
  const cases: IssueFilter[] = [
    { state: "open", label: null, author: null, sort: "newest", text: "" },
    { state: "all", label: null, author: null, sort: "newest", text: "" },
    { state: "closed", label: "bug", author: "ada", sort: "oldest", text: "" },
    {
      state: "open",
      label: "good first issue",
      author: null,
      sort: "oldest",
      text: "crash on push",
    },
    { state: "all", label: null, author: "bo", sort: "oldest", text: "panic" },
  ];

  it("round-trips every filter the controls can build", () => {
    // The dropdowns and the text box are one piece of state, not two
    // copies of it, and this is the property that makes that true.
    for (const f of cases) {
      expect(parseQuery(formatQuery(f)), formatQuery(f)).toEqual(f);
    }
  });

  it("quotes a value with a space and leaves one without alone", () => {
    // Both halves: quoting everything would round-trip just as well and
    // put `label:"bug"` in every URL somebody shares.
    expect(
      formatQuery({
        state: "all",
        label: "good first issue",
        author: null,
        sort: "newest",
        text: "",
      }),
    ).toBe('is:issue label:"good first issue"');
    expect(
      formatQuery({
        state: "all",
        label: "bug",
        author: null,
        sort: "newest",
        text: "",
      }),
    ).toBe("is:issue label:bug");
  });

  it("writes the index's own default for the default filter", () => {
    expect(formatQuery(parseQuery(DEFAULT_QUERY))).toBe(DEFAULT_QUERY);
  });
});

describe("toApiQuery", () => {
  it("sends state and sort always, and the rest only when set", () => {
    // `?author=` is a filter for the person whose handle is the empty
    // string, which is nobody, and the server would honour it. `sort`
    // is unconditional because ordering is the server's job now, and a
    // request with no `sort` leaves it to a default this page does not
    // own.
    expect(
      toApiQuery({
        state: "open",
        label: null,
        author: null,
        sort: "newest",
        text: "",
      }),
    ).toEqual({ state: "open", sort: "newest" });
  });

  it("carries every filter the server can answer", () => {
    expect(
      toApiQuery({
        state: "closed",
        label: "bug",
        author: "ada",
        sort: "oldest",
        text: "panic",
      }),
    ).toEqual({
      state: "closed",
      sort: "oldest",
      label: "bug",
      author: "ada",
      q: "panic",
    });
  });

  it("can never send the sort the server answers 400 to", () => {
    // `sort=updated` is refused with a 400 that says why: the cursor is
    // an issue number, so ordering by `updated_at` while paging by
    // `number` skips or repeats rows on the second page. A link
    // somebody shared from an older bundle must fall back to the
    // default rather than break the page it was pasted into.
    const shared = parseQuery("is:issue sort:updated");
    expect(shared.sort).toBe("newest");
    expect(toApiQuery(shared).sort).toBe("newest");
    // And the same for the other order that used to exist.
    expect(toApiQuery(parseQuery("is:issue sort:comments")).sort).toBe(
      "newest",
    );
  });
});

describe("comment counts", () => {
  it("says one comment, not one comments", () => {
    // Both renderers said "comments" unconditionally, so every issue
    // with a single reply read "1 comments" — including in the
    // `aria-label`, which is the entire accessible name of the link a
    // screen reader announces.
    expect(commentWord(1)).toBe("comment");
    expect(commentWord(0)).toBe("comments");
    expect(commentWord(2)).toBe("comments");
  });
});

describe("bySeq", () => {
  const comment = (seq: number, created_at: number): IssueComment => ({
    id: `c${seq}`,
    seq,
    body: `b${seq}`,
    author: "ada",
    author_label: null,
    created_at,
    updated_at: created_at,
  });

  it("reads a conversation in insertion order, not in wire order", () => {
    // The client sorts rather than trusting the wire so that a proxy, a
    // cache or a future paginated endpoint cannot reorder a thread
    // without anybody noticing.
    expect(
      bySeq([comment(3, 0), comment(1, 0), comment(2, 0)]).map((c) => c.seq),
    ).toEqual([1, 2, 3]);
  });

  it("orders comments posted in the same millisecond", () => {
    // `created_at` has millisecond resolution and ULID tails are random
    // within one, so neither the id nor the timestamp can order these.
    // Both are equal here on purpose: only `seq` can answer.
    const same = [comment(2, 1_700_000_000_000), comment(1, 1_700_000_000_000)];
    expect(bySeq(same).map((c) => c.seq)).toEqual([1, 2]);
  });

  it("does not reorder the caller's array", () => {
    const wire = [comment(3, 0), comment(1, 0)];
    bySeq(wire);
    expect(wire.map((c) => c.seq)).toEqual([3, 1]);
  });
});

describe("issuesRoute", () => {
  it("maps the three addresses the tab answers", () => {
    expect(issuesRoute([])).toEqual({ kind: "index" });
    expect(issuesRoute(["new"])).toEqual({ kind: "new" });
    expect(issuesRoute(["12"])).toEqual({ kind: "detail", number: 12 });
  });

  it("refuses everything else that Number() would have accepted", () => {
    // `Number("0x0c")` is 12, `Number("1e2")` is 100 and `Number(" 12 ")`
    // is 12, so a parse that asked only "is this a number?" would give
    // three more addresses for every issue — addresses that render the
    // page and that no canonical link points at.
    for (const bad of ["0x0c", "1e2", " 12 ", "012", "12abc", "-1", "0", ""]) {
      expect(issuesRoute([bad]), bad).toEqual({ kind: "not-found" });
    }
  });

  it("refuses a deeper path rather than showing the index", () => {
    // `/issues/12/edit` is not the issues index, and answering it with
    // one is how a dead link looks like a working feature.
    expect(issuesRoute(["12", "edit"])).toEqual({ kind: "not-found" });
    expect(issuesRoute(["new", "x"])).toEqual({ kind: "not-found" });
  });
});

describe("authorName", () => {
  it("prefers the resolved handle", () => {
    expect(authorName({ author: "ada", author_label: "Ada L" })).toBe("ada");
  });

  it("falls back to an import's own label rather than to nothing", () => {
    // An imported issue whose author has no account here still has a
    // name, and showing it is not the same as attributing the issue to
    // a local account that happens to share it.
    expect(authorName({ author: null, author_label: "octocat" })).toBe(
      "octocat",
    );
  });

  it("never renders an empty byline", () => {
    // A deleted account sets `author_id` to NULL. "opened by  " with a
    // hole in it reads as the page having failed to load.
    expect(authorName({ author: null, author_label: null })).toBe("somebody");
  });
});

describe("the labels route", () => {
  it("reads /issues/labels as the label manager", () => {
    // Under `issues` rather than a tab of its own: labels are what
    // issues are sorted by, not a kind of thing a repository has beside
    // them, and the strip names kinds of thing.
    expect(issuesRoute(["labels"])).toEqual({ kind: "labels" });
  });

  it("still reads numbers and new, and refuses the rest", () => {
    expect(issuesRoute([])).toEqual({ kind: "index" });
    expect(issuesRoute(["new"])).toEqual({ kind: "new" });
    expect(issuesRoute(["12"])).toEqual({ kind: "detail", number: 12 });
    expect(issuesRoute(["labels", "extra"])).toEqual({ kind: "not-found" });
    // "labels" is a word, not a number, and must not become issue NaN.
    expect(issuesRoute(["nope"])).toEqual({ kind: "not-found" });
  });
});
