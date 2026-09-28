# Working in spool

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

Eleven things stand between a change and `main`. Five are CI jobs; six
are a person, with real credentials, against something we do not control.
None of them is optional, and none of them is "usually fine".

| Gate | What it proves | Where |
|---|---|---|
| **correctness** | `cargo fmt --all --check`, `clippy -D warnings`, `cargo test --workspace --release` | CI job `correctness-gate` |
| **chaos** | the server survives a SIGKILL at every compaction boundary, and a seeded fault storm loses no acknowledged write | CI job `chaos` |
| **web** | dashboard unit tests, the production build, Playwright e2e | CI job `web` |
| **deploy** | the server and runner images build, the one-box compose stack comes up, and `deploy/smoke.sh` drives it with the **real `git` CLI** — clone, `fsck`, push over HTTP *and* SSH — and a self-hosted runner taking a job | CI job `deploy-validation` |
| **terraform** | `fmt -check` and `validate` on the AWS reference deployment, credential-free | CI job `terraform-validation` |
| **manual browser pass** | a person's-eye view of every screen the change touches, reporting **0 problems** | `web/dashboard/tools/walkthrough.mjs` |
| **manual S3 contract** | the conditional-PUT semantics I9 rests on hold on the backend a deployment actually uses, under the role it actually uses | `scripts/manual-s3.sh check --both-addressing-styles` |
| **manual CI contract** | real GitHub answers the refusals our poller classifies, every `status`/`conclusion` pair it emits is one we map, and a real non-GitHub CI's verdict reaches the intake | `scripts/manual-ci.sh all`, plus `intake watch` |
| **manual GitHub-sign-in contract** | real GitHub answers `GET /user/emails` in the shape the sign-in reads, and the `verified` flag on the `primary` entry — the single fact that lets a first GitHub sign-in be linked to the account with that address — is really there | `scripts/manual-github-signin.sh all`, then `fixtures` |
| **manual mirror-push contract** | real GitHub takes the exact `git push` a forwarded mirror push sends under the App installation, and every refusal it prints — a protected branch, a stale lease, a missing `Contents: write` — lands on the answer `classify` gives it | `scripts/manual-mirror-push.sh all`, then `fixtures` |
| **manual SSO contract** | a real identity provider — the one a deployment signs in with — answers discovery, the key set, the token endpoint and an ID token in the shapes `oidc.rs` reads, takes the client credentials as the server encodes them, and vouches for a person's address the way the trust rule needs | `scripts/manual-oidc.sh all`, then `fixtures`, once per provider |

The manual ones are manual for the same reason the browser pass is: they
need credentials for a real bucket, a real GitHub App and a real
identity provider, and none of that belongs in CI. Do not mistake the MinIO run inside
`correctness-gate` for the S3 contract. I9 — the manifest is the only ref
truth and changes only by CAS — is not a property of our code, it is a
property of the store, and it had only ever been checked against MinIO.
Real S3 answers **409 ConditionalRequestConflict** when two conditional
writes to one key overlap, where MinIO only answers 412; the store
client mapped 409 to a generic error, so on a multi-node deployment the
loser of a manifest CAS fell out of the retry loop and failed a user's
push. No MinIO test could have caught it.

Run it under the **deployment's IAM policy**, not an admin key — the
404-not-403 case exists to catch a least-privilege policy turning absence
into `AccessDenied`, and an admin key can never fail it — and against
**both addressing styles**, because the store derives its signing path
from the URL's shape.

The CI contract is the same argument about a different provider, and it
has already cost the same way. `get_page` treated a refusal as
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

So: run it with a real **App installation and Spool token**.
`scripts/manual-ci.sh denied` needs a second installation of the same App
that genuinely lacks `actions: read`, and it cannot fail under an
installation that holds every permission. Point
`STRATUM_GITHUB_ACTIONS_REPO` at a repository with a **varied** run
history — cancelled, timed out, skipped, awaiting approval — or the
mapping check confirms only that `completed/success` works.
`--exhaust-rate-limit` spends the installation's entire primary budget
and is the only way to observe the refusal we got wrong; without it the
script prints a NOTE and does not claim that case, the same way a
single-addressing-style S3 run does not claim the other.

