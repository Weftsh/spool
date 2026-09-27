// Settings → Runners: the policy, the groups, the machines, and the two
// commands that add one.
//
// `src/lib/runners.test.ts` covers the decisions — the state vocabulary,
// the label order, the command builder, the access sentence — and none
// of that is repeated here. What is here is the half a unit test cannot
// see:
//
// - **every write actually reaches the route it claims to.** A panel
//   that renders the new state optimistically and sends nothing looks
//   identical to one that works, until a reload. So the mocks are
//   stateful and the assertions are about what the server was told.
// - **the save is delayed.** A route that answers instantly cannot show
//   a "Saving…" state ever being on screen, and cannot catch a "Saved"
//   tick that was rendered before the response arrived. Every write here
//   answers after a beat, and the assertions about what is on screen
//   *during* it are as load-bearing as the ones after.
// - **"Saved" is a claim about the server.** It has to appear only after
//   the PATCH lands and disappear the moment somebody edits again — a
//   tick standing beside a control that has since changed is a lie about
//   what the organisation's policy actually is.
// - **destructive things take two steps, in place.** Never
//   `window.confirm`: a browser dialog cannot be styled, cannot be
//   driven by the walkthrough, and blocks the tab. The first press must
//   send nothing.
// - **a refusal is the server's own sentence.** These panels sit in
//   front of routes that answer 409 on a duplicate group name and 422 on
//   the default group, and the difference between "that name is taken"
//   and "you may not" is the whole content of the message.

import { expect, test, type Page } from "@playwright/test";
import { signIn } from "./fixtures";
import { PREVIEW_ORIGIN } from "./preview";

const NOW = Date.now();

interface Group {
  id: string;
  name: string;
  repo_access: "all" | "selected";
  allow_public: boolean;
  is_default: boolean;
  repos: string[];
  runners: number;
  created_at: number;
  updated_at: number;
}

interface Runner {
  id: string;
  name: string;
  labels: string[];
  os: string;
  arch: string;
  version: string;
  ephemeral: boolean;
  group: { id: string; name: string };
  state: string;
  last_seen_at: number;
  created_at: number;
  job: { run_id: string; job_id: string; key: string; repo?: string } | null;
}

function groups(): Group[] {
  return [
    {
      id: "g-default",
      name: "default",
      repo_access: "all",
      allow_public: false,
      is_default: true,
      repos: [],
      runners: 2,
      created_at: NOW - 86_400_000,
      updated_at: NOW - 86_400_000,
    },
    {
      id: "g-farm",
      name: "build-farm",
      repo_access: "selected",
      allow_public: false,
      is_default: false,
      repos: ["widget"],
      runners: 1,
      created_at: NOW - 3_600_000,
      updated_at: NOW - 3_600_000,
    },
  ];
}

function runners(): Runner[] {
  return [
    {
      id: "r1",
      name: "gpu-box",
      // Deliberately shuffled. The server composes these and the order
      // it composes them in is its business; the table's order is the
      // client's, and a runner whose chips reshuffle between reads
      // reads as two different machines.
      labels: ["gpu", "x64", "self-hosted", "linux"],
      os: "linux",
      arch: "x64",
      version: "0.1.0",
      ephemeral: false,
      group: { id: "g-farm", name: "build-farm" },
      state: "busy",
      last_seen_at: NOW - 5_000,
      created_at: NOW - 86_400_000,
      job: {
        run_id: "wr1",
        job_id: "build",
        key: "build (gpu)",
        repo: "widget",
      },
    },
    {
      id: "r2",
      name: "mac-mini",
      labels: ["self-hosted", "macos", "arm64"],
      os: "macos",
      arch: "arm64",
      version: "0.1.0",
      ephemeral: false,
      group: { id: "g-default", name: "default" },
      state: "online",
      last_seen_at: NOW - 20_000,
      created_at: NOW - 86_400_000,
      job: null,
    },
    {
      id: "r3",
      name: "spot-7f3a",
      labels: ["self-hosted", "linux", "x64"],
      os: "linux",
      arch: "x64",
      version: "0.1.0",
      ephemeral: true,
      group: { id: "g-default", name: "default" },
      state: "offline",
      last_seen_at: NOW - 3_600_000,
      created_at: NOW - 7_200_000,
      job: null,
    },
  ];
}

