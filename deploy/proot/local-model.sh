#!/usr/bin/env bash
# Run deploy/proot/ci.sh on a laptop, inside a container that models a
# locked-down CI sandbox (a Fargate task, say): uid 10002, no
# capabilities, Docker's default seccomp profile, and user-namespace
# creation refused. Docker is used only to build that box; nothing
# inside it can reach a daemon.
#
#   deploy/proot/local-model.sh            # everything, ~15 minutes
#   SKIP_BUILD=1 deploy/proot/local-model.sh   # sandbox probes + the stack
#
# The work directory is a Docker named volume, weft-proot-work, so a
# second run skips the pulls and kaniko's cache mounts stay warm. A
# volume and not a bind mount: a laptop's filesystem is case-insensitive
# (APFS, NTFS) and a Linux root filesystem holds names that differ only
# by case (xt_mark.h beside xt_MARK.h in the kernel headers), so a
# rootfs unpacked onto a bind mount silently loses one of each pair.
# `docker volume rm weft-proot-work` starts over.
set -euo pipefail
repo="$(cd "$(dirname "$0")/../.." && pwd)"
docker volume create weft-proot-work >/dev/null
# The box is the laptop's own architecture. An amd64 box on Apple Silicon
# runs under Rosetta, and Rosetta has no ptrace: `ptrace(TRACEME):
# Function not implemented`, so PRoot cannot start. Native arm64 models
# the mechanism (ptrace, no capabilities, no user namespaces) exactly.
# The scripts pick the PRoot build and the image layers for the host by
# themselves.
docker build -q -t weft-fargate-model - <<'DF' >/dev/null
FROM python:3.12-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates tar git openssh-client procps util-linux \
 && rm -rf /var/lib/apt/lists/* && useradd -u 10002 -m -s /bin/bash runner
USER 10002
DF
# The volume is root-owned when Docker creates it; hand it to uid 10002.
docker run --rm -v weft-proot-work:/home/runner weft-fargate-model test -w /home/runner 2>/dev/null \
  || docker run --rm --user 0 -v weft-proot-work:/home/runner weft-fargate-model chown 10002:10002 /home/runner
# A TTY only when there is one: under a CI step or a pipe, -t refuses.
# (A string, not an array: macOS ships bash 3.2, where an empty array is
# unbound under set -u.)
tty=""; [ -t 0 ] && tty="-it"
# shellcheck disable=SC2086
exec docker run --rm $tty --user 10002 --cap-add SYS_PTRACE --network host \
  -e HOME=/home/runner -e SKIP_BUILD="${SKIP_BUILD:-0}" -e SKIP_RUNNERS="${SKIP_RUNNERS:-0}" \
  -e KANIKO_IMAGE="${KANIKO_IMAGE:-gcr.io/kaniko-project/executor:v1.24.0}" \
  -v weft-proot-work:/home/runner -v "$repo:/repo:ro" \
  weft-fargate-model bash /repo/deploy/proot/ci.sh
