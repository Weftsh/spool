import { createElement, Fragment, type ReactNode } from "react";
import { Lexer, type Token, type Tokens } from "marked";
import { cn } from "@/lib/utils";
import { PROSE_LINK } from "@/lib/links";

/// Rendered markdown: a README, a repository description, a change comment.
///
/// The content is **untrusted** — it is whatever a stranger pushed to a
/// public repository — so the shape of this file is driven by that fact
/// more than by typography.
///
/// **We never build an HTML string.** `marked` is used as a *lexer only*:
/// `Lexer.lex` returns a token tree, and we walk that tree into React
/// elements. `marked.parse()` — the function that turns markdown into
/// HTML, and the reason marked's own README tells you to run the output
/// through DOMPurify — is never called, and `dangerouslySetInnerHTML`
/// appears nowhere in this file. There is therefore no window between
/// "HTML exists" and "HTML was scrubbed", because no HTML ever exists;
/// the switch in `renderToken` *is* the allowlist, and a token type it
/// does not name cannot produce an element.
///
/// That also answers the bundle-size caution `views/browse.tsx` raised
/// when it refused a syntax highlighter. marked is 42.8 kB of ESM with
/// **zero dependencies**, and it is the whole renderer: no sanitizer
/// library, and no `jsdom` in the unit suite, because a token walk is
/// testable in node where a DOM scrubber is not.
///
/// **Raw HTML in the source is shown as text, not parsed and not
/// dropped.** GitHub permits a restricted subset; we permit none. Text
/// is the honest failure of the two available ones: dropping the markup
/// silently loses a maintainer's content and reads as our bug, where a
/// literal `<details>` on the page says plainly that we don't support it
/// yet. Widening this later means adding tag cases to `renderToken`
/// under an explicit allowlist — not switching on a parser flag.

/// Where a *relative* link or image resolves to.
///
/// A README says `](./docs/x.md)` and `![](images/y.png)`, and those
/// mean "beside me in the repository at this ref" — something only the
/// view mounting this component knows the route for. Two prefixes
/// rather than one because they are genuinely different destinations: a
/// document link goes to the code browser, an image has to reach bytes.
///
/// Both are resolved with real URL semantics (`./`, `../` and query
/// strings all behave), and a relative path can never leave the prefix's
/// origin — see `resolveHref`.
export type MarkdownBase = {
  /// Prefix for relative document links — e.g. `/acme/core/blob/main/docs/`.
  links: string;
  /// Prefix for relative image sources — e.g. `/v1/repos/acme/core/raw/main/docs/`.
  images: string;
};

/// The only URL schemes that reach an `href` or a `src`.
///
/// An allowlist, not a blocklist, and that is the entire point: the
/// blocklist version of this has to think of `vbscript:` in advance,
/// and the day a browser ships a scheme nobody here has heard of, the
/// blocklist is wrong and this is not.
///
/// `data:` is refused even for images, where it cannot execute in an
/// `<img>`: the payload is unreviewable, it defeats every byte-size
/// expectation a page has, and an SVG one is a script the moment
/// somebody later renders it as a document instead.
export const ALLOWED_SCHEMES = ["http:", "https:", "mailto:"] as const;

const NAMED_ENTITIES: Record<string, string> = {
  amp: "&",
  colon: ":",
  sol: "/",
  NewLine: "\n",
  Tab: "\t",
  lpar: "(",
  rpar: ")",
  semi: ";",
  quot: '"',
  apos: "'",
  lt: "<",
  gt: ">",
};

