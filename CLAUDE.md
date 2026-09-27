# Working in stratum-core

This file is the operating discipline: what a change has to survive
before it lands, what the thresholds actually are, and what to do the
moment you find something wrong. [`CONTRIBUTING.md`](CONTRIBUTING.md) has
the mechanics — commands, test layout, conventions, the invariants. This
one is about judgement.

The short version: **run `scripts/ci-local.sh` before every push, fix
what you find rather than routing around it, and pin every fix with a
test that fails without it.**

---

## The gates

Fifteen things stand between a change and `main`. Six are CI jobs; nine
are a person, with real credentials, against something we do not control.
None of them is optional, and none of them is "usually fine".

| Gate | What it proves | Where |
|---|---|---|
| **correctness** | `cargo fmt --all --check`, `clippy -D warnings`, `cargo test --workspace --release` | CI job `correctness-gate` |
| **coverage** | 100% of coverable lines, ledger exact in both directions | CI job `coverage` |
| **chaos** | the server survives a SIGKILL at every compaction boundary, and a seeded fault storm loses no acknowledged write | CI job `chaos` |
| **web** | the site builds, the design-system contract holds over the *built* HTML, dashboard unit tests, Playwright e2e | CI job `web` |
| **deploy** | the production image and both runner images build, the prod-parity stack comes up, and the **real `git` CLI** clones, `fsck`s and pushes through it over HTTP *and* SSH — with no container runtime, on our own fleet (`deploy/proot`) | CI job `deploy-validation` |
| **terraform** | `fmt -check`, `validate` on the root, the bootstrap and the env-lock module, and the env-lock module's own test, all credential-free | CI job `terraform-validation` |
| **manual browser pass** | a person's-eye view of every screen the change touches, reporting **0 problems** | `web/dashboard/tools/walkthrough.mjs` |
| **manual S3 contract** | the conditional-PUT semantics I9 rests on hold on the backend we actually deploy on, under the role we actually deploy with | `scripts/manual-s3.sh check --both-addressing-styles` |
| **manual CI contract** | real GitHub answers the refusals our poller classifies, every `status`/`conclusion` pair it emits is one we map, and a real non-GitHub CI's verdict reaches the intake | `scripts/manual-ci.sh all`, plus `intake watch` |
| **manual ECS contract** | real ECS accepts the `RunTask` the dispatcher sends, every refusal it answers lands on the side of `Capacity`/`Refused` we meant, and a `StopTask` really stops a runner | `scripts/manual-ecs.sh all`, plus `stop <task-arn>` |
| **manual Stripe contract** | real Stripe accepts the bodies `billing/stripe.rs` sends under the restricted key we deploy with, and every webhook body it delivers is read by `parse_event` where we read it | `scripts/manual-stripe.sh all`, then `fixtures` |
| **manual GitHub-runner contract** | real GitHub mints a just-in-time runner for the body the dispatcher sends under the App we deploy, on a personal account *and* an organisation, every refusal it answers lands on the row we meant, and a real `actions/runner` takes the job and reports it | `scripts/manual-github-runners.sh all`, then `fixtures` |
| **manual registry contract** | real `npm`, `mvn`, `twine`/`pip`, `cargo` and `docker` publish to and install from the registry, and the licence gate refuses a real package fetched from real npmjs | `scripts/manual-registry.sh all`, then `fixtures`; against the live fleet, with a Weft runner publishing every ecosystem, `gh workflow run registry-e2e.yml` |
| **manual GitHub-sign-in contract** | real GitHub answers `GET /user/emails` in the shape the sign-in reads, and the `verified` flag on the `primary` entry — the single fact that lets a GitHub sign-up skip our confirmation mail — is really there | `scripts/manual-github-signin.sh all`, then `fixtures` |
| **manual mirror-push contract** | real GitHub takes the exact `git push` a forwarded mirror push sends under the installation we deploy with, and every refusal it prints — a protected branch, a stale lease, a missing `Contents: write` — lands on the answer `classify` gives it | `scripts/manual-mirror-push.sh all`, then `fixtures` |

The last eight are manual for the same reason the browser pass is: they
need credentials for a real bucket, a real App, a real AWS account and a
real Stripe account — or, for the registry, five package managers and a
docker daemon — and none of that belongs in CI. Do not
mistake the MinIO run inside `correctness-gate` for it. I9 — the manifest
is the only ref truth and changes only by CAS — is not a property of our
code, it is a property of the store, and it had only ever been checked
against MinIO. Real S3 answers **409 ConditionalRequestConflict** when two
conditional writes to one key overlap, where MinIO only answers 412; the
store client mapped 409 to a generic error, so on the multi-node fleet we
actually run, the loser of a manifest CAS fell out of the retry loop and
failed a user's push. No MinIO test could have caught it.

