# Root stack: network → data (Aurora, store bucket, secrets) → app (ECS
# Fargate behind ALB + CloudFront for HTTP, NLB for git-over-SSH).
# Module boundaries follow blast radius: the data module holds everything
# stateful, the app module is fully replaceable.
#
# One root, several environments: `env` names everything (`stratum-<env>-*`,
# `/stratum/<env>/…`), each environment has its own state key and its own
# `envs/<env>.tfvars`, and `scripts/tf.sh <env>` sets both from one
# argument. The lock below is what stops the two from disagreeing.

module "env_lock" {
  source = "./modules/env-lock"
  env    = var.env
}

module "network" {
  source   = "./modules/network"
  project  = var.project
  env      = var.env
  vpc_cidr = var.vpc_cidr
}

module "data" {
  source          = "./modules/data"
  project         = var.project
  env             = var.env
  vpc_id          = module.network.vpc_id
  private_subnets = module.network.private_subnet_ids
  db_min_acu      = var.db_min_acu
  db_max_acu      = var.db_max_acu

  ci_log_retention_days = var.ci_log_retention_days
  cache_retention_days  = var.cache_retention_days

  # The identity the app's credential may send mail as: the domain's,
  # when there is one. No domain, no mail — STRATUM_MAIL_TRANSPORT stays
  # `null` below for the same reason.
  ses_identity_arns = var.domain_name == "" ? [] : [module.dns[0].mail_identity_arn]
}

# Hosted CI runners, in a VPC of their own. The module is the isolation
# argument and is meant to be read as one unit: no peering, no transit
# gateway, no route between this VPC and the app's, no task role, and all
# egress forced through a domain allowlist. The ONLY wire between the two
# halves is the dispatch credential below, which can start and stop tasks
# on the runner cluster and nothing else.
#
# The two modules feed each other — the app gets the runner's cluster and
# dispatch key, the runner gets the app's CloudFront host for its firewall
# allowlist — and that is not a cycle: terraform's graph is per-resource,
# and no resource behind `cdn_domain_name` depends on anything in
# modules/runner. Keep it that way. Making the CloudFront distribution
# (or an ALB, or a target group) depend on a runner output would close the
# loop and terraform would refuse to plan.
module "runner" {
  source     = "./modules/runner"
  project    = var.project
  env        = var.env
  aws_region = var.aws_region

  runner_vpc_cidr  = var.runner_vpc_cidr
  image_tag        = var.runner_image_tag
  github_image_tag = var.github_runner_image_tag
  task_cpu         = var.runner_task_cpu
  task_memory      = var.runner_task_memory
  # The API is also served at `api.<domain>`, which is the host every
  # docs page and every published Action names. A job on a Weft runner
  # that did `curl https://api.weft.sh/…` sat in the firewall until the
  # connection timed out, while `https://weft.sh/…` answered — the
  # allowlist knew only the callback host. The alias goes in beside it.
  egress_allow_domains = concat(
    var.runner_egress_allow_domains,
    var.domain_name != "" ? ["api.${var.domain_name}"] : [],
  )
  runner_max_procs     = var.runner_max_procs
  control_plane_domain = local.control_plane_domain
  log_retention_days   = var.runner_log_retention_days
}

locals {
  # The host runners call back on: the same host STRATUM_PUBLIC_URL names,
  # which is the custom domain when there is one and CloudFront's own
  # hostname otherwise. The variable exists to override it if you front
  # the control plane with something else.
  control_plane_domain = (
    var.control_plane_domain != "" ? var.control_plane_domain :
    var.domain_name != "" ? var.domain_name :
    module.app.cdn_domain_name
  )
}

# The app tasks are the only thing allowed to reach Postgres. The rule
# lives here (not in either module) so neither module references the
# other's security group.
resource "aws_security_group_rule" "app_to_db" {
  type                     = "ingress"
  from_port                = 5432
  to_port                  = 5432
  protocol                 = "tcp"
  security_group_id        = module.data.db_sg_id
  source_security_group_id = module.app.app_sg_id
}

# The product's own name, when it has one. Absent, the stack answers on
# CloudFront's and the NLB's generated hostnames (the v1 posture).
module "dns" {
  count       = var.domain_name == "" ? 0 : 1
  source      = "./modules/dns"
  project     = var.project
  env         = var.env
  aws_region  = var.aws_region
  domain_name = var.domain_name

  parent_domain_name = var.parent_domain_name

