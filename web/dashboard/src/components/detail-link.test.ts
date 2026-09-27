import { describe, expect, it } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { DetailLink, inAppPath, sameOriginPath } from "./detail-link";

/// Which check rows navigate in-app and which leave for somebody else's
/// site. Getting this wrong is quiet in both directions: an outbound URL
/// treated as internal renders a link that goes nowhere, and one of ours
/// treated as outbound full-page-loads this very SPA in a new tab.

const HERE = "https://stratum.example";

describe("sameOriginPath", () => {
  it("keeps the path, query and fragment of a URL on this origin", () => {
    expect(
      sameOriginPath(`${HERE}/acme/widget/checks/runs/r1?job=build#tail`, HERE),
    ).toBe("/acme/widget/checks/runs/r1?job=build#tail");
  });

  it("refuses another origin", () => {
    expect(sameOriginPath("https://ci.example.test/runs/42", HERE)).toBeNull();
  });

  it("refuses a different scheme on the same host", () => {
    // `http://x` and `https://x` are different origins. Treating a
    // downgrade as internal would navigate in-app to a page that was
    // loaded over the other scheme.
    expect(sameOriginPath("http://stratum.example/a", HERE)).toBeNull();
  });

  it("refuses a different port on the same host", () => {
    expect(sameOriginPath("https://stratum.example:8443/a", HERE)).toBeNull();
  });

  it("refuses a host that merely ends with ours", () => {
    // The classic: a substring or `endsWith` test would read
    // `stratum.example.evil.test` as ours. `URL.origin` cannot be
    // fooled that way, and this is the assertion that keeps it from
    // being rewritten as a string comparison.
    expect(
      sameOriginPath("https://stratum.example.evil.test/a", HERE),
    ).toBeNull();
    expect(sameOriginPath("https://notstratum.example/a", HERE)).toBeNull();
  });

  it("refuses a relative URL rather than resolving it against us", () => {
    // A relative `detail_url` would resolve to our origin and so would
    // look internal by construction — which would let a value a third
    // party posted to the intake decide that a link navigates inside the
    // app. The server writes hosted URLs absolute, so requiring it costs
    // nothing.
    expect(sameOriginPath("/acme/widget/checks/runs/r1", HERE)).toBeNull();
    expect(sameOriginPath("//evil.test/a", HERE)).toBeNull();
  });

  it("refuses something that is not a URL at all", () => {
    // Parsing throws; a link that renders is better than a page that
    // does not.
    expect(sameOriginPath("", HERE)).toBeNull();
    expect(sameOriginPath("not a url", HERE)).toBeNull();
  });

  it("refuses a non-http scheme", () => {
    // `javascript:` never reaches an `href` we generated as internal.
    expect(sameOriginPath("javascript:alert(1)", HERE)).toBeNull();
  });
});

describe("inAppPath", () => {
  const ours = `${HERE}/acme/widget/checks/runs/wr1`;

  it("takes a hosted row on this origin in-app", () => {
    expect(inAppPath("weft", ours, HERE)).toBe(
      "/acme/widget/checks/runs/wr1",
    );
  });

  it("refuses a same-origin URL a third party reported", () => {
    // The one the origin check alone cannot see. `checks_intake` writes
    // `provider = "intake"` as a constant, so a reporter cannot spell
    // itself "weft" — but it can name our host in `detail_url`, and
    // without this arm that would buy it first-party treatment on our
    // own rows.
    expect(inAppPath("intake", ours, HERE)).toBeNull();
    expect(inAppPath("github", ours, HERE)).toBeNull();
  });

  it("refuses a hosted row pointing somewhere else", () => {
    // A deployment whose STRATUM_PUBLIC_URL is wrong. Degrading to a
    // plain outbound link is a page a reader can still reach; an in-app
    // navigation to another origin's path is a 404 wearing our chrome.
    expect(inAppPath("weft", "https://other.test/a/b/checks", HERE)).toBe(
      null,
    );
  });
});

/// The attributes each kind of row actually renders with. The decision
/// is one function above, but "which attributes end up on the anchor" is
/// the part a reader sees and the part the walkthrough asserts, so it is
/// pinned on the component itself.
describe("DetailLink attributes", () => {
  const render = (provider: string, href: string, newTab?: boolean) => {
    // No DOM here: `DetailLink` reads `window.location.origin` and falls
    // back to "" without one, so the origin is stubbed rather than
    // mocked away — the component takes the same branch it takes in a
    // browser.
    const had = "window" in globalThis;
    (globalThis as { window?: unknown }).window = {
      location: { origin: HERE },
    };
    try {
      return renderToStaticMarkup(
        createElement(DetailLink, {
          href,
          provider,
          newTab,
          navigate: () => {},
          children: "Details",
        }),
      );
    } finally {
      if (!had) delete (globalThis as { window?: unknown }).window;
    }
  };

  it("renders a hosted row as a plain in-app link", () => {
    const html = render("weft", `${HERE}/acme/widget/checks/runs/wr1`, true);
    expect(html).toContain('href="/acme/widget/checks/runs/wr1"');
    // Both of these are wrong on a first-party URL: `ugc` is a claim
    // about our own site, and `_blank` pops a second window onto the
    // app the reader is already in. `newTab` is passed here on purpose
    // — a row of ours ignores it.
    expect(html).not.toContain("_blank");
    expect(html).not.toContain("nofollow");
    expect(html).not.toContain("ugc");
  });

  it("renders a third-party row outbound and hardened", () => {
    const html = render("intake", "https://ci.example.test/builds/42", true);
    expect(html).toContain('href="https://ci.example.test/builds/42"');
    expect(html).toContain('rel="nofollow ugc noopener noreferrer"');
    expect(html).toContain('target="_blank"');
  });

  it("leaves a third-party row in this tab where the row asks it to", () => {
    // The Checks tab renders the run *name* as the link and has never
    // opened a second window; only the commit strip's separate
    // "Details" link does.
    const html = render("intake", "https://ci.example.test/builds/42");
    expect(html).toContain('rel="nofollow ugc noopener noreferrer"');
    expect(html).not.toContain("_blank");
  });
});