interface Refusal {
  status: number;
  error: string;
}

interface Options {
  policy?: {
    hosted: string;
    self_hosted: string;
    self_hosted_repos: string[];
  };
  groups?: Group[];
  runners?: Runner[];
  /// How long every write waits before answering. Never zero: a mock
  /// that answers before React has committed cannot show a busy state
  /// ever having been on screen.
  delay?: number;
  patchPolicy?: Refusal;
  createGroup?: Refusal;
  patchGroup?: Refusal;
  deleteGroup?: Refusal;
  removeRunner?: Refusal;
  mint?: Refusal;
}

interface Mocked {
  policyPatches: unknown[];
  groupPosts: unknown[];
  groupPatches: { id: string; body: unknown }[];
  groupDeletes: string[];
  runnerDeletes: string[];
  mints: unknown[];
}

/// The runners page, with nothing reachable but what this file mocked.
///
/// `signIn` registers the catch-all `**/v1/**` refusal first, so every
/// route below — registered after it — still wins, and anything neither
/// of us named 404s here rather than falling through vite's proxy to
/// whatever is listening on :8080.
async function runnersPage(page: Page, opts: Options = {}): Promise<Mocked> {
  const out: Mocked = {
    policyPatches: [],
    groupPosts: [],
    groupPatches: [],
    groupDeletes: [],
    runnerDeletes: [],
    mints: [],
  };
  const policy = opts.policy ?? {
    hosted: "allowed",
    self_hosted: "all",
    self_hosted_repos: [],
  };
  const gs = opts.groups ?? groups();
  const rs = opts.runners ?? runners();
  const delay = opts.delay ?? 300;
  const beat = () => new Promise((r) => setTimeout(r, delay));
  const refuse = (r: Refusal) => ({
    status: r.status,
    json: { error: r.error },
  });

  await signIn(page);

  await page.route("**/v1/orgs/acme/runner-policy", async (route) => {
    if (route.request().method() !== "PATCH") {
      return route.fulfill({ json: policy });
    }
    const body = route.request().postDataJSON();
    out.policyPatches.push(body);
    await beat();
    if (opts.patchPolicy) return route.fulfill(refuse(opts.patchPolicy));
    Object.assign(policy, body);
    return route.fulfill({ json: policy });
  });

  await page.route("**/v1/orgs/acme/runner-groups", async (route) => {
    if (route.request().method() !== "POST") {
      return route.fulfill({ json: { groups: gs } });
    }
    const body = route.request().postDataJSON() as { name: string };
    out.groupPosts.push(body);
    await beat();
    if (opts.createGroup) return route.fulfill(refuse(opts.createGroup));
    const g: Group = {
      id: `g-${gs.length + 1}`,
      name: body.name,
      repo_access: "all",
      allow_public: false,
      is_default: false,
      repos: [],
      runners: 0,
      created_at: Date.now(),
      updated_at: Date.now(),
    };
    gs.push(g);
    return route.fulfill({ status: 201, json: g });
  });

  await page.route("**/v1/orgs/acme/runner-groups/*", async (route) => {
    const id = new URL(route.request().url()).pathname.split("/").pop()!;
    const i = gs.findIndex((g) => g.id === id);
    if (route.request().method() === "DELETE") {
      out.groupDeletes.push(id);
      await beat();
      if (opts.deleteGroup) return route.fulfill(refuse(opts.deleteGroup));
      if (i >= 0) {
        // Its runners move to the default group, which is the fact the
        // confirmation promises.
        const moved = gs[i].runners;
        gs.splice(i, 1);
        const def = gs.find((g) => g.is_default);
        if (def) def.runners += moved;
      }
      return route.fulfill({ status: 204, body: "" });
    }
    const body = route.request().postDataJSON();
    out.groupPatches.push({ id, body });
    await beat();
    if (opts.patchGroup) return route.fulfill(refuse(opts.patchGroup));
    if (i >= 0) Object.assign(gs[i], body, { updated_at: Date.now() });
    return route.fulfill({ json: gs[i] });
  });

  await page.route("**/v1/orgs/acme/runners", (route) =>
    route.fulfill({ json: { runners: rs } }),
  );

  // Registered before the registration-token route so that the more
  // specific one, registered after it, wins — `runners/*` matches
  // `runners/registration-token` too.
  await page.route("**/v1/orgs/acme/runners/*", async (route) => {
    const id = new URL(route.request().url()).pathname.split("/").pop()!;
    out.runnerDeletes.push(id);
    await beat();
    if (opts.removeRunner) return route.fulfill(refuse(opts.removeRunner));
    const i = rs.findIndex((r) => r.id === id);
    if (i >= 0) rs.splice(i, 1);
    return route.fulfill({ status: 204, body: "" });
  });

  await page.route(
    "**/v1/orgs/acme/runners/registration-token",
    async (route) => {
      const body = route.request().postDataJSON();
      out.mints.push(body);
      await beat();
      if (opts.mint) return route.fulfill(refuse(opts.mint));
      const n = out.mints.length;
      return route.fulfill({
        status: 201,
        json: {
          token: `weftg_token${n}`,
          expires_at: Date.now() + 3_600_000,
          group: (body as { group?: string })?.group ?? "default",
          // The server's own composed command, which the panel is
          // expected to ignore in favour of this browser's origin.
          command:
            `weft-runner register --url https://stale.example ` +
            `--token weftg_token${n}`,
        },
      });
    },
  );

  await page.goto("/dashboard/settings/runners");
  return out;
}

