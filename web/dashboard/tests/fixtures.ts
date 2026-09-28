/// The mocked control plane every dashboard spec runs against.
///
/// These live here rather than in `dashboard.spec.ts` because they are
/// no longer one suite's private furniture: the forge, profile,
/// repo-tab and issues suites all need the same org, the same three
/// repos and the same two ways of signing in. A fixture copied into a
/// second spec is a fixture that drifts, and a suite that drifts from
/// the one it was copied from tests a world the other one does not.
import { expect, type Page } from "@playwright/test";

/// The three repositories every signed-in suite works with.
///
/// `viewer_admin`, `viewer_member` and `viewer_write` are on all of them
/// because the suite signs in as the org's owner. They are the server's
/// own answers to three different questions — may this person administer
/// it, do they hold any role here at all, may they push — handed down
/// rather than inferred, and the repository page gates a real control on
/// each: Settings on the first, Insights on the second, the Changes
/// tab's Land and start-review forms on the third. A fixture without
/// them renders the page a reader with no role gets, and every
/// assertion about a member's controls fails for a reason that has
/// nothing to do with the test.
export const REPOS = {
  repos: [
    {
      id: "01aaa",
      org_id: "01org",
      name: "widget",
      description: "the fast one",
      kind: "mirror",
      default_branch: "main",
      origin_url: "acme/widget",
      last_sync_at: Date.now() - 30_000,
      last_synced_commit: "abc",
      sync_error: null,
      created_at: Date.now() - 86_400_000,
      clone_url: "https://x/acme/widget.git",
      ssh_clone_url: "ssh://git@x:22/acme/widget.git",
      viewer_admin: true,
      viewer_member: true,
      viewer_write: true,
    },
    {
      id: "01bbb",
      org_id: "01org",
      name: "broken-mirror",
      description: null,
      kind: "mirror",
      default_branch: "main",
      origin_url: "acme/broken",
      last_sync_at: Date.now() - 600_000,
      last_synced_commit: "def",
      sync_error: "origin unreachable: connection refused",
      created_at: Date.now() - 86_400_000,
      clone_url: "https://x/acme/broken.git",
      ssh_clone_url: "ssh://git@x:22/acme/broken.git",
      viewer_admin: true,
      viewer_member: true,
      viewer_write: true,
    },
    {
      id: "01ccc",
      org_id: "01org",
      name: "session-1",
      description: null,
      kind: "native",
      default_branch: "main",
      origin_url: null,
      last_sync_at: null,
      last_synced_commit: null,
      sync_error: null,
      created_at: Date.now() - 3_600_000,
      clone_url: "https://x/acme/session-1.git",
      ssh_clone_url: null,
      viewer_admin: true,
      viewer_member: true,
      viewer_write: true,
    },
  ],
};

export const USAGE = {
  days: [
    {
      day: "2026-08-20",
      active_repos: 2,
      total_repos: 3,
      requests: 420,
      bytes_out: 52_428_800,
      reported_at: null,
    },
    {
      day: "2026-08-19",
      active_repos: 1,
      total_repos: 3,
      requests: 120,
      bytes_out: 10_485_760,
      reported_at: 1,
    },
  ],
};

export const METRICS = {
  repo: "widget",
  from: 0,
  to: 1,
  kinds: {
    clone: {
      count: 24,
      bytes: 1_073_741_824,
      ms_sum: 24_000,
      p50_ms: 512,
      p99_ms: 2048,
    },
    fetch: {
      count: 310,
      bytes: 20_971_520,
      ms_sum: 9_300,
      p50_ms: 32,
      p99_ms: 128,
    },
    freshness: {
      count: 0,
      bytes: 0,
      ms_sum: 12_000,
      p50_ms: 4096,
      p99_ms: 8192,
    },
  },
  sync: { last_sync_at: Date.now() - 30_000, sync_error: null },
};

export const ME = {
  id: "01user",
  email: "owner@acme.test",
  name: "Ada Owner",
  created_at: Date.now() - 86_400_000,
  orgs: [{ id: "01org", name: "acme", role: "owner" }],
};

