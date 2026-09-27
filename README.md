# Spool

**A self-hosted git forge, served from object storage, running in your own
infrastructure.**

Spool is the forge behind Weft, packaged to run on your own machines:
repositories live as immutable segments in S3-compatible object storage, any
stateless node serves any repository, and every repository is a real git
remote over HTTP and SSH. On top of that sit code review (changes and
changesets across repositories), issues, teams and per-repository access,
branch protection and required checks, forks, mirrors of GitHub/GitLab
repositories, imports, `.weft` workflows on your own self-hosted runners, and
a CI intake for whatever else you already run.

> **Pre-release.** Spool is being carved out of the hosted product right now.
> The tree builds and is being trimmed to what a self-hosted install needs;
> installation guides, a license file and release artifacts are on their way.

## Layout

```
crates/stratum-store    engine: manifest, S3 client (SigV4, conditional PUT),
                        locator plane, pack parsing
crates/stratum-proto    engine: git protocol v2 serving + receive-pack
crates/stratum-engine   ingest, locator build, compaction, GC
crates/stratum-control  control plane: orgs, repos, people, tokens, audit,
                        review, workflows, runners (PostgreSQL)
crates/stratum-server   the deployable binary: git smart HTTP + SSH + REST API
                        + background workers
crates/stratum-runner   the self-hosted runner agent (`weft-runner`)
crates/stratum-testkit  hermetic test harnesses: MinIO, PostgreSQL, fakes
web/dashboard           the web UI (React + Vite)
docs/                   operations, deployment and the user guide
docs/openapi.json       the REST API
```

## Running it locally

```sh
docker compose up -d    # PostgreSQL + MinIO
export STRATUM_STORE_URL=… AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… AWS_REGION=…
export STRATUM_DB_URL=postgres://stratum:stratum@127.0.0.1:5432/stratum
cargo run -p stratum-server -- admin bootstrap --org acme   # once
cargo run -p stratum-server                                  # serve on :8080
```

See [docs/operations.md](docs/operations.md) for the configuration reference.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace         # needs PostgreSQL binaries; MinIO is fetched
cd web/dashboard && npm ci && npx vitest run && npm run build
```
