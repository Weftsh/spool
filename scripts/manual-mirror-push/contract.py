#!/usr/bin/env python3
"""The write-through mirror contract, against real GitHub.

Driven by scripts/manual-mirror-push.sh, which has the preflight and the
prose. Every push here is the one
`crates/stratum-server/src/mirror/forward.rs::push_to_origin` sends —
`git push --porcelain --atomic --no-verify --force-with-lease=<ref>:<old>
origin <new>:<ref>` from a bare clone, under an installation token —
and what GitHub prints back is what `classify` reads. So a pass says
GitHub answers the shapes the classifier was written for, and
`fixtures` makes the observed wire the evidence the unit test replays.
"""
import base64
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FIXTURES = os.path.join(REPO, "crates", "stratum-testkit", "fixtures", "mirror-push")
STATE_DIR = os.environ.get("STRATUM_MANUAL_MIRROR_PUSH_DIR") or os.path.join(
    REPO, ".stack", "manual-mirror-push"
)
STATE_FILE = os.path.join(STATE_DIR, "state.json")
API = os.environ.get("STRATUM_GITHUB_API_BASE", "https://api.github.com").rstrip("/")
GIT_BASE = os.environ.get("STRATUM_GITHUB_GIT_BASE", "https://github.com").rstrip("/")

APP_ID = os.environ["STRATUM_GITHUB_APP_ID"]
APP_KEY = os.environ["STRATUM_GITHUB_APP_KEY_PEM"]
INSTALLATION = os.environ["STRATUM_GITHUB_INSTALLATION_ID"]
MIRROR_REPO = os.environ["STRATUM_GITHUB_MIRROR_REPO"]
PROTECTED_BRANCH = os.environ.get("STRATUM_GITHUB_PROTECTED_BRANCH")
DENIED_INSTALLATION = os.environ.get("STRATUM_GITHUB_DENIED_INSTALLATION_ID")
DENIED_REPO = os.environ.get("STRATUM_GITHUB_DENIED_REPO")

ZERO = "0" * 40

# The beliefs the fixtures and the e2e hook encode. `classify` in
# forward.rs reads exactly these phrases.
BELIEVED_PROTECTED_REASON = "protected branch hook declined"
BELIEVED_PROTECTED_REMOTE = "GH006: Protected branch update failed"
# OBSERVED, not believed: real GitHub answered this on 2026-09-16, and
# it is what git's own receive-pack says too. The previous value here,
# `atomic push failure`, was the *sending* end's phrase and matched
# nothing the receiving end sends.
BELIEVED_ATOMIC_SIBLING_REASON = "atomic transaction failed"
BELIEVED_DENIED_REMOTE = "Write access to repository not granted"
BELIEVED_DENIED_STATUS = "returned error: 403"
BELIEVED_STALE_REASON = "stale info"

# ------------------------------------------------------------ reporting

PASSES, NOTES, FAILS = [], [], []


def ok(msg):
    PASSES.append(msg)
    print(f"  \033[32mPASS\033[0m  {msg}")


def note(msg):
    NOTES.append(msg)
    print(f"  \033[33mNOTE\033[0m  {msg}")


def fail(msg):
    FAILS.append(msg)
    print(f"  \033[31mFAIL\033[0m  {msg}")


def die(msg):
    print(f"\033[31merror:\033[0m {msg}", file=sys.stderr)
    sys.exit(1)


def check(cond, msg, detail=None):
    if cond:
        ok(msg)
    else:
        fail(msg + (f" — {detail}" if detail else ""))
    return cond


def say(title):
    print(f"\n\033[1m== {title}\033[0m")


def summary():
    print()
    print(f"\033[1m{len(PASSES)} passed, {len(NOTES)} noted, {len(FAILS)} failed\033[0m")
    for n in NOTES:
        print(f"  NOTE  {n}")
    for f in FAILS:
        print(f"  FAIL  {f}")
    if NOTES:
        print("A NOTE is a case this run did not observe. It is not a pass.")
    return 1 if FAILS else 0


# ------------------------------------------------------------ state


def load_state():
    try:
        with open(STATE_FILE) as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def save_state(st):
    os.makedirs(STATE_DIR, exist_ok=True)
    with open(STATE_FILE, "w") as f:
        json.dump(st, f, indent=2)


# ------------------------------------------------------------ the App


