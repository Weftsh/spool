import { describe, expect, it } from "vitest";
import type { ChangesetLanding, DiffEntry, OrgChange } from "@/api";
import {
  type MemberDiff,
  canCreate,
  filterAcross,
  memberDiffProgress,
  setProgress,
  landRefusal,
  landingSummary,
  revertKey,
  suggestKey,
  togglePick,
  unpickable,
  validKey,
  validTitle,
  afterLabels,
  edgeError,
  gateTone,
  gateWord,
  MAX_MEMBERS,
  ranks,
  refLabel,
  stateTone,
  verdictWord,
} from "./changesets";

function change(over: Partial<OrgChange> = {}): OrgChange {
  return {
    key: "c1",
    title: "one",
    target_branch: "main",
    state: "open",
    land_verdict: null,
    landed_commit: null,
    created_at: 0,
    updated_at: 0,
    patchset: null,
    repo: "api",
    changeset: null,
    viewer_write: true,
    ...over,
  };
}

describe("validKey mirrors the server's valid_change_key", () => {
  it("accepts letters, digits, dot, underscore and dash up to 72", () => {
    expect(validKey("release-2026.09_a")).toBe(true);
    expect(validKey("a".repeat(72))).toBe(true);
  });
  it("refuses empty, over-long, and anything outside the set", () => {
    expect(validKey("")).toBe(false);
    expect(validKey("a".repeat(73))).toBe(false);
    expect(validKey("has space")).toBe(false);
    expect(validKey("slash/ed")).toBe(false);
    expect(validKey("ünïcode")).toBe(false);
  });
});

describe("suggestKey", () => {
  it("folds a title into a key the server accepts", () => {
    const k = suggestKey("Rename Payments → Billing (round 2)!");
    expect(k).toBe("rename-payments-billing-round-2");
    expect(validKey(k)).toBe(true);
  });
  it("cuts to 72 without leaving a trailing separator", () => {
    const k = suggestKey(`${"word ".repeat(20)}tail`);
    expect(k.length).toBeLessThanOrEqual(72);
    expect(k.endsWith("-")).toBe(false);
    expect(validKey(k)).toBe(true);
  });
  it("is empty when nothing survives, so the form asks for one", () => {
    expect(suggestKey("日本語だけ")).toBe("");
    expect(suggestKey("---")).toBe("");
  });
});

describe("validTitle", () => {
  it("counts bytes, as the server does, not characters", () => {
    expect(validTitle("x".repeat(200))).toBe(true);
    expect(validTitle("x".repeat(201))).toBe(false);
    // 67 three-byte characters is 201 bytes.
    expect(validTitle("日".repeat(67))).toBe(false);
    expect(validTitle("   ")).toBe(false);
  });
});

describe("unpickable gives the server's refusal before the round trip", () => {
  it("offers an open change nobody holds", () => {
    expect(unpickable(change(), [])).toBeNull();
  });
  it("names the changeset already holding a change", () => {
    expect(unpickable(change({ changeset: "rel-1" }), [])).toBe(
      "already in changeset rel-1",
    );
  });
  it("says which state a non-open change is in", () => {
    expect(unpickable(change({ state: "landed" }), [])).toBe(
      "this change is landed",
    );
  });
  it("says a change the caller may not write to cannot be composed", () => {
    // The server's own answer for the row's repository; missing reads as
    // no, so a form drawn before the answer arrives offers nothing it
    // would then have to take back.
    expect(unpickable(change({ viewer_write: false }), [])).toBe(
      "composing takes write access to api",
    );
    expect(unpickable(change({ viewer_write: undefined }), [])).toBe(
      "composing takes write access to api",
    );
  });
  it("holds one change per repository, but not against itself", () => {
    const picked = [{ repo: "api", change: "c1" }];
    expect(unpickable(change({ key: "c2" }), picked)).toBe(
      "one change per repository; api is already picked",
    );
    expect(unpickable(change({ key: "c1" }), picked)).toBeNull();
    expect(unpickable(change({ key: "c9", repo: "web" }), picked)).toBeNull();
  });
});

