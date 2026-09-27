import { describe, expect, it } from "vitest";
import { authorLabel, isMarkdown } from "./browse";

describe("isMarkdown", () => {
  it("opens prose on its preview and everything else on its source", () => {
    expect(isMarkdown("README.md")).toBe(true);
    expect(isMarkdown("CHANGELOG.MD")).toBe(true);
    expect(isMarkdown("notes.markdown")).toBe(true);
    expect(isMarkdown("main.rs")).toBe(false);
    expect(isMarkdown("md")).toBe(false);
    expect(isMarkdown("readme.md.bak")).toBe(false);
  });
});

describe("authorLabel", () => {
  it("does not put a raw principal id on the page", () => {
    // The repository front page said a twenty-six character ULID had
    // last touched the code, because a commit made over REST carries
    // `token:<id>` in its identity line.
    const out = authorLabel("token:01m1055ewr1t0myw7kcccrpxfd");
    expect(out).toEqual({ label: "token", machine: true });
  });

  it("names a labelled token, and still says it is one", () => {
    // The bootstrap token signs as `token:bootstrap-admin`; a seeded
    // history read as written by nobody in particular when every row
    // said "token". The label is the answer; the kind stays so a machine
    // cannot borrow a person's name.
    expect(authorLabel("token:release-bot")).toEqual({
      label: "release-bot (token)",
      machine: true,
    });
    // A bare prefix, from a server older than labels in the identity
    // line, is still just the kind.
    expect(authorLabel("token:")).toEqual({ label: "token", machine: true });
  });

  it("names the other machine principals too", () => {
    expect(authorLabel("system:lander")).toEqual({
      label: "lander (system)",
      machine: true,
    });
    expect(authorLabel("agent:claude").machine).toBe(true);
  });

  it("leaves a person alone, and does not let a machine borrow their name", () => {
    expect(authorLabel("Ada Owner")).toEqual({
      label: "Ada Owner",
      machine: false,
    });
    // A person whose name merely contains the word is still a person.
    expect(authorLabel("Token Smith").machine).toBe(false);
  });
});