  providers = {
    aws           = aws
    aws.us_east_1 = aws.us_east_1
  }
}

# The name published sites answer to, when there is one. A separate
# registration from the product's own name on purpose — see the module.
module "sites" {
  count       = var.sites_domain_name == "" ? 0 : 1
  source      = "./modules/sites"
  project     = var.project
  env         = var.env
  domain_name = var.sites_domain_name

  providers = {
    aws           = aws
    aws.us_east_1 = aws.us_east_1
  }
}

locals {
  # Mail needs a verified identity, and the only one the stack makes is
  # the domain's — so no domain means no sender (variables.tf refuses a
  # `mail_from` without one), and a domain means a default sender.
  mail_from = var.domain_name == "" ? "" : (var.mail_from != "" ? var.mail_from : "no-reply@${var.domain_name}")
}

module "app" {
  source               = "./modules/app"
  project              = var.project
  env                  = var.env
  aws_region           = var.aws_region
  vpc_id               = module.network.vpc_id
  vpc_cidr             = var.vpc_cidr
  public_subnets       = module.network.public_subnet_ids
  private_subnets      = module.network.private_subnet_ids
  image_tag            = var.image_tag
  task_cpu             = var.task_cpu
  task_memory          = var.task_memory
  desired_count        = var.desired_count
  max_count            = var.max_count
  domain_name          = var.domain_name
  zone_id              = var.domain_name == "" ? "" : module.dns[0].zone_id
  certificate_arn      = var.domain_name == "" ? "" : module.dns[0].certificate_arn
  origin_shield_region = var.origin_shield_region

  store_bucket            = module.data.store_bucket
  db_url_secret_arn       = module.data.db_url_secret_arn
  store_creds_secret_arn  = module.data.store_creds_secret_arn
  webhook_secret_arn      = module.data.webhook_secret_arn
  ssh_host_key_secret_arn = module.data.ssh_host_key_secret_arn

  store_bucket_arn                  = module.data.store_bucket_arn
  store_bucket_regional_domain_name = module.data.store_bucket_regional_domain_name
  cdn_key_secret_arn                = module.data.cdn_key_secret_arn
  cdn_public_key_pem                = module.data.cdn_public_key_pem
  cdn_url_ttl_secs                  = var.cdn_url_ttl_secs

  runner_dispatch_secret_arn    = module.runner.dispatch_secret_arn
  runner_cluster                = module.runner.cluster_name
  runner_task_definition        = module.runner.task_definition_family
  runner_github_task_definition = module.runner.github_task_definition_family
  runner_subnets                = module.runner.private_subnet_ids
  runner_security_group         = module.runner.security_group_id

  runner_minutes_per_month   = var.runner_minutes_per_month
  runner_max_timeout_minutes = var.runner_max_timeout_minutes

  billing_enabled               = var.billing_enabled
  stripe_secret_arn             = module.data.stripe_secret_arn
  github_app_slug               = var.github_app_slug
  github_app_secret_arn         = module.data.github_app_secret_arn
  github_runners_app_slug       = var.github_runners_app_slug
  github_runners_app_secret_arn = module.data.github_runners_app_secret_arn
  mail_from                     = local.mail_from
  free_ci_minutes               = var.free_ci_minutes
  paid_ci_minutes_per_seat      = var.paid_ci_minutes_per_seat
  paid_egress_gb_per_seat       = var.paid_egress_gb_per_seat
  paid_storage_gb_per_seat      = var.paid_storage_gb_per_seat
  paid_packages_gb_per_seat     = var.paid_packages_gb_per_seat
  price_per_seat_cents          = var.price_per_seat_cents

  overage_1000_minutes_cents      = var.overage_1000_minutes_cents
  overage_egress_gb_cents         = var.overage_egress_gb_cents
  overage_storage_gb_month_cents  = var.overage_storage_gb_month_cents
  overage_packages_gb_month_cents = var.overage_packages_gb_month_cents
  billing_rollup_secs             = var.billing_rollup_secs
  storage_sweep_secs              = var.storage_sweep_secs
  storage_inventory_secs          = var.storage_inventory_secs
  stripe_meter_minutes            = var.stripe_meter_minutes
  stripe_meter_egress             = var.stripe_meter_egress
  stripe_meter_storage            = var.stripe_meter_storage
  stripe_meter_packages           = var.stripe_meter_packages
  stripe_price_packages           = var.stripe_price_packages
}