export const MEMBERS = {
  members: [
    {
      user_id: "01user",
      email: "owner@acme.test",
      name: "Ada Owner",
      role: "owner",
      disabled: false,
      created_at: Date.now() - 86_400_000,
    },
    {
      user_id: "01user2",
      email: "dev@acme.test",
      name: "Dev Person",
      role: "member",
      disabled: false,
      created_at: Date.now() - 3_600_000,
    },
  ],
};

export const INVITES = {
  invites: [
    {
      id: "01inv",
      email: "pending@acme.test",
      role: "viewer",
      created_at: Date.now() - 60_000,
      expires_at: Date.now() + 86_400_000,
      accepted_at: null,
    },
  ],
};

export const TOKENS = {
  tokens: [
    {
      id: "01tok1",
      label: "laptop",
      scopes: ["repo:write"],
      repo_id: null,
      user_id: "01user",
      created_at: Date.now() - 3_600_000,
      revoked_at: null,
    },
  ],
};

/// A trail long enough to page through: the panel asks for 100 at a time
/// and only offers "Load more" when a page comes back full, so a short
/// fixture would never exercise the cursor.
/// Teams, and a repo's access map. Stateful in the mock so the tests
/// assert what the panel *did*, not just what it drew.
export const TEAMS = [
  {
    id: "01team1",
    name: "payments",
    description: "the squad",
    created_at: Date.now() - 86_400_000,
    member_count: 1,
  },
];

export const AUDIT_TOTAL = 105;
export const AUDIT = Array.from({ length: AUDIT_TOTAL }, (_, i) => ({
  // Newest first: seq 105 down to 1.
  seq: AUDIT_TOTAL - i,
  at: Date.now() - (i + 1) * 60_000,
  org_id: "01org",
  repo_id: i % 2 === 0 ? REPOS.repos[0].id : null,
  principal: "user:01user",
  user_id: i % 3 === 0 ? "01user" : "01user2",
  user_email: i % 3 === 0 ? "owner@acme.test" : "member@acme.test",
  user_name: i % 3 === 0 ? "Ada Owner" : "Bo Member",
  action: i === 0 ? "token.mint" : "repo.commit",
  context: i === 0 ? { scopes: "org:read,repo:write" } : null,
}));

export const KEYS = [
  {
    id: "01key1",
    token_id: null,
    user_id: "01user",
    algo: "ssh-ed25519",
    public_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGRlYWRiZWVm",
    fingerprint_sha256: "SHA256:xyrwjwNKqTIivpsCwlGBJoCJCtNo5voQKyNc3jGN9iI",
    label: "laptop",
    created_at: Date.now() - 86_400_000,
    revoked_at: null,
  },
];

export const ACCESS = {
  people: [
    {
      user_id: "01user",
      email: "owner@acme.test",
      name: "Ada Owner",
      role: "owner",
      source: "org_role",
      team_id: null,
      team_name: null,
    },
    {
      user_id: "01user2",
      email: "dev@acme.test",
      name: "Dev Person",
      role: "admin",
      source: "team",
      team_id: "01team1",
      team_name: "payments",
    },
  ],
  teams: [
    {
      team_id: "01team1",
      team_name: "payments",
      role: "admin",
      member_count: 1,
    },
  ],
};

