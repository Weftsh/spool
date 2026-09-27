import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import {
  ALLOWED_SCHEMES,
  Markdown,
  type MarkdownBase,
  type MarkdownProps,
  resolveHref,
  slugify,
} from "./markdown";

// Markdown here is untrusted input: it is whatever a stranger pushed to
// a public repository, rendered on a page a maintainer is signed in to.
// So most of this file is about the cases nobody writes on purpose.
//
// The tests come in two layers deliberately. `resolveHref` is checked
// directly, because a table of hostile URLs is the clearest way to say
// what the allowlist means — but a helper can be perfect and still be
// bypassed by the component that stopped calling it. So every security
// property is *also* asserted against the rendered output of the real
// `Markdown`, where the only way to pass is for the whole pipeline to
// be right.

const render = (source: string, props: Partial<MarkdownProps> = {}) =>
  renderToStaticMarkup(createElement(Markdown, { source, ...props }));

const BASE: MarkdownBase = {
  links: "/acme/core/blob/main/",
  images: "/v1/repos/acme/core/raw/main/",
};

describe("resolveHref: which destinations we will vouch for", () => {
  it("passes the three schemes a README legitimately uses", () => {
    expect(resolveHref("https://example.com/a", "link")).toBe(
      "https://example.com/a",
    );
    expect(resolveHref("http://example.com/a", "link")).toBe(
      "http://example.com/a",
    );
    expect(resolveHref("mailto:security@example.com", "link")).toBe(
      "mailto:security@example.com",
    );
    expect(ALLOWED_SCHEMES).toEqual(["http:", "https:", "mailto:"]);
  });

  it("refuses every scheme that is not on the list", () => {
    // An allowlist, so the interesting property is not that these five
    // are named — it is that anything unnamed lands here with them.
    for (const href of [
      "javascript:alert(1)",
      "JAVASCRIPT:alert(1)",
      "vbscript:msgbox(1)",
      "file:///etc/passwd",
      "data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==",
      "data:image/svg+xml,<svg onload=alert(1)>",
      "chrome-extension://abc/page.html",
      "intent://scan/#Intent;scheme=zxing;end",
      "a-scheme-nobody-has-invented-yet:payload",
    ]) {
      expect(resolveHref(href, "link"), href).toBeNull();
      expect(resolveHref(href, "image"), href).toBeNull();
    }
  });

  it("refuses a scheme hidden behind HTML entities", () => {
    // marked hands link destinations back exactly as written and does
    // no URL validation at all (verified against marked 18), so these
    // reach us with no colon and no readable `javascript` in them. A
    // scheme check on the raw string passes all four.
    for (const href of [
      "JaVaScRiPt&#58;alert(1)",
      "&#106;avascript:alert(1)",
      "&#x6a;avascript:alert(1)",
      "javascript&colon;alert(1)",
    ]) {
      expect(resolveHref(href, "link"), href).toBeNull();
    }
  });

  it("refuses a scheme hidden behind two layers of encoding", () => {
    // `&amp;#58;` decodes to `&#58;` decodes to `:`. This is why the
    // decision decodes to a fixed point rather than exactly once.
    expect(resolveHref("javascript&amp;#58;alert(1)", "link")).toBeNull();
    expect(resolveHref("javascript&amp;amp;#58;alert(1)", "link")).toBeNull();
  });

  it("refuses a scheme split by whitespace or control characters", () => {
    // Browsers strip these when resolving a URL, so `java\tscript:` is
    // not a different scheme — it is the same one wearing a hat.
    for (const href of [
      "java&Tab;script:alert(1)",
      "java&NewLine;script:alert(1)",
      "java&#09;script:alert(1)",
      "  javascript:alert(1)",
      "&#01;javascript:alert(1)",
    ]) {
      expect(resolveHref(href, "link"), href).toBeNull();
    }
  });

  it("still decodes entities that were only ever punctuation", () => {
    // The security decoding must not corrupt honest content: a file
    // really can be named with an ampersand, and one decode is what a
    // browser would have done with the attribute.
    expect(resolveHref("https://example.com/a&amp;b", "link")).toBe(
      "https://example.com/a&b",
    );
    expect(resolveHref("q&amp;a.png", "image", BASE)).toBe(
      "/v1/repos/acme/core/raw/main/q&a.png",
    );
  });

  it("treats an in-page anchor as a link and never as an image", () => {
    expect(resolveHref("#installation", "link")).toBe("#installation");
    // A `#foo` src would be sent at the file endpoint as a path.
    expect(resolveHref("#installation", "image")).toBeNull();
  });

  it("refuses a mailto: image, whatever the markdown claims", () => {
    expect(resolveHref("mailto:a@b.c", "image")).toBeNull();
  });

  it("resolves relative destinations against the repository", () => {
    expect(resolveHref("./docs/x.md", "link", BASE)).toBe(
      "/acme/core/blob/main/docs/x.md",
    );
    expect(resolveHref("docs/x.md", "link", BASE)).toBe(
      "/acme/core/blob/main/docs/x.md",
    );
    expect(resolveHref("../CONTRIBUTING.md", "link", BASE)).toBe(
      "/acme/core/blob/CONTRIBUTING.md",
    );
    expect(resolveHref("images/y.png", "image", BASE)).toBe(
      "/v1/repos/acme/core/raw/main/images/y.png",
    );
    expect(resolveHref("x.md?plain=1#L4", "link", BASE)).toBe(
      "/acme/core/blob/main/x.md?plain=1#L4",
    );
  });

  it("leaves a relative destination alone when there is no repository", () => {
    // A description or a comment rendered outside the repo page. The
    // alternative — resolving against whatever route is open — makes a
    // link that 404s, and a 404 we generated reads to the maintainer as
    // our bug rather than as a missing base.
    expect(resolveHref("./docs/x.md", "link")).toBeNull();
    expect(resolveHref("images/y.png", "image")).toBeNull();
  });

  it("treats //host as absolute rather than joining it to our base", () => {
    // No scheme, so the naive reading is "relative" — and joining it to
    // the base resolves off-origin anyway. Naming the case is what
    // keeps the two paths honest.
    expect(resolveHref("//example.com/p", "link", BASE)).toBe(
      "//example.com/p",
    );
  });

  it("refuses a relative-looking path that escapes the origin", () => {
    // Not theoretical: WHATWG URL treats a backslash like a slash for
    // http(s), so these carry no scheme and no leading `//` yet resolve
    // to https://evil.com. This is the case the origin guard exists for.
    expect(resolveHref("/\\evil.com/p", "link", BASE)).toBeNull();
    expect(resolveHref("\\\\evil.com/p", "link", BASE)).toBeNull();
    expect(resolveHref("\\\\evil.com/p.png", "image", BASE)).toBeNull();
    // A single leading backslash normalises to a root-relative path on
    // our own origin. It may 404 in the app; it cannot reach evil.com,
    // which is the only property this guard owes anybody.
    expect(resolveHref("\\evil.com/p", "link", BASE)).toBe("/evil.com/p");
  });

  it("keeps an absolute base absolute, and still refuses to leave it", () => {
    const cdn: MarkdownBase = {
      links: "https://cdn.example/x/",
      images: "https://cdn.example/x/",
    };
    expect(resolveHref("y.png", "image", cdn)).toBe(
      "https://cdn.example/x/y.png",
    );
    expect(resolveHref("\\\\evil.com/p.png", "image", cdn)).toBeNull();
  });

  it("has nothing to say about nothing", () => {
    expect(resolveHref("", "link")).toBeNull();
    expect(resolveHref("   ", "link")).toBeNull();
    expect(resolveHref(null, "link")).toBeNull();
    expect(resolveHref(undefined, "link")).toBeNull();
  });
});

