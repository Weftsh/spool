// Review ergonomics on a change: the file rail and its filter, the
// per-file "viewed" box and what a new patchset does to it, and the
// author-association badges.
//
// The mock is stateful and implements the *server's* rule for viewed
// marks — a mark is stored against a patchset, and a later patchset that
// touched the path takes it away — because a mock that simply echoed
// back whatever was ticked would let a client that cached viewed state
// across a reload pass. The rule itself is pinned against a real server
// in `crates/stratum-server/tests/change_views_e2e.rs`; what these tests
// hold is that the page renders what the server says and asks for
// nothing it has no business choosing.

import { expect, test, type Page } from "@playwright/test";
import { ME, REPOS, signIn, signInAsPerson } from "./fixtures";

const FILES = [
  "payments/gateway.rs",
  "payments/fees/rates.rs",
  "docs/README.md",
];

interface ReviewState {
  patchset: number;
  /** path -> the patchset it was ticked at. */
  marks: Map<string, number>;
  /** Paths patchset 2 rewrote. */
  touchedByTwo: string[];
  puts: unknown[];
  /// The parsed bodies posted to `…/comments`, so a test can assert what
  /// the client chose to send rather than only what came back.
  posts: Record<string, unknown>[];
  /// Every resolve/unresolve the page asked for, in order.
  resolves: { id: string; resolved: boolean }[];
  comments: CommentFixture[];
  /// The parsed bodies posted to `…/review/submit`. A list rather than
  /// a flag, because "one request" is the feature — a review that went
  /// out twice is exactly the twelve-mails bug wearing a new shape.
  submits: Record<string, unknown>[];
  /// How many times `…/review/withdraw` and `DELETE …/review` were
  /// called.
  withdraws: number;
  discards: number;
  /// The parsed bodies posted to `…/suggestions/apply`. A list, because
  /// "one patchset however many suggestions" is the whole feature — two
  /// requests where there should have been one is the bug the route was
  /// built to make impossible, and only the count can say so.
  applies: Record<string, unknown>[];
}

/// One comment as the server shapes it since migration 0050.
///
/// The threading fields are written out in full rather than left
/// `undefined`, because a fixture that omitted them would be testing the
/// *older-server* shape while claiming to test threads — and the client
/// treats absent and empty as different things on purpose.
interface CommentFixture {
  id: string;
  patchset: number;
  author: string;
  author_email: string | null;
  author_principal: string;
  path: string | null;
  line: number | null;
  line_end: number | null;
  side: "new" | "old";
  parent_id: string | null;
  thread_id: string;
  resolved: boolean;
  resolved_at: number | null;
  resolved_by: string | null;
  body: string;
  created_at: number;
  /// Migration 0051. `pending` is the field the whole draft surface
  /// reads; it is written out in full here — false on an ordinary
  /// comment — because a fixture that left it `undefined` would be
  /// answering as a *pre-0051* server while claiming to test drafts,
  /// which is the shape of failure this repository has already shipped
  /// once with `Retry-After`.
  pending: boolean;
  review_id: string | null;
  published_at: number | null;
}

/// One standing `request_changes`, as `blocks_json` shapes it.
interface BlockFixture {
  id: string;
  verdict: string;
  body: string | null;
  author: string;
  author_email: string | null;
  user_id: string;
  patchset_id: string;
  state: string;
  submitted_at: number;
  withdrawn_at: number | null;
  created_at: number;
  /// **Not** derived from `verdict`: whether this person's "no" carries
  /// weight over a path the patchset touches is the server's answer,
  /// and a mock that computed it would let a client that computes it
  /// pass. See `stands_on_a_touched_path`.
  blocking: boolean;
}

function standingBlock(
  id: string,
  author: string,
  userId: string,
  email: string | null,
  body: string | null,
  blocking: boolean,
): BlockFixture {
  return {
    id,
    verdict: "request_changes",
    body,
    author,
    author_email: email,
    user_id: userId,
    patchset_id: "01ps1",
    state: "submitted",
    submitted_at: Date.now() - 20_000,
    withdrawn_at: null,
    created_at: Date.now() - 20_000,
    blocking,
  };
}

/// A root comment on the change as a whole.
function rootComment(
  id: string,
  author: string,
  principal: string,
  email: string | null,
  body: string,
  age: number,
): CommentFixture {
  return {
    id,
    patchset: 1,
    author,
    author_email: email,
    author_principal: principal,
    path: null,
    line: null,
    line_end: null,
    side: "new",
    parent_id: null,
    thread_id: id,
    resolved: false,
    resolved_at: null,
    resolved_by: null,
    body,
    created_at: Date.now() - age,
    pending: false,
    review_id: null,
    published_at: Date.now() - age,
  };
}

/// A comment anchored to a line, which is what a suggestion has to be:
/// the apply route replaces the lines a comment names, so a change-wide
/// remark has nothing for one to stand in place of.
///
/// `side` and `line_end` are written out in full for the same reason
/// every other fixture here writes the threading fields out: a server
/// that has them and one that does not are two different deployments a
/// client meets, and a fixture that left them `undefined` would be
/// answering as the older one while claiming to test the newer.
function lineComment(
  id: string,
  body: string,
  over: Partial<CommentFixture> = {},
): CommentFixture {
  return {
    id,
    patchset: 1,
    author: "Olive Owner",
    author_email: "olive@acme.test",
    author_principal: "user:01owner",
    path: "payments/gateway.rs",
    line: 1,
    line_end: 1,
    side: "new",
    parent_id: null,
    thread_id: id,
    resolved: false,
    resolved_at: null,
    resolved_by: null,
    body,
    created_at: Date.now() - 5_000,
    pending: false,
    review_id: null,
    published_at: Date.now() - 5_000,
    ...over,
  };
}

/// One row of `ChangeDetail.approvals`, as the server shapes it.
type ApprovalFixture = {
  email: string;
  name: string;
  patchset_id: string;
  created_at: number;
};

