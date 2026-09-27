# Contributing to stratum-core

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

The research repo's dead-ends ledger also still applies: commit-order
segments, bitmap-assisted ingest walks, sparse walks, interleaved per-ref
emissions, and `index-pack --strict` on pushes were all tried and
rejected — don't reintroduce them.

## Vendored code

`crates/stratum-store` and `crates/stratum-proto` are vendored from the
research repo and stay byte-close to it. Every intentional change carries
a `// STRATUM-CORE DIVERGENCE:` comment explaining why. Don't refactor
these crates for style.

## Checks to run before pushing

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
pinned release and the GitHub release carries no assets — the same
withdrawal that took `minio/minio` off Docker Hub. The container image is
the only artifact left, `scripts/fetch-minio.sh` pulls the binary out of
it over the registry API (no daemon, because the `weft-2x` runners
deliberately have none), and that binary is Linux. So on a Mac there is
nothing to fetch and the answer is a container. `STRATUM_MINIO_URL` makes
the testkit *attach* to a MinIO it did not start — it never spawns or
kills that one. The suite is still hermetic: nothing reaches the real
network, the store is just on the other side of a socket.

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
GitHub runners ship them). The testkit spawns its own throwaway cluster
per test process — `initdb` + `postgres` on a random port, one database
per test — nothing touches a system or docker Postgres. Running as root
(dev containers), it drops to the `postgres` system user automatically.
`docker-compose.yml` at the repo root provides Postgres + MinIO for
running the *server* locally; the test suite never uses it.

Web surfaces:

```sh
cd web/site && npm ci && npm run build          # also regenerates llms.txt
cd web/dashboard && npm ci && npx vitest run && npm run build
CHROMIUM_PATH=$(which chromium) npx playwright test   # or let playwright install
```

Design: tokens live in `web/shared/tokens.css` (one `light-dark()` pair
per color), the Tailwind mapping in `web/shared/theme.css`, and the whole
system — palette, patterns, do/don'ts — in `web/DESIGN.md`. Never
hardcode a hex in a component. After a dashboard restyle, regenerate the
landing-page screenshots (command in DESIGN.md).

## Coverage

CI runs `cargo llvm-cov` across the workspace — unit, integration, and
e2e together — and enforces **100% of coverable lines** through
`scripts/coverage_gate.py`: every product line is either executed by the
test suite or listed in `coverage-ledger.toml` with the reason it is
exempt — unreachable by construction (fork/exec child code,
`unreachable!()` guards), not deterministically triggerable from outside
the process (TOCTOU windows, exact buffer boundaries), or an
equivalence-class member whose behavior is pinned by sibling tests. The gate fails in
both directions — an uncovered line missing from the ledger, and a
ledger entry whose lines have become covered (stale entries must be
pruned) — so the ledger can only shrink truthfully. `stratum-testkit`
(the test harness itself) is excluded from the metric. Adding a ledger
entry is a code-review event: prefer a test.

Two mechanics make e2e coverage real, keep
them intact when adding suites:

- Test harnesses that spawn the server binary re-export
  `LLVM_PROFILE_FILE` through their `env_clear()` so the instrumented
  child writes its profile.
- Harness `Drop` impls send SIGINT first (graceful shutdown flushes the
  profile); SIGKILL is only the bounded fallback.

Run it locally with:

```sh
rustup component add llvm-tools-preview && cargo install cargo-llvm-cov
cargo llvm-cov --workspace --lcov --output-path coverage.lcov
python3 scripts/coverage_gate.py coverage.lcov coverage-ledger.toml
```

New code arrives with the tests that cover it; a change that leaves an
unledgered uncovered line (or strands a stale ledger entry) won't merge.

The gate is also two-way, which is the part that surprises people: a new
test that happens to cover a currently-ledgered line fails CI as a *stale
entry* until the entry is pruned. Budget for ledger edits in the same
commit as the tests.

Editing a file also shifts every ledger entry below the edit, which
surfaces as a stale entry plus an unledgered line — the same exemption,
reported twice in two places. Remap rather than re-deriving:

```sh
python3 scripts/remap_ledger.py <last-green-commit> --dry-run
python3 scripts/remap_ledger.py <last-green-commit>
```

It walks the diff and moves each entry to where its line went. Entries
whose source line was deleted or rewritten are dropped and named — those
are the ones that need a decision, and re-adding a reason from memory
next to a line nobody looked at is the failure mode the ledger exists to
prevent. Re-run the gate afterwards; what it still reports is genuinely
new.