/// Turn HTML entities back into the characters they stand for.
///
/// This is not decoration, it is the attack. marked hands back link
/// destinations exactly as written, so `[a](JaVaScRiPt&#58;alert(1))`
/// and `[b](&#106;avascript:alert(1))` both arrive with no colon and no
/// `javascript` in sight, and a scheme check on the raw string passes
/// both. Verified against marked 18 — the lexer performs no URL
/// validation whatsoever.
///
/// `passes` is the knob that lets one function serve two masters. The
/// **decision** decodes to a fixed point, so `&amp;#58;` cannot hide a
/// colon behind two layers; the **attribute we emit** decodes exactly
/// once, which is what a browser would have done, so a file honestly
/// named `a&amp;b.png` still resolves to `a&b.png`.
///
/// An entity this table doesn't know is left alone. That is safe by
/// construction: refusing to decode `&hearts;` cannot manufacture a
/// colon, and the browser will render it correctly anyway.
function decodeEntities(value: string, passes: number): string {
  let out = value;
  for (let i = 0; i < passes; i++) {
    const next = out.replace(
      /&(#\d{1,7}|#[xX][0-9a-fA-F]{1,6}|[a-zA-Z][a-zA-Z0-9]{1,31});/g,
      (whole, body: string) => {
        if (body.startsWith("#")) {
          const code =
            body[1] === "x" || body[1] === "X"
              ? parseInt(body.slice(2), 16)
              : parseInt(body.slice(1), 10);
          if (!Number.isFinite(code) || code <= 0 || code > 0x10ffff)
            return whole;
          try {
            return String.fromCodePoint(code);
          } catch {
            return whole;
          }
        }
        return NAMED_ENTITIES[body] ?? whole;
      },
    );
    if (next === out) break;
    out = next;
  }
  return out;
}

/// The scheme a browser would read off this URL, lowercased, or null if
/// it has none and is therefore relative.
///
/// Control characters and spaces are stripped first because browsers
/// strip them too: a tab wedged into `java&Tab;script:` is not a
/// different scheme, it is the same scheme with a tab in it.
function schemeOf(url: string): string | null {
  const stripped = url.replace(/[\u0000-\u0020\u007f]/g, "");
  const match = /^([a-zA-Z][a-zA-Z0-9+.-]*):/.exec(stripped);
  return match ? `${match[1].toLowerCase()}:` : null;
}

const RELATIVE_ORIGIN = "https://relative.invalid";

/// Decide what a link destination becomes, or `null` for "render the
/// text, draw no link".
///
/// Refusal renders the label as plain prose rather than dropping it —
/// the reader still sees what the author wrote, they just cannot be
/// navigated somewhere we would not vouch for.
///
/// Exported because it is the security boundary, and a boundary that is
/// only reachable through a React tree is one that gets tested once.
export function resolveHref(
  href: string | null | undefined,
  kind: "link" | "image",
  base?: MarkdownBase,
): string | null {
  if (!href) return null;
  const raw = href.trim();
  if (raw === "") return null;

  // Two decodings, for the two different questions — see decodeEntities.
  const probe = decodeEntities(raw, 4);
  const emit = decodeEntities(raw, 1);

  // `#section`. An image cannot be an anchor, and treating one as a
  // relative path would send a garbage request at the file endpoint.
  if (probe.startsWith("#")) return kind === "image" ? null : emit;

  // `//host/path` carries no scheme but is absolute to a browser.
  // Naming that case is what stops it being joined onto our own base
  // and quietly resolving off-origin further down.
  if (/^\/\//.test(probe.replace(/^[\u0000-\u0020\u007f]+/, ""))) return emit;

  // Either decoding revealing a scheme is enough to be judged as one.
  const scheme = schemeOf(probe) ?? schemeOf(emit);
  if (scheme) {
    if (!(ALLOWED_SCHEMES as readonly string[]).includes(scheme)) return null;
    // A mail address is not an image, whatever the markdown claims.
    if (kind === "image" && scheme === "mailto:") return null;
    return emit;
  }

  // Relative, and nothing to resolve it against: the README is being
  // shown somewhere that is not the repository — a search result, a
  // description, a comment. Left as text on purpose. Resolving it
  // against whatever route happens to be open would produce a link
  // that 404s, and a 404 we generated reads to the maintainer as our
  // bug rather than as a missing base.
  if (!base) return null;

  const prefix = kind === "image" ? base.images : base.links;
  try {
    const baseUrl = new URL(prefix, RELATIVE_ORIGIN);
    const resolved = new URL(emit, baseUrl);
    // `../../..` may walk up; it may not walk out.
    if (resolved.origin !== baseUrl.origin) return null;
    return baseUrl.origin === RELATIVE_ORIGIN
      ? `${resolved.pathname}${resolved.search}${resolved.hash}`
      : resolved.href;
  } catch {
    return null;
  }
}

/// Does this destination leave the site? Absolute URLs and
/// scheme-relative ones do; a resolved repository path does not.
function isExternal(url: string): boolean {
  return /^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(url) || url.startsWith("//");
}

/// The id a heading answers to, in the shape a README's own table of
/// contents expects: lowercased, punctuation dropped, spaces hyphenated.
export function slugify(text: string): string {
  return (
    text
      .trim()
      .toLowerCase()
      // Inline markup is not part of the name: `## Install **now**` is
      // the "install-now" section on GitHub too.
      .replace(/[`*_~]/g, "")
      .replace(/[^\p{L}\p{N}\s-]/gu, "")
      .replace(/\s+/g, "-")
      .replace(/-+/g, "-")
      .replace(/^-|-$/g, "")
  );
}

/// Every heading id and every in-page link carries this prefix.
///
/// A README is a stranger's document dropped inside our application, and
/// `id` is a global namespace: an unprefixed `## Search` would mint
/// `id="search"` next to whatever the page's own chrome calls itself,
/// and `document.getElementById` would start returning a README heading
/// to code that has never heard of one. GitHub prefixes with
/// `user-content-` for exactly this reason and then intercepts clicks in
/// JavaScript to make the short fragment still work; we prefix both
/// halves instead, so the anchors work with no script at all. The cost
/// is that a fragment copied from GitHub does not carry over.
const ANCHOR_PREFIX = "md-";

type Ctx = {
  base?: MarkdownBase;
  /// How many headings have already claimed each slug, so the second
  /// "## Usage" in a document becomes `usage-1` rather than a duplicate
  /// id that no anchor can address unambiguously.
  slugs: Map<string, number>;
  /// Added to every markdown heading depth, so a README's `#` nests
  /// under the page's own `<h1>` instead of competing with it.
  headingOffset: number;
};

/// Visual weight follows the *authored* depth, so a README keeps the
/// hierarchy its writer intended even though the tags are shifted down
/// to sit under the page heading.
const HEADING_CLASS: Record<number, string> = {
  1: "mt-8 mb-4 border-b border-borderline pb-2 text-2xl font-semibold tracking-tight text-ink first:mt-0",
  2: "mt-8 mb-4 border-b border-borderline pb-2 text-xl font-semibold tracking-tight text-ink first:mt-0",
  3: "mt-6 mb-3 text-lg font-semibold text-ink first:mt-0",
  4: "mt-6 mb-3 text-base font-semibold text-ink first:mt-0",
  5: "mt-4 mb-2 text-sm font-semibold text-ink first:mt-0",
  6: "mt-4 mb-2 text-sm font-semibold text-ink-3 first:mt-0",
};

const INLINE_CODE =
  "rounded border border-borderline bg-surface-2 px-1.5 py-0.5 font-mono text-[0.9em] text-ink";

function renderTokens(tokens: Token[] | undefined, ctx: Ctx): ReactNode {
  if (!tokens) return null;
  return tokens.map((token, i) => (
    <Fragment key={i}>{renderToken(token, ctx)}</Fragment>
  ));
}

/// The allowlist, spelled as a switch.
///
/// A token type with no case here renders as its own source text: it is
/// never guessed at, and it is never silently swallowed.
function renderToken(token: Token, ctx: Ctx): ReactNode {
  switch (token.type) {
    case "space":
      return null;

    // A link reference definition is metadata — `[a]: https://…` has
    // already been folded into the links that used it.
    case "def":
      return null;

    case "text": {
      const text = token as Tokens.Text;
      return text.tokens ? renderTokens(text.tokens, ctx) : text.text;
    }

    case "escape":
      return (token as Tokens.Escape).text;

    // Raw HTML, rendered as the characters the author typed. React
    // escapes this on the way in; it is text, and it stays text.
    case "html":
      return (token as Tokens.HTML).raw;

    case "paragraph":
      return (
        <p className="my-4 first:mt-0 last:mb-0">
          {renderTokens((token as Tokens.Paragraph).tokens, ctx)}
        </p>
      );

    case "heading": {
      const heading = token as Tokens.Heading;
      const depth = Math.min(6, Math.max(1, heading.depth));
      const level = Math.min(6, depth + ctx.headingOffset);
      const slug = slugify(heading.text);
      let id: string | undefined;
      if (slug !== "") {
        const seen = ctx.slugs.get(slug) ?? 0;
        ctx.slugs.set(slug, seen + 1);
        id = `${ANCHOR_PREFIX}${seen === 0 ? slug : `${slug}-${seen}`}`;
      }
      return createElement(
        `h${level}`,
        { className: HEADING_CLASS[depth], id },
        renderTokens(heading.tokens, ctx),
      );
    }

    case "strong":
      return (
        <strong className="font-semibold text-ink">
          {renderTokens((token as Tokens.Strong).tokens, ctx)}
        </strong>
      );

    case "em":
      return (
        <em className="italic">
          {renderTokens((token as Tokens.Em).tokens, ctx)}
        </em>
      );

    case "del":
      return (
        <del className="text-ink-3 line-through">
          {renderTokens((token as Tokens.Del).tokens, ctx)}
        </del>
      );

    case "br":
      return <br />;

    case "hr":
      return <hr className="my-8 border-borderline" />;

    case "codespan":
      return (
        <code className={INLINE_CODE}>{(token as Tokens.Codespan).text}</code>
      );

    // The `pre` is its own scroll container. The walkthrough's audit
    // fails the build on `documentElement.scrollWidth`, and a fenced
    // block of shell output is the widest thing a README contains.
    case "code":
      return (
        <pre className="my-4 max-w-full overflow-x-auto rounded-lg border border-borderline bg-surface-2 p-4">
          <code className="font-mono text-sm text-ink">
            {(token as Tokens.Code).text}
          </code>
        </pre>
      );

    case "blockquote":
      return (
        <blockquote className="my-4 border-l-2 border-borderline pl-4 text-ink-3">
          {renderTokens((token as Tokens.Blockquote).tokens, ctx)}
        </blockquote>
      );

    case "list": {
      const list = token as Tokens.List;
      const children = list.items.map((item, i) => (
        <Fragment key={i}>{renderToken(item, ctx)}</Fragment>
      ));
      return list.ordered ? (
        <ol
          className="my-4 list-decimal space-y-1 pl-6 marker:text-ink-3"
          start={typeof list.start === "number" ? list.start : undefined}
        >
          {children}
        </ol>
      ) : (
        <ul className="my-4 list-disc space-y-1 pl-6 marker:text-ink-3">
          {children}
        </ul>
      );
    }

    case "list_item": {
      const item = token as Tokens.ListItem;
      return (
        <li className={cn(item.task && "list-none")}>
          {renderTokens(item.tokens, ctx)}
        </li>
      );
    }

    // A README's checkbox is a record of what somebody ticked in a file,
    // not a control: there is nothing here to submit and nothing to
    // toggle. Rendering it as a real `<input>` would announce a form
    // field that ignores you, so it is a glyph carrying its own label.
    case "checkbox": {
      const checked = (token as Tokens.Checkbox).checked;
      return (
        <span
          role="img"
          aria-label={checked ? "Done" : "Not done"}
          className={cn(
            "mr-1.5 inline-flex size-3.5 shrink-0 translate-y-px items-center justify-center rounded-sm border align-middle font-mono text-[9px] leading-none",
            checked
              ? "border-brand bg-brand text-brand-ink"
              : "border-borderline bg-surface-2",
          )}
        >
          {checked ? "✓" : ""}
        </span>
      );
    }

    case "link": {
      const link = token as Tokens.Link;
      const href = resolveHref(link.href, "link", ctx.base);
      const children = renderTokens(link.tokens, ctx);
      // Refused: the words survive, the navigation does not.
      if (href === null) return <>{children}</>;
      // A table of contents writes `](#installation)`; the heading that
      // answers it is `id="md-installation"`. Slugified on the way in
      // as well, so `](#Installation)` finds it too.
      if (href.startsWith("#")) {
        const target = slugify(href.slice(1));
        return (
          <a className={PROSE_LINK} href={`#${ANCHOR_PREFIX}${target}`}>
            {children}
          </a>
        );
      }
      const external = isExternal(href);
      return (
        <a
          className={PROSE_LINK}
          href={href}
          title={link.title ?? undefined}
          // `ugc` and `nofollow` say what this content is: a stranger's
          // repository, not our endorsement. `noopener`/`noreferrer`
          // hold even without target=_blank, and cost nothing.
          {...(external
            ? { rel: "nofollow ugc noopener noreferrer" }
            : undefined)}
        >
          {children}
        </a>
      );
    }

    case "image": {
      const image = token as Tokens.Image;
      const src = resolveHref(image.href, "image", ctx.base);
      // No src we will vouch for: show the alt text, which is the one
      // thing the author wrote for exactly this situation.
      if (src === null) return <>{image.text}</>;
      return (
        <img
          className="inline-block max-w-full rounded"
          src={src}
          alt={image.text}
          title={image.title ?? undefined}
          loading="lazy"
          // A README badge is a third-party request made on the reader's
          // behalf; sending our paths along with it is not necessary.
          referrerPolicy="no-referrer"
        />
      );
    }

    // The table scrolls inside its own border rather than widening the
    // page — same reason as `code`, and the case the audit catches most.
    case "table": {
      const table = token as Tokens.Table;
      const align = (i: number) => {
        const a = table.align[i];
        return a === "center"
          ? "text-center"
          : a === "right"
            ? "text-right"
            : "text-left";
      };
      return (
        <div className="my-4 max-w-full overflow-x-auto rounded-lg border border-borderline">
          <table className="w-full border-collapse text-sm">
            <thead className="bg-surface-2">
              <tr>
                {table.header.map((cell, i) => (
                  <th
                    key={i}
                    className={cn(
                      "border-b border-borderline px-3 py-2 font-semibold text-ink",
                      align(i),
                    )}
                  >
                    {renderTokens(cell.tokens, ctx)}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {table.rows.map((row, r) => (
                <tr key={r}>
                  {row.map((cell, i) => (
                    <td
                      key={i}
                      className={cn(
                        "border-b border-borderline px-3 py-2 last:border-b-0",
                        align(i),
                      )}
                    >
                      {renderTokens(cell.tokens, ctx)}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    }

    default:
      // Unknown token type. Show the source rather than losing it.
      return (token as Tokens.Generic).raw ?? null;
  }
}

export type MarkdownProps = {
  /// Untrusted markdown, as pushed to the repository.
  source: string | null | undefined;
  /// Repository-relative resolution. Omit it and relative links and
  /// images render as text — see `resolveHref`.
  base?: MarkdownBase;
  /// Render one line's worth of inline markdown with no block elements:
  /// a repository description, a table cell. Emphasis, code and links
  /// work; headings and lists are not parsed at all.
  inline?: boolean;
  /// The heading level a markdown `#` maps to. Defaults to 2, which
  /// puts a README's title under the repository name rather than
  /// beside it in the document outline.
  headingLevel?: 1 | 2 | 3;
  className?: string;
};

export function Markdown(props: MarkdownProps) {
  const source = props.source ?? "";
  // Nothing to say: say nothing. An empty bordered box reads as a
  // README that failed to load.
  if (source.trim() === "") return null;

  const ctx: Ctx = {
    base: props.base,
    slugs: new Map(),
    headingOffset: (props.headingLevel ?? 2) - 1,
  };
  const tokens = props.inline
    ? Lexer.lexInline(source, { gfm: true })
    : Lexer.lex(source, { gfm: true });

  if (props.inline) {
    return (
      <span className={cn("min-w-0 break-words", props.className)}>
        {renderTokens(tokens, ctx)}
      </span>
    );
  }

  return (
    // `min-w-0` and `break-words` are the pair that keeps a 300-character
    // URL from widening the whole forge column.
    <div
      className={cn(
        "min-w-0 break-words text-base leading-[1.65] text-ink-2",
        props.className,
      )}
    >
      {renderTokens(tokens, ctx)}
    </div>
  );
}