/// Everything the Changes tab calls for one change, with viewed marks
/// and associations behaving as the server does.
async function mockReview(
  page: Page,
  opts: {
    viewsStatus?: number;
    source?: string;
    /// A change the land queue is holding, and the note it wrote.
    landVerdict?: string;
    state?: string;
    /// What the checks route answers with. `required` is the branch's
    /// requirement list, which is deliberately **not** derived from
    /// `checks`: a required check that never reported has no row.
    checks?: unknown[];
    required?: string[];
    /// The key of the changeset holding this change, when one does.
    changeset?: string;
    /// The repository the change is on. `session-1` for the dashboard
    /// tests; the forge tests below put it on the public `widget`.
    repo?: string;
    /// Who opened it, as the associations name them.
    author?: string;
    /// Approvals standing on the latest patchset; none by default.
    approvals?: ApprovalFixture[];
    /// The computed reviewer set the verdict read carries. Defaults to
    /// an ungoverned change — nobody required, no `*` rule — which is
    /// what the other tests here are about. Pass `null` to answer as a
    /// server that predates the field.
    reviewers?: {
      required: {
        user_id: string;
        name: string;
        email: string;
        approved: boolean;
      }[];
      anyone_with_write: boolean;
    } | null;
    /// Comments the fixture does not carry by default — a thread that
    /// is already resolved, one anchored to a deleted line. Appended
    /// rather than replacing, because the three default ones are what
    /// the association tests read.
    extraComments?: CommentFixture[];
    /// Answer the conversation the way a deployment older than migration
    /// 0050 does: none of the seven threading fields on the wire at all.
    /// Implies `legacyReviews` — a server that has never heard of a
    /// thread has certainly never heard of a batched review, and a
    /// fixture that mixed the two would be describing a deployment that
    /// has never existed.
    legacyServer?: boolean;
    /// Answer as a deployment older than migration 0051: no `reviews`
    /// on the change read, no `blocks` on the verdict, and no `pending`
    /// on any comment. Absent keys, not empty ones — which is the whole
    /// distinction the capability gate turns on.
    legacyReviews?: boolean;
    /// The standing requests for changes the verdict carries.
    blocks?: BlockFixture[];
    /// What `…/suggestions/apply` answers with instead of making a
    /// patchset. A sentence and a status, because the refusals this
    /// route makes are written to be shown to the person who pressed
    /// the button — who can push here, which two comments overlap, which
    /// file has moved — and a page that dressed them up would be putting
    /// an apology in front of the answer.
    applyRefusal?: { status: number; error: string };
    /// The sufficiency verdict itself. An authoritative block makes the
    /// server answer `landable: false` with a sentence naming who asked;
    /// an advisory one leaves both alone, and the difference is the
    /// server's to make rather than the page's.
    landable?: boolean;
    explanation?: string;
  } = {},
): Promise<ReviewState> {
  const commit = (n: number) => String(n).repeat(40);
  const legacyReviews = opts.legacyReviews || opts.legacyServer;
  const s: ReviewState = {
    patchset: 1,
    marks: new Map(),
    touchedByTwo: ["payments/gateway.rs"],
    puts: [],
    posts: [],
    resolves: [],
    submits: [],
    withdraws: 0,
    discards: 0,
    applies: [],
    comments: [
      rootComment(
        "01c1",
        "Olive Owner",
        "user:01owner",
        "olive@acme.test",
        "ship it once the fee rounding is settled",
        60_000,
      ),
      rootComment(
        "01c2",
        "New Person",
        "user:01newbie",
        "new@acme.test",
        "my first review here",
        30_000,
      ),
      rootComment(
        "01c3",
        "service",
        "token:01ci",
        null,
        "the perf suite regressed 4%",
        10_000,
      ),
      ...(opts.extraComments ?? []),
    ],
  };
  const ps = () => ({
    number: s.patchset,
    commit: commit(s.patchset),
    parent: commit(9),
    message: "add gateway\n\nChange-Id: Icafe1234\n",
    created_at: Date.now() - 60_000,
  });
  const change = () => ({
    key: "Icafe1234",
    title: "add gateway",
    target_branch: "main",
    source: opts.source ?? null,
    state: opts.state ?? "open",
    land_verdict: opts.landVerdict ?? null,
    changeset: opts.changeset ?? null,
    landed_commit: null,
    created_at: Date.now() - 60_000,
    updated_at: Date.now() - 60_000,
    patchset: ps(),
  });
  const base = `**/v1/orgs/acme/repos/${opts.repo ?? "session-1"}`;

  await page.route(`${base}/changes`, (r) =>
    r.fulfill({ json: { changes: [change()] } }),
  );
  await page.route(`${base}/changes/Icafe1234`, (r) =>
    r.fulfill({
      json: {
        change: change(),
        patchsets: [ps()],
        approvals: opts.approvals ?? [],
        // The capability signal. `undefined` is dropped by
        // JSON.stringify, so the legacy case really is a body with no
        // `reviews` key rather than one with an empty array — and those
        // are the two answers the gate has to tell apart.
        reviews: legacyReviews ? undefined : [],
      },
    }),
  );
  await page.route(`${base}/changes/Icafe1234/verdict`, (r) =>
    r.fulfill({
      json: {
        change: "Icafe1234",
        state: "open",
        patchset: s.patchset,
        commit: commit(s.patchset),
        verdict: {
          landable: opts.landable ?? true,
          explanation: opts.explanation ?? "ok: all 3 changed path(s) approved",
          per_path: [],
        },
        blocks: legacyReviews ? undefined : (opts.blocks ?? []),
        // `undefined` is dropped by JSON.stringify, so passing null
        // here really does produce a body with no `reviewers` key —
        // the older-server case, not a null-valued one.
        reviewers:
          opts.reviewers === null
            ? undefined
            : (opts.reviewers ?? { required: [], anyone_with_write: false }),
      },
    }),
  );
  await page.route(`${base}/changes/Icafe1234/comments`, (r) => {
    if (r.request().method() !== "POST") {
      // A server that predates threads sends the six fields it has and
      // no more — not nulls, which are a different claim. `undefined`
      // is dropped by JSON.stringify, so this really is a body with no
      // `thread_id` key rather than one with a null there.
      const threadFields = opts.legacyServer
        ? s.comments.map((c) => ({
            ...c,
            parent_id: undefined,
            thread_id: undefined,
            side: undefined,
            line_end: undefined,
            resolved: undefined,
            resolved_at: undefined,
            resolved_by: undefined,
          }))
        : s.comments;
      // A pre-0051 server sends no `pending` either. Stripped in a
      // second pass rather than folded into the one above, because the
      // two migrations are two different deployments a client meets —
      // 0050-but-not-0051 is a real one, and a fixture that could only
      // produce "both or neither" could not answer for it.
      const wire = legacyReviews
        ? threadFields.map((c) => ({
            ...c,
            pending: undefined,
            review_id: undefined,
            published_at: undefined,
          }))
        : threadFields;
      return r.fulfill({ json: { comments: wire } });
    }
    const body = r.request().postDataJSON() as Record<string, unknown>;
    s.posts.push(body);
    const parent = (body.parent_id as string | undefined) ?? null;
    // The server's rule, not an echo: a reply **inherits** its root's
    // anchor whole and cannot set one of its own. A mock that simply
    // stored what the client sent would let a client that anchors its
    // replies pass, and the anchor is the one field a reply must not
    // choose — see `add_comment` in `crates/stratum-control`.
    const root = parent ? s.comments.find((c) => c.id === parent) : undefined;
    const id = `01n${s.comments.length}`;
    // The server's rule again, not an echo of a flag: a drafted comment
    // is unpublished, belongs to the caller's review — which the server
    // opens for them, so no `POST …/review` is ever required of the
    // client — and is returned to nobody else. A mock that stored
    // `pending` and left `published_at` set would let a client that
    // reads the wrong field pass.
    const pending = body.pending === true;
    const made: CommentFixture = {
      id,
      patchset: s.patchset,
      author: ME.name,
      author_email: ME.email,
      author_principal: `user:${ME.id}`,
      path: root ? root.path : ((body.path as string | undefined) ?? null),
      line: root ? root.line : ((body.line as number | undefined) ?? null),
      line_end: root
        ? root.line_end
        : ((body.line_end as number | undefined) ?? null),
      side: root ? root.side : ((body.side as "new" | "old") ?? "new"),
      parent_id: parent,
      thread_id: parent ?? id,
      resolved: false,
      resolved_at: null,
      resolved_by: null,
      body: body.body as string,
      created_at: Date.now(),
      pending,
      review_id: pending ? "01review" : null,
      published_at: pending ? null : Date.now(),
    };
    s.comments.push(made);
    return r.fulfill({ status: 201, json: made });
  });
  // The three review routes. Registered before the `…/comments` glob
  // above cannot match them — Playwright's `*` does not cross a `/` —
  // so they are plain exact routes.
  await page.route(`${base}/changes/Icafe1234/review/submit`, (r) => {
    const body = r.request().postDataJSON() as Record<string, unknown>;
    s.submits.push(body);
    // The one statement that turns a private pass into a public one.
    // Every draft becomes visible at the same instant, which is what
    // makes a review one act — so the mock does it in one pass rather
    // than row by row.
    const published = s.comments.filter((c) => c.pending);
    for (const c of published) {
      c.pending = false;
      c.review_id = null;
      c.published_at = Date.now();
    }
    return r.fulfill({
      json: {
        review: {
          id: "01review",
          verdict: body.verdict,
          body: body.body ?? null,
          author: ME.name,
          author_email: ME.email,
          user_id: ME.id,
          patchset_id: "01ps1",
          state: "submitted",
          submitted_at: Date.now(),
          withdrawn_at: null,
          created_at: Date.now(),
        },
        published: published.length,
        approved: body.verdict === "approve",
        approval_revoked: body.verdict === "request_changes",
      },
    });
  });
  await page.route(`${base}/changes/Icafe1234/suggestions/apply`, (r) => {
    const body = r.request().postDataJSON() as Record<string, unknown>;
    s.applies.push(body);
    if (opts.applyRefusal) {
      return r.fulfill({
        status: opts.applyRefusal.status,
        json: { error: opts.applyRefusal.error },
      });
    }
    // The server's rule, not an echo: **one** patchset however many
    // comments were named. A mock that made one per id would let a
    // client that clicks Apply five times pass, which is precisely the
    // thing this route exists to prevent.
    s.patchset += 1;
    return r.fulfill({
      status: 201,
      json: {
        change: change(),
        patchset: ps(),
        applied: body.comments,
        paths: ["payments/gateway.rs"],
      },
    });
  });
  await page.route(`${base}/changes/Icafe1234/review/withdraw`, (r) => {
    s.withdraws += 1;
    return r.fulfill({ status: 204, body: "" });
  });
  await page.route(`${base}/changes/Icafe1234/review`, (r) => {
    if (r.request().method() !== "DELETE") {
      return r.fulfill({ json: { review: null, comments: [] } });
    }
    s.discards += 1;
    // Drafted comments and all. Nothing anybody else ever saw is lost,
    // which is the entire point of a draft.
    s.comments = s.comments.filter((c) => !c.pending);
    return r.fulfill({ status: 204, body: "" });
  });
  // Both routes at once: they are one decision with a boolean in it, and
  // the client reaches them through one method for the same reason.
  await page.route(
    /\/changes\/Icafe1234\/comments\/([^/]+)\/(un)?resolve$/,
    (r) => {
      const m = /comments\/([^/]+)\/(un)?resolve$/.exec(r.request().url());
      const [, id, un] = m as RegExpExecArray;
      const resolved = un === undefined;
      const c = s.comments.find((x) => x.id === id);
      if (!c)
        return r.fulfill({ status: 404, json: { error: "no such comment" } });
      s.resolves.push({ id, resolved });
      c.resolved = resolved;
      c.resolved_at = resolved ? Date.now() : null;
      // The resolver's *name*, which is what the server sends and the
      // only thing the collapsed summary can say.
      c.resolved_by = resolved ? ME.name : null;
      return r.fulfill({ status: 200, json: c });
    },
  );
  await page.route(`${base}/changes/Icafe1234/checks`, (r) =>
    r.fulfill({
      json: {
        patchset: s.patchset,
        checks: opts.checks ?? [],
        required_checks: opts.required ?? [],
      },
    }),
  );
  await page.route(`${base}/changes/Icafe1234/associations`, (r) =>
    r.fulfill({
      json: {
        author: "contributor",
        author_principal: opts.author ?? "user:01newbie",
        authors: {
          "user:01owner": "owner",
          "user:01newbie": "first-time",
          [opts.author ?? "user:01newbie"]: "first-time",
        },
      },
    }),
  );
  await page.route(`${base}/changes/Icafe1234/views`, (r) => {
    if (opts.viewsStatus) {
      return r.fulfill({
        status: opts.viewsStatus,
        json: {
          error: "viewed state belongs to a person, not a service token",
        },
      });
    }
    if (r.request().method() === "PUT") {
      const body = r.request().postDataJSON() as {
        path: string;
        viewed: boolean;
      };
      s.puts.push(body);
      if (body.viewed) s.marks.set(body.path, s.patchset);
      else s.marks.delete(body.path);
      return r.fulfill({ status: 204, body: "" });
    }
    // The server's rule: a mark made at an older patchset survives only
    // for a path that patchset did not touch.
    const viewed = [...s.marks.entries()]
      .filter(
        ([path, at]) =>
          at === s.patchset ||
          !(s.patchset === 2 && s.touchedByTwo.includes(path)),
      )
      .map(([path]) => path)
      .sort();
    return r.fulfill({ json: { patchset: s.patchset, viewed } });
  });
  await page.route(`${base}/diff?*`, (r) =>
    r.fulfill({
      json: {
        from: commit(9),
        to: commit(s.patchset),
        changes: FILES.map((path) => ({
          status: "modified",
          path,
          old_oid: "1".repeat(40),
          new_oid: "2".repeat(40),
          old_mode: "100644",
          new_mode: "100644",
        })),
      },
    }),
  );
  await page.route(`${base}/files/**`, (r) => {
    const at = new URL(r.request().url()).searchParams.get("at");
    return r.fulfill({
      status: 200,
      contentType: "application/octet-stream",
      body: at === commit(9) ? "let fee = old();\n" : "let fee = new();\n",
    });
  });
  return s;
}

/// One change, open, ready to review — reached the way a person reaches
/// it, from the dashboard's repository list.
///
/// Clicked through rather than `goto`'d, and the distinction is not
/// cosmetic. A `goto` is a full page load, which re-boots the SPA; a
/// password session survives that only if `/v1/auth/me` answers, and
/// `signInAsPerson` mocks it to 401 for the *pre*-login probe and never
/// updates it. So a `goto` here silently signs the viewer out, and the
/// page renders "Sign in to watch" beside a change it is supposed to be
/// reviewing — which reads as the review surface being broken.
///
/// Clicking also exercises the two links this navigation now depends on:
/// the repository row (an anchor to `/{owner}/{repo}`) and the Changes
/// tab (`components/tab-strip.tsx` — "a tab is a URL somebody can send").
async function openChange(page: Page) {
  await page.getByRole("link", { name: "session-1", exact: true }).click();
  await page.waitForURL("**/acme/session-1");
  await page
    .getByRole("navigation", { name: "Repository" })
    .getByRole("link", { name: "Changes", exact: true })
    .click();
  await page.getByText("add gateway").click();
  await expect(page.getByText("Files in this patchset")).toBeVisible();
}

test("the rail nests files under their directories and the filter narrows both halves", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page);
  await openChange(page);

  const rail = page.getByRole("navigation", { name: "Files in this patchset" });
  // A tree, not a second copy of the flat list: directories and files
  // are both rows, each named by its own segment, and a directory says
  // whether it is open. Deep paths sharing a prefix read as one branch
  // rather than as three lines starting with the same word.
  const row = (name: string) =>
    rail.getByRole("treeitem", { name, exact: true });
  await expect(row("payments")).toHaveAttribute("aria-expanded", "true");
  await expect(row("fees")).toHaveAttribute("aria-expanded", "true");
  await expect(row("gateway.rs")).toBeVisible();

  // The filter is a path fragment, and it narrows the rail and the file
  // list together — a rail showing files the list has hidden would send
  // a reviewer clicking into nothing.
  await page.getByLabel("Filter files by path").fill("fees");
  await expect(row("rates.rs")).toBeVisible();
  await expect(row("gateway.rs")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: /payments\/gateway\.rs/ }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: /payments\/fees\/rates\.rs/ }),
  ).toBeVisible();

  // A filter that matches nothing says so rather than showing an empty
  // panel that reads as a broken page.
  await page.getByLabel("Filter files by path").fill("nothing-here");
  await expect(
    page.getByText(/No file in this patchset matches/),
  ).toBeVisible();

  // Clearing it brings everything back, and clicking a rail row opens
  // that file's diff in place and marks the row as the one that is open.
  await page.getByLabel("Filter files by path").fill("");
  await row("rates.rs").click();
  await expect(page.getByText("let fee = new();")).toBeVisible();
  await expect(row("rates.rs")).toHaveAttribute("aria-selected", "true");
});

test("ticking a file records it, and the client never chooses the patchset", async ({
  page,
}) => {
  await signIn(page);
  const s = await mockReview(page);
  await openChange(page);

  await expect(page.getByText("0 of 3 viewed")).toBeVisible();
  await page.getByLabel("Viewed payments/gateway.rs").check();
  await expect(page.getByText("1 of 3 viewed")).toBeVisible();
  await expect
    .poll(() => s.puts)
    .toEqual([{ path: "payments/gateway.rs", viewed: true }]);
  // No patchset in the body: which revision a tick applies to is the
  // server's to decide. A client that sent one could tick a file
  // against a revision nobody is looking at.
  expect(Object.keys(s.puts[0] as object).sort()).toEqual(["path", "viewed"]);

  // Unticking is the same request in reverse, and the count follows.
  await page.getByLabel("Viewed payments/gateway.rs").uncheck();
  await expect(page.getByText("0 of 3 viewed")).toBeVisible();
  await expect.poll(() => s.puts.length).toBe(2);
  expect(s.puts[1]).toEqual({ path: "payments/gateway.rs", viewed: false });
});

test("a new patchset unticks the files it changed, and only those", async ({
  page,
}) => {
  await signIn(page);
  const s = await mockReview(page);
  await openChange(page);

  await page.getByLabel("Viewed payments/gateway.rs").check();
  await page.getByLabel("Viewed docs/README.md").check();
  await expect(page.getByText("2 of 3 viewed")).toBeVisible();

  // Patchset 2 lands, rewriting gateway.rs and nothing else. Re-open the
  // change the way a reviewer coming back to it would.
  s.patchset = 2;
  await page.getByRole("button", { name: "← Changes" }).click();
  await page.getByText("add gateway").click();
  await expect(page.getByText("Files in this patchset")).toBeVisible();

  // Wait on the *positive* half first, and only then read the negative
  // one without retries.
  //
  // Order matters here in a way that is easy to get wrong. Every box
  // renders unchecked before the views response lands, so a retrying
  // `not.toBeChecked()` on gateway.rs passes in that transient window —
  // against a client that goes on to restore the tick wrongly, which is
  // the exact bug this test exists for. README being checked can only be
  // true once the server's answer has been applied, so it is the
  // observable to wait on; after it, a single non-retrying `isChecked()`
  // reads the settled state and cannot pass by being early.
  await expect(page.getByLabel("Viewed docs/README.md")).toBeChecked();
  expect(await page.getByLabel("Viewed payments/gateway.rs").isChecked()).toBe(
    false,
  );
  await expect(page.getByText("1 of 3 viewed")).toBeVisible();
});

