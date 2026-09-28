# Polyrepo changesets — P0 design

A *changeset* is one review, one verdict and one landing across several
repositories in one organization. This is the P0 cut of the research
document (v0.1) translated onto the subsystems this codebase actually has;
where the document assumed something that does not exist, the row below
says what P0 does instead.

## What P0 delivers

| Research doc | P0 here |
|---|---|
| Changeset object spanning N repos | `changesets` row + `changeset_members` (one existing `change` per repo, ≤ 16 members) with author-declared `depends_on` edges between members |
| Unified review with per-repo sufficiency | one view; `review::sufficiency::evaluate` runs per member and the changeset is landable only when every member is |
| Atomic cross-repo landing | the "honest middle" protocol below |
| Changeset revert | a new changeset whose members are reverts of each landed member's patchset, created by one call (`POST …/changesets/{key}/revert`); path-granular, refused whole if any path has changed since |
| Composed changeset CI | a workflow with `on: changeset` runs once per changeset patchset with every member repo materialised under `$WEFT_WORKSPACE/<repo>/`; per-repo `on: change` runs still happen |
| Org dependency graph | there is none. Edges are declared by the author on the changeset, and defaulted from `depends_on` in each repo's `stratum.toml` |
| Workspace read view | a read-only page listing every member at its proposed head, plus a composed clone URL that checks out all members at those heads |

Deferred to P1/P2 exactly as in the doc: speculative landing, stacked
changesets, campaigns, cross-org changesets, facets.

## Data model

```
changesets            id, org_id, key (hex, client-supplied like change_key),
                      title, body, author_person_id, state
                      state ∈ open | landing | landed | abandoned | failed
changeset_members     changeset_id, change_id (unique across all changesets
                      while the changeset is open), position
changeset_edges       changeset_id, from_change_id, to_change_id   (from lands first)
changeset_landings    changeset_id, started_at, finished_at, attempt,
                      per-member progress as JSONB [{change_id, ref, old, new, done}]
```

A change can belong to at most one open changeset; a landed change to any
number (revert changesets reference landed members).

## The landing protocol ("honest middle")

Each repo's refs live in its own `manifest.json` in object storage and
change only by compare-and-swap (`refops::transact`). There is no
transaction that spans two manifests, and P0 does not invent one. What it
does guarantee, and what the UI says:

> **All-or-nothing, never half-abandoned, sub-second apply window.**

1. **Pre-flight.** Every member's `LandGate` must be green at the same
   instant: approvals sufficient, required checks passing, patchset head
   unchanged, target ref unchanged since CI ran. Any red member fails the
   whole landing before anything is written.
2. **Commit point.** One row in `changeset_landings` is written with the
   full plan — for each member, the ref, the expected old oid and the new
   oid — and `changesets.state` flips to `landing` in the same
   transaction. From this row on, the landing *will* finish: either every
   member lands or every already-landed member is reverted.
3. **Apply.** Members are CASed in topological order of the declared
   edges, one manifest at a time, each attempt recorded in the plan row.
   A CAS conflict (someone pushed to the target in the window) re-reads
   the manifest and retries `CAS_RETRIES` times as `lander.rs` does today;
   if the old oid no longer matches, the landing is failed.
4. **Finish or unwind.** On success every member `change` is `set_landed`
   and the changeset is `landed`. On failure after k members have landed,
   the lander writes revert commits for those k members in reverse order,
   CASes them, and marks the changeset `failed` with the reason visible
   per member. The unwind restores the *whole tree* the trunk had at
   `old`, because it runs inside the landing against a trunk it expects
   still to be at `new`. The *revert changeset* (slice 4) cannot: it runs
   later, on a trunk other work has landed on, so it puts back only the
   paths the member changed — on top of the trunk as it is now — and is
   refused whole when any of those paths has changed since. Both write
   through `commits::build_tree` and `refops::transact`.
