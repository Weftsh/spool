#!/usr/bin/env python3
"""A stand-in for ECS `RunTask`/`StopTask` that starts the real runner
image with the local docker daemon.

This exists so the prod-parity stack can run a workflow end to end through
the code path production uses — the app signs a RunTask against an ECS
endpoint, a task boots the `weft-runner` image, the runner clones
over HTTP with its job token and reports back — without an AWS account.
It is the only component of that stack that is not what we deploy, so it
is kept small and honest about what it verifies:

  * the request is SigV4-signed by the dispatch key, over the `ecs`
    service, and the signature is recomputed and compared — a signer that
    drifts from AWS's canonical request fails here rather than in prod;
  * the RunTask asks for Fargate with no public IP, one of the task
    definitions this stack was told to expect, and overrides only the
    runner container — with the environment that definition's image
    reads, and for the GitHub Actions one (FAKE_ECS_GITHUB_TASK_DEFINITION)
    a task-level cpu/memory and NO registration credential;
  * the task is started with the container hardening the real task
    definition carries (an unprivileged uid, no capabilities, no privilege
    escalation, a pid cap) and NOTHING mounted — in particular not the
    docker socket this process itself holds.

It does not pretend to be more of ECS than that. No DescribeTasks, no
task lifecycle events, no capacity limits (the testkit's fake covers those
refusals hermetically). A container that exits is simply gone; StopTask on
a task that already finished answers as AWS does, with an error.

With FAKE_ECS_BACKEND=proot there is no daemon at all: the two images are
root filesystems on disk and a task is a process under PRoot, which is
how deploy/proot runs this same stack on the Fargate fleet. The RunTask
checks, the DescribeTasks window and the StopTask clock are the same.

Never deploy this. It holds the docker socket, which is the exact thing
the runner image is built without, and the reason the real fleet lives in
a VPC of its own.
"""

import hashlib
import hmac
import json
import os
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ACCESS_KEY = os.environ["FAKE_ECS_ACCESS_KEY_ID"]
SECRET_KEY = os.environ["FAKE_ECS_SECRET_ACCESS_KEY"]
REGION = os.environ.get("FAKE_ECS_REGION", "us-east-1")
CLUSTER = os.environ.get("FAKE_ECS_CLUSTER", "local")
TASK_DEFINITION = os.environ["FAKE_ECS_TASK_DEFINITION"]
RUNNER_IMAGE = os.environ["FAKE_ECS_RUNNER_IMAGE"]
# The second definition: a GitHub Actions runner (Dockerfile.github-runner).
# Optional — a stack that only runs Weft workflows sets neither. What is
# checked for it is the LAUNCH SHAPE and nothing more: the real agent
# cannot finish a job without GitHub on the other end, so a container
# this starts collects its registration from the app and then fails to
# reach github.com, which is the honest outcome here.
GITHUB_TASK_DEFINITION = os.environ.get("FAKE_ECS_GITHUB_TASK_DEFINITION")
GITHUB_RUNNER_IMAGE = os.environ.get("FAKE_ECS_GITHUB_RUNNER_IMAGE")
if bool(GITHUB_TASK_DEFINITION) != bool(GITHUB_RUNNER_IMAGE):
    raise SystemExit(
        "fake-ecs: FAKE_ECS_GITHUB_TASK_DEFINITION and FAKE_ECS_GITHUB_RUNNER_IMAGE go together"
    )
BIND_PORT = int(os.environ.get("FAKE_ECS_PORT", "8443"))
# uid 10002 is `runner` in Dockerfile.runner; the task definition pins it
# too, so a build never runs as root even if the image's USER changes.
RUNNER_UID = "10002"
# How a task becomes compute. `docker` is the compose stack: the image
# runs as a container on the daemon this process holds. `proot` is
# deploy/proot's stack, where there is no daemon: the image is a root
# filesystem already unpacked on disk (FAKE_ECS_RUNNER_IMAGE names the
# directory) and a task is a process under PRoot, started by
# deploy/proot/run.sh. Same RunTask, same shape checks, same
# StopTask clock; only the last step differs.
BACKEND = os.environ.get("FAKE_ECS_BACKEND", "docker")
if BACKEND not in ("docker", "proot"):
    raise SystemExit(f"fake-ecs: FAKE_ECS_BACKEND must be docker or proot, not {BACKEND!r}")