test("the policy is read, edited and saved, and Saved means the server said so", async ({
  page,
}) => {
  const mock = await runnersPage(page, { delay: 600 });

  // The section is reachable by its own address and names itself.
  await expect(page.getByText("Runner policy")).toBeVisible();
  await expect(
    page.getByLabel("Weft-hosted runners", { exact: true }),
  ).toContainText("Allowed");
  await expect(
    page.getByLabel("Self-hosted runners", { exact: true }),
  ).toContainText("All repositories");

  // Nothing is claimed before anything is sent.
  await expect(page.getByText("Saved")).toHaveCount(0);

  await page.getByLabel("Weft-hosted runners", { exact: true }).click();
  await page.getByRole("option", { name: "Disabled" }).click();

  // "Selected" is the one value that needs a second answer, so the repo
  // picker only exists for it. Choosing it must not send anything by
  // itself — the whole panel is one Save.
  await page.getByLabel("Self-hosted runners", { exact: true }).click();
  await page.getByRole("option", { name: "Selected repositories" }).click();
  await page.getByRole("checkbox", { name: "widget" }).check();
  expect(mock.policyPatches).toEqual([]);

  await page.getByRole("button", { name: "Save policy" }).click();
  // The busy state is on screen *before* the answer. A mock that
  // returned instantly could never assert this, and a Save that never
  // acknowledges the press is one people press twice.
  await expect(page.getByRole("button", { name: "Saving…" })).toBeVisible();
  await expect(page.getByText("Saved")).toBeVisible();

  // All three keys, in the shape the contract names them, and the repo
  // by **name**. The object, not a JSON string: a client that stringified
  // its own body would send `"{\"hosted\":…}"` and this would be a
  // string rather than an object.
  expect(mock.policyPatches).toEqual([
    {
      hosted: "disabled",
      self_hosted: "selected",
      self_hosted_repos: ["widget"],
    },
  ]);

  // …and the tick goes the moment somebody changes their mind again. A
  // "Saved" standing beside a control that has since moved is a claim
  // about the organisation's policy that is no longer true.
  await page.getByRole("checkbox", { name: "session-1" }).check();
  await expect(page.getByText("Saved")).toHaveCount(0);
});

