# deploy-validation without a container runtime

`deploy-validation` in `ci.yml` is the one CI job that needs a Docker
daemon: it builds three images, brings up the five-container stack in
`deploy/compose.yml`, and runs the real `git` CLI through the production
image over HTTP and SSH. Fargate, which our own runners are, refuses
every way of getting a daemon: no privileged mode, no capability but
`CAP_SYS_PTRACE`, and no user namespaces (`clone(CLONE_NEWUSER)` answers
`EPERM`; AWS has had the request open since 2023). So that job runs on
GitHub's hosted runners, and when those are unavailable the gate is
silent.

This directory runs the same loop with no daemon at all.

## How

**PRoot** is a user-space `chroot` with bind mounts, implemented by
tracing a process's syscalls and rewriting paths. It needs ptrace and
nothing else: no namespaces, no capabilities, no root. It is how Termux
runs Debian on Android, where namespaces are refused for Fargate's
reason. `run.sh` runs a command inside an image root filesystem under
it, applying the image config the way a runtime would.

**kaniko** builds images from a Dockerfile without a daemon, by
unpacking the base image over its own root and running each `RUN` in
place. Under PRoot with fake root, that root is a directory we own and
the `chown`/`mknod` that apt and useradd perform succeed. `build-image.sh`
builds the production `Dockerfile` unmodified, BuildKit `--mount` flags
included.

**pull-image.py** brings an image off a registry's HTTP API into a
directory, the way `scripts/fetch-minio.sh` already does for MinIO,
generalised: token auth for Docker Hub, ghcr.io and quay.io, manifest
lists resolved to linux/amd64, whiteouts applied between layers. It also
flattens the docker-archive tar kaniko writes.

**stack.sh** is `deploy/compose.yml` as processes: Postgres and MinIO
from their images, the bucket by one signed PUT, the ECS stand-in with
its proot backend, the production image as uid 10001, all on localhost
with a bound `/etc/hosts` for the compose service names. A Fargate task's containers share a network namespace,
so this is the shape a real task has.

**ci.sh** runs it all as a checklist and is what ci.yml's
`deploy-validation` job runs on `weft-4x`, on every push.

## What was measured

