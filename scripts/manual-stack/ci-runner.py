#!/usr/bin/env python3
"""A miniature but real CI provider, for the manual stack.

This is not a mock. It is the smallest honest implementation of the loop
a project hosted on Stratum actually has to build, and it is here to be
read as the worked example for that loop as much as to be run:

  1. Stratum POSTs a webhook to it on `push`, `change.landed` and
     `change.ejected`.
  2. It verifies `X-Weft-Signature-256` before trusting one byte of
     the delivery.
  3. It clones the repository from Stratum with a real credential — a
     `repo:read` token bound to that one repository — and runs the
     project's own `ci.sh` against the working tree.
  4. It signs a verdict with the repository's CI intake secret and POSTs
     it to `…/ci/checks`.

Every one of those is a seam no unit test in this workspace touches: that
the webhook fires at all, that the signature we send verifies under a
signature check somebody else wrote, that the clone credential works from
a third party, that the intake accepts what a real client sends, and that
the verdict reaches the Checks tab and the land gate.

Two things about the product are visible from in here, and both are the
point rather than an inconvenience:

**The push payload is `{"via":"git"}` and nothing else.** It does not say
which ref moved, or to what. A receiver has no choice but to go and look,
so this keeps its own snapshot of the repository's refs and diffs it
against a fresh `git ls-remote` on every delivery. That is exactly the
work a real provider does, and it is why the snapshot is primed at
startup: a ref that already existed before the first delivery is not news.

**A push does not create a patchset.** A change and its patchsets come
from `POST …/changes`, which somebody makes when they open or update a
review — so at the instant the push webhook fires, the change either does
not exist yet or its latest patchset still names the previous commit. A
patchset-scoped report for a commit the change has not caught up with is
answered `409` by the intake, correctly. So `wait_for_patchset` polls the
change until its tip is the commit we built, for a bounded time, and says
so in the run log when it gives up. Any receiver that wants to gate a
land has to do this; the docs' snippets do not mention it.

Configuration is all environment, all required except where noted:

  CI_RUNNER_PORT            where to listen                (default 59120)
  CI_RUNNER_PUBLIC_URL      how the runs log is linked to  (default from port)
  STRATUM_URL               the forge                      (default :8080)
  CI_RUNNER_ORG             org name, e.g. `acme`
  CI_RUNNER_REPO            repo name, e.g. `pipeline`
  CI_RUNNER_HOOK_SECRET     the webhook subscription's secret
  CI_RUNNER_INTAKE_SECRET   the repository's CI intake secret
  CI_RUNNER_CLONE_TOKEN     a `repo:read` token bound to that repository
  CI_RUNNER_NAME            check name                     (default ci/local)
  CI_RUNNER_WORK            scratch directory              (default a tempdir)
  CI_RUNNER_PATCHSET_WAIT   seconds to wait for a change to catch up (default 90)

It answers three of its own routes:

  POST /hook     the delivery endpoint you register with Stratum
  GET  /runs     everything it has done, as JSON — what to wait on
  GET  /runs/<n> one run, as text — this is the URL the verdict links to

`GET /runs` is the observable. Wait on the verdict appearing there rather
than on a sleep: this thing clones over HTTP and shells out to git, and
how long that takes is not a constant.

FINDING, reported rather than worked around: the webhook envelope carries
`repo_id` — an opaque ulid — and never the org/repo names. A receiver
cannot turn a delivery into a clone URL or an intake URL from the
delivery alone, so this runner is configured with the names out of band.
That is survivable for a per-repository subscription, which is what
`POST …/repos/{repo}/webhooks` creates, and it is a wall for anything
serving more than one. See the report accompanying this change.
"""
import hashlib
import hmac
import json
import os
import queue
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

PORT = int(os.environ.get("CI_RUNNER_PORT", "59120"))
PUBLIC_URL = os.environ.get("CI_RUNNER_PUBLIC_URL", f"http://127.0.0.1:{PORT}")
STRATUM = os.environ.get("STRATUM_URL", "http://127.0.0.1:8080").rstrip("/")
ORG = os.environ.get("CI_RUNNER_ORG", "acme")
REPO = os.environ.get("CI_RUNNER_REPO", "pipeline")
HOOK_SECRET = os.environ.get("CI_RUNNER_HOOK_SECRET", "")
INTAKE_SECRET = os.environ.get("CI_RUNNER_INTAKE_SECRET", "")
CLONE_TOKEN = os.environ.get("CI_RUNNER_CLONE_TOKEN", "")
CHECK_NAME = os.environ.get("CI_RUNNER_NAME", "ci/local")
PATCHSET_WAIT = float(os.environ.get("CI_RUNNER_PATCHSET_WAIT", "90"))
WORK = os.environ.get("CI_RUNNER_WORK") or tempfile.mkdtemp(prefix="ci-runner-")

