# The replaceable half: ECR, the ECS Fargate service, ALB (HTTP), NLB
# (git-over-SSH), CloudFront, autoscaling, and the SSM parameters the CD
# workflow reads so deploys never need terraform.

variable "project" { type = string }
variable "env" { type = string }
variable "aws_region" { type = string }
variable "vpc_id" { type = string }
variable "vpc_cidr" { type = string }
variable "public_subnets" { type = list(string) }
variable "private_subnets" { type = list(string) }
variable "image_tag" { type = string }
variable "task_cpu" { type = number }
variable "task_memory" { type = number }
variable "desired_count" { type = number }
variable "max_count" { type = number }
variable "domain_name" { type = string }
variable "zone_id" {
  description = "Route 53 zone for `domain_name`; empty without a domain."
  type        = string
}
variable "certificate_arn" {
  description = "An ISSUED us-east-1 certificate covering `domain_name` and `*.domain_name`; empty without a domain."
  type        = string
}
variable "origin_shield_region" {
  description = "Region for CloudFront Origin Shield (the regional cache tier that collapses edge misses into one origin fetch). Empty = use the deploy region; override if aws_region is one of the few that don't offer Origin Shield."
  type        = string
  default     = ""
}
variable "store_bucket" { type = string }
variable "db_url_secret_arn" { type = string }
variable "store_creds_secret_arn" { type = string }
variable "webhook_secret_arn" { type = string }
variable "ssh_host_key_secret_arn" { type = string }
variable "store_bucket_arn" { type = string }
variable "store_bucket_regional_domain_name" { type = string }
variable "cdn_key_secret_arn" { type = string }
variable "cdn_public_key_pem" { type = string }
variable "cdn_url_ttl_secs" {
  description = "Lifetime of a signed pack URL. Must comfortably exceed a slow clone: an opted-in client that started before expiry but fetches the pack after it would fail the clone outright, since git does not fall back to the server for an advertised pack."
  type        = number
  default     = 3600
}

# Hosted CI runners. These come from modules/runner, which builds its own
# isolated VPC; the app's only relationship with it is the ability to start
# and stop tasks there. Nothing about the runner network is reachable from
# here, and nothing here is reachable from a runner except the public URL.
variable "runner_dispatch_secret_arn" { type = string }
variable "runner_cluster" { type = string }
variable "runner_task_definition" { type = string }
variable "runner_github_task_definition" {
  description = "The GitHub Actions runner family (STRATUM_RUNNER_ECS_GITHUB_TASK_DEFINITION). Same cluster, same dispatch key, a different image."
  type        = string
}
variable "runner_subnets" { type = list(string) }
variable "runner_security_group" { type = string }
variable "runner_minutes_per_month" { type = number }
variable "runner_max_timeout_minutes" { type = number }

# Billing. Off, the app sells nothing and every organisation holds
# private repositories for free — the self-hosted build. On, the three
# Stripe values are injected from one secret and the plan refuses a
# blank one.
variable "billing_enabled" { type = bool }
variable "stripe_secret_arn" { type = string }
variable "free_ci_minutes" { type = number }
variable "paid_ci_minutes_per_seat" { type = number }
variable "paid_egress_gb_per_seat" { type = number }
variable "paid_storage_gb_per_seat" { type = number }
variable "paid_packages_gb_per_seat" { type = number }
variable "price_per_seat_cents" { type = number }
variable "overage_1000_minutes_cents" { type = number }
variable "overage_egress_gb_cents" { type = number }
variable "overage_storage_gb_month_cents" { type = number }
variable "overage_packages_gb_month_cents" { type = number }
variable "billing_rollup_secs" { type = number }
variable "storage_sweep_secs" { type = number }
variable "storage_inventory_secs" { type = number }
variable "stripe_meter_minutes" { type = string }
variable "stripe_meter_egress" { type = string }
variable "stripe_meter_storage" { type = string }
variable "stripe_meter_packages" { type = string }
variable "stripe_price_packages" { type = string }

data "aws_secretsmanager_secret_version" "stripe" {
  count     = var.billing_enabled ? 1 : 0
  secret_id = var.stripe_secret_arn
}