Run it under the **deployment's IAM policy**, not an admin key — the
404-not-403 case exists to catch a least-privilege policy turning absence
into `AccessDenied`, and an admin key can never fail it — and against
**both addressing styles**, because the store derives its signing path
from the URL's shape.

The CI contract is the same argument about a different provider, and it
has already cost us the same way. `get_page` treated a refusal as
rate-limited only when `Retry-After` was present; GitHub sends that on
**secondary** limits, while a **primary** budget exhaustion is a 403 with
`x-ratelimit-remaining: 0` and no `Retry-After`. We classified that as
"your App cannot read Actions", told a maintainer to re-approve a
permission they already held, and stopped the poller for good on a
condition that clears by itself inside an hour. The test named for
exactly that case could not fail, because `stratum-testkit`'s fake always
attached `Retry-After`. **A fake encodes what we believe the provider
does, and a suite built on a fake that is wrong is green precisely where
the product is broken.**

So: run it with the **App installation and Stratum token you deploy
with**. `scripts/manual-ci.sh denied` needs a second installation of the
same App that genuinely lacks `actions: read`, and it cannot fail under
an installation that holds every permission. Point
`STRATUM_GITHUB_ACTIONS_REPO` at a repository with a **varied** run
history — cancelled, timed out, skipped, awaiting approval — or the
mapping check confirms only that `completed/success` works.
`--exhaust-rate-limit` spends the installation's entire primary budget
and is the only way to observe the refusal we got wrong; without it the
script prints a NOTE and does not claim that case, the same way a
single-addressing-style S3 run does not claim the other.

The ECS contract is the third of these, and the argument is now familiar:
`FakeEcs` in `stratum-testkit` and `deploy/fake-ecs/fake-ecs.py` are
things we wrote, and what they cannot be wrong about is the only thing
`workflow/executor.rs` really needs to get right. A refusal classified
`Capacity` goes back on the queue without counting an attempt — right for
a full region, and a silent infinite retry for a policy edit that took
`ecs:RunTask` away. `scripts/manual-ecs.sh refusals` sends four real
malformed `RunTask`s and asserts each lands on `Refused`; with a second
credential that genuinely lacks `ecs:RunTask` in `STRATUM_ECS_DENIED_*`
it adds the AccessDenied case, which is the one an operator actually
meets and cannot fail under a key that holds the permission. Run all of
it with the **dispatch credential the app boots with**: half of what is
being checked is that the least-privilege policy in
`deploy/terraform/modules/runner` still admits the body the dispatcher
sends, and an admin key cannot fail that.

The capacity family — `RESOURCE:MEMORY`, `AGENT`, `ThrottlingException`
— is printed as NOTEs and never claimed: a busy region cannot be ordered
up, and spending the account's API budget to see a throttle is abuse of
it, the same reason `manual-ci.sh` declines to provoke a secondary rate
limit. So is the ending that matters most: `scripts/manual-ecs.sh stop`
with no argument checks only that `StopTask` is accepted and that the
255-character reason `Ecs::stop` truncates to is legal. To claim that a
stopped runner exits 0 without reporting a verdict — inside `stopTimeout`
rather than being SIGKILLed at the end of it — you need a task that is
running a real job, so trigger a hosted workflow with a slow step and
pass its ARN: `scripts/manual-ecs.sh stop <task-arn>`.

The Stripe contract is the fourth, and by now the argument writes itself:
`FakeStripe` says what we believe a webhook body looks like, and three of
those beliefs were wrong at once — the billing period moved onto the
subscription *item*, an invoice's subscription moved under `parent`, and
a setup-mode Checkout never had `line_items` on the wire — so a test
named for each case passed against a parser that read none of them.
`scripts/manual-stripe.sh` sends the bodies `stripe.rs` sends, observes
what a real test-mode account delivers through `stripe listen`, and
`fixtures` makes those bodies the suite's evidence: the ordinary test
`stripe_fixtures_parse_like_the_fake` fails the moment the fake and the
recorded wire disagree. Run it under the **restricted key you deploy
with** — half of what is checked is that its permissions admit every
call — never a live key, which the script refuses. `setup` needs a
person at a browser to finish a Checkout; `--no-browser` prints a NOTE
and does not claim that body. Making it truer has already paid once: a
subscription event that names an org this fleet does not have — a
staging and a production fleet share one Stripe account and both hear
everything — used to 500, and Stripe retries a 5xx until it disables the
endpoint.

