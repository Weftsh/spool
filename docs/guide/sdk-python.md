---
layout: ../../layouts/Docs.astro
title: Python SDK
description: "weftsh for Python — create repositories, commit, read, branch and undo from Python, sync or asyncio, with repo-scoped git remotes and webhook verification built in."
---

# Python SDK

`weftsh` is the Repos API in a few lines of Python: a repository per user,
session or agent, written and read without a checkout anywhere.

```python
import os
from weftsh import Weft

weft = Weft(token=os.environ["WEFT_TOKEN"], org="acme")

repo = weft.create_repo()

repo.create_commit(message="agent step 1") \
    .put("src/app.py", "print('hello')\n") \
    .put("README.md", "# session\n") \
    .send()

repo.read_file("src/app.py")  # "print('hello')\n"
repo.get_remote_url()         # https://x:weft_…@api.weft.sh/acme/repo-….git
```

There is a sync client, `Weft`, and an identical one for `asyncio`,
`AsyncWeft`. It needs Python 3.10+ and has one dependency, `httpx`. It is
fully typed, and every result is a frozen dataclass. The source and the full
reference are on [GitHub](https://github.com/weftsh/python-sdk); the same API
is in the [TypeScript SDK](/docs/sdk/).

## Quickstart

From nothing to a repository you have committed to over HTTP and cloned with
`git`, in about five minutes.

**1. Get a token.** [Create an account](/login?mode=signup) (free, no card)
and an organization, then mint a token under **Settings → Tokens** with
`org:read` and `repo:write`. `repo:write` creates and commits; `org:read` lets
the SDK mint the short-lived clone credential in the last step. An `org:admin`
token does both. See [authentication](/docs/authentication/) for the rest.

```bash
export WEFT_TOKEN=weft_…     # the token you just minted
export WEFT_ORG=acme         # your organization's name
```

**2. Install.**

```bash
pip install weftsh
```

**3. Save this as `quickstart.py` and run it** with `python quickstart.py`. It
needs `git` on your `PATH` for the last step.

```python
import os
import subprocess
import tempfile
from pathlib import Path

from weftsh import DEFAULT_BASE_URL, Weft

weft = Weft(
    token=os.environ["WEFT_TOKEN"],
    org=os.environ["WEFT_ORG"],
    base_url=os.environ.get("WEFT_URL", DEFAULT_BASE_URL),  # optional
)

# 1. A repository of its own: a real git remote, made in well under a second.
repo = weft.create_repo()
print("created   ", repo.name)

# 2. A commit, straight over HTTP. No clone, no checkout, no disk.
result = (
    repo.create_commit(message="first commit")
    .put("hello.txt", "hello from the Weft SDK\n")
    .send()
)
print("committed ", result.commit[:7])

# 3. Read it back, at the branch tip or at any commit.
print("read back ", repr(repo.read_file("hello.txt")))

# 4. It is still git. This URL carries a credential for this repository
#    only, and it expires in an hour.
url = repo.get_remote_url()
clone = Path(tempfile.mkdtemp(prefix="weft-")) / repo.name
subprocess.run(["git", "clone", "--quiet", url, str(clone)], check=True)
print("cloned    ", (clone / "hello.txt").read_text().strip())
```

**4. See what it did.** You should get something like this (your repository
name and commit will differ):

```text
created    repo-75f21a56-2e6d-445f-bcde-09a7f20d0bfb
committed  889ba6d
read back  'hello from the Weft SDK\n'
cloned     hello from the Weft SDK
```

That repository is yours: it is in the dashboard, you can `git push` to it,
and it costs nothing while it sits there.

## Set up the client

The token stays on your server; see
[giving a sandbox access](#give-a-sandbox-a-git-remote) for what to hand
anything else. 

```python
weft = Weft(
    token=os.environ["WEFT_TOKEN"],  # weft_<id>_<secret>
    org="acme",                      # your organization, or your personal namespace
    # base_url="https://api.weft.sh",  # the default
    # http_client=httpx.Client(...),   # for retries, proxies or logging
)
```

## Repositories

```python
repo = weft.create_repo()  # a generated name: one per session
docs = weft.create_repo(name="docs-site", description="Generated docs")

found = weft.find_one(name="docs-site")  # None if there is none
known = weft.repo("session-8412")        # no request at all

for r in weft.iterate_repos():
    print(r.name)

weft.create_repos(["agent-1", "agent-2"])  # up to 1,000 per call
weft.delete_repos(["agent-1", "agent-2"])
```

A dormant repository costs storage and nothing else, so keeping one per
session is the normal pattern, not a cleanup problem.

## Commit

A commit is built server-side from the changes you send. A `str` goes as
UTF-8 text; `bytes` go untouched.

```python
from weftsh import Identity

result = (
    repo.create_commit(
        message="agent step 12",
        branch="main",  # the default, whatever the repo's default branch is
        author=Identity(name="Build Agent", email="agent@acme.dev"),
        context={"run": "r-42", "prompt": "p-991"},  # lands in the audit trail
    )
    .put("src/app.py", source)
    .put("assets/logo.png", png_bytes)
    .delete("notes.txt")
    .send()
)
result.commit, result.parent
```

`context` is recorded immutably beside the commit and the token that made it
— see [audit & undo](/docs/audit-and-undo/).

**Concurrency.** Pass `expected_parent` with the commit you built against. If
the branch has moved, `send()` raises a `WeftConflictError` whose
`current_tip` is where the branch is now:

```python
from weftsh import WeftConflictError

try:
    repo.create_commit(message="step 13", expected_parent=seen).put("a.txt", "x").send()
except WeftConflictError as e:
    seen = e.current_tip  # rebase onto it and try again
```

Leave `expected_parent` out to commit on whatever the tip is now; pass `None`
to require that the branch does not exist yet.

## Read

Any file at any revision — a full commit SHA, a branch, a tag or `HEAD`:

```python
repo.read_file("src/app.py")                # str, or None
repo.read_file("src/app.py", ref=old_commit)

file = repo.get_file("assets/logo.png")     # content, binary flag, commit, etag
again = repo.get_file("assets/logo.png", if_none_match=file.etag)
again.not_modified                          # True — a 304 and no bytes
```

Directories, history and diffs:

```python
repo.get_tree(path="src")  # one level: name, kind, mode, oid
repo.list_files()          # every path, flat, in one request

for commit in repo.iterate_commits(path="src/app.py"):  # only commits that touched it
    print(commit.sha, commit.change)

repo.get_diff(from_="v1.0.0", to="main")  # changes: path, status, old_oid, new_oid
```

## Branch, tag and undo

```python
repo.create_branch(name="feature/login", from_="main")
repo.create_tag(name="v1.0.0", target="main")
repo.list_branches()  # sorted, with the default marked

# Put the branch back where it was before the agent went sideways.
repo.reset(to=good_commit, expected_head=bad_commit)

# Or append a commit that undoes the head.
repo.revert()
```

A reset erases nothing: the commits it leaves behind stay reachable by SHA
until garbage collection.

## Give a sandbox a git remote

Every repository is a real git remote. `get_remote_url()` puts a **fresh
credential scoped to that one repository** into the URL, so a sandbox, a CI
job or a subprocess can clone and push without ever seeing your token:

```python
url = repo.get_remote_url()                             # can push; dies in an hour
read_only = repo.get_remote_url(access="read", ttl=600)

subprocess.run(["git", "clone", url, "work"], check=True)
```

The credential cannot reach any other repository, and a `read` one cannot
push. Minting it needs a client token with `org:read` and
`repo:write` (or `org:admin`); without them the SDK says so. To mint tokens directly, `weft.create_token(scopes=…, repo=…, ttl=…)`
does the same with every option exposed.

## Verify webhooks

`verify_webhook` checks a delivery's `X-Weft-Signature-256` against the raw
body and returns the event, or `None` — answer `None` with a `401`:

```python
from weftsh import verify_webhook

@app.post("/hooks/weft")
def weft_hook():
    event = verify_webhook(
        payload=request.get_data(),  # the raw body
        signature=request.headers.get("X-Weft-Signature-256"),
        secret=os.environ["WEFT_WEBHOOK_SECRET"],
    )
    if event is None:
        return "bad signature", 401
    # event["event"] is "push", "change.landed" or "change.ejected"
    return "ok"
```

Subscribe with `repo.create_webhook(url=…)`, which returns the secret once.
What each event carries is in [webhooks](/docs/webhooks/#the-events).

## asyncio

`AsyncWeft` has the same methods, arguments and results, with `await`:

```python
from weftsh import AsyncWeft

async with AsyncWeft(token=os.environ["WEFT_TOKEN"], org="acme") as weft:
    repo = await weft.create_repo()
    await repo.create_commit(message="step 1").put("a.txt", "a").send()
    async for commit in repo.iterate_commits():
        print(commit.sha)
```

## Errors

Every failed request raises a `WeftError` with the HTTP `status` and the
server's own sentence as its `message`; a `409` from a concurrency check is
the `WeftConflictError` subclass. `find_one` returns `None` for a repository
that is not there, and `get_file` and `read_file` for a path or revision that
is not there; a file read from a repository that does not exist still raises.

The SDK never retries by itself — a write that failed on the way back may
still have happened. Pass your own `httpx` client to decide what is safe to
retry.

## Everything else

The SDK also covers forks, mirrors, repository settings, tokens and bundle
[export](/docs/export/); the [README](https://github.com/weftsh/python-sdk#readme)
has every method. Anything the SDK does not wrap is in the REST API, described
in full by [`/openapi.json`](/openapi.json).