test("author association badges say how to weigh each voice", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page);
  await openChange(page);

  // On the header, for the change's own author.
  await expect(
    page.getByTitle("Not a member, but has landed a change here before"),
  ).toBeVisible();
  // And on each comment.
  await expect(page.getByText("Owner", { exact: true })).toBeVisible();
  await expect(page.getByText("First-time", { exact: true })).toBeVisible();
  // A service principal has no standing in the org, and gets no badge
  // rather than an invented one.
  const robot = page
    .locator("li")
    .filter({ hasText: "the perf suite regressed 4%" });
  await expect(robot).toBeVisible();
  await expect(robot.getByText("Member", { exact: true })).toHaveCount(0);
  await expect(robot.getByText("Owner", { exact: true })).toHaveCount(0);
});

test("a refused viewed-state read leaves the rest of the review standing", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page, { viewsStatus: 403 });
  await openChange(page);

  // The diff, the verdict and the conversation are all there; the boxes
  // are simply unticked. A garnish that took the review down with it
  // would be worse than no garnish.
  await expect(page.getByText("0 of 3 viewed")).toBeVisible();
  await expect(page.getByText("Landable")).toBeVisible();
  await expect(
    page.getByText("ship it once the fee rounding is settled"),
  ).toBeVisible();
  await expect(page.getByRole("alert")).toHaveCount(0);
});

test("ticking a file folds it away, and hiding the viewed ones shortens the list", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page);
  await openChange(page);

  // Read the file, then tick it: the diff folds away, because what is
  // left on screen should be what is left to read.
  await page.getByRole("button", { name: /payments\/gateway\.rs/ }).click();
  await expect(page.getByText("let fee = new();")).toBeVisible();
  await page.getByLabel("Viewed payments/gateway.rs").check();
  await expect(page.getByText("let fee = new();")).toHaveCount(0);

  // Unticking is "let me look again", so the diff comes straight back.
  await page.getByLabel("Viewed payments/gateway.rs").uncheck();
  await expect(page.getByText("let fee = new();")).toBeVisible();

  // With something viewed, the list can be narrowed to what is left.
  await page.getByLabel("Viewed payments/gateway.rs").check();
  await page.getByLabel("Hide viewed").check();
  await expect(
    page.getByRole("button", { name: /payments\/gateway\.rs/ }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: /docs\/README\.md/ }),
  ).toBeVisible();
  // And the count still measures the whole change, not the remainder —
  // a progress number that shrank as you made progress would be worse
  // than none at all.
  await expect(page.getByText("1 of 3 viewed")).toBeVisible();
});

test("the change's own author is marked as such wherever they speak", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page);
  await openChange(page);

  // The newcomer opened this change, so their comment carries both
  // where they stand and that this is their work.
  const own = page.locator("li").filter({ hasText: "my first review here" });
  await expect(own.getByText("First-time", { exact: true })).toBeVisible();
  await expect(own.getByText("Author", { exact: true })).toBeVisible();
  // Somebody else reviewing it is not the author, and must not read as
  // one.
  const other = page
    .locator("li")
    .filter({ hasText: "ship it once the fee rounding is settled" });
  await expect(other.getByText("Owner", { exact: true })).toBeVisible();
  await expect(other.getByText("Author", { exact: true })).toHaveCount(0);
});

test("a change from a fork says so, and an ordinary one says nothing", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page, { source: "bob/session-1" });
  await openChange(page);

  // Code that arrived from outside this repository is the first thing a
  // reviewer needs to know, and it is what makes the association badge
  // beside the title worth reading.
  await expect(page.getByText("Proposed from")).toBeVisible();
  await expect(page.getByText("bob/session-1")).toBeVisible();
});

test("a change with no fork behind it grows no extra line", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page);
  await openChange(page);

  // The ordinary case is somebody with push access working here, and a
  // sentence saying "proposed from nothing" would be noise on every
  // change in the enterprise flow. Counted, not retried: this asserts an
  // absence in a settled state, and `openChange` has already waited for
  // the panel.
  expect(await page.getByText("Proposed from").count()).toBe(0);
});

/// A required check that has never reported, before anybody presses Land.
///
/// This is the moment the defect did the damage. `main` required two
/// checks, one reported green, the other had never reported — and because
/// a check with no row cannot carry a `required` flag, the page counted
/// one green row, announced "All checks have passed", and left the Land
/// button enabled with nothing under it. The reader pressed it and the
/// change entered a queue it could not leave for thirty minutes.
///
/// Two leaks, and fixing one alone would leave the page contradicting
/// itself: the panel headline counted only the rows that existed, and the
/// blockers list under the button was called without the requirement
/// list, so its missing-required branch was dead in the product while
/// passing its own unit test.
test("a required check that never reported blocks the button and says so", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page, {
    required: ["ci/local", "ci/tests"],
    checks: [
      {
        name: "ci/local",
        state: "passing",
        url: null,
        required: true,
        source: "commit",
        posted_by: "intake",
        updated_at: Date.now() - 20_000,
      },
    ],
  });
  await openChange(page);

  // The headline must not claim a pass. This is the assertion that would
  // have failed against the shipped page.
  await expect(page.getByText("All checks have passed")).toHaveCount(0);
  await expect(
    page.getByText("Some checks haven't completed yet"),
  ).toBeVisible();
  await expect(page.getByText("1 successful, 1 not reported")).toBeVisible();

  // And the name, in the words that send a reader to their CI config
  // rather than to a log they will not find.
  await expect(
    page.getByText("1 required check has not reported: ci/tests"),
  ).toBeVisible();
});

/// The same change once it is in the queue: the queue's own note.
///
/// `land_verdict` said "waiting on ci/tests" in the very payload this
/// page fetches, and the changes list one click back rendered it — while
/// this page filtered it out unless it began "ejected". So a reader
/// clicked a row saying "waiting on ci/tests" and arrived at a page
/// showing a spinner and a green checks panel.
test("a change held by the land queue names the check it is waiting for", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page, {
    state: "landing",
    landVerdict: "waiting on ci/tests",
    required: ["ci/local", "ci/tests"],
    checks: [
      {
        name: "ci/local",
        state: "passing",
        url: null,
        required: true,
        source: "commit",
        posted_by: "intake",
        updated_at: Date.now() - 20_000,
      },
    ],
  });
  await openChange(page);

  await expect(
    page.getByText(
      "The queue is holding this change for a check that has not reported",
    ),
  ).toBeVisible();
  await expect(page.getByText("ci/tests", { exact: true })).toBeVisible();
  // The bound, said out loud: without it a name that will never report
  // looks exactly like a slow build, forever.
  await expect(page.getByText(/the queue gives up and ejects/)).toBeVisible();
  await expect(page.getByText("All checks have passed")).toHaveCount(0);
});

/// The other half of the same rule: nothing invented when every required
/// name did report. A gate that cried wolf would be as bad as a silent
/// one — worse, because people learn to ignore it.
test("a change whose required checks all passed says so plainly", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page, {
    required: ["ci/local"],
    checks: [
      {
        name: "ci/local",
        state: "passing",
        url: null,
        required: true,
        source: "commit",
        posted_by: "intake",
        updated_at: Date.now() - 20_000,
      },
    ],
  });
  await openChange(page);

  await expect(page.getByText("All checks have passed")).toBeVisible();
  await expect(page.getByText("not reported")).toHaveCount(0);
  await expect(page.getByText("The queue is holding this change")).toHaveCount(
    0,
  );
});

/// A change that is a member of an open changeset lands only through it.
///
/// Before the change carried its changeset, this page offered "Land on
/// main" like any other and learned about the binding from the server's
/// 409 — after the press. The key is on the change now; the page turns the
/// solo actions off, says which set holds it, and the link goes there.
// "a change held by a changeset says so and lands only through it" used
// to live here, reaching the change through the dashboard's own repo
// screen and asserting the link landed on `/dashboard/changesets/…`.
// That mount is gone — a repository has one page now — and the test
// below already covers the surviving behaviour strictly better: it pins
// the `href`, and it proves the navigation is *client-side*, which the
// dashboard version could not have caught. "Lands with changeset", the
// disabled Land and the disabled Abandon are asserted there too.

test("from the forge, a held change reaches its changeset without leaving the forge", async ({
  page,
}) => {
  // The same panel is mounted at `/{owner}/{repo}/changes/{key}`. This
  // link used to be a deliberate full load to `/dashboard/changesets/…`,
  // and the comment beside it said why: the forge mount had no
  // changesets route, so `navigate` there would have gone to a blank
  // page. A changeset now has a public forge address of its own, so that
  // reason has expired — and the reader is one client-side move from it
  // rather than a whole SPA reload out of the shell they are standing in.
  //
  // The old test asserted the dashboard href and the resulting URL, and
  // *could not have caught* the thing that actually matters here: a
  // plain anchor to the forge address would satisfy both while still
  // reloading the document. So this one asserts the navigation is
  // client-side, which is the claim.
  await signIn(page);
  await mockReview(page, { changeset: "rename-payments" });
  await page.route("**/v1/orgs/acme/changesets/rename-payments", (route) =>
    route.fulfill({ status: 404, json: { error: "not in this mock" } }),
  );
  await page.goto("/acme/session-1/changes/Icafe1234");

  const link = page.getByRole("main").getByRole("link", {
    name: "rename-payments",
  });
  // A real `href` all the same, and the forge's: the link has to be
  // copyable, openable in a new tab and readable by anything that
  // scrapes links, which `preventDefault` alone does not give you.
  await expect(link).toHaveAttribute(
    "href",
    "/acme/changesets/rename-payments",
  );

  // A sentinel on `window`, because "did the document reload" is not
  // otherwise observable from here: a full load throws the whole
  // JavaScript context away, and this flag with it.
  await page.evaluate(() => {
    (window as unknown as { stayed?: boolean }).stayed = true;
  });
  await link.click();

  await expect(page).toHaveURL(/\/acme\/changesets\/rename-payments$/);
  expect(
    await page.evaluate(
      () => (window as unknown as { stayed?: boolean }).stayed === true,
    ),
  ).toBe(true);
});

// ---------------------------------------------------------------------
// Who is offered which action.
//
// The Actions card drew the same four buttons for everyone who could
// read the change. On a public repository that is everyone: a stranger
// with no account was offered Land, Approve and Abandon, and each of
// them answered "sign in" or "not found" only after being pressed. The
// server's doors are: anyone signed in may approve; landing takes write
// access; abandoning takes write access **or** being the author. The
// card now follows the same three, so nobody is offered a door that is
// shut — and the author of a change proposed from a fork, who can never
// write here, is the person Abandon is most for.

/// A change on the public `widget`, read from the forge by `who`.
async function onForgeChange(
  page: Page,
  who: {
    me: typeof ME | null;
    write: boolean;
    author?: string;
    approvals?: ApprovalFixture[];
    reviewers?: Parameters<typeof mockReview>[1]["reviewers"];
    extraComments?: CommentFixture[];
    /// Answer as a deployment older than migration 0050.
    legacy?: boolean;
    /// Answer as a deployment older than migration 0051.
    legacyReviews?: boolean;
    blocks?: BlockFixture[];
    landable?: boolean;
    explanation?: string;
  },
): Promise<ReviewState> {
  await page.route("**/v1/**", (r) =>
    r.fulfill({ status: 404, json: { error: "not mocked by this test" } }),
  );
  await page.route("**/v1/auth/me", (r) =>
    who.me
      ? r.fulfill({ status: 200, json: who.me })
      : r.fulfill({ status: 401, json: { error: "not signed in" } }),
  );
  await page.route("**/v1/orgs/acme/repos/widget", (r) =>
    r.fulfill({
      status: 200,
      json: {
        ...REPOS.repos[0],
        org: "acme",
        public: true,
        // `native`, and not incidentally: `REPOS.repos[0]` is a mirror,
        // and a mirror can never have a change. `changes_api::create`
        // refuses one at the door — "a mirror's trunk belongs to its
        // origin" — so every row this file reviews is a row the server
        // would not have produced. That went unnoticed while the
        // repository page offered a Changes tab to everything; it does
        // not any more, so the fixture has to describe a repository the
        // change it carries could actually exist in.
        kind: "native",
        viewer_write: who.write,
      },
    }),
  );
  const s = await mockReview(page, {
    repo: "widget",
    author: who.author,
    approvals: who.approvals,
    reviewers: who.reviewers,
    extraComments: who.extraComments,
    legacyServer: who.legacy,
    legacyReviews: who.legacyReviews,
    blocks: who.blocks,
    landable: who.landable,
    explanation: who.explanation,
  });
  await page.goto("/acme/widget/changes/Icafe1234");
  await expect(page.getByText("Files in this patchset")).toBeVisible();
  return s;
}