SIG_HEADER = "X-Weft-Signature-256"

# The run log. Appended to by the worker, read by `GET /runs` and by the
# walkthrough; guarded because those are different threads.
runs = []
runs_lock = threading.Lock()
work_q = queue.Queue()

# ref -> sha, as of the last time we looked. Primed at startup so that
# the repository's existing history is not mistaken for a push.
known_refs = {}


def log(*a):
    print("ci-runner:", *a, file=sys.stderr, flush=True)


def record(**fields):
    with runs_lock:
        fields["n"] = len(runs) + 1
        fields.setdefault("at", int(time.time() * 1000))
        runs.append(fields)
        return fields


# ---------------------------------------------------------------------
# The signature, both directions
# ---------------------------------------------------------------------


def verify_delivery(secret, presented, body):
    """Is `presented` a signature over `body` with `secret`?

    The same scheme the intake uses in the other direction — hex
    HMAC-SHA256 over the raw bytes, prefixed `sha256=` — so a receiver
    only ever learns one shape. `compare_digest`, not `==`: this decides
    whether a delivery is trusted.
    """
    if not presented or not secret:
        return False
    want = "sha256=" + hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(want, presented)


def sign_intake(body):
    return "sha256=" + hmac.new(
        INTAKE_SECRET.encode(), body, hashlib.sha256
    ).hexdigest()


# ---------------------------------------------------------------------
# Talking to the forge
# ---------------------------------------------------------------------


def clone_url():
    """The URL a third party clones with. The token is HTTP Basic, in the
    password field, which is what the docs tell a person to do."""
    u = urlparse(STRATUM)
    return f"{u.scheme}://x:{CLONE_TOKEN}@{u.netloc}/{ORG}/{REPO}.git"


def git(*args, cwd=None, check=True):
    p = subprocess.run(
        ["git", *args],
        cwd=cwd,
        capture_output=True,
        text=True,
        timeout=120,
        env={**os.environ, "GIT_TERMINAL_PROMPT": "0"},
    )
    if check and p.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)}: {p.stderr.strip()}")
    return p


def remote_refs():
    """Every branch the forge is serving us, by name.

    This is also the first proof that the clone credential works: an
    unauthenticated `ls-remote` against a private repository is refused,
    and a token with the wrong scope or bound to the wrong repository is
    refused too. If this raises, nothing below is worth trying.
    """
    out = git("ls-remote", "--heads", clone_url()).stdout
    refs = {}
    for line in out.splitlines():
        sha, _, ref = line.partition("\t")
        if ref.startswith("refs/heads/"):
            refs[ref] = sha.strip()
    return refs


def api_get(path):
    req = urllib.request.Request(
        f"{STRATUM}{path}", headers={"Authorization": f"Bearer {CLONE_TOKEN}"}
    )
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        return e.code, None


def post_verdict(body):
    raw = json.dumps(body).encode()
    req = urllib.request.Request(
        f"{STRATUM}/v1/orgs/{ORG}/repos/{REPO}/ci/checks",
        data=raw,
        method="POST",
        headers={"Content-Type": "application/json", SIG_HEADER: sign_intake(raw)},
    )
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return r.status, r.read().decode()[:400]
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()[:400]


# ---------------------------------------------------------------------
# The build
# ---------------------------------------------------------------------


def build(ref, sha):
    """Clone at `sha` and run the project's own `ci.sh`.

    The provider does not know what the script does, which is the whole
    relationship: a repository decides what passing means, and CI decides
    only whether the script said so. A repository with no `ci.sh` is not
    a failure — there is nothing to report, and reporting `failing`
    because a project has no build would be a lie about the project.
    """
    d = os.path.join(WORK, sha[:12])
    shutil.rmtree(d, ignore_errors=True)
    git("clone", "--quiet", "--branch", ref[len("refs/heads/") :], clone_url(), d)
    git("checkout", "--quiet", sha, cwd=d)
    message = git("log", "-1", "--format=%B", sha, cwd=d).stdout

    script = os.path.join(d, "ci.sh")
    if not os.path.isfile(script):
        return None, message, "no ci.sh in this repository — nothing to report"

    p = subprocess.run(
        ["sh", "ci.sh"], cwd=d, capture_output=True, text=True, timeout=120
    )
    ok = p.returncode == 0
    tail = (p.stdout + p.stderr).strip().splitlines()
    summary = tail[-1][:200] if tail else ("ci.sh passed" if ok else "ci.sh failed")
    return ok, message, summary


