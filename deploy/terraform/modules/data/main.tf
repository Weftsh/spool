# Everything stateful: Aurora Serverless v2 (control plane), the S3 store
# bucket (data plane), and the secrets tasks boot from.

variable "project" { type = string }
variable "env" { type = string }
variable "deletion_protection" {
  description = "Keep the database, the bucket and the secrets from being destroyed by accident. See the root variable."
  type        = bool
}
variable "vpc_id" { type = string }
variable "private_subnets" { type = list(string) }
variable "db_min_acu" { type = number }
variable "db_max_acu" { type = number }
variable "ci_log_retention_days" {
  description = "How long a workflow job's log survives under ci/logs/. Build logs are operational debris, not repository data, and nothing in the product reads one after the run is settled."
  type        = number
}

data "aws_caller_identity" "current" {}
data "aws_region" "current" {}

locals {
  prefix = "${var.project}-${var.env}"
  # A secret scheduled for deletion keeps its name for the recovery
  # window, and the next apply of the same environment fails on "already
  # scheduled for deletion" — so a stack meant to be torn down and
  # rebuilt gets none.
  secret_recovery_days = var.deletion_protection ? 30 : 0
}

# ------------------------------------------------------------- Aurora

resource "aws_db_subnet_group" "db" {
  name       = local.prefix
  subnet_ids = var.private_subnets
}

resource "aws_security_group" "db" {
  name_prefix = "${local.prefix}-db-"
  vpc_id      = var.vpc_id
  # Ingress is granted at the root (app SG → 5432); no other path in.

  lifecycle {
    create_before_destroy = true
  }
}

# The app's postgres client is deliberately NoTls (see stratum-control):
# traffic stays inside the VPC on an SG-fenced port. Aurora PG15+ ships
# rds.force_ssl=1 by default, which would refuse every connection — this
# parameter group is load-bearing, not cosmetic.
resource "aws_rds_cluster_parameter_group" "db" {
  name   = "${local.prefix}-pg16"
  family = "aurora-postgresql16"

  parameter {
    name  = "rds.force_ssl"
    value = "0"
  }
}

resource "random_password" "db_master" {
  length  = 32
  special = false
}

resource "aws_rds_cluster" "db" {
  cluster_identifier = local.prefix
  engine             = "aurora-postgresql"
  engine_mode        = "provisioned"
  # Major only. A pinned minor ("16.6") is one AWS retires on its own
  # schedule, and the first real apply in a fresh account failed on
  # `Cannot find version 16.6 for aurora-postgresql` — a stack that could
  # be rebuilt a month ago cannot be rebuilt today, and nothing in this
  # repository would have noticed until the day it mattered. RDS resolves
  # the major to its current default minor; the provider records what it
  # got in `engine_version_actual`, so there is no drift to ignore.
  engine_version                  = "16"
  database_name                   = "stratum"
  master_username                 = "stratum"
  master_password                 = random_password.db_master.result
  db_subnet_group_name            = aws_db_subnet_group.db.name
  vpc_security_group_ids          = [aws_security_group.db.id]
  db_cluster_parameter_group_name = aws_rds_cluster_parameter_group.db.name
  storage_encrypted               = true
  backup_retention_period         = 7
  # With protection on, `destroy` is refused until it is turned off, and
  # the cluster leaves a final snapshot behind when it does go. Without
  # it neither: a snapshot outlives `destroy` and is billed.
  deletion_protection       = var.deletion_protection
  skip_final_snapshot       = !var.deletion_protection
  final_snapshot_identifier = "${local.prefix}-final"

  serverlessv2_scaling_configuration {
    min_capacity = var.db_min_acu
    max_capacity = var.db_max_acu
  }
}

resource "aws_rds_cluster_instance" "db" {
  cluster_identifier   = aws_rds_cluster.db.id
  identifier           = "${local.prefix}-0"
  instance_class       = "db.serverless"
  engine               = aws_rds_cluster.db.engine
  engine_version       = aws_rds_cluster.db.engine_version
  db_subnet_group_name = aws_db_subnet_group.db.name
}

# The app consumes one composed URL (master user: migrations run DDL under
# the app's own advisory lock at boot; a least-privilege split is a noted
# future step, not a v1 requirement).
resource "aws_secretsmanager_secret" "db_url" {
  name                    = "${var.project}/${var.env}/db-url"
  recovery_window_in_days = local.secret_recovery_days
}

resource "aws_secretsmanager_secret_version" "db_url" {
  secret_id = aws_secretsmanager_secret.db_url.id
  secret_string = format(
    "postgres://%s:%s@%s:5432/%s",
    aws_rds_cluster.db.master_username,
    random_password.db_master.result,
    aws_rds_cluster.db.endpoint,
    aws_rds_cluster.db.database_name,
  )
}