locals {
  stripe = var.billing_enabled ? jsondecode(data.aws_secretsmanager_secret_version.stripe[0].secret_string) : { key = "", webhook_secret = "", price = "" }
  stripe_secrets = concat(var.billing_enabled ? [
    { name = "STRATUM_STRIPE_KEY", valueFrom = "${var.stripe_secret_arn}:key::" },
    { name = "STRATUM_STRIPE_WEBHOOK_SECRET", valueFrom = "${var.stripe_secret_arn}:webhook_secret::" },
    { name = "STRATUM_STRIPE_PRICE", valueFrom = "${var.stripe_secret_arn}:price::" },
    ] : [], local.stripe_metered ? [
    { name = "STRATUM_STRIPE_PRICE_MINUTES", valueFrom = "${var.stripe_secret_arn}:price_minutes::" },
    { name = "STRATUM_STRIPE_PRICE_EGRESS", valueFrom = "${var.stripe_secret_arn}:price_egress::" },
    { name = "STRATUM_STRIPE_PRICE_STORAGE", valueFrom = "${var.stripe_secret_arn}:price_storage::" },
    ] : [], local.stripe_metered && local.price_packages_in_secret ? [
    { name = "STRATUM_STRIPE_PRICE_PACKAGES", valueFrom = "${var.stripe_secret_arn}:price_packages::" },
  ] : [])
  # The packages price may come from the secret, like the other three,
  # or from `stripe_price_packages` when the secret predates it. A
  # price id is not a secret, and ECS refuses to start a task whose
  # `valueFrom` names a JSON key the secret does not have — so a fleet
  # whose secret was filled before the fourth meter existed would have
  # stopped starting at all. The secret wins when it has one.
  price_packages_in_secret = length(trimspace(lookup(local.stripe, "price_packages", ""))) > 0
  metered_prices = {
    price_minutes  = trimspace(lookup(local.stripe, "price_minutes", ""))
    price_egress   = trimspace(lookup(local.stripe, "price_egress", ""))
    price_storage  = trimspace(lookup(local.stripe, "price_storage", ""))
    price_packages = local.price_packages_in_secret ? trimspace(local.stripe["price_packages"]) : trimspace(var.stripe_price_packages)
  }
  # Use past the pool is metered only when the secret names the
  # metered prices. Without them the server sells seats alone, pins
  # every spend limit at $0 and treats the pool as a hard cap — which is
  # the honest state of a fleet whose Stripe account has no meters yet.
  # The meter *names* are public (they are event names, not secrets) and
  # ride as variables; the server refuses a partial set at boot, and the
  # precondition below refuses one before that.
  stripe_metered = var.billing_enabled && length(trimspace(lookup(local.stripe, "price_minutes", ""))) > 0
  metered_env = concat(local.stripe_metered ? [
    { name = "STRATUM_STRIPE_METER_MINUTES", value = var.stripe_meter_minutes },
    { name = "STRATUM_STRIPE_METER_EGRESS", value = var.stripe_meter_egress },
    { name = "STRATUM_STRIPE_METER_STORAGE", value = var.stripe_meter_storage },
    { name = "STRATUM_STRIPE_METER_PACKAGES", value = var.stripe_meter_packages },
    ] : [], local.stripe_metered && !local.price_packages_in_secret ? [
    { name = "STRATUM_STRIPE_PRICE_PACKAGES", value = trimspace(var.stripe_price_packages) },
  ] : [])
}

# The GitHub App, the same way: one secret, injected only when the App
# is named, and the plan refuses a blank value. The slug is public — it
# is in the install URL every dashboard visitor is sent to — so it is a
# variable, and the three things GitHub issued live in the secret.
variable "github_app_slug" {
  description = "Slug of the GitHub App this fleet is (`weft` for https://github.com/apps/weft). Empty = no GitHub provider: no mirrors, no imports, `/webhooks/github` unrouted, and the dashboard's Connect button answers 501. Set, the `<project>/<env>/github-app` secret must be filled — see modules/data."
  type        = string
  default     = ""
}
variable "github_app_secret_arn" { type = string }
variable "github_runners_app_slug" {
  type    = string
  default = ""
}
variable "github_runners_app_secret_arn" { type = string }

# Mail. Empty keeps the server's `null` transport (mail is dropped and
# the dashboard says so where it matters); set, the fleet sends through
# SES as this address, which must be under the domain the dns module
# verified — that is the only identity the app's credential may send as.
variable "mail_from" {
  type    = string
  default = ""
}

data "aws_secretsmanager_secret_version" "github_app" {
  count     = var.github_app_slug == "" ? 0 : 1
  secret_id = var.github_app_secret_arn
}

data "aws_secretsmanager_secret_version" "github_runners_app" {
  count     = var.github_runners_app_slug == "" ? 0 : 1
  secret_id = var.github_runners_app_secret_arn
}

