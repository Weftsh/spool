# Contributing to Spool

[`CLAUDE.md`](CLAUDE.md) is the companion to this file: the gates a
change has to survive, the thresholds as numbers, and what to do the
moment you find a bug or a gap. This file has the mechanics; that one has
the judgement.

## The contracts that bind every change

The engine's correctness model is written down and enforced, not folklore:

- [`reference/invariants.md`](reference/invariants.md) — the 15 invariants.
  **I11 is non-negotiable: every clone the server produces must pass
  `git fsck --full --strict`, and CI runs that check.** If your change can
  affect a produced pack, add or extend an e2e test that clones and fscks.
- [`reference/formats.md`](reference/formats.md) — normative byte formats:
  manifest schema, SLH4/SLH3 locator, segments, WAL entries, ref pages,
  epochs.
  Changing a format means versioning it there first.
- [`reference/storage-model.md`](reference/storage-model.md) — the object
  layout and the two-mutable-pointers rule (everything else in the bucket
  is immutable; the manifest moves only by compare-and-swap).

Some designs were tried in the research that produced the engine and
rejected: commit-order segments, bitmap-assisted ingest walks, sparse
walks, interleaved per-ref emissions, and `index-pack --strict` on
pushes. Don't reintroduce them.

## Vendored code

`crates/stratum-store` and `crates/stratum-proto` are vendored from the
research repo and stay byte-close to it. Every intentional change carries
a `// STRATUM-CORE DIVERGENCE:` comment explaining why. Don't refactor
these crates for style.

## Layout

| | |
|---|---|
| `crates/stratum-store` | the object-store client (SigV4, conditional PUT), manifest, locator plane |
| `crates/stratum-proto` | git protocol v2 serving and receive-pack |
| `crates/stratum-engine` | ingest, locator build, compaction, GC |
| `crates/stratum-control` | the control plane on PostgreSQL: orgs, repos, people, tokens, audit, review, workflows, runners |
| `crates/stratum-server` | the server binary: git over HTTP and SSH, the REST API, the dashboard, background workers |
| `crates/stratum-runner` | the runner agent, built as `weft-runner` |
| `crates/stratum-testkit` | hermetic test harnesses: MinIO, PostgreSQL, the fakes |
| `web/dashboard` | the web UI (React, Vite) |
| `web/shared` | design tokens and the Tailwind theme |
| `deploy/` | the container image's companions and the AWS reference deployment |
| `docs/` | operations, deployment, the user guide and `openapi.json` |
| `reference/` | the storage model, formats and invariants |

The crate names are the engine's original ones; the binaries are
`stratum-server` and `weft-runner`.

## Checks to run before pushing

```sh
scripts/ci-local.sh            # everything CI runs, in CI's order
scripts/ci-local.sh --fast     # without the chaos suite
scripts/ci-local.sh --only web # one job
```

CI has four jobs, and the script runs the same four from the same
commands:

| Job | What it runs |
|---|---|
| `correctness-gate` | `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --release` |
| `chaos` | `cargo test -p stratum-server --test chaos_e2e --release -- --ignored` |
| `web` | the dashboard's unit tests (`npx vitest run`), its production build, and Playwright e2e with `CI=true` |
| `terraform-validation` | `terraform fmt -check` and `validate` on `deploy/terraform`, its bootstrap and the env-lock module, and the env-lock module's own test, all credential-free |

The script also names two manual gates it cannot run for you —
`s3-contract` and `github-signin-contract` — as **SKIP**s. A skip is not
a pass; the summary refuses to say "good to push" while there is one.
`docs_e2e::the_local_ci_script_covers_every_job_the_workflow_declares`
fails if the workflow grows a job the script has not been taught.

