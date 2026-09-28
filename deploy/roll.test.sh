#!/usr/bin/env bash
# deploy/roll.sh against a fake `aws` on PATH: what it registers, what it
# refuses, and what it says when the service does not end up where it was
# sent.
#
#   - a bare tag: checked against the stack's repository, swapped into the
#     family's LATEST revision (not the one running), and rolled
#   - no argument: the running image, onto the latest configuration
#   - a tag the repository does not have: refused before anything is
#     registered
#   - a rollout the circuit breaker undid: a failure, named
#
# Run: bash deploy/roll.test.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
export FAKE_DIR="$tmp"
mkdir -p "$tmp/bin"
cat > "$tmp/bin/aws" <<'EOF'
#!/usr/bin/env bash
# Only the calls roll.sh makes; anything else is a test failure.
echo "$*" >> "$FAKE_DIR/calls"
arg() { # the value after flag $1
  local want=$1; shift
  while [ $# -gt 0 ]; do [ "$1" = "$want" ] && { echo "$2"; return; }; shift; done
}
case "$1 $2" in
  "ssm get-parameter")
    case "$(arg --name "$@")" in
      */cluster) echo spool-prod ;;
      */service) echo spool-prod ;;
      */ecr-repository) echo 111111111111.dkr.ecr.us-east-1.amazonaws.com/spool-prod-app ;;
      *) echo "unexpected parameter $*" >&2; exit 99 ;;
    esac ;;
  "ecs describe-services")
    # Where the service is: rev 3 until an update moves it, unless the
    # test says the breaker rolled it back.
    if [ -f "$FAKE_DIR/updated" ] && [ ! -f "$FAKE_DIR/breaker" ]; then cat "$FAKE_DIR/updated"
    else echo "arn:aws:ecs:us-east-1:1:task-definition/spool-prod:3"; fi ;;
  "ecs describe-task-definition")
    td="$(arg --task-definition "$@")"
    case "$(arg --query "$@")" in
      taskDefinition.family) echo spool-prod ;;
      "taskDefinition.containerDefinitions[?name=='app'].image | [0]")
        [ "$td" = "arn:aws:ecs:us-east-1:1:task-definition/spool-prod:3" ] || { echo "image asked of $td" >&2; exit 98; }
        echo "111111111111.dkr.ecr.us-east-1.amazonaws.com/spool-prod-app:running" ;;
      taskDefinition)
        # The family's latest is rev 4: terraform registered it after the
        # last roll, with a new setting and its own (stale) image tag.
        [ "$td" = "spool-prod" ] || { echo "revision read from $td, wanted the family" >&2; exit 97; }
        cat <<'JSON'
{"taskDefinitionArn":"arn:aws:ecs:us-east-1:1:task-definition/spool-prod:4","family":"spool-prod","revision":4,"status":"ACTIVE",
 "registeredAt":"2026-09-28T00:00:00Z","requiresAttributes":[{"name":"x"}],"compatibilities":["EC2","FARGATE"],
 "taskRoleArn":"arn:aws:iam::1:role/spool-prod-task","executionRoleArn":"arn:aws:iam::1:role/spool-prod-execution",
 "networkMode":"awsvpc","requiresCompatibilities":["FARGATE"],"cpu":"2048","memory":"8192","volumes":[],
 "ephemeralStorage":{"sizeInGiB":100},
 "containerDefinitions":[{"name":"app","image":"111111111111.dkr.ecr.us-east-1.amazonaws.com/spool-prod-app:bootstrap",
   "environment":[{"name":"STRATUM_GC_SECS","value":"3600"}]}]}
JSON
        ;;
      *) echo "unexpected query $*" >&2; exit 96 ;;
    esac ;;
  "ecr batch-get-image")
    [ "$(arg --image-ids "$@")" = "imageTag=v2" ] && echo "sha256:abc" || echo "None" ;;
  "ecs register-task-definition")
    cp "$(arg --cli-input-json "$@" | sed 's|^file://||')" "$FAKE_DIR/registered.json"
    echo "arn:aws:ecs:us-east-1:1:task-definition/spool-prod:5" ;;
  "ecs update-service") arg --task-definition "$@" > "$FAKE_DIR/updated"; echo '{}' ;;
  "ecs wait") ;;
  *) echo "unexpected aws call: $*" >&2; exit 99 ;;
esac
EOF
chmod +x "$tmp/bin/aws"
export PATH="$tmp/bin:$PATH"

fail() { echo "FAIL: $*" >&2; exit 1; }
reset() { rm -f "$tmp/calls" "$tmp/updated" "$tmp/breaker" "$tmp/registered.json"; }
registered() { python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print($1)" "$tmp/registered.json"; }

echo "== a bare tag"
reset
out="$(bash "$here/roll.sh" v2 2>"$tmp/err")" || fail "roll.sh v2 failed: $(cat "$tmp/err")"
[ "$out" = "arn:aws:ecs:us-east-1:1:task-definition/spool-prod:5" ] || fail "expected the new revision's arn, got: $out"
[ "$(registered "d['containerDefinitions'][0]['image']")" = "111111111111.dkr.ecr.us-east-1.amazonaws.com/spool-prod-app:v2" ] \
  || fail "the registered image is not the tag asked for"
[ "$(registered "d['containerDefinitions'][0]['environment'][0]['name']")" = "STRATUM_GC_SECS" ] \
  || fail "the revision was not built from the family's latest (rev 4)"
[ "$(registered "sorted(k for k in d if k in ('revision','status','taskDefinitionArn','registeredAt','requiresAttributes','compatibilities'))")" = "[]" ] \
  || fail "read-only fields were sent to RegisterTaskDefinition"
[ "$(cat "$tmp/updated")" = "arn:aws:ecs:us-east-1:1:task-definition/spool-prod:5" ] || fail "the service was not pointed at the new revision"
echo "ok"

echo "== no argument: the running image, on the latest configuration"
reset
bash "$here/roll.sh" >/dev/null 2>"$tmp/err" || fail "roll.sh with no argument failed: $(cat "$tmp/err")"
[ "$(registered "d['containerDefinitions'][0]['image']")" = "111111111111.dkr.ecr.us-east-1.amazonaws.com/spool-prod-app:running" ] \
  || fail "the running image was not kept"
[ "$(registered "d['containerDefinitions'][0]['environment'][0]['value']")" = "3600" ] \
  || fail "the latest configuration was not used"
echo "ok"

echo "== a tag the repository does not have"
reset
if bash "$here/roll.sh" nope >/dev/null 2>"$tmp/err"; then fail "a missing tag did not fail"; fi
grep -q "no image tagged nope" "$tmp/err" || fail "the missing tag was not named: $(cat "$tmp/err")"
! grep -q "register-task-definition" "$tmp/calls" || fail "a revision was registered for a missing image"
echo "ok"

echo "== the circuit breaker undid the rollout"
reset
touch "$tmp/breaker"
if bash "$here/roll.sh" v2 >/dev/null 2>"$tmp/err"; then fail "a rolled-back deploy reported success"; fi
grep -q "circuit breaker rolled back" "$tmp/err" || fail "the rollback was not named: $(cat "$tmp/err")"
echo "ok"
echo "roll.test.sh: all passed"
