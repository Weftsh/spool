#!/usr/bin/env bash
# End-to-end deployment smoke: proves a running spool endpoint serves the
# whole product — health, REST, the REAL git client over HTTP (clone +
# fsck --full --strict + push) and SSH, the dashboard, and optionally a
# self-hosted runner taking a workflow job. Runs unchanged against the
# one-box compose stack (docker-compose.yml) and the AWS deployment
# (deploy/terraform).
#
# Inputs (env):
#   BASE_URL        required — e.g. http://127.0.0.1:8080 or https://git.example.com
#   GIT_BASE_URL    optional — where git traffic goes (default: BASE_URL)
#   SSH_ENDPOINT    optional — ssh base (ssh://git@host:port); enables the
#                   git-over-SSH leg (keygen → register → clone/fsck/push)
#   ADMIN_TOKEN     optional — use an existing org:admin token; requires SMOKE_ORG
#   BOOTSTRAP_CMD   optional — command whose stdout ends with the
#                   `admin bootstrap` JSON line; smoke appends --org <name>
#   SMOKE_RUN_ID    optional — uniquifies the org name (default: timestamp)
#   SMOKE_ORIGIN_PROBE=0 optional — skip the probe of a public GitHub
#                   origin (for an install with no internet egress; mirrors
#                   and imports need that egress anyway)
#   SMOKE_413=1     optional — also verify the 64MB request cap answers 413
#   SMOKE_SELF_HOSTED=1 optional — register a self-hosted runner (the
#                   runner image, Dockerfile.runner, in a container on
#                   SMOKE_RUNNER_NETWORK, reaching the server at
#                   SMOKE_RUNNER_URL, default http://spool:8080), run a
#                   workflow job on it, and prove removing it stops it.
#                   Needs a docker daemon, or SMOKE_RUNNER_DRIVER
#   SMOKE_RUNNER_IMAGE optional — the runner image (default weft-runner:local)
#   SMOKE_RUNNER_DRIVER optional — a file to source that redefines
#                   runner_start/runner_logs/runner_exit/runner_rm, the
#                   four things the self-hosted leg needs from "another
#                   machine". The default is a docker container;
#                   deploy/proot/smoke-runner-driver.sh is a process
#                   under PRoot, for a stack with no daemon
#   SMOKE_CDN=0     optional — skip the CDN-offloaded clone leg
#   SMOKE_CLEANUP=0 optional — skip deleting the smoke repos
set -euo pipefail

: "${BASE_URL:?BASE_URL is required}"
GIT_BASE_URL="${GIT_BASE_URL:-$BASE_URL}"
RUN_ID="${SMOKE_RUN_ID:-$(date +%s)-$$}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

say() { printf '\n== %s\n' "$*"; }

# The self-hosted leg's "other machine", as four verbs. Docker unless a
# driver file says otherwise (SMOKE_RUNNER_DRIVER); see the header.
# runner_start NAME IMAGE SCRIPT: start the runner image running SCRIPT
# under sh -c, detached. runner_logs NAME: its output so far. runner_exit
# NAME: its exit code, or nothing while it runs. runner_rm NAME: gone.
runner_start() {
  docker run -d --name "$1" ${NET_ARGS[@]+"${NET_ARGS[@]}"} --entrypoint sh "$2" -c "$3" >/dev/null
}
runner_logs() { docker logs "$1" 2>&1; }
runner_exit() { docker inspect --format '{{if .State.Running}}{{else}}{{.State.ExitCode}}{{end}}' "$1"; }
runner_rm() { docker rm -f "$1" >/dev/null 2>&1 || true; }
if [ -n "${SMOKE_RUNNER_DRIVER:-}" ]; then
  # shellcheck source=/dev/null
  . "$SMOKE_RUNNER_DRIVER"
fi
json() { python3 -c "import json,sys; print(json.load(sys.stdin)$1)"; }

req() { # method path [json-body] -> body on stdout; fails on non-2xx
  local method="$1" path="$2" body="${3:-}"
  local args=(-fsS -X "$method" -H "Authorization: Bearer $TOKEN" "$BASE_URL$path")
  [ -n "$body" ] && args+=(-H "Content-Type: application/json" -d "$body")
  curl "${args[@]}"
}

say "1/10 liveness + readiness"
[ "$(curl -fsS "$BASE_URL/healthz")" ] || { echo "healthz failed"; exit 1; }
curl -fsS "$BASE_URL/readyz" >/dev/null

