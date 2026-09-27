#!/usr/bin/env bash
# The ECS contract, run against a real cluster. A manual gate.
#
# What this proves that CI cannot
# ------------------------------
# Every automated test of the hosted runner launches against something we
# wrote: `FakeEcs` in `stratum-testkit`, or `deploy/fake-ecs/fake-ecs.py`
# under the manual stack. Both encode what we *believe* ECS does. A fake
# that is wrong about the provider produces a suite that is green and a
# product that is broken, in exactly the shape the fake got wrong and
# nowhere else — which is not a hypothetical here. It is the third time:
# MinIO answers 412 where real S3 answers 409 (`scripts/manual-s3.sh`),
# and our fake GitHub always attached `Retry-After` where the real one
# does not (`scripts/manual-ci.sh`). This is their sibling for the
# provider that runs tenant code.
#
# The thing at risk is `workflow/executor.rs`. Two of its decisions are
# guesses about AWS until something checks them against AWS:
#
#   * the CLASSIFICATION of a refusal. `LaunchError::Capacity` puts the
#     job back in the queue without counting an attempt — correct for a
#     full cluster, and an infinite silent retry loop for a permissions
#     mistake. `LaunchError::Refused` fails the job and shows an operator
#     the reason. ECS reports "no room" as a `failures` entry inside a
#     **200**, throttling as a 400 with a `__type`, and everything wrong
#     with the request as some other 4xx. Every one of those strings is
#     AWS's to change.
#   * that a `StopTask` actually stops a runner. The whole cancel path —
#     supersede, a person pressing cancel, an org suspended for mining —
#     ends in `StopTask`, and what it has to mean is: SIGTERM reaches the
#     runner, the runner exits 0 without reporting a verdict, and it does
#     so inside `stopTimeout` rather than being SIGKILLed at the end of
#     it. The runner's own half of that is pinned deterministically
#     (`binary_e2e::a_sigterm_kills_the_step_group_and_exits_quietly`).
#     ECS's half is not, and cannot be.
#
#   scripts/manual-ecs.sh launch          # RunTask the real task definition
#   scripts/manual-ecs.sh refusals        # the classifier, against real refusals
#   scripts/manual-ecs.sh taskdef         # the hardening, as registered — of
#                                         # both families when the GitHub one is set
#   scripts/manual-ecs.sh stop [<task-arn>]
#   scripts/manual-ecs.sh all
#
# What it costs to run
# --------------------
# `launch` and `stop` start real Fargate tasks that exit in seconds; a
# handful of task-seconds and one image pull. `refusals` starts nothing —
# every case is refused before a task exists. Nothing here writes to the
# control plane, and nothing here can touch a task it did not start
# except the one whose ARN you hand `stop`.
#
# What it cannot prove
# --------------------
#   * The capacity branches. `RESOURCE:MEMORY`, `AGENT`, a
#     `ThrottlingException` — those are a busy region and a spent API
#     budget, and provoking either on purpose is abuse of the API in the
#     same way `manual-ci.sh` declines to provoke a secondary rate limit.
#     They are printed as NOTEs, and the run does not claim them.
#   * That the egress allowlist DROPS what it should. `launch` proves the
#     control-plane domain is reachable from a task — the runner's exit
#     code says which side of that it landed on — but a task cannot be
#     made to dial a pool by this script: the container's entrypoint is
#     the runner and ECS overrides cannot replace an entrypoint. The
#     negative needs a real job with a `run:` step, which is the
#     walkthrough's `workflows /` stages against the deployment.
#   * Anything about a task role. There is none; `taskdef` checks that
#     absence, which is a different claim from "the credential it does
#     not have would not work".
#
# Least privilege, and why it is not optional
# -------------------------------------------
# Run `launch`, `stop` and `refusals` with the DISPATCH credential the
# app boots with — the access key `modules/runner` puts in Secrets
# Manager — and not an admin key. Half of what is being checked is that
# the policy pinning `ecs:RunTask` to one task definition on one cluster
# still admits the request the dispatcher actually sends; an admin key
# cannot fail that, and a check that cannot fail proves nothing. The
# policy is also why `refusals` never calls `ListTasks`: the dispatcher
# does not have it, so neither does this.
#
# `taskdef` needs `ecs:DescribeTaskDefinition`, which the dispatch policy
# deliberately does not grant. Give it a separate read-only credential in
# STRATUM_ECS_ADMIN_*; without one it prints a NOTE and claims nothing.
# The AccessDenied case needs a THIRD credential that genuinely lacks
# `ecs:RunTask` (STRATUM_ECS_DENIED_*) — the same argument `manual-ci.sh`
# makes for its denied installation.
set -euo pipefail

cd "$(dirname "$0")/.."

