// The decisions the changesets screens make without a server: what a
// key may look like, which changes the picker may offer, what a landing
// is doing right now, and how a composed refusal reads. Pure, so each is
// held by a unit test and the components only render.

import type {
  ChangesetGate,
  ChangesetLanding,
  ChangesetRef,
  ChangesetState,
  DiffEntry,
  OrgChange,
} from "@/api";

/// The server's `valid_change_key`, exactly: non-empty, at most 72
/// bytes, ASCII letters, digits, `.`, `_` and `-`. Mirrored so the form
/// can say so before the round trip, never so it can disagree — a key
/// this accepts and the server refuses is a bug here.
export const MAX_KEY_LEN = 72;
export function validKey(key: string): boolean {
  return (
    key.length > 0 && key.length <= MAX_KEY_LEN && /^[A-Za-z0-9._-]+$/.test(key)
  );
}

/// The server's bounds on a changeset's shape, so the form's disabled
/// state and the server's 400 agree.
export const MAX_TITLE_BYTES = 200;
export const MAX_MEMBERS = 16;
export function validTitle(title: string): boolean {
  const t = title.trim();
  return t.length > 0 && new TextEncoder().encode(t).length <= MAX_TITLE_BYTES;
}

/// A key suggested from a title: lower-cased, every run of characters a
/// key may not hold folded to one `-`, trimmed of leading and trailing
/// separators, cut to the limit. Empty when nothing survives — a title
/// in a script with no ASCII letters — and the form asks for one.
export function suggestKey(title: string): string {
  return title
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-")
    .replace(/^[-._]+|[-._]+$/g, "")
    .slice(0, MAX_KEY_LEN)
    .replace(/[-._]+$/g, "");
}

/// Why a change cannot be ticked in the picker, or null when it can.
///
/// The picker never hides a change it will not offer: a reader looking
/// for "the payments one" should find it and be told why it is grey. Three
/// of the reasons are the server's three refusals for `POST …/members`,
/// in the words the server uses, so the row and a 409 would agree. The
/// fourth is the one the server does not put into words: composing needs
/// write on every member, and a member the caller may not write to is
/// refused with the same masked 404 a stranger gets — so the row has to
/// say it, or a viewer fills in the whole form and is told "no changeset".
/// Subtractive: a row that has not said whether it may be written is
/// treated as one that may not.
export function unpickable(
  c: OrgChange,
  picked: ChangesetRef[],
): string | null {
  if (c.state !== "open") return `this change is ${c.state}`;
  if (c.changeset) return `already in changeset ${c.changeset}`;
  if (c.viewer_write !== true)
    return `composing takes write access to ${c.repo}`;
  if (picked.some((p) => p.repo === c.repo && p.change !== c.key))
    return `one change per repository; ${c.repo} is already picked`;
  return null;
}

/// Toggle one change in the picked set.
export function togglePick(
  picked: ChangesetRef[],
  ref: ChangesetRef,
): ChangesetRef[] {
  const without = picked.filter(
    (p) => !(p.repo === ref.repo && p.change === ref.change),
  );
  return without.length === picked.length ? [...picked, ref] : without;
}

export function isPicked(picked: ChangesetRef[], ref: ChangesetRef): boolean {
  return picked.some((p) => p.repo === ref.repo && p.change === ref.change);
}

/// Whether the form may submit, in the server's terms.
export function canCreate(
  key: string,
  title: string,
  picked: ChangesetRef[],
): boolean {
  return (
    validKey(key) &&
    validTitle(title) &&
    picked.length >= 1 &&
    picked.length <= MAX_MEMBERS
  );
}

/// One line on where a landing stands, from its steps.
///
/// `outcome` is the server's word once it has one. Before that the
/// sentence counts, because a sixteen-member landing that says
/// "landing…" for a minute reads as stuck, and one that says "4 of 16
/// landed" does not. After a failure the count is what a reader needs
/// next: how many members landed and were put back, and which one
/// failed — the step's own `note` says why.
export function landingSummary(landing: ChangesetLanding): string {
  const total = landing.members.length;
  const done = landing.members.filter((m) => m.state === "done").length;
  const reverted = landing.members.filter((m) => m.state === "reverted").length;
  const failed = landing.members.find((m) => m.state === "failed");
  if (landing.outcome === "landed")
    return `Landed: ${total} ${total === 1 ? "member" : "members"} on their trunks.`;
  if (landing.outcome === "failed") {
    const parts: string[] = [];
    if (failed) parts.push(`${failed.repo}/${failed.change} failed`);
    if (reverted > 0)
      parts.push(
        `${reverted} landed ${reverted === 1 ? "member" : "members"} put back`,
      );
    if (done > 0) parts.push(`${done} left landed`);
    return `Failed: ${parts.join("; ")}.`;
  }
  return `Landing: ${done} of ${total} landed so far…`;
}

