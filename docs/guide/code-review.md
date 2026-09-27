---
layout: ../../layouts/Docs.astro
title: Changes, OWNERS & the land queue
description: Stack-native review — one commit per change, OWNERS-governed approval sufficiency, and fast-forward landing with verdicts in words.
---

# Changes, OWNERS & the land queue

Review on Weft is per commit, not per branch. A **change** is one
commit's review identity; its content moves through numbered
**patchsets**; approvals attach to a patchset; and landing goes through a
queue that only ever fast-forwards your target branch — through the same
compare-and-swap every push uses, so a landing and a racing push cannot
corrupt each other.

## Change identity: the Change-Id trailer

A change is keyed by a `Change-Id` trailer in the commit message's last
paragraph, the same convention Gerrit's commit-msg hook writes:

```
adjust the fee schedule

Change-Id: I8f3a2c94e1b7d605
```

Install the standard hook once and every commit gets one:

```bash
curl -Lo .git/hooks/commit-msg https://gerrit-review.googlesource.com/tools/hooks/commit-msg
chmod +x .git/hooks/commit-msg
```

Amend or rebase the commit and the trailer rides along, so the change
keeps its identity and the new commit becomes the next patchset. A commit
**without** a trailer still gets a change — keyed `g<commit-oid>` — but
that identity dies the moment the commit is rewritten: an amend mints a
new change. If you revise your work, use the trailer.

## Registering and revising

Push a branch, then register its tip:

```
POST /v1/orgs/{org}/repos/{repo}/changes
{ "from": "feature", "target": "main" }
```

`201` carries the change and its patchset. Re-registering the same commit
acks with `200` instead of duplicating; a new commit under the same
Change-Id becomes patchset N+1, and the change's title follows the newest
message. A change that is landing, landed or abandoned refuses new
patchsets with `409` and the state in the error.

### What changed since I last looked

Coming back to revision 4 of a forty-file change, the question is never
"what does this change do" — you read that last week — it is "what moved
since". Ask for it directly:

```
GET …/changes/{change}/interdiff?from=1&to=3
```

`from` and `to` are patchset **numbers**. Each is resolved to the commit
it was recorded at and the two trees are diffed, which is not the same
thing as diffing the newest patchset against its parent: a file touched
in patchset 2 and put back in patchset 3 is in *that* diff and is
correctly absent from this one. Entries come back in the same shape as
[`…/diff`](/openapi.json) — `{status, path, old_oid, new_oid, …}` — so
whatever renders one renders this, and the commit oids are echoed as
`from`/`to` beside the numbers you asked for as `from_patchset` /
`to_patchset`. Reversed ranges are fine; `from=3&to=1` shows what going
back would undo. Unknown patchset numbers are `404` and a range from a
patchset to itself is `400`, both in words.

The left-hand side does not have to be guessed. The viewed-marks read
carries it:

```
GET …/changes/{change}/views   → { patchset, since, viewed: [...] }
```

`since` is the newest patchset you have marked *anything* viewed at —
the revision your last pass was actually against — so the natural
request is `from=since&to=patchset`. It is `null` when you have marked
nothing, and that means "no last pass", never "patchset 1": with no
`since`, show the whole change rather than inventing a range over code
nobody read.

**Be clear about what this is not.** It is a two-commit tree diff between
the patchsets as they were pushed. If a patchset was rebased, everything
trunk picked up in between is in the result too — this is not a
rebase-aware three-way interdiff in Gerrit's sense, which subtracts that
move. And nothing migrates comment anchors across the range: comments
stay pinned to the patchset they were written against, which is exactly
the property that keeps them honest.

## OWNERS: who must approve what

Put an `OWNERS` file in any directory:

```
# payments needs a payments person
alice@acme.dev
@payments          # a team, resolved to its current roster
set noparent       # stop inheriting owners from parent directories
```

