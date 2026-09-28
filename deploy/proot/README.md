# The deployment rehearsal without a container runtime

`deploy/smoke.sh` proves a running spool serves the whole product: the
real `git` CLI cloning, `fsck`ing and pushing over HTTP and SSH, the
dashboard, CDN offload and a self-hosted runner taking a job. With a
Docker daemon, the one-box `docker-compose.yml` is the quickest thing to
run it against (docs/deployment-aws.md, "Prove it locally first"). This
directory runs the same loop with **no daemon at all** — for a CI
sandbox that cannot have one: no privileged mode, no capability but
`CAP_SYS_PTRACE`, and no user namespaces (a Fargate task is exactly
that).

## How

**PRoot** is a user-space `chroot` with bind mounts, implemented by
tracing a process's syscalls and rewriting paths. It needs ptrace and
nothing else: no namespaces, no capabilities, no root. It is how Termux
runs Debian on Android, where namespaces are refused for the same
reason. `run.sh` runs a command inside an image root filesystem under
it, applying the image config the way a runtime would.

**kaniko** builds images from a Dockerfile without a daemon, by
unpacking the base image over its own root and running each `RUN` in
place. Under PRoot with fake root, that root is a directory we own and
the `chown`/`mknod` that apt and useradd perform succeed. `build-image.sh`
builds the server `Dockerfile` (and `Dockerfile.runner`) unmodified,
BuildKit `--mount` flags included.

**pull-image.py** brings an image off a registry's HTTP API into a
directory, the way `scripts/fetch-minio.sh` does for MinIO, generalised:
token auth for Docker Hub, ghcr.io and quay.io, manifest lists resolved
to this machine's architecture, whiteouts applied between layers. It
also flattens the docker-archive tar kaniko writes.

**stack.sh** is `docker-compose.yml` as processes: Postgres and MinIO
from their images, the bucket by one signed PUT, and the server image as
uid 10001, all on localhost with a bound `/etc/hosts` for the compose
service names. It adds CDN offload in its origin-route shape, which the
compose file leaves off, so the smoke's CDN leg has something to prove.
The values are the compose file's; change them together.

**smoke-runner-driver.sh** is the smoke's self-hosted leg's "other
machine": the runner image's root filesystem run under PRoot as uid
10002, with the four verbs the leg needs (start, logs, exit code,
remove) as a pid file, a log file and a session to signal.
`deploy/smoke.sh` sources it when `SMOKE_RUNNER_DRIVER` names it; the
default is still docker.

**ci.sh** runs it all as a checklist: sandbox probes, PRoot, the stack,
the `admin-ecs.sh` test against a fake `aws`, the two image builds, the
server under PRoot, and the smoke with the self-hosted leg on. Every
step prints PASS or FAIL.

## What this does and does not prove

It proves the server image builds from the unmodified Dockerfile, its
contents (a CA store, `git`, `tini`) are what the smoke needs, and the
real `git` CLI clones, `fsck`s and pushes through it; and that a runner
built from `Dockerfile.runner` registers, takes a job, streams its log,
and stops itself when removed. It does not prove kernel-level isolation,
cgroup limits or capability drops; those live in the task definition
that `terraform validate` covers.

## Where the images come from

`fetch-proot.sh` pins a static PRoot by digest (`PROOT_IMAGE`), and
`build-image.sh` pulls kaniko from `KANIKO_IMAGE`; the stack's Postgres
and MinIO come from `POSTGRES_IMAGE` and `MINIO_IMAGE`, pinned by
`.postgres-version` and `.minio-version`. The defaults are copies on
ghcr.io: MinIO no longer publishes images anywhere else, Docker Hub
rate-limits anonymous pulls per source address, and the PRoot is a build
that translates `openat2` and `fchmodat2` (below), which no upstream
release does. Override any of them to use your own registry.

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
  was the PRoot here until the `openat2` finding below.)
- Docker Hub meters anonymous pulls per source address, and a CI NAT is
  one address. `pull-image.py` backs off on 429, and the stack's
  Postgres comes from a ghcr.io copy the way MinIO does, so the stack
  never asks Docker Hub.
- `tini` is not PID 1 under PRoot; `TINI_SUBREAPER=1` keeps it quiet.
- PRoot did not translate `openat2` (Linux 5.6) — neither the vendored
  udocker 4.8.0 nor upstream 5.4.1 — so the call ran untranslated,
  against the tracee's *kernel* cwd rather than the guest's. GNU tar
  1.35 (Ubuntu noble) creates every nested directory that way:
  `openat2(AT_FDCWD, "x/", O_PATH, RESOLVE_BENEATH)` and then
  `mkdirat(fd, "y")`, so under kaniko an image build that unpacked a
  tarball failed 22,000 times with `./bin/…: Cannot mkdir: No such file
  or directory` while the same tarball extracted fine natively. Not an
  Apple Silicon limitation, as the local box first made it look.
  `strace` cannot attach inside PRoot;
  `proot -v 5` showed tar's `mkdirat` never carrying a second-level
  path, and `strace` of the same tar chrooted into the kaniko root
  showed the `openat2`. `tar -C /abs/path` works because tar then holds
  a descriptor on the target and every later call is fd-relative, which
  needs no translation. The fix is the PRoot `fetch-proot.sh` pins: a
  static build of termux/proot — which rewrites `openat2` to `openat` —
  plus upstream's `fchmodat2` support (proot-me/proot#408, which tar's
  final chmod of each directory needs under fake root), for both
  architectures, pinned by digest.

## Running it

`scripts/ci-local.sh --only deploy-validation`, where the local gate
still has that job: on Linux it runs `ci.sh` itself, with its work under
`.proot-local`; on a laptop with Docker it runs `local-model.sh`, which
builds a locked-down box (uid 10002, no capabilities, user namespaces
refused) and runs `ci.sh` inside it — nothing inside can reach a daemon
— on this machine's architecture. About fifteen minutes cold either way.
By hand:

```sh
PROOT_WORK=$PWD/.proot-local deploy/proot/ci.sh     # everything
SKIP_RUNNERS=1 deploy/proot/ci.sh                   # no runner image or leg
SKIP_BUILD=1 deploy/proot/ci.sh                     # probes and the stack only
```

Behind a TLS-intercepting proxy, set `EXTRA_CA` to the proxy's CA and
`GIT_SSL_CAINFO`/`SSL_CERT_FILE` to the same file; the scripts pass
them through to kaniko and to the server under test.