5. **Reaper.** A `changeset_landings` row whose `finished_at` is null and
   whose `started_at` is older than `STRATUM_LAND_WAIT_SECS` is picked up
   by the existing stranded-landing sweep and driven to step 4 from the
   recorded progress: members marked `done` are trusted, others are
   re-checked against the manifest (the CAS may have succeeded after the
   crash), and the outcome is decided from what is actually in the store.

The window between the first and last CAS is a few round-trips to object
storage; a reader cloning both repos inside that window can see one landed
and the other not. That is the "honest" part, and the workspace view says
so on a landing changeset (its `note` field, non-null only in `landing`).

## Composed CI

**Delivered (slice 5), with one deliberate deviation from what follows.**
A composed job is declared with `on: changeset` in the workflow file
itself — `.weft/ci.yml`'s `on: [change, changeset]` — and **not** in
`stratum.toml`. Nothing in the product reads `stratum.toml`, and the
workflow file's `on:` already decides when a workflow runs; a second
place to say it would be a second place to get it wrong. The rest of
this section is as shipped, with `$WORKSPACE` spelled
`$WEFT_WORKSPACE`.

~~`stratum.toml` in a repo may declare
`[workflow] on = ["change", "changeset"]`.~~ For a changeset patchset the
dispatcher materialises every member at its proposed head under
`$WEFT_WORKSPACE/<repo-name>/` and runs each member's `on: changeset` workflow
with `WEFT_CHANGESET`, `WEFT_CHANGESET_MEMBERS` (JSON) in the
environment. Verdicts land on the changeset (`changeset_checks`), and the
composed check is required for the changeset land gate whenever any
member repo declares one. Per-repo `on: change` runs continue unchanged
and remain required for each member.

Composed jobs run on the organization's own runners, like every other
job; nothing is metered.

## Slices, in order

1. Model + migration + `changesets.rs` (create, add/remove member, edges,
   topological sort, get, list) with the API and OpenAPI.
2. Unified review view: per-member sufficiency, changeset-level verdict.
3. Landing protocol + reaper + chaos sibling (SIGKILL between member
   CASes), deterministic tests for every branch of step 4 and 5.
4. Revert changeset.
5. Composed CI: `on: changeset`, workspace materialisation in the runner,
   `changeset_checks`, land-gate integration. **Delivered** — declared in
   the workflow file's `on:`, not in `stratum.toml`; see the deviation
   noted under *Composed CI* above.
6. Workspace view + composed clone URL, ACL-filtered. **Delivered** —
   `GET …/changesets/{key}/workspace`, and a read-only git repository at
   `/{org}/changesets/{key}.git` (HTTP and SSH) served from memory: one
   commit whose tree is a gitlink per member at its proposed head, with
   `.gitmodules` using relative URLs so `git clone --recurse-submodules`
   carries the transport and credential across. The member set and the
   composition hash are `trigger::compose`'s, so the tip and the composed
   runs name the same thing. The repository name `changesets` is reserved.
7. Dashboard: create from N open changes, review, land, revert;
   walkthrough stages; Playwright. **Delivered** — a Changesets entry in
   the sidebar, a picker over the org-wide open-change list
   (`GET /v1/orgs/{org}/changes`, added for it, each row carrying the
   `changeset` that holds it so the three compose refusals are greyed
   out before submit), and a detail page that reads the verdict,
   members in landing order, composed checks and workspace, then lands,
   reverts and abandons through the same routes the CLI would. The
   single-change read grew `changeset` too, so the change view says
   "lands with `<key>`" and turns its own Land and Abandon off instead
   of learning the binding from the 409. The walkthrough's
   `changesets /` stages compose two seeded changes, clone the workspace
   with `--recurse-submodules` under a read token and fsck it, approve
   the governed member on its own page, land, read both trunks back,
   revert, and abandon the revert.

Each slice ships with its own tests, ledger remap, OpenAPI and docs.