CHANGE_ID = re.compile(r"^Change-Id:\s*([Ii][0-9a-fA-F]{8,40})\s*$", re.M)


def change_key(message):
    """The trailer, from the message's last non-empty paragraph, lowered.

    Matched the way the server matches it (`review/change_id.rs`): a
    `Change-Id:` line anywhere else in the message is not a trailer, and
    treating one as such would attach a verdict to a change the commit
    is not part of.
    """
    paras = [p for p in message.split("\n\n") if p.strip()]
    if not paras:
        return None
    m = CHANGE_ID.search(paras[-1])
    return "I" + m.group(1)[1:].lower() if m else None


def wait_for_patchset(key, sha):
    """Poll the change until its latest patchset is the commit we built.

    See the module docstring: a push does not create a patchset, so the
    commit we were told about is ahead of the change until somebody opens
    or updates the review. Returns True when they have, False when the
    wait ran out — and the caller records which, because "we never
    reported" and "we reported and it was refused" are different stories
    for whoever is looking at a change with no verdict on it.
    """
    deadline = time.time() + PATCHSET_WAIT
    seen = None
    while time.time() < deadline:
        status, body = api_get(f"/v1/orgs/{ORG}/repos/{REPO}/changes/{key}")
        if status == 200 and body.get("patchsets"):
            seen = body["patchsets"][-1]["commit"]
            if seen == sha:
                return True, seen
        time.sleep(1.0)
    return False, seen


# ---------------------------------------------------------------------
# One delivery
# ---------------------------------------------------------------------


def handle(event, payload):
    global known_refs
    if event not in ("push", "change.landed", "change.ejected"):
        record(event=event, skipped="not an event this provider builds for")
        return

    try:
        now = remote_refs()
    except RuntimeError as e:
        record(event=event, error=f"could not read refs: {e}")
        log("could not read refs —", e)
        return

    moved = [(r, s) for r, s in now.items() if known_refs.get(r) != s]
    known_refs = now
    if not moved:
        # A `change.landed` for a fast-forward we already built, or a
        # push that changed nothing we can see. Not a failure.
        record(event=event, payload=payload, skipped="no ref moved since the last look")
        return

    for ref, sha in moved:
        build_one(event, ref, sha)


def build_one(event, ref, sha):
    started = int(time.time() * 1000)
    try:
        ok, message, summary = build(ref, sha)
    except Exception as e:  # a clone or a checkout that did not work
        record(event=event, ref=ref, commit=sha, error=str(e)[:400])
        log("build failed to run —", e)
        return
    finished = int(time.time() * 1000)

    if ok is None:
        record(event=event, ref=ref, commit=sha, skipped=summary)
        return

    state = "passing" if ok else "failing"
    run = record(
        event=event, ref=ref, commit=sha, state=state, summary=summary, reports=[]
    )
    url = f"{PUBLIC_URL}/runs/{run['n']}"

    # The commit-scoped report: what the Checks tab lists. Every push
    # gets one, change or no change.
    status, body = post_verdict(
        {
            "commit": sha,
            "name": CHECK_NAME,
            "state": state,
            "url": url,
            "summary": summary,
            "ref": ref[len("refs/heads/") :],
            "event": "push",
            "actor": "ci-runner",
            "external_id": f"local-{run['n']}",
            "run_number": run["n"],
            "started_at": started,
            "completed_at": finished,
            "sent_at": int(time.time() * 1000),
        }
    )
    with runs_lock:
        run["reports"].append({"scope": "commit", "status": status, "body": body})
    log(f"commit-scoped {state} for {sha[:8]} -> {status}")

    # The patchset-scoped report: what the land gate reads. Only when the
    # commit carries a Change-Id, and only once the change has caught up.
    key = change_key(message)
    if not key:
        return
    caught_up, seen = wait_for_patchset(key, sha)
    if not caught_up:
        with runs_lock:
            run["reports"].append(
                {
                    "scope": "patchset",
                    "change": key,
                    "skipped": "the change's latest patchset is "
                    f"{seen or 'absent'}, not {sha} — a push does not create "
                    "a patchset, and nobody opened or updated the review "
                    f"within {PATCHSET_WAIT:.0f}s",
                }
            )
        log(f"no patchset for {key} at {sha[:8]} — nothing to gate on")
        return

    status, body = post_verdict(
        {
            "change": key,
            "commit": sha,
            "name": CHECK_NAME,
            "state": state,
            "url": url,
            "summary": summary,
            "sent_at": int(time.time() * 1000),
        }
    )
    with runs_lock:
        run["reports"].append(
            {"scope": "patchset", "change": key, "status": status, "body": body}
        )
    log(f"patchset-scoped {state} for {key} -> {status}")