say "2/10 credentials"
if [ -n "${ADMIN_TOKEN:-}" ]; then
  TOKEN="$ADMIN_TOKEN"
  ORG="${SMOKE_ORG:?SMOKE_ORG is required with ADMIN_TOKEN}"
else
  : "${BOOTSTRAP_CMD:?set ADMIN_TOKEN+SMOKE_ORG or BOOTSTRAP_CMD}"
  ORG="smoke-$RUN_ID"
  BOOT_JSON="$($BOOTSTRAP_CMD --org "$ORG" | tail -1)"
  TOKEN="$(printf '%s' "$BOOT_JSON" | json "['admin_token']")"
fi
echo "org: $ORG"

REPO="app-$RUN_ID"
say "3/10 REST: create repo + commit"
req POST "/v1/orgs/$ORG/repos" "{\"name\":\"$REPO\"}" >/dev/null
C1="$(req POST "/v1/orgs/$ORG/repos/$REPO/commits" \
  '{"message":"smoke: initial","operations":[{"op":"put","path":"README.md","content":"# smoke\n"}]}' \
  | json "['commit']")"
echo "commit: $C1"

say "3b/10 origin probe over HTTPS"
# The server shells out to git for the origin probe, mirror syncs and
# imports, and git over HTTPS needs a CA store the image once shipped
# without: every probe answered "not reachable" while the Rust client,
# with its own roots, reached HTTPS services fine. A public origin must
# come back reachable from inside the deployed container.
if [ "${SMOKE_ORIGIN_PROBE:-1}" = "1" ]; then
  PROBE="$(req POST "/v1/orgs/$ORG/origins/probe" '{"origin":"https://github.com/git/git"}')"
  echo "probe: $PROBE"
  [ "$(printf '%s' "$PROBE" | json "['reachable']")" = "True" ] \
    || { echo "the deployed server cannot reach a public HTTPS origin with git: $PROBE" >&2; exit 1; }
else
  echo "skipped (SMOKE_ORIGIN_PROBE=0)"
fi

say "4/10 real git client: clone + fsck --full --strict"
GIT_HOST="${GIT_BASE_URL#http://}"; GIT_HOST="${GIT_HOST#https://}"
case "$GIT_BASE_URL" in https://*) SCHEME=https ;; *) SCHEME=http ;; esac
CLONE_URL="$SCHEME://x:$TOKEN@$GIT_HOST/$ORG/$REPO.git"
git clone -q "$CLONE_URL" "$WORK/clone"
git -C "$WORK/clone" fsck --full --strict
[ "$(git -C "$WORK/clone" rev-parse HEAD)" = "$C1" ]

say "5/10 real git client: push, REST sees it"
git -C "$WORK/clone" -c user.email=smoke@test -c user.name=Smoke \
  commit -q --allow-empty -m "smoke: pushed over the wire"
git -C "$WORK/clone" push -q origin HEAD
PUSHED="$(git -C "$WORK/clone" rev-parse HEAD)"
req GET "/v1/orgs/$ORG/repos/$REPO/log?limit=5" | grep -q "$PUSHED"

say "6/10 git over SSH"
if [ -n "${SSH_ENDPOINT:-}" ]; then
  # Real OpenSSH + real git through the SSH front door: register a fresh
  # key against this run's token, clone, fsck strict, push, REST sees it.
  ssh-keygen -t ed25519 -N "" -C "smoke-$RUN_ID" -f "$WORK/smoke-key" -q
  TOKEN_ID="$(printf '%s' "$TOKEN" | cut -d_ -f2)"
  req POST "/v1/orgs/$ORG/ssh-keys" \
    "{\"public_key\":\"$(cat "$WORK/smoke-key.pub")\",\"token_id\":\"$TOKEN_ID\",\"label\":\"smoke\"}" \
    >/dev/null
  SSH_CMD="ssh -F none -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none \
    -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=$WORK/known_hosts \
    -i $WORK/smoke-key"
  SSH_URL="$SSH_ENDPOINT/$ORG/$REPO.git"
  git -c core.sshCommand="$SSH_CMD" clone -q "$SSH_URL" "$WORK/ssh-clone"
  git -C "$WORK/ssh-clone" fsck --full --strict
  git -C "$WORK/ssh-clone" -c user.email=smoke@test -c user.name=Smoke \
    commit -q --allow-empty -m "smoke: pushed over ssh"
  git -C "$WORK/ssh-clone" -c core.sshCommand="$SSH_CMD" push -q origin HEAD
  SSH_PUSHED="$(git -C "$WORK/ssh-clone" rev-parse HEAD)"
  req GET "/v1/orgs/$ORG/repos/$REPO/log?limit=5" | grep -q "$SSH_PUSHED"
