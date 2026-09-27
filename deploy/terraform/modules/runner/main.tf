# Hosted CI runners: a VPC of their own, and nothing else.
#
# A runner executes a `run:` line that a tenant wrote. Treat it as hostile
# code that we chose to run. Two properties have to be true by
# construction, not by convention, and every resource below exists to
# carry one of them:
#
#   1. A runner cannot reach other tenants' data. It has its OWN VPC with
#      no peering, no transit gateway, and no route toward the app VPC; it
#      holds no task role, no store credentials, and no database secret.
#      It reaches the control plane the same way any customer's laptop
#      does — over the public CloudFront URL, carrying a per-job token
#      that is scoped to one repo and expires with the job.
#   2. A runner cannot reach a mining pool. All egress is forced through
#      AWS Network Firewall with a stateful domain ALLOWLIST and a default
#      drop, so a miner has no pool to dial and no proxy to dial it
#      through. The security group narrows that further to 80/443.
#
# Nothing in this file references the app VPC, the data module's security
# groups, or any secret the app holds — deliberately, so the isolation
# argument can be checked by reading this one file.
#
# Mining, and where this file sits in the defence
# -----------------------------------------------
# The firewall below is LAYER 1 of four, and it is the only one that is a
# property of the network rather than of our code:
#
#   1. (here) the domain allowlist and the 80/443 security group. A pool
#      address is not reachable, and neither is a proxy that would reach
#      one. This holds against a miner we have never heard of and against
#      one that arrives inside a dependency, which is why it is first.
#   2. parse time, in the control plane's
#      `crates/stratum-server/src/workflow/parse.rs`: a `run:` line naming
#      a known miner, or carrying a `stratum+tcp://`-family pool URL, is
#      refused before a task is ever launched.
#   3. run time, in the runner (`crates/stratum-runner/src/watch.rs`,
#      which holds the runner's own copy of the name list): the step's
#      process group is sampled and a matching process ends the job with
#      `"abuse": "mining"`.
#   4. control: a verdict carrying `abuse` suspends the organisation's
#      hosted workflows.
#
# Layers 2 and 3 catch what the allowlist cannot say out loud — a job
# mining to a *permitted* domain, which is possible the moment a tenant
# is allowed to reach their own hosts — and layer 1 catches what a name
# list cannot. Neither is a substitute for the other, and nothing in this
# file changes when 2, 3 or 4 do.
#
# What is deliberately NOT here: a `pidsLimit` or an `nproc` ulimit
# against a fork bomb. Fargate takes neither — `pidsLimit` is EC2-only,
# and a Fargate task definition accepts `nofile` as its only ulimit — so
# there is nowhere in this file to put one. The runner does it itself
# instead: it sets `RLIMIT_NPROC` (4096, `STRATUM_RUNNER_MAX_PROCS`; see
# `steps::MAX_PROCS`) in the `pre_exec` of the step's own process, and
# because the container runs exactly one job as uid 10002 the only
# processes counted against that per-uid bound are that job's own.
# What stays under it is bounded here: the task's `cpu`/`memory` below,
# and one job per task — the bomb hits its own task's limits, that job
# fails, and the container is discarded. It cannot reach a second job,
# because there has never been a second job in it. See
# `docs/deployment-aws.md`.

variable "project" { type = string }
variable "env" { type = string }
variable "aws_region" { type = string }
variable "runner_vpc_cidr" { type = string }
variable "image_tag" { type = string }
variable "github_image_tag" {
  description = "Tag of the GitHub Actions runner image (Dockerfile.github-runner) in its own repository. Same bootstrap dance as image_tag."
  type        = string
}
variable "task_cpu" { type = number }
variable "task_memory" { type = number }
variable "egress_allow_domains" {
  description = "Domains a build may reach. A leading '.' matches the domain and every subdomain. Everything else is dropped."
  type        = list(string)
}
variable "control_plane_domain" {
  description = "Host the runner calls back on (the CloudFront domain, or your custom domain). Always allowed through the firewall. Empty is legal but means jobs cannot report — see the two-phase note in docs/deployment-aws.md."
  type        = string
}
variable "log_retention_days" { type = number }
# The runner's own `RLIMIT_NPROC`, when a deployment wants one that is not
# the binary's. `null` — the default — sends nothing, so the runner uses
# `steps::MAX_PROCS`. It is deliberately not pinned to 4096 here: an
# environment entry restating the default would shadow it, and the day the
# runner raises or lowers that number a stale literal in a task definition
# would silently win. Set it and the value is in the plan; leave it and
# the bound is whatever the image enforces.
variable "runner_max_procs" {
  description = "Processes one job may create (RLIMIT_NPROC). null uses the runner binary's own default."
  type        = number
  default     = null
}

