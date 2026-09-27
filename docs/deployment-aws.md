# Deploying Stratum on AWS

Merge to `main` → container built → running on AWS. This document is the
architecture, the one-time bootstrap runbook, and the honest limitations
table. Everything here is code in this repo: `Dockerfile`,
`deploy/compose.yml` + `deploy/smoke.sh` (the local rehearsal),
`deploy/terraform/` (the infrastructure), and
`.github/workflows/deploy.yml` (the pipeline).

## Architecture

```
                       ┌────────────────────────┐
   https (443)         │  CloudFront            │   TLS, default cert
  site / dashboard ───▶│  UseOriginCacheControl │──▶ ALB :80 ──▶ ECS tasks :8080
  REST / git HTTP      │  AllViewer (auth thru) │      │
                       └────────────────────────┘      │  idle 350s, /healthz,
   ssh (22)            ┌────────────────────────┐      │  /metrics → 404
  git clone/push ─────▶│  NLB TCP:22            │──▶ tasks :2222 (russh)
                       └────────────────────────┘
  ECS Fargate (2–10 tasks, CPU 60% + 500 req/target target-tracking)
      │                         │
      ▼                         ▼
  Aurora Serverless v2      S3 store bucket (via gateway endpoint)
  (control plane, NoTls,    (immutable segments, manifest CAS;
   rds.force_ssl=0)          static IAM-user creds — signer has no
                             role support)
```

- **One binary per task** serves the marketing site, dashboard, REST API,
  git smart HTTP v2, git-over-SSH, and all background workers. Tasks are
  stateless; `STRATUM_DATA_DIR` is 100 GiB Fargate ephemeral storage
  (mirror seed clones rebuild themselves after task churn).
- **Git clients need nothing special.** `git clone https://…` IS git's
  smart HTTP transport — every e2e suite proves it with the stock CLI.
  `git clone ssh://git@<nlb-dns>/org/repo.git` works against the NLB with
  a registered key, needs no domain or certificate at all, and is
  therefore the fully-encrypted git path from day one.
- **CDN and caching — what CloudFront does and deliberately does not
  cache.** CloudFront fronts the ALB with a cache policy that honors the
  app's own `Cache-Control` headers and an origin-request policy that
  passes `Authorization` through. **Origin Shield** (a regional cache tier,
  region = the deploy region) sits between the edge PoPs and the ALB:
  edge-cache misses from many locations collapse into one origin fetch and
  a warm regional cache absorbs repeats, so the cacheable surfaces scale
  without hammering the tasks. What is cacheable: the immutable, hashed web
  bundles (`max-age=31536000, immutable`) and any response the app marks
  public. What is **never** cached, by design: the git data plane and
  per-tenant authenticated responses. The git fetch/clone payload comes
  back from a `POST` (`git-upload-pack`) — CloudFront never caches POST,
  and the pack is computed per-negotiation anyway — so all git responses
  are `no-cache` and go to origin; private-repo responses vary per tenant
  and must not be shared at the edge. Git-at-scale is handled a layer down:
  immutable, content-addressed segments in S3 (the durable cache) plus
  stateless autoscaling nodes that assemble packs from them. Authorization
  is enforced at origin against the control plane (instant revocation), not
  at the edge. (If aws_region is one of the few without Origin Shield, set
  `-var origin_shield_region=<nearby supported region>`.)
- **CDN-offloaded clones — the one git payload that *is* edge-served.**
  Everything above says the git data plane never caches, and for the
  negotiated pack that stays true. Offload works differently: a background
  worker builds a self-contained packfile per repo into the store bucket,
  and a client that opted in (`git -c fetch.uriprotocols=https clone …`)
  is handed a signed URL for it via git's `packfile-uri` capability. It
  fetches the bulk from CloudFront and the server streams only the
  remainder — the commits pushed since the pack was built — inline. A
  second CloudFront origin points at the store bucket through origin
  access control, restricted by bucket policy to `o/*/cdn/*.pack`, so
  nothing else in the store is edge-reachable.

  Authorization for those URLs is **CloudFront's**, not the app's, because
  it has to be: git fetches an advertised `packfile-uri` with no
  credentials at all. The distribution therefore has a trusted key group,
  and the app signs each URL with the RSA key's private half from Secrets
  Manager. `STRATUM_CDN_URL_TTL_SECS` (default 1 h) must comfortably
  exceed your slowest clone.

  Three behaviours worth knowing before you turn this on, each measured
  against stock git rather than assumed:

  - **Clients that have not opted in are unaffected.** They clone exactly
    as before, with zero CDN traffic. Nothing about the existing fleet
    changes.
  - **Offload engages only while the pack covers the current tip.** After
    a push the pack lags, and until the packer catches up (seconds, see
    `STRATUM_CDNPACK_POLL_SECS`) clones are served inline — correct, just
    not offloaded. Serving a lagging pack is *not* safe: the inline
    remainder would be the pushers' own thin packs, whose delta bases sit
    inside the CDN pack, and git indexes the inline pack before fetching
    the advertised URIs. So a clone-heavy, push-light repo offloads nearly
    always; a repo under constant push churn offloads rarely.
  - **A missing pack is fatal to that clone.** git does not fall back for
    an advertised URL, because the server already excluded those objects
    from the inline stream. The server therefore confirms the object
    exists before it advertises anything, and `STRATUM_CDN_ENABLED=0`
    turns advertisement off fleet-wide without a redeploy.

  Offloaded clones meter as `cdn_clone` rather than `clone`, so the saving
  is visible in `/v1/orgs/:org/repos/:repo/metrics`.

  **What it actually costs and saves.** Worth being precise, because the
  intuition ("we stop paying to serve clone bytes") is wrong. Illustrative
  us-east-1 on-demand prices, per GB of clone served:

  | | inline (today) | offloaded |
  |---|---|---|
  | CloudFront → internet | $0.085 | $0.085 |
  | ALB processed bytes | $0.008 | ~0 |
  | Fargate assembly | ~$0.0003 | ~0 |
  | **total** | **~$0.093** | **~$0.085** |

  **CloudFront egress dominates and does not change** — the bytes still
  leave AWS, just from a different origin. The per-byte saving is about
  **9%**, not a step change. Anyone expecting offload to halve the bill
  will be disappointed.

  The real wins are elsewhere, and they are larger:

  - **Peak fleet size.** Task count is set by *concurrent* clone
    throughput, and offloaded bytes hold no task, no permit, and no
    memory. A fleet sized for clone peaks can shrink to one sized for
    push and API traffic. At ~$36/task-month, that is where the money is.
  - **Repeat clones become nearly free at origin.** A CI fleet cloning
    the same repo a thousand times a day assembles it a thousand times
    today. Offloaded, the pack is immutable and cached at the edge, so
    origin serves it a handful of times per TTL. Egress is unchanged;
    origin work collapses.

  **What it adds:**

  - **Storage** — the pack duplicates the repo's packed bytes, ~$0.023
    per GB-month.
  - **Repacking** — every push makes the pack stale, and catching up
    means materializing the repo and running `git pack-objects`: roughly
    $0.003 per GB of repo per rebuild. Bursts collapse into one rebuild
    (the worker de-duplicates queued jobs), so the real driver is push
    *episodes*, not commits.

  That second one is what decides whether the feature pays, and it makes
  the answer depend on the repo's read/write mix rather than its size:

  | repo, per GB per month | rebuilds | added cost | clones to break even |
  |---|---|---|---|
  | CI-consumed, pushed a few times a day | ~60 | ~$0.20 | ~25 |
  | mirror or release repo, pushed weekly | ~4 | ~$0.035 | ~5 |
  | monorepo under constant churn | ~500 | ~$1.52 | ~185 |

  So: **strongly positive for clone-heavy, push-light repos** — mirrors,
  release artifacts, anything a CI fleet pulls all day — and marginal to
  negative for a repo pushed constantly and cloned rarely. That second
  case is also the one where offload engages least often, since the pack
  spends most of its time lagging, so the wasted repacks are the main
  exposure.

  Turn it off (`STRATUM_CDN_ENABLED=0`) for a fleet that is push-heavy and
  clone-light: there, offload is a net cost.