The mirror-push contract is the same argument pointed at the other half
of the App. A push to a mirror is forwarded to its origin by
`mirror/forward.rs`, and what the origin prints back — `--porcelain`
lines and `remote:` lines — is read by `classify` to say *which* command
was refused and *why*, in a report `git push` shows the person. Every
hermetic test of it pushes to a bare repository on disk whose refusals
are a hook we wrote to sound like GitHub. So the reason phrase for a
protected branch, the `GH006` line, what the sibling of a refused command
reports under `--atomic`, and the transport's 403 for an installation
that never approved `Contents: write` are all beliefs, and
`crates/stratum-testkit/fixtures/mirror-push/provenance.json` says
`observed: false` until the script has run. `denied` needs a second
installation that genuinely lacks `Contents: write`, and refuses to claim
the case under one that holds it; `protected` needs
`STRATUM_GITHUB_PROTECTED_BRANCH` naming a branch GitHub really protects,
or the atomic sibling is a NOTE. `fixtures` writes the wire into the
suite, and `mirror_push_fixtures_classify_like_the_fake` holds the
classifier and the e2e hook to it.

The SSO contract is the same argument about the provider that decides
who gets an account. Single sign-on makes one for anybody the company's
provider vouches for, and every hermetic test of it signs in through
`stratum-testkit`'s fake provider, which we wrote from the OpenID
Connect specification and the providers' documentation. What Entra ID
leaves out of an ID token, whether Keycloak sends `email_verified` as a
boolean or a string, whether a provider percent-decodes the Basic
credentials RFC 6749 says to encode — each is a belief, and
`crates/stratum-testkit/fixtures/oidc/belief/provenance.json` says
`observed: false` until a real provider has been recorded beside it.
`scripts/manual-oidc.sh` checks what needs no browser (discovery, the key
set, a made-up code at the token endpoint), then has a person sign in
once and checks the token's signature, its claims, userinfo, and whether
the trust rule would admit them. `fixtures` writes a scrubbed recording
per provider, and `oidc_fixtures_parse_like_the_fake` holds the server's
own parsers to every one. Run it under the **issuer and client you
deploy with**, signed in as an ordinary person the application is
assigned to, and once **per provider** you support: a run against Okta
has not claimed Entra. The operator's version of the no-browser half is
`stratum-server admin sso-check`, which runs the product's own code.

One command reproduces those CI jobs, in CI's order, from CI's own
commands:

```sh
scripts/ci-local.sh            # everything this machine can run
scripts/ci-local.sh --fast     # skip chaos and deploy-validation
scripts/ci-local.sh --only web # one job
```

It exports `CI=true` for Playwright so that step is a reproduction and
not an approximation. Anything it cannot run — a missing toolchain, a
browser that is not installed, no docker daemon for `deploy-validation` —
is printed as a **SKIP**, and the summary
refuses to say "good to push". A skip is not a pass.
`docs_e2e::the_local_ci_script_covers_every_job_the_workflow_declares`
fails if the workflow grows a job the script has not been taught, because
a local gate that has quietly fallen behind CI is worse than none: you
trust it, and it lies.

---

## The thresholds, as numbers

Not "high coverage". Not "good test hygiene". These are the lines.

**A chaos test may never be the sole cover for a behaviour.**
Everything in `crates/stratum-server/tests/chaos_e2e.rs` is `#[ignore]`d,
so `cargo test --workspace --release` skips it, and the `chaos` CI job
runs it on its own with `-- --ignored`. A behaviour reachable only by
killing the server mid-fold is one the ordinary suite never checks: give
it a deterministic sibling test. A red chaos run prints
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

A second worked example, this time in the harness. A slow instrumented run failed
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
shippable, and clippy (`dead_code`) will say so. It is right. Either finish the slice — wire it up, test it
through its route — or keep it local until you can. A red commit on the
branch costs a CI cycle and the next person's trust in the signal.

**Split at the seam the test points at.** When a function cannot be
reached hermetically, that is usually a design note, not an obstacle.
The origin probe was one function doing URL parsing, a security guard, a
subprocess call and output classification; no test could reach any of
it without the internet. Split four ways, the guard is tested
exhaustively with no network at all, the subprocess path is tested
against a real local repository the guard would correctly refuse, and
only the three-line composition is left untested. That is a better
design *and* an honest suite.

A guard whose tests need the internet is a guard that gets tested once.

**Run the manual pass against a fully configured stack**, and build it
with the script rather than by hand:

```sh
scripts/manual-stack.sh up      # postgres, minio, the fakes, seeded
eval "$(scripts/manual-stack.sh env)"   # the stack's env, in this shell
cd web/dashboard && BASE=http://127.0.0.1:8080 \
  STRATUM_MAIL_DIR=$PWD/../../.stack/mail node tools/walkthrough.mjs
scripts/manual-stack.sh down
```