describe("togglePick and canCreate", () => {
  it("adds, then removes, the same reference", () => {
    const a = { repo: "api", change: "c1" };
    const once = togglePick([], a);
    expect(once).toEqual([a]);
    expect(togglePick(once, a)).toEqual([]);
  });
  it("needs a valid key, a title and between 1 and 16 members", () => {
    const one = [{ repo: "api", change: "c1" }];
    expect(canCreate("k", "t", one)).toBe(true);
    expect(canCreate("", "t", one)).toBe(false);
    expect(canCreate("k", " ", one)).toBe(false);
    expect(canCreate("k", "t", [])).toBe(false);
    const many = Array.from({ length: 17 }, (_, i) => ({
      repo: `r${i}`,
      change: "c",
    }));
    expect(canCreate("k", "t", many)).toBe(false);
    expect(canCreate("k", "t", many.slice(0, 16))).toBe(true);
  });
});

describe("landingSummary", () => {
  const step = (
    repo: string,
    state: ChangesetLanding["members"][number]["state"],
  ) => ({
    repo,
    change: "c",
    ref: "refs/heads/main",
    old: null,
    new: "b".repeat(40),
    state,
    note: null,
  });
  const landing = (
    outcome: ChangesetLanding["outcome"],
    members: ChangesetLanding["members"],
  ): ChangesetLanding => ({
    id: "l1",
    attempt: 1,
    started_at: 0,
    finished_at: null,
    outcome,
    members,
  });
  it("counts while landing, so a long landing does not read as stuck", () => {
    expect(
      landingSummary(
        landing(null, [
          step("a", "done"),
          step("b", "pending"),
          step("c", "pending"),
        ]),
      ),
    ).toBe("Landing: 1 of 3 landed so far…");
  });
  it("names the count once landed", () => {
    expect(landingSummary(landing("landed", [step("a", "done")]))).toBe(
      "Landed: 1 member on their trunks.",
    );
  });
  it("names the failed member and what was put back", () => {
    expect(
      landingSummary(
        landing("failed", [
          step("a", "reverted"),
          step("b", "failed"),
          step("c", "pending"),
        ]),
      ),
    ).toBe("Failed: b/c failed; 1 landed member put back.");
  });
  it("says when the unwind left a member landed", () => {
    expect(
      landingSummary(
        landing("failed", [step("a", "done"), step("b", "failed")]),
      ),
    ).toBe("Failed: b/c failed; 1 left landed.");
  });
});

describe("landRefusal reads the 409 body", () => {
  it("keeps the server's sentence, gate and list", () => {
    expect(
      landRefusal({
        error: "waiting on api/c1: check ci / build",
        gate: "waiting",
        waiting_on: ["api/c1: ci / build", 7],
      }),
    ).toEqual({
      message: "waiting on api/c1: check ci / build",
      gate: "waiting",
      waitingOn: ["api/c1: ci / build"],
    });
  });
  it("survives a body that is not the shape it hoped for", () => {
    expect(landRefusal(undefined)).toEqual({
      message: "the changeset cannot land",
      gate: null,
      waitingOn: [],
    });
    expect(landRefusal({ gate: "sideways" }).gate).toBeNull();
  });
});

describe("revertKey", () => {
  it("prefixes and stays a valid key", () => {
    expect(revertKey("rel-1")).toBe("revert-rel-1");
    const long = revertKey("x".repeat(72));
    expect(long.length).toBe(72);
    expect(validKey(long)).toBe(true);
  });
});

describe("gateWord", () => {
  it("shows the check gate when the review is satisfied", () => {
    expect(gateWord({ landable: true, gate: "ready" })).toBe("ready");
    expect(gateWord({ landable: true, gate: "waiting" })).toBe("waiting");
    expect(gateWord({ landable: true, gate: "blocked" })).toBe("blocked");
  });

  it("calls an unapproved member blocked whatever its checks say", () => {
    // gate: ready + landable: false is the ordinary shape of a member
    // nobody has approved yet; "ready · blocked: needs an owner" is the
    // contradiction this prevents.
    expect(gateWord({ landable: false, gate: "ready" })).toBe("blocked");
    expect(gateWord({ landable: false, gate: "waiting" })).toBe("blocked");
  });
});

