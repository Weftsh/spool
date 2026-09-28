# First commit over REST

Your Spool server can give every user, session, or agent its own real git
repository — created in under 100 ms, written and read entirely over HTTP.

## 0. Before you start

1. **An account and an organization** on your server
   ([Authentication](authentication.md)). The repositories below are
   created in the organization `acme`, and are private to it like every
   repository on the server.
2. [Mint a token](authentication.md#minting-and-revoking-tokens) for
   your scripts, with `repo:write`. It is the `$WEFT_TOKEN` in every
   example below.

In the commands on this page, `api.weft.sh` stands for your server's
host name: put yours in its place.

## 1. Create a repo

```bash
curl -X POST https://api.weft.sh/v1/orgs/acme/repos \
  -H "Authorization: Bearer $WEFT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "name": "session-8412" }'
```

Response: `201` with the repo and its `clone_url`. Need many? Batch up to
1,000 per call at `/v1/orgs/acme/repos/batch/create`. There is no
`"public"` flag to set: every repository is private to its organization,
and `"public": true` is refused with `400`.

## 2. Commit

```bash
curl -X POST https://api.weft.sh/v1/orgs/acme/repos/session-8412/commits \
  -H "Authorization: Bearer $WEFT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "branch": "main",
    "message": "agent step 1",
    "context": { "agent_run": "r-42", "prompt": "p-991" },
    "operations": [
      { "op": "put", "path": "src/app.js", "content": "console.log(1)\n" },
      { "op": "put", "path": "README.md", "content": "# session\n" }
    ]
  }'
```

The response carries the new `commit` id. The `context` blob lands in the
immutable [audit trail](audit-and-undo.md) alongside the acting token —
that's how you answer "what did the agent change and when" months later.

**Concurrency:** pass `expected_parent` with the commit you built against.
If the branch moved, you get `409` with the current tip — rebase and retry.
Commits are durable at acknowledgment.

## 3. Read anything at any version

```bash
# newest version (ETag = content hash; If-None-Match gives you 304s)
curl -H "Authorization: Bearer $WEFT_TOKEN" \
  https://api.weft.sh/v1/orgs/acme/repos/session-8412/files/src/app.js

# the same file two commits ago
curl -H "Authorization: Bearer $WEFT_TOKEN" \
  "https://api.weft.sh/v1/orgs/acme/repos/session-8412/files/src/app.js?at=$OLD_COMMIT"
```

Also available: `/tree` listings (each entry carries a `size`, `null` for
directories), `/diff?from=…&to=…`, paginated `/log`, `/refs` for
everything at once, and `/branches` and `/tags` when you want one kind,
sorted, with the default marked.

### One file's history

`/log` takes a `path`, and then returns only the commits that changed it:

```bash
curl -H "Authorization: Bearer $WEFT_TOKEN" \
  "https://api.weft.sh/v1/orgs/acme/repos/session-8412/log?path=src/app.js"
```

```json
{ "entries": [
    { "commit": "9f2c…", "message": "fix the parser", "change": "modified",
      "author": "Ada <ada@acme.dev> 1766000000 +0000", "parents": ["7b1a…"] } ],
  "next_after": null }
```

`change` is `added`, `modified` or `deleted` — what that commit did to
that path. It appears only on a filtered request, because an unfiltered
walk says nothing about any particular file.

Do this rather than asking for the whole log and dropping rows yourself.
Both give the same answer on a small repository; on a real one, a file
touched once near the start means downloading an entire history to find
a single commit. A filtered request examines at most 500 commits and
then hands back `next_after`, so a long search is several bounded
requests instead of one unbounded scan.

A `/files` response says what it is rather than leaving you to guess:
`X-Weft-Binary` is `true` or `false`, `X-Weft-Commit` is where the
content came from, and `ETag` is the blob oid — send it back as
`If-None-Match` and an unchanged file costs a 304 and no bytes.

`Content-Type` is sniffed, and deliberately narrow. Anything textual is
`text/plain; charset=utf-8` whatever it is called: this endpoint returns
whatever somebody committed, and answering `text/html` for a file named
`index.html` would let a repository serve script from this origin.
Images and PDFs get their real type, because a browser can display those
and cannot be tricked by them; everything else is
`application/octet-stream`.

**All of this is in the dashboard too.** Open a repository and press
*Browse files* — directory listings, a file view with line numbers, the
commit log and a branch switcher, all on real URLs, so a link to a line
of code is a link you can send somebody. A file page carries its own
history: who last touched it, what each commit did to it, and a click to
read any earlier version — which puts that revision in the URL, so an
old version is as sendable as the current one.

## 4. Undo

```bash
# put the branch back where it was before the agent went sideways
curl -X POST https://api.weft.sh/v1/orgs/acme/repos/session-8412/reset \
  -H "Authorization: Bearer $WEFT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "branch": "main", "to": "'$GOOD_COMMIT'", "expected_head": "'$BAD_COMMIT'" }'
```

The undone commits stay reachable by SHA until garbage collection — undo
never erases the record. See [audit & undo](audit-and-undo.md).

## 5. It's still git

```bash
git clone https://x:$WEFT_TOKEN@api.weft.sh/acme/session-8412.git
```

Clone it, push to it, or [export it as a standard bundle](export.md) any
time. Nothing about the repository is specific to spool.

## The same thing from an SDK

Everything above is one call each in the [TypeScript SDK](sdk.md),
`npm install @weftsh/sdk`. Both SDKs default to Weft's hosted service, so
give them your server's URL:

```ts
import { Weft } from "@weftsh/sdk";

const weft = new Weft({
  token: process.env.WEFT_TOKEN!,
  org: "acme",
  baseUrl: "https://spool.example.com",
});

const repo = await weft.createRepo({ name: "session-8412" });
const { commit } = await repo
  .createCommit({ message: "agent step 1", context: { agent_run: "r-42" } })
  .put("src/app.js", "console.log(1)\n")
  .send();

await repo.readFile("src/app.js", { ref: commit });
await repo.reset({ to: goodCommit, expectedHead: commit });
const url = await repo.getRemoteURL(); // git clone this: a credential for this repo only
```

and in the [Python SDK](sdk-python.md), `pip install weftsh`:

```python
import os
from weftsh import Weft

weft = Weft(
    token=os.environ["WEFT_TOKEN"],
    org="acme",
    base_url="https://spool.example.com",
)

repo = weft.create_repo(name="session-8412")
commit = (
    repo.create_commit(message="agent step 1", context={"agent_run": "r-42"})
    .put("src/app.js", "console.log(1)\n")
    .send()
    .commit
)

repo.read_file("src/app.js", ref=commit)
repo.reset(to=good_commit, expected_head=commit)
url = repo.get_remote_url()  # git clone this: a credential for this repo only
```