data "aws_availability_zones" "available" {
  state = "available"
}

data "aws_caller_identity" "current" {}

locals {
  prefix = "${var.project}-${var.env}-runner"
  azs    = slice(data.aws_availability_zones.available.names, 0, 2)

  # `compact` matters: an unset control_plane_domain must add NOTHING to
  # the allowlist rather than an empty target the engine would reject or,
  # worse, treat as a wildcard. Failing closed here costs a broken CI
  # callback, which is loud; failing open would be silent.
  allow_domains = distinct(compact(concat(var.egress_allow_domains, [var.control_plane_domain])))

  # How long ECS waits between SIGTERM and SIGKILL when a task is
  # stopped, which is the whole cost of cancelling a job: a superseded
  # build keeps burning a task for at most this long after the run says
  # cancelled. ECS's own default is 30s, and leaving it implicit meant
  # three times the necessary spend on every superseded push, and three
  # times the window in which a job whose token is already revoked is
  # still cloning and compiling.
  #
  # `deploy/fake-ecs/fake-ecs.py` passes the same number to
  # `docker stop -t`, so the manual stack cancels on the same clock the
  # deployment does. Change one and change the other.
  stop_timeout_seconds = 10
}

# ------------------------------------------------------------------ VPC
#
# Three tiers, two AZs. Private subnets hold the tasks and have no route
# to the internet of their own; firewall subnets hold the Network Firewall
# endpoints; the single public subnet holds the NAT gateway and is the only
# place with an IGW route.
#
# Address plan (over a /16, /20 per subnet — roomy on purpose, so a
# reviewer can match a CIDR to a tier at a glance):
#   index 0     public
#   index 1,2   firewall, one per AZ
#   index 8,9   private, one per AZ

resource "aws_vpc" "this" {
  cidr_block           = var.runner_vpc_cidr
  enable_dns_support   = true
  enable_dns_hostnames = true

  tags = { Name = local.prefix }
}

resource "aws_internet_gateway" "this" {
  vpc_id = aws_vpc.this.id

  tags = { Name = local.prefix }
}

resource "aws_subnet" "public" {
  vpc_id            = aws_vpc.this.id
  cidr_block        = cidrsubnet(var.runner_vpc_cidr, 4, 0)
  availability_zone = local.azs[0]
  # No task ever lands here; only the NAT gateway does.
  map_public_ip_on_launch = false

  tags = { Name = "${local.prefix}-public" }
}

resource "aws_subnet" "firewall" {
  count             = 2
  vpc_id            = aws_vpc.this.id
  cidr_block        = cidrsubnet(var.runner_vpc_cidr, 4, 1 + count.index)
  availability_zone = local.azs[count.index]

  tags = { Name = "${local.prefix}-firewall-${count.index}" }
}

resource "aws_subnet" "private" {
  count             = 2
  vpc_id            = aws_vpc.this.id
  cidr_block        = cidrsubnet(var.runner_vpc_cidr, 4, 8 + count.index)
  availability_zone = local.azs[count.index]

  tags = { Name = "${local.prefix}-private-${count.index}" }
}

resource "aws_eip" "nat" {
  domain = "vpc"

  tags = { Name = local.prefix }
}

# One NAT, same cost/availability trade as the app VPC. Note that it sits
# BEHIND the firewall from the task's point of view: nothing reaches it
# without being inspected first.
resource "aws_nat_gateway" "this" {
  allocation_id = aws_eip.nat.id
  subnet_id     = aws_subnet.public.id
  depends_on    = [aws_internet_gateway.this]

  tags = { Name = local.prefix }
}

# ------------------------------------------------------- Network Firewall
#
# The single control that makes "the runner rejects crypto miners" a
# property of the network rather than a hope about the image. A miner
# needs to reach a pool; a pool is a domain that is not on this list; the
# firewall drops the connection and logs an alert.
#
# ALLOWLIST + strict rule order + `aws:drop_established` is the
# combination AWS documents for a default-deny domain filter: the L3/L4
# handshake is allowed through so the engine can actually see the TLS SNI
# or the HTTP Host header, and then any established flow that did not
# match a pass rule is dropped. `aws:drop_strict` would kill the SYN
# before there was ever an SNI to inspect, i.e. block everything.
#
# Filtering is on SNI/Host, not IP: it never pauses a connection to do an
# out-of-band DNS lookup. A build that connects to a raw IP with no SNI
# therefore matches nothing and is dropped by the default action, which is
# exactly the behaviour we want. HOME_NET is left at its default — the
# CIDR of the VPC the firewall is deployed in — because every inspected
# flow originates in this VPC.

