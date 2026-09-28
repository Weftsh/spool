# Workflows

Put a YAML file in `.weft/` and pushing runs it. Each job runs on a
machine your organization registered — a **runner** — its output is a log
you can read while it is still being written, and its verdict arrives on
the commit's **Checks** tab as a check named after the job, where it
gates landing exactly like a check posted by any other CI.

There is no hosted fleet behind a Spool server. Every job runs on a
runner somebody in your organization registered with `weft-runner`, and
a push with no runner that can take its jobs fails at once and says so.
[Running a self-hosted runner](self-hosted-runners.md) is the operator's
side of this page.

This is one of two ways to get a verdict here. The other is
[CI integration](ci-integration.md): your own CI, wherever it runs,
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
patchset of every change, one job called `test` runs two commands on one
of your runners, in a fresh checkout of your repository at that commit,
and a check called `ci / test` appears on the commit. The job names no
`runs-on`, so any runner your organization registered may take it.

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

`.weft/site.yml` and `.weft/site.yaml` are skipped. They configure a
static site on Weft's hosted service, which Spool does not serve, and a
repository moved here keeps them without seeing them refused as broken
workflows.

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
[changeset](changesets.md) this repository is a member of: the job
runs with every member repository checked out beside this one. It is a
different event from `change`, not a wider one — `on: [change,
changeset]` asks for both, and a repository whose file says only
`changeset` runs nothing on its own changes. See [Composed runs for a
changeset](#composed-runs-for-a-changeset).

**A job** takes `name`, `needs`, `runs-on`, `env`, `strategy`,
`timeout-minutes` and `steps`. `image` and `container` are read so that
they can be refused with a reason; see [below](#image-and-container).

**A step** takes `name`, `run` and `env`. `run` is required and is a
shell command; a step with nothing to run is refused.

### `runs-on`

`runs-on` says which of your runners may take the job. Every runner
carries the label `self-hosted`, so that label is the one every
`runs-on` must contain:

```yaml
# no runs-on at all                 # any runner your organization registered
runs-on: self-hosted                # the same thing
runs-on: [self-hosted]              # the same thing
runs-on: [self-hosted, linux, gpu]  # …narrowed to runners with these labels
```

Leaving `runs-on` out is the same as writing `[self-hosted]`.

Every other entry in the list is a **label**, and the job goes to a
runner whose own labels contain all of them. A label is lowercase
letters, digits, dot, dash or underscore, at most 64 characters — the
same thing you pass to `weft-runner register --labels`. Labels are
**lowercased once, here**, and duplicates dropped, so `GPU` in the file
matches `gpu` on the runner rather than being a job that never runs and
an operator with nothing to look at; the order you wrote is kept,
because that order is what the refusals below print back at you. What
the labels mean is in [Runners](#runners) below.

A `runs-on` without `self-hosted` names a machine somebody else hosts —
`ubuntu-latest`, `macos-latest`, a pasted Actions file — and is refused
on its line, with the fix in the hint:

```
`runs-on: ubuntu-latest` names a hosted runner, and this server has none
```

The same goes for a list without `self-hosted`, such as `[linux, gpu]`.
With `self-hosted` in the list, every other entry is only a label:
`[self-hosted, ubuntu-latest]` asks for a runner you registered with the
label `ubuntu-latest`. An empty `runs-on: []` is refused too, because it
asks for no runner at all.

Different jobs in the same file may ask for different labels. A file
whose `build` runs on any runner and whose `gpu-test` asks for
`[self-hosted, gpu]` is ordinary, and `needs` between them works as it
does anywhere else.

### `image` and `container`

A job runs its steps directly on the runner's machine, as the account
the runner runs as. There is no container to put an image in, so a job
that names one is refused — whether it says `image: rust:1.83`,
`container: node:18` or the block form with an `image:` key. The run
fails at trigger time with

```
image "rust:1.83" is not available on self-hosted runners; steps run directly on the machine
```

Ignoring the line would be worse than refusing it: the build would run,
against whatever toolchain the machine has, and report a verdict about
the wrong thing. You choose the toolchain by building the machine, not by
naming a tag in the file. `image: default` is accepted and means the
same as saying nothing.

A container block's `credentials`, `ports`, `volumes` and `options` are
refused when the file is parsed.

### What is refused, and what to write instead

| You wrote | What happens |
|---|---|
| `uses:` on a step | Refused: "this forge does not run Actions". Run the command directly with `run:` |
| `runs-on:` without `self-hosted` | Refused on its line. Write `[self-hosted]`, `[self-hosted, <label>, …]`, or leave `runs-on` out |
| `image:` or `container:` naming an image | The run fails at trigger time. Install the toolchain on the runner |
| `if:` on a job | Refused, "not supported yet". A job runs when everything it `needs` has passed, and that is the only condition there is |
| `outputs:`, `defaults:` on a job | Refused, not supported yet |
| `strategy.fail-fast`, `strategy.max-parallel` | Refused. Both are scheduler behaviour that does not exist here, and accepting them would be a lie |
| `env:` at the top level | Refused. Job `env:` and step `env:` are the whole of it |
| `shell:` on a step | Refused — see [the job environment](#the-job-environment) for what a step actually runs under |

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

Each cell is a separate job with its own working directory, its own log
and its own check. A cell's name — in the run, in the log and in the
check — is `id (v1, v2)`, values in the order the axes were declared:
`test (linux, 1.84)`. Every cell inherits its job's `runs-on`: a matrix
expands what runs, not where.

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

## Runners

A runner is a machine — a build box, a VM, a laptop, an autoscaling
group — running `weft-runner`, the agent that asks the server for work.
It only ever makes **outbound** calls: it asks for a job and is handed
one or told there is nothing. Nothing connects to it, nothing has to be
port-forwarded, and it does not need a public address. The server keeps
everything else: the file, the checks, the log, the run page, the land
gate.

A runner may take a job when all of these hold:

1. **every label in the job's `runs-on` is one of the runner's**;
2. **the runner's group admits the repository**;
3. **the organization's policy admits self-hosted runners for the
   repository**;
4. the runner has not been removed.

The admission rules are checked again at the moment a runner claims the
job, not only when the push arrives, because a job can sit in the queue
across a settings change.

### The organization's policy

Under **Settings → Runners**, which needs `org:admin`:

| | |
|---|---|
| **Self-hosted runners** | `all` (default), `selected` — naming the repositories that may use them — or `disabled` |

Since every job runs on a registered runner, `disabled` switches
workflows off for the organization, and `selected` switches them off for
every repository not on the list. A push to such a repository fails its
runs with

```
self-hosted runners are not allowed for this repository (organisation policy)
```

It is a **failed run**, not a blocked one: nothing lifts by itself, and
somebody has to edit the settings. That is the point of refusing at
trigger time. A job queued for a runner it can never reach sits there
looking like a build that has not started yet.

Reading and writing the policy over the API:

```
GET   /v1/orgs/{org}/runner-policy
PATCH /v1/orgs/{org}/runner-policy
```

```json
{ "self_hosted": "selected", "self_hosted_repos": ["builds"] }
```

`PATCH` takes either or both keys, needs `org:admin`, answers `422` on a
value outside the set above, and lands in the audit log as
`runner_policy.updated`. `self_hosted_repos` is a list of repository
names and is replaced whole; `[]` clears it.

### Runner groups

A runner belongs to exactly one **group**, and a group decides which
repositories may send it work. Every organization has a `default` group,
created the first time one is needed; you can make others — a group of
GPU machines only the ML repositories may use, say.

| | |
|---|---|
| **Repository access** | `all` repositories in the organization (default), or `selected` ones by name |

```
GET    /v1/orgs/{org}/runner-groups
POST   /v1/orgs/{org}/runner-groups        {"name", "repo_access"?, "repos"?}
PATCH  /v1/orgs/{org}/runner-groups/{id}   any subset of the same keys
DELETE /v1/orgs/{org}/runner-groups/{id}
```

Reads need organization membership, writes need `org:admin`. A duplicate
name is `409`. Deleting a group is `204` and **moves its runners to the
default group** rather than orphaning them; the default group itself
cannot be deleted and cannot be renamed (`422`). The three writes are
audited as `runner_group.created`, `.updated` and `.deleted`.

### Registering a machine

An organization admin mints a **registration token**, under **Settings →
Runners → Add a runner** or over the API:

```
POST /v1/orgs/{org}/runners/registration-token   {"group": "default"}
```

```json
{ "token": "weftg_…", "expires_at": 1800000000000, "group": "default",
  "command": "weft-runner register --url https://spool.example.com --token weftg_…" }
```

It is **single-use and expires in one hour**. It is not the runner's
credential: it is the right to obtain one, once. The machine exchanges
it, and from then on holds a long-lived credential of its own:

```bash
weft-runner register --url https://spool.example.com --token weftg_… --labels gpu,cuda-12
weft-runner run
```

The URL in `command` is the server's own public URL. `register` writes
`.runner` in its working directory (mode `0600`), holding the URL, the
runner's id and its credential, and prints one line:

```
registered build-01 as 01hx… in group default with labels [self-hosted, linux, x64, gpu, cuda-12]
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
| Added by you | anything from `--labels`, lowercased |

`runs-on: [self-hosted]` — or no `runs-on` at all — therefore means *any*
runner the group and the policy allow, and `[self-hosted, linux, gpu]`
narrows it.

Labels are matched, never invented. If no runner could ever satisfy the
list, the run is refused at trigger time rather than queued:

```
no runner with labels [self-hosted, gpu] is registered for this repository
```

A repository with no runners at all gets the same sentence, with
`[self-hosted]` in it. And if the repository is allowed self-hosted
runners but no group will serve it:

```
no runner group admits this repository; add it to a group under Settings → Runners
```

A runner that is registered but switched off still counts: the job is
queued and waits for it, rather than being refused. The refusal is for a
label set no runner in an admitting group has.

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

**Ephemeral runners** (`--ephemeral`) take one job and exit, and the
server removes them the moment that job reaches a terminal state. It is
the honest way to get a clean machine per job, and it is what to reach
for if you are autoscaling.

Removing one yourself is `DELETE /v1/orgs/{org}/runners/{id}`, or
**Remove** in the table. Its credential is dead from that moment; the
process finds out on its next call, prints `this runner has been removed;
register it again` and exits. A job that was running on it is **failed**,
with

```
runner removed while the job was running
```

and it is *not* retried. A job whose runner simply went quiet — the
machine lost power mid-build — is different: once its lease runs out,
another runner that matches may claim it and run it again. Losing a
runner is an accident; removing one is a decision, and quietly re-running
the job on another of your machines is not what the person who pressed
the button asked for. The removal is audited as `runner.removed`.

## The job environment

The job runs in a fresh working directory under the runner's `--dir`,
which is removed before the job starts and again when it ends. It runs
**as the account the runner runs as**, directly on the machine. The
isolation between one job and the next is that directory and nothing
more — a step can write outside it, leave a process running or start a
daemon — so what else the job can reach is whatever you built the
machine to allow. [Running a runner](self-hosted-runners.md#isolating-it)
is about exactly that.

What the job gets from the server:

- The repository, checked out at the pushed commit, in the working
  directory every step starts in. The fetch is the **one ref** the job
  is about, with `--no-tags`.
- A **repository-read token minted for that one job**, held in the
  runner's own environment and never passed to a step or written to the
  checkout's `.git/config`. It expires with the job and is **revoked the
  moment the job reports its verdict**.
- The log, streamed live to the run page, and a check on the commit.

Every step runs as **`bash -e -c '<your run block>'`** in the checkout
(`sh -e -c` on a machine with no `bash` on the step's `PATH` — install
bash on your runners), with a scrubbed environment — a step does **not** inherit whatever the
runner's own environment held. So each step is one bash script: `-e`
means a multi-line `run:` block stops at its first failing command, and
`set +e` turns that off if you want it to. There is **no `pipefail`**
unless you set it yourself, so `a | b` reports `b`'s status and a
failure in `a` passes silently — put `set -o pipefail` at the top of the
block if that matters.

Steps run in order and stop at the first failure. The log lists the ones
that never ran, so a reader scrolling to the bottom of a failed job can
see what did not get a chance.

Whatever is on the machine's `PATH` is what the job gets. A job that
needs a tool the machine does not have installs it in a step, or you
install it on the machine.

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

### The process ceiling

**A job may have 4096 processes at once.** The runner sets that ceiling
(`RLIMIT_NPROC`) on the step's own process before it execs. It is
deliberately generous — a parallel build legitimately runs hundreds of
compilers, and a bound that failed an honest `make -j$(nproc)` would be a
worse bug than the fork bomb it prevents — and it exists because a job
that spawns until something breaks otherwise takes the whole machine
down. Past the ceiling, `fork` fails the way it does on any busy machine,
and the step sees that error. The runner's operator can lower it with
`STRATUM_RUNNER_MAX_PROCS`.

The kernel does not apply this limit to root. A runner started as root
says so when it starts, and its jobs have no ceiling at all — one more
reason to run it under an account of its own.

### Timeouts

`timeout-minutes` defaults to **360** (six hours) and must be a whole
number of at least 1 — a limit no run can meet is refused in the file.
The runner enforces it. If the runner stops reporting altogether, the
server fails the job five minutes past its timeout with

```
timed out after 360 minutes and the runner did not report back
```

so a machine that died underneath a build still gives it a verdict
rather than leaving it `running` forever.

There is also a **ceiling**, which whoever runs the server sets with
`STRATUM_RUNNER_MAX_TIMEOUT_MINUTES`, and which defaults to six hours. A
job asking for more than that does not run: the whole file is a **failed
run**, with the reason

```
timeout-minutes: 720 exceeds this server's limit of 360
```

on a check named after the file. It is refused rather than quietly
clamped down to the ceiling, because a build told it may run for twelve
hours and stopped at six fails in a way its author cannot explain from
anything they wrote. Like the parse refusals, it is reported whatever the
event that found it — a file that only asks for `change` still gets told
about it on a push.

There is no concurrency limit on the server. How many jobs run at once
is how many of your runners are free.

## What happens on a push

1. You push a branch. The server reads `.weft/` **at the commit you
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
5. A runner that may take a job — its labels, its group and the policy
   all admit it — claims it once every job it needs has passed, and is
   given a token for it. The check goes to `running`, then to `passing`
   or `failing` when the runner reports.
6. The run is over when nothing is left queued or running: `passed` if
   everything passed, `failed` if anything failed or was cancelled.

A **change** is the same, keyed on the patchset rather than the branch:
a new patchset cancels the previous patchset's runs, the run carries the
change key, and `WEFT_EVENT` is `change`.

### When the file is wrong, or nothing can run it

A refused file is a **failed run with a failing check named after the
file** — `.weft/ci.yml` — carrying the refusal, its line and its
hint. It is not silence. A workflow that quietly does not run looks
exactly like one that has not started yet, and somebody waits for it.

The same is true of a file that parses but that nothing on this server
can run. These are checked in this order, and the run carries the first
one that applies:

1. a job names an image — `image "…" is not available on self-hosted runners; …`
2. the organization's policy does not admit self-hosted runners for the
   repository — `self-hosted runners are not allowed for this repository (organisation policy)`
3. no runner group admits the repository — `no runner group admits this repository; …`
4. no registered runner has a job's labels — `no runner with labels […] is registered for this repository`

Each is somebody's decision — the file's author, the organization's
admins, whoever runs the runners — and the first thing that is wrong is
the one worth telling. All of them settle the run as `failed`, not
`blocked`: somebody has to edit the file or the settings, and a check
that says "waiting" for something that will never happen is the failure
this whole family exists to avoid.

And when the server itself could not read `.weft/` at the commit — the
store refused, say — the run fails with a check named for the
**directory**, `.weft`, carrying the store's answer and saying that
nothing ran: which files were there is exactly what could not be
learned, and a push whose CI failed to start must not look like a push
whose CI has not started yet.

### Changes pushed from a fork

A change whose commits come from a [fork](forks.md) is recorded as
**`blocked`**, not run, and its check sits at `queued` rather than red —
nothing is wrong with the change, it is waiting on a person. Its
workflow file was written by the contributor, and running it would hand
somebody who may only *read* your repository a repository token and one
of your machines. The run's reason says so:

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
not also start somebody's code on your machines. Two words, two
authorizations.

**Approval is per tip, not per change.** It starts the workflows for the
change's *current* patchset, and a new patchset from the fork is blocked
again — because the file the maintainer read is not the file the next
push contains. This is what GitHub's "Approve and run" does, for the same
reason.

The response is `202` with the runs that now exist at that tip, read back
from the database rather than reported optimistically: the trigger may
legitimately have settled a run instead of starting one — a file over
the timeout ceiling, labels no runner has — and a caller told "running"
about a run that failed would wait for a build that is not coming. `409`
if the change is not open, or if nothing is blocked at its current tip;
`404` if there is no such change, or it has no patchsets.

The refusals in the previous section are decided **before** the fork
gate, so a fork's file that names an image, or asks for labels no runner
has, fails at once rather than waiting for an approval that could not
help it.

**Branch on `blocked_reason`, never on the words.** Every run carries it:
`fork` when the run is `blocked`, and `null` otherwise. The sentence in
`error` is written for a person and will be rewritten; this will not,
and it is the only one of the two a client should read.

Behind the button, the blocked run and its mirrored check row are deleted
before the real runs are created, so one run per workflow file per commit
still holds and no permanently-queued check is left holding the land
gate.

## Composed runs for a changeset

A [changeset](changesets.md) is one review over changes in several
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
   "path": "/var/lib/weft-runner/work/01hx…/workspace/api"},
  {"repo": "web", "change": "Ibb000002", "commit": "3c1d90ab…",
   "path": "/var/lib/weft-runner/work/01hx…/workspace/web"}
]
```

`path` is absolute, so a script can `cd` to a sibling without knowing how
the workspace is laid out.

**Every other member is read with a token scoped to that member alone.**
The job's own repository is checked out with the job token as it always
was; each sibling is fetched with a fresh repository-read token minted
for *that one repository*, and all of them are revoked when the job
reports. There is deliberately no organization-wide read token here: a
member's CI script is code its author wrote, and one token that could
read the whole organization would let that script read repositories the
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
[landing it](changesets.md#composed-ci); it is not written to the
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
maintainer's own repository, but it materialises the contributor's tree
beside it under `$WEFT_WORKSPACE`, and the maintainer's own script is
free to build it, test it, or execute it. Blocking only the fork
member's run would leave the contributor's code being run by three
repositories that never asked.

Approving the fork change's workflows — the same button and the same
[route](#changes-pushed-from-a-fork) on that change — releases the whole
composition, and the held runs start. A tip already approved stays
approved: recomposing does not put it back behind the button.

## Mining software

A runner executes a `run:` line somebody wrote, on a machine you pay
for, and the thing people most often do with somebody else's CPU is mine
cryptocurrency. Three things stand in the way of that on a Spool server.
Each can be walked around on its own, which is why there are several.

**1 — the file is refused when you push it.** A workflow that names known
mining software as a command, or that carries a mining pool URL, does not
schedule anything at all. Both `run:` lines and `env:` values are read —
`run: ./m $POOL` says nothing on its own, so a check that read only
`run:` would be walked around by the first person who tried. The refusal
is the ordinary kind, with the file, the line and a hint. Its wording
still comes from the hosted service:

```
mining software is not permitted in a workflow
`xmrig` is mining software; workflows are for building and testing your code
```

The pool schemes are `stratum+tcp://`, `stratum+ssl://`, `stratum2+tcp://`
and `stratum+tls://`, matched anywhere on a `run:` line or in any `env:`
value — no build has a use for one, so the scheme alone is enough.

A miner name has to be **invoked as a command** for the `run:` check to
fire: `grep -rn xmrig .`, a step that writes `xmrig.log`, and a README
quoting this paragraph are all somebody working, and a refusal that
cannot tell those apart is one people learn to route around rather than
read. The line is not fully parsed as shell — leading `VAR=1`
assignments, and wrappers like `sudo`, `env`, `nice` and `timeout`, are
skipped to find the program, and that is as far as it goes on purpose. In
an `env:` **value** only the URL half applies: a value containing the
word `xmrig` is somebody naming a file.

This layer is a refusal, not a detector. `curl -o m https://…/x && ./m`
walks straight past it.

**2 — a running step is killed.** While a step runs, the runner samples
the processes in its group every two seconds and reads them from
`/proc`. A process is a miner if its **program** is one — its `comm`, or
the basename of `argv[0]`, never an argument — or if a pool URL appears
anywhere in its `argv`, since a renamed binary still has to be told
where to send its shares. When one is found the whole process group is
killed and the job ends `failed` with

```
✗ Build stopped: mining software detected: xmrig (3s)
```

in the log and `mining software detected: xmrig` as the job's error.

Nothing here is measured: there is no CPU heuristic on purpose, because a
release build with `-j8` looks exactly like a miner to one, and a compile
flagged as abuse is a person locked out of their own forge.

**3 — it is recorded.** The server writes a `workflow.abuse` entry to
the organization's audit log, naming the runner, the job and the run, so
an admin can see what happened and whose change it was. Nothing is
switched off: the machine is yours, and what to do next is your
decision.

What Spool does not give you is network control. Where a runner's
traffic may go is decided by the network you put it on — see
[Isolating it](self-hosted-runners.md#isolating-it).

### The audit trail

| Action | Recorded when | Details |
|---|---|---|
| `workflow.abuse` | a runner stops a job for abuse | `abuse`, `reason`, `pool` (always `self_hosted`), `runner`, `job`, `run` |
| `workflow.approved` | somebody approves a fork's workflows | `change_key`, `commit` (the tip approved), `files` (the workflow files that were being held) |

`workflow.approved` carries the approving principal, like every audit
row, and it matters more than it looks: approving **deletes** the blocked
placeholder runs and their check rows, so nothing in `workflow_runs`
afterwards remembers that these files were ever held. This row is the
only surviving record that they were, and who let them go. It names the
files for the same reason — "approved the workflows" without saying which
is not a trail anybody can audit.

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
nothing and it is the difference between learning about `uses:` or
`runs-on: ubuntu-latest` now and learning about it from a red push.

This reads the file only. Whether a runner exists for its labels is
decided when a push arrives.

## The run page

Every check a workflow job writes carries a **Details** link to that
run's page in the dashboard, at `/<org>/<repo>/checks/runs/<run id>`. It
shows the run's state, the commit and branch it is about, a link to the
change if it came from one, the run's own error verbatim when there is
one — the refused line of YAML, the cycle in `needs:` — and one panel per
job, labelled with the runner that took it. Selecting a job shows its
log; while the job is live the page reads the same SSE stream described
below, so the output arrives as it is written rather than on a refresh.
A viewer with `repo:write` gets a **Cancel run** control there, which is
the `cancel` route below.

There is no page that lists a repository's runs. The Checks tab lists
the *checks*, workflow ones beside everybody else's, and a row is how you
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
`blocked`; `blocked_reason` is `fork` for a blocked run and `null` for
every other. A job carries `id`, `job_id`, `key`, `matrix` (an object),
`state`, `attempts`, `error`, `detail_url`, `log_chunks` and timestamps;
job `state` is one of `queued`, `running`, `passed`, `failed`,
`skipped`, `cancelled`.

A job also carries the `labels` its `runs-on` asked for, and `runner` —
`{"id", "name"}` once a runner has claimed it, `null` before that. Its
`pool` is always `self_hosted`; the field is kept so that clients written
for Weft's hosted service keep working.

**`commit_sha` and `change_key` narrow the query, not the page.** The
filtering happens in the database, inside `limit`, which is the whole
point of having them: `?commit_sha=<sha>&limit=1` gives you that commit's
run, where fetching the newest 20 and filtering in the client gives you
nothing at all on a repository busy enough to have pushed 20 times since.
A panel that filters a window is a panel that loses its own controls
exactly when the repository is busiest.

A run or job belonging to another repository answers `404`, not `403`.
An id that resolves differently for somebody who cannot read it is an
existence oracle.

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

Logs live in the object store under `ci/logs/`. How long they are kept
is up to whoever runs the server: the AWS reference deployment expires
them after **90 days** (`ci_log_retention_days`). They are not permanent
records.

## How verdicts reach the rest of Spool

Each job mirrors itself into one check row on its commit, named
`<workflow name> / <job key>` and linking back to the run it came from
at `/<org>/<repo>/checks/runs/<run id>`, and that is the whole
integration. A workflow file the server refused gets a row of its own,
named for the file, and a `.weft/` it could not read gets one named for
the directory; each links to the run that carries the reason — the row
is the only thing on the commit page that says why nothing ran. The
Checks tab, the change under review and the land queue were all built for
verdicts other people's build systems reached, and they need to know
nothing about this one.

A workflow row carries `provider: "weft"`, which is the server saying
*we wrote this*: the intake stamps `provider: "intake"` as a constant,
so nothing posted from outside can claim it. That is what lets the
**Details** link navigate inside the dashboard instead of opening a new
tab the way a link to somebody else's build system does.

So everything on [CI integration](ci-integration.md) applies
unchanged: a `failing` check blocks the land queue, a check you have
[made required](ci-integration.md#making-a-check-required) must go
green before a change may land, and `ci / test` from a workflow and
`ci/tests` from Buildkite sit in the same list under the same rules.

Required-check names are matched against the check's name, so the name
to require is the mirrored one — `ci / test`, including the spaces.
Note that a **matrix cell's name contains its values**, so requiring
`ci / test (linux, 1.84)` requires that cell and adding an axis value
does not silently add a requirement.

A **composed run goes somewhere else entirely**. Its jobs mirror into the
changeset's own list of checks, under the same `<workflow> / <job>` name
and linking to the same run page, and never onto the commit. Nothing on
the commit page changes when a composed job reports, and the changeset's
[land gate](changesets.md#composed-ci) is the only thing that reads
it.

## What is not here yet

Said plainly, with no dates attached:

- **No re-run button, and no re-run route.** Push again, or cancel and
  push again. (A job whose *runner* went quiet is claimed again by
  another runner once its lease runs out; that is a different thing, and
  it is not something you can ask for.)
- **No artifacts and no caches.** Nothing is kept from a job but its log
  and its verdict. A job that needs a dependency downloads it, or finds
  it already on the machine.
- **No containers.** A job runs on the runner's machine directly; to
  run jobs in a container, run the runner in one
  ([`Dockerfile.runner`](self-hosted-runners.md#getting-the-binary)).
- **No annotations.** A verdict and a log, not marks on the diff.
- **No list of runs in the dashboard.** A run has a page and its log
  tails live there, but the way to it is a check row's **Details** link
  or, for a composed run, the Changeset builds panel on the Checks tab;
  there is no screen that pages through a repository's push runs. `GET
  …/workflow-runs` is that list, and it is the API only.
- **No scheduled or manual triggers.** `push`, `change` and `changeset`
  are the three doors, and all three are something that happened to the
  repository.
- **No conditions, no job outputs, no service containers.**
