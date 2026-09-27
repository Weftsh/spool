# Operating stratum-server

One binary runs everything: git smart HTTP v2, the REST API, the control
plane, and every background worker (compactor, epoch GC, mirror poller,
export runner, billing rollup, audit shipper, webhook dispatch) as tokio
tasks. Nodes are stateless — all repo data lives in object storage, all
control state in PostgreSQL — so a node can be replaced at any time;
in-flight requests are the only thing lost.

## Minimum viable deployment

```sh
export STRATUM_STORE_URL=https://s3.example.com/stratum-prod   # bucket URL
export AWS_ACCESS_KEY_ID=…  AWS_SECRET_ACCESS_KEY=…  AWS_REGION=us-east-1
export STRATUM_DB_URL=postgres://stratum@db.internal:5432/stratum
export STRATUM_DATA_DIR=/var/lib/stratum/data                  # mirror seed clones
export STRATUM_BIND=0.0.0.0:8080
export STRATUM_PUBLIC_URL=https://git.example.com              # clone URLs in responses

stratum-server admin bootstrap --org acme    # → org + org:admin token (once)
stratum-server                               # serve
```

Put a TLS terminator in front; the binary speaks plain HTTP. `git` needs
≥ 2.30 on the server host (ingest and mirror sync shell out to it).

Shutdown: SIGINT and SIGTERM both trigger graceful shutdown — the server
stops accepting connections and drains in-flight requests. Orchestrators
(ECS, Kubernetes) send SIGTERM on stop, then SIGKILL after their stop
timeout; size that timeout for your longest expected clone.

### Git over SSH