def b64url(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def _pem_path():
    if "-----BEGIN" not in APP_KEY:
        return APP_KEY
    fd, path = tempfile.mkstemp(prefix="weft-app-key-", suffix=".pem")
    with os.fdopen(fd, "w") as f:
        f.write(APP_KEY if APP_KEY.endswith("\n") else APP_KEY + "\n")
    os.chmod(path, 0o600)
    return path


def rs256(signing_input: bytes) -> bytes:
    try:
        from cryptography.hazmat.primitives import hashes, serialization
        from cryptography.hazmat.primitives.asymmetric import padding

        pem = APP_KEY.encode() if "-----BEGIN" in APP_KEY else open(APP_KEY, "rb").read()
        key = serialization.load_pem_private_key(pem, password=None)
        return key.sign(signing_input, padding.PKCS1v15(), hashes.SHA256())
    except ImportError:
        out = subprocess.run(
            ["openssl", "dgst", "-sha256", "-sign", _pem_path(), "-binary"],
            input=signing_input,
            capture_output=True,
            check=True,
        )
        return out.stdout


def app_jwt():
    now = int(time.time())
    header = b64url(json.dumps({"alg": "RS256", "typ": "JWT"}, separators=(",", ":")).encode())
    payload = b64url(
        json.dumps({"iat": now - 60, "exp": now + 540, "iss": APP_ID}, separators=(",", ":")).encode()
    )
    signing = f"{header}.{payload}"
    return f"{signing}.{b64url(rs256(signing.encode()))}"


def call(method, path, auth, body=None):
    req = urllib.request.Request(API + path, method=method)
    req.add_header("Authorization", auth)
    req.add_header("Accept", "application/vnd.github+json")
    req.add_header("User-Agent", "weft-manual-mirror-push")
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, data, timeout=30) as r:
            return r.status, json.loads(r.read().decode() or "{}")
    except urllib.error.HTTPError as e:
        text = e.read().decode(errors="replace")
        try:
            return e.code, json.loads(text)
        except ValueError:
            return e.code, {"unparseable": text[:200]}


_TOKENS = {}


def install_token(installation):
    if installation in _TOKENS:
        return _TOKENS[installation]
    st, body = call("POST", f"/app/installations/{installation}/access_tokens", f"Bearer {app_jwt()}")
    if st != 201 or not body.get("token"):
        die(f"could not mint an installation token for {installation}: {st} {body}")
    _TOKENS[installation] = body["token"]
    return body["token"]


def permissions(installation):
    st, body = call("GET", f"/app/installations/{installation}", f"Bearer {app_jwt()}")
    if st != 200:
        die(f"GET /app/installations/{installation}: {st} {body}")
    return body.get("permissions", {})


# ------------------------------------------------------------ git, the way forward.rs runs it


def git(cwd, *args, stdin=None):
    """A scrubbed git, like `stratum_engine::gitcmd`: no config, no prompt."""
    env = {
        "PATH": os.environ.get("PATH", ""),
        "HOME": "/nonexistent",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_TERMINAL_PROMPT": "0",
    }
    p = subprocess.run(["git", "-C", cwd, *args], input=stdin, capture_output=True, text=True, env=env)
    return p.returncode, p.stdout, p.stderr


def seed_for(installation, repo):
    """A bare clone with the origin the sync uses: token in the URL."""
    seed = os.path.join(STATE_DIR, "seeds", repo.replace("/", "-") + ".git")
    url = f"{GIT_BASE.split('://')[0]}://x-access-token:{install_token(installation)}@{GIT_BASE.split('://')[1]}/{repo}.git"
    if not os.path.isdir(seed):
        os.makedirs(os.path.dirname(seed), exist_ok=True)
        rc, _, err = git(os.path.dirname(seed), "clone", "-q", "--bare", url, seed)
        if rc != 0:
            die(f"clone {repo}: {err.strip()}")
    git(seed, "remote", "set-url", "origin", url)
    rc, _, err = git(seed, "fetch", "-q", "--prune", "origin", "+refs/heads/*:refs/heads/*")
    if rc != 0:
        die(f"fetch {repo}: {err.strip()}")
    return seed


def head_of(seed, ref):
    rc, out, _ = git(seed, "rev-parse", "--verify", "-q", ref)
    return out.strip() if rc == 0 else None


def new_commit(seed, parent, message):
    """A commit object the seed holds: the parent's tree, a new message."""
    rc, tree, err = git(seed, "rev-parse", f"{parent}^{{tree}}")
    if rc != 0:
        die(f"tree of {parent}: {err.strip()}")
    env_author = f"weft manual <manual@weft.test>"
    p = subprocess.run(
        ["git", "-C", seed, "commit-tree", tree.strip(), "-p", parent, "-m", message],
        capture_output=True,
        text=True,
        env={
            **{k: v for k, v in os.environ.items() if k == "PATH"},
            "GIT_AUTHOR_NAME": "weft manual", "GIT_AUTHOR_EMAIL": "manual@weft.test",
            "GIT_COMMITTER_NAME": "weft manual", "GIT_COMMITTER_EMAIL": "manual@weft.test",
            "HOME": "/nonexistent", "GIT_CONFIG_NOSYSTEM": "1",
        },
    )
    if p.returncode != 0:
        die(f"commit-tree: {p.stderr.strip()}")
    return p.stdout.strip()