For a quicker loop while you work, the same checks by hand:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # fetches a MinIO binary into .testkit/ on first run
```

**Set up this worktree's test dependencies first**, on a Mac always and
on any machine that holds more than one checkout:

```sh
eval "$(scripts/test-env.sh)"
```

It starts a MinIO container named for this worktree on a free port,
picks a preview port nobody else is using, and exports both. It is
idempotent — run it again and it reuses what is up — and everything it
sets has the old default when unset, so a single checkout and CI are
unaffected.

Two machine-wide singletons are why it exists.

*MinIO.* The tests need one, and MinIO no longer publishes prebuilt
binaries: `dl.min.io` answers `410 Gone` for every platform of the
pinned release and the GitHub release carries no assets. The container
image is the only artifact left. `scripts/fetch-minio.sh` pulls the
binary out of it over the registry API, with no container runtime — the
jobs that need MinIO must not depend on a docker daemon — and that binary
is Linux. So on a Mac there is nothing to fetch and the answer is a
container. `STRATUM_MINIO_URL` makes the testkit *attach* to a MinIO it
did not start — it never spawns or kills that one. The suite is still
hermetic: nothing reaches the real network, the store is just on the
other side of a socket. `.minio-version` pins the release, and both the
script and the testkit read it.

*The preview port.* Playwright previews on `--strictPort` and
`ci-local.sh` sets `CI=true` on purpose, which also turns off
`reuseExistingServer`. Two checkouts running the web gate at once
therefore collide — and the collision does not present as a port error.
It presents as a run where **every test passes and the process exits
non-zero**, which reads as a product failure. `STRATUM_PREVIEW_PORT`
gives each worktree its own; `web/dashboard/tests/preview.ts` is the one
place it is read, by the config and by the specs that need the origin as
a value.

Without the `eval`, the first test that needs a store fails with the
command that fixes it in the message rather than leaving you to find it.

Tests need PostgreSQL binaries on the machine (`apt install postgresql`;
GitHub's runners ship them). The testkit spawns its own throwaway cluster
per test process — `initdb` + `postgres` on a random port, one database
per test — nothing touches a system or docker Postgres. Running as root
(dev containers), it drops to the `postgres` system user automatically.
`docker-compose.yml` at the repo root runs Spool itself with Postgres and
MinIO, for trying the server on one machine; the test suite never uses
it.

The dashboard:

```sh
nvm use                                   # the Node .nvmrc pins
cd web/dashboard && npm ci && npx vitest run && npm run build
npx playwright install chromium           # once per machine
npx playwright test
```

Playwright previews the production build in `dist/`, so run `npm run
build` after a source edit or you are testing the previous bundle.

Design: tokens live in `web/shared/tokens.css` (one `light-dark()` pair
per color), the Tailwind mapping in `web/shared/theme.css`, and the whole
system — palette, patterns, do/don'ts — in `web/DESIGN.md`. Component
conventions are in `web/dashboard/COMPONENTS.md`. Never hardcode a hex in
a component.

## The manual gates

`ci-local.sh` covers the CI jobs and nothing else. Six gates are
**manual**, because each needs credentials for something the project
does not control, and those do not belong in CI:

| | |
|---|---|
| the browser pass | `scripts/manual-stack.sh up`, `eval "$(scripts/manual-stack.sh env)"`, then `node web/dashboard/tools/walkthrough.mjs` — 0 problems, **and no stage threw** |
| the store contract | `scripts/manual-s3.sh check --both-addressing-styles`, under the deployment's role |
| the CI-provider contract | `scripts/manual-ci.sh all`, under a real App installation and token |
| the GitHub sign-in contract | `scripts/manual-github-signin.sh all`, then `fixtures` |
| the mirror-push contract | `scripts/manual-mirror-push.sh all`, then `fixtures` |
| the SSO contract | `scripts/manual-oidc.sh all`, then `fixtures`, once per identity provider |

[`CLAUDE.md`](CLAUDE.md) says when each is required. The argument for
all of them is the same: every automated test of those features runs
against a fake in `stratum-testkit`, and a fake encodes what we
*believe* the provider does — when that belief is wrong the suite is
green exactly where the product is broken. Each script's header says
what a subcommand costs, what it proves, and what it deliberately does
not: `manual-ci.sh --exhaust-rate-limit` spends an installation's entire
hourly budget, so do not point it at one anything else depends on.

The local half of the CI loop runs with no credentials at all:
`scripts/manual-stack.sh up` starts a real CI provider
(`scripts/manual-stack/ci-runner.py`) against the private `acme/pipeline`
repository, and the walkthrough's `ci /` stages push to it with the stock
`git` CLI and wait for its verdict to reach the Checks tab and the land
gate. That fixture is also the worked example somebody copies for their
own receiver, so keep it small and keep the comments honest.

## Test layout

- Unit tests live beside the code.
- Integration tests live in each crate's `tests/`: the store's
  (`format_edges`, `store_faults`), the engine's (`ingest_roundtrip`,
  `forks`, `gc_forks`, …), the protocol's, and the testkit's own
  (`store_contract` holds the object-store semantics the engine depends
  on).
- End-to-end suites live in `crates/stratum-server/tests/`, one per
  surface — `git_e2e`, `repos_e2e`, `mirror_e2e`, `changes_e2e`,
  `changesets_e2e`, `workflow_e2e`, `self_hosted_e2e`, `runner_e2e`,
  `ssh_e2e`, `search_e2e` and the rest. Each spawns the compiled server
  against MinIO and a throwaway PostgreSQL and drives it over HTTP, SSH
  and the stock `git` CLI. Latency regressions are asserted as **store
  round-trip counts** through `stratum_testkit::CountingProxy`, not
  wall-clock.
- `crates/stratum-runner/tests/binary_e2e.rs` drives the built
  `weft-runner` through registration, claiming and running jobs.
- `docs_e2e` holds the docs to the code: the Repos quickstart
  (`docs/guide/quickstart-repos.md`) is executed verbatim against a live
  server, every route in the router must be in `docs/openapi.json` and
  every documented route must exist, and the CI and mirror guides must
  quote what the code actually accepts and sends.
- `chaos_e2e` is `#[ignore]`d and runs only in the `chaos` job; see
  `CLAUDE.md` for why it may never be the only test of a behaviour.
