# Spool

**A self-hosted git forge, served from object storage, running in your own
infrastructure.**

Spool keeps every repository in an S3-compatible bucket you own and serves it
from stateless nodes: any node can answer for any repository, and every
repository is an ordinary git remote over HTTP and SSH. On top of that sit
code review across repositories, issues, teams and per-repository access,
branch protection and required checks, mirrors and imports from GitHub, forks
inside your instance, and CI — `.weft` workflows on machines you register,
plus an intake for any CI you already run.

[![CI](https://github.com/Weftsh/spool/actions/workflows/ci.yml/badge.svg)](https://github.com/Weftsh/spool/actions/workflows/ci.yml)
[![License: FSL-1.1-ALv2](https://img.shields.io/badge/license-FSL--1.1--ALv2-blue)](LICENSE.md)

> **Pre-release.** No release has been published yet. You can build it and
> [try it on one machine](#try-it-on-one-machine) today; see
> [Project status](#project-status) for what that means.

---

## Why Spool

- **Your code stays in your infrastructure.** Repositories live in your
  bucket, the control plane in your PostgreSQL, and the server in your
  network. Nothing phones home.
- **Every repository is private to its organization.** There is no public
  surface: a request with no credential is refused, and a person with no
  role in an organization cannot tell its repositories from ones that do not
  exist.
- **Stateless serving.** Storage is immutable segments in object storage,
  with the manifest as the only ref truth, changed only by compare-and-swap.
  Scale by adding nodes; lose a node and nothing is lost.
- **Real git.** Clone, fetch and push with the git you already have, over
  HTTP or SSH. Every clone passes `git fsck --full --strict`.
- **Review that spans repositories.** A change is reviewed and landed like a
  pull request; a changeset lands changes in several repositories together,
  in dependency order, or not at all.
- **CI on your own machines.** Workflows in `.weft/` run on runners you
  register with `weft-runner`, in groups you decide which repositories may
  use. A change from a fork waits for a maintainer before any of its code
  runs. Verdicts from any other CI arrive through a signed intake.
- **Bring your GitHub history.** Mirror a GitHub repository (and push
  through to it), or import one with its issues.

## Try it on one machine

You need Docker with Compose.

```sh
git clone https://github.com/Weftsh/spool && cd spool
./deploy/dev-host-key.sh                     # once: the SSH host key
docker compose up -d --build --wait          # spool, PostgreSQL, MinIO
docker compose exec spool stratum-server admin bootstrap --org acme
docker compose exec spool stratum-server admin user-create \
  --org acme --email you@example.com --password 'a long enough password'
```

Open <http://127.0.0.1:8080/>, sign in, create a repository and push to it:

```sh
git remote add spool http://127.0.0.1:8080/acme/app.git
git push spool main
```

`docker-compose.yml` explains how to serve other machines (TLS in front of
port 8080, SSH on 2222) and which passwords to change first.

## Run workflows on your machines

A workflow is a YAML file in `.weft/`. A job with no `runs-on` runs on any
runner your organization registered; `runs-on: [self-hosted, gpu]` asks for
one with those labels.

```yaml
# .weft/ci.yml
name: ci
on: [push, change]
jobs:
  test:
    steps:
      - run: cargo test
```

Register a machine with a token from **Settings → Runners**, then start it —
under an account of its own, not root:

```sh
weft-runner register --url https://spool.example.com --token weftg_… --labels gpu
weft-runner run
```

Steps run directly on that machine, as that account. See
[docs/guide/self-hosted-runners.md](docs/guide/self-hosted-runners.md).

## Deploy it

- [docs/operations.md](docs/operations.md) — configuration reference,
  migrations, backups, upgrades, mail, SSH, the GitHub App, runners.
- [docs/deployment-aws.md](docs/deployment-aws.md) — the Terraform reference
  deployment for AWS (ECS, RDS, S3, CloudFront) in `deploy/terraform`.

## Documentation

- [The user guide](docs/guide/README.md): repositories, review, changesets,
  workflows, CI integration, mirrors and imports, SSH, webhooks, search.
- [The REST API](docs/openapi.json) (OpenAPI 3.1).
- [Storage model and invariants](reference/) — how the engine lays a
  repository out in object storage, and what it promises.

## Layout

```
crates/stratum-store    engine: manifest, S3 client (SigV4, conditional PUT),
                        locator plane, pack parsing
crates/stratum-proto    engine: git protocol v2 serving + receive-pack
crates/stratum-engine   ingest, locator build, compaction, GC
crates/stratum-control  control plane: orgs, repos, people, tokens, audit,
                        review, workflows, runners (PostgreSQL)
crates/stratum-server   the server binary: git over HTTP and SSH, the REST
                        API, the dashboard, background workers
crates/stratum-runner   the runner agent (`weft-runner`)
crates/stratum-testkit  hermetic test harnesses: MinIO, PostgreSQL, fakes
web/dashboard           the web UI (React + Vite)
deploy/                 the container image's companions and the AWS
                        reference deployment
docs/                   operations, deployment and the user guide
```

## Development

```sh
scripts/ci-local.sh            # everything CI runs, in CI's order
scripts/ci-local.sh --fast     # without the chaos suite
scripts/ci-local.sh --only web # one job
```

The suites are hermetic: they start their own PostgreSQL and MinIO and never
reach the network. [CONTRIBUTING.md](CONTRIBUTING.md) has the mechanics and
[CLAUDE.md](CLAUDE.md) the bar a change has to clear.

## Project status

Spool is the forge behind Weft's hosted product, carved out to run on your
own infrastructure. The engine, protocol, review, workflows and runner agent
are the code the hosted product runs; what the self-hosted edition removes is
listed in the commit history. Before the first release:

- there are no published binaries or images yet — build from source;
- upgrade paths between pre-release builds are not promised;
- the commercial license is not in place yet. It will work the way
  [Weft Sandboxes](https://github.com/Weftsh/sandy/blob/main/docs/licensing.md)
  does: a license key verified locally, at most one small daily check whose
  fields are listed in the docs, and warnings — never a refusal — when a
  license is missing or lapsed.

## Security

Please report vulnerabilities privately — see [SECURITY.md](SECURITY.md).

## License

The server, runner and dashboard are licensed under the
[Functional Source License 1.1, Apache-2.0 future license](LICENSES/FSL-1.1-ALv2.md);
deployment, docs and scripts under [Apache-2.0](LICENSES/Apache-2.0.txt).
See [LICENSE.md](LICENSE.md).
