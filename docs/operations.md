# Operating spool

This is the reference for running a spool installation: what the server
needs, how it is configured, how to upgrade and back it up, and the
day-to-day operator tasks. For a deployment on AWS from the terraform in
this repository, read [deployment-aws.md](deployment-aws.md) first; for a
single machine, `docker-compose.yml` at the repository root is the whole
setup and this document is its reference.

## What runs

One binary, `stratum-server`, runs everything: git over smart HTTP v2 and
SSH, the REST API, the dashboard, and every background worker —
compaction, garbage collection, mirror sync, imports, forks, landing
changes, notifications, the workflow sweeper — as tasks inside the same
process. The same binary is the operator CLI (`stratum-server admin …`).

All repository data lives in an S3-compatible bucket and all control
state in PostgreSQL. A node keeps nothing that matters on local disk, so
you can run one node or ten behind a load balancer, replace any of them
at any time, and scale on load; in-flight requests are the only thing a
stopped node loses. Every node runs every worker; jobs are claimed
through the database, so two nodes never do the same job at once.

Self-hosted runners (`weft-runner`) are a separate program on machines
you choose. They connect out to the server; the server never connects to
them. See [Self-hosted runners](#self-hosted-runners).

## Requirements

| | |
|---|---|
| **PostgreSQL** | 16 is what the test suite and the AWS deployment run. One database; the server creates and migrates its own schema. Connections are made **without TLS**, so keep the database on a private network (see [Security notes](#security-notes)). |
| **Object store** | Any S3-compatible store that implements **conditional writes** — `If-None-Match: *` and `If-Match: <etag>` on `PutObject`, answering 412 (or 409) when they fail. Each repository's manifest changes only by compare-and-swap, and a store that ignores the condition will lose pushes under concurrency. Amazon S3 and MinIO do; check anything else before trusting it. Path-style bucket URLs (`https://host/bucket`) are what the client signs. |
| **git** | ≥ 2.30 on the server host (the server shells out to it for ingest, mirror sync and imports). The image includes it. Clients need git ≥ 2.26, which speaks protocol v2 by default. |
| **TLS** | The server speaks plain HTTP. Put a TLS terminator (a load balancer, a reverse proxy, CloudFront) in front of it. SSH carries its own encryption and needs nothing in front. |
| **Disk** | `STRATUM_DATA_DIR` holds scratch only — mirror seed clones, export staging, compaction scratch. Losable; size it for your largest mirrored repository times a few. |

## Running it

With Docker, on one machine:

```sh
./deploy/dev-host-key.sh                 # once: the SSH host key
docker compose up -d --build --wait
```

That builds the server image from this repository and starts it with
PostgreSQL and MinIO beside it; the comments at the top of
`docker-compose.yml` cover publishing it to other machines. The image is
also what the AWS deployment runs: `docker build -t spool .` at the
repository root.

Without Docker, the binary needs at least this:

```sh
export STRATUM_DB_URL=postgres://spool:…@db.internal:5432/spool
export STRATUM_STORE_URL=https://s3.us-east-1.amazonaws.com/acme-spool-store   # bucket URL
export AWS_ACCESS_KEY_ID=…  AWS_SECRET_ACCESS_KEY=…  AWS_REGION=us-east-1
export STRATUM_BIND=0.0.0.0:8080
export STRATUM_PUBLIC_URL=https://git.example.com        # what people type
export STRATUM_DATA_DIR=/var/lib/spool
export STRATUM_DASHBOARD_DIR=/opt/spool/dashboard        # web/dashboard/dist
stratum-server
```

`cargo build --release -p stratum-server` builds the binary, and
`npm ci && npx vite build` in `web/dashboard` builds the dashboard;
without `STRATUM_DASHBOARD_DIR` the server is API-only.

**Shutdown.** SIGINT and SIGTERM both stop the server gracefully: it
stops accepting connections and drains in-flight requests. Orchestrators
send SIGTERM and then SIGKILL after a timeout; make that timeout longer
than your slowest clone or push.

### The first organisation and the first person

A fresh install has nobody in it. Create an organisation and its first
owner with the CLI, wherever the binary can reach the database:

```sh
stratum-server admin bootstrap --org acme
stratum-server admin user-create --org acme --email you@example.com --password '…'
```

`bootstrap` prints an `org:admin` API token for the organisation — shown
once, stored only as a hash. `user-create` makes an account that owns the
organisation (`--role` picks another role), marks its address verified,
and gives it a personal namespace. Sign in to the dashboard with it and
invite everyone else from there; invitations are mailed, so configure
[mail](#mail) first or hand the invitation link over yourself.

With [single sign-on](#single-sign-on) there is nobody to invite: people
arrive through your identity provider. Make only the first owner, with
the address they sign in to the provider with and no password —
`user-create --org acme --email you@example.com --no-password` — and
their first SSO sign-in finds that account and keeps its role.

With the compose file, prefix each command with `docker compose exec
spool`; on AWS, use `deploy/admin-ecs.sh` (see [Operator CLI](#operator-cli)).

## Configuration reference

Everything is an environment variable. Unset means the default.

### Required

| Variable | Meaning |
|---|---|
| `STRATUM_DB_URL` | PostgreSQL URL for the control plane: organisations, people, repositories, tokens, reviews, workflow runs, audit, jobs. |
| `STRATUM_STORE_URL` | Bucket base URL. Every repository lives under `o/<org>/r/<repo>/` in it. |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_REGION` | SigV4 credentials for the store — static keys; the client does not read instance or task roles. |

### Addresses and listeners

| Variable | Default | Meaning |
|---|---|---|
| `STRATUM_BIND` | `127.0.0.1:8080` | HTTP listen address. The image sets `0.0.0.0:8080`. |
| `STRATUM_PUBLIC_URL` | `http://$STRATUM_BIND` | The base URL people and git clients use. Clone URLs, links in mail, and the GitHub App's callback URLs are built from it; set it to the address in front of your TLS terminator. |
| `STRATUM_DASHBOARD_DIR` | *(unset)* | Built dashboard (`web/dashboard/dist`) to serve at `/dashboard/` and at repository addresses. Unset = API only. The image sets it. |
| `STRATUM_DATA_DIR` | `stratum-data` | Local scratch: mirror seed clones, export staging. Losable — rebuilt on demand. |
| `STRATUM_SSH_BIND` | *(unset)* | Listen address for git over SSH, e.g. `0.0.0.0:2222`. Unset = SSH off. |
| `STRATUM_SSH_HOST_KEY` | *(required with `STRATUM_SSH_BIND`)* | The server's SSH host key, the PEM itself (`ssh-keygen -t ed25519`), not a path. |
| `STRATUM_SSH_PUBLIC_URL` | *(unset)* | The SSH base clients use, `ssh://git@host:port`, shown as the SSH clone URL. Unset = the dashboard shows HTTPS only. |
| `STRATUM_INSTANCE_ID` | *(fresh id at boot)* | This process's identity, answered on `/healthz` as `x-weft-instance`. Leave unset. |

### Mail

| Variable | Default | Meaning |
|---|---|---|
| `STRATUM_MAIL_TRANSPORT` | `null` | `null` (drop), `capture` (write to a directory), `smtp` or `ses`. Anything else refuses to boot. |
| `STRATUM_MAIL_FROM` | — | Sender address; required by `smtp` and `ses`. |
| `STRATUM_MAIL_DIR` | — | Directory for `capture`. |
| `STRATUM_MAIL_SMTP_HOST` | — | `host[:port]` (port 25 if omitted). |
| `STRATUM_MAIL_SMTP_USER` / `STRATUM_MAIL_SMTP_PASSWORD` | *(unset)* | Both or neither. |
| `STRATUM_MAIL_SMTP_ALLOW_CLEARTEXT_AUTH` | *(unset)* | `1` to allow credentials to a non-loopback relay; see [Mail](#mail). |
| `STRATUM_MAIL_SMTP_HELO` | `stratum` | Name sent in `HELO`. |
| `STRATUM_MAIL_SES_REGION` | `AWS_REGION` | SES region. |
| `STRATUM_MAIL_SES_ENDPOINT` | `https://email.<region>.amazonaws.com` | Override for a VPC endpoint. |
| `STRATUM_MAIL_SES_CONFIGURATION_SET` | *(unset)* | SES configuration set to send under. |

### GitHub App (mirrors, imports, signing in with GitHub)

| Variable | Default | Meaning |
|---|---|---|
| `STRATUM_GITHUB_APP_ID` | *(unset)* | Setting this enables the GitHub App: private GitHub mirrors, forwarding pushes to a mirror's origin, issue imports, Actions verdicts. |
| `STRATUM_GITHUB_APP_KEY` / `STRATUM_GITHUB_APP_KEY_PEM` | — | The App's private key: the PEM itself, or a path to it. |
| `STRATUM_GITHUB_WEBHOOK_SECRET` | `STRATUM_WEBHOOK_SECRET` | HMAC secret for `/webhooks/github`. |
| `STRATUM_GITHUB_CLIENT_ID` / `STRATUM_GITHUB_CLIENT_SECRET` | *(unset)* | The App's OAuth client, both or neither. Set, the install callback proves the person connecting an installation controls it, and signing in with GitHub works. Unset, the callback trusts the installation id it is given and sign-in with GitHub is off. |
| `STRATUM_GITHUB_INSTALL_URL` | *(unset)* | `https://github.com/apps/<slug>/installations/new`. What the dashboard's **Connect GitHub** button sends people to; unset, that button answers 501. |
| `STRATUM_GITHUB_API_BASE` / `STRATUM_GITHUB_GIT_BASE` / `STRATUM_GITHUB_OAUTH_BASE` | github.com | Overrides for GitHub Enterprise Server. |

### Single sign-on (OpenID Connect)

| Variable | Default | Meaning |
|---|---|---|
| `STRATUM_OIDC_ISSUER` | *(unset)* | The provider's issuer URL, exactly as its discovery document spells it. Setting this, the client id and the secret together turns single sign-on on; any one without the others refuses to boot. `https://` only, except to this machine. |
| `STRATUM_OIDC_CLIENT_ID` / `STRATUM_OIDC_CLIENT_SECRET` | — | The client you registered at the provider. |
| `STRATUM_OIDC_ORG` | *(required with SSO)* | The organisation everybody arriving by SSO for the first time joins. It need not exist at boot; a sign-in before it does ends at `sso=error` naming it. |
| `STRATUM_OIDC_ROLE` | `member` | Their role in it: `viewer`, `member`, `admin` or `owner`. |
| `STRATUM_OIDC_ALLOWED_DOMAINS` | *(unset)* | Comma-separated. Addresses at these domains are trusted without the provider's `email_verified`, and no other address gets in. Required for Google. |
| `STRATUM_OIDC_NAME` | `SSO` | What the sign-in button says after **Continue with**, at most 40 characters. |
| `STRATUM_OIDC_SESSION_HOURS` | `12` | How long a session begun through the provider lasts, 1–336. |
| `STRATUM_SSO_ONLY` | `true` with SSO | Whether SSO is the only way into the dashboard: password and GitHub sign-in are off. `false` keeps them alongside. `true` without SSO configured refuses to boot. |

### License key

| Variable | Default | Meaning |
|---|---|---|
| `STRATUM_LICENSE_ENDPOINT` | `https://license.weft.sh/v1/spool/check` | Where the daily license check goes. `https://` only, except to this machine. |
| `STRATUM_LICENSE_TICK_SECS` | `3600` | How often the check worker wakes to see whether a check is due. `0` turns the worker off. |
| `STRATUM_LICENSE_CHECK_SECS` | `86400` | How old the last check must be before the next. |
| `STRATUM_LICENSE_RETRY_MS` | `30000` | Between the check's attempts, when the service does not answer. |
| `STRATUM_DEV_MODE` / `STRATUM_DEV_LICENSE_PUBLIC_KEYS` | *(unset)* | For a license service in development only: `1`, and a JSON map of kid to PEM public key to trust beside the built-in ones. Without `STRATUM_DEV_MODE=1` the second refuses to boot. |

### Webhooks

| Variable | Default | Meaning |
|---|---|---|
| `STRATUM_WEBHOOK_SECRET` | *(empty)* | HMAC secret for the generic inbound receiver, `/webhooks/generic`. |

### Workflows and self-hosted runners

| Variable | Default | Meaning |
|---|---|---|
| `STRATUM_RUNNER_URL` | `STRATUM_PUBLIC_URL` | The base URL a job's clone URL is built on. Set it when runners reach the server at a different address than people do — an internal hostname, or around a CDN. |
| `STRATUM_RUNNER_MAX_TIMEOUT_MINUTES` | `360` | The largest `timeout-minutes` a workflow may ask for. More is refused when the run is triggered, not clamped. |
| `STRATUM_RUNNER_MAX_ATTEMPTS` | `2` | How many times a job may be handed to a runner in all, when runners vanish mid-job. |
| `STRATUM_RUNNER_CLAIM_WAIT_MS` | `20000` | How long a runner's claim call waits for work before answering "nothing" (a long poll). |
| `STRATUM_RUNNER_START_LEASE_SECS` | `600` | How long a claimed job may take to report that it started. |
| `STRATUM_RUNNER_OVERDUE_SLACK_SECS` | `300` | Grace past a job's timeout before the sweeper fails it. |
| `STRATUM_RUNNER_POLL_SECS` | `5` | How often the sweeper settles jobs whose runner went away. `0` disables it on that node. |

### CDN offload for clones (optional)

Opted-in git clients (`git -c fetch.uriprotocols=https clone …`) can be
handed a signed URL for a repository's bulk pack and fetch it from a CDN
instead of the server. The AWS deployment turns this on with CloudFront;
[deployment-aws.md](deployment-aws.md#cdn-offload) has the trade-offs.

| Variable | Default | Meaning |
|---|---|---|
| `STRATUM_CDN_BASE` | *(unset)* | CDN base URL for offloaded packs. Unset = offload off, never advertised. |
| `STRATUM_CDN_ENABLED` | `1` | Kill switch: `0` stops advertising packs without a redeploy. Clones keep working, served inline. |
| `STRATUM_CDN_KEY_PAIR_ID` | *(unset)* | CloudFront public key id, when the CDN fronts the **bucket**. Needs the private key too. |
| `STRATUM_CDN_PRIVATE_KEY` / `STRATUM_CDN_PRIVATE_KEY_PEM` | — | CloudFront signing key (RSA-2048): the PEM itself, or a path. |
| `STRATUM_CDN_ORIGIN_SECRET` | *(unset)* | HMAC secret, when the CDN fronts **this server**: enables `GET /v1/orgs/:org/repos/:repo/cdn/:pack`. Mutually exclusive with `STRATUM_CDN_KEY_PAIR_ID`. |
| `STRATUM_CDN_URL_TTL_SECS` | `3600` | Lifetime of a pack URL. Must comfortably exceed a slow clone: git does not fall back to the server for an advertised pack. |
| `STRATUM_CDNPACK_POLL_SECS` | `60` | How often the pack builder looks for repositories whose pack has fallen behind. `0` disables. |

### Storage maintenance

| Variable | Default | Meaning |
|---|---|---|
| `STRATUM_GC_SECS` | `0` (off) | Interval of epoch garbage collection and deleted-repository sweeps. Off when unset, so superseded epochs and deleted repositories stay in the bucket; `docker-compose.yml` and the Terraform deployment set `3600` (`SPOOL_GC_SECS`, `gc_interval_secs`). |
| `STRATUM_GC_GRACE_SECS` | `86400` | Age an unreferenced epoch must reach before it is deleted. Keep it longer than your longest clone and your longest compaction. |
| `STRATUM_COMPACT_POLL_SECS` | `5` | How often the compactor looks for repositories over the WAL thresholds. |
| `STRATUM_COMPACT_SWEEP_SECS` | `3600` | How often every repository is checked for a fold nothing asked for. |
| `STRATUM_STORAGE_SWEEP_SECS` | `3600` | How often each repository's stored size is re-derived from its manifest. |
| `STRATUM_STORAGE_INVENTORY_SECS` | `86400` | How often the bucket itself is listed to compare physical bytes with logical ones — how a leak or a stuck GC gets noticed. |
| `STRATUM_AUDIT_SHIP_SECS` | `3600` | How often each organisation's new audit rows are written to the bucket as write-once JSONL (`o/<org>/audit/`). |

### Reads, mirrors and background work

| Variable | Default | Meaning |
|---|---|---|
| `STRATUM_READ_CACHE_MB` | `256` | Memory the read API may spend on objects, WAL sidecars and packs across requests. Everything cached is immutable, so a hit is never stale. `0` caches nothing. |
| `STRATUM_MIRROR_POLL_SECS` | `60` | How often mirrors poll their origin (`git ls-remote`) — the backstop for a lost webhook. `0` disables. |
| `STRATUM_FRESHNESS_TIMEOUT_SECS` | `8` | Bound on the synchronous origin sync a read of a stale mirror may trigger. |
| `STRATUM_IMPORT_POLL_SECS` / `STRATUM_IMPORT_PAGES_PER_RUN` | `30` / `20` | GitHub import worker: poll interval, and API pages per pass. |
| `STRATUM_CHECKS_POLL_SECS` / `STRATUM_CHECKS_PAGES_PER_RUN` | `30` / `5` | Polling GitHub Actions for verdicts on mirrored repositories. |
| `STRATUM_LAND_POLL_SECS` / `STRATUM_LAND_WAIT_SECS` / `STRATUM_LAND_RECHECK_SECS` | `2` / `1800` / `20` | The lander, which lands approved changes onto their target branch: its poll interval, how long a change may wait on required checks before it is ejected, and how often a waiting change is re-checked. Raise the wait if your CI takes longer than half an hour. |
| `STRATUM_USAGE_ROLLUP_SECS` | `3600` | Folding request and byte counters into the per-day usage the dashboard draws. `0` disables. |
| `STRATUM_SIGNALS_ROLLUP_SECS` | `300` | Per-repository daily counters behind the insights pages. |
| `STRATUM_DB_LOCK_TIMEOUT_MS` | `5000` | Session `lock_timeout`: a write that would wait on another session's lock fails after this instead of hanging a request. |
| `STRATUM_JOB_MAX_ATTEMPTS` | `5` | How many times a background job is tried before it is given up. |
| `STRATUM_TREE_HISTORY_BUDGET_MS` | `2500` | How long a directory listing may spend finding each entry's last commit before it answers with what it has. |

Every worker also has a `…_POLL_SECS` and a `…_LEASE_SECS` (fork,
promote, notify, changeset notify, commit authorship, CDN pack, compact,
import, checks, land). `0` as a poll interval disables that worker on
that node; a lease is how long a crashed node's claim blocks a job
before another node takes it. The defaults are right for almost
everyone. `STRATUM_REF_PAGE_SIZE`, `STRATUM_REF_PAGE_MAX`,
`STRATUM_MANIFEST_TIER`, `STRATUM_LAYOUT`, `STRATUM_LOCATOR_TIER`,
`STRATUM_LATENCY_MODEL`, `STRATUM_COMMIT_COUNT_CAP`,
`STRATUM_LOG_SCAN_BUDGET`, `STRATUM_META_WALK_*`,
`STRATUM_TREE_RECURSIVE_CAP` and `STRATUM_CONTRIB_MAX_VISITS` are engine
limits and benchmarking knobs; leave them alone unless a support
conversation says otherwise.

## Upgrades and migrations

The schema migrates itself. Every process that opens the database —
the server at boot, and every `admin` command — takes a PostgreSQL
advisory lock, applies any migrations it knows and the database has not
seen, records them in `schema_migrations`, and releases the lock. Nodes
starting together queue on the lock; nothing needs to be run by hand.

To upgrade:

1. **Back up the database** (below). Migrations are forward-only; there
   is no down-migration, and an older binary is not guaranteed to run
   against a newer schema.
2. Read the release notes for anything that needs doing first.
3. Roll the new image. A rolling replacement is fine: the first new node
   migrates, and the old nodes keep serving meanwhile. If a migration
   fails the new node refuses to start and says which migration and why;
   the old nodes are unaffected.

To roll back, restore the database backup taken in step 1 and run the
old image. Repository data needs no rollback: the bucket's format is
versioned and written append-only.

## Backup and restore

Two things hold all state.

1. **The bucket.** Immutable segments, WAL entries, epochs, and one
   `manifest.json` per repository that changes only by compare-and-swap.
   Use your store's replication or versioning to protect it. Note that
   compaction and GC delete superseded objects by design, so versioning
   keeps every one of them at full price; replication to a second bucket
   is usually the better fit.
2. **The database.** `pg_dump`/`pg_basebackup`, or your provider's
   point-in-time recovery (the AWS deployment keeps 7 days of Aurora
   backups).

Take them together and restore them together: a database from Monday and
a bucket from Friday disagree about which repositories and refs exist.
Of the two, the bucket is the source of truth for repository contents
and the database for everything around them.

A new node pointed at a restored pair serves everything. `STRATUM_DATA_DIR`
needs no backup — mirror seed clones rebuild on the next sync.

Also keep: the SSH host key (a new one makes every client refuse to
connect until it is re-trusted), and the GitHub App's private key.

## Git over SSH

Set `STRATUM_SSH_BIND`, `STRATUM_SSH_HOST_KEY` and
`STRATUM_SSH_PUBLIC_URL`, publish the port, and the same server answers
`git clone ssh://git@host:port/org/repo.git`. Authentication is public
key only: a person adds a key under their account settings (or `POST
/v1/orgs/{org}/ssh-keys` against a token) and it authenticates as that
account or token, with the same permissions and the same instant
revocation — every connection is checked against the database.

The host key must be **the same on every node and across restarts**. A
key generated per boot looks like a man-in-the-middle to every client.
Generate it once (`ssh-keygen -t ed25519 -N "" -f spool_host_key`),
store it as a secret, and give every node the same one. The AWS
deployment generates one and keeps it in Secrets Manager.

SSH needs no certificate or domain, which makes it a fully encrypted git
transport even on a deployment that has no TLS name yet.

## Mail

Mail carries invitations, password resets, confirmations of the extra
addresses people add to their accounts, and notifications. With the
default `null` transport it is dropped, and the dashboard says so where
it matters: an invitation's link is also shown to the admin who made it,
to deliver by hand, and people you create with `admin user-create` need
no mail at all.

- **`smtp`** — `STRATUM_MAIL_SMTP_HOST`, `STRATUM_MAIL_FROM`, and
  optionally `STRATUM_MAIL_SMTP_USER`/`_PASSWORD`. This build has **no
  STARTTLS**, so it refuses to send credentials to anything but a
  loopback relay unless `STRATUM_MAIL_SMTP_ALLOW_CLEARTEXT_AUTH=1` says
  the link is already private. The usual shape is a local relay (Postfix,
  or a sidecar) that holds the upstream credentials and speaks TLS
  onwards.
- **`ses`** — Amazon SES over HTTPS, signed with the same
  `AWS_ACCESS_KEY_ID` as the store, so that credential needs
  `ses:SendEmail`. `STRATUM_MAIL_FROM` must be an address or domain SES
  has verified. A new AWS account is in the SES sandbox and can only send
  to verified addresses until you request production access.
- **`capture`** — writes each message to `STRATUM_MAIL_DIR`. For testing.

## GitHub App

Mirrors of private GitHub repositories, forwarding a push made to a
mirror on to its GitHub origin, importing issues from GitHub, recording
GitHub Actions verdicts on mirrored repositories, and signing in with
GitHub all go through **one GitHub App that you create** and own.
Without one, spool is a complete forge and still mirrors public
repositories from any git host over HTTPS; those five features are off.

Create it under your organisation's **Settings → Developer settings →
GitHub Apps → New GitHub App**, with `PUBLIC` below standing for your
`STRATUM_PUBLIC_URL`:

| Setting | Value |
|---|---|
| Homepage URL | `PUBLIC` |
| Callback URLs | `PUBLIC/v1/github/setup` **and** `PUBLIC/v1/auth/github/callback` |
| Request user authorization (OAuth) during installation | on |
| Setup URL | `PUBLIC/v1/github/setup`, with **Redirect on update** on |
| Webhook URL | `PUBLIC/webhooks/github`, active, with a secret you generate |
| Repository permissions | **Contents: Read and write** (write is what forwards a push made to a mirror to its origin), **Workflows: Read and write** (a forwarded push that changes the origin's `.github/workflows/`), **Metadata: Read**, **Issues: Read** (imports), **Actions: Read** (verdicts of GitHub Actions runs on mirrored repositories) |
| Account permissions | **Email addresses: Read** (signing in with GitHub) |
| Subscribe to events | **Push** |
| Where can this App be installed | Only on this account — unless the organisations your people mirror from are elsewhere |

Then generate a **private key** and a **client secret** on the App's
page, and configure the server:

```sh
STRATUM_GITHUB_APP_ID=123456
STRATUM_GITHUB_APP_KEY="$(cat acme-spool.private-key.pem)"
STRATUM_GITHUB_WEBHOOK_SECRET=…            # the webhook secret you chose
STRATUM_GITHUB_CLIENT_ID=Iv1.…
STRATUM_GITHUB_CLIENT_SECRET=…
STRATUM_GITHUB_INSTALL_URL=https://github.com/apps/acme-spool/installations/new
```

On AWS, those five values go into the `<project>/<env>/github-app`
secret and the slug into `github_app_slug`; see
[deployment-aws.md](deployment-aws.md#8-the-github-app-optional).

Two mistakes are easy to make quietly:

- **A missing callback URL.** GitHub refuses a `redirect_uri` it has not
  been told about, and says so on its own error page, not ours. Both
  callback URLs above are needed: one for connecting an installation,
  one for signing in.
- **A missing Email addresses permission.** `GET /user/emails` answers
  403, the server cannot see which address GitHub has verified, and
  every sign-in ends at `github=noemail` — which reads as if the person's
  GitHub account were at fault. It is an *account* permission, consented
  to by each person when they sign in, so adding it later does not make
  existing installations re-approve anything.

Adding a permission to the App later (say, `Contents: write` after
starting with read) does not reach existing installations until each
installation's owner approves it on GitHub. Until then pushes to those
mirrors are refused with a message naming the missing permission and a
link to approve it.

## Single sign-on

Point the server at the identity provider your company already runs —
Okta, Entra ID, Google Workspace, Keycloak, or anything else that speaks
OpenID Connect — and people sign in with it. **Anybody the provider signs
in gets an account** on their first visit, in the organisation
`STRATUM_OIDC_ORG` names at the role `STRATUM_OIDC_ROLE` names: who may
use the forge is decided where you already decide who works for you, by
assigning the application to people or groups at the provider. A
provider that lets anybody register themselves — a Keycloak realm with
self-registration, a Google issuer without a domain list — is a forge
anybody can join, so restrict the application to your people there, and
set `STRATUM_OIDC_ALLOWED_DOMAINS` as a second fence. What signing in
looks like for the person is in
[guide/authentication.md](guide/authentication.md#single-sign-on).

Register a **web application** (a confidential client, authorization
code flow) with, `PUBLIC` standing for your `STRATUM_PUBLIC_URL`:

| Setting | Value |
|---|---|
| Redirect (callback) URI | `PUBLIC/v1/auth/sso/callback` |
| Scopes | `openid email profile` |
| Token endpoint authentication | client secret, Basic or POST — the server uses whichever the provider's discovery offers, Basic first |
| ID token signing | RS256 (every provider's default) |

and configure the server:

```sh
STRATUM_OIDC_ISSUER=https://acme.okta.com
STRATUM_OIDC_CLIENT_ID=0oa…
STRATUM_OIDC_CLIENT_SECRET=…
STRATUM_OIDC_ORG=acme
STRATUM_OIDC_NAME=Okta
```

Then, with the same environment the server runs with (`docker compose
exec spool …`, or `deploy/admin-ecs.sh` on AWS), ask the provider
everything that needs no person at a browser:

```sh
stratum-server admin sso-check
```

It fetches discovery and the signing keys through the server's own
checks, looks up the organisation, and trades a made-up code at the
token endpoint: `invalid_grant` back means the provider took the client
credentials and refused only the code, `invalid_client` that it refused
the credentials. One JSON line names each part and whether it passed,
and the exit status is non-zero when any did not.

The provider is found and checked when somebody first signs in, not at
boot, so a provider that is briefly down does not stop the server
starting; the sign-in screen's button answers `sso=error` until it is
back, and the log says why. So does a secret the provider refuses —
every sign-in would fail the same way, so it is not reported to the
person as a round trip to start again.

Per provider:

- **Okta** — the issuer is your org URL, `https://<you>.okta.com`, or a
  custom authorization server's, `https://<you>.okta.com/oauth2/default`.
  Okta puts `email` and `email_verified` in the ID token for the `email`
  scope, which is all the server needs.
- **Entra ID** — the issuer is your tenant's,
  `https://login.microsoftonline.com/<tenant-id>/v2.0`. The shared
  endpoints (`common`, `organizations`, `consumers`) are refused at boot:
  they sign in any Microsoft account. Entra sends no `email_verified`
  and, by default, no `email` in the ID token (the server asks userinfo
  for it), so set `STRATUM_OIDC_ALLOWED_DOMAINS` to your tenant's
  domains, or nobody new can be given an account. That tells the server
  to trust your tenant's administrators for those addresses, which is
  who sets them.
- **Google Workspace** — the issuer is `https://accounts.google.com`,
  which is every Google account on earth, so the server refuses to boot
  without `STRATUM_OIDC_ALLOWED_DOMAINS`, and admits only accounts whose
  Workspace (`hd`) is one of those domains — a personal Gmail account
  whose address happens to be at your domain is not one of yours.
- **Keycloak** — the issuer is the realm's,
  `https://<host>/realms/<realm>`. Keycloak says `email_verified: false`
  for an address nobody has confirmed, and the server believes it
  whatever the domain list says, so turn on *Verify email* for the realm
  or have an administrator mark addresses verified.

**Existing accounts** — made by `user-create` or an invitation before SSO
was switched on — are found by address the first time their owner signs
in through the provider, and keep every role they had. After that the
account is tied to the provider's own id for the person (its `sub`), not
the address.

**Only SSO, by default.** With SSO configured, password sign-in,
forgotten-password links, changing a password and signing in with
GitHub are all off: otherwise somebody switched off at the provider
could keep signing in with a password they still know.
`STRATUM_SSO_ONLY=false` keeps them alongside SSO.

**Break glass.** When the provider is down or misconfigured and nobody
can sign in, restart the server with `STRATUM_SSO_ONLY=false`; every
account that has a password can use it again. An account made by SSO
has none — `forgot-password` gives it one, or make a separate
break-glass owner with `user-create --password …` ahead of time and keep
its password where your other emergency credentials live. Put
`STRATUM_SSO_ONLY` back when you are done.

**Offboarding with SSO.** Switching somebody off at the provider stops
their next sign-in, and a session they already hold ends within
`STRATUM_OIDC_SESSION_HOURS` (twelve by default) — sessions begun before
SSO was switched on keep their fourteen days. It does **not** reach
their personal access tokens or SSH keys: those are how `git` and
scripts sign in, and the provider is never asked about them. Run
[`user-disable`](#offboarding) as part of the same checklist; it ends
all of it at once.

## License key

A Weft license key covers access to Spool's releases and security
patches for the organisation that bought it. **It never stops the
server.** No feature is behind a tier, and nothing is refused or slowed
when a key expires, lapses or is revoked, or when more people use the
server than it covers. What it produces is a sentence for you.

Install the key from the email with the operator CLI (`docker compose
exec spool …`, or `deploy/admin-ecs.sh` on AWS):

```sh
stratum-server admin license-install 'weft_lic_v1.…'
stratum-server admin license-status
```

`license-install` verifies the key first and refuses one this build
cannot read, saying why. `license-status` prints the key's entity, tier,
expiry and limit, the number of **people** the server has — accounts
that are not switched off, which is what a Spool license counts — and
what the license service last said. `license-remove` removes it.

Once a day an online key checks in with Weft's license service. The
check sends exactly three fields, and nothing else — no hostnames,
addresses, repository or organisation names, or anything about who the
people are:

```json
{ "keyId": "lic_…", "version": "0.1.0", "people": 42 }
```

The answer — `active`, `lapsed` or `revoked`, and sometimes a notice —
is logged and shown by `license-status`. A refusal is not retried until
the next day; no answer is retried twice. With several nodes, one of
them checks, once a day. `stratum-server admin license-check` checks
now. An offline key (enterprise) never calls out.

## Self-hosted runners

`.weft/` workflows run on runners you register: the `weft-runner`
program on machines of your choosing. A runner makes outbound HTTPS
calls to `STRATUM_RUNNER_URL` (by default the public URL) and nothing
else — it listens on no port and needs no inbound rule. A job with no
`runs-on` runs on any runner of the organisation whose group admits the
repository; labels narrow it.

1. Build the runner: `cargo build --release -p stratum-runner` (the
   binary is `target/release/weft-runner`), or the container image,
   `docker build -f Dockerfile.runner -t weft-runner:local .`.
2. An organisation admin mints a registration token under **Settings →
   Runners → Add a runner** (single use, valid one hour).
3. On the runner machine:

   ```sh
   weft-runner register --url https://git.example.com --token weftg_… \
     --name build-01 --labels linux,x64 --dir /var/lib/weft-runner
   weft-runner run --dir /var/lib/weft-runner
   ```

   or, in a container, the two commands at the top of `Dockerfile.runner`.

A runner executes shell commands from repositories as whatever user it
runs as. Run it as an unprivileged user on a machine you would not mind
a build having touched, and use `--ephemeral` with a fresh machine per
job where builds come from people you do not fully trust.
[guide/self-hosted-runners.md](guide/self-hosted-runners.md) covers
systemd, ephemeral runners, network needs and isolation in detail.

Runners that stop calling in are marked offline and removed after 14
days (1 day for ephemeral ones). Removing a runner in the dashboard makes
the process exit with status 2 on its next call.

## Operator CLI

The server binary is also the admin tool. It connects to `STRATUM_DB_URL`
(and migrates the schema, like the server does), so run it anywhere
that can reach the database. Each command prints one JSON line.

```sh
stratum-server admin bootstrap --org NAME            # organisation + org:admin token
stratum-server admin mint --org NAME --scopes repo:read[,repo:write,org:read,org:admin] \
    [--repo NAME] [--label L]                        # an API token
stratum-server admin user-create --org NAME --email ADDR \
    (--password SECRET | --no-password) \
    [--name N] [--role R] [--handle H]               # an account, added to the org
stratum-server admin user-disable --email ADDR       # offboarding
stratum-server admin user-enable  --email ADDR       # …and undoing it
stratum-server admin repair-identities [--dry-run]   # accounts missing a handle
stratum-server admin sso-check                       # ask the SSO provider, before anybody signs in
stratum-server admin license-install KEY             # the Weft license key, verified first
stratum-server admin license-status                  # what it covers, and what Weft last said
stratum-server admin license-check                   # check in with Weft now
stratum-server admin license-remove
```

- **Where to run it.** In the compose stack, `docker compose exec spool
  stratum-server admin …`. On the AWS deployment, `deploy/admin-ecs.sh
  <command> [flags…]` runs it as a one-off Fargate task with the
  service's own image, secrets and network, and prints the result from
  the task's log (`STRATUM_PROJECT`/`STRATUM_ENV` pick the environment).
- **Tokens** are shown once at mint and stored only as SHA-256 hashes.
  Revoking one (`DELETE /v1/orgs/{org}/tokens/{id}`, or the dashboard) is
  immediate; there is no verification cache.
- **`user-create`** is one of the ways an account is made — the others
  are an invitation and, when it is configured, single sign-on; nobody
  signs themselves up. On an address that already has an account it adds
  the membership instead of failing. It derives a handle from the
  address (`dev.eloper@` becomes `dev-eloper`); `--handle` picks another
  when that one is taken. `--no-password` makes an account that signs in
  only through [single sign-on](#single-sign-on) (or after a reset link);
  one of it and `--password` is required, so an account nobody can sign
  in to is never made by leaving a flag out.
- **`repair-identities`** gives a handle and personal namespace to any
  account made without one (by an older `user-create`). It names every
  account it could not repair and why; nothing is changed with
  `--dry-run`.

### Offboarding

`user-disable` ends someone's access everywhere, and nothing has to be
revoked by hand: bearer tokens and browser sessions check the flag on
every request, and SSH keys stop authenticating through the same check.
It is deliberately not a delete — the audit trail names people by id —
so memberships are kept and `user-enable` restores exactly what they had.
Deploy keys and organisation service tokens belong to nobody and keep
working. A disabled account cannot sign in through
[single sign-on](#single-sign-on) either: the provider still vouching
for the person does not switch the account back on.

## Health, readiness, metrics

- `GET /healthz` — liveness. Always 200 while the process runs, with the
  instance id in `x-weft-instance`. Point load-balancer health checks
  here.
- `GET /readyz` — readiness: the database answers a query **and** the
  store answers a LIST; 503 names the failing dependency. It costs a
  query and a LIST per call, so use it for deploy gating, not
  high-frequency polling.
- `GET /metrics` — Prometheus text: request counts, bytes, latency
  histograms per route class. It is **unauthenticated**; do not expose it
  to the internet (the AWS deployment's load balancer answers 404 for
  it). Scrape nodes directly.

## Reads on a long WAL

Every accepted write — a push, a REST commit, a mirror sync — appends
one entry to the repository's write-ahead log in its manifest, and the
compactor folds the log into a fresh epoch once it holds 8 entries or
16 MB. Reads consult the log first, because a just-written object exists
nowhere else, so the log's length is the read API's fixed cost: a
repository whose log had grown past a hundred entries took seconds to
show a three-byte file, while clones were unaffected.

If the code browser is slow on one repository and not another, look at
that repository's `manifest.json` in the bucket (`o/<org>/r/<repo>/…`)
for the length of `wal` before anything else. A log well past the
threshold means the `compact` job is not being enqueued or not being
claimed; the `jobs` table in the database says which.

## Security notes

- **Accounts.** Nobody can make their own account. An account comes
  from an organisation admin's invitation, accepted by the person it
  was mailed to, from `admin user-create`, or — when you configure
  [single sign-on](#single-sign-on) — from your identity provider
  signing the person in, which makes the provider's list of who may use
  this application the list of who has an account here. Signing in with
  GitHub only reaches an account that already exists. A signed-out visitor
  cannot tell which organisations or people exist: a name nobody holds
  answers exactly as one they may not see. The sign-in page itself is
  still reachable by anybody who can reach the server, so keep it on a
  private network if that is not acceptable.
- **Database.** The server connects to PostgreSQL without TLS. Keep the
  database on a network only the server can reach (the compose file and
  the AWS deployment both do).
- **Metrics** are unauthenticated; see above.
- **Runners** run repository code; see [Self-hosted runners](#self-hosted-runners).

## Service limits

- 64 MB request cap: a larger push answers 413; split it.
- About 16 concurrent pushers per repository; more wait their turn.
- Reads of just-pushed objects scan the WAL until compaction folds it.
- SHA-1 repositories only (the formats are versioned for SHA-256 later).