const actionButtons = (page: Page) => ({
  land: page.getByRole("button", { name: /^Land on main/ }),
  approve: page.getByRole("button", { name: /^Approve patchset/ }),
  // The door Approve moved behind on a 0051 server: one sheet, three
  // verdicts. Both are listed so the signed-out sweep below can assert
  // that neither is offered.
  review: page.getByRole("button", { name: /^Review patchset/ }),
  revoke: page.getByRole("button", { name: "Revoke my approval" }),
  abandon: page.getByRole("button", { name: "Abandon" }),
  comment: page.getByRole("button", { name: "Comment" }),
});

test("a stranger reads the whole review and is offered the way in, not buttons", async ({
  page,
}) => {
  await onForgeChange(page, { me: null, write: false });
  const b = actionButtons(page);
  for (const [name, button] of Object.entries(b)) {
    await expect(
      button,
      `${name} is offered to somebody signed out`,
    ).toHaveCount(0);
  }
  await expect(page.getByLabel("Comment on this change")).toHaveCount(0);
  // The review itself is not withheld: the verdict and the conversation
  // are public reading on a public repository.
  await expect(page.getByText("Landable")).toBeVisible();
  await expect(page.getByText("my first review here")).toBeVisible();
  // Two invitations, one per thing they cannot do — and each is a link
  // that comes back here, not a bare word. Scoped to `main`: the
  // masthead carries a "Sign in" of its own.
  const signIn = page.getByRole("main").getByRole("link", { name: "Sign in" });
  await expect(signIn).toHaveCount(2);
  for (const link of await signIn.all()) {
    await expect(link).toHaveAttribute(
      "href",
      /\/login\?next=%2Facme%2Fwidget%2Fchanges%2FIcafe1234$/,
    );
  }
});

test("a signed-in reader may approve and comment, and is told what landing takes", async ({
  page,
}) => {
  await onForgeChange(page, { me: ME, write: false });
  const b = actionButtons(page);
  // Approving is a verdict inside the review sheet on a 0051 server, so
  // this is the control that offers it. The standalone button is not a
  // second door to the same act — see the older-server test below,
  // which is the only place it still exists.
  await expect(b.review).toBeVisible();
  await expect(b.approve).toHaveCount(0);
  // They have approved nothing yet, so there is nothing to revoke: the
  // button is there, so the shape of the card does not jump when they
  // approve, but it is not live. Enabled, it answered 404 "no active
  // approval to revoke" — a refusal the page had given no warning of.
  await expect(b.revoke).toBeVisible();
  await expect(b.revoke).toBeDisabled();
  await expect(b.comment).toBeVisible();
  await expect(b.land).toHaveCount(0);
  await expect(
    page.getByText(/takes write access to this repository/),
  ).toBeVisible();
  // Not their change, so not theirs to withdraw.
  await expect(b.abandon).toHaveCount(0);
});

test("revoking is live only for somebody whose approval is standing", async ({
  page,
}) => {
  await onForgeChange(page, {
    me: ME,
    write: false,
    approvals: [
      {
        email: ME.email,
        name: ME.name,
        patchset_id: "01ps1",
        created_at: Date.now() - 1_000,
      },
    ],
  });
  let deleted = false;
  await page.route(
    "**/v1/orgs/acme/repos/widget/changes/Icafe1234/approve",
    (r) => {
      deleted = r.request().method() === "DELETE";
      return r.fulfill({ status: 204, body: "" });
    },
  );
  const b = actionButtons(page);
  await expect(b.revoke).toBeEnabled();
  await b.revoke.click();
  await expect
    .poll(() => deleted, { message: "Revoke never sent DELETE" })
    .toBe(true);
});

test("somebody else's approval does not make the revoke button mine", async ({
  page,
}) => {
  // The card lists every approval on the latest patchset; only the
  // viewer's own is theirs to withdraw.
  await onForgeChange(page, {
    me: ME,
    write: true,
    approvals: [
      {
        email: "olive@acme.test",
        name: "Olive Owner",
        patchset_id: "01ps1",
        created_at: Date.now() - 1_000,
      },
    ],
  });
  await expect(page.getByText("olive@acme.test").first()).toBeVisible();
  await expect(actionButtons(page).revoke).toBeDisabled();
});

test("the author of a change may abandon it without write access", async ({
  page,
}) => {
  await onForgeChange(page, {
    me: ME,
    write: false,
    author: `user:${ME.id}`,
  });
  let posted = false;
  await page.route(
    "**/v1/orgs/acme/repos/widget/changes/Icafe1234/abandon",
    (r) => {
      posted = r.request().method() === "POST";
      return r.fulfill({ status: 204, body: "" });
    },
  );
  const b = actionButtons(page);
  await expect(b.land).toHaveCount(0);
  await expect(b.abandon).toBeVisible();
  await b.abandon.click();
  await expect
    .poll(() => posted, { message: "Abandon never posted" })
    .toBe(true);
});

test("a writer keeps every action", async ({ page }) => {
  await onForgeChange(page, { me: ME, write: true });
  const { approve, ...b } = actionButtons(page);
  for (const [name, button] of Object.entries(b)) {
    await expect(button, `${name} is missing for a writer`).toBeVisible();
  }
  // Every action except the standalone Approve, which is not a missing
  // one: on a 0051 server approving is a verdict inside the review
  // sheet, and two doors to one act would eventually disagree about
  // what the second does to a reviewer's drafts.
  await expect(approve, "the standalone Approve came back").toHaveCount(0);
  await expect(
    page.getByText(/takes write access to this repository/),
  ).toHaveCount(0);
});

// ---------------------------------------------------------------------
// The computed reviewer set.
//
// The rule itself — who OWNERS requires, and that a `*` entry requires
// nobody in particular — is pinned against a real server in
// `crates/stratum-server/tests/changes_e2e.rs`. What these hold is that
// the page renders the server's answer, tells the two empty answers
// apart, says so when the reader is themselves on the list, and offers
// no way to add anybody to it.

/// The people OWNERS names for the fixture change. `01user` is `ME`, so
/// the viewer is one of them.
const REVIEWERS = [
  {
    user_id: "01rae",
    name: "Rae Payments",
    email: "rae@acme.test",
    approved: true,
  },
  {
    user_id: ME.id,
    name: ME.name,
    email: ME.email,
    approved: false,
  },
];

test("on the dashboard mount a standing approval can still be revoked", async ({
  page,
}) => {
  // `repo.tsx` mounted `ChangesPanel` with no `me`, so every question the
  // page asks about the viewer — are you the author, is your approval
  // standing, does OWNERS require you — answered false on this mount and
  // only on this mount. The button that says "Revoke my approval" was
  // therefore disabled for the one person it exists for.
  //
  // It survived because every revoke and abandon test reaches the change
  // through `onForgeChange`, and the forge mount always passed `me`. A
  // prop threaded on one of two mounts is invisible to a suite that only
  // ever drives the other.
  //
  // This signs in **as a person**, not with a token, and that is the
  // whole reason the test is written this way: `signIn` stores an API
  // token and mocks `/v1/auth/me` to 401, so `me` is null there by
  // construction — correctly, because a token is not a person and has no
  // approval to revoke. Only the password path sets `me`, from the login
  // response. A first version of this test used `signIn` and failed
  // against the *fixed* code, which reads exactly like the fix not
  // working.
  await signInAsPerson(page);
  await mockReview(page, {
    approvals: [
      { name: ME.name, email: ME.email, created_at: Date.now() - 60_000 },
    ],
  });
  await openChange(page);

  await expect(
    page.getByRole("button", { name: "Revoke my approval" }),
  ).toBeEnabled();
});

test("the sidebar names who the change is waiting on and calls out the viewer", async ({
  page,
}) => {
  // The forge mount, because it is the one that knows who is looking:
  // the dashboard's `ChangesPanel` in `repo.tsx` passes no `me`, so the
  // "you are required" sentence — like the Revoke button beside it —
  // cannot render there. See the report accompanying this change.
  await onForgeChange(page, {
    me: ME,
    write: true,
    reviewers: { required: REVIEWERS, anyone_with_write: false },
  });

  const card = page.getByRole("region", { name: "Required reviewers" });
  await expect(card).toBeVisible();
  // Where the list came from, which is the whole claim being made.
  await expect(card.getByText(/Computed from the OWNERS files/)).toBeVisible();
  await expect(card.getByText("1 of 2 approved")).toBeVisible();

  // Each person, with a word beside the glyph: a tick that differed only
  // by colour would say nothing to a reader who cannot see the
  // difference.
  const rae = card.getByRole("listitem").filter({ hasText: "Rae Payments" });
  await expect(rae.getByText("approved")).toBeVisible();
  await expect(rae.getByText("rae@acme.test")).toBeVisible();
  const ada = card.getByRole("listitem").filter({ hasText: ME.name });
  await expect(ada.getByText("waiting")).toBeVisible();

  // The viewer is told the change cannot land without them, which is
  // the thing a nominated reviewer list can never say for certain.
  await expect(
    card.getByText(
      /You are a required reviewer; this change is waiting on you/,
    ),
  ).toBeVisible();

  // ...and there is no way to add anybody. A reviewer picker would put
  // the set back in the author's hands, which is the design this whole
  // panel exists to refuse.
  await expect(card.getByRole("button")).toHaveCount(0);
  await expect(card.getByRole("textbox")).toHaveCount(0);
  await expect(page.getByText(/Request a review/i)).toHaveCount(0);
});

test("a required reviewer who has approved is told so, not asked again", async ({
  page,
}) => {
  await onForgeChange(page, {
    me: ME,
    write: true,
    reviewers: {
      required: REVIEWERS.map((r) => ({ ...r, approved: true })),
      anyone_with_write: false,
    },
  });

  const card = page.getByRole("region", { name: "Required reviewers" });
  await expect(card.getByText("2 of 2 approved")).toBeVisible();
  await expect(
    card.getByText(/You are a required reviewer — your approval is already in/),
  ).toBeVisible();
  await expect(card.getByText("waiting")).toHaveCount(0);
});

// The two empty lists below are byte-identical in `required` and are
// opposite sentences to somebody deciding whether they may approve, so
// each gets its own assertion rather than a shared "the list is empty".
test("a * rule says anyone with write access, not that nobody is required", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page, {
    reviewers: { required: [], anyone_with_write: true },
  });
  await openChange(page);

  const card = page.getByRole("region", { name: "Required reviewers" });
  await expect(card.getByText(/anyone with write access/)).toBeVisible();
  await expect(card.getByText(/No OWNERS rule governs/)).toHaveCount(0);
  await expect(card.getByRole("listitem")).toHaveCount(0);
});

test("an ungoverned change says no rule governs it", async ({ page }) => {
  await signIn(page);
  // The fixture's default: nobody required and no `*` rule either.
  await mockReview(page);
  await openChange(page);

  const card = page.getByRole("region", { name: "Required reviewers" });
  await expect(
    card.getByText(/No OWNERS rule governs what this patchset touches/),
  ).toBeVisible();
  await expect(card.getByText(/anyone with write access/)).toHaveCount(0);
});

test("a server that sends no reviewer set gets no card, not an empty one", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page, { reviewers: null });
  await openChange(page);

  // The rest of the review still renders — the field is an addition, not
  // a dependency — and the panel that would have made a claim the server
  // never made is simply absent.
  await expect(page.getByText("Landable")).toBeVisible();
  await expect(
    page.getByRole("region", { name: "Required reviewers" }),
  ).toHaveCount(0);
});

// ---------------------------------------------------------------------
// Reading the diff itself: unfolding context, the per-file counts, the
// whitespace toggle, and the address of a line.
//
// The fold marker used to be three dots and nothing else — the only way
// to see the function a changed line sits in was to clone the
// repository, which is the opposite of what a review screen is for. The
// lines are now the diff library's (`src/code/surface.tsx`): it folds
// the unchanged runs, prints how long each is, and offers its own
// controls to open them. Those controls are icon-only and carry no
// accessible name — the one data-attribute family COMPONENTS.md admits
// — so what these hold is that the folds are there, that each control
// reveals the lines on its side, and that ours (the whole-file switch,
// the whitespace toggle, the counts, the anchors) are wired to it.

