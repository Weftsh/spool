import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { match, TOP_LEVEL, type Match } from "./routes";

const SRC = dirname(fileURLToPath(import.meta.url));

const at = (path: string, q = ""): Match => match(path, new URLSearchParams(q));

describe("match", () => {
  it("hands the whole dashboard address space to the signed-in shell", () => {
    // Deliberately does not re-decide the rest of the path: that
    // dispatch is asserted on by the whole existing e2e suite, and two
    // places deciding it is one place too many.
    for (const p of ["/dashboard", "/dashboard/", "/dashboard/settings/tokens"])
      expect(at(p)).toEqual({ kind: "dash" });
  });

  it("carries a topic on explore, and the front page too", () => {
    // The About rail's topic pills have always linked to
    // `/explore?topic=…`, and nothing read the parameter — so every pill
    // in the product landed on the unfiltered list of everything, which
    // reads as the topic having no repositories rather than as the
    // filter having been dropped.
    expect(at("/explore", "topic=rust")).toEqual({
      kind: "explore",
      topic: "rust",
    });
    // The front page is the same listing and takes the same filter.
    expect(at("/", "topic=rust")).toEqual({ kind: "explore", topic: "rust" });
    // Absent is empty, not undefined: one shape for the view to read.
    expect(at("/explore")).toEqual({ kind: "explore", topic: "" });
  });

  it("reads a bare namespace as a profile", () => {
    expect(at("/ada")).toEqual({
      kind: "owner",
      owner: "ada",
      tab: "overview",
    });
    expect(at("/acme", "tab=stars")).toEqual({
      kind: "owner",
      owner: "acme",
      tab: "stars",
    });
    // An unknown tab is the overview, not a blank page: `?tab=` is
    // somebody else's URL as often as it is ours.
    expect(at("/acme", "tab=nonsense")).toEqual({
      kind: "owner",
      owner: "acme",
      tab: "overview",
    });
  });

  it("reads owner/repo as the code tab, because that is what links point at", () => {
    expect(at("/acme/widget")).toEqual({
      kind: "repo",
      owner: "acme",
      repo: "widget",
      tab: "code",
      rest: [],
    });
  });

  it("names each repo tab", () => {
    for (const tab of ["issues", "changes", "insights", "settings"] as const)
      expect(at(`/acme/widget/${tab}`)).toEqual({
        kind: "repo",
        owner: "acme",
        repo: "widget",
        tab,
        rest: [],
      });
  });

  it("puts a file path behind `tree`, so a directory cannot shadow a tab", () => {
    // A docs repository with a top-level `issues/` directory is the case
    // this exists for: without the explicit segment, whether the file
    // opened would depend on the repository's own contents.
    expect(at("/acme/widget/tree/issues/index.md")).toEqual({
      kind: "repo",
      owner: "acme",
      repo: "widget",
      tab: "code",
      rest: ["issues", "index.md"],
    });
    expect(at("/acme/widget/issues")).toMatchObject({ tab: "issues" });
  });

  it("gives the header's search box somewhere to land", () => {
    // `/explore` is curated lists, not results; a query sent there would
    // be ignored or would quietly redefine what explore means.
    expect(at("/search", "q=widget")).toEqual({ kind: "search", q: "widget" });
    expect(at("/search")).toEqual({ kind: "search", q: "" });
  });

  it("parses the address a review notification mails out", () => {
    // `mail/templates.rs::change_activity` builds
    // `{public_url}/{org}/{repo}/changes/{change_key}`. Every
    // notification we send points at this shape, and for a while it
    // parsed correctly and then rendered "We couldn't find changes" —
    // a feature working right up to the last inch.
    //
    // If the mail template's path ever changes, this is where it should
    // hurt.
    expect(at("/acme/widget/changes/I0123456789abcdef")).toEqual({
      kind: "repo",
      owner: "acme",
      repo: "widget",
      tab: "changes",
      rest: ["I0123456789abcdef"],
    });
    expect(at("/acme/widget/changes")).toMatchObject({
      tab: "changes",
      rest: [],
    });
  });

  it("refuses a claimed segment rather than reading it as a namespace", () => {
    // These are reserved server-side precisely so nobody owns them; if
    // one fell through to the profile page it would render a namespace
    // that cannot exist, which reads as the platform being broken.
    expect(at("/notifications")).toEqual({ kind: "not-found" });
    expect(at("/topics")).toEqual({ kind: "not-found" });
  });

  it("only returns after signing in to a path on this origin", () => {
    // An open redirect on a login page is a phishing kit somebody else
    // gets to host on our domain.
    expect(at("/login", "next=/acme/widget")).toEqual({
      kind: "login",
      next: "/acme/widget",
      mode: "signin",
    });
    for (const hostile of [
      "next=https://elsewhere.test/",
      "next=//elsewhere.test/",
      "next=javascript:alert(1)",
      // WHATWG URL treats a backslash like a slash for http(s), so each
      // of these begins with one slash, carries no scheme, and still
      // resolves to another origin. The guard used to be a prefix check
      // and waved the first two straight through.
      "next=/\\elsewhere.test/p",
      "next=/\\/elsewhere.test",
      "next=\\\\elsewhere.test/p",
    ])
      expect(at("/login", hostile)).toEqual({
        kind: "login",
        next: "/",
        mode: "signin",
      });
  });

  it("opens the sign-up form when the site's button asks for it", () => {
    // The marketing site's "Sign up free" lands here. Without `mode`
    // the visitor got the sign-in form and had to find the "Create an
    // account" link under it — a button that said "sign up" and led to
    // a form that said "sign in".
    expect(at("/login", "mode=signup")).toEqual({
      kind: "login",
      next: "/",
      mode: "signup",
    });
    // Anything else is sign-in: a typo in a link should land on the
    // ordinary door, not on nothing.
    expect(at("/login", "mode=register")).toEqual({
      kind: "login",
      next: "/",
      mode: "signin",
    });
    expect(at("/login")).toEqual({ kind: "login", next: "/", mode: "signin" });
    // The mode and the return address are independent; asking for one
    // must not drop the other, and the open-redirect guard still holds.
    expect(at("/login", "mode=signup&next=/acme/widget")).toEqual({
      kind: "login",
      next: "/acme/widget",
      mode: "signup",
    });
    expect(at("/login", "mode=signup&next=https://elsewhere.test/")).toEqual(
      { kind: "login", next: "/", mode: "signup" },
    );
  });

  it("gives a changeset an address anybody can open", () => {
    // A changeset used to live only at `/dashboard/changesets/{key}` —
    // behind sign-in, and unreadable by a stranger — while a single
    // change had `/{owner}/{repo}/changes/{key}`, a real public address.
    // That was an inconsistency rather than a gap: the API's
    // `Scope::RepoRead` already admits an anonymous reader over public
    // repositories, so the review was readable on the wire the whole
    // time and only the routing was missing.
    expect(at("/acme/changesets/rename-payments")).toEqual({
      kind: "changeset",
      owner: "acme",
      key: "rename-payments",
    });
  });

  it("does not read `/{owner}/changesets` as an org-wide list", () => {
    // Deliberately not-found rather than a public list. The list's rows
    // are filtered per caller — you see the sets whose members you can
    // read — so a public one would silently hide half of itself, and a
    // list that lies about its own completeness is worse than none. The
    // signed-in list stays under `/dashboard`, where the caller is known.
    expect(at("/acme/changesets")).toEqual({ kind: "not-found" });
    // And nothing hangs off a changeset's address either: a stray tail
    // is a mistyped link, not a sub-page to invent.
    expect(at("/acme/changesets/rename-payments/members")).toEqual({
      kind: "not-found",
    });
  });

  it("branches on `changesets` before the repo arm, which needs the server's reservation", () => {
    // This arm sits ahead of `/{owner}/{repo}`, so it shadows any
    // repository of that name. That is only safe because no such
    // repository can exist: `RESERVED_REPO` in
    // `crates/stratum-control/src/registry.rs` refuses the name, because
    // `/{org}/changesets/{key}.git` is already the git wire address of a
    // changeset's workspace.
    //
    // Asserted against the Rust source rather than trusted. If somebody
    // ever unreserves the name, the failure would otherwise be a
    // repository nobody can open — and the report would be "my repo
    // renders the wrong page", which is a long way from the one-word
    // edit that caused it.
    const registry = readFileSync(
      join(SRC, "../../../crates/stratum-control/src/registry.rs"),
      "utf8",
    );
    expect(registry).toMatch(
      /const RESERVED_REPO: &\[&str\] = &\[[^\]]*"changesets"/,
    );
    // The shadowing it licenses, spelled out: this address is the
    // changeset, never a repository page.
    expect(at("/acme/changesets/rename-payments")).toMatchObject({
      kind: "changeset",
    });
  });

  it("keeps TOP_LEVEL sorted, because the server reads it as a list", () => {
    // `docs_e2e` asserts every entry is a reserved name. Sorted so a
    // duplicate or a stray addition is visible in a diff.
    expect([...TOP_LEVEL]).toEqual([...TOP_LEVEL].sort());
    expect(new Set(TOP_LEVEL).size).toBe(TOP_LEVEL.length);
  });
});