In a container modelling a Fargate task (uid 10002, zero capabilities,
Docker's default seccomp, user namespaces refused), 2026-09-21:

| step | result |
|---|---|
| Postgres 16, MinIO (`ghcr.io/weftsh/minio`) under PRoot | ready in 2–4 s |
| production `Dockerfile`, kaniko under PRoot, cold | 12 m 25 s including image pulls |
| of which `cargo build --release` | 5 m 20 s cold, ~2 m 30 s with the cache mount warm |
| of which each Node stage | ~70 s |
| built image under PRoot as uid 10001 | `healthz` in 2 s, `readyz` 200 |
| `deploy/smoke.sh` over HTTP and SSH, CDN leg, web surfaces | **SMOKE OK** |
| the same loop through `ci.sh` as committed, cold, pulls included | 17 m 7 s build, then **PASS** |

On a real `weft-4x` Fargate task (x86_64), 2026-09-22, dispatched from
`docker-on-fargate.yml` with `skip_github_runner`:

| step | result |
|---|---|
| production `Dockerfile`, kaniko under PRoot | 12 m 3 s cold, 7 m 4 s with kaniko's cache mounts warm |
| Weft runner image | 2 m 15 s |
| built image under PRoot | `healthz` in 3 s, `readyz` 200 |
| `deploy/smoke.sh`, all eleven stages, both runner legs | **SMOKE OK**, then **PASS** |

On a real `weft-4x` Fargate task, 2026-09-23, everything on and nothing
skipped — the run that earned the job its place in ci.yml:

| step | result |
|---|---|
| sandbox probes, PRoot, Postgres + MinIO, the ECS stand-in's refusal checks | as before |
| production `Dockerfile`, Weft runner image, GitHub Actions runner image | all three built |
| built image under PRoot, `deploy/smoke.sh`, all eleven stages, both runner legs | **SMOKE OK**, then **PASS** |
| the whole job, checkout to PASS | 16 m 53 s |

Getting the GitHub runner image there took three fleet runs after the
allow-list learned `mcr.microsoft.com`: its regional data hosts
(`.data.mcr.microsoft.com`), then the Ubuntu apt mirrors, then the
`openat2` finding below.

The compile ran at roughly the pace of GitHub's hosted runner on four
cores, not the two to five times slower that ptrace was expected to
cost.

## What this does and does not prove

It proves what the gate exists to prove: the production image builds
from the unmodified Dockerfile, its contents (a CA store, `git`, `tini`)
are what the smoke needs, and the real `git` CLI clones, `fsck`s and
pushes through it. It does not prove kernel-level isolation, cgroup
limits or capability drops; compose never did either, and those live in
the task definition that `terraform validate` covers.

The runner legs run too. `deploy/fake-ecs/fake-ecs.py` has a `proot`
backend (`FAKE_ECS_BACKEND=proot`): a RunTask starts the runner's root
filesystem under PRoot as uid 10002 with its own `/work`, the job's
environment reaching it through a file `run.sh` reads and deletes
rather than argv, and a StopTask is SIGTERM to the task's session with
SIGKILL ten seconds later, the task definition's clock. The self-hosted
leg's "other machine" is the same thing through
`smoke-runner-driver.sh`, which `deploy/smoke.sh` sources when
`SMOKE_RUNNER_DRIVER` names it; the leg's four docker calls are behind
four functions whose default is still docker. `ci.sh` builds the two
runner images with kaniko after the app (`SKIP_RUNNERS=1` leaves them
out) and runs the smoke with both legs on.

## Things that bit, so they are not learned twice

- PRoot applies no image config and injects no `/etc/resolv.conf`.
  `run.sh` applies `Env`, `WorkingDir`, `User`, `Entrypoint`/`Cmd` and
  a `HOME`, and binds the host resolver: the first Postgres died on a
  missing `PGDATA`, the first cargo on `index.crates.io` not resolving,
  and MinIO on no `HOME`.
- kaniko wipes the filesystem between stages and trips over any path
  PRoot binds in. Every binding is also an `--ignore-path`.
- kaniko passes proxies into `RUN` only as the predefined build args,
  as `docker build` does. The `extra_ca` secret the Dockerfile reads is
  served by binding `$EXTRA_CA` at `/run/secrets/extra_ca`.
- udocker's wrapper around PRoot breaks kaniko on a `chown /` that raw
  PRoot fakes correctly. Drive PRoot directly. (udocker's static build
  was the PRoot here until the `openat2` finding below; it is
  `ghcr.io/weftsh/proot` now.)
- Docker Hub meters anonymous pulls per source address and the fleet's
  NAT is one address. `pull-image.py` backs off on 429, and Postgres is
  mirrored to `ghcr.io/weftsh` the way MinIO is, so the stack never
  asks Docker Hub.
- `tini` is not PID 1 under PRoot; `TINI_SUBREAPER=1` keeps it quiet.
- Docker Hub serves layers from an S3 bucket in us-east-1, and from a
  runner that request rides the VPC's S3 gateway endpoint, whose policy
  admitted only ECR's bucket: `AccessDenied` for Docker Hub's own
  signing role, on a host the firewall allows by name. The policy in
  `modules/runner/main.tf` now names `docker-images-prod`. The stack no
  longer needs it: Postgres comes from `ghcr.io/weftsh/postgres`, copied
  from the official image by `weftsh/postgres-mirror` and pinned by
  `.postgres-version`. The production Dockerfile's base images still do.
- The fleet's allow-list drops raw.githubusercontent.com whatever its
  wildcard entry says: the first fleet run sat five minutes on a TLS
  handshake there. PRoot was vendored under `vendor/` for that reason
  until it moved to `ghcr.io/weftsh/proot`, which `fetch-proot.sh` pulls
  by digest with `pull-image.py` — the same channel as kaniko, MinIO and
  Postgres.
- PRoot did not translate `openat2` (Linux 5.6) — neither the vendored
  udocker 4.8.0 nor upstream 5.4.1 — so the call ran untranslated,
  against the tracee's *kernel* cwd rather than the guest's. GNU tar
  1.35 (Ubuntu noble) creates every nested directory that way:
  `openat2(AT_FDCWD, "x/", O_PATH, RESOLVE_BENEATH)` and then
  `mkdirat(fd, "y")`, so under kaniko the first fleet build of
  `Dockerfile.github-runner` failed 22,000 times with
  `./bin/…: Cannot mkdir: No such file or directory` while the same
  tarball extracted fine natively. Not an Apple Silicon limitation, as
  the local box first made it look. `strace` cannot attach inside PRoot;
  `proot -v 5` showed tar's `mkdirat` never carrying a second-level
  path, and `strace` of the same tar chrooted into the kaniko root
  showed the `openat2`. `tar -C /abs/path` works because tar then holds
  a descriptor on the target and every later call is fd-relative, which
  needs no translation; the Dockerfile says so where it does it. The
  fix is the PRoot the fleet runs now: `ghcr.io/weftsh/proot`, built by
  `weftsh/proot-build` from termux/proot — which rewrites `openat2` to
  `openat` — plus upstream's `fchmodat2` support (proot-me/proot#408,
  2026-09-15, which termux had not taken and which tar's final chmod of
  each directory needs under fake root), static for both architectures,
  pinned by digest in `fetch-proot.sh`. `ci.sh` pins the behaviour on the
  GitHub runner image's own tar 1.35: a nested tarball extracted into a
  cwd under `-0`, which the old PRoot fails on the first second-level
  entry.

## Running it

On the fleet: it is the `deploy-validation` job of `.github/workflows/ci.yml`,
on every push to every branch, `weft-4x`, about seventeen minutes cold.
The kaniko image comes from `ghcr.io/weftsh/kaniko:v1.24.0`, copied from
`gcr.io/kaniko-project/executor` by `weftsh/kaniko-mirror` the way MinIO
and Postgres are; the Dockerfiles' own base images come from Docker Hub,
MCR and the Ubuntu and Debian mirrors, all of which the egress allow-list
in `deploy/terraform/variables.tf` names.

Locally, `scripts/ci-local.sh --only deploy-validation`: on Linux it runs
`ci.sh` itself, with its work under `.proot-local`; on a laptop with
Docker it runs `deploy/proot/local-model.sh`, which builds the
Fargate-model box and runs `ci.sh` inside it — nothing inside can reach a
daemon — on this machine's architecture rather than the fleet's.
Fifteen minutes cold either way.

Behind a TLS-intercepting proxy, set `EXTRA_CA` to the proxy's CA and
`GIT_SSL_CAINFO`/`SSL_CERT_FILE` to the same file; the scripts pass
them through to kaniko and to the server under test.
