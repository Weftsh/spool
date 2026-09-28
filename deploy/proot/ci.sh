#!/usr/bin/env bash
# The deployment rehearsal without a container runtime, end to end.
#
# Fetch PRoot, prove the sandbox is what we think it is, start the stack,
# build the server image with kaniko under PRoot, run it, and run the
# real smoke against it over HTTP and SSH. Every step prints PASS or FAIL
# and the script exits non-zero on the first FAIL, so the log reads as a
# checklist. `SKIP_BUILD=1` runs everything but the build and the app;
# `SKIP_RUNNERS=1` leaves out the runner image and the smoke's
# self-hosted runner leg.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"; repo="$(cd "$here/../.." && pwd)"
export PROOT_WORK="${PROOT_WORK:-$HOME/.proot}"
step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; "$here/stack.sh" down >/dev/null 2>&1; exit 1; }

step "sandbox"
echo "uid $(id -u); $(grep -E 'CapEff|Seccomp:' /proc/self/status | tr '\n' ' ')"
if unshare -Ur true 2>/dev/null; then echo "user namespaces: allowed"; else echo "user namespaces: refused (as on Fargate and most CI sandboxes)"; fi

step "proot"
"$here/fetch-proot.sh" "$PROOT_WORK/bin/proot" || fail "could not fetch PRoot"
export PROOT="$PROOT_WORK/bin/proot"

step "stack up: postgres + minio under PRoot"
"$here/stack.sh" up || fail "stack did not come up"
# The two probes the approach rests on: a chown by a multi-threaded
# process under fake root, and a static Go binary answering on a socket.
pg="$PROOT_WORK/stack/postgres"
"$PROOT" -0 -r "$pg" -b /proc -b /dev -w / /bin/sh -c 'touch /tmp/p && chown 1234:1234 /tmp/p && chown 0:0 / && echo "fake-root chown, incl. of /: ok"' || fail "PRoot fake root does not fake chown"
"$PROOT" -0 -r "$pg" -b /proc -b /dev -w / /usr/bin/python3 -c 'import os,threading; t=threading.Thread(target=lambda: os.chown("/",0,0)); t.start(); t.join(); print("chown from a second thread: ok")' 2>/dev/null || echo "(no python3 in the postgres image; thread probe skipped)"

step "admin task launcher: RunTask refusals against a fake aws"
# deploy/admin-ecs.sh against a fake `aws`; bash and python3, no daemon.
bash "$repo/deploy/admin-ecs.test.sh" || fail "admin-ecs.test.sh failed"

if [ "${SKIP_BUILD:-0}" = "1" ]; then
  echo; echo "SKIP_BUILD=1: not building or running the production image"
else
  step "build the server image with kaniko under PRoot"
  SECONDS=0
  "$here/build-image.sh" "$repo" Dockerfile "$PROOT_WORK/app" || fail "image build failed"
  echo "build took ${SECONDS}s"

  # The runner image the same way, so the smoke's self-hosted leg runs:
  # it starts a runner as a process under PRoot, registers it, and hands
  # it a job. SKIP_RUNNERS=1 leaves it out and the leg off, which is
  # what a laptop wants while the server image is the question.
  if [ "${SKIP_RUNNERS:-0}" = "1" ]; then
    echo; echo "SKIP_RUNNERS=1: not building the runner image; the smoke's self-hosted leg stays off"
  else
    step "build the runner image with kaniko under PRoot"
    SECONDS=0
    "$here/build-image.sh" "$repo" Dockerfile.runner "$PROOT_WORK/runner" || fail "runner image build failed"
    echo "runner build took ${SECONDS}s"
  fi

  step "run the server image under PRoot"
  "$here/stack.sh" app "$PROOT_WORK/app" || fail "the image did not become healthy"

  step "smoke: health, REST, git over HTTP and SSH, self-hosted runner, CDN, web"
  "$here/stack.sh" smoke || fail "smoke failed"
fi

step "down"
"$here/stack.sh" down
echo; echo "PASS"