| Entry | Meaning |
|---|---|
| `person@example.com` | this org member owns the subtree |
| `@team-name` | any current member of the team owns it |
| `*` | anyone with write access to the repo may approve |
| `set noparent` | do not inherit entries from parent directories |
| `# …` | comment, full-line or trailing |

Rules inherit by default: the owners of `a/b/f.rs` are the union of
`a/b/OWNERS`, `a/OWNERS` and the root `OWNERS`, walking deepest-first and
stopping at `set noparent`. Entries resolve against the org's live
membership at evaluation time — someone joining a team changes the next
verdict, with no file edit. A path no `OWNERS` file governs needs one
approval from anyone with write access. A malformed `OWNERS` file fails
closed: the paths it governs are blocked until the file is fixed, and the
verdict names the file and line.

The rules are read from the **target branch's tip**, not from the
patchset. The paths under judgement are the patchset's diff; the `OWNERS`
files that judge them are the ones already on trunk. A patchset that
deletes or rewrites an `OWNERS` file therefore needs the approval of the
owners it is removing, and the same reading decides who is notified of
the change. Only when the target branch does not exist yet is there no
trunk to ask, and the patchset's own files are used.

`GET …/owners?path=<p>&at=<rev>` shows the effective rule chain for one
path; `GET …/owners/check?from=<rev>&to=<rev>&approvers=a@b.c` previews a
diff — "would these approvals suffice?" — before anyone clicks anything.

## Approvals and the verdict

Approvals attach to the **latest patchset**. Push a revision and the
count starts over: an approval of yesterday's patchset says nothing about
today's. Approving takes a person (session or personal token); a service
token is refused with `403`. Anyone who can read the change may approve
it, including somebody with no role in the org reading a public
repository — but sufficiency counts write access, so an outsider's
approval is recorded and moves nothing.

```
POST   …/changes/{change}/approve     approve the latest patchset
DELETE …/changes/{change}/approve     take your approval back
GET    …/changes/{change}/verdict     landable, and exactly why or why not
```

The verdict is landable when, for every changed path, at least one
approver satisfies that path's rule. Every answer comes with an
explanation per path, in words:

```
blocked: needs an owner of /payments/gateway.rs (owners: alice@acme.dev, @payments)
ok: /payments/gateway.rs approved by alice@acme.dev
```

### Who the change is waiting on

The verdict also names the people OWNERS requires, resolved through teams
to actual accounts, with whether each has already approved the latest
patchset:

```json
"reviewers": {
  "required": [
    {"user_id": "01…", "name": "Alice", "email": "alice@acme.dev", "approved": true},
    {"user_id": "01…", "name": "Casey", "email": "casey@acme.dev", "approved": false}
  ],
  "anyone_with_write": false
}
```

Two empty results mean opposite things, so the flag is not decoration.
`required: []` with `anyone_with_write: true` means a `*` rule governs the
path: anybody with write access satisfies it, so nobody in particular is
required. `required: []` with `anyone_with_write: false` means no OWNERS
rule governs what this patchset touches at all.

**Nobody nominates this list.** There is no "request a review from" call,
and there will not be one — the whole point is that the set is derived
from the OWNERS files on the target branch, so it cannot be quietly wrong
about who is required. A change's reviewers are a fact about the paths it
touches, not a guess somebody made when they opened it.

That is also why the list expands teams: `@payments` in an OWNERS file is
a rule, and the people it resolves to are the answer. A client cannot do
that expansion, so the server does it here.

## Finding the changes that matter

A busy repository's change list is a wall. Both lists take a `?q=`
query — the same text the dashboard's query bar holds, so every filter
is a URL you can paste into a review thread:

```
GET …/repos/{repo}/changes?q=is:open author:@me
GET /v1/orgs/{org}/changes?q=needs:my-approval repo:api
```

Terms are whitespace-separated `name:value`, and there are four:

| Term | Means |
|---|---|
| `is:open` `is:landing` `is:landed` `is:abandoned` | the change's state |
| `author:@me`, `author:<email>` | who opened it |
| `needs:my-approval` | it cannot land, and **you** are why |
| `repo:<name>` | one repository, org-wide list only |

**An unrecognised term is a `400` that names it.** Nothing is silently
ignored, and that is deliberate: a filter that quietly does nothing is
how somebody reads an unfiltered list of forty changes, concludes none
of them is theirs, and closes the tab — and the same URL, shared, would
then mean something different to whoever opened it next. A word with no
colon is not a term either; there is no free-text search here to fall
back on.

Two terms are about a person, so `author:@me` and `needs:my-approval`
need one: anonymous is `401` and a service token is `403`. An address
that names no account here is an empty page rather than a refusal — this
list is readable by strangers, and it is not an address oracle. `?state=`
still works and is the same filter as `is:`; given both, they must agree.

### `needs:my-approval`

This is the term no other forge can answer, and the reason is
structural. On GitHub a reviewer is *nominated*, so "waiting on me" can
only mean somebody typed your name. Here the reviewer set is **derived**
from the OWNERS files governing the paths the patchset touches, so the
question has an answer before anybody has done anything at all.

It means all four of these, and each one drops changes that would
otherwise make the list untrustworthy:

- the change is still `open`;
- it is **not yet landable** — one that already has what it needs has
  stopped waiting for you;
- OWNERS *requires* you for a path it touches. Not "you have write
  access": where OWNERS says `*`, or governs nothing, anybody with write
  may approve and **nobody is required**, so those changes are not here.
  Otherwise every change to an ungoverned file would land in front of
  every writer in the organisation, which is the difference between a
  list people keep and one they filter away;
- you have not already approved this patchset. A change you have signed
  off but that is still blocked on somebody else has stopped waiting for
  you too.

It is the expensive term — it resolves the target branch's OWNERS tree
once per change examined — so its page is capped at 50 rows examined per
request, and it is the one query where a page can come back short, or
empty, with `next` still set. Keep walking.

### Paging

Both lists are keyset-paginated. The response carries a `next`; pass it
as `?after=`:

```json
{ "changes": [ … ], "next": "01JB…" }
```

```
GET …/changes?q=is:open&limit=50
GET …/changes?q=is:open&limit=50&after=01JB…
```

`next` is `null` at the end. It is a cursor and not an offset, and the
difference is correctness rather than speed: changes arrive at the *top*
of this ordering, so `offset=50` after somebody opens one walks past a
row that has shifted down into it and you never see that change at all.
A cursor names a position in the data, so a change opened mid-walk
displaces nothing — it simply sorts above where you already are. Treat
the value as opaque; one this list did not mint is a `400`.

With `needs:my-approval` the cursor is where the *query* stopped, not
where the surviving rows did — which is why a short page still hands you
one.

## Landing

```
POST …/changes/{change}/land          202 — queued
GET  …/land-queue                     what is landing right now
POST …/changes/{change}/abandon       close without landing
```

The land request prechecks the verdict (`409` with the explanation when
blocked) and enqueues. The lander then re-verifies sufficiency at claim
time, proves the patchset fast-forwards from the target branch's current
tip with a bounded ancestry walk, and promotes the ref by
compare-and-swap. Landing a stack's top lands the whole stack: open
changes whose commits are now ancestors of the new tip are marked landed
by inclusion.

Landing takes `repo:write`. Abandoning takes `repo:write` **or** being the
change's author: somebody who proposed from a [fork](/docs/forks/) holds
only read access on the repository they proposed to, and a change they
could open but never close would be theirs to leave lying around. Anyone
else without write access gets the same `404` a stranger does.

A change that is a member of a [changeset](/docs/changesets/) — one
review unit spanning several repositories — refuses to land or be
abandoned on its own; it moves with the changeset.

Every outcome is recorded on the change as a verdict:

| Verdict | Meaning |
|---|---|
| `landed` | trunk now points at the patchset commit |
| `landed: included in <tip>` | landed by inclusion when a descendant landed |
| `ejected: sufficiency lost — <why>` | an approval went away before the claim |
| `ejected: not fast-forward from <tip>` | trunk moved; rebase and re-register |
| `ejected: not fast-forward (trunk moved)` | pushes kept winning the CAS |
| `ejected: history walk exceeded bound` | ancestry too deep to prove; a human should look |
| `ejected: the landing was given up after <n> attempts; the last failed with: <error>` | the store or the database refused the landing the same way `n` times running (`STRATUM_JOB_MAX_ATTEMPTS`, five); fix the cause and land again |

A landing whose driver fails is retried under a fresh job after
`STRATUM_LAND_RECHECK_SECS`, and the retries are counted across those
jobs; a change that is *waiting* on a check is not failing and is never
counted, however long the check takes.

The queue never force-pushes, never merges silently, and never lands a
mirror — a mirror's trunk belongs to its origin, so review is refused
there at change creation.

## Protecting trunk

A verdict is advice until nobody can push around it. Protect a branch
and it moves **only** through the land queue — every other write door
refuses with the same sentence:

```
branch 'main' is protected: land through review
```

That sentence comes back from `git push` over HTTP and SSH (in-band, as
the push report), from `POST …/commits`, and from reset, revert and
branch deletion. The one writer left is the lander, which re-checks
approval sufficiency at claim time — so "landable" is not just the
verdict's opinion, it is the only road.

```
GET    …/protections              the protected set
POST   …/protections              { "branch": "main" }     (admin)
DELETE …/protections/{branch}                              (admin)
```

Protecting takes a repo admin and an existing branch; protecting twice
is an ack, not an error. Both directions are audited — the fence moving
is an authority change, and the trail says who moved it.

The **default branch** — where clones start and changes land when no
target is named — is repo policy too:

```
PATCH …/repos/{repo}   { "default_branch": "trunk" }       (admin)
```

The branch must exist, mirrors follow their origin's HEAD instead, and
the move is audited. Protect the default branch and you have the whole
discipline in two calls: work lands through review, and review is where
the work actually lands.

## The conversation

Approvals say whether; comments say why.

```
POST …/changes/{change}/comments    { "body": "…", "path": "file.rs", "line": 12 }
GET  …/changes/{change}/comments    the conversation, oldest first
POST …/changes/{change}/comments/{comment}/resolve
POST …/changes/{change}/comments/{comment}/unresolve
```