The GitHub-runner contract is the fifth, and it is the CI contract's
argument pointed at a different half of the same App. The fake in
`stratum-testkit` answers `generate-jitconfig`, the `workflow_job`
deliveries and the cancel with what we *believe* GitHub does, and the
first real run of `scripts/manual-github-runners.sh` (2026-09-07) found
two of those beliefs false. The fake gave every just-in-time runner the
default labels `self-hosted`, `linux` and `x64` beside the one we asked
for; real GitHub attaches **only what you ask for**, lowercased. The
dispatcher registered runners with the size label alone, so a workflow
saying `runs-on: [self-hosted, weft]` — the form GitHub's own docs
recommend — would never have matched, and the job would have sat queued
for a day. The dispatcher now registers with the job's own label list,
which GitHub accepts `self-hosted` and all. The fake's 422 bodies also
carried an `errors` list GitHub does not send, and refused a label with
a space that GitHub accepts. What did hold: cancelling a run that has
already finished answers **409**, which is the difference between a
refused job's cancel being "done" and being retried every hour forever.
`fixtures` makes the recorded bodies the suite's evidence, and the
fixture test pins the fake to them. Run it under the **App and installations you deploy with** — it
uses the App's JWT and the tokens minted from it and nothing else,
because half of what is checked is that the App's permissions admit
every call the server makes. `denied` needs a second installation of
the same App that genuinely lacks `Administration: write`, the refusal
every existing installation meets until its owner re-approves the App;
the script refuses to claim it under one that holds the permission.
Run it against a **personal-account installation and an organisation
one** (`STRATUM_GITHUB_ORG_INSTALLATION_ID`, `…_RUNNER_ORG_REPO`):
the dispatcher registers on the repository with `runner_group_id: 1`
for both account types, which the docs state and the fake merely
repeats, and a personal-account run does not claim the organisation
half. `run` prints the workflow to commit and, with `--runner-dir`
pointing at an unpacked `actions/runner`, boots the agent on a minted
configuration and watches the job complete; without it the end-to-end
case is a NOTE, and so is a live run's 202 without `--live-run`.

The registry contract is the sixth, and it is the same argument aimed at
five providers at once. Every automated test of the registry sends
bodies **we** built, from what we believe npm, Maven, twine, cargo and
docker send, and reads them back with assertions written from the same
belief — which is the shape of all three failures above. So
`scripts/manual-registry.sh` drives the real clients: `npm publish` and
`npm install`, `mvn deploy` and a resolve from an *empty* local
repository, `twine upload` and `pip install` from the simple index,
`cargo publish` and a build that resolves against the sparse index,
`docker push` and `docker pull` of an image whose layer is larger than
the block size. The assertions are on what came back **through the
client**: `npm view`'s output, not our own HTTP — an assertion made with
`urllib` against our own server proves what we already believe.

`proxy` is the stage that matters most, and it is the only one that
leaves the machine: it installs a package from real npmjs through the
licence gate, and then sets a ten-year cooldown and checks the same
install is refused — which is the only way to know npmjs's own publish
date was read at all. `fixtures` records what npmjs served into
`stratum-testkit/fixtures/registry`, and
`upstream_fixtures_parse_like_the_fake` fails the moment the fake and
the recorded wire disagree. A missing client is a **NOTE** and claims
nothing: a machine with no `mvn` has not proved the Maven contract, and
the failure this gate exists to catch is a belief nobody tested.
The mirror-push contract is the seventh, and it is the runner contract's
argument pointed at the other half of the App. A push to a mirror is
forwarded to its origin by `mirror/forward.rs`, and what the origin
prints back — `--porcelain` lines and `remote:` lines — is read by
`classify` to say *which* command was refused and *why*, in a report
`git push` shows the person. Every hermetic test of it pushes to a bare
repository on disk whose refusals are a `pre-receive` hook we wrote to
sound like GitHub. So the reason phrase for a protected branch, the
`GH006` line, what the sibling of a refused command reports under
`--atomic`, and the transport's 403 for an installation that never
approved `Contents: write` are all beliefs, and
`crates/stratum-testkit/fixtures/mirror-push/provenance.json` says
`observed: false` until the script has run. Run it under the **App and
installation you deploy with**; `denied` needs a second installation
that genuinely lacks `Contents: write` — which is every installation
made before the permission was added — and refuses to claim the case
under one that holds it; `protected` needs
`STRATUM_GITHUB_PROTECTED_BRANCH` naming a branch GitHub really
protects, or the atomic sibling is a NOTE. `fixtures` writes the wire
into the suite, and `mirror_push_fixtures_classify_like_the_fake` holds
the classifier and the e2e hook to it.

