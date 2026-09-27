#!/usr/bin/env bash
# Reclaim disk after a build/test/coverage cycle.
#
# The heavy hitters, in the order they fill a disk:
#   target/llvm-cov-target   a SECOND full build tree — `cargo llvm-cov`
#                            never reuses target/debug, so a coverage run
#                            roughly doubles the workspace on disk
#   target/debug/incremental recompilation cache; pure scratch, and the
#                            largest thing that is safe to delete blind
#   docker build cache       BuildKit keeps every layer of every image
#                            build; a few image builds is several GB
#   .testkit/                downloaded MinIO binary and its scratch
#   stratum-* temp dirs      test scratch that outlived a killed run
#
# Usage:
#   scripts/clean-build-artifacts.sh            # scratch only (safe, fast rebuild)
#   scripts/clean-build-artifacts.sh --deep     # also target/debug and docker images
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEEP=0
[ "${1:-}" = "--deep" ] && DEEP=1

before="$(df -Pk "$ROOT" | awk 'NR==2 {print $4}')"
say() { printf '  %s\n' "$*"; }

echo "cleaning build artifacts under $ROOT"

# Coverage's separate build tree: always disposable, always large.
if [ -d "$ROOT/target/llvm-cov-target" ]; then
  say "target/llvm-cov-target ($(du -sh "$ROOT/target/llvm-cov-target" | cut -f1))"
  rm -rf "$ROOT/target/llvm-cov-target"
fi

# Incremental cache: scratch by definition.
for d in "$ROOT"/target/*/incremental; do
  [ -d "$d" ] || continue
  say "$(basename "$(dirname "$d")")/incremental ($(du -sh "$d" | cut -f1))"
  rm -rf "$d"
done

# Test scratch that outlived a killed run (the testkit's own prefixes only).
# A long session leaves hundreds of these, so report a count, not a list.
# Rust's `temp_dir()` is `$TMPDIR` when set — on macOS a per-user
# /var/folders/... path — so sweeping /tmp alone found nothing there, and
# the postgres clusters (`stratum-testkit-pg-*`) and server data dirs
# (`stratum-e2e-*`) were never on the list at all. That is how one of them
# came to be adopted by a later run under a reused pid.
for tmp in "${TMPDIR:-/tmp}" /tmp; do
  scratch=$(find "$tmp" -maxdepth 1 \( -name 'stratum-test-*' -o -name 'stratum-testkit-*' \
    -o -name 'stratum-minio*' -o -name 'stratum-e2e-*' -o -name 'stratum-mail-*' \
    -o -name 'weft-runner-e2e-*' \) 2>/dev/null || true)
  if [ -n "$scratch" ]; then
    say "$(printf '%s\n' "$scratch" | wc -l | tr -d ' ') stale test scratch dirs in $tmp"
    printf '%s\n' "$scratch" | xargs -r rm -rf
  fi
  [ "$tmp" = /tmp ] && break
done

if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  say "docker build cache"
  docker builder prune -af >/dev/null 2>&1 || true
  if [ "$DEEP" = 1 ]; then
    # `-a`, so this is every image no container references — not just
    # the dangling ones. That is the point (it is where the tens of
    # gigabytes are) but it reaches beyond this repository: images other
    # projects on this machine pulled or built go too, and come back
    # only by pulling or building them again. Say so rather than
    # labelling it "dangling", which is what it used to say and is a
    # meaningfully smaller promise.
    say "docker images not referenced by a container (ALL projects, re-pullable)"
    docker image prune -af >/dev/null 2>&1 || true
  fi
fi

if [ "$DEEP" = 1 ]; then
  # A full rebuild costs minutes; only on request.
  say "target/debug + target/release (full rebuild next time)"
  rm -rf "$ROOT/target/debug" "$ROOT/target/release"
fi

# Orphaned System V shared-memory segments from test postgres clusters.
#
# A postmaster killed with SIGKILL never detaches its startup-interlock
# segment. The harness now shuts clusters down with SIGINT so this should
# stay empty, but a hard-killed test run (Ctrl-C, a panic in the wrong
# place, `cargo test` timing out) still leaks one per cluster.
#
# It matters far more than the size suggests — each segment is 56 bytes,
# but macOS caps the whole machine at 32 of them (`kern.sysv.shmmni`,
# versus thousands on Linux). Once they are gone, every `initdb` fails
# with "could not create shared memory segment: No space left on device",
# which is not a disk problem, says so in its own HINT, and surfaces in
# whichever unlucky test runs next rather than in the run that leaked.
#
# Only segments with nothing attached (NATTCH 0) are removed, so a real
# postgres or any other running program is left alone.
if command -v ipcs >/dev/null 2>&1 && command -v ipcrm >/dev/null 2>&1; then
  orphans="$(ipcs -m -o 2>/dev/null | awk '$1=="m" && $NF==0 {print $2}')"
  if [ -n "$orphans" ]; then
    say "orphaned shared-memory segments ($(echo "$orphans" | wc -w | tr -d ' '))"
    for id in $orphans; do ipcrm -m "$id" 2>/dev/null || true; done
  fi
fi

after="$(df -Pk "$ROOT" | awk 'NR==2 {print $4}')"
printf 'reclaimed %s MiB (%s MiB free)\n' \
  "$(( (after - before) / 1024 ))" "$(( after / 1024 ))"