describe("the renderer refuses HTML rather than scrubbing it", () => {
  it("shows a script tag as the text it is", () => {
    const out = render("<script>alert(1)</script>");
    expect(out).not.toContain("<script");
    expect(out).toContain("&lt;script&gt;alert(1)&lt;/script&gt;");
  });

  it("never emits an event-handler attribute", () => {
    const hostile = [
      "<img src=x onerror=alert(1)>",
      "<body onload=alert(1)>",
      "<div onmouseover=alert(1)>hover</div>",
      "<svg><animate onbegin=alert(1)></svg>",
      "<iframe src=https://evil.com></iframe>",
      "<style>body{background:url(https://evil.com/x)}</style>",
      "<a href=javascript:alert(1)>x</a>",
      "<form action=https://evil.com><input name=p></form>",
      "<math><mtext><option><FAKEFAKE><option></option><mglyph>",
    ].join("\n\n");
    const out = render(hostile);
    // The general property, not nine specific ones: no tag we did not
    // choose to emit, and no handler attribute on any of them.
    //
    // Scanned over real tags rather than the whole string, and that
    // distinction is the point: `&lt;img src=x onerror=alert(1)&gt;` is
    // the *correct* output and contains the characters " onerror=" as
    // visible text. Every unescaped `<` in the output opens a tag we
    // chose to emit, so this still goes red the moment one of them
    // carries an attribute from the source.
    const tags = out.match(/<[a-zA-Z][^>]*>/g) ?? [];
    expect(tags.length).toBeGreaterThan(0);
    for (const tag of tags) {
      expect(tag, tag).not.toMatch(/\son[a-z]+=/i);
      expect(tag, tag).not.toMatch(
        /^<(script|iframe|style|form|input|svg|object|embed|link|meta|base)\b/i,
      );
    }
  });

  it("shows raw HTML rather than dropping it on the floor", () => {
    // The other available failure is silence, and silence loses a
    // maintainer's content in a way that reads as our bug.
    const out = render(
      "Before\n\n<details><summary>More</summary></details>\n\nAfter",
    );
    expect(out).toContain("&lt;details&gt;");
    expect(out).toContain("Before");
    expect(out).toContain("After");
  });

  it("escapes exactly once — text is never double-encoded", () => {
    // marked's lexer hands back unescaped text and React escapes on the
    // way in. If anything ever escaped in between, this is where a
    // README full of `&amp;lt;` would show up.
    const out = render('```js\nif (a < b && c) x("<script>");\n```');
    expect(out).toContain("if (a &lt; b &amp;&amp; c)");
    expect(out).not.toContain("&amp;lt;");
    expect(out).not.toContain("&amp;amp;");
  });

  it("keeps inline code literal", () => {
    const out = render("use `a && b <tag>` here");
    expect(out).toContain("a &amp;&amp; b &lt;tag&gt;");
    expect(out).not.toContain("<tag>");
  });
});