resource "aws_networkfirewall_rule_group" "egress_allowlist" {
  name = "${local.prefix}-egress-allowlist"
  type = "STATEFUL"
  # Capacity is fixed at creation and changing it replaces the group, so
  # it is set well above the ~2 units per domain this list needs. It only
  # draws against the firewall's 30k budget; unused capacity is free.
  capacity = 1000

  rule_group {
    rules_source {
      rules_source_list {
        generated_rules_type = "ALLOWLIST"
        target_types         = ["HTTP_HOST", "TLS_SNI"]
        targets              = local.allow_domains
      }
    }

    stateful_rule_options {
      rule_order = "STRICT_ORDER"
    }
  }

  tags = { Name = "${local.prefix}-egress-allowlist" }
}

resource "aws_networkfirewall_firewall_policy" "this" {
  name = local.prefix

  firewall_policy {
    stateless_default_actions          = ["aws:forward_to_sfe"]
    stateless_fragment_default_actions = ["aws:forward_to_sfe"]

    stateful_engine_options {
      rule_order = "STRICT_ORDER"
    }

    # The default deny. Everything the allowlist did not pass dies here.
    stateful_default_actions = ["aws:drop_established", "aws:alert_established"]

    stateful_rule_group_reference {
      resource_arn = aws_networkfirewall_rule_group.egress_allowlist.arn
      priority     = 1
    }
  }

  tags = { Name = local.prefix }
}

resource "aws_networkfirewall_firewall" "this" {
  name                = local.prefix
  firewall_policy_arn = aws_networkfirewall_firewall_policy.this.arn
  vpc_id              = aws_vpc.this.id

  # Guard rails: a firewall deleted or detached by accident would silently
  # turn the allowlist into an open NAT. Delete protection is prod's: on
  # a test environment it only makes `destroy` fail, and the firewall is
  # gone with the VPC either way.
  delete_protection                 = var.env == "prod"
  firewall_policy_change_protection = true
  subnet_change_protection          = true

  dynamic "subnet_mapping" {
    for_each = aws_subnet.firewall
    content {
      subnet_id = subnet_mapping.value.id
    }
  }

  tags = { Name = local.prefix }
}

# The alert stream is the evidence that the allowlist is doing something:
# a build that tries to reach a pool shows up here by domain.
resource "aws_cloudwatch_log_group" "firewall" {
  name              = "/${var.project}/${var.env}/runner-firewall"
  retention_in_days = var.log_retention_days
}

resource "aws_networkfirewall_logging_configuration" "this" {
  firewall_arn = aws_networkfirewall_firewall.this.arn

  logging_configuration {
    log_destination_config {
      log_type             = "ALERT"
      log_destination_type = "CloudWatchLogs"
      log_destination = {
        logGroup = aws_cloudwatch_log_group.firewall.name
      }
    }
  }
}

# The firewall creates one VPC endpoint per subnet mapping, reported back
# per AZ in `sync_states`. Routing is AZ-local, so index them by AZ.
locals {
  firewall_endpoints = {
    for state in tolist(aws_networkfirewall_firewall.this.firewall_status[0].sync_states) :
    state.availability_zone => state.attachment[0].endpoint_id
  }
}

# ---------------------------------------------------------------- routing
#
# The whole point of the three tiers. Read it as one loop:
#
#   private-N  0.0.0.0/0 -> firewall endpoint in AZ N   (outbound, inspected)
#   firewall-N 0.0.0.0/0 -> the NAT gateway             (inspected -> translated)
#   public     0.0.0.0/0 -> the IGW                     (translated -> internet)
#   public     private-N -> firewall endpoint in AZ N   (return, SAME endpoint)
#
# That last route is what keeps the stateful engine correct with a single
# NAT across two AZs. The return packet leaves the NAT in AZ 0; without a
# per-private-CIDR route it would go straight to the private subnet and
# the AZ-1 endpoint would never see the other half of the flow it is
# tracking. Because each private subnet has its own CIDR, the public route
# table can send each flow's return leg back through the exact endpoint
# that inspected its outbound leg.

resource "aws_route_table" "private" {
  count  = 2
  vpc_id = aws_vpc.this.id

  route {
    cidr_block      = "0.0.0.0/0"
    vpc_endpoint_id = local.firewall_endpoints[local.azs[count.index]]
  }

  tags = { Name = "${local.prefix}-private-${count.index}" }
}

