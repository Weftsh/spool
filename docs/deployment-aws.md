# Deploying spool on AWS

`deploy/terraform` is a reference deployment of spool in your own AWS
account: the server on ECS Fargate behind CloudFront and a network load
balancer, Aurora PostgreSQL for the control plane, and an S3 bucket for
repositories. This document is the architecture, the first deployment
step by step, how to ship a new version, and the known limitations.
[operations.md](operations.md) is the reference for configuring and
running the server itself; read it alongside.

You need an AWS account you can create IAM roles and users in,
terraform ≥ 1.10, the AWS CLI, Docker (to build the image), and Python 3
(the helper scripts use it). A domain name is optional.

## Architecture

```
                       ┌────────────────────────┐
   https (443)         │  CloudFront            │   TLS (your certificate,
  dashboard / REST ───▶│  honours origin        │   or CloudFront's own)
  git over HTTP        │  Cache-Control; passes │──▶ ALB :80 ──▶ ECS tasks :8080
                       │  Authorization through │      │
                       └────────────────────────┘      │  idle 350 s, /healthz,
   ssh (22)            ┌────────────────────────┐      │  /metrics → 404
  git clone/push ─────▶│  NLB TCP:22            │──▶ tasks :2222
                       └────────────────────────┘
  ECS Fargate (2–10 tasks; scales on CPU 60% and 500 requests/target)
      │                         │
      ▼                         ▼
  Aurora Serverless v2      S3 store bucket (through a VPC gateway
  PostgreSQL 16             endpoint: immutable segments, manifests
  (control plane)           changed only by compare-and-swap)
```

- **One binary per task** serves the dashboard, the REST API, git over
  smart HTTP v2 and SSH, and every background worker. Tasks are
  stateless; each has 100 GiB of ephemeral storage for
  `STRATUM_DATA_DIR` (mirror seed clones, which rebuild themselves after
  a task is replaced).
- **Networking.** A two-AZ VPC: load balancers in the public subnets,
  tasks and the database in the private ones, one NAT gateway for
  outbound traffic (mirror fetches from GitHub, outgoing webhooks, image
  pulls). Clone traffic to S3 goes through the gateway endpoint, not the
  NAT. Only the tasks' security group may reach PostgreSQL.
- **Git clients need nothing special.** `git clone https://…` is git's
  smart HTTP transport through CloudFront, and `git clone ssh://…` goes
  to the NLB with a registered key. SSH needs no certificate, so it is a
  fully encrypted git transport even before you have a domain.
- **What CloudFront caches.** It honours the server's own
  `Cache-Control`: the dashboard's hashed bundles are cached for a year;
  the git data plane and every authenticated response are marked
  `no-cache` and always go to the origin (and CloudFront never caches a
  `POST`, which is what a fetch is). Authorization is enforced by the
  server against the database on every request, so revocation is
  immediate. **Origin Shield** in the deploy region collapses cache
  misses from many edge locations into one origin fetch.
- **Secrets** — the database URL, the store's access key, the webhook
  secret, the SSH host key, the CDN signing key and the GitHub App's
  credentials — live in Secrets Manager and are injected by the ECS
  execution role. The task role is deliberately empty.
- **The store credential is an IAM user's access key**, scoped to the
  one bucket (and to `ses:SendEmail` when the stack sends mail): the
  server's S3 client signs with static keys only and does not read task
  roles.
