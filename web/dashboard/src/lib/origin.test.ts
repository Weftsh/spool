import { describe, expect, it } from "vitest";
import { originHref, originLabel } from "./origin";

describe("originLabel", () => {
  it("names the forge a reader is being sent to", () => {
    expect(originLabel("https://github.com/rails/rails")).toBe("GitHub");
    expect(originLabel("https://gitlab.com/g/p")).toBe("GitLab");
    expect(originLabel("https://codeberg.org/a/b")).toBe("Codeberg");
  });

  it("falls back to the bare host rather than guessing a brand", () => {
    // Confidently printing the wrong forge's name beside somebody's
    // count is worse than printing the host: the label is a provenance
    // claim, and a wrong one is exactly the dishonesty the two-field
    // rule exists to prevent.
    expect(originLabel("https://git.example.org/a/b")).toBe("git.example.org");
  });

  it("says 'upstream' when there is nothing host-shaped to name", () => {
    // Mirror creation accepts a bare `owner/name` shorthand, so this is
    // a real stored value and not a hypothetical. "on acme" would read
    // as a forge called acme.
    expect(originLabel("acme/widget")).toBe("upstream");
    expect(originLabel(null)).toBe("upstream");
    expect(originLabel("")).toBe("upstream");
  });
});

describe("originHref", () => {
  it("passes the two schemes a link can be", () => {
    expect(originHref("https://github.com/a/b")).toBe("https://github.com/a/b");
    expect(originHref("http://git.example.org/a/b")).toBe(
      "http://git.example.org/a/b",
    );
  });

  it("refuses anything that executes rather than navigates", () => {
    // The origin URL is a person's typing in a mirror form. An
    // allowlist, not a denylist, for the same reason as the profile
    // rail's: a list of schemes to refuse is never finished.
    expect(originHref("javascript:alert(1)")).toBeNull();
    expect(originHref("data:text/html,<script>alert(1)</script>")).toBeNull();
    expect(originHref("vbscript:msgbox(1)")).toBeNull();
  });

  it("renders the shorthand as text rather than a broken link", () => {
    // `acme/widget` is a legitimate stored origin and not a URL. Making
    // it an href would produce a link to a path on our own host that
    // means nothing.
    expect(originHref("acme/widget")).toBeNull();
    expect(originHref(null)).toBeNull();
  });
});