def worker():
    while True:
        event, payload = work_q.get()
        try:
            handle(event, payload)
        except Exception as e:  # never let one delivery kill the loop
            record(event=event, error=f"unhandled: {e}")
            log("unhandled —", e)
        finally:
            work_q.task_done()


# ---------------------------------------------------------------------
# The listener
# ---------------------------------------------------------------------


class H(BaseHTTPRequestHandler):
    # HTTP/1.0 deliberately: one request per connection, closed when the
    # response is written. Under 1.1 the sender hangs up after reading
    # our 202 and the handler's next read raises ConnectionResetError
    # into the log — a stack trace beside a delivery that worked, which
    # is exactly the kind of noise that gets a real error skipped over.
    protocol_version = "HTTP/1.0"

    def log_message(self, *_a):
        pass

    def _send(self, status, body, ctype="application/json"):
        raw = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        if urlparse(self.path).path != "/hook":
            self._send(404, {"error": "no such endpoint"})
            return
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        # Before anything else. An unverified delivery is a stranger
        # telling us to go and clone something.
        if not verify_delivery(HOOK_SECRET, self.headers.get(SIG_HEADER), body):
            record(rejected="bad or missing signature", bytes=len(body))
            log("rejected a delivery: signature did not verify")
            self._send(401, {"error": "signature"})
            return
        try:
            env = json.loads(body)
        except ValueError:
            self._send(400, {"error": "not json"})
            return
        # Answer now, work later: the sender's timeout is 10s and a clone
        # is not bounded by it. A provider that builds inside the request
        # is a provider whose deliveries are recorded as failed.
        work_q.put((env.get("event", ""), env.get("payload")))
        self._send(202, {"queued": True})

    def do_GET(self):
        p = urlparse(self.path).path
        if p == "/runs":
            with runs_lock:
                self._send(200, {"runs": list(runs)})
            return
        if p.startswith("/runs/"):
            try:
                n = int(p[len("/runs/") :])
            except ValueError:
                self._send(404, {"error": "no such run"})
                return
            with runs_lock:
                one = next((r for r in runs if r["n"] == n), None)
            if one is None:
                self._send(404, {"error": "no such run"})
                return
            self._send(200, json.dumps(one, indent=2).encode(), "text/plain")
            return
        if p == "/healthz":
            self._send(200, {"ok": True})
            return
        self._send(404, {"error": "no such endpoint"})


if __name__ == "__main__":
    missing = [
        n
        for n, v in (
            ("CI_RUNNER_HOOK_SECRET", HOOK_SECRET),
            ("CI_RUNNER_INTAKE_SECRET", INTAKE_SECRET),
            ("CI_RUNNER_CLONE_TOKEN", CLONE_TOKEN),
        )
        if not v
    ]
    if missing:
        # Loudly, and before binding: a runner with no secret would
        # verify nothing and report nothing, and would look like it was
        # working right up until the walkthrough waited forever.
        log("refusing to start without:", ", ".join(missing))
        sys.exit(2)
    try:
        known_refs = remote_refs()
        log(f"primed with {len(known_refs)} refs from {ORG}/{REPO}")
    except RuntimeError as e:
        log("could not prime refs — the clone credential does not work:", e)
        sys.exit(2)
    threading.Thread(target=worker, daemon=True).start()
    log(f"listening on {PUBLIC_URL}/hook, reporting {CHECK_NAME} to {STRATUM}")
    ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
