#!/usr/bin/env bash
# Put the PRoot binary at $1 (default deploy/proot/bin/proot).
#
# One source: ghcr.io/weftsh/proot, built by weftsh/proot-build from
# termux/proot plus a fchmodat2 patch, pinned here by digest. It is the
# PRoot that translates openat2 and fchmodat2 — GNU tar 1.35 creates
# every nested directory through the first and chmods it through the
# second, and a PRoot that knows neither (udocker's 4.8.0, which used to
# be vendored here; upstream 5.4.1) fails every second-level entry of a
# tarball extracted into a WORKDIR (README, "Things that bit"). ghcr.io
# is reachable from the fleet and is where kaniko, MinIO and Postgres
# already come from; the checksum is the image digest.
#
# A binary already at $1 is kept only if it came from this pin: the stamp
# beside it names the image it was pulled from, so a bumped digest is
# fetched and a work directory holding an older PRoot is not trusted.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="${1:-$here/bin/proot}"
image="${PROOT_IMAGE:-ghcr.io/weftsh/proot@sha256:e7e88398e33040ac25b5be86d05b8ac811be54682190a88d13fe846adea69002}"
if [ -x "$out" ] && [ "$(cat "$out.source" 2>/dev/null)" = "$image" ] && "$out" --version >/dev/null 2>&1; then
  echo "proot already at $out ($image)"; exit 0
fi
mkdir -p "$(dirname "$out")"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
python3 "$here/pull-image.py" "$image" "$tmp/proot-image"
[ -f "$tmp/proot-image/proot" ] || { echo "the image carries no /proot" >&2; exit 1; }
install -m 755 "$tmp/proot-image/proot" "$out"
printf '%s\n' "$image" > "$out.source"
"$out" --version | head -1; echo "proot: $image"