def forward(seed, updates):
    """Exactly `push_to_origin`: (old, new, ref) triples → porcelain."""
    args = ["push", "--porcelain", "--atomic", "--no-verify"]
    for old, new, ref in updates:
        args.append(f"--force-with-lease={ref}:{'' if old == ZERO else old}")
    args.append("origin")
    for old, new, ref in updates:
        args.append(f":{ref}" if new == ZERO else f"{new}:{ref}")
    rc, out, err = git(seed, *args)
    # The token must never reach a fixture.
    tok = _TOKENS.get(INSTALLATION) or ""
    scrub = lambda s: s.replace(tok, "<token>") if tok else s
    return rc == 0, scrub(out), scrub(err)


def porcelain_lines(out):
    rows = []
    for line in out.splitlines():
        parts = line.split("\t")
        if len(parts) >= 3 and len(parts[0]) == 1:
            rows.append((parts[0], parts[1], "\t".join(parts[2:])))
    return rows


def record(st, name, exit_ok, out, err, updates):
    st.setdefault("observed", {})[name] = {
        "exit_ok": exit_ok, "stdout": out, "stderr": err,
        "updates": [{"old": o, "new": n, "ref": r} for o, n, r in updates],
    }
    save_state(st)


# ------------------------------------------------------------ steps


def step_perms(st):
    say("perms — the installation holds Contents: write")
    perms = permissions(INSTALLATION)
    check(perms.get("contents") == "write", f"installation {INSTALLATION}: contents: {perms.get('contents')!r}")
    st["perms"] = perms
    if DENIED_INSTALLATION:
        dp = permissions(DENIED_INSTALLATION)
        st["denied_perms"] = dp
        if dp.get("contents") == "write":
            fail(f"STRATUM_GITHUB_DENIED_INSTALLATION_ID={DENIED_INSTALLATION} holds contents: write; `denied` cannot be claimed under it")
        else:
            ok(f"installation {DENIED_INSTALLATION}: contents: {dp.get('contents')!r} — `denied` can be observed")
    else:
        note("no STRATUM_GITHUB_DENIED_INSTALLATION_ID: the missing-permission refusal (belief 3) will not be observed")
    save_state(st)


def step_push(st):
    say("push — a new branch with the dispatcher's flags, then its deletion")
    seed = seed_for(INSTALLATION, MIRROR_REPO)
    base = head_of(seed, "HEAD")
    branch = f"refs/heads/weft-manual-{int(time.time())}"
    commit = new_commit(seed, base, "weft manual: forwarded push")
    exit_ok, out, err = forward(seed, [(ZERO, commit, branch)])
    record(st, "push", exit_ok, out, err, [(ZERO, commit, branch)])
    rows = porcelain_lines(out)
    check(exit_ok and any(f == "*" and spec.endswith(f":{branch}") for f, spec, _ in rows),
          f"a new branch lands with a `*` porcelain line: {out.strip()!r}", err.strip())
    st["branch"] = {"ref": branch, "commit": commit, "base": base}
    exit_ok, out, err = forward(seed, [(commit, ZERO, branch)])
    record(st, "delete", exit_ok, out, err, [(commit, ZERO, branch)])
    rows = porcelain_lines(out)
    check(exit_ok and any(f == "-" for f, _, _ in rows),
          f"a deletion lands with a `-` porcelain line: {out.strip()!r}", err.strip())
    save_state(st)


def step_stale(st):
    say("stale — the same push with a lease the origin does not hold (belief 4)")
    seed = seed_for(INSTALLATION, MIRROR_REPO)
    base = head_of(seed, "HEAD")
    branch = f"refs/heads/weft-manual-stale-{int(time.time())}"
    c1 = new_commit(seed, base, "weft manual: first")
    exit_ok, out, err = forward(seed, [(ZERO, c1, branch)])
    if not exit_ok:
        die(f"could not create {branch}: {err.strip()}")
    # The mirror advertised "absent" (the empty lease) but the origin has c1.
    c2 = new_commit(seed, base, "weft manual: stale")
    exit_ok, out, err = forward(seed, [(ZERO, c2, branch)])
    record(st, "stale", exit_ok, out, err, [(ZERO, c2, branch)])
    rows = porcelain_lines(out)
    check(not exit_ok and any(f == "!" and BELIEVED_STALE_REASON in s for f, _, s in rows),
          f"a wrong lease is `! … ({BELIEVED_STALE_REASON})`: {out.strip()!r}", err.strip())
    forward(seed, [(c1, ZERO, branch)])
    save_state(st)