/// The tone a gate wears. Words carry the meaning — every place this is
/// used prints the gate's name beside it — so the colour only agrees.
/// The one word a verdict shows, for the set or for a member.
///
/// The wire keeps two answers apart: `gate` is what the *checks* say —
/// ready, waiting on a run, blocked by a red one — and `landable` is
/// what the *review* says. A member nobody has approved and nothing is
/// building for has `gate: "ready"` and `landable: false`, and its
/// explanation begins "blocked: needs an owner of …". Shown raw that
/// read "ready · blocked: needs an owner", which the walkthrough's
/// first pass over a governed member surfaced. The server's own
/// precedence — a person's approval before a machine's build — decides
/// the word: not landable is blocked whatever the checks say.
export function gateWord(v: {
  landable: boolean;
  gate: ChangesetGate;
}): ChangesetGate {
  return v.landable ? v.gate : "blocked";
}

/// The word and its colour, once the changeset's own state is known.
///
/// A verdict answers "can this land now", and the server keeps answering
/// it after the question is settled: a landed changeset's verdict is
/// `landable: false, "changeset is landed"`, and so is each landed
/// member's. Read as a gate, that painted the success state red — a set
/// that had landed cleanly read **blocked** in the Verdict box and
/// **blocked** on every row, which the walkthrough's first landed
/// screenshot showed. Once a changeset or a member is no longer open
/// there is no gate to report, only the state, in the state's colour.
/// `Change["state"]` is a subset of `ChangesetState`, so one function
/// serves the set and its rows.
export function verdictWord(
  state: ChangesetState,
  v: { landable: boolean; gate: ChangesetGate },
): { word: string; tone: string } {
  if (state !== "open") return { word: state, tone: stateTone(state) };
  const gate = gateWord(v);
  return { word: gate, tone: gateTone(gate) };
}

export function gateTone(gate: ChangesetGate): string {
  return gate === "ready"
    ? "text-good"
    : gate === "waiting"
      ? "text-warning"
      : "text-serious";
}

export function stateTone(state: ChangesetState): string {
  switch (state) {
    case "open":
      return "text-ink-2";
    case "landing":
      return "text-warning";
    case "landed":
      return "text-good";
    case "failed":
      return "text-serious";
    case "abandoned":
      return "text-ink-3";
  }
}

export function stateGlyph(state: ChangesetState): string {
  switch (state) {
    case "open":
      return "○";
    case "landing":
      return "◌";
    case "landed":
      return "●";
    case "failed":
      return "✕";
    case "abandoned":
      return "⊘";
  }
}

/// What a `POST …/land` refusal says, as the panel prints it.
///
/// The server's body is `{ error, gate, waiting_on }`. `error` is the
/// composed explanation and is printed as it is; `waiting_on` is the
/// list of members and checks still to report, one line each, because a
/// `waiting` refusal is an instruction — "come back when these have" —
/// and a list is what somebody can tick off.
export function landRefusal(body: unknown): {
  message: string;
  gate: ChangesetGate | null;
  waitingOn: string[];
} {
  const b = (body ?? {}) as {
    error?: unknown;
    gate?: unknown;
    waiting_on?: unknown;
  };
  const gate =
    b.gate === "ready" || b.gate === "waiting" || b.gate === "blocked"
      ? b.gate
      : null;
  const waitingOn = Array.isArray(b.waiting_on)
    ? b.waiting_on.filter((w): w is string => typeof w === "string")
    : [];
  return {
    message:
      typeof b.error === "string" ? b.error : "the changeset cannot land",
    gate,
    waitingOn,
  };
}

/// The default key for a revert, the way the server names the default
/// title: `revert-<key>`, cut to fit.
export function revertKey(key: string): string {
  return `revert-${key}`.slice(0, MAX_KEY_LEN).replace(/[-._]+$/g, "");
}

// ---------------------------------------------------------------------
// The landing order: waves, and the refusals an edge can earn

/// One declared dependency: `from` lands before `to`. This is the wire
/// shape of `PUT …/changesets/:key/edges` and the shape the editor
/// edits, because an edge-shaped form cannot express anything the
/// server has no word for.
export interface ChangesetEdge {
  from: ChangesetRef;
  to: ChangesetRef;
}

/// `repo/change`, the way every sentence on these screens names a
/// member. A changeset holds at most one change per repository, so this
/// identifies a member as exactly as the server's change id does.
export function refLabel(r: ChangesetRef): string {
  return `${r.repo}/${r.change}`;
}

