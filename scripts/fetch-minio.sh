#!/usr/bin/env bash
# Put the pinned MinIO server binary at .testkit/bin/minio.
#
# MinIO stopped publishing prebuilt binaries. `dl.min.io`, which the test
# harness and CI both used to curl, answers **410 Gone** for every
# platform of the pinned release, and the GitHub release carries no
# assets — the same withdrawal that took `minio/minio` and `minio/mc` off
# Docker Hub and moved `deploy/compose.yml` to quay.io, arriving at the
# other artifact a few weeks later. The container image is the only thing
# still published.
#
# **This pulls the image without a container runtime**, straight off a
# registry's HTTP API, and that is the whole reason the script exists
# rather than a `docker create` + `docker cp`: the jobs that need MinIO —
# correctness and chaos — must not depend on a docker daemon being there
# to fetch a test dependency, and neither must a runner machine, which is
# the one place a daemon is least welcome (a job that can talk to one can
# escape its container).
#
# Where from: STRATUM_MINIO_IMAGE, a repository reference without a tag
# (default ghcr.io/weftsh/minio, a byte-for-byte copy of
# quay.io/minio/minio, every platform, by digest). Any registry that
# speaks the distribution API with anonymous bearer tokens works — the
# token endpoint is read from the registry's own challenge, not assumed —
# so point it at quay.io/minio/minio, or at a mirror of your own, when
# the default is not reachable from where you build.
#
# The pin comes from `.minio-version`, which `crates/stratum-testkit`
# also `include_str!`s, so the harness and CI cannot run different
# MinIOs — a harness testing against a different store from the gate is
# a harness testing a different product.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
release="$(tr -d '[:space:]' < "$root/.minio-version")"
image="${STRATUM_MINIO_IMAGE:-ghcr.io/weftsh/minio}"
out="${1:-$root/.testkit/bin/minio}"

if [ -x "$out" ]; then
  echo "minio already at $out"
  exit 0
fi

# The image carries a Linux binary and nothing else does. There is no
# darwin build left to fetch anywhere, so say what actually works here
# instead of producing an ELF that cannot execute — which is how this
# last went wrong, and it presented as "minio never became ready".
case "$(uname -s)" in
  Linux) ;;
  *)
    cat >&2 <<MSG
MinIO publishes no $(uname -s) binary any more, and the container image
carries a Linux one. Run it and point the harness at it instead:

  docker run -d --name stratum-test-minio -p 9000:9000 \\
    -e MINIO_ROOT_USER=stratum-test -e MINIO_ROOT_PASSWORD=stratum-test-only \\
    $image:$release server /data
  export STRATUM_MINIO_URL=http://127.0.0.1:9000
MSG
    exit 1
    ;;
esac

case "$(uname -m)" in
  x86_64) arch=amd64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) echo "no minio image for $(uname -m)" >&2; exit 1 ;;
esac

# Not quay.io by default. Its anonymous tokens have come back granting no
# pull at all (`"access":[{"actions":[]}]`, then 401 on the manifest) from
# build machines behind an egress proxy, and a firewalled fleet died on it
# with a 403 the first day this ran for real. The mirror is on ghcr.io,
# which GitHub-hosted runners reach without an allow-list entry.
registry=${image%%/*}
repo=${image#*/}
api=https://$registry/v2/$repo
accept='application/vnd.docker.distribution.manifest.v2+json,application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.oci.image.index.v1+json'

# Anonymous pull. A registry answers an unauthenticated /v2/ with a
# `WWW-Authenticate: Bearer realm=…,service=…` challenge naming where to
# get a token; ghcr (ghcr.io/token) and quay (quay.io/v2/auth) name
# different places, so ask rather than hard-code one. The header is still
# required, so this is not an unauthenticated request.
challenge="$(curl -sS -o /dev/null -D - "https://$registry/v2/" | tr -d '\r' \
  | sed -n 's/^[Ww][Ww][Ww]-[Aa]uthenticate: *[Bb]earer *//p')"
realm="$(printf '%s' "$challenge" | sed -n 's/.*realm="\([^"]*\)".*/\1/p')"
service="$(printf '%s' "$challenge" | sed -n 's/.*service="\([^"]*\)".*/\1/p')"
[ -n "$realm" ] || { echo "$registry answered /v2/ with no bearer challenge; cannot fetch $image" >&2; exit 1; }
token="$(curl -fsSL "$realm?service=$service&scope=repository:$repo:pull" \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("token") or d["access_token"])')"
get() { curl -fsSL -H "Authorization: Bearer $token" -H "Accept: $accept" "$@"; }

# A multi-arch tag resolves to an index; pick this machine's entry.
index="$(get "$api/manifests/$release")" \
  || { echo "could not read $image:$release (a registry that grants anonymous tokens no pull answers 401 here); set STRATUM_MINIO_IMAGE to one that serves it" >&2; exit 1; }
digest="$(printf '%s' "$index" | python3 -c "
import json,sys
m = json.load(sys.stdin)
if 'manifests' not in m:
    print('')          # already a single-arch manifest
else:
    for e in m['manifests']:
        p = e.get('platform', {})
        if p.get('os') == 'linux' and p.get('architecture') == '$arch':
            print(e['digest']); break
    else:
        sys.exit('no linux/$arch in $release')
")"
[ -n "$digest" ] && manifest="$(get "$api/manifests/$digest")" || manifest="$index"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Newest layer first: the binary lives in one of the upper layers, and
# walking from the top means the first hit is the one the image would
# actually present at that path.
layers="$(printf '%s' "$manifest" | python3 -c \
  "import json,sys; print('\n'.join(l['digest'] for l in reversed(json.load(sys.stdin)['layers'])))")"

found=
for layer in $layers; do
  get -o "$tmp/layer.tgz" "$api/blobs/$layer"
  # Each layer is its own tar; the path is absent from most of them, and
  # a miss is expected rather than an error.
  if tar -xzf "$tmp/layer.tgz" -C "$tmp" usr/bin/minio 2>/dev/null; then
    found=yes
    break
  fi
done
[ -n "$found" ] || { echo "usr/bin/minio not found in any layer of $release" >&2; exit 1; }

mkdir -p "$(dirname "$out")"
install -m 0755 "$tmp/usr/bin/minio" "$out"
echo "minio $release -> $out"