- `web/dashboard` has its unit tests beside the code (vitest) and its
  Playwright suite in `web/dashboard/tests/`.

Tests must be hermetic: MinIO, PostgreSQL, and the fake GitHub/origin
servers come from `stratum-testkit`; nothing may reach the real network.

Harnesses that spawn the server stop it with SIGINT first, which takes
its graceful-shutdown path, and SIGKILL only as a bounded fallback. Keep
that order when you add one.

## Conventions

- Errors: vendored code keeps `String`; new code uses typed errors at
  module boundaries where practical.
- Every mutating control-plane function takes an `AuditCtx` — that is what
  makes audit coverage structural rather than best-effort.
- Multi-tenant isolation: S3 prefixes come only from
  `stratum_control::registry::RepoPrefix`, constructed after the
  token→org→repo check. Never format a bucket path by hand.
- Every repository is private to its organization. There is no public
  read path and no anonymous one: a request with no credential answers
  401, and a credential with no access answers the same 404 an absent
  repository gets. `hardening_e2e::isolation_matrix_cross_org_and_cross_repo`
  is the gate. A token acts only in the organization it was minted in.
- Identifier lookups (`org_by_name`, `repo_by_name`) short-circuit to
  "not found" for any name that fails `valid_name` — an invalid-shaped
  name is definitionally absent, and hostile bytes (NUL, control chars,
  injection shapes) must never reach a query, where a NUL would surface
  as a 500 instead of the masking 404.
- Adversarial coverage is a standing gate, not a one-off. Three suites
  hold it: `hardening_e2e` (the HTTP isolation matrix), `http_pentest_e2e`
  (injection-shaped identifiers against real Postgres, malformed/hostile
  Authorization headers, raw malformed requests), and `ssh_pentest_e2e`
  (raw SSH exec injection + path traversal + shell-escape attempts, the
  username-is-not-a-trust-boundary property, read-only keys barred from
  receive-pack, and protocol fuzzing + auth floods that must not wedge the
  server), joined by `cdn_pack_e2e` for the CDN offload surface (forged,
  tampered, expired, replayed, and cross-tenant pack tokens; traversal
  and non-pack names; a corrupt pack that must fail loudly rather than
  yield a silently incomplete clone). Each attack case ends by proving
  the server is still healthy and serving — a wedged server that refuses
  everything is a failure, not a pass. New attack surfaces get a matching
  negative suite.
- A change to the API updates `docs/openapi.json` and the guide in the
  same commit; `docs_e2e` fails on route drift.

## Disk hygiene

A full local cycle (a release build of the workspace, its test binaries,
MinIO's store, and perhaps a docker image build) writes tens of GB. Test
scratch under `$TMPDIR` (on macOS a `/var/folders/…` path, not `/tmp`)
also accumulates across killed runs. Run:

```bash
scripts/clean-build-artifacts.sh          # scratch only; rebuilds stay fast
scripts/clean-build-artifacts.sh --deep   # also target/debug and docker images
```

Never clean while another build or agent is compiling against the same
`target/`.

`scripts/ci-local.sh` refuses to start when there is not room for the
run it was asked for — about 20 GiB for a full run, 12 for `--fast` or
`--only`. That is a precondition rather than a warning on purpose:
running out happens *mid-job*, and the shape it takes is a compiler or a
test process dying with an I/O error somewhere unrelated to whatever is
actually wrong. The run gets retried, passes once something else frees a
little, and goes down as flaky. Refusing to start costs seconds; a
misdiagnosed full disk costs an afternoon, and this repository has
already spent one. `STRATUM_MIN_FREE_GIB=<n>` overrides it deliberately.

**`--deep` reaches past this repository.** It runs `docker image prune -af`,
which removes every image no container references — including images
other projects on this machine pulled or built. They come back by
pulling or building them again, but they do go. `docker volume prune` is
*not* run at any level, because a volume can be somebody's data rather
than a cache; reclaim those by hand if you mean to.

Symptom to recognize: "no space left on device" while `df` still shows
plenty of capacity — on a fixed per-session allowance, `Avail` hits 0 with
low `Used`. Deletes still succeed when writes fail, and freed space is
immediately writable.

**A second thing wears the same message, and it is not disk at all.** On
macOS, `initdb` failing with

```text
FATAL: could not create shared memory segment: No space left on device
DETAIL: Failed system call was shmget(...)
```

means the machine is out of System V shared-memory *segments*, and the
error's own HINT says so. macOS caps them at 32 for the whole machine
(`sysctl kern.sysv.shmmni`) where Linux allows thousands, and a postmaster
killed with SIGKILL never releases the one it holds. The harness shuts
clusters down with SIGINT so they are returned, but a hard-killed run
still leaks one per cluster — and the failure lands on whichever test runs
next, not the run that caused it.

```sh
ipcs -m -o                        # NATTCH 0 means orphaned
scripts/clean-build-artifacts.sh  # reclaims them (leaves attached ones alone)
```

This is worth recognising on sight: it is a resource leak from an earlier
run, presenting as a disk error, in an unrelated test, on one platform.