One command reproduces those CI jobs, in CI's order, from CI's own
commands:

```sh
scripts/ci-local.sh            # everything this machine can run
scripts/ci-local.sh --fast     # skip coverage (~4 min) and deploy
scripts/ci-local.sh --only web # one job
```

It exports `CI=true` for Playwright so that step is a reproduction and
not an approximation. Anything it cannot run — `deploy-validation` needs
Linux for PRoot, or a docker daemon for the Fargate model box — is
printed as a **SKIP**, and the summary
refuses to say "good to push". A skip is not a pass.
`docs_e2e::the_local_ci_script_covers_every_job_the_workflow_declares`
fails if the workflow grows a job the script has not been taught, because
a local gate that has quietly fallen behind CI is worse than none: you
trust it, and it lies.

---

## The thresholds, as numbers

Not "high coverage". Not "good test hygiene". These are the lines.

**Coverage: 100% of coverable lines, and the ledger is exact.**
Every product line is either executed by the suite or listed in
`coverage-ledger.toml` with a reason. The gate is **two-way**: an
unledgered uncovered line fails, *and* a ledger entry whose line is now
covered fails as stale. Budget for ledger edits in the same commit as
the tests.

A ledger reason must say **why the line cannot run**, not that covering
it is inconvenient. These are reasons:

- an error-propagation region on a multi-line call whose Err branch needs
  a Postgres failure the surrounding transaction would hit first
- a 500 arm behind input that is shape-checked before it arrives
- a guard asserting an invariant its only caller already enforces, kept
  because it sits on a security boundary

These are not: "hard to test", "needs a network", "only in production".
If it needs a network, that is usually a sign the seam is in the wrong
place — see *Split at the seam the gate points at* below.

**A chaos test may never be the sole cover for a product line.**
Everything in `crates/stratum-server/tests/chaos_e2e.rs` is `#[ignore]`d,
so `cargo test --workspace --release` and `cargo llvm-cov --workspace`
both skip it: the chaos suite contributes **zero** lines to the lcov and
cannot move `coverage-ledger.toml` in either direction. That is the whole
enforcement mechanism, and it costs nothing — if a product line is
reachable only by killing the server mid-fold, the deterministic coverage
run reports it uncovered and the gate demands a deterministic sibling
test. (It would be unenforceable the other way round in any case: a
SIGKILLed child never writes its `LLVM_PROFILE_FILE` profraw, so a killed
process is invisible to coverage even under instrumentation.) The `chaos`
CI job runs them with `-- --ignored` and no llvm-cov. A red run prints
`STRATUM_CHAOS_SEED=<n>` and the tail of the fault trace; re-running with
that seed replays the same verdicts, request for request.

**Manual browser pass: 0 problems.** The walkthrough records console
errors, page errors, failed requests and layout defects. Expected
refusals are classified as benign explicitly, by stage, so the count
means something.

**Every clone passes `git fsck --full --strict`.** Invariant I11, and
the e2e suites enforce it against packs produced by the real server.

**Every new attack surface gets a negative suite**, and every attack case
ends by proving the server is still healthy and serving. A server that
wedges and refuses everything is a failure, not a pass.

**Tests are hermetic.** MinIO, PostgreSQL and the fake GitHub come from
`stratum-testkit`. Nothing reaches the real network. If a test needs the
internet, the design is wrong, not the test.

---

## When you find a bug or a gap

This is the rule the rest of the file exists to support.

1. **Fix it now, in this change.** Not in a follow-up, not in a TODO, not
   in a comment describing what someone should do later.
2. **Pin it with a test that fails without the fix.** Write the test
   first if you can; if you cannot, revert the fix and watch the test go
   red before you keep it. A test that would have passed against the bug
   is decoration.
3. **Test the behaviour, not the symptom.** The bug you found is one
   instance of a class. Cover the class.
4. **Say so in the commit message**, in the plain words of what went
   wrong. Future-you is reading it during an incident.