test("a refused policy save shows the server's own sentence and claims nothing", async ({
  page,
}) => {
  const sentence = "self_hosted must be one of all, selected, disabled";
  await runnersPage(page, {
    patchPolicy: { status: 422, error: sentence },
  });

  await page.getByRole("button", { name: "Save policy" }).click();
  await expect(page.getByText(sentence)).toBeVisible();
  // The tick is a claim about the server, so a refusal must not leave
  // one standing.
  await expect(page.getByText("Saved")).toHaveCount(0);
});

test("groups list what they admit, and a new one is created", async ({
  page,
}) => {
  const mock = await runnersPage(page);

  // The rule the two access columns encode, composed into one line
  // rather than left for the reader to assemble.
  const table = page.getByRole("table").first();
  await expect(table).toContainText(
    "Every repository · private repositories only",
  );
  await expect(table).toContainText("1 repository · private repositories only");
  // The count is what makes deleting a group a decision.
  await expect(table.getByRole("row", { name: /build-farm/ })).toContainText(
    "1",
  );
  await expect(page.getByText("the default group")).toBeVisible();

  await page.getByLabel("Group name", { exact: true }).fill("gpu-farm");
  await page.getByRole("button", { name: "Create group" }).click();
  await expect(table.getByRole("cell", { name: "gpu-farm" })).toBeVisible();
  expect(mock.groupPosts).toEqual([{ name: "gpu-farm" }]);
  // The field clears, which is the observable the next interaction
  // depends on — waiting on the mock's call count would pass before the
  // form had finished with the response.
  await expect(page.getByLabel("Group name", { exact: true })).toHaveValue("");
});

test("a duplicate group name is reported in the server's words", async ({
  page,
}) => {
  const sentence = "a runner group named build-farm already exists";
  const mock = await runnersPage(page, {
    createGroup: { status: 409, error: sentence },
  });

  await page.getByLabel("Group name", { exact: true }).fill("build-farm");
  await page.getByRole("button", { name: "Create group" }).click();
  await expect(page.getByText(sentence)).toBeVisible();
  expect(mock.groupPosts).toHaveLength(1);
  // The row list is unchanged: nothing was invented locally. Scoped to
  // the groups table — `build-farm` is also the Group column of a
  // runner two panels down, and an unscoped cell lookup would be
  // asserting about the wrong table.
  await expect(
    page.getByRole("table").first().getByRole("cell", { name: "build-farm" }),
  ).toHaveCount(1);
});

test("a group's access is edited in place, and the default group's name is not", async ({
  page,
}) => {
  const mock = await runnersPage(page);

  // The default group first: its name is fixed server-side (422), so
  // the field is disabled rather than allowed to fail on Save.
  const table = page.getByRole("table").first();
  await table
    .getByRole("row", { name: /default/ })
    .getByRole("button", { name: "Edit" })
    .click();
  await expect(page.getByLabel("Group name to edit")).toBeDisabled();
  await expect(
    page.getByText("The default group's name cannot be changed."),
  ).toBeVisible();
  await page.getByRole("button", { name: "Cancel" }).click();

  // …and an ordinary group, where every field is editable.
  await table
    .getByRole("row", { name: /build-farm/ })
    .getByRole("button", { name: "Edit" })
    .click();
  const name = page.getByLabel("Group name to edit");
  await expect(name).toBeEnabled();
  await expect(name).toHaveValue("build-farm");
  // It opened on what the group actually holds, not on a default.
  await expect(page.getByLabel("Repository access")).toContainText(
    "Selected repositories",
  );
  await expect(page.getByRole("checkbox", { name: "widget" })).toBeChecked();

  await page.getByRole("checkbox", { name: "session-1" }).check();
  await page
    .getByRole("checkbox", { name: "Allow public repositories" })
    .check();
  await name.fill("build-farm-2");
  // Nothing is claimed before the server has answered.
  await expect(
    page.getByRole("status").filter({ hasText: /^Saved / }),
  ).toHaveCount(0);
  await page.getByRole("button", { name: "Save group" }).click();

  // The editor closing is not an answer: a save that failed and closed
  // would look exactly the same. The panel says the server took it, and
  // names which group, so it cannot be read as the policy panel's tick.
  await expect(
    page.getByRole("status").filter({ hasText: "Saved build-farm-2" }),
  ).toBeVisible();
  await expect(table.getByRole("cell", { name: "build-farm-2" })).toBeVisible();
  expect(mock.groupPatches).toEqual([
    {
      id: "g-farm",
      body: {
        name: "build-farm-2",
        repo_access: "selected",
        allow_public: true,
        repos: ["widget", "session-1"],
      },
    },
  ]);
  // The line recomposes itself from what came back.
  await expect(table).toContainText(
    "2 repositories · public repositories allowed",
  );

  // …and the claim does not outlive the thing it was about: opening a
  // group again is the start of a new edit, not proof of the last one.
  await table
    .getByRole("row", { name: /build-farm-2/ })
    .getByRole("button", { name: "Edit" })
    .click();
  await expect(
    page.getByRole("status").filter({ hasText: /^Saved / }),
  ).toHaveCount(0);
});

