variable "project" {
  description = "Prefix for every name the stack makes: `<project>-<env>-*` resources, `/<project>/<env>/…` parameters and log groups, `<project>/<env>/…` secrets."
  type        = string
  default     = "spool"
}

variable "env" {
  description = "Environment name, e.g. `prod` or `staging`. Part of every resource name, and recorded in the state on the first apply (modules/env-lock) so a later plan with a different value is refused."
  type        = string
  default     = "prod"
}

variable "aws_region" {
  type    = string
  default = "us-east-1"
}

variable "deletion_protection" {
  description = "Keep the stateful parts from being destroyed by accident: RDS deletion protection and a final snapshot, a store bucket and ECR repository that refuse to be deleted while they hold anything, and a 30-day recovery window on every secret. Set false only for a stack you intend to tear down and rebuild (a trial, a staging copy)."
  type        = bool
  default     = true
}

variable "vpc_cidr" {
  type    = string
  default = "10.40.0.0/16"
}

variable "image_tag" {
  description = "Image tag the service is created with. The first apply needs this tag already pushed to the ECR repository the stack creates (docs/deployment-aws.md). After that, deploy/roll.sh rolls new versions; terraform does not move the service."
  type        = string
  default     = "bootstrap"
}

variable "task_cpu" {
  type    = number
  default = 2048
}

variable "task_memory" {
  description = "MiB. Sized for 16 concurrent buffered 64 MiB pushes plus git subprocesses."
  type        = number
  default     = 8192
}

variable "desired_count" {
  description = "Tasks the service runs, and the autoscaling floor. Any number of nodes can serve any repository; two is the smallest count that survives losing one."
  type        = number
  default     = 2
}

variable "max_count" {
  description = "Autoscaling ceiling."
  type        = number
  default     = 10
}

variable "db_min_acu" {
  type    = number
  default = 0.5
}

variable "db_max_acu" {
  type    = number
  default = 4
}

variable "domain_name" {
  description = "Optional name to serve on, e.g. `git.example.com`. Empty = CloudFront's default certificate on its *.cloudfront.net hostname, and SSH on the NLB's hostname. Set, the stack creates a Route 53 zone for this name, issues a DNS-validated certificate for it, serves the product there, puts SSH at `ssh.<domain_name>`, verifies the name with SES for outgoing mail, and STRATUM_PUBLIC_URL follows it. The zone must be delegated before the certificate can issue — see docs/deployment-aws.md."
  type        = string
  default     = ""
}

variable "parent_domain_name" {
  description = "The Route 53 zone `domain_name` sits directly under, when that zone is in this same AWS account — `example.com` for `git.example.com`. The stack then writes the NS delegation into it, so the first apply needs no manual DNS step. Empty when the parent zone lives elsewhere; delegate `name_servers` by hand."
  type        = string
  default     = ""

  validation {
    condition     = var.parent_domain_name == "" || (var.domain_name != "" && endswith(var.domain_name, ".${var.parent_domain_name}"))
    error_message = "parent_domain_name must be the zone domain_name sits directly under (domain_name = git.example.com, parent_domain_name = example.com)."
  }
}

variable "origin_shield_region" {
  description = "Region for CloudFront Origin Shield. Empty = the deploy region (aws_region). Override only if aws_region is one of the few regions without Origin Shield support — point it at the nearest supported region."
  type        = string
  default     = ""
}

variable "cdn_url_ttl_secs" {
  description = "Lifetime of a signed CDN pack URL. Must comfortably exceed a slow clone: git does not fall back to the server for an advertised pack, so a URL that expires mid-clone fails it outright."
  type        = number
  default     = 3600
}

variable "gc_interval_secs" {
  description = "Seconds between garbage-collection passes, which reclaim superseded storage epochs and the objects of deleted repositories. 0 turns GC off, and nothing a repository no longer needs ever leaves the bucket."
  type        = number
  default     = 3600
}

variable "ci_log_retention_days" {
  description = "Days a workflow job's log survives under ci/logs/ in the store bucket before S3 expires it."
  type        = number
  default     = 90
}

variable "github_app_slug" {
  description = "Slug of your GitHub App (`acme-spool` for https://github.com/apps/acme-spool). Empty = no GitHub provider: no mirrors of or imports from GitHub, no signing in with GitHub, and the dashboard's Connect button answers 501. Set, the `<project>/<env>/github-app` secret must be filled first — see docs/operations.md — and the plan refuses if any of its five values is blank."
  type        = string
  default     = ""
}

variable "mail_from" {
  description = "Address the server sends mail as, through SES. Only meaningful with `domain_name`, and must be under it — the domain is the one identity SES is told to trust. Empty = `no-reply@<domain_name>` when there is a domain, and no SES mail at all when there is not (see ses_mail)."
  type        = string
  default     = ""

  validation {
    condition     = var.mail_from == "" || (var.domain_name != "" && endswith(var.mail_from, "@${var.domain_name}"))
    error_message = "mail_from needs domain_name and must be an address under it: the domain is the one identity SES is told to sign for."
  }
}

variable "ses_mail" {
  description = "Send mail through Amazon SES as mail_from. Needs domain_name; without one there is no identity to send as and the stack configures no mail. false leaves mail to you: set STRATUM_MAIL_TRANSPORT=smtp and the rest through extra_environment/extra_secrets (docs/operations.md)."
  type        = bool
  default     = true
}

variable "extra_environment" {
  description = "Further STRATUM_* settings for the server container, as plain values: `{ STRATUM_LAND_POLL_SECS = \"5\" }`. docs/operations.md lists them. A name the stack already sets is refused rather than silently doubled."
  type        = map(string)
  default     = {}
}

variable "extra_secrets" {
  description = "Further settings injected from Secrets Manager, name → secret ARN (optionally `<arn>:<json-key>::`): `{ STRATUM_MAIL_SMTP_PASSWORD = \"arn:aws:secretsmanager:…:secret:smtp-AbCdEf\" }`. The task's execution role is granted read on exactly these secrets."
  type        = map(string)
  default     = {}

  validation {
    condition     = alltrue([for v in values(var.extra_secrets) : startswith(v, "arn:") && length(split(":", v)) >= 7])
    error_message = "Each extra_secrets value is a Secrets Manager secret ARN (arn:aws:secretsmanager:<region>:<account>:secret:<name>), optionally followed by :<json-key>::."
  }
}