/// The `<li>` of the open file in the list, which is where its toolbar
/// and its diff live. Innermost, because a list item further out can
/// contain the same button.
const openFile = (page: Page, path: RegExp) =>
  page
    .locator("li")
    .filter({ has: page.getByRole("button", { name: path }) })
    .last();

/// `gateway.rs`, two hundred lines long, changed at line 5 and line 100
/// — so the diff has one fold small enough to open in a single press and
/// two too large for that, which are the two shapes of the control.
async function mockLongFile(page: Page) {
  const lines = (marks: Record<number, string>) =>
    Array.from({ length: 200 }, (_, i) => marks[i + 1] ?? `line ${i + 1}`).join(
      "\n",
    ) + "\n";
  await page.route(
    "**/v1/orgs/acme/repos/session-1/files/payments/gateway.rs?*",
    (r) => {
      const at = new URL(r.request().url()).searchParams.get("at");
      return r.fulfill({
        status: 200,
        contentType: "application/octet-stream",
        body:
          at === "9".repeat(40)
            ? lines({ 5: "was five", 100: "was a hundred" })
            : lines({ 5: "now five", 100: "now a hundred" }),
      });
    },
  );
}

/// Which lines of the long file are on screen, asked of the text of the
/// line. `exact`, because `line 1` is a substring of `line 100`, and a
/// test that could not tell them apart would pass on the wrong row.
const line = (page: Page, n: number) =>
  page.getByText(`line ${n}`, { exact: true });

test("a fold opens, from either end and in one go, and says how long it is", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page);
  await mockLongFile(page);
  await openChange(page);
  await page.getByRole("button", { name: /payments\/gateway\.rs/ }).click();
  await expect(page.getByText("now a hundred")).toBeVisible();
  const file = openFile(page, /payments\/gateway\.rs/);

  // Two folds: the long runs either side of the second hunk. Each says
  // how long it is — a count is what tells a reviewer whether to bother
  // — and the lines around each hunk are already there. The library
  // renders every separator twice (once per layout), so a fold is
  // always the first visible one.
  const fold = (lines: number) =>
    file
      .locator("[data-separator]")
      .filter({ hasText: new RegExp(`^${lines} unmodified lines`) })
      .first();
  await expect(fold(86)).toBeVisible();
  await expect(fold(96)).toBeVisible();
  await expect(line(page, 4)).toBeVisible();
  await expect(line(page, 9)).toBeVisible();
  await expect(line(page, 10)).toHaveCount(0);
  await expect(line(page, 95)).toHaveCount(0);

  // The long run between the hunks opens a screenful at a time, from
  // whichever end the reviewer needs — the lines just after the hunk
  // above, or the lines just before the hunk below — so it carries one
  // control per end.
  await fold(86).locator("[data-expand-up]").click();
  await expect(line(page, 10)).toBeVisible();
  await expect(line(page, 33)).toBeVisible();
  // …and the far end of the same gap is still folded.
  await expect(line(page, 95)).toHaveCount(0);
  await expect(fold(62)).toBeVisible();
  await fold(62).locator("[data-expand-down]").click();
  await expect(line(page, 95)).toBeVisible();
  await expect(line(page, 72)).toBeVisible();
  await expect(line(page, 50)).toHaveCount(0);
  await expect(fold(38)).toBeVisible();

  // The run after the last hunk has only the one end to open from.
  await expect(fold(96).locator("[data-expand-down]")).toHaveCount(0);
  await fold(96).locator("[data-expand-up]").click();
  await expect(line(page, 128)).toBeVisible();
  await expect(line(page, 200)).toHaveCount(0);
  await expect(fold(72)).toBeVisible();

  // The whole file, and back to just the changes. Ours, in the toolbar,
  // because "everything" is a decision about the file and not about one
  // fold.
  await page.getByRole("button", { name: "Expand whole file" }).click();
  await expect(line(page, 150)).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Expand whole file" }),
  ).toHaveCount(0);
  // Back to just the changes means the folds as the reviewer last left
  // them, not the pristine ones: what was opened by hand stays open.
  await page.getByRole("button", { name: "Collapse to changes" }).click();
  await expect(line(page, 150)).toHaveCount(0);
  await expect(fold(38)).toBeVisible();
  await expect(fold(72)).toBeVisible();
  await expect(line(page, 33)).toBeVisible();
  await expect(page.getByText("now a hundred")).toBeVisible();
});

test("every file carries its own +N −M before anything is opened", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page);
  await openChange(page);

  // The counts are read from the files themselves, so they are there
  // without a click — which is the point: a reviewer decides what to
  // read first by size, and could not see a size at all.
  await expect(page.getByText("1 added, 1 removed")).toHaveCount(FILES.length);
  await expect(
    page.getByRole("button", { name: /payments\/gateway\.rs/ }),
  ).toContainText("+1");
  // The rail repeats them as a decoration beside the row: the accessible
  // name of a rail row is still the file's segment, so the numbers are
  // not read out twice per file.
  const rail = page.getByRole("navigation", { name: "Files in this patchset" });
  const row = rail.getByRole("treeitem", { name: "gateway.rs", exact: true });
  await expect(row).toBeVisible();
  await expect(row).toContainText("−1");
});

test("ignoring whitespace changes what the diff says, per file", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page);
  // A pure reindent: the same three lines, one of them moved right.
  await page.route(
    "**/v1/orgs/acme/repos/session-1/files/docs/README.md?*",
    (r) => {
      const at = new URL(r.request().url()).searchParams.get("at");
      return r.fulfill({
        status: 200,
        contentType: "application/octet-stream",
        body:
          at === "9".repeat(40)
            ? "fn main() {\nlet fee = 1;\n}\n"
            : "fn main() {\n    let fee = 1;\n}\n",
      });
    },
  );
  await openChange(page);
  const row = page.getByRole("button", { name: /docs\/README\.md/ });
  await expect(row).toContainText("+1");
  await row.click();
  await expect(page.getByText("fn main() {")).toBeVisible();

  await page.getByLabel("Ignore whitespace").check();
  // Nothing is left to read, and the page says so in words rather than
  // drawing an empty diff; the count on the row agrees — the two are the
  // same parse, so a toggle that moved one and not the other would be
  // telling a reviewer two different things at once.
  await expect(
    page.getByText("No difference once whitespace is ignored."),
  ).toBeVisible();
  await expect(row).toContainText("+0");
  await expect(row).toContainText("−0");
  // Per file: the other files are untouched by this file's choice.
  await expect(page.getByText("1 added, 1 removed")).toHaveCount(
    FILES.length - 1,
  );
});

test("a link to a line opens that file, unfolds it and lands on the line", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page);
  await mockLongFile(page);
  // Arriving cold, the way somebody following a link out of a comment
  // does: no file open, and the line deep inside a fold.
  await page.goto("/acme/session-1/changes/Icafe1234#payments/gateway.rs:L150");
  await expect(page.getByText("Files in this patchset")).toBeVisible();

  // The linked line is the selected one, and it is on screen.
  const anchored = page.locator("[data-selected-line]");
  await expect(anchored.first()).toBeVisible();
  await expect(anchored.filter({ hasText: "line 150" }).first()).toBeVisible();
  await expect(line(page, 150)).toBeInViewport();
  // Its neighbours came with it — a line on its own says nothing.
  await expect(line(page, 148)).toBeVisible();
  // And a line's number is itself the address to send, so the next
  // reviewer does not have to describe it in prose: clicking the
  // new-side number writes the line into the URL.
  await page.locator('[data-column-number="148"]').last().click();
  await expect
    .poll(() => new URL(page.url()).hash)
    .toBe("#payments/gateway.rs:L148");
  await expect(anchored.filter({ hasText: "line 148" }).first()).toBeVisible();
  await expect(anchored.filter({ hasText: "line 150" })).toHaveCount(0);
});

test("the diff layout is a choice that survives a reload", async ({ page }) => {
  await signIn(page);
  await mockReview(page);
  await page.goto("/acme/session-1/changes/Icafe1234");
  await expect(page.getByText("Files in this patchset")).toBeVisible();
  // Unified by default: it is what every diff here looked like until the
  // split view existed, and a phone has one column's width.
  await expect(page.getByRole("radio", { name: "Unified" })).toBeChecked();
  await page.getByRole("radio", { name: "Split" }).check();
  await page.getByRole("button", { name: /payments\/gateway\.rs/ }).click();
  await expect(page.getByText("let fee = new();")).toBeVisible();

  // A preference, not a fact about the change: it follows the person
  // into the next load of the page.
  await page.reload();
  await expect(page.getByText("Files in this patchset")).toBeVisible();
  await expect(page.getByRole("radio", { name: "Split" })).toBeChecked();
});

test("since patchset N narrows the file list to what moved", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page);
  // Two patchsets, so there is an earlier reading to measure from. The
  // detail route answers both; the file list against the parent is the
  // fixture's three files, and the interdiff says only one of them
  // changed between the two.
  const base = "**/v1/orgs/acme/repos/session-1";
  const ps = (n: number) => ({
    number: n,
    commit: n === 1 ? "1".repeat(40) : "2".repeat(40),
    parent: "9".repeat(40),
    message: "add gateway\n\nChange-Id: Icafe1234\n",
    created_at: Date.now() - 60_000 * (3 - n),
  });
  await page.route(`${base}/changes/Icafe1234`, (r) =>
    r.fulfill({
      json: {
        change: {
          key: "Icafe1234",
          title: "add gateway",
          target_branch: "main",
          source: null,
          state: "open",
          land_verdict: null,
          changeset: null,
          landed_commit: null,
          created_at: Date.now() - 60_000,
          updated_at: Date.now() - 60_000,
          patchset: ps(2),
        },
        patchsets: [ps(1), ps(2)],
        approvals: [],
        reviews: [],
      },
    }),
  );
  const interdiffs: string[] = [];
  await page.route(`${base}/changes/Icafe1234/interdiff?**`, (r) => {
    interdiffs.push(new URL(r.request().url()).search);
    return r.fulfill({
      json: {
        from: "1".repeat(40),
        to: "2".repeat(40),
        changes: [
          {
            status: "modified",
            path: "payments/gateway.rs",
            old_oid: "1".repeat(40),
            new_oid: "2".repeat(40),
            old_mode: "100644",
            new_mode: "100644",
          },
        ],
      },
    });
  });
  await openChange(page);
  await expect(page.getByText("0 of 3 viewed")).toBeVisible();

  // The choice names the earlier patchset, and the server does the tree
  // diff between the two — the client sends the numbers and nothing
  // else.
  const compare = page.getByLabel("Compare against");
  await expect(compare).toHaveValue("");
  await compare.selectOption("since patchset 1");
  await expect(page.getByText("0 of 1 viewed")).toBeVisible();
  expect(interdiffs).toEqual(["?from=1&to=2"]);
  await expect(
    page.getByRole("button", { name: /payments\/gateway\.rs/ }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: /docs\/README\.md/ }),
  ).toHaveCount(0);

  // And back to the parent, which is the whole change again.
  await compare.selectOption("parent");
  await expect(page.getByText("0 of 3 viewed")).toBeVisible();
});

// ---------------------------------------------------------------------
// Threads: replies, resolution, and the two anchors that had no
// interface at all.
//
// The rules themselves — a reply is one level deep and inherits its
// root's anchor whole, who may call a thread settled, that a resolved
// thread keeps its first resolver — are pinned against a real server in
// `crates/stratum-server/tests/changes_e2e.rs`. What these hold is that
// the page groups the flat conversation the way the server says to,
// sends exactly the fields a reply may carry, and never draws a control
// the server would refuse.
//
// The mock implements the *server's* rule for a reply's anchor rather
// than echoing what was sent, for the same reason the viewed-mark mock
// implements the patchset rule: a fixture that stored whatever arrived
// would let a client that anchors its replies pass, and the anchor is
// the one field a reply must not choose.

test("a reply nests under its thread and carries no anchor of its own", async ({
  page,
}) => {
  const s = await onForgeChange(page, { me: ME, write: true });

  await page.getByRole("button", { name: "Reply to Olive Owner" }).click();
  await page
    .getByLabel("Reply to Olive Owner")
    .fill("rounding is settled in patchset 2");
  await page.getByRole("button", { name: "Post reply" }).click();

  // The body and the thread, and *nothing else*. A reply that also sent
  // `path`/`line`/`side` would be refused by the server, and one the
  // server accepted would let a thread describe two places at once.
  await expect.poll(() => s.posts.length).toBe(1);
  expect(s.posts[0]).toEqual({
    body: "rounding is settled in patchset 2",
    parent_id: "01c1",
  });

  // ...and it renders inside its root's thread, not as a fourth remark.
  const thread = page
    .locator("li")
    .filter({ hasText: "ship it once the fee rounding is settled" });
  await expect(
    thread.getByText("rounding is settled in patchset 2"),
  ).toBeVisible();
  const other = page.locator("li").filter({ hasText: "my first review here" });
  await expect(
    other.getByText("rounding is settled in patchset 2"),
  ).toHaveCount(0);
});