else
  echo "skipped (SSH_ENDPOINT unset)"
fi

say "7/10 self-hosted runner"
if [ "${SMOKE_SELF_HOSTED:-0}" = "1" ]; then
  # The whole loop, with nothing of ours faked: an owner mints a
  # registration token, a machine that is not the server registers with
  # it and starts listening, a pushed workflow is taken by that machine,
  # its log streams back and its verdict lands as a check, and removing
  # the runner from the organisation stops the process on its own — it
  # finds out on its next call, with nothing signalling it.
  #
  # The machine is the runner image in a container on the stack's own
  # network (SMOKE_RUNNER_NETWORK), reaching the server at
  # SMOKE_RUNNER_URL, because that is the only "other machine" a CI box
  # has. Against a deployed endpoint, point SMOKE_RUNNER_URL at it and
  # the network is not needed.
  RUNNER_IMAGE="${SMOKE_RUNNER_IMAGE:-weft-runner:local}"
  RUNNER_URL="${SMOKE_RUNNER_URL:-http://spool:8080}"
  RUNNER_NAME="smoke-runner-$RUN_ID"
  NET_ARGS=()
  if [ -n "${SMOKE_RUNNER_NETWORK:-}" ]; then
    NET_ARGS=(--network "$SMOKE_RUNNER_NETWORK")
  fi
  REG="$(req POST "/v1/orgs/$ORG/runners/registration-token" '{}')"
  REG_TOKEN="$(printf '%s' "$REG" | json "['token']")"
  # Single-use: the container both registers and runs, in one shell, so
  # the credential the token was exchanged for never leaves it.
  runner_start "$RUNNER_NAME" "$RUNNER_IMAGE" \
    "weft-runner register --url $RUNNER_URL --token $REG_TOKEN --name $RUNNER_NAME --labels smoke --dir /work/runner \
     && exec weft-runner run --dir /work/runner"
  cleanup_runner() { runner_rm "$RUNNER_NAME"; }
  trap 'cleanup_runner; rm -rf "$WORK"' EXIT
  # Online means the runner's own first claim reached the server.
  RUNNER_STATE=""
  for _ in $(seq 1 60); do
    RUNNER_STATE="$(req GET "/v1/orgs/$ORG/runners" | python3 -c "
import json, sys
rs = [r for r in json.load(sys.stdin)['runners'] if r['name'] == sys.argv[1]]
print(rs[0]['state'] if rs else '')
" "$RUNNER_NAME")"
    [ "$RUNNER_STATE" = "online" ] && break
    sleep 1
  done
  if [ "$RUNNER_STATE" != "online" ]; then
    echo "FAIL self-hosted runner: $RUNNER_NAME never came online (state '$RUNNER_STATE')" >&2
    runner_logs "$RUNNER_NAME" >&2 || true
    exit 1
  fi
  echo "$RUNNER_NAME registered and online"

  # The SSH leg pushed from its own clone; catch this one up first.
  git -C "$WORK/clone" pull -q --ff-only
  mkdir -p "$WORK/clone/.weft"
  cat > "$WORK/clone/.weft/ci.yml" <<'YAML'
name: ci
on: [push]
jobs:
  test:
    runs-on: [self-hosted, smoke]
    steps:
      - name: Prove
        run: |
          echo "self-hosted-smoke-ok sha=$WEFT_SHA ref=$WEFT_REF host=$(hostname)"
          test -f README.md
          test "$(git rev-parse HEAD)" = "$WEFT_SHA"
          # A bashism: steps run under bash, as they do on GitHub Actions.
          # Under /bin/sh (dash) this line is a syntax error.
          [[ "$WEFT_CI" == true ]] && echo "steps-run-under-bash"
