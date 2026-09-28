#!/usr/bin/env bash
# Roll the AWS reference deployment (deploy/terraform) to a server image:
# register a revision of the service's task definition with that image,
# point the service at it, and wait until the rollout is steady. ECS
# replaces tasks a few at a time; the database migrates itself when the
# first new task starts (docs/operations.md, "Upgrades and migrations"),
# and a revision whose tasks never become healthy is rolled back by the
# service's circuit breaker.
#
#   deploy/roll.sh 1f2e3d4        # a tag in the stack's ECR repository
#   deploy/roll.sh <registry>/<repository>:<tag>   # any image reference
#   deploy/roll.sh                # the image already running, re-rolled
#
# The new revision is the task-definition family's LATEST revision with
# the image swapped in — so settings terraform changed since the last
# roll (an apply registers a revision but, by design, does not move the
# service onto it) go out with it. With no argument that is all it does:
# `scripts/tf.sh <env> apply`, then `deploy/roll.sh`, rolls a
# configuration change.
#
# Reads cluster, service and repository from the SSM parameters terraform
# wrote under /<project>/<env>/; STRATUM_PROJECT and STRATUM_ENV pick them
# (defaults: spool, prod). Needs what the bootstrap's <project>-cd role
# grants: ssm:GetParameter, ecr:BatchGetImage, ecs:Describe*/
# RegisterTaskDefinition/UpdateService, iam:PassRole on the task's roles.
set -euo pipefail

[ $# -le 1 ] || { >&2 echo "usage: roll.sh [image-tag | image-reference]"; exit 2; }

PROJECT="${STRATUM_PROJECT:-spool}"
ENV_NAME="${STRATUM_ENV:-prod}"
param() { aws ssm get-parameter --name "/$PROJECT/$ENV_NAME/$1" --query Parameter.Value --output text; }

CLUSTER="$(param cluster)"
SERVICE="$(param service)"
REPO_URL="$(param ecr-repository)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

CURRENT="$(aws ecs describe-services --cluster "$CLUSTER" --services "$SERVICE" \
  --query 'services[0].taskDefinition' --output text)"
FAMILY="$(aws ecs describe-task-definition --task-definition "$CURRENT" \
  --query 'taskDefinition.family' --output text)"

case "${1:-}" in
  "")
    IMAGE="$(aws ecs describe-task-definition --task-definition "$CURRENT" \
      --query "taskDefinition.containerDefinitions[?name=='app'].image | [0]" --output text)" ;;
  */*) IMAGE="$1" ;;
  *)
    # A bare tag: it has to be in the stack's own repository, and saying
    # so now beats a rollout that waits ten minutes on an image pull.
    if ! aws ecr batch-get-image --repository-name "${REPO_URL#*/}" --image-ids "imageTag=$1" \
        --query 'images[0].imageId.imageDigest' --output text 2>/dev/null | grep -q '^sha256:'; then
      >&2 echo "no image tagged $1 in $REPO_URL — push it first (docs/deployment-aws.md, \"Deploying a new version\")"
      exit 1
    fi
    IMAGE="$REPO_URL:$1" ;;
esac

aws ecs describe-task-definition --task-definition "$FAMILY" --query taskDefinition --output json > "$tmp/latest.json"
python3 - "$tmp/latest.json" "$IMAGE" > "$tmp/next.json" <<'PY'
import json, sys
td = json.load(open(sys.argv[1]))
found = False
for c in td["containerDefinitions"]:
    if c["name"] == "app":
        c["image"] = sys.argv[2]
        found = True
if not found:
    sys.exit("the task definition has no container named `app`")
# What RegisterTaskDefinition accepts; the rest of a described revision
# (arn, revision, status, registeredAt, …) is read-only.
keep = ("family", "taskRoleArn", "executionRoleArn", "networkMode", "containerDefinitions",
        "volumes", "placementConstraints", "requiresCompatibilities", "cpu", "memory",
        "ephemeralStorage", "runtimePlatform", "proxyConfiguration", "pidMode", "ipcMode")
print(json.dumps({k: td[k] for k in keep if td.get(k) not in (None, [], {})}))
PY

NEXT="$(aws ecs register-task-definition --cli-input-json "file://$tmp/next.json" \
  --query 'taskDefinition.taskDefinitionArn' --output text)"
>&2 echo "registered $NEXT ($IMAGE)"
aws ecs update-service --cluster "$CLUSTER" --service "$SERVICE" --task-definition "$NEXT" >/dev/null
>&2 echo "rolling $SERVICE; waiting for it to settle"
aws ecs wait services-stable --cluster "$CLUSTER" --services "$SERVICE"
RUNNING="$(aws ecs describe-services --cluster "$CLUSTER" --services "$SERVICE" \
  --query 'services[0].taskDefinition' --output text)"
if [ "$RUNNING" != "$NEXT" ]; then
  >&2 echo "the service settled on $RUNNING, not $NEXT: the new revision's tasks did not become healthy and the circuit breaker rolled back. The app log group (/$PROJECT/$ENV_NAME/app) says why."
  exit 1
fi
echo "$NEXT"