test("a reply offers no Reply control of its own", async ({ page }) => {
  // The server refuses a reply to a reply — "replies are one level deep"
  // — so a button that exists only to produce that sentence is the
  // show-and-fail this page keeps avoiding. `ME` writes the reply, so
  // "Reply to Ada Reviewer" is the control that must not exist.
  await onForgeChange(page, { me: ME, write: true });
  await page.getByRole("button", { name: "Reply to Olive Owner" }).click();
  await page.getByLabel("Reply to Olive Owner").fill("one level only");
  await page.getByRole("button", { name: "Post reply" }).click();
  await expect(page.getByText("one level only")).toBeVisible();

  // Counted, not retried: the reply is on screen, so this reads a
  // settled state and cannot pass by being early.
  expect(
    await page.getByRole("button", { name: `Reply to ${ME.name}` }).count(),
  ).toBe(0);
  // The root still has exactly one, which is what says the absence above
  // is about replies rather than about the button having disappeared.
  await expect(
    page.getByRole("button", { name: "Reply to Olive Owner" }),
  ).toHaveCount(1);
});

test("resolving collapses a thread and names who settled it, and it can be reopened", async ({
  page,
}) => {
  const s = await onForgeChange(page, { me: ME, write: true });

  await page
    .getByRole("button", { name: "Resolve thread from Olive Owner" })
    .click();
  await expect.poll(() => s.resolves).toEqual([{ id: "01c1", resolved: true }]);

  // Collapsed to one line that says who called it settled. The byline
  // and the reply control go with the body: a thread nobody is arguing
  // about any more should not cost a screenful.
  await expect(page.getByText(`Resolved by ${ME.name}`)).toBeVisible();
  await expect(page.getByText("olive@acme.test")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Reply to Olive Owner" }),
  ).toHaveCount(0);

  // "Dealt with" is not "deleted": the next reviewer wants to know what
  // was decided, not only that something was.
  await page
    .getByRole("button", { name: "Show resolved thread from Olive Owner" })
    .click();
  await expect(page.getByText("olive@acme.test")).toBeVisible();

  // And it reopens, under the same rule — whoever could close it can
  // open it, which is what keeps "resolved" from being a silencer.
  await page
    .getByRole("button", { name: "Reopen thread from Olive Owner" })
    .click();
  await expect
    .poll(() => s.resolves)
    .toEqual([
      { id: "01c1", resolved: true },
      { id: "01c1", resolved: false },
    ]);
  await expect(
    page.getByRole("button", { name: "Resolve thread from Olive Owner" }),
  ).toBeVisible();
  await expect(page.getByText(`Resolved by ${ME.name}`)).toHaveCount(0);
});

test("a reader the server would refuse is not offered Resolve, but may still settle their own remark", async ({
  page,
}) => {
  // Ungoverned files — the fixture's default reviewer set — so the
  // server's rule reduces to write access, which this reader does not
  // have. Offering the button and learning that from a 403 is exactly
  // the show-and-fail the Actions card was rebuilt to stop.
  await onForgeChange(page, { me: ME, write: false });

  expect(
    await page.getByRole("button", { name: /^Resolve thread/ }).count(),
  ).toBe(0);
  // Hidden *with an explanation*, and exactly one of them: a sentence
  // repeated beside every thread would bury the review under it.
  await expect(
    page.getByText(/Nobody owns these files in particular/),
  ).toHaveCount(1);

  // Withdrawing your own remark needs nobody's permission, and that is
  // the half a write-access check would have got wrong.
  await page
    .getByLabel("Comment on this change")
    .fill("never mind, I misread the fee table");
  await page.getByRole("button", { name: "Comment" }).click();
  await expect(
    page.getByRole("button", { name: `Resolve thread from ${ME.name}` }),
  ).toBeVisible();
});

test("the unresolved count is a fact beside the viewed count, and gates nothing", async ({
  page,
}) => {
  const s = await onForgeChange(page, { me: ME, write: true });

  await expect(page.getByText("3 unresolved")).toBeVisible();
  await expect(page.getByText("0 of 3 viewed")).toBeVisible();

  // Nothing about landing consults it. A forge that turned an open
  // remark into a lock would teach people to resolve threads in order to
  // ship rather than because they were addressed — so the button is
  // live, and nothing under it names a thread.
  await expect(
    page.getByRole("button", { name: "Land on main" }),
  ).toBeEnabled();
  await expect(page.getByText(/unresolved/)).toHaveCount(1);

  // It follows the threads, not the comments: settling one takes it to
  // two, and a reply to another leaves it there.
  await page
    .getByRole("button", { name: "Resolve thread from Olive Owner" })
    .click();
  await expect(page.getByText("2 unresolved")).toBeVisible();
  await page.getByRole("button", { name: "Reply to New Person" }).click();
  await page.getByLabel("Reply to New Person").fill("welcome");
  await page.getByRole("button", { name: "Post reply" }).click();
  await expect.poll(() => s.posts.length).toBe(1);
  await expect(page.getByText("2 unresolved")).toBeVisible();
});

test("a comment on a deleted line anchors to the old side", async ({
  page,
}) => {
  // The gutter used to appear on new-side lines only, on the argument
  // that a deleted line has nowhere to point at. But the line a patchset
  // *removed* is the thing a reviewer most often objects to, and with no
  // control there the objection went into the conversation panel as
  // prose describing a line number — which is what anchors exist to
  // replace. The server has stored a side since migration 0050.
  await signInAsPerson(page);
  const s = await mockReview(page);
  await openChange(page);
  await page.getByRole("button", { name: /payments\/gateway\.rs/ }).click();
  await expect(page.getByText("let fee = old();")).toBeVisible();

  // The `+` rides in the gutter beside whichever line the pointer is
  // on, labelled with that line, so it is reached by hovering the line.
  await page.getByText("let fee = old();").hover();
  await page
    .getByRole("button", { name: "Comment on deleted line 1", exact: true })
    .click();
  await page
    .getByLabel("Comment on payments/gateway.rs deleted line 1", {
      exact: true,
    })
    .fill("this handled the zero case");
  await page.getByRole("button", { name: "Post line comment" }).click();

  await expect.poll(() => s.posts.length).toBe(1);
  expect(s.posts[0]).toEqual({
    body: "this handled the zero case",
    path: "payments/gateway.rs",
    line: 1,
    side: "old",
  });
  await expect(page.getByText("this handled the zero case")).toHaveCount(2);
});

test("a comment on a block of lines sends the end of the range", async ({
  page,
}) => {
  await signInAsPerson(page);
  const s = await mockReview(page);
  await mockLongFile(page);
  await openChange(page);
  await page.getByRole("button", { name: /payments\/gateway\.rs/ }).click();
  await expect(page.getByText("now five")).toBeVisible();

  await page.getByText("now five").hover();
  await page
    .getByRole("button", { name: "Comment on line 5", exact: true })
    .click();
  // A typed field rather than only a drag: a drag is the gesture people
  // arrive with, and it is not an affordance for a keyboard at all.
  await page.getByLabel("Through line").fill("8");
  await page
    .getByLabel("Comment on payments/gateway.rs line 5")
    .fill("this whole block wants a name");
  await page.getByRole("button", { name: "Post line comment" }).click();

  await expect.poll(() => s.posts.length).toBe(1);
  expect(s.posts[0]).toEqual({
    body: "this whole block wants a name",
    path: "payments/gateway.rs",
    line: 5,
    side: "new",
    line_end: 8,
  });

  // It hangs under the *end* of what it is about, which is where the
  // selection gesture left the reader — and the conversation panel is
  // the one place that repeats the range, because in the diff the thread
  // is already sitting on the lines it names.
  await expect(
    page.getByText("payments/gateway.rs:5–8", { exact: true }),
  ).toHaveCount(1);
  await expect(page.getByText("this whole block wants a name")).toHaveCount(2);
});

test("a range dragged across lines opens the composer with both ends", async ({
  page,
}) => {
  await signInAsPerson(page);
  const s = await mockReview(page);
  await mockLongFile(page);
  await openChange(page);
  await page.getByRole("button", { name: /payments\/gateway\.rs/ }).click();
  await expect(page.getByText("now five")).toBeVisible();

  // Press on one new-side line number, release on another. Line 5 is a
  // change, so its number is printed on both sides, the new side last.
  const number = (n: number) =>
    page.locator(`[data-column-number="${n}"]`).last();
  await number(5).hover();
  await page.mouse.down();
  await number(8).hover();
  await page.mouse.up();

  // The form opens for the block, both ends filled in from the gesture,
  // and posting sends the range — the same wire shape the typed field
  // produces, because a drag and a typed number are two ways of saying
  // one thing.
  await expect(page.getByLabel("Line", { exact: true })).toHaveValue("5");
  await expect(page.getByLabel("Through line")).toHaveValue("8");
  await page
    .getByLabel("Comment on payments/gateway.rs line 5")
    .fill("dragged");
  await page.getByRole("button", { name: "Post line comment" }).click();
  await expect.poll(() => s.posts.length).toBe(1);
  expect(s.posts[0]).toEqual({
    body: "dragged",
    path: "payments/gateway.rs",
    line: 5,
    side: "new",
    line_end: 8,
  });
});

test("Comment on a line… opens the composer from the keyboard", async ({
  page,
}) => {
  await signInAsPerson(page);
  const s = await mockReview(page);
  await mockLongFile(page);
  await openChange(page);
  await page.getByRole("button", { name: /payments\/gateway\.rs/ }).click();
  await expect(page.getByText("now five")).toBeVisible();

  // The gutter control appears under a pointer and nowhere else; the
  // toolbar button is the way in for a reviewer without one. It opens
  // the form at line 1, and both ends are plain number fields from
  // there. A number takes effect on Enter (or on leaving the field),
  // not per keystroke: the form hangs on the line it names, so a live
  // commit moved it to line 3 halfway through typing 34 and dropped the
  // second digit with the focus.
  await page.getByRole("button", { name: "Comment on a line…" }).click();
  const start = page.getByLabel("Line", { exact: true });
  await expect(start).toHaveValue("1");
  await start.fill("5");
  await start.press("Enter");
  await expect(
    page.getByLabel("Comment on payments/gateway.rs line 5", { exact: true }),
  ).toBeVisible();
  await page.getByLabel("Through line").fill("7");
  await page.getByLabel("Through line").press("Enter");
  await page
    .getByLabel("Comment on payments/gateway.rs line 5", { exact: true })
    .fill("typed, not dragged");
  await page.getByRole("button", { name: "Post line comment" }).click();
  await expect.poll(() => s.posts.length).toBe(1);
  expect(s.posts[0]).toEqual({
    body: "typed, not dragged",
    path: "payments/gateway.rs",
    line: 5,
    side: "new",
    line_end: 7,
  });
});

test("a comment body renders as markdown, and Preview shows what will post", async ({
  page,
}) => {
  // Bodies rendered `whitespace-pre-wrap` for a long time while the
  // hardened renderer sat next door doing READMEs: people write review
  // comments in markdown whatever the box does with them, so a list of
  // three points arrived as three lines beginning with hyphens.
  await onForgeChange(page, {
    me: ME,
    write: true,
    extraComments: [
      rootComment(
        "01md",
        "Olive Owner",
        "user:01owner",
        "olive@acme.test",
        "the **fee table** needs a `round_half_even`",
        5_000,
      ),
    ],
  });

  // What the server already holds renders as markup, not as asterisks.
  await expect(
    page.locator("strong").filter({ hasText: "fee table" }),
  ).toBeVisible();
  await expect(
    page.locator("code").filter({ hasText: "round_half_even" }),
  ).toBeVisible();

  // And the composer previews through the same renderer, which is the
  // whole point of having one: a preview that used a second parser would
  // show a reviewer a table and post them a paragraph.
  await page
    .getByLabel("Comment on this change")
    .fill("agreed — see `fees/rates.rs` for the **other** half");
  await page.getByRole("tab", { name: "Preview" }).click();
  await expect(page.getByLabel("Comment on this change")).toHaveCount(0);
  await expect(
    page.locator("strong").filter({ hasText: "other" }),
  ).toBeVisible();
  await expect(
    page.locator("code").filter({ hasText: "fees/rates.rs" }),
  ).toBeVisible();

  // Back to Write with the words intact, and what posts renders exactly
  // as the preview did.
  await page.getByRole("tab", { name: "Write" }).click();
  await expect(page.getByLabel("Comment on this change")).toHaveValue(
    "agreed — see `fees/rates.rs` for the **other** half",
  );
  await page.getByRole("button", { name: "Comment" }).click();
  await expect(page.getByLabel("Comment on this change")).toHaveValue("");
  await expect(
    page.locator("li").locator("strong").filter({ hasText: "other" }),
  ).toBeVisible();
});

