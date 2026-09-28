#!/usr/bin/env bash
# Per-worktree test dependencies: a MinIO of this checkout's own, and a
# preview port nobody else is using.
#
#   eval "$(scripts/test-env.sh)"
#   cargo test --workspace        # or scripts/ci-local.sh
#
# **Why this exists.** Several worktrees of this repository live on one
# machine, and the test harness had two machine-wide singletons in it.
# Playwright previews on a fixed 4173 with `--strictPort`, and
# `ci-local.sh` sets `CI=true` on purpose so the local run reproduces CI
# rather than approximating it — which also turns off `reuseExistingServer`.
# Two checkouts running the web gate at once therefore collide, and the
# collision does not present as a port error: it presents as a run where
# every test passes and the process exits non-zero. That reads as a
# product failure, and one session on this machine had already resorted
# to a hand-written `lsof` wait loop rather than to a fix.
#
# MinIO is the same shape of problem one step behind: the harness needs
# one (see scripts/fetch-minio.sh for why it is a container on anything
# that is not Linux), and two checkouts both publishing 9000 would fight
# over it the moment the second one started.
#
# Everything here is per-worktree and idempotent: run it again and it
# reuses what is already up. Unset, every default is what it always was —
# 4173, and a MinIO the testkit spawns itself — so a single checkout and
# CI are unaffected.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
release="$(tr -d '[:space:]' < "$root/.minio-version")"
# Another copy of the pinned image, when quay.io is not reachable from
# here — the same variable scripts/fetch-minio.sh and manual-stack.sh read
# (a repository, no tag).

# `--fresh` throws the container away and starts a new one.
#
# Worth knowing about, because the container is long-lived and every test
# ever run against it leaves its bucket behind — 797 of them after an
# afternoon here. That is not a correctness problem (buckets are unique
# per test) but it is a *speed* one: the same store test took 3.8s
# against a new container and 11.7s against that one, and the suites
# most sensitive to it are the ones timing a read against a compaction.
#
# CI never meets this. There the testkit spawns a MinIO per test process,
# so every binary gets an empty store; the shared container is a macOS
# accommodation (scripts/fetch-minio.sh explains why) and this is its
# one sharp edge.
fresh=0
[ "${1:-}" = "--fresh" ] && fresh=1

# Named for the worktree, so `docker ps` says which checkout a container
# belongs to and two of them cannot be mistaken for one.
slug="$(basename "$root" | tr -c 'a-zA-Z0-9_.-' '-' | tr -s '-' | sed 's/-$//' | cut -c1-40)"
name="stratum-test-minio-$slug"

# A free port, asked for the way the harness asks: bind 0, read it back,
# let it go. `stratum-testkit`'s `free_port` has the documented race this
# inherits — something else may take it in the gap — which is why the
# container start below is checked rather than assumed.
free_port() {
  python3 -c 'import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()'
}

# Reuse a running one. `docker start` on a stopped container of the same
# name rather than a second container: the bucket state does not matter
# (every test makes its own) but two containers with one name is a mess
# somebody has to clean up by hand.
if [ "$fresh" = 1 ]; then docker rm -f "$name" >/dev/null 2>&1 || true; fi
state="$(docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null || echo missing)"
case "$state" in
  true)  ;;
  false) docker start "$name" >/dev/null ;;
  *)
    port="$(free_port)"
    docker run -d --name "$name" \
      -p "127.0.0.1:$port:9000" \
      -e MINIO_ROOT_USER=stratum-test \
      -e MINIO_ROOT_PASSWORD=stratum-test-only \
      -e MINIO_BROWSER=off \
      "${STRATUM_MINIO_IMAGE:-quay.io/minio/minio}:$release" server /data >/dev/null
    ;;
esac

# Ask docker where it actually landed rather than remembering: on a reuse
# this script never chose the port, and on a fresh start the daemon is
# the one that bound it.
minio_port="$(docker port "$name" 9000/tcp | head -1 | sed 's/.*://')"
[ -n "$minio_port" ] || { echo "could not read the published port of $name" >&2; exit 1; }
url="http://127.0.0.1:$minio_port"

# Wait for readiness here rather than leaving the first test to meet a
# cold container: /health/ready and not /live, because liveness turns 200
# before the S3 API accepts a request.
for _ in $(seq 1 60); do
  if curl -fsS -o /dev/null "$url/minio/health/ready" 2>/dev/null; then ready=1; break; fi
  sleep 0.5
done
[ "${ready:-}" = 1 ] || { echo "$name never became ready at $url" >&2; exit 1; }

# Stable across invocations of this script within one worktree, so the
# address you opened by hand still works after a re-run — and so
# `reuseExistingServer` means something when CI is not set.
preview="${STRATUM_PREVIEW_PORT:-}"
if [ -z "$preview" ]; then
  base=$(( 4200 + $(printf '%s' "$root" | cksum | cut -d' ' -f1) % 300 ))
  preview=$base
  while lsof -i ":$preview" >/dev/null 2>&1; do preview=$(( preview + 1 )); done
fi

echo "export STRATUM_MINIO_URL=$url"
echo "export STRATUM_PREVIEW_PORT=$preview"
echo "# minio: $name on $url (docker rm -f $name to drop it)" >&2
echo "# preview: 127.0.0.1:$preview" >&2