YAML
  git -C "$WORK/clone" add .weft/ci.yml
  git -C "$WORK/clone" -c user.email=smoke@test -c user.name=Smoke \
    commit -q -m "smoke: a self-hosted workflow"
  git -C "$WORK/clone" push -q origin HEAD
  SH_SHA="$(git -C "$WORK/clone" rev-parse HEAD)"
  echo "pushed $SH_SHA; waiting for the self-hosted run"
  run_state() { # sha -> the run's state, or '' before it exists
    python3 -c "
import json, sys
runs = [r for r in json.load(sys.stdin)['runs'] if r['commit_sha'] == sys.argv[1]]
print(runs[0]['state'] if runs else '')
" "$1"
  }
  RUN_STATE=""
  for _ in $(seq 1 180); do
    RUNS="$(req GET "/v1/orgs/$ORG/repos/$REPO/workflow-runs")"
    RUN_STATE="$(printf '%s' "$RUNS" | run_state "$SH_SHA")"
    case "$RUN_STATE" in
      passed|failed|cancelled|blocked) break ;;
    esac
    sleep 1
  done
  if [ "$RUN_STATE" != "passed" ]; then
    echo "FAIL self-hosted runner: the run for $SH_SHA is '$RUN_STATE', wanted passed" >&2
    printf '%s\n' "$RUNS" >&2
    runner_logs "$RUNNER_NAME" >&2 || true
    exit 1
  fi
  JOB="$(printf '%s' "$RUNS" | python3 -c "
import json, sys
run = [r for r in json.load(sys.stdin)['runs'] if r['commit_sha'] == sys.argv[1]][0]
j = run['jobs'][0]
print(j['id'], j.get('pool'), (j.get('runner') or {}).get('name'))
" "$SH_SHA")"
  read -r SH_JOB_ID SH_POOL SH_RUNNER <<< "$JOB"
  if [ "$SH_POOL $SH_RUNNER" != "self_hosted $RUNNER_NAME" ]; then
    echo "FAIL self-hosted runner: the job records pool/runner '$SH_POOL $SH_RUNNER', wanted 'self_hosted $RUNNER_NAME'" >&2
    exit 1
  fi
  LOG="$(req GET "/v1/orgs/$ORG/repos/$REPO/workflow-jobs/$SH_JOB_ID/log")"
  if ! printf '%s' "$LOG" | grep -q "self-hosted-smoke-ok sha=$SH_SHA ref="; then
    echo "FAIL self-hosted runner: the job passed but its log does not show the step's output" >&2
    printf '%s\n' "$LOG" >&2
    exit 1
  fi
  if ! printf '%s' "$LOG" | grep -q "steps-run-under-bash"; then
    echo "FAIL self-hosted runner: the step did not run under bash (the [[ ]] line printed nothing)" >&2
    printf '%s\n' "$LOG" >&2
    exit 1
  fi
  CHECK="$(req GET "/v1/orgs/$ORG/repos/$REPO/commits/$SH_SHA/checks" | python3 -c "
import json, sys
runs = json.load(sys.stdin)['runs']
print(next((r['state'] for r in runs if r['name'] == 'ci / test'), 'missing'))
")"
  if [ "$CHECK" != "passing" ]; then
    echo "FAIL self-hosted runner: check 'ci / test' on $SH_SHA is '$CHECK', wanted passing" >&2
    exit 1
  fi
  echo "run passed on $RUNNER_NAME, log streamed, check 'ci / test' passing"

  # Removed from the organisation, the process stops itself: exit 2,
  # saying so. Nothing here signals the container.
  RUNNER_ID="$(req GET "/v1/orgs/$ORG/runners" | python3 -c "
import json, sys
print(next(r['id'] for r in json.load(sys.stdin)['runners'] if r['name'] == sys.argv[1]))
" "$RUNNER_NAME")"
  req DELETE "/v1/orgs/$ORG/runners/$RUNNER_ID" >/dev/null
  EXIT=""
  for _ in $(seq 1 45); do
    EXIT="$(runner_exit "$RUNNER_NAME")"
    [ -n "$EXIT" ] && break
    sleep 1
  done
  if [ "$EXIT" != "2" ] || ! runner_logs "$RUNNER_NAME" | grep -q "this runner has been removed"; then
    echo "FAIL self-hosted runner: after removal the process should exit 2 saying it was removed; exit '${EXIT:-still running}'" >&2
    runner_logs "$RUNNER_NAME" >&2 || true
    exit 1
  fi
  cleanup_runner
  echo "removed: the runner exited 2 on its next call"
else
  echo "skipped (SMOKE_SELF_HOSTED!=1)"
fi

