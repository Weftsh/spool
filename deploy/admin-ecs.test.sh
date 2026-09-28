#!/usr/bin/env bash
# deploy/admin-ecs.sh against a fake `aws` on PATH: the three answers
# RunTask gives, and what the script has to do with each.
#
#   - a capacity refusal, then a task: the script waits and retries,
#     then follows the task to its log line
#   - a refusal that is not capacity: printed, fatal, no retry
#   - an empty answer with no failure: fatal, named
#
# The fake records every RunTask it saw in $FAKE_LOG so the test can
# count retries. Run: bash deploy/admin-ecs.test.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
export FAKE_LOG="$tmp/calls" FAKE_SCRIPT="$tmp/runtask.py"
mkdir -p "$tmp/bin"
cat > "$tmp/bin/aws" <<'EOF'
#!/usr/bin/env bash
# Only the calls admin-ecs.sh makes; anything else is a test failure.
case "$1 $2" in
  "ssm get-parameter") echo "fake-$4" ;;                       # cluster/subnets/sg/log-group/service
  "ecs describe-services") echo "arn:aws:ecs:us-east-1:1:task-definition/fake:1" ;;
  "ecs run-task") echo "run-task" >> "$FAKE_LOG"; python3 "$FAKE_SCRIPT" "$(wc -l < "$FAKE_LOG")" ;;
  "ecs wait") echo "wait" >> "$FAKE_LOG" ;;
  "ecs describe-tasks") echo 0 ;;
  "logs get-log-events") printf 'starting\n{"org":"smoke","token":"t"}\n' ;;
  *) echo "unexpected aws call: $*" >&2; exit 99 ;;
esac
EOF
chmod +x "$tmp/bin/aws"
export PATH="$tmp/bin:$PATH"

fail() { echo "FAIL: $*" >&2; exit 1; }
run() { : > "$FAKE_LOG"; ADMIN_ECS_CAPACITY_WAIT_SECS=5 bash "$here/admin-ecs.sh" bootstrap --org smoke; }

echo "== capacity refusal, then a task"
cat > "$FAKE_SCRIPT" <<'EOF'
import sys, json
n = int(sys.argv[1])
if n == 1:
    print(json.dumps({"tasks": [], "failures": [{"arn": "arn:x", "reason": "You’ve reached the limit on the number of vCPUs you can run concurrently", "detail": None}]}))
else:
    print(json.dumps({"tasks": [{"taskArn": "arn:aws:ecs:us-east-1:1:task/fake/0123456789abcdef0123456789abcdef"}], "failures": []}))
EOF
out="$(run 2>"$tmp/err")" || fail "the script failed after capacity cleared: $(cat "$tmp/err")"
[ "$out" = '{"org":"smoke","token":"t"}' ] || fail "expected the task's JSON line, got: $out"
[ "$(grep -c run-task "$FAKE_LOG")" = 2 ] || fail "expected one retry, saw $(grep -c run-task "$FAKE_LOG") RunTask calls"
grep -q "RunTask refused: You" "$tmp/err" || fail "the refusal was not printed"
grep -q "waiting 30s for capacity" "$tmp/err" || fail "the wait was not announced"
echo "ok"

echo "== a refusal that is not capacity"
cat > "$FAKE_SCRIPT" <<'EOF'
import json
print(json.dumps({"tasks": [], "failures": [{"arn": "arn:x", "reason": "MISSING_SUBNET", "detail": "no route"}]}))
EOF
if run >/dev/null 2>"$tmp/err"; then fail "a non-capacity refusal did not fail the script"; fi
[ "$(grep -c run-task "$FAKE_LOG")" = 1 ] || fail "a non-capacity refusal was retried"
grep -q "RunTask refused: MISSING_SUBNET (no route)" "$tmp/err" || fail "the reason was not printed: $(cat "$tmp/err")"
echo "ok"

echo "== neither a task nor a failure"
cat > "$FAKE_SCRIPT" <<'EOF'
import json
print(json.dumps({"tasks": [], "failures": []}))
EOF
if run >/dev/null 2>"$tmp/err"; then fail "an empty answer did not fail the script"; fi
grep -q "neither a task nor a failure" "$tmp/err" || fail "the empty answer was not named: $(cat "$tmp/err")"
echo "ok"

echo "== capacity that never clears"
cat > "$FAKE_SCRIPT" <<'EOF'
import json
print(json.dumps({"tasks": [], "failures": [{"arn": "arn:x", "reason": "RESOURCE:MEMORY", "detail": None}]}))
EOF
if run >/dev/null 2>"$tmp/err"; then fail "a capacity refusal that never clears did not fail the script"; fi
grep -q "no Fargate capacity after 5s" "$tmp/err" || fail "the deadline was not named: $(cat "$tmp/err")"
echo "ok"
echo "admin-ecs.test.sh: all passed"