PROOT_RUN = os.environ.get("FAKE_ECS_PROOT_RUN", "")
PROOT_WORK = os.environ.get("FAKE_ECS_PROOT_WORK", "")
PROOT_HOSTS = os.environ.get("FAKE_ECS_PROOT_HOSTS", "")
if BACKEND == "proot" and not (PROOT_RUN and PROOT_WORK):
    raise SystemExit("fake-ecs: the proot backend needs FAKE_ECS_PROOT_RUN (run.sh) and FAKE_ECS_PROOT_WORK")


def log(msg):
    print(f"fake-ecs: {msg}", file=sys.stderr, flush=True)


def docker(*args, check=True):
    return subprocess.run(["docker", *args], capture_output=True, text=True, check=check)


def network():
    """The compose network this container is on, so the runners we start
    can resolve `app` the way a real task resolves the control plane."""
    forced = os.environ.get("FAKE_ECS_NETWORK")
    if forced:
        return forced
    out = docker(
        "inspect",
        "--format",
        "{{range $k, $v := .NetworkSettings.Networks}}{{$k}}\n{{end}}",
        os.environ["HOSTNAME"],
    ).stdout.split()
    if not out:
        raise SystemExit("fake-ecs: cannot tell which docker network I am on")
    return out[0]


NETWORK = network() if BACKEND == "docker" else "host"
log(f"starting runners from {RUNNER_IMAGE} on network {NETWORK} ({BACKEND})")
if GITHUB_TASK_DEFINITION:
    log(f"and GitHub Actions runners ({GITHUB_TASK_DEFINITION}) from {GITHUB_RUNNER_IMAGE}")


def sha256(b):
    return hashlib.sha256(b).hexdigest()


def derive_key(date, service):
    k = ("AWS4" + SECRET_KEY).encode()
    for part in (date, REGION, service, "aws4_request"):
        k = hmac.new(k, part.encode(), hashlib.sha256).digest()
    return k


def verify(handler, body):
    """Recompute the SigV4 signature of this request. Returns an error
    string, or None when it verifies."""
    auth = handler.headers.get("Authorization", "")
    if not auth.startswith("AWS4-HMAC-SHA256 "):
        return "no SigV4 Authorization header"
    parts = {}
    for piece in auth[len("AWS4-HMAC-SHA256 "):].split(","):
        k, _, v = piece.strip().partition("=")
        parts[k] = v
    cred = parts.get("Credential", "").split("/")
    if len(cred) != 5:
        return "malformed Credential"
    key_id, date, region, service, terminal = cred
    if key_id != ACCESS_KEY:
        return f"unknown access key {key_id}"
    if service != "ecs" or region != REGION or terminal != "aws4_request":
        return f"credential scope {'/'.join(cred[1:])} is not for ecs in {REGION}"
    signed = parts.get("SignedHeaders", "").split(";")
    amz_date = handler.headers.get("x-amz-date", "")
    if not amz_date.startswith(date):
        return "x-amz-date does not match the credential date"
    payload = handler.headers.get("x-amz-content-sha256", "")
    if payload != sha256(body):
        return "x-amz-content-sha256 does not match the body"
    canonical_headers = "".join(
        f"{h}:{' '.join(handler.headers.get(h, '').split())}\n" for h in signed
    )
    canonical = "\n".join(
        [handler.command, handler.path, "", canonical_headers, ";".join(signed), payload]
    )
    scope = f"{date}/{REGION}/ecs/aws4_request"
    string_to_sign = "\n".join(
        ["AWS4-HMAC-SHA256", amz_date, scope, sha256(canonical.encode())]
    )
    expect = hmac.new(derive_key(date, "ecs"), string_to_sign.encode(), hashlib.sha256).hexdigest()
    if not hmac.compare_digest(expect, parts.get("Signature", "")):
        return "signature mismatch"
    return None


def task_arn(task_id):
    return f"arn:aws:ecs:{REGION}:000000000000:task/{CLUSTER}/{task_id}"


def weft_shape(_req, env):
    """The Weft runner's override: a job id, its token, and where to
    report. Returns a problem, or None."""
    for k in ("STRATUM_JOB_ID", "STRATUM_JOB_TOKEN", "STRATUM_RUNNER_URL"):
        if k not in env:
            return f"override lacks {k}"
    return None