describe("verdictWord", () => {
  it("reads the gate while the changeset is open", () => {
    expect(verdictWord("open", { landable: true, gate: "waiting" })).toEqual({
      word: "waiting",
      tone: gateTone("waiting"),
    });
    expect(verdictWord("open", { landable: false, gate: "ready" }).word).toBe(
      "blocked",
    );
  });

  it("reads the state, not a gate, once the changeset is settled", () => {
    // The server keeps answering "not landable: changeset is landed";
    // shown as a gate that painted a clean landing red.
    expect(verdictWord("landed", { landable: false, gate: "blocked" })).toEqual(
      { word: "landed", tone: stateTone("landed") },
    );
    expect(
      verdictWord("abandoned", { landable: false, gate: "blocked" }).word,
    ).toBe("abandoned");
  });
});

// ---------------------------------------------------------------------
// The landing order

const r = (s: string) => {
  const [repo, change] = s.split("/");
  return { repo, change };
};
const e = (from: string, to: string) => ({ from: r(from), to: r(to) });
/// Waves as labels, which is what the strip renders.
const shape = (order: string[], edges: [string, string][]) =>
  ranks(
    order.map(r),
    edges.map(([a, b]) => e(a, b)),
  ).map((w) => w.map(refLabel));

describe("ranks lays the members out in waves", () => {
  it("puts everything in one wave when nothing depends on anything", () => {
    // The common case, and the one the section exists to render well:
    // no edges at all, members in the order they were added.
    expect(shape(["api/a", "web/b", "cli/c"], [])).toEqual([
      ["api/a", "web/b", "cli/c"],
    ]);
  });

  it("walks a chain one member per wave", () => {
    expect(
      shape(
        ["api/a", "web/b", "cli/c"],
        [
          ["api/a", "web/b"],
          ["web/b", "cli/c"],
        ],
      ),
    ).toEqual([["api/a"], ["web/b"], ["cli/c"]]);
  });

  it("stacks a tie in one wave, in member order", () => {
    // `web/b` and `cli/c` both land after `api/a` and neither before
    // the other: one column, two chips, and no edge drawn between them.
    expect(
      shape(
        ["api/a", "web/b", "cli/c"],
        [
          ["api/a", "web/b"],
          ["api/a", "cli/c"],
        ],
      ),
    ).toEqual([["api/a"], ["web/b", "cli/c"]]);
  });

  it("keeps two independent chains side by side", () => {
    expect(
      shape(
        ["api/a", "web/b", "cli/c", "doc/d"],
        [
          ["api/a", "web/b"],
          ["cli/c", "doc/d"],
        ],
      ),
    ).toEqual([
      ["api/a", "cli/c"],
      ["web/b", "doc/d"],
    ]);
  });

  it("ranks by the longest path, not the shortest", () => {
    // `cli/c` lands after `web/b`, which lands after `api/a`. Shortest
    // path would have put `cli/c` beside `web/b` — in a column whose
    // heading says these land together, next to something it must land
    // after.
    expect(
      shape(
        ["api/a", "web/b", "cli/c"],
        [
          ["api/a", "web/b"],
          ["api/a", "cli/c"],
          ["web/b", "cli/c"],
        ],
      ),
    ).toEqual([["api/a"], ["web/b"], ["cli/c"]]);
  });

  it("draws sixteen members, the maximum, as sixteen waves", () => {
    const order = Array.from({ length: MAX_MEMBERS }, (_, i) => `r${i}/c${i}`);
    const edges: [string, string][] = order
      .slice(1)
      .map((to, i) => [order[i], to]);
    expect(shape(order, edges)).toEqual(order.map((m) => [m]));
  });

  it("ignores an edge naming a member the changeset does not have", () => {
    // A draft mid-edit is momentarily this. The picture keeps drawing;
    // `edgeError` is what says the set cannot be saved.
    expect(shape(["api/a", "web/b"], [["api/a", "gone/x"]])).toEqual([
      ["api/a", "web/b"],
    ]);
  });

  it("appends the members a cycle leaves unplaceable rather than dropping them", () => {
    expect(
      shape(
        ["api/a", "web/b", "cli/c"],
        [
          ["api/a", "web/b"],
          ["web/b", "api/a"],
        ],
      ),
    ).toEqual([["cli/c"], ["api/a", "web/b"]]);
  });
});

