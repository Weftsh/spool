# The AWS reference deployment of spool:
#
#   network → data (Aurora, store bucket, secrets) → app (ECS Fargate
#   behind ALB + CloudFront for HTTP, NLB for git-over-SSH)
#
# plus an optional dns module when the stack serves on a name of its own.
# Module boundaries follow blast radius: the data module holds everything
# stateful, the app module is fully replaceable.
#
# One root, any number of environments: `env` names everything
# (`<project>-<env>-*`, `/<project>/<env>/…`), each environment has its own
# state key and its own `envs/<env>.tfvars`, and `scripts/tf.sh <env>` sets
# both from one argument. The env lock below is what stops the two from
# disagreeing when they are set some other way.

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
  source              = "./modules/data"
  project             = var.project
  env                 = var.env
  deletion_protection = var.deletion_protection
  vpc_id              = module.network.vpc_id
  private_subnets     = module.network.private_subnet_ids
  db_min_acu          = var.db_min_acu
  db_max_acu          = var.db_max_acu

  ci_log_retention_days = var.ci_log_retention_days

  # The identity the app's credential may send mail as: the domain's,
  # when there is one and SES is the transport. Otherwise the credential
  # cannot send mail at all.
  ses_identity_arns = local.mail_from == "" ? [] : [module.dns[0].mail_identity_arn]
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

# The name the product answers to, when it has one. Absent, the stack
# answers on CloudFront's and the NLB's generated hostnames.
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

locals {
  # SES needs a verified identity, and the only one the stack makes is
  # the domain's — so no domain means no SES sender (variables.tf refuses
  # a `mail_from` without one), and a domain means a default sender.
  # Empty means the stack configures no mail at all.
  mail_from = var.domain_name == "" || !var.ses_mail ? "" : (var.mail_from != "" ? var.mail_from : "no-reply@${var.domain_name}")
}

module "app" {
  source               = "./modules/app"
  project              = var.project
  env                  = var.env
  deletion_protection  = var.deletion_protection
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

  github_app_slug       = var.github_app_slug
  github_app_secret_arn = module.data.github_app_secret_arn
  mail_from             = local.mail_from

  extra_environment = var.extra_environment
  extra_secrets     = var.extra_secrets
}