The `eval` is not optional. The walkthrough reads `RUNNER_BIN` and
`CI_RUNNER_URL` from the environment to decide whether the runner and CI
stages can prove anything;
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
`docker info`) and the script pulls `postgres:16` and the MinIO release
`.minio-version` pins, and wires them up; nothing has to be installed on
the host. That is not a
convenience, it is the only path that works off the CI image: the host
version needed Debian's `/usr/lib/postgresql/*/bin` layout, a `postgres`
system user, and root to `su` to it. On a development machine the stack
simply refused to start, which is a large part of why the prerequisite
kept getting reconstructed by hand and reconstructed wrongly.

The host path is still there for the CI image and is selected
automatically when there is no docker daemon; `STRATUM_STACK_NO_DOCKER=1`
forces it. `.nvmrc` pins the Node the dashboard builds with —
`nvm use` before `npm run build`, or the build refuses and it reads as
the dashboard being broken.

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
`:8080` and `:2222`, so a running manual stack makes the deployment
smoke test talk to the wrong server and fail at the first REST call with
a bare 404 — a failure that says nothing about what is actually wrong.

The stack must have Postgres, MinIO, the built dashboard, *and* the SSH
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
  should have read "looks private". Nothing looked wrong until a person
  pasted a GitHub URL. `deploy/smoke.sh` now probes a public HTTPS origin
  from inside the deployed container; run against an image without the
  package it goes red with production's exact sentence.
- **Reading the manual pass's tail rather than its head** — the change
  view stopped rendering "landing is blocked while a check is failing" in
  `0ceb3b4`; the walkthrough still waited for it, `step()` did not catch,
  and **every stage after the tenth stopped running** while the pass
  still looked like it was being run. `step()` now records a throwing
  stage as a problem and carries on, and the summary says how many threw.
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
- [ ] the manual browser pass reports 0 problems against a fully
      configured stack — **and no stage threw**. A stage that throws is
      one problem in the report and the run continues, but every stage
      after it ran against whatever state the failure left; the summary
      prints `!!! N stage(s) threw` and that is not a pass
- [ ] if the change touches CI verdicts — the poller, the intake, the
      Checks tab, the land gate — `scripts/manual-ci.sh all` passes and
      you have read its NOTEs, and the walkthrough's `ci /` stages ran
      against a real local provider rather than skipping
- [ ] if the change touches forwarded pushes — `mirror/forward.rs`,
      `push.rs`, the REST doors' mirror branch, the `contents: write`
      permission, or the e2e hook's wording — `scripts/manual-mirror-push.sh
      all` passes under a real App installation with a protected branch and a
      denied installation, `fixtures` has been run, and the fixture test
      is green on the recorded bodies. A run without
      `STRATUM_GITHUB_PROTECTED_BRANCH` has not watched the atomic sibling
- [ ] if the change touches signing in with GitHub — the OAuth routes in
      `api/github_auth.rs`, `identities`, `verified_primary_email`, or
      the App's `Email addresses` permission —
      `scripts/manual-github-signin.sh all` passes under the OAuth
      client you deploy with, and you have read its NOTEs. A run whose
      GitHub account has an *unverified* primary address has not watched
      a proved address arrive, and `noperm` cannot be claimed without a
      second App that genuinely lacks the permission
- [ ] if the change touches single sign-on — `oidc.rs`, `api/sso_api.rs`,
      `stratum-control`'s `sso.rs`, the SSO-only doors in `auth_api.rs`
      and `github_auth.rs`, or the fake provider — `scripts/manual-oidc.sh
      all` passes under a real provider's issuer and client, `fixtures`
      has been run, and the fixture test is green on the recording. A run
      under one provider has not claimed another
- [ ] OpenAPI and the docs are updated in the same commit — `docs_e2e`
      fails on route drift, and the Repos quickstart is executed verbatim
      against a live server
- [ ] CI is green on the pushed commit before `main` moves
- [ ] build artifacts cleaned up — `scripts/clean-build-artifacts.sh`
      (`--deep` if you built the image). A full cycle writes tens of GB;
      leaving it there is how the next run hits "no space left on
      device" and gets misdiagnosed as a flaky test. See *Disk hygiene* in
      [`CONTRIBUTING.md`](CONTRIBUTING.md). Never clean while another
      build or agent is compiling against the same `target/`.

Report what happened faithfully. If something is still failing, say so
with the output. If a step was skipped, say which and why. "Done" means
verified, not "should be fine".