locals {
  github_enabled = var.github_app_slug != ""
  github         = local.github_enabled ? jsondecode(data.aws_secretsmanager_secret_version.github_app[0].secret_string) : { app_id = "", private_key = "", webhook_secret = "", client_id = "", client_secret = "" }
  github_secrets = local.github_enabled ? [
    { name = "STRATUM_GITHUB_APP_ID", valueFrom = "${var.github_app_secret_arn}:app_id::" },
    # The PEM itself, not a path: there is no file in the container.
    { name = "STRATUM_GITHUB_APP_KEY", valueFrom = "${var.github_app_secret_arn}:private_key::" },
    { name = "STRATUM_GITHUB_WEBHOOK_SECRET", valueFrom = "${var.github_app_secret_arn}:webhook_secret::" },
    # The App's OAuth client. With it the install callback proves the
    # person arriving with an installation id controls that installation
    # (GitHub's user authorization during install); without it the
    # callback trusts the id, which on a public App is a cross-tenant
    # read. Required on a fleet — see the precondition below.
    { name = "STRATUM_GITHUB_CLIENT_ID", valueFrom = "${var.github_app_secret_arn}:client_id::" },
    { name = "STRATUM_GITHUB_CLIENT_SECRET", valueFrom = "${var.github_app_secret_arn}:client_secret::" },
  ] : []
  github_env = local.github_enabled ? [
    { name = "STRATUM_GITHUB_INSTALL_URL", value = "https://github.com/apps/${var.github_app_slug}/installations/new" },
  ] : []
  # The Runners App: the same five values from its own secret, under
  # RUNNERS names, and its own install page. Without it the mirror App
  # runs jobs as well, as every deployment did before.
  runners_enabled = var.github_runners_app_slug != ""
  runners         = local.runners_enabled ? jsondecode(data.aws_secretsmanager_secret_version.github_runners_app[0].secret_string) : { app_id = "", private_key = "", webhook_secret = "", client_id = "", client_secret = "" }
  runners_secrets = local.runners_enabled ? [
    { name = "STRATUM_GITHUB_RUNNERS_APP_ID", valueFrom = "${var.github_runners_app_secret_arn}:app_id::" },
    { name = "STRATUM_GITHUB_RUNNERS_APP_KEY", valueFrom = "${var.github_runners_app_secret_arn}:private_key::" },
    { name = "STRATUM_GITHUB_RUNNERS_WEBHOOK_SECRET", valueFrom = "${var.github_runners_app_secret_arn}:webhook_secret::" },
    { name = "STRATUM_GITHUB_RUNNERS_CLIENT_ID", valueFrom = "${var.github_runners_app_secret_arn}:client_id::" },
    { name = "STRATUM_GITHUB_RUNNERS_CLIENT_SECRET", valueFrom = "${var.github_runners_app_secret_arn}:client_secret::" },
  ] : []
  runners_env = local.runners_enabled ? [
    { name = "STRATUM_GITHUB_RUNNERS_INSTALL_URL", value = "https://github.com/apps/${var.github_runners_app_slug}/installations/new" },
  ] : []
  mail_env = var.mail_from == "" ? [] : [
    { name = "STRATUM_MAIL_TRANSPORT", value = "ses" },
    { name = "STRATUM_MAIL_FROM", value = var.mail_from },
  ]
}

locals {
  prefix = "${var.project}-${var.env}"
  # Origin Shield sits in one region; default it to where the ALB lives.
  origin_shield_region = var.origin_shield_region != "" ? var.origin_shield_region : var.aws_region
}

# ---------------------------------------------------------------- ECR