test("deleting a group takes two deliberate steps, and the default is never offered one", async ({
  page,
}) => {
  const mock = await runnersPage(page);

  // The default group cannot be deleted — the server answers 422 — so
  // no button is drawn for it at all. Counted rather than
  // awaited-absent: a control that appears and is then refused is worse
  // than one that never appeared.
  const defaultRow = page
    .getByRole("table")
    .first()
    .getByRole("row", { name: /default/ });
  expect(await defaultRow.getByRole("button", { name: "Delete" }).count()).toBe(
    0,
  );

  // Scoped to the groups table throughout: the runners table two
  // panels down carries `build-farm` in its Group column, and a bare
  // row lookup matches both.
  const table = page.getByRole("table").first();
  const farmRow = table.getByRole("row", { name: /build-farm/ });
  await farmRow.getByRole("button", { name: "Delete", exact: true }).click();

  // Nothing has been sent. The first press arms; the second acts. No
  // `window.confirm` anywhere near it.
  expect(mock.groupDeletes).toEqual([]);
  // And the confirmation says the consequence, which is not obvious:
  // the runners are not deleted with the group.
  await expect(
    page.getByText(
      "Delete build-farm? Its 1 runner moves to the default group.",
    ),
  ).toBeVisible();

  // Backing out sends nothing.
  await page.getByRole("button", { name: "Keep it" }).click();
  expect(mock.groupDeletes).toEqual([]);
  await expect(table.getByRole("cell", { name: "build-farm" })).toBeVisible();

  await farmRow.getByRole("button", { name: "Delete", exact: true }).click();
  await page.getByRole("button", { name: "Delete the group" }).click();
  await expect(table.getByRole("cell", { name: "build-farm" })).toHaveCount(0);
  expect(mock.groupDeletes).toEqual(["g-farm"]);
  // The runners it held landed in the default group's count.
  await expect(table.getByRole("row", { name: /default/ })).toContainText("3");
});

test("the runners table says what each machine is, in words as well as tone", async ({
  page,
}) => {
  await runnersPage(page);

  const table = page.getByRole("table").nth(1);
  const gpu = table.getByRole("row", { name: /gpu-box/ });

  // The state as a **word**. Three states, three different words, so
  // the column is readable with the colour removed entirely.
  await expect(gpu).toContainText("Busy");
  await expect(table.getByRole("row", { name: /mac-mini/ })).toContainText(
    "Online",
  );
  await expect(table.getByRole("row", { name: /spot-7f3a/ })).toContainText(
    "Offline",
  );

  // The labels, in the client's order and not the server's: the fixture
  // sends them shuffled on purpose.
  const chips = await gpu
    .locator("span", { hasText: /^(self-hosted|linux|x64|gpu)$/ })
    .allTextContents();
  expect(
    chips.filter((c) => ["self-hosted", "linux", "x64", "gpu"].includes(c)),
  ).toEqual(["self-hosted", "linux", "x64", "gpu"]);

  // What it is running, and a link to the run — the next thing anybody
  // looking at a busy machine wants.
  const job = gpu.getByRole("link", { name: "build (gpu)" });
  await expect(job).toHaveAttribute("href", "/acme/widget/checks/runs/wr1");

  // The group it belongs to, and how long ago it checked in.
  await expect(gpu).toContainText("build-farm");
  await expect(table.getByRole("row", { name: /spot-7f3a/ })).toContainText(
    "ephemeral",
  );
  // A relative time with the exact instant one hover away.
  const seen = table.getByRole("row", { name: /mac-mini/ }).locator("time");
  await expect(seen).toContainText("ago");
  expect(await seen.getAttribute("title")).toBeTruthy();
});

