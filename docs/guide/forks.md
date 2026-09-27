---
layout: ../../layouts/Docs.astro
title: Forks and the contribution flow
description: Zero-copy forks and contributing without write access — what a fork is, how a change from one reaches review and lands, and what a fork does not get to skip.
---

# Forks and the contribution flow

A fork is how somebody with no push credential contributes at all. Here it
is a **new repository in your namespace that shares the upstream's stored
objects** until the two histories diverge: creating one takes milliseconds
and occupies nothing until you push something upstream does not have. A
contribution is then an ordinary [change](/docs/code-review/) against the
upstream whose commits happen to live in your fork, and it lands through
the same `OWNERS` sufficiency, required checks and serialized land queue as
a colleague's. There is no second path.

Two things this page describes as designed and not built — bringing a fork
up to date with its upstream, and [the maintainer
firewall](/docs/maintainer-firewall/) — are called out as such
[at the end](#what-is-not-built).

## What a fork is

`POST /v1/orgs/{org}/repos/{repo}/forks` creates one. You may fork what
you may read; where it goes is up to you:

```sh
curl -X POST "$STRATUM/v1/orgs/ada/repos/cantor/forks" \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"org": "bob", "name": "cantor"}'
```

Both body fields are optional — `org` defaults to your personal namespace
and `name` to the source's name — so the common case is a button with
nothing to fill in, which is what the dashboard's **Fork** button is.
Creating in the target namespace is gated exactly as any other repository
creation is: you need `repo:write` there, a verified address, and a plan
that admits what you are creating — a private source asks the target to
hold a private repository, so forking one into an organization on the
free plan is refused with the same `402 quota:` sentence a private create
gets. The free-tier repository cap counts forks too: a `free` namespace that is
at its cap is answered `402 quota:` and nothing is created. The sentence
says what the reader can actually do about it, which is not the same
thing in the two places a cap is met: an organization is told `the free
plan is limited to N repositories — subscribe from Billing to create
more`, and a personal namespace — which is always `free`, has no billing
page, and cannot be subscribed — is told `a personal namespace holds up
to N repositories — create an organization to hold more`. Being sent to
Billing for a namespace that has none is a dead end, and it read as the
product being broken rather than as a limit.
Pressing Fork on something you already forked still hands that fork back
at the cap, because nothing is being made. Forking is something a
signed-in person does; a service token is answered `401`.

The answer is **`202`, not `201`**, and the difference is honest rather
than pedantic. The repository row exists the moment the call returns; the
storage pointers that make it readable are written by a job a moment
later. Claiming `Created` would let you clone immediately and get an empty
repository with no explanation. Instead the repository view carries
`fork_state`: `pending`, then `ready` — or `failed`, because a fork that
says it is broken beats one that silently serves nothing. `fork_parent` is
the `owner/name` it came from, and the dashboard prints it under the
repository name as "Forked from …", the way every contributor expects. It
is named to anyone who may read the upstream — public or not — and is
`null` to a reader who may not, so a private repository is never named to
somebody who could not otherwise know it exists.

Forking a repository you have **already forked** into the same place
answers `200` with the fork you have, not a second copy and not an error:
pressing **Fork** on a project you forked last month takes you to your
fork, as it does on GitHub, and the dashboard says that is what happened.
The name being taken by a repository of yours that is *not* a fork of this
one is a real collision, answered `409` with the repository that is in the
way and the way past it — `bob/widget already exists and is not a fork of
ada/widget; fork it under another name`.

The fork has its own name, its own permissions, its own push path over
[HTTPS and SSH](/docs/ssh/) and its own [audit trail](/docs/audit-and-undo/).
Your pushes are new data in a repository you own. Nothing about it is a
view onto upstream that upstream can change out from under you: the
storage a fork refers to is pinned for as long as the fork refers to it.

### Visibility is not negotiable across a fork

A fork **inherits its source's visibility**. A fork of a private
repository is private, and it cannot be made public while its root is
private — the request is refused with `403 a fork of a private repository
cannot be made public`, not warned about. A fork shares the upstream's
bytes without ever having copied them, so publishing it would publish the
repository it came from to anonymous readers. The check is against the
fork **root**, not the immediate parent, so it holds down a chain of
forks. Making a fork *private* is always allowed.

### Counting forks

`GET /v1/orgs/{org}/repos/{repo}/forks` lists the repositories forked
directly from this one, and `count` is the number **you may see** — always
the length of the list, never a stored total. A public repository's forks
can be made private afterwards, and publishing the stored number would
tell every visitor exactly how many private forks exist. The dashboard's
fork count reads the same way.

### Deleting an upstream does not delete its forks

You may delete a repository that has forks, as on GitHub; what differs is
that the forks survive it. Every fork still reading the deleted
repository's data is **promoted** onto storage of its own first, and only
when the last reference is released does the upstream's storage become
sweepable. A promoted fork's `fork_parent` is `null` — the same answer as
"not a fork" and as "an upstream you may not see", deliberately, so that
the field never leaks the existence of a repository you cannot read.

## The contribution flow

The flow is the one every open-source contributor already knows:

1. **Fork** the repository you want to change.
2. **Push** to your fork — a clone and a push, over HTTPS or SSH, like any
   other repository.
3. **Open a change** against upstream, naming your fork as its `source`.

Step two is an **ordinary push to a repository you own**. There is no
special case in push authorization for forks, no borrowed permission on
upstream and no staging area with its own rules: you have write access to
your fork, which is why the push works; you do not have write access to
upstream, which is why a direct push there is refused. Forking adds no
case to push authorization at all: to the upstream, a forker is a reader,
and what a reader meets on both transports is what the forker meets. A
reader **reads** — your own token or SSH key clones and fetches the
upstream, which is how you bring your fork up to date — and a reader's
push is refused with the reason, over HTTPS and SSH alike: `you can read
ada/cantor but not push to it; fork it and open a change from your fork,
or ask an owner for write access`. It used to be refused as `repository
not found`, the answer meant for a repository you cannot read, and a
contributor who had cloned it a minute earlier was left checking the URL
for a typo.

Step three is `POST /v1/orgs/{org}/repos/{repo}/changes` with `source`:

```sh
curl -X POST "$STRATUM/v1/orgs/ada/repos/cantor/changes" \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"from": "fix-empty-config", "target": "main", "source": "bob/cantor"}'
```

With `source` set you need only **`repo:read` on the target** — which on
a public repository every signed-in person and every personal token
holds, whatever namespace it was minted in. The change is yours: it is
recorded with you as its author, the review's association badge calls you
`first-time` until something of yours has landed there and `contributor`
after, and you take part in the review as yourself — comment, tick off
the files you have read, revise, and withdraw it (`POST …/abandon`) if
you change your mind, which otherwise takes write access. Without `source`
you still need `repo:write`, because commits that are already in the
target could only have got there through an authorised push; a reader who
tries is answered `403` with a sentence that names forking as the way
through. `source` must be `owner/name` — a bare name is ambiguous between
"a repository in this org" and "a namespace" — it must be readable by you,
so a private fork is masked here exactly as it is everywhere else, and it
must actually be a fork of the repository being targeted. That last check
is not tidiness: landing copies objects out of the source into the target,
so an arbitrary repository named here would be a request to move somebody
else's bytes into a project they do not own.

The dashboard's Changes tab offers the same thing as **Propose a change**
to anyone who can read the repository but not push to it, with a field for
the fork; readers with write access see **Start a review** instead. The
server says which you are (`viewer_write` on the repository view), so the
form is never offered to somebody who will be refused after filling it in.

### The commits arrive when the change opens

Your commits are copied into the upstream **the moment the change is
registered**, onto `refs/staged/<change-key>` — outside `refs/heads/`, so
it is not a branch, does not appear in the branch list and no protection
rule applies to it. GitHub does the same with `refs/pull/N/head`, for the
same reason: the diff, `OWNERS` resolution and the land verdict then all
read one repository rather than two. Deferring the copy to landing was
tried first and failed exactly where you would expect — approval resolves
`OWNERS` at a commit that was still in the fork.

A second patchset moves only what is new; re-registering the same tip
moves nothing. The staged ref stays after the change is abandoned, so the
objects of a change that was approved and then ejected are not swept while
you are still working on it.

Everything downstream is identical to internal work: [patchsets, `OWNERS`
sufficiency, required checks and the land queue](/docs/code-review/). The
Land button on a change from a fork *is* the queue, which is how a
protection rule and an approval requirement mean the same thing on a
contribution from a stranger as on one from a colleague. A change's
`source` is reported on the wire as `owner/name`, and stays `null` for a
change that did not come from a fork — and for a landed change whose fork
has since been deleted, because a landing that happened is still a real
thing that happened.

### Workflows from a fork are held until a maintainer says so

A change whose commits come from a fork does **not** run its workflows.
The workflow file was written by the contributor, and running it would
hand a stranger a `repo:read` token and a machine — a fork's change is
somebody else's code on your hardware. Its runs are recorded as `blocked`
with `blocked_reason: fork`, its check rows stay `queued` rather than
failing because nothing is wrong with the commit, and a maintainer who
could land the change releases them with
`POST …/changes/{change}/workflows/approve`. Approval is **per tip**: a new
patchset from the fork is blocked again. In a [changeset](/docs/changesets/)
one unapproved fork member holds every member's composed run, because a
composed job checks out all of them. The full rules, including the
organisation-level block that no maintainer can approve away, are in
[Workflows](/docs/workflows/#changes-pushed-from-a-fork).

## What a fork does not get to skip

Sharing upstream's objects buys speed, not trust. Objects pushed to a fork
go through the same quarantine, the same fetch and hash-verification of
thin bases, and the same connectivity and fast-forward walk as any other
push; a fork does not skip verification on the grounds that the bases
"already exist". Copying a change's commits into the upstream materialises
both repositories and moves the objects with `git` itself rather than by
arithmetic on packs — correct and expensive over clever and conditionally
wrong, when being wrong means a repository that does not `fsck`. Every
clone of a fork, and of an upstream a fork has landed into, passes
`git fsck --full --strict`; the serving gates check it on every change to
this path.

## What is not built

**Syncing a fork from its upstream.** There is no request that brings a
fork up to date with the repository it came from. Today that is a `git
fetch` from upstream — your own credential reads it — and a push to your
fork, in your own clone, which is also where the decision a divergent
history forces, merge or rebase, is yours to make. When it is built it will be fast-forward only, refusing a
diverged fork in the [land queue's](/docs/code-review/) house style; a
one-click button that silently picked one would sometimes rewrite your
work.

**Following the upstream's storage.** A fork keeps referring to the
upstream's storage as it was when the fork was made, and that is
deliberate: the upstream's data is pinned for the fork, and the fork never
has to learn about a compaction it did not ask for.

**The [maintainer firewall](/docs/maintainer-firewall/)** — staged intake
in front of review — is designed and published as a design. A change from
a fork reaches review the moment it is registered.