- **CDN offload** for clones is on: a second CloudFront origin serves
  prebuilt packs straight from the bucket to clients that ask for them.
  See [CDN offload](#cdn-offload).

What it costs is mostly: two Fargate tasks (2 vCPU, 8 GB each) running
all the time, Aurora Serverless v2 between 0.5 and 4 ACUs, one NAT
gateway, an ALB and an NLB, and CloudFront transfer out. Everything is
sized in `envs/<env>.tfvars`; see `deploy/terraform/variables.tf` for the
knobs.

## Prove it locally first

The deployment runs the same image as the one-box compose stack, and the
smoke test that proves a deployment works runs unchanged against both.
Before trusting a deploy, run it locally:

```sh
./deploy/dev-host-key.sh
docker compose up -d --build --wait
BASE_URL=http://127.0.0.1:8080 \
SSH_ENDPOINT=ssh://git@127.0.0.1:2222 \
BOOTSTRAP_CMD="docker compose exec -T spool stratum-server admin bootstrap" \
  ./deploy/smoke.sh
docker compose down
```

That is the real image against PostgreSQL and MinIO, with the real git
client cloning (and `fsck --full --strict`ing), pushing and reading back
over both transports. `deploy/smoke.sh`'s header lists the optional legs.
To add a self-hosted runner taking a job, build the runner image and run
it on the host's network, so it reaches the server at the same loopback
address the server gives out:

```sh
docker build -f Dockerfile.runner -t weft-runner:local .
SMOKE_SELF_HOSTED=1 SMOKE_RUNNER_NETWORK=host \
BASE_URL=http://127.0.0.1:8080 SSH_ENDPOINT=ssh://git@127.0.0.1:2222 \
BOOTSTRAP_CMD="docker compose exec -T spool stratum-server admin bootstrap" \
  ./deploy/smoke.sh
```

The leg that probes a public GitHub origin needs the server container to
reach the internet; `SMOKE_ORIGIN_PROBE=0` skips it on a machine that
cannot.

## Before you start

- **Fargate vCPU quota.** A new account may run only **6** Fargate
  vCPUs at once (Service Quotas, "Fargate On-Demand vCPU resource
  count"). Two 2-vCPU tasks use 4, a rolling deploy briefly wants more,
  and a one-off admin task is another 2. The service's rollout is set to
  replace one task at a time so it fits, but request 16 or more before
  you go live; it costs nothing until used.
- **SES sandbox.** With a domain, the stack sends mail through Amazon SES
  from that domain. A new account's SES only delivers to addresses it has
  verified until you request production access in the SES console. Do
  that early; it can take a day. (Or send through your own SMTP relay
  instead: [step 9](#9-mail).)
- **Region.** Everything goes in `aws_region` except the CloudFront
  certificate, which is always issued in us-east-1.

## First deployment

### 1. Bootstrap: the state bucket

The root stack keeps its state in S3. The bootstrap stack creates that
bucket (versioned, private) with local state, once per account:

```sh
cd deploy/terraform/bootstrap
terraform init
terraform apply                       # the state bucket only
terraform output -raw state_bucket
```

Keep `deploy/terraform/bootstrap/terraform.tfstate` somewhere safe — it
is the only record of this stack — or move it into the bucket it made
afterwards.

If you will deploy from GitHub Actions, the same stack can also create
two roles a workflow assumes through GitHub's OIDC provider, so no AWS
key is stored in GitHub; see
[Deploying from GitHub Actions](#deploying-from-github-actions). You can
add them later by re-applying with `-var github_repo=OWNER/REPO`.

### 2. The environment's settings

Each environment is one file, `deploy/terraform/envs/<env>.tfvars`, and
the file name is the environment:

```sh
cd deploy/terraform
cp envs/example.tfvars envs/prod.tfvars
$EDITOR envs/prod.tfvars
```

Set at least `env` (equal to the file name) and `aws_region`. The rest
of the file is commented: `domain_name`, sizes, `deletion_protection`,
`github_app_slug`, and `extra_environment`/`extra_secrets` for any other
server setting from [operations.md](operations.md#configuration-reference).
`project` (default `spool`) prefixes every name the stack makes —
`spool-prod-*` resources, `/spool/prod/…` parameters, `spool/prod/…`
secrets — so several environments can share one account.

`scripts/tf.sh <env> <command>` runs terraform for one environment: it
initialises the backend with that environment's state key
(`<project>/<env>.tfstate` in the bootstrap bucket) and passes
`envs/<env>.tfvars` to every plan and apply, so the two cannot be mixed
up. Every command below uses it. By hand, the equivalent is:

```sh
terraform init -reconfigure \
  -backend-config="bucket=<state-bucket>" \
  -backend-config="key=spool/prod.tfstate" \
  -backend-config="region=<region>" \
  -backend-config="use_lockfile=true"
terraform apply -var-file=envs/prod.tfvars
```

If the two ever disagree anyway, `modules/env-lock` refuses the plan:
the state records the environment it was first applied as.

### 3. The domain (optional)

Without `domain_name` the stack answers on CloudFront's generated
`dxxxx.cloudfront.net` hostname and the NLB's hostname, which is enough
to try it. With one — say `git.example.com` — it creates a Route 53 zone
for that name, a certificate for it, `ssh.git.example.com` for SSH, and
an SES identity to send mail from, and `STRATUM_PUBLIC_URL` follows.

The certificate cannot be issued until the zone is delegated, so:

- **If the parent zone (`example.com`) is in the same AWS account**, set
  `parent_domain_name = "example.com"` as well. The stack writes the
  delegation itself and the first apply is one step.
- **Otherwise**, create the zone first and delegate it by hand:

  ```sh
  scripts/tf.sh prod apply -target='module.dns[0].aws_route53_zone.this'
  scripts/tf.sh prod output name_servers
  ```

  Add those four name servers as an `NS` record for `git.example.com`
  wherever `example.com` is served, and wait until
  `dig +short NS git.example.com` answers with them. A full apply before
  that is not wrong, only slow: the certificate validation waits up to
  thirty minutes and then fails, and the next apply picks up where it
  stopped.

You can also add a domain later; see
[Adding a domain later](#adding-a-domain-later).

### 4. The first image

The service starts with the image tag `image_tag` (default `bootstrap`)
from the ECR repository the stack creates, so that tag has to exist
before the service does:

```sh
scripts/tf.sh prod apply -target=module.app.aws_ecr_repository.app
REPO=$(scripts/tf.sh prod output -raw ecr_repository_url)
aws ecr get-login-password | docker login --username AWS --password-stdin "${REPO%%/*}"
docker build --platform linux/amd64 -t "$REPO:bootstrap" .    # at the repository root
docker push "$REPO:bootstrap"
```

Fargate runs x86_64 here, so build for `linux/amd64` even on an ARM
machine.

### 5. Everything else

```sh
scripts/tf.sh prod apply
scripts/tf.sh prod output
```

This takes a while; CloudFront and Aurora are the slow parts. The
outputs are the base URL (your domain, or CloudFront's hostname), the SSH
endpoint, and the direct ALB URL.

### 6. The first organisation and the first person

```sh
STRATUM_ENV=prod deploy/admin-ecs.sh bootstrap --org acme
STRATUM_ENV=prod deploy/admin-ecs.sh user-create --org acme \
  --email you@example.com --password '…'
```

`deploy/admin-ecs.sh` runs `stratum-server admin …` as a one-off
Fargate task with the service's own image, secrets and network, waits
for it, and prints its result from the task's log (it takes a minute).
`bootstrap` prints an `org:admin` API token, shown once. Sign in to the
base URL with the account `user-create` made; invite everyone else from
the dashboard. `STRATUM_PROJECT` selects a non-default `project`.

### 7. Smoke test

```sh
BASE_URL=$(scripts/tf.sh prod output -raw base_url) \
SSH_ENDPOINT=$(scripts/tf.sh prod output -raw ssh_endpoint) \
BOOTSTRAP_CMD="env STRATUM_ENV=prod deploy/smoke-bootstrap-ecs.sh" \
  deploy/smoke.sh
```

It creates an organisation named `smoke-<id>`, proves health and
readiness, REST, a clone (`fsck --full --strict`) and a push over HTTPS
and over SSH, that the server can reach a public HTTPS git origin, CDN
offload including CloudFront refusing a tampered signature, and the
dashboard, then deletes its repositories. (There is no organisation
delete, so the empty `smoke-…` organisation remains.)

### 8. The GitHub App (optional)

Private GitHub mirrors, forwarding pushes to GitHub, issue imports,
GitHub Actions verdicts and signing in with GitHub need a GitHub App of
your own; [operations.md](operations.md#github-app) says how to create
it, with `PUBLIC` being your base URL. Then put its five credentials in
the secret the first apply created blank, and name the App:

```sh
aws secretsmanager put-secret-value \
  --secret-id "$(scripts/tf.sh prod output -raw github_app_secret_name)" \
  --secret-string "$(jq -n --arg id 123456 --rawfile pem acme-spool.private-key.pem \
      --arg wh '<webhook secret>' --arg cid 'Iv1.…' --arg cs '<client secret>' \
      '{app_id:$id,private_key:$pem,webhook_secret:$wh,client_id:$cid,client_secret:$cs}')"
# envs/prod.tfvars: github_app_slug = "acme-spool"
scripts/tf.sh prod apply
deploy/roll.sh                         # put the new settings into service
```

The plan refuses while any of the five is blank: a server with an app id
and no key refuses to boot, and one with a blank webhook secret drops
every delivery from GitHub, which looks like GitHub not calling.

### 9. Mail

With a domain, the stack sends as `no-reply@<domain_name>` through SES
(`mail_from` picks another address under the domain) and publishes the
DKIM, SPF (custom MAIL FROM) and DMARC records for it. Request SES
production access (see [Before you start](#before-you-start)).

To use your own SMTP relay instead, set `ses_mail = false` and configure
SMTP through the extra settings, keeping the password in a secret of
your own:

```hcl
ses_mail = false
extra_environment = {
  STRATUM_MAIL_TRANSPORT = "smtp"
  STRATUM_MAIL_FROM      = "git@example.com"
  STRATUM_MAIL_SMTP_HOST = "smtp-relay.internal:25"
  STRATUM_MAIL_SMTP_USER = "spool"
}
extra_secrets = {
  STRATUM_MAIL_SMTP_PASSWORD = "arn:aws:secretsmanager:us-east-1:111111111111:secret:smtp-password-AbCdEf"
}
```

The server's SMTP client has no STARTTLS and refuses to send credentials
to anything but loopback unless `STRATUM_MAIL_SMTP_ALLOW_CLEARTEXT_AUTH`
is `1`; only do that for a relay inside your network.

### 10. Single sign-on (optional)

Register the server with your identity provider as
[operations.md](operations.md#single-sign-on) describes — the callback is
`https://<domain_name>/v1/auth/sso/callback` — and pass the settings the
same way, the client secret from a secret of your own:

```hcl
extra_environment = {
  STRATUM_OIDC_ISSUER    = "https://login.microsoftonline.com/<tenant-id>/v2.0"
  STRATUM_OIDC_CLIENT_ID = "<application-id>"
  STRATUM_OIDC_ORG       = "acme"
  STRATUM_OIDC_NAME      = "Microsoft"
  STRATUM_OIDC_ALLOWED_DOMAINS = "acme.com"
}
extra_secrets = {
  STRATUM_OIDC_CLIENT_SECRET = "arn:aws:secretsmanager:us-east-1:111111111111:secret:oidc-client-AbCdEf"
}
```

With SSO configured, signing in with a password is off unless
`STRATUM_SSO_ONLY = "false"` says otherwise. Make the first owner with
`deploy/admin-ecs.sh user-create --org acme --email you@acme.com
--no-password` before signing in, so that the first SSO sign-in finds
an owner rather than making a member.

## Deploying a new version

Terraform creates the service; it does not roll it afterwards.
`deploy/roll.sh` does: it registers a revision of the task definition
with the new image, points the service at it, and waits until the
rollout is steady.

```sh
TAG=$(git rev-parse --short HEAD)
REPO=$(scripts/tf.sh prod output -raw ecr_repository_url)
aws ecr get-login-password | docker login --username AWS --password-stdin "${REPO%%/*}"
docker build --platform linux/amd64 -t "$REPO:$TAG" . && docker push "$REPO:$TAG"
STRATUM_ENV=prod deploy/roll.sh "$TAG"
```

ECS starts new tasks beside the old ones, moves traffic once they are
healthy, and drains the old ones. The first new task migrates the
database; old tasks keep serving meanwhile. If the new tasks never
become healthy — an image that does not start, a migration that fails —
the service's circuit breaker rolls back to the previous revision and
`roll.sh` exits non-zero saying so; the app's log group
(`/<project>/<env>/app`) says why. Back up the database before an
upgrade whose release notes mention a migration
([operations.md](operations.md#upgrades-and-migrations)).

**Configuration changes** work the same way. A terraform apply that
changes the task definition — a new `extra_environment` entry, a GitHub
App — registers a new revision but, deliberately, does not move the
service onto it. `deploy/roll.sh` with no argument rolls the image
already running onto the latest configuration:

```sh
scripts/tf.sh prod apply
STRATUM_ENV=prod deploy/roll.sh
```

## Deploying from GitHub Actions

Applied with `-var github_repo=OWNER/REPO`, the bootstrap stack also
creates:

- **`<project>-cd`**, which a workflow run on a push to `main` in that
  repository may assume: push to the stack's ECR repositories, register
  task definitions and update and run ECS tasks, pass the stack's task
  roles, and read the SSM parameters and the app's logs. That is what
  `deploy/roll.sh` and `deploy/admin-ecs.sh` need, and nothing more — it
  cannot read secrets.
- **`<project>-infra`**, with `AdministratorAccess`, which only a job
  running in the repository's GitHub **environment named `infra`** may
  assume. Put required reviewers on that environment and every
  terraform apply from CI has a person approving it.

If the account already has a GitHub OIDC provider (another project made
one), pass its ARN as `-var github_oidc_provider_arn=…`; an account can
hold only one per issuer. Outputs `cd_role_arn` and `infra_role_arn` are
what a workflow's `aws-actions/configure-aws-credentials` step takes as
`role-to-assume`.

A deploy job is then: assume the cd role, build and push the image, and
run `deploy/roll.sh <tag>`. An infrastructure job, in the `infra`
environment: assume the infra role and run `scripts/tf.sh <env> apply`
with `TF_STATE_BUCKET` set to the bootstrap's `state_bucket` output.
This repository does not ship such a workflow; it is yours to write, in
the repository you deploy from.

### When the workflow cannot assume its role

```
Could not assume role with OIDC: Not authorized to perform sts:AssumeRoleWithWebIdentity
```

STS does not say which condition failed. Ask CloudTrail what claim
actually arrived — the `sub` is the event's `Username`:

```sh
aws cloudtrail lookup-events --region us-east-1 \
  --lookup-attributes AttributeKey=EventName,AttributeValue=AssumeRoleWithWebIdentity \
  --max-results 5 --query 'Events[].[EventTime,Username]' --output text
```

and compare it with the patterns each role trusts. The cd role expects
one of

```
repo:OWNER/NAME:ref:refs/heads/main
repo:OWNER@<owner-id>/NAME@<repo-id>:ref:refs/heads/main
```

and the infra role the same pair ending `:environment:infra`. The second
spelling is GitHub's immutable subject claim, which carries the numeric
ids so a rename or transfer cannot hand your trust to someone else;
GitHub is part-way through moving to it, so the roles trust both, and
only the ids are wildcarded. A renamed or transferred repository needs
the bootstrap re-applied with its new name.

## Environments

`env` names everything the stack makes, and each environment has its own
state key and its own tfvars, so `prod` and `staging` can share an
account without sharing a resource:

```sh
cp envs/example.tfvars envs/staging.tfvars    # env = "staging", its own domain
scripts/tf.sh staging apply
scripts/tf.sh staging destroy
```

Things to know:

- **`deletion_protection`** (default `true`) keeps the database (deletion
  protection, and a final snapshot when it does go), the store bucket and
  the ECR repository (refuse to be deleted while they hold anything), and
  the secrets (30-day recovery window) from going by accident. Set it
  `false` in a tfvars you intend to `destroy` and rebuild; a protected
  stack's `destroy` fails at the database until you apply with it off.
- **A second environment needs its own GitHub App**: an App has one
  webhook URL, so two environments on one App would each receive the
  other's deliveries.
- **Quotas are the account's**, shared by every environment in it:
  Fargate vCPUs, Aurora clusters, Elastic IPs.

## Adding a domain later

Set `domain_name` (and `parent_domain_name`, if it applies) in the
environment's tfvars and follow [step 3](#3-the-domain-optional). The
stack creates the zone, the certificate, the `ssh.` record and the SES
identity, and `STRATUM_PUBLIC_URL`, `STRATUM_SSH_PUBLIC_URL` and the mail
sender follow in the same apply; run `deploy/roll.sh` afterwards to put
them into service. Clone URLs change with it, so tell people, and update
the GitHub App's URLs if you have one. Removing the domain again is the
reverse and destroys the zone.

## CDN offload

Everything in [Architecture](#architecture) says git traffic is never
cached, and for the negotiated pack that stays true. Offload is
different: a background worker builds one self-contained pack per
repository into the bucket, and a client that opted in (`git -c
fetch.uriprotocols=https clone …`) is handed a signed CloudFront URL for
it through git's `packfile-uri` capability. It fetches the bulk from the
edge, and the server streams only what was pushed since the pack was
built. The bucket policy lets CloudFront read `o/*/cdn/*.pack` and
nothing else.

The URL's authorization is CloudFront's, not the server's, because git
fetches an advertised pack with no credentials at all: the distribution
has a trusted key group, and the server signs each URL with the private
half of a key terraform generated. `cdn_url_ttl_secs` (default an hour)
must comfortably exceed your slowest clone.

- **Clients that have not opted in are unaffected**: they clone exactly
  as before, with no CDN traffic.
- **Offload engages only while the pack covers the current tip.** After
  a push, clones are served inline until the pack catches up
  (`STRATUM_CDNPACK_POLL_SECS`). A clone-heavy, push-light repository
  offloads nearly always; one under constant pushes, rarely.
- **A missing pack fails that clone**, because git does not fall back
  for an advertised URL. The server checks the pack exists before
  advertising it, and `STRATUM_CDN_ENABLED=0` (through
  `extra_environment` — or directly in the task definition in an
  emergency) turns advertising off everywhere.

What it saves is task capacity, not transfer: the bytes still leave AWS
through CloudFront at the same price, but an offloaded clone holds no
task, no connection slot and no memory, and the same pack cloned a
thousand times a day by CI is assembled once. What it costs is the
pack's storage and a rebuild after each burst of pushes. It pays for
repositories that are cloned far more than they are pushed — mirrors,
anything CI pulls all day — and is marginal for one pushed constantly
and cloned rarely.

## Limitations

Each of these is a decision, not an accident.

| Limitation | Why, and what to do |
|---|---|
| The store credential is a static IAM user access key | The server's S3 client signs with static keys only (no task role); a missing key silently becomes anonymous access, which is denied. The key is scoped to one bucket. Rotate with `scripts/tf.sh <env> apply -replace=module.data.aws_iam_access_key.store`, then `deploy/roll.sh`. |
| `rds.force_ssl=0` on Aurora | The server connects without TLS; the traffic never leaves the VPC and 5432 admits only the tasks' security group. |
| CloudFront allows a request 60 s to start answering | A huge clone on a cold cache can outrun it. Use the direct ALB URL (`alb_url` output, plain HTTP inside your network) or SSH; neither has the limit, and the load balancers' idle timeouts (350 s) are above the server's own 300 s operation ceiling. |
| A stopping task gets 120 s | Fargate's maximum `stopTimeout`. SIGTERM drains gracefully; an operation still running 120 s later is killed. |
| Mirror seed clones re-clone after a task is replaced | `STRATUM_DATA_DIR` is ephemeral task storage; the first fetch after a replacement pays for the re-seed. |
| `/metrics` answers 404 through the load balancer | It is unauthenticated; scrape tasks directly from inside the VPC. |
| One database connection per task | The control plane's queries are short point reads and writes; Aurora scales with task count. |
| The ALB is not locked to CloudFront | It serves nothing CloudFront does not, and direct access is the documented path for long operations. Restrict its security group or add a secret-header rule if you need it closed. |
| One NAT gateway | Cost over redundancy for outbound traffic. Clones and pushes do not use it (S3 goes through the gateway endpoint), so if its zone fails the service keeps serving; mirror syncs and outgoing webhooks pause. |
| CloudFront's signature check is proved only against a real deployment | No local test can run CloudFront. `deploy/smoke.sh` checks it against the real one: an advertised pack URL must answer 200 and a tampered signature 403. |
| Smoke-test organisations accumulate | There is no organisation delete; each smoke run makes one `smoke-<id>` organisation and deletes its repositories. The rows are inert. |