resource "aws_ecr_repository" "app" {
  # Env-scoped like every other name: test and prod share one account,
  # and this was the one resource whose name let them collide.
  name                 = "${local.prefix}-app"
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

resource "aws_ecr_lifecycle_policy" "app" {
  repository = aws_ecr_repository.app.name
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

# ------------------------------------------------------------ security

resource "aws_security_group" "alb" {
  name_prefix = "${local.prefix}-alb-"
  vpc_id      = var.vpc_id

  ingress {
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_security_group" "app" {
  name_prefix = "${local.prefix}-app-"
  vpc_id      = var.vpc_id

  ingress {
    description     = "HTTP from the ALB"
    from_port       = 8080
    to_port         = 8080
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }

  # The NLB has no security group of its own (TCP passthrough, client IP
  # preserved is off → source is the NLB nodes inside the VPC).
  ingress {
    description = "SSH from the NLB nodes"
    from_port   = 2222
    to_port     = 2222
    protocol    = "tcp"
    cidr_blocks = [var.vpc_cidr]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  lifecycle {
    create_before_destroy = true
  }
}

# ---------------------------------------------------------------- ALB

resource "aws_lb" "alb" {
  name               = local.prefix
  load_balancer_type = "application"
  security_groups    = [aws_security_group.alb.id]
  subnets            = var.public_subnets
  # Above the store's 300 s per-request op ceiling so the LB never cuts a
  # legitimate long clone/push mid-flight.
  idle_timeout = 350
}

resource "aws_lb_target_group" "http" {
  name        = "${local.prefix}-http"
  port        = 8080
  protocol    = "HTTP"
  vpc_id      = var.vpc_id
  target_type = "ip"
  # Drain longer than the longest in-flight op the app will finish
  # (store ceiling 300 s + margin).
  deregistration_delay = 330

  health_check {
    # /healthz, NEVER /readyz: readiness costs a Postgres ping + S3 LIST
    # per probe — that is for orchestrator gating, not LB polling.
    path                = "/healthz"
    interval            = 15
    healthy_threshold   = 2
    unhealthy_threshold = 3
  }
}

resource "aws_lb_listener" "http" {
  load_balancer_arn = aws_lb.alb.arn
  port              = 80
  protocol          = "HTTP"

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.http.arn
  }
}

# /metrics is unauthenticated Prometheus text — fine inside the VPC,
# not for the internet. Scrapers hit tasks directly.
resource "aws_lb_listener_rule" "block_metrics" {
  listener_arn = aws_lb_listener.http.arn
  priority     = 1

  action {
    type = "fixed-response"
    fixed_response {
      content_type = "text/plain"
      message_body = "not found\n"
      status_code  = "404"
    }
  }

  condition {
    path_pattern {
      values = ["/metrics"]
    }
  }
}

# ------------------------------------------------------- NLB (git-over-SSH)

resource "aws_lb" "ssh" {
  name               = "${local.prefix}-ssh"
  load_balancer_type = "network"
  subnets            = var.public_subnets
}

resource "aws_lb_target_group" "ssh" {
  name                 = "${local.prefix}-ssh"
  port                 = 2222
  protocol             = "TCP"
  vpc_id               = var.vpc_id
  target_type          = "ip"
  deregistration_delay = 330

  health_check {
    protocol = "TCP"
  }
}

resource "aws_lb_listener" "ssh" {
  load_balancer_arn = aws_lb.ssh.arn
  port              = 22
  protocol          = "TCP"

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.ssh.arn
  }
}

# ---------------------------------------------------------------- ECS

resource "aws_cloudwatch_log_group" "app" {
  name              = "/${var.project}/${var.env}/app"
  retention_in_days = 30
}

# ECS creates this group itself the moment Container Insights has
# something to write, with no retention and nothing owning it, so it
# survives every `destroy`. Owning it here gives it both. The cluster
# depends on it so the name is ours before ECS reaches for it.
resource "aws_cloudwatch_log_group" "insights" {
  name              = "/aws/ecs/containerinsights/${local.prefix}/performance"
  retention_in_days = 30
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

# Execution role: pulls the image, writes logs, injects the secrets.
resource "aws_iam_role" "execution" {
  name               = "${local.prefix}-execution"
  assume_role_policy = data.aws_iam_policy_document.ecs_assume.json
}

resource "aws_iam_role_policy_attachment" "execution_managed" {
  role       = aws_iam_role.execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

data "aws_iam_policy_document" "execution_secrets" {
  statement {
    actions = ["secretsmanager:GetSecretValue"]
    resources = [
      var.db_url_secret_arn,
      var.store_creds_secret_arn,
      var.webhook_secret_arn,
      var.ssh_host_key_secret_arn,
      var.cdn_key_secret_arn,
      # Read-only access to the runner dispatch key. The app never sees
      # any OTHER runner credential: there is none.
      var.runner_dispatch_secret_arn,
      var.stripe_secret_arn,
      var.github_app_secret_arn,
      var.github_runners_app_secret_arn,
    ]
  }
}

resource "aws_iam_role_policy" "execution_secrets" {
  name   = "read-app-secrets"
  role   = aws_iam_role.execution.id
  policy = data.aws_iam_policy_document.execution_secrets.json
}

# Task role: EMPTY by design. The app's only AWS dependency is S3, reached
# with the static store credentials (the signer has no role support) — an
# empty role makes any future SSRF pivot worthless.
resource "aws_iam_role" "task" {
  name               = "${local.prefix}-task"
  assume_role_policy = data.aws_iam_policy_document.ecs_assume.json
}

resource "aws_ecs_task_definition" "app" {
  family                   = local.prefix
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = var.task_cpu
  memory                   = var.task_memory
  execution_role_arn       = aws_iam_role.execution.arn
  task_role_arn            = aws_iam_role.task.arn

  # Mirror seed clones, export staging, compaction scratch. Ephemeral by
  # design — a replaced task re-clones mirrors on demand.
  ephemeral_storage {
    size_in_gib = 100
  }

  # A blank Stripe value is a fleet that boots refusing to start (key
  # without price or webhook secret) or, worse, boots selling nothing
  # while the operator believes billing is on (blank key). Caught here.
  lifecycle {
    precondition {
      condition     = !var.billing_enabled || alltrue([for k in ["key", "webhook_secret", "price"] : length(trimspace(lookup(local.stripe, k, ""))) > 0])
      error_message = "billing_enabled is set but the Stripe secret still has a blank key, webhook_secret or price — fill it with `aws secretsmanager put-secret-value` first."
    }
    # The metered prices are all-or-nothing, like the server's own
    # boot check: a fleet with a minutes price and no storage price
    # would report minutes and silently give storage away.
    precondition {
      condition     = !var.billing_enabled || !anytrue([for v in values(local.metered_prices) : length(v) > 0]) || alltrue([for v in values(local.metered_prices) : length(v) > 0])
      error_message = "the Stripe secret names some of price_minutes, price_egress, price_storage and price_packages (or stripe_price_packages) but not all of them — metering is all-or-nothing; fill the rest or blank them all."
    }
    # Same argument for the App: an id without a key is a boot error, and
    # a blank webhook secret is a fleet that silently drops every push
    # GitHub delivers.
    precondition {
      condition     = !local.runners_enabled || (local.github_enabled && alltrue([for k in ["app_id", "private_key", "webhook_secret", "client_id", "client_secret"] : length(trimspace(lookup(local.runners, k, ""))) > 0]))
      error_message = "github_runners_app_slug is set but github_app_slug is not, or the github-runners-app secret still has a blank value — the Runners App is a second App beside the mirror App, and needs all five of app_id, private_key, webhook_secret, client_id and client_secret filled (modules/data says how)."
    }
    precondition {
      condition     = !local.github_enabled || alltrue([for k in ["app_id", "private_key", "webhook_secret", "client_id", "client_secret"] : length(trimspace(lookup(local.github, k, ""))) > 0])
      error_message = "github_app_slug is set but the github-app secret still has a blank app_id, private_key, webhook_secret, client_id or client_secret — fill it with `aws secretsmanager put-secret-value` first (modules/data says how). client_id/client_secret are the App's OAuth client: without them the install callback cannot prove an installation belongs to the person connecting it."
    }
  }

  container_definitions = jsonencode([
    {
      name      = "app"
      image     = "${aws_ecr_repository.app.repository_url}:${var.image_tag}"
      essential = true
      portMappings = [
        { containerPort = 8080, protocol = "tcp" },
        { containerPort = 2222, protocol = "tcp" },
      ]
      # Fargate's maximum. Ops longer than 120 s on a STOPPING task still
      # die at SIGKILL — documented in docs/deployment-aws.md.
      stopTimeout = 120
      environment = concat([
        { name = "STRATUM_BIND", value = "0.0.0.0:8080" },
        { name = "STRATUM_SSH_BIND", value = "0.0.0.0:2222" },
        # Path-style URL: exactly what the sigv4 signer implements; the
        # S3 gateway endpoint routes this hostname inside the VPC.
        { name = "STRATUM_STORE_URL", value = "https://s3.${var.aws_region}.amazonaws.com/${var.store_bucket}" },
        { name = "AWS_REGION", value = var.aws_region },
        { name = "STRATUM_PUBLIC_URL", value = local.public_url },
        { name = "STRATUM_SSH_PUBLIC_URL", value = local.ssh_url },
        { name = "STRATUM_DATA_DIR", value = "/var/lib/stratum" },
        { name = "STRATUM_CDN_BASE", value = local.public_url },
        { name = "STRATUM_CDN_KEY_PAIR_ID", value = aws_cloudfront_public_key.cdn.id },
        { name = "STRATUM_CDN_URL_TTL_SECS", value = tostring(var.cdn_url_ttl_secs) },
        # Where the dispatcher launches CI jobs. A family name, not a
        # pinned revision, so re-registering the runner definition takes
        # effect without rolling the app fleet.
        { name = "STRATUM_RUNNER_ECS_CLUSTER", value = var.runner_cluster },
        { name = "STRATUM_RUNNER_ECS_TASK_DEFINITION", value = var.runner_task_definition },
        # The second family on the same cluster: the official GitHub
        # Actions agent, for jobs GitHub sends this fleet. Absent, the
        # app treats the feature as off and refuses those launches.
        { name = "STRATUM_RUNNER_ECS_GITHUB_TASK_DEFINITION", value = var.runner_github_task_definition },
        { name = "STRATUM_RUNNER_ECS_SUBNETS", value = join(",", var.runner_subnets) },
        { name = "STRATUM_RUNNER_ECS_SECURITY_GROUP", value = var.runner_security_group },
        # A number rather than nothing, deliberately. Unset means
        # unlimited hosted minutes for every organisation, which on a
        # fleet where signup is self-serve is an abuse control that is
        # built and switched off; an organisation that needs more gets
        # an override in `orgs.ci_minutes_per_month`.
        { name = "STRATUM_RUNNER_MINUTES_PER_MONTH", value = tostring(var.runner_minutes_per_month) },
        { name = "STRATUM_RUNNER_MAX_TIMEOUT_MINUTES", value = tostring(var.runner_max_timeout_minutes) },
        # The deal, as numbers: what a free (public-only) organisation
        # and a person get in hosted minutes, what each paid seat adds,
        # and the price the dashboard prints. The price is display
        # only — Stripe's price object is what is charged.
        { name = "STRATUM_FREE_CI_MINUTES", value = tostring(var.free_ci_minutes) },
        { name = "STRATUM_PAID_CI_MINUTES_PER_SEAT", value = tostring(var.paid_ci_minutes_per_seat) },
        { name = "STRATUM_PAID_EGRESS_GB_PER_SEAT", value = tostring(var.paid_egress_gb_per_seat) },
        { name = "STRATUM_PAID_STORAGE_GB_PER_SEAT", value = tostring(var.paid_storage_gb_per_seat) },
        { name = "STRATUM_PAID_PACKAGES_GB_PER_SEAT", value = tostring(var.paid_packages_gb_per_seat) },
        { name = "STRATUM_PRICE_PER_SEAT_CENTS", value = tostring(var.price_per_seat_cents) },
        # What use past the pool is quoted at. Display and estimate
        # only, like the seat price: Stripe's metered prices charge.
        { name = "STRATUM_OVERAGE_1000_MINUTES_CENTS", value = tostring(var.overage_1000_minutes_cents) },
        { name = "STRATUM_OVERAGE_EGRESS_GB_CENTS", value = tostring(var.overage_egress_gb_cents) },
        { name = "STRATUM_OVERAGE_STORAGE_GB_MONTH_CENTS", value = tostring(var.overage_storage_gb_month_cents) },
        { name = "STRATUM_OVERAGE_PACKAGES_GB_MONTH_CENTS", value = tostring(var.overage_packages_gb_month_cents) },
        # Every fifteen minutes rather than the hour: the last hour of a
        # billing period has to reach Stripe inside the meter's grace.
        { name = "STRATUM_BILLING_ROLLUP_SECS", value = tostring(var.billing_rollup_secs) },
        { name = "STRATUM_STORAGE_SWEEP_SECS", value = tostring(var.storage_sweep_secs) },
        { name = "STRATUM_STORAGE_INVENTORY_SECS", value = tostring(var.storage_inventory_secs) },
      ], local.metered_env, local.github_env, local.runners_env, local.mail_env)
      secrets = concat([
        { name = "STRATUM_DB_URL", valueFrom = var.db_url_secret_arn },
        { name = "AWS_ACCESS_KEY_ID", valueFrom = "${var.store_creds_secret_arn}:AWS_ACCESS_KEY_ID::" },
        { name = "AWS_SECRET_ACCESS_KEY", valueFrom = "${var.store_creds_secret_arn}:AWS_SECRET_ACCESS_KEY::" },
        { name = "STRATUM_WEBHOOK_SECRET", valueFrom = var.webhook_secret_arn },
        { name = "STRATUM_SSH_HOST_KEY", valueFrom = var.ssh_host_key_secret_arn },
        { name = "STRATUM_CDN_PRIVATE_KEY", valueFrom = var.cdn_key_secret_arn },
        # Deliberately NOT the store credentials above: this key can only
        # RunTask/StopTask/DescribeTasks on the runner cluster.
        { name = "STRATUM_RUNNER_AWS_ACCESS_KEY_ID", valueFrom = "${var.runner_dispatch_secret_arn}:AWS_ACCESS_KEY_ID::" },
        { name = "STRATUM_RUNNER_AWS_SECRET_ACCESS_KEY", valueFrom = "${var.runner_dispatch_secret_arn}:AWS_SECRET_ACCESS_KEY::" },
      ], local.stripe_secrets, local.github_secrets, local.runners_secrets)
      logConfiguration = {
        logDriver = "awslogs"
        options = {
          awslogs-group         = aws_cloudwatch_log_group.app.name
          awslogs-region        = var.aws_region
          awslogs-stream-prefix = "app"
        }
      }
    }
  ])
}

resource "aws_ecs_service" "app" {
  name            = local.prefix
  cluster         = aws_ecs_cluster.this.id
  task_definition = aws_ecs_task_definition.app.arn
  desired_count   = var.desired_count
  launch_type     = "FARGATE"

  # Rolling deploys with headroom; boot-time migrations serialize under
  # the app's own pg_advisory_lock, so overlap is safe.
  # 50, not 100, while the account's Fargate vCPU quota is 6: a rollout
  # at 100 needs a third 2-vCPU app task beside the two running, and a
  # single fleet job holds the last two vCPUs — the deploy of 2026-09-08
  # failed to place its task 160 times while the dispatcher re-took the
  # slot every five seconds. At 50 the rollout replaces one task at a
  # time inside the quota, at the cost of one task serving for a minute.
  # Put it back to 100 when the quota increase (32 requested) lands.
  deployment_minimum_healthy_percent = 50
  deployment_maximum_percent         = 200

  network_configuration {
    subnets          = var.private_subnets
    security_groups  = [aws_security_group.app.id]
    assign_public_ip = false
  }

  load_balancer {
    target_group_arn = aws_lb_target_group.http.arn
    container_name   = "app"
    container_port   = 8080
  }

  load_balancer {
    target_group_arn = aws_lb_target_group.ssh.arn
    container_name   = "app"
    container_port   = 2222
  }

  health_check_grace_period_seconds = 60

  # CD registers task-definition revisions and autoscaling owns the count;
  # terraform must not fight either.
  lifecycle {
    ignore_changes = [task_definition, desired_count]
  }

  depends_on = [aws_lb_listener.http, aws_lb_listener.ssh]
}

# ------------------------------------------------------------ autoscaling

resource "aws_appautoscaling_target" "app" {
  max_capacity       = var.max_count
  min_capacity       = var.desired_count
  resource_id        = "service/${aws_ecs_cluster.this.name}/${aws_ecs_service.app.name}"
  scalable_dimension = "ecs:service:DesiredCount"
  service_namespace  = "ecs"
}

resource "aws_appautoscaling_policy" "cpu" {
  name               = "${local.prefix}-cpu"
  policy_type        = "TargetTrackingScaling"
  resource_id        = aws_appautoscaling_target.app.resource_id
  scalable_dimension = aws_appautoscaling_target.app.scalable_dimension
  service_namespace  = aws_appautoscaling_target.app.service_namespace

  target_tracking_scaling_policy_configuration {
    target_value = 60
    predefined_metric_specification {
      predefined_metric_type = "ECSServiceAverageCPUUtilization"
    }
  }
}

resource "aws_appautoscaling_policy" "requests" {
  name               = "${local.prefix}-requests"
  policy_type        = "TargetTrackingScaling"
  resource_id        = aws_appautoscaling_target.app.resource_id
  scalable_dimension = aws_appautoscaling_target.app.scalable_dimension
  service_namespace  = aws_appautoscaling_target.app.service_namespace

  target_tracking_scaling_policy_configuration {
    target_value = 500
    predefined_metric_specification {
      predefined_metric_type = "ALBRequestCountPerTarget"
      resource_label         = "${aws_lb.alb.arn_suffix}/${aws_lb_target_group.http.arn_suffix}"
    }
  }
}

# ------------------------------------------------------------- CloudFront

data "aws_cloudfront_cache_policy" "origin_cache_control" {
  # Honors the app's own Cache-Control headers (immutable segments cache,
  # git/REST responses marked no-cache do not) and keeps query strings in
  # the key.
  name = "UseOriginCacheControlHeaders-QueryStrings"
}

data "aws_cloudfront_origin_request_policy" "all_viewer" {
  # Authorization must reach the origin: git and the REST API live behind
  # the same distribution.
  name = "Managed-AllViewer"
}

# ------------------------------------------- CDN-offloaded clone packs
#
# Opted-in git clients (`fetch.uriprotocols`) are handed a signed URL for
# the repo's bulk pack and fetch it straight from the edge, so those bytes
# never traverse a task or the ALB.
#
# Edge auth is mandatory here, not optional: git fetches an advertised
# `packfile-uri` with NO credentials at all, so authorization has to live
# in the URL. Hence a trusted key group — CloudFront verifies the
# signature before it ever reaches S3.
resource "aws_cloudfront_public_key" "cdn" {
  name        = "${local.prefix}-cdn"
  encoded_key = var.cdn_public_key_pem
  comment     = "Verifies signed URLs for CDN-offloaded clone packs"

  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_cloudfront_key_group" "cdn" {
  name  = "${local.prefix}-cdn"
  items = [aws_cloudfront_public_key.cdn.id]
}

# OAC rather than a public bucket: the bucket keeps its public-access
# block fully on, and only this distribution can read the packs.
resource "aws_cloudfront_origin_access_control" "store" {
  name                              = "${local.prefix}-store"
  origin_access_control_origin_type = "s3"
  signing_behavior                  = "always"
  signing_protocol                  = "sigv4"
}

# Exactly the pack objects, and nothing else in the store. The `cdn/`
# directory also holds the descriptor the serve path reads (current.json);
# `*.pack` keeps that — and every segment, manifest, and export — off the
# edge entirely.
data "aws_iam_policy_document" "store_cdn_read" {
  statement {
    principals {
      type        = "Service"
      identifiers = ["cloudfront.amazonaws.com"]
    }
    actions   = ["s3:GetObject"]
    resources = ["${var.store_bucket_arn}/o/*/cdn/*.pack"]

    condition {
      test     = "StringEquals"
      variable = "AWS:SourceArn"
      values   = [aws_cloudfront_distribution.cdn.arn]
    }
  }
}

resource "aws_s3_bucket_policy" "store_cdn_read" {
  bucket = var.store_bucket
  policy = data.aws_iam_policy_document.store_cdn_read.json
}

data "aws_cloudfront_cache_policy" "cdn_packs" {
  # Packs are immutable — named by tip and content hash — so the cache key
  # is the path alone. The signature query params must NOT be part of it,
  # or every client's differently-signed URL would be a separate object.
  name = "Managed-CachingOptimized"
}

resource "aws_cloudfront_distribution" "cdn" {
  enabled         = true
  is_ipv6_enabled = true
  comment         = "${local.prefix} — site, dashboard, API, git smart HTTP"
  aliases         = local.aliases

  origin {
    domain_name = aws_lb.alb.dns_name
    origin_id   = "alb"

    custom_origin_config {
      # TLS terminates at CloudFront (default cert, v1 no-domain posture);
      # CloudFront→ALB rides AWS's network in plain HTTP. The ALB is not
      # origin-locked in v1 — acceptable while it serves nothing CloudFront
      # does not (documented limitation).
      http_port              = 80
      https_port             = 443
      origin_protocol_policy = "http-only"
      origin_ssl_protocols   = ["TLSv1.2"]
      # The maximum without a quota increase. Heavier ops (giant clones on
      # cold caches) go ALB-direct; documented.
      origin_read_timeout      = 60
      origin_keepalive_timeout = 60
    }

    # A regional cache tier in front of the ALB: edge-cache misses from many
    # PoPs collapse into ONE origin fetch (request collapsing), and a warm
    # regional cache absorbs repeat misses — so cacheable traffic (immutable
    # web bundles, public reads) scales without hammering the tasks. It only
    # affects what is cacheable at all: git POST payloads and per-tenant
    # authenticated responses still pass straight through to origin, by
    # design (the app marks them no-cache and CloudFront never caches POST).
    origin_shield {
      enabled              = true
      origin_shield_region = local.origin_shield_region
    }
  }

  # The store bucket, for offloaded clone packs only (see the key group
  # and bucket policy above).
  origin {
    domain_name              = var.store_bucket_regional_domain_name
    origin_id                = "store"
    origin_access_control_id = aws_cloudfront_origin_access_control.store.id

    origin_shield {
      enabled              = true
      origin_shield_region = local.origin_shield_region
    }
  }

  # Pack keys are `o/<org>/r/<repo>/<layout>/cdn/<tip>-<hash>.pack`, which
  # is exactly the URL path the server advertises.
  ordered_cache_behavior {
    path_pattern           = "o/*/cdn/*.pack"
    target_origin_id       = "store"
    viewer_protocol_policy = "https-only"
    allowed_methods        = ["GET", "HEAD"]
    cached_methods         = ["GET", "HEAD"]
    # Already-compressed data; re-compressing it at the edge only burns CPU.
    compress           = false
    cache_policy_id    = data.aws_cloudfront_cache_policy.cdn_packs.id
    trusted_key_groups = [aws_cloudfront_key_group.cdn.id]
  }

  default_cache_behavior {
    target_origin_id         = "alb"
    viewer_protocol_policy   = "redirect-to-https"
    allowed_methods          = ["GET", "HEAD", "OPTIONS", "PUT", "POST", "PATCH", "DELETE"]
    cached_methods           = ["GET", "HEAD"]
    compress                 = true
    cache_policy_id          = data.aws_cloudfront_cache_policy.origin_cache_control.id
    origin_request_policy_id = data.aws_cloudfront_origin_request_policy.all_viewer.id
  }

  restrictions {
    geo_restriction {
      restriction_type = "none"
    }
  }

  # With a domain, TLS is our certificate for our names; without one it
  # is CloudFront's for its own hostname.
  viewer_certificate {
    cloudfront_default_certificate = var.domain_name == ""
    acm_certificate_arn            = var.domain_name == "" ? null : var.certificate_arn
    ssl_support_method             = var.domain_name == "" ? null : "sni-only"
    minimum_protocol_version       = var.domain_name == "" ? "TLSv1" : "TLSv1.2_2021"
  }
}

# ------------------------------------------------------------ the names
#
# Every URL the product hands out — the one it tells a browser, the one
# it signs CDN pack URLs against, the one runners call back on, the SSH
# host in a clone command — comes from these three locals and nowhere
# else. With a domain they are the domain; without one they are the
# generated hostnames, which is what the first version shipped and what
# a rehearsal with no delegated domain still gets.

locals {
  public_host = var.domain_name == "" ? aws_cloudfront_distribution.cdn.domain_name : var.domain_name
  public_url  = "https://${local.public_host}"
  ssh_host    = var.domain_name == "" ? aws_lb.ssh.dns_name : "ssh.${var.domain_name}"
  ssh_url     = "ssh://git@${local.ssh_host}"
  # What CloudFront answers to besides its own name. The docs address the
  # API as `api.`, and `www.` is where a typed URL ends up.
  aliases = var.domain_name == "" ? [] : [var.domain_name, "api.${var.domain_name}", "www.${var.domain_name}"]
}

resource "aws_route53_record" "web" {
  for_each = toset(local.aliases)
  zone_id  = var.zone_id
  name     = each.value
  type     = "A"

  alias {
    name                   = aws_cloudfront_distribution.cdn.domain_name
    zone_id                = aws_cloudfront_distribution.cdn.hosted_zone_id
    evaluate_target_health = false
  }
}

resource "aws_route53_record" "web_v6" {
  for_each = toset(local.aliases)
  zone_id  = var.zone_id
  name     = each.value
  type     = "AAAA"

  alias {
    name                   = aws_cloudfront_distribution.cdn.domain_name
    zone_id                = aws_cloudfront_distribution.cdn.hosted_zone_id
    evaluate_target_health = false
  }
}

resource "aws_route53_record" "ssh" {
  count   = var.domain_name == "" ? 0 : 1
  zone_id = var.zone_id
  name    = local.ssh_host
  type    = "A"

  alias {
    name                   = aws_lb.ssh.dns_name
    zone_id                = aws_lb.ssh.zone_id
    evaluate_target_health = false
  }
}

# --------------------------------------------------- SSM (CD's interface)

locals {
  params = {
    "base-url"        = local.public_url
    "alb-url"         = "http://${aws_lb.alb.dns_name}"
    "ssh-endpoint"    = local.ssh_url
    "cluster"         = aws_ecs_cluster.this.name
    "service"         = aws_ecs_service.app.name
    "private-subnets" = join(",", var.private_subnets)
    "app-sg"          = aws_security_group.app.id
    "log-group"       = aws_cloudwatch_log_group.app.name
    "ecr-repository"  = aws_ecr_repository.app.repository_url
  }
}

resource "aws_ssm_parameter" "params" {
  for_each = local.params
  name     = "/${var.project}/${var.env}/${each.key}"
  type     = "String"
  value    = each.value
}

# ------------------------------------------------------------------ outputs

output "app_sg_id" {
  value = aws_security_group.app.id
}

output "base_url" {
  value = local.public_url
}

output "cdn_domain_name" {
  description = "CloudFront's own hostname, bare. Without a custom domain this is the host STRATUM_PUBLIC_URL names and so the one modules/runner allowlists; with one, the root stack allowlists the domain instead."
  value       = aws_cloudfront_distribution.cdn.domain_name
}

output "alb_url" {
  value = "http://${aws_lb.alb.dns_name}"
}

output "ssh_endpoint" {
  value = local.ssh_url
}

output "ecr_repository_url" {
  value = aws_ecr_repository.app.repository_url
}

output "cluster_name" {
  value = aws_ecs_cluster.this.name
}

output "service_name" {
  value = aws_ecs_service.app.name
}
