#!/usr/bin/env bash
# The store contract, run against a real S3 bucket. A manual gate.
#
# I9 — the manifest is the only ref truth and it changes only by
# compare-and-swap — is not a property of our code. It is a property of
# the object store. Everything above it assumes that `If-Match` with a
# stale etag is refused, that `If-None-Match: *` is refused on a key that
# exists, that a losing conditional write is reported as a *conflict* and
# not as some opaque error, and that a missing key reports 404 rather than
# 403. If any of that is false on the backend we deploy on, the CAS is not
# a CAS and the invariant is decoration.
#
# Those semantics had been verified against exactly one implementation —
# MinIO, in tests — while a deployment runs stateless nodes against
# whatever S3-compatible store it chose. reference/formats.md called that
# the top open item.
#
# It was right to. Real S3 answers 409 ConditionalRequestConflict when two
# conditional writes to one key overlap, where MinIO only ever answers
# 412; the store client mapped 409 to a generic error, so the loser of a
# manifest CAS fell out of the retry loop and failed a push that should
# have re-run. That bug was invisible to every MinIO test we had, and it
# lived on exactly the path the multi-node design exists to support.
#
# CI runs the same cases against MinIO on every run
# (crates/stratum-testkit/tests/store_contract.rs). This script is the
# other half, and it is manual because credentials do not belong in CI —
# the same shape as the manual browser pass. scripts/ci-local.sh names it
# as a SKIP rather than counting it as a pass.
#
#   scripts/manual-s3.sh check     # run the contract once
#   scripts/manual-s3.sh check --both-addressing-styles
#   scripts/manual-s3.sh clean     # remove a prefix left behind by a failed run
#
# Two things decide whether the result means anything:
#
#   * Run it against BOTH addressing styles. ObjectStore::new derives its
#     SigV4 path prefix from the URL's shape, so path-style and
#     virtual-host-style exercise different code and can disagree.
#     --both-addressing-styles does the pair for you.
#   * Run it with the DEPLOYMENT'S IAM policy, not an admin key. The
#     404-not-403 case exists to catch a least-privilege policy turning
#     absence into AccessDenied, and an admin key can never fail it.
#
# Not on AWS? Set STRATUM_S3_ENDPOINT to the store's origin (e.g.
# https://s3.example.com) and the bucket is addressed under it —
# path-style as <endpoint>/<bucket>, virtual-host-style as
# <bucket>.<endpoint host>. A pass there says nothing about AWS, and a
# pass on AWS says nothing about it: the point of this gate is that each
# backend answers conditional writes its own way.
set -euo pipefail

cd "$(dirname "$0")/.."

die() { printf '\033[31m%s\033[0m\n' "$*" >&2; exit 1; }
say() { printf '\033[1m%s\033[0m\n' "$*"; }

require_env() {
  [ -n "${STRATUM_S3_BUCKET:-}" ] || die \
    "STRATUM_S3_BUCKET is not set. This writes to a real bucket; it will not guess one.
Set STRATUM_S3_BUCKET, AWS_REGION, and the AWS_* credentials for the role you deploy with."
  [ -n "${AWS_REGION:-}" ] || die "AWS_REGION is not set."
  [ -n "${AWS_ACCESS_KEY_ID:-}" ] || die \
    "AWS_ACCESS_KEY_ID is not set. Unsigned requests fail every case for the wrong reason."
  [ -n "${AWS_SECRET_ACCESS_KEY:-}" ] || die "AWS_SECRET_ACCESS_KEY is not set."
}

# A prefix unique to this run, so a bucket that is also serving something
# else is never disturbed and two runs cannot collide.
prefix() { echo "store-contract/$(date +%s)-$$"; }

# AWS by default; any S3-compatible origin when STRATUM_S3_ENDPOINT says so.
endpoint() { echo "${STRATUM_S3_ENDPOINT:-https://s3.${AWS_REGION}.amazonaws.com}" | sed 's#/*$##'; }
path_style_url() { echo "$(endpoint)/${STRATUM_S3_BUCKET}"; }
vhost_style_url() {
  local e scheme host
  e=$(endpoint); scheme=${e%%://*}; host=${e#*://}
  echo "${scheme}://${STRATUM_S3_BUCKET}.${host}"
}

run_one() {
  local url="$1" style="$2" pfx
  pfx="$(prefix)"
  say "── ${style} ──"
  echo "   ${url}"
  echo "   prefix ${pfx}"
  echo
  if STRATUM_STORE_URL="$url" \
     cargo run -q --release -p stratum-testkit --bin store-contract -- --prefix "$pfx"; then
    return 0
  fi
  # A failed run may have left keys behind; the cases clean up as they go,
  # but a case that failed mid-way may not have reached its cleanup.
  echo
  echo "   keys may remain under ${pfx} — scripts/manual-s3.sh clean ${pfx}"
  return 1
}

cmd_check() {
  require_env
  local both=0
  [ "${1:-}" = "--both-addressing-styles" ] && both=1

  local failed=0
  run_one "$(path_style_url)" "path-style" || failed=1

  if [ "$both" = 1 ]; then
    echo
    run_one "$(vhost_style_url)" "virtual-host-style" || failed=1
  else
    echo
    printf '\033[33m   NOTE  only path-style was checked. ObjectStore derives its\033[0m\n'
    printf '\033[33m         signing path from the URL shape, so this is not a pass for\033[0m\n'
    printf '\033[33m         virtual-host-style. Re-run with --both-addressing-styles.\033[0m\n'
  fi

  echo
  if [ "$failed" = 0 ]; then
    say "The store contract holds."
    echo "Record the endpoint, the addressing style, and the IAM role with this"
    echo "result. A pass under an admin key is not a pass under the deploy role."
  else
    die "The store contract does NOT hold on this backend. Every failure names the
code that depends on the semantic — that is where it surfaces in production."
  fi
}

cmd_clean() {
  require_env
  local pfx="${1:-}"
  [ -n "$pfx" ] || die "usage: scripts/manual-s3.sh clean <prefix>"
  command -v aws >/dev/null || die "the aws CLI is needed to sweep a prefix"
  say "removing s3://${STRATUM_S3_BUCKET}/${pfx}"
  aws s3 rm "s3://${STRATUM_S3_BUCKET}/${pfx}" --recursive \
    ${STRATUM_S3_ENDPOINT:+--endpoint-url "$STRATUM_S3_ENDPOINT"}
}

case "${1:-}" in
  check) shift; cmd_check "$@" ;;
  clean) shift; cmd_clean "$@" ;;
  *)
    cat >&2 <<'USAGE'
usage: scripts/manual-s3.sh check [--both-addressing-styles]
       scripts/manual-s3.sh clean <prefix>

Environment: STRATUM_S3_BUCKET, AWS_REGION, AWS_ACCESS_KEY_ID,
AWS_SECRET_ACCESS_KEY (and AWS_SESSION_TOKEN if the role needs one);
STRATUM_S3_ENDPOINT for an S3-compatible store that is not AWS.

Use the credentials you actually deploy with, not an admin key.
USAGE
    exit 2
    ;;
esac