None of these are reasons to skip a fix:

> "unrelated to this change" · "pre-existing" · "probably a flake" ·
> "the test is wrong" · "it passes locally" · "not in scope"

If a fix genuinely does not belong in this change — it is large, or it
changes a contract — then it gets **written down as a finding with a
proposed patch**, in the commit and to whoever owns the decision. It does
not get discovered twice.

### "Flaky" is a claim, and it needs evidence

A test that fails once and passes on retry has told you something. Find
out what before you dismiss it.

The rule: **a failure is real until you can name the mechanism that made
it spurious.** Re-running is how you confirm a mechanism you already
suspect, never how you decide there wasn't one.

A worked example from this repo. A probe test failed in a full run and
passed alone, three times in a row. It would have been easy to call it
flaky. The failing process was pid **4018**; the test harness puts the
pid in its scratch path; `git` quotes the path back in its error message;
and the classifier was matching the bare substring `401`. So a real
repository named `rfc-403` would have been reported to users as "this
looks private — connect GitHub", sending them to a flow that could not
help them. The fix matches git's actual phrasing; the regression test
uses those exact URLs.

That bug was in the product, not the test, and only the *inconsistent*
failure pointed at it.

A second worked example, this time in the harness. A coverage run failed
with `postgres never became ready` in one e2e test; the same suite passed
alone, and passed on a re-run. Two hypotheses were named and one was
eliminated by experiment: the disk was 92% full, so a release re-link
running out of space was plausible — and freeing 10 GB reproduced the
failure anyway. The real mechanism was in `stratum-testkit`'s
`Pg::start`, which picked a port by binding it and letting it go, with no
retry. Every test binary in the workspace does that at once, for its own
cluster and again for every server it spawns, and llvm-cov widens the
window by slowing everything down. Losing that race is worse than a
clash: `connect` **succeeds** against whoever won, so the harness would
create its databases inside a stranger's cluster and watch them vanish
when that cluster died. `spawn_on_free_port` had documented the identical
hazard for the server and defended against it; the postgres harness had
not. It now retries on a fresh port, notices a child that exits early
instead of waiting out the deadline, and proves the cluster is its own
with `SHOW data_directory` before trusting it.

The lesson worth keeping: **re-running told me nothing, and the first
plausible mechanism was wrong.** Eliminating it by experiment — rather
than accepting it because it sounded right — is what left the real one
standing.

### A failure on one machine and not another is a harness bug

If CI is red and your machine is green, the difference is the finding.
Chase it before you chase the test.

This has bitten here twice, and both fixes were to make local runs
*reproduce* CI rather than approximate it:

- Playwright's default worker count is half the machine's cores, so a
  two-core runner serialised the suite while a development machine did
  not. `workers: 1` is pinned in `playwright.config.ts`. "Passes here,
  fails there" should be a property of the code, not the hardware.
- `scripts/ci-local.sh` sets `CI=true` for the Playwright step so it
  refuses to reuse a running preview server and fails on a stray
  `test.only`, exactly as CI does.

When you fix a real race, prefer waiting on **the thing the code actually
depends on** over waiting on a proxy for it. A test that polled a mock's
call count passed before the page had finished with the response; waiting
for the form to clear — the observable the next interaction needs — is
what made it deterministic.

---

## Habits that keep the gates green

**Don't push half an increment.** A module nobody calls is not
shippable, and both clippy (`dead_code`) and the coverage gate will say
so. They are right. Either finish the slice — wire it up, test it
through its route — or keep it local until you can. A red commit on the
branch costs a CI cycle and the next person's trust in the signal.

**Split at the seam the gate points at.** When coverage says a function
cannot be reached hermetically, that is usually a design note, not an
obstacle. The origin probe was one function doing URL parsing, a security
guard, a subprocess call and output classification; coverage could reach
none of it without the internet. Split four ways, the guard is tested
exhaustively with no network at all, the subprocess path is tested
against a real local repository the guard would correctly refuse, and only
the three-line composition is exempt. That is a better design *and* an
honest 100%.

A guard whose tests need the internet is a guard that gets tested once.

**Remap the ledger, do not re-derive it.** Editing a file shifts every
entry below the edit, which the gate reports as a stale entry *plus* an
unledgered line — the same exemption, twice, in two places. A large
increment produces dozens.

```sh
python3 scripts/remap_ledger.py <last-green-commit> --dry-run
python3 scripts/remap_ledger.py <last-green-commit>
```