describe("afterLabels says what the column cannot", () => {
  const order = ["api/a", "web/b", "cli/c", "doc/d"].map(r);

  it("says nothing for the first wave, or when the whole previous wave is the answer", () => {
    const edges = [e("api/a", "web/b"), e("api/a", "cli/c")];
    const waves = ranks(order.slice(0, 3), edges);
    expect(afterLabels(r("api/a"), edges, waves)).toEqual([]);
    expect(afterLabels(r("web/b"), edges, waves)).toEqual([]);
  });

  it("names the predecessors when the previous wave holds something else too", () => {
    // `doc/d` lands after `api/a` only, but `cli/c` is in the same wave
    // as `api/a` and nothing says `doc/d` waits for it. The column
    // alone would be a stronger claim than the edges make.
    const edges = [e("api/a", "doc/d"), e("web/b", "cli/c")];
    const waves = ranks(order, edges);
    expect(waves.map((w) => w.map(refLabel))).toEqual([
      ["api/a", "web/b"],
      ["cli/c", "doc/d"],
    ]);
    expect(afterLabels(r("doc/d"), edges, waves)).toEqual(["api/a"]);
    expect(afterLabels(r("cli/c"), edges, waves)).toEqual(["web/b"]);
  });
});

describe("edgeError mirrors the server's check_edges", () => {
  const members = ["api/a", "web/b", "cli/c"].map(r);

  it("takes a set the server takes", () => {
    expect(edgeError(members, [e("api/a", "web/b")])).toBeNull();
    expect(edgeError(members, [])).toBeNull();
  });

  it("refuses an endpoint that is not a member, in the server's words", () => {
    expect(edgeError(members, [e("api/a", "docs/I9")])).toBe(
      "docs/I9 is not a member of this changeset",
    );
    expect(edgeError(members, [e("docs/I9", "api/a")])).toBe(
      "docs/I9 is not a member of this changeset",
    );
  });

  it("refuses an edge onto itself", () => {
    expect(edgeError(members, [e("api/a", "api/a")])).toBe(
      "api/a cannot land before itself",
    );
  });

  it("refuses a cycle, naming every member left standing", () => {
    expect(
      edgeError(members, [
        e("api/a", "web/b"),
        e("web/b", "cli/c"),
        e("cli/c", "api/a"),
      ]),
    ).toBe("edges form a cycle through api/a, web/b, cli/c");
  });

  it("takes a duplicated edge, because the server deduplicates it", () => {
    // Mirroring means agreeing about what is allowed as much as about
    // what is refused: `check_edges` inserts each pair once and takes
    // the set, so refusing here would send somebody to fix a set the
    // server would have accepted.
    expect(edgeError(members, [e("api/a", "web/b"), e("api/a", "web/b")])).toBe(
      null,
    );
  });
});

// ---------------------------------------------------------------------
// The combined cross-repo diff

function file(path: string): DiffEntry {
  return { status: "modified", path, old_oid: "a", new_oid: "b" };
}

function member(over: Partial<MemberDiff> = {}): MemberDiff {
  return {
    repo: "api",
    change: "c-api",
    patchset: 2,
    files: [file("src/pay.rs")],
    viewed: new Set<string>(),
    error: null,
    ...over,
  };
}

