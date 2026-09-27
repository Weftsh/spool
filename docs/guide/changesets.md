---
layout: ../../layouts/Docs.astro
title: Changesets across repositories
description: One review unit made of changes in several repositories — composed, ordered, and landed together, all or nothing.
---

# Changesets across repositories

A **changeset** is one review unit made of changes that live in
different repositories of the same organization: the API that grows a
field, the web app that reads it, the CLI that prints it. Each of those
is still an ordinary [change](/docs/code-review/) in its own repository,
with its own patchsets and its own approvals. The changeset is the thing
that says *these belong together*, in what order they land, and — once
one is a member — that none of them lands or is abandoned on its own.

This page covers what is on the wire: composing a changeset, shaping
its landing order, the rules a member is held to, the verdict over all
of it, landing it as one unit, the [composed CI](#composed-ci) that
tests every member together, and the [workspace](#the-workspace) that
checks every member out at its proposed head with one `git clone`.

## What a changeset is

- **Members** are open changes, at most one per repository and at most
  sixteen in all. Sixteen is a ceiling on coordination, not storage: a
  unit that touches more repositories than that is not one change.
- A change is a member of **at most one open changeset**. Trying to add
  it to a second is refused by name until the first lands or is
  abandoned.
- **Edges** say which member lands before which: `{ from, to }` means
  `from` lands first. Edges are optional, must not form a cycle, and are
  replaced as a whole when you set them — acyclicity is a property of
  the set, so the set is what you send.
- The **landing order** is derived from the edges, ties broken by the
  order members were added. Every response carries it as `order`, so
  what a reviewer sees is exactly what the lander will do.
- A changeset is `open`, then `landing`, `landed`, `abandoned` or
  `failed`. Only an open one can be shaped.

## Composing one

Register a change in each repository first, as you would alone. Then:

```
POST /v1/orgs/{org}/changesets
{
  "key": "Ic5000001",
  "title": "Rename the customer field",
  "body": "web first, then api",
  "members": [
    { "repo": "api", "change": "Iaa000001" },
    { "repo": "web", "change": "Ibb000002" }
  ],
  "edges": [
    { "from": { "repo": "web", "change": "Ibb000002" },
      "to":   { "repo": "api", "change": "Iaa000001" } }
  ]
}
```

The key is yours to choose — same alphabet as a Change-Id, unique
within the org — so a tool that composes changesets can name them
deterministically and re-post safely: the second attempt is a `409` that
says the key exists, not a duplicate.

`201` returns the changeset: its members with each change as the change
API shows it, its edges, and `order`. Every refusal is decided before
anything is written, and each names the member it is about:

| Status | Why |
|---|---|
| `400` | bad key or title; no members, or more than sixteen; the same change listed twice; two members from one repository; an edge that names a non-member, a change before itself, or a cycle |
| `404` | `no change web/Inope0001` — a member that is not a change, or one the caller may not write to |
| `409` | the key exists; a member is not open; `api/Iaa000001 is already in changeset Ic5000001` |
| `402` | the organization is read-only and a member is in a private repository |

Finding the changes to compose is a request of its own:

```
GET /v1/orgs/{org}/changes?state=open
```

It answers every change in the organization you may read, newest first,
each row naming its `repo` and the `changeset` already holding it — or
`null` when the change is free. That `changeset` field is the same
question the `409` above answers, so a picker can grey out exactly the
changes a compose would refuse. This is what the dashboard's changeset
picker reads; asking each repository's own change list in turn is a round
trip per repository before anyone can see what there is to compose.

Authority is per repository, not org-wide: a `repo:read` token sees its
own repository's changes here, a public repository's are readable with no
credential at all, and a private one's are not — the same answers
`GET /v1/orgs/{org}/repos/{repo}/changes` gives one repository at a time.

## Shaping an open changeset

```
POST   /v1/orgs/{org}/changesets/{key}/members          { "repo": "cli", "change": "Icc000003" }
DELETE /v1/orgs/{org}/changesets/{key}/members/cli/Icc000003
PUT    /v1/orgs/{org}/changesets/{key}/edges            { "edges": [ … ] }
POST   /v1/orgs/{org}/changesets/{key}/abandon
```

A new member joins at the end of the order with no edges. Removing a
member removes every edge that touched it. The last member cannot be
removed — a changeset with nothing in it is not a thing — abandon the
changeset instead. Abandoning releases every member: each change is open
again in its own repository and may land, be abandoned, or join another
changeset. The record stays, closed, for the audit trail.

## What a member may no longer do alone

While a change is a member of an open changeset, landing it or
abandoning it through its own repository is refused with `409` and the
changeset's key:

```
this change is a member of changeset Ic5000001 — it lands with the
changeset; remove it from the changeset to land it alone
```

That is the whole point of the unit: nobody lands the API rename on a
Friday and leaves the web app reading a field that is gone.

The change itself says so before you try. `GET
/v1/orgs/{org}/repos/{repo}/changes/{change}` answers `change.changeset`:
the key of the changeset holding it, or `null` when it is free to land
alone. It is read from the same binding the `409` above is refused on, so
a client can show "lands with `Ic5000001`" up front rather than offering
a Land button and discovering the truth from the refusal.

## Who can see and shape one

Authority is per member, and comes from the repositories:

- **Reading** a changeset needs read on **every** member. If any member
  is in a repository you cannot read, the changeset does not exist for
  you — `404`, the same answer whether or not it exists — and it is left
  out of the list. A changeset over public repositories reads
  anonymously, like the repositories do.
- **Composing and shaping** need write on every member, the one being
  added included. So do landing, reverting and abandoning. Every
  changeset you read says whether you hold that — `"viewer_write": true`
  or `false` — so a client can withhold the controls rather than offer
  them and be answered with the masked `404` a stranger gets; each row of
  `GET /v1/orgs/{org}/changes` says the same about its own repository, for
  the picker.
- A credential from **another organization** reads a changeset over
  public repositories the way anyone does — anonymously, as it reads the
  repositories themselves — and may shape nothing.
- A [read-only organization](/docs/billing/) — no card yet, or a
  subscription that has ended — can still read its changesets and shape
  those over public repositories; anything touching a private repository
  answers `402` with the sentence that says what to do.

Every composition, membership change, edge change and abandonment is in
the org's [audit trail](/docs/audit-and-undo/) as `changeset.create`,
`changeset.member.add`, `changeset.member.remove`, `changeset.edges` and
`changeset.abandon`.

## Reading

```
GET /v1/orgs/{org}/changesets?state=open&limit=50
GET /v1/orgs/{org}/changesets/{key}
```

The list is newest first and shows only changesets whose every member
you may read. `state` is one of `open`, `landing`, `landed`, `abandoned`,
`failed`; anything else is `400`.

## How big it is

```
GET /v1/orgs/{org}/changesets/{key}/diffstat
```

The `+412 −77` a reader wants before they open anything, counted from the
same tree diff `…/diff` reports path by path — per member, and in total:

```json
{
  "changeset": "Ic5000001",
  "total": { "files": 9, "insertions": 412, "deletions": 77, "truncated": false },
  "members": [
    { "repo": "api", "change": "Iaa000001", "patchset": 2,
      "files": 3, "insertions": 41, "deletions": 12, "truncated": false },
    { "repo": "client", "change": "Ibb000002", "patchset": 1,
      "files": 6, "insertions": 371, "deletions": 65, "truncated": false }
  ]
}
```

Per member and not only in total, because "one review over several
repositories" is the thing a changeset *is*: a set that is +12 in the API
and +900 in the generated client is a different review from one that is
+450 in each, and a single number says the same thing about both. Members
are in landing order, and each member's numbers are its **latest
patchset** against its parent commit — the same range the change page
diffs.

**`truncated` is honesty, not an error.** Some files have no line count
this server is willing to claim: over 512 KiB (the same limit the diff
view refuses to fetch as text, so it is a file you would never be shown a
diff of anyway), binary, a submodule pointer — which is a commit oid
rather than content — or a rewrite so total that matching it up would run
past the request's work bound. Those files are left out of `insertions`
and `deletions` and the member says so. `files` stays exact either way,
because it comes from the tree walk, which never declines a path. A
number invented for a file the server declined to read would be worse
than an absent one: nothing about it would look wrong.

On `total`, `truncated` means *some* member's is — the only reading of it
that cannot overstate what was counted.

Reading it needs read on **every** member, like the changeset itself; a
member you cannot see makes the whole thing `404`, because the size of a
review over a private repository is a fact about that repository.

## The workspace

```
GET /v1/orgs/{org}/changesets/{key}/workspace
```

A changeset is several changes in several repositories, and the question
a reviewer keeps asking is *what does it all look like together*. The
workspace answers it with one git repository, served for the changeset,
that you can clone:

```sh
git clone --recurse-submodules https://weft.example/acme/changesets/Ic5000001.git
```

That checkout has one directory per member repository, named after it,
with the member's proposed head checked out inside — the `api` change's
latest patchset under `api/`, the `web` change's under `web/`. The
superproject is one commit on a branch called `workspace`; its tree holds
one submodule per member pinned to that member's commit, and its
`.gitmodules` names each member by a **relative** URL (`../../api.git`),
so the members are fetched over whatever transport and credential you
cloned the workspace with — HTTPS with a token, or SSH with your key,
from the same `ssh_clone_url` shape the changeset reports.

The response says the same thing in JSON:

```json
{
  "key": "Ic5000001",
  "title": "Add the field end to end",
  "state": "open",
  "composition": "3f9c2b…",
  "tip": "8ad14e…",
  "clone_url": "https://weft.example/acme/changesets/Ic5000001.git",
  "ssh_clone_url": "ssh://git@weft.example/acme/changesets/Ic5000001.git",
  "members": [
    {
      "repo": "api", "change": "Iaa000001", "title": "Add the field",
      "path": "api", "commit": "e0d37a…", "fetch_ref": "refs/patchsets/e0d37a…",
      "clone_url": "https://weft.example/acme/api.git",
      "ssh_clone_url": "ssh://git@weft.example/acme/api.git"
    }
  ],
  "note": null
}
```

- `composition` is the hash the changeset's
  [composed CI](#composed-ci) runs are named by, and `tip` is the
  workspace commit. Both move together — a new patchset on any member, or
  a member added or removed, is a new composition and a new tip — and a
  workspace at a given `tip` is exactly the combination a given composed
  run tested. Each tip is a fresh root commit with no parent: the history
  is in the members, not in the workspace, so the same changeset clones
  to the same commit from any node and at any time. That also means
  `git pull` will not follow it — there is nothing to merge. To move a
  checkout to the current tip:

  ```sh
  git fetch origin && git reset --hard origin/workspace && git submodule update --init
  ```
- `fetch_ref` is where each member's commit lives in its own repository,
  `refs/patchsets/<sha>`, so it can be fetched even when the branch it
  was pushed from has moved on or been deleted; the workspace's submodule
  pins reach it the same way.
- The workspace is **read-only**. A push to `clone_url` is refused on
  the wire with the reason; changes go to the member repositories, as
  patchsets of the member changes.
- **Who may clone it** is who may read every member — the rule for the
  changeset itself. A member you cannot read makes the whole workspace
  `404`, and anonymous clones work only when every member repository is
  public. The `.gitmodules` URLs carry nothing you did not already hold.
- `note` is `null` except while the changeset is `landing`, when it warns
  that some members may already be on their trunks while others are not,
  and that the workspace is the proposed state, not the trunks. That
  window is a few round-trips to object storage, and the view says so
  rather than pretending it is zero.
- A member whose repository has since been deleted is not in the tree, and
  a changeset with no member left has `composition` and `tip` `null` and
  an empty `members` list; its clone URL advertises no refs.

The repository name `changesets` is reserved in every organization,
because `/{org}/changesets/{key}.git` is where its workspaces are served
from.

## The verdict: one review at a time

```
GET /v1/orgs/{org}/changesets/{key}/verdict
```

A changeset is one review, so it has one answer to "can this land?" —
and because a member is still a change in its own repository, that
answer is composed from the answers each member already has. Nothing is
re-decided here: every member's `verdict` is exactly what
`GET …/changes/{change}/verdict` gives for it — the
[OWNERS](/docs/code-review/) sufficiency at its latest patchset — and
its `gate` is exactly what the change's own Land button consults, the
[required checks](/docs/ci-integration/#making-a-check-required) on its target branch.

```json
{
  "changeset": "Ic5000001",
  "state": "open",
  "landable": false,
  "gate": "waiting",
  "explanation": "api/Iaa000001: feature.txt: needs approval from oa@acme.test",
  "waiting_on": ["web/Ibb000002: ci/tests"],
  "members": [
    {
      "repo": "api", "change": "Iaa000001", "state": "open",
      "patchset": 1, "commit": "…",
      "landable": false,
      "explanation": "feature.txt: needs approval from oa@acme.test",
      "gate": "ready", "waiting_on": [], "reason": null,
      "verdict": { "landable": false, "explanation": "…", "per_path": [ … ] },
      "approvals": []
    },
    { "repo": "web", "change": "Ibb000002", "…": "…",
      "landable": true, "gate": "waiting", "waiting_on": ["ci/tests"] }
  ]
}
```

Members are in **landing order**. `landable` is true when every member
is: its change is open, its verdict says approved, and no required check
has failed. When it is not, `explanation` is the first member's that
stands in the way, with `repo/change` in front — so the reader of one
review over four repositories is told which repository to go to. Within
a member the order is the order you would fix things in: a change that
is not open cannot be helped by approving it, and an unapproved one
cannot be helped by a green build.

`gate` keeps the three answers the per-change gate has. `waiting` is not
a *review* refusal — a changeset whose members are all approved and
waiting on CI has nothing left for a person to do — and `waiting_on`
lists the checks as `repo/change: check`; but it is a reason not to land
yet, and [landing](#landing-a-changeset) refuses it until they have
reported. `blocked` is different in kind: something has already said no,
and waiting cannot rescue it. `reason` on the member carries the check's
own words.

Approvals stay where they are given — on each member, by someone the
repository's OWNERS names — and each member's row lists them. A viewer
who may read every member may read the verdict; anyone else is told there
is no such changeset.

## Landing a changeset

```
POST /v1/orgs/acme/changesets/Ic5000001/land
```

Landing is the promise the unit exists for: **every member lands, or
none is left landed.** There is no transaction across repositories — each
repository's refs live in its own manifest and change only by
compare-and-swap — so the promise is kept by a protocol rather than by
the store, and this is what it does.

**Pre-flight, which writes nothing.** Every member must be open, approved
per its OWNERS at its latest patchset, its required checks **passing**,
and its patchset a fast-forward of its target branch. Anything short of
that is a `409` naming the first member in landing order that stands in
the way:

```json
{ "error": "web/Ibb000002: blocked: required check 'ci/tests' is failing",
  "gate": "blocked", "waiting_on": [] }
```

`gate` is what stands in the way — `blocked` for anything a person has
to act on, including a member nobody has approved yet — and is never
`ready` in a refusal. A required check that has not reported yet is a
refusal here, with `"gate": "waiting"` and the checks in `waiting_on`. That is the one place
a changeset is stricter than a change on its own: a single change is held
in the queue and lands unattended when its check reports, but the plan a
changeset lands from is made against the trunks *as they stand at
pre-flight*, and "the trunk has not moved since CI ran" is one of the
things being promised. Land again when the checks have reported. A member
whose trunk has moved since its patchset was made is
`repo/change: not fast-forward from <tip>`; push a new patchset on top of
the trunk and land again.

**The commit point.** When every member is green, one landing record is
written with the full plan — for each member its target ref, the tip it
was judged against (`old`) and the commit it lands (`new`) — and the
changeset turns `landing` in the same transaction. The response is `202`
with that plan:

```json
{ "queued": true, "job": "…", "changeset": "Ic5000001", "landing": "…",
  "plan": [
    { "repo": "api", "change": "Iaa000001", "ref": "refs/heads/main",
      "old": "9f6de998…", "new": "9e54f5f2…" },
    { "repo": "web", "change": "Ibb000002", "ref": "refs/heads/main",
      "old": "80b5f804…", "new": "0b8105e4…" }
  ] }
```

From this row on the landing *will* finish, on this node or any other.
While it lands, the changeset cannot be reshaped, abandoned or landed
again (`409 changeset is landing`), and a member cannot land or be
abandoned on its own.

**Apply.** The lander walks the plan in landing order, one repository at
a time, moving each target ref from `old` to `new` by compare-and-swap.
The window between the first and last swap is a few round-trips to
object storage; a reader cloning both repositories inside it can see one
landed and the other not. That is the honest part of the promise, and
the changeset says `landing` while it is true.

**Finish, or unwind.** When every step is done, every member change is
`landed` at its commit with the verdict `landed with changeset
Ic5000001`, and the changeset is `landed`. If a trunk moved between the
plan and its turn — somebody pushed to `web`'s `main` in the window — that
member's swap fails, the landing fails, and every member that had already
landed is put back: a **revert commit** on top of the landed one,
restoring the tree the trunk had before, authored `weft-lander` and
saying why —

```
Revert api/Iaa000001: changeset Ic5000001 did not land

Restores refs/heads/main to the tree of 9f6de9988c33 because
web/Ibb000002 — refs/heads/main moved to 3c1d0a9e4b7f before
web/Ibb000002 could land.

Reverts commit 9e54f5f216c1…
```

— never a rewind, because the landed commit may already have been fetched
by somebody. The changeset is `failed`, and each member says what
happened to it: the reverted one is `open` again with
`landed, then reverted in <revert>: <what failed> — push a new patchset
to land again` (its patchset is no longer a fast-forward of the reverted
trunk, so a new one is needed); the one that could not land is `open`
with `ejected: refs/heads/main moved to <tip> before web/Ibb000002 could
land`; any member that never got its turn is `open` with `ejected: not
attempted, <what failed>`. A `failed` changeset is final, like an
abandoned one; its members are released and may be composed into a new
changeset.

If a landed member's trunk moved *again* before it could be reverted, it
is left landed rather than fought over, and its note says
`not reverted: refs/heads/main moved to <tip> after it landed`. That is
the one outcome where a person has to look, and it takes two writers
racing the same trunk inside one landing to produce it.

**Reading the progress.** `GET …/changesets/Ic5000001` carries `landing`
— null until the changeset has been asked to land, then the plan with
each member's progress:

```json
"landing": {
  "id": "…", "attempt": 1, "started_at": 1788395527022,
  "finished_at": 1788395528901, "outcome": "failed",
  "members": [
    { "repo": "api", "change": "Iaa000001", "ref": "refs/heads/main",
      "old": "9f6de998…", "new": "9e54f5f2…",
      "state": "reverted", "note": "b2d4e6f8…" },
    { "repo": "web", "change": "Ibb000002", "ref": "refs/heads/main",
      "old": "80b5f804…", "new": "0b8105e4…",
      "state": "failed",
      "note": "refs/heads/main moved to 3c1d0a9e4b7f before web/Ibb000002 could land" }
  ] }
```

`state` is `pending`, `done`, `failed` or `reverted`; `note` says why a
step failed, names the revert commit of a reverted one, or says why a
landed member could not be reverted. `outcome` is null while landing,
then `landed` or `failed`.

**If the node dies mid-landing.** The landing record, not the process,
is what promises to finish. A node that dies between two swaps leaves a
job whose lease lapses (`STRATUM_LAND_LEASE_SECS`, two minutes) and is
claimed again; a landing whose job has failed outright is picked up by
the reaper after `STRATUM_LAND_RECHECK_SECS` (twenty seconds) under a
fresh job, and `attempt` counts them. The count is bounded: at
`STRATUM_JOB_MAX_ATTEMPTS` failed drivers (five) the landing is given
up rather than rescued again. If the plan has not yet failed, the member
the drivers died on is `failed` with `the landing was given up after
<n> attempts; the last failed with: <error>` and one more driver unwinds
what landed, so the outcome reads like any other failure. If the drivers
were dying in the *unwind* itself, the landing is closed as it stands:
`failed`, with every landed member left landed and its note saying
`not reverted: the landing was given up after <n> attempts; …` — the
second outcome where a person has to look, and one that takes a store
refusing the same write five times running to produce. Either way the
new driver decides from the **store**, not the record: a trunk already at a member's `new`
is done — the swap landed and only the acknowledgement was lost — and
one still at `old` is swapped now. A trunk anywhere else is read through
its history, because by the time a second driver looks the world may
have moved on top of the first one's work: if `new` is in the tip's
history the member landed and somebody has since pushed over it, so it
is `done` with the note `refs/heads/main moved to <tip> after
api/Iaa000001 landed` and is never reverted over that push; if it is not,
the member is `failed` with `moved to <tip> before … could land` and the
landing unwinds as above. The same reading recognises a revert whose
acknowledgement was lost — the commit directly on the landed one, with
the landed one as its only parent and the pre-landing tree — as the
revert it is, rather than as a stranger's push. The walk is bounded
(4096 commits); a trunk that has taken more than that inside one landing
is `failed` with a note saying the question is beyond the bound, and a
person has to look. Nothing is landed twice and nothing is reverted that
did not land.

**A member aimed at a branch that does not exist yet.** A change may
target a branch its repository does not have; the plan records `old` as
`null`, the swap expects the ref to be absent, and landing creates the
branch. Reverting such a member has no tree to go back to but the empty
one: the revert commit restores the empty tree and its message says so
(`Restores refs/heads/release to the empty tree because …`).

## Reverting a landed changeset

```
POST /v1/orgs/acme/changesets/Ic5000001/revert
{ "key": "Ic5000002" }
```

A changeset that landed and should not have is undone the way it was
done: as one unit, across every repository, reviewed and landed through
the same protocol. One call makes a **revert changeset** — `Ic5000002`
here — with one member per repository the original landed in, and the
response is that changeset as `GET …/changesets/Ic5000002` would return
it, `open`, with `"reverts": "Ic5000001"`. The original is untouched and
says `"reverted_by": ["Ic5000002"]`.

Each member is an ordinary change. In its repository a commit is made on
a new branch `revert/Ic5000002`, off the target branch **as it is now**,
that puts back every path the landed member changed — a deleted file
returns, a rewritten one is its old self again at its old mode, an added
one is gone, and a directory emptied by that is gone with it — and
touches nothing else, so work that has landed since on other paths is
kept. The commit carries a `Change-Id` of its own and is registered
exactly as a push would register it: pinned, its CI triggered, its owners
notified.

```
Revert "change api"

Reverts api/Iaa000001, landed by changeset Ic5000001 as 9e54f5f216c1….

Change-Id: I4b7c…
```

The members are composed with the original's edges **reversed**: what
landed after its dependency is undone before it, so `web` is put back
before `api` when `api` landed first. Then it is a changeset like any
other. Nothing has moved yet — no trunk, and nothing about `Ic5000001` —
and nothing does until `Ic5000002` is reviewed under the same OWNERS
that governed the paths the first time and landed with
`POST …/changesets/Ic5000002/land`. A revert changeset can itself be
reverted, which is how a change is re-landed after a wrong revert.

`title` and `body` are optional and default to `Revert "<the original's
title>"` and `Reverts changeset Ic5000001.`.

**What is reverted.** The members the landing record says are landed:
every member of a `landed` changeset, and, of a `failed` one, the members
the unwind could not put back — those `done` with a `not reverted` note.
A `failed` changeset the unwind fully put back has nothing to revert and
says so (`409 nothing of changeset Ic5000001 is landed: every member that
landed was reverted`), and a changeset that never landed is refused
outright (`409 changeset is open: nothing of it has landed`).

**Everything is checked before anything is written.** A revert of three
members out of four would be a half-landed changeset by another name, so
any one member that cannot be reverted cleanly refuses the whole call,
and no branch is made in any repository. The refusal names the first such
member in landing order and lists every one of them:

```json
{ "error": "web/Ibb000002: refs/heads/main has changed since it landed at readme",
  "conflicts": [
    { "repo": "web", "change": "Ibb000002",
      "why": "refs/heads/main has changed since it landed at readme",
      "changed": ["readme"] }
  ] }
```

A path is *changed* when the trunk no longer has, at that path, exactly
what the landing left there — edited, deleted, or turned into a
directory. That is somebody's later work, and a revert that quietly undid
it would be a regression in the shape of a fix; put the path back by hand
(or revert their change first) and call again. The other refusals are a
target branch that has been deleted since the landing
(`refs/heads/release no longer exists`), a `revert/Ic5000002` branch
already in a repository (`revert/Ic5000002 already exists` — an earlier
call under this key got that far, or somebody made one), a member whose
repository has been deleted (`web/Ibb000002: the repository no longer
exists`), and a `key` that is not a valid changeset key (`400`) or is
already a changeset (`409 changeset Ic5000002 already exists`).

Making a revert changeset needs `repo:write` on every member's
repository, the same as composing one; anyone short of that is told there
is no such changeset.

## Composed CI

A member's own CI tests that member's repository. It cannot tell you
whether the API and the web app still agree, because it never has both.
A **composed run** does: one run, per member repository, with every
member checked out at the head the changeset proposes for it.

Declare it in the repository's [workflow file](/docs/workflows/), by
adding `changeset` to `on:`:

```yaml
name: contract
on: [change, changeset]
jobs:
  contract:
    steps:
      - name: Test against the sibling api
        run: |
          make test API_DIR="$WEFT_WORKSPACE/api"
```

Steps start in the job's own repository; the siblings are beside it under
`$WEFT_WORKSPACE`, one directory per repository name, and
`$WEFT_CHANGESET_MEMBERS` lists them as JSON with an absolute `path`
each. Each sibling is fetched with a read token minted for that one
repository, so a composed script cannot read organization repositories
the change's author cannot. The details are in
[Composed runs for a changeset](/docs/workflows/#composed-runs-for-a-changeset).

Only repositories whose `.weft/` asks for `changeset` get a run; a
changeset of four repositories where one declares a composed workflow has
one composed run. Per-repository `on: change` runs continue exactly as
before and still gate their own member.

**A fork member holds the whole composition.** If any member's change
comes from a fork and its workflows have not been approved yet, every
member's composed run is held — `blocked`, with the reason `fork` — and
not just the fork member's. A composed job runs in a maintainer's own
repository, but the stranger's tree is checked out beside it under
`$WEFT_WORKSPACE` and the maintainer's own script may execute it, so
holding only the fork member's run would still run a stranger's code on
every other member's behalf. Approving that change's workflows, with the
button on the change itself, releases the whole composition at once. The
per-repository `on: change` runs are gated exactly as they were.

### What the changeset reports

`GET …/changesets/{key}` carries two fields for this:

```json
{
  "composition": "3f9c2b1e…",
  "checks": [
    { "repo": "api", "name": "contract / contract", "state": "passing",
      "detail_url": "https://weft.example/acme/api/checks/runs/wr_01H…",
      "run": "wr_01H…" },
    { "repo": "web", "name": "contract / contract", "state": "running",
      "detail_url": "https://weft.example/acme/web/checks/runs/wr_01H…",
      "run": "wr_01H…" }
  ]
}
```

`composition` identifies **this set of members at these commits**: it is
a hash over each member's repository and its latest patchset commit. A
new patchset on any member, or a member added or removed, is a different
composition — the live composed runs of the old one are cancelled and a
fresh set starts, and `checks` only ever shows the current one. A
changeset with a member that has no patchset yet has no composition and
no composed checks; `composition` is `null`.

`state` is `queued`, `running`, `passing`, `failing`, `cancelled` or
`skipped`, the same six a check on a commit has. `run` and `detail_url`
lead to the run page, where the log tails live; `detail_url` is absolute,
under the deployment's public URL. The run page names the changeset and
the composition it was started for and links back here — a composed run
is not listed on the member repository's Checks tab (see [where
verdicts land](/docs/workflows/#composed-runs-for-a-changeset)), so
the changeset is where its runs are found.

### What it does to the gate

The composed verdict is folded into the changeset's `gate` alongside
every member's own, with the same precedence: blocked beats waiting beats
ready.

- Any composed check `failing`, `cancelled` or `skipped` → the changeset
  is **blocked**, and `explanation` names it: `composed check contract /
  contract in api is failing`.
- Otherwise any composed check `queued` or `running` → **waiting**, and
  the check is listed in `waiting_on` as `repo: check` — `builds:
  contract / contract` — naming the repository whose composed job it is,
  where a member's own check is listed as `repo/change: check`.
- Otherwise — including a changeset with no composed checks at all —
  composed CI has nothing to say and the gate is whatever the members
  make it.

`GET …/verdict` reports that fold, and
[`POST …/land`](#landing-a-changeset) refuses on it the same way it
refuses a member's own red check. Landing does **not** cancel a composed
run that is still going; it cannot start, because a running composed
check is `waiting`.

## Who gets told

Composing a changeset, landing one, and a landing that fails each send
one email. The recipients are the union, over every member, of that
member's participants — its author, everyone who commented, everyone who
approved — and the people that repository's OWNERS files *require*. The
person who took the action is never mailed about their own act.

Two rules are worth stating because they are the ones that would
otherwise surprise you.

**A path governed by `*` makes nobody required.** Whoever has write access
may approve it, so no individual is on the hook, and mailing everyone with
commit access about every change is how a notification becomes something
people filter. "The OWNERS file names you" is a reason to interrupt
somebody; "you happen to have commit access here" is not.

**You are never told about a changeset you could not read.** A changeset
mail names its members, so mailing somebody who cannot see one of those
repositories would publish that repository's existence. Every candidate
recipient is checked against every member before the mail goes out — the
same answer the API reaches when it masks a changeset you may not read as
a `404`.

One message per event, not one per member: a changeset is one review, and
four emails about it would be four reviews.

## In the dashboard

Everything above has a screen. **Changesets** in the sidebar lists the
organization's changesets, filterable by state, and **New changeset**
composes one: name it, then pick members from every open change in the
organization, grouped by repository. A change that is already in a
changeset, or that would be a second member from one repository, is
greyed out with the reason — the same three refusals `POST /changesets`
would give, shown before you submit rather than after. So is a change in
a repository you cannot push to: composing takes write access to every
member, and the server would refuse it without saying why.

A changeset's page is the verdict first — one word for the whole set
and the member it is waiting on, by name — then the members in landing
order with each one's own gate and approvals, the composed checks for
the current composition, and the workspace clone URL. Approving a member
happens where it is reviewed, on the change's own page — each member's
key in the table is a link to it; that page says **Lands with changeset
`<key>`**, links back, and turns its own Land and Abandon off while the
change is held. A member whose paths no OWNERS rule governs is not free:
it needs one approval from anybody with write access, and its row says
so. **Land all members** is enabled only when the
gate is `ready`; the page follows the landing step by step until every
member is on its trunk, or a failure has been put back. **Revert…**
prefills `revert-<key>`, makes the reverting changeset and opens it; the
original links to it under *Reverted by*, and it links back under
*Reverts*. **Abandon** releases the members. All of those are a writer's
controls — write access to every member repository — and somebody without
it is told so in their place, rather than shown buttons that answer
`no changeset` when pressed.
