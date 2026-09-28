# Forks and the contribution flow

A fork is how somebody who may read a repository but not push to it
contributes. On a Spool server it is a **new repository, in a namespace
of yours, that shares the upstream's stored objects** until the two
histories diverge: creating one takes milliseconds and occupies nothing
until you push something upstream does not have. A contribution is then
an ordinary [change](code-review.md) against the upstream whose commits
happen to live in your fork, and it lands through the same `OWNERS`
sufficiency, required checks and serialized land queue as a colleague's.
There is no second path.

Forks live inside your server. There is no forking to or from another
server, and nobody outside your server can see a fork, because nobody
outside it can see anything.

Two things this page describes as designed and not built — bringing a fork
up to date with its upstream, and [the maintainer
firewall](maintainer-firewall.md) — are called out as such
[at the end](#what-is-not-built).

## Who can fork what

**You may fork what you may read.** For a repository in an organization,
that means you are a member of the organization, at any role — `viewer`
is enough — or you hold a grant on that repository. The usual shape is a
contributor who is a `viewer` in `acme`, forking `acme/widget` into
their own namespace, pushing there, and proposing the change back.

**The fork goes where you may create a repository.** That is your
personal namespace by default, or an organization where you hold
`repo:write`. You also need a confirmed address, as for any repository
you create.

**Forking takes a signed-in person.** A service token has nobody behind
it and is answered `401`. And because a token acts only in the one
organization it was minted in, a fork that crosses two namespaces —
`acme/widget` into your personal namespace, say — takes a **browser
session**, which spans every namespace you belong to. The dashboard's
**Fork** button is that. A personal token can fork only within its own
organization.

A personal namespace cannot have members, so nobody else can read a
fork in yours. A repository that other people are meant to read or fork
belongs in an organization.

## What a fork is

`POST /v1/orgs/{org}/repos/{repo}/forks` creates one:

```sh
curl -X POST "$WEFT_URL/v1/orgs/acme/repos/cantor/forks" \
  -b "$COOKIE_JAR" -H "Content-Type: application/json" \
  -d '{"org": "bob", "name": "cantor"}'
```

Both body fields are optional — `org` defaults to your personal namespace
and `name` to the source's name — so the common case is a button with
nothing to fill in, which is what the dashboard's **Fork** button is.

The answer is **`202`, not `201`**, and the difference is honest rather
than pedantic. The repository row exists the moment the call returns; the
storage pointers that make it readable are written by a job a moment
later. Claiming `Created` would let you clone immediately and get an empty
repository with no explanation. Instead the repository view carries
`fork_state`: `pending`, then `ready` — or `failed`, because a fork that
says it is broken beats one that silently serves nothing. `fork_parent` is
the `owner/name` it came from, and the dashboard prints it under the
repository name as "Forked from …", the way every contributor expects. It
is named to anyone who may read the upstream, and is `null` to a reader
who may not, so a repository is never named to somebody who could not
otherwise know it exists.

Forking a repository you have **already forked** into the same place
answers `200` with the fork you have, not a second copy and not an error:
pressing **Fork** on a project you forked last month takes you to your
fork, as it does on GitHub, and the dashboard says that is what happened.
The name being taken by a repository of yours that is *not* a fork of this
one is a real collision, answered `409` with the repository that is in the
way and the way past it — `bob/widget already exists and is not a fork of
acme/widget; fork it under another name`.

The fork has its own name, its own permissions, its own push path over
[HTTPS and SSH](ssh.md) and its own [audit trail](audit-and-undo.md).
Your pushes are new data in a repository you own. Nothing about it is a
view onto upstream that upstream can change out from under you: the
storage a fork refers to is pinned for as long as the fork refers to it.

A fork is private to the namespace it is in, like every repository. The
people who can read the upstream cannot, by that fact alone, read your
fork: they read it only if they belong to the namespace it lives in.

### Counting forks

`GET /v1/orgs/{org}/repos/{repo}/forks` lists the repositories forked
directly from this one, and `count` is the number **you may see** — always
the length of the list, never a stored total. Forks live in other
namespaces, most of which you cannot read, and publishing the stored
number would tell everybody how many forks exist that they cannot see.
The dashboard's fork count reads the same way.

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
reader **reads** — you clone and fetch the upstream, which is how you
bring your fork up to date — and a reader's push is refused with the
reason, over HTTPS and SSH alike: `you can read acme/cantor but not push
to it; fork it and open a change from your fork, or ask an owner for
write access`.

Step three is `POST /v1/orgs/{org}/repos/{repo}/changes` with `source`:

```sh
curl -X POST "$WEFT_URL/v1/orgs/acme/repos/cantor/changes" \
  -b "$COOKIE_JAR" -H "Content-Type: application/json" \
  -d '{"from": "fix-empty-config", "target": "main", "source": "bob/cantor"}'
```

With `source` set you need only **`repo:read` on the target**, plus read
access to the fork. A fork in your personal namespace and an upstream in
an organization are two namespaces, so this too takes a session rather
than a token. The change is yours: it is recorded with you as its author,
the review's association badge calls you `first-time` until something of
yours has landed there and `contributor` after, and you take part in the
review as yourself — comment, tick off the files you have read, revise,
and withdraw it (`POST …/abandon`) if you change your mind, which
otherwise takes write access. Without `source` you still need
`repo:write`, because commits that are already in the target could only
have got there through an authorised push; a reader who tries is
answered `403` with a sentence that names forking as the way through.
`source` must be `owner/name` — a bare name is ambiguous between "a
repository in this org" and "a namespace" — it must be readable by you,
so a fork you cannot read is masked here exactly as it is everywhere
else, and it must actually be a fork of the repository being targeted.
That last check is not tidiness: landing copies objects out of the source
into the target, so an arbitrary repository named here would be a
request to move somebody else's bytes into a project they do not own.

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
sufficiency, required checks and the land queue](code-review.md). The
Land button on a change from a fork *is* the queue, which is how a
protection rule and an approval requirement mean the same thing on a
contribution from a reader as on one from a colleague. A change's
`source` is reported on the wire as `owner/name`, and stays `null` for a
change that did not come from a fork — and for a landed change whose fork
has since been deleted, because a landing that happened is still a real
thing that happened.

### Workflows from a fork are held until a maintainer says so

A change whose commits come from a fork does **not** run its workflows.
The workflow file was written by the contributor, and running it would
hand somebody who may only read your repository a `repo:read` token and
one of your runners — their code on your hardware. Its runs are recorded
as `blocked` with `blocked_reason: fork`, its check rows stay `queued`
rather than failing because nothing is wrong with the commit, and a
maintainer who could land the change releases them with
`POST …/changes/{change}/workflows/approve`. Approval is **per tip**: a new
patchset from the fork is blocked again. In a [changeset](changesets.md)
one unapproved fork member holds every member's composed run, because a
composed job checks out all of them. The full rules are in
[Workflows](workflows.md#changes-pushed-from-a-fork).

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
history forces, merge or rebase, is yours to make. When it is built it
will be fast-forward only, refusing a diverged fork in the
[land queue's](code-review.md) house style; a one-click button that
silently picked one would sometimes rewrite your work.

**Following the upstream's storage.** A fork keeps referring to the
upstream's storage as it was when the fork was made, and that is
deliberate: the upstream's data is pinned for the fork, and the fork never
has to learn about a compaction it did not ask for.

**The [maintainer firewall](maintainer-firewall.md)** — staged intake
in front of review — is designed and published as a design. A change from
a fork reaches review the moment it is registered.
