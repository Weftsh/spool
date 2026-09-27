import { describe, expect, it } from "vitest";
import type { SiteDeploy, SiteStatus } from "@/api";
import {
  CONFIG_PATH,
  EXAMPLE_CONFIG,
  MAX_SHOWN_DEPLOYS,
  PUBLIC_WARNING,
  isCurrent,
  nothingPublishedLine,
  siteLabel,
  siteLinkLabel,
  siteView,
  unknownConfigLine,
} from "./site";

/// Every assertion here is **exact** — `toEqual`, `toBe`, `toEqual([…])`
/// — and never `toContain`.
///
/// That is the same rule the Playwright specs need for a different
/// reason: `getByText` and `getByRole`'s `name` both match by substring,
/// so "the panel says the site is public" passes against a panel saying
/// something else that happens to contain the words. A `toContain` here
/// would have the identical failure mode: `expect(view.publish)
/// .toContain("dist")` passes for `"dist"`, `"public/dist"` and
/// `"dist-old"`, and only one of those is the directory being served.

function deploy(over: Partial<SiteDeploy> = {}): SiteDeploy {
  return {
    id: "01dep1",
    commit: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
    tree: "ffffffffffffffffffffffffffffffffffffffff",
    publish: "dist",
    spa: false,
    not_found: null,
    created_at: 1_757_000_000_000,
    ...over,
  };
}

/// A site nobody has configured: the shape the route answers for the
/// overwhelming majority of repositories.
function status(over: Partial<SiteStatus> = {}): SiteStatus {
  return {
    enabled: false,
    host: null,
    url: null,
    branch: null,
    config_state: "absent",
    config_error: null,
    config: null,
    current: null,
    deploys: [],
    ...over,
  };
}

/// A live site: config parses, a deploy is being served, and the
/// deployment hosts sites so there is a real address.
function live(over: Partial<SiteStatus> = {}): SiteStatus {
  return status({
    enabled: true,
    host: "widget--acme",
    url: "https://widget--acme.weft.dev",
    branch: null,
    config_state: "ok",
    config: { publish: "dist", branch: null, spa: false, not_found: null },
    current: "01dep1",
    deploys: [deploy()],
    ...over,
  });
}

// ---------------------------------------------------------------------
// The four states the panel has to draw.

describe("siteView — nothing configured", () => {
  it("reports the config absent and the site off", () => {
    const v = siteView(status(), "main");
    expect(v.config).toEqual({ kind: "absent" });
    expect(v.site).toEqual({ kind: "off" });
    // No directory is known, and guessing `dist` here would put a
    // sentence on screen claiming this repository publishes something.
    expect(v.publish).toBeNull();
    // The default branch, resolved. `null` would reach the panel as the
    // word "null" beside "Publishes from".
    expect(v.branch).toBe("main");
  });

  it("says nothing extra underneath — the how-to block is the answer", () => {
    expect(nothingPublishedLine(siteView(status(), "main"))).toBeNull();
  });
});

describe("siteView — the config was refused", () => {
  const REFUSAL =
    '.weft/site.yml:3: unknown key "publsh" — did you mean "publish"?';

  it("carries the server's sentence through unchanged", () => {
    const v = siteView(
      status({ config_state: "refused", config_error: REFUSAL }),
      "main",
    );
    expect(v.config).toEqual({ kind: "refused", error: REFUSAL });
  });

  it("is still a refusal when the server sent no sentence", () => {
    // The one lie this must never tell is "the config is fine". A
    // missing `config_error` is a server bug, not a passing config.
    const v = siteView(
      status({ config_state: "refused", config_error: null }),
      "main",
    );
    expect(v.config).toEqual({
      kind: "refused",
      error: "no reason was recorded",
    });
  });

  it("reports the live site AND the refusal, not one or the other", () => {
    // The whole reason the two halves are separate. The file has just
    // been broken; the last good deploy keeps serving. A panel that
    // switched on one field would show either the address or the error,
    // and this is exactly the moment somebody needs both: "the site is
    // up, and here is why your change is not on it."
    const v = siteView(
      live({ config_state: "refused", config_error: REFUSAL, config: null }),
      "main",
    );
    expect(v.config).toEqual({ kind: "refused", error: REFUSAL });
    expect(v.site.kind).toBe("live");
    if (v.site.kind !== "live") throw new Error("unreachable");
    expect(v.site.url).toBe("https://widget--acme.weft.dev");
    // The directory being served is the deploy's, and it is knowable
    // even though the config that would name it no longer parses.
    expect(v.publish).toBe("dist");
  });

  it("adds no second sentence about nothing having published", () => {
    const v = siteView(
      status({ config_state: "refused", config_error: REFUSAL }),
      "main",
    );
    expect(nothingPublishedLine(v)).toBeNull();
  });
});