export async function mockApi(
  page: Page,
  keys = [...KEYS],
  auditQueries: URLSearchParams[] = [],
  teams = TEAMS.map((t) => ({ ...t })),
  teamRoster: Record<
    string,
    { user_id: string; email: string; name: string; created_at: number }[]
  > = {
    "01team1": [
      {
        user_id: "01user2",
        email: "dev@acme.test",
        name: "Dev Person",
        created_at: Date.now(),
      },
    ],
  },
  access = ACCESS,
  grants: unknown[] = [],
) {
  // Refuse anything below does not name. **Registered first**, so every
  // specific route that follows wins — Playwright matches the most
  // recently registered handler.
  //
  // `mockApi` enumerates the endpoints the dashboard calls, which is
  // stricter than a catch-all for everything it lists and no protection
  // at all for anything it does not. That gap is not hypothetical and
  // does not come from anybody editing this file: it opens when a
  // *page* gains a fetch. `/orgs/acme/repos/ledger` was reaching vite's
  // proxy — and therefore whatever is listening on :8080, which during
  // development is a real seeded server — from tests written long
  // before that call existed.
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) => r.fulfill({ json: ME }));
  await page.route("**/v1/auth/login", (r) => r.fulfill({ json: ME }));
  await page.route("**/v1/auth/logout", (r) =>
    r.fulfill({ status: 204, body: "" }),
  );
  await page.route("**/v1/orgs/acme/members", (r) =>
    r.fulfill({ json: MEMBERS }),
  );
  await page.route("**/v1/orgs/acme/members/*", (r) =>
    r.fulfill({ status: 204, body: "" }),
  );
  await page.route("**/v1/orgs/acme/invites", (r) => {
    if (r.request().method() === "POST") {
      return r.fulfill({
        status: 201,
        json: {
          id: "01inv2",
          invite_link: "stinv_01inv2_secretsecret",
          mail: { sent: true },
        },
      });
    }
    return r.fulfill({ json: INVITES });
  });
  await page.route("**/v1/orgs/acme/invites/*", (r) =>
    r.fulfill({ status: 204, body: "" }),
  );
  await page.route("**/v1/orgs/acme/tokens", (r) => {
    if (r.request().method() === "POST") {
      return r.fulfill({
        status: 201,
        json: { id: "01tok9", token: "weft_01tok9_freshsecret" },
      });
    }
    return r.fulfill({
      json: {
        ...TOKENS,
        mintable_scopes: ["org:admin", "org:read", "repo:read", "repo:write"],
      },
    });
  });
  await page.route("**/v1/orgs/acme/tokens/*", (r) =>
    r.fulfill({ status: 204, body: "" }),
  );
  await page.route("**/v1/orgs/acme/repos?limit=200", (r) =>
    r.fulfill({ json: REPOS }),
  );
  await page.route("**/v1/orgs/acme/usage", (r) => r.fulfill({ json: USAGE }));
  await page.route("**/v1/orgs/acme/repos/widget/metrics", (r) =>
    r.fulfill({ json: METRICS }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({ json: REPOS.repos[0] }),
  );
  await page.route("**/v1/orgs/acme/repos/session-1/metrics", (r) =>
    r.fulfill({ json: { ...METRICS, repo: "session-1", kinds: {} } }),
  );
  await page.route("**/v1/orgs/acme/repos/session-1", (r) =>
    r.fulfill({ json: REPOS.repos[2] }),
  );
  // Stateful audit mock: honours order, the `before` cursor, the filters
  // and `format=csv`, so the panel's query string is the thing under test
  // rather than a fixture that answers the same way whatever it is asked.
  await page.route("**/v1/orgs/acme/audit*", (r) => {
    const url = new URL(r.request().url());
    auditQueries.push(url.searchParams);
    let rows = AUDIT;
    const user = url.searchParams.get("user");
    if (user) rows = rows.filter((e) => e.user_id === user);
    const action = url.searchParams.get("action");
    if (action) rows = rows.filter((e) => e.action === action);
    const since = url.searchParams.get("since");
    if (since) rows = rows.filter((e) => e.at >= Number(since));
    const before = url.searchParams.get("before");
    if (before) rows = rows.filter((e) => e.seq < Number(before));
    const limit = Number(url.searchParams.get("limit") ?? 100);
    rows = rows.slice(0, limit);
    if (url.searchParams.get("format") === "csv") {
      return r.fulfill({
        status: 200,
        contentType: "text/csv",
        body: `seq,at,action,principal,user_email,repo_id,context\n${rows
          .map(
            (e) =>
              `${e.seq},${e.at},"${e.action}","${e.principal}","${e.user_email}","",""`,
          )
          .join("\n")}\n`,
      });
    }
    return r.fulfill({
      json: {
        entries: rows,
        next_after: null,
        next_before: rows.length ? rows[rows.length - 1].seq : null,
      },
    });
  });
  // Stateful teams mock: create appends, delete removes, membership
  // moves, so "the panel called the right endpoint" is observable.
  await page.route("**/v1/orgs/acme/teams", (r) => {
    if (r.request().method() === "POST") {
      const body = r.request().postDataJSON() as {
        name: string;
        description: string | null;
      };
      const t = {
        id: `01team${teams.length + 1}`,
        name: body.name,
        description: body.description,
        created_at: Date.now(),
        member_count: 0,
      };
      teams.push(t);
      teamRoster[t.id] = [];
      return r.fulfill({ status: 201, json: t });
    }
    return r.fulfill({ json: { teams } });
  });
  await page.route("**/v1/orgs/acme/teams/*/members/*", (r) => {
    const parts = new URL(r.request().url()).pathname.split("/");
    const user = parts.pop()!;
    parts.pop();
    const team = parts.pop()!;
    const roster = (teamRoster[team] ??= []);
    if (r.request().method() === "PUT") {
      if (!roster.some((m) => m.user_id === user)) {
        const who = MEMBERS.members.find((m) => m.user_id === user)!;
        roster.push({
          user_id: user,
          email: who.email,
          name: who.name,
          created_at: Date.now(),
        });
      }
    } else {
      teamRoster[team] = roster.filter((m) => m.user_id !== user);
    }
    const t = teams.find((x) => x.id === team);
    if (t) t.member_count = teamRoster[team].length;
    return r.fulfill({ status: 204, body: "" });
  });
  await page.route("**/v1/orgs/acme/teams/*/members", (r) => {
    const team = new URL(r.request().url()).pathname.split("/").at(-2)!;
    return r.fulfill({ json: { members: teamRoster[team] ?? [] } });
  });
  await page.route("**/v1/orgs/acme/teams/*", (r) => {
    const id = new URL(r.request().url()).pathname.split("/").pop()!;
    if (r.request().method() === "DELETE") {
      const i = teams.findIndex((t) => t.id === id);
      if (i >= 0) teams.splice(i, 1);
    }
    return r.fulfill({ status: 204, body: "" });
  });
  // Every repo answers, so no test leaks a request past the mocks to
  // vite's proxy — a hermetic suite must not depend on a connection
  // being refused. Repos other than `widget` answer the way the server
  // answers a caller who may not read the map, which is what the panel
  // is built to handle.
  // The branch-policy panel reads the protections list on every native
  // repo view; an empty fence is the honest default for the fixtures.
  await page.route("**/v1/orgs/acme/repos/*/protections", (r) =>
    r.fulfill({ json: { protections: [] } }),
  );
  // The About rail's panel, read on every repository page.
  //
  // It lives here rather than in one suite because *every* spec that
  // navigates to a repository page now makes this call, and an unmocked
  // `/v1` request does not fail — it falls through vite's proxy to
  // whatever is listening on :8080, which during development is a real
  // seeded server. A suite in that state is hermetic only while nobody
  // has the manual stack up, which is the sort of intermittency that
  // gets called a flake and re-run. It has been found here once already.
  //
  // Empty rather than populated on purpose: this is the fixture for
  // suites that are about something else, and a language bar appearing
  // in a screenshot test nobody asked to change is its own noise. The
  // populated shapes live in `repo-about.spec.ts`, which is about them.
  await page.route("**/v1/orgs/acme/repos/*/meta", (r) =>
    r.fulfill({
      json: {
        topics: [],
        languages: [],
        languages_truncated: false,
        license: null,
        community: [],
        // Present and null, not absent. The server always serialises
        // this key, and a fixture that omitted it would let a client
        // reading `meta.readme` work here and break in production
        // against a server that sends `null` — the mock has to be the
        // contract, not a convenient subset of it.
        readme: null,
      },
    }),
  );
  // The rest of the About rail's reads, and the Checks tab's.
  //
  // Registered here even though the suites that use this fixture are
  // about other things, because the alternative is worse than noise:
  // each unregistered path falls to the catch-all 404 above, every
  // component swallows it by design, and the rail silently renders
  // short in every screenshot and every audit — which is exactly the
  // state a regression in one of these would also produce. A mock that
  // is indistinguishable from the bug is not a mock.
  //
  // Empty rather than populated, for the same reason `meta` is: the
  // populated shapes belong in the suites that are about them.
  await page.route("**/v1/orgs/acme/repos/*/contributors*", (r) =>
    r.fulfill({ json: { contributors: [] } }),
  );
  await page.route("**/v1/orgs/acme/repos/*/forks", (r) =>
    r.request().method() === "POST"
      ? r.fulfill({ status: 404, json: { error: "not mocked by this test" } })
      : r.fulfill({ json: { forks: [], count: 0 } }),
  );
  await page.route("**/v1/orgs/acme/repos/*/tags", (r) =>
    r.fulfill({ json: { tags: [] } }),
  );
  await page.route("**/v1/orgs/acme/repos/*/checks/runs*", (r) =>
    r.fulfill({ json: { runs: [], workflows: [], next_before: null } }),
  );
  // A native repository with no GitHub origin — the ordinary case, and
  // the one whose empty Checks tab points at the intake rather than at
  // a permission problem.
  await page.route("**/v1/orgs/acme/repos/*/ci/poll", (r) =>
    r.fulfill({
      json: {
        provider: "github",
        connected: false,
        polled: false,
        denied: false,
        error: null,
        high_water: null,
        resuming_from: null,
        retry_in_ms: null,
      },
    }),
  );
  await page.route("**/v1/orgs/acme/repos/*/access", (r) =>
    r.request().url().includes("/widget/")
      ? r.fulfill({ json: access })
      : r.fulfill({ status: 404, json: { error: "not found" } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget/grants", (r) => {
    grants.push(r.request().postDataJSON());
    return r.fulfill({ status: 204, body: "" });
  });
  await page.route("**/v1/orgs/acme/repos/widget/grants/*", (r) => {
    grants.push({
      revoked_user: new URL(r.request().url()).pathname.split("/").pop(),
    });
    return r.fulfill({ status: 204, body: "" });
  });
  await page.route("**/v1/orgs/acme/repos/widget/team-grants/*", (r) => {
    grants.push({
      revoked_team: new URL(r.request().url()).pathname.split("/").pop(),
    });
    return r.fulfill({ status: 204, body: "" });
  });
  // Stateful ssh-keys mock: POST appends, DELETE marks revoked.
  await page.route("**/v1/orgs/acme/ssh-keys", (r) => {
    if (r.request().method() === "POST") {
      const body = r.request().postDataJSON() as {
        public_key: string;
        token_id?: string;
        label?: string;
      };
      const key = {
        id: `01key${keys.length + 1}`,
        token_id: body.token_id ?? null,
        user_id: body.token_id ? null : "01user",
        algo: "ssh-ed25519",
        public_key: body.public_key,
        fingerprint_sha256: "SHA256:newkeyfingerprintnewkeyfingerprintnewkeyfp",
        label: body.label ?? null,
        created_at: Date.now(),
        revoked_at: null,
      };
      keys.push(key);
      return r.fulfill({ status: 201, json: key });
    }
    return r.fulfill({ json: { keys } });
  });
  await page.route("**/v1/orgs/acme/ssh-keys/*", (r) => {
    const id = r.request().url().split("/").pop()!;
    const k = keys.find((k) => k.id === id);
    if (k) k.revoked_at = Date.now();
    return r.fulfill({ status: 204, body: "" });
  });
}

/// Sign in the way a service caller does: paste an org and a token. The
/// boot probe must answer "not signed in" first, or the app would restore
/// a cookie session instead of showing the form.
export async function signIn(
  page: Page,
  auditQueries: URLSearchParams[] = [],
  teams = TEAMS.map((t) => ({ ...t })),
  grants: unknown[] = [],
) {
  await mockApi(
    page,
    [...KEYS],
    auditQueries,
    teams,
    undefined,
    ACCESS,
    grants,
  );
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.goto("/dashboard/");
  await page
    .getByRole("button", { name: "Sign in with an API token instead" })
    .click();
  await page.getByPlaceholder("acme").fill("acme");
  await page.getByPlaceholder("weft_…").fill("weft_test_token");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  // Wait for the credential to actually be stored, not merely for the
  // click to return. Signing in is a click, a state update and a write to
  // localStorage, and a test that reloads the page next would otherwise
  // race that write and boot signed-out — intermittently, and only when
  // the machine is busy enough, which is the worst way to find out.
  // Waiting on the storage entry rather than on a rendered element waits
  // on the thing the next interaction actually depends on.
  await page.waitForFunction(() => !!localStorage.getItem("stratum-session"));
}

/// Sign in the way a person does: email and password, and the session is
/// a cookie the page can never read.
export async function signInAsPerson(page: Page) {
  await mockApi(page);
  await page.route("**/v1/auth/me", (r) =>
    r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.goto("/dashboard/");
  await page.getByLabel("Email").fill("owner@acme.test");
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(page.getByText("Requests today")).toBeVisible();
}