describe("the checks tab and its GitHub-shaped alias", () => {
  it("routes /{owner}/{repo}/checks to the checks tab", () => {
    const m = match("/acme/widget/checks", new URLSearchParams());
    expect(m).toMatchObject({ kind: "repo", tab: "checks", rest: [] });
    // Not a redirect: this is the real address, and a page that
    // redirected to itself would loop.
    expect((m as { redirect?: string }).redirect).toBeUndefined();
  });

  it("routes a hosted run's own address under the checks tab", () => {
    // `detail_url` on a hosted check row is this address, so it is a
    // link people are sent and a URL they paste. It is dispatched from
    // `rest` rather than by a tab of its own, which is what keeps the
    // tab strip lit and what makes the `/actions` alias carry it for
    // free.
    expect(
      match("/acme/widget/checks/runs/wr1", new URLSearchParams()),
    ).toMatchObject({
      kind: "repo",
      owner: "acme",
      repo: "widget",
      tab: "checks",
      rest: ["runs", "wr1"],
    });
  });

  it("sends /{owner}/{repo}/actions to checks, once, by moving the browser", () => {
    // The address somebody arriving from GitHub types. 404ing them to
    // prove a naming point helps nobody; giving the page two live URLs
    // is two things to keep the tab strip agreeing about and two for a
    // crawler to index as duplicates. So it redirects.
    const m = match("/acme/widget/actions", new URLSearchParams());
    expect(m).toMatchObject({
      kind: "repo",
      owner: "acme",
      repo: "widget",
      tab: "checks",
      redirect: "/acme/widget/checks",
    });
    // And the destination is a terminal state, which is what stops the
    // redirect being a loop.
    expect(
      (
        match("/acme/widget/checks", new URLSearchParams()) as {
          redirect?: string;
        }
      ).redirect,
    ).toBeUndefined();
  });

  it("does not read `actions` as an alias anywhere but a repo tab", () => {
    // A namespace called `actions` is a namespace, and a repository
    // called `actions` is a repository. The alias is a *third* path
    // segment and nothing else — which is also why it needs no
    // server-side name reservation.
    expect(match("/actions", new URLSearchParams())).toMatchObject({
      kind: "owner",
      owner: "actions",
    });
    expect(match("/acme/actions", new URLSearchParams())).toMatchObject({
      kind: "repo",
      repo: "actions",
      tab: "code",
    });
  });

  it("keeps whatever follows the alias, so a deep link is not truncated", () => {
    // The redirect target, not just the parsed `rest`. Building the
    // target from the first three segments alone parses correctly and
    // then sends the reader to a list — which reads as their link being
    // stale rather than as us having thrown the tail away.
    expect(
      match("/acme/widget/actions/runs/7", new URLSearchParams()),
    ).toMatchObject({
      tab: "checks",
      rest: ["runs", "7"],
      redirect: "/acme/widget/checks/runs/7",
    });
  });

  it("keeps the query string too, which is where the filters live", () => {
    // The same argument the deep-link test above makes, one component
    // over, and it was the same bug. The redirect was built from path
    // segments alone, so `/o/r/actions?workflow=CI` landed on an
    // unfiltered Checks table with nothing to say the filter had been
    // dropped — which reads as the link being stale. Every filter the
    // Checks page has rides in the query, so dropping it throws away
    // most of what a shared address is *for*.
    expect(
      match(
        "/acme/widget/actions",
        new URLSearchParams({ workflow: "CI", state: "failure" }),
      ),
    ).toMatchObject({
      tab: "checks",
      redirect: "/acme/widget/checks?workflow=CI&state=failure",
    });
    // Segments and query together, since a deep link is allowed both.
    expect(
      match(
        "/acme/widget/actions/runs/7",
        new URLSearchParams({ workflow: "CI" }),
      ),
    ).toMatchObject({ redirect: "/acme/widget/checks/runs/7?workflow=CI" });
    // A value needing encoding survives it, rather than re-parsing as
    // syntax on the way out — the same hazard the segments have.
    expect(
      match(
        "/acme/widget/actions",
        new URLSearchParams({ branch: "feat/a b&c" }),
      ),
    ).toMatchObject({
      redirect: "/acme/widget/checks?branch=feat%2Fa+b%26c",
    });
    // And no query means no trailing `?`: a bare `?` is a different
    // address to share, to bookmark and to compare, for no gain.
    expect(match("/acme/widget/actions", new URLSearchParams())).toMatchObject({
      redirect: "/acme/widget/checks",
    });
  });

  it("re-encodes the segments it rebuilds the address from", () => {
    // `segments()` hands these back decoded, so a name carrying a `?`
    // or a `#` would re-parse as query or fragment syntax on the way
    // out and the redirect would go somewhere else entirely.
    const m = match(
      "/acme/widget/actions/a%3Fb/c%23d",
      new URLSearchParams(),
    ) as { redirect?: string };
    expect(m.redirect).toBe("/acme/widget/checks/a%3Fb/c%23d");
  });
});