die() { printf '\033[31m%s\033[0m\n' "$*" >&2; exit 1; }
say() { printf '\033[1m%s\033[0m\n' "$*"; }
note() { printf '\033[33m   NOTE  %s\033[0m\n' "$*"; }
pass() { printf '\033[32m   PASS  %s\033[0m\n' "$*"; }
fail() { printf '\033[31m   FAIL  %s\033[0m\n' "$*"; FAILED=$((FAILED + 1)); }
FAILED=0

need() {
  local n=$1 hint=$2
  [ -n "${!n:-}" ] || die "$n is not set. $hint"
}

require_env() {
  need STRATUM_RUNNER_ECS_CLUSTER "The cluster the deployment dispatches to."
  need STRATUM_RUNNER_ECS_TASK_DEFINITION "The runner task definition family."
  need STRATUM_RUNNER_ECS_SUBNETS "Comma-separated private subnets, as the app has them."
  need STRATUM_RUNNER_ECS_SECURITY_GROUP "The runner security group."
  need STRATUM_RUNNER_AWS_ACCESS_KEY_ID "The DISPATCH key, not an admin key."
  need STRATUM_RUNNER_AWS_SECRET_ACCESS_KEY ""
  need STRATUM_RUNNER_URL "The control plane a launched runner calls back on."
  REGION=${STRATUM_RUNNER_AWS_REGION:-${AWS_REGION:-${AWS_DEFAULT_REGION:-us-east-1}}}
  ECS_URL=${STRATUM_RUNNER_ECS_URL:-https://ecs.${REGION}.amazonaws.com}
  ECS_URL=${ECS_URL%/}
  CONTAINER=${STRATUM_RUNNER_ECS_CONTAINER:-runner}
  # The GitHub Actions runner family is optional: a deployment that
  # offers no runners to GitHub has none, and `taskdef` says so with a
  # NOTE rather than a failure.
  GITHUB_CONTAINER=${STRATUM_RUNNER_ECS_GITHUB_CONTAINER:-runner}
}

# ---------------------------------------------------------------------
# One signed ECS call, and the classification our code would make of the
# answer. Both are python3 rather than the aws CLI on purpose: the CLI
# hides the status line and the `__type`, which are the two things being
# checked. python3 rather than bash+openssl because `openssl dgst -mac
# HMAC -macopt` is an OpenSSL-only spelling and macOS ships LibreSSL — a
# gate that runs on the CI image and not on the machine an operator runs
# it from is worse than none.
# ---------------------------------------------------------------------

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

cat > "$TMP/call.py" <<'PY'
import hashlib, hmac, os, sys, time, urllib.error, urllib.request
from urllib.parse import urlsplit

url, region, target, key_id, secret = sys.argv[1:6]
token = os.environ.get("STRATUM_ECS_SESSION_TOKEN", "")
body = sys.stdin.buffer.read()
now = time.gmtime()
amzdate = time.strftime("%Y%m%dT%H%M%SZ", now)
datestamp = time.strftime("%Y%m%d", now)
headers = {
    "content-type": "application/x-amz-json-1.1",
    "host": urlsplit(url).netloc,
    "x-amz-date": amzdate,
    "x-amz-target": "AmazonEC2ContainerServiceV20141113." + target,
}
if token:
    headers["x-amz-security-token"] = token
names = sorted(headers)
signed = ";".join(names)
canonical = "\n".join([
    "POST", "/", "",
    "".join("%s:%s\n" % (n, headers[n]) for n in names),
    signed,
    hashlib.sha256(body).hexdigest(),
])
scope = "%s/%s/ecs/aws4_request" % (datestamp, region)
to_sign = "\n".join([
    "AWS4-HMAC-SHA256", amzdate, scope,
    hashlib.sha256(canonical.encode()).hexdigest(),
])


def mac(key, msg):
    return hmac.new(key, msg.encode(), hashlib.sha256).digest()


k = mac(mac(mac(mac(("AWS4" + secret).encode(), datestamp), region), "ecs"), "aws4_request")
headers["authorization"] = "AWS4-HMAC-SHA256 Credential=%s/%s, SignedHeaders=%s, Signature=%s" % (
    key_id, scope, signed, hmac.new(k, to_sign.encode(), hashlib.sha256).hexdigest())

req = urllib.request.Request(url + "/", data=body, method="POST")
for name, value in headers.items():
    if name != "host":
        req.add_header(name, value)
try:
    with urllib.request.urlopen(req, timeout=20) as resp:
        status, out = resp.status, resp.read().decode()
except urllib.error.HTTPError as e:
    status, out = e.code, e.read().decode()
except Exception as e:  # a transport failure is status 0 to executor.rs
    status, out = 0, str(e)
print(status)
print("---BODY---")
print(out)
PY

# The classifier, transcribed from `workflow/executor.rs`. It is
# deliberately a second implementation: if it and the Rust drift apart,
# that is a finding this gate should surface rather than inherit.
cat > "$TMP/classify.py" <<'PY'
import json, sys

status = int(sys.argv[1])
body = sys.stdin.read()
try:
    doc = json.loads(body)
except ValueError:
    doc = {}

if status == 200:
    arn = (doc.get("tasks") or [{}])[0].get("taskArn")
    if arn:
        print("launched %s" % arn)
        raise SystemExit(0)
    failure = (doc.get("failures") or [{}])[0]
    reason = failure.get("reason") or ""
    capacity = (not reason) or reason.startswith(("RESOURCE:", "AGENT", "Capacity")) \
        or "InternalError" in reason or "reached the limit on the number of" in reason
    print("%s failures[0].reason=%r detail=%r" % (
        "capacity" if capacity else "refused", reason, failure.get("detail") or ""))
    raise SystemExit(0)

ty = doc.get("__type") or ""
capacity = status == 0 or status >= 500 or ty.endswith(
    ("ThrottlingException", "LimitExceededException", "ServerException"))
print("%s HTTP %d __type=%r message=%r" % (
    "capacity" if capacity else "refused", status, ty,
    doc.get("message") or doc.get("Message") or body[:200]))
PY

# ecs <target> <body> [<key-id> <secret>] -> sets STATUS and BODY
ecs() {
  local target=$1 body=$2
  local key=${3:-$STRATUM_RUNNER_AWS_ACCESS_KEY_ID}
  local sec=${4:-$STRATUM_RUNNER_AWS_SECRET_ACCESS_KEY}
  local out
  out=$(printf %s "$body" | python3 "$TMP/call.py" "$ECS_URL" "$REGION" "$target" "$key" "$sec")
  STATUS=${out%%$'\n'*}
  BODY=${out#*---BODY---$'\n'}
}

# What executor.rs would make of the last answer.
classified() { printf %s "$BODY" | python3 "$TMP/classify.py" "$STATUS"; }

# One field out of the last body, dotted. Same shape as manual-ci.sh's.
jf() {
  printf %s "$BODY" | python3 -c '
import json, sys
d = json.loads(sys.stdin.read() or "{}")
for k in sys.argv[1].split("."):
    if isinstance(d, list):
        d = d[int(k)] if len(d) > int(k) else None
    elif isinstance(d, dict):
        d = d.get(k)
    else:
        d = None
print("" if d is None else (json.dumps(d) if isinstance(d, (dict, list)) else d))' "$1"
}

# The dispatcher's RunTask body, field for field — `Ecs::run_task_body`.
# Overridable per case so a refusal can change exactly one field and
# leave the rest the shape production sends.
run_task_body() { # run_task_body <job-id> <cluster> <task-definition> <container> <subnet-json>
  python3 -c '
import json, sys
job, cluster, taskdef, container, subnets = sys.argv[1:6]
print(json.dumps({
    "cluster": cluster,
    "taskDefinition": taskdef,
    "launchType": "FARGATE",
    "count": 1,
    "networkConfiguration": {"awsvpcConfiguration": {
        "subnets": json.loads(subnets),
        "securityGroups": [sys.argv[6]],
        "assignPublicIp": "DISABLED",
    }},
    "overrides": {"containerOverrides": [{
        "name": container,
        "environment": [
            {"name": "STRATUM_JOB_ID", "value": job},
            {"name": "STRATUM_JOB_TOKEN", "value": "manual-ecs-not-a-real-token"},
            {"name": "STRATUM_RUNNER_URL", "value": sys.argv[7]},
        ],
    }]},
    "startedBy": "stratum:" + job,
}))' "$1" "$2" "$3" "$4" "$5" "$STRATUM_RUNNER_ECS_SECURITY_GROUP" "$STRATUM_RUNNER_URL"
}

subnets_json() {
  python3 -c '
import json, sys
print(json.dumps([s.strip() for s in sys.argv[1].split(",") if s.strip()]))' \
    "$STRATUM_RUNNER_ECS_SUBNETS"
}

job_id() { echo "manual-ecs-$(date +%s)-$$"; }

# Poll DescribeTasks until the task stops, printing each state it passes
# through. `stopped_at - started_at` is not what is measured: the wall
# clock from the call that mattered is, because that is what an operator
# waits and what `stopTimeout` bounds.
await_stopped() { # await_stopped <arn> <deadline-secs> -> BODY is the last DescribeTasks
  local arn=$1 limit=$2 last="" state=""
  local start=$SECONDS
  while [ $((SECONDS - start)) -lt "$limit" ]; do
    ecs DescribeTasks "$(printf '{"cluster":"%s","tasks":["%s"]}' \
      "$STRATUM_RUNNER_ECS_CLUSTER" "$arn")"
    [ "$STATUS" = 200 ] || { fail "DescribeTasks: HTTP $STATUS $BODY"; return 1; }
    state=$(jf tasks.0.lastStatus)
    if [ -z "$state" ]; then
      fail "DescribeTasks knows nothing about $arn: $(jf failures.0.reason)"
      return 1
    fi
    if [ "$state" != "$last" ]; then
      echo "   ${state} ($((SECONDS - start))s)"
      last=$state
    fi
    [ "$state" = STOPPED ] && return 0
    sleep 5
  done
  fail "the task was still $state after ${limit}s"
  return 1
}

# ---------------------------------------------------------------- cases

cmd_launch() {
  require_env
  say "── RunTask, the request the dispatcher sends ──"
  local job arn
  job=$(job_id)
  ecs RunTask "$(run_task_body "$job" "$STRATUM_RUNNER_ECS_CLUSTER" \
    "$STRATUM_RUNNER_ECS_TASK_DEFINITION" "$CONTAINER" "$(subnets_json)")"
  if [ "$STATUS" != 200 ]; then
    fail "the dispatch credential could not RunTask: $(classified)"
    echo "   this is the policy in modules/runner refusing the body in run_task_body —"
    echo "   compare the cluster condition and the task-definition ARN pattern."
    return
  fi
  arn=$(jf tasks.0.taskArn)
  if [ -z "$arn" ]; then
    fail "RunTask answered 200 with no task: $(classified)"
    return
  fi
  pass "RunTask accepted the production body: $arn"
  if [ "$(jf tasks.0.startedBy)" = "stratum:$job" ]; then
    pass "startedBy round-trips as stratum:<job id>"
  else
    fail "startedBy came back as '$(jf tasks.0.startedBy)'; the dispatcher's tag is how a task is tied to a job"
  fi

  echo "   waiting for it to run and exit (an image pull can be slow)"
  await_stopped "$arn" 420 || return
  local code stop_code reason
  code=$(jf tasks.0.containers.0.exitCode)
  stop_code=$(jf tasks.0.stopCode)
  reason=$(jf tasks.0.stoppedReason)
  echo "   stopCode=${stop_code} exitCode=${code:-none} stoppedReason=${reason}"
  case "$code" in
    2)
      pass "the runner reached ${STRATUM_RUNNER_URL} and was refused this made-up job (exit 2)"
      echo "   that exit code is the whole egress path: DNS, NAT, the firewall's"
      echo "   allowlist for the control-plane domain, and TLS."
      ;;
    0)
      pass "the runner reached the control plane, which answered 410 for a job it has never heard of (exit 0)"
      ;;
    1)
      fail "exit 1: the runner could NOT reach ${STRATUM_RUNNER_URL} from inside the runner VPC"
      echo "   that is the egress path, not the job: NAT, the Network Firewall"
      echo "   allowlist (is control_plane_domain in it?), or DNS. Every hosted job"
      echo "   on this deployment is failing the same way."
      ;;
    "")
      fail "the container never ran: ${reason}"
      echo "   an image pull through the ECR endpoints, an ENI in the private subnets,"
      echo "   or the execution role — none of which is the job's own doing."
      ;;
    *) fail "the runner exited ${code}, which is not one of the three endings main.rs documents" ;;
  esac
  if [ "$stop_code" = EssentialContainerExited ]; then
    pass "stopCode=EssentialContainerExited — the task ended because the runner did"
  else
    note "stopCode=${stop_code}, not EssentialContainerExited"
  fi
}

