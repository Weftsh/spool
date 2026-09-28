# TypeScript SDK

`@weftsh/sdk` is the Repos API in a few lines of TypeScript: a repository per
user, session or agent, written and read without a checkout anywhere.

```ts
import { Weft } from "@weftsh/sdk";

const weft = new Weft({ token: process.env.WEFT_TOKEN!, org: "acme" });

const repo = await weft.createRepo();

await repo
  .createCommit({ message: "agent step 1" })
  .put("src/app.ts", "export const answer = 42;\n")
  .put("README.md", "# session\n")
  .send();

await repo.readFile("src/app.ts"); // "export const answer = 42;\n"
await repo.getRemoteURL(); // https://x:weft_…@api.weft.sh/acme/repo-….git
```

It has no dependencies, ships ESM and CommonJS with full types, and runs
anywhere `fetch` does: Node 20+, Bun, Deno and edge runtimes. The source and
the full reference are on
[GitHub](https://github.com/weftsh/typescript-sdk); the same API is in the
[Python SDK](sdk-python.md).

## Quickstart

From nothing to a repository you have committed to over HTTP and cloned with
`git`, in about five minutes.

**1. Get a token.** [Create an account](/login?mode=signup) (free, no card)
and an organization, then mint a token under **Settings → Tokens** with
`org:read` and `repo:write`. `repo:write` creates and commits; `org:read` lets
the SDK mint the short-lived clone credential in the last step. An `org:admin`
token does both. See [authentication](authentication.md) for the rest.

```bash
export WEFT_TOKEN=weft_…     # the token you just minted
export WEFT_ORG=acme         # your organization's name
```

**2. Install.**

```bash
npm install @weftsh/sdk
```

**3. Save this as `quickstart.mts` and run it** with `node quickstart.mts`
(Node 22.18 or later; on older versions, `npx tsx quickstart.mts`). It needs
`git` on your `PATH` for the last step.

```ts
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Weft } from '@weftsh/sdk';

const weft = new Weft({
  token: process.env.WEFT_TOKEN!,
  org: process.env.WEFT_ORG!,
  baseUrl: process.env.WEFT_URL, // optional: defaults to https://api.weft.sh
});

// 1. A repository of its own: a real git remote, made in well under a second.
const repo = await weft.createRepo();
console.log('created   ', repo.name);

// 2. A commit, straight over HTTP. No clone, no checkout, no disk.
const { commit } = await repo
  .createCommit({ message: 'first commit' })
  .put('hello.txt', 'hello from the Weft SDK\n')
  .send();
console.log('committed ', commit.slice(0, 7));

// 3. Read it back, at the branch tip or at any commit.
console.log('read back ', JSON.stringify(await repo.readFile('hello.txt')));

// 4. It is still git. This URL carries a credential for this repository
//    only, and it expires in an hour.
const url = await repo.getRemoteURL();
const dir = join(mkdtempSync(join(tmpdir(), 'weft-')), repo.name);
execFileSync('git', ['clone', '--quiet', url, dir]);
console.log('cloned    ', readFileSync(join(dir, 'hello.txt'), 'utf8').trim());
```

**4. See what it did.** You should get something like this (your repository
name and commit will differ):

```text
created    repo-4259aedc-3173-4cb7-a3cd-1dbf15fc30c7
committed  889ba6d
read back  "hello from the Weft SDK\n"
cloned     hello from the Weft SDK
```

That repository is yours: it is in the dashboard, you can `git push` to it,
and it costs nothing while it sits there.

## Set up the client

The token stays on your server; see
[giving a sandbox access](#give-a-sandbox-a-git-remote) for what to hand
anything else. 

```ts
const weft = new Weft({
  token: process.env.WEFT_TOKEN!, // weft_<id>_<secret>
  org: "acme", // your organization, or your personal namespace
  // baseUrl: "https://api.weft.sh", // the default
  // fetch: myFetch,                 // for retries, logging or a proxy
});
```

## Repositories

```ts
const repo = await weft.createRepo(); // a generated name: one per session
const docs = await weft.createRepo({ name: "docs-site", description: "Generated docs" });

const found = await weft.findOne({ name: "docs-site" }); // null if there is none
const known = weft.repo("session-8412"); // no request at all

for await (const r of weft.iterateRepos()) console.log(r.name);

await weft.createRepos([{ name: "agent-1" }, { name: "agent-2" }]); // up to 1,000 per call
await weft.deleteRepos(["agent-1", "agent-2"]);
```

A dormant repository costs storage and nothing else, so keeping one per
session is the normal pattern, not a cleanup problem.

## Commit

A commit is built server-side from the changes you send. Strings go as UTF-8;
`Uint8Array`, `ArrayBuffer` and `Blob` go as bytes, untouched.

```ts
const { commit, parent } = await repo
  .createCommit({
    message: "agent step 12",
    branch: "main", // the default, whatever the repo's default branch is
    author: { name: "Build Agent", email: "agent@acme.dev" },
    context: { run: "r-42", prompt: "p-991" }, // lands in the audit trail
  })
  .put("src/app.ts", source)
  .put("assets/logo.png", pngBytes)
  .delete("notes.txt")
  .send();
```

`context` is recorded immutably beside the commit and the token that made it
— see [audit & undo](audit-and-undo.md).

**Concurrency.** Pass `expectedParent` with the commit you built against. If
the branch has moved, `send()` throws a `WeftConflictError` whose
`currentTip` is where the branch is now:

```ts
import { WeftConflictError } from "@weftsh/sdk";

try {
  await repo.createCommit({ message: "step 13", expectedParent: seen }).put("a.txt", "x").send();
} catch (e) {
  if (!(e instanceof WeftConflictError)) throw e;
  seen = e.currentTip!; // rebase onto it and try again
}
```

Omit `expectedParent` to commit on whatever the tip is now; pass `null` to
require that the branch does not exist yet.

## Read

Any file at any revision — a full commit SHA, a branch, a tag or `HEAD`:

```ts
await repo.readFile("src/app.ts"); // string, or null
await repo.readFile("src/app.ts", { ref: oldCommit });

const file = await repo.getFile("assets/logo.png"); // bytes, binary flag, commit, etag
const again = await repo.getFile("assets/logo.png", { ifNoneMatch: file!.etag });
again!.notModified; // true — a 304 and no bytes
```

Directories, history and diffs:

```ts
await repo.getTree({ path: "src" }); // one level: name, kind, mode, oid
await repo.listFiles(); // every path, flat, in one request

const { commits, nextCursor } = await repo.listCommits({ limit: 50 });
await repo.listCommits({ path: "src/app.ts" }); // only commits that touched it

await repo.getDiff({ from: "v1.0.0", to: "main" }); // [{ path, status, oldOid, newOid }]
```

## Branch, tag and undo

```ts
await repo.createBranch({ name: "feature/login", from: "main" });
await repo.createTag({ name: "v1.0.0", target: "main" });
await repo.listBranches(); // sorted, with the default marked

// Put the branch back where it was before the agent went sideways.
await repo.reset({ to: goodCommit, expectedHead: badCommit });

// Or append a commit that undoes the head.
await repo.revert();
```

A reset erases nothing: the commits it leaves behind stay reachable by SHA
until garbage collection.

## Give a sandbox a git remote

Every repository is a real git remote. `getRemoteURL()` puts a **fresh
credential scoped to that one repository** into the URL, so a sandbox, a CI
job or a subprocess can clone and push without ever seeing your token:

```ts
const url = await repo.getRemoteURL(); // can push; dies in an hour
const readOnly = await repo.getRemoteURL({ access: "read", ttl: 600 });
```

```bash
git clone "$url" work && cd work
git commit -am "from the sandbox" && git push
```

The credential cannot reach any other repository, and a `read` one cannot
push. Minting it needs a client token with `org:read` and
`repo:write` (or `org:admin`); without them the SDK says so. To mint tokens directly, `weft.createToken({ scopes, repo, ttl })` does
the same with every option exposed.

## Verify webhooks

`verifyWebhook` checks a delivery's `X-Weft-Signature-256` against the raw
body and returns the event, or `null` — answer `null` with a `401`:

```ts
import { verifyWebhook } from "@weftsh/sdk";

export async function POST(request: Request) {
  const event = await verifyWebhook({
    payload: await request.text(),
    signature: request.headers.get("x-weft-signature-256"),
    secret: process.env.WEFT_WEBHOOK_SECRET!,
  });
  if (!event) return new Response("bad signature", { status: 401 });
  // event.event is "push", "change.landed" or "change.ejected"
  return new Response("ok");
}
```

Subscribe with `repo.createWebhook({ url })`, which returns the secret once.
What each event carries is in [webhooks](webhooks.md#the-events).

## Errors

Every failed request throws a `WeftError` with the HTTP `status` and the
server's own sentence as its `message`; a `409` from a concurrency check is
the `WeftConflictError` subclass. `findOne` returns `null` for a repository
that is not there, and `getFile` and `readFile` for a path or revision that is
not there; a file read from a repository that does not exist still throws.

The SDK never retries by itself — a write that failed on the way back may
still have happened. Pass your own `fetch` to decide what is safe to retry.

## Everything else

The SDK also covers forks, mirrors, repository settings, tokens and bundle
[export](export.md); the [README](https://github.com/weftsh/typescript-sdk#readme)
has every method. Anything the SDK does not wrap is in the REST API, described
in full by [`/openapi.json`](../openapi.json).