def github_shape(req, env):
    """The GitHub Actions runner's override, as `run_task_body_github` in
    crates/stratum-server/src/workflow/executor.rs sends it.

    Three things are asserted, and each is a property the design rests
    on rather than a formality. The five names are what
    deploy/github-runner/entrypoint.sh reads, so a rename on either side
    is caught here rather than as a container that exits 2 with "not
    set". STRATUM_JOB_TOKEN and RUNNER_JITCONFIG must be ABSENT: the
    registration credential is collected by the container with a
    one-shot token and never travels in a RunTask, where a stopped
    task's description would keep it. And the task-level cpu/memory
    override must be there, as strings, because that is how a job's
    size label becomes compute — a definition-level default is only the
    smallest size, and a launch without the override silently runs
    every job at it.
    """
    for k in ("WEFT_JOB_ID", "WEFT_JIT_TOKEN", "WEFT_RUNNER_URL", "RUNNER_NAME", "WEFT_RUNNER_SIZE"):
        if k not in env:
            return f"override lacks {k}"
    for k in ("STRATUM_JOB_TOKEN", "RUNNER_JITCONFIG"):
        if k in env:
            return f"override carries {k}: a GitHub runner is registered by the container, not by the launch"
    overrides = req.get("overrides", {})
    for k in ("cpu", "memory"):
        v = overrides.get(k)
        if not isinstance(v, str) or not v.isdigit():
            return f"overrides.{k} must be a string of digits (the job's size), got {v!r}"
    return None


def run_task(req):
    """Start one runner container, or say why not in ECS's own shape."""
    if req.get("cluster") != CLUSTER:
        return 400, {"__type": "ClusterNotFoundException", "message": "Cluster not found."}
    definition = req.get("taskDefinition")
    if definition == TASK_DEFINITION:
        image = RUNNER_IMAGE
        shape = weft_shape
    elif GITHUB_TASK_DEFINITION and definition == GITHUB_TASK_DEFINITION:
        image = GITHUB_RUNNER_IMAGE
        shape = github_shape
    else:
        return 400, {"__type": "ClientException", "message": "TaskDefinition not found."}
    if req.get("launchType") != "FARGATE":
        return 400, {"__type": "InvalidParameterException", "message": "launchType must be FARGATE here."}
    net = req.get("networkConfiguration", {}).get("awsvpcConfiguration", {})
    if net.get("assignPublicIp") != "DISABLED":
        return 400, {
            "__type": "InvalidParameterException",
            "message": "a runner task with a public IP would have an unfiltered path out",
        }
    overrides = req.get("overrides", {}).get("containerOverrides", [])
    if len(overrides) != 1 or overrides[0].get("name") != "runner":
        return 400, {"__type": "InvalidParameterException", "message": "override the runner container only."}
    env = {e["name"]: e["value"] for e in overrides[0].get("environment", [])}
    problem = shape(req, env)
    if problem:
        return 400, {"__type": "InvalidParameterException", "message": problem}
    job_id = env.get("STRATUM_JOB_ID") or env.get("WEFT_JOB_ID")
    task_id = uuid.uuid4().hex
    started_by = req.get("startedBy", "")
    if BACKEND == "proot":
        err = proot_start(task_id, image, env, started_by)
    else:
        err = docker_start(task_id, image, env, started_by)
    if err:
        log(f"RunTask for {job_id}: launch failed: {err}")
        return 200, {"tasks": [], "failures": [{"arn": task_arn(task_id), "reason": "AGENT", "detail": err}]}
    log(f"RunTask for {job_id}: task {task_id}")
    return 200, {"tasks": [{"taskArn": task_arn(task_id), "lastStatus": "PROVISIONING"}], "failures": []}