describe("memberDiffProgress", () => {
  it("counts what this member has read, and calls the group finished", () => {
    const m = member({
      files: [file("a.rs"), file("b.rs")],
      viewed: new Set(["a.rs"]),
    });
    expect(memberDiffProgress(m)).toEqual({
      total: 2,
      seen: 1,
      complete: false,
    });
    expect(
      memberDiffProgress({ ...m, viewed: new Set(["a.rs", "b.rs"]) }).complete,
    ).toBe(true);
  });

  it("counts only paths this member has, so another member's tick cannot finish it", () => {
    // The tick set is per member and the files are per member; a stray
    // path from somewhere else must not raise the count, because the
    // count is what auto-collapses the group.
    const m = member({
      files: [file("a.rs")],
      viewed: new Set(["a.rs", "elsewhere/b.rs"]),
    });
    expect(memberDiffProgress(m)).toEqual({
      total: 1,
      seen: 1,
      complete: true,
    });
  });

  it("does not call a member with nothing to read finished", () => {
    // A group with no files is carrying a sentence — no patchset yet,
    // or a patchset that changes nothing — and collapsing it would hide
    // the one thing it had to say.
    expect(memberDiffProgress(member({ files: [] })).complete).toBe(false);
    expect(
      memberDiffProgress(member({ files: null, patchset: null })).complete,
    ).toBe(false);
  });

  it("treats a member that has not loaded as having nothing counted", () => {
    expect(memberDiffProgress(member({ files: null }))).toEqual({
      total: 0,
      seen: 0,
      complete: false,
    });
  });
});

describe("setProgress", () => {
  it("adds up the whole set", () => {
    expect(
      setProgress([
        member({
          files: [file("a.rs"), file("b.rs")],
          viewed: new Set(["a.rs"]),
        }),
        member({
          repo: "web",
          change: "c-web",
          files: [file("c.ts")],
          viewed: new Set(["c.ts"]),
        }),
      ]),
    ).toEqual({ files: 3, seen: 2 });
  });

  it("does not shrink as the reader makes progress", () => {
    // The rule `FilesPanel` already keeps, restated across members: the
    // denominator is what the filter matched, and ticking a file moves
    // the numerator and nothing else. Auto-collapse hides a finished
    // group from the rail; it must never take that group's files out of
    // the count, or the header would count down towards "0 of 0".
    const files = [file("a.rs"), file("b.rs")];
    const none = setProgress([member({ files })]);
    const all = setProgress([
      member({ files, viewed: new Set(["a.rs", "b.rs"]) }),
    ]);
    expect(none).toEqual({ files: 2, seen: 0 });
    expect(all).toEqual({ files: 2, seen: 2 });
  });

  it("is zero over no members", () => {
    expect(setProgress([])).toEqual({ files: 0, seen: 0 });
  });
});

describe("filterAcross", () => {
  const set = [
    member({
      repo: "api",
      change: "c-api",
      files: [file("README.md"), file("src/pay.rs")],
    }),
    member({ repo: "web", change: "c-web", files: [file("README.md")] }),
  ];

  it("matches on repo/path, so a repository name narrows to a repository", () => {
    const out = filterAcross(set, "api/");
    expect(out.map((m) => m.repo)).toEqual(["api"]);
    expect(out[0].files?.map((f) => f.path)).toEqual([
      "README.md",
      "src/pay.rs",
    ]);
  });

  it("matches on the path too, across every member that has it", () => {
    const out = filterAcross(set, "readme");
    expect(out.map((m) => m.repo)).toEqual(["api", "web"]);
    expect(out[0].files?.map((f) => f.path)).toEqual(["README.md"]);
  });

  it("returns the set unchanged for an empty or blank needle", () => {
    expect(filterAcross(set, "")).toBe(set);
    expect(filterAcross(set, "   ")).toBe(set);
  });

  it("drops a member nothing in matched, and never mutates the input", () => {
    expect(filterAcross(set, "pay.rs").map((m) => m.repo)).toEqual(["api"]);
    expect(set[0].files).toHaveLength(2);
  });

  it("keeps a member that has nothing loaded, whatever the needle", () => {
    // A group with no file list is saying something — "no patchset yet",
    // "loading", or "this repository could not be read" — and a filter
    // that swallowed the refusal would turn it into an absence. That is
    // the one difference between "the page is broken" and "one repo is".
    const out = filterAcross(
      [
        ...set,
        member({ repo: "secret", change: "c-x", files: null, error: "404" }),
        member({ repo: "fresh", change: "c-y", files: null, patchset: null }),
      ],
      "zzz-matches-nothing",
    );
    expect(out.map((m) => m.repo)).toEqual(["secret", "fresh"]);
  });
});