describe("the renderer refuses hostile links and images", () => {
  it("renders a javascript: link as its own words, with no anchor", () => {
    const out = render("[click me](javascript:alert(1))");
    expect(out).not.toContain("<a");
    expect(out).toContain("click me");
  });

  it("renders an entity-hidden javascript: link as words too", () => {
    // The end-to-end half of the entity test: proves the component is
    // still calling the allowlist, not just that the allowlist works.
    const out = render("[click me](&#106;avascript:alert(1))");
    expect(out).not.toContain("<a");
    expect(out).not.toContain("javascript");
    expect(out).toContain("click me");
  });

  it("renders a refused image as its alt text", () => {
    for (const src of [
      "javascript:alert(1)",
      "data:image/svg+xml,<svg onload=alert(1)>",
    ]) {
      const out = render(`![the logo](${src})`);
      expect(out, src).not.toContain("<img");
      expect(out, src).toContain("the logo");
    }
  });

  it("renders a relative image with no base as its alt text", () => {
    const out = render("![the logo](images/logo.png)");
    expect(out).not.toContain("<img");
    expect(out).toContain("the logo");
  });

  it("renders a relative image against the repository when it has one", () => {
    const out = render("![the logo](images/logo.png)", { base: BASE });
    expect(out).toContain('src="/v1/repos/acme/core/raw/main/images/logo.png"');
    expect(out).toContain('alt="the logo"');
  });

  it("marks an outbound link as somebody else's content", () => {
    const out = render("[docs](https://example.com/docs)");
    expect(out).toContain('rel="nofollow ugc noopener noreferrer"');
    expect(out).toContain('href="https://example.com/docs"');
  });

  it("does not mark an in-repository link as outbound", () => {
    const out = render("[contributing](./CONTRIBUTING.md)", { base: BASE });
    expect(out).toContain('href="/acme/core/blob/main/CONTRIBUTING.md"');
    expect(out).not.toContain("rel=");
  });

  it("does not leak our paths to a third-party badge host", () => {
    const out = render("![build](https://shields.io/b.svg)");
    expect(out).toMatch(/referrerpolicy="no-referrer"/i);
  });
});

