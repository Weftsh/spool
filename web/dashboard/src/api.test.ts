import { afterEach, describe, expect, it, vi } from "vitest";
import { api, type Session } from "@/api";

/// The required-checks routes are the two-slashes case, and they are the
/// reason this file exists.
///
/// `/required-checks/*branch` is a trailing catch-all because a branch
/// name contains slashes (`release/2.0`), and the check's *own* name
/// contains them too (`ci/tests`) — which is why the name rides in the
/// body on POST and in the query string on DELETE rather than being a
/// second path segment. Getting either encoding wrong is silent: the
/// request goes out, the server answers about a branch or a check nobody
/// has, and the UI reports an empty list as though the policy were empty.
/// Nothing on screen says which.
function captureFetch() {
  const calls: { url: string; init?: RequestInit }[] = [];
  const spy = vi
    .spyOn(globalThis, "fetch")
    .mockImplementation(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        calls.push({ url: String(input), init });
        return new Response(JSON.stringify({ required_checks: [] }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      },
    );
  return { calls, spy };
}

const session: Session = { org: "acme", token: "t0ken" };

afterEach(() => vi.restoreAllMocks());

describe("required-checks request shapes", () => {
  it("leaves the slashes in a branch name alone", async () => {
    // `encodeURIComponent` would send `release%2F2.0`, which the
    // wildcard route reads as a single segment naming a branch that does
    // not exist — a 200 with an empty list, indistinguishable from a
    // branch that genuinely requires nothing.
    const { calls } = captureFetch();
    await api.requiredChecks(session, "app", "release/2.0");
    expect(calls[0].url).toContain("/required-checks/release/2.0");
    expect(calls[0].url).not.toContain("%2F");
  });

  it("still escapes what is not a separator", async () => {
    const { calls } = captureFetch();
    await api.requiredChecks(session, "app", "feature/a b");
    expect(calls[0].url).toContain("/required-checks/feature/a%20b");
  });

  it("sends a check name in the body, never in the path", async () => {
    // `ci/tests` in the path would be read as two branch segments.
    const { calls } = captureFetch();
    await api.requireCheck(session, "app", "main", "ci/tests");
    expect(calls[0].url).toContain("/required-checks/main");
    expect(calls[0].url).not.toContain("ci/tests");
    expect(String(calls[0].init?.body)).toContain("ci/tests");
  });

  it("escapes a check name into the query string on delete", async () => {
    // Here the slash *is* escaped: it is a value inside a parameter, not
    // a path separator, and `?name=ci/tests` would be a different string
    // to whatever the server parsed.
    const { calls } = captureFetch();
    await api.unrequireCheck(session, "app", "release/2.0", "ci/tests");
    expect(calls[0].url).toContain("/required-checks/release/2.0?");
    expect(calls[0].url).toContain("name=ci%2Ftests");
    expect(calls[0].init?.method).toBe("DELETE");
  });
});