def docker_start(task_id, image, env, started_by):
    """The compose stack's launch: a container on the daemon. Returns a
    problem, or None."""
    # The token goes in through a file the daemon reads, not argv: a
    # `ps` on this container must not show a repository credential.
    with tempfile.NamedTemporaryFile("w", delete=False) as f:
        for k, v in env.items():
            f.write(f"{k}={v}\n")
        env_file = f.name
    try:
        os.chmod(env_file, 0o600)
        res = docker(
            # Not `--rm`: a task that died is the one whose stderr you
            # want, and ECS keeps a stopped task around for the same
            # reason. `docker ps -a --filter label=stratum.fake-ecs`
            # finds them; `compose down` does not remove them.
            "run", "-d",
            # `--init`, because the task definition sets
            # `initProcessEnabled = true`. On Fargate that puts tini at
            # PID 1, and tini is what forwards SIGTERM to the runner when
            # ECS stops a task. Without it here the runner is PID 1
            # itself, where the kernel drops any signal the process has
            # no handler for — so a StopTask fired on time was ignored,
            # the container ran every remaining step to completion, and
            # only the 10s SIGKILL ended it. A superseded job that keeps
            # burning a container is the opposite of what cancelling is
            # for, and the stand-in must not be the reason it looks
            # fixed.
            "--init",
            "--name", f"weft-runner-{task_id}",
            "--label", "stratum.fake-ecs=1",
            "--label", f"stratum.started_by={started_by}",
            "--network", NETWORK,
            "--user", RUNNER_UID,
            "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges",
            "--pids-limit", "1024",
            "--env-file", env_file,
            image,
            check=False,
        )
    finally:
        os.unlink(env_file)
    if res.returncode != 0:
        return f"docker run failed: {res.stderr.strip()}"
    return None


# The proot backend's tasks: task_id -> (Popen, started_by, work dir).
# A finished task stays here the way a stopped container stays on the
# daemon: its stderr is in the work dir and DescribeTasks still answers.
_PROOT_TASKS = {}
_PROOT_LOCK = threading.Lock()


def proot_start(task_id, rootfs, env, started_by):
    """deploy/proot's launch: the runner's root filesystem under PRoot, as
    uid 10002, with a scratch /work of its own and the stack's hosts file
    so `app` resolves. The environment goes in through a file run.sh
    reads and deletes, never argv, for the reason the docker branch
    gives. The process is its own session so StopTask can signal the
    runner and everything a step spawned together, as SIGTERM to a
    task's init would."""
    if not os.path.isdir(os.path.join(rootfs, "usr")):
        return f"{rootfs} is not an unpacked image root filesystem"
    work = os.path.join(PROOT_WORK, f"task-{task_id}")
    os.makedirs(os.path.join(work, "work"), mode=0o700)
    env_file = os.path.join(work, "env")
    with open(env_file, "w") as f:
        for k, v in env.items():
            f.write(f"{k}={v}\n")
    os.chmod(env_file, 0o600)
    cmd = [PROOT_RUN, rootfs, "-u", f"{RUNNER_UID}:{RUNNER_UID}", "-b", f"{work}/work:/work", "--env-file", env_file]
    if PROOT_HOSTS:
        cmd += ["-b", f"{PROOT_HOSTS}:/etc/hosts"]
    try:
        with open(os.path.join(work, "stdout"), "wb") as out, open(os.path.join(work, "stderr"), "wb") as err:
            proc = subprocess.Popen(cmd, stdout=out, stderr=err, start_new_session=True)
    except OSError as e:
        return f"proot launch failed: {e}"
    with _PROOT_LOCK:
        _PROOT_TASKS[task_id] = (proc, started_by, work)
    return None


def proot_state(proc):
    return "running" if proc.poll() is None else "exited"


def proot_stop(task_id):
    """SIGTERM now, SIGKILL ten seconds later if it is still there: the
    task definition's stopTimeout, same clock as the docker branch."""
    with _PROOT_LOCK:
        entry = _PROOT_TASKS.get(task_id)
    if entry is None:
        return False
    proc = entry[0]
    if proc.poll() is not None:
        return True
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        return True

    def reap():
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                return
            time.sleep(0.2)
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass

    threading.Thread(target=reap, daemon=True).start()
    return True


# docker's own words for a container's life, in ECS's.
_LAST_STATUS = {
    "created": "PENDING",
    "running": "RUNNING",
    "restarting": "RUNNING",
    "paused": "RUNNING",
    "removing": "STOPPED",
    "exited": "STOPPED",
    "dead": "STOPPED",
}