test("a server that predates threads gets a flat conversation, not a wrong count", async ({
  page,
}) => {
  // Absent is not empty. Every row from such a server has no `parent_id`
  // and no `resolved_at`, so read naively the page would announce "3
  // unresolved" about a server with no notion of resolution and offer a
  // Reply and a Resolve to routes that answer 404. The same degradation
  // the reviewer set already makes: silence, not an invented claim.
  await onForgeChange(page, { me: ME, write: true, legacy: true });

  // The conversation is all there and still renders as markdown.
  await expect(
    page.getByText("ship it once the fee rounding is settled"),
  ).toBeVisible();
  await expect(page.getByText("my first review here")).toBeVisible();

  expect(await page.getByText(/unresolved/).count()).toBe(0);
  expect(
    await page.getByRole("button", { name: /^Resolve thread/ }).count(),
  ).toBe(0);
  expect(await page.getByRole("button", { name: /^Reply to/ }).count()).toBe(0);
  // ...and the thing that still works, works: a comment on the change as
  // a whole is a route that server has always had.
  await expect(page.getByLabel("Comment on this change")).toBeVisible();
});

// ---------------------------------------------------------------------
// The batched review — migration 0051.
//
// A review is one act: a pile of drafted comments nobody else can see, a
// verdict, and one request that publishes the lot. Two things about it
// are easy to get wrong in ways that look like the page working, and
// both are pinned below. A draft rendered like a published comment is a
// review that silently never happened; and a `request_changes` from
// somebody with no standing on any touched path is *recorded and
// advisory*, so telling its author their change is stuck would send them
// chasing a person who does not gate anything.

test("a drafted comment goes out as pending, is marked, and is not the conversation yet", async ({
  page,
}) => {
  const s = await onForgeChange(page, { me: ME, write: true });

  await page
    .getByLabel("Comment on this change")
    .fill("the retry is unbounded");
  await page.getByRole("button", { name: "Add to review" }).click();

  // The wire, parsed. Asserted with `toEqual` on the parsed body rather
  // than on a substring: this repository has shipped a double-encoded
  // request body once, and a `toContain` over the raw text would have
  // passed against it.
  await expect
    .poll(() => s.posts.length, { message: "the draft never posted" })
    .toBe(1);
  expect(s.posts[0]).toEqual({
    body: "the retry is unbounded",
    pending: true,
  });

  // ...and it is on the page saying what it is. A drafted remark that
  // looked exactly like a published one is how a review gets forgotten
  // in a tab with nothing on screen to say so.
  const drafted = page
    .getByRole("listitem")
    .filter({ hasText: "the retry is unbounded" });
  await expect(drafted.getByText("Pending")).toBeVisible();
  await expect(
    drafted.getByText("Only you can see this until you submit your review."),
  ).toBeVisible();
  // It is not yet a conversation, so neither control that presumes one
  // is offered on it: nobody has read a word of it.
  await expect(drafted.getByRole("button", { name: /^Reply to/ })).toHaveCount(
    0,
  );
  await expect(
    drafted.getByRole("button", { name: /^Resolve thread/ }),
  ).toHaveCount(0);
  // ...and it does not move the unresolved count, which is a fact about
  // the *conversation*. Three roots were already there and the draft is
  // not in it yet: counting it would print a number nobody else on the
  // change can see, beside one they can, with nothing saying which is
  // which.
  // `exact` here for the same reason as the pending count below: a
  // substring match would let "13 unresolved" satisfy an assertion
  // written about 3, and a count is precisely where that bites.
  await expect(page.getByText("3 unresolved", { exact: true })).toBeVisible();
});

test("the bar counts the drafts and is the way into the sheet", async ({
  page,
}) => {
  await onForgeChange(page, { me: ME, write: true });
  const bar = page.getByRole("status", { name: "Pending review" });
  // Nothing drafted, no bar. A standing "you have 0 pending comments"
  // is a permanent piece of furniture that means nothing.
  await expect(bar).toHaveCount(0);

  const box = page.getByLabel("Comment on this change");
  await box.fill("first");
  await page.getByRole("button", { name: "Add to review" }).click();
  // Wait on the box clearing — the observable the next interaction
  // needs — rather than on a request count, which is true before the
  // page has finished with the response.
  await expect(box).toHaveValue("");
  // `exact`, and it is load-bearing: `getByText` is a substring match,
  // so "1 pending comments" contains "1 pending comment" and the
  // assertion this line exists to make would have passed against the
  // bug it exists to catch.
  await expect(
    bar.getByText("1 pending comment", { exact: true }),
  ).toBeVisible();

  await box.fill("second");
  await page.getByRole("button", { name: "Add to review" }).click();
  await expect(box).toHaveValue("");
  // The singular is not one interpolation away from "1 pending
  // comments", which is the first thing a reader notices.
  await expect(
    bar.getByText("2 pending comments", { exact: true }),
  ).toBeVisible();

  await bar.getByRole("button", { name: "Finish your review" }).click();
  await expect(
    page.getByRole("region", { name: "Submit your review" }),
  ).toBeVisible();
});

test("submitting sends one request carrying the verdict and the body", async ({
  page,
}) => {
  const s = await onForgeChange(page, { me: ME, write: true });
  const box = page.getByLabel("Comment on this change");
  await box.fill("the retry is unbounded");
  await page.getByRole("button", { name: "Add to review" }).click();
  await expect(box).toHaveValue("");

  await page
    .getByRole("status", { name: "Pending review" })
    .getByRole("button", { name: "Finish your review" })
    .click();
  const sheet = page.getByRole("region", { name: "Submit your review" });
  await sheet.getByRole("radio", { name: "Request changes" }).check();
  await sheet
    .getByLabel("Review message")
    .fill("not while the retry is unbounded");
  await sheet.getByRole("button", { name: "Submit review" }).click();

  await expect
    .poll(() => s.submits.length, { message: "the review never submitted" })
    .toBe(1);
  // The whole feature, asserted as a parsed object: one request, and it
  // carries exactly the two fields. Twelve remarks used to be twelve
  // posts and — for a while — twelve mails.
  expect(s.submits[0]).toEqual({
    verdict: "request_changes",
    body: "not while the retry is unbounded",
  });
  // One, and it stays one: nothing about the reload that follows sends
  // a second.
  await expect(
    page.getByRole("status", { name: "Pending review" }),
  ).toHaveCount(0);
  expect(s.submits).toHaveLength(1);
  // The sheet closes and the remark is now everybody's.
  await expect(sheet).toHaveCount(0);
  await expect(
    page
      .getByRole("listitem")
      .filter({ hasText: "the retry is unbounded" })
      .getByText("Pending"),
  ).toHaveCount(0);
});

test("asking for changes with nothing in it is refused before the trip", async ({
  page,
}) => {
  const s = await onForgeChange(page, { me: ME, write: true });
  await page.getByRole("button", { name: /^Review patchset/ }).click();
  const sheet = page.getByRole("region", { name: "Submit your review" });
  await sheet.getByRole("radio", { name: "Request changes" }).check();

  // The server's own rule — "a block with nothing in it is a wall with
  // no door" — said in the sheet rather than discovered as a 400 with
  // the reviewer's paragraph attached to it.
  await expect(sheet.getByRole("alert")).toContainText("needs words");
  await expect(
    sheet.getByRole("button", { name: "Submit review" }),
  ).toBeDisabled();
  expect(s.submits).toHaveLength(0);

  // A cover message is enough on its own; so would a drafted comment be.
  await sheet.getByLabel("Review message").fill("say what would fix it");
  await expect(
    sheet.getByRole("button", { name: "Submit review" }),
  ).toBeEnabled();
});