cmd_stop() {
  require_env
  local given=${1:-}
  say "── StopTask ──"

  # The reason bound. `Ecs::stop` truncates to 255 characters on the
  # belief that ECS refuses longer, and a supersede reason is
  # "superseded by <sha>" today but is a user-facing string that will
  # grow. If 255 were refused, every cancel on the fleet would fail.
  local job arn
  job=$(job_id)
  ecs RunTask "$(run_task_body "$job" "$STRATUM_RUNNER_ECS_CLUSTER" \
    "$STRATUM_RUNNER_ECS_TASK_DEFINITION" "$CONTAINER" "$(subnets_json)")"
  arn=$(jf tasks.0.taskArn)
  if [ -z "$arn" ]; then
    fail "could not start a task to stop: $(classified)"
  else
    echo "   ${arn}"
    local at_limit over_limit
    at_limit=$(python3 -c 'print("x" * 255)')
    over_limit=$(python3 -c 'print("x" * 256)')
    ecs StopTask "$(printf '{"cluster":"%s","task":"%s","reason":"%s"}' \
      "$STRATUM_RUNNER_ECS_CLUSTER" "$arn" "$over_limit")"
    if [ "$STATUS" = 200 ]; then
      note "a 256-character reason was ACCEPTED, so the truncation in Ecs::stop is
         belt-and-braces rather than a requirement. This gate found that once and
         the comment beside the truncation already records it; check that it still
         does before changing anything. Reported every run because it is the
         provider's to change back, not because the code is wrong."
    else
      pass "a 256-character reason is refused ($(classified)) — the truncation in Ecs::stop is load-bearing"
    fi
    ecs StopTask "$(printf '{"cluster":"%s","task":"%s","reason":"%s"}' \
      "$STRATUM_RUNNER_ECS_CLUSTER" "$arn" "$at_limit")"
    if [ "$STATUS" = 200 ]; then
      pass "a 255-character reason is accepted — the length Ecs::stop truncates TO is legal"
    else
      fail "a 255-character reason was refused ($(classified)). Ecs::stop truncates to
         exactly this, so every cancel, supersede and suspension on this fleet
         fails at StopTask and the compute keeps running."
    fi
  fi

  if [ -z "$given" ]; then
    note "the SIGTERM ending was NOT checked. It needs a task that is running a real
         job — a runner that is past its fetch and inside a step — and this script
         will not push to your deployment to make one. Trigger a hosted workflow
         with a step that sleeps, take the task ARN from the ECS console or from
         the dispatch log line, and re-run:

             scripts/manual-ecs.sh stop <task-arn>

         Until then this run does not claim that StopTask stops a runner; it
         claims only that the API accepts the call."
    return
  fi

  say "── the SIGTERM ending, against a task running a real job ──"
  local started code reason elapsed
  started=$SECONDS
  ecs StopTask "$(printf '{"cluster":"%s","task":"%s","reason":"%s"}' \
    "$STRATUM_RUNNER_ECS_CLUSTER" "$given" "manual-ecs: the SIGTERM contract")"
  if [ "$STATUS" != 200 ]; then
    fail "StopTask refused: $(classified)"
    return
  fi
  pass "StopTask accepted"
  await_stopped "$given" 300 || return
  elapsed=$((SECONDS - started))
  code=$(jf tasks.0.containers.0.exitCode)
  reason=$(jf tasks.0.stoppedReason)
  family=$(jf tasks.0.taskDefinitionArn)
  echo "   stopped after ${elapsed}s, exitCode=${code:-none}, stoppedReason=${reason}"
  # The two runner families end differently on purpose, and demanding one
  # ending of both is how this check spent a day reporting a bug that was
  # not there.
  #
  # Ours is `weft-runner`, whose signal handling is our own code: it kills
  # the step group, flushes the log, answers nothing, and exits 0.
  #
  # The GitHub family is `actions/runner`, which is not. With
  # RUNNER_MANUALLY_TRAP_SIG=1 its run.sh traps SIGTERM, cancels the job
  # and — measured on 2026-09-13 — logs `Runner listener exit with 0
  # return code`, then re-raises the signal on itself so the shell dies by
  # it. That is 128+15, and it is what a clean shutdown of that agent
  # looks like. Exit 0 is not reachable there, so asking for it fails a
  # runner that did everything right.
  case "$family" in
    *runner-github*) want=143; other=0 ;;
    *)               want=0;   other=143 ;;
  esac
  case "$code" in
    "$want")
      if [ "$want" = 0 ]; then
        pass "the runner caught SIGTERM and exited 0 without reporting a verdict"
        echo "   that is the ending signals.rs promises: the step group is killed, the"
        echo "   log is flushed, and a stopped task does not get to answer for itself."
      else
        pass "the agent caught SIGTERM and died by it after a clean shutdown (143)"
        echo "   Read the task's log stream to claim the shutdown itself — this exit"
        echo "   code cannot tell a clean cancel from a crash on the signal:"
        echo "     aws logs get-log-events --log-group-name /stratum/prod/runner \\"
        echo "       --log-stream-name github/runner/<task-id>"
        echo "   It must carry 'result: Canceled' and 'Runner listener exit with 0'."
      fi
      ;;
    137)
      fail "exit 137: the runner was SIGKILLed, so it did not act on SIGTERM inside
         stopTimeout. A superseded build's steps ran to the end of the window, and
         whatever the step spawned was orphaned rather than killed with its group."
      ;;
    "")
      fail "no exit code — the container did not exit on its own: ${reason}"
      ;;
    "$other")
      fail "exit ${code} from ${family##*/}. That is the *other* family's ending:
         either this ARN is not the family it looks like, or one of the two runners
         has changed how it handles SIGTERM. Both are worth knowing before a stop
         is trusted."
      ;;
    *)
      fail "the runner exited ${code}, which is neither family's ending (0 for ours,
         143 for the GitHub agent): a task an operator has to investigate for
         something we did on purpose."
      ;;
  esac
  if [ "$elapsed" -gt 90 ]; then
    note "it took ${elapsed}s. stopTimeout is 10s in modules/runner, so most of that
         was ECS's own state machine (draining the ENI, reporting) rather than the
         runner. Worth watching if it grows: the dispatcher does not wait on it."
  fi
  echo
  case "$family" in
    *runner-github*)
      note "the job will read **failure** on GitHub, and that is not this stop going
         wrong. Measured 2026-09-13: the agent reports \`result: Canceled\` and
         GitHub settles the job at that same instant — so the verdict is the
         runner's own report, not GitHub reaping a runner that vanished. GitHub
         records a runner-side cancel as a failure when nothing cancelled the run
         on its side, and there is no per-job cancel API to do that with:
         \`cancel_run\` takes the whole run and would kill sibling jobs. Until that
         changes, a deliberately stopped hosted job looks failed to its author." ;;
    *)
      echo "   Confirm on the deployment that the job carries NO verdict from this"
      echo "   runner — the run is settled by whoever asked for the stop, or by the"
      echo "   overdue sweep. A 'failed' verdict here would mean a stopped runner"
      echo "   reported anyway." ;;
  esac
}

cmd_refusals() {
  require_env
  say "── the refusals executor.rs classifies ──"
  local job subnets verdict
  job=$(job_id)
  subnets=$(subnets_json)

  refusal() { # refusal <label> <expected-class> <body>
    local label=$1 want=$2 body=$3
    ecs RunTask "$body"
    verdict=$(classified)
    case "$verdict" in
      launched*)
        fail "$label was ACCEPTED and started a task — the case is not what it says it is"
        echo "   ${verdict}  (stop it: scripts/manual-ecs.sh takes no cleanup command)"
        ;;
      "$want"*) pass "$label -> $verdict" ;;
      *)
        fail "$label was classified $verdict, wanted ${want}"
        if [ "$want" = refused ]; then
          echo "   as capacity, a job in this state is re-queued forever without ever"
          echo "   counting an attempt, and nobody is told. This is the branch that turns"
          echo "   an operator's mistake into a silent loop."
        fi
        ;;
    esac
  }

  refusal "a task definition that does not exist" refused \
    "$(run_task_body "$job" "$STRATUM_RUNNER_ECS_CLUSTER" \
      "${STRATUM_RUNNER_ECS_TASK_DEFINITION%%:*}-no-such-family:1" "$CONTAINER" "$subnets")"
  refusal "a cluster that does not exist" refused \
    "$(run_task_body "$job" "manual-ecs-no-such-cluster" \
      "$STRATUM_RUNNER_ECS_TASK_DEFINITION" "$CONTAINER" "$subnets")"
  refusal "an override for a container the task definition has not got" refused \
    "$(run_task_body "$job" "$STRATUM_RUNNER_ECS_CLUSTER" \
      "$STRATUM_RUNNER_ECS_TASK_DEFINITION" "not-the-runner-container" "$subnets")"
  refusal "a subnet that does not exist" refused \
    "$(run_task_body "$job" "$STRATUM_RUNNER_ECS_CLUSTER" \
      "$STRATUM_RUNNER_ECS_TASK_DEFINITION" "$CONTAINER" '["subnet-0000000000000ffff"]')"

  if [ -n "${STRATUM_ECS_DENIED_ACCESS_KEY_ID:-}" ]; then
    ecs RunTask "$(run_task_body "$job" "$STRATUM_RUNNER_ECS_CLUSTER" \
      "$STRATUM_RUNNER_ECS_TASK_DEFINITION" "$CONTAINER" "$subnets")" \
      "$STRATUM_ECS_DENIED_ACCESS_KEY_ID" "$STRATUM_ECS_DENIED_SECRET_ACCESS_KEY"
    verdict=$(classified)
    case "$verdict" in
      refused*) pass "a credential without ecs:RunTask -> $verdict" ;;
      launched*)
        fail "the 'denied' credential started a task. It holds ecs:RunTask, so it cannot
         fail this case, and a check that cannot fail proves nothing — the same
         argument manual-ci.sh makes about an installation with every permission."
        ;;
      *)
        fail "a credential without ecs:RunTask was classified $verdict, wanted refused.
         AccessDenied re-queued as capacity is the exact shape of the GitHub
         rate-limit bug: a condition that never clears, retried forever, with the
         operator told nothing."
        ;;
    esac
  else
    note "the AccessDenied case was NOT run. Set STRATUM_ECS_DENIED_ACCESS_KEY_ID and
         STRATUM_ECS_DENIED_SECRET_ACCESS_KEY to a credential that genuinely lacks
         ecs:RunTask on this cluster. It is the one refusal in the 'Refused' family
         that a real operator meets — a policy edit, an expired key — and the one
         whose misclassification is unrecoverable."
  fi

  echo
  note "the capacity family is NOT observed by this run, and cannot be on demand:
         RESOURCE:MEMORY / RESOURCE:CPU and AGENT arrive inside a 200 only when the
         region is genuinely short, and the account's vCPU quota (the sentence
         'You’ve reached the limit on the number of vCPUs…', also inside a 200)
         only when the account is; ThrottlingException and LimitExceededException
         need the API budget spent, which is abuse of it; ServerException and any
         5xx are AWS having a bad day; and status 0 is our own transport. Each is
         pinned hermetically in workflow/executor.rs's unit tests against the exact
         bodies AWS documents. If one of those strings changes, this gate will not
         tell you — a job silently failing instead of re-queueing will."
}

cmd_taskdef() {
  require_env
  if [ -z "${STRATUM_ECS_ADMIN_ACCESS_KEY_ID:-}" ]; then
    say "── the task definitions, as registered ──"
    note "not checked: DescribeTaskDefinition is not in the dispatch policy (correctly —
         the dispatcher never calls it). Set STRATUM_ECS_ADMIN_ACCESS_KEY_ID and
         STRATUM_ECS_ADMIN_SECRET_ACCESS_KEY to a read-only credential to check what
         terraform actually registered."
    return
  fi
  check_taskdef "$STRATUM_RUNNER_ECS_TASK_DEFINITION" "$CONTAINER" "the Weft runner"
  echo
  # The same hardening, on the second family. It is a different image —
  # the official GitHub agent — under the same execution role, and every
  # property below is one the isolation argument needs to hold for BOTH:
  # a task role on this one would be a credential inside somebody
  # else's `run:` step just the same.
  if [ -n "${STRATUM_RUNNER_ECS_GITHUB_TASK_DEFINITION:-}" ]; then
    check_taskdef "$STRATUM_RUNNER_ECS_GITHUB_TASK_DEFINITION" "$GITHUB_CONTAINER" "the GitHub Actions runner"
  else
    say "── the GitHub Actions runner task definition ──"
    note "STRATUM_RUNNER_ECS_GITHUB_TASK_DEFINITION is not set, so this deployment
         offers no runners to GitHub and there is no second definition to check.
         If it should, it is modules/runner's aws_ecs_task_definition.github_runner
         and the app reads its family from the same variable."
  fi
}

# One task definition's hardening, as ECS registered it: no task role,
# the container the dispatcher addresses, stopTimeout, a real init, and
# uid 10002.
check_taskdef() {
  local family=$1 container=$2 label=$3
  say "── the task definition for $label, as registered ──"
  ecs DescribeTaskDefinition \
    "$(printf '{"taskDefinition":"%s"}' "$family")" \
    "$STRATUM_ECS_ADMIN_ACCESS_KEY_ID" "$STRATUM_ECS_ADMIN_SECRET_ACCESS_KEY"
  if [ "$STATUS" != 200 ]; then
    fail "DescribeTaskDefinition $family: $(classified)"
    return
  fi
  local role cpu mem
  role=$(jf taskDefinition.taskRoleArn)
  cpu=$(jf taskDefinition.cpu)
  mem=$(jf taskDefinition.memory)
  if [ -z "$role" ]; then
    pass "no task role: a build that finds an SSRF finds no credential at 169.254.170.2"
  else
    fail "the task has a role: ${role}. Tenant code runs in this container; the
         module omits task_role_arn on purpose and something has put one back."
  fi
  # Not a PASS: it cannot fail, and a check that cannot fail proves nothing.
  # It is printed because the number IS the answer to "how much can a
  # hostile build spend", and an operator reading this run should see it.
  if [ "$family" = "$STRATUM_RUNNER_ECS_TASK_DEFINITION" ]; then
  echo "   the compute a job gets is cpu=${cpu} memory=${mem}MiB, and there is no other:"
  echo "   no autoscaling, no second container, one job per task. A fork bomb, or a"
  echo "   miner that got past every other layer, is bounded by those two numbers and"
  echo "   by the concurrency the control plane allows. Fargate offers no pidsLimit"
  echo "   and takes no nproc ulimit, so the process ceiling is the runner's own"
  echo "   RLIMIT_NPROC (steps::MAX_PROCS, STRATUM_RUNNER_MAX_PROCS) rather than"
  echo "   anything this task definition can say."
  else
  echo "   registered at cpu=${cpu} memory=${mem}MiB, which is only the smallest size:"
  echo "   the dispatcher sends a task-level cpu/memory override with every RunTask"
  echo "   for the size the job's labels asked for (1x, 2x, 4x), and that override is"
  echo "   the whole budget of that job."
  fi
  local body
  body=$(printf %s "$BODY" | python3 -c '
import json, sys
d = json.load(sys.stdin)["taskDefinition"]
c = d["containerDefinitions"][0]
print("name=%s user=%s stopTimeout=%s init=%s ulimits=%s" % (
    c.get("name"), c.get("user"), c.get("stopTimeout"),
    (c.get("linuxParameters") or {}).get("initProcessEnabled"),
    json.dumps(c.get("ulimits") or [])))')
  echo "   ${body}"
  case "$body" in
    *"name=${container} "*) pass "the container is named ${container}, which is what the RunTask overrides address" ;;
    *) fail "the container is not named ${container}; every override the dispatcher sends is addressed to a container that is not there" ;;
  esac
  case "$body" in
    *"stopTimeout=10 "*) pass "stopTimeout=10 — a cancelled job costs at most ten more seconds" ;;
    *"stopTimeout=None "*) fail "stopTimeout is unset, so ECS waits its implicit 30s: three times the spend on every superseded push" ;;
    *) note "stopTimeout is not 10; docs/deployment-aws.md says it is" ;;
  esac
  case "$body" in
    *"init=True"*) pass "initProcessEnabled — a step's orphaned grandchildren are reaped" ;;
    *) fail "initProcessEnabled is not set; a build that leaves grandchildren behind leaves zombies holding the task open past its verdict" ;;
  esac
  case "$body" in
    *"user=10002 "*) pass "the build runs as uid 10002, not root" ;;
    *) fail "the container's user is not 10002 — builds are running as root" ;;
  esac
  # The module says Fargate takes `nofile` and nothing else, and documents
  # cpu/memory as the containment for a fork bomb because of it. If that is
  # wrong, it is wrong HERE, in what AWS actually registered — and then the
  # module header and docs/deployment-aws.md are the things to fix.
  case "$body" in
    *nproc*)
      note "this task definition carries an nproc ulimit and Fargate registered it.
         modules/runner and docs/deployment-aws.md both say Fargate takes only
         nofile, which is why the process ceiling lives in the runner; that claim
         is wrong and a platform-enforced limit is available instead." ;;
  esac
}

# ---------------------------------------------------------------------

summary() {
  echo
  if [ "$FAILED" = 0 ]; then
    say "The ECS contract holds on what was checked."
    echo "Record the cluster, the task-definition revision, the platform version and"
    echo "the IAM user with this result. A pass under an admin key is not a pass under"
    echo "the dispatch credential, and a run without a task-ARN for \`stop\` has not"
    echo "checked that StopTask stops a runner."
  else
    die "$FAILED case(s) failed. Every failure above names the code that depends on the
semantic — that is where it surfaces in production."
  fi
}

case "${1:-}" in
  launch)   shift; cmd_launch;        summary ;;
  stop)     shift; cmd_stop "$@";     summary ;;
  refusals) shift; cmd_refusals;      summary ;;
  taskdef)  shift; cmd_taskdef;       summary ;;
  all)
    cmd_launch
    echo
    cmd_refusals
    echo
    cmd_taskdef
    echo
    cmd_stop
    note "\`taskdef\` checked the GitHub Actions runner definition only if
         STRATUM_RUNNER_ECS_GITHUB_TASK_DEFINITION was set; \`launch\`, \`refusals\`
         and \`stop\` exercise the Weft family, and the GitHub family's RunTask
         shape is proven by the deterministic executor tests and the stack's
         ECS stand-in, not here."
    note "\`all\` runs \`stop\` without a task ARN, so the SIGTERM ending is not claimed.
         Run \`scripts/manual-ecs.sh stop <arn>\` against a task that is running a
         real job to claim it."
    summary
    ;;
  *)
    cat >&2 <<'USAGE'
usage: scripts/manual-ecs.sh launch
       scripts/manual-ecs.sh refusals
       scripts/manual-ecs.sh taskdef
       scripts/manual-ecs.sh stop [<task-arn>]
       scripts/manual-ecs.sh all

The deployment's own dispatch environment, exactly as the app reads it:
  STRATUM_RUNNER_ECS_CLUSTER            the runner cluster
  STRATUM_RUNNER_ECS_TASK_DEFINITION    family[:revision]
  STRATUM_RUNNER_ECS_SUBNETS            comma-separated private subnets
  STRATUM_RUNNER_ECS_SECURITY_GROUP     the runner security group
  STRATUM_RUNNER_ECS_CONTAINER          default `runner`
  STRATUM_RUNNER_ECS_GITHUB_TASK_DEFINITION
                                        the GitHub Actions runner family, if the
                                        deployment has one (`taskdef` checks it too)
  STRATUM_RUNNER_ECS_GITHUB_CONTAINER   default `runner`
  STRATUM_RUNNER_AWS_ACCESS_KEY_ID      the DISPATCH key from Secrets Manager
  STRATUM_RUNNER_AWS_SECRET_ACCESS_KEY
  STRATUM_RUNNER_AWS_REGION             or AWS_REGION
  STRATUM_RUNNER_URL                    the control plane a runner calls back on

Two more credentials, each of which unlocks one case that cannot otherwise
be claimed:
  STRATUM_ECS_ADMIN_*      read-only, with ecs:DescribeTaskDefinition
  STRATUM_ECS_DENIED_*     a credential that genuinely lacks ecs:RunTask

Use the dispatch credential you actually deploy with. The AccessDenied case
cannot fail under a key that holds ecs:RunTask, and a check that cannot fail
proves nothing.
USAGE
    exit 2
    ;;
esac