- **Secrets** (DB URL, store access key, webhook secret, SSH host key,
  CDN signing key, runner dispatch key)
  live in Secrets Manager, injected by the ECS execution role. The task
  role is empty on purpose. The `stratum-cd` deploy role cannot read
  secrets.
- **Hosted CI runners live in their own VPC** with no route to any of the
  above — see [Hosted runners](#hosted-runners). They hold no task role
  and no store credentials, and all their egress goes through a
  default-drop domain allowlist.
- **Two CI roles via GitHub OIDC** (no stored AWS keys): `stratum-cd`
  (narrow: ECR push, task-def register, service roll, run-task, SSM read)
  runs on every main merge; `stratum-infra` (broad) is only assumable
  through the `infra` GitHub environment — put required reviewers on it.
  Both trust **two spellings** of the OIDC `sub` claim, because GitHub is
  migrating to immutable identifiers and is part-way through it — see
  [When the deploy cannot assume its role](#when-the-deploy-cannot-assume-its-role).

## Prove it locally first

CI's `deploy-validation` job runs this loop on every push — on the fleet,
under PRoot, with no container runtime (`deploy/proot/README.md`). With
Docker to hand, the prod-parity compose stack is the quickest way to run
it yourself before trusting any deploy:

```sh
./deploy/dev-host-key.sh
docker compose -f deploy/compose.yml up -d --build --wait
BASE_URL=http://127.0.0.1:8080 \
SSH_ENDPOINT=ssh://git@127.0.0.1:2222 \
BOOTSTRAP_CMD="docker compose -f deploy/compose.yml exec -T app stratum-server admin bootstrap" \
  ./deploy/smoke.sh
```

That is the REAL deployable image against Postgres and MinIO-as-S3, with
the real git client cloning (+ `fsck --full --strict`), pushing, and
reading back over BOTH transports.

## Signing in to AWS

Every `aws`, `terraform` and `scripts/manual-*.sh` run in this document
needs a live login session, and the two ways that step has gone wrong are
both silent until something downstream fails with "session has expired".

```sh
# Once per session. Needs a real terminal — the browser hand-off cannot
# run from a background job or an agent's shell — and the region on the
# command line, or the CLI stops to ask for one and dies on a non-tty.
aws login --profile stratum-test --region us-east-1

# Then point everything at the *re-exporting* profile, never at the login
# profile itself and never at a pasted `aws configure export-credentials`.
export AWS_PROFILE=stratum-tf        # weft-tf for a shell that should read as prod
aws sts get-caller-identity          # 541260258547 means it worked
```

`stratum-test` is the login profile (the browser session); `stratum-tf`
and `weft-tf` carry no credentials of their own — their
`credential_process` re-reads that session on every call, which is what
keeps a long apply from expiring halfway (see below). If a command
answers "Your session has expired", run the first line again; nothing
else needs to change.

## One-time bootstrap runbook

1. **Bootstrap terraform** (operator credentials, local state). If those
   credentials come from `aws login`, give terraform a profile whose
   `credential_process` refreshes them — a static export expires mid-apply,
   and an apply that dies holding the lock leaves an `errored.tfstate` to
   push back by hand:

   ```ini
   [profile stratum-tf]
   region = us-east-1
   credential_process = aws configure export-credentials --profile <login-profile> --region us-east-1 --format process
   ```


   ```sh
   cd deploy/terraform/bootstrap
   terraform init && terraform apply -var github_repo=OWNER/REPO
   ```

   `github_repo` is the only thing naming who may deploy, and it is spent
   on the OIDC `sub` claim in **both** of the spellings GitHub currently
   mints — see below. Re-apply this root after a rename or transfer: the
   trust policy still names the old repository and every deploy stops.

2. **GitHub repository configuration** from the outputs:
   - variables `AWS_REGION`, `AWS_CD_ROLE_ARN`, `AWS_INFRA_ROLE_ARN`,
     `TF_STATE_BUCKET`;
   - environment **`infra`** with required reviewers.

   The reviewers rule is a paid-plan feature on a private repository —
   GitHub answers `422 … ensure the billing plan supports the required
   reviewers protection rule` on the free plan. Without it the
   environment still exists (a `main`-only branch policy is the most the
   free plan allows, and the OIDC trust requires it), but a merge to
   `main` that touches `deploy/terraform/` applies **unattended**; the
   pull request review is then the only gate on an infrastructure
   change, so treat a terraform diff as one.

3. **The environment's variables** live in `deploy/terraform/envs/<env>.tfvars`
   — `prod.tfvars` is committed and is what `deploy.yml` applies with, so
   every command below passes the same file. An apply without it is an
   apply of `variables.tf`'s defaults (no domain, no App, no billing), and
   `docs_e2e::the_deploy_workflow_applies_the_environments_own_variables`
   holds the workflow to the file for that reason.

   ```sh
   cd deploy/terraform
   terraform init -backend-config="bucket=<state-bucket>" \
     -backend-config="key=stratum/prod.tfstate" \
     -backend-config="region=<region>" \
     -backend-config="use_lockfile=true"
   ```

   Or let `scripts/tf.sh prod <command>` do both halves — the init above
   and the `-var-file` on every plan, apply and destroy — from the one
   word; see [Two environments, one root](#two-environments-one-root).

4. **The domain**, if `domain_name` is set. The stack owns the Route 53
   zone and issues a DNS-validated certificate, and the certificate cannot
   issue until the registrar points at the zone — so the first apply is
   two steps with a person in the middle:

   ```sh
   terraform apply -var-file=envs/prod.tfvars -target=module.dns[0].aws_route53_zone.this
   terraform output name_servers      # hand these four to the registrar
   dig +short NS weft.sh @1.1.1.1     # until it answers with them, the next step waits
   ```

   A full apply before delegation is not wrong, only slow: the validation
   resource waits up to thirty minutes for ACM and then fails, and the
   next apply picks up where it stopped. The same step verifies the domain
   with SES and writes its DKIM, MAIL FROM and DMARC records. What it
   cannot do is take the account out of the **SES sandbox** — a new
   account sends only to addresses it has verified, so request production
   access in the SES console before the first invitation is expected to
   arrive anywhere.

   **The sites domain, if `sites_domain_name` is set**, is the same shape
   on a second registration — `weft.cx` in production, deliberately not a
   subdomain of `weft.sh`, because a page a customer publishes must not be
   able to set a cookie the dashboard would accept. Its zone and its
   wildcard certificate are created by any ordinary apply, and no apply
   waits on them: the certificate is *requested* and stays
   `PENDING_VALIDATION` until that registrar is delegated too, and ACM
   issues it by itself minutes later.

   ```sh
   terraform output sites_name_servers   # hand these four to the registrar
   dig +short NS weft.cx @1.1.1.1        # until it answers with them, nothing serves
   ```

   Nothing on that name is served until the slice that fronts it with
   CloudFront lands and `STRATUM_SITES_DOMAIN` is set; until then the
   server's host dispatch is one string comparison that never matches.

5. **First image** (ECR must hold `:bootstrap` before the first full
   apply):

   ```sh
   terraform apply -var-file=envs/prod.tfvars -target=module.app.aws_ecr_repository.app
   aws ecr get-login-password | docker login --username AWS --password-stdin <ecr-url>
   docker build -t <ecr-url>:bootstrap . && docker push <ecr-url>:bootstrap
   ```

6. **First full apply**: `terraform apply -var-file=envs/prod.tfvars`.
   Outputs print the base URL — the domain, or CloudFront's hostname
   without one — the ALB URL, and the SSH endpoint.

7. **First org**: `./deploy/smoke-bootstrap-ecs.sh --org yourco --plan paid`
   runs `admin bootstrap` as a one-off task and prints the org:admin
   token — save it; it is shown exactly once. `--plan paid` matters on a
   fleet with `billing_enabled`: an org made without it has no card and
   holds public repositories only, so its first private repository — and
   the deployed smoke's — answers 402.

8. **Merge to main.** From here `deploy.yml` does build → (gated infra
   apply when `deploy/terraform/**` changed) → task-def roll →
   deployed smoke over HTTP and SSH.

9. **The GitHub App.** Create it against the deployed origin —
   `scripts/github-app-create.py --org <org> --name <name> --public-url
   https://api.<domain>` sets the webhook and Setup URLs from a manifest
   and leaves the private key and webhook secret in `.secrets/` — then
   generate the App's **client secret** on its settings page (the client
   id is shown there), put the five things GitHub issued into the secret
   the first apply created blank, and name the App in the tfvars:

   ```sh
   aws secretsmanager put-secret-value --secret-id <project>/<env>/github-app \
     --secret-string "$(jq -n --arg id <app-id> \
       --rawfile pem .secrets/<slug>.private-key.pem --arg wh <webhook-secret> \
       --arg cid <client-id> --arg cs <client-secret> \
       '{app_id:$id,private_key:$pem,webhook_secret:$wh,client_id:$cid,client_secret:$cs}')"
   # envs/prod.tfvars: github_app_slug = "<slug>"   — commit it; CD applies it
   ```

   The plan refuses if any of the five is blank. A server with an app id
   and no key refuses to boot; one with a blank webhook secret verifies
   every delivery against `""` and drops it — which from the outside
   looks like GitHub never calling; and one without the OAuth client
   cannot prove that the person finishing an install controls the
   installation they arrive with, so on a public App any org admin could
   connect — and mirror — another customer's installation. The manifest
   turns on "Request user authorization (OAuth) during installation"
   and "Redirect on update" for that reason; for an App created before
   this, flip both by hand and set the callback URL to
   `https://api.<domain>/v1/github/setup`.

10. **Billing**, when you are ready to sell seats. The first apply creates
    the `<project>/<env>/stripe` secret with three blank values and leaves
    `billing_enabled = false`, so the fleet runs as the self-hosted build:
    every org holds private repositories for free. To turn it on, fill the
    secret from the Stripe dashboard — a restricted key, the recurring
    price id, and the signing secret of a webhook endpoint pointed at
    `https://<public-url>/webhooks/stripe`, **pinned to the API version
    `stripe.rs` speaks** (`API_VERSION`; the fixtures were recorded at
    it, and an endpoint at the account's default version delivers
    different bodies) — then set `billing_enabled` in the tfvars and
    commit it, or apply it yourself. Put the three values in a gitignored
    env file and let the script move them, so none of them lands in a
    shell history:

    ```sh
    # .env.prod: STRATUM_STRIPE_KEY=rk_live_…  STRATUM_STRIPE_PRICE=price_…
    #            STRATUM_STRIPE_WEBHOOK_SECRET=whsec_…
    scripts/fill-stripe-secret.sh .env.prod <project>/<env>/stripe
    # envs/prod.tfvars: billing_enabled = true
    terraform apply -var-file=envs/prod.tfvars
    ```

    Portal sessions need a customer-portal configuration to exist, and a
    new account has none: Settings → Billing → Customer portal → Save
    once, or every "Manage billing" click 500s.

    The plan refuses if any of the three is still blank, because the server
    does too: `STRATUM_STRIPE_KEY`, `STRATUM_STRIPE_PRICE` and
    `STRATUM_STRIPE_WEBHOOK_SECRET` are all-or-nothing and a half-set
    trio is a boot error, not a provider that quietly half-works. Set
    `price_per_seat_cents` to what the Stripe price actually charges — the
    dashboard and `/pricing` print that number; Stripe charges the price
    object. Then run `scripts/manual-stripe.sh` against the test-mode
    account before flipping the key to live; see
    [`LAUNCH.md`](LAUNCH.md#the-stripe-contract).

## When the deploy cannot assume its role

`deploy` goes red at its first step with:

```
Could not assume role with OIDC: Not authorized to perform sts:AssumeRoleWithWebIdentity
```

That message reads as a bootstrap that was never run, and it almost never
is. STS says nothing about *which* condition failed, so do not re-check
the role, the provider and the repository variables by eye — ask
CloudTrail what claim actually arrived. The `sub` is the event's
`Username`:

```sh
aws cloudtrail lookup-events --region us-east-1 \
  --lookup-attributes AttributeKey=EventName,AttributeValue=AssumeRoleWithWebIdentity \
  --max-results 5 --query 'Events[].[EventTime,Username]' --output text
```

Compare that against the two patterns each role trusts. On a push to main
the CD role expects one of:

```
repo:OWNER/NAME:ref:refs/heads/main
repo:OWNER@<owner-id>/NAME@<repo-id>:ref:refs/heads/main
```

and the infra role the same pair ending `:environment:infra`.

The second spelling is GitHub's **immutable subject claim**: owner and
repository names carry their numeric ids so that a rename or a transfer
cannot silently hand someone else your trust. The rollout is not finished
and it is not consistent — this repository's
`GET /repos/OWNER/NAME/actions/oidc/customization/sub` reports an
id-bearing `sub_claim_prefix` while still reporting
`use_immutable_subject: false` — so the bootstrap trusts both rather than
betting on either, and only the ids are wildcarded. It cost a red deploy
to learn: the trust matched the legacy spelling alone, GitHub sent the
other, and nothing on the AWS side looked wrong.

`the_bootstrap_oidc_trust_admits_both_github_subject_formats` in
`crates/stratum-server/tests/docs_e2e.rs` holds both halves — that each
role admits both spellings of its own claim, and that neither admits
another owner, another repository, another branch or a pull-request ref.
If GitHub adds a third spelling, add it there first; a pattern is one `*`
away from trusting every repository on GitHub, and the deploy goes green
either way.

## Two environments, one root

`deploy/terraform` is one root module for every environment. `env` names
everything it makes — `stratum-<env>-*` for resources, `/stratum/<env>/…`
for parameters and log groups, `stratum/<env>/…` for secrets, its own ECR
repositories — so `prod` and `test` share an account without sharing a
resource, and each has its own state key (`stratum/<env>.tfstate`) and its
own `envs/<env>.tfvars`. The bootstrap stack (state bucket, the two OIDC
roles) is the only thing they share, and `deploy.yml` only ever deploys
`prod`; a rehearsal is a laptop's to build and destroy:

```sh
AWS_PROFILE=<profile> scripts/tf.sh test apply     # build test.weft.sh
AWS_PROFILE=<profile> scripts/tf.sh test output    # its URLs
AWS_PROFILE=<profile> scripts/tf.sh test destroy   # and take it down
```

Three things make that safe rather than merely possible:

- **The state key and the tfvars are chosen by the same word.**
  `scripts/tf.sh <env>` re-initialises the backend for `<env>` before
  every command and passes `envs/<env>.tfvars` to the ones that take it.
  Typed by hand the two flags are independent, and `envs/test.tfvars`
  against a directory initialised for prod is a plan to rename every
  `stratum-prod-*` resource to `stratum-test-*` — destroy-and-create for
  most of them, with every `var.env == "prod"` protection reading "test"
  by then.
- **The state refuses the wrong environment anyway.** `modules/env-lock`
  records `env` in the state on the first apply and fails any later plan
  whose `env` disagrees, before a resource is planned. Its contract is a
  `terraform test` (`modules/env-lock/tests`) the `deploy-validation` job
  runs against a real terraform, no provider needed.
- **A rehearsal delegates itself.** `test.tfvars` serves at
  `test.weft.sh` with `parent_domain_name = "weft.sh"`: the dns module
  finds production's zone by name and writes the NS record into it, so
  the rehearsal's first apply is one step and its certificate validates in
  the same run. That one record is the rehearsal's state's, inside
  production's zone; `scripts/tf.sh test destroy` removes it. Prod's own
  delegation is the registrar's, as in step 4.

What a rehearsal does *not* get by default is production's providers —
`github_app_slug` and `billing_enabled` are off in `test.tfvars`, because a
second fleet on production's GitHub App would receive production's
webhooks, and the Stripe account is one per fleet. Give it its own App
(`scripts/github-app-create.py --name weftsh-test --public-url
https://api.test.weft.sh`) and the sandbox's keys in `stratum/test/stripe`,
then flip the two flags in `test.tfvars`. SES sandbox status is the
account's, so mail from `no-reply@test.weft.sh` reaches wherever prod's
does. And two environments in one account share its quotas — Fargate
vCPUs, Aurora clusters, Elastic IPs — which is the trade
[`LAUNCH.md`](LAUNCH.md#the-aws-rehearsal) records taking.

**Raise the Fargate vCPU quota before you need it.** A new account
starts at **6** concurrent Fargate On-Demand vCPUs (Service Quotas
`L-3032A538`), and everything in this design draws on it: the app
service at 2 vCPUs a task and 2 tasks is 4 of them; a rolling deploy
briefly wants 4 tasks; every hosted job is another 1; and the one-off
admin task the deploy's smoke bootstraps with is another 2. On 2026-09-07
the smoke ran while the old tasks were still draining beside the new
ones and `RunTask` answered "You've reached the limit on the number of
vCPUs you can run concurrently" — `deploy/admin-ecs.sh` now prints that
and retries for ten minutes, but the smoke is only the first thing to
hit the ceiling. At 6 vCPUs, **two hosted jobs** fill the fleet. Request
32 or more as part of step 1; the increase is usually approved within
the hour and costs nothing until used.

**Until it lands, a deploy and the fleet fight for the last slot.** On
2026-09-08 a rollout sat in `amazon-ecs-deploy-task-definition` until
its 30-minute waiter timed out: the app's two tasks and one `weft-2x` job
made 6, the rollout's extra task could not place, and each time a job
finished the dispatcher — polling every 5 seconds — launched the next
queued job before the ECS scheduler retried. The service's
`deployment_minimum_healthy_percent` is **50** for exactly this reason,
so a rollout can retire one old task and start one new one inside the
quota; put it back to 100 once the quota increase is in. When a deploy is
stuck on the sentence above, cancel the fleet's queued runs
(`gh run cancel`) so the scheduler gets the slot, and re-run them
afterwards. The `smoke` job is skipped when the waiter fails, so re-run
the deploy workflow once the service reports the rollout `COMPLETED`.

## Adding a domain later

Set `domain_name` in the environment's tfvars and follow step 4 of the
runbook: the stack creates the zone, the certificate, the `api.`/`www.`
aliases, `ssh.` on the NLB, and the SES identity, and
`STRATUM_PUBLIC_URL`/`STRATUM_SSH_PUBLIC_URL` follow. Mail switches from
the `null` transport to SES as `no-reply@<domain>` (`mail_from` to choose
another address under it) in the same apply. Removing the domain again is
the reverse and is destructive — the zone goes with it — which is what
the tfvars-in-CD rule above protects against.

## Hosted runners

Workflow jobs run on Fargate, in a VPC that exists for no other purpose.
`deploy/terraform/modules/runner/` is that VPC and everything in it, and
it is meant to be read as a single argument: a runner executes a `run:`
line a tenant wrote, so treat it as hostile code we chose to run, and make
the two properties we care about true by construction.

```
   runner VPC 10.41.0.0/16          NO peering, NO transit gateway,
   ──────────────────────           NO route to the app VPC at all
   private-0 / private-1  ──▶ Network Firewall ──▶ NAT ──▶ IGW ──▶ internet
   (Fargate tasks, no             (domain allowlist,
    public IP, no task role)       default drop)
        │
        └── ecr.api / ecr.dkr / logs interface endpoints + S3 gateway
            (the platform's own traffic, never the build's)
```

**1. A runner cannot reach another tenant's data.** It has no task role —
`aws_ecs_task_definition.runner` sets no `task_role_arn` at all, so the
container credentials endpoint returns nothing and there is no key in the
environment to steal. It holds no store credentials and no database
secret. It has no network path to the app VPC, because there is no
peering, no transit gateway, and no route. It talks to the control plane
over the public CloudFront URL exactly like a laptop does, carrying a
per-job token that is scoped to one repo and expires with the job.
Ephemeral storage is per-task, so two jobs never share a filesystem.

**2. A runner cannot reach a mining pool.** All egress leaves through AWS
Network Firewall with a stateful domain **allowlist** and a default drop.
A miner needs a pool; a pool is a domain that is not on the list; the
connection dies and an alert lands in `/stratum/prod/runner-firewall`.
The security group narrows it further — **no ingress rule of any kind**,
egress only to TCP 80/443 and UDP 53 to the VPC resolver. That last one
matters as much as the rest: pinning DNS to the VPC resolver is what stops
a build tunnelling to a resolver of its own, which the domain allowlist
would never see.

That is **layer 1 of four**, and the only one that is a property of the
network rather than of our code. Layer 2 refuses a `run:` line naming a
known miner or carrying a `stratum+tcp://`-family pool URL, at parse time,
before a task is launched. Layer 3 samples the step's process group while
it runs and ends the job with `"abuse": "mining"` if a miner is running
under a name the parser did not see. Layer 4 suspends the organisation's
hosted workflows when such a verdict arrives. They catch different things
on purpose: the allowlist holds against a miner nobody has heard of and
against one that arrives inside a dependency, while 2 and 3 catch a job
mining to a domain the allowlist *permits* — which is possible the moment
a tenant is allowed to reach their own hosts. Nothing in the terraform
changes when 2, 3 or 4 do.

Filtering is on TLS SNI and the HTTP Host header, not on IP addresses —
Network Firewall never pauses a connection for an out-of-band DNS lookup.
A build that dials a raw IP with no SNI matches no pass rule and is
dropped by the default action, which is the behaviour we want.

The allowlist is `runner_egress_allow_domains`, and it defaults to the
package and source registries a build realistically needs (GitHub, crates,
npm, PyPI, Debian, Docker Hub, ghcr) plus the control plane's own
CloudFront domain, added automatically. The default now also carries
what the official `actions/runner` agent dials —
`.actions.githubusercontent.com`,
`results-receiver.actions.githubusercontent.com`,
`codeload.github.com`, `.blob.core.windows.net` (where GitHub keeps
caches, artifacts and the runner's own downloads), the
`githubusercontent.com` hosts release assets and objects are served
from, `github-cloud.s3.amazonaws.com` and `.pkg.github.com`; `api.github.com`
was already inside `.github.com` — because a
[GitHub Actions job](#github-actions-runners) runs in this same VPC
behind this same firewall, and an agent that cannot reach its results
receiver fails every job in a way that says nothing about the job. The
hosts are listed by name rather than as `.githubusercontent.com` so
that `raw.githubusercontent.com`, a fetch-anything-anyone-hosts
endpoint, stays off the list. A leading `.` matches the domain
and all its subdomains. Extend it with
`-var 'runner_egress_allow_domains=[...]'` — and extend it deliberately,
because every entry is somewhere tenant code can send bytes.

Two details that look like accidents and are not:

- **The default action is `aws:drop_established`, not `aws:drop_strict`.**
  A domain rule cannot match until the engine has seen the SNI, which
  means the TCP handshake has to complete first. `drop_strict` would kill
  the SYN and block everything; `drop_established` allows the L3/L4
  handshake and then drops any established flow no pass rule matched.
  This is what AWS documents for a default-deny domain filter.
- **The image pull and the container log do not go through the
  firewall.** Fargate pulls the image and ships `awslogs` over the
  *task's* ENI, so without the `ecr.api` / `ecr.dkr` / `logs` interface
  endpoints and the S3 gateway endpoint that traffic would hit the
  allowlist and every task would die at PROVISIONING with an opaque
  `CannotPullContainerError`. Keeping the platform's traffic on endpoints
  also means the allowlist describes exactly one thing: the build's own
  egress.

  **Each of those endpoints carries a policy, and that is not
  belt-and-braces.** An endpoint is reached over the VPC's *local* route,
  so it is the one path out of a runner that does not pass the firewall.
  An unpolicied S3 gateway endpoint would hand tenant code a direct,
  uninspected route to the whole of S3. The policies scope them to exactly
  what the ECS agent needs to start the task: the ECR layer bucket for
  that region, pull actions on the runner repository alone, and
  `CreateLogStream`/`PutLogEvents` on the runner's own log group.

**Routing, which `terraform validate` cannot check for you.** Three tiers,
two AZs, one NAT. Outbound from a task: `aws_route_table.private[N]` sends
`0.0.0.0/0` to the firewall endpoint in AZ N; `aws_route_table.firewall[N]`
sends `0.0.0.0/0` to the single NAT; `aws_route_table.public` sends
`0.0.0.0/0` to the IGW. The return leg is the part worth checking:
`aws_route_table.public` also carries **one route per private subnet CIDR**
back to the firewall endpoint in *that subnet's* AZ. Without it, a reply
de-NATed in AZ 0 would go straight to a task in AZ 1 and the AZ-1 endpoint
would never see the other half of the flow it is tracking, so the stateful
engine would drop it. Because each private subnet has its own CIDR, the
public route table can steer each flow's return leg through the exact
endpoint that inspected its outbound leg.

**The one wire between the two halves** is a dispatch credential. The app
starts jobs with `ecs:RunTask`, and its own task role is empty (the
vendored SigV4 signer reads static env credentials only — the same
constraint that produced the store-credentials user), so
`modules/runner` creates an IAM user whose access key reaches the app
through Secrets Manager as `STRATUM_RUNNER_AWS_ACCESS_KEY_ID` /
`STRATUM_RUNNER_AWS_SECRET_ACCESS_KEY`. Its policy is the compensating
control: `RunTask` on the two task-definition families the module
registers — the Weft runner's and the GitHub runner's `<prefix>-github`,
below — conditioned on this one cluster; `StopTask`/`DescribeTasks` on
that cluster; `PassRole` on the runner execution role alone, conditioned
on `ecs-tasks.amazonaws.com`. It cannot register a task definition, so
it cannot smuggle in a different image, a task role, or an environment
of its choosing.

**The dispatcher's knobs**, all on the app and all optional:

| Variable | Default | What it sets |
|---|---|---|
| `STRATUM_RUNNER_POLL_SECS` | `5` | how often each app node sweeps and claims; `0` turns dispatch off on that node (runs are still recorded and stay queued for a node that does) |
| `STRATUM_RUNNER_LEASE_SECS` | `90` | how long a claimed job may go without a heartbeat before it is handed out again |
| `STRATUM_RUNNER_START_LEASE_SECS` | `600` | the lease granted at launch, sized for a cold task pulling its image |
| `STRATUM_RUNNER_MAX_ATTEMPTS` | `2` | launches a job gets before "the runner was lost" fails it |
| `STRATUM_RUNNER_OVERDUE_SLACK_SECS` | `300` | grace past a job's `timeout-minutes` before the sweep fails it on the runner's behalf; must exceed the time a healthy runner needs to upload its final log |
| `STRATUM_RUNNER_MINUTES_PER_MONTH` | unset in the binary; **`2000`** as terraform ships it | hosted-runner minutes an organisation may burn in a rolling 30 days when it has no override of its own. To the app, **unset, zero, or unparseable is unlimited** — the reason a typo here does not take the fleet down. The reference deployment does not leave it unset: `runner_minutes_per_month` defaults to 2000 and terraform puts it in the app's task definition, so a fleet applied from this repository is metered from the first apply. Usage is counted per job, rounded up, over a rolling window; a running job counts from its start. `orgs.ci_minutes_per_month` overrides it per organisation — see below |
| `STRATUM_CACHE_RETENTION_DAYS` | `10` in the binary; terraform's `cache_retention_days` is the S3 lifecycle rule on `cache/` in the store bucket and defaults to the same | how long a build-cache archive saved by a job on a Weft runner is restorable, from its save. The rule removes the objects; the dispatcher's sweep drops the rows the rule has emptied. Change both together, or rows outlive objects (a hit that 404s) or objects outlive rows (paid for, restorable by nobody) |
| `STRATUM_RUNNER_MAX_TIMEOUT_MINUTES` | `360` in the binary; **`360`** as terraform ships it | the largest `timeout-minutes` this fleet accepts. A file asking for more is refused at trigger time — the run is `failed` with `timeout-minutes: N exceeds this fleet's limit of 360` — rather than clamped, because a build told it may run for twelve hours and stopped at six fails in a way its author cannot explain. `runner_max_timeout_minutes` pins the same number the binary would have defaulted to, so that changing the binary's default cannot silently move a deployed fleet's ceiling |

**2000 minutes is a choice this repository makes, not a limit the
platform has.** It is set to sit roughly where GitHub's free tier does,
so that a fleet applied from here is metered by default rather than
handing whoever finds it an unbounded compute budget — the failure mode
of an unmetered default is discovered as a bill. Change
`runner_minutes_per_month` to whatever your deployment should give an
organisation, or set it to `0` for unmetered. `deploy/compose.yml` and
`deploy/proot/stack.sh` — the prod-parity stack, and the same stack as
`deploy-validation` brings it up under PRoot — set the same two values,
so the smoke test runs against the metering the fleet runs against.

### GitHub Actions runners

A workflow kept on GitHub can send a job here with `runs-on: weft`
([the docs page](../web/site/src/pages/docs/github-runners.md) is the
customer's side). On this side it is **a second task definition and a
second image in the same VPC**, and nothing else new: the same subnets,
the same security group, the same firewall, the same execution role, no
task role, and the same dispatch credential.

- **The task definition** is the `<prefix>-github` family, registered by
  `modules/runner` beside the Weft runner's. Its container is named
  `runner` — `STRATUM_RUNNER_ECS_GITHUB_CONTAINER` if you change it —
  and it runs the official `actions/runner` agent from
  `Dockerfile.github-runner`, which collects its just-in-time
  registration from the control plane once at boot
  (`GET /v1/github-runner/jobs/{id}/jitconfig`, a single-use runner
  token), takes exactly one job, and exits. The three sizes (`weft`,
  `weft-2x`, `weft-4x`) are `cpu`/`memory` overrides on `RunTask` —
  1 vCPU / 2 GiB, doubled and quadrupled, all legal Fargate pairs — so
  one task definition serves all three.
- **The image** has its own ECR repository, bootstrapped the same way as
  the Weft runner's: `terraform apply -target` the repository, then
  `docker build -f Dockerfile.github-runner` and push `:bootstrap`
  before the first full apply. After that, `deploy.yml` rebuilds both
  runner images on every deploy and pushes `:bootstrap` (and the
  commit's sha) itself — the tag the task definitions name, which
  Fargate pulls fresh for every task, so a merged Dockerfile change is
  on the fleet as soon as the deploy has run. The `ecr.dkr` endpoint policy has to
  admit both repositories — an endpoint policy scoped to the Weft
  runner's repository alone leaves every GitHub task dying at
  PROVISIONING with `CannotPullContainerError`, the same failure the
  endpoints exist to prevent.
- **The app learns of it** through `STRATUM_RUNNER_ECS_GITHUB_TASK_DEFINITION`,
  which terraform puts in the app's task definition from the module's
  output; absent, the feature is off and every job asking for a Weft
  size is recorded as refused with "this Weft deployment has no GitHub
  Actions runner configured". The dispatcher's own knobs are
  `STRATUM_GITHUB_RUNNER_POLL_SECS` (default `5`; `0` off on that node)
  and `STRATUM_GITHUB_RUNNER_IDLE_SECS` (default `600`: a runner GitHub
  hands no job within that window is stopped, removed from GitHub and
  the job marked `abandoned`, unbilled). `STRATUM_RUNNER_MAX_TIMEOUT_MINUTES`
  caps a GitHub job the same way it caps a Weft one — past it the task
  is stopped and the job billed for what ran.
- **The GitHub App** must hold `Contents: write` (a push to a mirror is
  forwarded to its origin under the installation token; an installation
  that has not approved it has its pushes refused naming the
  permission), `Administration: write` and
  `Actions: write` on repositories, and be subscribed to `workflow_job`
  — see `docs/LAUNCH.md`; its own `installation` events arrive without a
  subscription. Neither the terraform nor
  the app can grant those; an installation that has not approved them
  has its jobs refused naming the permission.

What the GitHub runner cannot do is what the Weft runner cannot do,
and for the same reason: no Docker daemon (Fargate has no privileged
mode, so `container:`, `services:` and Docker-based actions fail), no
egress to a domain that is not on the allow-list, Linux x64 only.

The manual gate is `scripts/manual-github-runners.sh` — see `CLAUDE.md`
for what it proves and why it has to run under the deployed App against
both a personal and an organisation repository.

### Self-hosted runners: nothing new to deploy

Self-hosted runners are the customer's machines, so there is **no
infrastructure on this side of them** — no VPC, no cluster, no task
definition, no image to push, and nothing in `deploy/terraform` changes.
A runner is a binary somebody runs on their own hardware, and it reaches
the app over the same public CloudFront URL a laptop uses, holding a
credential it exchanged a registration token for. There is no inbound
path to a runner from here, which is the property that makes this cheap:
the app never connects to a customer's network.

Two consequences worth knowing before the first support question:

- **They cost the organisation no minutes**, so the metering above and
  `runner_minutes_per_month` say nothing about them. An organisation
  that is out of minutes still runs its self-hosted workflow files.
- **The mining watch still kills the job and does not suspend the
  organisation.** Suspension exists to stop somebody spending *our*
  compute; on their hardware there is nothing of ours to protect, and
  switching their hosted CI off over it would be a punishment for a
  thing that cost us nothing. The verdict is still audited.

One knob, on the app:

| Variable | Default | What it sets |
|---|---|---|
| `STRATUM_RUNNER_CLAIM_WAIT_MS` | `20000` | how long `POST /v1/runners/claim` holds an empty request open before answering `204`. The runner polls again immediately, so this is really "how often an idle runner costs you a request" — the server checks for work every 500 ms while it waits, so raising it does not make a job wait longer to start. Lower it only in tests. Anything between the app and a runner — a load balancer, a customer's proxy — needs an idle timeout above this, or every empty poll is cut and a healthy runner flaps between `online` and `offline` |

**The per-organisation overrides are SQL, on `orgs`.** They are on the
table rather than in a config file because they are per-tenant and have
to be changeable without a deploy, and `NULL` in each of them keeps
meaning *whatever the server's default is today* rather than freezing
today's default into every row. There is no route and no dashboard
control for any of them: there is no operator role on this server to hang
one off, and inventing one in passing would be a security surface nobody
reviewed.

```sql
-- Concurrency: how many jobs this organisation may have running at once.
-- NULL = the server default (4).
UPDATE orgs SET ci_concurrency = 12 WHERE name = 'acme';

-- Minutes: the rolling-30-day hosted-runner budget.
-- NULL = STRATUM_RUNNER_MINUTES_PER_MONTH; 0 = unlimited.
UPDATE orgs SET ci_minutes_per_month = 5000 WHERE name = 'acme';
UPDATE orgs SET ci_minutes_per_month = NULL WHERE name = 'acme';  -- back to the default
```

**Clearing an abuse suspension. Read the reason first.** A verdict
carrying `"abuse"` — today only the runner's mining watch produces one —
switches hosted workflows off for the whole organisation and cancels
everything it has running. Clearing it is the operator saying the
organisation may spend the fleet's compute again, so find out what it did
before you do:

```sql
SELECT name, ci_suspended_reason, to_timestamp(ci_suspended_at / 1000)
  FROM orgs WHERE ci_suspended_reason IS NOT NULL;
```

The reason is the runner's own sentence — `mining software detected:
xmrig` — and the audit log has the rest under the `workflow.suspended`
action: the job, the run, and the repository it was in. Then:

```sql
UPDATE orgs SET ci_suspended_reason = NULL, ci_suspended_at = NULL
  WHERE name = 'acme';
```

Both columns together: `ci_suspended_reason IS NULL` is the entire "not
suspended" test, so leaving `ci_suspended_at` set would be a stale
timestamp on a live organisation. Nothing restarts the runs that were
cancelled; the next push starts new ones.

**Cancelling a job is bounded by `stopTimeout`, which is set to 10 s.**
When a run is superseded by a newer push, or a person cancels it, the app
calls `ecs:StopTask`; ECS sends SIGTERM to pid 1, waits `stopTimeout`, then
SIGKILLs. The runner handles SIGTERM itself — it installs a handler (see
`crates/stratum-runner/src/signals.rs`) and takes the step's process group
down with it — so the signal reaches it whether it is pid 1 or a child of
the init; the kernel only drops signals pid 1 has *no* handler for.
`initProcessEnabled = true` is not what delivers it, it is there to reap
grandchildren a step orphans. All `stopTimeout` decides is how long ECS
waits before SIGKILL, so the worst case for a cancelled job is ten seconds
of extra compute, and the job token is already revoked for all of it. ECS's implicit default is 30 s;
leaving it implicit was three times that on every superseded push. The
same number is `docker stop -t 10` in `deploy/fake-ecs/fake-ecs.py`, so
the manual stack cancels on the deployment's clock — change one and change
the other.

**Build logs** stream to the control plane and land in the store bucket
under `ci/logs/`, aged out by the `expire-ci-logs` lifecycle rule
(`ci_log_retention_days`, default 90). Only the runner's own one-line-per-
phase stderr goes to CloudWatch.

**First image**, mirroring the app's bootstrap in step 3 above — ECR must
hold `:bootstrap` before the first full apply. This is the only hand
push: every deploy after it rebuilds both images and moves `:bootstrap`
to the merged commit (see the `deploy` job in `deploy.yml`, which reads
the repositories from the `runner-ecr-repository` and
`github-runner-ecr-repository` SSM parameters `modules/runner` publishes).

```sh
cd deploy/terraform
terraform apply -target=module.runner.aws_ecr_repository.runner
aws ecr get-login-password | docker login --username AWS --password-stdin <runner-ecr-url>
docker build -f Dockerfile.runner -t <runner-ecr-url>:bootstrap .
docker push <runner-ecr-url>:bootstrap
# And the GitHub Actions runner image, to its own repository — the
# module refuses a first apply whose task definition names an image
# that is not there. Fargate is x64, so build for it explicitly.
terraform apply -target=module.runner.aws_ecr_repository.github_runner
docker build --platform linux/amd64 -f Dockerfile.github-runner -t <github-ecr-url>:bootstrap .
docker push <github-ecr-url>:bootstrap
```

`terraform output runner_ecr_repository_url` and
`terraform output github_runner_ecr_repository_url` print the two URLs.

**What it costs to have this switched on**, before a single job runs
(illustrative us-east-1 on-demand, per month):

| | | |
|---|---|---|
| Network Firewall endpoints | 2 × $0.395/h | **$577** |
| NAT gateway | 1 × $0.045/h | **$33** |
| Interface endpoints (ecr.api, ecr.dkr, logs × 2 AZ) | 6 × $0.01/h | **$44** |
| | | **~$654/month idle** |

Plus $0.065/GB through the firewall, $0.045/GB through the NAT, and about
$0.05 per job-hour of Fargate at the default 1 vCPU / 2 GiB. **The
firewall is the bill**, and it is a fixed cost that does not scale down
with an idle fleet. Collapsing to a single AZ (one firewall endpoint, one
private subnet) takes the idle figure to roughly $343/month and costs the
runner fleet its AZ redundancy; that is the trade to make if hosted CI is
a small fraction of the deployment. Turning hosted runners off entirely
means not creating the module — the app simply has no dispatcher target
and jobs stay queued.

**The process ceiling is compiled into the runner, because Fargate has
nowhere to put it.** A job may have **4096** processes at once. There is
no `pidsLimit` and no `nproc` ulimit on the runner *container* — Fargate
takes neither: `pidsLimit` is EC2-launch-type only, and a Fargate task
definition accepts `nofile` as its only ulimit — so the runner sets
`RLIMIT_NPROC` itself, between `fork` and `exec`, on the step's own
process (`MAX_PROCS` in `crates/stratum-runner/src/steps.rs`).
`RLIMIT_NPROC` is per *uid* and the container runs exactly one job as uid
10002, so the only processes counted against the bound are that job's
own. It is clamped to whatever hard limit the process already holds,
since an unprivileged process may only lower one.

The number is deliberately generous: a parallel build legitimately runs
hundreds of compilers, and a bound that failed an honest `make -j$(nproc)`
would be a worse bug than the fork bomb it prevents. It is a compile-time
constant on the deployed fleet — `STRATUM_RUNNER_MAX_PROCS` overrides it
in the runner's environment, and the dispatcher does not pass it. Set
`runner_max_procs` and terraform puts it in the task definition's
environment; leave it `null`, the default, and the task definition
carries nothing, so the bound is whatever the image enforces rather than
a literal in terraform that would silently shadow a future default.
Either way changing it is a task-definition revision or a new image, not
a config change on the app.

Past the ceiling, `fork` fails the way it does on any busy machine and
the step sees that error. Everything below it is still the task's own
`cpu`/`memory` (`runner_task_cpu`, `runner_task_memory`; 1 vCPU / 2 GiB by
default) and the fact that there is exactly one job in a task: a bomb
that stays under 4096 processes exhausts its own task, that job fails,
the container is discarded, and there is no second job in it to reach.
The blast radius is one tenant's own build, which is the same radius as
an honest build that runs out of memory.

**Proving the ECS contract on a real cluster** — `scripts/manual-ecs.sh`,
a manual gate in the same family as `scripts/manual-s3.sh` and
`scripts/manual-ci.sh`, and manual for the same reason: it needs the
deployment's own credentials, which do not belong in CI.

```sh
scripts/manual-ecs.sh all               # launch + refusals + taskdef + stop
scripts/manual-ecs.sh stop <task-arn>   # the SIGTERM ending, on a real job
```

Everything that launches or stops a hosted job goes through
`workflow/executor.rs`, and two of its decisions are beliefs about AWS
until something checks them against AWS. The first is how a refusal is
classified: `Capacity` re-queues the job without counting an attempt —
right for a region that is genuinely full, and a silent infinite retry
for a policy edit that took `ecs:RunTask` away — while `Refused` fails the
job and shows an operator why. The second is that `StopTask` actually
stops a runner: every cancel path ends there, and what it has to mean is
SIGTERM reaching the runner, the runner exiting 0 without reporting a
verdict, and doing it inside `stopTimeout` instead of being SIGKILLed at
the end of it.

Run it with the **dispatch credential from Secrets Manager**, not an
admin key: half of what is being checked is that the least-privilege
policy still admits the exact `RunTask` body the dispatcher sends, and an
admin key cannot fail that. It reads its environment with the same
variable names the app boots with (`STRATUM_RUNNER_ECS_CLUSTER`,
`…_TASK_DEFINITION`, `…_SUBNETS`, `…_SECURITY_GROUP`,
`STRATUM_RUNNER_AWS_*`, `STRATUM_RUNNER_URL`). Two optional credentials
each unlock one case that is otherwise only a NOTE:
`STRATUM_ECS_ADMIN_*` (read-only, with `ecs:DescribeTaskDefinition`,
which the dispatch policy deliberately lacks) checks what terraform
actually registered — no task role, uid 10002, `stopTimeout`,
`initProcessEnabled`, the container name the overrides address; and
`STRATUM_ECS_DENIED_*`, a credential that genuinely lacks `ecs:RunTask`,
is the only way to see the AccessDenied refusal an operator actually
meets, and it cannot fail under a key that holds the permission.

What it will not claim: the capacity family (`RESOURCE:MEMORY`, `AGENT`,
the account's vCPU-quota sentence, `ThrottlingException`) is printed as
NOTEs, because a busy region cannot be ordered up, a quota cannot be
spent on purpose without holding real tasks, and spending the account's
API budget to see a throttle is abuse of it. The quota sentence was
observed once, unasked, on 2026-09-07, and the fake and the unit test
carry it verbatim. Nor does `launch` prove the allowlist *drops* anything — the
container's entrypoint is the runner and an ECS override cannot replace an
entrypoint, so the negative needs a real job with a `run:` step. What
`launch` does prove is the other half: a runner that exits **2** reached
the control plane and was refused a job that does not exist, which is DNS,
NAT, the firewall's allowlist for the control-plane domain and TLS all
working; one that exits **1** could not reach it at all, and every hosted
job on that deployment is failing the same way.

## Limitations (v1, by design — each is a decision, not an accident)

| Limitation | Why / mitigation |
|---|---|
| Store credentials are a static IAM user access key | The vendored signer supports only env credentials (no IMDS/task role); missing creds silently go anonymous. Scoped to one bucket; rotate by re-applying (`terraform taint aws_iam_access_key.store`). |
| `rds.force_ssl=0` on Aurora | The control-plane client is deliberately NoTls; traffic never leaves the VPC and 5432 admits only the app SG. |
| CloudFront caps request duration at 60 s (origin read timeout) | Giant cold clones can outrun it: use the ALB URL directly (`alb-url` SSM param / terraform output), or SSH — neither has the 60 s cap. ALB/NLB idle timeouts are 350 s, above the store's 300 s op ceiling. |
| `stopTimeout` 120 s (Fargate max) | Ops still running 120 s after a task is told to stop die at SIGKILL. SIGTERM drains gracefully (proved by the SIGTERM e2e harness). |
| A cancelled runner job costs up to 10 s more compute | `stopTimeout` on the runner container: how long ECS waits after SIGTERM before SIGKILL. The runner handles SIGTERM itself and stops at the current step; anything still running at 10 s is SIGKILLed. Lower it and a legitimate mid-step cleanup gets killed instead. |
| The 4096-process ceiling is compiled into the runner, not in the task definition | Fargate takes no `pidsLimit` and no `nproc` ulimit (`nofile` is the only one it accepts), so the runner sets `RLIMIT_NPROC` on the step itself. Changing it needs a new image or a task-definition revision carrying `STRATUM_RUNNER_MAX_PROCS` (`runner_max_procs`), not an app config change. Below the ceiling, one job per task and an ephemeral container mean the blast radius is that tenant's own build. |
| Clearing an abuse suspension is SQL, with no route and no audit trail of its own | There is no operator role on this server to hang a route off, and adding one in passing would be an unreviewed security surface. The suspension itself *is* audited (`workflow.suspended`); the clearing is a DBA action. |
| Mirror seeds re-clone after task churn | `STRATUM_DATA_DIR` is ephemeral; first fetch after churn pays the re-seed. Accepted for stateless tasks. |
| Smoke orgs accumulate | There is deliberately no org-delete API; each deployed smoke uses a unique `smoke-<run-id>` org and deletes its repos. Org rows are inert. |
| `/metrics` answers 404 through the ALB | The endpoint is unauthenticated Prometheus text; scrape tasks directly in-VPC. |
| One DB connection per task | The control plane is point reads/writes behind a mutex (measured µs); Aurora ACUs scale with task count, not connection storms. |
| ALB is not origin-locked to CloudFront | The ALB serves nothing CloudFront does not; direct HTTP to it is the documented long-op path. Lock down with a custom-header rule when a domain lands. |
| Signed-URL validation at the edge is not covered by CI | CloudFront verifies the signature itself, so no test in CI can exercise it. Everything up to the edge is covered end to end with real git (advertise, sign, fetch, top-up, fsck), and `deploy/smoke.sh` adds the operator-side proof against a real deployment: an advertised URL must answer 200 and a tampered signature 403. |
| An opted-in client makes the CDN a hard dependency for that clone | git does not fall back to the server for an advertised pack. Mitigated by confirming the object exists before advertising, and by `STRATUM_CDN_ENABLED=0` as a fleet-wide kill switch. Clients that have not set `fetch.uriprotocols` are never affected. |
| Offload pauses after every push until the packer catches up | A lagging pack cannot be served: the inline remainder would be the pushers' thin packs, whose delta bases sit in the CDN pack, and git indexes the inline pack first. Clones stay correct (served inline); only the saving pauses. Lower `STRATUM_CDNPACK_POLL_SECS` to shorten the window. |
| Single NAT gateway | Cost over AZ redundancy for egress; S3 traffic (the volume) bypasses NAT via the gateway endpoint. Store serving continues if the NAT's AZ dies; mirror origin sync pauses. |
| Network Firewall costs ~$577/month whether or not CI is busy | Two endpoints at $0.395/h each is the price of a default-deny egress filter that is a property of the network, not of the image. Halve it by collapsing the runner VPC to one AZ; do not replace it with a security group, which cannot filter by domain. |
| The runner egress allowlist will block a build that needs an unlisted domain | By design — the failure is a dropped connection and an alert in the firewall log naming the domain. Add it to `runner_egress_allow_domains` after reading what wants it. |
| VPC endpoints bypass the firewall by construction | An endpoint is on the VPC local route, so firewall inspection cannot apply. Mitigated by an endpoint policy on every one of them, scoped to the ECR layer bucket, the runner repository, and the runner log group. Adding an endpoint here without a policy reopens an uninspected egress path. |
| Domain filtering is on SNI/Host, not IP | Network Firewall does no out-of-band DNS lookup, so a build connecting to a bare IP matches no pass rule and is dropped by the default action. A build that manipulates SNI to a permitted name still reaches only whatever answers on that IP; add IP-based rules if that matters to you. |
| The runner dispatch credential is a static IAM user access key | Same constraint as the store credentials: the vendored SigV4 signer has no role support. Scoped to RunTask/StopTask/DescribeTasks on one cluster and PassRole on one execution role; it cannot register a task definition. Rotate with `terraform taint module.runner.aws_iam_access_key.dispatch`. |
