# Mirror a GitHub repository

A mirror is a provably-fresh copy of your origin repository, kept on your
Spool server and served from your own object storage. You point your CI
at it, and agents can push to it: a push to the mirror is forwarded to
your origin first, so GitHub stays canonical and developers keep working
there exactly as before.

Like every repository on the server, a mirror is private to the
organization it is in, whatever the origin's visibility on GitHub.

## 0. Before you start

1. **An account and an organization** on your server
   ([Authentication](authentication.md)). The mirror is created in the
   organization, and everybody who should read it is a member.
2. **A GitHub App on the server**, for a private origin or for pushing
   through. Whoever runs the server creates it once and gives the server
   its id and key; after that, each organization connects its own
   installation from the dashboard. A server without one can still
   mirror an origin anybody can fetch, read-side only.
3. [Mint a token](authentication.md#minting-and-revoking-tokens) for
   your CI. It is the `$WEFT_TOKEN` in every example below.

## The short version: paste a URL

In the dashboard, **New repository → Mirror an existing one**, paste
`github.com/acme/widget`, and press *Check origin*.

- **An origin anybody can fetch** mirrors immediately. No credentials,
  no App to install — this is the whole flow.
- **A private one** answers *"this looks private"* and offers **Connect
  GitHub**. You install your server's GitHub App on the account, choose
  which repositories it may read, and come back to a list you pick from.
  The installation id is never shown or typed, and the connection is per
  organization — you do it once, not once per repository. Installing
  the App from GitHub's side first works too: you land on sign-in, and
  after signing in you choose the organization the installation belongs
  to.

The screen then follows the first sync — refs discovered, objects
ingested — and ends on the clone command. If the sync fails it says why,
on the same screen, instead of leaving a repository that just looks
broken.

The rest of this page is the same flow over the API, for CI and for
scripting. On GitHub Actions the switch is one step:
[`weftsh/checkout`](actions-checkout.md) replaces `actions/checkout`
and falls back to it, by name, when the mirror cannot serve the commit.

## Pushing to a mirror

A mirror made through the GitHub App forwards pushes to its origin:

```bash
git remote add spool https://spool.example.com/acme/widget.git
git push spool main
```

The push goes to GitHub first — under the App's installation, with the
value the mirror advertised as its lease, so nothing is ever forced —
and only once GitHub has taken it does the mirror serve it and answer
`ok`. A refusal is GitHub's, repeated per branch in the report:
a protected branch says `protected branch hook declined` with GitHub's
own explanation; a mirror that had fallen behind its origin says
`the mirror was behind its origin and has just caught up; fetch and
push again`. Nothing lands on the mirror that the origin did not take.

Two mirrors cannot push, and say so before you build a pack:

- A mirror made from a pasted URL, with no installation, has no
  credential — `this mirror has no credential that can push to its
  origin` — because it fetches as a stranger. Attach an installation from
  the repository page (or `PATCH …/repos/{repo}` with `installation_id`)
  and it forwards.
- An installation approved before the App asked for **`Contents: write`**
  is refused naming the permission, with the approve link on the
  repository page and in the picker.

The same holds over the API: `POST …/commits`, branches, tags, `reset`
and `revert` on a mirror land on the origin first and answer the shape
a native repository's do, with `409` and the current tip when the parent
you pinned has moved.

## 1. Check the origin first

A mirror registered against a typo answers `202` and then fails minutes
later, on a repo that looks broken. Ask first — it is one request and it
takes about as long as the round trip to your forge:

```bash
curl -X POST https://spool.example.com/v1/orgs/acme/origins/probe \
  -H "Authorization: Bearer $WEFT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "origin": "github.com/acme/widget" }'
```

```json
{ "reachable": true, "private": false,
  "default_branch": "main", "refs": 214, "reason": null }
```

`origin` takes whatever you have: a full git URL, `owner/repo`, a
`git@host:owner/repo` remote, or the browser URL with `/tree/main` still
on the end. It is normalised.

The answer is `200` whether or not the origin turned out to be reachable
— the *probe* worked either way, and the finding is in the body:

| Answer | What it means |
| --- | --- |
| `reachable: true` | Mirror it. `refs` and `default_branch` are what the probe saw. |
| `private: true` | It exists but wants credentials — connect GitHub and mirror it through the App. |
| `reachable: false`, `private: false` | `reason` says why: not a git repository, no such host, an origin the server will not fetch from. |

**What this endpoint will not do.** It fetches a URL you supply, so it is
`org:admin` only, `https` only, follows no redirects, refuses IP
addresses, and refuses any hostname that resolves to a private, loopback,
link-local or cloud-metadata address — checked on every address the name
answers with, not on the name. It is rate-limited per org. Creating a
mirror without an installation runs the same check, so an origin on your
own private network cannot be mirrored through it: a server that fetched
whatever a member typed would be a way into that network.

## 2. Register the mirror

With an org token (`repo:write` or admin), from a confirmed account:

```bash
curl -X POST https://spool.example.com/v1/orgs/acme/mirrors \
  -H "Authorization: Bearer $WEFT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "widget",
    "provider": "github",
    "origin": "acme/widget",
    "installation_id": "12345678"
  }'
```

`installation_id` is optional, and you can get one without ever reading a
number off a settings page — see [Connecting GitHub](#connecting-github)
below. Leave it out for an origin anybody can fetch. `provider` must be
one your server has: `github` exists when the server has a GitHub App,
and `generic` — any other git host, fetched from a URL — always does.

**Creation checks the origin.** Without an `installation_id`, the origin
is probed before anything is created, and an unreachable one answers
`422` with the probe attached rather than `202` and a failure minutes
later:

```json
{ "error": "that origin is not reachable as a git repository",
  "probe": { "reachable": false, "private": false, "refs": 0, "reason": "…" } }
```

With an `installation_id` the check is skipped — a private origin
refusing an anonymous probe is the expected answer, not a reason to
refuse creation.

Otherwise the response is `202 Accepted` and the initial ingest runs in
the background. Follow it (a 10 GB repo completes in well under 30
minutes):

```bash
curl -H "Authorization: Bearer $WEFT_TOKEN" \
  https://spool.example.com/v1/orgs/acme/repos/widget/sync-status
```

```json
{ "state": "syncing", "origin": "acme/widget", "commit": null, "error": null,
  "clone_url": "https://spool.example.com/acme/widget.git" }
```

`state` is `syncing` until the first sync finishes, then `ready`, or
`failed` with `error` saying why. A mirror that synced before and failed
since stays `failed`: it is serving stale content and somebody should
know.

For any other git host, use `"provider": "generic"` with a fetchable
`origin` URL. A `"public": true` in the body is refused with `400`: the
mirror is private to the organization either way.

## Connecting GitHub

A private origin needs a GitHub App installation. You connect one per
organization, and the id stays out of sight.

```bash
curl -X POST https://spool.example.com/v1/orgs/acme/github/install \
  -H "Authorization: Bearer $WEFT_TOKEN"
```

```json
{ "url": "https://github.com/apps/<your-app>/installations/new?state=stinst_…",
  "state": "stinst_…", "expires_in": 600 }
```

Open `url` in a browser and install the App. GitHub sends you back to
`/v1/github/setup` on your server, which binds the installation to the
organization that started the flow and redirects into the dashboard.

That `state` is the whole security of the round trip: GitHub's callback
carries no other proof of who began it, so the state is random,
single-use, expires in ten minutes, and is stored only as a hash. A
callback without a live one binds nothing — and every way of being wrong
answers the same, so it cannot be used to probe which flows exist.

Then list what you can mirror:

```bash
curl -H "Authorization: Bearer $WEFT_TOKEN" \
  https://spool.example.com/v1/orgs/acme/github/installations

curl -H "Authorization: Bearer $WEFT_TOKEN" \
  "https://spool.example.com/v1/orgs/acme/github/installations/4001/repos?per_page=100"
```

```json
{ "repositories": [
  { "full_name": "acme/widget", "private": true, "default_branch": "main",
    "description": "the fast one", "size": 16384 } ] }
```

`private` there is the repository's visibility on GitHub. Pass the
`full_name` as `origin` and the installation as `installation_id`, and
the mirror is created against a repository you know that installation
can read.

**An installation belongs to exactly one organization.** Another
organization asking about yours gets a `404`, and one trying to claim it
is refused — an installation is a key to somebody's source, and two
claimants would mean one organization reading another's code.

## 3. Install the webhook

Point your origin's push webhook at:

```
POST https://spool.example.com/webhooks/github
```

signed with the webhook secret your server was configured with. For a
GitHub App, that is the App's own webhook, set once when the App is
created. Pushes land on the mirror within seconds (p50 under 10 s); a
60-second poll is the loss-recovery floor, so a missed webhook never
strands the mirror.

## 4. Switch the CI checkout

```yaml
# before
- run: git clone https://github.com/acme/widget.git

# after
- run: git clone https://x:$WEFT_TOKEN@spool.example.com/acme/widget.git
```

Everything stock git does works: full clones, incremental fetches,
`--depth 1` (served from a precomputed snapshot). A push to the mirror can
never fork your write path: it is forwarded to the origin first (see
[Pushing to a mirror](#pushing-to-a-mirror)), or, on a mirror with no
credential that can push, refused with a message naming the origin.

## What you get

- **Provable freshness.** A fetch for a commit the mirror lacks triggers a
  synchronous origin sync before the response. See
  [the freshness contract](freshness-contract.md).
- **Outage behavior you can put in a runbook.** Origin down → last-known
  state serves, with `X-Weft-Staleness` on every response.
- **Numbers.** Per-repo clone p50/p99, bytes served, and requests
  absorbed at [`/v1/orgs/acme/repos/widget/metrics`](metrics.md), JSON
  or CSV.
