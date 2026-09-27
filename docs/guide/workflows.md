---
layout: ../../layouts/Docs.astro
title: Workflows
description: CI from a `.weft/*.yml` file in your repository, on Weft's runners or on machines you register yourself, reported as checks on the commit.
---

# Workflows

Put a YAML file in `.weft/` and pushing runs it. Each job runs in a
throwaway container on Weft's runners — or on a machine you registered
yourself, if it asks for one — its output is a log you can read while it
is still being written, and its verdict arrives on the commit's
**Checks** tab as a check named after the job, where it gates landing
exactly like a check posted by any other CI.

This is one of two ways to get a verdict here. The other is
[CI integration](/docs/ci-integration/): your own CI, wherever it runs,
signing a result back. They coexist, and a change waits on both.

The workflow file is deliberately a *subset* of GitHub Actions' syntax
rather than a lookalike of it. Everything not implemented is **refused
by name, with a line number**, and never accepted-and-ignored. A key
that silently did nothing would be the worst outcome available: a job
reported green for not having done what its author wrote.

## A complete example

`.weft/ci.yml`:

```yaml
name: ci
on: [push, change]
jobs:
  test:
    steps:
      - name: Build
        run: make build
      - name: Test
        run: make test
```

That is a whole workflow. On every push to any branch, and on every
patchset of every change, one job called `test` runs two commands in a
container holding your repository at that commit, and a check called
`ci / test` appears on the commit.

## Where the files live

| | |
|---|---|
| Directory | `.weft/`, at the repository root. Not configurable |
| Extensions | `.yml` and `.yaml` |
| Files read | the first **32**, in tree order |
| Size | **64 KiB** per file |

They are read **from the pushed commit**, not from the default branch:
a workflow change is tested by the push that contains it, and a branch
that has not landed yet runs its own version of the file.

One file is one workflow, and one workflow is one run. `---` document
separators, YAML anchors, aliases and `!` tags are all refused — anchors
and aliases because they are how a small file expands into a very large
one, and tags because everything in this subset is plain text and a tag
that was honoured nowhere would only mislead.

A duplicated key in a block is refused rather than resolved last-wins:
a file with `steps:` twice has one of them doing nothing, and which one
is not something anybody should have to work out.

## The keys

**Top level:** `name`, `on`, `jobs`. A file with no `name:` is named
after itself, so `.weft/nightly.yml` is the workflow `nightly`.

**`on:`** takes `push`, `change`, `changeset`, or a list. `pull_request`
is accepted as a spelling of `change`, so a pasted Actions workflow
works. There is no `schedule` and no `workflow_dispatch`.

`push` means a push that arrives through one of the three push doors —
`git push` over HTTPS, `git push` over SSH, or `POST …/commits`. It does
not mean a ref moving for some other reason: commits that reach a
**mirror** by syncing from its origin start no run, and neither do tags.
Only `refs/heads/*` triggers anything.