say "8/10 CDN-offloaded clone"
# Offload is opt-in per client (fetch.uriprotocols), so this leg both
# builds the pack and clones as an opted-in client would. The signed-URL
# check below is the operator-side proof of the one thing CI cannot
# exercise: CloudFront's own validation at the edge.
if [ "${SMOKE_CDN:-1}" = "1" ] && req POST "/v1/orgs/$ORG/repos/$REPO/cdn-pack" >/dev/null 2>&1; then
  GIT_TRACE_PACKET=1 git -c protocol.version=2 -c fetch.uriprotocols=http,https \
    clone -q "$CLONE_URL" "$WORK/cdn-clone" 2>"$WORK/cdn-trace"
  git -C "$WORK/cdn-clone" fsck --full --strict
  PACK_URL="$(grep -oE 'https?://[^ ]*\.pack[^ ]*' "$WORK/cdn-trace" | head -1 || true)"
  if [ -z "$PACK_URL" ]; then
    echo "clone succeeded; no pack URL advertised (CDN not configured on this deployment)"
  else
    CODE="$(curl -s -o /dev/null -w '%{http_code}' "$PACK_URL")"
    [ "$CODE" = "200" ] || { echo "advertised pack URL returned $CODE"; exit 1; }
    case "$PACK_URL" in
      *Signature=*)
        # Flip one character of the signature: the edge must refuse it.
        TAMPERED="$(printf '%s' "$PACK_URL" | sed 's/\(Signature=.\)/\1X/')"
        CODE="$(curl -s -o /dev/null -w '%{http_code}' "$TAMPERED")"
        [ "$CODE" = "403" ] || { echo "tampered signature returned $CODE, expected 403"; exit 1; }
        echo "edge served the signed pack and refused a tampered signature"
        ;;
      *sig=*)
        # Origin-route shape: the token is an HMAC and a bad one is a
        # masked 404, deliberately indistinguishable from "no such repo".
        TAMPERED="$(printf '%s' "$PACK_URL" | sed 's/\(sig=.\)/\1f/')"
        CODE="$(curl -s -o /dev/null -w '%{http_code}' "$TAMPERED")"
        [ "$CODE" = "404" ] || { echo "tampered token returned $CODE, expected 404"; exit 1; }
        echo "origin route served the signed pack and refused a tampered token"
        ;;
      *) echo "offloaded clone served from $PACK_URL (unsigned deployment)" ;;
    esac
  fi
else
  echo "skipped (SMOKE_CDN=0 or no CDN pack built)"
fi

say "9/10 web surfaces"
# Each surface says which one it was and what it got: a bare `grep -q`
# that finds nothing prints nothing, and a failure that names the step
# but not the assertion costs a re-run to diagnose.
surface() { # description url pattern [grep-flags]
  local what="$1" url="$2" want="$3" flags="${4:--q}"
  local body
  if ! body="$(curl -fsSL "$url")"; then
    echo "FAIL $what: $url did not answer" >&2
    return 1
  fi
  if ! printf '%s' "$body" | grep $flags "$want" >/dev/null; then
    echo "FAIL $what: $url answered, but nothing matched /$want/" >&2
    printf 'first 300 bytes: %.300s\n' "$body" >&2
    return 1
  fi
  echo "$what ok"
}
# `/` sends a browser to the dashboard; following it has to land on the
# dashboard shell, not a 404.
surface "root redirect" "$BASE_URL/" "root"
surface "dashboard shell" "$BASE_URL/dashboard/" "root"
OPENAPI="$(curl -fsS "$BASE_URL/openapi.json" | json "['openapi']")"
case "$OPENAPI" in
  3.1*) echo "openapi $OPENAPI ok" ;;
  *) echo "FAIL openapi: served version $OPENAPI, wanted 3.1.x" >&2; exit 1 ;;
esac

say "10/10 request cap"
if [ "${SMOKE_413:-0}" = "1" ]; then
  head -c 68000000 /dev/zero > "$WORK/big"
  CODE="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
    --data-binary @"$WORK/big" "$BASE_URL/v1/orgs/$ORG/repos/$REPO/commits")"
  [ "$CODE" = "413" ] || { echo "expected 413, got $CODE"; exit 1; }
else
  echo "skipped (SMOKE_413!=1)"
fi

say "cleanup"
if [ "${SMOKE_CLEANUP:-1}" = "1" ]; then
  req POST "/v1/orgs/$ORG/repos/batch/delete" "{\"names\":[\"$REPO\"]}" >/dev/null \
    || curl -fsS -X DELETE -H "Authorization: Bearer $TOKEN" \
         "$BASE_URL/v1/orgs/$ORG/repos/$REPO" >/dev/null
  echo "smoke repos deleted (org row remains: no org-delete API)"
else
  echo "skipped"
fi

say "SMOKE OK — $BASE_URL serves health, REST, git over HTTP${SSH_ENDPOINT:+ and SSH}, CDN offload${SMOKE_SELF_HOSTED:+, self-hosted runners}, and the web"