/// The edges of a changeset that name two members this page has.
///
/// The wire type allows either end to be null — an id the server could
/// not resolve to a member — and an edge with a missing end can neither
/// be drawn nor edited. They are dropped here and counted by the caller,
/// which says so rather than rendering the bare "?" the old one-line
/// order summary did.
export function declaredEdges(
  edges: { from: ChangesetRef | null; to: ChangesetRef | null }[],
): ChangesetEdge[] {
  return edges.filter((e): e is ChangesetEdge => !!e.from && !!e.to);
}

/// The members no ordering could place: a cycle, and everything
/// downstream of one. Mirrors the `Err` arm of `toposort` in
/// `crates/stratum-control/src/changesets.rs` — the ids left standing
/// when nothing was free to go — and answers them in member order,
/// which is the order the server's own message lists them in.
function stuck(ids: string[], edges: [string, string][]): string[] {
  const indegree = new Map<string, number>(ids.map((id) => [id, 0]));
  for (const [, to] of edges) indegree.set(to, (indegree.get(to) ?? 0) + 1);
  const placed = new Set<string>();
  for (;;) {
    const next = ids.find((id) => !placed.has(id) && indegree.get(id) === 0);
    if (next === undefined) break;
    placed.add(next);
    for (const [from, to] of edges)
      if (from === next) indegree.set(to, (indegree.get(to) ?? 0) - 1);
  }
  return ids.filter((id) => !placed.has(id));
}

/// Why the server would refuse this set of edges, in the server's own
/// sentence, or null when it would take them.
///
/// A mirror of `check_edges`
/// (`crates/stratum-control/src/changesets.rs:236`), in the same order —
/// an endpoint that is not a member, then an edge onto itself, then the
/// cycle the whole set forms — with the strings its unit tests pin
/// (`crates/stratum-control/src/changesets.rs:1384`-`1405`). The mirror
/// exists so the editor can refuse before the round trip; it may never
/// *disagree* with the server, so a set this accepts and the server
/// refuses is a bug here, not there.
///
/// Note what is deliberately not a refusal: the server deduplicates a
/// repeated edge and takes the set, so this does too. Refusing a
/// duplicate here would be a disagreement.
export function edgeError(
  members: ChangesetRef[],
  edges: ChangesetEdge[],
): string | null {
  const ids = members.map(refLabel);
  const seen = new Set<string>();
  const kept: [string, string][] = [];
  for (const e of edges) {
    for (const end of [e.from, e.to]) {
      if (!ids.includes(refLabel(end)))
        return `${refLabel(end)} is not a member of this changeset`;
    }
    const from = refLabel(e.from);
    const to = refLabel(e.to);
    if (from === to) return `${from} cannot land before itself`;
    const pair = `${from} ${to}`;
    if (!seen.has(pair)) {
      seen.add(pair);
      kept.push([from, to]);
    }
  }
  const left = stuck(ids, kept);
  return left.length > 0
    ? `edges form a cycle through ${left.join(", ")}`
    : null;
}

/// The landing order as waves: one array per topological rank, members
/// within a wave in the order they were given.
///
/// The rank is the longest path to a member, not the shortest, so a
/// member never sits in a column beside something it lands after. That
/// is the same relation the server's toposort walks; the difference is
/// only that the server flattens ties into one list, and ties are what
/// a wave is.
///
/// Two kinds of edge are ignored rather than refused: one naming a
/// member this changeset does not have, and one inside a cycle. Both are
/// what `edgeError` is for, and a draft mid-edit is momentarily both —
/// the strip has to keep drawing something while the form says what is
/// wrong. Members no rank could place are appended as a final wave, so
/// nothing is dropped from the picture.
export function ranks(
  order: ChangesetRef[],
  edges: ChangesetEdge[],
): ChangesetRef[][] {
  const ids = order.map(refLabel);
  const known = (r: ChangesetRef) => ids.includes(refLabel(r));
  const pairs: [string, string][] = [];
  const seen = new Set<string>();
  for (const e of edges) {
    if (!known(e.from) || !known(e.to)) continue;
    const pair: [string, string] = [refLabel(e.from), refLabel(e.to)];
    const k = pair.join(" ");
    if (seen.has(k)) continue;
    seen.add(k);
    pairs.push(pair);
  }
  const unplaceable = new Set(stuck(ids, pairs));
  const rank = new Map<string, number>();
  for (const id of ids) if (!unplaceable.has(id)) rank.set(id, 0);
  // Relax until nothing moves. Each pass raises at least one member of
  // an acyclic set, so this settles in at most `ids.length` passes.
  for (let pass = 0; pass < ids.length; pass += 1) {
    let moved = false;
    for (const [from, to] of pairs) {
      const a = rank.get(from);
      const b = rank.get(to);
      if (a === undefined || b === undefined) continue;
      if (b < a + 1) {
        rank.set(to, a + 1);
        moved = true;
      }
    }
    if (!moved) break;
  }
  const depth = Math.max(0, ...[...rank.values()].map((r) => r + 1));
  const waves: ChangesetRef[][] = [];
  for (let i = 0; i < depth; i += 1)
    waves.push(order.filter((r) => rank.get(refLabel(r)) === i));
  const left = order.filter((r) => unplaceable.has(refLabel(r)));
  if (left.length > 0) waves.push(left);
  return waves;
}