def step_nonff(st):
    say("nonff — a non-fast-forward under a matching lease is forced (belief 4)")
    seed = seed_for(INSTALLATION, MIRROR_REPO)
    base = head_of(seed, "HEAD")
    branch = f"refs/heads/weft-manual-nonff-{int(time.time())}"
    c1 = new_commit(seed, base, "weft manual: first")
    exit_ok, out, err = forward(seed, [(ZERO, c1, branch)])
    if not exit_ok:
        die(f"could not create {branch}: {err.strip()}")
    # c2 does not descend from c1 (both descend from base): a rewrite.
    c2 = new_commit(seed, base, "weft manual: rewritten")
    exit_ok, out, err = forward(seed, [(c1, c2, branch)])
    record(st, "nonff", exit_ok, out, err, [(c1, c2, branch)])
    rows = porcelain_lines(out)
    check(exit_ok and any(f == "+" for f, _, _ in rows),
          f"a rewrite under the right lease is forced (`+`): {out.strip()!r}", err.strip())
    forward(seed, [(c2, ZERO, branch)])
    save_state(st)


def step_protected(st):
    say("protected — a push to a protected branch, alone and beside a sibling (beliefs 1, 2)")
    if not PROTECTED_BRANCH:
        note("no STRATUM_GITHUB_PROTECTED_BRANCH: the protected-branch refusal and the atomic sibling (beliefs 1, 2) will not be observed")
        return
    seed = seed_for(INSTALLATION, MIRROR_REPO)
    ref = f"refs/heads/{PROTECTED_BRANCH}"
    tip = head_of(seed, ref)
    if not tip:
        fail(f"{ref} does not exist on {MIRROR_REPO}")
        return
    c = new_commit(seed, tip, "weft manual: onto a protected branch")
    exit_ok, out, err = forward(seed, [(tip, c, ref)])
    record(st, "protected", exit_ok, out, err, [(tip, c, ref)])
    rows = porcelain_lines(out)
    refused = [s for f, _, s in rows if f == "!"]
    if exit_ok:
        fail(f"the push to {PROTECTED_BRANCH} was accepted: it is not protected against this installation")
        forward(seed, [(c, tip, ref)])
        return
    check(any(BELIEVED_PROTECTED_REASON in s for s in refused),
          f"belief 1: the porcelain reason is `{BELIEVED_PROTECTED_REASON}`: {refused!r}")
    check(BELIEVED_PROTECTED_REMOTE in err,
          f"belief 1: the remote explains itself with `{BELIEVED_PROTECTED_REMOTE}`", err.strip()[:300])
    # Beside a sibling the origin has no objection to.
    sibling = f"refs/heads/weft-manual-sibling-{int(time.time())}"
    c2 = new_commit(seed, tip, "weft manual: the sibling")
    exit_ok, out, err = forward(seed, [(tip, c, ref), (ZERO, c2, sibling)])
    record(st, "protected-atomic", exit_ok, out, err, [(tip, c, ref), (ZERO, c2, sibling)])
    rows = porcelain_lines(out)
    sib = [s for f, spec, s in rows if f == "!" and spec.endswith(f":{sibling}")]
    check(not exit_ok and not head_of(seed, sibling) and not any(f in "*+" for f, _, _ in rows),
          "the atomic push refused the sibling too, and nothing landed", out.strip())
    if any(BELIEVED_ATOMIC_SIBLING_REASON in s for s in sib):
        ok(f"belief 2: the sibling reports `{BELIEVED_ATOMIC_SIBLING_REASON}`")
        st["atomic_sibling_reason"] = BELIEVED_ATOMIC_SIBLING_REASON
    else:
        st["atomic_sibling_reason"] = sib[0] if sib else None
        fail(f"belief 2: the sibling's reason is {sib!r}, not `{BELIEVED_ATOMIC_SIBLING_REASON}` — fix classify and the fixture")
    # Make sure nothing is left behind.
    git(seed, "fetch", "-q", "--prune", "origin", "+refs/heads/*:refs/heads/*")
    if head_of(seed, sibling):
        forward(seed, [(head_of(seed, sibling), ZERO, sibling)])
    save_state(st)


