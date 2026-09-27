#!/usr/bin/env bash
# BOOTSTRAP_CMD for the deployed smoke: runs `stratum-server admin
# bootstrap --org <name>` as a one-off ECS task on the live cluster and
# prints its JSON line — exactly what deploy/smoke.sh expects on stdout.
# Every other operator command goes through deploy/admin-ecs.sh the same
# way; this is that script with `bootstrap` in front.
#
# Usage: smoke-bootstrap-ecs.sh --org NAME [--plan P]
set -euo pipefail
exec "$(dirname "$0")/admin-ecs.sh" bootstrap "$@"