describe("the rendered document", () => {
  it("renders nothing at all for nothing at all", () => {
    // An empty bordered box reads as a README that failed to load.
    expect(render("")).toBe("");
    expect(render("   \n\n  ")).toBe("");
    expect(
      renderToStaticMarkup(createElement(Markdown, { source: null })),
    ).toBe("");
  });

  it("nests a README's headings under the page's own h1", () => {
    // The repo page already has an h1 with the repository's name on it.
    // A README that opens with `# Acme` must not compete with it in the
    // document outline.
    const out = render("# Acme\n\n## Install");
    expect(out).toContain("<h2");
    expect(out).toContain("<h3");
    expect(out).not.toContain("<h1");
  });

  it("can be told to own the outline instead", () => {
    expect(render("# Acme", { headingLevel: 1 })).toContain("<h1");
  });

  it("never emits a heading deeper than h6", () => {
    // `###### x` at offset 1 would be an h7, which is not a tag.
    const out = render("###### deep");
    expect(out).toContain("<h6");
    expect(out).not.toContain("<h7");
  });

  it("scrolls wide content inside its own container", () => {
    // The walkthrough's audit fails the build on
    // documentElement.scrollWidth, and a fenced block of shell output
    // and a wide table are the two things a README uses to break it.
    const code = render("```\n" + "x".repeat(400) + "\n```");
    expect(code).toMatch(/<pre[^>]*overflow-x-auto/);
    const table = render("| a | b |\n|:--|--:|\n| 1 | 2 |");
    expect(table).toMatch(/<div[^>]*overflow-x-auto[^>]*>\s*<table/);
  });

  it("builds a table out of real table elements", () => {
    // Positional `td` selectors and getByRole("cell") depend on it —
    // COMPONENTS.md makes this a contract, not a preference.
    const out = render("| a | b |\n|:--|--:|\n| 1 | 2 |");
    expect(out).toContain("<thead");
    expect(out).toContain("<th");
    expect(out).toContain("<tbody");
    expect(out).toContain("<td");
    // Column alignment from the delimiter row survives.
    expect(out).toMatch(/<th[^>]*text-left/);
    expect(out).toMatch(/<th[^>]*text-right/);
  });

  it("gives a prose link the brand colour, per the settled rule", () => {
    // FORGE-UX §9.2: structural links are ink and reveal on hover;
    // prose links — "inside a rendered README" — are the one kind that
    // carries --brand, because a paragraph gives them no other way to
    // be legible.
    expect(render("see [the docs](https://example.com)")).toContain(
      "text-brand",
    );
  });

  it("renders a task list as a labelled glyph, not a dead form control", () => {
    // A README checkbox records what somebody ticked in a file. There
    // is nothing to submit and nothing to toggle, and the spec counts
    // native checkboxes inside a form.
    const out = render("- [x] shipped\n- [ ] pending");
    expect(out).not.toContain("<input");
    expect(out).toContain('aria-label="Done"');
    expect(out).toContain('aria-label="Not done"');
  });

  it("renders the ordinary things a README is made of", () => {
    const out = render(
      [
        "# Title",
        "Some **bold**, some _em_, some ~~gone~~.",
        "> a quote",
        "- one\n- two",
        "1. first\n2. second",
        "---",
        "see https://example.com/bare",
      ].join("\n\n"),
    );
    for (const tag of [
      "<strong",
      "<em",
      "<del",
      "<blockquote",
      "<ul",
      "<ol",
      "<li",
      "<hr",
    ]) {
      expect(out, tag).toContain(tag);
    }
    // gfm autolinking: a bare URL in a README is a link people expect.
    expect(out).toContain('href="https://example.com/bare"');
  });

  it("gives a README's own table of contents somewhere to land", () => {
    // Rendering `#installation` as a link while minting no matching id
    // is the silent-404 case: the link works on GitHub, does nothing
    // here, and reads to the maintainer as our bug.
    const out = render("# Installation\n\nsee [install](#installation)");
    expect(out).toContain('id="md-installation"');
    expect(out).toContain('href="#md-installation"');
  });

  it("matches an anchor written in the heading's own capitalisation", () => {
    // The fragment is slugified on the way in as well as on the way
    // out, so an author who wrote the section name rather than its slug
    // still gets a working link.
    const out = render("## Getting Started\n\n[go](#Getting-Started)");
    expect(out).toContain('id="md-getting-started"');
    expect(out).toContain('href="#md-getting-started"');
  });

  it("namespaces heading ids away from the application's own", () => {
    // `id` is a global namespace and a README is a stranger's document
    // dropped into our page. An unprefixed `## Search` would answer to
    // getElementById("search") for code that has never heard of it.
    const out = render("## Search");
    expect(out).toContain('id="md-search"');
    expect(out).not.toContain('id="search"');
  });

  it("gives two headings of the same name two different ids", () => {
    const out = render("## Usage\n\n## Usage\n\n## Usage");
    expect(out).toContain('id="md-usage"');
    expect(out).toContain('id="md-usage-1"');
    expect(out).toContain('id="md-usage-2"');
  });

  it("mints no id at all for a heading with no name to make one from", () => {
    // `## ***` slugs to the empty string; `id=""` is not an anchor, it
    // is an attribute that makes the next duplicate check meaningless.
    const out = render("## ***");
    expect(out).not.toContain('id=""');
    expect(out).not.toContain('id="md-"');
  });

  it("renders inline mode without block elements", () => {
    // A repository description is one line: emphasis and code, no
    // headings and no lists, whatever the text happens to start with.
    const out = render("# not a heading, but `code` and **bold**", {
      inline: true,
    });
    expect(out).not.toContain("<h");
    expect(out).not.toContain("<p");
    expect(out).toContain("<code");
    expect(out).toContain("<strong");
  });
});