Set `STRATUM_SSH_BIND` (plus `STRATUM_SSH_HOST_KEY` and, for clone URLs
in API responses, `STRATUM_SSH_PUBLIC_URL`) and the same binary also
answers `git clone ssh://git@host:port/org/repo`. Auth is publickey
only: a key is registered against a token (`POST
/v1/orgs/{org}/ssh-keys`, or the dashboard's SSH keys panel) and
authenticates as that token's principal — same scopes, same instant
revocation, checked against the database on every connection. SSH needs
no TLS terminator or domain certificate at all, which makes it the
fully-encrypted git transport on day one of a deployment that has no
domain yet. Fetches require protocol v2 (git ≥ 2.26 sends it by
default), matching the HTTP door.

## Configuration reference

| Variable | Default | Meaning |
|---|---|---|
| `STRATUM_STORE_URL` | *(required)* | S3-compatible bucket base URL. Every repo lives under `o/<org>/r/<repo>/prod/` in this bucket. |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_REGION` | *(required)* | SigV4 credentials for the store. |
| `STRATUM_DB_URL` | *(required)* | PostgreSQL connection URL for the control plane (registry, tokens, audit, metrics, jobs). The only state besides the bucket. |
| `STRATUM_DB_LOCK_TIMEOUT_MS` | `5000` | Session `lock_timeout`: writes that would wait on another session's lock fail after this instead of hanging a request. |
| `STRATUM_DATA_DIR` | `stratum-data` | Local scratch: bare seed clones for mirrors, export staging. Losable — rebuilt on demand. |
| `STRATUM_BIND` | `127.0.0.1:8080` | Listen address. |
| `STRATUM_INSTANCE_ID` | *(fresh id at boot)* | This process's identity, answered on `/healthz` as `x-weft-instance`. Leave unset in a fleet; the test harness sets one per spawn to prove the server it reaches is the one it started. |
| `STRATUM_PUBLIC_URL` | `http://$STRATUM_BIND` | Base URL written into `clone_url` fields. |
| `STRATUM_FRESHNESS_TIMEOUT_SECS` | `8` | Bound on the synchronous origin sync a freshness miss may trigger (Mirror M2). |
| `STRATUM_MIRROR_POLL_SECS` | `60` | Origin drift poll (`git ls-remote`) — webhook-loss recovery. `0` disables. |
| `STRATUM_TREE_HISTORY_BUDGET_MS` | `2500` | How long one `tree?history=1` listing may spend walking history for its last-commit column before answering with what it has (`history_truncated: true`). Enforced per object read, on the reader: a single commit's tree diff is dozens of store round trips, and a budget checked only between commits let the first diff on a real mirror run three minutes past it into the edge's 60 s timeout. Reads the process cache answers are not charged, so a warm walk finishes in memory. `0` walks nothing and answers truncated at once. |
| `STRATUM_READ_CACHE_MB` | `256` | Memory the read API may spend remembering objects, WAL sidecars and WAL packs across requests. Everything cached is content-addressed or written once, so a hit is never stale; the only policy is this budget, spent oldest-first. `0` remembers nothing. See *Reads on a long WAL* below. |
| `STRATUM_COMPACT_POLL_SECS` | `5` | How often the compactor looks for repos over the WAL thresholds. |
| `STRATUM_GC_SECS` | `0` (off) | Epoch/deleted-repo GC interval. |
| `STRATUM_GC_GRACE_SECS` | `86400` | Age an unreferenced epoch must reach before deletion. Keep this longer than your longest clone or compaction. |
| `STRATUM_AUDIT_SHIP_SECS` | `3600` | Audit-log JSONL batch shipping to the bucket (write-once immutability copy). |
| `STRATUM_BILLING_ROLLUP_SECS` | `3600` | How often served bytes and finished minutes are rolled into the org's meters and, where metered prices are configured, reported to the payment provider. `900` recommended on a fleet that sells: a meter that trails a large clone by an hour lets an organization run an hour past its spend limit. |
| `STRATUM_STORAGE_SWEEP_SECS` | `3600` | How often private storage is summed from each repository's `stored_bytes` into the storage meter. |
| `STRATUM_STORAGE_INVENTORY_SECS` | `86400` | How often that sum is reconciled against the object store itself, so a repository whose `stored_bytes` drifted from what the bucket holds is corrected within a day. |
| `STRATUM_FREE_TIER_REPOS` | `10000` | Repo cap on the free plan; writes past it answer 402, reads keep working. |
| `STRATUM_WEBHOOK_SECRET` | *(empty)* | HMAC secret for the generic inbound `/webhooks/generic` receiver. |
| `STRATUM_GITHUB_APP_ID` | *(unset)* | Setting this enables the GitHub App origin provider. The App must hold `Contents: write` on repositories for a mirror to forward pushes to its origin; an installation that predates the permission has its pushes refused naming it, with the approve link on the repository page. |
| `STRATUM_GITHUB_APP_KEY_PEM` / `STRATUM_GITHUB_APP_KEY` | — | App private key: path to a PEM file, or the PEM inline. |
| `STRATUM_GITHUB_WEBHOOK_SECRET` | falls back to `STRATUM_WEBHOOK_SECRET` | HMAC secret for `/webhooks/github`. |
| `STRATUM_GITHUB_CLIENT_ID` / `STRATUM_GITHUB_CLIENT_SECRET` | *(unset)* | The App's OAuth client, both or neither. Set, the install callback exchanges the `code` GitHub appends (the App must request user authorization during installation) and binds an installation only if that person's own token lists it. Unset, the callback trusts the installation id in the URL — fine for a private single-tenant App, a cross-tenant read on a public one. **Also what enables signing in with GitHub** — unset, `/v1/auth/github/start` answers `github=unavailable` rather than leaving for a URL that cannot work. See the note below. |
| `STRATUM_GITHUB_RUNNERS_APP_ID` / `_APP_KEY_PEM` or `_APP_KEY` / `_WEBHOOK_SECRET` / `_CLIENT_ID` / `_CLIENT_SECRET` / `_INSTALL_URL` | *(unset)* | A second GitHub App that only registers runners and cancels runs, so it can be listed on the Marketplace on its own with only those permissions. Its installations are recorded under the provider `github-runners`, it delivers `workflow_job` to `/webhooks/github-runners`, and the dashboard's runner page installs it (`?app=runners`); its Setup URL and Callback URL on GitHub are `/v1/github/setup/runners` — a path, because GitHub drops a callback URL's query string. Requires the mirror App above. Unset, the mirror App runs jobs too. |
| `STRATUM_GITHUB_API_BASE` / `STRATUM_GITHUB_GIT_BASE` / `STRATUM_GITHUB_OAUTH_BASE` | github.com | Override for GHE (and for the test fakes); the last is where the OAuth token exchange lives. |
| `STRATUM_RUNNER_ECS_GITHUB_TASK_DEFINITION` | *(unset)* | The ECS task definition that runs the official `actions/runner` agent for a GitHub Actions job (`runs-on: weft`), alongside the Weft runner's own `STRATUM_RUNNER_ECS_TASK_DEFINITION`. Unset = the feature is off: a job asking for a Weft size is recorded as refused with "this Weft deployment has no GitHub Actions runner configured". Needs the GitHub App configured too, or the dispatcher logs that and does nothing. See `docs/deployment-aws.md`. |
| `STRATUM_RUNNER_ECS_GITHUB_CONTAINER` | `runner` | The container name in that task definition the dispatcher's overrides address. |
| `STRATUM_RUNNER_GITHUB_EXEC` | *(unset)* | The exec-executor equivalent for a single box or the e2e suite: a command run per GitHub job in place of `RunTask`. Mutually exclusive with the ECS variables. |
| `STRATUM_GITHUB_RUNNER_POLL_SECS` | `5` | How often each node runs the GitHub-runner dispatcher: cancels refused runs on GitHub, sweeps idle and overdue runners, claims queued jobs. `0` turns the dispatcher off on that node; jobs are still recorded and stay queued for a node that runs it. |
| `STRATUM_GITHUB_RUNNER_IDLE_SECS` | `600` | How long a launched runner may wait for GitHub to hand it a job before it is stopped, removed from GitHub and the job marked `abandoned`, unbilled. |
| `STRATUM_STRIPE_KEY` / `STRATUM_STRIPE_PRICE` / `STRATUM_STRIPE_WEBHOOK_SECRET` | *(unset)* | Set all three to enable billing: orgs are free for public work with no card, the first private repo sends the owner to a Stripe Checkout that opens the per-seat subscription (the card and any promotion code are taken there; Stripe is the merchant of record), `/webhooks/stripe` is verified with the secret. Setting one or two of them is a boot error, never a half-configured provider. Unset = billing off, every plan gate answers yes. |
| `STRATUM_STRIPE_BASE` | `https://api.stripe.com` | Override for the test fake. |
| `STRATUM_PRICE_PER_SEAT_CENTS` | `400` | What the dashboard and `GET …/billing` display; the price Stripe charges is the one behind `STRATUM_STRIPE_PRICE`, so keep them in step. |
| `STRATUM_FREE_CI_MINUTES` | `500` | Hosted-runner minutes per rolling 30 days for a free namespace or organization. `0` = unlimited. |
| `STRATUM_PAID_CI_MINUTES_PER_SEAT` | `1000` | Hosted-runner minutes each paid seat adds to the org's pool per billing period (was `2000` before use past the pool was billable). `0` = unlimited. |
| `STRATUM_PAID_EGRESS_GB_PER_SEAT` | `10` | GB transferred out of private repositories each paid seat adds to the pool per period. `0` = unlimited. |
| `STRATUM_PAID_STORAGE_GB_PER_SEAT` | `5` | GB stored in private repositories each paid seat adds to the pool, averaged over the period. `0` = unlimited. |
| `STRATUM_OVERAGE_1000_MINUTES_CENTS` / `STRATUM_OVERAGE_EGRESS_GB_CENTS` / `STRATUM_OVERAGE_STORAGE_GB_MONTH_CENTS` | `800` / `10` / `10` | What the dashboard and `/pricing` quote past the pool — cents per thousand minutes, per GB transferred, per GB-month stored. Display and estimate only, like `STRATUM_PRICE_PER_SEAT_CENTS`: the metered prices below are what charge, so keep them in step. |
| `STRATUM_STRIPE_PRICE_MINUTES` / `_EGRESS` / `_STORAGE` / `_PACKAGES` | *(unset)* | The metered Stripe prices use past the pool is billed at. All eight of these and the `STRATUM_STRIPE_METER_*` below, or none; some of eight is a boot error. Unset = the spend limit is pinned at `$0`, `PATCH …/billing/spend-limit` answers 503, and the pool is a hard cap. |
| `STRATUM_STRIPE_METER_MINUTES` / `STRATUM_STRIPE_METER_EGRESS` / `STRATUM_STRIPE_METER_STORAGE` | *(unset)* | The Stripe billing meters those prices read; the rollup reports usage to them. Same all-or-nothing rule as the prices. |
| `STRATUM_REF_PAGE_SIZE` | *(unset)* | Opt ingests/compactions into sharded ref pages at this page size. Unset = flat refs until a repo exceeds the default page size (1000). Set for fleets with very high ref counts. |
| `STRATUM_SITE_DIR` | *(unset)* | Built `web/site/dist` to serve at `/`. Unset = API-only. |
| `STRATUM_DASHBOARD_DIR` | *(unset)* | Built `web/dashboard/dist` to serve at `/dashboard/`. |
| `STRATUM_SSH_BIND` | *(unset)* | Listen address for the git-over-SSH front door (e.g. `0.0.0.0:2222`). Unset = SSH off. |
| `STRATUM_SSH_HOST_KEY` | *(required with `STRATUM_SSH_BIND`)* | The server's host key, PEM inline (`ssh-keygen -t ed25519`). Must be the SAME key on every node and across restarts — a per-boot key looks like a MITM to every client. Boot fails if the bind is set without it. |
| `STRATUM_SSH_PUBLIC_URL` | *(unset)* | Externally-visible SSH base (`ssh://git@host:port`) written into `ssh_clone_url` fields; unset = field is null and the dashboard shows HTTPS only. |
| `STRATUM_CDN_BASE` | *(unset)* | CDN base URL for offloaded clone packs (git `packfile-uri`). Unset = offload off and the capability is never advertised. |
| `STRATUM_CDN_ENABLED` | `1` | Kill switch. `0` turns offload off fleet-wide without a redeploy; clones keep working, served inline. |
| `STRATUM_CDN_KEY_PAIR_ID` | *(unset)* | CloudFront key-pair ID, for the shape where the CDN fronts the **bucket**. Must be set together with the private key or boot fails. |
| `STRATUM_CDN_PRIVATE_KEY_PEM` / `STRATUM_CDN_PRIVATE_KEY` | — | CloudFront signing key (RSA-2048): path to a PEM file, or the PEM inline. |
| `STRATUM_CDN_ORIGIN_SECRET` | *(unset)* | HMAC secret for the shape where the CDN fronts **this server**; enables `GET /v1/orgs/:org/repos/:repo/cdn/:pack`. Mutually exclusive with `STRATUM_CDN_KEY_PAIR_ID` — the two select different origins, and setting both is a boot error. |
| `STRATUM_CDN_URL_TTL_SECS` | `3600` | Lifetime of a pack URL. Must comfortably exceed a slow clone: git does not fall back to the server for an advertised pack, so a URL that expires mid-clone fails it. |
| `STRATUM_CDNPACK_POLL_SECS` | `60` | How often the CDN pack worker looks for repos whose pack has fallen behind the tip. Offload only engages while the pack covers the current tip exactly, so this is also how quickly offload resumes after a push. `0` disables. |

### Signing in with GitHub

Two things have to be true on the **App itself**, and neither lives in
this repository:

* The callback URL list must include
  `<STRATUM_PUBLIC_URL>/v1/auth/github/callback`. The server derives it
  from its own public URL rather than taking it as a separate variable,
  so it cannot drift from the host people are actually on — but GitHub
  will refuse a `redirect_uri` it has not been told about.
* The App needs the **`Email addresses` account permission, read-only**.
  Without it `GET /user/emails` answers 403, the callback cannot see
  which address GitHub has proved, and every sign-in lands on
  `github=noemail`. This is an *account* permission, consented to by
  each person at the authorization screen, so adding it does **not**
  make existing installations re-approve anything.

Both are easy to get wrong quietly: the first fails at GitHub with the
App's own error page, and the second fails on our side with a sentence
that reads like the person's GitHub account is at fault. If sign-in is
refusing everybody with `noemail`, check the permission first.

`STRATUM_LAYOUT`, `STRATUM_MANIFEST_TIER`, `STRATUM_LOCATOR_TIER`,
`STRATUM_REF_PAGE_MAX`, and `STRATUM_LATENCY_MODEL` are engine/bench knobs
inherited from the research code; leave them alone in production.

## Reads on a long WAL

Every accepted write — a push, a REST commit, a mirror sync — appends one
WAL entry to the repository's manifest, and the compactor folds the WAL
into a fresh epoch once it holds 8 entries or 16 MB. Reads consult the WAL
first, because a just-written object exists nowhere else, and that makes
the WAL's length the read API's fixed cost.

It was a cost nobody had measured until the first real mirror on weft.sh
reached **102** WAL entries. Nothing ever asked the compactor to fold it:
only pushes and REST commits enqueued the job, and nobody pushes to a
mirror. Each REST read then downloaded all 102 packs — two GETs an entry,
sequentially, on a connection opened for that request — before it could
find a single object, so a three-byte file took 10.8 s and a directory
listing 22 s, while a repository of the same age that had been *pushed*
answered the same requests in 0.4 s. Clones were unaffected: the wire path
streams segments and never walks the WAL this way.

Three things changed, and each holds on its own:

- **A sync is a write.** Mirror sync enqueues `compact` the way a push
  does, so a mirror's WAL is folded at the same thresholds.
- **Membership before payload.** A reader learns which objects a WAL
  entry holds from its oid sidecar and fetches the pack only on a hit.
  A plane object behind *N* entries costs *N* small GETs, not *2N* pack
  downloads.
- **One store client and one cache per process.** The read API shares
  a pooled S3 client instead of opening a connection per request, and
  objects, sidecars and packs it has fetched stay in memory under
  `STRATUM_READ_CACHE_MB`.

If the code browser is slow on one repository and not another, check
that repository's `manifest.json` in the bucket for the length of `wal`
before anything else. A WAL past the threshold on a mirror means the
`compact` job is not being enqueued or not being claimed; `jobs` in the
control database says which.

## Health, readiness, metrics

- `GET /healthz` — liveness: the process is up. Always 200, with the
  answering instance's id in `x-weft-instance`.
- `GET /readyz` — readiness: the control DB answers a query **and** the
  object store answers a LIST. 503 names the failing dependency. Gate
  load-balancer traffic on this.
- `GET /metrics` — Prometheus text format: request counts, bytes, latency
  histograms per route class.

## Operator CLI

The same binary is the admin tool (connects to `STRATUM_DB_URL`, so run
it anywhere that can reach the database):

```sh
stratum-server admin bootstrap --org NAME [--plan P] # create org + admin token; P entitles it (paid|free)
stratum-server admin mint --org NAME --scopes repo:read[,…] [--repo NAME] [--label L]
stratum-server admin set-plan --org NAME --plan paid
stratum-server admin user-create --org NAME --email ADDR --password SECRET [--name N] [--role R]
stratum-server admin user-disable --email ADDR       # offboarding
stratum-server admin user-enable  --email ADDR       # …and undoing it
stratum-server admin verify-link  --email ADDR       # the confirmation link, for a mail that never arrived
```

`verify-link` exists because sign-up mails a link and stores only the
token's hash: a mail that bounced, was filtered, or was refused by a
provider still in its sandbox leaves an account that can sign in and
create nothing, with nothing to read back. It mints a fresh token and
prints exactly the URL the mail would have carried — one use, 24 hours,
redeemed at the same route — for the operator to hand over. It does not
mark the address verified by itself. `STRATUM_PUBLIC_URL` (or
`--public-url`) is what the link points at. On the deployed fleet every
one of these runs as a one-off task: `deploy/admin-ecs.sh verify-link
--email ADDR`.

Tokens are shown once at mint and stored only as SHA-256 hashes.
Revocation (`DELETE /v1/orgs/{org}/tokens/{id}`) is immediate — there is
no verification cache.

`user-create` on an address that already has an account adds the
membership rather than failing: one person in several namespaces is the
normal case.

### Offboarding

`user-disable` is the one command that ends someone's access everywhere,
and **nothing has to be revoked by hand**. Every credential resolves the
account on use — bearer tokens and browser sessions check the flag
directly, SSH keys through the role lookup — so their personal tokens
stop verifying (`401`) and their SSH keys stop reaching any namespace
(masked as "not found") on the very next request.

It is deliberately not a delete. The audit trail names people by id, and
rows pointing at somebody who vanished are worse than rows pointing at
somebody disabled. The membership and role are kept, so `user-enable`
restores exactly what they had, and an administrator can still see them
on the members screen, flagged, in order to put them back.

Deploy keys and org service tokens belong to nobody and are unaffected —
disabling a person is not an outage.

## Backup and disaster recovery

Two things hold all state:

1. **The bucket.** Immutable segments, WAL entries, epochs, and one
   `manifest.json` per repo changed only by compare-and-swap. Use your
   store's versioning/replication; there are no in-place overwrites
   except the manifest pointer.
2. **The control database.** PostgreSQL: use `pg_dump`/`pg_basebackup`
   or your provider's point-in-time recovery. It holds orgs, repos,
   tokens, audit, metrics, jobs.

A new node with those two restored serves everything. Mirror seed clones
under `STRATUM_DATA_DIR` re-create themselves on the next sync.

## Service limits (v1, by design — see docs site for the full story)

- 64 MB request cap: bigger pushes answer 413 (split the push).
- ~16 concurrent pushers per repo; more queue behind the semaphore.
- Reads of just-pushed objects may scan the WAL until compaction folds it.
- sha1 repos only (formats are versioned for sha256 later).