It walks the diff and moves each entry to where its line went. Entries
whose source line was deleted or rewritten are **dropped and named** —
those need a decision. Pasting a plausible reason next to a line nobody
looked at is precisely the failure the ledger exists to prevent.

**Run the manual pass against a fully configured stack**, and build it
with the script rather than by hand:

```sh
scripts/manual-stack.sh up      # postgres, minio, the fakes, seeded
eval "$(scripts/manual-stack.sh env)"   # the stack's env, in this shell
cd web/dashboard && BASE=http://127.0.0.1:8080 \
  STRATUM_MAIL_DIR=$PWD/../../.stack/mail node tools/walkthrough.mjs
scripts/manual-stack.sh down
```

The `eval` is not optional. The walkthrough reads `RUNNER_BIN`,
`RUNNER_ECS_URL` and `CI_RUNNER_URL` from the environment to decide
whether the self-hosted, hosted-runner and CI stages can prove anything;
without them each reports a **harness** problem rather than skipping,
which is right — but the instructions the stack printed used to leave
the `eval` out, so following them to the letter produced a red pass.

The stack also brings up a **real CI provider** —
`scripts/manual-stack/ci-runner.py`, watching the private `acme/pipeline`
— and the walkthrough's `ci /` stages drive the whole loop through it:
push with the real `git` CLI, the outbound webhook fires, the provider
verifies our signature, clones with a `repo:read` token, runs the
repository's own `ci.sh`, and signs a verdict back into
`…/ci/checks`. Four seams only exist when somebody else is on the other
end of the wire, and none of them had a test: that the webhook fires at
all, that the signature we send verifies under a check we did not write,
that the clone credential works from outside, and that the intake accepts
what a real client sends rather than what our own helper sends. If the
provider is not up, those stages report it as a problem — the same rule
as a missing SSH URL: a walkthrough against a half-configured stack is a
walkthrough of a different product.

**It runs Postgres and MinIO in Docker, and that is the supported way.**
Start Docker (Docker Desktop, OrbStack, colima — anything that answers
`docker info`) and the script pulls `postgres:16` and `minio/minio` and
wires them up; nothing has to be installed on the host. That is not a
convenience, it is the only path that works off the CI image: the host
version needed Debian's `/usr/lib/postgresql/*/bin` layout, a `postgres`
system user, and root to `su` to it. On a development machine the stack
simply refused to start, which is a large part of why the prerequisite
kept getting reconstructed by hand and reconstructed wrongly.

The host path is still there for the CI image and is selected
automatically when there is no docker daemon; `STRATUM_STACK_NO_DOCKER=1`
forces it. `.nvmrc` pins the Node the site and dashboard build with —
`nvm use` before `npm run build`, or Astro refuses and it reads as the
site being broken.

You also need the browser the walkthrough drives, once per machine:

```sh
nvm use                                        # Node 22, per .nvmrc
cd web/dashboard && npx playwright install chromium
```

**Prefer running it against real Chrome.** Left alone the walkthrough
uses Playwright's bundled `chromium-headless-shell` — a stripped,
headless-only binary that nobody browses with. This pass exists to be a
person's-eye view and it reports *layout* defects, so the browser it runs
in is part of what is being tested:

```sh
CHROMIUM_PATH="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  BASE=http://127.0.0.1:8080 STRATUM_MAIL_DIR=… node tools/walkthrough.mjs
```

The opposite is true of the Playwright suite in the `web` job: leave that
on the pinned Chromium. Pointing it at whichever Chrome a machine happens
to have reintroduces exactly the "passes here, fails there" class that
`workers: 1` is pinned to prevent.

**Take the stack down before running `deploy-validation`.** Both bind
`:8080` and `:2222`, so a running manual stack makes the deployment smoke
test talk to the wrong server and fail at the first REST call with a bare
404 — a failure that says nothing about what is actually wrong.

It must have Postgres, MinIO, the built site and dashboard, *and* the SSH
front door (`STRATUM_SSH_BIND`, `STRATUM_SSH_HOST_KEY` — the PEM itself,
not a path — and `STRATUM_SSH_PUBLIC_URL`). Without SSH the dashboard
correctly hides the SSH clone row, and the pass silently becomes a
walkthrough of a different product: per-user SSH keys were registered,
listed and revoked for weeks without anything ever *cloning* with one.
The walkthrough now reports a missing SSH URL as a problem rather than
passing quietly.