resource "aws_route_table_association" "private" {
  count          = 2
  subnet_id      = aws_subnet.private[count.index].id
  route_table_id = aws_route_table.private[count.index].id
}

resource "aws_route_table" "firewall" {
  count  = 2
  vpc_id = aws_vpc.this.id

  route {
    cidr_block     = "0.0.0.0/0"
    nat_gateway_id = aws_nat_gateway.this.id
  }

  tags = { Name = "${local.prefix}-firewall-${count.index}" }
}

resource "aws_route_table_association" "firewall" {
  count          = 2
  subnet_id      = aws_subnet.firewall[count.index].id
  route_table_id = aws_route_table.firewall[count.index].id
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.this.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.this.id
  }

  # Return path, one route per private subnet — see the comment above.
  dynamic "route" {
    for_each = aws_subnet.private
    content {
      cidr_block      = route.value.cidr_block
      vpc_endpoint_id = local.firewall_endpoints[route.value.availability_zone]
    }
  }

  tags = { Name = "${local.prefix}-public" }
}

resource "aws_route_table_association" "public" {
  subnet_id      = aws_subnet.public.id
  route_table_id = aws_route_table.public.id
}

# ------------------------------------------------------- VPC endpoints
#
# Fargate pulls the image and ships container logs over the TASK's ENI, so
# that traffic would otherwise take the private route table straight into
# the firewall — where the allowlist would drop it and every task would
# die at PROVISIONING with an opaque CannotPullContainerError. Keeping the
# platform's own traffic on interface/gateway endpoints fixes that without
# widening the allowlist, and has the better property besides: the
# allowlist then describes exactly one thing, the build's own egress.
#
# The S3 gateway endpoint is not optional decoration — ECR stores image
# layers in S3 and serves them by redirect, so ecr.dkr alone is not enough.

resource "aws_security_group" "endpoints" {
  name_prefix = "${local.prefix}-endpoints-"
  vpc_id      = aws_vpc.this.id
  description = "AWS interface endpoints for the runner VPC"

  ingress {
    description     = "HTTPS from runner tasks"
    from_port       = 443
    to_port         = 443
    protocol        = "tcp"
    security_groups = [aws_security_group.task.id]
  }

  lifecycle {
    create_before_destroy = true
  }

  tags = { Name = "${local.prefix}-endpoints" }
}

# Every endpoint below carries a policy, and that is load-bearing rather
# than belt-and-braces. An endpoint is reached over the VPC's LOCAL route,
# so it is the one path out of a runner that does NOT pass the firewall.
# An unpolicied S3 gateway endpoint would hand tenant code a direct,
# uninspected route to all of S3; an unpolicied ecr/logs endpoint would do
# the same for those APIs. Each policy is therefore scoped to exactly what
# the ECS agent needs to start this task and nothing else.

data "aws_iam_policy_document" "endpoint_s3" {
  # ECR serves image layers by redirect to a regional S3 bucket, so the
  # gateway endpoint has to exist — but only for that bucket, and for
  # Docker Hub's.
  #
  # Docker Hub serves its layers the same way: a blob GET answers with a
  # redirect to a presigned URL on `docker-images-prod`, an S3 bucket in
  # us-east-1. From a task here that request rides the gateway endpoint,
  # so this policy — not the egress allow-list, which admits the Hub's
  # hosts by name — decides it, and until this statement existed it
  # answered `AccessDenied` for Docker Hub's own signing role. The docs
  # promised Docker Hub for base images; a job on the fleet that pulled
  # one without a daemon (deploy/proot, 2026-09-21) was the first to
  # find out. Object reads only, on the Hub's bucket only.
  statement {
    principals {
      type        = "AWS"
      identifiers = ["*"]
    }
    actions = ["s3:GetObject"]
    resources = [
      "arn:aws:s3:::prod-${var.aws_region}-starport-layer-bucket/*",
      "arn:aws:s3:::docker-images-prod/*",
    ]
  }
}

resource "aws_vpc_endpoint" "s3" {
  vpc_id            = aws_vpc.this.id
  service_name      = "com.amazonaws.${var.aws_region}.s3"
  vpc_endpoint_type = "Gateway"
  route_table_ids   = aws_route_table.private[*].id
  policy            = data.aws_iam_policy_document.endpoint_s3.json

  tags = { Name = "${local.prefix}-s3" }
}