describe("siteView — live, with deploys", () => {
  it("reports the address, the branch and the served directory", () => {
    const v = siteView(live(), "main");
    expect(v.config).toEqual({
      kind: "ok",
      publish: "dist",
      spa: false,
      notFound: null,
    });
    expect(v.branch).toBe("main");
    expect(v.publish).toBe("dist");
    expect(v.site.kind).toBe("live");
    if (v.site.kind !== "live") throw new Error("unreachable");
    expect(v.site.url).toBe("https://widget--acme.weft.dev");
    expect(v.site.current.id).toBe("01dep1");
    expect(v.site.recent.map((d) => d.id)).toEqual(["01dep1"]);
  });

  it("prefers the site's own branch over the repository default", () => {
    const v = siteView(live({ branch: "release" }), "main");
    expect(v.branch).toBe("release");
  });

  it("serves the deploy named by `current`, not the newest one", () => {
    // A rollback. `deploys` is newest first, so `deploys[0]` is the
    // commit that is **not** live — and taking it would put the wrong
    // sha under a "Serving" badge on the one occasion somebody is
    // reading the panel to find out which commit is live.
    const older = deploy({ id: "01dep1", commit: "1111111111111111" });
    const newer = deploy({ id: "01dep2", commit: "2222222222222222" });
    const v = siteView(
      live({ current: "01dep1", deploys: [newer, older] }),
      "main",
    );
    if (v.site.kind !== "live") throw new Error("unreachable");
    expect(v.site.current.commit).toBe("1111111111111111");
    expect(isCurrent(v, older)).toBe(true);
    expect(isCurrent(v, newer)).toBe(false);
  });

  it("shows the deploy's directory, not the one the file now names", () => {
    // The edit has landed and nothing has published since. The panel
    // must describe what is actually being served: saying `public` here
    // would tell somebody their change is live when it is not.
    const v = siteView(
      live({
        config: { publish: "public", branch: null, spa: false, not_found: null },
        deploys: [deploy({ publish: "dist" })],
      }),
      "main",
    );
    expect(v.publish).toBe("dist");
    expect(v.config).toEqual({
      kind: "ok",
      publish: "public",
      spa: false,
      notFound: null,
    });
  });

  it("lists a few deploys and no more, newest first", () => {
    const many = Array.from({ length: 12 }, (_, i) =>
      deploy({ id: `01dep${i}`, created_at: 1_757_000_000_000 - i * 1000 }),
    );
    const v = siteView(live({ current: "01dep0", deploys: many }), "main");
    if (v.site.kind !== "live") throw new Error("unreachable");
    expect(v.site.recent.map((d) => d.id)).toEqual([
      "01dep0",
      "01dep1",
      "01dep2",
      "01dep3",
      "01dep4",
    ]);
    expect(v.site.recent).toHaveLength(MAX_SHOWN_DEPLOYS);
  });

  it("reports no address on a deployment that does not host sites", () => {
    // `url` is the server's answer and is never rebuilt from `host`:
    // an address that resolves nowhere gets sent to a colleague.
    const v = siteView(live({ url: null }), "main");
    if (v.site.kind !== "live") throw new Error("unreachable");
    expect(v.site.url).toBeNull();
    // Still live, still serving — the address is the only thing missing.
    expect(v.site.current.id).toBe("01dep1");
  });

  it("says nothing about waiting for a push", () => {
    expect(nothingPublishedLine(siteView(live(), "main"))).toBeNull();
  });
});