This requirement stood for a long time with nothing in the repository
that could satisfy it, so the stack got rebuilt by hand each time — and
each rebuild rediscovered the same defects: readiness loops that retried
without sleeping, a liveness probe standing in for a readiness one, a
missing bucket that made every repo creation look like a broken product,
and a data directory the `postgres` uid could not traverse. A prerequisite
you have to reconstruct from memory is one that will be reconstructed
wrongly.

**Both browser gates read `dist`, not `src`.** Playwright previews the
production build and the walkthrough drives the server's
`STRATUM_DASHBOARD_DIR`, so a source edit with no `npm run build` behind
it is tested against the previous bundle. This is quiet in both
directions: a fix looks like it did not work, and — worse — reverting a
fix to check that its test really goes red shows the test passing, which
reads as "the test is decoration" when in fact nothing was rebuilt.
`scripts/ci-local.sh` builds first; a hand-run `npx playwright test` does
not.

**Exercise the thing, not the form.** Registering a key and seeing it in
a table proves the form works, not the key. Copy the URL the UI shows,
clone with it, `fsck` the result, revoke, and prove the next clone is
refused.

---

## What each gate has actually caught

A rules document with no evidence gets skimmed. These are real, from this
repo:

- **Playwright, against a mocked API** — client methods passing
  `JSON.stringify(...)` as a request body that the transport then
  stringified again. Every team call would have arrived double-encoded.
- **The coverage gate** — a module written and wired to nothing; a
  duplicated membership guard left behind by an edit.
- **The manual browser pass** — a per-repo grant that could not raise a
  personal token, only lower it; an authorization seam that ignored
  grants entirely, letting an admin held down to viewer still delete the
  repo; a date filter reading a picked day as UTC midnight rather than
  the user's; table cells that widened the layout instead of ellipsising;
  a name run into an inline tag, which reads as one word to anything that
  strips markup.
- **An inconsistent test failure** — the `401` substring bug above, and a
  test-harness port race that could have silently pointed one suite's
  databases at another suite's PostgreSQL.
- **CI disagreeing with a local run** — the Playwright worker-count
  difference, and a form-clear racing the next interaction.
- **A fake being wrong about the provider** — the `Retry-After`
  classification above. The fake could not produce the failing input, so
  the test named for the case passed against the bug.
- **The first real customer, not any gate** — the production image was
  built with `--no-install-recommends` and shipped without
  `ca-certificates`, so every `git` over HTTPS from the server — the
  origin probe, mirror syncs, imports — failed certificate verification
  and the probe answered "not reachable" for a private repository that
  should have read "looks private". Stripe kept working because the Rust
  client bundles its own roots, so nothing looked wrong until a person
  pasted a GitHub URL. `deploy/smoke.sh` now probes a public HTTPS origin
  from inside the deployed container, in `deploy-validation` and after
  every deploy; run against an image without the package it goes red
  with production's exact sentence.
- **Reading the manual pass's tail rather than its head** — the change
  view stopped rendering "landing is blocked while a check is failing" in
  `0ceb3b4`; the walkthrough still waited for it, `step()` did not catch,
  and **every stage after the tenth stopped running** while the pass
  still looked like it was being run. `step()` now records a throwing
  stage as a problem and carries on, and the summary says how many threw.
- **The manual GitHub-runner contract, on its first run** — the fake
  gave every just-in-time runner the default labels `self-hosted`,
  `linux` and `x64`; real GitHub attaches only the labels you register,
  lowercased. The dispatcher registered with the size label alone, so
  `runs-on: [self-hosted, weft]` — the form GitHub's own docs recommend
  — would never have matched, and every such job would have sat queued
  on GitHub for a day. Ten green e2e tests and a 0-problem intake could
  not have found it: the fake was wrong in exactly the place the product
  depended on it. The dispatcher now registers with the job's own label
  list, and the fixture test holds the fake to the recorded answer.
- **The manual mirror-push contract, on its first run** — under
  `--atomic`, the refs GitHub did *not* object to report
  `atomic transaction failed`. `classify` knew only `atomic push failed`
  and `atomic push failure`, and the receiving end sends neither. So in
  a push where one ref hit a protected branch, every innocent sibling
  was reported as refused **for its own reason**, carrying the blocked
  branch's `GH006` text — sending the pusher to look for a fault in a
  branch that was fine. The hermetic test could not see it twice over:
  its origin used a `pre-receive` hook, which declines the whole push
  with one message and so cannot produce a per-ref sibling reason at
  all, and its assertion was `err.contains("atomic")`, loose enough to
  pass against a phrase nothing emits. The hook is an `update` hook now,
  which is per ref and reproduces GitHub's shape, and the assertion
  names both refs and which was which.