describe("slugify", () => {
  it("makes the name a table of contents would have guessed", () => {
    expect(slugify("Getting Started")).toBe("getting-started");
    expect(slugify("  Install  the   thing ")).toBe("install-the-thing");
    expect(slugify("Install **now**")).toBe("install-now");
    expect(slugify("What's new?")).toBe("whats-new");
    expect(slugify("v1.2.3 — release notes")).toBe("v123-release-notes");
  });

  it("keeps letters that are not ASCII", () => {
    // A README is not necessarily in English, and stripping to [a-z]
    // would give every non-Latin heading the same empty slug.
    expect(slugify("Привет мир")).toBe("привет-мир");
    expect(slugify("日本語")).toBe("日本語");
  });

  it("returns nothing when there is no name in there", () => {
    expect(slugify("***")).toBe("");
    expect(slugify("   ")).toBe("");
    expect(slugify("!!!")).toBe("");
  });
});

describe("the module keeps its own promises", () => {
  const source = readFileSync(
    join(dirname(fileURLToPath(import.meta.url)), "markdown.tsx"),
    "utf8",
  );
  // Strip the doc comments: they are allowed to *name* the things the
  // code may not do. (The same trap theme-contract.test.ts documents —
  // a comment quoting a forbidden name failing as though it used it.)
  const code = source
    .replace(/^\s*\/\/\/.*$/gm, "")
    .replace(/^\s*\/\/.*$/gm, "");

  it("never sets inner HTML", () => {
    // The whole design rests on this: no HTML string is ever built, so
    // there is no window between generating markup and scrubbing it.
    // If this line ever goes red, the security tests above stopped
    // meaning what they say.
    expect(code).not.toContain("dangerouslySetInnerHTML");
  });

  it("uses marked as a lexer and never as a renderer", () => {
    // marked.parse() is the function whose output marked's own README
    // tells you to run through DOMPurify. We do not call it.
    expect(code).not.toMatch(/\bmarked\s*\.\s*parse/);
    expect(code).not.toMatch(/\bparseInline\b/);
    expect(code).toMatch(/\bLexer\.lex(Inline)?\(/);
  });

  it("uses design tokens rather than colours of its own", () => {
    // DESIGN.md: colours change in web/shared/tokens.css only.
    expect(code).not.toMatch(/#[0-9a-fA-F]{3,8}\b/);
    expect(code).not.toMatch(/\b(rgb|hsl|oklch)\(/);
  });
});