Comments pin to the patchset they were written against — after a
revision, "this was about patchset 1" stays visible — and can anchor to
a file the change touches, or to a **line** of that file as of the
patchset (`line` is 1-based and needs `path`). A comment may name a
**range** with `line_end`, and may sit on either side of the diff:
`side: "old"` puts it on a line the patchset deleted, which is where
"why did this go?" belongs. People and service principals
can both comment (CI saying "the perf suite regressed" is review too),
and every comment is attributed honestly: a person as themselves, a
service principal as `service`, never borrowing a human name. Bodies
are bounded at 4,000 characters; anonymous readers of public repos may
read a conversation, not join it. Signed in is enough: a person with no
role in the org comments on a public repository's change as themselves —
that is the outside contributor answering a review of their own fork's
change — while a service token from another organisation reads as
anonymous and is refused. In the dashboard, line comments sit
inside the diff under the exact line, and the conversation records the
anchor as `path:line`. A comment can also be **drafted** rather than
posted — see [A review is one act](#a-review-is-one-act-and-it-can-say-no).

### Threads, and closing them

A comment may reply to another with `parent_id`, and a reply inherits its
root's anchor — it is part of that thread, not separately attached. Threads
are **one level deep**. A reply cannot itself be replied to, because a tree
is a forum and a forum is a different product; the refusal says so.

A thread is resolved on its **root**:

```
POST …/comments/{comment}/resolve     { }
POST …/comments/{comment}/unresolve   { }
```

**Who may resolve** is the interesting part, and it is the same question
the verdict asks. A thread may be closed by the person who opened it, or
by anyone who satisfies that path under OWNERS — the identical judge that
decides whether an approval counts, asked with one candidate. On a path
governed by `*`, or by no rule at all, write access is what OWNERS asks
for, so write access is what closes a thread there. Not two rules for one
file: somebody who may approve a change should not be unable to close a
nit on it.

The change's author does not get to resolve unilaterally. "I have read
your objection and I am closing it" is not a thing the person being
reviewed can say about a path they do not own.

Resolving takes a person. A service token is refused — resolution is a
judgement, like an approval, and unlike a check it is not a fact anybody
can report.

**Unresolved threads do not block landing.** They are rendered, counted
and impossible to miss, but the land gate turns on checks and on the
verdict — things somebody decided deliberately. A stray unresolved nit
holding up a landing is how "resolve everything before merge" becomes
ceremony that people learn to click through.

### Suggested changes

Say it in code. A reviewer writes a fenced `suggestion` block in a
comment anchored to a line or a line range, and the author applies it:

````
POST …/changes/{change}/comments
{ "body": "name it:\n```suggestion\nlet total = subtotal + tax;\n```",
  "path": "pay/gateway.rs", "line": 41 }

POST …/changes/{change}/suggestions/apply   { "comments": ["01hx…", "01hy…"] }
````

**A suggestion is not a field.** It is a fenced block inside the comment
body, parsed where it is read. There is no `suggestion` column and there
will not be one: a column beside the body would be a second, ungoverned
way to say the same thing, and the two would disagree the first time
somebody edited one of them. It also means an imported or mirrored
comment carries a suggestion exactly as one written here does, with
nothing to migrate.

The block replaces the lines the comment is anchored to — `line`, or
`line` through `line_end`. An **empty** block means *delete these
lines*, which is distinct from a comment with no block at all: one is a
suggestion, the other is a remark, and the API says two different things
about them. The file's own line endings and its final newline are kept,
so applying a suggestion to a CRLF file does not come back as every line
changing.

**Several suggestions, one patchset.** `…/suggestions/apply` takes a
list of comment ids and makes **one** commit on top of the latest
patchset. A reviewer leaves five remarks and the author takes them
together — one revision, one CI run, one notification — where a commit
per click would put five revisions on the change for a single act. The
commit goes through the same door a push does, so it is pinned, given
its CI and announced to the people OWNERS names, exactly like any other
patchset.

Nothing is rewritten. The new commit's parent is the patchset the
reviewer read, the previous patchset stays pinned and readable, and the
comments written against it stay pinned to it — which is why this is
clean under fast-forward-only landing: there is no history rewrite to
reason about. The message is the patchset's own, so the `Change-Id`
trailer keeps it a patchset of the same change. A change whose commit
carries no `Change-Id` is refused for exactly the reason
[above](#change-identity-the-change-id-trailer): a commit built from it
would open a new change rather than a revision of this one.

**Applying commits, so it takes write access.** A reader of a public
repository is told, in words, that the change's author applies it. A
"commit suggestion" button in front of somebody who cannot push is a
control that leads nowhere.

Refusals are in words, and the call is all-or-nothing — the change is
left exactly as it was:

- **overlapping anchors.** Two suggestions over one line are two
  reviewers disagreeing, and quietly picking either would commit a
  hybrid neither of them proposed. The refusal names both comments and
  both ranges; apply one, then the other against the patchset it makes.
- **a stale anchor.** A comment's line numbers are line numbers *of the
  patchset it was written against*. If that file has changed since — or
  the latest patchset no longer has it, or the anchor runs past its end
  — the suggestion is refused and the sentence names the file and the
  patchset. A file nobody has touched is still applicable from patchset
  1; the rule is "this file moved", not "you are late".
- **nothing to apply.** A comment that is still an unsubmitted draft,
  one with no line anchor, one on the `old` side of the diff (there is
  no such line to replace), one carrying no block, or one whose lines
  already read exactly as suggested.

An info string we do not understand is not a suggestion. GitHub's
anchor-moving `suggestion:-0+2` form is deliberately *not* read as a
plain suggestion: applying it as one would put the reviewer's text on
lines they were not talking about.

The response is `{ change, patchset, applied, paths }` — one patchset,
whatever the number of suggestions.

## A review is one act, and it can say no

Every comment above is published the instant you write it. That is the
right default for a passing remark and the wrong one for a review: a
reviewer working through forty files publishes their half-formed first
reaction, argues with it eleven comments later in public, and the author
watches the whole thing happen. So a review can be **drafted** and
submitted as one act.

```
POST   …/changes/{change}/comments        { "body": "…", "pending": true }
POST   …/changes/{change}/review          start or save the pending review
GET    …/changes/{change}/review          your draft, and the comments in it
DELETE …/changes/{change}/review          throw it away
POST   …/changes/{change}/review/submit   { "verdict": "…", "body": "…" }
POST   …/changes/{change}/review/withdraw take back a standing "no"
```

A comment posted with `pending: true` is **yours alone** until you
submit. Not visible to the author, not to another reviewer, not to an
admin, not to a service token, and not to an anonymous reader of a
public repository. `GET …/comments` shows everybody the published
conversation and shows *you* your own drafts, flagged `pending: true`;
that filter lives in one query, deliberately, because a leaked draft is
the worst thing this feature could do. Discarding a review takes its
drafts with it, and nobody ever knew they existed.

**Submitting sends one notification.** A twelve-comment review is one
email, because it is one act. Drafting sends none at all.

### The three verdicts

```
POST …/changes/{change}/review/submit
{ "verdict": "request_changes", "body": "the retry loop is unbounded" }
```

- **`approve`** writes exactly the row `POST …/approve` writes. Reviews
  do not have their own idea of sufficiency: the engine, the land gate
  and the lander read `approvals` and are untouched by any of this.
- **`comment`** is words and no verdict — the ordinary "here are my
  notes" pass. It leaves approvals alone in both directions.
- **`request_changes`** is the one the product did not have. Before it,
  the only negative signal was *silence*, which is exactly what "hasn't
  looked yet" also looks like. It revokes your own approval on that
  patchset — nobody approves and blocks the same code — and it stands.

Submitting takes a person, like approving and for the same reason: a
review says whether code should land, and that is not a fact a machine
can observe. A service token may still post checks and comments.

### What `request_changes` blocks, and what it does not

**It does not vanish on the next patchset.** An approval dies when new
code arrives, because the approver never saw the new code. A block must
not, or the author clears every objection by force-pushing over it —
which is the exact move the objection existed to stop. It ends when its
author withdraws it, or when they submit a different verdict of their
own. Nobody else can lift it; `…/review/withdraw` withdraws *your* block
and cannot name anybody else's.

**It blocks the land gate only when its author has standing on a path
this patchset touches.** That is the rule OWNERS already answers for
approvals, asked about a "no" instead of a "yes": your objection holds
up the change when the repository would have counted your approval —
because you own one of the touched paths, or, where OWNERS says `*` or
governs nothing, because you have write access. Everyone else's is
**recorded, rendered, and advisory**. GitHub lets any passer-by wedge a
pull request; we can do better precisely because the reviewer set is
computed rather than nominated.

The verdict endpoint says which is which:

```json
"blocks": [
  {"author": "Casey", "verdict": "request_changes", "blocking": true,
   "body": "the retry loop is unbounded"},
  {"author": "Dev", "verdict": "request_changes", "blocking": false,
   "body": "I would not, personally"}
]
```

and when one of them is blocking, the verdict itself says so in words:

```
blocked: casey@acme.dev asked for changes; it stands until they withdraw it
```

A `request_changes` needs words: either a cover message, or comments of
its own. A block with neither tells the author no and never what would
make it a yes.

**There is still no reviewer nomination**, and there will not be one.
Blocking is the one place a person's opinion becomes authoritative here,
and the authority comes from the same OWNERS resolution as everything
else — not from anybody adding a name to a list.

## How a change should flow

The pieces above are one discipline, end to end:

1. **Protect trunk once.** From then on the verdict is not advice; it
   is the only way trunk moves.
2. **Small changes, stacked.** One commit is one reviewable idea; a
   stack lands together when the top lands, so nothing blocks on batch
   size.
3. **Read the diff where you approve it.** The change view puts the
   line diff, the conversation and the verdict on one screen — approval
   and reading are the same sitting, not two tabs.
4. **Say why on the line, and say it once.** A comment anchored to the
   line survives the revision that answers it, pinned to the patchset it
   was about. Draft the whole pass and submit it as one review: twelve
   remarks are one act, one verdict and one email.
5. **Approve the patchset, not the person.** A new revision starts the
   count over; nobody lands code their approver never saw.
6. **Let CI vote where reviewers look.** Wire your CI to post checks on
   the change; a failing check blocks the queue, and the change page is
   the one place both verdicts — human and machine — read together.
7. **Land through the queue and stop watching.** Sufficiency and checks
   are re-verified at claim time, the promotion is a compare-and-swap,
   and every outcome — landed, included, ejected — arrives in words, on
   the change and over webhooks.

## CI in the loop

Your CI is a reviewer with a badge, not a bystander. The loop is three
steps, using systems you already run:

1. **Hear about work**: subscribe a [webhook](/docs/webhooks/) — `push`
   fires on every branch update, `change.landed` / `change.ejected` on
   queue outcomes.
2. **Run whatever you run** — GitHub Actions, Buildkite, Jenkins, a
   shell script. Weft does not care who does the computing.
3. **Report the verdict as a check** on the change:

```
POST …/changes/{change}/checks   { "name": "ci/tests", "state": "failing",
                                   "url": "https://ci.example.com/run/812" }
GET  …/changes/{change}/checks   the latest patchset's checks
```

Checks attach to the **latest patchset**, like approvals: push a
revision and CI reports again, because a green run of yesterday's code
says nothing about today's. Posting again under the same name updates
in place (`pending` → `passing`), and a service token is exactly the
right credential — reporting a build is a machine's job, unlike
approving, which stays human-only.

The states mean what they say: `pending` is running, `passing` is
green, `failing` blocks. A failing check refuses the land request —
`blocked: check 'ci/tests' is failing` — and the queue re-checks at
claim time, so a red that lands between enqueue and claim ejects with
`ejected: check failing — ci/tests`. A pending or absent check does not
block: which checks are *required* before landing is per-repo policy,
in design; today the contract is exactly this — red stops the queue,
and the change page shows every check with its state, who posted it,
and a link to the run.

## Webhooks

`change.landed` and `change.ejected` deliver beside `push`, HMAC-signed
the same way — see [Webhooks](/docs/webhooks/):

```json
{ "event": "change.landed",
  "payload": { "change": "I8f3a2c94e1b7d605", "commit": "…", "branch": "main", "patchset": 2 } }
```

## In design

Speculative batching, target-aware parallel landing, and three-way merge
for drifted changes are in design for Repos customers. Until they ship,
the queue's contract is exactly what this page describes — fast-forward
only, with the verdicts above.

Patchset-to-patchset review shipped and is
[above](#what-changed-since-i-last-looked); what remains in design there
is the rebase-aware half — subtracting the trunk move from a rebased
patchset's diff, and carrying comment anchors across the range.