data "aws_iam_policy_document" "endpoint_ecr" {
  statement {
    principals {
      type        = "AWS"
      identifiers = ["*"]
    }
    # Resource-less by API definition; the pull actions below are the ones
    # that name a repository.
    actions   = ["ecr:GetAuthorizationToken"]
    resources = ["*"]
  }

  statement {
    principals {
      type        = "AWS"
      identifiers = ["*"]
    }
    actions = [
      "ecr:BatchCheckLayerAvailability",
      "ecr:BatchGetImage",
      "ecr:GetDownloadUrlForLayer",
    ]
    # The runner image, and no other repository in the account.
    resources = [aws_ecr_repository.runner.arn, aws_ecr_repository.github_runner.arn]
  }
}

data "aws_iam_policy_document" "endpoint_logs" {
  statement {
    principals {
      type        = "AWS"
      identifiers = ["*"]
    }
    actions = [
      "logs:CreateLogStream",
      "logs:PutLogEvents",
    ]
    # The runner's own container stderr group only. A build cannot write
    # into — or read from — any other log group in the account.
    resources = ["${aws_cloudwatch_log_group.runner.arn}:*"]
  }
}

locals {
  endpoint_policies = {
    "ecr.api" = data.aws_iam_policy_document.endpoint_ecr.json
    "ecr.dkr" = data.aws_iam_policy_document.endpoint_ecr.json
    "logs"    = data.aws_iam_policy_document.endpoint_logs.json
  }
}

resource "aws_vpc_endpoint" "interface" {
  for_each = local.endpoint_policies

  vpc_id              = aws_vpc.this.id
  service_name        = "com.amazonaws.${var.aws_region}.${each.key}"
  vpc_endpoint_type   = "Interface"
  subnet_ids          = aws_subnet.private[*].id
  security_group_ids  = [aws_security_group.endpoints.id]
  private_dns_enabled = true
  policy              = each.value

  tags = { Name = "${local.prefix}-${each.key}" }
}

# ------------------------------------------------------- task security group

resource "aws_security_group" "task" {
  name_prefix = "${local.prefix}-task-"
  vpc_id      = aws_vpc.this.id
  description = "Runner tasks: no ingress, egress only to 80/443 and DNS"

  # NO ingress rule of any kind. Nothing reaches into a runner: not the
  # app, not the internet, not another runner. Jobs are pulled by the
  # runner over HTTPS, never pushed to it.

  egress {
    description = "HTTPS. The firewall decides WHERE this may go."
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  egress {
    description = "HTTP, for apt/deb mirrors that still serve plaintext."
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  # The VPC resolver only. This is a mining control as much as a
  # convenience: without it a build could tunnel over DNS to a resolver of
  # its own choosing, which the domain allowlist would never see.
  egress {
    description = "DNS to the VPC resolver"
    from_port   = 53
    to_port     = 53
    protocol    = "udp"
    cidr_blocks = [var.runner_vpc_cidr]
  }

  lifecycle {
    create_before_destroy = true
  }

  tags = { Name = "${local.prefix}-task" }
}

# ---------------------------------------------------------------- ECR

resource "aws_ecr_repository" "runner" {
  # Env-scoped like every other name: test and prod share one account,
  # and this was the one resource whose name let them collide.
  name                 = "${local.prefix}-runner"
  image_tag_mutability = "MUTABLE"
  # A repository that still holds images refuses to be deleted, which is
  # the right refusal in prod and the wrong one everywhere else: a test
  # environment's `destroy` stalled on this, and terraform stops
  # scheduling work at the first error, so the ALB and the distribution
  # behind it never got their turn either.
  force_delete = var.env != "prod"

  image_scanning_configuration {
    scan_on_push = true
  }
}

resource "aws_ecr_lifecycle_policy" "runner" {
  repository = aws_ecr_repository.runner.name
  policy = jsonencode({
    rules = [{
      rulePriority = 1
      description  = "keep the last 20 images"
      selection = {
        tagStatus   = "any"
        countType   = "imageCountMoreThan"
        countNumber = 20
      }
      action = { type = "expire" }
    }]
  })
}

# The GitHub Actions runner image, in a repository of its own. A different
# image with a different lifecycle from the Weft runner — the official
# agent is pinned and bumped on its own schedule — and a task definition
# that names a different repository, so a mis-tag can never start one
# agent where the other was meant.
resource "aws_ecr_repository" "github_runner" {
  name                 = "${local.prefix}-github"
  image_tag_mutability = "MUTABLE"
  force_delete         = var.env != "prod"

  image_scanning_configuration {
    scan_on_push = true
  }
}

resource "aws_ecr_lifecycle_policy" "github_runner" {
  repository = aws_ecr_repository.github_runner.name
  policy = jsonencode({
    rules = [{
      rulePriority = 1
      description  = "keep the last 20 images"
      selection = {
        tagStatus   = "any"
        countType   = "imageCountMoreThan"
        countNumber = 20
      }
      action = { type = "expire" }
    }]
  })
}

# ---------------------------------------------------------------- ECS

# The runner binary's own stderr — one line per phase. The BUILD output is
# not here: it streams to the control plane and lands in the store bucket
# under `ci/logs/`, where the `expire-ci-logs` lifecycle rule ages it out.
resource "aws_cloudwatch_log_group" "runner" {
  name              = "/${var.project}/${var.env}/runner"
  retention_in_days = var.log_retention_days
}

# ECS creates this group itself the moment Container Insights has
# something to write, with no retention and nothing owning it, so it
# survives every `destroy`. Owning it here gives it both. The cluster
# depends on it so the name is ours before ECS reaches for it.
resource "aws_cloudwatch_log_group" "insights" {
  name              = "/aws/ecs/containerinsights/${local.prefix}/performance"
  retention_in_days = var.log_retention_days
}

resource "aws_ecs_cluster" "this" {
  name = local.prefix

  setting {
    name  = "containerInsights"
    value = "enabled"
  }

  depends_on = [aws_cloudwatch_log_group.insights]
}

data "aws_iam_policy_document" "ecs_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ecs-tasks.amazonaws.com"]
    }
  }
}