/// The predecessors a chip has to name, or none when its column already
/// says it: a member of the first wave lands first, and a member whose
/// predecessors are exactly the whole wave before it lands after that
/// wave, which is what the column beside it means.
///
/// This is the whole reason the strip draws no edge splines: the case
/// where the picture is not enough is a sentence, and a sentence has an
/// accessible name a test can assert on.
export function afterLabels(
  ref: ChangesetRef,
  edges: ChangesetEdge[],
  waves: ChangesetRef[][],
): string[] {
  const at = waves.findIndex((w) =>
    w.some((r) => refLabel(r) === refLabel(ref)),
  );
  if (at <= 0) return [];
  const members = waves.flat().map(refLabel);
  const preds = [
    ...new Set(
      edges
        .filter((e) => refLabel(e.to) === refLabel(ref))
        .map((e) => refLabel(e.from))
        .filter((id) => members.includes(id)),
    ),
  ];
  if (preds.length === 0) return [];
  const previous = waves[at - 1].map(refLabel);
  const sameAsPrevious =
    preds.length === previous.length &&
    previous.every((p) => preds.includes(p));
  if (sameAsPrevious) return [];
  return members.filter((id) => preds.includes(id));
}

// ---------------------------------------------------------------------
// The combined cross-repo diff: progress across the whole set, and the
// filter that narrows it

/// One member's slice of the combined diff, as the section holds it.
///
/// `files` is deliberately three-valued through `null`: a member whose
/// change has no patchset yet and a member whose file list has not
/// landed both have nothing to count, and both have a different
/// sentence to say. `error` is separate again, because a repository this
/// reader cannot see must degrade to *that group* saying so — one
/// unreadable member is not a broken page, and the fan-out that fills
/// this is `Promise.allSettled` for exactly that reason.
export interface MemberDiff {
  repo: string;
  /// The member change's key, which is what a viewed mark is written
  /// against. Carried per member because the whole hazard of this
  /// surface is sending one member's tick to another.
  change: string;
  /// The patchset number on screen, or null when there is none yet.
  patchset: number | null;
  files: DiffEntry[] | null;
  /// Paths this reader has already read, as the server reports them for
  /// *this member's* latest patchset.
  viewed: ReadonlySet<string>;
  error: string | null;
}

/// How far through one member's files the reader is.
///
/// `complete` is what auto-collapses the group, and it is false for a
/// member with no files at all: a group that has nothing to read has not
/// been read, and collapsing it would hide the sentence saying why.
export function memberDiffProgress(m: MemberDiff): {
  total: number;
  seen: number;
  complete: boolean;
} {
  const files = m.files ?? [];
  const seen = files.filter((f) => m.viewed.has(f.path)).length;
  return {
    total: files.length,
    seen,
    complete: files.length > 0 && seen === files.length,
  };
}

/// `N files · X of Y viewed` for the whole set.
///
/// Summed over the members it is given, which is always the *filtered*
/// list and never the list left after auto-collapse has hidden the
/// finished groups. A progress number that shrank as you made progress
/// would be worse than none — the same rule `FilesPanel` keeps for one
/// change, for the same reason.
export function setProgress(members: MemberDiff[]): {
  files: number;
  seen: number;
} {
  let files = 0;
  let seen = 0;
  for (const m of members) {
    const p = memberDiffProgress(m);
    files += p.total;
    seen += p.seen;
  }
  return { files, seen };
}

/// Narrow every member's files by a needle matched against
/// `repo/path`, so typing `api/` narrows to a repository and `pay.rs`
/// narrows to a file wherever it lives. Case-insensitive, and the empty
/// needle is the whole set.
///
/// A member with nothing loaded — no patchset, still fetching, or
/// refused — survives any needle: those groups are carrying a sentence
/// rather than a file list, and a filter that swallowed the one saying
/// "this repository could not be read" would turn a refusal into an
/// absence. A member whose files all failed to match is dropped, which
/// is what shortens the rail.
export function filterAcross(
  members: MemberDiff[],
  needle: string,
): MemberDiff[] {
  const q = needle.trim().toLowerCase();
  if (!q) return members;
  const out: MemberDiff[] = [];
  for (const m of members) {
    if (m.files === null) {
      out.push(m);
      continue;
    }
    const files = m.files.filter((f) =>
      `${m.repo}/${f.path}`.toLowerCase().includes(q),
    );
    if (files.length > 0) out.push({ ...m, files });
  }
  return out;
}