Every one of those is now held by a test.

---

## Before you say it is done

- [ ] `scripts/ci-local.sh` passes, and you have read what it **skipped**
- [ ] every bug found along the way is fixed *and* pinned by a test
- [ ] the coverage ledger has no entry you could not defend out loud
- [ ] the manual browser pass reports 0 problems against a fully
      configured stack — **and no stage threw**. A stage that throws is
      one problem in the report and the run continues, but every stage
      after it ran against whatever state the failure left; the summary
      prints `!!! N stage(s) threw` and that is not a pass
- [ ] if the change touches CI verdicts — the poller, the intake, the
      Checks tab, the land gate — `scripts/manual-ci.sh all` passes and
      you have read its NOTEs, and the walkthrough's `ci /` stages ran
      against a real local provider rather than skipping
- [ ] if the change touches how a job is launched or stopped — the
      dispatcher, `workflow/executor.rs`, the runner task definition, the
      signal path — `scripts/manual-ecs.sh all` passes against a real
      cluster under the dispatch credential, and `stop <task-arn>` has
      been run once against a task running a real job. A run with no ARN
      has not checked that `StopTask` stops anything
- [ ] if the change touches billing — `stripe.rs`, the webhook, the plan
      gates, org creation — `scripts/manual-stripe.sh all` passes under the
      restricted test-mode key, `fixtures` has been run and the fixture
      test is green on the recorded bodies
- [ ] if the change touches forwarded pushes — `mirror/forward.rs`,
      `push.rs`, the REST doors' mirror branch, the `contents: write`
      permission, or the e2e hook's wording — `scripts/manual-mirror-push.sh
      all` passes under the deployed App with a protected branch and a
      denied installation, `fixtures` has been run, and the fixture test
      is green on the recorded bodies. A run without
      `STRATUM_GITHUB_PROTECTED_BRANCH` has not watched the atomic sibling
- [ ] if the change touches GitHub runners — the `workflow_job` intake,
      the dispatcher in `workers/github_runner.rs`, the `jitconfig`
      route, `Dockerfile.github-runner`, or the App's permissions —
      `scripts/manual-github-runners.sh all` passes under the deployed
      App against a personal repository *and* an organisation
      repository, `fixtures` has been run, and the fixture test is green
      on the recorded bodies. A run without a real `runs-on: weft` job
      has not checked that a runner ever took one
- [ ] if the change touches the package registry — an adapter, the
      admission policy, `registry_door.rs`, the blob or block path, the
      runner's config files or its proxy — `scripts/manual-registry.sh all`
      passes against a live stack and you have read its NOTEs; a stage
      that NOTEd is a client this run did not check. If an upstream
      document's shape may have moved, `fixtures` has been re-recorded
      and committed. After the deploy, `registry-e2e.yml` is green on
      the live fleet — the contract, the plan gate, and a Weft runner
      job publishing all five, `docker push` included
- [ ] if the change touches signing in with GitHub — the OAuth routes in
      `api/github_auth.rs`, `identities`, `verified_primary_email`, or
      the App's `Email addresses` permission —
      `scripts/manual-github-signin.sh all` passes under the OAuth
      client you deploy with, and you have read its NOTEs. A run whose
      GitHub account has an *unverified* primary address has not watched
      a proved address arrive, and `noperm` cannot be claimed without a
      second App that genuinely lacks the permission
- [ ] OpenAPI and the docs are updated in the same commit — `docs_e2e`
      fails on route drift, and the Repos quickstart is executed verbatim
      against a live server
- [ ] CI is green on the pushed commit before `main` moves
- [ ] build artifacts cleaned up — `scripts/clean-build-artifacts.sh`
      (`--deep` if you ran coverage or built the image). A full cycle
      writes well over 30 GB and `cargo llvm-cov` keeps a second build
      tree; leaving it there is how the next run hits "no space left on
      device" and gets misdiagnosed as a flaky test. See *Disk hygiene* in
      [`CONTRIBUTING.md`](CONTRIBUTING.md). Never clean while another
      build or agent is compiling against the same `target/`.

Report what happened faithfully. If something is still failing, say so
with the output. If a step was skipped, say which and why. "Done" means
verified, not "should be fine".