def step_denied(st):
    say("denied — the same push under an installation without Contents: write (belief 3)")
    if not (DENIED_INSTALLATION and DENIED_REPO):
        note("no STRATUM_GITHUB_DENIED_INSTALLATION_ID / STRATUM_GITHUB_DENIED_REPO: belief 3 will not be observed")
        return
    if permissions(DENIED_INSTALLATION).get("contents") == "write":
        fail("the denied installation holds contents: write; refusing to claim this case")
        return
    seed = seed_for(DENIED_INSTALLATION, DENIED_REPO)
    base = head_of(seed, "HEAD")
    branch = f"refs/heads/weft-manual-denied-{int(time.time())}"
    c = new_commit(seed, base, "weft manual: without the permission")
    exit_ok, out, err = forward(seed, [(ZERO, c, branch)])
    tok = _TOKENS.get(DENIED_INSTALLATION) or ""
    err = err.replace(tok, "<token>") if tok else err
    record(st, "denied", exit_ok, out, err, [(ZERO, c, branch)])
    check(not exit_ok, "the push was refused")
    check(not porcelain_lines(out) or not any(f in "*+" for f, _, _ in porcelain_lines(out)),
          "no command was accepted")
    check(BELIEVED_DENIED_STATUS in err, f"belief 3: the transport says `{BELIEVED_DENIED_STATUS}`", err.strip()[:300])
    if BELIEVED_DENIED_REMOTE in err:
        ok(f"belief 3: the remote says `{BELIEVED_DENIED_REMOTE}`")
    else:
        note(f"the remote did not say `{BELIEVED_DENIED_REMOTE}`; stderr was: {err.strip()[:300]!r} — classify reads the 403 either way")
    save_state(st)


def step_fixtures(st):
    say("fixtures — the observed wire, written where the unit test replays it")
    observed = st.get("observed", {})
    if not observed:
        fail("nothing observed yet; run the steps first")
        return
    os.makedirs(FIXTURES, exist_ok=True)
    for name, o in observed.items():
        with open(os.path.join(FIXTURES, f"{name}.stdout"), "w") as f:
            f.write(o["stdout"])
        with open(os.path.join(FIXTURES, f"{name}.stderr"), "w") as f:
            f.write(o["stderr"])
    provenance = {
        "observed": True,
        "recorded_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "api": API,
        "repository": MIRROR_REPO,
        "cases": {name: {"exit_ok": o["exit_ok"], "updates": o["updates"]} for name, o in observed.items()},
        "beliefs": {
            "protected_reason": BELIEVED_PROTECTED_REASON,
            "protected_remote": BELIEVED_PROTECTED_REMOTE,
            "atomic_sibling_reason": st.get("atomic_sibling_reason"),
            "denied_status": BELIEVED_DENIED_STATUS,
            "stale_reason": BELIEVED_STALE_REASON,
        },
        "note": "stdout/stderr of the exact `git push` forward.rs sends, against real GitHub, written by "
                "scripts/manual-mirror-push.sh fixtures. Tokens are redacted.",
    }
    with open(os.path.join(FIXTURES, "provenance.json"), "w") as f:
        json.dump(provenance, f, indent=2)
        f.write("\n")
    ok(f"wrote {', '.join(sorted(observed))} (+ provenance.json) to {os.path.relpath(FIXTURES, REPO)}")
    print("        Now: cargo test -p stratum-server --bin stratum-server mirror_push_fixtures")


# ------------------------------------------------------------ main


def main(argv):
    cmd = argv[0]
    st = load_state()
    if cmd == "reset":
        shutil.rmtree(STATE_DIR, ignore_errors=True)
        print("state cleared")
        return 0
    if cmd == "all":
        shutil.rmtree(STATE_DIR, ignore_errors=True)
        st = {}
    print(f"App {APP_ID}; installation {INSTALLATION} on {MIRROR_REPO}; API {API}; state in {STATE_DIR}")
    steps = {
        "perms": lambda: step_perms(st),
        "push": lambda: step_push(st),
        "stale": lambda: step_stale(st),
        "nonff": lambda: step_nonff(st),
        "protected": lambda: step_protected(st),
        "denied": lambda: step_denied(st),
        "fixtures": lambda: step_fixtures(st),
    }
    order = ["perms", "push", "stale", "nonff", "protected", "denied", "fixtures"]
    if cmd == "all":
        for name in order:
            steps[name]()
        return summary()
    if cmd not in steps:
        print(f"unknown step {cmd!r}; one of {', '.join(order)}, all, reset", file=sys.stderr)
        return 2
    steps[cmd]()
    return summary()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