def describe(task_ids=None):
    """Every task this stand-in started, in DescribeTasks' shape.

    `task_ids` narrows it; `None` is all of them, which is what the
    read-only GET below serves. The states are read from docker at the
    moment of asking rather than remembered, because the interesting
    question — did StopTask actually stop the compute? — is exactly the
    one a remembered status would answer wrongly.
    """
    rows = []
    if BACKEND == "proot":
        with _PROOT_LOCK:
            rows = [(tid, proot_state(proc), by) for tid, (proc, by, _) in _PROOT_TASKS.items()]
    else:
        res = docker(
            "ps", "-a",
            "--filter", "label=stratum.fake-ecs",
            "--format", "{{.Names}}\t{{.State}}\t{{.Label \"stratum.started_by\"}}",
            check=False,
        )
        for line in res.stdout.splitlines():
            parts = line.split("\t")
            if len(parts) != 3 or not parts[0].startswith("weft-runner-"):
                continue
            rows.append((parts[0][len("weft-runner-"):], parts[1], parts[2]))
    out = []
    for task_id, state, started_by in rows:
        if task_ids is not None and task_id not in task_ids:
            continue
        out.append(
            {
                "taskArn": task_arn(task_id),
                "lastStatus": _LAST_STATUS.get(state, "STOPPED"),
                "desiredStatus": "RUNNING" if state == "running" else "STOPPED",
                "startedBy": started_by,
                "containers": [{"name": "runner", "lastStatus": _LAST_STATUS.get(state, "STOPPED")}],
            }
        )
    return out


def describe_tasks(req):
    if req.get("cluster") != CLUSTER:
        return 400, {"__type": "ClusterNotFoundException", "message": "Cluster not found."}
    want = [arn.rsplit("/", 1)[-1] for arn in req.get("tasks", [])]
    found = describe(set(want))
    seen = {t["taskArn"].rsplit("/", 1)[-1] for t in found}
    failures = [
        {"arn": task_arn(t), "reason": "MISSING"} for t in want if t not in seen
    ]
    return 200, {"tasks": found, "failures": failures}


def stop_task(req):
    arn = req.get("task", "")
    task_id = arn.rsplit("/", 1)[-1]
    if req.get("cluster") != CLUSTER or not task_id:
        return 400, {"__type": "InvalidParameterException", "message": "The referenced task was not found."}
    # -t 10 is `stopTimeout` from the task definition
    # (`local.stop_timeout_seconds` in deploy/terraform/modules/runner):
    # SIGTERM, then SIGKILL that many seconds later. Change one and
    # change the other, or the stack cancels on a different clock from
    # the deployment and a job that looks promptly cancelled here is
    # still building in production.
    if BACKEND == "proot":
        if not proot_stop(task_id):
            log(f"StopTask {task_id}: no such task")
            return 400, {"__type": "InvalidParameterException", "message": "The referenced task was not found."}
    else:
        res = docker("stop", "-t", "10", f"weft-runner-{task_id}", check=False)
        if res.returncode != 0:
            log(f"StopTask {task_id}: {res.stderr.strip()}")
            return 400, {"__type": "InvalidParameterException", "message": "The referenced task was not found."}
    log(f"StopTask {task_id}: {req.get('reason', '')!r}")
    return 200, {"task": {"taskArn": arn, "lastStatus": "STOPPED"}}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/x-amz-json-1.1")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        problem = verify(self, body)
        if problem:
            log(f"refused: {problem}")
            return self.reply(403, {"__type": "InvalidSignatureException", "message": problem})
        target = self.headers.get("X-Amz-Target", "")
        try:
            req = json.loads(body)
        except ValueError:
            return self.reply(400, {"__type": "SerializationException", "message": "not JSON"})
        if target == "AmazonEC2ContainerServiceV20141113.RunTask":
            return self.reply(*run_task(req))
        if target == "AmazonEC2ContainerServiceV20141113.StopTask":
            return self.reply(*stop_task(req))
        if target == "AmazonEC2ContainerServiceV20141113.DescribeTasks":
            return self.reply(*describe_tasks(req))
        return self.reply(400, {"__type": "UnknownOperationException", "message": target})

    def do_GET(self):
        # `/tasks` is a window on what this stand-in started, in
        # DescribeTasks' shape but unsigned and read-only — the same
        # affordance the CI provider's `/runs` gives, and for the same
        # reason: a manual pass has to be able to ask whether the compute
        # really stopped without holding the docker socket or a
        # credential. Real ECS has no such route; nothing in the product
        # reads it.
        if self.path.split("?", 1)[0] == "/tasks":
            return self.reply(200, {"tasks": describe(), "failures": []})
        # A liveness answer for compose's healthcheck; ECS has no such thing.
        self.reply(
            200,
            {
                "ok": True,
                "image": RUNNER_IMAGE,
                "github_image": GITHUB_RUNNER_IMAGE,
                "network": NETWORK,
            },
        )


ThreadingHTTPServer(("0.0.0.0", BIND_PORT), Handler).serve_forever()
