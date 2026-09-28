#!/usr/bin/env bash
# Run one `stratum-server admin …` command as a one-off ECS task on the
# AWS reference deployment (deploy/terraform) — same image, same secrets,
# same network as the service — and print its JSON line.
# `deploy/smoke-bootstrap-ecs.sh` is this with `bootstrap` in front.
#
# Reads cluster/subnets/SG/log group from the SSM parameters terraform
# wrote under /<project>/<env>/. STRATUM_PROJECT and STRATUM_ENV pick
# them (defaults: spool, prod — the terraform defaults).
# Requires: the aws CLI, with the bootstrap's <project>-cd role or any
# credential allowed ecs:RunTask/DescribeTasks, iam:PassRole on the
# task's roles, ssm:GetParameter and logs:GetLogEvents.
#
# Usage: admin-ecs.sh <subcommand> [flags…]
#   admin-ecs.sh bootstrap --org acme
#   admin-ecs.sh user-create --org acme --email you@example.com --password '…'
#   admin-ecs.sh user-disable --email person@example.com
set -euo pipefail

[ $# -ge 1 ] || { >&2 echo "usage: admin-ecs.sh <admin subcommand> [flags…]"; exit 2; }

PROJECT="${STRATUM_PROJECT:-spool}"
ENV_NAME="${STRATUM_ENV:-prod}"

param() { aws ssm get-parameter --name "/$PROJECT/$ENV_NAME/$1" --query Parameter.Value --output text; }

CLUSTER="$(param cluster)"
SUBNETS="$(param private-subnets)"
SG="$(param app-sg)"
LOG_GROUP="$(param log-group)"

TASK_DEF="$(aws ecs describe-services --cluster "$CLUSTER" --services "$(param service)" \
  --query 'services[0].taskDefinition' --output text)"

OVERRIDES="$(python3 - "$@" <<'PY'
import json, sys
print(json.dumps({"containerOverrides": [{
    "name": "app",
    "command": ["admin", *sys.argv[1:]],
}]}))
PY
)"

# RunTask answers 200 with an empty `tasks` and the refusal under
# `failures` — capacity, a task definition the cluster cannot place, a
# subnet with no route. Reading only `tasks[0].taskArn` turns every one
# of those into the word "None", and the waiter then fails on the
# length of the string "None" while the real reason never reaches the
# log. The common one is "You've reached the limit on the number of
# vCPUs you can run concurrently": run straight after a deploy, while the
# old tasks are still draining beside the new ones, a new account's
# Fargate vCPU quota has no room for one more. That clears by itself
# within minutes, so a capacity refusal is retried for a while; any other
# refusal is printed and fatal.
run_task() {
  aws ecs run-task \
    --cluster "$CLUSTER" \
    --task-definition "$TASK_DEF" \
    --launch-type FARGATE \
    --network-configuration "awsvpcConfiguration={subnets=[${SUBNETS}],securityGroups=[${SG}],assignPublicIp=DISABLED}" \
    --overrides "$OVERRIDES" \
    --output json
}
# Prints the task ARN; exits 2 on a capacity refusal, 1 on any other.
task_arn_of() {
  python3 - "$1" <<'PY'
import json, sys
run = json.loads(sys.argv[1])
tasks = run.get("tasks") or []
if tasks:
    print(tasks[0]["taskArn"])
    sys.exit(0)
failures = run.get("failures") or []
if not failures:
    print("RunTask answered neither a task nor a failure", file=sys.stderr)
    sys.exit(1)
capacity = ("vCPU", "RESOURCE:", "AGENT", "Throttl", "capacity")
code = 1
for f in failures:
    reason = f.get("reason") or ""
    print(f"RunTask refused: {reason} ({f.get('detail') or 'no detail'}) on {f.get('arn')}", file=sys.stderr)
    if any(k in reason for k in capacity):
        code = 2
sys.exit(code)
PY
}
CAPACITY_WAIT="${ADMIN_ECS_CAPACITY_WAIT_SECS:-600}"
deadline=$(( $(date +%s) + CAPACITY_WAIT ))
while :; do
  RUN="$(run_task)"
  set +e
  TASK_ARN="$(task_arn_of "$RUN")"
  rc=$?
  set -e
  [ "$rc" = 0 ] && break
  [ "$rc" = 2 ] || exit 1
  [ "$(date +%s)" -lt "$deadline" ] || { >&2 echo "no Fargate capacity after ${CAPACITY_WAIT}s"; exit 1; }
  >&2 echo "waiting 30s for capacity"
  sleep 30
done
>&2 echo "admin task: $TASK_ARN"

aws ecs wait tasks-stopped --cluster "$CLUSTER" --tasks "$TASK_ARN"

EXIT_CODE="$(aws ecs describe-tasks --cluster "$CLUSTER" --tasks "$TASK_ARN" \
  --query 'tasks[0].containers[0].exitCode' --output text)"
TASK_ID="${TASK_ARN##*/}"

# The container writes the JSON to stdout → CloudWatch. Emit the last
# JSON-looking line; a failure's sentence is on stderr there too.
aws logs get-log-events \
  --log-group-name "$LOG_GROUP" \
  --log-stream-name "app/app/$TASK_ID" \
  --start-from-head \
  --query 'events[].message' --output text | grep '^{' | tail -1 || true

[ "$EXIT_CODE" = "0" ] || { >&2 echo "admin task exited $EXIT_CODE"; exit 1; }