test("removing a runner takes two steps and says what it costs", async ({
  page,
}) => {
  const mock = await runnersPage(page);

  const table = page.getByRole("table").nth(1);
  await table.getByRole("button", { name: "Remove mac-mini" }).click();
  expect(mock.runnerDeletes).toEqual([]);
  await expect(
    page.getByText(
      "Remove mac-mini? Its credential stops working and anything it is running fails.",
    ),
  ).toBeVisible();

  await page.getByRole("button", { name: "Keep it" }).click();
  expect(mock.runnerDeletes).toEqual([]);

  await table.getByRole("button", { name: "Remove mac-mini" }).click();
  await page.getByRole("button", { name: "Remove the runner" }).click();
  await expect(table.getByRole("row", { name: /mac-mini/ })).toHaveCount(0);
  expect(mock.runnerDeletes).toEqual(["r2"]);
});

test("a refused removal keeps the row and shows the server's sentence", async ({
  page,
}) => {
  const sentence = "this runner has already been removed";
  await runnersPage(page, {
    removeRunner: { status: 409, error: sentence },
  });

  const table = page.getByRole("table").nth(1);
  await table.getByRole("button", { name: "Remove mac-mini" }).click();
  await page.getByRole("button", { name: "Remove the runner" }).click();
  await expect(page.getByText(sentence)).toBeVisible();
  // The row is still there, because the server still has it.
  await expect(table.getByRole("row", { name: /mac-mini/ })).toHaveCount(1);
});

test("Add a runner shows both commands verbatim, and a second press mints a fresh token", async ({
  page,
}) => {
  const mock = await runnersPage(page);

  // Nothing is minted until somebody asks: a page that mints on load
  // hands out a credential to anybody who opens settings.
  expect(mock.mints).toEqual([]);
  await expect(page.getByLabel("Runner registration commands")).toHaveCount(0);

  await page.getByRole("button", { name: "Add a runner" }).click();
  const block = page.getByLabel("Runner registration commands");

  // Both commands, in order, and the URL is **this** origin — not the
  // `command` string the server composed, which names a host the
  // operator has no reason to be able to reach.
  await expect(block).toContainText(
    `weft-runner register --url ${PREVIEW_ORIGIN} --token weftg_token1`,
  );
  await expect(block).toContainText("weft-runner run");
  await expect(block).not.toContainText("stale.example");
  await expect(
    page.getByText("This token expires in 60 minutes."),
  ).toBeVisible();

  // A second press mints a fresh one rather than re-showing the first.
  await page.getByRole("button", { name: "Mint another token" }).click();
  await expect(block).toContainText("--token weftg_token2");
  expect(mock.mints).toHaveLength(2);

  // The group it registers into is named, because it decides which
  // repositories the machine will ever see work from.
  await page.getByLabel("Register into group").click();
  await page.getByRole("option", { name: "build-farm" }).click();
  await page.getByRole("button", { name: "Mint another token" }).click();
  expect(mock.mints.at(-1)).toEqual({ group: "build-farm" });
  await expect(page.getByText("build-farm group")).toBeVisible();
});

test("a refused mint shows the server's sentence and no commands at all", async ({
  page,
}) => {
  const sentence = "no runner group named archive exists";
  await runnersPage(page, { mint: { status: 404, error: sentence } });

  await page.getByRole("button", { name: "Add a runner" }).click();
  await expect(page.getByText(sentence)).toBeVisible();
  // A command block with no working token in it would be worse than
  // none: the operator pastes it and gets a 401 from another machine.
  await expect(page.getByLabel("Runner registration commands")).toHaveCount(0);
});