describe("siteView — enabled, nothing published yet", () => {
  it("is pending, not live, when there is no current deploy", () => {
    const v = siteView(live({ current: null, deploys: [] }), "main");
    expect(v.site).toEqual({ kind: "pending" });
    // The config is fine, so the directory it names is the honest
    // answer to "what will be served".
    expect(v.publish).toBe("dist");
  });

  it("is pending when `current` names a deploy that is not in the page", () => {
    // Not live. A deploy id with no row behind it cannot be described,
    // and rendering `undefined.commit` is how a settings page goes
    // blank on a state nobody had seen before.
    const v = siteView(live({ current: "01gone", deploys: [deploy()] }), "main");
    expect(v.site).toEqual({ kind: "pending" });
  });

  it("tells the reader which push will publish what", () => {
    const v = siteView(
      live({ current: null, deploys: [], branch: "release" }),
      "main",
    );
    expect(nothingPublishedLine(v)).toBe(
      "Nothing has published yet. The next push to release publishes dist/.",
    );
  });

  it("names the repository default when the site names no branch", () => {
    const v = siteView(live({ current: null, deploys: [] }), "trunk");
    expect(nothingPublishedLine(v)).toBe(
      "Nothing has published yet. The next push to trunk publishes dist/.",
    );
  });
});

// ---------------------------------------------------------------------
// Honesty about a server this bundle is older than.

describe("siteView — a config_state we have never heard of", () => {
  it("reports the server's own word rather than guessing", () => {
    const v = siteView(status({ config_state: "quarantined" }), "main");
    expect(v.config).toEqual({ kind: "unknown", word: "quarantined" });
    // Emphatically not folded into `ok`: a panel claiming a config
    // parsed on the strength of not recognising the word is the same
    // class of bug as a runner card that says "ready" because it could
    // not check.
    expect(v.config.kind).not.toBe("ok");
    expect(nothingPublishedLine(v)).toBeNull();
  });

  it("quotes it in the sentence, whole", () => {
    expect(unknownConfigLine("quarantined")).toBe(
      'This server reports the site configuration as "quarantined", which ' +
        "this page is too old to explain.",
    );
  });
});

describe("siteView — an `ok` config with nothing attached", () => {
  it("falls back to the parser's own defaults, not to blanks", () => {
    const v = siteView(status({ config_state: "ok", config: null }), "main");
    expect(v.config).toEqual({
      kind: "ok",
      publish: "dist",
      spa: false,
      notFound: null,
    });
  });
});

// ---------------------------------------------------------------------
// The address, as it is read and as it is announced.

describe("siteLabel", () => {
  it("drops the scheme and any trailing slash", () => {
    expect(siteLabel("https://widget--acme.weft.dev/")).toBe(
      "widget--acme.weft.dev",
    );
    expect(siteLabel("http://widget--acme.weft.dev")).toBe(
      "widget--acme.weft.dev",
    );
  });

  it("leaves a path alone — only the trailing slash goes", () => {
    expect(siteLabel("https://widget--acme.weft.dev/docs/")).toBe(
      "widget--acme.weft.dev/docs",
    );
  });
});

describe("siteLinkLabel", () => {
  it("says what following the link does, and still contains the text", () => {
    const url = "https://widget--acme.weft.dev";
    const label = siteLinkLabel(url);
    // Exact, because "not just the raw URL" is the requirement and a
    // substring check would pass against the raw URL.
    expect(label).toBe("Open the published site at widget--acme.weft.dev");
    expect(label).not.toBe(url);
    // Voice control addresses a control by what is on screen, so the
    // visible text has to be inside the accessible name.
    expect(label.endsWith(siteLabel(url))).toBe(true);
  });
});

// ---------------------------------------------------------------------
// The copy the panel is required to carry.

describe("the panel's fixed words", () => {
  it("teaches one config path", () => {
    expect(CONFIG_PATH).toBe(".weft/site.yml");
  });

  it("shows a config that would actually work", () => {
    // The server's own default is `dist`, and the example has to be a
    // file somebody can paste — a key with no value publishes nothing.
    expect(EXAMPLE_CONFIG).toBe("publish: dist\n");
  });

  it("says the site is public, and says it about a private repository", () => {
    // The footgun, in words, exactly. Asserted whole rather than by
    // keyword: a warning that has drifted into something vaguer would
    // still contain "public".
    expect(PUBLIC_WARNING).toBe(
      "A published site is public. Anyone with the address can read every " +
        "file in the published directory, signed in or not, even while this " +
        "repository stays private.",
    );
  });
});