# Execution role: the ECS agent's role, not the job's. It pulls the image
# and writes the container log, and that is the entire list. There is
# deliberately NO secrets policy attached — unlike the app's execution
# role, this one has no secret to inject, because the job id and token
# arrive as RunTask overrides that live only for the length of one job.
resource "aws_iam_role" "execution" {
  name               = "${local.prefix}-execution"
  assume_role_policy = data.aws_iam_policy_document.ecs_assume.json
}

resource "aws_iam_role_policy_attachment" "execution_managed" {
  role       = aws_iam_role.execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

# There is NO task role, and the task definition below sets no
# `task_role_arn`. This is the point of the module: tenant code runs in
# this container, so the container's AWS identity must not exist. With no
# task role the credentials endpoint at 169.254.170.2 returns nothing,
# and a build that finds an SSRF or reads the environment finds no key to
# steal — the runner's only credential is the per-job, per-repo, expiring
# Stratum token it was handed on RunTask.

resource "aws_ecs_task_definition" "runner" {
  family                   = local.prefix
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  # The whole compute budget of a job, and the only one. There is no
  # autoscaling, no burst credit and no second container: a task gets
  # these and dies, so "how much can a hostile build spend" is answered
  # by this pair times the concurrency the control plane allows, and by
  # nothing else. A fork bomb, a miner that got past every other layer
  # and an honest slow compiler are all bounded by the same two numbers.
  cpu                = var.task_cpu
  memory             = var.task_memory
  execution_role_arn = aws_iam_role.execution.arn
  # task_role_arn intentionally omitted — see above.

  # Checkout plus build scratch. Ephemeral and per-task: two jobs never
  # share a filesystem, so nothing leaks from one tenant's build to the
  # next by way of a cache.
  ephemeral_storage {
    size_in_gib = 50
  }

  container_definitions = jsonencode([
    {
      name      = "runner"
      image     = "${aws_ecr_repository.runner.repository_url}:${var.image_tag}"
      essential = true
      # uid 10002 from Dockerfile.runner. Builds are not root.
      user = "10002"
      # Builds write: compilers, package managers, /work. A read-only root
      # would be nice and is simply not compatible with running arbitrary
      # toolchains.
      readonlyRootFilesystem = false
      linuxParameters = {
        # A real init as pid 1 so a build that leaves grandchildren behind
        # does not leave zombies holding the task open past its verdict.
        # Reaping is all it is for. Delivery of SIGTERM does not depend on
        # it: the runner installs its own handler (see `signals.rs`), so
        # the signal reaches it whether it is pid 1 itself or a child of
        # the init — the kernel only drops signals pid 1 has no handler
        # for.
        initProcessEnabled = true
      }
      # How long ECS waits after SIGTERM before it SIGKILLs — bounded,
      # not ECS's implicit 30s. See `local.stop_timeout_seconds`.
      stopTimeout = local.stop_timeout_seconds
      environment = concat([
        # Baked-in configuration, and all of it: this and the optional
        # process ceiling below. The job id and token are NOT
        # here: the dispatcher supplies STRATUM_JOB_ID and
        # STRATUM_JOB_TOKEN (and re-states this URL) as RunTask container
        # overrides, so a task definition revision never holds a
        # credential and this value is only the default.
        { name = "STRATUM_RUNNER_URL", value = "https://${var.control_plane_domain}" },
        ],
        var.runner_max_procs == null ? [] : [
          { name = "STRATUM_RUNNER_MAX_PROCS", value = tostring(var.runner_max_procs) },
      ])
      logConfiguration = {
        logDriver = "awslogs"
        options = {
          awslogs-group         = aws_cloudwatch_log_group.runner.name
          awslogs-region        = var.aws_region
          awslogs-stream-prefix = "runner"
        }
      }
    }
  ])
}

# The GitHub Actions runner: the same container hardening as the Weft
# runner — no task role, uid 10002, a real init, the same stopTimeout,
# the same ephemeral scratch — around a different image, the official
# `actions/runner` agent (Dockerfile.github-runner). The two are separate
# families so the dispatch policy can name each one, and so that the
# Weft definition never has to change because GitHub's agent did.
#
# cpu/memory here are the SMALLEST size the feature offers and only a
# default: the dispatcher sends a task-level `overrides.cpu`/`memory`
# with every RunTask (1024/2048, 2048/4096 or 4096/8192 — see
# `run_task_body_github` in crates/stratum-server/src/workflow/executor.rs)
# for the size the job's labels asked for. Fargate takes an override only
# for a pair it would have accepted at registration, which those three
# are.
#
# The registration credential is NOT here and is not an override either.
# The dispatcher passes a job id and a one-shot token; the container
# trades the token for GitHub's `encoded_jit_config` at boot (see
# deploy/github-runner/entrypoint.sh), so no revision of this definition
# and no stopped task's description ever holds a credential that can
# take a job from a repository's queue.
resource "aws_ecs_task_definition" "github_runner" {
  family                   = "${local.prefix}-github"
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = var.task_cpu
  memory                   = var.task_memory
  execution_role_arn       = aws_iam_role.execution.arn
  # task_role_arn intentionally omitted — see the Weft definition above.

  ephemeral_storage {
    size_in_gib = 50
  }

  container_definitions = jsonencode([
    {
      # The name the dispatcher's containerOverrides address
      # (STRATUM_RUNNER_ECS_GITHUB_CONTAINER, default `runner`).
      name      = "runner"
      image     = "${aws_ecr_repository.github_runner.repository_url}:${var.github_image_tag}"
      essential = true
      # uid 10002 from Dockerfile.github-runner. Jobs are not root.
      user                   = "10002"
      readonlyRootFilesystem = false
      linuxParameters = {
        initProcessEnabled = true
      }
      stopTimeout = local.stop_timeout_seconds
      environment = [
        # The one baked-in value, and only a default: the dispatcher
        # re-states it as a RunTask override with the job id and the
        # one-shot token beside it.
        { name = "WEFT_RUNNER_URL", value = "https://${var.control_plane_domain}" },
      ]
      logConfiguration = {
        logDriver = "awslogs"
        options = {
          awslogs-group         = aws_cloudwatch_log_group.runner.name
          awslogs-region        = var.aws_region
          awslogs-stream-prefix = "github"
        }
      }
    }
  ])
}

# --------------------------------------------------- dispatch credential
#
# The app calls ecs:RunTask to start a job. It cannot use a task role for
# that (its own task role is empty by design and the vendored SigV4 signer
# reads static env credentials only — the same constraint that produced
# the store-credentials user in modules/data), so this is a dedicated IAM
# user whose access key reaches the app tasks through Secrets Manager.
#
# The policy is the compensating control for handing out a long-lived key:
# it can start, stop and describe THIS task definition on THIS cluster and
# can do nothing else in the account. It cannot register a task definition
# — so it cannot smuggle in a different image, a task role, or an
# environment of its choosing — and its PassRole is pinned to the one
# execution role that has no secrets policy.

resource "aws_iam_user" "dispatch" {
  name = "${local.prefix}-dispatch"
}

data "aws_iam_policy_document" "dispatch" {
  statement {
    sid     = "RunOnlyTheRunnerTaskDefinitions"
    actions = ["ecs:RunTask"]
    # The two families the dispatcher may start, and no other. The
    # GitHub one is named separately rather than by a `${local.prefix}*`
    # glob, so that a third definition registered under the prefix by
    # anything else is not automatically startable with this key.
    resources = [
      "arn:aws:ecs:${var.aws_region}:${data.aws_caller_identity.current.account_id}:task-definition/${local.prefix}:*",
      "arn:aws:ecs:${var.aws_region}:${data.aws_caller_identity.current.account_id}:task-definition/${local.prefix}-github:*",
    ]

    condition {
      test     = "ArnEquals"
      variable = "ecs:cluster"
      values   = [aws_ecs_cluster.this.arn]
    }
  }

  statement {
    sid     = "ManageOnlyTasksOnTheRunnerCluster"
    actions = ["ecs:StopTask", "ecs:DescribeTasks"]
    resources = [
      "arn:aws:ecs:${var.aws_region}:${data.aws_caller_identity.current.account_id}:task/${aws_ecs_cluster.this.name}/*",
    ]

    condition {
      test     = "ArnEquals"
      variable = "ecs:cluster"
      values   = [aws_ecs_cluster.this.arn]
    }
  }

  # Without the service condition this would be a general "pass this role
  # to anything" grant; with it, the role can only ever be handed to ECS.
  statement {
    sid       = "PassOnlyTheRunnerExecutionRole"
    actions   = ["iam:PassRole"]
    resources = [aws_iam_role.execution.arn]

    condition {
      test     = "StringEquals"
      variable = "iam:PassedToService"
      values   = ["ecs-tasks.amazonaws.com"]
    }
  }
}

resource "aws_iam_user_policy" "dispatch" {
  name   = "dispatch-runner-tasks"
  user   = aws_iam_user.dispatch.name
  policy = data.aws_iam_policy_document.dispatch.json
}

resource "aws_iam_access_key" "dispatch" {
  user = aws_iam_user.dispatch.name
}

# Same JSON shape as the store credentials in modules/data, so the app's
# task definition injects it with the same `:KEY::` selector.
resource "aws_secretsmanager_secret" "dispatch" {
  name = "${var.project}/${var.env}/runner-dispatch-credentials"
  # Prod keeps the recovery window; a test environment must not. A
  # secret scheduled for deletion keeps its name for the window, and the
  # next apply of the same environment fails on "already scheduled for
  # deletion" — the person rebuilding a test stack is exactly who meets
  # that. Seven of these outlived the first rehearsal's `destroy`.
  recovery_window_in_days = var.env == "prod" ? 30 : 0
}

resource "aws_secretsmanager_secret_version" "dispatch" {
  secret_id = aws_secretsmanager_secret.dispatch.id
  secret_string = jsonencode({
    AWS_ACCESS_KEY_ID     = aws_iam_access_key.dispatch.id
    AWS_SECRET_ACCESS_KEY = aws_iam_access_key.dispatch.secret
  })
}

# --------------------------------------------------- SSM (CD's interface)

# deploy.yml rebuilds both runner images on every deploy and pushes them
# to these repositories under the tag the task definitions name. It
# reads the URLs from here, the same way it reads the app's from
# modules/app, so that the workflow never spells a repository name
# terraform chose. The parameter names are CD's contract: change one and
# deploy.yml stops finding it.
resource "aws_ssm_parameter" "runner_ecr_repository" {
  name  = "/${var.project}/${var.env}/runner-ecr-repository"
  type  = "String"
  value = aws_ecr_repository.runner.repository_url
}

resource "aws_ssm_parameter" "github_runner_ecr_repository" {
  name  = "/${var.project}/${var.env}/github-runner-ecr-repository"
  type  = "String"
  value = aws_ecr_repository.github_runner.repository_url
}

# ------------------------------------------------------------------ outputs

output "dispatch_secret_arn" {
  value = aws_secretsmanager_secret.dispatch.arn
}

output "cluster_name" {
  value = aws_ecs_cluster.this.name
}

output "task_definition_family" {
  description = "What the dispatcher passes as RunTask's taskDefinition. A family name resolves to the ACTIVE revision, so a re-registered runner definition takes effect without rolling the app."
  value       = aws_ecs_task_definition.runner.family
}

output "task_definition_arn" {
  description = "family:revision, for auditing which revision terraform last registered."
  value       = aws_ecs_task_definition.runner.arn
}

output "github_task_definition_family" {
  description = "What the dispatcher passes as RunTask's taskDefinition for a GitHub Actions runner (STRATUM_RUNNER_ECS_GITHUB_TASK_DEFINITION). A family name, for the same reason as task_definition_family."
  value       = aws_ecs_task_definition.github_runner.family
}

output "github_task_definition_arn" {
  description = "family:revision of the GitHub Actions runner definition terraform last registered."
  value       = aws_ecs_task_definition.github_runner.arn
}

output "private_subnet_ids" {
  value = aws_subnet.private[*].id
}

output "security_group_id" {
  value = aws_security_group.task.id
}

output "ecr_repository_url" {
  value = aws_ecr_repository.runner.repository_url
}

output "github_ecr_repository_url" {
  value = aws_ecr_repository.github_runner.repository_url
}
