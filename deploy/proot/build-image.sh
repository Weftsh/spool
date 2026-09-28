#!/usr/bin/env bash
# Build an image from a Dockerfile with kaniko running under PRoot.
#
#   build-image.sh CONTEXT DOCKERFILE OUT_ROOTFS [kaniko args...]
#
# kaniko already builds without a daemon or privileges: it unpacks the
# base image over its own root and runs each RUN step in place. Under
# PRoot that root is a directory we own, PRoot's fake root (`-0`) makes
# the chown/mknod that apt and useradd do succeed, and the result is a
# docker-archive tar that pull-image.py flattens into OUT_ROOTFS. The
# Dockerfile is not changed: BuildKit's `RUN --mount` flags parse, the
# `extra_ca` secret is served from $EXTRA_CA if set, and proxies travel
# as the predefined build args, exactly as with `docker build`.
#
# Three things learned the hard way, so they are not learned again:
# - kaniko wipes the filesystem between stages and trips over any path
#   PRoot binds in, so every binding is also an --ignore-path;
# - the build context is bound read-only in spirit but kaniko writes
#   into it (it snapshots there), so it gets a scratch copy;
# - the kaniko image itself comes from KANIKO_IMAGE, by default a copy
#   on ghcr.io (upstream's gcr.io/kaniko-project/executor works too);
# - Docker Hub's front door for daemon-less clients is index.docker.io,
#   which an egress allow-list may not name (it usually names
#   registry-1, the daemon's host); --registry-map sends kaniko to
#   registry-1 directly.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
ctx=${1:?context}; dockerfile=${2:?dockerfile}; out=${3:?out rootfs}; shift 3
work="${PROOT_WORK:-$HOME/.proot}"; mkdir -p "$work"
KANIKO_IMAGE="${KANIKO_IMAGE:-ghcr.io/weftsh/kaniko:v1.24.0}"
kaniko="$work/kaniko"
if [ ! -x "$kaniko/kaniko/executor" ]; then
  echo "pulling $KANIKO_IMAGE"
  python3 "$here/pull-image.py" "$KANIKO_IMAGE" "$kaniko"
fi
# A fresh copy of the executor's root per build: kaniko unpacks the base
# image over "/" and a previous build's leftovers are not a base image.
# A previous build's root is owned by this uid but carries the modes
# kaniko set under fake root, some without owner write, so `rm` alone
# cannot clear it; make it writable first.
root="$work/kaniko-build"; [ -d "$root" ] && { chmod -R u+rwX "$root" 2>/dev/null || true; }; rm -rf "$root"; cp -a "$kaniko" "$root"
src="$work/context"; rm -rf "$src"; mkdir -p "$src"
# Tracked files only when the context is a git checkout: a developer's
# tree carries target/ and node_modules/, which .dockerignore keeps out
# of `docker build` and which kaniko would otherwise snapshot.
if git -C "$ctx" rev-parse --show-toplevel >/dev/null 2>&1; then
  git -C "$ctx" ls-files -z | tar --null -C "$ctx" -T - -cf - | tar -xf - -C "$src"
else
  cp -a "$ctx/." "$src"
fi
outdir="$work/out"; rm -rf "$outdir"; mkdir -p "$outdir"
binds=(-b "$src:/workspace" -b "$outdir:/out" -b /etc/resolv.conf)
ignore=(--ignore-path=/workspace --ignore-path=/out --ignore-path=/etc/resolv.conf)
# The kaniko image's own environment, on the PRoot command and nowhere
# else (its PATH has no grep, and exporting it once broke this script):
# PRoot passes the environment through and the image has no `env` to
# apply its config inside. PATH for the executor, and
# SSL_CERT_DIR for its bundled CA store, without which every registry
# answers "certificate signed by unknown authority" (the first laptop
# run; the fleet run had an explicit CA that hid it).
image_env=()
while IFS= read -r kv; do [ -n "$kv" ] && image_env+=("$kv"); done < <(
  python3 -c 'import json,sys; print("\n".join(json.load(open(sys.argv[1]))["config"].get("Env") or []))' "$kaniko/.image.json")
if [ -n "${EXTRA_CA:-}" ]; then
  binds+=(-b "$EXTRA_CA:/run/secrets/extra_ca" -b "$EXTRA_CA:/extra-ca.crt")
  ignore+=(--ignore-path=/run/secrets --ignore-path=/extra-ca.crt)
fi
proxies=()
for v in HTTPS_PROXY https_proxy HTTP_PROXY http_proxy NO_PROXY no_proxy; do
  [ -n "${!v:-}" ] && proxies+=(--build-arg "$v=${!v}")
done
# The platform arguments BuildKit defines for every build and kaniko does
# not, so a Dockerfile that reads TARGETARCH builds the same way under
# both.
case "$(uname -m)" in x86_64|amd64) tarch=amd64 ;; aarch64|arm64) tarch=arm64 ;; *) tarch=$(uname -m) ;; esac
proxies+=(--build-arg TARGETOS=linux --build-arg TARGETARCH="$tarch" --build-arg TARGETPLATFORM="linux/$tarch"
          --build-arg BUILDOS=linux --build-arg BUILDARCH="$tarch" --build-arg BUILDPLATFORM="linux/$tarch")
echo "kaniko under PRoot: $dockerfile"
env "${image_env[@]}" ${EXTRA_CA:+SSL_CERT_FILE=/extra-ca.crt} \
  "${PROOT:-$here/bin/proot}" -0 -r "$root" -b /proc -b /dev -b /sys "${binds[@]}" -w / \
  /kaniko/executor --context dir:///workspace --dockerfile "/workspace/$dockerfile" \
    --no-push --tar-path /out/image.tar --destination build:local \
    --snapshot-mode=redo --use-new-run "${ignore[@]}" "${proxies[@]}" \
    --image-download-retry=3 --image-fs-extract-retry=3 \
    --registry-map index.docker.io=registry-1.docker.io "$@" \
  2>&1 | grep --line-buffered -v 'Ignore list'
python3 "$here/pull-image.py" --archive "$outdir/image.tar" "$out"
echo "built $out"