`changeset` means a patchset of a
[changeset](/docs/changesets/) this repository is a member of: the job
runs with every member repository checked out beside this one. It is a
different event from `change`, not a wider one — `on: [change,
changeset]` asks for both, and a repository whose file says only
`changeset` runs nothing on its own changes. See [Composed runs for a
changeset](#composed-runs-for-a-changeset).

**A job** takes `name`, `needs`, `image` (or `container`), `runs-on`,
`env`, `strategy`, `timeout-minutes` and `steps`.

**A step** takes `name`, `run` and `env`. `run` is required and is a
shell command; a step with nothing to run is refused.

### `runs-on`

`runs-on` picks the **pool** a job runs on, and there are two: Weft's
runners, and machines you registered yourself.

**Weft's runners** are `ubuntu-latest`, `ubuntu-24.04`, `ubuntu-22.04`
and `linux`. All four mean the same thing — the ordinary Linux runner,
which is the only hosted one there is — and the key is accepted only
because those four are unambiguous. `macos-latest` and `windows-latest`
are refused. Running a macOS job on Linux is not a smaller version of
what was asked for; it is a different thing reported green.

**Your own machines** are a `runs-on` containing `self-hosted`:

```yaml
runs-on: ubuntu-latest              # hosted, unchanged
runs-on: self-hosted                # your machines; any runner allowed to take it
runs-on: [self-hosted]              # the same thing
runs-on: [self-hosted, linux, gpu]  # …narrowed by labels
```

Every other entry in that list is a **label**, and the job goes to a
runner whose own labels contain all of them. A label is letters, digits,
dot, dash or underscore, at most 64 characters. Labels are **lowercased
once, here**, and duplicates dropped, so `GPU` in the file matches `gpu`
on the runner rather than being a job that never runs and an operator
with nothing to look at; the order you wrote is kept, because that order
is what the refusals below print back at you. What the labels mean is in
[Self-hosted runners](#self-hosted-runners) below.

The two do not mix inside one `runs-on`. Naming a hosted runner *and*
`self-hosted` in the same list is refused:

```
`runs-on` names both a hosted runner and `self-hosted`; pick one
```

A list without `self-hosted` must be exactly one hosted label, and an
unrecognised label is refused by name as it always was — the hint now
also tells you about the `[self-hosted, …]` form, because "`gpu` is not a
runner we have" is unhelpful to somebody who does have a GPU machine.

Different jobs in the same file may use different pools. A file whose
`build` runs here and whose `gpu-test` runs on your hardware is
ordinary, and `needs` between them works exactly as it does within one
pool.

### `image`

`image:` and `container:` both name a container image, in either the
scalar form (`container: node:18`) or the block form with an `image:`
key. Both **parse**, but only `image: default` — which is what a job
gets when it says nothing — runs on the hosted fleet. Anything else
fails that job at dispatch with

```
image "node:18" is not available on hosted runners (only default)
```

so the file is legal and the job is not. A container block's
`credentials`, `ports`, `volumes` and `options` are refused outright
rather than dropped quietly.

A [self-hosted](#what-a-self-hosted-job-gets) job has no container at
all, so it is refused for naming an image rather than for naming the
wrong one.

### What is refused, and what to write instead

| You wrote | What happens |
|---|---|
| `uses:` on a step | Refused: "this forge does not run Actions". Run the command directly with `run:` |
| `if:` on a job | Refused, "not supported yet". A job runs when everything it `needs` has passed, and that is the only condition there is |
| `outputs:`, `defaults:` on a job | Refused, not supported yet |
| `strategy.fail-fast`, `strategy.max-parallel` | Refused. Both are scheduler behaviour that does not exist here, and accepting them would be a lie |
| `env:` at the top level | Refused. Job `env:` and step `env:` are the whole of it |
| `shell:` on a step | Refused — see [the runner environment](#the-runner-environment) for what a step actually runs under |

`uses:` is the one most people meet first, and it has no equivalent. A
workflow here is shell commands; if an action did something you need,
do that thing directly.

## `needs`, and what a failure does

`needs` names other jobs in the same file. A job whose `needs` names a
job that does not exist is refused with the list of jobs that do, and
so is a cycle — a typo that silently ungated a job, or a run that hung
forever, are both worse than a red push.

A job starts when **every** job it needs has passed. When a job fails,
is cancelled, or times out, everything downstream of it is marked
`skipped`, transitively, with the reason *a job it needs did not pass*
— except a dependent that was already running, which is doing real work
and reports its own verdict.

Readiness is computed, not stored, so a job becomes runnable the instant
its last dependency passes.

## Matrices

```yaml
name: ci
on: push
jobs:
  test:
    strategy:
      matrix:
        os: [linux]
        toolchain: ["1.83", "1.84"]
        exclude:
          - os: linux
            toolchain: "1.83"
        include:
          - os: linux
            toolchain: "1.85"
            flags: "-D warnings"
    steps:
      - run: ./test.sh $WEFT_MATRIX_TOOLCHAIN
```

Each cell is a separate job with its own container, its own log and its
own check. A cell's name — in the run, in the log and in the check — is
`id (v1, v2)`, values in the order the axes were declared:
`test (linux, 1.84)`.

- **`exclude` is applied first and `include` after**, following GitHub's
  documented order, so an `include` entry can put back a combination an
  `exclude` removed. An `include` merges into cells it does not
  contradict and is appended as a new cell where it cannot merge; it may
  add keys but never overwrites an axis value.
- **A workflow expands to at most 256 jobs in total**, not per job. The
  cap is checked during expansion, so a matrix that would be enormous is
  refused cheaply rather than built and then rejected.
- **A cell that `needs` a matrix job waits for all of that job's cells.**
  Pairing cells up by matching values would be a different feature.

Every axis value is also in the environment as
`WEFT_MATRIX_<AXIS>`, upper-cased with anything outside `[A-Z0-9_]`
replaced by `_` — so `node-version` arrives as
`WEFT_MATRIX_NODE_VERSION`.

## The runner environment

This section describes a job on **Weft's** runners. Each job is one
**isolated task, discarded when the job ends**. It is not a machine
shared with your other jobs, and there is nothing left over from the
previous one. On a self-hosted runner the machine is yours and that
guarantee is yours to provide — see [what a self-hosted job
gets](#what-a-self-hosted-job-gets).

What the job can reach:

- The repository, checked out at the pushed commit, in the working
  directory every step starts in. The fetch is the **one ref** the job
  is about, with `--no-tags`.
- A **repository-read token minted for that one job**, held in the
  runner's own environment and never passed to a step. It expires with
  the job and is **revoked the moment the job reports its verdict**.
- The public internet, and nothing of Weft's beyond the runner API it
  reports to. No database, no object store, no cloud credentials, no
  Docker socket. The task holds no IAM role at all.

Every step runs as **`bash -e -c '<your run block>'`** in the checkout,
with a scrubbed environment — a step does **not** inherit whatever the
container happened to hold. So each step is one bash script: `-e` means
a multi-line `run:` block stops at its first failing command, and `set
+e` turns that off if you want it to. There is **no `pipefail`** unless
you set it yourself, so `a | b` reports `b`'s status and a failure in
`a` passes silently — put `set -o pipefail` at the top of the block if
that matters.

Steps run in order and stop at the first failure. The log lists the ones
that never ran, so a reader scrolling to the bottom of a failed job can
see what did not get a chance.

### The environment a step sees

| Variable | Value |
|---|---|
| `CI` | `true` |
| `WEFT_CI` | `true` |
| `WEFT_JOB` | the cell's name — `test`, or `test (linux, 1.84)` |
| `WEFT_SHA` | the commit being built |
| `WEFT_REF` | the branch: the pushed branch, or the change's target |
| `WEFT_EVENT` | `push`, `change` or `changeset` |
| `WEFT_CHANGE` | the change key — **only** on a change- or changeset-triggered run |
| `WEFT_WORKSPACE` | absolute path of the directory the member repositories are checked out under — **only** on a changeset run |
| `WEFT_CHANGESET` | the changeset key — **only** on a changeset run |
| `WEFT_CHANGESET_MEMBERS` | JSON, described below — **only** on a changeset run |
| `WEFT_MATRIX_<AXIS>` | one per matrix axis |
| `PATH`, `HOME`, `LANG` | inherited from the runner |

Those three inherited names are the entire allowlist. Then, in order,
lowest first: the job's `env:`, then the step's `env:` — so a step's
`env` beats the job's — and the matrix variables last under their own
prefix where nothing can shadow them.

### What is in the image

`git`, `curl`, `ca-certificates`, `build-essential`, `python3` and `jq`,
on Debian bookworm. That is the list, and a job that needs anything else
installs it in a step. There is no Docker daemon and no Docker socket —
a runner that can talk to a daemon can escape its container — but there
is a `docker` that builds and runs images without one, with kaniko and
PRoot, for `docker build`, `docker run`, `push`, `login`, `pull`,
`images`, `tag` and `rmi`; [the GitHub runners page](/docs/github-runners/) says exactly what
it does and does not do, and it is the same tool in both images.

**A job may have 4096 processes at once.** The runner sets that ceiling
on the step's own process before it execs, so it counts only that job's
processes. It is deliberately generous — a parallel build legitimately
runs hundreds of compilers, and a bound that failed an honest `make
-j$(nproc)` would be a worse bug than the fork bomb it prevents — and it
exists because a job that spawns until something breaks otherwise takes
its whole host down. Past the ceiling, `fork` fails the way it does on
any busy machine, and the step sees that error.

### Timeouts and concurrency

`timeout-minutes` defaults to **360** (six hours) and must be a whole
number of at least 1 — a limit no run can meet is refused in the file.
The runner enforces it; a separate sweep fails a job five minutes past its timeout if the
runner has stopped reporting at all, so a task killed underneath you
still gets a verdict rather than sitting `running` forever.

There is also a **fleet ceiling**, which an operator sets and which is
six hours on our deployment. A job asking for more than that does not
run: the whole file is a **failed run**, with the reason

```
timeout-minutes: 720 exceeds this fleet's limit of 360
```

on a check named after the file. It is refused rather than quietly
clamped down to the ceiling, because a build told it may run for twelve
hours and stopped at six fails in a way its author cannot explain from
anything they wrote. Like the parse refusals, it is reported whatever the
event that found it — a file that only asks for `change` still gets told
about it on a push.

An organisation runs **4 jobs at once** by default. The limit is
enforced inside the statement that hands out work, so it holds across
the whole fleet, and it can be raised for an organisation by name.

## What happens on a push

1. You push a branch. Weft reads `.weft/` **at the commit you
   pushed**.
2. If this was a push to a branch other than the default branch, the
   in-flight run for that branch is **cancelled**, recorded as
   `cancelled` with the reason `superseded by <first 12 of the new sha>`.
   Its jobs' checks follow. A push to the **default branch never
   supersedes** anything: "was `main` green at 14:02" has to stay
   answerable. Deleting a branch cancels its runs too, with the reason
   `branch deleted`.
3. Each file that parses and asks for this event gets one run. **A
   second push of the same commit does not start a second run** — the
   existing one is found and left alone.
4. Every job in the run is written as a `queued` check on the commit,
   named `<workflow> / <job>`: `ci / test`, `ci / test (linux, 1.84)`.
5. The dispatcher claims jobs whose dependencies have passed, mints each
   one a token, and starts a task. The check goes to `running`, then to
   `passing` or `failing` when the job reports.
6. The run is over when nothing is left queued or running: `passed` if
   everything passed, `failed` if anything failed or was cancelled.

A **change** is the same, keyed on the patchset rather than the branch:
a new patchset cancels the previous patchset's runs, the run carries the
change key, and `WEFT_EVENT` is `change`.

### When the file is wrong

A refused file is a **failed run with a failing check named after the
file** — `.weft/ci.yml` — carrying the refusal, its line and its
hint. It is not silence. A workflow that quietly does not run looks
exactly like one that has not started yet, and somebody waits for it.

The same is true when a deployment has no runner configured: the run
fails at trigger time with a message naming what the operator has to
set. And when Weft itself could not read `.weft/` at the commit —
the store refused, say — the run fails with a check named for the
**directory**, `.weft`, carrying the store's answer and saying that
nothing ran: which files were there is exactly what could not be
learned, and a push whose CI failed to start must not look like a push
whose CI has not started yet.

### Changes pushed from a fork

A change whose commits come from a fork is recorded as **`blocked`**,
not run, and its check sits at `queued` rather than red — nothing is
wrong with the change, it is waiting on a person. Its workflow file was
written by the contributor, and running it would hand a stranger a
repository token and a machine. The run's reason says so:

```
this change comes from a fork; a maintainer has to approve its workflows before they run
```

A maintainer starts it with **Approve and run workflows**, on the
change's page beside its checks. The button appears only for people who
could land the change — the route is

```
POST /v1/orgs/{org}/repos/{repo}/changes/{change}/workflows/approve
```

and it takes the same `repo:write` as landing does. Deliberately not the
review-approval door beside it: `POST …/approve` is a review opinion,
which somebody with read access may hold, and holding an opinion must
not also start compute on our fleet. Two words, two authorizations.

**Approval is per tip, not per change.** It starts the workflows for the
change's *current* patchset, and a new patchset from the fork is blocked
again — because the file the maintainer read is not the file the next
push contains. This is what GitHub's "Approve and run" does, for the same
reason.

The response is `202` with the runs that now exist at that tip, read back
from the database rather than reported optimistically: the trigger may
legitimately have settled a run instead of starting one — an
organisation out of minutes, a file over the timeout cap — and a caller
told "running" about a run that is `blocked` would wait for a build that
is not coming. `409` if the change is not open, or if nothing is blocked
at its current tip; `404` if there is no such change, or it has no
patchsets.

**Branch on `blocked_reason`, never on the words.** Every run carries it:
`fork`, `budget` or `suspended`, and `null` unless the run is `blocked`.
The sentence in `error` is written for a person and will be rewritten;
this will not, and it is the only one of the two a client should read.
The distinction matters most here. An organisation's own refusals are
decided **before** the fork gate, so a fork change under an organisation
that is out of minutes or suspended is coded `budget` or `suspended`,
not `fork` — it is blocked for a reason no maintainer can approve away.
This route does not refuse such a change: it accepts the approval,
re-triggers, and honestly answers `202` with a run that is *still*
blocked, for the reason that was actually stopping it. That is the right
answer to give and a poor thing to have asked for, which is why the
dashboard offers the button only when `blocked_reason` is `fork`, and why
anything else building this UI should do the same.

Behind the button, the blocked run and its mirrored check row are deleted
before the real runs are created, so one run per workflow file per commit
still holds and no permanently-queued check is left holding the land
gate.

## Composed runs for a changeset

A [changeset](/docs/changesets/) is one review over changes in several
repositories, and a test that only ever sees one of them cannot say
whether the unit works. A file with `changeset` in its `on:` gets a run
where **every member repository is checked out**, each at the head that
changeset proposes for it.

```yaml
name: ci
on: [change, changeset]
jobs:
  test:
    steps:
      - name: Test
        run: make test
```

Each member is materialised under `$WEFT_WORKSPACE`, in a directory
named after its repository:

```
$WEFT_WORKSPACE/api    # the api member, at its latest patchset
$WEFT_WORKSPACE/web    # the web member, at its latest patchset
```

**Steps run in the job's own repository** — `$WEFT_WORKSPACE/<this
repo>` is the working directory every step starts in, so a file written
for `on: change` keeps working under `on: changeset` and the siblings are
simply there beside it. `WEFT_SHA`, `WEFT_REF` and `WEFT_CHANGE`
are this repository's member, as on a change run.

`WEFT_CHANGESET_MEMBERS` is the whole list, in the changeset's member
order, as JSON:

```json
[
  {"repo": "api", "change": "Iaa000001", "commit": "9e54f5f2…",
   "path": "/work/workspace/api"},
  {"repo": "web", "change": "Ibb000002", "commit": "3c1d90ab…",
   "path": "/work/workspace/web"}
]
```

`path` is absolute, so a script can `cd` to a sibling without knowing how
the workspace is laid out.

**Every other member is read with a token scoped to that member alone.**
The job's own repository is checked out with the job token as it always
was; each sibling is fetched with a fresh repository-read token minted
for *that one repository*, and all of them are revoked when the job
reports. There is deliberately no organisation-wide read token here: a
member's CI script is code its author wrote, and one token that could
read the whole organisation would let that script read repositories the
author cannot see. Like the job token, none of them is ever put in an
environment a step can read, or printed in a log.

**One run per member repository, per composition.** The composition is
the set of members and the commit each is at; every member repository
whose `.weft/` asks for `changeset` gets one run, so a changeset of
three repositories where two declare a composed workflow has two composed
runs. A new patchset on *any* member, or a member being added or removed,
is a new composition: the live composed runs of the old one are cancelled
with the reason

```
superseded by a new composition of changeset Ic5000001
```

and a fresh set is started. A member whose repository has been deleted
drops out of the changeset — its members list, its landing order and its
composition all leave it out — so the next patchset on a surviving member
is built as the combination the changeset now shows. A changeset with no
member left, or one with a member that has no patchset yet, cannot be
composed at all: nothing starts, and whatever is already running is left
alone.

**Verdicts land on the changeset, not on the commit.** A composed job's
check appears on the changeset and gates
[landing it](/docs/changesets/#composed-ci); it is not written to the
member commit's Checks tab. The per-change `on: change` runs are
untouched and still gate their own member — a file with `on: [change,
changeset]` produces the same check name from both events on the same
commit, and the per-change gate must not read the composed answer as the
member's own.

The consequence is worth saying out loud, because it is not what a person
expects: composing three repositories and then opening one of them shows
its push and change runs and nothing about the composition, which reads
as the composed build having never happened. So a member repository's
**Checks** tab carries a *Changeset builds* panel under its check rows —
the composed runs of that repository, each linking to its run page and to
the changeset that owns the verdict. It is filled from `GET
…/workflow-runs?event=changeset`, filtered server-side because the
composed runs of a busy repository would otherwise be paged out by its
pushes.

**One unapproved fork member holds the whole composition.** If any
member's change comes from a fork and no maintainer has approved that
tip yet, **every** member's composed run is `blocked` with
`blocked_reason` `fork` — not only the fork member's own. A composed job
is the one place where that has to be true: the job runs in a
maintainer's own repository, but it materialises the stranger's tree
beside it under `$WEFT_WORKSPACE`, and the maintainer's own script is
free to build it, test it, or execute it. Blocking only the fork
member's run would leave a stranger's code being run by three
repositories that never asked.

Approving the fork change's workflows — the same button and the same
[route](#changes-pushed-from-a-fork) on that change — releases the whole
composition, and the held runs start. A tip already approved stays
approved: recomposing does not put it back behind the button.

## Hosted-runner minutes

A hosted fleet is somebody's compute bill, so an organisation has a
budget of minutes and it can run out. How many is the operator's choice —
the reference deployment gives an organisation **2000** minutes per
rolling thirty days, and a self-hosted Weft can set any number or turn
metering off entirely, in which case everything below is inert there.

**How the number is arrived at.** Usage is counted **per job, rounded
up**: a job that ran for eleven seconds costs a minute, because a minute
is the smallest thing the fleet bills. Summing raw milliseconds and
rounding once at the end would let a thousand ten-second jobs cost almost
nothing, which is exactly the shape of a workload you would want to
notice. The window is a **rolling thirty days**, not a calendar month —
a calendar month hands every tenant the same reset instant, which is both
a stampede on the first and an obvious way to abuse the budget: burn the
allowance, wait for midnight, burn it again.

**A job still running counts from the moment it started.** The number
moves while builds run, which is what somebody watching the page expects
to see, and it means an organisation cannot hide its usage by keeping
everything in flight.

**GitHub Actions jobs count too.** A job that ran on this fleet from a
workflow kept on GitHub — `runs-on: weft`, see
[Weft runners for GitHub Actions](/docs/github-runners/) — draws from
the same budget, rounded up the same way and multiplied by its size:
`weft` at 1×, `weft-2x` at 2×, `weft-4x` at 4×. The billing page says
how much of the total those were.

**Where to read it.** Settings → Billing, in the organisation's
dashboard, and on `GET /v1/orgs/{org}/billing`, which carries
`ci_minutes_limit`, `ci_minutes_used` and `ci_minutes_remaining` (plus
`ci_suspended_reason` and `ci_suspended_at`, below).

**`null` is not zero.** A `null` limit means *unlimited* — no override on
the organisation and no deployment default, which is the right default
for a deployment paying its own compute bill. "0 minutes left" and "no
limit at all" are opposite facts and a panel that rendered them the same
way would be worse than showing nothing.

`ci_minutes_limit` is therefore **never `0`**: a budget of zero is read
as "no budget configured", so an unmetered organisation reports `null`
for both `ci_minutes_limit` and `ci_minutes_remaining`. The zero that
means *metered and out* is `ci_minutes_remaining: 0`, which is a real
state and the one worth rendering loudly. It is floored there rather than
going negative — a running job may take an organisation past its limit,
since nothing is killed for budget — so `used` can legitimately exceed
`limit` while `remaining` reads `0`.

**When the budget is gone,** a push does not fail and it does not run:
each workflow file gets a `blocked` run carrying, on a free organization
or a personal namespace,

```
this organisation has used its 500 hosted-runner minutes for the month
```

with the number that was actually configured — "for the month" there is
the rolling thirty days above; there is no reset date to wait for, and
the oldest minutes fall out of the window as they age — and, on a paid
organization whose pool and spend limit are both spent,

```
quota: hosted minutes are past this organization's pool and its spend limit — the job waits; raise the limit in Settings → Billing, or run it on your own runner
```

because a paid organization has a way out that costs money as well as
the one that costs none (see [Billing](/docs/billing/#the-spend-limit)).
It is refused at trigger
time, where there is still somebody to tell — a job that quietly never
got claimed leaves a build that looks like it has not started yet. The
dispatcher asks again before it claims a job, because an organisation can
cross its budget between queueing and claiming; a job that crossed it
while waiting is cancelled with the same reason and no task is launched.

**A running job is never killed for budget.** It is bounded by its
timeout, and the minutes it spends are counted. The budget decides what
starts, not what stops.

An organisation's allowance is set by an operator, not from the
dashboard: the deployment has a default for every organisation and an
operator can override it for one by name. If you run Weft yourself,
the statement to run is in `docs/deployment-aws.md` in the repository.

## What is refused for abuse

A hosted runner executes a `run:` line somebody wrote, on a machine we
pay for. The one thing that is worth real money to steal here is CPU, and
the thing people do with stolen CPU is mine cryptocurrency. Four separate
things stand in the way of that, deliberately, because every one of them
can be walked around on its own.

**1 — nothing can reach a mining pool.** All egress from the runner VPC
goes through a firewall with a domain **allowlist** and a default drop.
There is no pool to dial and no open proxy to dial it through. This holds
against a miner nobody has heard of and against one that arrives inside a
dependency, which is why it is first. It is also why a build that needs
an unlisted domain sees a dropped connection: the allowlist is the
product, not a bug in it.

**2 — the file is refused when you push it.** A workflow that names known
mining software as a command, or that carries a mining pool URL, does not
schedule anything at all. Both `run:` lines and `env:` values are read —
`run: ./m $POOL` says nothing on its own, so a check that read only
`run:` would be walked around by the first person who tried. The refusal
is the ordinary kind, with the file, the line and a hint:

```
mining software is not permitted on hosted runners
`xmrig` is mining software; hosted runners are for building and testing your code
```

The pool schemes are `stratum+tcp://`, `stratum+ssl://`, `stratum2+tcp://`
and `stratum+tls://`, matched anywhere on a `run:` line or in any `env:`
value — no build has a use for one, so the scheme alone is enough.

A miner name has to be **invoked as a command** for the run: check to
fire: `grep -rn xmrig .`, a step that writes `xmrig.log`, and a README
quoting this paragraph are all somebody working, and a refusal that
cannot tell those apart is one people learn to route around rather than
read. The line is not fully parsed as shell — leading `VAR=1`
assignments, and wrappers like `sudo`, `env`, `nice` and `timeout`, are
skipped to find the program, and that is as far as it goes on purpose. In
an `env:` **value** only the URL half applies: a value containing the
word `xmrig` is somebody naming a file.

This layer is a refusal, not a detector. `curl -o m https://…/x && ./m`
walks straight past it, which is exactly why there are four layers.

**3 — a running step is killed.** While a step runs, the processes in its
group are sampled every two seconds and read from `/proc`. A process is a
miner if its **program** is one — its `comm`, or the basename of
`argv[0]`, never an argument — or if a pool URL appears anywhere in its
`argv`, since a renamed binary still has to be told where to send its
shares. When one is found the whole process group is killed and the job
ends `failed` with

```
✗ Build stopped: mining software detected: xmrig (3s)
```

in the log and `mining software detected: xmrig` as the job's error.

Nothing here is measured: there is no CPU heuristic on purpose, because a
release build with `-j8` looks exactly like a miner to one, and a compile
flagged as abuse is a person locked out of their own forge.

**4 — the organisation is suspended.** A verdict that reports abuse
switches hosted workflows off for the whole organisation, not just that
repository: everything it has running, in every repository it owns, is
cancelled, and every later trigger is `blocked` with

```
hosted workflows are suspended for this organisation: mining software detected: xmrig
```

— the runner's own sentence, because it names what was found. It is
recorded in the audit log as `workflow.suspended`. The **first** reason
stands: a second offence does not overwrite the explanation somebody is
in the middle of acting on with an identical-looking one bearing a later
timestamp.

The reason and the time also appear on the billing view, as
`ci_suspended_reason` and `ci_suspended_at`, so the page a member goes to
when their builds stop says why.

**Clearing a suspension is an operator action, by SQL.** There is no
route and no button, because there is no operator role on this server to
hang one off and inventing one here would be a security surface built in
passing. If you run Weft yourself, the statement is in
`docs/deployment-aws.md`; on a hosted deployment, ask whoever operates
it.

### The audit trail for both of these

Two events, and between them they are the whole record of a run that was
held and what happened next:

| Action | Recorded when | Details |
|---|---|---|
| `workflow.suspended` | a verdict reports abuse | `abuse`, `reason`, `job`, `run` |
| `workflow.approved` | somebody approves a fork's workflows | `change_key`, `commit` (the tip approved), `files` (the workflow files that were being held) |

`workflow.approved` carries the approving principal, like every audit
row, and it matters more than it looks: approving **deletes** the blocked
placeholder runs and their check rows, so nothing in `workflow_runs`
afterwards remembers that these files were ever held. This row is the
only surviving record that they were, and who let them go. It names the
files for the same reason — "approved the workflows" without saying which
is not a trail anybody can audit.

## Self-hosted runners

A job that says `runs-on: [self-hosted]` runs on a machine **you**
registered — your laptop, a box under a desk, an autoscaling group, a
GPU host that could never be a line item on our fleet. Weft keeps
everything else: the file, the checks, the log, the run page, the land
gate. What changes is whose CPU it is, and therefore who is responsible
for what the job can reach.

A runner only ever makes **outbound** calls. It asks for work, and it is
handed a job or told there is nothing. Nothing of ours connects to it,
nothing has to be port-forwarded, and it does not need a public address.
[Running a runner](/docs/self-hosted-runners/) is the operator's side of
this page: the binary, a systemd unit, and how to isolate it.

### The organisation's policy

Under **Settings → Runners**, which needs `org:admin` like Billing:

| | |
|---|---|
| **Weft-hosted runners** | `allowed` (default) or `disabled` |
| **Self-hosted runners** | `all` (default), `selected` — naming the repositories that may use them — or `disabled` |

An organisation that only trusts its own machines sets hosted to
`disabled`, and a `runs-on: ubuntu-latest` file is then refused at
trigger time with

```
hosted runners are disabled for this organisation; use runs-on: [self-hosted, …]
```

and a self-hosted job in an organisation that has not enabled them, or
in a repository that is not one of the `selected` ones, is refused with

```
self-hosted runners are not allowed for this repository (organisation policy)
```

Both are **failed runs**, not blocked ones: nothing lifts by itself, and
somebody has to edit either the file or the settings. That is the whole
point of refusing at trigger time. A job queued against a pool it can
never reach sits there looking like a build that has not started yet,
which is the single most common thing people ask about somebody else's
self-hosted setup.

Reading and writing the policy over the API:

```
GET   /v1/orgs/{org}/runner-policy
PATCH /v1/orgs/{org}/runner-policy
```

```json
{ "hosted": "allowed", "self_hosted": "selected", "self_hosted_repos": ["builds"] }
```

`PATCH` takes any subset of those three keys, needs `org:admin`, answers
`422` on a value outside the sets above, and lands in the audit log as
`runner_policy.updated`.

### Runner groups

A runner belongs to exactly one **group**, and a group decides which
repositories may send it work. Every organisation has a `default` group,
created the first time one is needed; you can make others.

| | |
|---|---|
| **Repository access** | `all` repositories in the organisation, or `selected` ones by name |
| **Allow public repositories** | off by default |

**Public repositories are excluded until you say otherwise, and that
default is the important one.** Anybody may fork a public repository and
open a change; the change carries its own `.weft/*.yml`; and a
workflow file is a shell script. A group that admits public repositories
is a group whose machines will, sooner or later, be asked to run a
stranger's code as the runner's own user, on your network. The
[fork-approval gate](#changes-pushed-from-a-fork) still stands in front
of that — a maintainer has to press **Approve and run workflows** for
each new tip — but a gate a tired person clicks through is one layer, not
two, so the group starts closed.

```
GET    /v1/orgs/{org}/runner-groups
POST   /v1/orgs/{org}/runner-groups        {"name", "repo_access"?, "allow_public"?, "repos"?}
PATCH  /v1/orgs/{org}/runner-groups/{id}   any subset of the same keys
DELETE /v1/orgs/{org}/runner-groups/{id}
```

Reads need organisation membership, writes need `org:admin`. A duplicate
name is `409`. Deleting a group is `204` and **moves its runners to the
default group** rather than orphaning them; the default group itself
cannot be deleted and cannot be renamed (`422`). The three writes are
audited as `runner_group.created`, `.updated` and `.deleted`.

### Registering a machine

An organisation admin mints a **registration token**:

```
POST /v1/orgs/{org}/runners/registration-token   {"group": "default"}
```

```json
{ "token": "weftg_…", "expires_at": 1800000000000, "group": "default",
  "command": "weft-runner register --url https://weft.sh --token weftg_…" }
```

It is **single-use and expires in one hour**. It is not the runner's
credential: it is the right to obtain one, once. The machine exchanges
it, and from then on holds a long-lived credential of its own:

```bash
weft-runner register --url https://weft.sh --token weftg_… --labels gpu,cuda-12
weft-runner run
```

`register` writes `.runner` in its working directory (mode `0600`),
holding the URL, the runner's id and its credential, and prints one line:

```
registered build-01 as rnr_… in group default with labels [self-hosted, linux, x64, gpu, cuda-12]
```

`run` then loops: ask for a job, run it, ask again. `--name` defaults to
the machine's hostname and `--dir` to the current directory.

**Rotating a credential is re-registering.** Running `register` again
with the same name replaces that runner: the old credential stops working
immediately, the runner keeps its identity in the list, and there is no
separate rotation dance to remember. Registration is audited as
`runner.registered`, the token mint as
`runner.registration_token.created`.

### Labels

A runner's labels are what it offered plus what the server always adds:

| Added always | `self-hosted`, the OS (`linux`, `macos`, `windows`), the architecture (`x64`, `arm64`) |
|---|---|
| Added by you | anything from `--labels`, lowercase |

A job may run on a runner when **every label in its `runs-on` is one of
the runner's**, the runner's group admits the repository, the
organisation's policy admits self-hosted for that repository, and the
runner has not been removed. `runs-on: [self-hosted]` on its own
therefore means *any* machine the first three rules allow, and
`[self-hosted, linux, gpu]` narrows it.

Labels are matched, never invented. If no runner could ever satisfy the
list, the run is refused at trigger time rather than queued:

```
no runner with labels [self-hosted, gpu] is registered for this repository
```

and if the repository is allowed self-hosted runners but no group will
serve it — most often a public repository and no group with **allow
public repositories** turned on:

```
no runner group admits this repository; add it to a group under Settings → Runners
```

### What a self-hosted job gets

Everything a hosted job gets from Weft, and nothing a hosted job gets
from the fleet:

- The same **repository-read token minted for that one job**, expiring
  with the job and revoked the moment it reports. The runner holds it;
  steps never see it.
- The same environment — `CI`, `WEFT_JOB`, `WEFT_SHA`,
  `WEFT_REF`, `WEFT_EVENT`, `WEFT_CHANGE`, the matrix variables
  — under the same rules, and the same `bash -e -c` per step.
- The same log, streamed live to the same run page, and the same check
  on the commit under the same name. Nothing downstream can tell the two
  apart, which is the point.

The job runs in a fresh working directory under the runner's own
directory, which is removed when the job ends. It runs **as the user the
runner runs as**, directly on the machine — so the process ceiling and
the egress allowlist described above are properties of *our* fleet and
not of yours, and the isolation between one job and the next is whatever
you built. [Running a runner](/docs/self-hosted-runners/) is about
exactly that.

Because there is no container, there is nothing for `image:` to name. A
self-hosted job asking for one is refused at trigger time:

```
image "rust:1.83" is not available on self-hosted runners; steps run directly on the machine
```

Whatever is on that machine's `PATH` is what the job gets, which is the
trade: you choose the toolchain by building the machine, not by naming a
tag in the file.

**Ephemeral runners** (`--ephemeral`) take one job and exit, and the
server removes them the moment that job reaches a terminal state. It is
the honest way to get a clean machine per job, and it is what to reach
for if you are autoscaling.

### The runner list, and disappearing machines

`GET /v1/orgs/{org}/runners` — and the same table under Settings →
Runners — shows every runner with its labels, its group, whether it is
ephemeral, when it was last seen, and the job it is running:

| State | Means |
|---|---|
| `busy` | a running job is assigned to it |
| `online` | it called in within the last 60 seconds |
| `offline` | it did not |

State is **derived on read, never stored**, so a machine that loses power
is `offline` a minute later without anything having to notice.

A runner that stays away is eventually removed for you: **14 days**
unseen for an ordinary runner, **1 day** for an ephemeral one. That is
housekeeping, not a policy — a laptop that was registered for an
afternoon should not be in the list forever.

Removing one yourself is `DELETE /v1/orgs/{org}/runners/{id}`, or
**Remove** in the table. Its credential is dead from that moment; the
process finds out on its next call, prints `this runner has been removed;
register it again` and exits. A job that was running on it is **failed**,
with

```
runner removed while the job was running
```

and it is *not* retried. The dispatcher retries a job whose runner was
lost, because losing a runner is an accident; removing one is a decision,
and quietly re-running the job on another of your machines is not what
the person who pressed the button asked for. The removal is audited as
`runner.removed`.

### What is the same, and what is not

**Minutes are not metered.** [Hosted-runner
minutes](#hosted-runner-minutes) count hosted jobs and only hosted jobs —
it is your hardware and your electricity bill. So a file whose jobs are
all self-hosted runs when the organisation is out of minutes, and runs
when the organisation's hosted workflows are suspended: those two
refusals apply only to files that contain a hosted job. A mixed file is
refused as a whole, because it contains one.

**The fork gate applies to both pools**, and matters more here. A change
from a fork is `blocked` until a maintainer approves it, per tip, exactly
as [described above](#changes-pushed-from-a-fork). On our fleet that
protects our bill; on yours it protects your machine.

**A miner is still killed, and your organisation is not suspended.** The
[mining watch](#what-is-refused-for-abuse) runs wherever the runner runs:
the file is refused when you push it, and a step caught running a miner
has its whole process group killed and the job failed with the same
sentence,

```
✗ Build stopped: mining software detected: xmrig (3s)
```

because a stranger's change mining on *your* hardware is the thing the
watch exists to stop. What does **not** happen is the fourth layer:
hosted workflows are not switched off for the organisation, because
there is no compute bill of ours being stolen. The event is still
recorded as `workflow.abuse`, carrying `pool: self_hosted`, so it is in
the audit log for you to act on.

The egress allowlist is not there either. Ours is a firewall in front of
our VPC; your runner's network is yours.

## Checking a file before you push

`GET /v1/orgs/{org}/repos/{repo}/workflows?at={rev}` reads `.weft/`
at any rev and tells you what would run — the expanded jobs, in start
order, with their `needs` — or what is wrong with the file, with a line
number and a hint. `at` defaults to `HEAD`.

```bash
curl -sS "$WEFT_URL/v1/orgs/$ORG/repos/$REPO/workflows?at=my-branch" \
  -H "Authorization: Bearer $TOKEN"
```

```json
{ "workflows": [
  { "file": ".weft/ci.yml", "ok": true, "name": "ci", "on": ["push", "change"],
    "jobs": [ { "key": "test", "job": "test", "matrix": {}, "needs": [] } ] }
] }
```

An `ok: false` entry carries a `problems` array instead, each with
`line`, `key`, `message`, `hint` and a pre-rendered `text`. It costs
nothing and it is the difference between learning about `uses:` now and
learning about it from a red push.

## The run page

Every check a hosted job writes carries a **Details** link to that run's
page in the dashboard, at `/<org>/<repo>/checks/runs/<run id>`. It shows
the run's state, the commit and branch it is about, a link to the change
if it came from one, the run's own error verbatim when there is one —
the refused line of YAML, the cycle in `needs:` — and one panel per job.
Selecting a job shows its log; while the job is live the page reads the
same SSE stream described below, so the output arrives as it is written
rather than on a refresh. A viewer with `repo:write` gets a **Cancel
run** control there, which is the `cancel` route below. A job that ran on
one of your own machines is labelled with the runner that took it.

There is no page that lists a repository's runs. The Checks tab lists
the *checks*, hosted ones beside everybody else's, and a row is how you
reach its run.

## Runs, jobs and logs over the API

Everything below needs `repo:read`, except cancel, which needs
`repo:write`.

| Route | |
|---|---|
| `GET /v1/orgs/{org}/repos/{repo}/workflow-runs` | The repository's runs, newest first, each with its jobs. `?limit=` defaults to 20 and is **clamped** to 1–100 rather than refused. `?commit_sha=`, `?change_key=` and `?event=` (`push`, `change`, `changeset`) narrow it, in the query rather than after the limit |
| `GET …/workflow-runs/{id}` | One run and its jobs |
| `POST …/workflow-runs/{id}/cancel` | Stop a run. `409` if it is not running any more. Needs `repo:write` — a job's own token is `repo:read` and cannot cancel anything |
| `GET …/workflow-jobs/{id}/log` | The log as `text/plain`: complete if the job is over, so far if it is not |
| `GET …/workflow-jobs/{id}/log/stream` | The same log as server-sent events, tailing until the job ends |

A run carries `id`, `file`, `name`, `commit_sha`, `ref_name`, `event`,
`change_key`, `state`, `error`, `blocked_reason`, timestamps, and `jobs`.
Run `state` is one of `running`, `passed`, `failed`, `cancelled`,
`blocked`; `blocked_reason` is `fork`, `budget` or `suspended`, and
`null` for every state but `blocked`. A job carries `id`, `job_id`,
`key`, `matrix` (an object), `state`, `attempts`, `error`, `detail_url`,
`log_chunks` and timestamps; job `state` is one of `queued`, `running`,
`passed`, `failed`, `skipped`, `cancelled`.

A job also carries `pool` (`hosted` or `self_hosted`), the `labels` its
`runs-on` asked for, and `runner` — `{"id", "name"}` for a job that ran
on one of your machines, `null` otherwise. Branch on `pool`, not on
whether `runner` happens to be set: a self-hosted job that has not been
claimed yet has no runner either.

**`commit_sha` and `change_key` narrow the query, not the page.** The
filtering happens in the database, inside `limit`, which is the whole
point of having them: `?commit_sha=<sha>&limit=1` gives you that commit's
run, where fetching the newest 20 and filtering in the client gives you
nothing at all on a repository busy enough to have pushed 20 times since.
A panel that filters a window is a panel that loses its own controls
exactly when the repository is busiest.

A run or job belonging to another repository answers `404`, not `403`.
An id that resolves differently for a stranger is an existence oracle.

### The live log

`…/log/stream` is an `EventSource` feed with three event types:

| Event | Data |
|---|---|
| `queued` | `{}`, once, if the job has not started yet |
| `chunk` | `{"text": "…"}` — the next slice of output |
| `done` | `{"state": "passed"}` — the feed then closes |

The text arrives as JSON rather than raw, because an SSE `data:` field
cannot carry a trailing newline and a log whose lines quietly ran
together would disagree with the plain `…/log` route beside it. The feed
polls the same stored chunks that route reads, so the two can never
disagree, and it is held open for at most six hours.

Logs live in object storage under a lifecycle rule and age out — **90
days** on the reference AWS deployment (`ci_log_retention_days`). They
are not permanent records.

## How verdicts reach the rest of Weft

Each job mirrors itself into one check row on its commit, named
`<workflow name> / <job key>` and linking back to the run it came from
at `/<org>/<repo>/checks/runs/<run id>`, and that is the whole
integration. A workflow file we refused gets a row of its own, named for
the file, and a `.weft/` we could not read gets one named for the
directory; each links to the run that carries the reason — the row is
the only thing on the commit page that says why nothing ran. The
Checks tab, the change under review, the land queue and the README badge
were all built for verdicts other people's build systems reached, and
they need to know nothing about this one.

A hosted row carries `provider: "weft"`, which is the server saying
*we wrote this*: the intake stamps `provider: "intake"` as a constant,
so nothing posted from outside can claim it. That is what lets the
**Details** link navigate inside the dashboard instead of opening a new
tab the way a link to somebody else's build system does.

So everything on [CI integration](/docs/ci-integration/) applies
unchanged: a `failing` check blocks the land queue, a check you have
[made required](/docs/ci-integration/#making-a-check-required) must go
green before a change may land, and `ci / test` from a hosted workflow
and `ci/tests` from Buildkite sit in the same list under the same rules.

Required-check names are matched against the check's name, so the name
to require is the mirrored one — `ci / test`, including the spaces.
Note that a **matrix cell's name contains its values**, so requiring
`ci / test (linux, 1.84)` requires that cell and adding an axis value
does not silently add a requirement.

A **composed run goes somewhere else entirely**. Its jobs mirror into the
changeset's own list of checks, under the same `<workflow> / <job>` name
and linking to the same run page, and never onto the commit. Nothing on
the commit page changes when a composed job reports, and the changeset's
[land gate](/docs/changesets/#composed-ci) is the only thing that reads
it.

## What is not here yet

Said plainly, with no dates attached:

- **No re-run button, and no re-run route.** Push again, or cancel and
  push again. (The dispatcher retries a job whose *runner* was lost;
  that is a different thing, and it is not something you can ask for.)
- **No artifacts and no caches.** Nothing is kept from a job but its log
  and its verdict. A job that needs a dependency downloads it.
- **No annotations.** A verdict and a log, not marks on the diff.
- **No way to clear a suspension from the product.** Suspending an
  organisation for abuse is automatic; switching it back on is an
  operator running SQL, because there is no operator role here to give a
  route to.
- **No images but `default`.** The parser takes `image:` so that a
  workflow written today reads correctly later; the fleet runs one image
  today.
- **No list of runs in the dashboard.** A run has a page and its log
  tails live there, but the way to it is a check row's **Details** link
  or, for a composed run, the Changeset builds panel on the Checks tab;
  there is no screen that pages through a repository's push runs. `GET
  …/workflow-runs` is that list, and it is the API only.
- **No scheduled or manual triggers.** `push`, `change` and `changeset`
  are the three doors, and all three are something that happened to the
  repository.
- **No composed clone URL, and no workspace page.** A composed run
  materialises the members on the runner; there is no way yet to check
  the same set out on your own machine, and no screen that lists a
  changeset's members at their proposed heads.
- **No conditions, no job outputs, no service containers.**