test("the sheet offers three verdicts and no reviewer picker, and says why", async ({
  page,
}) => {
  await onForgeChange(page, {
    me: ME,
    write: true,
    reviewers: {
      required: [
        {
          user_id: "01owner",
          name: "Olive Owner",
          email: "olive@acme.test",
          approved: false,
        },
      ],
      anyone_with_write: false,
    },
  });
  await page.getByRole("button", { name: /^Review patchset/ }).click();
  const sheet = page.getByRole("region", { name: "Submit your review" });

  const verdicts = sheet.getByRole("radiogroup", { name: "Verdict" });
  await expect(verdicts.getByRole("radio")).toHaveCount(3);
  // `exact`, and it is the point rather than tidiness. Playwright
  // matches an accessible name by *substring* — for `getByRole`'s
  // `name` exactly as for `getByText` — so a lax match would have been
  // satisfied by the name these controls actually had: a wrapping
  // `<label>` handed each radio its whole help sentence, and the
  // control announced itself as "Request changes Say no, and say what
  // would make it a yes…". Asserting the whole string is what caught
  // that; `aria-labelledby` is what fixed it.
  for (const name of ["Approve", "Comment", "Request changes"]) {
    await expect(
      verdicts.getByRole("radio", { name, exact: true }),
    ).toBeVisible();
  }

  // A deliberate, permanent refusal — not an oversight, and not a
  // feature waiting to be added. The sentence points at the card that
  // does answer the question, which is on the page already.
  await expect(sheet.getByText(/nobody to nominate/)).toBeVisible();
  await expect(sheet.getByText(/computed from OWNERS/)).toBeVisible();
  await expect(
    sheet.getByRole("textbox", { name: /request a review/i }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("region", { name: "Required reviewers" }),
  ).toBeVisible();
});

test("a draft can be thrown away, and only after saying so twice", async ({
  page,
}) => {
  const s = await onForgeChange(page, { me: ME, write: true });
  const box = page.getByLabel("Comment on this change");
  await box.fill("never mind");
  await page.getByRole("button", { name: "Add to review" }).click();
  await expect(box).toHaveValue("");

  await page
    .getByRole("status", { name: "Pending review" })
    .getByRole("button", { name: "Finish your review" })
    .click();
  const sheet = page.getByRole("region", { name: "Submit your review" });
  // One stray click must not throw away somebody's pass over a change.
  await sheet.getByRole("button", { name: "Discard draft review" }).click();
  expect(s.discards).toBe(0);
  await sheet.getByRole("button", { name: "Discard them" }).click();

  await expect
    .poll(() => s.discards, { message: "the draft was never discarded" })
    .toBe(1);
  await expect(sheet).toHaveCount(0);
  await expect(page.getByText("never mind")).toHaveCount(0);
});

test("an authoritative request for changes blocks landing and says so in the server's words", async ({
  page,
}) => {
  // The server answers the block by folding it into the verdict: not
  // landable, with a sentence naming who asked. Both halves are the
  // server's, and the page renders them rather than deciding either.
  await onForgeChange(page, {
    me: ME,
    write: true,
    landable: false,
    explanation:
      "blocked: olive@acme.test asked for changes; it stands until they withdraw it",
    blocks: [
      standingBlock(
        "01r1",
        "Olive Owner",
        "01owner",
        "olive@acme.test",
        "the retry loop is unbounded",
        true,
      ),
    ],
  });

  const card = page.getByRole("region", { name: "Requested changes" });
  await expect(card.getByText("Olive Owner")).toBeVisible();
  await expect(card.getByText("Blocking")).toBeVisible();
  // Their own words, verbatim — the only thing on the page that says
  // what would make this a yes.
  await expect(card.getByText("the retry loop is unbounded")).toBeVisible();
  await expect(
    card.getByText(/cannot land until they withdraw it/),
  ).toBeVisible();

  await expect(actionButtons(page).land).toBeDisabled();
  await expect(
    page.getByText(/olive@acme\.test asked for changes/),
  ).toBeVisible();
});

test("a request for changes from somebody with no standing is advisory, and lands anyway", async ({
  page,
}) => {
  // The whole difference between this and GitHub, where any passer-by
  // can wedge a pull request. `blocking` is the server's answer and the
  // page never re-derives it from the verdict string — a client that
  // did would tell an author their change is stuck when it is not.
  await onForgeChange(page, {
    me: ME,
    write: true,
    blocks: [
      standingBlock(
        "01r2",
        "Passing Stranger",
        "01stranger",
        "stranger@acme.test",
        "I would not do it this way",
        false,
      ),
    ],
  });

  const card = page.getByRole("region", { name: "Requested changes" });
  // Recorded and rendered, in full: hiding it would hide half the
  // review from the author who has to answer it.
  await expect(card.getByText("I would not do it this way")).toBeVisible();
  // The badge, and then the sentence. Both, because a glyph and a tint
  // say nothing to a reader who cannot tell the two apart, and the
  // words are the half that says what to do about it.
  await expect(card.getByText("Advisory")).toBeVisible();
  await expect(card.getByText(/does not block landing/)).toBeVisible();
  await expect(card.getByText("Blocking")).toHaveCount(0);

  await expect(actionButtons(page).land).toBeEnabled();
});

test("withdrawing is offered to the block's author and to nobody else", async ({
  page,
}) => {
  const withdraw = (page: Page) =>
    page.getByRole("button", { name: "Withdraw my request for changes" });

  // Somebody else's block: rendered in full, with no control on it. The
  // route cannot name another person's review, so a button here would
  // answer 404 — and a block somebody else could clear is not a block.
  await onForgeChange(page, {
    me: ME,
    write: true,
    blocks: [
      standingBlock(
        "01r1",
        "Olive Owner",
        "01owner",
        "olive@acme.test",
        "the retry loop is unbounded",
        true,
      ),
    ],
  });
  await expect(
    page.getByRole("region", { name: "Requested changes" }),
  ).toBeVisible();
  await expect(withdraw(page)).toHaveCount(0);
});

test("the author of a block may take it back", async ({ page }) => {
  const s = await onForgeChange(page, {
    me: ME,
    write: true,
    landable: false,
    explanation:
      "blocked: owner@acme.test asked for changes; it stands until they withdraw it",
    blocks: [
      // Matched on the user id, which is what the server keys a
      // withdrawal by. A namesake or a shared display name must not
      // hand somebody else's block to this reader.
      standingBlock("01r3", ME.name, ME.id, ME.email, "not yet", true),
    ],
  });
  await page
    .getByRole("button", { name: "Withdraw my request for changes" })
    .click();
  await expect
    .poll(() => s.withdraws, { message: "withdraw was never sent" })
    .toBe(1);
});

test("a server that predates batched review keeps today's page, not a broken one", async ({
  page,
}) => {
  // Absent is not empty, one migration on from threads. Such a server
  // answers with no `reviews`, no `blocks` and no `pending`; read
  // naively the page would take the standalone Approve away in exchange
  // for a sheet posting to `…/review/submit`, a route that is not there,
  // and would offer "Add to review" for a field the comments route
  // ignores — so every drafted remark would be published immediately
  // while the page said only the author could see it.
  const s = await onForgeChange(page, {
    me: ME,
    write: true,
    legacyReviews: true,
  });

  const b = actionButtons(page);
  await expect(b.approve).toBeVisible();
  await expect(b.review).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Add to review" })).toHaveCount(
    0,
  );
  await expect(
    page.getByRole("status", { name: "Pending review" }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("region", { name: "Requested changes" }),
  ).toHaveCount(0);

  // ...and the thing that has always worked still does: a comment on
  // the change as a whole, published, with no `pending` on the wire.
  const box = page.getByLabel("Comment on this change");
  await box.fill("looks fine");
  await page.getByRole("button", { name: "Comment" }).click();
  await expect(box).toHaveValue("");
  expect(s.posts).toEqual([{ body: "looks fine" }]);
});

test("a line remark can be drafted into the review with its anchor", async ({
  page,
}) => {
  await signInAsPerson(page);
  const s = await mockReview(page);
  await openChange(page);
  await page.getByRole("button", { name: /payments\/gateway\.rs/ }).click();
  await expect(page.getByText("let fee = new();")).toBeVisible();
  await page.getByText("let fee = new();").hover();
  await page
    .getByRole("button", { name: "Comment on line 1", exact: true })
    .click();
  await page
    .getByLabel("Comment on payments/gateway.rs line 1", { exact: true })
    .fill("this rounds the wrong way");
  // The line composer's own control, not the change-wide one below it:
  // both are called "Add to review" and only one of them carries an
  // anchor. The line composer is the form hanging under the line.
  await page
    .locator("form")
    .filter({
      has: page.getByRole("button", { name: "Post line comment" }),
    })
    .getByRole("button", { name: "Add to review" })
    .click();

  await expect
    .poll(() => s.posts.length, { message: "the line draft never posted" })
    .toBe(1);
  // The anchor still goes with it. Drafting changes when a remark is
  // published, not what it is about — a draft that lost its line would
  // surface on submit as a change-wide comment nobody could place.
  expect(s.posts[0]).toEqual({
    body: "this rounds the wrong way",
    path: "payments/gateway.rs",
    line: 1,
    side: "new",
    pending: true,
  });
});

// Suggested changes: a reviewer proposing the exact lines, and the
// author taking them.
//
// The thing under test that is easiest to get wrong is not the button —
// it is that **one** patchset comes out however many suggestions went
// in. A page that made a request per click would look identical on
// screen and put five revisions on the change, start CI five times and
// notify everybody OWNERS names five times, for one act. So every test
// here that applies anything asserts the request *count* as well as its
// body.

/// The suggestion the tests below apply, and the line it stands in
/// place of. `payments/gateway.rs` line 1 is `let fee = new();` in the
/// fixture's file text, which is what makes the mini-diff assertable.
const SUGGESTION =
  "round the fee:\n```suggestion\nlet fee = round(new());\n```";
const REPLACED = "let fee = new();";

/// The comment card carrying a suggestion, wherever the page drew it.
const suggestionCard = (page: Page, words: string) =>
  page.locator("li").filter({ hasText: words }).first();

test("a suggestion renders as a diff against the lines it replaces, not as a code fence", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page, {
    extraComments: [lineComment("01s1", SUGGESTION)],
  });
  await openChange(page);

  const card = suggestionCard(page, "round the fee");
  // The caption names the file and the line, so an author reading the
  // conversation panel — nowhere near the diff — knows what this is
  // about without hunting for it.
  await expect(
    card.getByText("Suggested change to payments/gateway.rs line 1", {
      exact: true,
    }),
  ).toBeVisible();
  // The line being replaced is on screen. This is the whole difference
  // between a diff and a fence: a fence can only ever show the proposed
  // text, leaving the author to find the original and diff the two in
  // their head.
  await expect(card.getByText(`− ${REPLACED}`, { exact: true })).toBeVisible();
  await expect(
    card.getByText("+ let fee = round(new());", { exact: true }),
  ).toBeVisible();
  // And it is not still being rendered as markdown's code block. A
  // `<pre>` here would mean the fence went through `Markdown` untouched
  // and the mini-diff was drawn beside it rather than instead of it.
  await expect(card.locator("pre")).toHaveCount(0);
  // The prose around the block still renders as prose, through the same
  // renderer it always did.
  await expect(card.getByText("round the fee:", { exact: true })).toBeVisible();
});

test("applying one suggestion sends exactly the comment it names", async ({
  page,
}) => {
  await signIn(page);
  const s = await mockReview(page, {
    extraComments: [lineComment("01s1", SUGGESTION)],
  });
  await openChange(page);

  await page
    .getByRole("button", {
      name: "Apply Olive Owner's suggestion on payments/gateway.rs line 1",
      exact: true,
    })
    .click();

  await expect.poll(() => s.applies.length).toBe(1);
  // The parsed body, not a string: `call` serialises, and a body handed
  // over already stringified arrives double-encoded — a bug this
  // repository has shipped once.
  expect(s.applies[0]).toEqual({ comments: ["01s1"] });
});

test("two suggestions become one request carrying both ids", async ({
  page,
}) => {
  await signIn(page);
  const s = await mockReview(page, {
    extraComments: [
      lineComment("01s1", SUGGESTION),
      lineComment("01s2", "and here:\n```suggestion\nlet vat = 0;\n```", {
        author: "New Person",
        author_principal: "user:01newbie",
        author_email: "new@acme.test",
        path: "payments/fees/rates.rs",
        line: 1,
        line_end: 1,
      }),
    ],
  });
  await openChange(page);

  await page
    .getByRole("button", {
      name: "Add Olive Owner's suggestion on payments/gateway.rs line 1 to the next patchset",
      exact: true,
    })
    .click();
  await page
    .getByRole("button", {
      name: "Add New Person's suggestion on payments/fees/rates.rs line 1 to the next patchset",
      exact: true,
    })
    .click();

  const bar = page.getByRole("status", { name: "Gathered suggestions" });
  await expect(
    bar.getByText("2 suggestions gathered", { exact: true }),
  ).toBeVisible();
  await bar
    .getByRole("button", { name: "Apply as one patchset", exact: true })
    .click();

  await expect.poll(() => s.applies.length).toBe(1);
  expect(s.applies[0]).toEqual({ comments: ["01s1", "01s2"] });
  // And the gathering is put away once it has become a patchset, so
  // nobody applies the same two suggestions twice.
  await expect(bar).toHaveCount(0);
});

test("a reader without write access reads the suggestion and is offered no control", async ({
  page,
}) => {
  await onForgeChange(page, {
    me: ME,
    write: false,
    extraComments: [lineComment("01s1", SUGGESTION)],
  });

  const card = suggestionCard(page, "round the fee");
  // What is proposed is public reading on a public repository: knowing
  // what a reviewer suggested is not a permission.
  await expect(
    card.getByText("Suggested change to payments/gateway.rs line 1", {
      exact: true,
    }),
  ).toBeVisible();
  // The control is not drawn at all. The 403 is the honest fallback for
  // a race, not the check — a button that answers "you cannot push
  // here" is a door that was never open.
  await expect(
    page.getByRole("button", { name: /^Apply .*'s suggestion/ }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: /to the next patchset$/ }),
  ).toHaveCount(0);
});

test("a refusal is printed in the server's own words", async ({ page }) => {
  const sentence =
    'comments "01s1" and "01s2" both suggest changes to core.rs — lines 1-2 ' +
    "and 2-3 overlap. Apply one, then the other against the patchset it makes";
  await signIn(page);
  const s = await mockReview(page, {
    extraComments: [lineComment("01s1", SUGGESTION)],
    applyRefusal: { status: 409, error: sentence },
  });
  await openChange(page);

  await page
    .getByRole("button", {
      name: "Apply Olive Owner's suggestion on payments/gateway.rs line 1",
      exact: true,
    })
    .click();

  await expect.poll(() => s.applies.length).toBe(1);
  // Verbatim, and with nothing in front of it: the sentence *is* the
  // instruction — drop one of the two and apply the other — and
  // "Could not apply the suggestions:" welded onto its front would be
  // the page apologising over the top of the answer.
  await expect(page.getByText(sentence, { exact: true })).toBeVisible();
  await expect(page.getByText(/Could not apply/)).toHaveCount(0);
});

test("an empty suggestion reads as a deletion rather than as an empty box", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page, {
    extraComments: [
      lineComment("01s1", "this line does nothing:\n```suggestion\n```"),
    ],
  });
  await openChange(page);

  const card = suggestionCard(page, "this line does nothing");
  // Said in words, not merely drawn: a block with nothing in it and a
  // box that failed to render look identical on screen, and only one of
  // them is somebody's proposal.
  await expect(
    card.getByText("Suggested change: delete payments/gateway.rs line 1", {
      exact: true,
    }),
  ).toBeVisible();
  await expect(
    card.getByText("Nothing takes their place — this suggests removing them.", {
      exact: true,
    }),
  ).toBeVisible();
  // The lines going away are still shown, struck out of the file rather
  // than merely named.
  await expect(card.getByText(`− ${REPLACED}`, { exact: true })).toBeVisible();
  // And it is appliable: an empty block is a suggestion, not the
  // absence of one.
  await expect(
    page.getByRole("button", {
      name: "Apply Olive Owner's suggestion on payments/gateway.rs line 1",
      exact: true,
    }),
  ).toBeVisible();
});

test("a server that predates anchored comments offers no Apply", async ({
  page,
}) => {
  await signIn(page);
  await mockReview(page, {
    legacyServer: true,
    extraComments: [lineComment("01s1", SUGGESTION)],
  });
  await openChange(page);

  // The words are still rendered — the block is what the reviewer
  // wrote, whatever this deployment can do about it.
  const card = suggestionCard(page, "round the fee");
  await expect(
    card.getByText("+ let fee = round(new());", { exact: true }),
  ).toBeVisible();
  // But nothing is offered. A pre-0050 server sends no `side`, so no
  // comment names lines the apply route could replace — and *absent* is
  // not *empty*: a page that read the missing field as "new" would draw
  // a button whose route is not there.
  await expect(
    page.getByRole("button", { name: /suggestion on payments/ }),
  ).toHaveCount(0);
  // Including the caption naming a line it cannot claim to know.
  await expect(
    card.getByText("Suggested change", { exact: true }),
  ).toBeVisible();
});