## Before you push

```sh
scripts/ci-local.sh
```

It runs all four CI jobs' checks in CI's own order. Two of them are easy
to run by habit; the other two — the coverage gate, and the
design-system contract over the *built* site — are the ones that fail on
work that felt finished, because nothing about editing a page suggests
they exist. The script names anything it had to skip instead of counting
a skip as a pass, and `docs_e2e::the_local_ci_script_covers_every_job_the_workflow_declares`
fails if the workflow grows a job the script has not been taught.

`ci-local.sh` covers the CI jobs and nothing else. Four gates are
**manual**, because each needs credentials for something we do not
control, and those do not belong in CI:

| | |
|---|---|
| the browser pass | `scripts/manual-stack.sh up`, then `node web/dashboard/tools/walkthrough.mjs` — 0 problems, **and no stage threw** |
| the store contract | `scripts/manual-s3.sh check --both-addressing-styles`, under the deploy role |
| the CI-provider contract | `scripts/manual-ci.sh all`, under the App installation and token you deploy with |
| the ECS contract | `scripts/manual-ecs.sh all`, under the dispatch credential the app boots with; `stop <task-arn>` on a task running a real job |

Run the CI-provider one whenever a change touches the Actions poller, the
signed check intake, the Checks tab or the land gate, and the ECS one
whenever a change touches how a job is launched or stopped — the
dispatcher, `workflow/executor.rs`, the runner task definition or the
signal path. The argument for both is the same one: every automated
test of the checks feature runs against `stratum-testkit`'s fake GitHub,
and every automated test of dispatch runs against its `FakeEcs`, and both
encode what we *believe* the provider does — and when that belief is
wrong the suite is green exactly where the product is broken.
`manual-ci.sh` and `manual-ecs.sh` each read their own header for what a
subcommand costs, what it proves, and what it deliberately does not:
`--exhaust-rate-limit` spends an installation's entire hourly budget, so
do not point it at one anything else depends on, and `manual-ecs.sh`
starts real Fargate tasks that exit in seconds.

The local half of the same loop runs with no credentials at all:
`scripts/manual-stack.sh up` starts a real CI provider
(`scripts/manual-stack/ci-runner.py`) against the private `acme/pipeline`
repository, and the walkthrough's `ci /` stages push to it with the stock
`git` CLI and wait for its verdict to reach the Checks tab and the land
gate. That fixture is also the worked example a maintainer copies for
their own receiver, so keep it small and keep the comments honest.

## Test layout

- Unit tests live beside the code.
- Integration tests (MinIO + fakes, in-process) live in each crate's
  `tests/`.
- End-to-end suites — `git_e2e`, `mirror_e2e`, `repos_e2e`, `workers_e2e`,
  `hardening_e2e` under `crates/stratum-server/tests/` — spawn the
  compiled binary against MinIO and drive it with the stock `git` CLI.
  Latency regressions are asserted as **store round-trip counts** through
  `stratum_testkit::CountingProxy`, not wall-clock.

Tests must be hermetic: MinIO, PostgreSQL, and the fake GitHub/origin
servers come from `stratum-testkit`; nothing may reach the real network.

## Conventions

- Errors: vendored code keeps `String`; new code uses typed errors at
  module boundaries where practical.
- Every mutating control-plane function takes an `AuditCtx` — that is what
  makes audit coverage structural rather than best-effort.
- Multi-tenant isolation: S3 prefixes come only from
  `stratum_control::registry::RepoPrefix`, constructed after the
  token→org→repo check. Never format a bucket path by hand.
- Existence masking: cross-tenant probes answer 404, missing credentials
  answer 401. `hardening_e2e::isolation_matrix_cross_org_and_cross_repo`
  is the gate.
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

## Disk hygiene

A full local cycle (build → test → `cargo llvm-cov` → a docker image
build) writes well over 30 GB, and `cargo llvm-cov` keeps a *second*
build tree (`target/llvm-cov-target`) rather than reusing `target/debug`.
Test scratch under `$TMPDIR` (on macOS a `/var/folders/…` path, not
`/tmp`) also accumulates across killed runs. Run:

```bash
scripts/clean-build-artifacts.sh          # scratch only; rebuilds stay fast
scripts/clean-build-artifacts.sh --deep   # also target/debug and docker images
```

`scripts/ci-local.sh` refuses to start when there is not room for the
run it was asked for — about 30 GiB for a full cycle, 12 for `--fast` or
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
