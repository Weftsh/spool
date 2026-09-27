import { describe, expect, it } from "vitest";
import { httpUrl, linkLabel } from "./profile-rail";

describe("httpUrl", () => {
  it("passes the two schemes a website can be", () => {
    expect(httpUrl("https://example.com")).toBe("https://example.com");
    expect(httpUrl("http://notes.example.com/x")).toBe(
      "http://notes.example.com/x",
    );
  });

  it("refuses a scheme that executes rather than navigates", () => {
    // The control plane refuses these at the write. This is the second
    // lock on the same door: the value reaching an `href` is the point
    // where a disagreement between the two becomes script execution,
    // and rows are read by a client that cannot see the validator.
    expect(httpUrl("javascript:alert(1)")).toBeNull();
    expect(httpUrl("JavaScript:alert(1)")).toBeNull();
    expect(httpUrl("data:text/html,<script>alert(1)</script>")).toBeNull();
    expect(httpUrl("vbscript:msgbox(1)")).toBeNull();
    expect(httpUrl("file:///etc/passwd")).toBeNull();
  });

  it("does not drop somebody's real link over stored whitespace", () => {
    // This is the assertion the `trim()` exists for, and it is a
    // false-negative one rather than a security one. An allowlist
    // refuses `" javascript:…"` whether or not it trims — the scheme
    // never matches `http` either way — so a test that spelled this as
    // "stops a smuggled scheme" would be describing a mechanism it does
    // not have. What trimming really prevents is a stored
    // `" https://example.com"` being read as hostile and the owner's
    // link vanishing from their own profile with no error anywhere.
    expect(httpUrl("  https://example.com  ")).toBe("https://example.com");
    expect(httpUrl("\n\thttps://example.com")).toBe("https://example.com");
    // Refused with the whitespace, as it is without it — for the
    // allowlist's reason, not the trim's.
    expect(httpUrl("  \n\tjavascript:alert(1)")).toBeNull();
  });

  it("refuses a scheme-relative URL, which has no scheme to check", () => {
    expect(httpUrl("//evil.example")).toBeNull();
    expect(httpUrl("/relative")).toBeNull();
    expect(httpUrl("")).toBeNull();
  });
});

describe("linkLabel", () => {
  it("prefers the label its owner gave", () => {
    expect(linkLabel({ label: "My notes", url: "https://example.com" })).toBe(
      "My notes",
    );
  });

  it("falls back to the URL without its scheme or trailing slash", () => {
    // The rail is 296px. A reader deciding whether to follow a link is
    // deciding about the host, and `https://` is eight characters of
    // that budget spent saying nothing.
    expect(linkLabel({ label: null, url: "https://example.com/" })).toBe(
      "example.com",
    );
    expect(linkLabel({ label: null, url: "http://notes.example.com/x" })).toBe(
      "notes.example.com/x",
    );
  });

  it("treats a label of only spaces as no label", () => {
    // Whitespace survives a text input and the server's cap, and a link
    // rendered as an empty string is a link nobody can click.
    expect(linkLabel({ label: "   ", url: "https://example.com" })).toBe(
      "example.com",
    );
  });
});