# ------------------------------------------------------------ store bucket

resource "aws_s3_bucket" "store" {
  bucket = "${local.prefix}-store-${data.aws_caller_identity.current.account_id}"
  # The store is every repository's packs. Refusing to delete a bucket
  # that still has them is the point of deletion protection; without it,
  # `destroy` empties the bucket first.
  force_destroy = !var.deletion_protection
}

resource "aws_s3_bucket_public_access_block" "store" {
  bucket                  = aws_s3_bucket.store.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

# NO versioning, deliberately: the store is content-addressed (immutable
# segments; the manifest pointer is the only overwrite, protected by CAS),
# and compaction deletes superseded packs — versioning would retain every
# one at full price. Abort stale multipart uploads instead.
resource "aws_s3_bucket_lifecycle_configuration" "store" {
  bucket = aws_s3_bucket.store.id

  rule {
    id     = "abort-incomplete-mpu"
    status = "Enabled"

    filter {}

    abort_incomplete_multipart_upload {
      days_after_initiation = 7
    }
  }

  # Workflow logs are the one thing in this bucket that is NOT
  # content-addressed and NOT referenced forever: a settled run's chunks
  # are deleted by the finish handler, and this rule catches the
  # stragglers a crashed runner leaves behind plus the complete logs of
  # runs nobody will read again.
  rule {
    id     = "expire-ci-logs"
    status = "Enabled"

    filter {
      prefix = "ci/logs/"
    }

    expiration {
      days = var.ci_log_retention_days
    }
  }
}

# The store client (stratum-store/src/sig.rs) signs with STATIC env
# credentials only — no task-role/IMDS support, and missing credentials
# silently fall back to anonymous (= every request denied). Hence a
# dedicated IAM user whose access key reaches tasks via Secrets Manager.
#
# It is the one AWS credential the app holds, so the SES transport
# (stratum-server/src/mail/ses.rs) signs with it too — the same SigV4,
# read from the same AWS_ACCESS_KEY_ID. Sending is allowed only from the
# identities the dns module verified, which is nothing at all without a
# domain.
resource "aws_iam_user" "store" {
  name = "${local.prefix}-store"
}

variable "ses_identity_arns" {
  description = "SES identities the server sends as. Non-empty turns the grant on; the grant itself covers every identity in the account, because a sandboxed SES checks the recipient's identity as well as the sender's. Empty = the app's credential cannot send mail, which is right for a stack with no domain: STRATUM_MAIL_TRANSPORT stays `null` there."
  type        = list(string)
  default     = []
}

data "aws_iam_policy_document" "store_user" {
  statement {
    actions = [
      "s3:AbortMultipartUpload",
      "s3:DeleteObject",
      "s3:GetObject",
      "s3:ListBucket",
      "s3:ListBucketMultipartUploads",
      "s3:ListMultipartUploadParts",
      "s3:PutObject",
    ]
    resources = [
      aws_s3_bucket.store.arn,
      "${aws_s3_bucket.store.arn}/*",
    ]
  }

  # Every identity in the account, not only the one the server sends
  # *as*. SES authorizes SendEmail against the recipient's identity too
  # while the account is in the sandbox — where every recipient must be a
  # verified identity — so a grant on the sending domain alone answers
  # `not authorized to perform ses:SendEmail on resource
  # …identity/<recipient>` for every message: sign-up verification,
  # invitations, notifications, all of it, with nothing on screen to say
  # so. If this account holds SES identities that belong to something
  # else, narrow this to the ones the server should use.
  dynamic "statement" {
    for_each = length(var.ses_identity_arns) == 0 ? [] : [1]
    content {
      actions   = ["ses:SendEmail"]
      resources = ["arn:aws:ses:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:identity/*"]
    }
  }
}

resource "aws_iam_user_policy" "store" {
  name   = "store-bucket"
  user   = aws_iam_user.store.name
  policy = data.aws_iam_policy_document.store_user.json
}

resource "aws_iam_access_key" "store" {
  user = aws_iam_user.store.name
}

resource "aws_secretsmanager_secret" "store_creds" {
  name                    = "${var.project}/${var.env}/store-credentials"
  recovery_window_in_days = local.secret_recovery_days
}

resource "aws_secretsmanager_secret_version" "store_creds" {
  secret_id = aws_secretsmanager_secret.store_creds.id
  secret_string = jsonencode({
    AWS_ACCESS_KEY_ID     = aws_iam_access_key.store.id
    AWS_SECRET_ACCESS_KEY = aws_iam_access_key.store.secret
  })
}

# ------------------------------------------------------------ app secrets

resource "random_password" "webhook" {
  length  = 40
  special = false
}

resource "aws_secretsmanager_secret" "webhook" {
  name                    = "${var.project}/${var.env}/webhook-secret"
  recovery_window_in_days = local.secret_recovery_days
}

resource "aws_secretsmanager_secret_version" "webhook" {
  secret_id     = aws_secretsmanager_secret.webhook.id
  secret_string = random_password.webhook.result
}

# One stable SSH host identity: every task presents this key, so clients
# that pinned it on first connect keep trusting the service across
# deploys and scale events.
resource "tls_private_key" "ssh_host" {
  algorithm = "ED25519"
}

resource "aws_secretsmanager_secret" "ssh_host_key" {
  name                    = "${var.project}/${var.env}/ssh-host-key"
  recovery_window_in_days = local.secret_recovery_days
}

resource "aws_secretsmanager_secret_version" "ssh_host_key" {
  secret_id     = aws_secretsmanager_secret.ssh_host_key.id
  secret_string = tls_private_key.ssh_host.private_key_openssh
}

# The CloudFront signing key for CDN-offloaded clones. RSA-2048 because
# that is what CloudFront signed URLs require (RSA-SHA1 over a canned
# policy) — unlike the SSH host identity above, the algorithm is not a
# choice. The private half reaches tasks via Secrets Manager; the public
# half is uploaded to CloudFront by the app module.
resource "tls_private_key" "cdn" {
  algorithm = "RSA"
  rsa_bits  = 2048
}

resource "aws_secretsmanager_secret" "cdn_key" {
  name                    = "${var.project}/${var.env}/cdn-key"
  recovery_window_in_days = local.secret_recovery_days
}

resource "aws_secretsmanager_secret_version" "cdn_key" {
  secret_id     = aws_secretsmanager_secret.cdn_key.id
  secret_string = tls_private_key.cdn.private_key_pem
}

# The GitHub App, for mirrors, imports and signing in with GitHub: one
# JSON secret with the five things GitHub issues when the App is created —
# `app_id`, `private_key` (the whole PEM; JSON carries the newlines),
# `webhook_secret`, and the App's OAuth client, `client_id` and
# `client_secret`. Created blank here, because GitHub issues them rather
# than terraform generating them, and filled by a person:
#
#   aws secretsmanager put-secret-value --secret-id <project>/<env>/github-app \
#     --secret-string "$(jq -n --arg id 123456 --rawfile pem app.private-key.pem \
#         --arg wh <webhook-secret> --arg cid Iv1… --arg cs <client-secret> \
#         '{app_id:$id,private_key:$pem,webhook_secret:$wh,client_id:$cid,client_secret:$cs}')"
#
# `ignore_changes` keeps a later apply from putting the blanks back. The
# app module injects these only when `github_app_slug` is set, and
# refuses to plan a task definition with any of the five blank: a server
# with an app id and no key refuses to boot, and one with a blank webhook
# secret verifies every GitHub delivery against "" and drops it, which
# looks like GitHub not calling rather than like a misconfiguration.
resource "aws_secretsmanager_secret" "github_app" {
  name                    = "${var.project}/${var.env}/github-app"
  recovery_window_in_days = local.secret_recovery_days
}

resource "aws_secretsmanager_secret_version" "github_app" {
  secret_id     = aws_secretsmanager_secret.github_app.id
  secret_string = jsonencode({ app_id = "", private_key = "", webhook_secret = "", client_id = "", client_secret = "" })
  lifecycle {
    ignore_changes = [secret_string]
  }
}

# ------------------------------------------------------------------ outputs

output "db_sg_id" {
  value = aws_security_group.db.id
}

output "store_bucket" {
  value = aws_s3_bucket.store.bucket
}

output "db_url_secret_arn" {
  value = aws_secretsmanager_secret.db_url.arn
}

output "store_creds_secret_arn" {
  value = aws_secretsmanager_secret.store_creds.arn
}

output "webhook_secret_arn" {
  value = aws_secretsmanager_secret.webhook.arn
}

output "ssh_host_key_secret_arn" {
  value = aws_secretsmanager_secret.ssh_host_key.arn
}

output "cdn_key_secret_arn" {
  value = aws_secretsmanager_secret.cdn_key.arn
}

output "github_app_secret_arn" {
  value = aws_secretsmanager_secret.github_app.arn
}

output "github_app_secret_name" {
  value = aws_secretsmanager_secret.github_app.name
}

output "cdn_public_key_pem" {
  value = tls_private_key.cdn.public_key_pem
}

output "store_bucket_arn" {
  value = aws_s3_bucket.store.arn
}

output "store_bucket_regional_domain_name" {
  value = aws_s3_bucket.store.bucket_regional_domain_name
}
